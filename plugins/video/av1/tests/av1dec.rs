// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

#[path = "support/md5.rs"]
mod md5;

use pf_av1::dav1d;
use pf_av1::{Av1Dec, Av1DecError};
use pf_harness::Harness;
use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

const FRAME_NS: u64 = 40_000_000;

const SLOT: usize = 64 * 1024;

const EXACT: [&str; 7] = [
    "inter_64x48",
    "altref_64x48",
    "odd_61x37",
    "mono_64x48",
    "tiles_256x128",
    "grain_64x48",
    "resize_64x48_96x64",
];

struct Expected {
    width: i64,
    height: i64,
    md5: String,
}

struct Fixture {
    name: &'static str,
    units: Vec<Vec<u8>>,
    expected: Vec<Expected>,
}

fn ivf_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    assert_eq!(&bytes[..4], b"DKIF");
    let mut out = Vec::new();
    let mut at = 32;
    while at + 12 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 12;
        out.push(bytes[at..at + n].to_vec());
        at += n;
    }
    out
}

fn fixture(name: &'static str) -> Fixture {
    let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let ivf = std::fs::read(format!("{dir}/{name}.ivf")).expect("fixture ivf");
    let manifest = std::fs::read_to_string(format!("{dir}/{name}.md5")).expect("fixture md5");
    let expected = manifest
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Expected {
                width: f[1].parse().unwrap(),
                height: f[2].parse().unwrap(),
                md5: f[3].to_string(),
            }
        })
        .collect();
    Fixture { name, units: ivf_units(&ivf), expected }
}

fn at(i: usize) -> Timestamp {
    Timestamp::from_nanos(i as u64 * FRAME_NS)
}

fn harness() -> Harness {
    Harness::with_slot_size(Av1Dec::new(), SLOT)
}

struct Frame {
    pts: Timestamp,
    duration: Timestamp,
    bytes: Vec<u8>,
}

fn frames(out: Vec<Buffer>) -> Vec<Frame> {
    out.into_iter()
        .map(|b| Frame { pts: b.pts, duration: b.duration, bytes: b.memory.data().to_vec() })
        .collect()
}

fn feed(h: &mut Harness, units: &[Vec<u8>], range: std::ops::Range<usize>) -> Vec<Frame> {
    let mut out = Vec::new();
    for i in range {
        let mut buf = h.alloc(&units[i]);
        buf.pts = at(i);
        buf.duration = Timestamp::from_nanos(FRAME_NS);
        h.push(buf).expect("push");
        out.extend(frames(h.drain_outputs()));
    }
    out
}

fn decode_all(h: &mut Harness, units: &[Vec<u8>]) -> Vec<Frame> {
    let mut out = feed(h, units, 0..units.len());
    out.extend(frames(h.eos().expect("eos")));
    out
}

fn index(f: &Frame) -> usize {
    (f.pts.nanos().expect("stamped") / FRAME_NS) as usize
}

fn check(f: &Fixture, out: &[Frame], from: usize) {
    assert_eq!(out.len(), f.expected.len() - from, "{}: frames out from {from}", f.name);
    for (k, b) in out.iter().enumerate() {
        let n = from + k;
        assert_eq!(b.pts, at(n), "{}: frame {n} pts", f.name);
        assert_eq!(md5::md5_hex(&b.bytes), f.expected[n].md5, "{}: frame {n} pixels", f.name);
    }
}

fn warnings(h: &mut Harness) -> Vec<Error> {
    h.bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } => Some(error),
            _ => None,
        })
        .collect()
}

fn announced_int(h: &Harness, field: &str) -> Option<i64> {
    let f = h.announced()?;
    match f.get(h.vocabulary().field_id(field)?)? {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

fn announced_id(h: &Harness, field: &str) -> Option<String> {
    let f = h.announced()?;
    let Value::Id(v) = f.get(h.vocabulary().field_id(field)?)? else {
        return None;
    };
    h.vocabulary().value_name(v).map(str::to_string)
}

#[test]
fn every_fixture_decodes_bit_exactly_to_libaom() {
    for name in EXACT {
        let f = fixture(name);
        let mut h = harness();
        let out = decode_all(&mut h, &f.units);
        check(&f, &out, 0);
        let last = f.expected.last().unwrap();
        assert_eq!(announced_int(&h, "width"), Some(last.width), "{name}: announced width");
        assert_eq!(announced_int(&h, "height"), Some(last.height), "{name}: announced height");
        assert_eq!(h.input_len(), 0, "{name}: every unit taken");
        assert!(warnings(&mut h).is_empty(), "{name}: no warning on a clean stream");
    }
}

#[test]
fn a_resolution_change_is_announced_ahead_of_the_first_frame_in_the_new_size() {
    let f = fixture("resize_64x48_96x64");
    let mut h = harness();
    let mut out = feed(&mut h, &f.units, 0..5);
    out.extend(frames(h.eos().expect("eos")));
    assert_eq!(h.format_changes(), 1);
    assert_eq!((announced_int(&h, "width"), announced_int(&h, "height")), (Some(64), Some(48)));
    out.extend(feed(&mut h, &f.units, 5..10));
    out.extend(frames(h.eos().expect("eos")));
    assert_eq!(h.format_changes(), 2);
    assert_eq!((announced_int(&h, "width"), announced_int(&h, "height")), (Some(96), Some(64)));
    check(&f, &out, 0);
    for (k, b) in out.iter().enumerate() {
        let e = &f.expected[k];
        assert_eq!(b.bytes.len() as i64, e.width * e.height * 3 / 2, "frame {k} size");
    }
}

#[test]
fn an_invisible_altref_keeps_the_timing_of_the_units_that_show_frames() {
    let f = fixture("altref_64x48");
    assert!(f.units.iter().any(|u| u.len() <= 8), "the fixture has a show_existing_frame unit");
    let mut h = harness();
    let out = decode_all(&mut h, &f.units);
    check(&f, &out, 0);
    for b in &out {
        assert_eq!(b.duration, Timestamp::from_nanos(FRAME_NS), "the unit's duration rides along");
    }
}

#[test]
fn monochrome_comes_out_as_gray8_and_420_as_i420() {
    let mono = fixture("mono_64x48");
    let mut h = harness();
    let out = decode_all(&mut h, &mono.units);
    check(&mono, &out, 0);
    assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some("gray8"));
    assert_eq!(out[0].bytes.len(), 64 * 48, "Y only");

    let inter = fixture("inter_64x48");
    let mut h = harness();
    let out = decode_all(&mut h, &inter.units);
    check(&inter, &out, 0);
    assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some("i420"));
}

fn p010_to_planar16(p010: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    assert_eq!(p010.len(), 2 * (w * h + 2 * cw * ch), "a tight p010 frame");
    let word = |i: usize| {
        let v = u16::from_le_bytes([p010[2 * i], p010[2 * i + 1]]);
        assert_eq!(v & 0x3F, 0, "word {i} has its low six bits clear");
        (v >> 6).to_le_bytes()
    };
    let mut out = Vec::with_capacity(p010.len());
    (0..w * h).for_each(|i| out.extend(word(i)));
    (0..cw * ch).for_each(|i| out.extend(word(w * h + 2 * i)));
    (0..cw * ch).for_each(|i| out.extend(word(w * h + 2 * i + 1)));
    out
}

#[test]
fn ten_bit_420_comes_out_as_p010_bit_exactly_to_libaom() {
    for name in ["tenbit_64x48", "tenbit_odd_61x37"] {
        let f = fixture(name);
        let mut h = harness();
        let out = decode_all(&mut h, &f.units);
        assert_eq!(out.len(), f.expected.len(), "{name}: every frame");
        assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some("p010"), "{name}");
        for (n, b) in out.iter().enumerate() {
            let e = &f.expected[n];
            assert_eq!(b.pts, at(n), "{name}: frame {n} pts");
            let planar = p010_to_planar16(&b.bytes, e.width as usize, e.height as usize);
            assert_eq!(md5::md5_hex(&planar), e.md5, "{name}: frame {n} samples");
        }
        assert!(warnings(&mut h).is_empty(), "{name}: no warning on a clean stream");
    }
}

fn harness_taking_422() -> Harness {
    let mut h = harness();
    let names = ["i420", "gray8", "p010", "i422", "i422_10"].map(ValueDesc::Id);
    h.fix_src_takes(Av1Dec::OUT, "pixfmt", &names);
    h
}

#[test]
fn four_two_two_comes_out_as_decoded_bit_exactly_to_libaom_where_the_link_takes_it() {
    for (name, pixfmt) in [("i422_64x48", "i422"), ("tenbit422_64x48", "i422_10")] {
        let f = fixture(name);
        let mut h = harness_taking_422();
        let out = decode_all(&mut h, &f.units);
        assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some(pixfmt), "{name}");
        check(&f, &out, 0);
        assert!(warnings(&mut h).is_empty(), "{name}: no warning on a clean stream");
    }
}

#[test]
fn four_two_two_goes_out_as_420_where_the_link_does_not_take_it() {
    for (name, pixfmt, wide) in [("i422_64x48", "i420", false), ("tenbit422_64x48", "p010", true)] {
        let f = fixture(name);
        let native = decode_all(&mut harness_taking_422(), &f.units);
        let mut h = harness();
        let out = decode_all(&mut h, &f.units);
        assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some(pixfmt), "{name}");
        assert_eq!(out.len(), f.expected.len(), "{name}: every frame");
        let (luma, chroma) = (64 * 48, 2 * 32 * 24);
        for (n, (b, d)) in out.iter().zip(&native).enumerate() {
            assert_eq!(b.pts, at(n), "{name}: frame {n} pts");
            match wide {
                false => {
                    assert_eq!(b.bytes.len(), luma + chroma, "{name}: frame {n} tight i420");
                    assert_eq!(b.bytes[..luma], d.bytes[..luma], "{name}: frame {n} luma");
                }
                true => {
                    assert_eq!(b.bytes.len(), 2 * (luma + chroma), "{name}: frame {n} tight p010");
                    let down: Vec<u8> = b.bytes[..2 * luma]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .flat_map(|w| (u16::from_le_bytes(*w) >> 6).to_le_bytes())
                        .collect();
                    assert_eq!(down, d.bytes[..2 * luma], "{name}: frame {n} luma");
                }
            }
        }
        assert!(warnings(&mut h).is_empty(), "{name}: no warning on a clean stream");
    }
}

#[test]
fn what_the_link_does_not_take_goes_out_as_the_i420_it_does() {
    let mut h = harness();
    h.fix_src_takes(Av1Dec::OUT, "pixfmt", &["i420"].map(ValueDesc::Id));
    let f = fixture("mono_64x48");
    let gray = decode_all(&mut harness(), &f.units);
    let out = decode_all(&mut h, &f.units);
    assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some("i420"));
    assert_eq!(out.len(), f.expected.len(), "every frame");
    let luma = 64 * 48;
    for (n, (b, g)) in out.iter().zip(&gray).enumerate() {
        assert_eq!(b.bytes.len(), luma * 3 / 2, "frame {n}: tight i420");
        assert_eq!(b.bytes[..luma], g.bytes[..], "frame {n}: luma as decoded");
        assert!(b.bytes[luma..].iter().all(|c| *c == 128), "frame {n}: neutral chroma");
    }
    let mut h = harness();
    h.fix_src_takes(Av1Dec::OUT, "pixfmt", &["i420"].map(ValueDesc::Id));
    let f = fixture("tenbit_64x48");
    let deep = decode_all(&mut harness(), &f.units);
    let out = decode_all(&mut h, &f.units);
    assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some("i420"));
    for (n, (b, d)) in out.iter().zip(&deep).enumerate() {
        let want: Vec<u8> = d.bytes[..2 * luma]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|w| ((u32::from(u16::from_le_bytes(*w) >> 6) + 2) >> 2).min(255) as u8)
            .collect();
        assert_eq!(b.bytes[..luma], want[..], "frame {n}: luma rounded to 8 bits");
    }
}

#[test]
fn ten_bit_444_goes_out_as_p010_reduced_to_420() {
    let f = fixture("tenbit444_64x48");
    let mut h = harness();
    let out = decode_all(&mut h, &f.units);
    assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some("p010"));
    assert_eq!(out.len(), f.expected.len(), "every frame");
    for (n, b) in out.iter().enumerate() {
        assert_eq!(b.pts, at(n), "frame {n} pts");
        assert_eq!(b.bytes.len(), 2 * (64 * 48 + 2 * 32 * 24), "frame {n} tight p010");
    }
    assert!(warnings(&mut h).is_empty(), "no warning on a clean stream");
}

#[test]
fn the_announcement_carries_the_streams_range_over_the_containers_fields() {
    let f = fixture("inter_64x48");
    let mut h = harness();
    h.fix_sink_format(
        Av1Dec::IN,
        "av1",
        &[("matrix", ValueDesc::Id("bt709")), ("range", ValueDesc::Id("full"))],
    );
    let out = decode_all(&mut h, &f.units);
    check(&f, &out, 0);
    assert_eq!(
        announced_id(&h, "matrix").as_deref(),
        Some("bt709"),
        "the container's matrix rides along"
    );
    assert_eq!(announced_id(&h, "range").as_deref(), Some("limited"), "the stream's range wins");
    assert_eq!(announced_id(&h, "primaries"), None, "an unspecified field stays absent");
}

#[test]
fn corrupt_unit_warns_drops_and_recovers_at_next_keyframe() {
    let f = fixture("inter_64x48");
    let mut units = vec![vec![0xDE; 512]];
    units.extend(f.units.iter().cloned());
    let mut h = harness();
    let out = decode_all(&mut h, &units);
    assert_eq!(out.len(), f.expected.len(), "every real frame decoded");
    for (k, b) in out.iter().enumerate() {
        assert_eq!(index(b), k + 1);
        assert_eq!(md5::md5_hex(&b.bytes), f.expected[k].md5, "frame {k}");
    }
    let w = warnings(&mut h);
    assert!(!w.is_empty(), "the dropped unit surfaced as a bus warning");
    let Error::ElementFailed { source, .. } = &w[0] else { panic!("typed element failure") };
    assert!(matches!(source.downcast_ref::<Av1DecError>(), Some(Av1DecError::Unit(_))), "{source}");
}

#[test]
fn garbage_mid_stream_is_dropped_and_what_decodes_is_still_exact() {
    let f = fixture("inter_64x48");
    let mut units = f.units[..4].to_vec();
    units.push(vec![0x12, 0x00, 0x32, 0x7F, 0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00]);
    units.extend(f.units[4..].iter().cloned());
    let mut h = harness();
    let out = decode_all(&mut h, &units);
    assert!(out.len() >= 4, "at least the frames before the damage come out");
    for b in &out {
        let n = index(b) - (index(b) > 4) as usize;
        assert_eq!(md5::md5_hex(&b.bytes), f.expected[n].md5, "frame {n}");
    }
    assert!(!warnings(&mut h).is_empty());
}

#[test]
fn garbage_only_stream_emits_nothing_and_never_panics() {
    let garbage: Vec<Vec<u8>> = (0..4).map(|i| vec![i as u8; 64 + i * 33]).collect();
    let mut h = harness();
    let out = decode_all(&mut h, &garbage);
    assert!(out.is_empty());
    assert!(h.announced().is_none());
    let empty = h.alloc(&[]);
    h.push(empty).expect("an empty unit is a warning, not a failure");
    assert!(h.eos().expect("eos").is_empty());
}

#[test]
fn flush_start_resets_the_decoder_and_no_stale_frame_leaks() {
    let f = fixture("inter_64x48");
    let mut h = harness();
    let pre = feed(&mut h, &f.units, 0..6);
    let pre_pts: Vec<Timestamp> = pre.iter().map(|b| b.pts).collect();
    drop(pre);

    h.push_event(Event::FlushStart).expect("flush");
    assert!(h.pull(Av1Dec::OUT).is_none(), "flush emits nothing");

    let post_pts = Timestamp::from_nanos(1000 * FRAME_NS);
    let mut buf = h.alloc(&f.units[0]);
    buf.pts = post_pts;
    h.push(buf).expect("push post-flush keyframe");
    let mut out = frames(h.drain_outputs());
    out.extend(frames(h.eos().expect("eos")));
    assert_eq!(out.len(), 1, "exactly the post-flush keyframe");
    assert_eq!(out[0].pts, post_pts);
    assert!(!pre_pts.contains(&out[0].pts));
    assert_eq!(md5::md5_hex(&out[0].bytes), f.expected[0].md5);
    assert_eq!(announced_int(&h, "width"), Some(64));
}

#[test]
fn from_the_presentation_point_on_the_pictures_are_those_of_a_full_decode() {
    let f = fixture("inter_64x48");
    let n = f.units.len();
    for point in 0..=n {
        let mut h = harness();
        h.seek(0, at(point)).expect("seek");
        let out = decode_all(&mut h, &f.units);
        check(&f, &out, point);
        assert_eq!(h.input_len(), 0, "every unit before the point was still taken");

        let mut h = harness();
        let before = feed(&mut h, &f.units, 0..2);
        h.skip_to(at(point)).expect("skip");
        let mut out = feed(&mut h, &f.units, 2..n);
        out.extend(frames(h.eos().expect("eos")));
        let from = point.max(before.len());
        check(&f, &out, from);
    }
}

#[test]
fn pictures_decode_into_slots_of_the_pool() {
    let f = fixture("inter_64x48");
    let mut h = harness();
    let out = decode_all(&mut h, &f.units);
    check(&f, &out, 0);
    let s = h.pool_stats();
    assert!(
        s.acquires as usize > out.len() + dav1d::SPARE,
        "slots decoded into beyond the packed, {s:?}"
    );
    assert!(
        s.outstanding <= 8 + 2 * dav1d::SPARE as u64,
        "no more held than references and spares, {s:?}"
    );
}

#[test]
fn a_pool_of_one_slot_grows_to_what_dav1d_holds() {
    let f = fixture("inter_64x48");
    let mut h = Harness::with_pool(Av1Dec::new(), SLOT, 1);
    for (i, u) in f.units.iter().enumerate() {
        let mut buf = h.alloc(u);
        buf.pts = at(i);
        h.push(buf).expect("push");
    }
    let mut out = Vec::new();
    let mut inputs_seen = Vec::new();
    for _ in 0..4 * f.units.len() {
        out.extend(frames(h.drain_outputs()));
        inputs_seen.push(h.input_len());
        h.crank().expect("crank");
    }
    assert!(inputs_seen.windows(2).all(|w| w[1] <= w[0]), "input only shrinks");
    h.push_event(Event::Eos).expect("eos");
    for _ in 0..8 {
        out.extend(frames(h.drain_outputs()));
        h.crank().expect("crank");
    }
    check(&f, &out, 0);
    let s = h.pool_stats();
    assert!(s.max_slots > 1 && s.max_slots <= 40, "grown from one to the stream's need, {s:?}");
    assert!(s.outstanding < s.max_slots, "what dav1d still holds leaves room, {s:?}");
}

static AV1_OFFERS: [OfferDesc; 1] = [OfferDesc::any("av1")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &AV1_OFFERS }];

static FRAMESRC_DESC: ElementDesc = ElementDesc {
    name: "framesrc",
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

struct FrameSrc {
    units: Vec<Vec<u8>>,
    next: usize,
}

impl Element for FrameSrc {
    fn desc(&self) -> &'static ElementDesc {
        &FRAMESRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.next >= self.units.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let bytes = &self.units[self.next];
        buf.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
        buf.memory.set_len(bytes.len());
        buf.pts = at(self.next);
        ctx.out(SrcPad(0)).push(buf);
        self.next += 1;
        Ok(Flow::Ok)
    }
}

static PIXFMTS: [ValueDesc; 2] = [ValueDesc::Id("i420"), ValueDesc::Id("gray8")];
static RAW_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Set(&PIXFMTS), preferred: None },
];
static RAW_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &RAW_FIELDS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_OFFERS }];

static RECORD_DESC: ElementDesc = ElementDesc {
    name: "planerecordsink",
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

type Frames = Arc<Mutex<Vec<(Timestamp, Vec<u8>)>>>;

struct PlaneRecordSink {
    frames: Frames,
}

impl Element for PlaneRecordSink {
    fn desc(&self) -> &'static ElementDesc {
        &RECORD_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.frames.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.push((buf.pts, buf.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }
}

fn run_pipeline(units: Vec<Vec<u8>>, props: &[(&str, i64)]) -> Vec<(Timestamp, Vec<u8>)> {
    let frames = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    p.set_pool(SLOT, 8);
    let src = p.add(FrameSrc { units, next: 0 });
    let dec = p.add(Av1Dec::new());
    let snk = p.add(PlaneRecordSink { frames: Arc::clone(&frames) });
    for (name, value) in props {
        p.set(dec, name, Value::Int(*value)).expect("a declared property");
    }
    p.link(src.src(SrcPad(0)), dec.sink(Av1Dec::IN)).expect("link src!dec");
    p.link(dec.src(Av1Dec::OUT), snk.sink(SinkPad(0))).expect("link dec!sink");
    p.run().expect("run");

    frames.lock().unwrap().clone()
}

#[test]
fn threads_and_frame_delay_are_properties_and_every_setting_decodes_exactly() {
    let f = fixture("altref_64x48");
    for props in
        [&[("threads", 1), ("frame_delay", 1)][..], &[("threads", 8), ("frame_delay", 4)], &[]]
    {
        let out = run_pipeline(f.units.clone(), props);
        assert_eq!(out.len(), f.expected.len(), "{props:?}: frames");
        for (k, (pts, bytes)) in out.iter().enumerate() {
            assert_eq!(*pts, at(k), "{props:?}: frame {k} pts");
            assert_eq!(md5::md5_hex(bytes), f.expected[k].md5, "{props:?}: frame {k} pixels");
        }
    }
}

#[test]
fn an_unknown_property_is_refused_at_the_call_site() {
    let mut p = Pipeline::new();
    let dec = p.add(Av1Dec::new());
    assert!(p.set(dec, "threads", Value::Int(2)).is_ok());
    assert!(p.set(dec, "frame_delay", Value::Int(1)).is_ok());
    assert!(p.set(dec, "cores", Value::Int(2)).is_err());
}
