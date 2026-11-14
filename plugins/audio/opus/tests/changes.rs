//! `opusenc` across a channel count change on its sink and a gap in its input stamps inside a
//! frame.

#![allow(clippy::disallowed_methods)] // test fixtures, not a hot path

use pf_harness::Harness;
use pf_opus::OpusEnc;
use profluens_core::buffer::Buffer;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::time::Timestamp;

const MS: u64 = 1_000_000;
/// libopus's lookahead, the last packet ends this far past the input.
const LOOKAHEAD: u64 = 312;

fn raw(channels: i64) -> [(&'static str, ValueDesc); 3] {
    [
        ("rate", ValueDesc::Int(48_000)),
        ("channels", ValueDesc::Int(channels)),
        ("sample", ValueDesc::Id("s16")),
    ]
}

fn tone(samples: usize, channels: usize) -> Vec<u8> {
    (0..samples * channels).flat_map(|i| (((i % 96) as i16 - 48) * 200).to_le_bytes()).collect()
}

fn push(h: &mut Harness, pcm: &[u8], pts: Timestamp, out: &mut Vec<Buffer>) {
    for (k, c) in pcm.chunks(4096).enumerate() {
        let mut b = h.alloc(c);
        if k == 0 {
            b.pts = pts;
        }
        h.push(b).expect("push");
        out.extend(h.drain_outputs());
    }
}

fn samples(ns: u64) -> u64 {
    (u128::from(ns) * 48_000).div_ceil(1_000_000_000) as u64
}

fn end(out: &[Buffer]) -> u64 {
    out.last().map_or(0, |b| b.pts.0 + b.duration.0)
}

fn said_channels(h: &Harness) -> Option<Value> {
    h.announced()?.get(h.vocabulary().field_id("channels")?)
}

#[test]
fn a_mono_input_after_a_stereo_link_is_encoded_as_mono() {
    let mut h = Harness::new(OpusEnc::new());
    h.fix_sink_format(OpusEnc::IN, "audio/raw", &raw(2));
    h.start().expect("start");
    h.change_sink_format(OpusEnc::IN, "audio/raw", &raw(1)).expect("format change");
    let mut out = Vec::new();
    push(&mut h, &tone(48_000, 1), Timestamp::ZERO, &mut out);
    out.extend(h.eos().expect("eos"));
    assert_eq!(said_channels(&h), Some(Value::Int(1)));
    assert_eq!(samples(end(&out)), 48_000 + LOOKAHEAD, "1 s of mono in, 1 s out, not 500 ms");
}

#[test]
fn a_channel_change_mid_stream_pads_the_frame_begun_and_runs_on() {
    let mut h = Harness::new(OpusEnc::new());
    h.fix_sink_format(OpusEnc::IN, "audio/raw", &raw(2));
    let mut out = Vec::new();
    push(&mut h, &tone(24_100, 2), Timestamp::ZERO, &mut out);
    h.change_sink_format(OpusEnc::IN, "audio/raw", &raw(1)).expect("format change");
    out.extend(h.drain_outputs());
    let at = Timestamp(24_100 * 1_000_000_000 / 48_000);
    assert_eq!(samples(end(&out)), 24_100, "the 100 carried samples went out padded");
    push(&mut h, &tone(24_000, 1), at, &mut out);
    out.extend(h.eos().expect("eos"));
    assert_eq!(said_channels(&h), Some(Value::Int(1)));
    assert_eq!(samples(end(&out)), 48_100 + LOOKAHEAD);
    for w in out.windows(2) {
        assert!(w[1].pts.0.abs_diff(w[0].pts.0 + w[0].duration.0) <= 1, "contiguous stamps");
    }
}

#[test]
fn a_gap_inside_a_frame_is_filled_with_silence() {
    let mut h = Harness::new(OpusEnc::new());
    h.fix_sink_format(OpusEnc::IN, "audio/raw", &raw(1));
    let mut out = Vec::new();
    push(&mut h, &tone(1000, 1), Timestamp::ZERO, &mut out);
    // 40 samples carried, the next input 10 ms after them
    let at = Timestamp((1000 + 480) * 1_000_000_000 / 48_000);
    push(&mut h, &tone(2000, 1), at, &mut out);
    out.extend(h.eos().expect("eos"));
    assert_eq!(samples(end(&out)), 3480 + LOOKAHEAD, "the 480 sample gap kept its place");
    let stamps: Vec<u64> = out.iter().map(|b| b.pts.0 / MS).collect();
    assert_eq!(stamps, [0, 20, 40, 60]);
}

#[test]
fn a_gap_past_the_frame_pads_out_and_starts_at_the_stamp() {
    let mut h = Harness::new(OpusEnc::new());
    h.fix_sink_format(OpusEnc::IN, "audio/raw", &raw(1));
    let mut out = Vec::new();
    push(&mut h, &tone(1000, 1), Timestamp::ZERO, &mut out);
    push(&mut h, &tone(960, 1), Timestamp::from_secs(5), &mut out);
    out.extend(h.eos().expect("eos"));
    let got: Vec<(u64, u64)> = out.iter().map(|b| (b.pts.0 / MS, samples(b.duration.0))).collect();
    assert_eq!(got, [(0, 960), (20, 40), (5_000, 960), (5_020, LOOKAHEAD)]);
}

#[test]
fn jitter_inside_a_frame_stays_on_the_count() {
    let mut h = Harness::new(OpusEnc::new());
    h.fix_sink_format(OpusEnc::IN, "audio/raw", &raw(1));
    let mut out = Vec::new();
    push(&mut h, &tone(1000, 1), Timestamp::ZERO, &mut out);
    let at = Timestamp(1000 * 1_000_000_000 / 48_000 + 2 * MS);
    push(&mut h, &tone(920, 1), at, &mut out);
    out.extend(h.eos().expect("eos"));
    let stamps: Vec<u64> = out.iter().map(|b| b.pts.0 / MS).collect();
    assert_eq!(stamps, [0, 20, 40], "2 ms is under the slip");
}
