//! End-to-end logging wiring. Enabling logging must not perturb the transported data.

use profluens_core::log::Level;
use profluens_core::pipeline::Pipeline;
use profluens_elements::testing::{FNV_OFFSET, TestSink, TestSrc, fold, pattern_byte};

fn expected_hash(n: u64) -> u64 {
    let mut h = FNV_OFFSET;
    for i in 0..n {
        h = fold(h, pattern_byte(i));
    }
    h
}

fn run_and_digest(n: u64, level: Option<Level>) -> (u64, u64) {
    let (sink, stats) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(n));
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).expect("link");
    if let Some(l) = level {
        p.log_to_stderr(l);
    }
    p.run().expect("run completes with logging wired");
    assert!(stats.is_done(), "sink saw EOS");
    (stats.bytes(), stats.hash())
}

#[test]
fn run_with_trace_logging_is_byte_identical_and_terminates() {
    let n = 1_000_003u64; // not a multiple of the buffer size
    let (bytes, hash) = run_and_digest(n, Some(Level::Trace));
    assert_eq!(bytes, n, "every byte arrived exactly once with logging on");
    assert_eq!(hash, expected_hash(n), "content intact and in order");
}

#[test]
fn logging_on_and_off_agree() {
    let n = 300_007u64;
    let off = run_and_digest(n, None);
    let on = run_and_digest(n, Some(Level::Debug));
    assert_eq!(off, on, "logging changed only what is emitted, not the data");
    assert_eq!(off.1, expected_hash(n));
}

#[test]
fn empty_stream_with_logging_terminates() {
    let (bytes, hash) = run_and_digest(0, Some(Level::Trace));
    assert_eq!(bytes, 0);
    assert_eq!(hash, FNV_OFFSET);
}
