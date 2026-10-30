//! `flacdec` and the presentation point, at a rate whose sample stamps are rounded.

// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use pf_libflac::{FlacDec, FlacEncoder, SampleFormat};
use profluens_core::time::Timestamp;

#[path = "../../../../lib/audio/tests/support/exact.rs"]
mod exact;
mod support;

use exact::{Settle, Stream, delays, grid};

const RATE: u32 = 22_050;
const CHANNELS: u32 = 2;
const STRIDE: usize = 4;

fn pcm(frames: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(frames * STRIDE);
    for i in 0..frames {
        for c in 0..CHANNELS {
            let phase = std::f64::consts::TAU * (3.0 + c as f64) * (i as f64) / 384.0;
            bytes.extend_from_slice(&((11_000.0 * phase.sin()).round() as i16).to_le_bytes());
        }
    }
    bytes
}

fn encode(pcm: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut enc, header) =
        FlacEncoder::new_streaming(RATE, CHANNELS, SampleFormat::S16, 1152).expect("encoder");
    let mut frames = Vec::new();
    enc.encode_interleaved(pcm, &mut frames).expect("encode");
    enc.finish(&mut frames);
    (header, frames)
}

fn per_frame(enc: &mut FlacEncoder, header: Vec<u8>, pcm: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    enc.encode_interleaved(pcm, &mut frames).expect("encode");
    enc.finish(&mut frames);
    let whole = [header.as_slice(), frames.as_slice()].concat();
    let (head, cuts) = support::split_frames(&whole);
    let mut owned = vec![whole[head].to_vec()];
    owned.extend(cuts.into_iter().map(|r| whole[r].to_vec()));
    owned
}

fn new() -> Harness {
    Harness::new(FlacDec::new())
}

#[test]
fn after_a_skip_the_samples_from_the_point_on_are_those_of_a_full_decode() {
    let source = pcm(RATE as usize / 2);
    let (header, frames) = encode(&source);
    let bytes = [header, frames].concat();
    let pieces: Vec<&[u8]> = bytes.chunks(700).collect();
    let stream =
        Stream { pieces: &pieces, rate: RATE, stride: STRIDE, stamp: &|_, _| {}, first: 0 };
    assert!(stream.full(&new) == source, "lossless, so the full decode is the source");
    stream.check_skips(&new, 8, 24);
}

#[test]
fn after_a_seek_to_a_packet_the_first_sample_out_is_the_one_at_the_point() {
    const BLOCK: usize = 1152;
    let source = pcm(RATE as usize);
    let (mut enc, header) =
        FlacEncoder::new_streaming(RATE, CHANNELS, SampleFormat::S16, BLOCK as u32)
            .expect("encoder");
    let owned = per_frame(&mut enc, header, &source);
    let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let first = 3 * 3_600 * u64::from(RATE) + 5;
    let start_of = |piece: usize| first + ((piece - 1) * BLOCK) as u64;
    let stamp = |piece: usize, buffer: &mut profluens_core::buffer::Buffer| {
        if piece > 0 {
            buffer.pts = grid(start_of(piece), RATE);
        }
    };
    let stream = Stream { pieces: &pieces, rate: RATE, stride: STRIDE, stamp: &stamp, first };
    assert!(stream.full(&new) == source, "lossless, and stamped from the first packet's stamp");
    let last = pieces.len() - 1;
    let restarts = [1, 2, 4, last / 2, last - 1];
    stream.check_seeks(&new, (1, &restarts), &start_of, Settle { frames: 0, within: 0 });
}

#[test]
fn a_seek_stamps_from_its_time_and_puts_nothing_out_before_it() {
    let source = pcm(4096);
    let (header, frames) = encode(&source);
    let push = |h: &mut Harness, bytes: &[u8]| {
        let buffer = h.alloc(bytes);
        h.push(buffer).expect("push");
    };
    let prime = |h: &mut Harness| {
        push(h, &header);
        push(h, &frames);
        drop(h.eos().expect("eos"));
        drop(h.drain_outputs());
    };
    let rerun = |h: &mut Harness, at: Timestamp| -> Vec<(Timestamp, Vec<u8>)> {
        let mut buffer = h.alloc(&frames);
        buffer.pts = at;
        h.push(buffer).expect("push");
        let mut out = h.drain_outputs();
        for _ in 0..16 {
            h.crank().expect("crank");
            out.extend(h.drain_outputs());
        }
        out.extend(h.eos().expect("eos"));
        out.iter().map(|b| (b.pts, b.memory.data().to_vec())).collect()
    };

    let mut h = new();
    prime(&mut h);
    let on = grid(22_050 * 3 + 7, RATE);
    h.seek(0, on).expect("seek");
    let whole = rerun(&mut h, on);
    assert_eq!(whole[0].0, on);
    let whole = whole.concat_bytes();
    assert!(whole.len() >= 1152 * STRIDE, "at least a block came out");
    assert!(source.windows(whole.len()).any(|w| w == whole), "and it is the source's");

    let mut h = new();
    prime(&mut h);
    let off = Timestamp(on.0 + 3);
    h.seek(0, off).expect("seek");
    let late = rerun(&mut h, off);
    assert_eq!(late[0].0, grid(22_050 * 3 + 8, RATE));
    assert!(late[0].0 >= off);
    assert!(late.concat_bytes() == whole);
}

trait ConcatBytes {
    fn concat_bytes(&self) -> Vec<u8>;
}

impl ConcatBytes for Vec<(Timestamp, Vec<u8>)> {
    fn concat_bytes(&self) -> Vec<u8> {
        self.iter().flat_map(|(_, bytes)| bytes.iter().copied()).collect()
    }
}

#[test]
fn a_declared_delay_is_taken_off_the_stamps_and_what_falls_before_zero_is_cut() {
    const BLOCK: usize = 1152;
    let source = pcm(RATE as usize / 2);
    let (mut enc, header) =
        FlacEncoder::new_streaming(RATE, CHANNELS, SampleFormat::S16, BLOCK as u32)
            .expect("encoder");
    let owned = per_frame(&mut enc, header, &source);
    let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    for first in [0u64, 700] {
        let stamp = |piece: usize, buffer: &mut profluens_core::buffer::Buffer| {
            if piece > 0 {
                buffer.pts = grid(first + ((piece - 1) * BLOCK) as u64, RATE);
            }
        };
        let stream = Stream { pieces: &pieces, rate: RATE, stride: STRIDE, stamp: &stamp, first };
        stream.check_delays(&new, "flac", &delays(RATE));
    }
}

#[test]
fn a_target_anywhere_past_the_restart_starts_on_the_samples_of_a_full_decode() {
    const BLOCK: usize = 1152;
    let source = pcm(RATE as usize / 2);
    let (mut enc, header) =
        FlacEncoder::new_streaming(RATE, CHANNELS, SampleFormat::S16, BLOCK as u32)
            .expect("encoder");
    let owned = per_frame(&mut enc, header, &source);
    let pieces: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let start_of = |piece: usize| ((piece - 1) * BLOCK) as u64;
    let stamp = |piece: usize, buffer: &mut profluens_core::buffer::Buffer| {
        if piece > 0 {
            buffer.pts = grid(start_of(piece), RATE);
        }
    };
    let stream = Stream { pieces: &pieces, rate: RATE, stride: STRIDE, stamp: &stamp, first: 0 };
    let needed = stream.primed_after(&new, (1, &[1, 3, 5]), &start_of, (288, 2 * BLOCK as u64, 0));
    assert_eq!(needed, Some(0), "primed from the first sample behind a restart");
}
