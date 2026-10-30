// Tests own their fixtures and staging Vecs.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pf_mkv::config::{NalCodec, ParamSets};
use pf_mkv::{MatroskaReader, MkvMux, MkvMuxN};
use pf_mp4::Mp4Reader;
use pf_mp4::mux::{Mode, Mp4Mux};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

static FIELDS: [FieldDesc; 5] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "delay", allowed: ConstraintDesc::Any, preferred: None },
];
static SRC_OFFERS: [OfferDesc; 3] = [
    OfferDesc { family: "h264/annexb", fields: &FIELDS },
    OfferDesc { family: "aac", fields: &FIELDS },
    OfferDesc::any("subtitle/srt"),
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

#[derive(Clone)]
struct Frame {
    pts: u64,
    dur: u64,
    key: bool,
    /// Flagged `HEADER`, a codec head sent again mid-stream.
    head: bool,
    bytes: Vec<u8>,
}

struct Src {
    family: &'static str,
    fields: Vec<(&'static str, ValueDesc)>,
    head: Vec<u8>,
    frames: Vec<Frame>,
    at: usize,
    head_sent: bool,
    stall: Option<(usize, Arc<AtomicBool>)>,
    done: Option<Arc<AtomicBool>>,
    watch: Option<(usize, Arc<AtomicUsize>, Arc<AtomicUsize>)>,
}

impl Src {
    const OUT: SrcPad = SrcPad(0);

    fn new(
        family: &'static str,
        fields: Vec<(&'static str, ValueDesc)>,
        head: Vec<u8>,
        frames: Vec<Frame>,
    ) -> Self {
        Self {
            family,
            fields,
            head,
            frames,
            at: 0,
            head_sent: false,
            stall: None,
            done: None,
            watch: None,
        }
    }

    fn emit(ctx: &mut Ctx, bytes: &[u8], f: Option<&Frame>) -> bool {
        let Some(mut buf) = ctx.try_alloc() else {
            return false;
        };
        buf.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
        buf.memory.set_len(bytes.len());
        if let Some(f) = f {
            buf.pts = Timestamp::from_nanos(f.pts);
            if f.dur != 0 {
                buf.duration = Timestamp::from_nanos(f.dur);
            }
            buf.flags = if f.key { BufferFlags::KEYFRAME } else { BufferFlags::DELTA };
            if f.head {
                buf.flags |= BufferFlags::HEADER;
            }
        }
        ctx.out(Self::OUT).push(buf);
        true
    }
}

impl Element for Src {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.announce_format(Self::OUT, self.family, &self.fields);
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.head_sent {
            if !self.head.is_empty() && !Self::emit(ctx, &self.head.clone(), None) {
                ctx.wake_within(Duration::from_millis(1));
                return Ok(Flow::Ok);
            }
            self.head_sent = true;
        }
        if self.at >= self.frames.len() {
            if let Some(d) = &self.done {
                d.store(true, Ordering::Release);
            }
            return Ok(Flow::Eos);
        }
        if let Some((i, flag)) = &self.stall
            && self.at == *i
            && !flag.load(Ordering::Acquire)
        {
            ctx.wake_within(Duration::from_millis(1));
            return Ok(Flow::Ok);
        }
        if let Some((i, sink, seen)) = &self.watch
            && self.at == *i
        {
            seen.store(sink.load(Ordering::Acquire), Ordering::Release);
        }
        let f = self.frames[self.at].clone();
        if Self::emit(ctx, &f.bytes, Some(&f)) {
            self.at += 1;
        } else {
            ctx.wake_within(Duration::from_millis(1));
        }
        Ok(Flow::Ok)
    }
}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "collect",
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

struct Collect {
    bytes: Arc<Mutex<Vec<u8>>>,
    count: Arc<AtomicUsize>,
}

impl Collect {
    const IN: SinkPad = SinkPad(0);
}

impl Element for Collect {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut b = self.bytes.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            b.extend_from_slice(buf.memory.data());
        }
        self.count.store(b.len(), Ordering::Release);
        Ok(Flow::Ok)
    }
    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::Patch { offset, data } = event {
            let mut b = self.bytes.lock().unwrap();
            let at = *offset as usize;
            b[at..at + data.len()].copy_from_slice(data.data());
        }
        Ok(())
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn sets() -> Vec<u8> {
    let rec = hex("0142c00affe100176742c00ad9020db0110000030001000003001e0f12264801000568cb83cb20");
    pf_mkv::nal_head_from_config(&rec, false).unwrap().0
}

fn picture(k: usize, key: bool) -> Vec<u8> {
    let mut b = vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }];
    b.extend((0..60 + k % 40).map(|i| (i * 7 + k) as u8 | 1));
    b
}

fn length_prefixed(p: &[u8]) -> Vec<u8> {
    let mut out = ((p.len() - 4) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&p[4..]);
    out
}

fn video(n: usize) -> Vec<Frame> {
    let order = |k: usize| -> usize {
        match k % 3 {
            0 => k,
            1 => k + 2,
            _ => k - 1,
        }
    };
    (0..n)
        .map(|k| {
            let shown = if k + 2 >= n { k } else { order(k) };
            let key = k % 25 == 0;
            Frame {
                pts: shown as u64 * 40_000_000,
                dur: 0,
                key,
                head: false,
                bytes: picture(k, key),
            }
        })
        .collect()
}

fn audio(n: usize) -> Vec<Frame> {
    (0..n)
        .map(|k| {
            let ms = ((k * 1024) as f64 / 48.0).round() as u64;
            Frame {
                pts: ms * 1_000_000,
                dur: 0,
                key: true,
                head: false,
                bytes: vec![0x21, k as u8, 0x5A, 0xA5, (k >> 8) as u8],
            }
        })
        .collect()
}

fn cue(pts_ms: u64, dur_ms: u64, text: &str) -> Frame {
    Frame {
        pts: pts_ms * 1_000_000,
        dur: dur_ms * 1_000_000,
        key: true,
        head: false,
        bytes: text.as_bytes().to_vec(),
    }
}

fn video_src(frames: Vec<Frame>) -> Src {
    Src::new(
        "h264/annexb",
        vec![("width", ValueDesc::Int(128)), ("height", ValueDesc::Int(96))],
        sets(),
        frames,
    )
}

fn audio_src(frames: Vec<Frame>) -> Src {
    let fields = vec![
        ("rate", ValueDesc::Int(48_000)),
        ("channels", ValueDesc::Int(2)),
        ("delay", ValueDesc::Int(21_333_333)),
    ];
    Src::new("aac", fields, vec![0x11, 0x90], frames)
}

fn text_src(frames: Vec<Frame>) -> Src {
    Src::new("subtitle/srt", Vec::new(), Vec::new(), frames)
}

fn run(
    srcs: Vec<Src>,
    mux: Box<dyn Element>,
    ins: [SinkPad; 8],
    out: SrcPad,
    pool: Option<u32>,
) -> (Vec<u8>, Arc<AtomicUsize>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let mut p = Pipeline::new();
    let m = p.add_boxed(mux);
    let sink = p.add(Collect { bytes: Arc::clone(&bytes), count: Arc::clone(&count) });
    for (i, s) in srcs.into_iter().enumerate() {
        let video = s.family == "h264/annexb";
        let e = p.add(s);
        if video && let Some(slots) = pool {
            p.set_element_pool(e, 4096, slots);
        }
        p.link(e.src(Src::OUT), m.sink(ins[i])).expect("src -> mux");
    }
    p.link(m.src(out), sink.sink(Collect::IN)).expect("mux -> sink");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(p.run().map(|_| ()).map_err(|e| format!("{e:?}")));
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(r) => r.expect("the mux runs"),
        Err(_) => panic!("the mux did not finish in a minute"),
    }
    let b = bytes.lock().unwrap().clone();
    (b, count)
}

type Got = (Vec<u8>, u64, bool);

fn read(file: &[u8], fragmented: bool) -> (Mp4Reader, Vec<Vec<Got>>) {
    let head = match fragmented {
        true => {
            let moof = file.windows(4).position(|w| w == b"moof").expect("a fragment") - 4;
            &file[..moof]
        }
        false => file,
    };
    let mut r = Mp4Reader::new(head).expect("the moov resolves");
    r.push_bytes(file);
    let mut got = vec![Vec::new(); r.tracks().len()];
    while let Some(s) = r.next_sample() {
        let ns = r.tracks()[s.track_index].stamp_ns(s.pts);
        got[s.track_index].push((s.payload.data().to_vec(), ns, s.sync));
    }
    (r, got)
}

#[test]
fn annex_b_aac_and_cues_round_trip_both_ways() {
    let (v, a) = (video(50), audio(94));
    let cues = vec![cue(200, 500, "one"), cue(1000, 300, "two"), cue(1300, 400, "three")];
    let mut want_rec = ParamSets::new(NalCodec::H264);
    want_rec.absorb(&sets()).unwrap();
    let want_rec = want_rec.record().unwrap();
    for mode in [Mode::Progressive, Mode::Fragmented] {
        let srcs = vec![video_src(v.clone()), audio_src(a.clone()), text_src(cues.clone())];
        let (file, _) =
            run(srcs, Box::new(Mp4Mux::tracks(mode, 3)), Mp4Mux::INS, Mp4Mux::OUT, None);
        let (r, got) = read(&file, mode == Mode::Fragmented);
        assert_eq!(r.tracks().len(), 3, "{mode:?}");
        assert_eq!(r.tracks()[0].entry.config_record, want_rec, "the avcC of the in-band sets");
        assert_eq!(r.tracks()[1].entry.config_record, vec![0x11, 0x90]);
        assert_eq!(r.tracks()[1].media_time, 1024, "the declared delay is the audio edit");
        assert_eq!(r.tracks()[2].family(), "subtitle/srt");

        assert_eq!(got[0].len(), v.len(), "{mode:?} pictures");
        for (i, (g, f)) in got[0].iter().zip(&v).enumerate() {
            assert_eq!(g.0, length_prefixed(&f.bytes), "{mode:?} picture {i} bytes");
            assert!(
                g.1.abs_diff(f.pts) <= 12_000,
                "{mode:?} picture {i} at {} ns, sent at {} ns",
                g.1,
                f.pts
            );
            assert_eq!(g.2, f.key, "{mode:?} picture {i} sync");
        }
        assert_eq!(got[1].len(), a.len(), "{mode:?} audio frames");
        for (i, (g, f)) in got[1].iter().zip(&a).enumerate() {
            assert_eq!(g.0, f.bytes, "{mode:?} audio {i}");
            assert_eq!(
                g.1,
                (i as u64 * 1024) * 1_000_000_000 / 48_000,
                "{mode:?} audio {i} on the frame grid"
            );
        }
        let text: Vec<(Vec<u8>, u64)> =
            got[2].iter().map(|g| (g.0.clone(), g.1 / 1_000_000)).collect();
        let sample = |s: &str| {
            let mut b = (s.len() as u16).to_be_bytes().to_vec();
            b.extend_from_slice(s.as_bytes());
            b
        };
        let want = vec![
            (sample("one"), 200),
            (vec![0, 0], 700),
            (sample("two"), 1000),
            (sample("three"), 1300),
        ];
        assert_eq!(text, want, "{mode:?} cues");
    }
}

fn sparse_sources(cue_late: bool) -> (Vec<Src>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let sink_seen = Arc::new(AtomicUsize::new(0));
    let sink_count = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let mut v = video_src(video(250));
    v.done = Some(Arc::clone(&done));
    v.watch = Some((240, Arc::clone(&sink_count), Arc::clone(&sink_seen)));
    let a = audio_src(audio(94));
    let mut t = match cue_late {
        true => text_src(vec![cue(1000, 1000, "early"), cue(9500, 400, "late")]),
        false => audio_src(audio(400)),
    };
    t.stall = Some((1, Arc::clone(&done)));
    (vec![v, a, t], sink_count, sink_seen)
}

#[test]
fn a_sparse_input_and_an_early_end_hold_mp4mux_back_by_a_bound() {
    for mode in [Mode::Progressive, Mode::Fragmented] {
        let (srcs, count, seen) = sparse_sources(true);
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut p = Pipeline::new();
        let m = p.add(Mp4Mux::tracks(mode, 3));
        let sink = p.add(Collect { bytes: Arc::clone(&bytes), count: Arc::clone(&count) });
        for (i, s) in srcs.into_iter().enumerate() {
            let video = s.family == "h264/annexb";
            let e = p.add(s);
            if video {
                p.set_element_pool(e, 4096, 56);
            }
            p.link(e.src(Src::OUT), m.sink(Mp4Mux::INS[i])).expect("src -> mux");
        }
        p.link(m.src(Mp4Mux::OUT), sink.sink(Collect::IN)).expect("mux -> sink");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(p.run().map(|_| ()).map_err(|e| format!("{e:?}")));
        });
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(r) => r.expect("the mux runs"),
            Err(_) => panic!("{mode:?}: the mux stalled on the sparse input"),
        }
        let file = bytes.lock().unwrap().clone();
        let early = seen.load(Ordering::Acquire);
        assert!(
            early > file.len() / 2,
            "{mode:?}: {early} of {} bytes out before the video ended",
            file.len()
        );
        let (_, got) = read(&file, mode == Mode::Fragmented);
        assert_eq!(got[0].len(), 250, "{mode:?} every picture");
        assert_eq!(got[1].len(), 94, "{mode:?} the audio that ended early");
        let cues: Vec<u64> =
            got[2].iter().filter(|g| g.0.len() > 2).map(|g| g.1 / 1_000_000).collect();
        assert_eq!(cues, vec![1000, 9500], "{mode:?} the late cue keeps its stamp");
    }
}

#[test]
fn a_stalled_input_holds_mkvmuxn_back_by_a_bound() {
    let (srcs, count, seen) = sparse_sources(false);
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let m = p.add(MkvMux::multi(3));
    let sink = p.add(Collect { bytes: Arc::clone(&bytes), count: Arc::clone(&count) });
    for (i, s) in srcs.into_iter().enumerate() {
        let video = s.family == "h264/annexb";
        let e = p.add(s);
        if video {
            p.set_element_pool(e, 4096, 56);
        }
        p.link(e.src(Src::OUT), m.sink(MkvMuxN::INS[i])).expect("src -> mux");
    }
    p.link(m.src(MkvMuxN::OUT), sink.sink(Collect::IN)).expect("mux -> sink");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(p.run().map(|_| ()).map_err(|e| format!("{e:?}")));
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(r) => r.expect("the mux runs"),
        Err(_) => panic!("mkvmuxn stalled on the stalled input"),
    }
    let file = bytes.lock().unwrap().clone();
    let early = seen.load(Ordering::Acquire);
    assert!(early > file.len() / 2, "{early} of {} bytes out before the video ended", file.len());
    let mut r = MatroskaReader::new();
    r.push(&file).expect("the output parses");
    let mut per = [0usize; 3];
    while let Some(f) = r.next_frame() {
        per[f.track_number as usize - 1] += 1;
    }
    assert_eq!(per, [250, 94, 400], "every frame of every track");
    assert_eq!(r.tracks()[0].codec_id, "V_MPEG4/ISO/AVC");
    let mut want = ParamSets::new(NalCodec::H264);
    want.absorb(&sets()).unwrap();
    assert_eq!(r.tracks()[0].codec_private, want.record().unwrap(), "the avcC of the in-band sets");
}

#[test]
fn entries_name_the_tracks() {
    let srcs = vec![video_src(video(5)), audio_src(audio(5))];
    let (file, _) = run(
        srcs,
        Box::new(Mp4Mux::tracks(Mode::Progressive, 2)),
        Mp4Mux::INS,
        Mp4Mux::OUT,
        Some(64),
    );
    let (r, _) = read(&file, false);
    assert!(matches!(r.tracks()[0].entry.kind, [b'a', b'v', b'c', b'1']));
    assert!(matches!(r.tracks()[1].entry.kind, [b'm', b'p', b'4', b'a']));
}

/// A sample stamped the maximum once held every sample behind it for good, starving the source.
#[test]
fn a_sample_at_the_end_of_time_holds_none_behind_it() {
    for mode in [Mode::Progressive, Mode::Fragmented] {
        let mut frames = audio(2);
        frames[1].pts = Timestamp::MAX.0;
        // `u64::MAX` is `Timestamp::NONE`, an unstamped sample.
        frames.extend(audio(200).into_iter().map(|f| Frame { pts: u64::MAX, ..f }));
        let (file, _) = run(
            vec![audio_src(frames)],
            Box::new(Mp4Mux::tracks(mode, 1)),
            Mp4Mux::INS,
            Mp4Mux::OUT,
            None,
        );
        let (_, got) = read(&file, mode == Mode::Fragmented);
        assert_eq!(got[0].len(), 202, "{mode:?}: every sample written");
    }
}

#[test]
fn a_head_sent_again_on_a_ready_lane_is_not_a_sample() {
    let a = audio(12);
    let mut frames = a.clone();
    let again = Frame { pts: a[4].pts, dur: 0, key: true, head: true, bytes: vec![0x11, 0x90] };
    frames.insert(4, again);
    let srcs = vec![audio_src(frames)];
    let (file, _) =
        run(srcs, Box::new(Mp4Mux::tracks(Mode::Progressive, 1)), Mp4Mux::INS, Mp4Mux::OUT, None);
    let (_, got) = read(&file, false);
    assert_eq!(got[0].len(), a.len(), "the head again is dropped");
    for (g, f) in got[0].iter().zip(&a) {
        assert_eq!(g.0, f.bytes);
    }
}
