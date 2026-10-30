//! `Control::wake` has a pipeline look again at what it waits on outside itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use profluens_core::batch::Inputs;
use profluens_core::clock::MockClock;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

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

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ONE_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static ONE_OUT: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SRC: ElementDesc = desc("wsrc", &[], &ONE_OUT);
static GATE: ElementDesc = desc("wgate", &ONE_IN, &[]);

struct Count(u32);

impl Element for Count {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.0 == 0 {
            return Ok(Flow::Eos);
        }
        let Some(buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        ctx.out(OUT).push(buf);
        self.0 -= 1;
        Ok(Flow::Ok)
    }
}

/// Takes nothing while its gate is shut and asks for no wake, as a sink held by another thread.
struct Gate {
    open: Arc<AtomicBool>,
    taken: Arc<AtomicU64>,
}

impl Element for Gate {
    fn desc(&self) -> &'static ElementDesc {
        &GATE
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while self.open.load(Ordering::Acquire) && inputs.pop().is_some() {
            self.taken.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn a_wake_has_a_held_sink_look_again() {
    let (open, taken) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)));
    let mut p = Pipeline::new();
    let src = p.add(Count(3));
    let gate = p.add(Gate { open: Arc::clone(&open), taken: Arc::clone(&taken) });
    p.link(src.src(OUT), gate.sink(IN)).expect("link");
    let mut run = p.step(Arc::new(MockClock::starting_at(Timestamp::from_secs(1)))).expect("step");
    let settle = |run: &mut profluens_core::sched::Stepped| {
        for _ in 0..200 {
            let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
                return;
            };
            run.pass(g).expect("pass");
        }
        panic!("the run never settled");
    };
    settle(&mut run);
    assert_eq!(taken.load(Ordering::Relaxed), 0);
    open.store(true, Ordering::Release);
    assert!((0..run.groups()).all(|g| !run.ready(g)), "nothing tells the run the gate opened");
    run.control().wake();
    assert!((0..run.groups()).any(|g| run.ready(g)));
    settle(&mut run);
    assert_eq!(taken.load(Ordering::Relaxed), 3);
    assert!(run.control().is_finished());
    run.finish().expect("finish");
}
