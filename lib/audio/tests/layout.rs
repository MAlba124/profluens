// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_audio::format::{FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
use profluens_audio::layout::{FC, FIELD_LAYOUT, FL, FR, LFE, SL, SR, STEREO, default_layout};
use profluens_audio::{
    AudioChop, AudioConvert, AudioDownmix, AudioGain, AudioJoin, AudioResample, AudioStretch,
    SampleFormat, downmix_layout_to_stereo,
};
use profluens_core::element::Element;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::SinkPad;
use profluens_core::time::{Rate, Timestamp};

const IN: SinkPad = SinkPad(0);

const SIDE_51: u64 = FL | FR | FC | LFE | SL | SR;

fn rig(element: impl Element + 'static, channels: i64, layout: Option<u64>) -> Harness {
    let mut h = Harness::with_slot_size(element, 1 << 16);
    let mut fields = vec![
        (FIELD_RATE, ValueDesc::Int(48_000)),
        (FIELD_CHANNELS, ValueDesc::Int(channels)),
        (FIELD_SAMPLE, ValueDesc::Id("s16")),
    ];
    if let Some(mask) = layout {
        fields.push((FIELD_LAYOUT, ValueDesc::Int(mask as i64)));
    }
    h.fix_sink_format(IN, "audio/raw", &fields);
    h.start().expect("start");
    h
}

fn run(h: &mut Harness, samples: &[i16]) -> Vec<u8> {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut buf = h.alloc(&bytes);
    buf.pts = Timestamp::ZERO;
    h.push(buf).expect("push");
    let mut out = Vec::new();
    for b in h.eos().expect("eos") {
        out.extend_from_slice(b.memory.data());
    }
    out
}

fn said_layout(h: &Harness) -> Option<u64> {
    let f = h.announced().expect("announced");
    match f.get(h.vocabulary().field_id(FIELD_LAYOUT)?) {
        Some(Value::Int(n)) => Some(n as u64),
        _ => None,
    }
}

fn hands_on<E: Element + 'static>(name: &str, make: impl Fn() -> E) {
    let mut h = rig(make(), 6, Some(SIDE_51));
    run(&mut h, &[0; 6 * 480]);
    assert_eq!(said_layout(&h), Some(SIDE_51), "{name}");
    let mut h = rig(make(), 6, None);
    run(&mut h, &[0; 6 * 480]);
    assert_eq!(said_layout(&h), None, "{name}: none came");
}

#[test]
fn every_glue_element_that_keeps_the_channels_hands_on_the_layout() {
    hands_on("audioconvert", || AudioConvert::new(SampleFormat::F32));
    hands_on("audioresample", || AudioResample::new(44_100));
    hands_on("audiogain", || AudioGain::new(0.5));
    hands_on("audiostretch", || AudioStretch::new(Rate::ONE));
    hands_on("audiochop", AudioChop::new);
    hands_on("audiojoin", AudioJoin::new);
}

#[test]
fn the_downmix_folds_by_the_layout_and_says_stereo() {
    let frame: [i16; 7] = [1000, 2000, 3000, 30000, 800, 400, 600];
    let input: Vec<u8> = frame.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut want = [0u8; 4];
    downmix_layout_to_stereo(SampleFormat::S16, default_layout(7), 7, &input, &mut want).unwrap();
    assert_ne!(&want[..], &input[..4], "not the front pair as is");
    for layout in [Some(default_layout(7)), None] {
        let mut h = rig(AudioDownmix::new(), 7, layout);
        assert_eq!(run(&mut h, &frame), want, "{layout:?}");
        assert_eq!(said_layout(&h), Some(STEREO));
    }
}

#[test]
fn a_mono_downmix_folds_a_stereo_input_to_one_channel() {
    let mut h = rig(AudioDownmix::new().to_mono(), 2, Some(STEREO));
    let frames: Vec<i16> = (0..480).flat_map(|_| [1000, 3000]).collect();
    let out = run(&mut h, &frames);
    assert_eq!(out.len(), 480 * 2, "one s16 sample a frame");
    let first = i16::from_le_bytes([out[0], out[1]]);
    assert!((first - 2000).abs() <= 1, "the mean of the pair, {first}");
    assert_eq!(said_layout(&h), Some(FC));
}

#[test]
fn the_downmix_hands_on_the_layout_of_what_it_passes() {
    let mut h = rig(AudioDownmix::new(), 1, Some(FC));
    run(&mut h, &[7; 480]);
    assert_eq!(said_layout(&h), Some(FC));
}
