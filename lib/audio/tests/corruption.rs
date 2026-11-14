// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use pf_sim::corrupt::harness::{Fed, drive};
use pf_sim::corrupt::{
    Chunking, Corpus, Input, Tracking, corpus_gate, heap_bounded, heap_during, iterations,
};
use profluens_audio::time_at;
use profluens_audio::{
    AudioConvert, AudioDownmix, AudioError, AudioErrorKind, AudioFormat, AudioFrameRef, AudioGain,
    AudioResample, AudioStretch, ChannelResampler, MAX_CHANNELS, MAX_SAMPLE_RATE, PolyphaseFilter,
    SampleFormat, WavError, WavParse, convert_interleaved_vec, converted_len, downmix_to_stereo,
    gcd, parse_wav_header, remap_channels, write_pcm_wav,
};
use profluens_core::buffer::Buffer;
use profluens_core::element::Element;
use profluens_core::error::Error;
use profluens_core::format::ValueDesc;
use profluens_core::id::SinkPad;
use profluens_core::time::{Rate, Timestamp};

#[path = "support/pointed.rs"]
mod pointed;

const IN: SinkPad = SinkPad(0);

#[global_allocator]
static HEAP: Tracking = Tracking;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

const SLOT: usize = 1 << 16;

/// Input frames the resample filter reads past an output, `HALF_TAPS` in the element.
const RESAMPLE_HALF_TAPS: u64 = 32;

const SAMPLES: [SampleFormat; 5] =
    [SampleFormat::U8, SampleFormat::S16, SampleFormat::S24, SampleFormat::S32, SampleFormat::F32];

fn pcm(format: &AudioFormat, frames: usize) -> Vec<u8> {
    (0..frames * format.frame_stride()).map(|i| (i * 7 % 251) as u8).collect()
}

fn extensible_wav() -> Vec<u8> {
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
    w.extend_from_slice(&40u32.to_le_bytes());
    w.extend_from_slice(&0xFFFEu16.to_le_bytes());
    w.extend_from_slice(&6u16.to_le_bytes());
    w.extend_from_slice(&48_000u32.to_le_bytes());
    w.extend_from_slice(&(48_000u32 * 18).to_le_bytes());
    w.extend_from_slice(&18u16.to_le_bytes());
    w.extend_from_slice(&24u16.to_le_bytes());
    w.extend_from_slice(&22u16.to_le_bytes());
    w.extend_from_slice(&24u16.to_le_bytes());
    w.extend_from_slice(&0x3Fu32.to_le_bytes());
    w.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
    w.extend_from_slice(b"fact");
    w.extend_from_slice(&4u32.to_le_bytes());
    w.extend_from_slice(&8u32.to_le_bytes());
    w.extend_from_slice(b"LIST");
    w.extend_from_slice(&5u32.to_le_bytes());
    w.extend_from_slice(b"hello\0");
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(8u32 * 18).to_le_bytes());
    w.extend_from_slice(&pcm(&AudioFormat::new(48_000, 6, SampleFormat::S24), 8));
    w
}

fn wav_fields(bytes: &[u8]) -> Vec<usize> {
    let mut at = vec![0, 4, 8];
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let size = &bytes[pos + 4..pos + 8];
        let size = u32::from_le_bytes([size[0], size[1], size[2], size[3]]);
        at.extend([pos, pos + 4, pos + 8]);
        match &bytes[pos..pos + 4] {
            b"fmt " => at.extend([0, 2, 4, 8, 12, 14, 16, 18, 20, 24].map(|o| pos + 8 + o)),
            b"data" => break,
            _ => {}
        }
        pos += 8 + size as usize + (size as usize & 1);
    }
    at.retain(|&a| a < bytes.len());
    at
}

struct Fixture {
    name: &'static str,
    bytes: Vec<u8>,
    fields: Vec<usize>,
}

fn wav_fixtures() -> Vec<Fixture> {
    let make = |name, bytes: Vec<u8>| Fixture { name, fields: wav_fields(&bytes), bytes };
    let canonical = |name, rate, channels, sample, frames| {
        let format = AudioFormat::new(rate, channels, sample);
        make(name, write_pcm_wav(&format, &pcm(&format, frames)))
    };
    vec![
        canonical("stereo_s16", 44_100, 2, SampleFormat::S16, 64),
        canonical("mono_u8", 8_000, 1, SampleFormat::U8, 100),
        canonical("six_ch_s24", 48_000, 6, SampleFormat::S24, 16),
        canonical("stereo_f32", 96_000, 2, SampleFormat::F32, 32),
        canonical("mono_s32_empty", 22_050, 1, SampleFormat::S32, 0),
        make("extensible_fact_list", extensible_wav()),
    ]
}

fn inputs(fixtures: &[Fixture]) -> Vec<Input<'_>> {
    fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &f.fields }).collect()
}

const WAV_TOKENS: &[&[u8]] = &[
    b"RIFF",
    b"WAVE",
    b"fmt ",
    b"data",
    b"RIFF\xFF\xFF\xFF\xFFWAVE",
    b"fmt \x10\0\0\0\x01\0\x02\0\x44\xAC\0\0\x10\xB1\x02\0\x04\0\x10\0",
    b"fmt \x10\0\0\0\x01\0\0\0\x44\xAC\0\0\0\0\0\0\0\0\x10\0",
    b"fmt \x10\0\0\0\x01\0\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF\x20\0",
    b"fmt \x28\0\0\0\xFE\xFF",
    b"fmt \x10\0\0\0\xFE\xFF\x02\0\x44\xAC\0\0\x10\xB1\x02\0\x04\0\x10\0",
    b"fmt \0\0\0\0",
    b"data\xFF\xFF\xFF\xFF",
    b"data\0\0\0\0",
    b"data\x03\0\0\0",
    b"LIST\0\0\0\0",
    b"JUNK\x01\0\0\0\0\0",
    b"JUNK\xFF\xFF\xFF\xFF",
    b"\xFF\xFF\xFF\xFF",
    b"\0\0",
];

const MIN_DATA_OFFSET: usize = 12 + 8 + 16 + 8;

fn wav_target(bytes: &[u8], _: Chunking) {
    let header = match parse_wav_header(bytes) {
        Ok(header) => header,
        Err(e) => {
            assert!(!format!("{e}").is_empty());
            return;
        }
    };
    assert!((MIN_DATA_OFFSET..=bytes.len()).contains(&header.data_offset), "{header:?}");
    assert!(header.format.channels >= 1 && header.format.frame_stride() >= 1);
    assert!(header.data_len.is_none_or(|n| n != 0 && n != u32::MAX as u64));
    assert_eq!(parse_wav_header(&bytes[..header.data_offset]), Ok(header));
    assert_eq!(parse_wav_header(&bytes[..header.data_offset - 1]), Err(WavError::NeedMore));

    let payload = &bytes[header.data_offset..];
    let rewritten = write_pcm_wav(&header.format, payload);
    assert_eq!(rewritten.len(), 44 + payload.len());
    let again = parse_wav_header(&rewritten).expect("the writer's own header");
    assert_eq!((again.format, again.data_offset), (header.format, 44));

    let stride = header.format.frame_stride();
    let whole = &payload[..payload.len() - payload.len() % stride];
    let view = AudioFrameRef::new(whole, header.format).expect("whole frames");
    if let Some(last) = view.frames().checked_sub(1) {
        let channel = header.format.channels as usize - 1;
        if header.format.format.is_float() {
            let _ = (view.sample_f32(0, 0), view.sample_f32(last, channel));
        } else {
            let _ = (view.sample_i32(0, 0), view.sample_i32(last, channel));
        }
    }
    let viewable = AudioFrameRef::new(payload, header.format).is_some();
    assert_eq!(viewable, payload.len().is_multiple_of(stride));
}

#[test]
fn wav_header_parser_survives_corruption() {
    let fixtures = wav_fixtures();
    let gate = Corpus {
        name: "wav_header",
        inputs: &inputs(&fixtures),
        tokens: WAV_TOKENS,
        iters: iterations(3000),
        sweep: 48,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(64 * KIB, 2, wav_target));
}

const MAX_PIECES: usize = 4096;

fn coarse(bytes: &[u8], chunking: Chunking) -> Chunking {
    if chunking.pieces(bytes).count() <= MAX_PIECES {
        chunking
    } else {
        Chunking::Fixed(bytes.len().div_ceil(MAX_PIECES))
    }
}

fn wavparse_target(bytes: &[u8], chunking: Chunking) {
    let chunking = coarse(bytes, chunking);
    let parsed = parse_wav_header(bytes);
    for flush in [None, Some(1 + bytes.len() % 3)] {
        let mut h = Harness::with_slot_size(WavParse::new(), SLOT);
        let driven =
            pointed::drive_bytes(&mut h, bytes, chunking, flush, Timestamp::from_millis(2));
        assert!(driven.out_bytes <= bytes.len(), "{} out of {}", driven.out_bytes, bytes.len());
        if flush.is_some() {
            continue;
        }
        match parsed {
            Ok(header) => {
                let payload = bytes.len() - header.data_offset;
                let want = header.data_len.map_or(payload, |n| payload.min(n as usize));
                let stride = header.format.frame_stride();
                if stride > SLOT {
                    assert_eq!(
                        (driven.failed, driven.out_bytes),
                        (want >= stride, 0),
                        "{header:?}"
                    );
                } else {
                    assert!(!driven.failed, "{header:?} parses, the element failed");
                    assert_eq!(driven.out_bytes, want - want % stride, "{header:?}");
                }
            }
            Err(WavError::NeedMore) => assert_eq!((driven.failed, driven.out_bytes), (false, 0)),
            Err(e) => assert!(driven.failed && driven.out_bytes == 0, "{e:?} went unreported"),
        }
    }
}

#[test]
fn wavparse_element_survives_corruption() {
    let fixtures = wav_fixtures();
    let gate = Corpus {
        name: "wavparse_element",
        inputs: &inputs(&fixtures),
        tokens: WAV_TOKENS,
        iters: iterations(250),
        sweep: 8,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(4 * MIB, 8, wavparse_target));
}

fn extreme_pcm() -> Vec<u8> {
    let mut out = Vec::new();
    let floats = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::MAX,
        f32::MIN,
        f32::MIN_POSITIVE / 2.0,
        -0.0,
        1.0,
        -1.0,
        1.0e30,
    ];
    for f in floats {
        out.extend_from_slice(&f.to_le_bytes());
    }
    for v in [i32::MIN, i32::MAX, -1, 0, 1] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in [i16::MIN, i16::MAX, -1, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&[0x00, 0x00, 0x80, 0xFF, 0xFF, 0x7F, 0x00, 0xFF, 0x80, 0x7F]);
    out.extend((0..600usize).map(|i| (i * 13 % 256) as u8));
    out
}

fn tone_pcm() -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..600 {
        let s = ((i as f32 * 0.11).sin() * 12_000.0) as i16;
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&(s / 2).to_le_bytes());
    }
    out
}

fn pcm_fixtures() -> Vec<Fixture> {
    vec![
        Fixture { name: "extreme_samples", bytes: extreme_pcm(), fields: Vec::new() },
        Fixture { name: "stereo_tone", bytes: tone_pcm(), fields: Vec::new() },
    ]
}

const PCM_TOKENS: &[&[u8]] = &[
    &[0x00, 0x00, 0xC0, 0x7F],
    &[0x00, 0x00, 0x80, 0x7F],
    &[0x00, 0x00, 0x80, 0xFF],
    &[0x00, 0x00, 0x00, 0x80],
    &[0xFF, 0xFF, 0xFF, 0x7F],
    &[0x00, 0x80],
    &[0xFF, 0x7F],
    &[0x80],
];

const SHAPES: [(i64, i64); 25] = [
    (48_000, 2),
    (44_100, 1),
    (8_000, 6),
    (96_000, 8),
    (48_000, 3),
    (22_050, 4),
    (48_000, 5),
    (8_000, 7),
    (48_000, 9),
    (1, 1),
    (400, 2),
    (MAX_SAMPLE_RATE as i64, 2),
    (48_000, MAX_CHANNELS as i64),
    (MAX_SAMPLE_RATE as i64 + 1, 2),
    (48_000, MAX_CHANNELS as i64 + 1),
    (48_000, 65_535),
    (4_294_967_295, 1),
    (0, 2),
    (48_000, 0),
    (-1, 2),
    (48_000, -1),
    (48_000, 65_536),
    (4_294_967_296, 2),
    (i64::MAX, i64::MAX),
    (44_100, 2),
];

struct Shape {
    rate: i64,
    channels: i64,
    sample: SampleFormat,
    format: Option<AudioFormat>,
    knob: usize,
}

fn shape(bytes: &[u8], salt: usize) -> Shape {
    let seed = bytes.len() + 31 * bytes.first().copied().unwrap_or(0) as usize + 7 * salt;
    let (rate, channels) = SHAPES[seed % SHAPES.len()];
    let sample = SAMPLES[seed / SHAPES.len() % SAMPLES.len()];
    let representable =
        (1..=u32::MAX as i64).contains(&rate) && (1..=u16::MAX as i64).contains(&channels);
    let format = representable.then(|| AudioFormat::new(rate as u32, channels as u16, sample));
    Shape { rate, channels, sample, format, knob: seed / (SHAPES.len() * SAMPLES.len()) }
}

fn rig(element: impl Element + 'static, shape: &Shape) -> Harness {
    let mut h = Harness::with_slot_size(element, SLOT);
    h.fix_sink_format(
        IN,
        "audio/raw",
        &[
            ("rate", ValueDesc::Int(shape.rate)),
            ("channels", ValueDesc::Int(shape.channels)),
            ("sample", ValueDesc::Id(shape.sample.caps_name())),
        ],
    );
    h
}

fn assert_refused(driven: &Fed, shape: &Shape, len: usize) {
    assert!(driven.failed || len == 0, "{} Hz, {} ch was accepted", shape.rate, shape.channels);
    assert_eq!(driven.out_bytes, 0);
}

fn whole_frames(format: &AudioFormat, len: usize) -> usize {
    len - len % format.frame_stride()
}

fn each_shape(bytes: &[u8], check: impl Fn(&Shape, Option<usize>)) {
    for salt in 0..2 {
        let shape = shape(bytes, salt);
        check(&shape, None);
        if salt == 0 {
            check(&shape, Some(1 + bytes.len() % 3));
        }
    }
}

fn drive_stamped(
    h: &mut Harness,
    bytes: &[u8],
    chunking: Chunking,
    flush: Option<usize>,
    shape: &Shape,
) -> Fed {
    let (stride, rate) = shape.format.map_or((1, 0), |f| (f.frame_stride().max(1), f.sample_rate));
    let base = (bytes.len() as u64 % 5) << 38;
    let pieces: Vec<&[u8]> = chunking.pieces(bytes).collect();
    let mut at = 0;
    let frames: Vec<u64> = pieces
        .iter()
        .map(|piece| {
            at += piece.len();
            base + ((at - piece.len()) / stride) as u64
        })
        .collect();
    let end = time_at(base + (bytes.len() / stride) as u64 + 1, rate);
    let span = if end.is_some() { end } else { Timestamp::from_secs(1) };
    let stamp = |index: usize, buffer: &mut Buffer| buffer.pts = time_at(frames[index], rate);
    pointed::drive_packets(h, bytes, &pieces, flush, span, &stamp)
}

fn pcm_gate<'a>(name: &'a str, inputs: &'a [Input<'a>]) -> Corpus<'a> {
    Corpus { name, inputs, tokens: &[], iters: iterations(150), sweep: 8, tolerate: &[] }
}

fn convert_target(bytes: &[u8], chunking: Chunking) {
    let chunking = coarse(bytes, chunking);
    each_shape(bytes, |shape, flush| {
        let target = SAMPLES[shape.knob % SAMPLES.len()];
        let mut h = rig(AudioConvert::new(target), shape);
        let driven = drive_stamped(&mut h, bytes, chunking, flush, shape);
        let Some(format) = shape.format else {
            assert_refused(&driven, shape, bytes.len());
            return;
        };
        let want = whole_frames(&format, bytes.len()) / shape.sample.bytes() * target.bytes();
        if format.channels as usize * target.bytes() > SLOT {
            assert!(
                driven.out_bytes == 0 && (driven.failed || want == 0),
                "{format} to {target:?}"
            );
            return;
        }
        assert!(!driven.failed, "{format} to {target:?} failed");
        if flush.is_none() {
            assert_eq!(driven.out_bytes, want, "{format} to {target:?}");
        }
        assert!(driven.out_bytes <= want);
    });
}

#[test]
fn audioconvert_survives_any_pcm() {
    let fixtures = pcm_fixtures();
    let inputs = inputs(&fixtures);
    let gate = pcm_gate("audioconvert_element", &inputs);
    corpus_gate(&gate, &heap_bounded(4 * MIB, 16, convert_target));
}

fn downmix_target(bytes: &[u8], chunking: Chunking) {
    let chunking = coarse(bytes, chunking);
    each_shape(bytes, |shape, flush| {
        let mut h = rig(AudioDownmix::new(), shape);
        let driven = drive_stamped(&mut h, bytes, chunking, flush, shape);
        let Some(format) = shape.format else {
            assert_refused(&driven, shape, bytes.len());
            return;
        };
        assert!(!driven.failed, "{format} failed");
        let whole = whole_frames(&format, bytes.len());
        let want = if format.channels <= 2 { whole } else { whole / format.channels as usize * 2 };
        if flush.is_none() {
            assert_eq!(driven.out_bytes, want, "{format}");
        }
        assert!(driven.out_bytes <= want);
    });
}

#[test]
fn audiodownmix_survives_any_pcm() {
    let fixtures = pcm_fixtures();
    let inputs = inputs(&fixtures);
    let gate = pcm_gate("audiodownmix_element", &inputs);
    corpus_gate(&gate, &heap_bounded(4 * MIB, 8, downmix_target));
}

const GAINS: [f32; 5] = [1.0, 0.0, 0.5, 2.0, 4.0];

fn gain_target(bytes: &[u8], chunking: Chunking) {
    let chunking = coarse(bytes, chunking);
    each_shape(bytes, |shape, flush| {
        let gain = GAINS[shape.knob % GAINS.len()];
        let mut h = rig(AudioGain::new(gain), shape);
        let driven = drive_stamped(&mut h, bytes, chunking, flush, shape);
        let Some(format) = shape.format else {
            assert_refused(&driven, shape, bytes.len());
            return;
        };
        let want = whole_frames(&format, bytes.len());
        let out = driven.out_bytes;
        assert!(out <= want, "{format} gave {out} bytes out of {}", bytes.len());
        if format.frame_stride() <= SLOT {
            assert!(!driven.failed, "{format} at gain {gain} failed");
            if flush.is_none() {
                assert_eq!(driven.out_bytes, want, "{format} at gain {gain}");
            }
        }
    });
}

#[test]
fn audiogain_survives_any_pcm() {
    let fixtures = pcm_fixtures();
    let inputs = inputs(&fixtures);
    let gate = pcm_gate("audiogain_element", &inputs);
    corpus_gate(&gate, &heap_bounded(4 * MIB, 8, gain_target));
}

fn target_rates(rate: u32) -> Vec<u32> {
    let rate = rate as u64;
    let steps = [(1, 1), (2, 1), (1, 2), (3, 2), (1, 3), (7, 1), (160, 147), (1, 100)];
    let candidates = steps.into_iter().map(|(up, down)| rate * up / down).chain([1]);
    candidates
        .filter(|&t| (1..=MAX_SAMPLE_RATE as u64).contains(&t))
        .map(|t| t as u32)
        .filter(|&t| t / gcd(rate as u32, t) <= 160)
        .collect()
}

fn resample_target(bytes: &[u8], chunking: Chunking) {
    let chunking = coarse(bytes, chunking);
    each_shape(bytes, |shape, flush| {
        let targets = shape.format.map_or(vec![48_000], |f| target_rates(f.sample_rate));
        let target = targets[shape.knob % targets.len()];
        let mut h = rig(AudioResample::new(target), shape);
        let driven = drive_stamped(&mut h, bytes, chunking, flush, shape);
        let Some(format) = shape.format.filter(|f| f.within_dsp_limits()) else {
            assert_refused(&driven, shape, bytes.len());
            return;
        };
        assert!(!driven.failed, "{format} to {target} Hz failed");
        let stride = format.frame_stride();
        let frames = (bytes.len() / stride) as u64;
        let (l, m) = {
            let g = gcd(format.sample_rate, target);
            ((target / g) as u64, (format.sample_rate / g) as u64)
        };
        // The filter's lead ahead of the first input, given back as its tail at the end
        let most = ((frames + RESAMPLE_HALF_TAPS) * l).div_ceil(m) + 1;
        assert!(
            driven.out_bytes as u64 <= most * stride as u64,
            "{format} to {target} Hz gave {} bytes out of {}",
            driven.out_bytes,
            bytes.len()
        );
        if l == m && flush.is_none() {
            assert_eq!(driven.out_bytes, frames as usize * stride, "an equal rate passes through");
        }
    });
}

#[test]
fn audioresample_survives_any_pcm() {
    let fixtures = pcm_fixtures();
    let inputs = inputs(&fixtures);
    let gate = pcm_gate("audioresample_element", &inputs);
    corpus_gate(&gate, &heap_bounded(4 * MIB, 256, resample_target));
}

const STRETCH_RATES: [(i64, i64); 9] =
    [(1, 1), (1, 4), (1, 2), (3, 4), (3, 2), (2, 1), (3, 1), (4, 1), (101, 100)];

fn stretch_rate(num: i64, den: i64) -> Rate {
    Rate::new(num, den).unwrap()
}

fn stretch_target(bytes: &[u8], chunking: Chunking) {
    let chunking = coarse(bytes, chunking);
    each_shape(bytes, |shape, flush| {
        let (num, den) = STRETCH_RATES[shape.knob % STRETCH_RATES.len()];
        let mut h = rig(AudioStretch::new(stretch_rate(num, den)), shape);
        let rate = format!("{num}/{den}");
        let driven = drive_stamped(&mut h, bytes, chunking, flush, shape);
        let Some(format) = shape.format.filter(|f| f.within_dsp_limits()) else {
            assert_refused(&driven, shape, bytes.len());
            return;
        };
        assert!(!driven.failed, "{format} at {rate}x failed");
        let stride = format.frame_stride();
        let frames = bytes.len() / stride;
        if num == den {
            if flush.is_none() {
                assert_eq!(driven.out_bytes, frames * stride, "bypass passes {format} through");
            }
            assert!(driven.out_bytes <= frames * stride);
            return;
        }
        let window = 3 * (format.sample_rate as usize / 65).max(2);
        let most = 4 * frames + 2 * window;
        assert!(
            driven.out_bytes <= most * stride,
            "{format} at {rate}x gave {} bytes out of {}",
            driven.out_bytes,
            bytes.len()
        );
    });
}

#[test]
fn audiostretch_survives_any_pcm() {
    let fixtures = pcm_fixtures();
    let inputs = inputs(&fixtures);
    let gate = pcm_gate("audiostretch_element", &inputs);
    corpus_gate(&gate, &heap_bounded(12 * MIB, 512, stretch_target));
}

const RATIOS: [(u32, u32); 7] = [(1, 1), (2, 1), (1, 2), (3, 2), (100, 1), (1_048_575, 1), (1, 7)];

fn library_target(bytes: &[u8], chunking: Chunking) {
    let channels = 1 + bytes.len() % 8;
    for from in SAMPLES {
        for to in SAMPLES {
            let whole = bytes.len().is_multiple_of(from.bytes() * channels);
            let len = converted_len(from, to, channels, bytes.len());
            let out = convert_interleaved_vec(from, to, channels, bytes);
            assert_eq!(len.is_some(), whole, "{from:?} to {to:?}");
            assert_eq!(out.map(|o| o.len()), len);
            assert!(len.is_none_or(|n| n == bytes.len() / from.bytes() * to.bytes()));
        }
        assert!(convert_interleaved_vec(from, SampleFormat::F32, 0, bytes).is_none());
        if from.bytes() > 1 {
            assert!(converted_len(from, SampleFormat::S16, usize::MAX, bytes.len()).is_none());
        }

        let bps = from.bytes();
        for wide in 2..=9 {
            let frames = bytes.len() / (bps * wide);
            let mut out = vec![0u8; frames * 2 * bps];
            let folded = downmix_to_stereo(from, wide, bytes, &mut out);
            assert_eq!(folded, bytes.len().is_multiple_of(bps * wide).then_some(frames * 2 * bps));
        }
        let mut out = vec![0u8; bytes.len() * 2];
        let doubled = remap_channels(from, 1, 2, bytes, &mut out);
        assert_eq!(doubled, bytes.len().is_multiple_of(bps).then_some(bytes.len() / bps * 2 * bps));
        let halved = remap_channels(from, 2, 1, bytes, &mut out);
        assert_eq!(halved, bytes.len().is_multiple_of(2 * bps).then_some(bytes.len() / 2));
    }

    let samples: Vec<f32> =
        bytes.as_chunks::<4>().0.iter().map(|w| f32::from_le_bytes(*w)).collect();
    let (in_rate, out_rate) = RATIOS[bytes.len() % RATIOS.len()];
    let filter = PolyphaseFilter::design(in_rate, out_rate, 8, 9.0);
    let (mut whole, mut pieces) = (Vec::new(), Vec::new());
    ChannelResampler::new(filter.clone()).process(&samples, &mut whole);
    let mut streaming = ChannelResampler::new(filter);
    let mut rest = &samples[..];
    for chunk in chunking.pieces(bytes) {
        let (head, tail) = rest.split_at((chunk.len() / 4).min(rest.len()));
        streaming.process(head, &mut pieces);
        rest = tail;
    }
    streaming.process(rest, &mut pieces);
    let most = (samples.len() as u64 * out_rate as u64).div_ceil(in_rate as u64);
    assert!(whole.len() as u64 <= most, "{} samples out of {}", whole.len(), samples.len());
    assert_eq!(streaming.consumed(), samples.len() as u64);
    assert_eq!(pieces.len(), whole.len(), "{in_rate} to {out_rate}");
    assert!(pieces.iter().zip(&whole).all(|(a, b)| a.to_bits() == b.to_bits()));
}

#[test]
fn conversion_library_survives_any_pcm() {
    let fixtures = pcm_fixtures();
    let gate = Corpus {
        name: "pcm_library",
        inputs: &inputs(&fixtures),
        tokens: PCM_TOKENS,
        iters: iterations(250),
        sweep: 16,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(256 * KIB, 32, library_target));
}

#[test]
fn fixtures_are_valid() {
    let formats = [
        (44_100, 2, SampleFormat::S16, 64),
        (8_000, 1, SampleFormat::U8, 100),
        (48_000, 6, SampleFormat::S24, 16),
        (96_000, 2, SampleFormat::F32, 32),
        (22_050, 1, SampleFormat::S32, 0),
        (48_000, 6, SampleFormat::S24, 8),
    ];
    for (fixture, (rate, channels, sample, frames)) in wav_fixtures().iter().zip(formats) {
        let format = AudioFormat::new(rate, channels, sample);
        let header = parse_wav_header(&fixture.bytes).expect(fixture.name);
        assert_eq!(header.format, format, "{}", fixture.name);
        let payload = frames * format.frame_stride();
        assert_eq!(fixture.bytes.len() - header.data_offset, payload, "{}", fixture.name);
        let mut h = Harness::with_slot_size(WavParse::new(), SLOT);
        let driven = drive(&mut h, &fixture.bytes, Chunking::Fixed(7), None);
        assert_eq!((driven.failed, driven.out_bytes), (false, payload), "{}", fixture.name);
    }
    let tone = tone_pcm();
    let format = AudioFormat::new(8_000, 2, SampleFormat::S16);
    let mut h = Harness::with_slot_size(AudioStretch::with_input(format, stretch_rate(1, 2)), SLOT);
    let driven = drive(&mut h, &tone, Chunking::Fixed(400), None);
    assert!(!driven.failed && driven.out_bytes > tone.len() * 3 / 2, "{} bytes", driven.out_bytes);
}

fn audio_error(error: &Error) -> Option<AudioErrorKind> {
    match error {
        Error::ElementFailed { source, .. } => source.downcast_ref::<AudioError>().map(|e| e.kind),
        _ => None,
    }
}

fn push_error(h: &mut Harness, bytes: &[u8]) -> Option<AudioErrorKind> {
    let buf = h.alloc(bytes);
    let error = h.push(buf).err()?;
    Some(audio_error(&error).expect("an AudioError"))
}

fn caps(rate: i64, channels: i64, sample: SampleFormat) -> Shape {
    Shape { rate, channels, sample, format: None, knob: 0 }
}

#[test]
fn wav_writer_saturates_a_byte_rate_that_overflows_32_bits() {
    let wav: [u8; 44] = [
        0x52, 0x49, 0x46, 0x46, 0x24, 0x01, 0x00, 0x00, 0x57, 0x41, 0x56, 0x45, 0x66, 0x6d, 0x74,
        0x20, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0xff, 0x10, 0xb1,
        0x02, 0x00, 0x04, 0x00, 0x10, 0x00, 0x64, 0x61, 0x74, 0x61, 0x00, 0x07, 0x0e, 0x15,
    ];
    let header = parse_wav_header(&wav).expect("a valid header");
    assert_eq!(header.format, AudioFormat::new(0xFF00_0000, 2, SampleFormat::S16));
    let rewritten = write_pcm_wav(&header.format, &[1, 2, 3, 4]);
    assert_eq!(rewritten[28..32], [0xFF; 4], "the byte rate saturates");
    assert_eq!(parse_wav_header(&rewritten).map(|h| h.format), Ok(header.format));
}

#[test]
fn extensible_fmt_too_short_for_a_subformat_is_refused() {
    let format = AudioFormat::new(48_000, 2, SampleFormat::S16);
    let mut wav = write_pcm_wav(&format, &[0x01, 0x00, 0x34, 0x12]);
    wav[20..22].copy_from_slice(&0xFFFEu16.to_le_bytes());
    assert_eq!(parse_wav_header(&wav), Err(WavError::BadFmt));
    let mut h = Harness::with_slot_size(WavParse::new(), SLOT);
    assert_eq!(push_error(&mut h, &wav), Some(AudioErrorKind::Wav(WavError::BadFmt)));
}

#[test]
fn wavparse_skips_a_chunk_without_holding_it() {
    let format = AudioFormat::new(48_000, 2, SampleFormat::S16);
    let canonical = write_pcm_wav(&format, &pcm(&format, 4));
    let mut wav = canonical[..36].to_vec();
    wav.extend_from_slice(b"LIST");
    wav.extend_from_slice(&(512 * KIB as u32).to_le_bytes());
    wav.resize(wav.len() + 512 * KIB, 0x5A);
    wav.extend_from_slice(&canonical[36..]);
    let mut h = Harness::with_pool(WavParse::new(), SLOT, 2);
    let (driven, grew) = heap_during(|| drive(&mut h, &wav, Chunking::Fixed(4096), None));
    assert_eq!((driven.failed, driven.out_bytes), (false, 16));
    assert!(grew < 256 * KIB, "skipping 512 KiB grew the heap by {grew} bytes");
}

#[test]
fn caps_past_u16_and_u32_do_not_wrap_into_a_format() {
    for (rate, channels) in [(48_000, 65_536), (4_294_967_296 + 48_000, 2)] {
        let shape = caps(rate, channels, SampleFormat::S16);
        let rigs = [
            rig(AudioConvert::new(SampleFormat::F32), &shape),
            rig(AudioDownmix::new(), &shape),
            rig(AudioResample::new(44_100), &shape),
            rig(AudioStretch::new(stretch_rate(3, 2)), &shape),
            rig(AudioGain::new(0.5), &shape),
        ];
        for mut h in rigs {
            let failure = push_error(&mut h, &[0u8; 64]);
            assert_eq!(failure, Some(AudioErrorKind::PcmBeforeFormat), "{rate} Hz, {channels} ch");
        }
    }
}

#[test]
fn a_pinned_format_of_zero_channels_is_a_typed_failure() {
    let none = AudioFormat::new(48_000, 0, SampleFormat::S16);
    let stride = Some(AudioErrorKind::InputStrideUnknown);
    let refused = Some(AudioErrorKind::UnsupportedFormat { format: none });
    let mut h = Harness::with_slot_size(AudioConvert::with_input(none, SampleFormat::F32), SLOT);
    assert_eq!(push_error(&mut h, &[0u8; 64]), stride);
    let mut h = Harness::with_slot_size(AudioGain::with_input(none, 0.5), SLOT);
    assert_eq!(push_error(&mut h, &[0u8; 64]), stride);
    let mut h = Harness::with_slot_size(AudioResample::with_input(none, 44_100), SLOT);
    assert_eq!(push_error(&mut h, &[0u8; 64]), refused);
    let mut h = Harness::with_slot_size(AudioStretch::with_input(none, stretch_rate(3, 2)), SLOT);
    assert_eq!(push_error(&mut h, &[0u8; 64]), refused);
}

#[test]
fn resampler_survives_a_decimation_step_longer_than_its_delay_line() {
    let mut resampler = ChannelResampler::new(PolyphaseFilter::design(100, 1, 8, 9.0));
    let mut out = Vec::new();
    resampler.process(&[0.5; 10], &mut out);
    assert_eq!(out.len(), 1);
    resampler.process(&[0.5; 190], &mut out);
    assert_eq!(out.len(), 2, "200 samples at 100 to 1");

    let format = AudioFormat::new(96_000, 1, SampleFormat::F32);
    let mut h = Harness::with_slot_size(AudioResample::with_input(format, 1_000), SLOT);
    let driven = drive(&mut h, &[0u8; 4 * 300], Chunking::Fixed(40), None);
    assert_eq!((driven.failed, driven.out_bytes), (false, 4 * 4), "300 frames at 96 to 1");
}

#[test]
fn resampler_channels_share_one_filter_table() {
    let format = AudioFormat::new(44_100, MAX_CHANNELS, SampleFormat::S16);
    let (_element, grew) = heap_during(|| AudioResample::with_input(format, 48_000));
    assert!(grew < 256 * KIB, "64 channels grew the heap by {grew} bytes");
}

#[test]
fn a_declared_rate_and_channel_count_do_not_size_dsp_state() {
    let shape = caps(4_000_000, 65, SampleFormat::F32);
    let format = AudioFormat::new(4_000_000, 65, SampleFormat::F32);
    let refused = Some(AudioErrorKind::UnsupportedFormat { format });
    let (failure, grew) = heap_during(|| {
        let mut h = rig(AudioStretch::new(stretch_rate(3, 2)), &shape);
        push_error(&mut h, &[0u8; 260])
    });
    assert_eq!(failure, refused);
    assert!(grew < MIB, "a refused format grew the heap by {grew} bytes");
    let mut h = rig(AudioResample::new(48_000), &shape);
    assert_eq!(push_error(&mut h, &[0u8; 260]), refused);

    let widest = caps(MAX_SAMPLE_RATE as i64, MAX_CHANNELS as i64, SampleFormat::F32);
    let (failure, grew) = heap_during(|| {
        let mut h = rig(AudioStretch::new(stretch_rate(3, 2)), &widest);
        push_error(&mut h, &[0u8; 256])
    });
    assert_eq!(failure, None);
    assert!(grew < 72 * MIB, "the widest format grew the heap by {grew} bytes");
}

#[test]
fn audiostretch_sizes_its_output_buffers_for_the_round() {
    let format = AudioFormat::new(1, 1, SampleFormat::U8);
    let mut h = Harness::with_slot_size(AudioStretch::with_input(format, stretch_rate(1, 2)), SLOT);
    let input: Vec<u8> = (0..2000u32).map(|i| (i * 37 % 256) as u8).collect();
    let (driven, grew) = heap_during(|| drive(&mut h, &input, Chunking::Whole, None));
    assert!(!driven.failed && driven.out_bytes >= input.len(), "{} bytes out", driven.out_bytes);
    assert!(grew < 2 * MIB, "2000 bytes grew the heap by {grew} bytes");
}
