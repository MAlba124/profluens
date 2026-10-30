//! Geometry, testsrc and rawvideoparse runs under a MockClock, and negotiation failures.

// Test fixtures, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use profluens_core::batch::Inputs;
use profluens_core::clock::MockClock;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, LinkError};
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::{Rational, Timestamp};

use profluens_elements::io::FileSrc;
use profluens_video::format::PixelFormat;
use profluens_video::geometry::{frame_size, plane_count, plane_geometry, plane_offset};
use profluens_video::sink::checksum;
use profluens_video::testsrc::{frame_pattern_byte, frame_pts};
use profluens_video::{RawVideoParse, VideoCkSink, VideoFormat, VideoTestSrc};

#[test]
fn geometry_planes_pack_tightly_and_size_is_the_sum() {
    type Case = (PixelFormat, u32, u32, &'static [(usize, usize)]);
    let cases: &[Case] = &[
        (PixelFormat::I420, 4, 4, &[(4, 4), (2, 2), (2, 2)]),
        (PixelFormat::I420, 3, 3, &[(3, 3), (2, 2), (2, 2)]), // chroma ceil(3/2)=2
        (PixelFormat::Nv12, 4, 4, &[(4, 4), (4, 2)]),
        (PixelFormat::Nv12, 3, 5, &[(3, 5), (4, 3)]), // chroma stride 2*ceil(3/2)=4
        (PixelFormat::P010, 4, 4, &[(8, 4), (8, 2)]), // 16-bit words, NV12 strides doubled
        (PixelFormat::P010, 3, 5, &[(6, 5), (8, 3)]), // chroma stride 4*ceil(3/2)=8
        (PixelFormat::Rgb24, 5, 3, &[(15, 3)]),
        (PixelFormat::Gray8, 5, 3, &[(5, 3)]),
    ];
    for &(pixfmt, w, h, planes) in cases {
        assert_eq!(plane_count(pixfmt), planes.len(), "{pixfmt:?} {w}x{h}");
        let mut off = 0usize;
        let mut total = 0usize;
        for (i, &(stride, height)) in planes.iter().enumerate() {
            let g = plane_geometry(pixfmt, w, h, i).unwrap();
            assert_eq!((g.stride, g.height, g.size), (stride, height, stride * height));
            assert_eq!(plane_offset(pixfmt, w, h, i), Some(off));
            off += g.size;
            total += g.size;
        }
        assert_eq!(frame_size(pixfmt, w, h), total, "{pixfmt:?} {w}x{h} total");
    }
}

#[test]
fn videotestsrc_renders_on_fps_deadlines_and_pixels_round_trip() {
    // About an hour of virtual time at 30 fps, run in milliseconds without sleeping.
    let fps = Rational::new(30, 1);
    let format = VideoFormat::new(16, 16, PixelFormat::I420, fps);
    let seed = 0xABCD_1234u64;
    let count = 108_000u64; // one virtual hour at 30 fps

    let clock = MockClock::new();
    let (sink, stats) = VideoCkSink::new();
    let mut p = Pipeline::new();
    let src = p.add(VideoTestSrc::new(format, seed, count));
    let snk = p.add(sink);
    p.link(src.src(VideoTestSrc::OUT), snk.sink(VideoCkSink::IN)).expect("video/raw negotiates");
    p.set_clock(Arc::new(clock.clone()));

    let start = StdInstant::now();
    let run = std::thread::spawn(move || p.run());

    let step = Timestamp::from_secs(60); // one virtual minute per jump
    while !run.is_finished() {
        clock.advance(step);
        std::thread::yield_now();
    }
    run.join().expect("run joined").expect("run ok");
    let wall = start.elapsed();

    assert!(stats.is_done(), "sink saw EOS");
    let renders = stats.renders();
    assert_eq!(renders.len() as u64, count, "every frame rendered exactly once");

    let frame_bytes = frame_size(format.pixfmt, format.width, format.height);
    for &i in &[0u64, 1, 2, 100, 29, 30, count - 1] {
        let r = renders[i as usize];
        assert_eq!(r.pts, frame_pts(fps, i), "frame {i} PTS on the fps grid");
        assert!(r.rendered_at >= r.pts, "frame {i} not rendered ahead of its deadline");
        let expect: Vec<u8> =
            (0..frame_bytes).map(|pos| frame_pattern_byte(seed, i, pos)).collect();
        assert_eq!(r.checksum, checksum(&expect), "frame {i} pixels round-trip");
    }
    assert!(
        wall < Duration::from_secs(20),
        "one virtual hour took {wall:?} of wall time, should be well under a second"
    );
}

#[test]
fn videocksink_parks_until_the_clock_reaches_a_deadline() {
    // The second frame is a virtual hour away and its group parks, so the run cannot finish early.
    let fps = Rational::new(1, 3600); // one frame per hour
    let format = VideoFormat::new(2, 2, PixelFormat::Gray8, fps);
    let clock = MockClock::new();
    let (sink, stats) = VideoCkSink::new();
    let mut p = Pipeline::new();
    let src = p.add(VideoTestSrc::new(format, 1, 2));
    let snk = p.add(sink);
    p.link(src.src(VideoTestSrc::OUT), snk.sink(VideoCkSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));

    let run = std::thread::spawn(move || p.run());
    for _ in 0..60 {
        if run.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!run.is_finished(), "run finished before the 2nd frame's deadline");
    assert_eq!(stats.count(), 1, "only frame 0 (due at t=0) rendered while parked");

    clock.advance(Timestamp::from_secs(3600));
    run.join().expect("joined").expect("run ok");
    let renders = stats.renders();
    assert_eq!(renders.len(), 2);
    assert_eq!(renders[1].pts, Timestamp::from_secs(3600));
    assert!(renders[1].rendered_at >= renders[1].pts);
}

#[test]
fn filesrc_rawvideoparse_chunks_a_raw_file_into_frames() {
    let fps = Rational::new(25, 1);
    let format = VideoFormat::new(16, 16, PixelFormat::I420, fps);
    let frame_bytes = frame_size(format.pixfmt, format.width, format.height);
    let frames = 40u64;

    // Each frame a distinct pattern so the checksum catches mis-chunking across buffers.
    let mut raw = Vec::with_capacity(frames as usize * frame_bytes);
    for f in 0..frames {
        for pos in 0..frame_bytes {
            raw.push(frame_pattern_byte(0xF00D, f, pos));
        }
    }
    let dir = std::env::temp_dir();
    let path = dir.join(format!("pf_video_test_{}.raw", std::process::id()));
    std::fs::write(&path, &raw).expect("write temp raw file");

    let clock = MockClock::new();
    let (sink, stats) = VideoCkSink::new();
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&path));
    let parse = p.add(RawVideoParse::new(format));
    let snk = p.add(sink);
    p.link(src.src(FileSrc::OUT), parse.sink(RawVideoParse::IN))
        .expect("filesrc -> rawvideoparse (bytes)");
    p.link(parse.src(RawVideoParse::OUT), snk.sink(VideoCkSink::IN))
        .expect("rawvideoparse -> videocksink");
    p.set_clock(Arc::new(clock.clone()));

    let run = std::thread::spawn(move || p.run());
    let step = Timestamp::from_millis(200);
    while !run.is_finished() {
        clock.advance(step);
        std::thread::yield_now();
    }
    let run_result = run.join().expect("joined");
    std::fs::remove_file(&path).ok();
    run_result.expect("run ok");

    assert!(stats.is_done(), "sink saw EOS");
    let renders = stats.renders();
    assert_eq!(renders.len() as u64, frames, "one buffer per whole frame");
    for (f, r) in renders.iter().enumerate() {
        assert_eq!(r.pts, frame_pts(fps, f as u64), "frame {f} PTS on the fps grid");
        let expect: Vec<u8> =
            (0..frame_bytes).map(|pos| frame_pattern_byte(0xF00D, f as u64, pos)).collect();
        assert_eq!(r.checksum, checksum(&expect), "frame {f} chunked intact");
    }
}

// An i420-only source against a gray8-only sink, an empty intersection rejected at link().

static NARROW_PIXFMT: [ValueDesc; 1] = [ValueDesc::Id("i420")];
static NARROW_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Set(&NARROW_PIXFMT), preferred: None }];
static NARROW_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &NARROW_FIELDS }];
static NARROW_SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &NARROW_OFFERS }];
static NARROW_SRC_DESC: ElementDesc = ElementDesc {
    name: "i420only_src",
    sinks: &[],
    srcs: &NARROW_SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct I420OnlySrc;
impl I420OnlySrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for I420OnlySrc {
    fn desc(&self) -> &'static ElementDesc {
        &NARROW_SRC_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Eos)
    }
}

static GRAY_PIXFMT: [ValueDesc; 1] = [ValueDesc::Id("gray8")];
static GRAY_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Set(&GRAY_PIXFMT), preferred: None }];
static GRAY_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &GRAY_FIELDS }];
static GRAY_SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &GRAY_OFFERS }];
static GRAY_SINK_DESC: ElementDesc = ElementDesc {
    name: "gray8only_sink",
    sinks: &GRAY_SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct Gray8OnlySink;
impl Gray8OnlySink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for Gray8OnlySink {
    fn desc(&self) -> &'static ElementDesc {
        &GRAY_SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Ok)
    }
}

#[test]
fn incompatible_pixfmt_fails_negotiation_at_link() {
    let mut p = Pipeline::new();
    let src = p.add(I420OnlySrc);
    let snk = p.add(Gray8OnlySink);
    let err = p.link(src.src(I420OnlySrc::OUT), snk.sink(Gray8OnlySink::IN)).unwrap_err();
    assert!(
        matches!(err, Error::Link(LinkError::NoCommonFormat(_))),
        "error should be the empty pixfmt intersection, got: {err:?}"
    );
}

#[test]
fn videotestsrc_props_override_constructor_defaults() {
    use profluens_core::format::Value;

    let ctor = VideoFormat::new(320, 240, PixelFormat::Rgb24, Rational::new(25, 1));
    let want = VideoFormat::new(8, 8, PixelFormat::Gray8, Rational::new(30, 1));
    let want_seed = 0x00C0_FFEE_u64;
    let want_frames = 7u64;

    let clock = MockClock::new();
    let (sink, stats) = VideoCkSink::new();
    let mut p = Pipeline::new();
    let src = p.add(VideoTestSrc::new(ctor, 1, 1)); // ctor seed=1, frames=1
    let snk = p.add(sink);
    p.link(src.src(VideoTestSrc::OUT), snk.sink(VideoCkSink::IN)).expect("link");

    p.set(src, "width", Value::Int(want.width as i64)).expect("width");
    p.set(src, "height", Value::Int(want.height as i64)).expect("height");
    p.set_str(src, "pixfmt", want.pixfmt.caps_name()).expect("pixfmt");
    p.set(src, "fps", Value::Rat(want.fps.num, want.fps.den)).expect("fps");
    p.set(src, "frames", Value::Int(want_frames as i64)).expect("frames");
    p.set(src, "seed", Value::Int(want_seed as i64)).expect("seed");

    p.set_clock(Arc::new(clock.clone()));
    let run = std::thread::spawn(move || p.run());
    let step = Timestamp::from_millis(200);
    while !run.is_finished() {
        clock.advance(step);
        std::thread::yield_now();
    }
    run.join().expect("joined").expect("run ok");

    let renders = stats.renders();
    assert_eq!(renders.len() as u64, want_frames, "frames prop overrode the constructor");

    let frame_bytes = frame_size(want.pixfmt, want.width, want.height);
    for (i, r) in renders.iter().enumerate() {
        assert_eq!(r.pts, frame_pts(want.fps, i as u64), "frame {i} on the overridden fps grid");
        let expect: Vec<u8> =
            (0..frame_bytes).map(|pos| frame_pattern_byte(want_seed, i as u64, pos)).collect();
        assert_eq!(r.checksum, checksum(&expect), "frame {i} matches the overridden format");
    }
}

#[test]
fn the_source_and_the_parser_say_their_format_once_from_the_start() {
    use pf_harness::Harness;
    use profluens_core::format::Value;
    let fmt = VideoFormat::new(16, 8, PixelFormat::I420, Rational { num: 25, den: 1 });
    let size = frame_size(fmt.pixfmt, fmt.width, fmt.height);
    let width_of = |h: &Harness| {
        let said = h.announced().expect("a format");
        said.get(h.vocabulary().field_id("width").expect("the field"))
    };

    let mut src = Harness::with_slot_size(VideoTestSrc::new(fmt, 7, 3), size);
    src.crank().expect("the first frame");
    assert_eq!((src.format_changes(), width_of(&src)), (1, Some(Value::Int(16))));
    assert!(src.pull(VideoTestSrc::OUT).is_some(), "with a frame behind it");
    src.crank().expect("the second frame");
    assert_eq!(src.format_changes(), 1, "nothing more is said");

    let mut parse = Harness::with_slot_size(RawVideoParse::new(fmt), size);
    parse.crank().expect("a pass with no input");
    assert_eq!((parse.format_changes(), width_of(&parse)), (1, Some(Value::Int(16))));
    let bytes = parse.alloc(&vec![0u8; size]);
    parse.push(bytes).expect("a frame of bytes");
    assert!(parse.pull(RawVideoParse::OUT).is_some(), "with a frame behind it");
    assert_eq!(parse.format_changes(), 1, "nothing more is said");
}

#[test]
fn the_format_properties_are_solved_at_link() {
    use profluens_core::format::Value;
    let int = |p: &Pipeline, l, f: &str| p.negotiated(l).and_then(|x| x.get(p.field_id(f)?));
    let fmt = VideoFormat::new(64, 48, PixelFormat::Gray8, Rational::new(25, 1));
    let mut p = Pipeline::new();
    let src = p.add(VideoTestSrc::new(fmt, 1, 1));
    let (sink, _) = VideoCkSink::new();
    let snk = p.add(sink);
    let l = p.link(src.src(VideoTestSrc::OUT), snk.sink(VideoCkSink::IN)).expect("link");
    let pixfmt = int(&p, l, "pixfmt").and_then(|v| match v {
        Value::Id(v) => p.value_name(v).map(str::to_string),
        _ => None,
    });
    assert_eq!(
        (int(&p, l, "width"), int(&p, l, "height"), pixfmt.as_deref()),
        (Some(Value::Int(64)), Some(Value::Int(48)), Some("gray8"))
    );
    assert_eq!(int(&p, l, "fps"), Some(Value::Rat(25, 1)));
    p.set(src, "width", Value::Int(32)).expect("width");
    p.solve_check().expect("solves");
    let mut p = Pipeline::new();
    let src = p.add(VideoTestSrc::new(
        VideoFormat::new(8, 8, PixelFormat::I420, Rational::new(30, 1)),
        1,
        1,
    ));
    let snk = p.add(Gray8OnlySink);
    let err = p.link(src.src(VideoTestSrc::OUT), snk.sink(Gray8OnlySink::IN)).unwrap_err();
    assert!(matches!(err, Error::Link(LinkError::NoCommonFormat(_))), "{err:?}");
    let mut p = Pipeline::new();
    let file = p.add(FileSrc::new("/dev/null"));
    let parse =
        p.add(RawVideoParse::new(VideoFormat::new(8, 8, PixelFormat::Gray8, Rational::new(30, 1))));
    let snk = p.add(Gray8OnlySink);
    p.link(file.src(FileSrc::OUT), parse.sink(RawVideoParse::IN)).expect("bytes in");
    let l = p.link(parse.src(RawVideoParse::OUT), snk.sink(Gray8OnlySink::IN)).expect("gray8 out");
    assert_eq!(int(&p, l, "width"), Some(Value::Int(8)));
}
