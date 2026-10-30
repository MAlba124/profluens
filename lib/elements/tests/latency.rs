//! Latency is declared by elements and compensated at sinks. `Delay` declares 20 ms and
//! never sleeps.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

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
use profluens_elements::testing::{TimedTestSink, TimedTestSrc};

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

const DELAY: Timestamp = Timestamp::from_millis(20);

static DELAY_DESC: ElementDesc = ElementDesc {
    name: "delay20",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc { min: DELAY, max: DELAY, is_live: false, jitter: Timestamp::ZERO },
    make_default: None,
    io: false,
    couplings: &[],
};

struct Delay;

impl Delay {
    const IN: SinkPad = SinkPad(0);
    const OUT: SrcPad = SrcPad(0);
}

impl Element for Delay {
    fn desc(&self) -> &'static ElementDesc {
        &DELAY_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            ctx.out(Self::OUT).push(buf);
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn latency_report_sums_declared_latencies_per_path() {
    let mut p = Pipeline::new();
    let src_a = p.add(TimedTestSrc::new(1, Timestamp::from_millis(10)));
    let delay = p.add(Delay);
    let (sink, _stats) = TimedTestSink::new();
    let snk_a = p.add(sink);
    p.link(src_a.src(TimedTestSrc::OUT), delay.sink(Delay::IN)).expect("src!delay");
    p.link(delay.src(Delay::OUT), snk_a.sink(TimedTestSink::IN)).expect("delay!sink");

    let src_b = p.add(TimedTestSrc::new(1, Timestamp::from_millis(10)));
    let (sink_b, _stats_b) = TimedTestSink::new();
    let snk_b = p.add(sink_b);
    p.link(src_b.src(TimedTestSrc::OUT), snk_b.sink(TimedTestSink::IN)).expect("src_b!sink_b");

    let report = p.latency_report();
    assert_eq!(report.paths.len(), 2, "one path per sink");
    assert_eq!(report.total(), DELAY, "pipeline latency = worst sink path");

    let worst = &report.paths[0];
    assert_eq!(worst.sink, snk_a);
    assert_eq!(worst.total, DELAY);
    assert!(!worst.is_live);
    let names: Vec<u32> = worst.per_element.iter().map(|(el, _)| el.0).collect();
    assert_eq!(names, vec![src_a.0, delay.0, snk_a.0], "path in source→sink order");
    assert_eq!(worst.per_element[1], (delay, DELAY), "the delay's own declaration");

    let quiet = &report.paths[1];
    assert_eq!(quiet.sink, snk_b);
    assert_eq!(quiet.total, Timestamp::ZERO, "undeclared path adds nothing");
}

#[test]
fn sink_render_deadline_shifts_by_path_latency() {
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(1, Timestamp::from_millis(10)));
    let delay = p.add(Delay);
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), delay.sink(Delay::IN)).expect("src!delay");
    p.link(delay.src(Delay::OUT), snk.sink(TimedTestSink::IN)).expect("delay!sink");
    p.set_clock(Arc::new(clock.clone()));

    let run = std::thread::spawn(move || p.run());

    for _ in 0..60 {
        if run.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!run.is_finished(), "sink rendered before its latency-shifted deadline");
    assert_eq!(stats.count(), 0, "PTS-0 buffer held back by 20 ms of path latency");

    clock.advance(DELAY);
    run.join().expect("run joined").expect("run ok");

    let renders = stats.renders();
    assert_eq!(renders.len(), 1);
    assert_eq!(renders[0].pts, Timestamp::ZERO);
    assert!(
        renders[0].rendered_at >= DELAY,
        "rendered at running time {:?}, before pts + path latency",
        renders[0].rendered_at
    );
}
