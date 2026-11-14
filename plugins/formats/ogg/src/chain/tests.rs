// A test's own files, not an element.
#![allow(clippy::disallowed_methods)]

use profluens_core::fetch::{drive_slice, drive_slice_log};

use super::*;
use crate::writer::OggWriter;

const PRE_SKIP: u64 = 312;

fn opus_head(channels: u8) -> Vec<u8> {
    let mut h = b"OpusHead\x01".to_vec();
    h.push(channels);
    h.extend_from_slice(&(PRE_SKIP as u16).to_le_bytes());
    h.extend_from_slice(&[0x80, 0xBB, 0, 0, 0, 0, 0]);
    h
}

/// The streams of one link, each `packets` 20 ms packets of `size` bytes, ten a page, their
/// pages taking turns.
fn link(out: &mut Vec<u8>, serials: &[u32], packets: u64, size: usize) {
    let mut w: Vec<OggWriter> = serials.iter().map(|s| OggWriter::new(*s)).collect();
    for w in &mut w {
        w.write_packet(out, &opus_head(2), 0).unwrap();
        w.flush(out);
    }
    for w in &mut w {
        w.write_packet(out, b"OpusTags\0\0\0\0\0\0\0\0", 0).unwrap();
        w.flush(out);
    }
    for i in 0..packets {
        for w in &mut w {
            let mut p = vec![(i % 251) as u8; size];
            p[0] = 31 << 3;
            w.write_packet(out, &p, (i + 1) * 960).unwrap();
            if i % 10 == 9 {
                w.flush(out);
            }
        }
    }
    for w in &mut w {
        w.finish(out);
    }
}

fn ns(packets: u64) -> u64 {
    (packets * 960 - PRE_SKIP) * 1_000_000_000 / 48_000
}

#[test]
fn every_link_is_found_with_its_start_and_time() {
    let sizes = [(1000u64, 400usize), (50, 300), (600, 500), (3, 10)];
    let (mut file, mut starts) = (Vec::new(), Vec::new());
    for (i, (packets, size)) in sizes.iter().enumerate() {
        starts.push(file.len() as u64);
        link(&mut file, &[100 + i as u32], *packets, *size);
    }
    let mut log = Vec::new();
    let links = drive_slice_log(&mut ChainScan::new(file.len() as u64), &file, &mut log)
        .unwrap()
        .expect("timed");
    assert_eq!(links.links.len(), sizes.len());
    let mut start_ns = 0;
    for (i, l) in links.links.iter().enumerate() {
        assert_eq!(
            (l.at, l.serial, &l.serials[..]),
            (starts[i], 100 + i as u32, &[100 + i as u32][..])
        );
        assert_eq!((l.start_ns, l.ns), (start_ns, ns(sizes[i].0)), "link {i}");
        start_ns += l.ns;
        assert_eq!(links.end(i), starts.get(i + 1).copied().unwrap_or(file.len() as u64));
    }
    assert_eq!(links.total_ns(), start_ns);
    assert_eq!(links.at_time(0), 0);
    assert_eq!(links.at_time(links.links[2].start_ns), 2);
    assert_eq!(links.at_time(u64::MAX), 3);
    assert_eq!(links.of_serial(102), Some(2));
    assert!(log.len() < 80, "{} reads", log.len());
    // a tail read already gives the same
    let tail = &file[file.len() - OGG_TAIL_LEN..];
    let again = drive_slice(&mut ChainScan::with_tail(file.len() as u64, tail), &file).unwrap();
    assert_eq!(again, Some(links));
}

#[test]
fn grouped_links_are_read_by_their_first_stream() {
    let mut file = Vec::new();
    link(&mut file, &[1, 2], 400, 300);
    let second = file.len() as u64;
    link(&mut file, &[3, 4], 200, 300);
    let links = drive_slice(&mut ChainScan::new(file.len() as u64), &file).unwrap().unwrap();
    assert_eq!(links.links.len(), 2);
    assert_eq!((links.links[1].at, &links.links[1].serials[..]), (second, &[3, 4][..]));
    assert_eq!(links.links[1].ns, ns(200));
}

#[test]
fn one_link_is_one_and_a_stray_page_leaves_the_file_untimed() {
    let mut file = Vec::new();
    link(&mut file, &[1], 300, 300);
    let links = drive_slice(&mut ChainScan::new(file.len() as u64), &file).unwrap().unwrap();
    assert_eq!(links.links.len(), 1);
    assert_eq!(links.total_ns(), ns(300));
    // a page of a serial of no link behind the first, no bos page ahead of it
    let mut stray = Vec::new();
    link(&mut stray, &[1], 300, 300);
    let mut w = OggWriter::new(9);
    w.write_packet(&mut Vec::new(), b"x", 0).unwrap();
    w.flush(&mut Vec::new());
    w.write_packet(&mut stray, &[31 << 3, 1], 960).unwrap();
    w.finish(&mut stray);
    assert_eq!(drive_slice(&mut ChainScan::new(stray.len() as u64), &stray).unwrap(), None);
}

#[test]
fn a_tail_of_no_page_leaves_the_file_untimed() {
    let file = vec![0u8; 100_000];
    assert_eq!(drive_slice(&mut ChainScan::new(file.len() as u64), &file).unwrap(), None);
}
