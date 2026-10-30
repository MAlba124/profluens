//! The decoder shell driven through the harness with a fake codec that reads one opcode byte per
//! unit.

// Test code.
#![allow(clippy::disallowed_methods)]

use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;

use pf_harness::Harness;
use pf_hdr::side::{FrameMeta, Kind, Writer};
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::element::{Element, ElementDesc, PadDesc};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::memory::{Memory, Pool, PoolConfig};
use profluens_core::time::Timestamp;
use profluens_video::color::{Matrix, Range};
use profluens_video::decoder::{
    self, Codec, Color, Held, HeldLayout, MAX_SLOTS, Out, Picture, Planar, PlaneRef, SILENT_UNITS,
    SLOT_SLACK, Sent, VideoDec, VideoDecError, pack_gray, pack_p010, pack_planes, pack_reduced,
};
use profluens_video::gpuframe::{
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_NV12, DRM_FORMAT_P010, DRM_FORMAT_YUV420, DmabufFormat,
    F_HEIGHT, F_LAYOUT, F_MODIFIER, F_WIDTH, FAMILY_VIDEO_GPU, GpuFrameChannel, GpuFrameHeader,
    LAYOUT_I420_DMABUF, LAYOUT_P010_DMABUF,
};
use profluens_video::{FAMILY, PixelFormat};

const SHOW: u8 = b'S';
// Decodes but shows nothing, like a hidden reference frame.
const HIDE: u8 = b'H';
const BAD: u8 = b'B';
const TWO: u8 = b'T';
const KEEP: u8 = b'K';
// Lends a strided picture stating full range and a BT.2020 matrix.
const LEND: u8 = b'L';
const LEND10: u8 = b'P';
const LEND444: u8 = b'Q';
const LEND422: u8 = b'W';
const FAIL: u8 = b'F';
// Decodes into a pool slot kept as the one reference, or starves when the pool is dry.
const INTO: u8 = b'I';
const INTO_HEAP: u8 = b'J';
const PIN: u8 = b'N';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Decode(u8),
    Reset,
    Finish,
    Release(u32),
    Starve,
}

thread_local! {
    // The harness is threadless, so the codec runs on the test's thread.
    static CALLS: RefCell<Vec<Call>> = const { RefCell::new(Vec::new()) };
}

fn calls() -> Vec<Call> {
    CALLS.with(|c| c.borrow().clone())
}

#[derive(Debug, PartialEq, Eq)]
struct BadUnit;

impl fmt::Display for BadUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("fake unit dropped")
    }
}

impl std::error::Error for BadUnit {}

struct Fake {
    seq: u8,
    kept: Vec<Picture>,
    lent: Vec<Option<[Lent; 3]>>,
    reference: Option<Memory>,
    pinned: Vec<Memory>,
}

struct Lent {
    bytes: Vec<u8>,
    stride: usize,
}

fn lent_sample(fill: u8, p: usize, x: usize, y: usize) -> u8 {
    fill.wrapping_add((p as u8) * 100).wrapping_add((y * 16 + x) as u8)
}

fn lend(w: u8, fill: u8, pad: usize) -> [Lent; 3] {
    let w = w as usize;
    let cw = w.div_ceil(2);
    let mut p = 0;
    [(w, w), (cw, cw), (cw, cw)].map(|(cols, rows)| {
        let stride = cols + pad;
        let mut bytes = vec![0xEE; stride * rows];
        for y in 0..rows {
            for x in 0..cols {
                bytes[y * stride + x] = lent_sample(fill, p, x, y);
            }
        }
        p += 1;
        Lent { bytes, stride }
    })
}

// Interns the colour names as a raw video sink's offer would in a pipeline.
fn lend_ready(h: &mut Harness) {
    h.fix_sink_format(
        FakeDec::IN,
        "fake",
        &[("matrix", ValueDesc::Id("bt2020")), ("range", ValueDesc::Id("full"))],
    );
    h.fix_sink_format(FakeDec::IN, "fake", &[]);
}

fn lent10_sample(fill: u8, p: usize, x: usize, y: usize) -> u16 {
    (u16::from(lent_sample(fill, p, x, y)) * 4 + ((x + 3 * y) & 3) as u16) & 0x3FF
}

fn lend10(w: u8, fill: u8, pad: usize) -> [Lent; 3] {
    let w = w as usize;
    let cw = w.div_ceil(2);
    let mut p = 0;
    [(w, w), (cw, cw), (cw, cw)].map(|(cols, rows)| {
        let stride = 2 * (cols + pad);
        let mut bytes = vec![0xEE; stride * rows];
        for y in 0..rows {
            for x in 0..cols {
                let at = y * stride + 2 * x;
                bytes[at..at + 2].copy_from_slice(&lent10_sample(fill, p, x, y).to_le_bytes());
            }
        }
        p += 1;
        Lent { bytes, stride }
    })
}

fn lend444(w: u8, fill: u8) -> [Lent; 3] {
    let w = w as usize;
    [0, 1, 2].map(|p| {
        let stride = 2 * (w + 1);
        let mut bytes = vec![0xEE; stride * w];
        for y in 0..w {
            for x in 0..w {
                let v = match p {
                    0 => lent10_sample(fill, 0, x, y),
                    1 => 4 * x as u16,
                    _ => 8 * y as u16,
                };
                bytes[y * stride + 2 * x..][..2].copy_from_slice(&v.to_le_bytes());
            }
        }
        Lent { bytes, stride }
    })
}

fn lent10_p010(w: u8, fill: u8) -> Vec<u8> {
    let w = w as usize;
    let cw = w.div_ceil(2);
    let mut out = Vec::new();
    for y in 0..w {
        for x in 0..w {
            out.extend((lent10_sample(fill, 0, x, y) << 6).to_le_bytes());
        }
    }
    for y in 0..cw {
        for x in 0..cw {
            out.extend((lent10_sample(fill, 1, x, y) << 6).to_le_bytes());
            out.extend((lent10_sample(fill, 2, x, y) << 6).to_le_bytes());
        }
    }
    out
}

fn lent_packed(w: u8, fill: u8) -> Vec<u8> {
    let w = w as usize;
    let cw = w.div_ceil(2);
    let mut out = Vec::new();
    for (p, (cols, rows)) in [(w, w), (cw, cw), (cw, cw)].into_iter().enumerate() {
        for y in 0..rows {
            for x in 0..cols {
                out.push(lent_sample(fill, p, x, y));
            }
        }
    }
    out
}

fn picture(w: u8, fill: u8, pts: Timestamp, duration: Timestamp) -> Picture {
    let n = w as usize * w as usize;
    Picture {
        width: w as u32,
        height: w as u32,
        pixfmt: "i420",
        planes: [vec![fill; n], vec![fill + 100; 1], vec![fill + 200; 1]],
        pts,
        duration,
    }
}

static PIXFMTS: [ValueDesc; 3] =
    [ValueDesc::Id("i420"), ValueDesc::Id("p010"), ValueDesc::Id("i422_10")];
static SRC_FIELDS: [FieldDesc; 7] = decoder::src_fields(&PIXFMTS);
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }];
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "fake", fields: &decoder::COLOR_FIELDS }];
static SINKS: [PadDesc; 1] = decoder::sink_pad(&SINK_OFFERS);
static SRCS: [PadDesc; 1] = decoder::src_pad(&SRC_OFFERS);
static DESC: ElementDesc = decoder::desc("fakedec", &SINKS, &SRCS, || Box::new(FakeDec::new()));

type FakeDec = VideoDec<Fake>;

impl Codec for Fake {
    const DESC: &'static ElementDesc = &DESC;

    fn new() -> Self {
        Self { seq: 0, kept: Vec::new(), lent: Vec::new(), reference: None, pinned: Vec::new() }
    }

    fn reset(&mut self) {
        CALLS.with(|c| c.borrow_mut().push(Call::Reset));
        self.seq = 0;
        self.kept.clear();
        self.lent.clear();
        self.reference = None;
        self.pinned.clear();
    }

    fn decode_memory(
        &mut self,
        unit: &Memory,
        pts: Timestamp,
        duration: Timestamp,
        out: &mut Out<'_>,
    ) {
        assert_eq!(unit.len(), 2, "the unit's memory is the unit");
        self.decode(unit.data(), pts, duration, out);
    }

    fn decode(&mut self, unit: &[u8], pts: Timestamp, duration: Timestamp, out: &mut Out<'_>) {
        let (op, w) = (unit[0], unit[1]);
        CALLS.with(|c| c.borrow_mut().push(Call::Decode(op)));
        let fill = self.seq;
        self.seq += 1;
        match op {
            SHOW => out.push(picture(w, fill, pts, duration)),
            HIDE => {}
            BAD => out.warn(BadUnit),
            TWO => {
                out.push(picture(w, fill, pts, duration));
                out.push(picture(w, fill + 50, pts, duration));
            }
            KEEP => self.kept.push(picture(w, fill, pts, duration)),
            LEND => {
                if out.decode_only(pts) {
                    return;
                }
                let handle = self.lent.iter().position(Option::is_none).unwrap_or_else(|| {
                    self.lent.push(None);
                    self.lent.len() - 1
                });
                self.lent[handle] = Some(lend(w, fill, 3));
                let color = Color {
                    range: Some(Range::Full),
                    matrix: Some(Matrix::Bt2020),
                    ..Color::default()
                };
                let held = Held {
                    width: w as u32,
                    height: w as u32,
                    pixfmt: PixelFormat::I420,
                    pts,
                    duration,
                    handle: handle as u32,
                    color,
                    layout: HeldLayout::Native,
                };
                assert!(out.push_held(held), "the point was asked already");
            }
            LEND10 => {
                let handle = self.lent.iter().position(Option::is_none).unwrap_or_else(|| {
                    self.lent.push(None);
                    self.lent.len() - 1
                });
                self.lent[handle] = Some(lend10(w, fill, 3));
                let held = Held {
                    width: w as u32,
                    height: w as u32,
                    pixfmt: PixelFormat::P010,
                    pts,
                    duration,
                    handle: handle as u32,
                    color: Color::default(),
                    layout: HeldLayout::Planar10,
                };
                if !out.push_held(held) {
                    self.lent[handle] = None;
                }
            }
            LEND444 => {
                let handle = self.lent.iter().position(Option::is_none).unwrap_or_else(|| {
                    self.lent.push(None);
                    self.lent.len() - 1
                });
                self.lent[handle] = Some(lend444(w, fill));
                let held = Held {
                    width: w as u32,
                    height: w as u32,
                    pixfmt: PixelFormat::P010,
                    pts,
                    duration,
                    handle: handle as u32,
                    color: Color::default(),
                    layout: HeldLayout::Reduce { chroma_shift: (0, 0), bits: 10 },
                };
                if !out.push_held(held) {
                    self.lent[handle] = None;
                }
            }
            LEND422 => {
                let (pixfmt, layout) =
                    out.place(Planar { bits: 10, chroma_shift: (1, 0) }).expect("placed");
                let handle = self.lent.iter().position(Option::is_none).unwrap_or_else(|| {
                    self.lent.push(None);
                    self.lent.len() - 1
                });
                self.lent[handle] = Some(lend422(w, fill));
                let (width, height, color) = (w as u32, w as u32, Color::default());
                let held = Held {
                    width,
                    height,
                    pixfmt,
                    pts,
                    duration,
                    handle: handle as u32,
                    color,
                    layout,
                };
                if !out.push_held(held) {
                    self.lent[handle] = None;
                }
            }
            FAIL => out.fail(BadUnit),
            INTO | INTO_HEAP | PIN => {
                let planes = lend(w, fill, 3);
                let bytes = planes.iter().map(|l| l.bytes.len()).sum();
                let memory = match op {
                    INTO | PIN => out.frame_memory(bytes),
                    _ => Some(Pool::bounded(bytes, 1).acquire()),
                };
                let Some(mut memory) = memory else {
                    CALLS.with(|c| c.borrow_mut().push(Call::Starve));
                    self.seq -= 1;
                    return out.starve();
                };
                let (mut offsets, mut strides, mut at) = ([0; 3], [0; 3], 0);
                for (p, l) in planes.iter().enumerate() {
                    memory.as_mut_full()[at..at + l.bytes.len()].copy_from_slice(&l.bytes);
                    (offsets[p], strides[p]) = (at, l.stride);
                    at += l.bytes.len();
                }
                memory.set_len(bytes);
                if op == PIN {
                    self.pinned.push(memory.clone());
                } else {
                    self.reference = Some(memory.clone());
                }
                let sent = Sent {
                    memory,
                    width: w as u32,
                    height: w as u32,
                    pixfmt: PixelFormat::I420,
                    layout: HeldLayout::Native,
                    offsets,
                    strides,
                    pts,
                    duration,
                    color: Color::default(),
                };
                out.push_sent(sent);
            }
            _ => unreachable!("unknown fake opcode"),
        }
    }

    fn finish(&mut self, out: &mut Out<'_>) {
        CALLS.with(|c| c.borrow_mut().push(Call::Finish));
        for p in self.kept.drain(..) {
            out.push(p);
        }
    }

    fn held_planes(&self, handle: u32) -> [PlaneRef<'_>; 3] {
        let lent = self.lent[handle as usize].as_ref().expect("a lent picture");
        [0, 1, 2].map(|p| PlaneRef { data: &lent[p].bytes, stride: lent[p].stride })
    }

    fn release_held(&mut self, handle: u32) {
        CALLS.with(|c| c.borrow_mut().push(Call::Release(handle)));
        assert!(self.lent[handle as usize].take().is_some(), "released twice");
    }

    fn for_each_held(&self, f: &mut dyn FnMut(&Memory)) {
        self.reference.iter().chain(&self.pinned).for_each(f);
    }

    fn scan(&mut self, unit: &[u8], scan: &mut decoder::Scan<'_>) {
        if unit[0] == SHOW {
            scan.side.push(Kind::DoviEl, &[]);
        }
    }
}

fn packed(w: u8, fill: u8) -> Vec<u8> {
    let mut v = vec![fill; w as usize * w as usize];
    v.push(fill + 100);
    v.push(fill + 200);
    v
}

fn unit(h: &mut Harness, op: u8, w: u8, pts_ns: u64) -> Buffer {
    let mut b = h.alloc(&[op, w]);
    b.pts = Timestamp::from_nanos(pts_ns);
    b.duration = Timestamp::from_nanos(7);
    b
}

fn push(h: &mut Harness, op: u8, w: u8, pts_ns: u64) {
    let b = unit(h, op, w, pts_ns);
    h.push(b).expect("push");
}

fn announced_dims(h: &Harness) -> (Option<Value>, Option<Value>) {
    let f = h.announced().expect("format announced");
    let v = h.vocabulary();
    (f.get(v.field_id("width").unwrap()), f.get(v.field_id("height").unwrap()))
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

// Exactly one output slot. Two byte input units come from a small size class.
fn one_slot() -> Harness {
    Harness::with_pool(FakeDec::new(), 64, 1)
}

#[test]
fn a_picture_is_packed_plane_after_plane_with_the_unit_timing() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, SHOW, 2, 40);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].memory.data(), packed(2, 0).as_slice());
    assert_eq!(out[0].pts, Timestamp::from_nanos(40));
    assert_eq!(out[0].duration, Timestamp::from_nanos(7));
}

#[test]
fn one_unit_may_show_several_pictures_or_none() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, HIDE, 2, 0);
    assert!(h.drain_outputs().is_empty(), "a hidden frame emits nothing");
    assert!(h.announced().is_none(), "and announces nothing");

    push(&mut h, TWO, 2, 10);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].memory.data(), packed(2, 1).as_slice());
    assert_eq!(out[1].memory.data(), packed(2, 51).as_slice());
    assert!(warnings(&mut h).is_empty());
}

#[test]
fn no_slot_holds_the_picture_and_stops_taking_input() {
    let mut h = one_slot();
    push(&mut h, SHOW, 2, 0);
    push(&mut h, SHOW, 2, 10);
    push(&mut h, SHOW, 2, 20);
    h.crank().expect("crank");
    assert_eq!(calls(), [Call::Decode(SHOW), Call::Decode(SHOW)], "unit 2 not decoded yet");

    let staged = h.drain_outputs();
    assert_eq!(staged.len(), 1, "only unit 0 got out");
    assert_eq!(staged[0].pts, Timestamp::from_nanos(0));
    drop(staged);

    h.crank().expect("crank");
    assert_eq!(calls().len(), 3, "unit 2 decoded once the held picture left");
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].pts, Timestamp::from_nanos(10));
    assert_eq!(out[0].memory.data(), packed(2, 1).as_slice(), "held picture emitted intact");
    drop(out);

    h.crank().expect("crank");
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].pts, Timestamp::from_nanos(20));
    assert_eq!(calls().len(), 3, "nothing decoded twice");
}

#[test]
fn the_format_is_announced_on_the_first_picture() {
    let mut h = Harness::new(FakeDec::new());
    h.fix_sink_format(FakeDec::IN, "fake", &[("matrix", ValueDesc::Id("bt709"))]);
    assert!(h.announced().is_none(), "nothing announced before a picture");

    push(&mut h, SHOW, 2, 0);
    assert_eq!(announced_dims(&h), (Some(Value::Int(2)), Some(Value::Int(2))));
    let f = h.announced().unwrap();
    let v = h.vocabulary();
    assert_eq!(f.get(v.field_id("pixfmt").unwrap()), v.value_id("i420").map(Value::Id));
    assert_eq!(
        f.get(v.field_id("matrix").unwrap()),
        v.value_id("bt709").map(Value::Id),
        "the sink's colour field rides along"
    );
    assert_eq!(f.get(v.field_id("range").unwrap()), None, "an absent colour field stays absent");

    push(&mut h, SHOW, 2, 10);
    assert_eq!(h.format_changes(), 1);
    push(&mut h, SHOW, 3, 20);
    assert_eq!(announced_dims(&h), (Some(Value::Int(3)), Some(Value::Int(3))));
    assert_eq!(h.format_changes(), 2);
    assert_eq!(h.drain_outputs().len(), 3, "every picture still emitted");
}

#[test]
fn a_flush_announces_again_and_an_unchanged_format_is_sent_once() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, SHOW, 2, 0);
    assert_eq!(h.format_changes(), 1);

    for pts in [10, 20] {
        h.push_event(Event::FlushStart).expect("flush");
        push(&mut h, SHOW, 2, pts);
    }
    assert_eq!(h.format_changes(), 1, "downstream saw exactly one FormatChange");

    h.push_event(Event::FlushStart).expect("flush");
    push(&mut h, SHOW, 3, 30);
    assert_eq!(h.format_changes(), 2);
    assert_eq!(announced_dims(&h), (Some(Value::Int(3)), Some(Value::Int(3))));

    h.push_event(Event::FlushStart).expect("flush");
    push(&mut h, SHOW, 2, 40);
    assert_eq!(h.format_changes(), 3);
    assert_eq!(announced_dims(&h), (Some(Value::Int(2)), Some(Value::Int(2))));
    assert_eq!(h.drain_outputs().len(), 5, "every picture still emitted");
}

#[test]
fn a_decode_error_is_one_warning_and_the_stream_continues() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, SHOW, 2, 0);
    push(&mut h, BAD, 2, 10);
    push(&mut h, SHOW, 2, 20);

    let out = h.drain_outputs();
    assert_eq!(out.len(), 2, "the good units on both sides decoded");
    assert_eq!(out[0].pts, Timestamp::from_nanos(0));
    assert_eq!(out[1].pts, Timestamp::from_nanos(20));

    let w = warnings(&mut h);
    assert_eq!(w.len(), 1, "exactly one warning for the one bad unit");
    let Error::ElementFailed { source, .. } = &w[0] else { panic!("typed element failure") };
    assert_eq!(source.downcast_ref::<BadUnit>(), Some(&BadUnit));
}

#[test]
fn flush_start_drops_the_held_picture_and_resets_the_codec() {
    let mut h = one_slot();
    push(&mut h, SHOW, 2, 0);
    push(&mut h, SHOW, 2, 10); // held, no slot
    drop(h.drain_outputs()); // the scheduler discards in-flight output on a seek

    h.push_event(Event::FlushStart).expect("flush");
    assert_eq!(calls().last(), Some(&Call::Reset));
    h.crank().expect("crank");
    assert!(h.drain_outputs().is_empty(), "the held picture did not leak past the flush");

    push(&mut h, SHOW, 2, 1000);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].pts, Timestamp::from_nanos(1000));
    assert_eq!(out[0].memory.data(), packed(2, 0).as_slice(), "codec state restarted from zero");
}

#[test]
fn eos_emits_the_held_picture() {
    let mut h = one_slot();
    push(&mut h, SHOW, 2, 0);
    push(&mut h, SHOW, 2, 10); // held, no slot
    drop(h.drain_outputs());

    let tail = h.eos().expect("eos");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].pts, Timestamp::from_nanos(10));
    assert_eq!(tail[0].memory.data(), packed(2, 1).as_slice());
}

#[test]
fn eos_asks_the_codec_for_what_it_still_holds() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, KEEP, 2, 0);
    push(&mut h, SHOW, 2, 10);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1, "the kept picture is still inside the codec");
    assert_eq!(out[0].pts, Timestamp::from_nanos(10));

    let tail = h.eos().expect("eos");
    assert_eq!(calls().last(), Some(&Call::Finish));
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].pts, Timestamp::from_nanos(0));
    assert_eq!(tail[0].memory.data(), packed(2, 0).as_slice());
}

#[test]
fn a_picture_larger_than_a_pool_slot_is_a_typed_failure() {
    // A 3x3 picture packs to 11 bytes, the slot holds 8.
    let mut h = Harness::with_pool(FakeDec::new(), 8, 4);
    let b = unit(&mut h, SHOW, 3, 0);
    let err = h.push(b).expect_err("the picture cannot fit");
    let Error::ElementFailed { source, .. } = err else { panic!("typed element failure") };
    let Some(VideoDecError::SlotTooSmall { width, height, pixfmt, need, capacity }) =
        source.downcast_ref::<VideoDecError>()
    else {
        panic!("slot too small")
    };
    assert_eq!((*width, *height, *pixfmt, *need, *capacity), (3, 3, "i420", 11, 8));
    assert!(h.drain_outputs().is_empty(), "nothing truncated got out");
}

fn releases() -> Vec<u32> {
    calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Release(h) => Some(h),
            _ => None,
        })
        .collect()
}

#[test]
fn a_held_picture_is_copied_once_from_its_strided_planes_and_released() {
    let mut h = Harness::new(FakeDec::new());
    lend_ready(&mut h);
    forget_calls();
    push(&mut h, LEND, 5, 40);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].memory.data(), lent_packed(5, 0).as_slice(), "rows packed tight, no padding");
    assert_eq!(out[0].memory.len(), 25 + 9 + 9);
    assert_eq!(out[0].pts, Timestamp::from_nanos(40));
    assert_eq!(out[0].duration, Timestamp::from_nanos(7));
    assert_eq!(releases(), [0], "given back once, after the copy");
    let f = h.announced().unwrap();
    let v = h.vocabulary();
    assert_eq!(f.get(v.field_id("pixfmt").unwrap()), v.value_id("i420").map(Value::Id));
    assert_eq!(
        f.get(v.field_id("range").unwrap()),
        v.value_id("full").map(Value::Id),
        "the stream's colour"
    );
}

#[test]
fn the_streams_colour_overrides_the_containers_field_and_leaves_the_rest() {
    let mut h = Harness::new(FakeDec::new());
    lend_ready(&mut h);
    h.fix_sink_format(
        FakeDec::IN,
        "fake",
        &[("matrix", ValueDesc::Id("bt709")), ("transfer", ValueDesc::Id("pq"))],
    );
    push(&mut h, LEND, 2, 0);
    let f = h.announced().unwrap();
    let v = h.vocabulary();
    assert_eq!(
        f.get(v.field_id("matrix").unwrap()),
        v.value_id("bt2020").map(Value::Id),
        "overridden"
    );
    assert_eq!(
        f.get(v.field_id("transfer").unwrap()),
        v.value_id("pq").map(Value::Id),
        "left standing"
    );
    assert_eq!(f.get(v.field_id("range").unwrap()), v.value_id("full").map(Value::Id), "added");
    assert_eq!(f.get(v.field_id("primaries").unwrap()), None);
    let range = v.field_id("range").unwrap();
    push(&mut h, SHOW, 2, 10);
    assert_eq!(h.format_changes(), 2);
    assert_eq!(h.announced().unwrap().get(range), None);
}

#[test]
fn no_slot_holds_the_lent_picture_as_the_codecs_until_the_copy() {
    let mut h = one_slot();
    lend_ready(&mut h);
    forget_calls();
    push(&mut h, LEND, 2, 0); // takes the slot, staged, handle 0 released
    push(&mut h, LEND, 2, 10); // decoded into handle 0 again, waits as the codec's own
    push(&mut h, LEND, 2, 20); // stays in the input
    h.crank().expect("crank");
    assert_eq!(releases(), [0], "the waiting picture is still the codec's");
    assert_eq!(calls().iter().filter(|c| matches!(c, Call::Decode(_))).count(), 2);

    drop(h.drain_outputs());
    h.crank().expect("crank");
    assert_eq!(releases(), [0, 0], "released once the slot came back and the copy was made");
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].pts, Timestamp::from_nanos(10));
    assert_eq!(out[0].memory.data(), lent_packed(2, 1).as_slice());
    drop(out);
    h.crank().expect("crank");
    assert_eq!(h.drain_outputs()[0].pts, Timestamp::from_nanos(20));
    assert_eq!(releases(), [0, 0, 0], "handles are reused");
}

#[test]
fn a_flush_gives_the_held_picture_back_before_the_reset() {
    let mut h = one_slot();
    lend_ready(&mut h);
    forget_calls();
    push(&mut h, LEND, 2, 0);
    push(&mut h, LEND, 2, 10); // held
    drop(h.drain_outputs());
    h.push_event(Event::FlushStart).expect("flush");
    let c = calls();
    let release = c.iter().rposition(|c| matches!(c, Call::Release(_))).expect("released");
    let reset = c.iter().position(|c| *c == Call::Reset).expect("reset");
    assert_eq!(releases().len(), 2, "the staged one after its copy, the held one at the flush");
    assert!(release < reset, "given back, then the codec reset");
    h.crank().expect("crank");
    assert!(h.drain_outputs().is_empty());
}

#[test]
fn a_skip_gives_back_a_held_picture_from_before_the_point() {
    let mut h = one_slot();
    lend_ready(&mut h);
    forget_calls();
    push(&mut h, LEND, 2, 0);
    push(&mut h, LEND, 2, 10); // held
    push(&mut h, LEND, 2, 30); // in the input
    h.skip_to(Timestamp::from_nanos(20)).expect("skip");
    assert_eq!(releases().len(), 2, "the held picture at 10 is before the point");
    assert!(!calls().contains(&Call::Reset));
    drop(h.drain_outputs());
    h.crank().expect("crank");
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].pts, Timestamp::from_nanos(30));
}

#[test]
fn pack_planes_rounds_chroma_up_and_refuses_short_planes() {
    let [y, u, v] = lend(3, 7, 2);
    let planes = [
        PlaneRef { data: &y.bytes, stride: y.stride },
        PlaneRef { data: &u.bytes, stride: u.stride },
        PlaneRef { data: &v.bytes, stride: v.stride },
    ];
    let mut dst = vec![0; 9 + 4 + 4];
    assert_eq!(pack_planes(&mut dst, PixelFormat::I420, 3, 3, &planes), Ok(17));
    assert_eq!(dst, lent_packed(3, 7));

    let mut dst = vec![0; 9];
    assert_eq!(pack_planes(&mut dst, PixelFormat::Gray8, 3, 3, &planes), Ok(9));
    assert_eq!(dst, lent_packed(3, 7)[..9]);

    let exact = PlaneRef { data: &y.bytes[..2 * y.stride + 3], stride: y.stride };
    let mut dst = vec![0; 9];
    assert_eq!(
        pack_planes(&mut dst, PixelFormat::Gray8, 3, 3, &[exact, planes[1], planes[2]]),
        Ok(9)
    );

    let short = PlaneRef { data: &y.bytes[..2 * y.stride + 2], stride: y.stride };
    assert_eq!(
        pack_planes(&mut dst, PixelFormat::Gray8, 3, 3, &[short, planes[1], planes[2]]),
        Err(VideoDecError::PlaneShort { plane: 0, need: 2 * y.stride + 3, have: 2 * y.stride + 2 })
    );
    let narrow = PlaneRef { data: &y.bytes, stride: 2 };
    assert_eq!(
        pack_planes(&mut dst, PixelFormat::Gray8, 3, 3, &[narrow, planes[1], planes[2]]),
        Err(VideoDecError::StrideShort { plane: 0, stride: 2, row: 3 })
    );
}

const REF: u8 = b'P';
const LEAF: u8 = b'b';

// A reference chain and a reorder window. A missed reference shows in every later picture.
struct Gop {
    chain: u8,
    window: Vec<Picture>,
}

const WINDOW: usize = 1;

static GOP_SINK_OFFERS: [OfferDesc; 1] =
    [OfferDesc { family: "gop", fields: &decoder::COLOR_FIELDS }];
static GOP_SINKS: [PadDesc; 1] = decoder::sink_pad(&GOP_SINK_OFFERS);
static GOP_DESC: ElementDesc =
    decoder::desc("gopdec", &GOP_SINKS, &SRCS, || Box::new(GopDec::new()));

type GopDec = VideoDec<Gop>;

impl Gop {
    fn release(&mut self, keep: usize, out: &mut Out<'_>) {
        while self.window.len() > keep {
            let first = (0..self.window.len()).min_by_key(|&i| self.window[i].pts).unwrap();
            let p = self.window.remove(first);
            if !out.decode_only(p.pts) {
                out.push(p);
            }
        }
    }
}

impl Codec for Gop {
    const DESC: &'static ElementDesc = &GOP_DESC;

    fn new() -> Self {
        Self { chain: 1, window: Vec::new() }
    }

    fn reset(&mut self) {
        CALLS.with(|c| c.borrow_mut().push(Call::Reset));
        self.chain = 1;
        self.window.clear();
    }

    fn decode(&mut self, unit: &[u8], pts: Timestamp, duration: Timestamp, out: &mut Out<'_>) {
        let (op, w) = (unit[0], unit[1]);
        CALLS.with(|c| c.borrow_mut().push(Call::Decode(op)));
        let stamp = pts.nanos().unwrap_or(0) as u8;
        let fill = match op {
            REF => {
                self.chain = self.chain.wrapping_mul(31).wrapping_add(stamp) % 50;
                self.chain
            }
            LEAF => (self.chain ^ stamp) % 50,
            _ => unreachable!("unknown gop opcode"),
        };
        self.window.push(picture(w, fill, pts, duration));
        self.release(WINDOW, out);
    }

    fn finish(&mut self, out: &mut Out<'_>) {
        CALLS.with(|c| c.borrow_mut().push(Call::Finish));
        self.release(0, out);
    }

    fn disposable(&self, unit: &[u8]) -> bool {
        unit[0] == LEAF
    }
}

// Decode order, pts in tens of ns. Each reference is followed by the two leaves shown ahead of it.
const STREAM: [(u8, u64); 10] = [
    (REF, 0),
    (REF, 30),
    (LEAF, 10),
    (LEAF, 20),
    (REF, 60),
    (LEAF, 40),
    (LEAF, 50),
    (REF, 90),
    (LEAF, 70),
    (LEAF, 80),
];

fn ns(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

fn play(h: &mut Harness, from: usize) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut take = |h: &mut Harness, tail: Vec<Buffer>| {
        let staged = h.drain_outputs().into_iter().chain(tail);
        out.extend(staged.map(|b| (b.pts.0, b.memory.data().to_vec())));
    };
    for &(op, pts) in &STREAM[from..] {
        push(h, op, 2, pts);
        take(h, Vec::new());
    }
    for _ in 0..STREAM.len() {
        h.crank().expect("crank");
        take(h, Vec::new());
    }
    assert_eq!(h.input_len(), 0, "every unit was taken");
    let tail = h.eos().expect("eos");
    take(h, tail);
    for _ in 0..STREAM.len() {
        h.crank().expect("crank");
        take(h, Vec::new());
    }
    out
}

fn decoded() -> usize {
    calls().iter().filter(|c| matches!(c, Call::Decode(_))).count()
}

fn forget_calls() {
    CALLS.with(|c| c.borrow_mut().clear());
}

#[test]
fn before_the_point_a_picture_is_decoded_and_goes_nowhere() {
    let mut full = Harness::new(GopDec::new());
    let all = play(&mut full, 0);
    assert_eq!(
        all.iter().map(|o| o.0).collect::<Vec<_>>(),
        [0, 10, 20, 30, 40, 50, 60, 70, 80, 90]
    );
    forget_calls();

    let mut h = Harness::new(GopDec::new());
    h.seek(0, ns(45)).expect("seek");
    let got = play(&mut h, 0);
    assert_eq!(got, all[5..], "from the point on, bit for bit what a full decode shows");
    assert_eq!(decoded(), STREAM.len() - 3);
}

#[test]
fn the_point_is_judged_on_the_picture_and_not_on_the_unit_that_released_it() {
    let mut h = Harness::new(GopDec::new());
    h.seek(0, ns(30)).expect("seek");
    forget_calls();
    push(&mut h, REF, 2, 0);
    push(&mut h, REF, 2, 30);
    assert!(h.drain_outputs().is_empty(), "the picture at 0 left the window as decode-only");
    push(&mut h, LEAF, 2, 10);
    push(&mut h, LEAF, 2, 20);
    assert!(h.drain_outputs().is_empty());
    assert_eq!(decoded(), 2, "neither leaf was decoded");
    push(&mut h, REF, 2, 60);
    let out = h.drain_outputs();
    assert_eq!(out.iter().map(|b| b.pts.0).collect::<Vec<_>>(), [30]);
}

#[test]
fn the_format_is_announced_by_the_first_picture_that_is_output() {
    let mut h = Harness::new(GopDec::new());
    h.seek(0, ns(60)).expect("seek");
    for &(op, pts) in &STREAM[..4] {
        push(&mut h, op, 2, pts);
    }
    assert!(h.announced().is_none(), "a decode-only picture announces nothing");
    assert_eq!(h.format_changes(), 0);
    for &(op, pts) in &STREAM[4..] {
        push(&mut h, op, 2, pts);
    }
    assert_eq!(announced_dims(&h), (Some(Value::Int(2)), Some(Value::Int(2))));
    assert_eq!(h.format_changes(), 1, "announced once, ahead of the picture at the point");
    assert_eq!(h.drain_outputs().first().map(|b| b.pts.0), Some(60));
}

#[test]
fn a_decode_only_unit_is_decoded_and_its_picture_is_not_shown() {
    let mut full = Harness::new(GopDec::new());
    let all = play(&mut full, 0);
    forget_calls();
    let mut h = Harness::new(GopDec::new());
    let mut out = Vec::new();
    for &(op, pts) in &STREAM {
        let mut b = unit(&mut h, op, 2, pts);
        if pts < 30 {
            b.flags = BufferFlags::DECODE_ONLY;
        }
        h.push(b).expect("push");
        out.extend(h.drain_outputs().into_iter().map(|b| (b.pts.0, b.memory.data().to_vec())));
    }
    out.extend(h.eos().expect("eos").into_iter().map(|b| (b.pts.0, b.memory.data().to_vec())));
    assert_eq!(out, all[3..], "from 30 on, bit for bit what a full decode shows");
    assert_eq!(decoded(), STREAM.len() - 2, "the flagged leaves at 10 and 20 were skipped");
}

#[test]
fn a_decode_only_stamp_hides_the_first_picture_under_it() {
    let mut h = Harness::new(GopDec::new());
    let mut hidden = unit(&mut h, REF, 2, 30);
    hidden.flags = BufferFlags::DECODE_ONLY;
    h.push(hidden).expect("push");
    push(&mut h, REF, 2, 30);
    push(&mut h, REF, 2, 60);
    let mut out: Vec<u64> = h.drain_outputs().iter().map(|b| b.pts.0).collect();
    out.extend(h.eos().expect("eos").iter().map(|b| b.pts.0));
    assert_eq!(out, [30, 60]);
}

#[test]
fn a_decode_only_picture_takes_no_pool_slot() {
    let mut h = Harness::with_pool(GopDec::new(), 64, 1);
    h.seek(0, ns(60)).expect("seek");
    forget_calls();
    let held = h.alloc(&[0; 64]);
    for &(op, pts) in &STREAM[..4] {
        push(&mut h, op, 2, pts);
    }
    assert_eq!(h.input_len(), 0, "every unit before the point was taken with no slot free");
    assert_eq!(decoded(), 2, "the two references were decoded");
    drop(held);
}

#[test]
fn a_skip_keeps_the_codec_and_drops_what_it_holds_from_before_the_point() {
    let mut full = Harness::new(GopDec::new());
    let all = play(&mut full, 0);
    forget_calls();

    let mut h = Harness::with_pool(GopDec::new(), 64, 1);
    for &(op, pts) in &STREAM[..4] {
        push(&mut h, op, 2, pts);
    }
    h.skip_to(ns(50)).expect("skip");
    assert!(!calls().contains(&Call::Reset), "a skip is no flush");
    drop(h.drain_outputs()); // the picture at 0, which the sink drops
    let got = play(&mut h, 4);
    assert_eq!(got, all[5..], "the held 10 and the windowed 30 never left");
    assert_eq!(decoded(), STREAM.len() - 2);
}

#[test]
fn a_skip_backwards_is_applied_as_given() {
    let mut full = Harness::new(GopDec::new());
    let all = play(&mut full, 0);
    forget_calls();

    let mut h = Harness::new(GopDec::new());
    h.seek(0, ns(70)).expect("seek");
    forget_calls();
    for &(op, pts) in &STREAM[..4] {
        push(&mut h, op, 2, pts);
    }
    assert!(h.drain_outputs().is_empty());
    h.skip_to(ns(40)).expect("skip");
    let got = play(&mut h, 4);
    assert_eq!(got, all[4..], "40 on is shown, and 30 was already gone");
    assert_eq!(decoded(), STREAM.len() - 2, "only the leaves at 10 and 20 were skipped");
}

#[test]
fn a_unit_with_no_pts_is_never_before_the_point() {
    let mut h = Harness::new(GopDec::new());
    h.seek(0, ns(1000)).expect("seek");
    forget_calls();
    let leaf = h.alloc(&[LEAF, 2]);
    h.push(leaf).expect("push");
    assert_eq!(decoded(), 1, "it cannot be placed, so it is decoded");
    assert_eq!(h.eos().expect("eos").len(), 1, "and shown");
}

#[test]
fn with_no_point_a_disposable_unit_is_decoded_like_any_other() {
    let mut h = Harness::new(GopDec::new());
    assert_eq!(play(&mut h, 0).len(), STREAM.len());
    assert_eq!(decoded(), STREAM.len());
}

#[test]
fn dmabuf_mode_hands_the_sink_the_slot_as_a_dmabuf() {
    let dev = match pf_memory::udmabuf::device() {
        Ok(dev) => dev,
        Err(e) => return eprintln!("skip: {e}"),
    };
    let formats = [
        DmabufFormat { fourcc: DRM_FORMAT_NV12, modifiers: vec![DRM_FORMAT_MOD_LINEAR] },
        DmabufFormat { fourcc: DRM_FORMAT_YUV420, modifiers: vec![DRM_FORMAT_MOD_LINEAR] },
    ];
    let ch = GpuFrameChannel::new();
    let dec = FakeDec::new().with_dmabuf(Arc::clone(&ch), &formats);
    assert!(OfferDesc::is_probed(dec.desc().srcs[0].offers), "the src pad is probed");
    assert_eq!(dec.desc().name, "fakedec");
    let page = pf_memory::udmabuf::align();
    let config =
        PoolConfig { align: page, allocator: dev.allocator(), ..PoolConfig::heap(page, 4) };
    let mut h = Harness::with_pool_config(dec, config);
    let fixed = [
        (F_WIDTH, ValueDesc::Int(8)),
        (F_HEIGHT, ValueDesc::Int(8)),
        (F_LAYOUT, ValueDesc::Id(LAYOUT_I420_DMABUF)),
        (F_MODIFIER, ValueDesc::Int(0)),
    ];
    h.fix_src_format(FakeDec::OUT, FAMILY_VIDEO_GPU, &fixed);
    let mut rx = ch.take_rx().expect("the sink's end");

    let hidden = h.alloc(&[HIDE, 16]);
    h.push(hidden).expect("decoded, nothing shown");
    let unit = h.alloc(&[SHOW, 16]);
    h.push(unit).expect("a picture");
    let out = h.pull(FakeDec::OUT).expect("the descriptor");
    let header = GpuFrameHeader::from_bytes(out.memory.data()).expect("a header");
    assert_eq!((header.disp_w, header.disp_h, header.coded_w), (16, 16, 16));
    let announced = h.announced().expect("video/gpu announced");
    assert_eq!(h.vocabulary().family_name(announced.family), Some(FAMILY_VIDEO_GPU));

    let frame = rx.take(header.token).expect("the frame of the descriptor");
    assert_eq!(frame.drm_format, DRM_FORMAT_YUV420);
    assert_eq!(frame.fds.len(), 1, "one object, the slot");
    let planes: Vec<(u32, u32, u32)> =
        frame.planes.iter().map(|p| (p.object_index, p.offset, p.pitch)).collect();
    assert_eq!(planes, [(0, 0, 16), (0, 256, 8), (0, 320, 8)], "packed back to back");

    // SAFETY: the sink's dup of the slot's dmabuf, mapped read-only and unmapped below.
    let view = unsafe {
        libc::mmap(std::ptr::null_mut(), page, libc::PROT_READ, libc::MAP_SHARED, frame.fds[0], 0)
    };
    assert_ne!(view, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
    let bytes = unsafe { std::slice::from_raw_parts(view as *const u8, page) };
    assert!(bytes[..256].iter().all(|&b| b == 1), "the Y plane through the dmabuf");
    assert_eq!(&bytes[256..258], &[101, 201], "the chroma bytes behind it");
    unsafe { libc::munmap(view, page) };
    drop(frame);
    assert!(h.pull(FakeDec::OUT).is_none());
}

#[test]
fn dmabuf_mode_exports_a_picture_decoded_into_the_pool_as_it_lies() {
    let dev = match pf_memory::udmabuf::device() {
        Ok(dev) => dev,
        Err(e) => return eprintln!("skip: {e}"),
    };
    let formats =
        [DmabufFormat { fourcc: DRM_FORMAT_YUV420, modifiers: vec![DRM_FORMAT_MOD_LINEAR] }];
    let ch = GpuFrameChannel::new();
    let dec = FakeDec::new().with_dmabuf(Arc::clone(&ch), &formats);
    let page = pf_memory::udmabuf::align();
    let config =
        PoolConfig { align: page, allocator: dev.allocator(), ..PoolConfig::heap(page, 4) };
    let mut h = Harness::with_pool_config(dec, config);
    let fixed = [
        (F_WIDTH, ValueDesc::Int(8)),
        (F_HEIGHT, ValueDesc::Int(8)),
        (F_LAYOUT, ValueDesc::Id(LAYOUT_I420_DMABUF)),
        (F_MODIFIER, ValueDesc::Int(0)),
    ];
    h.fix_src_format(FakeDec::OUT, FAMILY_VIDEO_GPU, &fixed);
    let mut rx = ch.take_rx().expect("the sink's end");
    let unit = h.alloc(&[INTO, 16]);
    h.push(unit).expect("a picture");
    let out = h.pull(FakeDec::OUT).expect("the descriptor");
    let header = GpuFrameHeader::from_bytes(out.memory.data()).expect("a header");
    let frame = rx.take(header.token).expect("the frame of the descriptor");
    assert_eq!(frame.drm_format, DRM_FORMAT_YUV420);
    let planes: Vec<(u32, u32, u32)> =
        frame.planes.iter().map(|p| (p.object_index, p.offset, p.pitch)).collect();
    assert_eq!(planes, [(0, 0, 19), (0, 304, 11), (0, 392, 11)], "the codec's own layout");
    // SAFETY: the sink's dup of the slot's dmabuf, mapped read-only and unmapped below.
    let view = unsafe {
        libc::mmap(std::ptr::null_mut(), page, libc::PROT_READ, libc::MAP_SHARED, frame.fds[0], 0)
    };
    assert_ne!(view, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
    let bytes = unsafe { std::slice::from_raw_parts(view as *const u8, page) };
    let want = lend(16, 0, 3);
    assert_eq!(&bytes[..304], want[0].bytes.as_slice(), "Y with its padding");
    assert_eq!(&bytes[304..392], want[1].bytes.as_slice(), "Cb");
    assert_eq!(&bytes[392..480], want[2].bytes.as_slice(), "Cr");
    unsafe { libc::munmap(view, page) };
    drop(frame);
}

#[test]
fn dmabuf_mode_packs_a_picture_in_memory_of_the_codecs_own() {
    let dev = match pf_memory::udmabuf::device() {
        Ok(dev) => dev,
        Err(e) => return eprintln!("skip: {e}"),
    };
    let formats =
        [DmabufFormat { fourcc: DRM_FORMAT_YUV420, modifiers: vec![DRM_FORMAT_MOD_LINEAR] }];
    let ch = GpuFrameChannel::new();
    let dec = FakeDec::new().with_dmabuf(Arc::clone(&ch), &formats);
    let page = pf_memory::udmabuf::align();
    let config =
        PoolConfig { align: page, allocator: dev.allocator(), ..PoolConfig::heap(page, 4) };
    let mut h = Harness::with_pool_config(dec, config);
    let fixed = [
        (F_WIDTH, ValueDesc::Int(8)),
        (F_HEIGHT, ValueDesc::Int(8)),
        (F_LAYOUT, ValueDesc::Id(LAYOUT_I420_DMABUF)),
        (F_MODIFIER, ValueDesc::Int(0)),
    ];
    h.fix_src_format(FakeDec::OUT, FAMILY_VIDEO_GPU, &fixed);
    let mut rx = ch.take_rx().expect("the sink's end");
    let unit = h.alloc(&[INTO_HEAP, 16]);
    h.push(unit).expect("a picture");
    let out = h.pull(FakeDec::OUT).expect("the descriptor");
    let header = GpuFrameHeader::from_bytes(out.memory.data()).expect("a header");
    let frame = rx.take(header.token).expect("the frame of the descriptor");
    let planes: Vec<(u32, u32, u32)> =
        frame.planes.iter().map(|p| (p.object_index, p.offset, p.pitch)).collect();
    assert_eq!(planes, [(0, 0, 16), (0, 256, 8), (0, 320, 8)], "packed");
    // SAFETY: the sink's dup of the slot's dmabuf, mapped read-only and unmapped below.
    let view = unsafe {
        libc::mmap(std::ptr::null_mut(), page, libc::PROT_READ, libc::MAP_SHARED, frame.fds[0], 0)
    };
    assert_ne!(view, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
    let bytes = unsafe { std::slice::from_raw_parts(view as *const u8, page) };
    let want = lent_packed(16, 0);
    assert_eq!(&bytes[..want.len()], want.as_slice());
    unsafe { libc::munmap(view, page) };
    drop(frame);
    assert!(warnings(&mut h).is_empty());
}

#[test]
fn a_picture_decoded_into_the_pool_is_packed_once_on_a_raw_link() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, INTO, 5, 40);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].memory.data(), lent_packed(5, 0).as_slice());
    assert_eq!(out[0].pts, Timestamp::from_nanos(40));
}

#[test]
fn a_codec_out_of_slots_is_given_the_unit_again() {
    let slack = SLOT_SLACK as usize;
    let mut h = Harness::with_pool(FakeDec::new(), 256, 1 + SLOT_SLACK);
    for i in 0..slack {
        push(&mut h, INTO, 4, i as u64 * 10);
    }
    push(&mut h, INTO, 4, slack as u64 * 10);
    let mut expected = vec![Call::Decode(INTO); slack + 1];
    expected.push(Call::Starve);
    assert_eq!(calls(), expected);
    push(&mut h, SHOW, 4, slack as u64 * 10 + 10);
    h.crank().expect("crank");
    let decodes: Vec<Call> = calls().into_iter().filter(|c| matches!(c, Call::Decode(_))).collect();
    assert!(
        decodes.iter().all(|c| *c == Call::Decode(INTO)),
        "the show unit waits behind the starved unit"
    );
    let staged = h.drain_outputs();
    assert_eq!(staged.len(), slack, "only the units before got out");
    drop(staged);
    h.crank().expect("crank");
    let out = h.drain_outputs();
    assert_eq!(out.len(), 2, "the starved unit and the show unit behind it");
    assert_eq!(out[0].pts, Timestamp::from_nanos(slack as u64 * 10));
    assert_eq!(
        out[0].memory.data(),
        lent_packed(4, slack as u8).as_slice(),
        "decoded as it would have been"
    );
    let calls = calls();
    let retried = calls.iter().rposition(|c| *c == Call::Decode(INTO));
    let shown = calls.iter().position(|c| *c == Call::Decode(SHOW));
    assert!(retried < shown && shown.is_some(), "unit 1 went again before unit 2, {calls:?}");
    assert_eq!(calls[retried.unwrap_or(0) - 1], Call::Starve, "the decode before it starved");
}

#[test]
fn a_pool_the_decoder_fills_by_itself_grows_to_its_need() {
    let mut h = Harness::with_pool(FakeDec::new(), 256, 1);
    push(&mut h, INTO, 4, 0);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1, "packed into a slot the growth gave");
    assert_eq!(out[0].memory.data(), lent_packed(4, 0).as_slice());
    assert_eq!(h.pool_stats().max_slots, 1 + SLOT_SLACK as u64, "held plus the slack");
    assert!(!calls().contains(&Call::Starve), "the codec never starved, the shell packed");
    let mut h = Harness::with_pool(FakeDec::new(), 256, 1);
    h.skip_to(Timestamp::from_nanos(1000)).expect("skip");
    let grown = 1 + SLOT_SLACK;
    for _ in 0..=grown {
        push(&mut h, PIN, 4, 0);
    }
    assert!(h.drain_outputs().is_empty(), "all before the point");
    assert_eq!(
        calls().iter().filter(|c| **c == Call::Starve).count(),
        2,
        "starved at 1 and at {grown} held"
    );
    assert_eq!(
        calls().iter().filter(|c| **c == Call::Decode(PIN)).count(),
        grown as usize + 3,
        "each went again"
    );
    assert_eq!(h.pool_stats().max_slots, u64::from(grown + SLOT_SLACK));
}

#[test]
fn a_pool_grows_no_further_than_the_most_slots() {
    let mut h = Harness::with_pool(FakeDec::new(), 256, 2);
    let mut failed = None;
    for i in 0..=MAX_SLOTS {
        let unit = unit(&mut h, PIN, 4, u64::from(i) * 10);
        if let Err(e) = h.push(unit) {
            failed = Some((i, e));
            break;
        }
        drop(h.drain_outputs());
    }
    let (at, error) = failed.expect("the pool is too small at the ceiling");
    let Error::ElementFailed { source, .. } = &error else { panic!("{error:?}") };
    let Some(&VideoDecError::PoolTooSmall { slots, held }) = source.downcast_ref::<VideoDecError>()
    else {
        panic!("{error}")
    };
    assert_eq!(slots, MAX_SLOTS as u64, "grown to the ceiling and no further");
    assert!(
        held as u32 + SLOT_SLACK > MAX_SLOTS && held as u32 <= MAX_SLOTS,
        "pinned under the slack, {held}"
    );
    assert_eq!(at as usize + 1, held, "the unit that pinned the slot under the slack");
    assert_eq!(h.pool_stats().max_slots, MAX_SLOTS as u64);
}

#[test]
fn a_slot_out_downstream_is_backpressure_only_past_the_slack() {
    let mut h = Harness::with_pool(FakeDec::new(), 256, 2);
    push(&mut h, INTO, 4, 0);
    push(&mut h, INTO, 4, 10);
    assert_eq!(h.drain_outputs().len(), 2, "the second unit decoded into the grown pool");
    assert_eq!(h.pool_stats().max_slots, 1 + u64::from(SLOT_SLACK), "the reference plus the slack");
    let slack = 1 + SLOT_SLACK;
    let mut h = Harness::with_pool(FakeDec::new(), 256, slack);
    for i in 0..slack {
        push(&mut h, INTO, 4, u64::from(i) * 10);
    }
    assert_eq!(
        calls().last(),
        Some(&Call::Starve),
        "the unit past the slack waits for a staged slot"
    );
    assert_eq!(h.pool_stats().max_slots, u64::from(slack), "nothing grew, the slots come back");
    assert_eq!(h.drain_outputs().len(), slack as usize - 1);
    h.crank().expect("the slots came back, decoding goes on");
    assert_eq!(h.drain_outputs().len(), 1);
}

#[test]
fn pack_p010_shifts_every_sample_up_and_interleaves_the_chroma() {
    for w in [1u8, 2, 5, 16] {
        let lent = lend10(w, 9, 5);
        let planes = [0, 1, 2].map(|p| PlaneRef { data: &lent[p].bytes, stride: lent[p].stride });
        let want = lent10_p010(w, 9);
        let mut dst = vec![0xAA; want.len() + 8];
        let n = pack_p010(&mut dst, w as u32, w as u32, &planes).expect("packed");
        assert_eq!(n, want.len(), "{w}: bytes written");
        assert_eq!(&dst[..n], want.as_slice(), "{w}: P010");
        assert!(dst[n..].iter().all(|&b| b == 0xAA), "{w}: nothing past the picture");
        let low = dst[..n].as_chunks::<2>().0.iter().all(|s| u16::from_le_bytes(*s) & 0x3F == 0);
        assert!(low, "{w}: the low six bits are zero");
    }
    let lent = lend10(4, 0, 0);
    let short = [
        PlaneRef { data: &lent[0].bytes[..10], stride: 8 },
        PlaneRef { data: &lent[1].bytes, stride: 4 },
        PlaneRef { data: &lent[2].bytes, stride: 4 },
    ];
    let mut dst = vec![0; 64];
    assert_eq!(
        pack_p010(&mut dst, 4, 4, &short),
        Err(VideoDecError::PlaneShort { plane: 0, need: 32, have: 10 })
    );
}

fn planes_of(lent: &[Lent; 3]) -> [PlaneRef<'_>; 3] {
    [0, 1, 2].map(|p| PlaneRef { data: &lent[p].bytes, stride: lent[p].stride })
}

fn planar(
    dims: [(usize, usize); 3],
    bytes: usize,
    f: impl Fn(usize, usize, usize) -> u16,
) -> [Lent; 3] {
    let mut p = 0;
    dims.map(|(cols, rows)| {
        let stride = bytes * (cols + 1);
        let mut out = vec![0xEE; stride * rows];
        for y in 0..rows {
            for x in 0..cols {
                let v = f(p, x, y);
                match bytes {
                    1 => out[y * stride + x] = v as u8,
                    _ => out[y * stride + 2 * x..][..2].copy_from_slice(&v.to_le_bytes()),
                }
            }
        }
        p += 1;
        Lent { bytes: out, stride }
    })
}

#[test]
fn pack_reduced_filters_444_chroma_onto_the_420_sites() {
    let lent = planar([(6, 3); 3], 1, |p, x, y| match p {
        0 => (y * 6 + x) as u16,
        1 => 10 * x as u16,
        _ => 16 * y as u16,
    });
    let mut dst = vec![0xAA; 40];
    let n = pack_reduced(&mut dst, 6, 3, (0, 0), 8, PixelFormat::I420, &planes_of(&lent))
        .expect("packed");
    assert_eq!(n, 18 + 2 * 3 * 2, "I420 of 6x3");
    assert_eq!(&dst[..18], (0..18).collect::<Vec<u8>>().as_slice(), "luma");
    assert_eq!(&dst[18..24], &[3, 20, 40, 3, 20, 40], "Cb 1 2 1 across, the left edge clamped");
    assert_eq!(
        &dst[24..30],
        &[8, 8, 8, 32, 32, 32],
        "Cr the mean of rows 0 and 1, then row 2 alone"
    );
    assert!(dst[n..].iter().all(|&b| b == 0xAA), "nothing past the picture");
}

#[test]
fn pack_reduced_takes_422_chroma_columns_as_they_are() {
    let lent = planar([(4, 4), (2, 4), (2, 4)], 2, |p, x, y| match p {
        0 => 0x3FF,
        1 => (100 * x + y) as u16,
        _ => (200 + 100 * x + y) as u16,
    });
    let mut dst = vec![0; 2 * 16 + 4 * 4];
    let n = pack_reduced(&mut dst, 4, 4, (1, 0), 10, PixelFormat::P010, &planes_of(&lent))
        .expect("packed");
    assert_eq!(n, dst.len());
    assert!(
        dst[..32].as_chunks::<2>().0.iter().all(|s| u16::from_le_bytes(*s) == 0x3FF << 6),
        "luma"
    );
    let uv: Vec<u16> =
        dst[32..].as_chunks::<2>().0.iter().map(|s| u16::from_le_bytes(*s) >> 6).collect();
    // Rows 2y and 2y+1 of 100x + y average to 100x + 2y, rounded up from the half.
    assert_eq!(uv, [1, 201, 101, 301, 3, 203, 103, 303]);
}

#[test]
fn pack_reduced_narrows_deep_samples_to_i420_rounded() {
    let words = |ws: &[u16]| -> Vec<u8> { ws.iter().flat_map(|w| w.to_le_bytes()).collect() };
    let (y, u, v) = (words(&[512, 513, 514, 1023]), words(&[2]), words(&[1021]));
    let planes = [
        PlaneRef { data: &y, stride: 4 },
        PlaneRef { data: &u, stride: 2 },
        PlaneRef { data: &v, stride: 2 },
    ];
    let mut dst = [0u8; 6];
    assert_eq!(pack_reduced(&mut dst, 2, 2, (1, 1), 10, PixelFormat::I420, &planes), Ok(6));
    assert_eq!(dst, [128, 128, 129, 255, 1, 255]);
    let err = pack_reduced(&mut dst, 2, 2, (1, 1), 8, PixelFormat::P010, &planes);
    assert!(matches!(err, Err(VideoDecError::HeldLayout { .. })), "8 bits do not widen to P010");
}

#[test]
fn pack_gray_fills_neutral_chroma() {
    let luma = [1, 2, 3, 0xEE, 4, 5, 6, 0xEE, 7, 8, 9, 0xEE];
    let planes = [PlaneRef { data: &luma, stride: 4 }, PlaneRef::EMPTY, PlaneRef::EMPTY];
    let mut dst = [0u8; 9 + 2 * 4];
    assert_eq!(pack_gray(&mut dst, 3, 3, &planes), Ok(17));
    assert_eq!(&dst[..9], &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    assert!(dst[9..].iter().all(|c| *c == 128));
}

#[test]
fn pack_reduced_moves_twelve_bits_up_by_four() {
    let lent = planar([(2, 2); 3], 2, |p, _, _| if p == 0 { 0xFFF } else { 0x800 });
    let mut dst = vec![0; 8 + 4];
    pack_reduced(&mut dst, 2, 2, (0, 0), 12, PixelFormat::P010, &planes_of(&lent)).expect("packed");
    let words: Vec<u16> = dst.as_chunks::<2>().0.iter().map(|s| u16::from_le_bytes(*s)).collect();
    assert_eq!(words, [0xFFF0, 0xFFF0, 0xFFF0, 0xFFF0, 0x8000, 0x8000]);
    let small = planar([(2, 2), (1, 1), (1, 1)], 2, |_, _, _| 0);
    let err = pack_reduced(&mut dst, 2, 2, (0, 0), 12, PixelFormat::P010, &planes_of(&small));
    assert_eq!(err, Err(VideoDecError::PlaneShort { plane: 1, need: 8, have: 4 }));
}

#[test]
fn a_444_held_picture_goes_out_as_p010_reduced_to_420() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, LEND444, 6, 40);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    let data = out[0].memory.data();
    assert_eq!(data.len(), 2 * 36 + 4 * 9);
    let luma: Vec<u16> = (0..36).map(|i| lent10_sample(0, 0, i % 6, i / 6) << 6).collect();
    let words: Vec<u16> = data.as_chunks::<2>().0.iter().map(|s| u16::from_le_bytes(*s)).collect();
    assert_eq!(&words[..36], luma.as_slice());
    let uv: Vec<u16> = words[36..].iter().map(|w| w >> 6).collect();
    // Cb 4x is 1 2 1 filtered to 8x, left edge clamped. Cr 8y over rows 2y and 2y+1 is 16y + 4.
    assert_eq!(uv, [1, 4, 8, 4, 16, 4, 1, 20, 8, 20, 16, 20, 1, 36, 8, 36, 16, 36]);
    let f = h.announced().expect("announced");
    let v = h.vocabulary();
    let Some(Value::Id(pixfmt)) = f.get(v.field_id("pixfmt").unwrap()) else { panic!("pixfmt") };
    assert_eq!(v.value_name(pixfmt), Some("p010"));
    assert_eq!(calls().last(), Some(&Call::Release(0)), "given back after the copy");
}

#[test]
fn a_ten_bit_held_picture_goes_out_as_p010() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, LEND10, 6, 40);
    let out = h.drain_outputs();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].memory.data(), lent10_p010(6, 0).as_slice());
    let f = h.announced().expect("announced");
    let v = h.vocabulary();
    let Some(Value::Id(pixfmt)) = f.get(v.field_id("pixfmt").unwrap()) else { panic!("pixfmt") };
    assert_eq!(v.value_name(pixfmt), Some("p010"));
    assert_eq!(calls().last(), Some(&Call::Release(0)), "given back after the copy");
}

fn lend422(w: u8, fill: u8) -> [Lent; 3] {
    let (w, cw) = (w as usize, (w as usize).div_ceil(2));
    planar([(w, w), (cw, w), (cw, w)], 2, |p, x, y| lent10_sample(fill, p, x, y))
}

fn announced_pixfmt(h: &Harness) -> String {
    let f = h.announced().expect("announced");
    let v = h.vocabulary();
    let Some(Value::Id(id)) = f.get(v.field_id("pixfmt").unwrap()) else { panic!("pixfmt") };
    v.value_name(id).expect("named").to_string()
}

#[test]
fn place_takes_the_format_that_names_the_layout_where_the_link_takes_it() {
    use profluens_video::PixelFormats;
    let p422 = Planar { bits: 10, chroma_shift: (1, 0) };
    let both = PixelFormats::of_names(["i420", "p010", "i422", "i422_10"]);
    let reduce = |bits| HeldLayout::Reduce { chroma_shift: (1, 0), bits };
    assert_eq!(decoder::place(p422, both), Some((PixelFormat::I422P10, HeldLayout::Native)));
    assert_eq!(
        decoder::place(p422, PixelFormats::of_names(["i422"])),
        Some((PixelFormat::P010, reduce(10)))
    );
    assert_eq!(
        decoder::place(p422, PixelFormats::NONE),
        Some((PixelFormat::P010, reduce(10))),
        "nothing said"
    );
    let p8 = Planar { bits: 8, chroma_shift: (1, 0) };
    assert_eq!(decoder::place(p8, both), Some((PixelFormat::I422, HeldLayout::Native)));
    assert_eq!(decoder::place(p8, PixelFormats::NONE), Some((PixelFormat::I420, reduce(8))));
    let p420 = |bits| Planar { bits, chroma_shift: (1, 1) };
    assert_eq!(
        decoder::place(p420(8), PixelFormats::NONE),
        Some((PixelFormat::I420, HeldLayout::Native))
    );
    assert_eq!(decoder::place(p420(10), both), Some((PixelFormat::P010, HeldLayout::Planar10)));
    let p444 = Planar { bits: 12, chroma_shift: (0, 0) };
    assert_eq!(
        decoder::place(p444, both),
        Some((PixelFormat::P010, HeldLayout::Reduce { chroma_shift: (0, 0), bits: 12 }))
    );
    let i420 = PixelFormats::of_names(["i420", "gray8"]);
    assert_eq!(
        decoder::place(p420(10), i420),
        Some((PixelFormat::I420, HeldLayout::Reduce { chroma_shift: (1, 1), bits: 10 }))
    );
    assert_eq!(
        decoder::place(p422, PixelFormats::of_names(["i422", "i420"])),
        Some((PixelFormat::I420, reduce(10)))
    );
    assert_eq!(
        decoder::place(p444, i420),
        Some((PixelFormat::I420, HeldLayout::Reduce { chroma_shift: (0, 0), bits: 12 }))
    );
    assert_eq!(decoder::place(Planar { bits: 17, chroma_shift: (1, 1) }, both), None);
    assert_eq!(decoder::place(Planar { bits: 8, chroma_shift: (2, 1) }, both), None);
}

#[test]
fn a_422_picture_goes_out_as_decoded_only_where_the_link_takes_it() {
    let tight: Vec<u8> = {
        let lent = lend422(6, 0);
        let mut out = vec![0; 2 * 36 + 2 * 2 * 18];
        let n =
            pack_planes(&mut out, PixelFormat::I422P10, 6, 6, &planes_of(&lent)).expect("packed");
        assert_eq!(n, out.len());
        out
    };
    let reduced: Vec<u8> = {
        let lent = lend422(6, 0);
        let mut out = vec![0; 2 * 36 + 4 * 9];
        pack_reduced(&mut out, 6, 6, (1, 0), 10, PixelFormat::P010, &planes_of(&lent))
            .expect("packed");
        out
    };
    let takes = |names: &[&'static str]| names.iter().map(|n| ValueDesc::Id(n)).collect::<Vec<_>>();
    for (names, want, bytes) in [
        (&["i420", "p010", "i422", "i422_10"][..], "i422_10", &tight),
        (&["i420", "p010", "i422"][..], "p010", &reduced),
        (&[][..], "p010", &reduced),
    ] {
        let mut h = Harness::new(FakeDec::new());
        if !names.is_empty() {
            h.fix_src_takes(FakeDec::OUT, "pixfmt", &takes(names));
        }
        push(&mut h, LEND422, 6, 40);
        let out = h.drain_outputs();
        assert_eq!(out.len(), 1, "{names:?}");
        assert_eq!(announced_pixfmt(&h), want, "{names:?}");
        assert_eq!(out[0].memory.data(), bytes.as_slice(), "{names:?}");
    }
}

#[test]
fn a_decoder_that_drops_every_unit_is_said_to_show_nothing_once() {
    let nothing = |h: &mut Harness| {
        warnings(h)
            .iter()
            .filter(|e| match e {
                Error::ElementFailed { source, .. } => {
                    source.downcast_ref::<VideoDecError>()
                        == Some(&VideoDecError::NothingShown { units: SILENT_UNITS })
                }
                _ => false,
            })
            .count()
    };
    let mut h = Harness::new(FakeDec::new());
    for i in 0..SILENT_UNITS - 1 {
        push(&mut h, BAD, 2, i as u64);
    }
    assert_eq!(nothing(&mut h), 0, "not before the limit");
    push(&mut h, BAD, 2, 1000);
    assert_eq!(nothing(&mut h), 1, "at the limit");
    for i in 0..2 * SILENT_UNITS {
        push(&mut h, BAD, 2, 2000 + i as u64);
    }
    assert_eq!(nothing(&mut h), 0, "said once");
    h.push_event(Event::FlushStart).expect("flush");
    for i in 0..SILENT_UNITS {
        push(&mut h, BAD, 2, 5000 + i as u64);
    }
    assert_eq!(nothing(&mut h), 1, "said again after a seek");

    let mut h = Harness::new(FakeDec::new());
    for i in 0..SILENT_UNITS {
        push(&mut h, HIDE, 2, i as u64);
    }
    push(&mut h, SHOW, 2, 100);
    for i in 0..2 * SILENT_UNITS {
        push(&mut h, BAD, 2, 200 + i as u64);
    }
    assert_eq!(nothing(&mut h), 0);
    assert_eq!(h.drain_outputs().len(), 1);
}

#[test]
fn a_failure_the_codec_raises_ends_the_run_by_name() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, SHOW, 2, 0);
    let b = unit(&mut h, FAIL, 2, 10);
    let Err(Error::ElementFailed { source, .. }) = h.push(b) else { panic!("the run fails") };
    assert_eq!(source.downcast_ref::<BadUnit>(), Some(&BadUnit));
}

#[test]
fn a_ten_bit_picture_moves_a_gpu_link_to_p010_or_fails_by_name() {
    let dev = match pf_memory::udmabuf::device() {
        Ok(dev) => dev,
        Err(e) => return eprintln!("skip: {e}"),
    };
    let page = pf_memory::udmabuf::align();
    let fixed = [
        (F_WIDTH, ValueDesc::Int(8)),
        (F_HEIGHT, ValueDesc::Int(8)),
        (F_LAYOUT, ValueDesc::Id(LAYOUT_I420_DMABUF)),
        (F_MODIFIER, ValueDesc::Int(0)),
    ];
    let i420 = DmabufFormat { fourcc: DRM_FORMAT_YUV420, modifiers: vec![DRM_FORMAT_MOD_LINEAR] };
    let p010 = DmabufFormat { fourcc: DRM_FORMAT_P010, modifiers: vec![DRM_FORMAT_MOD_LINEAR] };
    let open = |formats: &[DmabufFormat]| {
        let ch = GpuFrameChannel::new();
        let dec = FakeDec::new().with_dmabuf(Arc::clone(&ch), formats);
        let config =
            PoolConfig { align: page, allocator: dev.allocator(), ..PoolConfig::heap(page, 4) };
        let mut h = Harness::with_pool_config(dec, config);
        let mut p010 = fixed;
        p010[2] = (F_LAYOUT, ValueDesc::Id(LAYOUT_P010_DMABUF));
        h.fix_src_format(FakeDec::OUT, FAMILY_VIDEO_GPU, &p010);
        h.fix_src_format(FakeDec::OUT, FAMILY_VIDEO_GPU, &fixed);
        (h, ch)
    };

    let (mut h, ch) = open(&[i420.clone(), p010]);
    let mut rx = ch.take_rx().expect("the sink's end");
    push(&mut h, LEND10, 16, 0);
    let out = h.pull(FakeDec::OUT).expect("the descriptor");
    let header = GpuFrameHeader::from_bytes(out.memory.data()).expect("a header");
    let announced = h.announced().expect("announced");
    let v = h.vocabulary();
    let Some(Value::Id(layout)) = announced.get(v.field_id(F_LAYOUT).unwrap()) else {
        panic!("layout")
    };
    assert_eq!(v.value_name(layout), Some(LAYOUT_P010_DMABUF));
    let frame = rx.take(header.token).expect("the frame");
    assert_eq!(frame.drm_format, DRM_FORMAT_P010);
    let planes: Vec<(u32, u32)> = frame.planes.iter().map(|p| (p.offset, p.pitch)).collect();
    assert_eq!(planes, [(0, 32), (512, 32)], "Y then CbCr, 16-bit words");
    // SAFETY: the sink's dup of the slot's dmabuf, mapped read-only and unmapped below.
    let view = unsafe {
        libc::mmap(std::ptr::null_mut(), page, libc::PROT_READ, libc::MAP_SHARED, frame.fds[0], 0)
    };
    assert_ne!(view, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
    let want = lent10_p010(16, 0);
    let bytes = unsafe { std::slice::from_raw_parts(view as *const u8, want.len()) };
    assert_eq!(bytes, want.as_slice(), "the P010 picture through the dmabuf");
    unsafe { libc::munmap(view, page) };
    drop(frame);

    let (mut h, _ch) = open(&[i420]);
    let b = unit(&mut h, LEND10, 16, 0);
    let Err(Error::ElementFailed { source, .. }) = h.push(b) else { panic!("the run fails") };
    let refused = VideoDecError::Layout { pixfmt: "p010", fixed: DRM_FORMAT_YUV420 };
    assert_eq!(source.downcast_ref::<VideoDecError>(), Some(&refused));
}

fn unit_with_hdr10plus(h: &mut Harness, op: u8, pts_ns: u64, maxscl: u32) -> Buffer {
    let mut b = unit(h, op, 2, pts_ns);
    let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
    t35.extend(pf_hdr::hdr10plus::sample_payload([maxscl, 1, 1], 1000));
    let mut side = [0u8; 256];
    let mut w = Writer::new(&mut side);
    w.push(Kind::T35, &t35);
    let n = w.len();
    b.memory.set_side(&side[..n]).expect("a fresh slot's side area");
    b
}

fn maxscl_of(b: &Buffer) -> Option<u32> {
    let meta = FrameMeta::read(b.memory.side()).expect("valid side data");
    Some(meta.hdr10plus()?.ok()?.windows[0].maxscl[0])
}

#[test]
fn side_data_follows_its_unit_to_its_picture_in_presentation_order() {
    let mut h = Harness::new(FakeDec::new());
    let kept = unit_with_hdr10plus(&mut h, KEEP, 0, 10_000);
    h.push(kept).expect("kept");
    let shown = unit_with_hdr10plus(&mut h, SHOW, 40, 90_000);
    h.push(shown).expect("shown");
    let out = h.drain_outputs();
    assert_eq!((out.len(), out[0].pts), (1, Timestamp::from_nanos(40)));
    assert_eq!(maxscl_of(&out[0]), Some(90_000), "the container's record");
    assert!(
        FrameMeta::read(out[0].memory.side()).unwrap().dovi_el(),
        "with what the codec's scan found"
    );
    let tail = h.eos().expect("eos");
    assert_eq!(tail[0].pts, Timestamp::ZERO);
    assert_eq!(maxscl_of(&tail[0]), Some(10_000), "the kept picture takes its own unit's");
    assert!(!FrameMeta::read(tail[0].memory.side()).unwrap().dovi_el());
}

#[test]
fn a_flush_drops_side_data_no_picture_took() {
    let mut h = Harness::new(FakeDec::new());
    let hidden = unit_with_hdr10plus(&mut h, HIDE, 0, 10_000);
    h.push(hidden).expect("decoded, nothing shown");
    h.push_event(Event::FlushStart).expect("flush");
    h.push_event(Event::FlushStop).expect("flush");
    push(&mut h, SHOW, 2, 0);
    let out = h.drain_outputs();
    assert_eq!(
        maxscl_of(&out[0]),
        None,
        "the pts came again, the side data from before the flush did not"
    );
}

#[test]
fn a_picture_with_no_side_data_has_none() {
    let mut h = Harness::new(FakeDec::new());
    push(&mut h, HIDE, 2, 0);
    push(&mut h, TWO, 2, 10);
    for b in h.drain_outputs() {
        assert!(b.memory.side().is_empty());
    }
}

#[test]
fn dmabuf_mode_puts_side_data_on_the_descriptor() {
    let dev = match pf_memory::udmabuf::device() {
        Ok(dev) => dev,
        Err(e) => return eprintln!("skip: {e}"),
    };
    let formats =
        [DmabufFormat { fourcc: DRM_FORMAT_YUV420, modifiers: vec![DRM_FORMAT_MOD_LINEAR] }];
    let ch = GpuFrameChannel::new();
    let dec = FakeDec::new().with_dmabuf(Arc::clone(&ch), &formats);
    let page = pf_memory::udmabuf::align();
    let config =
        PoolConfig { align: page, allocator: dev.allocator(), ..PoolConfig::heap(page, 4) };
    let mut h = Harness::with_pool_config(dec, config);
    let fixed = [
        (F_WIDTH, ValueDesc::Int(2)),
        (F_HEIGHT, ValueDesc::Int(2)),
        (F_LAYOUT, ValueDesc::Id(LAYOUT_I420_DMABUF)),
        (F_MODIFIER, ValueDesc::Int(0)),
    ];
    h.fix_src_format(FakeDec::OUT, FAMILY_VIDEO_GPU, &fixed);
    let _rx = ch.take_rx().expect("the sink's end");
    let shown = unit_with_hdr10plus(&mut h, SHOW, 0, 40_000);
    h.push(shown).expect("a picture");
    let out = h.pull(FakeDec::OUT).expect("the descriptor");
    assert!(GpuFrameHeader::from_bytes(out.memory.data()).is_some());
    assert_eq!(maxscl_of(&out), Some(40_000));
}
