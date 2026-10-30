//! Time-based seeking. A seek rebases running time to the target. Driven by a MockClock.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use profluens_core::clock::MockClock;
use profluens_core::id::ElementId;
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_elements::testing::{TimedTestSink, TimedTestSrc};

fn settle(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

const PERIOD: Timestamp = Timestamp::from_millis(10);

// Without the rebase this run would need ~300 ms of advances, with it ~110 ms.
#[test]
fn forward_seek_schedules_immediately() {
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(30, PERIOD)); // 300 ms stream
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    let seek = p.control();

    let run = std::thread::spawn(move || p.run());
    assert!(settle(|| stats.count() >= 1), "first buffer rendered");

    let target = Timestamp::from_millis(200);
    let _ = seek.seek_bytes(target, &[(profluens_core::command::ALL, 0, target)]);

    let mut advanced = Timestamp::ZERO;
    let mut guard = 0;
    while !run.is_finished() {
        clock.advance(PERIOD);
        advanced = advanced.saturating_add(PERIOD);
        std::thread::sleep(Duration::from_millis(1));
        guard += 1;
        assert!(guard < 5_000, "run did not finish (stalled seek?)");
    }
    run.join().expect("joined").expect("run ok");

    let renders = stats.renders();
    let post: Vec<_> = renders.iter().filter(|r| r.pts >= target).collect();
    assert!(!post.is_empty(), "post-seek buffers rendered: {renders:?}");
    assert_eq!(post.last().unwrap().pts, Timestamp::from_millis(290), "tail completed");
    for r in &post {
        assert!(r.rendered_at >= r.pts, "never renders ahead of the clock: {r:?}");
    }
    assert!(
        advanced < Timestamp::from_millis(160),
        "forward seek must not stall: advanced {advanced:?}"
    );
}

#[test]
fn backward_seek_replays_paced() {
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(30, PERIOD));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    let seek = p.control();

    let run = std::thread::spawn(move || p.run());

    let mut guard = 0;
    while stats.count() < 10 {
        clock.advance(PERIOD);
        std::thread::sleep(Duration::from_millis(1));
        guard += 1;
        assert!(guard < 5_000, "warm-up did not reach 10 renders");
    }

    let target = Timestamp::from_millis(20);
    let _ = seek.seek_bytes(target, &[(profluens_core::command::ALL, 0, target)]);

    // Lock-step so virtual time never races ahead of the render thread.
    let mut guard = 0;
    while !run.is_finished() {
        let c = stats.count();
        clock.advance(PERIOD);
        let _ = settle(|| stats.count() > c || run.is_finished());
        guard += 1;
        assert!(guard < 1_000, "run did not finish after backward seek");
    }
    run.join().expect("joined").expect("run ok");

    // Slice where pts jumps back. Pre-seek renders may still land after the async flush.
    let renders = stats.renders();
    let boundary = renders
        .windows(2)
        .position(|w| w[1].pts < w[0].pts)
        .map(|i| i + 1)
        .expect("replay boundary (a backward pts jump) exists");
    let post = &renders[boundary..];
    assert!(!post.is_empty(), "replayed tail rendered");
    assert!(
        post[0].pts <= Timestamp::from_millis(40),
        "replay starts near the 20 ms target, got {:?}",
        post[0].pts
    );
    assert_eq!(post.last().unwrap().pts, Timestamp::from_millis(290));
    for w in post.windows(2) {
        assert!(w[1].pts > w[0].pts, "monotonic replay: {w:?}");
    }
    for r in post {
        assert!(r.rendered_at >= r.pts, "never ahead of the clock: {r:?}");
    }
    // Rendered on the rebased clock, not ~80 ms late.
    assert!(
        post[0].rendered_at.saturating_sub(post[0].pts) <= Timestamp::from_millis(60),
        "replay re-scheduled on the rebased clock: {:?}",
        post[0]
    );
}

#[test]
fn seek_while_paused_resumes_from_target() {
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(30, PERIOD));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    let seek = p.control();
    let pause = p.control();
    let tap = p.tap_handle();

    let run = std::thread::spawn(move || p.run());
    assert!(settle(|| stats.count() >= 1), "first buffer rendered");

    pause.pause();
    std::thread::sleep(Duration::from_millis(10));
    let produced_before = tap.snapshot(ElementId(0)).unwrap().buffers_out;

    let target = Timestamp::from_millis(150);
    let _ = seek.seek_bytes(target, &[(profluens_core::command::ALL, 0, target)]);

    // The flush runs while paused, so a display sink can preroll the seeked frame.
    assert!(
        settle(|| tap.snapshot(ElementId(0)).unwrap().buffers_out > produced_before),
        "source re-primes while paused (flush not deferred to resume)"
    );
    assert_eq!(tap.now(), target, "position shows the seek target while paused");
    let held = stats.count();

    clock.advance(Timestamp::from_secs(10));
    assert!(
        !settle(|| stats.count() > held),
        "nothing renders while paused (got {} > {held})",
        stats.count()
    );

    pause.resume();
    let mut guard = 0;
    while !run.is_finished() {
        clock.advance(PERIOD);
        std::thread::sleep(Duration::from_millis(1));
        guard += 1;
        assert!(guard < 5_000, "run did not finish after resume");
    }
    run.join().expect("joined").expect("run ok");

    let renders = stats.renders();
    let post = &renders[held as usize..];
    assert!(!post.is_empty(), "post-resume renders exist");
    assert_eq!(post[0].pts, target, "resumes at the seek target");
    assert!(
        post[0].rendered_at.saturating_sub(post[0].pts) <= Timestamp::from_millis(50),
        "paused interval excised: rendered_at {:?} vs pts {:?}",
        post[0].rendered_at,
        post[0].pts
    );
}

#[test]
fn tap_position_jumps_to_seek_target() {
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(30, PERIOD));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    let seek = p.control();
    let tap = p.tap_handle();

    let run = std::thread::spawn(move || p.run());
    assert!(settle(|| stats.count() >= 1), "first buffer rendered");

    let target = Timestamp::from_millis(120);
    let _ = seek.seek_bytes(target, &[(profluens_core::command::ALL, 0, target)]);
    assert_eq!(tap.now(), target, "position reads the seek target");

    let mut guard = 0;
    while !run.is_finished() {
        clock.advance(PERIOD);
        std::thread::sleep(Duration::from_millis(1));
        guard += 1;
        assert!(guard < 5_000, "run did not finish");
    }
    run.join().expect("joined").expect("run ok");
}

#[test]
fn rapid_seeks_last_target_wins() {
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(30, PERIOD));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    let seek = p.control();

    let run = std::thread::spawn(move || p.run());
    assert!(settle(|| stats.count() >= 1), "first buffer rendered");

    let _ = seek.seek_bytes(
        Timestamp::from_millis(100),
        &[(profluens_core::command::ALL, 0, Timestamp::from_millis(100))],
    );
    let _ = seek.seek_bytes(
        Timestamp::from_millis(240),
        &[(profluens_core::command::ALL, 0, Timestamp::from_millis(240))],
    );

    let mut guard = 0;
    while !run.is_finished() {
        clock.advance(PERIOD);
        std::thread::sleep(Duration::from_millis(1));
        guard += 1;
        assert!(guard < 5_000, "run did not finish");
    }
    run.join().expect("joined").expect("run ok");

    let renders = stats.renders();
    let t2 = Timestamp::from_millis(240);
    let first_t2 = renders.iter().position(|r| r.pts >= t2).expect("t2 range rendered");
    for r in &renders[first_t2..] {
        assert!(r.pts >= t2, "no pre-target-2 pts after the last flush: {r:?}");
    }
    assert_eq!(renders.last().unwrap().pts, Timestamp::from_millis(290));
}
