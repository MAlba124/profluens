//! The seek index of a bare MP3 from `HeadScan`, against values worked out by hand.

// Test fixtures allocate freely.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use pf_mp3::index::HeadScan;
use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::time::Timestamp;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name))
        .expect("a fixture")
}

fn scanned(file: &[u8], chunk: usize, len: Option<u64>) -> (Index, bool) {
    scanned_by(&mut HeadScan::new(len), file, chunk)
}

fn scanned_by(scan: &mut HeadScan, file: &[u8], chunk: usize) -> (Index, bool) {
    for piece in file.chunks(chunk) {
        if let Some(index) = scan.feed(piece) {
            assert!(
                scan.done() && scan.feed(piece).is_none() && scan.end().is_none(),
                "given once"
            );
            return (index, true);
        }
    }
    (scan.end().expect("the index at the end"), false)
}

/// 111 byte ID3v2 tag, then an Info header for 15 frames with LAME delay 576 and padding 1269.
/// 15 * 1152 - 1845 is 15,435 samples, 350 ms at 44.1 kHz, so the line has one entry.
#[test]
fn the_info_header_of_a_tagged_file_makes_a_line_of_one_bitrate() {
    let file = fixture("mono_128_id3.mp3");
    let len = file.len() as u64;
    assert_eq!(len, 6797);
    for chunk in [1, 64, 1000, 1 << 16] {
        let mut scan = HeadScan::new(Some(len));
        let (index, early) = scanned_by(&mut scan, &file, chunk);
        assert!(!early, "chunk {chunk}");
        assert_eq!(index.duration, Timestamp(350_000_000), "chunk {chunk}");
        assert_eq!(index.file_len, Some(len));
        assert!(index.complete, "the header counts the frames");
        assert_eq!(index.entries, [IndexEntry::key(0, 111)]);
        let layout = scan.layout().expect("a layout");
        assert_eq!((layout.first, layout.audio, layout.header), (111, 111 + 417, true));
        assert_eq!(layout.lead, 1152 + 576 + 529);
        assert_eq!(layout.cbr, Some((144 * 128_000, 44_100)));
        assert_eq!(layout.frames, Some(15));
        assert_eq!(layout.frame_at(111 + 417), Some(1));
        assert_eq!(layout.byte_of(15), Some(111 + 417 + 14 * 144 * 128_000 / 44_100));
        assert_eq!(layout.time_ns(1), 0, "ahead of zero");
    }
}

#[test]
fn a_long_file_gives_its_index_once_the_window_is_in() {
    let file = fixture("js_128.mp3").repeat(4);
    let (index, early) = scanned(&file, 4096, Some(file.len() as u64));
    assert!(early, "the window is in before the end");
    assert_eq!(index.entries[0].byte, 0, "no tag");
    assert_eq!(index.file_len, Some(file.len() as u64));
}

#[test]
fn a_scan_of_unknown_length_states_none() {
    let file = fixture("js_128.mp3");
    let (index, _) = scanned(&file, 4096, None);
    assert_eq!(index.entries[0].byte, 0, "no tag");
    assert_eq!(index.file_len, None);
}
