//! Seeded corruption gate for pf-libflac. Regression cases with exact bytes are at the bottom.

// Test code, fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_harness::Harness;
use pf_libflac::{DecodeError, FlacDec, FlacDecoder, FlacEnc, FlacEncoder};
use pf_libflac::{OggFlacDec, OggFlacDeframe};
use pf_libflac::{SampleFormat, StreamDecoder};
use pf_sim::corrupt::harness::drive;
use pf_sim::corrupt::{
    Chunking, Corpus, Input, Op, Tracking, base_seed, corpus_gate, corpus_gate_quiet, corrupt,
    heap_bounded, heap_during, iterations,
};
use pf_sim::prng::Prng;
use pf_tags::flacmeta as tags;
use profluens_core::event::TagSink;
use profluens_core::time::Timestamp;
use support::{BitWriter, crc8, crc16};

#[path = "../../../../lib/audio/tests/support/pointed.rs"]
mod pointed;

#[global_allocator]
static HEAP: Tracking = Tracking;

/// 65536 interchannel samples of at most 8 channels (RFC 9639 section 9.1).
const MAX_FRAME_SAMPLES: usize = 65_536 * 8;

/// 4 header bytes, a 1-byte coded number, CRC-8, a 2-byte CONSTANT subframe and CRC-16.
const MIN_FRAME_BYTES: usize = 10;

const MIB: usize = 1024 * 1024;

const DECODER_HEAP: usize = 3 * MAX_FRAME_SAMPLES * 8 * 2;

fn pcm(frames: usize, channels: u32, format: SampleFormat) -> Vec<u8> {
    let width = format.bytes_per_sample();
    let mut out = Vec::with_capacity(frames * channels as usize * width);
    for i in 0..frames {
        for c in 0..channels {
            let phase = (i as f64) * (0.021 + 0.007 * c as f64);
            let s = (phase.sin() * 0.4 * (1u64 << (width * 8 - 1)) as f64) as i64;
            out.extend_from_slice(&s.to_le_bytes()[..width]);
        }
    }
    out
}

fn stream(channels: u32, format: SampleFormat, block: u32, frames: usize, tagged: bool) -> Vec<u8> {
    let (mut enc, mut header) =
        FlacEncoder::new_streaming(22_050, channels, format, block).expect("encoder");
    if tagged {
        header[4] &= 0x7f;
        let mut vc = Vec::new();
        vc.extend_from_slice(&3u32.to_le_bytes());
        vc.extend_from_slice(b"ref");
        let comments = ["TITLE=Set Theory", "artist=Carbon Based Lifeforms", "TRACKNUMBER=3"];
        vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
            vc.extend_from_slice(c.as_bytes());
        }
        header.push(4);
        header.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
        header.extend_from_slice(&vc);

        let mut seek = Vec::new();
        for i in 0..3u64 {
            seek.extend_from_slice(&(i * block as u64).to_be_bytes());
            seek.extend_from_slice(&(i * 100).to_be_bytes());
            seek.extend_from_slice(&(block as u16).to_be_bytes());
        }
        seek.extend_from_slice(&[0xff; 18]);
        header.push(3);
        header.extend_from_slice(&(seek.len() as u32).to_be_bytes()[1..]);
        header.extend_from_slice(&seek);

        let art = b"\xFF\xD8\xFF\xE0 not really a jpeg";
        let mut pic = Vec::new();
        pic.extend_from_slice(&3u32.to_be_bytes());
        pic.extend_from_slice(&10u32.to_be_bytes());
        pic.extend_from_slice(b"image/jpeg");
        pic.extend_from_slice(&5u32.to_be_bytes());
        pic.extend_from_slice(b"cover");
        pic.extend_from_slice(&[0u8; 16]);
        pic.extend_from_slice(&(art.len() as u32).to_be_bytes());
        pic.extend_from_slice(art);
        header.push(0x80 | 6);
        header.extend_from_slice(&(pic.len() as u32).to_be_bytes()[1..]);
        header.extend_from_slice(&pic);
    }
    let mut out = header;
    let audio = pcm(frames, channels, format);
    let stride = block as usize * channels as usize * format.bytes_per_sample();
    for piece in audio.chunks(stride) {
        enc.encode_interleaved(piece, &mut out).expect("encode");
    }
    enc.finish(&mut out);
    out
}

fn fields(bytes: &[u8]) -> Vec<usize> {
    let mut at = vec![0, 4, 5, 8, 10, 12, 15, 18, 21, 26];
    let mut pos = 4;
    while pos + 4 <= bytes.len() {
        let last = bytes[pos] & 0x80 != 0;
        let len = u32::from_be_bytes([0, bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        at.extend([pos, pos + 1, pos + 4, pos + 8, pos + 12]);
        pos += 4 + len;
        if last {
            break;
        }
    }
    at.push(pos);
    let syncs = (pos..bytes.len().saturating_sub(1))
        .filter(|&i| bytes[i] == 0xFF && bytes[i + 1] & 0xFE == 0xF8);
    for i in syncs.take(3) {
        at.extend([i, i + 2, i + 4]);
    }
    at.retain(|&a| a < bytes.len());
    at
}

struct Fixture {
    name: &'static str,
    bytes: Vec<u8>,
    fields: Vec<usize>,
}

fn fixtures() -> Vec<Fixture> {
    let make = |name, bytes: Vec<u8>| Fixture { name, fields: fields(&bytes), bytes };
    vec![
        make("mono_s16_block64", stream(1, SampleFormat::S16, 64, 400, false)),
        make("stereo_s16_tagged", stream(2, SampleFormat::S16, 96, 400, true)),
        make("stereo_s24_block128", stream(2, SampleFormat::S24, 128, 300, false)),
        make("six_ch_s8_tagged", stream(6, SampleFormat::S8, 64, 150, true)),
        make("stereo_s32_block64", stream(2, SampleFormat::S32, 64, 200, false)),
    ]
}

fn inputs(fixtures: &[Fixture]) -> Vec<Input<'_>> {
    fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &f.fields }).collect()
}

const TOKENS: &[&[u8]] = &[
    b"fLaC",
    &[0xFF, 0xF8],
    &[0xFF, 0xF9],
    &[0x80, 0x00, 0x00, 0x22],
    &[0x04, 0xFF, 0xFF, 0xFF],
    &[0x86, 0xFF, 0xFF, 0xFF],
    &[0x03, 0x00, 0x00, 0x12],
    &[0x7F, b'F', b'L', b'A', b'C', 1, 0, 0, 0],
    &[0x00; 16],
    &[0xFF; 16],
];

fn random_bits(p: &mut Prng, n: u32) -> u64 {
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    let v = match p.below(4) {
        0 => p.next_u64(),
        1 => p.below(16).wrapping_sub(8),
        2 => 1u64 << (n.max(1) - 1),
        _ => (1u64 << (n.max(1) - 1)) - 1,
    };
    v & mask
}

fn synthetic_residual(p: &mut Prng, w: &mut BitWriter, block: usize, order: usize) {
    let method = if p.chance(3) { 2 + p.below(2) } else { p.below(2) };
    w.write_bits(method, 2);
    let param_bits = if method == 1 { 5 } else { 4 };
    let escape = (1u64 << param_bits) - 1;
    let valid: Vec<u32> =
        (0..=15).filter(|&o| block.is_multiple_of(1 << o) && (block >> o) > order).collect();
    let partition_order = match valid.len() {
        0 => p.below(16) as u32,
        n if p.chance(95) => valid[p.below(n as u64) as usize],
        _ => p.below(16) as u32,
    };
    w.write_bits(partition_order as u64, 4);
    let parts = 1usize << partition_order;
    let part_len = block / parts;
    for part in 0..parts {
        let count = if part == 0 { part_len.saturating_sub(order) } else { part_len };
        if p.chance(10) {
            w.write_bits(escape, param_bits);
            let width = p.below(32) as u32;
            w.write_bits(width as u64, 5);
            for _ in 0..count {
                if width > 0 {
                    w.write_bits(random_bits(p, width), width);
                }
            }
        } else {
            let k = p.below(escape.min(24)) as u32;
            w.write_bits(k as u64, param_bits);
            for _ in 0..count {
                let q = if p.chance(97) { p.below(4) } else { p.below(200) };
                w.write_unary(q as u32);
                if k > 0 {
                    w.write_bits(random_bits(p, k), k);
                }
            }
        }
    }
}

fn synthetic_subframe(p: &mut Prng, w: &mut BitWriter, block: usize, bps: u32) {
    w.write_bits(0, 1);
    let kind = p.below(10);
    let (type_bits, order) = match kind {
        0 => (0b000000, 0),
        1 | 2 => (0b000001, 0),
        3..=6 => {
            let order = p.below(5);
            (0b001000 + order, order as usize)
        }
        _ => {
            let order = 1 + p.below(32);
            (0b100000 + order - 1, order as usize)
        }
    };
    w.write_bits(type_bits, 6);
    let mut bps = bps;
    if p.chance(15) && bps > 1 {
        let zeros = p.below((bps as u64 - 1).min(6)) as u32;
        w.write_bits(1, 1);
        w.write_unary(zeros);
        bps -= zeros + 1;
    } else {
        w.write_bits(0, 1);
    }
    match kind {
        0 => w.write_bits(random_bits(p, bps), bps),
        1 | 2 => {
            for _ in 0..block {
                w.write_bits(random_bits(p, bps), bps);
            }
        }
        3..=6 => {
            for _ in 0..order {
                w.write_bits(random_bits(p, bps), bps);
            }
            synthetic_residual(p, w, block, order);
        }
        _ => {
            for _ in 0..order {
                w.write_bits(random_bits(p, bps), bps);
            }
            let precision = 1 + p.below(15) as u32;
            w.write_bits(precision as u64 - 1, 4);
            w.write_bits(if p.chance(95) { p.below(16) } else { p.below(32) }, 5);
            for _ in 0..order {
                w.write_bits(random_bits(p, precision), precision);
            }
            synthetic_residual(p, w, block, order);
        }
    }
}

fn synthetic_frame(p: &mut Prng, out: &mut Vec<u8>, index: u64, info_channels: u32, info_bps: u32) {
    let mut w = BitWriter::new();
    w.write_bits(0b111_1111_1111_1100, 15);
    w.write_bits(0, 1);
    let (bs_code, block) = match p.below(12) {
        0 | 1 => (1u64, 192usize),
        2 => (8, 256),
        3..=7 => (6, 1 + p.below(256) as usize),
        8..=10 => (7, 1 + p.below(2048) as usize),
        _ => (7, 1 + p.below(65_536) as usize),
    };
    let ch_code = if p.chance(60) { info_channels as u64 - 1 } else { p.below(11) };
    let bd_code = if p.chance(50) { 0 } else { [1u64, 2, 4, 5, 6, 7][p.below(6) as usize] };
    w.write_bits(bs_code, 4);
    w.write_bits([0u64, 9, 10, 4][p.below(4) as usize], 4);
    w.write_bits(ch_code, 4);
    w.write_bits(bd_code, 3);
    w.write_bits(0, 1);
    w.write_bits(index & 0x7f, 8);
    match bs_code {
        6 => w.write_bits(block as u64 - 1, 8),
        7 => w.write_bits(block as u64 - 1, 16),
        _ => {}
    }
    let crc = crc8(w.as_aligned_bytes());
    w.write_bits(crc as u64, 8);
    let depth = match bd_code {
        0 => info_bps,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        _ => 32,
    };
    let (channels, side) = match ch_code {
        0..=7 => (ch_code as u32 + 1, None),
        8 | 10 => (2, Some(1)),
        _ => (2, Some(0)),
    };
    for c in 0..channels {
        let extra = u32::from(side == Some(c));
        synthetic_subframe(p, &mut w, block, depth + extra);
    }
    w.align();
    let crc = crc16(w.as_aligned_bytes());
    w.write_bits(crc as u64, 16);
    out.extend_from_slice(&w.into_bytes());
}

fn synthetic_stream(seed: u64) -> Vec<u8> {
    let mut p = Prng::new(seed);
    let channels = 1 + p.below(8) as u32;
    let bps = [8u32, 12, 16, 20, 24, 32, 4, 5][p.below(8) as usize];
    let mut w = BitWriter::new();
    w.write_bits(0x664C_6143, 32);
    w.write_bits(0x80, 8);
    w.write_bits(34, 24);
    w.write_bits(16, 16);
    w.write_bits(65_535, 16);
    w.write_bits(0, 48);
    w.write_bits(44_100, 20);
    w.write_bits(channels as u64 - 1, 3);
    w.write_bits(bps as u64 - 1, 5);
    w.write_bits(0, 36);
    w.write_bits(0, 64);
    w.write_bits(0, 64);
    let mut out = w.into_bytes();
    for index in 0..1 + p.below(4) {
        synthetic_frame(&mut p, &mut out, index, channels, bps);
    }
    out
}

fn pull_all(bytes: &[u8], chunking: Chunking, reset_before: Option<usize>) -> usize {
    let mut dec = StreamDecoder::new();
    let mut frame = Vec::new();
    let mut frames = 0usize;
    let mut pulls = 0usize;
    let budget = chunking.pieces(bytes).count() + bytes.len() + 8;
    let mut chunks = chunking.pieces(bytes).enumerate();
    loop {
        let done = match chunks.next() {
            Some((i, chunk)) => {
                if reset_before == Some(i) {
                    dec.seek_reset();
                }
                dec.push(chunk);
                false
            }
            None => true,
        };
        loop {
            pulls += 1;
            assert!(pulls <= budget, "more than {budget} pulls on {} bytes", bytes.len());
            match dec.pull_into(&mut frame) {
                Ok(Some(())) => {
                    assert!(frame.len() <= MAX_FRAME_SAMPLES, "frame of {} samples", frame.len());
                    frames += 1;
                }
                Ok(None) => break,
                Err(_) => return frames,
            }
        }
        if done {
            break;
        }
    }
    assert!(
        frames * MIN_FRAME_BYTES <= bytes.len(),
        "{frames} frames out of {} bytes",
        bytes.len()
    );
    frames
}

fn stream_decoder_target(bytes: &[u8], chunking: Chunking) {
    pull_all(bytes, chunking, None);
    pull_all(bytes, chunking, Some(1 + bytes.len() % 3));
}

fn flacdec_target(bytes: &[u8], chunking: Chunking) {
    for flush in [None, Some(1 + bytes.len() % 3)] {
        let mut h = Harness::new(FlacDec::new());
        let span = Timestamp::from_millis(400);
        let driven = pointed::drive_bytes(&mut h, bytes, chunking, flush, span);
        let limit = (bytes.len() / MIN_FRAME_BYTES + 1) * MAX_FRAME_SAMPLES * 4;
        assert!(driven.out_bytes <= limit, "{} PCM bytes from {}", driven.out_bytes, bytes.len());
    }
}

#[test]
fn stream_decoder_survives_corruption() {
    let fixtures = fixtures();
    let gate = Corpus {
        name: "flac_stream_decoder",
        inputs: &inputs(&fixtures),
        tokens: TOKENS,
        iters: iterations(1200),
        sweep: 96,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(DECODER_HEAP, 8, stream_decoder_target));
}

#[test]
fn whole_file_decoder_survives_corruption() {
    let fixtures = fixtures();
    let gate = Corpus {
        name: "flac_whole_decoder",
        inputs: &inputs(&fixtures),
        tokens: TOKENS,
        iters: iterations(1200),
        sweep: 96,
        tolerate: &[],
    };
    let bounded = heap_bounded(DECODER_HEAP, 64 * 8 * 2, |bytes, _| {
        if let Ok(decoded) = FlacDecoder::decode(bytes) {
            assert!(decoded.samples.len() / MAX_FRAME_SAMPLES * MIN_FRAME_BYTES <= bytes.len());
        }
    });
    corpus_gate(&gate, &bounded);
}

#[derive(Default)]
struct Counting {
    entries: usize,
    bytes: usize,
}

impl TagSink for Counting {
    fn text(&mut self, key: &str, value: &str) {
        self.entries += 1;
        self.bytes += key.len() + value.len();
    }
    fn picture(&mut self, mime: &str, data: &[u8]) {
        self.entries += 1;
        self.bytes += mime.len() + data.len();
    }
}

#[test]
fn metadata_readers_survive_corruption() {
    let fixtures = fixtures();
    let gate = Corpus {
        name: "flac_metadata",
        inputs: &inputs(&fixtures),
        tokens: TOKENS,
        iters: iterations(4000),
        sweep: 256,
        tolerate: &[],
    };
    let bounded = heap_bounded(64 * 1024, 6, |bytes, _| {
        let mut sink = Counting::default();
        tags::parse_into(bytes, &mut sink);
        assert!(sink.bytes <= bytes.len() && sink.entries <= bytes.len());
        let owned = tags::parse(bytes);
        assert_eq!(owned.len(), sink.entries);
        let _ = tags::stream_info(bytes);
        assert!(tags::seek_table(bytes).count() <= bytes.len() / 18);
        if let Some(start) = tags::audio_start(bytes) {
            assert!(start <= bytes.len());
        }
    });
    corpus_gate(&gate, &bounded);
}

#[test]
fn flacdec_element_survives_corruption() {
    let fixtures = fixtures();
    let gate = Corpus {
        name: "flacdec_element",
        inputs: &inputs(&fixtures),
        tokens: TOKENS,
        iters: iterations(400),
        sweep: 48,
        tolerate: &[],
    };
    let bounded = heap_bounded(DECODER_HEAP + MAX_FRAME_SAMPLES * 4 + 4 * MIB, 16, flacdec_target);
    corpus_gate(&gate, &bounded);
}

/// A native stream behind the 9 byte Ogg mapping prefix, as one first packet.
fn mapped() -> Vec<u8> {
    let native = stream(2, SampleFormat::S16, 192, 700, true);
    let mut mapped = vec![0x7F];
    mapped.extend_from_slice(b"FLAC");
    mapped.extend_from_slice(&[1, 0, 0, 2]);
    mapped.extend_from_slice(&native);
    mapped
}

#[test]
fn oggflacdeframe_element_survives_corruption() {
    let mapped = mapped();
    let gate = Corpus {
        name: "oggflacdeframe_element",
        inputs: &[Input {
            name: "mapped_stereo_s16",
            bytes: &mapped,
            fields: &[0, 1, 5, 7, 9, 13],
        }],
        tokens: TOKENS,
        iters: iterations(1500),
        sweep: 64,
        tolerate: &[],
    };
    let bounded = heap_bounded(2 * MIB, 16, |bytes, chunking| {
        let mut h = Harness::new(OggFlacDeframe::new());
        let driven = drive(&mut h, bytes, chunking, None);
        assert!(driven.out_bytes <= bytes.len(), "{} out of {}", driven.out_bytes, bytes.len());
    });
    corpus_gate(&gate, &bounded);
}

#[test]
fn oggflacdec_element_survives_corruption() {
    let mapped = mapped();
    let gate = Corpus {
        name: "oggflacdec_element",
        inputs: &[Input {
            name: "mapped_stereo_s16",
            bytes: &mapped,
            fields: &[0, 1, 5, 7, 9, 13],
        }],
        tokens: TOKENS,
        iters: iterations(400),
        sweep: 48,
        tolerate: &[],
    };
    let bounded =
        heap_bounded(DECODER_HEAP + MAX_FRAME_SAMPLES * 4 + 4 * MIB, 16, |bytes, chunking| {
            for flush in [None, Some(1 + bytes.len() % 3)] {
                let mut h = Harness::new(OggFlacDec::new());
                let span = Timestamp::from_millis(400);
                let driven = pointed::drive_bytes(&mut h, bytes, chunking, flush, span);
                let limit = (bytes.len() / MIN_FRAME_BYTES + 1) * MAX_FRAME_SAMPLES * 4;
                assert!(
                    driven.out_bytes <= limit,
                    "{} PCM bytes from {}",
                    driven.out_bytes,
                    bytes.len()
                );
            }
        });
    corpus_gate(&gate, &bounded);
}

#[test]
fn flacenc_element_survives_any_pcm() {
    let formats = [SampleFormat::S8, SampleFormat::S16, SampleFormat::S24, SampleFormat::S32];
    let pcm = pcm(600, 2, SampleFormat::S16);
    let gate = Corpus {
        name: "flacenc_element",
        inputs: &[Input { name: "stereo_s16_pcm", bytes: &pcm, fields: &[] }],
        tokens: &[],
        iters: iterations(100),
        sweep: 24,
        tolerate: &[],
    };
    let bounded = heap_bounded(16 * MIB, 16, |bytes, chunking| {
        let channels = 1 + (bytes.len() % 8) as u32;
        let format = formats[bytes.len() / 8 % 4];
        let mut h = Harness::new(FlacEnc::new(44_100, channels, format));
        let driven = drive(&mut h, bytes, chunking, None);
        let frames = chunking.pieces(bytes).count();
        assert!(driven.out_bytes <= bytes.len() * 2 + frames * 64 + 64 * 1024);
    });
    corpus_gate(&gate, &bounded);
}

#[test]
fn decoders_survive_valid_crc_frames_with_random_content() {
    let count = iterations(48);
    let base = Prng::new(base_seed("flac_synthetic_frames"));
    let decoded = std::cell::Cell::new(0usize);
    let mut cases = 0;
    let mut replay = false;
    for first in (0..count).step_by(64) {
        let batch = first..(first + 64).min(count);
        let streams: Vec<Vec<u8>> =
            batch.clone().map(|i| synthetic_stream(base.substream(i).next_u64())).collect();
        let names: Vec<String> = batch.map(|i| format!("synthetic stream {i}")).collect();
        let inputs: Vec<Input<'_>> = streams
            .iter()
            .zip(&names)
            .map(|(bytes, name)| Input { name, bytes, fields: &[] })
            .collect();
        let gate = Corpus {
            name: "flac_synthetic_frames",
            inputs: &inputs,
            tokens: TOKENS,
            iters: 0,
            sweep: 1,
            tolerate: &[],
        };
        let (base, per_byte) = (DECODER_HEAP + MAX_FRAME_SAMPLES * 4 + 4 * MIB, 16);
        let bounded = heap_bounded(base, per_byte, |bytes, chunking| {
            let chunking = match chunking {
                Chunking::Whole => Chunking::Whole,
                _ => Chunking::Fixed(1 + bytes.len() / 5),
            };
            decoded.set(decoded.get() + pull_all(bytes, chunking, None));
            let _ = FlacDecoder::decode(bytes);
            let mut h = Harness::new(FlacDec::new());
            drive(&mut h, bytes, chunking, None);
        });
        let summary = corpus_gate_quiet(&gate, &bounded);
        cases += summary.cases;
        replay |= summary.replay;
    }
    eprintln!("corruption gate flac_synthetic_frames passed {cases} cases");
    let enough = replay || decoded.get() as u64 >= count / 4;
    assert!(enough, "only {} synthetic frames decoded", decoded.get());
}

#[test]
fn pure_noise_never_decodes_to_more_than_it_holds() {
    let gate = Corpus {
        name: "flac_noise",
        inputs: &[],
        tokens: TOKENS,
        iters: iterations(3000),
        sweep: 0,
        tolerate: &[],
    };
    let bounded = heap_bounded(DECODER_HEAP, 64, |bytes, chunking| {
        stream_decoder_target(bytes, chunking);
        let _ = FlacDecoder::decode(bytes);
        let _ = tags::parse(bytes);
    });
    corpus_gate(&gate, &bounded);
}

fn stream_header(channels: u64, bps: u64) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0x664C_6143, 32);
    w.write_bits(0x8000_0022, 32);
    w.write_bits(16, 16);
    w.write_bits(65_535, 16);
    w.write_bits(0, 48);
    w.write_bits(44_100, 20);
    w.write_bits(channels - 1, 3);
    w.write_bits(bps - 1, 5);
    w.write_bits(0, 36);
    w.write_bits(0, 64);
    w.write_bits(0, 64);
    w.into_bytes()
}

fn frame_head(ch_code: u64, bd_code: u64, block: usize) -> BitWriter {
    let mut w = BitWriter::new();
    w.write_bits(0xFFF8, 16);
    w.write_bits(0b0111, 4);
    w.write_bits(0b1001, 4);
    w.write_bits(ch_code, 4);
    w.write_bits(bd_code, 3);
    w.write_bits(0, 1);
    w.write_bits(0, 8);
    w.write_bits(block as u64 - 1, 16);
    let crc = crc8(w.as_aligned_bytes());
    w.write_bits(crc as u64, 8);
    w
}

fn frame_close(mut w: BitWriter) -> Vec<u8> {
    w.align();
    let crc = crc16(w.as_aligned_bytes());
    w.write_bits(crc as u64, 16);
    w.into_bytes()
}

fn lpc_order_1(w: &mut BitWriter, bps: u32, warm_up: i64, coeff: i64, shift: u64, block: usize) {
    w.write_bits(0b0100_0000, 8);
    w.write_signed(warm_up, bps);
    w.write_bits(14, 4);
    w.write_bits(shift, 5);
    w.write_signed(coeff, 15);
    w.write_bits(0, 2 + 4 + 4);
    for _ in 1..block {
        w.write_bits(1, 1);
    }
}

fn decode_every_way(stream: &[u8]) -> usize {
    let whole = FlacDecoder::decode(stream).expect("valid CRCs and structure").samples.len();
    assert_eq!(pull_all(stream, Chunking::Whole, None), 1);
    let mut h = Harness::new(FlacDec::new());
    assert!(!drive(&mut h, stream, Chunking::Fixed(7), None).failed);
    whole
}

#[test]
fn lpc_prediction_that_overflows_i64_wraps() {
    // Warm-up 2^31 - 1 times 16383 per sample passes 2^63 at the fourth sample.
    let mut stream = stream_header(1, 32);
    let mut w = frame_head(0b0000, 0b111, 16);
    lpc_order_1(&mut w, 32, i32::MAX as i64, 16_383, 0, 16);
    stream.extend_from_slice(&frame_close(w));
    assert_eq!(decode_every_way(&stream), 16);
}

#[test]
fn fixed_prediction_that_overflows_i64_wraps() {
    // Order 4 integrates the residual four times, so 1020 escaped residuals of 2^30 - 1 pass 2^63.
    let block = 1024;
    let mut stream = stream_header(1, 32);
    let mut w = frame_head(0b0000, 0b111, block);
    w.write_bits(0b0001_1000, 8);
    for _ in 0..4 {
        w.write_signed(i32::MAX as i64, 32);
    }
    w.write_bits(0, 2 + 4);
    w.write_bits(0b1111, 4);
    w.write_bits(31, 5);
    for _ in 4..block {
        w.write_bits(0x3FFF_FFFF, 31);
    }
    stream.extend_from_slice(&frame_close(w));
    assert_eq!(decode_every_way(&stream), block);
}

fn doubling_stereo(ch_code: u64, bps: [u32; 2], warm_up: [i64; 2]) -> Vec<u8> {
    let mut stream = stream_header(2, 32);
    let mut w = frame_head(ch_code, 0b111, 32);
    for c in 0..2 {
        lpc_order_1(&mut w, bps[c], warm_up[c], -2, 0, 32);
    }
    stream.extend_from_slice(&frame_close(w));
    stream
}

#[test]
fn left_side_restore_that_overflows_i64_wraps() {
    let stream = doubling_stereo(0b1000, [32, 33], [-(1 << 31), 1 << 31]);
    assert_eq!(decode_every_way(&stream), 64);
}

#[test]
fn side_right_restore_that_overflows_i64_wraps() {
    let stream = doubling_stereo(0b1001, [33, 32], [-(1 << 31), -(1 << 31)]);
    assert_eq!(decode_every_way(&stream), 64);
}

#[test]
fn mid_side_restore_that_overflows_i64_wraps() {
    let stream = doubling_stereo(0b1010, [32, 33], [-(1 << 31), 1 << 31]);
    assert_eq!(decode_every_way(&stream), 64);
}

#[test]
fn declared_total_samples_does_not_size_the_output() {
    // STREAMINFO claiming 2^24 samples ahead of one 64-sample frame.
    let stream: [u8; 42] = [
        0x66, 0x4c, 0x61, 0x43, 0x80, 0x00, 0x00, 0x22, 0x00, 0x40, 0x00, 0x40, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x72, 0x16, 0x00, 0x00,
        0x01, 0x13, 0x02, 0x26, 0x00, 0x6d, 0x4c, 0xbb, 0x23, 0x11, 0x97, 0x5a,
    ];
    let (decoded, grew) = heap_during(|| FlacDecoder::decode(&stream).map(|d| d.samples.len()));
    assert!(grew < MIB, "decoding 42 bytes grew the heap by {grew} bytes ({decoded:?})");
}

#[test]
fn a_frame_that_never_ends_is_refused_at_the_frame_size_cap() {
    // A Rice residual of zero bits never terminates. Buffering stops at the largest frame size.
    let mut dec = StreamDecoder::new();
    let mut head = stream_header(1, 16);
    let mut w = frame_head(0b0000, 0b100, 4096);
    w.write_bits(0b0001_0000, 8);
    w.write_bits(0, 2 + 4 + 4);
    w.align();
    head.extend_from_slice(&w.into_bytes());
    dec.push(&head);
    let zeros = vec![0u8; 6 * MIB];
    let mut frame = Vec::new();
    let mut pushed = 0;
    let refused = loop {
        match dec.pull_into(&mut frame) {
            Ok(None) => {}
            Ok(Some(())) => panic!("an endless frame decoded"),
            Err(e) => break e,
        }
        assert!(pushed <= 3, "still buffering after {pushed} pushes of 6 MiB");
        dec.push(&zeros);
        pushed += 1;
    };
    assert!(matches!(refused, DecodeError::TooLarge(_)), "{refused:?}");
}

#[test]
fn a_metadata_chain_that_never_ends_is_refused_at_the_buffering_cap() {
    let mut dec = StreamDecoder::new();
    let mut head = stream_header(2, 16);
    head[4] &= 0x7f;
    dec.push(&head);
    let mut block = vec![0u8; 1 << 24];
    block[..4].copy_from_slice(&[0x01, 0xFF, 0xFF, 0xFC]);
    let mut frame = Vec::new();
    let mut pushed = 0;
    let refused = loop {
        match dec.pull_into(&mut frame) {
            Ok(None) => {}
            Ok(Some(())) => panic!("a frame decoded out of padding"),
            Err(e) => break e,
        }
        assert!(pushed <= 5, "still buffering after {pushed} blocks of 16 MiB");
        dec.push(&block);
        pushed += 1;
    };
    assert!(matches!(refused, DecodeError::TooLarge(_)), "{refused:?}");
}

// Open cases, inputs the gate found that decode slowly. Ignored until the decoder is linear.

fn zero_filled(name: &str, keep: usize, at: usize, tokens: usize) -> Vec<u8> {
    let fixtures = fixtures();
    let base = fixtures.iter().find(|f| f.name == name).expect("a fixture of that name");
    let ops = [Op::Truncate { at: keep }, Op::Token { at, times: tokens, bytes: vec![0; 16] }];
    corrupt(&base.bytes, &ops)
}

fn within_seconds(limit: u64, target: impl FnOnce()) {
    let begin = std::time::Instant::now();
    target();
    let took = begin.elapsed().as_secs();
    assert!(took <= limit, "took {took} s, limit {limit} s");
}

#[test]
#[ignore = "StreamDecoder needs 164 s (debug) for 94 KiB of zero tokens pushed a byte at a time"]
fn stream_decoder_is_linear_in_token_filled_input_pushed_a_byte_at_a_time() {
    // Gate `flac_stream_decoder`, PF_FUZZ_CASE=0xAF3A95A6DD5AAD.
    let bytes = zero_filled("six_ch_s8_tagged", 127, 52, 5899);
    assert_eq!(bytes.len(), 94_511);
    within_seconds(20, || stream_decoder_target(&bytes, Chunking::Fixed(1)));
}

#[test]
#[ignore = "flacdec needs 386 s (debug) for 198 KiB of zero tokens in chunks of at most 8 bytes"]
fn flacdec_is_linear_in_token_filled_input_pushed_in_tiny_chunks() {
    // Gate `flacdec_element`, PF_FUZZ_CASE=0xEF0DFD1F069BB934.
    let bytes = zero_filled("stereo_s16_tagged", usize::MAX, 28, 12_619);
    assert_eq!(bytes.len(), 202_566);
    let chunking = Chunking::Random { max: 8, seed: 4_236_946_965_260_334_374 };
    within_seconds(20, || flacdec_target(&bytes, chunking));
}
