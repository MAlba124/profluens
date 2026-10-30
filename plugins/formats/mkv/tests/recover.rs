//! `mkvdemux` played pushed and pulled through damage, trailing garbage, truncation, seeks sent
//! before the stream head was read, and Block timing.

// Test fixtures and a test rig, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mkv::MkvDemux;
use pf_mkv::ebml::{self, id};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::ALL;
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::{Pipeline, SeekMode, Serves};
use profluens_core::time::Timestamp;

const MS: u64 = 1_000_000;
const S: u64 = 1_000_000_000;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static SRC: ElementDesc = desc("memsrc", &[], &SRC_PADS);
static SINK: ElementDesc = desc("recsink", &SINK_PADS, &[]);
const OUT: SrcPad = SrcPad(0);

/// Pushes `chunk` octets a pass, or serves ranges when pulled.
struct MemSrc {
    file: Arc<[u8]>,
    pull: bool,
    pos: usize,
    stamp: Timestamp,
    chunk: usize,
}

impl Element for MemSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        if self.pull {
            ctx.offer_ranges(OUT);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if ctx.pulled(OUT) {
            while let Some(ask) = ctx.next_ask(OUT) {
                let profluens_core::pull::Ask { offset, len, tag, mut buf } = ask;
                let from = offset.min(self.file.len() as u64) as usize;
                let n = (len as usize).min(self.file.len() - from).min(buf.memory.capacity());
                buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[from..from + n]);
                buf.memory.set_len(n);
                ctx.serve(OUT, tag, buf);
            }
            return Ok(if ctx.asks_over(OUT) { Flow::Eos } else { Flow::Ok });
        }
        if self.pos >= self.file.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let n = self.chunk.min(buf.memory.capacity()).min(self.file.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        buf.pts = std::mem::replace(&mut self.stamp, Timestamp::NONE);
        ctx.out(OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if !matches!(event, Event::FlushStart) || ctx.pulled(OUT) {
            return Ok(());
        }
        let t = ctx.seek_target().unwrap_or(SeekTarget { to_byte: 0, at: Timestamp::ZERO });
        self.pos = t.to_byte.min(self.file.len() as u64) as usize;
        self.stamp = t.at;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Rec {
    pts: Option<u64>,
    dur: Option<u64>,
    key: bool,
    first: u8,
    head: Vec<u8>,
}

struct RecSink {
    out: Arc<Mutex<Vec<Rec>>>,
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            if buf.flags.contains(BufferFlags::HEADER) {
                continue;
            }
            self.out.lock().unwrap().push(Rec {
                pts: buf.pts.nanos(),
                dur: buf.duration.nanos(),
                key: buf.flags.contains(BufferFlags::KEYFRAME),
                first: buf.memory.data().first().copied().unwrap_or(0),
                head: buf.memory.data()[..buf.memory.len().min(4)].to_vec(),
            });
        }
        Ok(Flow::Ok)
    }
}

enum Seek {
    None,
    Bytes { to_byte: u64, at: Timestamp },
    Time { to: Timestamp, mode: SeekMode },
}

struct Ran {
    frames: Vec<Rec>,
    failed: bool,
    warnings: Vec<String>,
}

impl Ran {
    fn pts(&self) -> Vec<Option<u64>> {
        self.frames.iter().map(|f| f.pts).collect()
    }
}

/// Plays `file` through mkvdemux with the seek sent before the first pass.
fn play(file: &[u8], pull: bool, chunk: usize, seek: Seek) -> Ran {
    let cl = file.windows(4).position(|w| w == id::CLUSTER).unwrap_or(file.len());
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    let src = p.add(MemSrc { file: Arc::from(file), pull, pos: 0, stamp: Timestamp::NONE, chunk });
    let demux = p.add(MkvDemux::new(file[..cl].to_vec()));
    p.set_element_pool(src, 1 << 20, 8);
    p.set_element_pool_bytes(demux, 64 << 10, 64, 64 << 20);
    p.link(src.src(OUT), demux.sink(MkvDemux::IN)).unwrap();
    let added = p.preroll().unwrap();
    let out = Arc::new(Mutex::new(Vec::new()));
    for a in &added.srcs {
        let sink = p.add(RecSink { out: Arc::clone(&out) });
        p.link(demux.src(a.pad), sink.sink(SinkPad(0))).unwrap();
    }
    let mut run = p.step(Arc::clone(&shared)).unwrap();
    let control = p.control();
    match seek {
        Seek::Bytes { to_byte, at } => {
            control.seek_bytes(at, &[(ALL, to_byte, at)]).unwrap();
        }
        Seek::Time { to, mode } => {
            control.seek(to, mode).unwrap();
        }
        Seek::None => {}
    }
    let mut failed = false;
    for passes in 0u64.. {
        assert!(passes < 2_000_000, "the run did not end");
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        if ready.is_empty() {
            match run.next_deadline() {
                Some(at) if at > clock.now() => clock.advance(Timestamp(at.0 - clock.now().0)),
                Some(_) => {}
                None => break,
            }
            continue;
        }
        if run.pass(ready[passes as usize % ready.len()]).is_err() {
            failed = true;
            break;
        }
    }
    let _ = run.finish();
    let mut warnings = Vec::new();
    while let Some(m) = p.bus().try_recv() {
        match m {
            BusMessage::Error { error, .. } => panic!("an error on the bus, {error}"),
            BusMessage::Warning { error, .. } => warnings.push(error.to_string()),
            _ => {}
        }
    }
    drop(p);
    let frames = Arc::try_unwrap(out).unwrap().into_inner().unwrap();
    Ran { frames, failed, warnings }
}

fn el(out: &mut Vec<u8>, eid: &[u8], body: &[u8]) {
    ebml::write_id(out, eid);
    ebml::write_size(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn uint(eid: &[u8], v: u64) -> Vec<u8> {
    let mut o = Vec::new();
    ebml::write_uint(&mut o, eid, v);
    o
}

fn text(eid: &[u8], v: &str) -> Vec<u8> {
    let mut o = Vec::new();
    ebml::write_string(&mut o, eid, v);
    o
}

fn master(eid: &[u8], kids: &[Vec<u8>]) -> Vec<u8> {
    let mut o = Vec::new();
    el(&mut o, eid, &kids.concat());
    o
}

fn simple_block(track: u8, rel: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend_from_slice(&rel.to_be_bytes());
    b.push(flags);
    b.extend_from_slice(payload);
    let mut o = Vec::new();
    el(&mut o, id::SIMPLE_BLOCK, &b);
    o
}

fn cluster(tick: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut kids = vec![uint(id::TIMESTAMP, tick)];
    kids.extend(blocks.iter().cloned());
    master(id::CLUSTER, &kids)
}

fn vp8_track() -> Vec<u8> {
    master(
        id::TRACK_ENTRY,
        &[
            uint(id::TRACK_NUMBER, 1),
            uint(id::TRACK_TYPE, 1),
            text(id::CODEC_ID, "V_VP8"),
            master(id::VIDEO, &[uint(id::PIXEL_WIDTH, 64), uint(id::PIXEL_HEIGHT, 48)]),
        ],
    )
}

/// EBML header, a Segment, Info and Tracks, then the Clusters. A `duration` in ticks sizes the
/// Segment so a time seek has a byte estimate.
fn file(scale: u64, duration: Option<f64>, tracks: &[Vec<u8>], clusters: &[Vec<u8>]) -> Vec<u8> {
    let mut f = master(id::EBML, &[text(id::DOC_TYPE, "matroska"), uint(id::DOC_TYPE_VERSION, 4)]);
    let mut info = uint(id::TIMESTAMP_SCALE, scale);
    if let Some(d) = duration {
        ebml::write_f64(&mut info, id::DURATION, d);
    }
    let mut body = master(id::INFO, &[info]);
    body.extend(master(id::TRACKS, tracks));
    clusters.iter().for_each(|c| body.extend_from_slice(c));
    ebml::write_id(&mut f, id::SEGMENT);
    match duration {
        Some(_) => ebml::write_size(&mut f, body.len() as u64),
        None => ebml::write_unknown_size(&mut f),
    }
    f.extend(body);
    f
}

/// Clusters of three 40 octet VP8 frames 10 ticks apart, the first a keyframe. A frame's octets
/// are its Cluster's index.
fn vp8_clusters(ticks_per_cluster: u64, count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|c| {
            let blocks: Vec<Vec<u8>> = (0..3)
                .map(|k| simple_block(1, k * 10, if k == 0 { 0x80 } else { 0 }, &[c as u8; 40]))
                .collect();
            cluster(c as u64 * ticks_per_cluster, &blocks)
        })
        .collect()
}

fn vp8_file(scale: u64, ticks_per_cluster: u64, count: usize) -> Vec<u8> {
    file(scale, None, &[vp8_track()], &vp8_clusters(ticks_per_cluster, count))
}

fn cluster_offsets(f: &[u8]) -> Vec<usize> {
    f.windows(4).enumerate().filter(|(_, w)| *w == id::CLUSTER).map(|(i, _)| i).collect()
}

/// Every mode, pushed in small chunks and in one push, and pulled.
const MODES: [(bool, usize); 3] = [(false, 64), (false, 1 << 20), (true, 0)];

fn clusters_seen(ran: &Ran) -> Vec<u8> {
    let mut seen: Vec<u8> = ran.frames.iter().map(|f| f.first).collect();
    seen.dedup();
    seen
}

#[test]
fn a_junk_octet_in_one_cluster_costs_that_cluster_alone() {
    let mut f = vp8_file(MS, 1000, 4);
    let at = cluster_offsets(&f);
    // A 0x00 inside the second Cluster's Timestamp, so it reads as a VINT wider than 8 octets.
    f.insert(at[1] + 4 + 1 + 3, 0x00);
    for (pull, chunk) in MODES {
        let ran = play(&f, pull, chunk, Seek::None);
        let how = format!("pull {pull} chunk {chunk}");
        assert!(!ran.failed, "{how}: the run failed");
        let seen = clusters_seen(&ran);
        assert_eq!(seen.first(), Some(&0), "{how}: {seen:?}");
        assert!(seen.ends_with(&[2, 3]), "{how}: the clusters past the damage play, {seen:?}");
        let tail: Vec<_> = ran.frames.iter().filter(|r| r.first >= 2).map(|r| r.pts).collect();
        let want: Vec<_> = [2000, 2010, 2020, 3000, 3010, 3020].map(|t| Some(t * MS)).into();
        assert_eq!(tail, want, "{how}");
        assert!(ran.warnings.iter().any(|w| w.contains("damaged")), "{how}: {:?}", ran.warnings);
    }
}

#[test]
fn trailing_zeros_end_the_stream_like_its_end() {
    let mut f = vp8_file(MS, 1000, 4);
    f.extend(std::iter::repeat_n(0u8, 64));
    for (pull, chunk) in MODES {
        let ran = play(&f, pull, chunk, Seek::None);
        assert!(!ran.failed, "pull {pull} chunk {chunk}: the run failed");
        assert_eq!(ran.frames.len(), 12, "pull {pull} chunk {chunk}");
    }
}

#[test]
fn a_file_cut_inside_a_cluster_plays_up_to_the_cut() {
    let f = vp8_file(MS, 1000, 4);
    let at = cluster_offsets(&f);
    let cut = &f[..at[3] + 30];
    for (pull, chunk) in MODES {
        let ran = play(cut, pull, chunk, Seek::None);
        assert!(!ran.failed, "pull {pull} chunk {chunk}: the run failed");
        assert_eq!(clusters_seen(&ran), [0, 1, 2], "pull {pull} chunk {chunk}");
        assert_eq!(ran.frames.len(), 9, "pull {pull} chunk {chunk}");
    }
}

#[test]
fn a_bad_lace_costs_its_block_alone() {
    let mut clusters = vp8_clusters(1000, 4);
    let blocks: Vec<Vec<u8>> = (0..3)
        .map(|k| match k {
            // Xiph lacing of three frames whose sizes run past the block.
            1 => simple_block(1, 10, 0x02, &[2, 0xFF, 0xFF, 0x10, 1, 2]),
            _ => simple_block(1, k * 10, if k == 0 { 0x80 } else { 0 }, &[1; 40]),
        })
        .collect();
    clusters[1] = cluster(1000, &blocks);
    let f = file(MS, None, &[vp8_track()], &clusters);
    for (pull, chunk) in MODES {
        let ran = play(&f, pull, chunk, Seek::None);
        assert!(!ran.failed, "pull {pull} chunk {chunk}: the run failed");
        let pts = ran.pts();
        assert_eq!(pts.len(), 11, "pull {pull} chunk {chunk}: {pts:?}");
        assert!(!pts.contains(&Some(1010 * MS)), "pull {pull} chunk {chunk}: {pts:?}");
        assert!(ran.warnings.iter().any(|w| w.contains("damaged")), "{:?}", ran.warnings);
    }
}

/// TimestampScale 100 us and a Cluster every 2 s, the seek sent before the head was read.
#[test]
fn a_seek_before_the_head_was_read_stamps_by_the_file_scale() {
    let f = vp8_file(100_000, 20_000, 4);
    let at = cluster_offsets(&f);
    for (pull, chunk) in MODES {
        let how = format!("pull {pull} chunk {chunk}");
        let exact = Seek::Bytes { to_byte: at[2] as u64, at: Timestamp(4 * S) };
        let ran = play(&f, pull, chunk, exact);
        assert!(!ran.failed, "{how}: the run failed");
        let want: Vec<_> = [4000, 4001, 4002, 6000, 6001, 6002].map(|t| Some(t * MS)).into();
        assert_eq!(ran.pts(), want, "{how}");
        // An estimated byte inside the Cluster before resyncs to the next.
        let inside = Seek::Bytes { to_byte: at[2] as u64 - 20, at: Timestamp(3 * S) };
        let ran = play(&f, pull, chunk, inside);
        assert!(!ran.failed, "{how}: the run failed");
        assert_eq!(ran.pts(), want, "{how}: from a byte inside a Cluster");
    }
}

/// A sized Segment with a Duration and no Cues, so the byte is estimated. The seek coalesces
/// with the start in pull mode.
#[test]
fn a_time_seek_sent_before_the_first_pass_plays_from_a_cluster() {
    let f = file(MS, Some(12_000.0), &[vp8_track()], &vp8_clusters(2000, 6));
    for (pull, chunk) in MODES {
        let how = format!("pull {pull} chunk {chunk}");
        let ran =
            play(&f, pull, chunk, Seek::Time { to: Timestamp(7 * S), mode: SeekMode::Keyframe });
        assert!(!ran.failed, "{how}: the run failed");
        let first = ran.frames.first().unwrap_or_else(|| panic!("{how}: nothing played"));
        assert!(first.key && first.pts.is_some_and(|t| t >= 4 * S), "{how}: {:?}", ran.pts());
        assert_eq!(ran.frames.last().and_then(|f| f.pts), Some(10_020 * MS), "{how}");
    }
}

fn opus_track() -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
    let mut private = Vec::new();
    el(&mut private, id::CODEC_PRIVATE, &head);
    master(
        id::TRACK_ENTRY,
        &[
            uint(id::TRACK_NUMBER, 1),
            uint(id::TRACK_TYPE, 2),
            text(id::CODEC_ID, "A_OPUS"),
            private,
            master(id::AUDIO, &[uint(id::CHANNELS, 2)]),
        ],
    )
}

/// The last Block of an ffmpeg Opus WebM, a BlockGroup carrying DiscardPadding.
fn padded_group(rel: i16, packet: &[u8], discard: &[u8]) -> Vec<u8> {
    let mut block = vec![0x81];
    block.extend_from_slice(&rel.to_be_bytes());
    block.push(0);
    block.extend_from_slice(packet);
    let mut b = Vec::new();
    el(&mut b, id::BLOCK, &block);
    b.extend(uint(id::BLOCK_DURATION, 7));
    el(&mut b, id::DISCARD_PADDING, discard);
    master(id::BLOCK_GROUP, &[b])
}

#[test]
fn discard_padding_shortens_the_last_opus_packet() {
    // TOC config 31 code 0, one 20 ms CELT frame.
    let packet = [0xF8, 0xFF, 0xFE];
    let mut blocks: Vec<Vec<u8>> = (0..3).map(|k| simple_block(1, k * 20, 0x80, &packet)).collect();
    blocks.push(padded_group(60, &packet, &[0x00, 0xCD, 0xFE, 0x60]));
    // A negative DiscardPadding pads the start, not cut at the end.
    blocks.push(padded_group(80, &packet, &[0xFF, 0x0B, 0xDC]));
    let f = file(MS, None, &[opus_track()], &[cluster(0, &blocks)]);
    for (pull, chunk) in MODES {
        let ran = play(&f, pull, chunk, Seek::None);
        assert!(!ran.failed, "pull {pull} chunk {chunk}: the run failed");
        let durations: Vec<_> = ran.frames.iter().map(|r| r.dur).collect();
        let want = [20_000, 20_000, 20_000, 6_500, 20_000].map(|us| Some(us * 1_000));
        assert_eq!(durations, want, "pull {pull} chunk {chunk}");
    }
}

#[test]
fn a_lace_without_default_duration_stamps_its_first_frame_and_splits_the_block_duration() {
    let track = master(
        id::TRACK_ENTRY,
        &[uint(id::TRACK_NUMBER, 1), uint(id::TRACK_TYPE, 2), text(id::CODEC_ID, "A_VORBIS")],
    );
    // Xiph lacing of three 3 octet frames, the Block lasting 60 ms.
    let mut laced = vec![0x81, 0, 0, 0x02, 2, 3, 3];
    laced.extend_from_slice(&[1, 1, 1, 2, 2, 2, 3, 3, 3]);
    let mut block = Vec::new();
    el(&mut block, id::BLOCK, &laced);
    block.extend(uint(id::BLOCK_DURATION, 60));
    let after = simple_block(1, 100, 0x80, &[4; 3]);
    let f = file(MS, None, &[track], &[cluster(0, &[master(id::BLOCK_GROUP, &[block]), after])]);
    let mut r = pf_mkv::MatroskaReader::new();
    r.push(&f).expect("the file reads");
    let frames: Vec<_> =
        std::iter::from_fn(|| r.next_frame()).map(|f| (f.pts_ns, f.duration_ns)).collect();
    let none = pf_mkv::PTS_NONE;
    let twenty = Some(20 * MS);
    assert_eq!(frames, [(0, twenty), (none, twenty), (none, twenty), (100 * MS, None)]);
    for (pull, chunk) in MODES {
        let ran = play(&f, pull, chunk, Seek::None);
        let got: Vec<_> = ran.frames.iter().map(|r| (r.pts, r.dur, r.first)).collect();
        let want =
            [(Some(0), twenty, 1), (None, twenty, 2), (None, twenty, 3), (Some(100 * MS), None, 4)];
        assert_eq!(got, want, "pull {pull} chunk {chunk}");
    }
}

/// Opus at 20 ms a packet for 3 s, a keyframe and so a Cluster and a CuePoint each second.
fn cued_opus() -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
    let track = pf_mkv::TrackConfig::audio(1, "A_OPUS", head, 48_000.0, 2, 0);
    let mut w = pf_mkv::MatroskaWriter::new(vec![track]);
    w.enable_cues();
    w.set_duration_ns(3 * S);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    for k in 0..150u64 {
        let packet = [0xF8, (k % 50) as u8, (k / 50) as u8];
        w.write_frame(&mut out, 1, k * 20 * MS, &packet, k % 50 == 0).unwrap();
    }
    w.finalize(&mut out);
    out
}

/// A pulled seek holds the packet at the pre-roll line, copied out of its window.
#[test]
fn a_pulled_audio_seek_hands_on_the_packet_at_its_line() {
    let f = cued_opus();
    let ran = play(&f, true, 0, Seek::Time { to: Timestamp(1_500 * MS), mode: SeekMode::Accurate });
    assert!(!ran.failed);
    // The packet at the line, the point less the Opus pre-roll, then every one after it.
    let first = ran.frames[0].pts.expect("stamped") / (20 * MS);
    assert!((51..=71).contains(&first), "the held packet {first} is past 1 s and the line");
    let want: Vec<_> = (first..150).map(|k| Some(k * 20 * MS)).collect();
    assert_eq!(ran.pts(), want);
    for (k, rec) in (first..).zip(&ran.frames) {
        assert_eq!(rec.head, [0xF8, (k % 50) as u8, (k / 50) as u8], "packet {k} intact");
    }
}

fn binary(eid: &[u8], body: &[u8]) -> Vec<u8> {
    let mut o = Vec::new();
    el(&mut o, eid, body);
    o
}

/// A SeekHead naming `target` at Segment Position `pos`, the position always 8 octets.
fn seek_head(target: &[u8], pos: u64) -> Vec<u8> {
    let entry = master(
        id::SEEK,
        &[binary(id::SEEK_ID, target), binary(id::SEEK_POSITION, &pos.to_be_bytes())],
    );
    master(id::SEEK_HEAD, &[entry])
}

/// Its front SeekHead names only a second one at the end, which names the Cues. The first
/// Cluster is most of the file, so a byte estimate lands in it.
fn hopped_file() -> Vec<u8> {
    let mut clusters = vp8_clusters(2000, 6);
    let blocks: Vec<Vec<u8>> = (0..3)
        .map(|k| simple_block(1, k * 10, if k == 0 { 0x80 } else { 0 }, &[0; 4000]))
        .collect();
    clusters[0] = cluster(0, &blocks);
    let mut info = uint(id::TIMESTAMP_SCALE, MS);
    ebml::write_f64(&mut info, id::DURATION, 12_000.0);
    let head_len = seek_head(id::SEEK_HEAD, 0).len();
    let mut body = vec![0u8; head_len];
    body.extend(master(id::INFO, &[info]));
    body.extend(master(id::TRACKS, &[vp8_track()]));
    let mut points = Vec::new();
    for (c, bytes) in clusters.iter().enumerate() {
        let at = binary(id::CUE_CLUSTER_POSITION, &(body.len() as u64).to_be_bytes());
        let track = master(id::CUE_TRACK_POSITIONS, &[uint(id::CUE_TRACK, 1), at]);
        points.push(master(id::CUE_POINT, &[uint(id::CUE_TIME, c as u64 * 2000), track]));
        body.extend_from_slice(bytes);
    }
    let cues_at = body.len() as u64;
    body.extend(master(id::CUES, &points));
    let second = body.len() as u64;
    body.extend(seek_head(id::CUES, cues_at));
    body.splice(..head_len, seek_head(id::SEEK_HEAD, second));
    let mut f = master(id::EBML, &[text(id::DOC_TYPE, "matroska"), uint(id::DOC_TYPE_VERSION, 4)]);
    ebml::write_id(&mut f, id::SEGMENT);
    ebml::write_size(&mut f, body.len() as u64);
    f.extend(body);
    f
}

#[test]
fn a_pulled_seek_finds_the_cues_through_a_second_seek_head() {
    let f = hopped_file();
    let info = pf_mkv::parse_seek_head(&f[..cluster_offsets(&f)[0]]).expect("a Segment");
    assert_eq!((info.cues_pos, info.next_head.is_some()), (None, true));
    let ran = play(&f, true, 0, Seek::Time { to: Timestamp(9 * S), mode: SeekMode::Keyframe });
    assert!(!ran.failed);
    let want: Vec<_> = [8000, 8010, 8020, 10_000, 10_010, 10_020].map(|t| Some(t * MS)).into();
    assert_eq!(ran.pts(), want, "the Cluster the Cues place at 8 s");
}

/// Unknown-size Clusters have no end to hold a child to, the sized Segment does.
#[test]
fn a_pulled_walk_takes_a_size_past_the_segment_for_damage() {
    let clusters: Vec<Vec<u8>> = vp8_clusters(1000, 4)
        .iter()
        .enumerate()
        .map(|(c, sized)| {
            let h = ebml::read_element_header(sized, 0).unwrap();
            let mut open = id::CLUSTER.to_vec();
            ebml::write_unknown_size(&mut open);
            if c == 1 {
                // A Void claiming 256 MiB, its size damaged.
                open.extend_from_slice(&[0xEC, 0x1F, 0xFF, 0xFF, 0xFE]);
            }
            open.extend_from_slice(&sized[h.data_start..]);
            open
        })
        .collect();
    let f = file(MS, Some(4000.0), &[vp8_track()], &clusters);
    let ran = play(&f, true, 0, Seek::None);
    assert!(!ran.failed);
    assert_eq!(clusters_seen(&ran), [0, 2, 3], "the walk resyncs at the next Cluster");
    assert!(ran.warnings.iter().any(|w| w.contains("past its Segment")), "{:?}", ran.warnings);
}
