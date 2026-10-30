//! `libavaacenc` end to end, decoded again by `libavaacdec`.

// Test code.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use pf_libav::{LibavAacEnc, LibavAudioDec};
use profluens_core::buffer::Buffer;
use profluens_core::event::Event;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::time::Timestamp;

const SLOT: usize = 64 * 1024;
const RATE: i64 = 48_000;

fn ns(sample: i64) -> u64 {
    (sample * 1_000_000_000 / RATE) as u64
}

fn encoder(e: LibavAacEnc) -> Harness {
    let mut h = Harness::with_slot_size(e, SLOT);
    h.fix_sink_format(
        LibavAacEnc::IN,
        "audio/raw",
        &[
            ("rate", ValueDesc::Int(RATE)),
            ("channels", ValueDesc::Int(2)),
            ("sample", ValueDesc::Id("s16")),
        ],
    );
    h
}

fn tone(from: i64, samples: usize) -> Vec<u8> {
    (0..samples)
        .flat_map(|i| {
            let t = (from + i as i64) as f32 / RATE as f32;
            let v = (0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin() * 32767.0) as i16;
            let b = v.to_le_bytes();
            [b[0], b[1], b[0], b[1]]
        })
        .collect()
}

fn feed(h: &mut Harness, from: i64, samples: usize, chunk: usize, out: &mut Vec<Buffer>) {
    feed_with(h, from, samples, chunk, true, out);
}

/// `stamped` false leaves every buffer at `Timestamp::NONE`.
fn feed_with(
    h: &mut Harness,
    from: i64,
    samples: usize,
    chunk: usize,
    stamped: bool,
    out: &mut Vec<Buffer>,
) {
    let mut at = 0;
    while at < samples {
        let n = chunk.min(samples - at);
        let mut b = h.alloc(&tone(from + at as i64, n));
        if stamped {
            b.pts = Timestamp(ns(from + at as i64));
        }
        h.push(b).expect("push");
        out.extend(h.drain_outputs());
        at += n;
    }
}

fn stamps(out: &[Buffer]) -> Vec<u64> {
    out.iter().map(|b| b.pts.0).collect()
}

#[test]
fn a_tone_comes_back_through_libavaacdec() {
    tone_round_trip(LibavAacEnc::new());
}

#[test]
fn the_fast_search_brings_the_tone_back_too() {
    tone_round_trip(LibavAacEnc::new().fast());
}

fn tone_round_trip(e: LibavAacEnc) {
    let mut h = encoder(e);
    let mut out = Vec::new();
    feed(&mut h, 0, 2 * RATE as usize, 1000, &mut out);
    out.extend(h.eos().expect("eos"));
    let f = h.announced().expect("announced");
    assert_eq!(f.get(h.vocabulary().field_id("channels").expect("field")), Some(Value::Int(2)));

    let asc = LibavAacEnc::config_for(48_000, 2).expect("config");
    assert_eq!(out[0].memory.data(), asc.as_slice(), "the config first");
    let packets = &out[1..];
    // the priming packet would present before zero and is left out
    let want: Vec<u64> = (0..packets.len() as i64).map(|k| ns(k * 1024)).collect();
    assert_eq!(stamps(packets), want);
    assert_eq!(out[0].pts.0, 0);
    assert_eq!(packets.len(), 94);

    let mut d = Harness::with_slot_size(LibavAudioDec::aac(), SLOT);
    let mut pcm = Vec::new();
    for b in &out {
        let mut i = d.alloc(b.memory.data());
        i.pts = b.pts;
        d.push(i).expect("push");
        for o in d.drain_outputs() {
            pcm.extend_from_slice(o.memory.data());
        }
    }
    for o in d.eos().expect("eos") {
        pcm.extend_from_slice(o.memory.data());
    }
    let left: Vec<f32> = pcm
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
        .collect();
    assert!(left.len() >= 2 * RATE as usize, "{} samples back", left.len());
    let mid = &left[2048..2 * RATE as usize - 2048];
    let rms = (mid.iter().map(|v| v * v).sum::<f32>() / mid.len() as f32).sqrt();
    assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.03, "rms {rms}");
    let crossings = mid.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    let per_s = crossings as f32 * RATE as f32 / mid.len() as f32;
    assert!((per_s - 2000.0).abs() < 20.0, "{per_s} crossings a second");
}

#[test]
fn aligned_runs_from_different_starts_share_their_stamps() {
    let run = |from: i64| {
        let mut h = encoder(LibavAacEnc::new().aligned());
        let mut out = Vec::new();
        feed(&mut h, from, 3 * RATE as usize - from as usize, 1536, &mut out);
        out.extend(h.eos().expect("eos"));
        stamps(&out[1..])
    };
    let all = run(0);
    let late = run(48_336);
    assert_eq!(late[0], ns(48_128), "the priming packet a frame ahead of the grid point");
    assert_eq!(late[1], ns(49_152));
    let shared = late.iter().filter(|s| all.contains(s)).count();
    // the short last frame and the tail land off the grid
    assert!(shared >= late.len() - 2, "{shared} of {} stamps shared", late.len());
}

#[test]
fn a_gap_starts_a_new_encode_with_the_stamps_in_order() {
    let mut h = encoder(LibavAacEnc::new());
    let mut out = Vec::new();
    feed(&mut h, 0, RATE as usize / 2, 1000, &mut out);
    feed(&mut h, 2 * RATE, RATE as usize / 2, 1000, &mut out);
    out.extend(h.eos().expect("eos"));
    let packets = &out[1..];
    assert!(packets.windows(2).all(|w| w[1].pts.0 >= w[0].pts.0 + w[0].duration.0), "no overlap");
    assert!(stamps(packets).contains(&ns(2 * RATE)), "the new encode starts on the new stamp");
    assert_eq!(
        packets.iter().filter(|b| b.memory.data() == out[0].memory.data()).count(),
        0,
        "one config"
    );
}

#[test]
fn a_flush_starts_over_with_the_config() {
    let mut h = encoder(LibavAacEnc::new());
    let mut out = Vec::new();
    feed(&mut h, RATE, RATE as usize / 2, 1000, &mut out);
    h.push_event(Event::FlushStart).expect("flush");
    h.push_event(Event::FlushStop).expect("flush");
    let mut after = Vec::new();
    feed(&mut h, 10 * RATE, RATE as usize / 2, 1000, &mut after);
    after.extend(h.eos().expect("eos"));
    let asc = LibavAacEnc::config_for(48_000, 2).expect("config");
    assert_eq!(after[0].memory.data(), asc.as_slice());
    assert_eq!(after[1].pts.0, ns(10 * RATE - 1024));
    assert_eq!(after[2].pts.0, ns(10 * RATE));
}

#[test]
fn unstamped_pcm_comes_out_whole_stamped_from_zero() {
    let total = 5 * RATE as usize;
    let mut h = encoder(LibavAacEnc::new());
    let mut out = Vec::new();
    feed_with(&mut h, 0, total, 1000, false, &mut out);
    out.extend(h.eos().expect("eos"));
    let asc = LibavAacEnc::config_for(48_000, 2).expect("config");
    assert_eq!(out[0].memory.data(), asc.as_slice(), "the config first");
    let packets = &out[1..];
    // one packet per started 1024 sample frame, priming and padding in, the priming one left out
    assert_eq!(packets.len(), (total + 1024).div_ceil(1024) - 1);
    let want: Vec<u64> = (0..packets.len() as i64).map(|k| ns(k * 1024)).collect();
    assert_eq!(stamps(packets), want, "contiguous from zero");
    // no PCM lost, the packets cover every input sample and at most one frame of padding
    let last = packets.last().expect("packets");
    let end = (last.pts.0 + last.duration.0) as i64 * RATE / 1_000_000_000;
    assert!(end >= total as i64 && end < total as i64 + 1024, "covers {end} of {total}");
}

#[test]
fn unstamped_and_stamped_from_zero_put_out_the_same_stream() {
    let run = |stamped: bool| {
        let mut h = encoder(LibavAacEnc::new());
        let mut out = Vec::new();
        feed_with(&mut h, 0, 3 * RATE as usize, 1536, stamped, &mut out);
        out.extend(h.eos().expect("eos"));
        out.iter().map(|b| (b.pts.0, b.duration.0, b.memory.data().to_vec())).collect::<Vec<_>>()
    };
    let stamped = run(true);
    assert_eq!(stamped.len(), (3 * RATE as usize + 1024).div_ceil(1024));
    assert_eq!(run(false), stamped);
}
