// A test's own streams, not an element.
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::ident::TheoraIdent;
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
    land_on(CLOCK, data, target_ms * 1_000_000)
}

fn land_on(clock: GranuleClock, data: &[u8], target: u64) -> (Option<Landing>, u32) {
    land_ending(clock, data, target, None)
}

fn land_ending(
    clock: GranuleClock,
    data: &[u8],
    target: u64,
    end: Option<u64>,
) -> (Option<Landing>, u32) {
    let mut b = Bisect::new(KEY, clock, target, data.len() as u64).ending(end);
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
    assert!(most <= 4, "{most} probes");
}

#[test]
fn another_streams_pages_and_junk_between_change_nothing() {
    let (data, pages) = stream(300, 20, |i| 200 + (i as usize * 53) % 1200, 3, 5);
    for target in (0..6_100).step_by(11) {
        check(&data, &pages, target);
    }
}

#[test]
fn pages_cut_at_a_probes_end_are_read_whole() {
    let (data, pages) = stream(60, 40, |i| if i % 4 == 1 { 60_000 } else { 700 }, 2, 0);
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

/// 10 frames a second, KFGSHIFT 6, version 3.2.1.
const THEORA: GranuleClock = GranuleClock::Theora(TheoraIdent {
    width: 16,
    height: 16,
    fps_num: 10,
    fps_den: 1,
    shift: 6,
    lag: 0,
});

/// A header page then a frame a page, a keyframe every `gop`, with another stream between.
fn theora(frames: u64, gop: u64) -> (Vec<u8>, Vec<(u64, u64)>) {
    let (mut out, mut pages) = (Vec::new(), vec![(0, 0)]);
    page(&mut out, KEY, 0, 600, 0);
    for i in 0..frames {
        let granule = ((i / gop * gop + 1) << 6) | (i % gop);
        pages.push((out.len() as u64, granule));
        page(&mut out, KEY, granule, 300 + (i as usize * 41) % 2_000, i as u32 + 1);
        if i % 3 == 0 {
            page(&mut out, OTHER, i, 900, i as u32);
        }
    }
    (out, pages)
}

/// The last page ending by `target`, then the last before it ending by the start of the
/// keyframe its last frame decodes from, the first where there is none.
fn keyed(pages: &[(u64, u64)], target: u64) -> Option<(u64, u64)> {
    let ns = |g: u64| THEORA.ns(g).expect("a frame time");
    let found = pages.iter().rev().find(|(_, g)| ns(*g) <= target).copied()?;
    let Some(key) = THEORA.keyframe_ns(found.1) else {
        return Some(found);
    };
    let before = pages.iter().rev().find(|(at, g)| *at < found.0 && ns(*g) <= key);
    Some(before.copied().unwrap_or(found))
}

#[test]
fn a_theora_seek_lands_ahead_of_the_keyframe_its_frame_decodes_from() {
    const FRAME: u64 = 100_000_000;
    let (data, pages) = theora(400, 12);
    let mut most = 0;
    for target in (0..41_000_000_000u64).step_by(37_000_000) {
        let (got, probes) = land_on(THEORA, &data, target);
        most = most.max(probes);
        let got = got.expect("the header page ends by every target");
        assert_eq!(Some((got.at, got.granule)), keyed(&pages, target), "target {target}");
        assert_eq!(Some(got.ns), THEORA.ns(got.granule));
        let at = pages.iter().position(|p| p.0 == got.at).expect("a page start");
        let Some(&(_, next)) = pages.get(at + 1) else { continue };
        assert_eq!(next & 63, 0, "target {target}: the next frame is a keyframe");
        // at most a group of pictures more than the target's own, where the target's page
        // ends on the frame ahead of a keyframe
        let start = THEORA.ns(next).expect("a frame time") - FRAME;
        assert!(start <= target && target - start < 24 * FRAME, "target {target} from {start}");
    }
    assert!(most <= 8, "{most} probes for both searches");
}

#[test]
fn a_theora_target_inside_a_keyframe_page_or_before_any_lands_ahead_of_it() {
    let (data, pages) = theora(60, 12);
    // frame 12 is the keyframe at count 13, its page ends at 1.3 s
    let (got, _) = land_on(THEORA, &data, 1_300_000_000);
    assert_eq!(got.map(|l| l.granule), Some(1 << 6 | 11), "the page before the keyframe");
    // a target before the first frame ends lands on the header page, the search for its
    // keyframe has none to do
    let (got, _) = land_on(THEORA, &data, 50_000_000);
    assert_eq!(got.map(|l| (l.at, l.granule)), Some(pages[0]));
    // a keyframe page with no page ending before its keyframe lands on the target's own
    let mut bare = Vec::new();
    page(&mut bare, KEY, 1 << 6 | 3, 500, 0);
    page(&mut bare, KEY, 1 << 6 | 7, 500, 1);
    assert_eq!(land_on(THEORA, &bare, 600_000_000).0.map(|l| l.granule), Some(1 << 6 | 3));
}

/// Another stream's bos page and the key stream's header page ahead of its frames, as libtheora
/// with libspeex writes them.
fn headed(frames: u64, gop: u64) -> Vec<u8> {
    let mut out = Vec::new();
    page(&mut out, KEY, 0, 42, 0);
    page(&mut out, OTHER, 0, 80, 0);
    page(&mut out, KEY, 0, 3_000, 1);
    page(&mut out, OTHER, 0, 55, 1);
    for i in 0..frames {
        let granule = ((i / gop * gop + 1) << 6) | (i % gop);
        page(&mut out, KEY, granule, 700, i as u32 + 2);
    }
    out
}

#[test]
fn a_landing_on_a_header_page_lands_on_the_first_byte() {
    let data = headed(60, 12);
    // before the first frame ends, on the first group of pictures, at 0
    for target in [0, 50_000_000, 700_000_000, 1_150_000_000] {
        assert_eq!(land_on(THEORA, &data, target).0, Some(Landing::HEAD), "target {target}");
    }
    // the second keyframe's group lands on the frame before it, past the headers
    let got = land_on(THEORA, &data, 1_400_000_000).0.expect("a landing");
    assert_eq!(got.granule, 1 << 6 | 11);
    let (audio, _) = stream(100, 20, |_| 500, 0, 0);
    let mut late = Vec::new();
    page(&mut late, OTHER, 0, 80, 0);
    late.extend_from_slice(&audio);
    assert_eq!(land(&late, 10).0, Some(Landing::HEAD), "an audio header page");
    assert_ne!(land(&late, 40).0, Some(Landing::HEAD));
}

/// Lands every 997th ms of a stream with and without its end time, checked against reading
/// its pages in order, the most probes each took.
fn sweep(data: &[u8], pages: &[(u64, u64)]) -> (u32, u32) {
    let end = Some(pages.last().expect("a page").1 * 1_000_000);
    let (mut most, mut known) = (0, 0);
    for target in (0..pages.last().expect("a page").1 + 100).step_by(997) {
        most = most.max(check(data, pages, target));
        let (got, probes) = land_ending(CLOCK, data, target * 1_000_000, end);
        assert_eq!(got.map(|l| (l.at, l.granule)), want(pages, target), "target {target} ms");
        known = known.max(probes);
    }
    (most, known)
}

/// Ten minutes of 20 ms pages filling a probe dozens of times over. Plain bisection read a page
/// cut at a probe's end again, that read was cut in turn, and it crept on a read a probe, up to
/// 47.
#[test]
fn ten_minutes_land_in_a_probe_or_two() {
    let (data, pages) = stream(30_000, 20, |i| 150 + (i as usize * 37) % 40, 0, 0);
    let (most, known) = sweep(&data, &pages);
    assert!(most <= 3 && known <= 2, "{most} probes, {known} with the end known");
}

/// A first half of ten times the bytes a second of the second half, and the other way round. A
/// line through the bounds misjudges targets past the swing, the halvings between bound them.
#[test]
fn a_bitrate_ten_times_another_halfway_still_lands_within_the_cap() {
    for heavy in [true, false] {
        let len = |i: u64| if (i < 15_000) == heavy { 1_500 } else { 150 };
        let (data, pages) = stream(30_000, 20, len, 0, 0);
        let (most, known) = sweep(&data, &pages);
        assert!(most.max(known) <= 14, "heavy first {heavy}: {most} probes, {known} known");
    }
}
