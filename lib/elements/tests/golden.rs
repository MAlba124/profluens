//! Golden pipeline tests. Data must arrive with no loss, duplication or reordering.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use profluens_core::pipeline::Pipeline;
use profluens_elements::flow::PassThrough;
use profluens_elements::testing::{FNV_OFFSET, TestSink, TestSrc, fold, pattern_byte};

fn expected_hash(n: u64) -> u64 {
    let mut h = FNV_OFFSET;
    for i in 0..n {
        h = fold(h, pattern_byte(i));
    }
    h
}

#[test]
fn testsrc_to_testsink_transports_exactly() {
    let n = 1_000_003u64; // not a multiple of the buffer size
    let (sink, stats) = TestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(n));
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).expect("link");
    p.run().expect("run");

    assert!(stats.is_done(), "sink saw EOS");
    assert_eq!(stats.bytes(), n, "every byte arrived exactly once");
    assert_eq!(stats.hash(), expected_hash(n), "content intact and in order");
}

#[test]
fn testsrc_passthrough_testsink_matches() {
    let n = 500_009u64;
    let (sink, stats) = TestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(n));
    let mid = p.add(PassThrough::new());
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), mid.sink(PassThrough::IN)).expect("link 1");
    p.link(mid.src(PassThrough::OUT), snk.sink(TestSink::IN)).expect("link 2");
    p.run().expect("run");

    assert_eq!(stats.bytes(), n);
    assert_eq!(stats.hash(), expected_hash(n));
}

#[test]
fn empty_stream() {
    let (sink, stats) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(0));
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).expect("link");
    p.run().expect("run");
    assert!(stats.is_done());
    assert_eq!(stats.bytes(), 0);
    assert_eq!(stats.hash(), FNV_OFFSET, "no bytes → offset basis unchanged");
}

#[test]
fn counters_track_throughput() {
    let n = 300_007u64;
    let (sink, _stats) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(n));
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).expect("link");
    p.run().expect("run");

    let src_c = p.counters(src);
    let snk_c = p.counters(snk);
    assert_eq!(src_c.bytes_out, n, "source produced exactly n bytes");
    assert_eq!(snk_c.bytes_in, n, "sink consumed exactly n bytes");
    assert!(src_c.buffers_out >= 1);
    assert_eq!(
        snk_c.buffers_in, src_c.buffers_out,
        "sink received the same buffer count the source produced"
    );
    assert_eq!(src_c.bytes_in, 0, "a source has no input");
    assert_eq!(snk_c.bytes_out, 0, "a sink has no output");
}

#[test]
fn dump_dot_shows_topology_and_groups() {
    let (sink, _stats) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(0));
    let mid = p.add(PassThrough::new());
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), mid.sink(PassThrough::IN)).unwrap();
    p.link(mid.src(PassThrough::OUT), snk.sink(TestSink::IN)).unwrap();

    let dot = p.dump_dot();
    assert!(dot.contains("digraph profluens"));
    assert!(dot.contains("testsrc") && dot.contains("passthrough") && dot.contains("testsink"));
    assert!(dot.contains("e0 -> e1") && dot.contains("e1 -> e2"));
    assert!(dot.contains("cluster_0") && dot.contains("cluster_1"));
}

#[test]
fn link_validates_pads() {
    let (sink, _stats) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(0));
    let snk = p.add(sink);
    assert!(p.src_pad(src, "nope").is_err());
    assert!(p.src_pad(snk, "src").is_err());
    assert!(p.link(snk.src(TestSrc::OUT), snk.sink(TestSink::IN)).is_err());
    let (out, inp) = (p.src_pad(src, "src").unwrap(), p.sink_pad(snk, "sink").unwrap());
    assert_eq!((out, inp), (TestSrc::OUT, TestSink::IN));
    assert!(p.link(src.src(out), snk.sink(inp)).is_ok());
}

#[test]
fn stop_handle_cancels_a_running_pipeline() {
    let (sink, stats) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(u64::MAX));
    let snk = p.add(sink);
    p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).unwrap();

    let handle = p.control();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        handle.stop();
    });
    p.run().expect("run returns after stop");
    stopper.join().unwrap();

    let n = stats.bytes();
    assert!(n > 0, "transported some bytes before the stop");
    assert!(n < u64::MAX, "did not run to completion");
}
