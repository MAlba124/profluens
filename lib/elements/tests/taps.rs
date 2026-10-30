//! Taps. A `TapHandle` reads cumulative counters live, rates are the observer's arithmetic.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use profluens_core::clock::MockClock;
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_elements::testing::{TestSink, TestSrc};

const TOTAL: u64 = 256 * 1024;

#[test]
fn tap_reads_live_while_streaming_and_reconciles_at_eos() {
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(TOTAL));
    let (sink, stats) = TestSink::new();
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).expect("link");

    let tap = p.tap_handle();
    assert_eq!(tap.len(), 2);
    assert_eq!(tap.element_name(src), Some("testsrc"));
    assert_eq!(tap.element_name(snk), Some("testsink"));

    let run = std::thread::spawn(move || p.run());

    let mut saw_progress = false;
    for _ in 0..500 {
        let s = tap.snapshot(src).expect("known element");
        if s.bytes_out > 0 {
            saw_progress = true;
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    run.join().expect("joined").expect("run ok");
    assert!(saw_progress, "observer saw counters move (or the finished totals)");

    let s = tap.snapshot(src).unwrap();
    let k = tap.snapshot(snk).unwrap();
    assert_eq!(s.bytes_out, TOTAL, "source produced everything");
    assert_eq!(k.bytes_in, TOTAL, "sink consumed everything");
    assert_eq!(k.buffers_in, s.buffers_out, "no loss or duplication across the link");
    assert!(s.batches_out >= 1);
    assert!(k.batches_in >= 1);
    assert!(
        k.batches_in <= s.batches_out,
        "the ring may coalesce (consumer-side append), never split"
    );
    // Ring fill is recorded at push time. Default capacity is 4 batches.
    assert!(
        (1..=4).contains(&s.queue_high_water),
        "high-water within the ring's capacity, got {}",
        s.queue_high_water
    );
    assert_eq!(k.queue_high_water, 0, "a sink has no downstream ring");
    assert!(stats.is_done());

    assert_eq!(p_counters_bytes(&tap, src), TOTAL);
}

fn p_counters_bytes(
    tap: &profluens_core::counters::TapHandle,
    el: profluens_core::id::ElementId,
) -> u64 {
    tap.snapshot(el).map(|s| s.bytes_out).unwrap_or(0)
}

#[test]
fn rate_windows_come_from_delta_bytes_over_delta_now() {
    let clock = MockClock::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(TOTAL));
    let (sink, _stats) = TestSink::new();
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));

    let tap = p.tap_handle();
    assert!(tap.now().is_none(), "no run yet: no running-time base");

    p.run().expect("run");

    assert_eq!(tap.now(), Timestamp::ZERO);
    clock.advance(Timestamp::from_millis(2));
    let dt = tap.now();
    assert_eq!(dt, Timestamp::from_millis(2));
    let bytes = tap.snapshot(src).unwrap().bytes_out;
    let bits_per_sec = bytes * 8 * 1_000_000_000 / dt.0;
    assert_eq!(bits_per_sec, TOTAL * 8 * 500, "Δbytes / Δt over the 2 ms window");
}
