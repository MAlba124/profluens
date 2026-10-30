//! `MkvMuxN` end to end in a real pipeline, plus its loud failure modes.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mkv::{MatroskaReader, MkvError, MkvMux, MkvMuxN, MuxError, mux_multi};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

fn mkv_error(err: &Error) -> &MkvError {
    std::error::Error::source(err)
        .and_then(|s| s.downcast_ref::<MkvError>())
        .unwrap_or_else(|| panic!("expected an element failure carrying MkvError, got {err:?}"))
}

static ANN_FIELDS: [FieldDesc; 5] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "duration", allowed: ConstraintDesc::Any, preferred: None },
];
static SRC_OFFERS: [OfferDesc; 2] = [
    OfferDesc { family: "h264/avcc", fields: &ANN_FIELDS },
    OfferDesc { family: "aac", fields: &ANN_FIELDS },
];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "annsrc",
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

type Frame = (u64, bool, Vec<u8>);

struct AnnSrc {
    family: &'static str,
    fields: Vec<(&'static str, ValueDesc)>,
    head: Vec<u8>,
    frames: Vec<Frame>,
    at: usize,
    head_sent: bool,
}

impl AnnSrc {
    const OUT: SrcPad = SrcPad(0);

    fn new(
        family: &'static str,
        fields: Vec<(&'static str, ValueDesc)>,
        head: Vec<u8>,
        frames: Vec<Frame>,
    ) -> Self {
        Self { family, fields, head, frames, at: 0, head_sent: false }
    }

    fn emit(ctx: &mut Ctx, bytes: &[u8], pts: Option<u64>, flags: BufferFlags) -> bool {
        let Some(mut buf) = ctx.try_alloc() else {
            return false;
        };
        assert!(buf.memory.capacity() >= bytes.len(), "test frame exceeds pool slot");
        buf.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
        buf.memory.set_len(bytes.len());
        if let Some(t) = pts {
            buf.pts = Timestamp::from_nanos(t);
        }
        buf.flags = flags;
        ctx.out(Self::OUT).push(buf);
        true
    }
}

impl Element for AnnSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.at = 0;
        self.head_sent = false;
        ctx.announce_format(Self::OUT, self.family, &self.fields);
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.head_sent {
            if !self.head.is_empty()
                && !Self::emit(ctx, &self.head.clone(), None, BufferFlags::empty())
            {
                return Ok(Flow::Ok); // pool dry, retry
            }
            self.head_sent = true;
            return Ok(Flow::Ok);
        }
        if self.at >= self.frames.len() {
            return Ok(Flow::Eos);
        }
        let (pts, key, bytes) = self.frames[self.at].clone();
        let flags = if key { BufferFlags::KEYFRAME } else { BufferFlags::DELTA };
        if Self::emit(ctx, &bytes, Some(pts), flags) {
            self.at += 1;
        }
        Ok(Flow::Ok)
    }
}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "collectsink",
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

struct CollectSink {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl CollectSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for CollectSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut sink = self.bytes.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            sink.extend_from_slice(buf.memory.data());
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, _ctx: &mut Ctx, event: &profluens_core::event::Event) -> Result<(), Error> {
        if let profluens_core::event::Event::Patch { offset, data } = event {
            let mut sink = self.bytes.lock().unwrap();
            let at = *offset as usize;
            sink[at..at + data.len()].copy_from_slice(data.data());
        }
        Ok(())
    }
}

fn avcc_record() -> Vec<u8> {
    let sps = [0x67u8, 0x42, 0xC0, 0x1E, 0xD9];
    let pps = [0x68u8, 0xCB, 0x83, 0xCB, 0x20];
    let mut rec = vec![1u8, 0x42, 0xC0, 0x1E, 0xFF, 0xE1];
    rec.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    rec.extend_from_slice(&sps);
    rec.push(1);
    rec.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    rec.extend_from_slice(&pps);
    rec
}

fn asc() -> Vec<u8> {
    vec![0x11, 0x90]
}

fn video_frames() -> Vec<Frame> {
    (0..10u64)
        .map(|i| {
            let body = vec![if i % 5 == 0 { 0x65u8 } else { 0x41 }, i as u8, 0xAA, 0xBB];
            let mut f = (body.len() as u32).to_be_bytes().to_vec();
            f.extend_from_slice(&body);
            (i * 40_000_000, i % 5 == 0, f)
        })
        .collect()
}

fn audio_frames() -> Vec<Frame> {
    (0..15u64).map(|i| (10_000_000 + i * 20_000_000, true, vec![0x21, i as u8, 0xCC])).collect()
}

#[test]
fn two_tracks_mux_pts_ordered_bit_exact() {
    let vframes = video_frames();
    let aframes = audio_frames();
    let (dur_v, dur_a) = (400_000_000u64, 300_000_000u64);

    let out = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let v = p.add(AnnSrc::new(
        "h264/avcc",
        vec![
            ("width", ValueDesc::Int(320)),
            ("height", ValueDesc::Int(240)),
            ("duration", ValueDesc::Int(dur_v as i64)),
        ],
        avcc_record(),
        vframes.clone(),
    ));
    let a = p.add(AnnSrc::new(
        "aac",
        vec![
            ("rate", ValueDesc::Int(48_000)),
            ("channels", ValueDesc::Int(2)),
            ("duration", ValueDesc::Int(dur_a as i64)),
        ],
        asc(),
        aframes.clone(),
    ));
    let mux = p.add(MkvMux::multi(2));
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(v.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("v -> mux.sink_0");
    p.link(a.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[1])).expect("a -> mux.sink_1");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    p.run().expect("run");

    let bytes = out.lock().unwrap().clone();
    let mut r = MatroskaReader::new();
    r.push(&bytes).expect("output parses as Matroska");

    let tracks = r.tracks().to_vec();
    assert_eq!(tracks.len(), 2, "two TrackEntries");
    assert_eq!(tracks[0].codec_id, "V_MPEG4/ISO/AVC");
    assert_eq!(tracks[0].track_number, 1, "pad 0 → track 1");
    assert_eq!(tracks[0].codec_private, avcc_record(), "CodecPrivate = the record verbatim");
    assert_eq!((tracks[0].pixel_width, tracks[0].pixel_height), (320, 240));
    assert_eq!(tracks[1].codec_id, "A_AAC");
    assert_eq!(tracks[1].track_number, 2, "pad 1 → track 2");
    assert_eq!(tracks[1].codec_private, asc(), "CodecPrivate = the ASC verbatim");
    assert_eq!(tracks[1].sampling_frequency, 48_000.0);
    assert_eq!(tracks[1].channels, 2);

    assert_eq!(r.duration_ns(), Some(dur_v.max(dur_a)), "duration = max across pads");

    let (mut vi, mut ai) = (0usize, 0usize);
    let mut last_pts = 0u64;
    let mut blocks = 0usize;
    while let Some(f) = r.next_frame() {
        assert!(
            blocks == 0 || f.pts_ns > last_pts,
            "block {blocks}: pts {} not ascending after {last_pts}, interleave broken",
            f.pts_ns
        );
        last_pts = f.pts_ns;
        blocks += 1;
        match f.track_number {
            1 => {
                let (pts, key, ref bytes) = vframes[vi];
                assert_eq!(f.data, *bytes, "video frame {vi} bit-exact");
                assert_eq!(f.pts_ns, pts, "video frame {vi} pts");
                assert_eq!(f.keyframe, key, "video frame {vi} keyframe bit");
                vi += 1;
            }
            2 => {
                let (pts, _, ref bytes) = aframes[ai];
                assert_eq!(f.data, *bytes, "audio frame {ai} bit-exact");
                assert_eq!(f.pts_ns, pts, "audio frame {ai} pts");
                assert!(f.keyframe, "audio frame {ai} is a keyframe (untagged/KEYFRAME)");
                ai += 1;
            }
            n => panic!("unexpected track {n}"),
        }
    }
    assert_eq!((vi, ai), (vframes.len(), aframes.len()), "every frame arrived");
}

#[test]
fn single_pad_aac_mux() {
    let aframes = audio_frames();
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let a = p.add(AnnSrc::new(
        "aac",
        vec![("rate", ValueDesc::Int(44_100)), ("channels", ValueDesc::Int(1))],
        asc(),
        aframes.clone(),
    ));
    let mux = p.add(MkvMuxN::new());
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(a.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("a -> mux.sink_0");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    p.run().expect("run");

    let bytes = out.lock().unwrap().clone();
    let mut r = MatroskaReader::new();
    r.push(&bytes).expect("output parses");
    assert_eq!(r.tracks().len(), 1);
    assert_eq!(r.tracks()[0].codec_id, "A_AAC");
    assert_eq!(r.tracks()[0].codec_private, asc());
    assert_eq!(r.tracks()[0].sampling_frequency, 44_100.0);
    let mut i = 0usize;
    while let Some(f) = r.next_frame() {
        assert_eq!(f.data, aframes[i].2, "frame {i} bit-exact");
        i += 1;
    }
    assert_eq!(i, aframes.len());
}

#[test]
fn expected_count_mismatch_is_loud() {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let a = p.add(AnnSrc::new(
        "aac",
        vec![("rate", ValueDesc::Int(48_000)), ("channels", ValueDesc::Int(2))],
        asc(),
        audio_frames(),
    ));
    let mux = p.add(MkvMux::multi(2)); // but only one pad gets linked
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(a.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("a -> mux.sink_0");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    let err = p.run().expect_err("mislinked count must fail");
    assert_eq!(
        mkv_error(&err),
        &MkvError::MuxN(MuxError::LinkedCount { expected: 2, linked: 1 }),
        "names the expected and the linked count"
    );
    assert!(format!("{err}").contains("2 track"), "names the expected count: {err}");
}

#[test]
fn a_multichannel_opus_lane_without_a_head_is_loud() {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let a = p.add(AnnSrc::new(
        "opus",
        vec![("rate", ValueDesc::Int(48_000)), ("channels", ValueDesc::Int(6))],
        Vec::new(),
        vec![(0, true, vec![0xFC, 0xFF, 0xFE]), (20_000_000, true, vec![0xFC, 0xFF, 0xFE])],
    ));
    let mux = p.add(MkvMuxN::new());
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(a.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("a -> mux.sink_0");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    let err = p.run().expect_err("a synthesized head maps at most two channels");
    assert_eq!(mkv_error(&err), &MkvError::MuxN(MuxError::OpusHeadMissing { channels: 6 }));
}

static FLOOD_PADS: [PadDesc; 2] =
    [PadDesc { name: "a", offers: &SRC_OFFERS }, PadDesc { name: "b", offers: &SRC_OFFERS }];
static FLOOD_DESC: ElementDesc = ElementDesc {
    name: "floodsrc",
    sinks: &[],
    srcs: &FLOOD_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct FloodSrc {
    sent: usize,
    total: usize,
    head_sent: bool,
}

impl FloodSrc {
    const A: SrcPad = SrcPad(0);
    const B: SrcPad = SrcPad(1);
}

impl Element for FloodSrc {
    fn desc(&self) -> &'static ElementDesc {
        &FLOOD_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.announce_format(
            Self::A,
            "h264/avcc",
            &[("width", ValueDesc::Int(64)), ("height", ValueDesc::Int(48))],
        );
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.head_sent {
            if !AnnSrc::emit(ctx, &avcc_record(), None, BufferFlags::empty()) {
                return Ok(Flow::Ok);
            }
            self.head_sent = true;
            return Ok(Flow::Ok);
        }
        if self.sent >= self.total {
            return Ok(Flow::Eos);
        }
        let pts = self.sent as u64 * 1_000_000;
        if AnnSrc::emit(ctx, &[0x00], Some(pts), BufferFlags::DELTA) {
            self.sent += 1;
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn silent_linked_pad_is_a_loud_error() {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src =
        p.add(FloodSrc { sent: 0, total: mux_multi::PRECONFIG_MAX_BUFS + 64, head_sent: false });
    let mux = p.add(MkvMuxN::new());
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(src.src(FloodSrc::A), mux.sink(MkvMuxN::INS[0])).expect("a -> mux.sink_0");
    p.link(src.src(FloodSrc::B), mux.sink(MkvMuxN::INS[1])).expect("b -> mux.sink_1");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    // Queued frames pin the source's pool slots, so the pool must exceed the count bound for it to
    // fire first.
    p.set_element_pool(src, 256, (mux_multi::PRECONFIG_MAX_BUFS + 256) as u32);
    let err = p.run().expect_err("silent pad must fail loudly, not hang");
    match mkv_error(&err) {
        MkvError::MuxN(MuxError::PrebufferFull { buffers, waiting, .. }) => {
            assert!(waiting.contains(1) && waiting.len() == 1, "the silent pad is named: {err}");
            assert!(*buffers > mux_multi::PRECONFIG_MAX_BUFS, "the count bound fired: {err}");
        }
        other => panic!("expected PrebufferFull, got {other:?}"),
    }
    let msg = format!("{err}");
    assert!(msg.contains("sink_1"), "the silent pad is named: {msg}");
    assert!(msg.contains("never announced"), "the cause is named: {msg}");
}

/// A lane that ends before its head, here an AAC pad with no ASC, must not hold the header back
/// while the other floods the prebuffer. The header goes without it, and the drop is said.
#[test]
fn a_pad_that_ends_before_its_head_does_not_hold_the_header() {
    let total = mux_multi::PRECONFIG_MAX_BUFS + 64;
    let frames: Vec<Frame> =
        (0..total).map(|i| (i as u64 * 1_000_000, i == 0, vec![0x00, i as u8])).collect();
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let v = p.add(AnnSrc::new(
        "h264/avcc",
        vec![("width", ValueDesc::Int(64)), ("height", ValueDesc::Int(48))],
        avcc_record(),
        frames,
    ));
    let a = p.add(AnnSrc::new(
        "aac",
        vec![("rate", ValueDesc::Int(48_000)), ("channels", ValueDesc::Int(2))],
        Vec::new(),
        Vec::new(),
    ));
    let mux = p.add(MkvMuxN::new());
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(v.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("v -> mux.sink_0");
    p.link(a.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[1])).expect("a -> mux.sink_1");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    // Enough slots for the count bound to fire first, were the header held.
    p.set_element_pool(v, 256, (mux_multi::PRECONFIG_MAX_BUFS + 256) as u32);
    p.run().expect("the header goes without the empty lane");
    let mut dropped = false;
    while let Some(m) = p.bus().try_recv() {
        if let profluens_core::bus::BusMessage::Warning { error, .. } = m {
            dropped |= matches!(mkv_error(&error), MkvError::MuxN(MuxError::TracksDropped { .. }));
        }
    }
    assert!(dropped, "the empty lane is named on the bus");
    let bytes = out.lock().unwrap().clone();
    let mut r = MatroskaReader::new();
    r.push(&bytes).expect("output parses as Matroska");
    let tracks = r.tracks().to_vec();
    assert_eq!(tracks.len(), 1, "the ended lane is no track");
    assert_eq!(tracks[0].codec_id, "V_MPEG4/ISO/AVC");
    let mut n = 0;
    while r.next_frame().is_some() {
        n += 1;
    }
    assert_eq!(n, total, "every video frame went out");
}

#[test]
fn data_before_announcement_is_loud() {
    struct RawSrc {
        sent: bool,
    }
    impl RawSrc {
        const OUT: SrcPad = SrcPad(0);
    }

    impl Element for RawSrc {
        fn desc(&self) -> &'static ElementDesc {
            &SRC_DESC
        }
        fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
            if self.sent {
                return Ok(Flow::Eos);
            }
            if AnnSrc::emit(ctx, &[1, 2, 3], Some(0), BufferFlags::KEYFRAME) {
                self.sent = true;
            }
            Ok(Flow::Ok)
        }
    }

    let out = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(RawSrc { sent: false });
    let mux = p.add(MkvMuxN::new());
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(src.src(RawSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("src -> mux");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    let err = p.run().expect_err("unannounced data must fail");
    assert_eq!(
        mkv_error(&err),
        &MkvError::MuxN(MuxError::DataBeforeFormat),
        "the cause is named: {err:?}"
    );
    assert!(format!("{err}").contains("before any format announcement"), "{err}");
}

struct SparseSrc {
    step: u8,
    over: Arc<std::sync::atomic::AtomicBool>,
}

impl Element for SparseSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        match self.step {
            0 => {
                let fields = [("rate", ValueDesc::Int(48_000)), ("channels", ValueDesc::Int(2))];
                ctx.announce_format(AnnSrc::OUT, "aac", &fields);
                self.step = 1;
            }
            1 if AnnSrc::emit(ctx, &asc(), None, BufferFlags::empty()) => {
                let until = Timestamp::from_secs(10);
                ctx.push_event(AnnSrc::OUT, profluens_core::event::Event::Gap { until });
                self.step = 2;
            }
            _ if self.over.load(std::sync::atomic::Ordering::Acquire) => return Ok(Flow::Eos),
            _ => ctx.wake_within(std::time::Duration::from_millis(2)),
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn a_track_that_says_a_gap_holds_no_cluster_back() {
    let out = Arc::new(Mutex::new(Vec::new()));
    let over = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut p = Pipeline::new();
    let fields = vec![("width", ValueDesc::Int(320)), ("height", ValueDesc::Int(240))];
    let v = p.add(AnnSrc::new("h264/avcc", fields, avcc_record(), video_frames()));
    let s = p.add(SparseSrc { step: 0, over: Arc::clone(&over) });
    let mux = p.add(MkvMux::multi(2));
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(v.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("v -> mux.sink_0");
    p.link(s.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[1])).expect("sparse -> mux.sink_1");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    let run = std::thread::spawn(move || p.run());

    let fifth = [0x41u8, 4, 0xAA, 0xBB];
    let written = |bytes: &[u8]| bytes.windows(fifth.len()).any(|w| w == fifth);
    let began = std::time::Instant::now();
    while !written(&out.lock().unwrap()) {
        assert!(
            began.elapsed() < std::time::Duration::from_secs(10),
            "the first cluster waits on the silent track"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    over.store(true, std::sync::atomic::Ordering::Release);
    run.join().expect("joined").expect("run");
    let bytes = out.lock().unwrap().clone();
    let mut r = MatroskaReader::new();
    r.push(&bytes).expect("output parses as Matroska");
    let ninth = [0x41u8, 9, 0xAA, 0xBB];
    assert!(bytes.windows(ninth.len()).any(|w| w == ninth), "and every picture is in it");
}

#[test]
fn long_gop_streams_through_a_bounded_pool() {
    const FRAME: usize = 60 << 10;
    const FRAMES: u64 = 300;
    let frames: Vec<Frame> = (0..FRAMES)
        .map(|i| {
            let mut f = vec![i as u8; FRAME];
            f[..4].copy_from_slice(&(FRAME as u32 - 4).to_be_bytes());
            (i * 40_000_000, i == 0, f)
        })
        .collect();
    let want = frames.clone();
    let out = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = std::sync::mpsc::channel();
    let sink_out = Arc::clone(&out);
    std::thread::spawn(move || {
        let mut p = Pipeline::new();
        let v = p.add(AnnSrc::new(
            "h264/avcc",
            vec![("width", ValueDesc::Int(320)), ("height", ValueDesc::Int(240))],
            avcc_record(),
            frames,
        ));
        let mux = p.add(MkvMuxN::new());
        let sink = p.add(CollectSink { bytes: sink_out });
        p.link(v.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("v -> mux.sink_0");
        p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
        p.set_element_pool(v, 64 << 10, 128);
        let _ = tx.send(p.run());
    });
    rx.recv_timeout(std::time::Duration::from_secs(30))
        .expect("the mux finishes, a GOP held past the pool stalled it")
        .expect("run");

    let bytes = out.lock().unwrap().clone();
    let mut r = MatroskaReader::new();
    r.push(&bytes).expect("output parses as Matroska");
    let mut n = 0usize;
    while let Some(f) = r.next_frame() {
        let (pts, key, ref data) = want[n];
        assert_eq!((f.pts_ns, f.keyframe), (pts, key), "frame {n}");
        assert!(f.data == *data, "frame {n} bit-exact");
        n += 1;
    }
    assert_eq!(n, want.len(), "every frame arrived");
}

struct PacedSrc {
    inner: AnnSrc,
    pace: std::time::Duration,
    next: Option<std::time::Instant>,
}

impl Element for PacedSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.inner.start(ctx)
    }
    fn process(&mut self, ctx: &mut Ctx, inputs: Inputs<'_>) -> Result<Flow, Error> {
        let now = std::time::Instant::now();
        if let Some(next) = self.next.filter(|n| *n > now) {
            ctx.wake_within(next - now);
            return Ok(Flow::Ok);
        }
        self.next = Some(now + self.pace);
        self.inner.process(ctx, inputs)
    }
}

#[test]
fn a_slow_track_beside_a_fast_one_interleaves_and_the_duration_is_patched() {
    const MS: u64 = 1_000_000;
    let video: Vec<Frame> = (0..75u64)
        .map(|i| {
            let mut f = vec![i as u8; 64];
            f[..4].copy_from_slice(&60u32.to_be_bytes());
            (i * 40 * MS, i % 25 == 0, f)
        })
        .collect();
    let audio: Vec<Frame> = (0..141u64)
        .map(|i| (i * 1024 * 1_000_000_000 / 48_000, true, vec![0x21, i as u8]))
        .collect();
    let last_audio = audio.last().unwrap().0;
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let vfields = vec![("width", ValueDesc::Int(320)), ("height", ValueDesc::Int(240))];
    let v = p.add(PacedSrc {
        inner: AnnSrc::new("h264/avcc", vfields, avcc_record(), video),
        pace: std::time::Duration::from_millis(8),
        next: None,
    });
    let afields = vec![("rate", ValueDesc::Int(48_000)), ("channels", ValueDesc::Int(2))];
    let a = p.add(AnnSrc::new("aac", afields, asc(), audio));
    let mux = p.add(MkvMux::multi(2));
    let sink = p.add(CollectSink { bytes: Arc::clone(&out) });
    p.link(v.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[0])).expect("v -> mux.sink_0");
    p.link(a.src(AnnSrc::OUT), mux.sink(MkvMuxN::INS[1])).expect("a -> mux.sink_1");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(CollectSink::IN)).expect("mux -> sink");
    p.run().expect("run");

    let bytes = out.lock().unwrap().clone();
    let mut r = MatroskaReader::new();
    r.push(&bytes).expect("output parses as Matroska");
    let (mut picture, mut worst, mut n) = (0u64, 0u64, [0usize; 2]);
    while let Some(f) = r.next_frame() {
        n[f.track_number as usize - 1] += 1;
        match f.track_number {
            1 => picture = picture.max(f.pts_ns),
            _ => worst = worst.max(f.pts_ns.saturating_sub(picture)),
        }
    }
    assert_eq!(n, [75, 141], "every frame of both tracks");
    let slack = 200 * MS;
    assert!(
        worst <= mux_multi::LEAD_NS + slack,
        "audio {} ms ahead of the pictures before it",
        worst / MS
    );
    let step = 1024 * 1_000_000_000 / 48_000;
    assert_eq!(r.duration_ns(), Some(last_audio + step), "the last audio frame ends the stream");
}
