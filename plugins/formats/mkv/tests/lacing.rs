//! Xiph, EBML and fixed-size lacing (RFC 9559 section 10.3) on hand-built blocks.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_mkv::MatroskaReader;
use pf_mkv::ebml::{self, id};

fn build_stream(block_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();

    ebml::write_id(&mut out, id::EBML);
    let at = ebml::reserve_size(&mut out);
    let body = out.len();
    ebml::write_uint(&mut out, id::EBML_VERSION, 1);
    ebml::write_string(&mut out, id::DOC_TYPE, "matroska");
    let len = (out.len() - body) as u64;
    ebml::patch_size(&mut out, at, len);

    ebml::write_id(&mut out, id::SEGMENT);
    ebml::write_unknown_size(&mut out);

    ebml::write_id(&mut out, id::INFO);
    let at = ebml::reserve_size(&mut out);
    let body = out.len();
    ebml::write_uint(&mut out, id::TIMESTAMP_SCALE, 1_000_000);
    let len = (out.len() - body) as u64;
    ebml::patch_size(&mut out, at, len);

    ebml::write_id(&mut out, id::TRACKS);
    let at = ebml::reserve_size(&mut out);
    let body = out.len();
    ebml::write_id(&mut out, id::TRACK_ENTRY);
    let te_at = ebml::reserve_size(&mut out);
    let te_body = out.len();
    ebml::write_uint(&mut out, id::TRACK_NUMBER, 1);
    ebml::write_uint(&mut out, id::TRACK_TYPE, 2); // audio
    ebml::write_string(&mut out, id::CODEC_ID, "A_FLAC");
    let te_len = (out.len() - te_body) as u64;
    ebml::patch_size(&mut out, te_at, te_len);
    let len = (out.len() - body) as u64;
    ebml::patch_size(&mut out, at, len);

    ebml::write_id(&mut out, id::CLUSTER);
    ebml::write_unknown_size(&mut out);
    ebml::write_uint(&mut out, id::TIMESTAMP, 0);
    out.extend_from_slice(block_bytes);

    out
}

fn simple_block(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    ebml::write_id(&mut out, id::SIMPLE_BLOCK);
    ebml::write_size(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    out
}

fn block_prefix(flags: u8) -> Vec<u8> {
    let mut b = vec![0x81]; // track number 1 as a VINT
    b.extend_from_slice(&0i16.to_be_bytes()); // relative timestamp 0
    b.push(flags);
    b
}

fn decode_frames(stream: &[u8]) -> Vec<Vec<u8>> {
    let mut r = MatroskaReader::new();
    r.push(stream).expect("valid stream parses");
    let mut got = Vec::new();
    while let Some(f) = r.next_frame() {
        assert_eq!(f.track_number, 1);
        got.push(f.data);
    }
    got
}

fn example_frames() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (vec![0xA1u8; 800], vec![0xB2u8; 500], vec![0xC3u8; 1000])
}

#[test]
fn xiph_lacing_splits_three_frames() {
    let (f1, f2, f3) = example_frames();
    let mut body = block_prefix(0x02);
    body.push(2); // number of frames minus 1
    body.extend_from_slice(&[0xFF, 0xFF, 0xFF, 35]); // 800
    body.extend_from_slice(&[0xFF, 245]); // 500
    body.extend_from_slice(&f1);
    body.extend_from_slice(&f2);
    body.extend_from_slice(&f3);

    let stream = build_stream(&simple_block(&body));
    let got = decode_frames(&stream);
    assert_eq!(got, vec![f1, f2, f3], "Xiph lace splits into the three frames, in order");
}

#[test]
fn ebml_lacing_splits_three_frames() {
    let (f1, f2, f3) = example_frames();
    let mut body = block_prefix(0x06);
    body.push(2); // number of frames minus 1
    body.extend_from_slice(&[0x43, 0x20]); // 800 = 0x320 | 0x4000 (2-octet VINT)
    body.extend_from_slice(&[0x5E, 0xD3]); // -300 signed: 0x1FFF - 0x12C = 0x1ED3 | 0x4000
    body.extend_from_slice(&f1);
    body.extend_from_slice(&f2);
    body.extend_from_slice(&f3);

    let stream = build_stream(&simple_block(&body));
    let got = decode_frames(&stream);
    assert_eq!(got, vec![f1, f2, f3], "EBML lace splits into the three frames, in order");
}

#[test]
fn fixed_lacing_splits_equal_frames() {
    let f1 = vec![0xA1u8; 800];
    let f2 = vec![0xB2u8; 800];
    let f3 = vec![0xC3u8; 800];
    let mut body = block_prefix(0x04);
    body.push(2); // number of frames minus 1 (3 frames)
    body.extend_from_slice(&f1);
    body.extend_from_slice(&f2);
    body.extend_from_slice(&f3);

    let stream = build_stream(&simple_block(&body));
    let got = decode_frames(&stream);
    assert_eq!(got, vec![f1, f2, f3], "fixed lace splits into equal-size frames");
}

#[test]
fn no_lacing_is_one_frame() {
    let frame = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
    let mut body = block_prefix(0x00);
    body.extend_from_slice(&frame);
    let stream = build_stream(&simple_block(&body));
    let got = decode_frames(&stream);
    assert_eq!(got, vec![frame], "no-lacing block is exactly one frame");
}

#[test]
fn laced_frames_past_the_first_are_unstamped() {
    let mut body = block_prefix(0x04); // fixed lacing
    body.push(2);
    let (f1, f2, f3) = (vec![1u8; 300], vec![2u8; 300], vec![3u8; 300]);
    body.extend_from_slice(&f1);
    body.extend_from_slice(&f2);
    body.extend_from_slice(&f3);

    let stream = build_stream(&simple_block(&body));
    let mut r = MatroskaReader::new();
    r.push(&stream).unwrap();
    let mut ptss = Vec::new();
    while let Some(f) = r.next_frame() {
        ptss.push(f.pts_ns);
    }
    // Undetermined past the first with no DefaultDuration (RFC 9559 section 10.3.5).
    let none = pf_mkv::PTS_NONE;
    assert_eq!(ptss, vec![0, none, none], "the block pts on the first laced frame alone");
}
