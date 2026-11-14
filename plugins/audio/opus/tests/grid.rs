//! `opusenc` with `aligned`, frames on an absolute 20 ms grid of the input timeline, so a run
//! started mid-stream cuts and stamps its frames where a full run did.

#![allow(clippy::disallowed_methods)] // test fixtures, not a hot path

use pf_harness::Harness;
use pf_opus::OpusEnc;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::event::Event;
use profluens_core::format::ValueDesc;
use profluens_core::time::Timestamp;

const MS: u64 = 1_000_000;
/// libopus's lookahead, the last packet ends this far past the input so a reader that takes the
/// pre-skip off the stamps ends where the input did.
const LOOKAHEAD: u64 = 312;

fn enc(channels: i64, aligned: bool) -> Harness {
    let e = if aligned { OpusEnc::new().aligned() } else { OpusEnc::new() };
    let mut h = Harness::new(e);
    h.fix_sink_format(OpusEnc::IN, "audio/raw", &fields(channels));
    h
}

fn fields(channels: i64) -> [(&'static str, ValueDesc); 3] {
    [
        ("rate", ValueDesc::Int(48_000)),
        ("channels", ValueDesc::Int(channels)),
        ("sample", ValueDesc::Id("s16")),
    ]
}

fn tone(samples: usize, channels: usize) -> Vec<u8> {
    (0..samples * channels).flat_map(|i| (((i % 96) as i16 - 48) * 200).to_le_bytes()).collect()
}

fn push(h: &mut Harness, pts: Option<u64>, samples: usize, out: &mut Vec<Buffer>) {
    let mut buf = h.alloc(&tone(samples, 2));
    if let Some(ns) = pts {
        buf.pts = Timestamp::from_nanos(ns);
    }
    h.push(buf).expect("push");
    out.extend(h.drain_outputs());
}

/// Stereo chunks of `(pts ns, samples)`, the packets' stamps in ns once EOS drained the tail.
fn run(h: &mut Harness, chunks: &[(u64, usize)]) -> Vec<Buffer> {
    let mut out = Vec::new();
    for &(pts, samples) in chunks {
        push(h, Some(pts), samples, &mut out);
    }
    out.extend(h.eos().expect("eos"));
    out
}

fn stamps(out: &[Buffer]) -> Vec<u64> {
    out.iter().map(|b| b.pts.0).collect()
}

/// AC-3 sized chunks of 1536 samples, 32 ms each, from chunk `from` to `to`.
fn ac3(from: u64, to: u64) -> Vec<(u64, usize)> {
    (from..to).map(|i| (i * 32 * MS, 1536)).collect()
}

#[test]
fn every_packet_is_stamped_and_a_sync_sample() {
    for aligned in [false, true] {
        let mut h = enc(2, aligned);
        let out = run(&mut h, &ac3(0, 10));
        assert!(!out.is_empty());
        for b in &out {
            assert!(b.pts.is_some(), "aligned {aligned}");
            assert_eq!(b.dts, b.pts, "aligned {aligned}");
            assert!(b.duration.0 > 0, "aligned {aligned}");
            assert!(b.flags.contains(BufferFlags::KEYFRAME), "aligned {aligned}");
        }
    }
}

#[test]
fn frames_start_on_the_grid_after_the_first_stamp() {
    let mut h = enc(2, true);
    let out = run(&mut h, &(0..4).map(|i| (1984 * MS + i * 32 * MS, 1536)).collect::<Vec<_>>());
    let got = stamps(&out);
    assert_eq!(got.first(), Some(&(2000 * MS)), "16 ms dropped up to the grid");
    assert!(got.windows(2).all(|w| w[1] - w[0] == 20 * MS));
    // 4 * 1536 less the 768 dropped is 5376 samples, five whole frames and a padded tail
    assert_eq!(got.len(), 6);
    assert!(out[..5].iter().all(|b| b.duration.0 == 20 * MS));
    assert_eq!(out[5].duration.0, (576 + LOOKAHEAD) * 1_000_000_000 / 48_000);
}

#[test]
fn without_aligned_frames_start_at_the_first_stamp() {
    let mut h = enc(2, false);
    let got = stamps(&run(&mut h, &[(1984 * MS, 1920)]));
    assert_eq!(got, [1984 * MS, 2004 * MS, 2024 * MS], "two frames and the lookahead flush");
}

#[test]
fn a_run_from_mid_stream_lands_on_the_full_runs_frames() {
    let all = run(&mut enc(2, true), &ac3(0, 200));
    let late = run(&mut enc(2, true), &ac3(62, 200));
    let (all, late) = (stamps(&all), stamps(&late));
    assert_eq!(all.first(), Some(&0));
    assert_eq!(late.first(), Some(&(2000 * MS)), "1984 ms rounded up to the grid");
    assert_eq!(late[..], all[all.len() - late.len()..], "the same stamps to the end");
}

#[test]
fn a_restart_after_a_flush_lands_on_the_same_grid() {
    let mut h = enc(2, true);
    let mut out = Vec::new();
    for (pts, samples) in ac3(0, 20) {
        push(&mut h, Some(pts), samples, &mut out);
    }
    h.push_event(Event::FlushStart).expect("flush");
    h.push_event(Event::FlushStop).expect("flush");
    out.clear();
    let after = run(&mut h, &ac3(62, 80));
    let fresh = run(&mut enc(2, true), &ac3(62, 80));
    assert_eq!(stamps(&after), stamps(&fresh));
    assert_eq!(after.first().map(|b| b.pts.0), Some(2000 * MS));
}

#[test]
fn a_skip_longer_than_a_chunk_spans_inputs() {
    // 100 samples a chunk from 1981 ms, the 912 samples up to 2000 ms span ten chunks
    let chunk_ns = 100 * 1_000_000_000 / 48_000;
    let chunks: Vec<(u64, usize)> = (0..40).map(|i| (1981 * MS + i * chunk_ns, 100)).collect();
    let got = stamps(&run(&mut enc(2, true), &chunks));
    assert_eq!(got.first(), Some(&(2000 * MS)));
    assert!(got.windows(2).all(|w| w[1] - w[0] == 20 * MS));
    // 4000 less 912 is 3088 samples, three whole frames and a tail
    assert_eq!(got.len(), 4);
}

#[test]
fn a_jump_in_stamps_starts_over_on_the_grid() {
    let mut h = enc(2, true);
    let got = stamps(&run(&mut h, &[(0, 960), (5_007 * MS, 2000)]));
    assert_eq!(got, [0, 5_020 * MS, 5_040 * MS]);
}

#[test]
fn a_jump_pads_out_the_partial_frame_before_it() {
    let mut h = enc(2, true);
    let out = run(&mut h, &[(0, 1000), (5_007 * MS, 2000)]);
    assert_eq!(stamps(&out), [0, 20 * MS, 5_020 * MS, 5_040 * MS]);
    assert_eq!(out[1].duration.0, 40 * 1_000_000_000 / 48_000, "the 40 carried samples");
}

#[test]
fn stamp_jitter_under_the_slip_stays_on_the_count() {
    // every other chunk stamped 3 ms late, under the 5 ms slip
    let chunks: Vec<(u64, usize)> =
        (0..10).map(|i| (i * 32 * MS + (i % 2) * 3 * MS, 1536)).collect();
    let got = stamps(&run(&mut enc(2, true), &chunks));
    assert_eq!(got, (0..17).map(|k| k * 20 * MS).collect::<Vec<_>>(), "and the flush");
}

#[test]
fn a_discont_starts_over_even_on_the_count() {
    let mut h = enc(2, true);
    let mut out = Vec::new();
    push(&mut h, Some(0), 1000, &mut out);
    let mut buf = h.alloc(&tone(960, 2));
    buf.pts = Timestamp::from_nanos(1000 * 1_000_000_000 / 48_000);
    buf.flags = BufferFlags::DISCONT;
    h.push(buf).expect("push");
    out.extend(h.drain_outputs());
    out.extend(h.eos().expect("eos"));
    // the 40 carried samples go out padded, the discont input drops to 40 ms
    assert_eq!(stamps(&out), [0, 20 * MS, 40 * MS]);
}

#[test]
fn unstamped_input_frames_from_zero() {
    let mut h = enc(2, true);
    let mut out = Vec::new();
    push(&mut h, None, 960 * 2, &mut out);
    out.extend(h.eos().expect("eos"));
    assert_eq!(stamps(&out), [0, 20 * MS, 40 * MS], "two frames and the lookahead flush");
}

#[test]
fn input_before_the_format_is_dropped_up_to_the_grid() {
    let mut h = Harness::new(OpusEnc::new().aligned());
    let mut out = Vec::new();
    // 10 ms of samples from 1990 ms, then the rest of three frames
    push(&mut h, Some(1990 * MS), 480, &mut out);
    assert!(out.is_empty());
    h.change_sink_format(OpusEnc::IN, "audio/raw", &fields(2)).expect("format change");
    out.extend(h.drain_outputs());
    push(&mut h, Some(2000 * MS), 960 * 3, &mut out);
    out.extend(h.eos().expect("eos"));
    assert_eq!(stamps(&out), [2000 * MS, 2020 * MS, 2040 * MS, 2060 * MS], "and the flush");
}

#[test]
fn the_tail_is_padded_at_eos() {
    let mut h = enc(2, true);
    let out = run(&mut h, &[(0, 100)]);
    assert_eq!(stamps(&out), [0], "the padding flushes the lookahead");
    assert_eq!(out[0].duration.0, (100 + LOOKAHEAD) * 1_000_000_000 / 48_000);
}
