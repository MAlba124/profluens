//! Regression test. EOS reaches a group's elements in chain order, one per pass, so what an
//! element flushes on EOS reaches its successor before that successor's own EOS.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const STREAMED: usize = 5;
const FLUSH_MARK: u8 = 0xFE;
const STREAM_MARK: u8 = 0x01;

type Trace = Arc<Mutex<Vec<String>>>;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static XFORM_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static XFORM_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

static SRC_DESC: ElementDesc = ElementDesc {
    name: "eossrc",
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

struct EosSrc {
    sent: usize,
    trace: Trace,
}

impl EosSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for EosSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
        if self.sent >= STREAMED {
            return Ok(Flow::Eos);
        }
        let mut b = ctx.alloc();
        b.memory.as_mut_full()[0] = STREAM_MARK;
        b.memory.set_len(1);
        ctx.out(Self::OUT).push(b);
        self.sent += 1;
        Ok(Flow::Ok)
    }
    fn event(&mut self, _c: &mut Ctx, e: &Event) -> Result<(), Error> {
        if matches!(e, Event::Eos) {
            self.trace.lock().unwrap().push("eos:src".into());
        }
        Ok(())
    }
}

// Passes buffers through and emits one final buffer on EOS.
static XFORM_DESC: ElementDesc = ElementDesc {
    name: "flushxform",
    sinks: &XFORM_SINKS,
    srcs: &XFORM_SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct FlushXform {
    trace: Trace,
}

impl FlushXform {
    const IN: SinkPad = SinkPad(0);
    const OUT: SrcPad = SrcPad(0);
}

impl Element for FlushXform {
    fn desc(&self) -> &'static ElementDesc {
        &XFORM_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, mut i: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(b) = i.pop() {
            let mut out = ctx.alloc();
            out.memory.as_mut_full()[0] = b.memory.data()[0];
            out.memory.set_len(1);
            ctx.out(Self::OUT).push(out);
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, ctx: &mut Ctx, e: &Event) -> Result<(), Error> {
        if matches!(e, Event::Eos) {
            self.trace.lock().unwrap().push("eos:xform".into());
            let mut out = ctx.alloc();
            out.memory.as_mut_full()[0] = FLUSH_MARK;
            out.memory.set_len(1);
            ctx.out(Self::OUT).push(out);
        }
        Ok(())
    }
}

static TAIL_DESC: ElementDesc = ElementDesc {
    name: "tracesink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct TraceSink {
    trace: Trace,
    n: Arc<AtomicU64>,
}

impl TraceSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for TraceSink {
    fn desc(&self) -> &'static ElementDesc {
        &TAIL_DESC
    }
    fn process(&mut self, _c: &mut Ctx, mut i: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(b) = i.pop() {
            self.trace.lock().unwrap().push(format!("recv:{:02x}", b.memory.data()[0]));
            self.n.fetch_add(1, Ordering::Release);
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, _c: &mut Ctx, e: &Event) -> Result<(), Error> {
        if matches!(e, Event::Eos) {
            self.trace.lock().unwrap().push("eos:sink".into());
        }
        Ok(())
    }
}

fn run_chain() -> Vec<String> {
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let got = Arc::new(AtomicU64::new(0));

    let mut p = Pipeline::new();
    p.set_pool(64, 16);
    let src = p.add(EosSrc { sent: 0, trace: Arc::clone(&trace) });
    let xform = p.add(FlushXform { trace: Arc::clone(&trace) });
    let snk = p.add(TraceSink { trace: Arc::clone(&trace), n: Arc::clone(&got) });
    p.link(src.src(EosSrc::OUT), xform.sink(FlushXform::IN)).expect("src!xform");
    p.link(xform.src(FlushXform::OUT), snk.sink(TraceSink::IN)).expect("xform!sink");
    p.run().expect("run");

    assert_eq!(
        got.load(Ordering::Acquire) as usize,
        STREAMED + 1,
        "the tail received every streamed buffer plus the EOS flush"
    );

    trace.lock().unwrap().clone()
}

#[test]
fn eos_reaches_elements_in_chain_order() {
    let trace = run_chain();
    let eos_order: Vec<&str> =
        trace.iter().filter(|s| s.starts_with("eos:")).map(|s| s.as_str()).collect();
    assert_eq!(
        eos_order,
        vec!["eos:src", "eos:xform", "eos:sink"],
        "EOS must reach elements in chain order; full trace: {trace:?}"
    );
}

#[test]
fn an_eos_flush_reaches_the_tail_before_the_tails_own_eos() {
    let trace = run_chain();

    let flush_at = trace
        .iter()
        .position(|s| s == &format!("recv:{FLUSH_MARK:02x}"))
        .unwrap_or_else(|| panic!("the EOS-flushed buffer never reached the tail: {trace:?}"));
    let sink_eos_at = trace
        .iter()
        .position(|s| s == "eos:sink")
        .unwrap_or_else(|| panic!("the tail never got EOS: {trace:?}"));

    assert!(
        flush_at < sink_eos_at,
        "the tail was told EOS before it processed the upstream's flushed final buffer, this is \
         the truncated-tail bug; trace: {trace:?}"
    );

    let streamed = trace.iter().filter(|s| s == &&format!("recv:{STREAM_MARK:02x}")).count();
    assert_eq!(streamed, STREAMED, "every streamed buffer arrived too: {trace:?}");
    assert_eq!(
        trace.last().map(String::as_str),
        Some("eos:sink"),
        "nothing happens after the tail's EOS: {trace:?}"
    );
}
