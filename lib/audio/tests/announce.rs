// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_audio::format::{AudioFormat, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
use profluens_audio::{
    AudioChop, AudioConvert, AudioDownmix, AudioGain, AudioJoin, AudioResample, AudioStretch,
    SampleFormat,
};
use profluens_core::element::Element;
use profluens_core::event::Event;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Rate;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

fn caps(rate: i64, channels: i64, sample: &'static str) -> [(&'static str, ValueDesc); 3] {
    [
        (FIELD_RATE, ValueDesc::Int(rate)),
        (FIELD_CHANNELS, ValueDesc::Int(channels)),
        (FIELD_SAMPLE, ValueDesc::Id(sample)),
    ]
}

fn push(h: &mut Harness, frames: usize, stride: usize) {
    let buf = h.alloc(&vec![0u8; frames * stride]);
    h.push(buf).expect("push");
    while h.pull(OUT).is_some() {}
}

fn said(h: &Harness) -> Option<(i64, i64, String)> {
    let f = h.announced()?;
    let v = h.vocabulary();
    let int = |name| match f.get(v.field_id(name)?) {
        Some(Value::Int(n)) => Some(n),
        _ => None,
    };
    let sample = match f.get(v.field_id(FIELD_SAMPLE)?) {
        Some(Value::Id(id)) => v.value_name(id)?.to_string(),
        _ => return None,
    };
    Some((int(FIELD_RATE)?, int(FIELD_CHANNELS)?, sample))
}

fn play_on(h: &mut Harness, input: (i64, i64, &'static str), stride: usize, what: &str) {
    let first = (h.format_changes(), said(h));
    assert_eq!(first.0, 1, "{what}: one change with the first buffer");
    h.push_event(Event::FlushStart).expect("flush");
    push(h, 2048, stride);
    let fields = caps(input.0, input.1, input.2);
    h.fix_sink_format(IN, "audio/raw", &fields);
    let again = h.vocabulary().build_fixed("audio/raw", &fields).expect("names the rig interned");
    h.push_event(Event::FormatChange(again)).expect("the same format again");
    push(h, 2048, stride);
    h.eos().expect("eos");
    assert_eq!((h.format_changes(), said(h)), first, "{what}: nothing more was said");
}

fn linked(
    element: impl Element + 'static,
    input: (i64, i64, &'static str),
    want: (i64, i64, &str),
    what: &str,
) {
    let stride =
        input.1 as usize * SampleFormat::from_caps_name(input.2).expect("a sample name").bytes();
    let mut h = Harness::with_slot_size(element, 1 << 16);
    h.fix_sink_format(IN, "audio/raw", &caps(input.0, input.1, input.2));
    h.start().expect("start");
    push(&mut h, 2048, stride);
    assert_eq!(said(&h), Some((want.0, want.1, want.2.to_string())), "{what}: linked");
    play_on(&mut h, input, stride, what);
}

fn late(
    element: impl Element + 'static,
    input: (i64, i64, &'static str),
    want: (i64, i64, &str),
    what: &str,
) {
    let stride =
        input.1 as usize * SampleFormat::from_caps_name(input.2).expect("a sample name").bytes();
    let mut h = Harness::with_slot_size(element, 1 << 16);
    h.start().expect("start");
    assert_eq!(h.format_changes(), 0, "{what}: nothing to say before the input is known");
    let fields = caps(input.0, input.1, input.2);
    h.fix_sink_format(IN, "audio/raw", &fields);
    let fixed = h.vocabulary().build_fixed("audio/raw", &fields).expect("names the rig interned");
    h.push_event(Event::FormatChange(fixed)).expect("format");
    push(&mut h, 2048, stride);
    assert_eq!(said(&h), Some((want.0, want.1, want.2.to_string())), "{what}: late");
    play_on(&mut h, input, stride, what);
}

fn pinned(
    element: impl Element + 'static,
    input: (i64, i64, &'static str),
    want: (i64, i64, &str),
    what: &str,
) {
    let stride =
        input.1 as usize * SampleFormat::from_caps_name(input.2).expect("a sample name").bytes();
    let mut h = Harness::with_slot_size(element, 1 << 16);
    h.fix_src_format(OUT, "audio/raw", &[]);
    for sample in ["u8", "s16", "s24", "s32", "f32"] {
        h.fix_sink_format(SinkPad(7), "audio/raw", &caps(1, 1, sample));
    }
    h.start().expect("start");
    push(&mut h, 2048, stride);
    assert_eq!(said(&h), Some((want.0, want.1, want.2.to_string())), "{what}: pinned");
    assert_eq!(h.format_changes(), 1, "{what}: one change with the first buffer");
    h.push_event(Event::FlushStart).expect("flush");
    push(&mut h, 2048, stride);
    h.eos().expect("eos");
    assert_eq!(h.format_changes(), 1, "{what}: nothing more was said");
}

const S16: (i64, i64, &str) = (44_100, 2, "s16");

#[test]
fn every_glue_element_says_its_output_once_for_an_input_fixed_on_the_link() {
    linked(AudioConvert::new(SampleFormat::F32), S16, (44_100, 2, "f32"), "audioconvert");
    linked(AudioResample::new(48_000), S16, (48_000, 2, "s16"), "audioresample");
    linked(AudioGain::new(0.5), S16, S16, "audiogain");
    linked(AudioStretch::new(Rate::ONE), S16, S16, "audiostretch");
    linked(AudioDownmix::new(), (48_000, 6, "s16"), (48_000, 2, "s16"), "audiodownmix");
    linked(AudioDownmix::new(), (48_000, 1, "s16"), (48_000, 1, "s16"), "audiodownmix of mono");
    linked(AudioChop::new(), S16, S16, "audiochop");
    linked(AudioJoin::new(), S16, S16, "audiojoin");
}

#[test]
fn every_glue_element_says_its_output_once_for_an_input_said_after_the_start() {
    late(AudioConvert::new(SampleFormat::F32), S16, (44_100, 2, "f32"), "audioconvert");
    late(AudioResample::new(48_000), S16, (48_000, 2, "s16"), "audioresample");
    late(AudioGain::new(0.5), S16, S16, "audiogain");
    late(AudioStretch::new(Rate::ONE), S16, S16, "audiostretch");
    late(AudioDownmix::new(), (48_000, 6, "s16"), (48_000, 2, "s16"), "audiodownmix");
    late(AudioChop::new(), S16, S16, "audiochop");
    late(AudioJoin::new(), S16, S16, "audiojoin");
}

#[test]
fn a_pinned_input_is_said_at_start_and_once() {
    let input = AudioFormat::new(44_100, 2, SampleFormat::S16);
    pinned(
        AudioConvert::with_input(input, SampleFormat::F32),
        S16,
        (44_100, 2, "f32"),
        "audioconvert",
    );
    pinned(AudioResample::with_input(input, 48_000), S16, (48_000, 2, "s16"), "audioresample");
    pinned(AudioGain::with_input(input, 0.5), S16, S16, "audiogain");
    pinned(AudioStretch::with_input(input, Rate::ONE), S16, S16, "audiostretch");
}
