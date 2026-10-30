//! A file copied through `filesrc` and `filesink`, odd length so the last read and write are
//! partial.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use profluens_core::pipeline::Pipeline;
use profluens_elements::io::{FileSink, FileSrc};

use pf_reactor::Want;

fn temp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("pf-reactor-pipe-{}-{tag}", std::process::id()))
}

#[test]
fn copies_bytes_identically_over_the_ring() {
    copies_bytes_identically(Want::Uring, "uring");
}

#[test]
fn copies_bytes_identically_over_epoll() {
    copies_bytes_identically(Want::Epoll, "epoll");
}

fn copies_bytes_identically(want: Want, tag: &str) {
    let inp = temp_path(&format!("copy_in_{tag}"));
    let outp = temp_path(&format!("copy_out_{tag}"));
    let data: Vec<u8> = (0..3_000_003u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&inp, &data).expect("write input");

    let mut p = Pipeline::new();
    p.set_reactor_factory(pf_reactor::factory_on(want));
    let src = p.add(FileSrc::new(&inp));
    let sink = p.add(FileSink::new(&outp));
    p.link(src.src(FileSrc::OUT), sink.sink(FileSink::IN)).expect("link");
    p.run().expect("run");

    let got = std::fs::read(&outp).expect("read output");
    assert_eq!(got.len(), data.len());
    assert!(got == data, "the copy differs");
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
}

#[test]
fn copies_an_empty_file_over_the_ring() {
    let inp = temp_path("empty_in");
    let outp = temp_path("empty_out");
    std::fs::write(&inp, b"").expect("write input");
    let mut p = Pipeline::new();
    p.set_reactor_factory(pf_reactor::factory_on(Want::Uring));
    let src = p.add(FileSrc::new(&inp));
    let sink = p.add(FileSink::new(&outp));
    p.link(src.src(FileSrc::OUT), sink.sink(FileSink::IN)).expect("link");
    p.run().expect("run");
    assert!(std::fs::read(&outp).expect("read output").is_empty());
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
}
