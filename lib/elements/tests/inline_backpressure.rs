//! Regression test for the inline-group livelock and the head backlog cap that bounds it.
//! Without the scheduler gate a fast inlined source exhausts the shared pool and wedges.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

/// Mirrors the private `pipeline::INLINE_INPUT_CAP`.
const INLINE_INPUT_CAP: usize = 8;
const BURST: usize = 4;
const TOTAL: usize = 400;
/// Larger than `INLINE_INPUT_CAP`, so the gate, not the pool, bounds the backlog.
const SLOTS: u32 = 24;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static XFORM_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static XFORM_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

static SPEW_DESC: ElementDesc = ElementDesc {
    name: "spewsrc",
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

struct SpewSrc {
    sent: usize,
}

impl SpewSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for SpewSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SPEW_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
        if self.sent >= TOTAL {
            return Ok(Flow::Eos);
        }
        for _ in 0..BURST {
            if self.sent >= TOTAL {
                break;
            }
            let Some(mut b) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            b.memory.set_len(1);
            ctx.out(Self::OUT).push(b);
            self.sent += 1;
        }
        Ok(Flow::Ok)
    }
}

// Refuses to consume until it holds its own output slot, which turns pool exhaustion
// into a livelock.
static XFORM_DESC: ElementDesc = ElementDesc {
    name: "throttledxform",
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

struct ThrottledXform {
    peak: Arc<AtomicUsize>,
}

impl ThrottledXform {
    const IN: SinkPad = SinkPad(0);
    const OUT: SrcPad = SrcPad(0);
}

impl Element for ThrottledXform {
    fn desc(&self) -> &'static ElementDesc {
        &XFORM_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, mut i: Inputs<'_>) -> Result<Flow, Error> {
        self.peak.fetch_max(i.len(), Ordering::AcqRel);
        if i.is_empty() {
            return Ok(Flow::Ok);
        }
        let Some(mut out) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let Some(inb) = i.pop() else {
            return Ok(Flow::Ok);
        };
        out.memory.set_len(inb.memory.len());
        drop(inb);
        ctx.out(Self::OUT).push(out);
        Ok(Flow::Ok)
    }
}

static SINK_DESC: ElementDesc = ElementDesc {
    name: "drainsink",
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

struct DrainSink {
    n: Arc<AtomicU64>,
}

impl DrainSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for DrainSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _c: &mut Ctx, mut i: Inputs<'_>) -> Result<Flow, Error> {
        while i.pop().is_some() {
            self.n.fetch_add(1, Ordering::Release);
        }
        Ok(Flow::Ok)
    }
}

fn run_with_watchdog(p: &mut Pipeline, budget: Duration) -> bool {
    let fired = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let handle = p.control();
    let watchdog = {
        let (fired, done) = (Arc::clone(&fired), Arc::clone(&done));
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + budget;
            while std::time::Instant::now() < deadline {
                if done.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            fired.store(true, Ordering::Release);
            handle.stop();
        })
    };
    p.run().expect("run");
    done.store(true, Ordering::Release);
    watchdog.join().expect("watchdog");
    fired.load(Ordering::Acquire)
}

#[test]
fn inline_gate_paces_a_fast_source_and_the_group_never_livelocks() {
    let peak = Arc::new(AtomicUsize::new(0));
    let got = Arc::new(AtomicU64::new(0));

    let mut p = Pipeline::new();
    p.set_pool(64, SLOTS);
    let src = p.add(SpewSrc { sent: 0 });
    let xform = p.add(ThrottledXform { peak: Arc::clone(&peak) });
    let snk = p.add(DrainSink { n: Arc::clone(&got) });
    p.link(src.src(SpewSrc::OUT), xform.sink(ThrottledXform::IN)).expect("src!xform");
    p.link(xform.src(ThrottledXform::OUT), snk.sink(DrainSink::IN)).expect("xform!sink");

    let timed_out = run_with_watchdog(&mut p, Duration::from_secs(10));

    assert!(
        !timed_out,
        "pipeline livelocked, the inline backpressure gate is not pacing the source"
    );
    assert_eq!(got.load(Ordering::Acquire), TOTAL as u64, "every buffer reached the tail");

    // The producer emits a whole burst past the gate, so the ceiling is CAP - 1 + BURST.
    let observed = peak.load(Ordering::Acquire);
    assert!(
        observed <= INLINE_INPUT_CAP + BURST,
        "inline backlog reached {observed} buffers (cap {INLINE_INPUT_CAP} + one {BURST}-buffer \
         burst), the producer is not being paced by its successor's backlog"
    );
    assert!(observed > 1, "the transform never saw a backlog, the test is not exercising the gate");
}

#[test]
fn a_bounded_backlog_keeps_pool_allocation_flat() {
    let peak = Arc::new(AtomicUsize::new(0));
    let got = Arc::new(AtomicU64::new(0));

    let mut p = Pipeline::new();
    p.set_pool(64, SLOTS);
    let src = p.add(SpewSrc { sent: 0 });
    let xform = p.add(ThrottledXform { peak: Arc::clone(&peak) });
    let snk = p.add(DrainSink { n: Arc::clone(&got) });
    p.link(src.src(SpewSrc::OUT), xform.sink(ThrottledXform::IN)).expect("src!xform");
    p.link(xform.src(ThrottledXform::OUT), snk.sink(DrainSink::IN)).expect("xform!sink");

    let tap = p.tap_handle();
    let timed_out = run_with_watchdog(&mut p, Duration::from_secs(10));
    assert!(!timed_out, "pipeline livelocked");
    assert_eq!(got.load(Ordering::Acquire), TOTAL as u64);

    let pool = tap.pool();
    assert_eq!(
        pool.outstanding, 0,
        "every slot came back at end of run (outstanding leak = the freeze's failure mode)"
    );
    assert!(
        pool.slot_allocations <= SLOTS as u64,
        "{TOTAL} buffers cost {} slot allocations for a {SLOTS}-slot pool, recycling is not \
         flat, i.e. the pool is being missed and the run heap-allocates per buffer",
        pool.slot_allocations
    );
    assert!(
        pool.high_water <= SLOTS as u64,
        "occupancy peaked at {} of {SLOTS} slots",
        pool.high_water
    );
}
