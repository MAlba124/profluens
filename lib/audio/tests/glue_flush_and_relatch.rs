// Test setup, signal generation and byte staging allocate freely.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_audio::format::{FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
use profluens_audio::{
    AudioConvert, AudioDownmix, AudioGain, AudioResample, SampleFormat, downmix_to_stereo,
};
use profluens_core::bus::BusMessage;
use profluens_core::element::Element;
use profluens_core::event::Event;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

fn f32_bytes(input: &[f32]) -> Vec<u8> {
    input.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn take_f32(out: &mut Vec<f32>, bytes: &[u8]) {
    for w in bytes.as_chunks::<4>().0 {
        out.push(f32::from_le_bytes(*w));
    }
}

fn caps(rate: i64, channels: i64, sample: &'static str) -> [(&'static str, ValueDesc); 3] {
    [
        (FIELD_RATE, ValueDesc::Int(rate)),
        (FIELD_CHANNELS, ValueDesc::Int(channels)),
        (FIELD_SAMPLE, ValueDesc::Id(sample)),
    ]
}

fn rig(element: impl Element + 'static, rate: i64, channels: i64, sample: &'static str) -> Harness {
    let mut h = Harness::with_slot_size(element, 1 << 16);
    h.fix_sink_format(IN, "audio/raw", &caps(rate, channels, sample));
    h.start().expect("start");
    h
}

fn format_change(h: &mut Harness, rate: i64, channels: i64, sample: &'static str) -> Event {
    let fields = caps(rate, channels, sample);
    h.fix_sink_format(IN, "audio/raw", &fields);
    let fixed = h
        .vocabulary()
        .build_fixed("audio/raw", &fields)
        .expect("the harness vocabulary interned these names in fix_format");
    Event::FormatChange(fixed)
}

fn warnings(h: &mut Harness) -> usize {
    h.bus_messages().iter().filter(|m| matches!(m, BusMessage::Warning { .. })).count()
}

fn push_all(h: &mut Harness, bytes: &[u8]) -> Vec<u8> {
    let buf = h.alloc(bytes);
    h.push(buf).expect("push");
    let mut out = Vec::new();
    while let Some(b) = h.pull(OUT) {
        out.extend_from_slice(b.memory.data());
    }
    out
}

#[test]
fn audioconvert_flush_drops_the_partial_frame_carry() {
    let mut h = rig(AudioConvert::new(SampleFormat::S32), 48_000, 2, "s16");
    let stale = push_all(&mut h, &[0x11u8, 0x11, 0x22, 0x22, 0x33, 0x33]);
    assert_eq!(stale.len(), 8, "one whole frame converted; 2 bytes held in the carry");

    h.push_event(Event::FlushStart).expect("flush");

    let frames = 1_024usize;
    let pcm: Vec<u8> =
        (0..frames).flat_map(|_| [1_000i16, -1_000]).flat_map(|s| s.to_le_bytes()).collect();
    let got = push_all(&mut h, &pcm);

    let want: Vec<u8> = (0..frames)
        .flat_map(|_| [1_000i32 << 16, (-1_000i32) << 16])
        .flat_map(|v| v.to_le_bytes())
        .collect();
    assert_eq!(
        got.len(),
        want.len(),
        "post-flush output is the wrong length, the stale carry was prepended"
    );
    assert_eq!(got, want, "the carry survived the flush and shifted the interleave");
}

fn said_channels(h: &Harness) -> Option<Value> {
    let field = h.vocabulary().field_id(FIELD_CHANNELS)?;
    h.announced()?.get(field)
}

#[test]
fn audioconvert_follows_a_format_change_mid_stream() {
    let mut h = rig(AudioConvert::new(SampleFormat::S32), 48_000, 2, "s16");
    assert_eq!(push_all(&mut h, &[0u8; 64]).len(), 128, "s16 to s32 doubles");

    let ev = format_change(&mut h, 44_100, 2, "f32");
    h.push_event(ev).expect("format change");
    let got = push_all(&mut h, &f32_bytes(&[0.5, -0.5, 0.25, -0.25]));
    let want: Vec<u8> = [0.5f64, -0.5, 0.25, -0.25]
        .iter()
        .flat_map(|v| ((v * 2_147_483_648.0) as i32).to_le_bytes())
        .collect();
    assert_eq!(got, want, "read as f32, not as the s16 it was");

    let ev = format_change(&mut h, 96_000, 6, "s16");
    h.push_event(ev).expect("format change");
    assert_eq!(push_all(&mut h, &[0u8; 24]).len(), 48, "two 6 channel s16 frames");
    assert_eq!(said_channels(&h), Some(Value::Int(6)), "the new channel count is said");
    assert_eq!(warnings(&mut h), 0, "a followed change is no warning");
}

#[test]
fn audiogain_follows_a_format_change_mid_stream() {
    let mut h = rig(AudioGain::new(0.5), 48_000, 2, "s16");
    push_all(&mut h, &[0u8; 64]);
    let ev = format_change(&mut h, 44_100, 1, "f32");
    h.push_event(ev).expect("format change");
    let got = push_all(&mut h, &f32_bytes(&[1.0, -1.0, 0.5]));
    let mut out = Vec::new();
    take_f32(&mut out, &got);
    assert_eq!(out, [0.5, -0.5, 0.25], "scaled as f32 mono, not as the s16 stereo it was");
    assert_eq!(said_channels(&h), Some(Value::Int(1)));
}

#[test]
fn audioresample_flush_clears_the_filter_delay_line() {
    let mut h = rig(AudioResample::new(24_000), 48_000, 1, "f32");
    push_all(&mut h, &f32_bytes(&vec![1.0f32; 8_192]));

    h.push_event(Event::FlushStart).expect("flush");

    let got = push_all(&mut h, &f32_bytes(&vec![0.0f32; 8_192]));
    let mut out = Vec::new();
    take_f32(&mut out, &got);
    assert!(!out.is_empty(), "expected output after the flush");
    for (i, s) in out.iter().enumerate() {
        assert_eq!(*s, 0.0, "sample {i} = {s}: pre-seek audio smeared through the delay line");
    }
}

#[test]
fn audioresample_flush_drops_the_partial_frame_carry() {
    let mut h = rig(AudioResample::new(24_000), 48_000, 2, "f32");
    push_all(&mut h, &[0xAAu8; 12]);

    h.push_event(Event::FlushStart).expect("flush");

    let frames = 8_192usize;
    let input: Vec<f32> = (0..frames).flat_map(|_| [1.0f32, -1.0]).collect();
    let got = push_all(&mut h, &f32_bytes(&input));
    let mut out = Vec::new();
    take_f32(&mut out, &got);
    assert!(out.len() >= 2, "expected resampled output");
    assert_eq!(out.len() % 2, 0, "output is not a whole number of stereo frames");
    for (i, frame) in out.as_chunks::<2>().0.iter().enumerate().skip(64) {
        assert!(
            frame[0] > 0.0 && frame[1] < 0.0,
            "frame {i} = {frame:?}: the carry survived the flush and swapped the lanes"
        );
    }
}

#[test]
fn audioresample_follows_a_format_change_mid_stream() {
    let mut h = rig(AudioResample::new(24_000), 48_000, 1, "f32");
    push_all(&mut h, &f32_bytes(&vec![0.0f32; 4_800]));

    let ev = format_change(&mut h, 24_000, 2, "s16");
    h.push_event(ev).expect("format change");
    let mut tail = Vec::new();
    while let Some(b) = h.pull(OUT) {
        tail.extend_from_slice(b.memory.data());
    }
    assert!(!tail.is_empty() && tail.len() % 4 == 0, "the old filter's tail, f32 mono");
    let pcm: Vec<u8> =
        (0..480).flat_map(|_| [1_000i16, -1_000]).flat_map(i16::to_le_bytes).collect();
    let got = push_all(&mut h, &pcm);
    assert_eq!(said_channels(&h), Some(Value::Int(2)));
    assert_eq!(got, pcm, "the new input at the target rate passes as it is");
    assert_eq!(warnings(&mut h), 0, "a followed change is no warning");
}

fn surround_pcm(frames: usize) -> Vec<u8> {
    (0..frames)
        .flat_map(|_| [1_000i16, -1_000, 500, 200, 300, -300])
        .flat_map(|s| s.to_le_bytes())
        .collect()
}

#[test]
fn audiodownmix_flush_drops_the_partial_frame_carry() {
    let mut h = rig(AudioDownmix::new(), 48_000, 6, "s16");
    push_all(&mut h, &[0x55u8; 14]);

    h.push_event(Event::FlushStart).expect("flush");

    let frames = 1_024usize;
    let pcm = surround_pcm(frames);
    let got = push_all(&mut h, &pcm);

    let mut want = vec![0u8; frames * 2 * 2];
    downmix_to_stereo(SampleFormat::S16, 6, &pcm, &mut want).expect("fold");
    assert_eq!(
        got.len(),
        want.len(),
        "post-flush output is the wrong length, the stale carry was prepended"
    );
    assert_eq!(got, want, "the carry survived the flush and rotated the channels");
}

#[test]
fn audiodownmix_follows_a_format_change_mid_stream() {
    let mut h = rig(AudioDownmix::new(), 48_000, 6, "s16");
    assert_eq!(push_all(&mut h, &surround_pcm(4)).len(), 16, "four stereo frames");

    let ev = format_change(&mut h, 48_000, 8, "s16");
    h.push_event(ev).expect("format change");
    assert_eq!(push_all(&mut h, &[0u8; 48]).len(), 12, "three 8 channel frames folded");

    let ev = format_change(&mut h, 44_100, 2, "s16");
    h.push_event(ev).expect("format change");
    let stereo: Vec<u8> = (0..8i16).flat_map(i16::to_le_bytes).collect();
    assert_eq!(push_all(&mut h, &stereo), stereo, "stereo passes as it is");
    assert_eq!(warnings(&mut h), 0, "a followed change is no warning");
}

#[test]
fn audioconvert_follows_the_announcement_before_the_first_buffer() {
    let mut h = rig(AudioConvert::new(SampleFormat::S32), 48_000, 2, "s16");
    let ev = format_change(&mut h, 48_000, 2, "f32");
    h.push_event(ev).expect("format change");
    assert_eq!(warnings(&mut h), 0, "an announcement before any buffer is not a relatch");
    let got = push_all(&mut h, &[0u8; 64]);
    assert_eq!(got.len(), 64, "read as the announced f32");
}

#[test]
fn audioresample_follows_the_announcement_before_the_first_buffer() {
    let mut h = rig(AudioResample::new(48_000), 48_000, 1, "f32");
    let ev = format_change(&mut h, 24_000, 2, "s16");
    h.push_event(ev).expect("format change");
    assert_eq!(warnings(&mut h), 0, "an announcement before any buffer is not a relatch");
    let got = push_all(&mut h, &[0u8; 1024]);
    assert!(
        got.len() > 1024 && got.len().is_multiple_of(4),
        "{} bytes, not an upsample of s16 stereo",
        got.len()
    );
}

#[test]
fn audiodownmix_follows_the_announcement_before_the_first_buffer() {
    let mut h = rig(AudioDownmix::new(), 48_000, 6, "s16");
    let ev = format_change(&mut h, 48_000, 8, "s16");
    h.push_event(ev).expect("format change");
    assert_eq!(warnings(&mut h), 0, "an announcement before any buffer is not a relatch");
    let got = push_all(&mut h, &[0u8; 48]);
    assert_eq!(got.len(), 12, "folded as the announced 8 channels");
}

#[test]
fn a_flush_never_disturbs_the_learned_format() {
    let mut h = rig(AudioConvert::new(SampleFormat::S32), 48_000, 2, "s16");
    for _ in 0..5 {
        h.push_event(Event::FlushStart).expect("flush");
        let got = push_all(&mut h, &[0u8; 4]);
        assert_eq!(got.len(), 8, "the flush dropped the latched input format");
    }
    assert_eq!(warnings(&mut h), 0, "a plain flush is not a format change");
}

#[test]
fn audiodownmix_announces_the_channels_it_puts_out() {
    // No src link fixed here, the linked mono case is in format_change.rs
    for (channels, out_channels) in [(1i64, 1i64), (2, 2), (6, 2)] {
        let mut h = rig(AudioDownmix::new(), 48_000, channels, "s16");
        let frames = 480usize;
        let pcm: Vec<u8> =
            (0..frames * channels as usize).flat_map(|n| (n as i16).to_le_bytes()).collect();
        let got = push_all(&mut h, &pcm);
        let announced = h.announced().expect("an announcement with the first buffer");
        let field = h.vocabulary().field_id(FIELD_CHANNELS).expect("interned");
        assert_eq!(announced.get(field), Some(Value::Int(out_channels)), "{channels} in");
        assert_eq!(got.len(), frames * out_channels as usize * 2, "{channels} in");
        if channels <= 2 {
            assert_eq!(got, pcm, "{channels} channels pass through as they are");
        }
    }
}

#[test]
fn what_needs_no_change_goes_on_in_the_buffer_it_came_in() {
    use profluens_core::time::Timestamp;
    let pcm: Vec<u8> = (0..480 * 2).flat_map(|n: i16| n.to_le_bytes()).collect();
    let pts = Timestamp::from_millis(250);
    for name in ["audiodownmix", "audioconvert"] {
        let mut h = match name {
            "audiodownmix" => rig(AudioDownmix::new(), 48_000, 2, "s16"),
            _ => rig(AudioConvert::new(SampleFormat::S16), 48_000, 2, "s16"),
        };
        let mut buf = h.alloc(&pcm);
        buf.pts = pts;
        let came_at = buf.memory.data().as_ptr();
        h.push(buf).expect("push");
        let out = h.pull(OUT).expect("a buffer out");
        assert_eq!(out.memory.data().as_ptr(), came_at, "{name} copied it");
        assert_eq!(out.memory.data(), pcm, "{name}");
        assert_eq!((out.pts, out.duration), (pts, Timestamp::from_millis(10)), "{name}");
        assert!(h.pull(OUT).is_none(), "{name} put out one buffer");

        let ragged = h.alloc(&pcm[..pcm.len() - 1]);
        let ragged_at = ragged.memory.data().as_ptr();
        h.push(ragged).expect("push");
        let out = h.pull(OUT).expect("the whole frames of it");
        assert_ne!(out.memory.data().as_ptr(), ragged_at, "{name}");
        assert_eq!(out.memory.data(), &pcm[..pcm.len() - 4], "{name}");
    }
}
