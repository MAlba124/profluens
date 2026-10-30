//! Per-element inbound ring capacity. A deeper ring lets its source run further ahead.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use profluens_core::batch::Inputs;
use profluens_core::clock::MockClock;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SrcPad;
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_elements::testing::TimedTestSink;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "countingsrc",
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

struct CountingSrc {
    n: usize,
    sent: Arc<AtomicUsize>,
}

impl CountingSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for CountingSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let sent = self.sent.load(Ordering::Relaxed);
        if sent >= self.n {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.set_len(1);
        buf.pts = Timestamp::from_millis(sent as u64);
        ctx.out(Self::OUT).push(buf);
        self.sent.store(sent + 1, Ordering::Relaxed);
        Ok(Flow::Ok)
    }
}

const COUNT: usize = 400;

fn settled_progress(queue_cap: Option<usize>) -> usize {
    let clock = MockClock::new();
    let (sink, _stats) = TimedTestSink::new();
    let sent = Arc::new(AtomicUsize::new(0));

    let mut p = Pipeline::new();
    let src = p.add(CountingSrc { n: COUNT, sent: Arc::clone(&sent) });
    let snk = p.add(sink);
    p.link(src.src(CountingSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_element_pool(src, 64, 1024);
    if let Some(cap) = queue_cap {
        p.set_queue_capacity(snk, cap);
    }
    p.set_clock(Arc::new(clock.clone()));

    let run = std::thread::spawn(move || p.run());

    let mut last = usize::MAX;
    loop {
        let now = sent.load(Ordering::Relaxed);
        if now == last {
            break;
        }
        last = now;
        std::thread::sleep(Duration::from_millis(20));
    }

    clock.advance(Timestamp::from_millis(COUNT as u64));
    run.join().expect("run joined").expect("run ok");
    last
}

#[test]
fn deeper_inbound_ring_lets_the_source_run_further_ahead() {
    let shallow = settled_progress(None); // default queue_cap = 4 batches
    let deep = settled_progress(Some(64));
    assert!(
        deep >= shallow + 16,
        "capacity override had no effect: default ring parked the source at \
         {shallow} buffers, 64-batch ring at {deep}"
    );
    assert!(
        deep < COUNT,
        "source was never backpressured at all ({deep} of {COUNT}), the ring \
         cap is not binding and this test proves nothing"
    );
}
