// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mp4::Mp4Demux;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

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
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok),
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

static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any("h264/annexb"), OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = sink_desc("recsink", &SINK_PADS);

// The menu of a decoder that reads units either way and names them as stored first, as the
// hardware ones do.
static STORED_OFFERS: [OfferDesc; 2] = [OfferDesc::any("h264/avcc"), OfferDesc::any("h264/annexb")];
static STORED_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &STORED_OFFERS }];
static STORED_DESC: ElementDesc = sink_desc("storedsink", &STORED_PADS);

// A muxer's, which reads either and names Annex B first.
static EITHER_OFFERS: [OfferDesc; 2] = [OfferDesc::any("h264/annexb"), OfferDesc::any("h264/avcc")];
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

#[derive(Default, Debug, PartialEq)]
struct Recorded {
    bytes: Vec<u8>,
    ptss: Vec<Option<u64>>,
    lens: Vec<usize>,
    flags: Vec<BufferFlags>,
    /// The family of each format the pad was told.
    families: Vec<String>,
}

impl Recorded {
    fn buffers(&self) -> Vec<&[u8]> {
        let mut at = 0;
        let cut = |len: &usize| {
            at += len;
            &self.bytes[at - len..at]
        };
        self.lens.iter().map(cut).collect()
    }
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
            rec.bytes.extend_from_slice(buf.memory.data());
            rec.ptss.push(buf.pts.nanos());
            rec.lens.push(buf.memory.data().len());
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

fn fixture_bytes(name: &str) -> Vec<u8> {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

fn head_through_moov(file: &[u8]) -> Vec<u8> {
    let mut at = 0usize;
    while at + 8 <= file.len() {
        let size =
            u32::from_be_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]) as usize;
        if &file[at + 4..at + 8] == b"mdat" {
            return file[..at].to_vec();
        }
        let advance = if size == 0 {
            file.len() - at
        } else if size == 1 {
            u64::from_be_bytes(file[at + 8..at + 16].try_into().unwrap()) as usize
        } else {
            size
        };
        if advance == 0 {
            break;
        }
        at += advance;
    }
    file.to_vec()
}

fn run_demux(file: Vec<u8>, chunk: usize) -> Vec<Recorded> {
    run_into(file, chunk, &SINK_DESC)
}

/// What sinks of `desc` take of each track.
fn run_into(file: Vec<u8>, chunk: usize, desc: &'static ElementDesc) -> Vec<Recorded> {
    let head = head_through_moov(&file);
    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: file, chunk, pos: 0 });
    let demux = p.add(Mp4Demux::new(head));
    p.link(src.src(ByteSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    let mut recs: Vec<Arc<Mutex<Recorded>>> = Vec::new();
    for ap in &added {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let sink = p.add(RecSink { desc, rec: Arc::clone(&rec) });
        p.link(ap.element.src(ap.pad), sink.sink(RecSink::IN)).expect("pad -> sink");
        recs.push(rec);
    }

    p.run().expect("run");
    drop(p);
    recs.into_iter().map(|rec| Arc::try_unwrap(rec).unwrap().into_inner().unwrap()).collect()
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[test]
fn single_h264_track_reconstructs_annex_b() {
    let file = fixture_bytes("tiny_h264.mp4");
    let recs = run_demux(file, 4096);
    assert_eq!(recs.len(), 1, "one track → one src pad");
    let rec = &recs[0];

    assert_eq!(&rec.bytes[..4], &START_CODE, "reconstruction begins with an Annex B start code");
    assert_eq!(rec.lens.len(), 16, "codec head + one buffer per sample");
    assert!(rec.flags[0].contains(BufferFlags::HEADER), "the head is flagged");
    assert!(
        rec.flags[1..].iter().all(|f| !f.contains(BufferFlags::HEADER)),
        "media is not: {:?}",
        rec.flags
    );
    let mut off = 0;
    for &len in &rec.lens {
        assert!(len >= 4, "each buffer is at least a start code + payload");
        assert_eq!(
            &rec.bytes[off..off + 4],
            &START_CODE,
            "each output buffer starts with a start code"
        );
        off += len;
    }
    assert_eq!(off, rec.bytes.len(), "buffer lengths tile the reconstructed stream");
}

#[test]
fn demuxed_timestamps_are_present_and_monotone() {
    let file = fixture_bytes("tiny_h264.mp4");
    let recs = run_demux(file, 4096);
    let rec = &recs[0];
    let ptss: Vec<u64> =
        rec.ptss.iter().map(|p| p.expect("every demuxed buffer carries a pts")).collect();
    assert_eq!(ptss.len(), 16);
    assert_eq!(ptss[0], ptss[1], "codec head rides the first sample's pts");
    assert_eq!(ptss[0], 0, "first sample presents at 0");
    for w in ptss[1..].windows(2) {
        assert!(w[1] > w[0], "sample pts strictly increase: {} !> {}", w[1], w[0]);
    }
    let last = *ptss.last().unwrap();
    let expected = 14u64 * 1_000_000_000 / 15; // ~933_333_333 ns
    assert!(
        last.abs_diff(expected) < 2_000_000,
        "last pts ~ 14/15 s (got {last}, want ~{expected})"
    );
}

#[test]
fn ragged_chunking_is_chunk_size_invariant() {
    let file = fixture_bytes("tiny_h264.mp4");
    let reference = run_demux(file.clone(), 1 << 20); // whole-file chunk
    let ref_rec = &reference[0];

    for &chunk in &[1usize, 2, 3, 7, 13, 64, 137, 512, 1000, 4096] {
        let got = run_demux(file.clone(), chunk);
        assert_eq!(got.len(), 1, "chunk {chunk}: still one track");
        assert_eq!(got[0].bytes, ref_rec.bytes, "chunk {chunk}: reconstructed stream identical");
        assert_eq!(got[0].ptss, ref_rec.ptss, "chunk {chunk}: pts sequence identical");
        assert_eq!(got[0].lens, ref_rec.lens, "chunk {chunk}: per-buffer framing identical");
    }
}

#[test]
fn bframes_fixture_demuxes() {
    let file = fixture_bytes("bframes_h264.mp4");
    let recs = run_demux(file, 4096);
    assert_eq!(recs.len(), 1);
    let rec = &recs[0];
    assert_eq!(rec.lens.len(), 16, "codec head + 15 samples");
    assert!(rec.ptss.iter().all(|p| p.is_some()), "every buffer carries a pts");
    assert_eq!(&rec.bytes[..4], &START_CODE);
}

/// The payload of the file's first `avcC` box.
fn avcc_of(file: &[u8]) -> &[u8] {
    let at = file.windows(4).position(|w| w == b"avcC").expect("an avcC");
    let size = u32::from_be_bytes(file[at - 4..at].try_into().unwrap()) as usize;
    &file[at + 4..at - 4 + size]
}

/// A length prefixed sample with a start code for each length.
fn to_annex_b(sample: &[u8], size: usize) -> Vec<u8> {
    let (mut out, mut at) = (Vec::new(), 0);
    while at + size <= sample.len() {
        let len = sample[at..at + size].iter().fold(0usize, |n, &b| (n << 8) | usize::from(b));
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&sample[at + size..at + size + len]);
        at += size + len;
    }
    assert_eq!(at, sample.len(), "the lengths tile the sample");
    out
}

#[test]
fn a_consumer_that_names_the_stored_family_first_takes_the_record_and_the_samples_as_stored() {
    let file = fixture_bytes("tiny_h264.mp4");
    let record = avcc_of(&file).to_vec();
    let size = usize::from(record[4] & 3) + 1;
    let reframed = &run_demux(file.clone(), 4096)[0];
    let stored = &run_into(file, 4096, &STORED_DESC)[0];
    let (want, got) = (reframed.buffers(), stored.buffers());
    assert_eq!(got.len(), 16, "the record and one buffer a sample");
    assert_eq!(got[0], record, "led by the avcC as the file holds it");
    assert!(stored.flags[0].contains(BufferFlags::HEADER), "the record is the head");
    for (i, (sample, annex_b)) in got.iter().zip(&want).enumerate().skip(1) {
        assert_ne!(sample[..4], START_CODE, "sample {i} is not rewritten");
        assert_eq!(to_annex_b(sample, size), *annex_b, "sample {i} holds the units of its Annex B");
    }
    assert_eq!(stored.ptss, reframed.ptss, "stamped alike");
    assert_eq!(stored.flags, reframed.flags, "flagged alike");
    assert_eq!(stored.families, ["h264/avcc"], "told the family, whatever fields are named");
}

#[test]
fn every_other_consumer_takes_annex_b_as_before() {
    let file = fixture_bytes("tiny_h264.mp4");
    let want = &run_demux(file.clone(), 4096)[0];
    for (what, desc) in [("names Annex B first", &EITHER_DESC), ("takes any family", &ANY_DESC)] {
        let got = &run_into(file.clone(), 4096, desc)[0];
        assert_eq!(got.bytes, want.bytes, "a consumer that {what}");
        assert_eq!(got.lens, want.lens, "a consumer that {what}");
        assert!(!got.families.iter().any(|f| f == "h264/avcc"), "a consumer that {what}");
    }
}
