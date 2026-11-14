//! The glue elements follow a format change mid stream, as a broadcast switching 5.1 to 2.0 at
//! an ad break sends it, with the stamps running on across it.

// Test setup, signal generation and byte staging allocate freely.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_audio::format::{FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
use profluens_audio::layout::{FC, FIELD_LAYOUT, FL, FR, LFE};
use profluens_audio::{AudioDownmix, AudioResample};
use profluens_core::buffer::Buffer;
use profluens_core::element::Element;
use profluens_core::event::Event;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

const IN: SinkPad = SinkPad(0);

fn caps(rate: i64, channels: i64, layout: Option<u64>) -> Vec<(&'static str, ValueDesc)> {
    let mut f = vec![
        (FIELD_RATE, ValueDesc::Int(rate)),
        (FIELD_CHANNELS, ValueDesc::Int(channels)),
        (FIELD_SAMPLE, ValueDesc::Id("f32")),
    ];
    if let Some(mask) = layout {
        f.push((FIELD_LAYOUT, ValueDesc::Int(mask as i64)));
    }
    f
}

fn rig(element: impl Element + 'static, rate: i64, channels: i64, layout: Option<u64>) -> Harness {
    let mut h = Harness::with_slot_size(element, 1 << 16);
    h.fix_sink_format(IN, "audio/raw", &caps(rate, channels, layout));
    h.start().expect("start");
    h
}

fn change(h: &mut Harness, rate: i64, channels: i64, layout: Option<u64>) {
    let fields = caps(rate, channels, layout);
    h.fix_sink_format(IN, "audio/raw", &fields);
    let fixed = h.vocabulary().build_fixed("audio/raw", &fields).expect("interned names");
    h.push_event(Event::FormatChange(fixed)).expect("format change");
}

fn feed(h: &mut Harness, samples: &[f32], pts: Timestamp) -> Vec<Buffer> {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut b = h.alloc(&bytes);
    b.pts = pts;
    h.push(b).expect("push");
    h.drain_outputs()
}

fn f32s(out: &[Buffer]) -> Vec<f32> {
    out.iter()
        .flat_map(|b| b.memory.data().as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)))
        .collect()
}

fn frames(out: &[Buffer], channels: usize) -> usize {
    out.iter().map(|b| b.memory.len() / (4 * channels)).sum()
}

fn said_channels(h: &Harness) -> Option<Value> {
    h.announced()?.get(h.vocabulary().field_id(FIELD_CHANNELS)?)
}

#[test]
fn downmix_reads_stereo_as_stereo_after_a_switch_from_5_1() {
    let mut h = rig(AudioDownmix::new().normalized(), 48_000, 6, None);
    let six = feed(&mut h, &[0.1f32; 4800 * 6], Timestamp::ZERO);
    assert_eq!(frames(&six, 2), 4800);
    change(&mut h, 48_000, 2, None);
    let pairs: Vec<f32> = (0..4800).flat_map(|_| [0.5f32, -0.5]).collect();
    let two = feed(&mut h, &pairs, Timestamp::from_millis(100));
    assert_eq!(frames(&two, 2), 4800, "4800 stereo frames in, 4800 out");
    assert_eq!(f32s(&two), pairs, "stereo passes as it is");
    assert_eq!(two[0].pts, Timestamp::from_millis(100));
    let dur: u64 = two.iter().map(|b| b.duration.0).sum();
    assert_eq!(dur, 100_000_000);
}

#[test]
fn downmix_folds_5_1_after_a_switch_from_stereo() {
    let mut h = rig(AudioDownmix::new(), 48_000, 2, None);
    feed(&mut h, &[0.25f32; 480 * 2], Timestamp::ZERO);
    change(&mut h, 48_000, 6, None);
    let out = feed(&mut h, &[0.1f32; 480 * 6], Timestamp::from_millis(10));
    assert_eq!(frames(&out, 2), 480, "six channel frames, not pairs");
    assert_eq!(said_channels(&h), Some(Value::Int(2)));
}

#[test]
fn downmix_follows_a_layout_only_change() {
    // Centre at -3 dB on both sides, then the third channel an LFE, which the fold drops.
    let mut h = rig(AudioDownmix::new(), 48_000, 3, Some(FL | FR | FC));
    let centre = feed(&mut h, &[0.0, 0.0, 1.0], Timestamp::ZERO);
    let got = f32s(&centre);
    assert!(got.iter().all(|s| (s - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3), "{got:?}");
    change(&mut h, 48_000, 3, Some(FL | FR | LFE));
    let lfe = feed(&mut h, &[0.0, 0.0, 1.0], Timestamp::from_micros(21));
    assert_eq!(f32s(&lfe), [0.0, 0.0], "the new layout's LFE is dropped");
}

#[test]
fn a_mono_input_to_the_stereo_downmix_goes_out_on_both_sides() {
    let mut h = rig_out(AudioDownmix::new(), 1, 2);
    let out = feed(&mut h, &[0.5, -0.25, 0.125], Timestamp::ZERO);
    assert_eq!(said_channels(&h), Some(Value::Int(2)), "two, as the link solve fixed");
    assert_eq!(f32s(&out), [0.5, 0.5, -0.25, -0.25, 0.125, 0.125]);
    assert_eq!(out[0].duration, Timestamp(62_500), "three frames, not six");
}

#[test]
fn a_stereo_input_to_a_link_fixed_mono_is_folded_to_one_channel() {
    let mut h = rig_out(AudioDownmix::new(), 2, 1);
    let out = feed(&mut h, &[0.5, 0.25, -0.5, -0.25], Timestamp::ZERO);
    assert_eq!(said_channels(&h), Some(Value::Int(1)));
    assert_eq!(f32s(&out), [0.375, -0.375]);
}

/// A 48 kHz rig with the src link fixed too, as the link solve does before start.
fn rig_out(element: impl Element + 'static, channels: i64, out: i64) -> Harness {
    let mut h = Harness::with_slot_size(element, 1 << 16);
    h.fix_sink_format(IN, "audio/raw", &caps(48_000, channels, None));
    h.fix_src_format(SrcPad(0), "audio/raw", &caps(48_000, out, None));
    h.start().expect("start");
    h
}

/// Stamps that run on, each buffer where the one before it ended, within a sample.
fn assert_contiguous(out: &[Buffer], rate: u64) {
    let sample = 1_000_000_000 / rate + 1;
    for w in out.windows(2) {
        let end = w[0].pts.0 + w[0].duration.0;
        assert!(w[1].pts.0.abs_diff(end) <= sample, "{} then {}", end, w[1].pts.0);
    }
}

#[test]
fn resample_follows_a_rate_change_and_its_stamps_run_on() {
    let mut h = rig(AudioResample::new(48_000), 44_100, 1, None);
    let mut out = feed(&mut h, &[0.25f32; 4410], Timestamp::ZERO);
    change(&mut h, 48_000, 1, None);
    out.extend(h.drain_outputs());
    let before = frames(&out, 1);
    assert_eq!(before, 4800, "100 ms of 44.1 kHz is 4800 frames, the filter's tail included");
    out.extend(feed(&mut h, &[0.5f32; 4800], Timestamp::from_millis(100)));
    out.extend(h.eos().expect("eos"));
    assert_eq!(frames(&out, 1), 9600, "the 48 kHz input passes, not read as 44.1 kHz");
    assert_contiguous(&out, 48_000);
    let last = out.last().expect("output");
    assert_eq!(last.pts.0 + last.duration.0, 200_000_000);
}

#[test]
fn resample_rebuilds_its_filter_for_a_new_input_rate() {
    let mut h = rig(AudioResample::new(48_000), 32_000, 2, None);
    let mut out = feed(&mut h, &vec![0.0f32; 3200 * 2], Timestamp::ZERO);
    change(&mut h, 44_100, 2, None);
    out.extend(feed(&mut h, &vec![0.0f32; 4410 * 2], Timestamp::from_millis(100)));
    out.extend(h.eos().expect("eos"));
    assert_eq!(frames(&out, 2), 9600, "100 ms of each at 48 kHz");
    assert_contiguous(&out, 48_000);
}

#[test]
fn resample_puts_out_the_filter_tail_at_eos() {
    let mut h = rig(AudioResample::new(48_000), 44_100, 1, None);
    let mut out = feed(&mut h, &[0.25f32; 4410], Timestamp::ZERO);
    out.extend(h.eos().expect("eos"));
    assert_eq!(frames(&out, 1), 4800, "every input frame, the last 32 included");
    let last = out.last().expect("output");
    assert_eq!(last.pts.0 + last.duration.0, 100_000_000);
    let tail = f32s(&out);
    assert!((tail[4700] - 0.25).abs() < 0.01, "the tail is the signal, {}", tail[4700]);
}
