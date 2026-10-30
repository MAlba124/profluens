//! `FileSrc::from_file` reads the file it was handed, whatever its path names later.

// A test's own fixtures, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs::File;
use std::path::{Path, PathBuf};

use profluens_elements::io::{FileSink, FileSrc};

fn temp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("pf_fd_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn pattern(len: u32, salt: u32) -> Vec<u8> {
    (0..len).map(|i| ((i + salt) % 251) as u8).collect()
}

fn copy_through(src: FileSrc, out: &Path) -> Vec<u8> {
    let mut p = pf_reactor::pipeline();
    let src = p.add(src);
    let sink = p.add(FileSink::new(out));
    p.link(src.src(FileSrc::OUT), sink.sink(FileSink::IN)).expect("link");
    p.run().expect("run");
    std::fs::read(out).expect("read output")
}

#[test]
fn an_open_file_reads_byte_identically() {
    let dir = temp_dir("plain");
    let (inp, outp) = (dir.join("in.bin"), dir.join("out.bin"));
    let data = pattern(1_000_003, 0);
    std::fs::write(&inp, &data).expect("write input");
    let file = File::open(&inp).expect("open");
    let got = copy_through(FileSrc::from_file(file, &inp), &outp);
    assert_eq!(got.len(), data.len());
    assert!(got == data, "the bytes of the file handed in");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_renamed_and_replaced_path_still_reads_what_was_opened() {
    let dir = temp_dir("rename");
    let (inp, outp) = (dir.join("in.bin"), dir.join("out.bin"));
    let first = pattern(300_001, 7);
    std::fs::write(&inp, &first).expect("write input");
    let file = File::open(&inp).expect("open");
    std::fs::rename(&inp, dir.join("moved.bin")).expect("rename");
    std::fs::write(&inp, pattern(1000, 3)).expect("a new file at the path");
    let got = copy_through(FileSrc::from_file(file, &inp), &outp);
    assert!(got == first, "the original content, {} bytes", got.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_path_replaced_by_a_symlink_still_reads_what_was_opened() {
    let dir = temp_dir("symlink");
    let (inp, outp, other) = (dir.join("in.bin"), dir.join("out.bin"), dir.join("other.bin"));
    let first = pattern(200_003, 11);
    let second = pattern(5000, 1);
    std::fs::write(&inp, &first).expect("write input");
    std::fs::write(&other, &second).expect("write the link target");
    let file = File::open(&inp).expect("open");
    std::fs::remove_file(&inp).expect("unlink");
    std::os::unix::fs::symlink(&other, &inp).expect("symlink");
    let got = copy_through(FileSrc::from_file(file, &inp), &outp);
    assert!(got == first, "the original content, {} bytes", got.len());
    let by_path = copy_through(FileSrc::new(&inp), &dir.join("out2.bin"));
    assert!(by_path == second, "the path form follows the link, the fd form did not");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_open_directory_fails_the_run() {
    let dir = temp_dir("dir");
    let file = File::open(&dir).expect("open the directory");
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::from_file(file, &dir));
    let sink = p.add(FileSink::new(dir.join("out.bin")));
    p.link(src.src(FileSrc::OUT), sink.sink(FileSink::IN)).expect("link");
    assert!(p.run().is_err(), "a directory is not a file to read");
    let _ = std::fs::remove_dir_all(&dir);
}
