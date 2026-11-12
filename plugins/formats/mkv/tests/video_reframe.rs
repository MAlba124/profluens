//! AVC and HEVC tracks demux to an Annex B head and start-code access units, through the element,
//! or as stored to a consumer that takes them so.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mkv::{MatroskaWriter, MkvDemux, TrackConfig};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::Outcome;
use profluens_core::time::Timestamp;
use profluens_core::topology::Leave;

const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "bytesrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct ByteSrc {
    data: Vec<u8>,
    chunk: usize,
    pos: usize,
}

impl ByteSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for ByteSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.pos = 0;
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let cap = buf.memory.capacity();
        let n = self.chunk.min(cap).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

static SINK_OFFERS: [OfferDesc; 7] = [
    OfferDesc::any("flac"),
    OfferDesc::any("bytes"),
    OfferDesc::any("vp8"),
    OfferDesc::any("vp9"),
    OfferDesc::any("av1"),
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = sink_desc("recsink", &SINK_PADS);

// The menu of a decoder that reads units either way and names them as stored first, as the
// hardware ones do.
static STORED_OFFERS: [OfferDesc; 4] = [
    OfferDesc::any("h264/avcc"),
    OfferDesc::any("h265/hvcc"),
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
];
static STORED_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &STORED_OFFERS }];
static STORED_DESC: ElementDesc = sink_desc("storedsink", &STORED_PADS);

// A muxer's, which reads either and names Annex B first.
static EITHER_OFFERS: [OfferDesc; 4] = [
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
    OfferDesc::any("h264/avcc"),
    OfferDesc::any("h265/hvcc"),
];
static EITHER_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &EITHER_OFFERS }];
static EITHER_DESC: ElementDesc = sink_desc("eithersink", &EITHER_PADS);

static ANY_OFFERS: [OfferDesc; 1] = [OfferDesc::wildcard()];
static ANY_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY_OFFERS }];
static ANY_DESC: ElementDesc = sink_desc("anysink", &ANY_PADS);

const fn sink_desc(name: &'static str, sinks: &'static [PadDesc]) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs: &[],
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

#[derive(Default, Debug)]
struct Recorded {
    buffers: Vec<Vec<u8>>,
    flags: Vec<BufferFlags>,
    /// The family of each format the pad was told.
    families: Vec<String>,
}

struct RecSink {
    desc: &'static ElementDesc,
    rec: Arc<Mutex<Recorded>>,
}

impl RecSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.buffers.push(buf.memory.data().to_vec());
            rec.flags.push(buf.flags);
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            let family = ctx.family_name(f.family).unwrap_or("?").to_owned();
            self.rec.lock().unwrap().families.push(family);
        }
        Ok(())
    }
}

fn header_prefix(stream: &[u8]) -> Vec<u8> {
    let cluster = stream
        .windows(4)
        .position(|w| w == pf_mkv::ebml::id::CLUSTER)
        .expect("stream has a Cluster");
    stream[..cluster].to_vec()
}

fn run(stream: Vec<u8>, chunk: usize) -> Vec<Vec<u8>> {
    run_flagged(stream, chunk).0
}

fn run_flagged(stream: Vec<u8>, chunk: usize) -> (Vec<Vec<u8>>, Vec<BufferFlags>) {
    let rec = run_into(stream, chunk, &SINK_DESC);
    (rec.buffers, rec.flags)
}

/// What a sink of `desc` takes of the stream's one track.
fn run_into(stream: Vec<u8>, chunk: usize, desc: &'static ElementDesc) -> Recorded {
    let header = header_prefix(&stream);
    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: stream, chunk, pos: 0 });
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(ByteSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 1, "one track → one src pad");

    let rec = Arc::new(Mutex::new(Recorded::default()));
    let sink = p.add(RecSink { desc, rec: Arc::clone(&rec) });
    let ap = &added[0];
    p.link(ap.element.src(ap.pad), sink.sink(RecSink::IN)).expect("pad -> sink");
    p.run().expect("run");
    drop(p);

    Arc::try_unwrap(rec).unwrap().into_inner().unwrap()
}

fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut cfg = vec![1, 0x42, 0x00, 0x0A, 0xFF, 0xE1];
    cfg.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    cfg.extend_from_slice(sps);
    cfg.push(1);
    cfg.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    cfg.extend_from_slice(pps);
    cfg
}

fn hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut cfg = vec![1u8];
    cfg.extend_from_slice(&[0u8; 20]);
    cfg.push(0xFC | 0x03); // octet 21: lengthSizeMinusOne = 3
    cfg.push(3); // numOfArrays
    for (t, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        cfg.push(t);
        cfg.extend_from_slice(&1u16.to_be_bytes());
        cfg.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        cfg.extend_from_slice(nal);
    }
    cfg
}

fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
    let mut b = Vec::new();
    for nal in nals {
        b.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        b.extend_from_slice(nal);
    }
    b
}

fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
    let mut b = Vec::new();
    for nal in nals {
        b.extend_from_slice(&START_CODE);
        b.extend_from_slice(nal);
    }
    b
}

#[test]
fn avc_track_reframes_to_annex_b() {
    let sps = [0x67, 0x42, 0x00, 0x0A, 0x11, 0x22];
    let pps = [0x68, 0xCE, 0x3C, 0x80];
    let cfg = avcc(&sps, &pps);

    let au0 = length_prefixed(&[&[0x65, 0xAA, 0xBB], &[0x41, 0xCC]]);
    let au1 = length_prefixed(&[&[0x41, 0xDD, 0xEE, 0xFF]]);

    let track = TrackConfig::video(1, "V_MPEG4/ISO/AVC", cfg, 320, 240);
    let mut w = MatroskaWriter::new(vec![track]);
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    w.write_frame(&mut stream, 1, 0, &au0, true).unwrap();
    w.write_frame(&mut stream, 1, 33_000_000, &au1, false).unwrap();
    w.finalize(&mut stream);

    let bufs = run(stream, 500);
    assert_eq!(bufs.len(), 3, "head + one buffer per access unit");
    assert_eq!(bufs[0], annex_b(&[&sps, &pps]), "SPS+PPS Annex B head");
    assert_eq!(bufs[1], annex_b(&[&[0x65, 0xAA, 0xBB], &[0x41, 0xCC]]), "AU0 reframed");
    assert_eq!(bufs[2], annex_b(&[&[0x41, 0xDD, 0xEE, 0xFF]]), "AU1 reframed");
}

#[test]
fn the_head_buffer_is_flagged_and_media_is_not() {
    let sps = [0x67, 0x42, 0x00, 0x0A, 0x11, 0x22];
    let pps = [0x68, 0xCE, 0x3C, 0x80];
    let au0 = length_prefixed(&[&[0x65, 0xAA, 0xBB]]);
    let au1 = length_prefixed(&[&[0x41, 0xDD]]);

    let track = TrackConfig::video(1, "V_MPEG4/ISO/AVC", avcc(&sps, &pps), 320, 240);
    let mut w = MatroskaWriter::new(vec![track]);
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    w.write_frame(&mut stream, 1, 0, &au0, true).unwrap();
    w.write_frame(&mut stream, 1, 33_000_000, &au1, false).unwrap();
    w.finalize(&mut stream);

    let (bufs, flags) = run_flagged(stream, 500);
    assert_eq!(bufs.len(), 3, "head + one buffer per access unit");
    assert_eq!(bufs[0], annex_b(&[&sps, &pps]), "SPS+PPS Annex B head");
    assert!(flags[0].contains(BufferFlags::HEADER), "the head is flagged");
    assert!(flags[1..].iter().all(|f| !f.contains(BufferFlags::HEADER)), "media is not: {flags:?}");
}

#[test]
fn hevc_track_reframes_to_annex_b() {
    let vps = [0x40, 0x01, 0x0C, 0x01];
    let sps = [0x42, 0x01, 0x01, 0x02];
    let pps = [0x44, 0x01, 0xC0];
    let cfg = hvcc(&vps, &sps, &pps);
    let au0 = length_prefixed(&[&[0x26, 0x01, 0xAF], &[0x02, 0x01, 0xD0]]);

    let track = TrackConfig::video(1, "V_MPEGH/ISO/HEVC", cfg, 640, 360);
    let mut w = MatroskaWriter::new(vec![track]);
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    w.write_frame(&mut stream, 1, 0, &au0, true).unwrap();
    w.finalize(&mut stream);

    let bufs = run(stream, 64);
    assert_eq!(bufs.len(), 2, "head + one access unit");
    assert_eq!(bufs[0], annex_b(&[&vps, &sps, &pps]), "VPS+SPS+PPS Annex B head");
    assert_eq!(bufs[1], annex_b(&[&[0x26, 0x01, 0xAF], &[0x02, 0x01, 0xD0]]), "AU0 reframed");
}

#[test]
fn vp8_track_forwards_frames_verbatim() {
    let f0: &[u8] = &[0x10, 0x20, 0x30, 0x40];
    let f1: &[u8] = &[0x50, 0x60];
    let track = TrackConfig::vp8(1, 128, 96);
    let mut w = MatroskaWriter::new(vec![track]);
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    w.write_frame(&mut stream, 1, 0, f0, true).unwrap();
    w.write_frame(&mut stream, 1, 33_000_000, f1, true).unwrap();
    w.finalize(&mut stream);

    let bufs = run(stream, 500);
    assert_eq!(bufs, vec![f0.to_vec(), f1.to_vec()], "no head; VP8 frames verbatim");
}

#[test]
fn malformed_avcc_degrades_without_panic() {
    let cfg = vec![1u8];
    let f0: &[u8] = &[0x00, 0x00, 0x00, 0x02, 0x65, 0xAB]; // one length-prefixed NAL
    let track = TrackConfig::video(1, "V_MPEG4/ISO/AVC", cfg, 320, 240);
    let mut w = MatroskaWriter::new(vec![track]);
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    w.write_frame(&mut stream, 1, 0, f0, true).unwrap();
    w.finalize(&mut stream);

    let bufs = run(stream, 500);
    assert_eq!(bufs, vec![f0.to_vec()], "malformed record → passthrough, no panic");
}

const SPS: [u8; 6] = [0x67, 0x42, 0x00, 0x0A, 0x11, 0x22];
const PPS: [u8; 4] = [0x68, 0xCE, 0x3C, 0x80];
const HEVC_SETS: [&[u8]; 3] =
    [&[0x40, 0x01, 0x0C, 0x01], &[0x42, 0x01, 0x01, 0x02], &[0x44, 0x01, 0xC0]];

/// A key picture and one that leans on it, per codec.
fn pictures(hevc: bool) -> [Vec<&'static [u8]>; 2] {
    match hevc {
        true => [vec![&[0x26, 0x01, 0xAF], &[0x26, 0x01, 0xB0]], vec![&[0x02, 0x01, 0xD0]]],
        false => [vec![&[0x65, 0xAA, 0xBB], &[0x65, 0xCC]], vec![&[0x41, 0xDD, 0xEE, 0xFF]]],
    }
}

fn record(hevc: bool) -> Vec<u8> {
    match hevc {
        true => hvcc(HEVC_SETS[0], HEVC_SETS[1], HEVC_SETS[2]),
        false => avcc(&SPS, &PPS),
    }
}

/// A stream of one AVC or HEVC track, `frames` pictures 40 ms apart, every tenth a key.
fn nal_stream(hevc: bool, cfg: Vec<u8>, frames: u64) -> Vec<u8> {
    let codec_id = if hevc { "V_MPEGH/ISO/HEVC" } else { "V_MPEG4/ISO/AVC" };
    let [key, delta] = pictures(hevc);
    let mut w = MatroskaWriter::new(vec![TrackConfig::video(1, codec_id, cfg, 320, 240)]);
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    for i in 0..frames {
        let nals = if i.is_multiple_of(10) { &key } else { &delta };
        w.write_frame(&mut stream, 1, i * 40_000_000, &length_prefixed(nals), i.is_multiple_of(10))
            .unwrap();
    }
    w.finalize(&mut stream);
    stream
}

/// The record and the blocks untouched, as a pad linked by the stored family puts them out.
fn as_stored(hevc: bool, frames: usize) -> Vec<Vec<u8>> {
    let [key, delta] = pictures(hevc);
    let block = |i: usize| length_prefixed(if i.is_multiple_of(10) { &key } else { &delta });
    std::iter::once(record(hevc)).chain((0..frames).map(block)).collect()
}

/// The Annex B head and each block reframed.
fn reframed(hevc: bool, frames: usize) -> Vec<Vec<u8>> {
    let [key, delta] = pictures(hevc);
    let head = match hevc {
        true => annex_b(&HEVC_SETS),
        false => annex_b(&[&SPS, &PPS]),
    };
    let block = |i: usize| annex_b(if i.is_multiple_of(10) { &key } else { &delta });
    std::iter::once(head).chain((0..frames).map(block)).collect()
}

fn told(rec: &Recorded) -> Option<&str> {
    rec.families.last().map(String::as_str)
}

#[test]
fn a_consumer_that_names_the_stored_family_first_takes_the_record_and_the_blocks_as_stored() {
    for (hevc, family) in [(false, "h264/avcc"), (true, "h265/hvcc")] {
        let got = run_into(nal_stream(hevc, record(hevc), 2), 500, &STORED_DESC);
        assert_eq!(got.buffers, as_stored(hevc, 2), "{family}");
        assert!(got.flags[0].contains(BufferFlags::HEADER), "the record is the head");
        assert!(got.flags[1..].iter().all(|f| !f.contains(BufferFlags::HEADER)));
        assert_eq!(told(&got), Some(family));
    }
}

#[test]
fn every_other_consumer_takes_annex_b_as_before() {
    let cases: [(&str, &'static ElementDesc); 3] = [
        ("names Annex B alone", &SINK_DESC),
        ("names Annex B first", &EITHER_DESC),
        ("takes any family", &ANY_DESC),
    ];
    for (hevc, family) in [(false, "h264/annexb"), (true, "h265/annexb")] {
        for (what, desc) in cases {
            let got = run_into(nal_stream(hevc, record(hevc), 2), 500, desc);
            assert_eq!(got.buffers, reframed(hevc, 2), "a consumer that {what}");
            assert_eq!(told(&got), Some(family), "a consumer that {what}");
        }
    }
}

#[test]
fn a_record_that_does_not_parse_leaves_nothing_as_stored() {
    let stream = nal_stream(false, vec![1u8], 1);
    let got = run_into(stream, 500, &STORED_DESC);
    assert_eq!(got.buffers, [length_prefixed(&pictures(false)[0])], "passed on as before, no head");
    assert_eq!(told(&got), Some("h264/annexb"));
}

/// The HEVC stream played into a sink of `first`, which a sink of `second` takes over from while
/// it runs. What each took.
fn relinked(first: &'static ElementDesc, second: &'static ElementDesc) -> [Recorded; 2] {
    spliced(first, second, true)
}

/// [`relinked`], or with `cut` off the second sink beside the first on the one pad.
fn spliced(first: &'static ElementDesc, second: &'static ElementDesc, cut: bool) -> [Recorded; 2] {
    let stream = nal_stream(true, record(true), 120);
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p = Pipeline::with(DebugSpec::parse("debug"), false, Arc::new(clock.clone()));
    p.set_debug(DebugSpec::default());
    p.set_select_all(false);
    let header = header_prefix(&stream);
    let src = p.add(ByteSrc { data: stream, chunk: 64, pos: 0 });
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(ByteSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");
    assert!(p.preroll().expect("preroll").srcs.is_empty(), "describing tracks adds no pad");
    let recs: [Arc<Mutex<Recorded>>; 2] = Default::default();
    let play = |p: &mut Pipeline, desc, rec: &Arc<Mutex<Recorded>>| {
        let pad = p.select(demux, 0).expect("select");
        let sink = p.add(RecSink { desc, rec: Arc::clone(rec) });
        p.link(demux.src(pad), sink.sink(RecSink::IN)).expect("pad -> sink");
        sink
    };
    let old = play(&mut p, first, &recs[0]);
    let mut run = p.step(Arc::new(clock.clone())).expect("step");
    let pass = |run: &mut profluens_core::sched::Stepped, until: &dyn Fn() -> bool| {
        for _ in 0..2_000_000 {
            if until() {
                return false;
            }
            run.service();
            let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
                let Some(to) = run.next_deadline() else { break };
                clock.advance(Timestamp(to.0 - clock.now().0));
                continue;
            };
            run.pass(g).expect("pass");
        }
        (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
    };
    assert!(!pass(&mut run, &|| recs[0].lock().unwrap().buffers.len() >= 30), "it plays");
    if cut {
        p.remove(old, Leave::Cut).expect("remove");
    }
    play(&mut p, second, &recs[1]);
    p.commit().expect("commit");
    assert!(pass(&mut run, &|| false), "the run ends");
    run.finish().expect("finish");
    drop(p);
    recs.map(|r| std::mem::take(&mut *r.lock().unwrap()))
}

/// The frames of `rec` past its head, which are the stream's from some key picture on.
fn tail_of<'a>(rec: &'a Recorded, whole: &'a [Vec<u8>]) -> bool {
    let frames = &rec.buffers[1..];
    frames.len() > 20 && whole[1..].ends_with(frames) && frames[0] == whole[1]
}

#[test]
fn a_pad_goes_back_to_annex_b_for_a_consumer_that_takes_over_and_reads_only_that() {
    for second in [&ANY_DESC, &SINK_DESC] {
        let [first, second] = relinked(&STORED_DESC, second);
        assert!(as_stored(true, 120).starts_with(&first.buffers), "as stored up to the relink");
        assert_eq!(told(&first), Some("h265/hvcc"));
        let whole = reframed(true, 120);
        assert_eq!(second.buffers[0], whole[0], "led by the Annex B head");
        assert!(second.flags[0].contains(BufferFlags::HEADER));
        assert!(tail_of(&second, &whole), "and reframed from the next key picture on");
        assert_eq!(second.families, ["h265/annexb"], "it is told no family but its own");
    }
}

#[test]
fn a_pad_turns_to_stored_for_a_consumer_that_takes_over_and_names_it_first() {
    for first in [&ANY_DESC, &SINK_DESC] {
        let [first, second] = relinked(first, &STORED_DESC);
        assert!(reframed(true, 120).starts_with(&first.buffers), "reframed up to the relink");
        let whole = as_stored(true, 120);
        assert_eq!(second.buffers[0], whole[0], "led by the record");
        assert!(second.flags[0].contains(BufferFlags::HEADER));
        assert!(tail_of(&second, &whole), "and as stored from the next key picture on");
        assert_eq!(told(&second), Some("h265/hvcc"));
    }
}

#[test]
fn a_pad_two_consumers_share_leaves_as_stored_only_while_both_name_it_first() {
    let [first, second] = spliced(&STORED_DESC, &STORED_DESC, false);
    let whole = as_stored(true, 120);
    assert_eq!(first.buffers, whole, "the first takes the whole stream as stored");
    assert_eq!(second.buffers[0], whole[0], "the one that joins is led by the record");
    assert!(whole.ends_with(&second.buffers[1..]) && second.buffers.len() > 20);

    let [first, second] = spliced(&STORED_DESC, &ANY_DESC, false);
    let (stored, annex_b) = (as_stored(true, 120), reframed(true, 120));
    let turn = first.buffers.iter().position(|b| *b == annex_b[0]).expect("an Annex B head");
    assert!(turn > 20 && first.flags[turn].contains(BufferFlags::HEADER));
    assert!(stored.starts_with(&first.buffers[..turn]), "as stored until the other joined");
    let rest = &first.buffers[turn + 1..];
    assert!(rest.len() > 20 && annex_b.ends_with(rest), "then every frame reframed");
    assert_eq!(first.buffers.len(), 122, "no frame lost at the turn");
    assert_eq!(told(&first), Some("h265/annexb"));
    let head = second.buffers.iter().position(|b| *b == annex_b[0]).expect("its head");
    assert!(annex_b.ends_with(&second.buffers[head + 1..]), "the one that joined reads Annex B");
    assert!(second.buffers[..head].is_empty(), "and nothing as stored ahead of it");
}
