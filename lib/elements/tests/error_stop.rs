//! An element error anywhere stops every group and `run()` returns it, even with a
//! healthy endless chain joined first.

use std::sync::mpsc;

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_elements::testing::TestSink;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

static SRC_DESC: ElementDesc = ElementDesc {
    name: "endless_src",
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

static FAIL_DESC: ElementDesc = ElementDesc {
    name: "failing_sink",
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

struct EndlessSrc;

impl EndlessSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for EndlessSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = buf.memory.capacity().min(64);
        buf.memory.as_mut_full()[..n].fill(0xAB);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        Ok(Flow::Ok)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct InjectedFailure {
    seen: u32,
}

impl std::fmt::Display for InjectedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failing_sink: injected failure on buffer {}", self.seen)
    }
}

impl std::error::Error for InjectedFailure {}

struct FailingSink {
    seen: u32,
    fail_at: u32,
}

impl FailingSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for FailingSink {
    fn desc(&self) -> &'static ElementDesc {
        &FAIL_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(_buf) = inputs.pop() {
            self.seen += 1;
            if self.seen >= self.fail_at {
                return Err(Error::element_failed(
                    ctx.element(),
                    InjectedFailure { seen: self.seen },
                ));
            }
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn element_error_stops_disjoint_siblings() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut p = Pipeline::new();
        let a_src = p.add(EndlessSrc);
        let (a_sink, _stats) = TestSink::new();
        let a_sink = p.add(a_sink);
        p.link(a_src.src(EndlessSrc::OUT), a_sink.sink(TestSink::IN)).expect("a_src ! a_sink");
        let b_src = p.add(EndlessSrc);
        let b_sink = p.add(FailingSink { seen: 0, fail_at: 3 });
        p.link(b_src.src(EndlessSrc::OUT), b_sink.sink(FailingSink::IN)).expect("b_src ! b_sink");
        let _ = tx.send(p.run());
    });
    let result = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("run() must return once a group fails, the error-cascades-stop contract");
    let err = result.expect_err("the injected sink failure must surface from run()");
    let injected =
        std::error::Error::source(&err).and_then(|s| s.downcast_ref::<InjectedFailure>());
    assert_eq!(
        injected,
        Some(&InjectedFailure { seen: 3 }),
        "run() returns the failing element's error, got: {err:?}"
    );
}
