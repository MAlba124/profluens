//! Seeded corruption gate for pf-opus, the elements and wrappers must survive corrupt input within
//! their step and heap budgets. A fault inside libopus aborts the binary, not a case.

#![allow(clippy::disallowed_methods)] // test fixtures and case buffers

use pf_harness::Harness;
use pf_opus::libopus::{LibopusDecoder, LibopusEncoder};
use pf_opus::{Application, EncoderConfig, OpusDec, OpusEnc, Signal};
use pf_sim::corrupt::harness::{Fed, drive};
use pf_sim::corrupt::{
    Chunking, Corpus, Input, Tracking, corpus_gate, heap_bounded, heap_during, iterations,
};
use profluens_core::format::{Value, ValueDesc};
use profluens_core::time::Timestamp;

#[path = "../../../../lib/audio/tests/support/pointed.rs"]
mod pointed;

#[global_allocator]
static HEAP: Tracking = Tracking;

/// 120 ms, RFC 6716 section 3.4 R5.
const MAX_PACKET_SAMPLES: usize = 5760;

const MAX_FRAME_BYTES: usize = 1275;

/// TOC, frame count and a two byte length per frame (RFC 6716 section 3.2.5).
const fn max_packet_bytes(frames: usize) -> usize {
    2 + frames * (2 + MAX_FRAME_BYTES)
}

/// Two bytes can be 120 ms of audio, so a case's work goes by packets, not bytes.
const MAX_CASE_PACKETS: usize = 64;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

fn pcm(samples: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * channels * 2);
    for i in 0..samples {
        for c in 0..channels {
            let t = i as f64 / 48_000.0;
            let tone = (std::f64::consts::TAU * (440.0 + 220.0 * c as f64) * t).sin();
            let s = (tone * 9000.0 + (i % 480) as f64 * 8.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

fn libopus_packets(cfg: EncoderConfig, frames: usize) -> Vec<Vec<u8>> {
    let mut enc = LibopusEncoder::new(cfg).expect("libopus encoder");
    let frame = enc.frame_bytes();
    let pcm = pcm(frames * frame / (2 * cfg.channels as usize), cfg.channels as usize);
    pcm.chunks_exact(frame)
        .map(|f| {
            let mut packet = Vec::new();
            enc.encode(f, &mut packet).expect("encode");
            packet
        })
        .collect()
}

fn opus_head(channels: u8, pre_skip: u16, gain: i16, family: u8) -> Vec<u8> {
    let mut out = b"OpusHead".to_vec();
    out.extend_from_slice(&[1, channels]);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&gain.to_le_bytes());
    out.push(family);
    if family != 0 {
        let coupled = channels / 2;
        out.extend_from_slice(&[channels - coupled, coupled]);
        out.extend(0..channels);
    }
    out
}

const OPUS_TAGS: &[u8] = b"OpusTags\x07\0\0\0pf-opus\0\0\0\0";

struct Fixture {
    name: &'static str,
    bytes: Vec<u8>,
    fields: Vec<usize>,
}

fn inputs(fixtures: &[Fixture]) -> Vec<Input<'_>> {
    fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &f.fields }).collect()
}

/// `[u16 length][packet]` records, so corrupting a length moves the packet boundaries.
fn fixture(name: &'static str, head: Option<Vec<u8>>, packets: &[Vec<u8>]) -> Fixture {
    let (mut bytes, mut fields) = (Vec::new(), Vec::new());
    let mut record = |packet: &[u8], inner: &[usize]| {
        fields.extend([bytes.len(), bytes.len() + 2]);
        fields.extend(inner.iter().map(|i| bytes.len() + 2 + i));
        bytes.extend_from_slice(&(packet.len() as u16).to_be_bytes());
        bytes.extend_from_slice(packet);
    };
    if let Some(head) = &head {
        record(head, &[9, 10, 16, 18]);
        record(OPUS_TAGS, &[]);
    }
    for packet in packets {
        record(packet, &[]);
    }
    fields.retain(|&f| f < bytes.len());
    Fixture { name, bytes, fields }
}

fn records(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while rest.len() >= 2 {
        let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
        let (packet, tail) = rest[2..].split_at(len.min(rest.len() - 2));
        out.push(packet);
        rest = tail;
    }
    out
}

fn packet_fixtures() -> Vec<Fixture> {
    let cfg = |channels, application, bitrate_bps, frame_ms_tenths, signal| EncoderConfig {
        application,
        bitrate_bps,
        frame_ms_tenths,
        signal,
        ..EncoderConfig::new(channels)
    };
    let celt = libopus_packets(cfg(2, Application::Audio, 96_000, 200, Signal::Music), 2);
    let silk = libopus_packets(cfg(1, Application::Voip, 12_000, 200, Signal::Voice), 2);
    let hybrid = libopus_packets(cfg(2, Application::Voip, 40_000, 200, Signal::Voice), 2);
    let short = libopus_packets(cfg(2, Application::LowDelay, 128_000, 25, Signal::Music), 4);
    let long = libopus_packets(cfg(1, Application::Voip, 16_000, 600, Signal::Voice), 1);
    let celt_mono = libopus_packets(cfg(1, Application::Audio, 64_000, 200, Signal::Music), 2);
    vec![
        fixture("celt_stereo_headed", Some(opus_head(2, 312, 0x0180, 0)), &celt),
        fixture("silk_mono_headed", Some(opus_head(1, 312, 0, 0)), &silk),
        fixture("hybrid_stereo", None, &hybrid),
        fixture("celt_2_5_ms", None, &short),
        fixture("silk_60_ms_headed", Some(opus_head(2, 0, -0x0400, 0)), &long),
        fixture("celt_mono", None, &celt_mono),
        fixture("surround_head", Some(opus_head(6, 312, 0, 1)), &celt),
        surround_5_1(),
    ]
}

fn surround_5_1() -> Fixture {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sines_5_1.mka");
    let file = std::fs::read(path).expect("the 5.1 fixture");
    let mut reader = pf_mkv::MatroskaReader::new();
    reader.push(&file).expect("the fixture parses");
    let head = reader.tracks()[0].codec_private.clone();
    let packets: Vec<Vec<u8>> =
        std::iter::from_fn(|| reader.next_frame()).take(4).map(|f| f.data).collect();
    assert_eq!(packets.len(), 4);
    fixture("surround_5_1", Some(head), &packets)
}

const TOKENS: &[&[u8]] = &[
    b"OpusHead",
    b"OpusTags",
    &[b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', 1, 2, 0x38, 1, 0x80, 0xBB, 0, 0, 0, 0, 0],
    &[b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', 1, 255, 0, 0, 0, 0, 0, 0, 0, 0, 255, 1, 0],
    &[0x83, 0x30],
    &[0xFB, 0x3F],
    &[0xFF, 0xF0, 0xFF, 0xFF, 0xFF],
    &[0x03, 0x80 | 2, 0xFF, 0xFF],
    &[0x00],
    &[0x01],
    &[0x02, 0xFF, 0xFF],
    &[0x00, 0x00],
    &[0x00, 0x02, 0x83, 0x30],
    &[0xFF; 16],
];

fn announced(h: &Harness) -> Option<(i64, i64)> {
    let format = h.announced()?;
    let int = |name| match format.get(h.vocabulary().field_id(name)?) {
        Some(Value::Int(v)) => Some(v),
        _ => None,
    };
    Some((int("rate")?, int("channels")?))
}

fn decode_pieces(pieces: &[&[u8]], flush_after: Option<usize>) -> Fed {
    let mut h = Harness::new(OpusDec::new());
    let seed = pieces.last().copied().unwrap_or_default();
    let span = Timestamp::from_millis(pieces.len() as u64 * 20);
    let stamped = seed.len() % 2 == 1;
    let stamp = |index: usize, buffer: &mut profluens_core::buffer::Buffer| {
        if stamped {
            buffer.pts = Timestamp::from_millis(20 * index as u64);
        }
    };
    let driven = pointed::drive_packets(&mut h, seed, pieces, flush_after, span, &stamp);
    match announced(&h) {
        Some((rate, channels)) => {
            assert!(
                rate == 48_000 && (1..=8).contains(&channels),
                "announced {channels} ch at {rate}"
            );
            let ch = channels as usize;
            assert_eq!(driven.out_bytes % (2 * ch), 0, "a split sample frame");
            let packet = MAX_PACKET_SAMPLES * ch * 2;
            let most = pieces.len() * packet;
            assert!(
                driven.out_bytes <= most,
                "{} PCM bytes from {} packets",
                driven.out_bytes,
                pieces.len()
            );
            let buffers = pieces.len() * packet.div_ceil(64 * 1024);
            assert!(driven.out_buffers <= buffers, "{} buffers", driven.out_buffers);
        }
        None => assert_eq!(driven.out_bytes, 0, "PCM without an announced format"),
    }
    driven
}

fn opusdec_target(bytes: &[u8], chunking: Chunking) {
    let flush = (bytes.len() % 2 == 1).then_some(2 + bytes.len() % 3);
    let body;
    let mut pieces = records(bytes);
    if chunking != Chunking::Whole && !pieces.is_empty() {
        body = pieces[1..].concat();
        pieces.truncate(1);
        pieces.extend(chunking.pieces(&body));
    }
    pieces.truncate(MAX_CASE_PACKETS);
    decode_pieces(&pieces, flush);
}

fn libopus_decoder_target(bytes: &[u8], _: Chunking) {
    let mut dec = LibopusDecoder::new();
    dec.set_channels([0, 1, 2, 9][bytes.len() % 4]);
    let mut pcm = Vec::new();
    for (i, packet) in records(bytes).into_iter().take(MAX_CASE_PACKETS).enumerate() {
        if i % 5 == 4 {
            dec.reset();
        }
        if let Ok(channels) = dec.decode_packet_into(packet, &mut pcm) {
            assert!((1..=2).contains(&channels), "{channels} channels");
            assert!(pcm.len() <= MAX_PACKET_SAMPLES * channels as usize, "{} samples", pcm.len());
            assert_eq!(pcm.len() % channels as usize, 0);
            assert!(!packet.is_empty(), "an empty packet decoded to {} samples", pcm.len());
        }
    }
}

fn encoder_harness(channels: i64) -> Harness {
    let mut h = Harness::new(OpusEnc::new());
    h.fix_sink_format(
        OpusEnc::IN,
        "audio/raw",
        &[
            ("rate", ValueDesc::Int(48_000)),
            ("channels", ValueDesc::Int(channels)),
            ("sample", ValueDesc::Id("s16")),
        ],
    );
    h
}

fn opusenc_target(bytes: &[u8], chunking: Chunking) {
    let channels = 1 + bytes.len() % 2;
    let flush = (bytes.len() % 3 == 1).then_some(1 + bytes.len() % 5);
    let mut h = encoder_harness(channels as i64);
    let driven = drive(&mut h, bytes, chunking, flush);
    assert!(!driven.failed, "opusenc failed on PCM");
    // The partial tail and one frame of silence that flushes the lookahead.
    let frames = bytes.len() / (960 * channels * 2) + 2;
    assert!(driven.out_buffers <= frames, "{} packets from {frames} frames", driven.out_buffers);
    assert!(driven.out_bytes <= frames * max_packet_bytes(1), "{} packet bytes", driven.out_bytes);
}

fn encoder_wrappers_target(bytes: &[u8], _: Chunking) {
    let channels = 1 + (bytes.len() % 2) as u8;
    let mut packet = Vec::new();

    let tenths = [25, 50, 100, 200, 400, 600][bytes.len() % 6];
    let cfg = EncoderConfig { frame_ms_tenths: tenths, ..EncoderConfig::new(channels) };
    let mut reference = LibopusEncoder::new(cfg).expect("a legal config");
    let frame = reference.frame_bytes();
    for (i, f) in bytes[bytes.len().min(bytes.len() % 3)..].chunks(frame).enumerate() {
        if i % 4 == 3 {
            reference.reset();
        }
        if reference.encode(f, &mut packet).is_ok() {
            // 40 and 60 ms go out as two and three 20 ms frames in one packet.
            let most = max_packet_bytes(tenths.div_ceil(200) as usize);
            assert!(f.len() == frame && (1..=most).contains(&packet.len()), "{}", packet.len());
        }
    }
}

const DECODER_HEAP: usize = MIB;

/// Per input byte the piece list and a bus warning per dropped packet nobody drains.
const DECODER_HEAP_PER_BYTE: usize = 32 + 256;

#[test]
fn opusdec_element_survives_corruption() {
    let fixtures = packet_fixtures();
    let gate = Corpus {
        name: "opusdec_element",
        inputs: &inputs(&fixtures),
        tokens: TOKENS,
        iters: iterations(200),
        sweep: 8,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(DECODER_HEAP, DECODER_HEAP_PER_BYTE, opusdec_target));
}

#[test]
fn libopus_decoder_wrapper_survives_corruption() {
    let fixtures = packet_fixtures();
    let gate = Corpus {
        name: "opus_libopus_decoder",
        inputs: &inputs(&fixtures),
        tokens: TOKENS,
        iters: iterations(800),
        sweep: 8,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(256 * KIB, 16, libopus_decoder_target));
}

#[test]
fn opusenc_element_survives_any_pcm() {
    let pcm = pcm(2 * 960 + 333, 2);
    let gate = Corpus {
        name: "opusenc_element",
        inputs: &[Input { name: "stereo_pcm", bytes: &pcm, fields: &[] }],
        tokens: &[],
        iters: iterations(30),
        sweep: 8,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(MIB, 4, opusenc_target));
}

#[test]
fn encoder_wrappers_survive_any_pcm() {
    let pcm = pcm(960 + 77, 2);
    let gate = Corpus {
        name: "opus_encoder_wrappers",
        inputs: &[Input { name: "stereo_pcm", bytes: &pcm, fields: &[] }],
        tokens: &[],
        iters: iterations(16),
        sweep: 2,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(MIB, 4, encoder_wrappers_target));
}

#[test]
fn pure_noise_never_decodes_to_more_than_it_holds() {
    let gate = Corpus {
        name: "opus_noise",
        inputs: &[],
        tokens: TOKENS,
        iters: iterations(60),
        sweep: 0,
        tolerate: &[],
    };
    let bounded = heap_bounded(DECODER_HEAP, DECODER_HEAP_PER_BYTE, |bytes, chunking| {
        opusdec_target(bytes, chunking);
        libopus_decoder_target(bytes, chunking);
        let pieces: Vec<&[u8]> = chunking.pieces(bytes).take(MAX_CASE_PACKETS).collect();
        decode_pieces(&pieces, None);
    });
    corpus_gate(&gate, &bounded);
}

// Regression cases, each the input that broke the crate before its fix.

fn running_decoder() -> Harness {
    let mut h = Harness::new(OpusDec::new());
    let packet = &libopus_packets(EncoderConfig::new(2), 1)[0];
    let buf = h.alloc(packet);
    h.push(buf).expect("push");
    assert_eq!(h.drain_outputs().len(), 1);
    assert_eq!(announced(&h), Some((48_000, 2)));
    h
}

#[test]
fn an_empty_packet_is_refused_not_concealed() {
    // RFC 6716 section 3.4 R1. It reached libopus as a loss, 120 ms of concealment.
    let mut dec = LibopusDecoder::new();
    dec.set_channels(2);
    let mut pcm = Vec::new();
    assert_eq!(dec.decode_packet_into(&[], &mut pcm), Err(pf_opus::CodecError::EmptyPacket));
    let packet = &libopus_packets(EncoderConfig::new(2), 1)[0];
    assert_eq!(dec.decode_packet_into(packet, &mut pcm), Ok(2));
    assert_eq!(pcm.len(), 960 * 2);
    assert_eq!(dec.decode_packet_into(&[], &mut pcm), Err(pf_opus::CodecError::EmptyPacket));

    let mut h = running_decoder();
    let buf = h.alloc(&[]);
    h.push(buf).expect("a dropped packet is a warning");
    assert_eq!(h.drain_outputs().len(), 0, "an empty buffer decoded to audio");
}

#[test]
fn an_encoder_setting_libopus_refuses_is_an_error() {
    // The refused ctl status was dropped, so the config was silently not applied.
    let refused = LibopusEncoder::new(EncoderConfig { bitrate_bps: 0, ..EncoderConfig::new(2) });
    let expected = pf_opus::CodecError::Libopus { call: "opus_encoder_ctl", code: -1 };
    assert_eq!(refused.err(), Some(expected));
    // `u32::MAX` used to wrap to -1, `OPUS_BITRATE_MAX`.
    let cfg = EncoderConfig { bitrate_bps: u32::MAX, ..EncoderConfig::new(2) };
    let mut enc = LibopusEncoder::new(cfg).expect("a clamped bitrate");
    let mut packet = Vec::new();
    enc.encode(&pcm(960, 2), &mut packet).expect("encode");
    assert!((1..=max_packet_bytes(1)).contains(&packet.len()));
}

#[test]
fn a_family_1_head_is_announced_and_family_255_refused() {
    // 5.1 used to decode as one stream announced as 6 channels over stereo PCM.
    let mut head = [
        0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, 0x01, 0x06, 0x38, 0x01, 0x80, 0xbb, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x04, 0x02, 0x00, 0x04, 0x01, 0x02, 0x03, 0x05,
    ];
    let mut h = Harness::new(OpusDec::new());
    let buf = h.alloc(&head);
    h.push(buf).expect("a family 1 head");
    assert_eq!(announced(&h), Some((48_000, 6)));
    head[18] = 255;
    let mut h = Harness::new(OpusDec::new());
    let buf = h.alloc(&head);
    let refused = h.push(buf).expect_err("a family 255 stream must fail");
    assert!(format!("{refused}").contains("mapping family 255"), "{refused}");
    assert_eq!(announced(&h), None);
}

#[test]
fn a_second_head_cannot_move_the_decoder_off_the_announced_width() {
    // The second head used to re-aim the decoder off the announced width.
    let mut h = Harness::new(OpusDec::new());
    for head in [opus_head(2, 0, 0, 0), opus_head(1, 0, 0, 0)] {
        let buf = h.alloc(&head);
        h.push(buf).expect("push");
    }
    assert_eq!(announced(&h), Some((48_000, 2)));
    let packet = &libopus_packets(EncoderConfig::new(2), 1)[0];
    let buf = h.alloc(packet);
    h.push(buf).expect("push");
    let pcm: usize = h.drain_outputs().iter().map(|b| b.memory.len()).sum();
    assert_eq!(pcm, 960 * 2 * 2, "20 ms of stereo");
}

#[test]
fn pcm_without_an_input_format_fails_instead_of_piling_up() {
    // As over a `bytes` link. Every buffer used to be kept forever.
    let mut h = Harness::new(OpusEnc::new());
    let second = [0u8; 48_000];
    let (pushed, grew) = heap_during(|| {
        (0..64)
            .take_while(|_| {
                let buf = h.alloc(&second);
                h.push(buf).is_ok()
            })
            .count()
    });
    assert!(pushed <= 4, "still buffering after {pushed} quarter seconds of stereo");
    assert!(grew < MIB, "buffered {grew} bytes without a format");
}
