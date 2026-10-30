//! Regression test. Idle group threads must park, not spin. Its own binary, so process CPU
//! counts only this pipeline.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_elements::testing::{TimedTestSink, TimedTestSrc};

/// Process CPU time in ms, which unlike wall time ignores machine load.
fn process_cpu_ms() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    // comm may contain spaces, so index past the last ')'.
    let after = stat.rsplit(')').next().expect("stat has a comm field");
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = fields[11].parse().expect("utime");
    let stime: u64 = fields[12].parse().expect("stime");
    // USER_HZ is 100 on every Linux ABI this crate targets.
    (utime + stime) * 1000 / 100
}

#[test]
fn idle_groups_park_instead_of_spinning() {
    // Two pool slots keep the source starved most of each 50 ms period.
    let (sink, stats) = TimedTestSink::new();
    let mut p = Pipeline::new();
    p.set_pool(1024, 2);
    let src = p.add(TimedTestSrc::new(20, Timestamp::from_millis(50)));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");

    let cpu_before = process_cpu_ms();
    let t0 = std::time::Instant::now();
    p.run().expect("run");
    let wall = t0.elapsed();
    let cpu = process_cpu_ms().saturating_sub(cpu_before);

    assert_eq!(stats.count(), 20, "every buffer rendered");
    assert!(wall >= Duration::from_millis(900), "sink did not pace: {wall:?}");
    // A spin burns > 900 ms. Parked groups measure ~10-40 ms.
    assert!(
        cpu < 500,
        "group threads burned {cpu} ms CPU over a ~{wall:?} idle-paced run, spinning?"
    );
}
