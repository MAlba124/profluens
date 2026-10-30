//! What `opusdec` puts out by what its link takes, float and native lower rates included.

#![allow(clippy::disallowed_methods)] // test fixtures, not a hot path

use pf_harness::Harness;
use pf_opus::OpusDec;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::time::Timestamp;

const CHANNELS: usize = 6;

fn track() -> (Vec<u8>, Vec<Vec<u8>>) {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sines_5_1.mka");
    let file = std::fs::read(path).expect("the fixture");
    let mut reader = pf_mkv::MatroskaReader::new();
    reader.push(&file).expect("the fixture parses");
    let head = reader.tracks()[0].codec_private.clone();
    (head, std::iter::from_fn(|| reader.next_frame()).map(|f| f.data).collect())
}

struct Out {
    bytes: Vec<u8>,
    end: Timestamp,
    rate: i64,
    sample: String,
    layout: i64,
}

fn decode(takes: &[(&'static str, &[ValueDesc])]) -> Out {
    let (head, packets) = track();
    let mut h = Harness::new(OpusDec::new());
    for (field, values) in takes {
        h.fix_src_takes(OpusDec::OUT, field, values);
    }
    let (mut bytes, mut end) = (Vec::new(), Timestamp::ZERO);
    let mut take = |out: Vec<profluens_core::buffer::Buffer>| {
        for b in out {
            end = b.pts.saturating_add(b.duration);
            bytes.extend_from_slice(b.memory.data());
        }
    };
    let buf = h.alloc(&head);
    h.push(buf).expect("head");
    for (i, p) in packets.iter().enumerate() {
        let mut buf = h.alloc(p);
        buf.pts = Timestamp::from_millis(20 * i as u64);
        h.push(buf).expect("push");
        take(h.drain_outputs());
    }
    take(h.eos().expect("eos"));
    let f = h.announced().expect("announced");
    let v = h.vocabulary();
    let Some(Value::Int(rate)) = f.get(v.field_id("rate").expect("rate")) else { panic!("rate") };
    let Some(Value::Id(id)) = f.get(v.field_id("sample").expect("sample")) else {
        panic!("sample")
    };
    let Some(Value::Int(layout)) = f.get(v.field_id("layout").expect("layout")) else {
        panic!("layout")
    };
    Out { bytes, end, rate, sample: v.value_name(id).expect("named").to_string(), layout }
}

fn s16(b: &[u8]) -> Vec<i16> {
    b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect()
}

fn f32s(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

const S16_F32: [ValueDesc; 2] = [ValueDesc::Id("s16"), ValueDesc::Id("f32")];

#[test]
fn nothing_said_is_48_khz_s16() {
    let out = decode(&[]);
    assert_eq!((out.rate, out.sample.as_str()), (48_000, "s16"));
    assert_eq!(out.layout as u64, profluens_audio::default_layout(6), "5.1 with backs, Vorbis's");
}

#[test]
fn float_goes_out_where_the_link_takes_it() {
    let ints = decode(&[]);
    let out = decode(&[("sample", &S16_F32)]);
    assert_eq!((out.rate, out.sample.as_str()), (48_000, "f32"));
    let (ints, floats) = (s16(&ints.bytes), f32s(&out.bytes));
    assert_eq!(ints.len(), floats.len(), "the same samples");
    let worst = ints
        .iter()
        .zip(&floats)
        .map(|(i, f)| (f * 32768.0 - f32::from(*i)).abs())
        .fold(0f32, f32::max);
    assert!(worst <= 1.0, "a float {worst} steps off the s16 decode");
    let fine = floats.iter().filter(|f| (**f * 32768.0).fract().abs() > 0.01).count();
    assert!(fine > floats.len() / 2, "the float carries what s16 rounds off");
}

#[test]
fn a_link_that_takes_a_lower_opus_rate_gets_it_decoded_so() {
    let full = decode(&[("sample", &S16_F32)]);
    let low = decode(&[
        ("rate", &[ValueDesc::Int(44_100), ValueDesc::Int(16_000)]),
        ("sample", &S16_F32),
    ]);
    assert_eq!((low.rate, low.sample.as_str()), (16_000, "f32"));
    let (full_frames, low_frames) =
        (full.bytes.len() / 4 / CHANNELS, low.bytes.len() / 4 / CHANNELS);
    assert_eq!(low_frames * 3, full_frames, "a third of the frames");
    assert_eq!(low.end, full.end, "the stamps end alike");
    let c: Vec<f32> = f32s(&low.bytes).as_chunks::<CHANNELS>().0.iter().map(|f| f[2]).collect();
    let rms = (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt();
    assert!(rms > 0.01, "the centre carries its tone, rms {rms}");
}

#[test]
fn a_link_that_takes_48_khz_or_no_opus_rate_gets_48_khz() {
    let both = decode(&[("rate", &[ValueDesc::Int(16_000), ValueDesc::Int(48_000)])]);
    assert_eq!(both.rate, 48_000);
    let none = decode(&[("rate", &[ValueDesc::Int(44_100)])]);
    assert_eq!(none.rate, 48_000, "for a resampler");
}

/// A corrupt stereo packet opens libopus before a mono head, which must reopen it as mono.
#[test]
fn a_head_after_a_failed_packet_decodes_at_its_own_width() {
    let mut h = Harness::new(OpusDec::new());
    let mut head = b"OpusHead\x01\x01".to_vec();
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    // A stereo TOC whose frame count is out of range, then a mono 20 ms CELT frame.
    let stereo_bad = b"Op\xff\x03Head\x01\x018\x01\x80\xbb\0\0\0\0\0";
    let mono = [0xD8u8, 0xE0, 0x96, 0x4C];
    let mut bytes = 0;
    for (i, p) in [&stereo_bad[..], &head, &mono].into_iter().enumerate() {
        let mut buf = h.alloc(p);
        buf.pts = Timestamp::from_millis(20 * i as u64);
        h.push(buf).expect("push");
        bytes += h.drain_outputs().iter().map(|b| b.memory.len()).sum::<usize>();
    }
    bytes += h.eos().expect("eos").iter().map(|b| b.memory.len()).sum::<usize>();
    let f = h.announced().expect("announced");
    let v = h.vocabulary();
    assert_eq!(f.get(v.field_id("channels").expect("channels")), Some(Value::Int(1)));
    assert_eq!(bytes, (960 - 312) * 2, "one 20 ms mono frame less the pre-skip, s16");
}
