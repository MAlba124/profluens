//! `filesrc` on a named pipe. The open does not wait for a writer.

// A test's own fixtures and threads, not an element.
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::time::{Duration, Instant};

use profluens_elements::io::{FileSink, FileSrc};

fn temp(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("pf_pipe_{}_{}", tag, std::process::id()))
}

fn fifo(tag: &str) -> std::path::PathBuf {
    let path = temp(tag);
    let _ = std::fs::remove_file(&path);
    let made = std::process::Command::new("mkfifo").arg(&path).status().expect("mkfifo runs");
    assert!(made.success(), "mkfifo {}", path.display());
    path
}

#[test]
fn a_pipe_with_no_writer_is_stopped_and_does_not_hang() {
    let (pipe, out) = (fifo("idle"), temp("idle_out"));
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&pipe));
    let sink = p.add(FileSink::new(&out));
    p.link(src.src(FileSrc::OUT), sink.sink(FileSink::IN)).expect("link");
    let stop = p.control();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        stop.stop();
    });
    let began = Instant::now();
    p.run().expect("a stopped run is not a failed one");
    stopper.join().expect("the stopper");
    let took = began.elapsed();
    assert!(took >= Duration::from_millis(250), "it ended on its own after {took:?}");
    assert!(took < Duration::from_secs(5), "the stop took {took:?}");
    assert_eq!(std::fs::read(&out).expect("the output").len(), 0);
    let _ = (std::fs::remove_file(&pipe), std::fs::remove_file(&out));
}

#[test]
fn a_writer_that_comes_late_is_read_whole_and_its_leaving_ends_the_run() {
    let (pipe, out) = (fifo("late"), temp("late_out"));
    let data: Vec<u8> = (0..300_007u32).map(|i| (i % 251) as u8).collect();
    let writer = {
        let (pipe, data) = (pipe.clone(), data.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let mut to =
                std::fs::OpenOptions::new().write(true).open(pipe).expect("the writer opens");
            for piece in data.chunks(70_001) {
                to.write_all(piece).expect("write");
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&pipe));
    let sink = p.add(FileSink::new(&out));
    p.link(src.src(FileSrc::OUT), sink.sink(FileSink::IN)).expect("link");
    p.run().expect("the run ends when the writer leaves");
    writer.join().expect("the writer");
    assert!(std::fs::read(&out).expect("the output") == data, "the bytes differ");
    let _ = (std::fs::remove_file(&pipe), std::fs::remove_file(&out));
}
