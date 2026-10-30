//! `filesrc ! passthrough ! filesink` through the passive-inline group path.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use profluens_elements::flow::PassThrough;
use profluens_elements::io::{FileSink, FileSrc};

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("pf_tf_{}_{}.bin", tag, std::process::id()));
    p
}

#[test]
fn passthrough_copy_is_byte_identical() {
    let inp = temp_path("in");
    let outp = temp_path("out");
    let data: Vec<u8> = (0..1_000_003u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&inp, &data).expect("write input");

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&inp));
    let mid = p.add(PassThrough::new());
    let sink = p.add(FileSink::new(&outp));
    p.link(src.src(FileSrc::OUT), mid.sink(PassThrough::IN)).expect("link src->mid");
    p.link(mid.src(PassThrough::OUT), sink.sink(FileSink::IN)).expect("link mid->sink");
    p.run().expect("run");

    let got = std::fs::read(&outp).expect("read output");
    assert_eq!(got, data, "passthrough forwarded bytes unchanged");

    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
}
