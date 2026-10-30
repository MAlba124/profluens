//! Pause freezes running time and resume excises the paused interval. MockClock driven.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use profluens_core::clock::MockClock;
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

#[test]
fn pause_holds_rendering_resume_excises_the_interval() {
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(5, Timestamp::from_millis(10)));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    let pause = p.control();

    let run = std::thread::spawn(move || p.run());

    assert!(settle(|| stats.count() >= 1), "first buffer rendered");
    let rendered_before_pause = stats.count();

    pause.pause();
    clock.advance(Timestamp::from_secs(10));
    assert!(
        !settle(|| stats.count() > rendered_before_pause),
        "nothing rendered while paused (got {} > {})",
        stats.count(),
        rendered_before_pause
    );

    pause.resume();
    while !run.is_finished() {
        clock.advance(Timestamp::from_millis(20));
        std::thread::yield_now();
    }
    run.join().expect("joined").expect("run ok");
    assert_eq!(stats.renders().len(), 5, "full schedule after resume");
}

#[test]
fn start_paused_holds_until_resume() {
    let clock = MockClock::new();
    let (sink, stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(3, Timestamp::from_millis(10)));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    p.start_paused(true);
    let pause = p.control();

    let run = std::thread::spawn(move || p.run());

    assert!(
        !settle(|| stats.count() > 0),
        "held everything while start-paused (rendered {})",
        stats.count()
    );
    assert!(pause.is_paused());

    pause.resume();
    while !run.is_finished() {
        clock.advance(Timestamp::from_millis(20));
        std::thread::yield_now();
    }
    run.join().expect("joined").expect("run ok");
    assert_eq!(stats.renders().len(), 3, "full schedule after the first resume");
}

#[test]
fn toggle_and_stop_while_paused() {
    let clock = MockClock::new();
    let (sink, _stats) = TimedTestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TimedTestSrc::new(100, Timestamp::from_millis(10)));
    let snk = p.add(sink);
    p.link(src.src(TimedTestSrc::OUT), snk.sink(TimedTestSink::IN)).expect("link");
    p.set_clock(Arc::new(clock.clone()));
    let pause = p.control();
    let stop = p.control();

    assert!(pause.toggle(), "now paused");
    assert!(pause.is_paused());

    let run = std::thread::spawn(move || p.run());
    std::thread::sleep(Duration::from_millis(20)); // let it park in the gate

    stop.stop();
    clock.advance(Timestamp::from_secs(100));
    let joined = std::thread::spawn(move || run.join());
    for _ in 0..300 {
        if joined.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(joined.is_finished(), "stop unstuck the paused pipeline");
    let _ = joined.join();
}
