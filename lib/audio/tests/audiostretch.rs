// Test setup, signals, staging and the recording sink allocate freely.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_audio::format::{FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
use profluens_audio::{AudioErrorKind, AudioStretch, SampleFormat, position_at, time_at};
use profluens_core::element::Flow;
use profluens_core::event::Event;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::{Rate, Timestamp};

fn r(num: i64, den: i64) -> Rate {
    Rate::new(num, den).unwrap()
}

fn gen_input(rate_hz: u32, channels: usize, frames: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * channels);
    let mut seed: u32 = 0x1234_5678;
    let period = (rate_hz / 120) as usize;
    let eperiod = (rate_hz * 7 / 10) as usize;
    for n in 0..frames {
        let phase = (n % period) as f32 / period as f32;
        let tri = 1.0 - (2.0 * phase - 1.0).abs();
        let voiced = tri * tri * 2.0 - 0.5;
        let ephase = (n % eperiod) as f32 / eperiod as f32;
        let env = 1.0 - (2.0 * ephase - 1.0).abs();
        for c in 0..channels {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 9) as f32 / 4_194_304.0 - 1.0;
            out.push(env * voiced * 0.6 + 0.05 * noise - 0.02 * c as f32);
        }
    }
    out
}

fn fnv1a64(samples: &[f32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for s in samples {
        for b in s.to_bits().to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

fn f32_bytes(input: &[f32]) -> Vec<u8> {
    input.iter().flat_map(|s| s.to_ne_bytes()).collect()
}

fn take_f32(out: &mut Vec<f32>, bytes: &[u8]) {
    for w in bytes.as_chunks::<4>().0 {
        out.push(f32::from_ne_bytes(*w));
    }
}

fn rig(rate_hz: u32, channels: u16, rate: Rate) -> Harness {
    let mut h = Harness::with_slot_size(AudioStretch::new(rate), 1 << 16);
    h.fix_sink_format(
        AudioStretch::IN,
        "audio/raw",
        &[
            (FIELD_RATE, ValueDesc::Int(rate_hz as i64)),
            (FIELD_CHANNELS, ValueDesc::Int(channels as i64)),
            (FIELD_SAMPLE, ValueDesc::Id(SampleFormat::F32.caps_name())),
        ],
    );
    h.start().expect("start");
    h
}

fn stamped(
    h: &mut Harness,
    bytes: &[u8],
    chunk_bytes: usize,
    (rate_hz, channels, first): (u32, u16, u64),
) -> Vec<profluens_core::buffer::Buffer> {
    let (stride, step) = (4 * channels as usize, chunk_bytes.max(1));
    let at = |byte: usize| time_at(first + (byte / stride) as u64, rate_hz);
    let chunks = bytes.chunks(step).enumerate();
    chunks
        .map(|(k, chunk)| {
            let mut buf = h.alloc(chunk);
            buf.pts = at(k * step);
            buf.duration = at(k * step + chunk.len()).saturating_sub(buf.pts);
            buf
        })
        .collect()
}

fn run(
    rate_hz: u32,
    channels: u16,
    rate: Rate,
    input: &[f32],
    chunk_bytes: usize,
) -> (Vec<f32>, Vec<(Timestamp, Timestamp)>) {
    let mut h = rig(rate_hz, channels, rate);
    let bytes = f32_bytes(input);
    let mut out = Vec::new();
    let mut stamps = Vec::new();
    for buf in stamped(&mut h, &bytes, chunk_bytes, (rate_hz, channels, 0)) {
        h.push(buf).expect("push");
        while let Some(b) = h.pull(AudioStretch::OUT) {
            stamps.push((b.pts, b.duration));
            take_f32(&mut out, b.memory.data());
        }
    }
    for b in h.eos().expect("eos") {
        stamps.push((b.pts, b.duration));
        take_f32(&mut out, b.memory.data());
    }
    (out, stamps)
}

#[allow(clippy::type_complexity)]
const GOLDEN: [(u32, u16, u64, [((i64, i64), usize, u64); 4]); 3] = [
    (
        44_100,
        2,
        0x0b47_5d41_0cfb_b3e8,
        [
            ((3, 4), 351_754, 0xb12c_d620_7fa2_1f53),
            ((5, 4), 211_978, 0xfd88_7187_523d_7277),
            ((3, 2), 177_446, 0xcddf_4e2b_e517_7950),
            ((2, 1), 133_902, 0xea0b_895a_9df7_d0f6),
        ],
    ),
    (
        48_000,
        2,
        0x6770_b4dd_90ae_010d,
        [
            ((3, 4), 383_214, 0x300b_0a3a_f89d_f1d7),
            ((5, 4), 230_518, 0x1474_deed_d0b7_5429),
            ((3, 2), 192_786, 0x09d8_c6f6_8d55_3cb5),
            ((2, 1), 145_630, 0xdceb_87e6_13d9_0088),
        ],
    ),
    (
        48_000,
        1,
        0x31ef_8128_0305_995d,
        [
            ((3, 4), 191_362, 0x5841_d4d5_8218_8918),
            ((5, 4), 115_446, 0x3330_ebf3_d483_c16b),
            ((3, 2), 96_638, 0x4c42_d1db_d78b_4e6c),
            ((2, 1), 73_011, 0xc6f7_f939_329c_4819),
        ],
    ),
];

#[test]
fn golden_vectors_match_the_original_bit_exactly() {
    for &(rate_hz, channels, in_digest, rows) in &GOLDEN {
        let frames = rate_hz as usize * 3; // 3 seconds
        let input = gen_input(rate_hz, channels as usize, frames);
        assert_eq!(
            fnv1a64(&input),
            in_digest,
            "{rate_hz} Hz {channels} ch: the test signal itself changed, the goldens below \
             describe a different input and must be regenerated (see the module docs)"
        );
        for &((num, den), golden_len, golden_digest) in &rows {
            let rate = format!("{num}/{den}");
            let (out, _) =
                run(rate_hz, channels, r(num, den), &input, 4096 * channels as usize * 4);
            assert_eq!(
                out.len(),
                golden_len,
                "{rate_hz} Hz {channels} ch rate={rate}: output length differs from the \
                 original's ({} vs {golden_len})",
                out.len()
            );
            assert_eq!(
                fnv1a64(&out),
                golden_digest,
                "{rate_hz} Hz {channels} ch rate={rate}: same length but different samples, \
                 the port is no longer bit-exact against mskp-stretch"
            );
        }
    }
}

#[test]
fn ragged_buffers_are_transparent() {
    let (rate_hz, channels) = (48_000u32, 2u16);
    let input = gen_input(rate_hz, channels as usize, rate_hz as usize * 3);
    for &((num, den), golden_len, golden_digest) in &GOLDEN[1].3 {
        let rate = format!("{num}/{den}");
        let (out, _) = run(rate_hz, channels, r(num, den), &input, 997);
        assert_eq!(out.len(), golden_len, "rate={rate}: ragged buffering changed the length");
        assert_eq!(
            fnv1a64(&out),
            golden_digest,
            "rate={rate}: ragged buffering changed the samples, the partial-frame carry \
             desynced the channel lanes"
        );
    }
}

#[test]
fn rate_one_is_bit_exact_passthrough() {
    for &(rate_hz, channels) in &[(44_100u32, 2u16), (48_000, 1)] {
        let input = gen_input(rate_hz, channels as usize, rate_hz as usize);
        let (out, stamps) = run(rate_hz, channels, Rate::ONE, &input, 4096 * channels as usize * 4);
        let grid = (0..stamps.len()).map(|k| time_at(k as u64 * 4096, rate_hz));
        assert!(stamps.iter().map(|(pts, _)| *pts).eq(grid), "bypass moved a stamp");
        assert_eq!(out.len(), input.len(), "bypass must not change the sample count");
        for (i, (a, b)) in input.iter().zip(&out).enumerate() {
            assert_eq!(a.to_bits(), b.to_bits(), "bypass altered sample {i}");
        }
    }
}

#[test]
fn a_rate_near_one_is_played_and_only_one_itself_is_bypass() {
    let input = gen_input(48_000, 2, 48_000 * 4);
    let (out, stamps) = run(48_000, 2, r(101, 100), &input, 1 << 15);
    let frames = out.len() as f64 / 2.0;
    let expected = 4.0 * 48_000.0 / 1.01;
    assert!((frames - expected).abs() < 800.0, "{frames} frames out, {expected} at 1.01");
    let (pts, dur) = *stamps.last().unwrap();
    assert_eq!(pts.saturating_add(dur), Timestamp::from_secs(4), "all of the stream is covered");
}

#[test]
fn every_stamp_is_stream_time_and_a_buffer_covers_the_rate_times_what_it_plays_for() {
    for (num, den) in [(3, 2), (3, 4), (5, 2), (2, 5)] {
        let input = gen_input(48_000, 2, 48_000 * 2);
        let (out, stamps) = run(48_000, 2, r(num, den), &input, 1 << 15);
        assert!(stamps.len() > 2, "expected several output buffers");
        assert_eq!(stamps[0].0, Timestamp::ZERO, "the first source sample");
        for w in stamps.windows(2) {
            let ((pts, dur), (next, _)) = (w[0], w[1]);
            assert!(pts.is_some() && dur.is_some(), "audiostretch must stamp pts and duration");
            assert_eq!(pts.saturating_add(dur), next, "gap or overlap in stream time at {pts:?}");
        }
        let (last_pts, last_dur) = *stamps.last().unwrap();
        assert_eq!(last_pts.saturating_add(last_dur), Timestamp::from_secs(2));
        let secs = (out.len() / 2) as f64 / 48_000.0;
        let want = 2.0 * den as f64 / num as f64;
        assert!((secs - want).abs() < 0.08, "{num}/{den}: plays for {secs} s, expected {want} s");
    }
}

#[test]
fn the_place_is_taken_from_the_first_input_buffer() {
    let input = gen_input(48_000, 1, 48_000);
    let mut h = rig(48_000, 1, r(3, 2));
    let bytes = f32_bytes(&input);
    let mut stamps = Vec::new();
    for buf in stamped(&mut h, &bytes, 1 << 15, (48_000, 1, 480_000)) {
        h.push(buf).expect("push");
        while let Some(b) = h.pull(AudioStretch::OUT) {
            stamps.push((b.pts, b.duration));
        }
    }
    assert_eq!(stamps[0].0, Timestamp::from_secs(10), "the first output is where the input is");
    assert!(stamps.windows(2).all(|w| w[0].0.saturating_add(w[0].1) == w[1].0));
    let (pts, dur) = *stamps.last().unwrap();
    let end = pts.saturating_add(dur);
    assert!(end > Timestamp::from_millis(10_900) && end <= Timestamp::from_secs(11), "{end:?}");
}

#[test]
fn input_with_no_stamp_gives_output_with_none() {
    let input = gen_input(48_000, 1, 48_000);
    let mut h = rig(48_000, 1, r(3, 2));
    let mut seen = 0;
    for chunk in f32_bytes(&input).chunks(1 << 15) {
        let buf = h.alloc(chunk);
        h.push(buf).expect("push");
        while let Some(b) = h.pull(AudioStretch::OUT) {
            assert!(b.pts.is_none() && b.duration.is_none());
            seen += 1;
        }
    }
    assert!(seen > 2);
}

fn triangle(rate_hz: u32, frames: usize, hz: usize) -> Vec<f32> {
    let period = rate_hz as usize / hz;
    (0..frames)
        .map(|n| {
            let phase = (n % period) as f32 / period as f32;
            0.8 * (2.0 * (1.0 - (2.0 * phase - 1.0).abs()) - 1.0)
        })
        .collect()
}

#[test]
fn a_rate_asked_for_mid_stream_keeps_the_stamps_and_the_waveform_continuous() {
    let rate_hz = 48_000u32;
    let input = triangle(rate_hz, rate_hz as usize * 2, 100);
    let mut h = rig(rate_hz, 1, Rate::ONE);
    let bytes = f32_bytes(&input);
    let half = bytes.len() / 2 / 4 * 4;

    let mut out = Vec::new();
    let mut stamps = Vec::new();
    let drain = |h: &mut Harness, out: &mut Vec<f32>, stamps: &mut Vec<_>| {
        while let Some(b) = h.pull(AudioStretch::OUT) {
            stamps.push((b.pts, b.duration));
            take_f32(out, b.memory.data());
        }
    };

    for buf in stamped(&mut h, &bytes[..half], 1 << 15, (rate_hz, 1, 0)) {
        h.push(buf).expect("push");
        drain(&mut h, &mut out, &mut stamps);
    }
    let before = out.len();

    h.push_event(Event::RateAsked { rate: r(7, 4) }).expect("rate event");
    h.push_event(Event::RateAsked { rate: r(5, 2) }).expect("rate event");

    for buf in stamped(&mut h, &bytes[half..], 1 << 15, (rate_hz, 1, half as u64 / 4)) {
        h.push(buf).expect("push");
        drain(&mut h, &mut out, &mut stamps);
    }
    for b in h.eos().expect("eos") {
        stamps.push((b.pts, b.duration));
        take_f32(&mut out, b.memory.data());
    }

    let secs = out.len() as f64 / rate_hz as f64;
    assert!((secs - 1.4).abs() < 0.05, "playback duration {secs}s, expected ~1.4s");
    assert!(before > 0 && out.len() > before, "output on both sides of the change");

    for w in stamps.windows(2) {
        assert_eq!(w[0].0.saturating_add(w[0].1), w[1].0, "the rate change tore the stamps");
    }
    let (pts, dur) = *stamps.last().unwrap();
    assert_eq!(pts.saturating_add(dur), Timestamp::from_secs(2));

    let slope = 0.8 * 4.0 / (rate_hz as f32 / 100.0);
    let (worst, at) = out
        .windows(2)
        .enumerate()
        .map(|(i, w)| ((w[1] - w[0]).abs(), i))
        .fold((0.0f32, 0), |a, b| if b.0 > a.0 { b } else { a });
    assert!(
        worst < 10.0 * slope,
        "discontinuity of {worst} at sample {at} (natural slope {slope}), the splice clicked"
    );
}

#[test]
fn a_rate_it_does_not_play_is_a_warning_and_changes_nothing() {
    let input = gen_input(48_000, 1, 48_000);
    let golden = run(48_000, 1, r(3, 2), &input, 1 << 15).0;
    let mut h = rig(48_000, 1, r(3, 2));
    for (num, den) in [(8, 1), (1, 8)] {
        h.push_event(Event::RateAsked { rate: r(num, den) }).expect("a refusal is no failure");
    }
    let refused: Vec<String> = h
        .bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            profluens_core::bus::BusMessage::Warning { error, .. } => Some(error.to_string()),
            _ => None,
        })
        .collect();
    let named = |num, den| AudioErrorKind::UnsupportedRate { num, den }.to_string();
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert!(refused[0].contains(&named(8, 1)) && refused[1].contains(&named(1, 8)), "{refused:?}");
    let mut out = Vec::new();
    let bytes = f32_bytes(&input);
    for buf in stamped(&mut h, &bytes, 1 << 15, (48_000, 1, 0)) {
        h.push(buf).expect("push");
        while let Some(b) = h.pull(AudioStretch::OUT) {
            take_f32(&mut out, b.memory.data());
        }
    }
    for b in h.eos().expect("eos") {
        take_f32(&mut out, b.memory.data());
    }
    assert!(out == golden, "the refused rates changed what plays");
    assert_eq!(AudioStretch::new(r(10, 1)).rate(), Rate::ONE);
    assert_eq!(AudioStretch::new(r(1, 10)).rate(), Rate::ONE);
}

#[test]
fn flush_drops_every_trace_of_pre_seek_audio() {
    let rate_hz = 48_000u32;
    let mut h = rig(rate_hz, 1, r(2, 1));

    let pre = vec![1.0f32; rate_hz as usize / 2];
    let buf = h.alloc(&f32_bytes(&pre));
    h.push(buf).expect("push");
    while h.pull(AudioStretch::OUT).is_some() {}

    h.push_event(Event::FlushStart).expect("flush");

    let post = vec![-1.0f32; rate_hz as usize / 2];
    let mut out = Vec::new();
    let buf = h.alloc(&f32_bytes(&post));
    h.push(buf).expect("push");
    while let Some(b) = h.pull(AudioStretch::OUT) {
        take_f32(&mut out, b.memory.data());
    }
    for b in h.eos().expect("eos") {
        take_f32(&mut out, b.memory.data());
    }
    assert!(!out.is_empty(), "expected output after the flush");
    for (i, s) in out.iter().enumerate() {
        assert!(*s < -0.9, "sample {i} = {s}: pre-flush audio blended into post-flush output");
    }
    assert_eq!(run(rate_hz, 1, r(2, 1), &post, 1 << 15).1[0].0, Timestamp::ZERO);
}

#[test]
fn behind_a_seek_and_behind_a_skip_the_output_is_stamped_where_its_input_is() {
    let rate_hz = 48_000u32;
    for (num, den) in [(3, 2), (3, 4), (5, 2), (1, 1)] {
        let mut h = rig(rate_hz, 1, r(num, den));
        let input = gen_input(rate_hz, 1, 30_000);
        let bytes = f32_bytes(&input);
        let feed = |h: &mut Harness, first: u64| {
            let (mut stamps, mut out) = (Vec::new(), Vec::new());
            for buf in stamped(h, &bytes, 1 << 14, (rate_hz, 1, first)) {
                h.push(buf).expect("push");
                while let Some(b) = h.pull(AudioStretch::OUT) {
                    stamps.push((b.pts, b.duration));
                    take_f32(&mut out, b.memory.data());
                }
            }
            (stamps, out)
        };
        feed(&mut h, 0);
        let target = Timestamp(2_912_708_333 + 7_000);
        h.seek(0, target).expect("seek");
        let first = position_at(target, rate_hz);
        assert_eq!(first, 139_811);
        let (stamps, out) = feed(&mut h, first);
        assert_eq!(
            stamps[0].0,
            time_at(first, rate_hz),
            "{num}/{den}: starts where the input does"
        );
        assert_eq!(out[0], input[0], "{num}/{den}: on the input's first sample");
        assert!(stamps.windows(2).all(|w| w[0].0.saturating_add(w[0].1) == w[1].0));

        let point = Timestamp::from_secs(5);
        h.skip_to(point).expect("skip");
        let (after, out) = feed(&mut h, position_at(point, rate_hz));
        let at = after.iter().position(|(pts, _)| *pts >= point).expect("output at the point");
        assert_eq!(after[at].0, point, "{num}/{den}: the first buffer at the point is on it");
        assert!(after[..at].iter().all(|(pts, dur)| pts.saturating_add(*dur) <= point));
        assert!(after[at..].windows(2).all(|w| w[0].0.saturating_add(w[0].1) == w[1].0));
        assert!(!out.is_empty());
    }
}

fn window(rate_hz: u32) -> usize {
    3 * (rate_hz as usize / 65)
}

#[test]
fn eos_drains_the_final_window() {
    let rate_hz = 48_000u32;
    let frames = 20_000usize;
    let mut h = rig(rate_hz, 1, r(3, 2));
    let input = gen_input(rate_hz, 1, frames);
    let (mut before, mut end) = (0usize, Timestamp::ZERO);
    let bytes = f32_bytes(&input);
    for buf in stamped(&mut h, &bytes, 1 << 15, (rate_hz, 1, 0)) {
        h.push(buf).expect("push");
        while let Some(b) = h.pull(AudioStretch::OUT) {
            before += b.memory.data().len() / 4;
            end = b.pts.saturating_add(b.duration);
        }
    }
    let all = time_at(frames as u64, rate_hz);
    assert!(end < all, "mid-stream the element holds a search window back");

    let mut after = before;
    for b in h.eos().expect("eos") {
        after += b.memory.data().len() / 4;
        end = b.pts.saturating_add(b.duration);
    }
    assert!(after > before, "EOS must flush the held-back search window ({before} -> {after})");
    assert_eq!(end, all, "after the drain every input frame is accounted for");

    let w = window(rate_hz);
    let lo = (frames - w) as f64 / 1.5;
    assert!(
        (lo..=lo + w as f64 + 2.0).contains(&(after as f64)),
        "{after} frames out of {frames} at 1.5x: outside [{lo}, {}] (stretched body + \
         un-stretched tail of at most {w} frames)",
        lo + w as f64
    );
}

#[test]
fn non_finite_samples_do_not_wedge_the_state_machine() {
    let rate_hz = 48_000u32;
    let clean = gen_input(rate_hz, 2, 20_000);
    let mut poisoned = gen_input(rate_hz, 2, 20_000);
    for (i, s) in poisoned.iter_mut().enumerate() {
        *s = match i % 4 {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            _ => *s,
        };
    }

    let mut h = rig(rate_hz, 2, r(7, 4));
    let mut frames = 0usize;
    for part in [&poisoned, &clean] {
        for chunk in f32_bytes(part).chunks(1 << 15) {
            let buf = h.alloc(chunk);
            h.push(buf).expect("hostile input must not error");
            while let Some(b) = h.pull(AudioStretch::OUT) {
                frames += b.memory.data().len() / 8;
            }
        }
    }
    for b in h.eos().expect("eos") {
        frames += b.memory.data().len() / 8;
    }
    let ratio = frames as f64 / (40_000.0 / 1.75);
    assert!(
        (0.95..=1.05).contains(&ratio),
        "NaN/inf derailed the splice schedule: {frames} frames out of 40000 in at 1.75x"
    );
}

#[test]
fn absurd_buffer_shapes_are_survivable() {
    let mut h = rig(48_000, 2, r(3, 2));
    for len in [0usize, 1, 3, 7] {
        let buf = h.alloc(&vec![0u8; len]);
        assert_eq!(h.push(buf).expect("push"), Flow::Ok);
    }
    let input = gen_input(48_000, 2, 10_000);
    for chunk in f32_bytes(&input).chunks(1 << 15) {
        let buf = h.alloc(chunk);
        h.push(buf).expect("push");
        while h.pull(AudioStretch::OUT).is_some() {}
    }
    h.eos().expect("eos");
}

#[derive(Clone, Copy, Debug)]
enum Act {
    Rate(i64, i64),
    Seek(u64),
    Skip(u64),
    Pause,
    Resume,
}

mod raw_src {
    use std::sync::{Arc, Mutex};

    use super::Act;
    use profluens_audio::{FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE, position_at, time_at};
    use profluens_core::batch::Inputs;
    use profluens_core::command::{Command, CommandError, Control};
    use profluens_core::ctx::Ctx;
    use profluens_core::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use profluens_core::error::Error;
    use profluens_core::event::Event;
    use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
    use profluens_core::id::SrcPad;
    use profluens_core::time::{Rate, Timestamp};

    pub const RATE: i64 = 48_000;
    pub const CHANNELS: i64 = 2;
    const STRIDE: usize = 8;

    static FIELDS: [FieldDesc; 3] = [
        FieldDesc {
            field: FIELD_RATE,
            allowed: ConstraintDesc::Eq(ValueDesc::Int(RATE)),
            preferred: None,
        },
        FieldDesc {
            field: FIELD_CHANNELS,
            allowed: ConstraintDesc::Eq(ValueDesc::Int(CHANNELS)),
            preferred: None,
        },
        FieldDesc {
            field: FIELD_SAMPLE,
            allowed: ConstraintDesc::Eq(ValueDesc::Id("f32")),
            preferred: None,
        },
    ];
    static OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &FIELDS }];
    static PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
    static DESC: ElementDesc = ElementDesc {
        name: "rawaudiosrc",
        sinks: &[],
        srcs: &PADS,
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::None,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    };

    /// What the log answered to each rate the script asked for.
    pub type Answers = Arc<Mutex<Vec<Result<u64, CommandError>>>>;

    pub struct RawAudioSrc {
        pcm: Vec<u8>,
        off: usize,
        sent: usize,
        script: Vec<(usize, Act)>,
        done: usize,
        handles: Option<(Arc<Control>, Arc<Control>)>,
        answers: Answers,
    }
    impl RawAudioSrc {
        pub const OUT: SrcPad = SrcPad(0);

        pub fn new(pcm: Vec<u8>) -> Self {
            let (script, answers) = (Vec::new(), Answers::default());
            Self { pcm, off: 0, sent: 0, script, done: 0, handles: None, answers }
        }

        pub fn scripted(
            mut self,
            script: &[(usize, Act)],
            seek: Arc<Control>,
            pause: Arc<Control>,
        ) -> Self {
            (self.script, self.handles) = (script.to_vec(), Some((seek, pause)));
            self
        }

        pub fn answers(&self) -> Answers {
            Arc::clone(&self.answers)
        }

        fn frame_at(ms: u64) -> usize {
            position_at(Timestamp::from_millis(ms), RATE as u32) as usize
        }

        fn act(&mut self, act: Act) {
            let Some((seek, pause)) = &self.handles else {
                return;
            };
            match act {
                Act::Rate(num, den) => {
                    let answer =
                        seek.append(Command::SetRate { rate: Rate::new(num, den).unwrap() });
                    self.answers.lock().unwrap().push(answer);
                }
                Act::Seek(ms) => {
                    let _ = seek.seek_bytes(
                        Timestamp::from_millis(ms),
                        &[(profluens_core::command::ALL, 0, Timestamp::from_millis(ms))],
                    );
                }
                Act::Skip(ms) => seek.skip_to(Timestamp::from_millis(ms)),
                Act::Pause => pause.pause(),
                Act::Resume => pause.resume(),
            }
        }
    }
    impl Element for RawAudioSrc {
        fn desc(&self) -> &'static ElementDesc {
            &DESC
        }
        fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
            (self.off, self.sent, self.done) = (0, 0, 0);
            Ok(())
        }
        fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
            let due =
                self.script[self.done..].iter().take_while(|(at, _)| *at == self.sent).count();
            if due > 0 {
                for k in self.done..self.done + due {
                    self.act(self.script[k].1);
                }
                self.done += due;
                return Ok(Flow::Ok);
            }
            if self.off >= self.pcm.len() {
                return Ok(Flow::Eos);
            }
            let Some(mut buf) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            let cap = buf.memory.capacity() / STRIDE * STRIDE;
            let n = cap.min(self.pcm.len() - self.off);
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.pcm[self.off..self.off + n]);
            buf.memory.set_len(n);
            let frame = (self.off / STRIDE) as u64;
            buf.pts = time_at(frame, RATE as u32);
            buf.duration =
                time_at(frame + (n / STRIDE) as u64, RATE as u32).saturating_sub(buf.pts);
            (self.off, self.sent) = (self.off + n, self.sent + 1);
            ctx.out(Self::OUT).push(buf);
            Ok(Flow::Ok)
        }
        fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
            match event {
                Event::FlushStart => {
                    let to = ctx.presentation_point().map_or(0, |t| t.0 / 1_000_000);
                    self.off = (Self::frame_at(to) * STRIDE).min(self.pcm.len());
                }
                Event::SkippedTo { to_time } => {
                    let at = Self::frame_at(to_time.0 / 1_000_000) * STRIDE;
                    self.off = self.off.max(at).min(self.pcm.len());
                }
                _ => {}
            }
            Ok(())
        }
    }
}

mod record_sink {
    use std::sync::{Arc, Mutex};

    use profluens_core::batch::Inputs;
    use profluens_core::ctx::Ctx;
    use profluens_core::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use profluens_core::error::Error;
    use profluens_core::event::Event;
    use profluens_core::format::OfferDesc;
    use profluens_core::id::SinkPad;
    use profluens_core::time::Timestamp;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Row {
        Buffer {
            pts: Timestamp,
            duration: Timestamp,
            bytes: usize,
            running: Timestamp,
            before: bool,
        },
        Flush,
        Skip {
            to: Timestamp,
            running: Timestamp,
        },
    }

    pub type Log = Arc<Mutex<Vec<Row>>>;

    static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
    static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
    static DESC: ElementDesc = ElementDesc {
        name: "recordsink",
        sinks: &PADS,
        srcs: &[],
        props: &[],
        sched: SchedHint::Passive,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    };

    pub struct RecordSink(Log);
    impl RecordSink {
        pub const IN: SinkPad = SinkPad(0);

        pub fn new() -> (Self, Log) {
            let log: Log = Arc::new(Mutex::new(Vec::new()));
            (Self(Arc::clone(&log)), log)
        }
    }
    impl Element for RecordSink {
        fn desc(&self) -> &'static ElementDesc {
            &DESC
        }
        fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
            while let Some(b) = inputs.pop() {
                let (pts, duration, bytes) = (b.pts, b.duration, b.memory.data().len());
                let running = ctx.running_time(pts);
                let before = ctx.before_presentation(pts, duration);
                self.0.lock().unwrap().push(Row::Buffer { pts, duration, bytes, running, before });
            }
            Ok(Flow::Ok)
        }
        fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
            match event {
                Event::FlushStart => self.0.lock().unwrap().push(Row::Flush),
                Event::SkippedTo { to_time } => {
                    let running = ctx.running_time(*to_time);
                    self.0.lock().unwrap().push(Row::Skip { to: *to_time, running });
                }
                _ => {}
            }
            Ok(())
        }
    }
}

use record_sink::Row;

struct Played {
    rows: Vec<Row>,
    answers: Vec<Result<u64, profluens_core::command::CommandError>>,
    warnings: Vec<String>,
}

fn play(seconds: usize, script: &[(usize, Act)], rate: Option<Value>) -> Played {
    let pcm = f32_bytes(&gen_input(48_000, 2, 48_000 * seconds));
    let mut p = Pipeline::new();
    p.set_pool(1 << 13, 64);
    let src = raw_src::RawAudioSrc::new(pcm).scripted(script, p.control(), p.control());
    let answers = src.answers();
    let src = p.add(src);
    let stretch = p.add(AudioStretch::new(Rate::ONE));
    let (sink, log) = record_sink::RecordSink::new();
    let snk = p.add(sink);
    p.link(src.src(raw_src::RawAudioSrc::OUT), stretch.sink(AudioStretch::IN))
        .expect("link src->stretch");
    p.link(stretch.src(AudioStretch::OUT), snk.sink(record_sink::RecordSink::IN))
        .expect("link stretch->sink");
    if let Some(rate) = rate {
        p.set(stretch, "rate", rate).expect("the rate to start at");
    }
    p.run().expect("run");
    let mut warnings = Vec::new();
    while let Some(m) = p.bus().try_recv() {
        if let profluens_core::bus::BusMessage::Warning { error, .. } = m {
            warnings.push(error.to_string());
        }
    }
    let rows = log.lock().unwrap().clone();
    let answers = answers.lock().unwrap().clone();
    Played { rows, answers, warnings }
}

const HOP_NS: u64 = 1_000_000_000 / 65 + 1_000_000;

fn held_to_the_device(rows: &[Row]) -> (u64, usize) {
    held(rows, 0)
}

fn held(rows: &[Row], settled: usize) -> (u64, usize) {
    const FRAME_NS: u64 = 1_000_000_000 / 48_000 + 2;
    let mut at_one = 0usize;
    let mut anchor: Option<(u64, u64)> = None;
    let (mut worst, mut held) = (0u64, 0usize);
    let mut point: Option<(Timestamp, Timestamp)> = None;
    for row in rows {
        let Row::Buffer { pts, duration, bytes, running, before } = *row else {
            anchor = None;
            point = match *row {
                Row::Skip { to, running } => Some((to, running)),
                _ => None,
            };
            continue;
        };
        if before {
            continue;
        }
        if let (None, Some((to, at))) = (anchor, point.take()) {
            assert!(
                pts <= to && to < pts.saturating_add(duration) || pts == to,
                "{row:?} at {to:?}"
            );
            assert!(running.0.abs_diff(at.0) <= HOP_NS, "{row:?} maps {HOP_NS} ns off its point");
        }
        let (start, frames) = *anchor.get_or_insert((running.0, 0));
        let device = start + frames * 1_000_000_000 / 48_000;
        let off = running.0.abs_diff(device);
        assert!(off <= HOP_NS, "{row:?} is {off} ns from where the device plays it, {device}");
        let plays = (bytes / 8) as u64 * 1_000_000_000 / 48_000;
        at_one = if duration.0.abs_diff(plays) <= 1 { at_one + 1 } else { 0 };
        if settled > 0 && at_one > settled {
            assert!(off <= FRAME_NS, "{row:?} is {off} ns off at rate one, {at_one} buffers in");
        }
        worst = worst.max(off);
        held += 1;
        anchor = Some((start, frames + (bytes / 8) as u64));
    }
    (worst, held)
}

fn stamps_are_contiguous(rows: &[Row]) {
    let mut last: Option<Timestamp> = None;
    for row in rows {
        match *row {
            Row::Buffer { pts, duration, .. } => {
                assert!(pts.is_some() && duration.is_some(), "{row:?}");
                if last.is_some_and(|end| end != pts) {
                    let jumped = last.is_some_and(|end| pts > end);
                    assert!(jumped, "{row:?} overlaps what ended at {last:?}");
                }
                last = Some(pts.saturating_add(duration));
            }
            _ => last = None,
        }
    }
}

fn frames_out(rows: &[Row]) -> usize {
    let bytes = |row: &Row| match *row {
        Row::Buffer { bytes, .. } => bytes,
        _ => 0,
    };
    rows.iter().map(bytes).sum::<usize>() / 8
}

#[test]
fn the_rate_property_is_the_rate_a_pipeline_built_by_hand_starts_at() {
    let played = play(2, &[], Some(Value::Rat(3, 2)));
    let expected = 96_000.0 / 1.5;
    let ratio = frames_out(&played.rows) as f64 / expected;
    assert!(
        (0.99..=1.02).contains(&ratio),
        "{} frames out, expected ~{expected}",
        frames_out(&played.rows)
    );
    stamps_are_contiguous(&played.rows);
    let Row::Buffer { pts, running, .. } = played.rows[played.rows.len() / 2] else { panic!() };
    assert!(pts > Timestamp::from_millis(500));
    assert_eq!(running.0, pts.0 * 2 / 3, "the timeline is at 3/2 from zero");
    let (worst, held) = held_to_the_device(&played.rows);
    assert!(held > 20, "{held} buffers, {worst} ns");
}

#[test]
fn a_rate_changed_many_times_leaves_the_timeline_where_the_device_is() {
    let rates = [
        (3, 2),
        (1, 1),
        (3, 4),
        (2, 1),
        (1, 2),
        (5, 4),
        (1, 1),
        (4, 1),
        (1, 4),
        (9, 8),
        (21, 20),
        (1, 1),
        (19, 20),
        (1, 1),
    ];
    let script: Vec<(usize, Act)> =
        rates.iter().enumerate().map(|(k, (n, d))| (20 + 25 * k, Act::Rate(*n, *d))).collect();
    let played = play(10, &script, None);
    assert!(played.answers.iter().all(Result::is_ok), "{:?}", played.answers);
    assert_eq!(played.answers.len(), rates.len());
    assert!(played.warnings.is_empty(), "{:?}", played.warnings);
    stamps_are_contiguous(&played.rows);
    let (worst, held) = held(&played.rows, 12);
    assert!(held > 250, "{held} buffers");
    let Some(Row::Buffer { pts, duration, .. }) = played.rows.last().copied() else { panic!() };
    assert_eq!(pts.saturating_add(duration), Timestamp::from_secs(10));
    let tail = &played.rows[played.rows.len() - 40..];
    let Row::Buffer { running: first, .. } = tail[0] else { panic!() };
    let Row::Buffer { running: last, bytes, .. } = tail[39] else { panic!() };
    let frames = frames_out(&tail[..39]) as u64;
    let device = first.0 + frames * 1_000_000_000 / 48_000;
    assert!(last.0.abs_diff(device) <= HOP_NS, "{worst} ns at worst, {bytes} bytes last");
}

#[test]
fn a_seek_a_skip_and_a_pause_inside_a_pending_change_leave_one_consistent_map() {
    let script = [
        (20, Act::Rate(3, 2)),
        (60, Act::Rate(3, 4)),
        (60, Act::Seek(1_000)),
        (100, Act::Rate(2, 1)),
        (100, Act::Skip(6_000)),
        (130, Act::Skip(8_000)),
        (130, Act::Rate(1, 2)),
        (160, Act::Seek(3_000)),
        (160, Act::Rate(5, 4)),
        (200, Act::Rate(1, 1)),
        (200, Act::Pause),
        (230, Act::Resume),
        (240, Act::Rate(3, 2)),
    ];
    let played = play(12, &script, None);
    assert!(played.answers.iter().all(Result::is_ok), "{:?}", played.answers);
    assert!(played.warnings.is_empty(), "{:?}", played.warnings);
    stamps_are_contiguous(&played.rows);
    let (worst, held) = held_to_the_device(&played.rows);
    assert!(held > 250, "{held} buffers, {worst} ns");
    let flushes = played.rows.iter().filter(|r| **r == Row::Flush).count();
    let skips = played.rows.iter().filter(|r| matches!(r, Row::Skip { .. })).count();
    assert_eq!((flushes, skips), (2, 2));
    let firsts: Vec<(Timestamp, Timestamp)> = played
        .rows
        .windows(2)
        .filter_map(|w| match (w[0], w[1]) {
            (Row::Flush, Row::Buffer { pts, running, .. }) => Some((pts, running)),
            _ => None,
        })
        .collect();
    let (one, three) = (Timestamp::from_secs(1), Timestamp::from_secs(3));
    assert_eq!(firsts, [(one, one), (three, three)]);
}

#[test]
fn a_refused_rate_is_on_the_bus_and_the_timeline_keeps_the_old_one() {
    let script = [
        (20, Act::Rate(3, 2)),
        (60, Act::Rate(8, 1)),
        (90, Act::Seek(500)),
        (120, Act::Rate(8, 1)),
    ];
    let played = play(4, &script, None);
    assert!(played.answers.iter().all(Result::is_ok), "{:?}", played.answers);
    assert_eq!(played.warnings.len(), 2, "{:?}", played.warnings);
    let named = AudioErrorKind::UnsupportedRate { num: 8, den: 1 }.to_string();
    assert!(played.warnings.iter().all(|w| w.contains(&named)), "{:?}", played.warnings);
    stamps_are_contiguous(&played.rows);
    let (worst, held) = held_to_the_device(&played.rows);
    assert!(held > 100, "{held} buffers, {worst} ns");
    let at = played.rows.iter().position(|r| *r == Row::Flush).expect("the seek");
    let Row::Buffer { pts, running, .. } = played.rows[at + 20] else { panic!() };
    let half = Timestamp::from_millis(500);
    assert_eq!(running.0 - half.0, (pts.0 - half.0) * 2 / 3, "{pts:?} at {running:?}");
}

#[test]
fn registry_knows_audiostretch() {
    let mut reg = pf_registry::Registry::new();
    profluens_audio::register(&mut reg);
    let desc = reg.get("audiostretch").expect("audiostretch is registered");
    assert_eq!(desc.name, "audiostretch");
    assert!(desc.make_default.is_some(), "constructible from a launch string");
    let rate = desc.props.iter().find(|p| p.name == "rate").expect("rate prop");
    assert!(!rate.live, "while playing the rate is the pipeline's command");

    let mut p = Pipeline::new();
    reg.parse(&mut p, "audiostretch rate=1.5").expect("parse audiostretch rate=1.5");
}
