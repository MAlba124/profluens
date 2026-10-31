//! A pipeline's own scheduling hint for an element: Active elements made Passive join their
//! upstream's group and still see every buffer, a Passive one with two inputs is refused.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use profluens_core::batch::Inputs;
use profluens_core::clock::MockClock;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, GraphError};
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::Outcome;
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
static TWO_IN: [PadDesc; 2] =
    [PadDesc { name: "a", offers: &BYTES }, PadDesc { name: "b", offers: &BYTES }];
static SRC: ElementDesc = desc("osrc", &[], &ONE_OUT);
static FWD: ElementDesc = desc("ofwd", &ONE_IN, &ONE_OUT);
static SINK: ElementDesc = desc("osink", &ONE_IN, &[]);
static JOIN: ElementDesc = desc("ojoin", &TWO_IN, &[]);

/// A source of `left` buffers, a forward or a sink counting what it took.
struct Node {
    desc: &'static ElementDesc,
    left: u32,
    took: Arc<AtomicU32>,
}

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let fwd = !self.desc.srcs.is_empty();
        while let Some(buf) = inputs.pop() {
            self.took.fetch_add(1, Ordering::Relaxed);
            if fwd {
                ctx.out(OUT).push(buf);
            }
        }
        if !self.desc.sinks.is_empty() {
            return Ok(Flow::Ok);
        }
        if self.left == 0 {
            return Ok(Flow::Eos);
        }
        let buf = ctx.alloc();
        ctx.out(OUT).push(buf);
        self.left -= 1;
        Ok(Flow::Ok)
    }
}

fn add(p: &mut Pipeline, desc: &'static ElementDesc, left: u32) -> (ElementId, Arc<AtomicU32>) {
    let took = Arc::new(AtomicU32::new(0));
    (p.add(Node { desc, left, took: Arc::clone(&took) }), took)
}

/// `src ! fwd ! fwd ! sink`, every element Active by its own hint.
fn line(p: &mut Pipeline, buffers: u32) -> ([ElementId; 4], Arc<AtomicU32>) {
    let (src, _) = add(p, &SRC, buffers);
    let (a, _) = add(p, &FWD, 0);
    let (b, _) = add(p, &FWD, 0);
    let (sink, took) = add(p, &SINK, 0);
    p.link(src.src(OUT), a.sink(IN)).expect("link");
    p.link(a.src(OUT), b.sink(IN)).expect("link");
    p.link(b.src(OUT), sink.sink(IN)).expect("link");
    ([src, a, b, sink], took)
}

/// Runs every group to its end, the groups as they were.
fn step_out(p: &mut Pipeline) -> Vec<Vec<ElementId>> {
    let mut run = p.step(Arc::new(MockClock::starting_at(Timestamp::from_secs(1)))).expect("step");
    let groups = (0..run.groups()).map(|g| run.elements(g).to_vec()).collect();
    for _ in 0..1_000 {
        let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
            break;
        };
        run.pass(g).expect("pass");
    }
    assert!((0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit)), "every group left");
    run.finish().expect("finish");
    groups
}

#[test]
fn active_elements_made_passive_join_their_upstreams_group() {
    let mut p = Pipeline::new();
    let ([_, _, _, _], took) = line(&mut p, 5);
    assert_eq!(step_out(&mut p).len(), 4, "a group each by their own hints");
    assert_eq!(took.load(Ordering::Relaxed), 5);

    let mut p = Pipeline::new();
    let ([src, a, b, sink], took) = line(&mut p, 5);
    p.set_element_sched(b, SchedHint::Passive);
    p.set_element_sched(sink, SchedHint::Passive);
    assert_eq!(step_out(&mut p), [vec![src], vec![a, b, sink]], "b and the sink inline on a");
    assert_eq!(took.load(Ordering::Relaxed), 5, "every buffer through the inline hops");
}

#[test]
fn a_passive_override_on_two_inputs_is_refused() {
    let mut p = Pipeline::new();
    let (x, _) = add(&mut p, &SRC, 1);
    let (y, _) = add(&mut p, &SRC, 1);
    let (join, _) = add(&mut p, &JOIN, 0);
    p.link(x.src(OUT), join.sink(SinkPad(0))).expect("link");
    p.link(y.src(OUT), join.sink(SinkPad(1))).expect("link");
    p.set_element_sched(join, SchedHint::Passive);
    let r = p.step(Arc::new(MockClock::starting_at(Timestamp::from_secs(1))));
    assert!(
        matches!(r, Err(Error::Graph(GraphError::PassiveMultipleInputs { element, inputs: 2 })) if element == join),
        "{:?}",
        r.err()
    );
}
