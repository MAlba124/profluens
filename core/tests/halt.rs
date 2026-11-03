use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ONE_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static ONE_OUT: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];

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

static SRC: ElementDesc = desc("hsrc", &[], &ONE_OUT);
static SINK: ElementDesc = desc("hsink", &ONE_IN, &[]);

/// Pushes a buffer a pass without end, so the run ends only by a stop.
struct Src {
    pushed: Arc<AtomicU64>,
}

impl Element for Src {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn process(&mut self, ctx: &mut Ctx, _: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(mut buf) = ctx.try_alloc_sized(16).expect("within the budget") {
            buf.memory.set_len(16);
            ctx.out(OUT).push(buf);
            self.pushed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }
}

struct Sink;

impl Element for Sink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {}
        Ok(Flow::Ok)
    }
}

fn pipeline() -> (Pipeline, Arc<AtomicU64>) {
    let pushed = Arc::new(AtomicU64::new(0));
    let mut p = Pipeline::new();
    let src = p.add(Src { pushed: Arc::clone(&pushed) });
    let sink = p.add(Sink);
    p.link(src.src(OUT), sink.sink(IN)).expect("link");
    (p, pushed)
}

#[test]
fn a_stop_said_before_the_run_starts_stops_it_at_once() {
    let (mut p, pushed) = pipeline();
    let control = p.control();
    control.stop();
    let t0 = Instant::now();
    assert!(p.run().is_ok());
    assert!(t0.elapsed() < Duration::from_secs(5), "the run returned on its own");
    assert!(control.is_stopped() && !control.is_finished(), "stopped, not run out");
    assert!(pushed.load(Ordering::Relaxed) <= 1, "at most the first pass ran");
}

#[test]
fn a_run_posts_its_start_on_the_bus() {
    let (mut p, _) = pipeline();
    let (control, bus) = (p.control(), p.bus().clone());
    control.stop();
    assert!(p.run().is_ok());
    let started =
        std::iter::from_fn(|| bus.try_recv()).any(|m| matches!(m, BusMessage::StateChanged { .. }));
    assert!(started, "StateChanged on the bus at the run's start");
}
