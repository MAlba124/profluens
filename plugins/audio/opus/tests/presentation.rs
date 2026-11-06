//! `opusdec` and the presentation point, packets encoded in the test behind a pre-skip head.

#![allow(clippy::disallowed_methods)] // test code, the ban is for element hot paths

use pf_harness::Harness;
use pf_opus::libopus::LibopusEncoder;
use pf_opus::{EncoderConfig, OpusDec};
use profluens_core::buffer::Buffer;
use profluens_core::time::Timestamp;

#[path = "../../../../lib/audio/tests/support/exact.rs"]
mod exact;

use exact::{Settle, Stream, grid};

fn pcm(samples: usize, channels: usize) -> Vec<i16> {
    let mut out = Vec::with_capacity(samples * channels);
    for i in 0..samples {
        for c in 0..channels {
            let t = i as f64 / 48_000.0;
            let tone = (std::f64::consts::TAU * (440.0 + 220.0 * c as f64) * t).sin();
            out.push((tone * 9000.0 + (i % 480) as f64 * 8.0) as i16);
        }
    }
    out
}

fn opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
    let mut out = b"OpusHead".to_vec();
    out.extend_from_slice(&[1, channels]);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0]);
    out
}

fn packets(channels: u8, frames: usize) -> Vec<Vec<u8>> {
    let mut enc = LibopusEncoder::new(EncoderConfig::new(channels)).expect("encoder");
    let frame_bytes = enc.frame_bytes();
    let ch = channels as usize;
    let bytes: Vec<u8> =
        pcm(frames * frame_bytes / (2 * ch), ch).iter().flat_map(|s| s.to_le_bytes()).collect();
    let frame = |f: &[u8]| {
        let mut packet = Vec::new();
        enc.encode(f, &mut packet).expect("encode");
        packet
    };
    bytes.chunks_exact(frame_bytes).map(frame).collect()
}

fn new() -> Harness {
    Harness::new(OpusDec::new())
}

#[test]
fn after_a_skip_the_samples_from_the_point_on_are_those_of_a_full_decode() {
    for channels in [1u8, 2] {
        let mut owned = vec![opus_head(channels, 312)];
        owned.extend(packets(channels, 24));
        let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        let stride = 2 * channels as usize;
        let stream = Stream { pieces: &pieces, rate: 48_000, stride, stamp: &|_, _| {}, first: 0 };
        stream.check_skips(&new, 8, 24);
    }
}

const PRE_SKIP: u64 = 312;
const PACKET: u64 = 960;

/// RFC 7845 section 4.6 asks 80 ms of pre-roll, this stream takes 200 ms to converge.
#[test]
fn after_a_seek_to_a_packet_the_first_sample_out_is_the_one_at_the_point() {
    for channels in [1u8, 2] {
        let mut owned = vec![opus_head(channels, PRE_SKIP as u16)];
        owned.extend(packets(channels, 24));
        let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        let first = 3 * 3_600 * 48_000 + 5;
        let stamp = |piece: usize, buffer: &mut Buffer| {
            if let Some(packet) = piece.checked_sub(1) {
                buffer.pts = grid(first + PACKET * packet as u64, 48_000);
            }
        };
        let start_of = |piece: usize| first + PACKET * (piece as u64 - 1) - PRE_SKIP;
        let stride = 2 * channels as usize;
        let stream = Stream { pieces: &pieces, rate: 48_000, stride, stamp: &stamp, first };
        let settle = Settle { frames: 200 * 48, within: 64 };
        stream.check_seeks(&new, (1, &[2, 5, 12, 22]), &start_of, settle);
    }
}

#[test]
fn a_seek_to_the_first_packet_cuts_the_pre_skip_again() {
    let mut owned = vec![opus_head(2, PRE_SKIP as u16)];
    owned.extend(packets(2, 12));
    let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let stamp = |piece: usize, buffer: &mut Buffer| {
        if let Some(packet) = piece.checked_sub(1) {
            buffer.pts = grid(PACKET * packet as u64, 48_000);
        }
    };
    let stream = Stream { pieces: &pieces, rate: 48_000, stride: 4, stamp: &stamp, first: 0 };
    let full = stream.full(&new);
    let mut h = new();
    drop(stream.feed(&mut h, 0..6));
    h.seek(0, Timestamp::ZERO).expect("seek");
    let mut out = stream.feed(&mut h, 1..pieces.len());
    out.extend(stream.finish(&mut h));
    assert_eq!(out[0].pts, Timestamp::ZERO);
    let again: Vec<u8> = out.into_iter().flat_map(|pcm| pcm.bytes).collect();
    assert!(
        again == full,
        "the same samples as from the start, {} for {}",
        again.len(),
        full.len()
    );
}

/// A seek to the start has `oggdemux` read again from byte 0, the head comes again ahead of the
/// first packet. Its pre-skip is cut once, not on top of the one the stamp places.
#[test]
fn a_seek_to_the_start_with_the_head_again_cuts_the_pre_skip_once() {
    let mut owned = vec![opus_head(2, PRE_SKIP as u16)];
    owned.extend(packets(2, 12));
    let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let stamp = |piece: usize, buffer: &mut Buffer| {
        if let Some(packet) = piece.checked_sub(1) {
            buffer.pts = grid(PACKET * packet as u64, 48_000);
        }
    };
    let stream = Stream { pieces: &pieces, rate: 48_000, stride: 4, stamp: &stamp, first: 0 };
    let full = stream.full(&new);
    let mut h = new();
    drop(stream.feed(&mut h, 0..6));
    h.seek(0, Timestamp::ZERO).expect("seek");
    let mut out = stream.feed(&mut h, 0..pieces.len());
    out.extend(stream.finish(&mut h));
    assert_eq!(out[0].pts, Timestamp::ZERO);
    let again: Vec<u8> = out.into_iter().flat_map(|pcm| pcm.bytes).collect();
    assert!(
        again == full,
        "the same samples as from the start, {} for {}",
        again.len(),
        full.len()
    );
}

#[test]
fn a_declared_delay_that_repeats_the_pre_skip_is_taken_off_once() {
    use profluens_core::format::ValueDesc;
    let mut owned = vec![opus_head(2, PRE_SKIP as u16)];
    owned.extend(packets(2, 12));
    let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let stamp = |piece: usize, buffer: &mut Buffer| {
        if let Some(packet) = piece.checked_sub(1) {
            buffer.pts = grid(PACKET * packet as u64, 48_000);
        }
    };
    let stream = Stream { pieces: &pieces, rate: 48_000, stride: 4, stamp: &stamp, first: 0 };
    let full = stream.full(&new);
    let declaring = |ns: u64| {
        let mut h = new();
        h.fix_sink_format(OpusDec::IN, "opus", &[("delay", ValueDesc::Int(ns as i64))]);
        let mut out = stream.feed(&mut h, 0..pieces.len());
        out.extend(stream.finish(&mut h));
        let first = out.first().map(|pcm| pcm.pts);
        (first, out.into_iter().flat_map(|pcm| pcm.bytes).collect::<Vec<u8>>())
    };
    let (first, same) = declaring(grid(PRE_SKIP, 48_000).0);
    assert_eq!(first, Some(Timestamp::ZERO));
    assert!(same == full, "{} bytes for {}", same.len(), full.len());
    for hostile in [7_200_000_000_000, i64::MAX as u64] {
        let (first, nothing) = declaring(hostile);
        assert!(first.is_none() && nothing.is_empty(), "{} bytes out", nothing.len());
    }
}

/// Within 64 steps after 200 ms, not after 80 (RFC 7845 section 4.6, RFC 9559 section 5.1.4.1.26).
#[test]
fn a_target_past_the_pre_roll_starts_within_a_few_steps_of_a_full_decode() {
    let mut owned = vec![opus_head(2, PRE_SKIP as u16)];
    owned.extend(packets(2, 36));
    let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let stamp = |piece: usize, buffer: &mut Buffer| {
        if let Some(packet) = piece.checked_sub(1) {
            buffer.pts = grid(PACKET * packet as u64, 48_000);
        }
    };
    let start_of = |piece: usize| PACKET * (piece as u64 - 1) - PRE_SKIP;
    let stream = Stream { pieces: &pieces, rate: 48_000, stride: 4, stamp: &stamp, first: 0 };
    let restarts = [3, 9];
    let within = |steps: i32| {
        stream.primed_after(&new, (1, &restarts), &start_of, (PACKET, 20 * PACKET, steps))
    };
    let (near, nearer) = (within(64), within(8));
    eprintln!("opus: within 64 steps {near:?} frames past a restart, within 8 steps {nearer:?}");
    assert!(near.is_some_and(|frames| frames > 80 * 48 && frames <= 200 * 48), "{near:?}");
}
