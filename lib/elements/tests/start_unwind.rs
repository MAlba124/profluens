//! A group whose `start()` fails part-way must still stop the elements that already started.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "startsrc",
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
static SNK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SNK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SNK_OFFERS }];
static SNK_DESC: ElementDesc = ElementDesc {
    name: "failsink",
    sinks: &SNK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Opener(Arc<AtomicUsize>);
impl Opener {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for Opener {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, _c: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Eos)
    }
    fn stop(&mut self, _c: &mut Ctx) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StartRefused;

impl std::fmt::Display for StartRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("start refused")
    }
}

impl std::error::Error for StartRefused {}

struct Failer;
impl Failer {
    const IN: SinkPad = SinkPad(0);
}

impl Element for Failer {
    fn desc(&self) -> &'static ElementDesc {
        &SNK_DESC
    }
    fn start(&mut self, c: &mut Ctx) -> Result<(), Error> {
        Err(Error::element_failed(c.element(), StartRefused))
    }
    fn process(&mut self, _c: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Ok)
    }
}

#[test]
fn a_failed_start_still_stops_the_elements_that_started() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let mut p = Pipeline::new();
    let src = p.add(Opener(Arc::clone(&stopped)));
    let snk = p.add(Failer);
    p.link(src.src(Opener::OUT), snk.sink(Failer::IN)).expect("link");
    assert!(p.run().is_err(), "the run must surface the start failure");
    assert_eq!(
        stopped.load(Ordering::SeqCst),
        1,
        "the element that started must be stopped when a later start() fails"
    );
}
