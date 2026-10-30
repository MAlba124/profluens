//! Latency tracing fills per-element histograms when on and stays empty when off.

use std::sync::Arc;

use profluens_core::clock::MockClock;
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_elements::testing::{TimedTestSink, TimedTestSrc};

#[test]
fn tracing_fills_histograms() {
    let clock = MockClock::new();
    let (sink, _stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(20, Timestamp::from_millis(1)));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    p.set_tracing(true);
    let tap = p.tap_handle();

    let run = std::thread::spawn(move || p.run());
    while !run.is_finished() {
        clock.advance(Timestamp::from_millis(5));
        std::thread::yield_now();
    }
    run.join().expect("joined").expect("run ok");

    let (src_process, _, _) = tap.latency(src).expect("src histograms");
    assert!(src_process.count > 0, "source process() time recorded");

    let (snk_process, snk_queue, snk_wait) = tap.latency(snk).expect("sink histograms");
    assert!(snk_process.count > 0, "sink process() time recorded");
    assert!(snk_queue.count > 0, "ring residency recorded on the consuming side");
    assert!(snk_wait.count > 0, "sink wait overshoot recorded");
    let p50 = snk_process.quantile_ns(0.5);
    let p99 = snk_process.quantile_ns(0.99);
    assert!(p50 <= p99, "p50 {p50} <= p99 {p99}");
    assert!(snk_process.mean_ns() <= snk_process.max_ns);
}

#[test]
fn tracing_off_records_nothing() {
    let (sink, _stats) = TimedTestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(10, Timestamp::ZERO));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    let tap = p.tap_handle();
    p.run().expect("run ok");

    for id in [src, snk] {
        let (process, queue, wait) = tap.latency(id).expect("histograms exist");
        assert_eq!(process.count, 0, "no process records while off");
        assert_eq!(queue.count, 0, "no queue records while off");
        assert_eq!(wait.count, 0, "no wait records while off");
    }
}
