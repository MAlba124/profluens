// Tests own their signals, staging `Vec`s and `format!` in assertions.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_audio::{
    AudioConvert, AudioDownmix, AudioFormat, AudioGain, AudioResample, SampleFormat, WavParse,
    convert_interleaved_vec, downmix_to_stereo, position_at, time_at, write_pcm_wav,
};
use profluens_core::buffer::Buffer;
use profluens_core::element::Element;
use profluens_core::format::ValueDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);
const HOURS: u64 = 5;

type Frames = Vec<(u64, Vec<u8>)>;

fn rig(element: impl Element + 'static, format: AudioFormat, slot: usize) -> Harness {
    let mut h = Harness::with_slot_size(element, slot);
    let fields = [
        ("rate", ValueDesc::Int(i64::from(format.sample_rate))),
        ("channels", ValueDesc::Int(i64::from(format.channels))),
        ("sample", ValueDesc::Id(format.format.caps_name())),
    ];
    h.fix_sink_format(IN, "audio/raw", &fields);
    h.start().expect("start");
    h
}

fn draw(seed: &mut u64, below: u64) -> u64 {
    *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
    (*seed >> 33) % below
}

fn pull_frames(h: &mut Harness, stride: usize, rate: u32, into: &mut Frames) {
    while let Some(buf) = h.pull(OUT) {
        let data = buf.memory.data();
        assert_eq!(data.len() % stride, 0, "a buffer of {} bytes is not whole frames", data.len());
        assert!(buf.pts.is_some(), "a buffer with no pts");
        let first = position_at(buf.pts, rate);
        let frames = (data.len() / stride) as u64;
        assert_eq!(buf.pts, time_at(first, rate), "a pts off the grid");
        assert_eq!(buf.duration, time_at(first + frames, rate).saturating_sub(buf.pts));
        for (k, frame) in data.chunks_exact(stride).enumerate() {
            into.push((first + k as u64, frame.to_vec()));
        }
    }
}

fn stamped(h: &mut Harness, bytes: &[u8], byte: u64, stride: usize, rate: u32) -> Buffer {
    let mut buf = h.alloc(bytes);
    buf.pts = time_at(byte / stride as u64, rate);
    buf
}

struct Case {
    rate: u32,
    in_stride: usize,
    out_stride: usize,
    frame: fn(u64) -> Vec<u8>,
    made: fn(&[u8]) -> Vec<u8>,
}

fn feed(
    h: &mut Harness,
    case: &Case,
    (first, frames): (u64, u64),
    (seed, ragged): (&mut u64, bool),
    (got, owed): (&mut Frames, &mut Frames),
) {
    let bytes: Vec<u8> = (first..first + frames).flat_map(case.frame).collect();
    let step = if ragged { 1 } else { case.in_stride };
    let mut at = 0;
    while at < bytes.len() {
        let size = (1 + draw(seed, 700) as usize).next_multiple_of(step);
        let end = (at + size).min(bytes.len());
        let byte = first * case.in_stride as u64 + at as u64;
        let buf = stamped(h, &bytes[at..end], byte, case.in_stride, case.rate);
        h.push(buf).expect("push");
        pull_frames(h, case.out_stride, case.rate, got);
        at = end;
    }
    owed.extend((first..first + frames).map(|n| (n, (case.made)(&(case.frame)(n)))));
}

fn frame_for_frame(element: impl Element + 'static, format: AudioFormat, case: Case) {
    let mut h = rig(element, format, 100);
    let start = HOURS * 3_600 * u64::from(case.rate) + 7;
    let (mut seed, mut got, mut owed) = (0x5EED, Vec::new(), Vec::new());
    feed(&mut h, &case, (start, 3_000), (&mut seed, true), (&mut got, &mut owed));
    let half = (case.frame)(start + 3_000);
    let byte = (start + 3_000) * case.in_stride as u64;
    let buf = stamped(&mut h, &half[..case.in_stride / 2], byte, case.in_stride, case.rate);
    h.push(buf).expect("push");
    let back = start - 40_000;
    h.seek(0, time_at(back, case.rate)).expect("seek");
    feed(&mut h, &case, (back, 2_000), (&mut seed, true), (&mut got, &mut owed));
    let ahead = back + 90_000;
    h.skip_to(time_at(ahead, case.rate)).expect("skip");
    feed(&mut h, &case, (ahead, 2_000), (&mut seed, false), (&mut got, &mut owed));
    assert_eq!(got.len(), owed.len(), "every whole frame goes out once");
    let wrong = got.iter().zip(&owed).position(|(g, o)| g != o);
    assert_eq!(
        wrong, None,
        "the first frame put out under the wrong stamp or with the wrong bytes"
    );
}

fn stereo_s16(n: u64) -> Vec<u8> {
    let v = (n % 20_000) as i16;
    [v.to_le_bytes(), (-v).to_le_bytes()].concat()
}

fn six_s16(n: u64) -> Vec<u8> {
    (0..6i16).flat_map(|ch| ((n % 4_000) as i16 * (ch + 1) - 9_000).to_le_bytes()).collect()
}

fn stereo_f32(n: u64) -> Vec<u8> {
    let v = (n % 20_000) as f32;
    [v.to_le_bytes(), (-v).to_le_bytes()].concat()
}

#[test]
fn audioconvert_stamps_every_frame_as_it_came() {
    let format = AudioFormat::new(44_100, 2, SampleFormat::S16);
    let made = |frame: &[u8]| {
        convert_interleaved_vec(SampleFormat::S16, SampleFormat::F32, 2, frame).expect("one frame")
    };
    let case = Case { rate: 44_100, in_stride: 4, out_stride: 8, frame: stereo_s16, made };
    frame_for_frame(AudioConvert::new(SampleFormat::F32), format, case);
    let case =
        Case { rate: 44_100, in_stride: 4, out_stride: 4, frame: stereo_s16, made: <[u8]>::to_vec };
    frame_for_frame(AudioConvert::new(SampleFormat::S16), format, case);
}

#[test]
fn audiodownmix_stamps_every_frame_as_it_came() {
    let made = |frame: &[u8]| {
        let mut out = vec![0u8; 4];
        downmix_to_stereo(SampleFormat::S16, 6, frame, &mut out).expect("one frame");
        out
    };
    let case = Case { rate: 48_000, in_stride: 12, out_stride: 4, frame: six_s16, made };
    frame_for_frame(AudioDownmix::new(), AudioFormat::new(48_000, 6, SampleFormat::S16), case);
    let case =
        Case { rate: 48_000, in_stride: 4, out_stride: 4, frame: stereo_s16, made: <[u8]>::to_vec };
    frame_for_frame(AudioDownmix::new(), AudioFormat::new(48_000, 2, SampleFormat::S16), case);
}

#[test]
fn audiogain_stamps_every_frame_as_it_came() {
    let made = |frame: &[u8]| {
        let (samples, _) = frame.as_chunks::<4>();
        samples.iter().flat_map(|s| (f32::from_le_bytes(*s) * 0.5).to_le_bytes()).collect()
    };
    let case = Case { rate: 96_000, in_stride: 8, out_stride: 8, frame: stereo_f32, made };
    frame_for_frame(AudioGain::new(0.5), AudioFormat::new(96_000, 2, SampleFormat::F32), case);
}

#[test]
fn wavparse_stamps_from_its_byte_and_the_converter_behind_it_from_that() {
    let format = AudioFormat::new(44_100, 2, SampleFormat::S24);
    let stride = format.frame_stride();
    let frame = |n: u64| -> Vec<u8> {
        let (l, r) = (n as i32 * 3 - 70_000, 90_000 - n as i32);
        [&l.to_le_bytes()[..3], &r.to_le_bytes()[..3]].concat()
    };
    let total = 20_000u64;
    let pcm: Vec<u8> = (0..total).flat_map(frame).collect();
    let wav = write_pcm_wav(&format, &pcm);
    let header = wav.len() - pcm.len();
    let mut parse = Harness::with_slot_size(WavParse::new(), 100);
    let mut convert = rig(AudioConvert::with_input(format, SampleFormat::S32), format, 100);
    let (mut seed, mut got) = (0xBEEF, Vec::new());

    let mut play = |(parse, convert): (&mut Harness, &mut Harness), got: &mut Frames, from, to| {
        let mut at: usize = from;
        while at < to {
            let end = (at + 1 + draw(&mut seed, 900) as usize).min(to);
            let buf = parse.alloc(&wav[at..end]);
            parse.push(buf).expect("parse");
            loop {
                let mut pieces = 0;
                while let Some(piece) = parse.pull(OUT) {
                    let frame = position_at(piece.pts, 44_100);
                    assert_eq!(piece.pts, time_at(frame, 44_100), "wavparse stamps on the grid");
                    assert_eq!(piece.memory.len() % stride, 0, "whole frames");
                    let mut buf = convert.alloc(piece.memory.data());
                    buf.pts = piece.pts;
                    convert.push(buf).expect("convert");
                    pull_frames(convert, 8, 44_100, got);
                    pieces += 1;
                }
                if pieces == 0 {
                    break;
                }
                parse.crank().expect("parse");
            }
            at = end;
        }
    };
    let cut = header + 7_001 * stride + 2;
    play((&mut parse, &mut convert), &mut got, 0, cut);
    let to = 12_000u64;
    let to_byte = header as u64 + to * stride as u64;
    parse.seek(to_byte, time_at(to, 44_100)).expect("seek the parser");
    convert.seek(to_byte, time_at(to, 44_100)).expect("seek the converter");
    play((&mut parse, &mut convert), &mut got, to_byte as usize, wav.len());

    let made = |n: u64| {
        convert_interleaved_vec(SampleFormat::S24, SampleFormat::S32, 2, &frame(n))
            .expect("a frame")
    };
    let owed: Frames = (0..7_001).chain(to..total).map(|n| (n, made(n))).collect();
    assert_eq!(got.len(), owed.len());
    assert_eq!(got.iter().zip(&owed).position(|(g, o)| g != o), None);
}

const TONE: u64 = 300;

fn phase(n: u64, rate: u32) -> f64 {
    let turn = (TONE * n) % u64::from(rate);
    std::f64::consts::TAU * turn as f64 / f64::from(rate)
}

fn tone(first: u64, frames: u64, rate: u32) -> Vec<u8> {
    (first..first + frames)
        .flat_map(|n| (0.5 * phase(n, rate).sin() as f32).to_le_bytes())
        .collect()
}

fn offset_of(run: &[f32], first: u64, rate: u32) -> f64 {
    let (mut i, mut q) = (0.0f64, 0.0f64);
    for (k, y) in run.iter().enumerate() {
        let at = phase(first + k as u64, rate);
        i += f64::from(*y) * at.sin();
        q += f64::from(*y) * at.cos();
    }
    -q.atan2(i) / (std::f64::consts::TAU * TONE as f64 / f64::from(rate))
}

struct Resampled {
    h: Harness,
    from: u32,
    to: u32,
    seed: u64,
    runs: Vec<(u64, Vec<f32>)>,
}

impl Resampled {
    fn new(from: u32, to: u32) -> Resampled {
        let format = AudioFormat::new(from, 1, SampleFormat::F32);
        let h = rig(AudioResample::new(to), format, 1 << 14);
        Resampled { h, from, to, seed: u64::from(from) ^ 0xA5A5, runs: Vec::new() }
    }

    fn feed(&mut self, first: u64, frames: u64, stamp: fn(Timestamp) -> Timestamp) {
        let mut at = first;
        while at < first + frames {
            let n = (1 + draw(&mut self.seed, 3_000)).min(first + frames - at);
            let mut buf = self.h.alloc(&tone(at, n, self.from));
            buf.pts = stamp(time_at(at, self.from));
            self.h.push(buf).expect("push");
            self.pull();
            at += n;
        }
    }

    fn pull(&mut self) {
        while let Some(buf) = self.h.pull(OUT) {
            let data = buf.memory.data();
            assert!(buf.pts.is_some() && data.len() % 4 == 0);
            let first = position_at(buf.pts, self.to);
            assert_eq!(buf.pts, time_at(first, self.to), "a pts off the output grid");
            let frames = (data.len() / 4) as u64;
            assert_eq!(buf.duration, time_at(first + frames, self.to).saturating_sub(buf.pts));
            let samples = data.as_chunks::<4>().0.iter().map(|s| f32::from_le_bytes(*s));
            match self.runs.last_mut() {
                Some((start, run)) if *start + run.len() as u64 == first => run.extend(samples),
                _ => self.runs.push((first, samples.collect())),
            }
        }
    }

    fn worst_offset(&self, margin: usize, periods: usize) -> f64 {
        let size = periods * (self.to as usize / TONE as usize);
        let (mut worst, mut windows) = (0.0f64, 0);
        for (first, run) in &self.runs {
            let inner = run.get(margin..run.len().saturating_sub(margin)).unwrap_or(&[]);
            for (k, window) in inner.chunks_exact(size).enumerate() {
                let at = first + (margin + size * k) as u64;
                worst = worst.max(offset_of(window, at, self.to).abs());
                windows += 1;
            }
        }
        assert!(windows >= 20, "{windows} windows measured");
        worst
    }
}

const PAIRS: [(u32, u32); 4] =
    [(44_100, 48_000), (48_000, 44_100), (96_000, 48_000), (8_000, 48_000)];

fn as_stamped(pts: Timestamp) -> Timestamp {
    pts
}

#[test]
fn audioresample_stamps_the_output_grid_from_the_input_and_the_filter() {
    for (from, to) in PAIRS {
        let mut r = Resampled::new(from, to);
        let start = HOURS * 3_600 * u64::from(from) + 12_345;
        let second = u64::from(from);
        r.feed(start, 3 * second, as_stamped);
        assert_eq!(r.runs.len(), 1, "{from} to {to}, unbroken input is one count");
        let back = start - 7 * second - 3;
        r.h.seek(0, time_at(back, from)).expect("seek");
        r.feed(back, 2 * second, as_stamped);
        assert_eq!(r.runs.len(), 2, "{from} to {to}, the flush starts over");
        let ahead = back + 9 * second + 1;
        r.h.skip_to(time_at(ahead, from)).expect("skip");
        r.feed(ahead, 2 * second, as_stamped);
        assert_eq!(r.runs.len(), 3, "{from} to {to}, the jump moves the stamps");
        r.h.skip_to(time_at(ahead + 2 * second + 500, from)).expect("skip");
        r.feed(ahead + 2 * second, 2 * second, as_stamped);
        assert_eq!(r.runs.len(), 3, "{from} to {to}, no jump and no new count");

        let firsts = [start, back, ahead];
        let lengths = [3 * second, 2 * second, 4 * second];
        for ((first, run), (input, frames)) in r.runs.iter().zip(firsts.iter().zip(lengths)) {
            let scale = |n: u64| (n as u128 * u128::from(to) / u128::from(from)) as u64;
            let starts = scale(*input - 32);
            assert!(first.abs_diff(starts) <= 1, "{from} to {to}, a run at {first} for {starts}");
            assert!((run.len() as u64).abs_diff(scale(frames)) <= 1, "{from} to {to}");
        }
        let worst = r.worst_offset(400, 16);
        assert!(worst <= 0.5 + 0.02, "{from} to {to}, the tone is {worst} samples off its stamps");
    }
}

#[test]
fn audioresample_puts_nothing_out_before_time_zero() {
    for (from, to) in PAIRS {
        let mut r = Resampled::new(from, to);
        r.feed(0, 2 * u64::from(from), as_stamped);
        assert_eq!(r.runs.len(), 1);
        let (first, run) = &r.runs[0];
        assert_eq!(*first, 0, "{from} to {to}");
        let made = 2 * u64::from(to) - (32 * u64::from(to)).div_ceil(u64::from(from));
        assert!((run.len() as u64).abs_diff(made) <= 1, "{from} to {to}, {} for {made}", run.len());
        let worst = r.worst_offset(400, 16);
        assert!(worst <= 0.5 + 0.02, "{from} to {to}, the tone is {worst} samples off its stamps");
    }
}

#[test]
fn audioresample_follows_input_stamps_that_jitter() {
    let round = |pts: Timestamp| Timestamp((pts.0 + 500_000) / 1_000_000 * 1_000_000);
    let mut r = Resampled::new(44_100, 48_000);
    r.feed(44_100 * 60, 3 * 44_100, round);
    let half_a_ms = 24.0;
    let worst = r.worst_offset(0, 2);
    assert!(worst <= half_a_ms + 1.0, "the tone is {worst} samples off its stamps");
    let total: usize = r.runs.iter().map(|(_, run)| run.len()).sum();
    assert!((total as u64).abs_diff(3 * 48_000) <= 2, "{total} samples out");
}
