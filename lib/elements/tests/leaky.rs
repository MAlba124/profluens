//! Leaky inbound rings drop batches when full instead of blocking a live producer.

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
use profluens_core::ring::Leaky;
use profluens_core::time::Timestamp;
use profluens_elements::testing::TimedTestSink;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "livesrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc {
        min: Timestamp::ZERO,
        max: Timestamp::ZERO,
        is_live: true,
        jitter: Timestamp::ZERO,
    },
    make_default: None,
    io: false,
    couplings: &[],
};

struct LiveSrc {
    n: usize,
    sent: Arc<AtomicUsize>,
}

impl LiveSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for LiveSrc {
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

#[test]
fn leaky_ring_never_blocks_the_live_source() {
    const COUNT: usize = 400;
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();
    let sent = Arc::new(AtomicUsize::new(0));

    let mut p = Pipeline::new();
    let src = p.add(LiveSrc { n: COUNT, sent: Arc::clone(&sent) });
    let snk = p.add(sink);
    p.link(src.src(LiveSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_element_pool(src, 64, 1024);
    p.set_queue_leaky(snk, Leaky::DropNewest);
    p.set_clock(Arc::new(clock.clone()));
    let tap = p.tap_handle();

    let run = std::thread::spawn(move || p.run());

    // With a blocking ring the source would wedge within a few batches.
    let mut settled = false;
    for _ in 0..500 {
        if sent.load(Ordering::Relaxed) >= COUNT {
            settled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        settled,
        "live source finished without blocking (sent {})",
        sent.load(Ordering::Relaxed)
    );

    while !run.is_finished() {
        clock.advance(Timestamp::from_millis(50));
        std::thread::yield_now();
    }
    run.join().expect("joined").expect("run ok");

    let rendered = stats.renders().len();
    let drops = tap.snapshot(src).expect("src counters").drops;
    assert!(rendered < COUNT, "a parked sink cannot have kept up ({rendered} of {COUNT})");
    assert!(drops > 0, "dropped batches surfaced in the producer's drops counter");
    assert!(rendered > 0, "the ring still delivered what fit");
}
