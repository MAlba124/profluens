//! `opusenc` stamps each packet with its first sample's stream time, counted from the input stamps.

#![allow(clippy::disallowed_methods)] // test fixtures, not a hot path

use pf_harness::Harness;
use pf_opus::OpusEnc;
use profluens_core::event::Event;
use profluens_core::format::ValueDesc;
use profluens_core::time::Timestamp;

const MS: u64 = 1_000_000;
const FRAME: usize = 960 * 2;

fn mono() -> Harness {
    let mut h = Harness::new(OpusEnc::new());
    h.fix_sink_format(
        OpusEnc::IN,
        "audio/raw",
        &[
            ("rate", ValueDesc::Int(48_000)),
            ("channels", ValueDesc::Int(1)),
            ("sample", ValueDesc::Id("s16")),
        ],
    );
    h
}

fn tone(bytes: usize) -> Vec<u8> {
    (0..bytes / 2).flat_map(|i| (((i % 96) as i16 - 48) * 200).to_le_bytes()).collect()
}

fn encode(h: &mut Harness, pcm: &[u8], chunk: usize, from: Option<u64>) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    for c in pcm.chunks(chunk) {
        let mut buf = h.alloc(c);
        if let Some(ms) = from {
            buf.pts = Timestamp::from_nanos(ms * MS + (at / 2) as u64 * 1_000_000_000 / 48_000);
        }
        at += c.len();
        h.push(buf).expect("push");
        out.extend(h.drain_outputs());
    }
    out.extend(h.eos().expect("eos"));
    out.iter().map(|b| (b.pts.0 / MS, b.duration.0 / MS)).collect()
}

#[test]
fn packets_follow_the_input_stamps_across_ragged_chunks() {
    let mut h = mono();
    let got = encode(&mut h, &tone(FRAME * 10), 777, Some(5_000));
    let want: Vec<(u64, u64)> = (0..10).map(|k| (5_000 + 20 * k, 20)).collect();
    assert_eq!(got, want);
}

#[test]
fn unstamped_input_counts_from_zero() {
    let mut h = mono();
    let got = encode(&mut h, &tone(FRAME * 3 + 100), 1000, None);
    assert_eq!(got, [(0, 20), (20, 20), (40, 20), (60, 1)], "the tail is stamped with its 50 samples");
}

#[test]
fn a_flush_counts_again_from_the_next_stamp() {
    let mut h = mono();
    let first = h.alloc(&tone(FRAME + 300));
    h.push(first).expect("push");
    assert_eq!(h.drain_outputs().len(), 1);
    h.push_event(Event::FlushStart).expect("flush");
    h.push_event(Event::FlushStop).expect("flush");
    let got = encode(&mut h, &tone(FRAME * 2), FRAME * 2, Some(30_000));
    assert_eq!(got, [(30_000, 20), (30_020, 20)], "the carried partial frame went with the flush");
}

#[test]
fn a_gap_at_a_frame_boundary_carries_through() {
    let mut h = mono();
    let mut out = Vec::new();
    for at in [0u64, 1_000] {
        let mut buf = h.alloc(&tone(FRAME * 2));
        buf.pts = Timestamp::from_nanos(at * MS);
        h.push(buf).expect("push");
        out.extend(h.drain_outputs());
    }
    let got: Vec<u64> = out.iter().map(|b| b.pts.0 / MS).collect();
    assert_eq!(got, [0, 20, 1_000, 1_020]);
}

fn unformatted() -> Harness {
    Harness::new(OpusEnc::new())
}

fn fix_mono(h: &mut Harness) {
    let fields = [
        ("rate", ValueDesc::Int(48_000)),
        ("channels", ValueDesc::Int(1)),
        ("sample", ValueDesc::Id("s16")),
    ];
    h.change_sink_format(OpusEnc::IN, "audio/raw", &fields).expect("format change");
}

/// Packet count, contiguous 20 ms stamps from `from_ms`, the last stamped with its real samples.
fn assert_covers(got: &[(u64, u64)], from_ms: u64, samples: usize) {
    let n = samples.div_ceil(960);
    let rem = (samples - (n - 1) * 960) as u64;
    let mut want: Vec<(u64, u64)> = (0..n as u64).map(|k| (from_ms + 20 * k, 20)).collect();
    let (last, real) = (got.last().map_or(0, |p| p.1), rem / 48);
    assert!(last == real || last == real + 1, "last packet {last} ms, {rem} samples");
    if let Some(w) = want.last_mut() {
        w.1 = last;
    }
    assert_eq!(got, want);
}

#[test]
fn input_before_the_format_is_encoded_whole_at_eos() {
    let mut h = unformatted();
    let pcm = tone(FRAME * 5 + 300);
    for (i, c) in pcm.chunks(FRAME).enumerate() {
        let mut buf = h.alloc(c);
        buf.pts = Timestamp::from_nanos((7_000 + 20 * i as u64) * MS);
        h.push(buf).expect("push");
    }
    assert!(h.drain_outputs().is_empty(), "nothing encodes before the format");
    fix_mono(&mut h);
    let out = h.eos().expect("eos");
    let got: Vec<(u64, u64)> = out.iter().map(|b| (b.pts.0 / MS, b.duration.0 / MS)).collect();
    assert_covers(&got, 7_000, pcm.len() / 2);
}

#[test]
fn input_before_the_format_continues_into_later_input() {
    let mut h = unformatted();
    let pcm = tone(FRAME * 6 + 100);
    let (early, late) = pcm.split_at(FRAME * 3 + 500);
    let mut buf = h.alloc(early);
    buf.pts = Timestamp::from_nanos(2_000 * MS);
    h.push(buf).expect("push");
    fix_mono(&mut h);
    let mut out = h.drain_outputs();
    assert_eq!(out.len(), 3, "the whole buffered frames encode on the format change");
    let buf = h.alloc(late);
    h.push(buf).expect("push");
    out.extend(h.eos().expect("eos"));
    let got: Vec<(u64, u64)> = out.iter().map(|b| (b.pts.0 / MS, b.duration.0 / MS)).collect();
    assert_covers(&got, 2_000, pcm.len() / 2);
}

#[test]
fn a_known_format_pads_the_partial_tail_into_one_more_packet() {
    for (frames, extra) in [(7usize, 1usize), (4, 959), (3, 0)] {
        let mut h = mono();
        let pcm = tone(FRAME * frames + extra * 2);
        let got = encode(&mut h, &pcm, 1234, Some(1_000));
        assert_covers(&got, 1_000, pcm.len() / 2);
    }
}
