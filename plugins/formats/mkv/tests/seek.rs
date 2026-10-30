//! Demux seeks in a real pipeline, driven from inside the source, and each track restarting at its
//! own point.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pf_mkv::ebml::{self, id};
use pf_mkv::{MatroskaWriter, MkvDemux, TrackConfig, parse_cues, parse_seek_head};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::command::Control;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "seeksrc",
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

const PRE_SEEK_SPIN_LIMIT: u32 = 100_000;

struct SeekSrc {
    data: Vec<u8>,
    chunk: usize,
    pos: usize,
    trigger_byte: usize,
    seek_byte: usize,
    seek: Arc<Control>,
    seeked: bool,
    /// Sink buffers recorded before the seek is published. Without the gate a flush can discard
    /// in-flight pre-seek output, making pre-seek assertions a scheduling coin-flip.
    pre_seek_gate: usize,
    seen: Arc<AtomicUsize>,
    spins: u32,
    jump: Jump,
    stamp: Timestamp,
}

#[derive(Clone, Copy)]
enum Jump {
    Byte,
    One { to_time: Timestamp, at: Timestamp },
    Two { to_time: Timestamp, at: Timestamp, demux: ElementId, key: (u64, Timestamp) },
    Unstamped { to_time: Timestamp, at: Timestamp, demux: ElementId, key: (u64, Timestamp) },
}

impl SeekSrc {
    const OUT: SrcPad = SrcPad(0);

    fn send(&self, me: ElementId) {
        let byte = self.seek_byte as u64;
        match self.jump {
            Jump::Byte => {
                let _ = self.seek.seek_bytes(
                    Timestamp::ZERO,
                    &[(profluens_core::command::ALL, byte, Timestamp::ZERO)],
                );
            }
            Jump::One { to_time, at } => {
                let _ = self.seek.seek_bytes(to_time, &[(profluens_core::command::ALL, byte, at)]);
            }
            Jump::Two { to_time, at, demux, key } | Jump::Unstamped { to_time, at, demux, key } => {
                let _ = self.seek.seek_bytes(to_time, &[(me, byte, at), (demux, key.0, key.1)]);
            }
        }
    }
}

impl Element for SeekSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        // Push nothing on the seeking call so no post-seek bytes leak ahead of the FlushStart.
        if !self.seeked && self.pos >= self.trigger_byte {
            if self.seen.load(Ordering::Acquire) < self.pre_seek_gate
                && self.spins < PRE_SEEK_SPIN_LIMIT
            {
                self.spins += 1;
                return Ok(Flow::Ok);
            }
            self.send(ctx.element());
            self.pos = self.seek_byte;
            self.seeked = true;
            return Ok(Flow::Ok);
        }
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok),
        };
        let cap = buf.memory.capacity();
        let limit = if self.pre_seek_gate > 0 && !self.seeked {
            self.trigger_byte.min(self.data.len())
        } else {
            self.data.len()
        };
        let n = self.chunk.min(cap).min(limit.saturating_sub(self.pos));
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        buf.pts = std::mem::replace(&mut self.stamp, Timestamp::NONE);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        let stamps = !matches!(self.jump, Jump::Unstamped { .. });
        if let (Event::FlushStart, true, Some(target)) = (event, stamps, ctx.seek_target()) {
            self.stamp = target.at;
        }
        Ok(())
    }
}

static SINK_OFFERS: [OfferDesc; 3] =
    [OfferDesc::any("vp8"), OfferDesc::any("flac"), OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "recsink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default, Debug)]
struct Recorded {
    bufs: Vec<(Vec<u8>, Option<u64>, bool, bool)>,
    flushes: Vec<usize>,
}

struct RecSink {
    rec: Arc<Mutex<Recorded>>,
    seen: Arc<AtomicUsize>,
}

impl RecSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.bufs.push((
                buf.memory.data().to_vec(),
                buf.pts.nanos(),
                buf.flags.contains(BufferFlags::KEYFRAME),
                buf.flags.contains(BufferFlags::DELTA),
            ));
        }
        // Release-store after the push, so a source that sees the count knows the buffers are in
        // `rec`.
        self.seen.store(rec.bufs.len(), Ordering::Release);
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            let mut rec = self.rec.lock().unwrap();
            let at = rec.bufs.len();
            rec.flushes.push(at);
        }
        Ok(())
    }
}

fn build_cued_video(frames: &[(u64, bool, &[u8])]) -> Vec<u8> {
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240)]);
    w.enable_cues();
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    for &(ts, key, data) in frames {
        w.write_frame(&mut out, 1, ts, data, key).unwrap();
    }
    w.finalize(&mut out);
    out
}

fn header_prefix(stream: &[u8]) -> Vec<u8> {
    let cl = stream.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster");
    stream[..cl].to_vec()
}

fn seek_index(stream: &[u8]) -> Vec<(u64, u64)> {
    let info = parse_seek_head(&header_prefix(stream)).expect("SeekHead");
    let cues_abs = (info.segment_data_start + info.cues_pos.expect("Cues")) as usize;
    let h = ebml::read_element_header(stream, cues_abs).unwrap();
    let cues_bytes = &stream[cues_abs..h.data_end().unwrap().unwrap()];
    parse_cues(cues_bytes, info.segment_data_start, info.timestamp_scale)
}

fn run_seek(stream: Vec<u8>, chunk: usize, trigger_byte: usize, seek_byte: usize) -> Recorded {
    run_seek_gated(stream, chunk, trigger_byte, seek_byte, 0)
}

fn run_seek_gated(
    stream: Vec<u8>,
    chunk: usize,
    trigger_byte: usize,
    seek_byte: usize,
    pre_seek_gate: usize,
) -> Recorded {
    let header = header_prefix(&stream);
    let mut p = Pipeline::new();
    let seek = p.control();
    let seen = Arc::new(AtomicUsize::new(0));
    let src = p.add(SeekSrc {
        data: stream,
        chunk,
        pos: 0,
        trigger_byte,
        seek_byte,
        seek,
        seeked: false,
        pre_seek_gate,
        seen: Arc::clone(&seen),
        spins: 0,
        jump: Jump::Byte,
        stamp: Timestamp::NONE,
    });
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(SeekSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 1, "one video track → one src pad");

    let rec = Arc::new(Mutex::new(Recorded::default()));
    let sink = p.add(RecSink { rec: Arc::clone(&rec), seen });
    p.link(added[0].element.src(added[0].pad), sink.sink(RecSink::IN)).expect("pad -> sink");

    p.run().expect("run");
    Arc::try_unwrap(rec).unwrap().into_inner().unwrap()
}

#[test]
fn seek_to_keyframe_cluster_resyncs_and_regates() {
    let stream = build_cued_video(&[
        (0, true, &[1, 1, 1]),
        (40_000_000, true, &[2, 2]),
        (60_000_000, false, &[3]),
        (80_000_000, true, &[4, 4, 4, 4]),
        (120_000_000, true, &[5, 5]),
    ]);
    let index = seek_index(&stream);
    let (seek_time, seek_byte) = index[2];
    assert_eq!(seek_time, 80_000_000);
    let trigger = index[1].1 as usize; // once we've streamed into cluster #2

    let rec = run_seek(stream, 64, trigger, seek_byte as usize);

    let ptss: Vec<Option<u64>> = rec.bufs.iter().map(|b| b.1).collect();
    let post: Vec<&(Vec<u8>, Option<u64>, bool, bool)> =
        rec.bufs.iter().filter(|b| b.1 == Some(80_000_000) || b.1 == Some(120_000_000)).collect();
    assert_eq!(post.len(), 2, "the two post-seek clusters' frames arrive");
    assert_eq!(post[0].0, vec![4, 4, 4, 4], "80 ms frame bit-exact");
    assert!(post[0].2 && !post[0].3, "first post-seek frame is KEYFRAME, not DELTA");
    assert_eq!(post[1].0, vec![5, 5], "120 ms frame bit-exact");
    assert!(post[1].2, "120 ms frame is a keyframe");

    let key80 = rec.bufs.iter().position(|b| b.1 == Some(80_000_000)).expect("80 ms present");
    assert!(
        !rec.bufs[key80 + 1..].iter().any(|b| b.1 == Some(60_000_000)),
        "no pre-seek 60 ms delta leaks after the resync"
    );
    assert!(ptss.contains(&Some(80_000_000)) && ptss.contains(&Some(120_000_000)));
}

#[test]
fn seek_gate_drops_leading_delta_until_keyframe() {
    // Frame 2 is a delta past the 16-bit tick window, so it opens a cluster with a delta first.
    let stream = build_cued_video(&[
        (0, true, &[1, 1]),
        (40_000_000_000, false, &[2, 2, 2]), // delta, opens cluster #2 (ts overflow)
        (40_040_000_000, true, &[3, 3]),     // keyframe in cluster #2
    ]);
    let first = stream.windows(4).position(|w| w == id::CLUSTER).unwrap();
    let second = first + 4 + stream[first + 4..].windows(4).position(|w| w == id::CLUSTER).unwrap();
    let trigger = first; // seek once we've streamed into cluster #1

    let rec = run_seek(stream, 48, trigger, second);

    let post: Vec<&(Vec<u8>, Option<u64>, bool, bool)> =
        rec.bufs.iter().filter(|b| b.1 == Some(40_040_000_000)).collect();
    assert_eq!(post.len(), 1, "the post-seek keyframe is emitted");
    assert_eq!(post[0].0, vec![3, 3], "keyframe frame bit-exact");
    assert!(post[0].2 && !post[0].3, "it is KEYFRAME");
    let kf = rec.bufs.iter().position(|b| b.1 == Some(40_040_000_000)).unwrap();
    assert!(
        !rec.bufs[..=kf].iter().any(|b| b.1 == Some(40_000_000_000)),
        "the mid-GOP delta was gated out before the keyframe"
    );
}

#[test]
fn seek_into_midblock_recovers_and_gates() {
    let stream = build_cued_video(&[
        (0, true, &[9, 9]),
        (40_000_000, true, &[7, 7, 7]),
        (80_000_000, true, &[8, 8, 8, 8]),
    ]);
    let index = seek_index(&stream);
    let target_cluster = index[2].1 as usize;
    let seek_byte = target_cluster - 6;
    let trigger = index[1].1 as usize;

    let rec = run_seek(stream, 32, trigger, seek_byte);

    let post: Vec<&(Vec<u8>, Option<u64>, bool, bool)> =
        rec.bufs.iter().filter(|b| b.1 == Some(80_000_000)).collect();
    assert_eq!(post.len(), 1, "recovered exactly the 80 ms keyframe frame");
    assert_eq!(post[0].0, vec![8, 8, 8, 8], "80 ms frame bit-exact after mid-block recovery");
    assert!(post[0].2 && !post[0].3, "first post-seek frame is KEYFRAME");
}

#[test]
fn seek_reemits_codec_head() {
    let head: Vec<u8> = {
        let mut h = vec![b'f', b'L', b'a', b'C'];
        h.extend_from_slice(&[0x80, 0, 0, 34]); // last-metadata-block STREAMINFO header
        h.extend_from_slice(&[0xAB; 34]); // 34-byte STREAMINFO body (opaque here)
        h
    };
    let mut w = MatroskaWriter::new(vec![TrackConfig::flac(1, head.clone(), 48_000.0, 2, 16)]);
    w.enable_cues();
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    w.write_frame(&mut stream, 1, 0, &[0x11, 0x22], true).unwrap();
    // An audio anchor splits its Clusters on time, so the second frame sits a span on.
    let second = pf_mkv::writer::AUDIO_CLUSTER_NS;
    w.write_frame(&mut stream, 1, second, &[0x33, 0x44, 0x55], true).unwrap();
    w.finalize(&mut stream);

    let index = seek_index(&stream);
    let (_t, seek_byte) = index[1]; // the second cluster
    let trigger = seek_byte as usize;
    let rec = run_seek_gated(stream, 40, trigger, seek_byte as usize, 2);

    let shape: Vec<String> = rec
        .bufs
        .iter()
        .map(|b| if b.0 == head { "HEAD".into() } else { format!("{:?}@{:?}", b.0, b.1) })
        .collect();
    let head_count = rec.bufs.iter().filter(|b| b.0 == head).count();
    assert!(head_count >= 2, "codec head re-emitted after the seek (saw {head_count}) {shape:?}");
    let pre = rec.bufs.iter().position(|b| b.0 == vec![0x11, 0x22]).expect("pre-seek 0 ms frame");
    assert!(pre > 0 && rec.bufs[pre - 1].0 == head, "head precedes the pre-seek frame {shape:?}");
    let frame_at = rec
        .bufs
        .iter()
        .position(|b| b.1 == Some(second) && b.0 == vec![0x33, 0x44, 0x55])
        .expect("second frame present");
    assert!(
        frame_at > 0 && rec.bufs[frame_at - 1].0 == head,
        "head precedes the post-seek frame {shape:?}"
    );
}

const MS: fn(u64) -> Timestamp = Timestamp::from_millis;

const AAC_HEAD: [u8; 2] = [0x11, 0x90];

fn build_av(pre_roll_ns: u64) -> Vec<u8> {
    let mut audio = TrackConfig::audio(2, "A_AAC", AAC_HEAD.to_vec(), 48_000.0, 2, 0);
    audio.seek_pre_roll_ns = pre_roll_ns;
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240), audio]);
    w.enable_cues();
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    for ms in (0..1_600u64).step_by(20) {
        if ms % 40 == 0 {
            w.write_frame(&mut out, 1, ms * 1_000_000, &[1, (ms / 40) as u8], ms % 400 == 0)
                .unwrap();
        }
        w.write_frame(&mut out, 2, ms * 1_000_000, &[2, (ms / 20) as u8], true).unwrap();
    }
    w.finalize(&mut out);
    out
}

fn run_tracks(
    stream: Vec<u8>,
    seek_byte: u64,
    jump: impl FnOnce(ElementId) -> Jump,
) -> Vec<Vec<u64>> {
    let header = header_prefix(&stream);
    let mut p = Pipeline::new();
    let seek = p.control();
    let demux = p.add(MkvDemux::new(header));
    let src = p.add(SeekSrc {
        trigger_byte: stream.len(),
        data: stream,
        chunk: 96,
        pos: 0,
        seek_byte: seek_byte as usize,
        seek,
        seeked: false,
        pre_seek_gate: 0,
        seen: Arc::new(AtomicUsize::new(0)),
        spins: 0,
        jump: jump(demux),
        stamp: Timestamp::NONE,
    });
    p.link(src.src(SeekSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    let mut recs = Vec::new();
    for pad in &added {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let sink = p.add(RecSink { rec: Arc::clone(&rec), seen: Arc::new(AtomicUsize::new(0)) });
        p.link(pad.element.src(pad.pad), sink.sink(RecSink::IN)).expect("pad -> sink");
        recs.push(rec);
    }
    p.run().expect("run");
    let behind = |rec: &Arc<Mutex<Recorded>>| {
        let rec = rec.lock().unwrap();
        assert_eq!(rec.flushes.len(), 1, "one seek, one flush");
        let frames = rec.bufs[rec.flushes[0]..].iter().filter(|b| b.0 != AAC_HEAD);
        frames.map(|b| b.1.expect("a stamped frame") / 1_000_000).collect::<Vec<u64>>()
    };
    recs.iter().map(behind).collect()
}

fn every(step: usize, from: u64) -> Vec<u64> {
    (from..1_600).step_by(step).collect()
}

#[test]
fn a_source_restarted_a_gop_early_starts_video_at_its_keyframe_and_audio_at_its_line() {
    for (target, first_audio) in [(830, 760), (800, 740)] {
        let stream = build_av(50_000_000);
        let cues = seek_index(&stream);
        assert_eq!(cues.iter().map(|c| c.0 / 1_000_000).collect::<Vec<_>>(), [0, 400, 800, 1_200]);
        let (early, key) = (cues[1], cues[2]);
        let jump = |demux| Jump::Two {
            to_time: MS(target),
            at: Timestamp(early.0),
            demux,
            key: (key.1, Timestamp(key.0)),
        };
        let pads = run_tracks(stream, early.1, jump);
        assert_eq!(pads[0], every(40, 800), "target {target}, video from the keyframe it was told");
        assert_eq!(pads[1], every(20, first_audio), "target {target}, audio from its line");
    }
}

#[test]
fn a_track_that_declares_no_pre_roll_has_its_codecs() {
    let stream = build_av(0);
    let cues = seek_index(&stream);
    let (early, key) = (cues[1], cues[2]);
    let jump = |demux| Jump::Two {
        to_time: MS(830),
        at: Timestamp(early.0),
        demux,
        key: (key.1, Timestamp(key.0)),
    };
    let pads = run_tracks(stream, early.1, jump);
    assert_eq!(pads[0], every(40, 800));
    assert_eq!(pads[1], every(20, 800));
}

#[test]
fn a_source_restarted_at_the_keyframe_starts_video_at_the_first_keyframe_read() {
    let cues = seek_index(&build_av(50_000_000));
    let (early, key) = (cues[1], cues[2]);
    let one = |_| Jump::One { to_time: MS(830), at: Timestamp(early.0) };
    let unstamped = |demux| Jump::Unstamped {
        to_time: MS(830),
        at: Timestamp(early.0),
        demux,
        key: (key.1, Timestamp(key.0)),
    };
    for jump in [Box::new(one) as Box<dyn FnOnce(ElementId) -> Jump>, Box::new(unstamped)] {
        let pads = run_tracks(build_av(50_000_000), early.1, jump);
        assert_eq!(pads[0], every(40, 400), "video from the keyframe at the byte");
        assert_eq!(pads[1], every(20, 760), "audio from its line");
    }
    let guess = |_| Jump::One { to_time: MS(830), at: MS(830) };
    let pads = run_tracks(build_av(50_000_000), key.1 - 6, guess);
    assert_eq!(pads[0], every(40, 800));
    assert_eq!(pads[1], every(20, 800), "nothing at or before the line was read");
}

#[test]
fn a_track_that_ends_before_its_line_hands_on_its_last_packet() {
    let stream = build_av(50_000_000);
    let cues = seek_index(&stream);
    let last = cues[3];
    let pads =
        run_tracks(stream, last.1, |_| Jump::One { to_time: MS(5_000), at: Timestamp(last.0) });
    assert_eq!(pads[0], every(40, 1_200));
    assert_eq!(pads[1], [1_580]);
}

#[test]
fn the_frames_of_a_lace_are_held_and_handed_on_together() {
    let track = TrackConfig::audio(1, "A_AAC", AAC_HEAD.to_vec(), 48_000.0, 2, 0);
    let mut w = MatroskaWriter::new(vec![track]);
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    let cluster_at = stream.len() as u64;
    let mut cluster = Vec::new();
    ebml::write_uint(&mut cluster, id::TIMESTAMP, 0);
    for lace in 0..3u8 {
        let mut block = vec![0x81];
        block.extend_from_slice(&(i16::from(lace) * 80).to_be_bytes());
        block.extend_from_slice(&[0x04, 3]);
        (0..4u8).for_each(|frame| block.extend_from_slice(&[lace, frame]));
        ebml::write_binary(&mut cluster, id::SIMPLE_BLOCK, &block);
    }
    ebml::write_id(&mut stream, id::CLUSTER);
    ebml::write_size(&mut stream, cluster.len() as u64);
    stream.extend_from_slice(&cluster);

    let header = header_prefix(&stream);
    let mut p = Pipeline::new();
    let seek = p.control();
    let src = p.add(SeekSrc {
        trigger_byte: stream.len(),
        data: stream,
        chunk: 96,
        pos: 0,
        seek_byte: cluster_at as usize,
        seek,
        seeked: false,
        pre_seek_gate: 0,
        seen: Arc::new(AtomicUsize::new(0)),
        spins: 0,
        jump: Jump::One { to_time: MS(130), at: Timestamp::ZERO },
        stamp: Timestamp::NONE,
    });
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(SeekSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    let rec = Arc::new(Mutex::new(Recorded::default()));
    let sink = p.add(RecSink { rec: Arc::clone(&rec), seen: Arc::new(AtomicUsize::new(0)) });
    p.link(added[0].element.src(added[0].pad), sink.sink(RecSink::IN)).expect("pad -> sink");
    p.run().expect("run");

    let rec = rec.lock().unwrap();
    let behind: Vec<(Vec<u8>, Option<u64>)> = rec.bufs[rec.flushes[0]..]
        .iter()
        .filter(|b| b.0 != AAC_HEAD)
        .map(|b| (b.0.clone(), b.1.map(|ns| ns / 1_000_000)))
        .collect();
    // Only a lace's first frame is stamped, the rest follow it unstamped.
    let at = |n: u8, frame: u8| (frame == 0).then_some(u64::from(n) * 80);
    let lace = |n: u8| (0..4u8).map(move |frame| (vec![n, frame], at(n, frame)));
    assert_eq!(behind, lace(1).chain(lace(2)).collect::<Vec<_>>());
}
