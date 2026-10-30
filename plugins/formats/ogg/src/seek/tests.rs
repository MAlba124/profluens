// A test's own streams, not an element.
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::page::write_page;

const KEY: u32 = 7;
const OTHER: u32 = 9;
const CLOCK: GranuleClock = GranuleClock::Rate(1000);

fn page(out: &mut Vec<u8>, serial: u32, granule: u64, len: usize, seq: u32) {
    let mut lacing = vec![255u8; len / 255];
    lacing.push((len % 255) as u8);
    let payload = vec![seq as u8; len];
    write_page(out, 0, granule, serial, seq, &lacing, &payload);
}

fn stream(
    n: u64,
    step: u64,
    len: impl Fn(u64) -> usize,
    other: u64,
    junk: u64,
) -> (Vec<u8>, Vec<(u64, u64)>) {
    let (mut out, mut pages) = (Vec::new(), Vec::new());
    for i in 0..n {
        pages.push((out.len() as u64, i * step));
        page(&mut out, KEY, i * step, len(i), i as u32);
        if other > 0 && i % other == 0 {
            page(&mut out, OTHER, i * 3, 900, i as u32);
        }
        if junk > 0 && i % junk == 0 {
            out.extend_from_slice(b"junk OggS not a page at all");
        }
    }
    (out, pages)
}

fn land(data: &[u8], target_ms: u64) -> (Option<Landing>, u32) {
    let mut b = Bisect::new(KEY, CLOCK, target_ms * 1_000_000, data.len() as u64);
    for probes in 0.. {
        match b.step() {
            Step::Land(l) => return (l, probes),
            Step::Probe { at, len } => {
                let from = at as usize;
                b.served(at, &data[from..from + len as usize]);
            }
        }
    }
    unreachable!()
}

fn want(pages: &[(u64, u64)], target_ms: u64) -> Option<(u64, u64)> {
    pages.iter().rev().find(|(_, g)| *g <= target_ms).copied()
}

fn check(data: &[u8], pages: &[(u64, u64)], target_ms: u64) -> u32 {
    let (got, probes) = land(data, target_ms);
    let got = got.map(|l| (l.at, l.granule));
    assert_eq!(got, want(pages, target_ms), "target {target_ms} ms");
    if let Some(l) = land(data, target_ms).0 {
        assert_eq!(l.ns, l.granule * 1_000_000);
    }
    probes
}

#[test]
fn every_target_lands_on_the_last_page_ending_by_it() {
    let (data, pages) = stream(400, 20, |i| 300 + (i as usize * 37) % 900, 0, 0);
    let mut most = 0;
    for target in (0..8_100).step_by(7) {
        most = most.max(check(&data, &pages, target));
    }
    assert!(most <= 9, "{most} probes");
}

#[test]
fn another_streams_pages_and_junk_between_change_nothing() {
    let (data, pages) = stream(300, 20, |i| 200 + (i as usize * 53) % 1200, 3, 5);
    for target in (0..6_100).step_by(11) {
        check(&data, &pages, target);
    }
}

#[test]
fn pages_longer_than_a_probe_are_read_whole() {
    let (data, pages) = stream(60, 40, |i| if i % 4 == 1 { 40_000 } else { 700 }, 2, 0);
    for target in (0..2_500).step_by(13) {
        check(&data, &pages, target);
    }
}

#[test]
fn a_target_before_the_first_page_or_past_the_last_lands_on_the_ends() {
    let (data, pages) = stream(100, 20, |_| 500, 0, 0);
    assert_eq!(land(&data, 0).0.map(|l| l.at), Some(0), "the first page ends at 0");
    let (last, _) = land(&data, 1_000_000);
    assert_eq!(last.map(|l| (l.at, l.granule)), pages.last().copied());
    let mut late = Vec::new();
    page(&mut late, KEY, 500, 400, 0);
    page(&mut late, KEY, 900, 400, 1);
    assert_eq!(land(&late, 100).0, None);
}

#[test]
fn a_file_cut_inside_its_last_page_still_lands() {
    let (mut data, pages) = stream(80, 20, |_| 3_000, 0, 0);
    data.truncate(data.len() - 1_000);
    let (got, _) = land(&data, 1_000_000);
    assert_eq!(got.map(|l| l.at), Some(pages[pages.len() - 2].0), "the last whole page");
    check(&data, &pages, 700);
}

#[test]
fn a_stream_of_no_page_of_the_serial_does_not_land() {
    let mut data = Vec::new();
    for i in 0..50 {
        page(&mut data, OTHER, i * 10, 800, i as u32);
    }
    assert_eq!(land(&data, 200).0, None);
}

#[test]
fn another_streams_pages_longer_than_a_probe_between_change_nothing() {
    let (mut data, mut pages) = (Vec::new(), Vec::new());
    for i in 0..80u64 {
        pages.push((data.len() as u64, i * 25));
        page(&mut data, KEY, i * 25, 400, i as u32);
        page(&mut data, OTHER, i, if i % 3 == 0 { 50_000 } else { 9_000 }, i as u32);
    }
    let mut most = 0;
    for target in (0..2_100).step_by(9) {
        most = most.max(check(&data, &pages, target));
    }
    assert!(most < PROBES_MAX / 2, "{most} probes");
}
