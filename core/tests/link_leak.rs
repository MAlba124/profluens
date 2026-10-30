use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static CALLS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

fn live_add(delta: i64) {
    LIVE.with(|c| c.set(c.get() + delta));
}

fn live_bytes() -> i64 {
    LIVE.with(Cell::get)
}

fn call_add(bytes: usize) {
    CALLS.with(|c| c.set(c.get() + 1));
    BYTES.with(|c| c.set(c.get() + bytes as u64));
}

pub(crate) fn alloc_calls() -> u64 {
    CALLS.with(Cell::get)
}

/// Bytes asked of the allocator on this thread, a realloc's new size included.
pub(crate) fn alloc_bytes() -> u64 {
    BYTES.with(Cell::get)
}

struct CountingAlloc;

// SAFETY: every call forwards to `System` unchanged, the count is bookkeeping only.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        call_add(layout.size());
        if !p.is_null() {
            live_add(layout.size() as i64);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        call_add(layout.size());
        if !p.is_null() {
            live_add(layout.size() as i64);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        live_add(-(layout.size() as i64));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        call_add(new_size);
        if !p.is_null() {
            live_add(new_size as i64 - layout.size() as i64);
        }
        p
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

static SAMPLE_FORMATS: [ValueDesc; 4] =
    [ValueDesc::Id("s16"), ValueDesc::Id("s24"), ValueDesc::Id("s32"), ValueDesc::Id("f32")];
static LAYOUTS: [ValueDesc; 2] = [ValueDesc::Id("interleaved"), ValueDesc::Id("planar")];

const fn audio_fields(formats: &'static [ValueDesc]) -> [FieldDesc; 4] {
    [
        FieldDesc {
            field: "rate",
            allowed: ConstraintDesc::Range {
                min: ValueDesc::Int(8000),
                max: ValueDesc::Int(192_000),
                step: ValueDesc::Int(1),
            },
            preferred: Some(ValueDesc::Int(48_000)),
        },
        FieldDesc {
            field: "channels",
            allowed: ConstraintDesc::Range {
                min: ValueDesc::Int(1),
                max: ValueDesc::Int(8),
                step: ValueDesc::Int(1),
            },
            preferred: Some(ValueDesc::Int(2)),
        },
        FieldDesc { field: "format", allowed: ConstraintDesc::Set(formats), preferred: None },
        FieldDesc { field: "layout", allowed: ConstraintDesc::Set(&LAYOUTS), preferred: None },
    ]
}

static AUDIO_FIELDS: [FieldDesc; 4] = audio_fields(&SAMPLE_FORMATS);
static AUDIO_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: "audio/raw", fields: &AUDIO_FIELDS }, OfferDesc::any("bytes")];

static PICKY_FORMATS: [ValueDesc; 2] = [ValueDesc::Id("u8"), ValueDesc::Id("f64")];
static PICKY_FIELDS: [FieldDesc; 4] = audio_fields(&PICKY_FORMATS);
static PICKY_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &PICKY_FIELDS }];

static WILDCARD_OFFERS: [OfferDesc; 1] = [OfferDesc::wildcard()];

const fn pad(name: &'static str, offers: &'static [OfferDesc]) -> PadDesc {
    PadDesc { name, offers }
}

type Pads = &'static [PadDesc];

const fn desc(name: &'static str, sinks: Pads, srcs: Pads, inputs: InputPolicy) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static SRC_PADS: [PadDesc; 1] = [pad("src", &AUDIO_OFFERS)];
static SINK_PADS: [PadDesc; 1] = [pad("sink", &AUDIO_OFFERS)];
static PICKY_PADS: [PadDesc; 1] = [pad("sink", &PICKY_OFFERS)];
static PIPE_SINKS: [PadDesc; 1] = [pad("sink", &WILDCARD_OFFERS)];
static PIPE_SRCS: [PadDesc; 1] = [pad("src", &WILDCARD_OFFERS)];

static SRC_DESC: ElementDesc = desc("leaksrc", &[], &SRC_PADS, InputPolicy::None);
static SINK_DESC: ElementDesc = desc("leaksink", &SINK_PADS, &[], InputPolicy::Single);
static PICKY_DESC: ElementDesc = desc("leakpicky", &PICKY_PADS, &[], InputPolicy::Single);
static PIPE_DESC: ElementDesc = desc("leakpipe", &PIPE_SINKS, &PIPE_SRCS, InputPolicy::Single);

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

struct Node(&'static ElementDesc);

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.0
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Eos)
    }
}

fn build_link_drop() {
    let mut p = Pipeline::new();
    let src = p.add(Node(&SRC_DESC));
    let sink = p.add(Node(&SINK_DESC));
    let picky = p.add(Node(&PICKY_DESC));
    let src2 = p.add(Node(&SRC_DESC));
    let pipe = p.add(Node(&PIPE_DESC));
    let sink2 = p.add(Node(&SINK_DESC));

    assert!(p.link(src.src(OUT), picky.sink(IN)).is_err(), "disjoint sample formats");

    let direct = p.link(src.src(OUT), sink.sink(IN)).expect("audio/raw links");
    assert_eq!(p.negotiated(direct).expect("fixed").len(), 4, "all four fields fixed");

    p.link(src2.src(OUT), pipe.sink(IN)).expect("wildcard adopts audio/raw");
    let bound = p.link(pipe.src(OUT), sink2.sink(IN)).expect("transport offers its format");
    assert_eq!(p.negotiated(bound).expect("fixed").len(), 4, "the pinned format carried over");
}

static PROBED_SRCS: [PadDesc; 1] = [pad("src", OfferDesc::PROBED)];
static PROBED_SINKS: [PadDesc; 1] = [pad("sink", OfferDesc::PROBED)];
static PROBED_SRC_DESC: ElementDesc = desc("leakprobesrc", &[], &PROBED_SRCS, InputPolicy::None);
static PROBED_SINK_DESC: ElementDesc =
    desc("leakprobesink", &PROBED_SINKS, &[], InputPolicy::Single);

struct ProbedNode(&'static ElementDesc);

impl Element for ProbedNode {
    fn desc(&self) -> &'static ElementDesc {
        self.0
    }
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        match self.0.sinks.is_empty() {
            true => ctx.offer_src(OUT, &AUDIO_OFFERS),
            false => ctx.offer_sink(IN, &AUDIO_OFFERS),
        }
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Eos)
    }
}

fn link_calls(probed: bool) -> (u64, u64) {
    let mut p = Pipeline::new();
    let pair = |p: &mut Pipeline| match probed {
        true => (p.add(ProbedNode(&PROBED_SRC_DESC)), p.add(ProbedNode(&PROBED_SINK_DESC))),
        false => (p.add(Node(&SRC_DESC)), p.add(Node(&SINK_DESC))),
    };
    let (first, second) = (pair(&mut p), pair(&mut p));
    p.preroll().expect("preroll");
    let mut calls = |(src, sink): (ElementId, ElementId)| {
        let before = alloc_calls();
        let link = p.link(src.src(OUT), sink.sink(IN)).expect("audio/raw links");
        let made = alloc_calls() - before;
        assert_eq!(p.negotiated(link).expect("fixed").len(), 4, "all four fields fixed");
        made
    };
    (calls(first), calls(second))
}

#[test]
fn linking_a_probed_pad_allocates_no_more_than_a_static_one() {
    link_calls(true);
    let (static_first, static_next) = link_calls(false);
    let (probed_first, probed_next) = link_calls(true);
    assert!(probed_first <= static_first, "{probed_first} calls against {static_first}");
    assert!(probed_next <= static_next, "{probed_next} calls against {static_next}");
}

fn build_probe_link_drop() {
    let mut p = Pipeline::new();
    let src = p.add(ProbedNode(&PROBED_SRC_DESC));
    let sink = p.add(ProbedNode(&PROBED_SINK_DESC));
    let picky = p.add(Node(&PICKY_DESC));
    assert!(p.link(src.src(OUT), sink.sink(IN)).is_err(), "pending");
    p.preroll().expect("preroll");
    assert!(p.link(src.src(OUT), picky.sink(IN)).is_err(), "disjoint sample formats");
    let link = p.link(src.src(OUT), sink.sink(IN)).expect("audio/raw links");
    assert_eq!(p.negotiated(link).expect("fixed").len(), 4, "all four fields fixed");
}

#[test]
fn probing_and_linking_retain_no_heap() {
    for _ in 0..16 {
        build_probe_link_drop();
    }
    let before = live_bytes();
    for _ in 0..1000 {
        build_probe_link_drop();
    }
    let leaked = live_bytes() - before;
    assert_eq!(leaked, 0, "1000 build/probe/link/drop cycles retained {leaked} bytes");
}

#[test]
fn linking_retains_no_heap() {
    for _ in 0..16 {
        build_link_drop();
    }
    let before = live_bytes();
    for _ in 0..1000 {
        build_link_drop();
    }
    let leaked = live_bytes() - before;
    assert_eq!(leaked, 0, "1000 build/link/drop cycles retained {leaked} bytes");
}
