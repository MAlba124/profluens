// Test code builds its inputs on the heap.
#![allow(clippy::disallowed_methods)]

use super::*;

const MS: u64 = 1_000_000;
const MOVI_DATA: u64 = 2_048;
const BASE: u64 = MOVI_DATA - 4;

fn tracks() -> Vec<Track> {
    vec![
        Track { number: 0, sample_size: 0, tick_ns: 40 * MS, audio: false, lead_ns: 0 },
        Track { number: 1, sample_size: 0, tick_ns: 32 * MS, audio: true, lead_ns: 5 * MS },
    ]
}

fn scan(file_len: u64) -> Scan {
    Scan::new(tracks(), 0, MOVI_DATA, Some(MOVI_DATA), Some(file_len))
}

fn body(rows: &[(&[u8; 4], u32, u32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (id, flags, off, size) in rows {
        out.extend_from_slice(*id);
        for v in [flags, off, size] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

const K: u32 = AVIIF_KEYFRAME;
use crate::riff::AVIIF_KEYFRAME;

fn lagging() -> Vec<(&'static [u8; 4], u32, u32, u32)> {
    vec![
        (b"00dc", K, 4, 100),
        (b"01wb", 0, 112, 50),
        (b"00dc", 0, 170, 100),
        (b"01wb", 0, 278, 50),
        (b"00dc", K, 336, 100),
        (b"01wb", K, 444, 50),
    ]
}

#[test]
fn movi_relative_offsets_resolve_from_the_movi_fourcc() {
    let mut s = scan(100_000);
    s.push(&body(&lagging()));
    assert_eq!(s.entries, [IndexEntry::key(0, BASE + 4), IndexEntry::key(80 * MS, BASE + 336)]);
    assert_eq!(s.read, 6);
}

#[test]
fn absolute_offsets_are_taken_as_they_are() {
    let rows: Vec<_> =
        lagging().into_iter().map(|(id, f, off, size)| (id, f, off + BASE as u32, size)).collect();
    let mut s = scan(100_000);
    s.push(&body(&rows));
    assert_eq!(s.entries, [IndexEntry::key(0, BASE + 4), IndexEntry::key(80 * MS, BASE + 336)]);
}

#[test]
fn without_the_first_chunk_the_file_length_judges() {
    let rows: Vec<_> =
        lagging().into_iter().map(|(id, f, off, size)| (id, f, off + BASE as u32, size)).collect();
    let mut s = Scan::new(tracks(), 0, MOVI_DATA, None, Some(BASE + 600));
    s.push(&body(&rows));
    assert_eq!(s.entries[1].byte, BASE + 336);
    let mut s = Scan::new(tracks(), 0, MOVI_DATA, None, Some(100_000));
    s.push(&body(&lagging()));
    assert_eq!(s.entries[1].byte, BASE + 336);
}

#[test]
fn only_flagged_pictures_are_keyframes() {
    let mut rows = lagging();
    rows.insert(2, (b"rec ", K, 160, 0));
    rows.insert(3, (b"07dc", K, 161, 0));
    let mut s = scan(100_000);
    s.push(&body(&rows));
    let times: Vec<u64> = s.entries.iter().map(|e| e.time_ns).collect();
    assert_eq!(times, [0, 80 * MS], "frame 0 and frame 2");
}

#[test]
fn a_truncated_idx1_indexes_its_whole_entries() {
    let all = body(&lagging());
    let mut whole = scan(100_000);
    whole.push(&all);
    let mut cut = scan(100_000);
    cut.push(&all[..all.len() - 7]);
    assert_eq!(cut.read, 5);
    assert_eq!(cut.entries, whole.entries, "the cut entry is audio, the keyframes are in");
    let mut cut = scan(100_000);
    cut.push(&all[..16 * 4 + 3]);
    assert_eq!(cut.entries, [IndexEntry::key(0, BASE + 4)]);
    let mut pieces = scan(100_000);
    for piece in all.chunks(32) {
        pieces.push(piece);
    }
    assert_eq!(pieces.entries, whole.entries);
}

#[test]
fn audio_ahead_restarts_at_its_chunk_with_the_clocks_there() {
    let rows = [
        (b"01wb", 0, 4, 10),   // a0 at 0
        (b"01wb", 0, 22, 10),  // a1 at 32
        (b"00dc", K, 40, 10),  // v0 at 0
        (b"01wb", 0, 58, 10),  // a2 at 64
        (b"00dc", 0, 76, 10),  // v1 at 40
        (b"01wb", 0, 94, 10),  // a3 at 96
        (b"01wb", 0, 112, 10), // a4 at 128
        (b"00dc", K, 130, 10), // v2 at 80
        (b"01wb", 0, 148, 10), // a5 at 160
    ];
    let mut s = scan(100_000);
    s.push(&body(&rows));
    let want = [
        IndexEntry { time_ns: 0, byte: BASE + 40, restart: BASE + 4, restart_ns: 0 },
        IndexEntry { time_ns: 80 * MS, byte: BASE + 130, restart: BASE + 58, restart_ns: 64 * MS },
    ];
    assert_eq!(s.entries, want);
    assert_eq!(s.clocks_at(BASE + 4), Some(&[0, 0][..]));
    assert_eq!(s.clocks_at(BASE + 58), Some(&[1, 2][..]), "next are v1 and a2");
    assert_eq!(s.clocks_at(BASE + 130), None, "no entry restarts at v2's own byte");
    assert_eq!(s.cut, 0);
}

#[test]
fn audio_behind_restarts_at_the_keyframe() {
    let mut s = scan(100_000);
    s.push(&body(&lagging()));
    assert!(s.entries.iter().all(|e| (e.restart, e.restart_ns) == (e.byte, e.time_ns)));
    assert_eq!(s.clocks_at(BASE + 336), Some(&[2, 2][..]));
}

#[test]
fn audio_past_the_ring_restarts_at_the_keyframe() {
    let mut rows: Vec<(&[u8; 4], u32, u32, u32)> =
        (0..RING as u32 + 50).map(|n| (b"01wb", 0, 4 + n * 18, 10)).collect();
    rows.push((b"00dc", K, 4 + (RING as u32 + 50) * 18, 10));
    let mut s = scan(1 << 30);
    s.push(&body(&rows));
    let key = BASE + 4 + (RING as u64 + 50) * 18;
    assert_eq!(s.entries, [IndexEntry::key(0, key)]);
    assert_eq!(s.cut, 1);
    assert_eq!(s.clocks_at(key), Some(&[0, RING as u64 + 50][..]));
}

#[test]
fn a_sample_size_counts_samples() {
    assert_eq!(ticks(0, 999), 1);
    assert_eq!(ticks(4, 4_096), 1_024);
    assert_eq!(ticks(4, 2), 1, "a short chunk is one");
}

#[test]
fn the_rate_is_the_slope_of_idx1_else_what_was_measured_else_the_average() {
    const S: u64 = 1_000_000_000;
    let entries = (0..=20u64)
        .map(|t| IndexEntry::key(t * S, if t <= 10 { t } else { 10 + 3 * (t - 10) } * 100_000))
        .collect();
    let indexed = Index {
        entries,
        file_len: Some(4_000_000),
        duration: Timestamp(20 * S),
        ..Index::default()
    };
    let mut measured = ByteRate::default();
    for k in 0..=4u64 {
        measured.note(k * 25_000, Timestamp(k * S / 2));
    }
    let bps = |b: Option<Bitrate>| b.map(|b| (b.bits_per_second, b.kind));
    use profluens_core::feedback::RateKind::{Average, Local};
    assert_eq!(
        bps(rate_at(&indexed, &measured, Timestamp(12 * S))),
        Some((2_400_000, Local)),
        "the slope ahead"
    );
    let bare = Index { entries: Vec::new(), ..indexed.clone() };
    assert_eq!(
        bps(rate_at(&bare, &measured, Timestamp(12 * S))),
        Some((400_000, Local)),
        "measured"
    );
    assert_eq!(
        bps(rate_at(&bare, &ByteRate::default(), Timestamp::ZERO)),
        Some((1_600_000, Average)),
        "the average"
    );
    let unknown = Index { file_len: None, ..bare };
    assert_eq!(rate_at(&unknown, &ByteRate::default(), Timestamp::ZERO), None);
}
