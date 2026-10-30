//! Malformed and truncated input never panics, fed whole and byte by byte.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_mkv::MatroskaReader;
use pf_mkv::ebml::{self, id};

fn valid_head() -> Vec<u8> {
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
    ebml::write_uint(&mut out, id::TRACK_TYPE, 2);
    ebml::write_string(&mut out, id::CODEC_ID, "A_FLAC");
    let te_len = (out.len() - te_body) as u64;
    ebml::patch_size(&mut out, te_at, te_len);
    let len = (out.len() - body) as u64;
    ebml::patch_size(&mut out, at, len);

    ebml::write_id(&mut out, id::CLUSTER);
    ebml::write_unknown_size(&mut out);
    ebml::write_uint(&mut out, id::TIMESTAMP, 0);
    out
}

fn feed_no_panic(data: &[u8]) {
    let mut r = MatroskaReader::new();
    let _ = r.push(data);
    while r.next_frame().is_some() {}

    let mut r = MatroskaReader::new();
    for &b in data {
        if r.push(&[b]).is_err() {
            break;
        }
        while r.next_frame().is_some() {}
    }
}

fn with_head(block: &[u8]) -> Vec<u8> {
    let mut s = valid_head();
    s.extend_from_slice(block);
    s
}

fn simple_block(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    ebml::write_id(&mut out, id::SIMPLE_BLOCK);
    ebml::write_size(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    out
}

#[test]
fn malformed_inputs_never_panic() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("single zero byte (invalid VINT first octet)", vec![0x00]),
        ("all 0xFF", vec![0xFF; 32]),
        ("random-ish bytes", (0u8..64).map(|b| b.wrapping_mul(37)).collect()),
        ("a bare EBML id with no size", id::EBML.to_vec()),
        ("an element claiming a huge size but no data", {
            let mut v = id::TRACKS.to_vec();
            // An 8-octet size claiming ~2^40 bytes must stay Incomplete, not allocate.
            v.push(0x01);
            v.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
            v
        }),
        ("TrackEntry with a zero TrackNumber", {
            let mut out = Vec::new();
            ebml::write_id(&mut out, id::EBML);
            let at = ebml::reserve_size(&mut out);
            let b = out.len();
            ebml::write_uint(&mut out, id::EBML_VERSION, 1);
            let l = (out.len() - b) as u64;
            ebml::patch_size(&mut out, at, l);
            ebml::write_id(&mut out, id::SEGMENT);
            ebml::write_unknown_size(&mut out);
            ebml::write_id(&mut out, id::TRACKS);
            let at = ebml::reserve_size(&mut out);
            let b = out.len();
            ebml::write_id(&mut out, id::TRACK_ENTRY);
            let te = ebml::reserve_size(&mut out);
            let teb = out.len();
            ebml::write_uint(&mut out, id::TRACK_NUMBER, 0); // illegal (RFC 9559 section 5.1.4.1.1)
            let tel = (out.len() - teb) as u64;
            ebml::patch_size(&mut out, te, tel);
            let l = (out.len() - b) as u64;
            ebml::patch_size(&mut out, at, l);
            out
        }),
        ("a child claiming to run past its master", {
            let mut out = valid_head();
            ebml::write_id(&mut out, id::SIMPLE_BLOCK);
            out.push(0x01); // 8-octet size...
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00]); // huge
            out.extend_from_slice(&[0x81, 0, 0, 0x80]); // a few real bytes then nothing
            out
        }),
        ("SimpleBlock body empty", with_head(&simple_block(&[]))),
        ("SimpleBlock body one byte (track VINT only)", with_head(&simple_block(&[0x81]))),
        ("SimpleBlock header truncated (no flags byte)", with_head(&simple_block(&[0x81, 0x00]))),
        (
            "Xiph lace: frame count but no sizes",
            with_head(&simple_block(&[0x81, 0, 0, 0x02, 0x03])), // flags 0x02 (Xiph), count-1=3
        ),
        (
            "Xiph lace: size runs off the end",
            with_head(&simple_block(&[0x81, 0, 0, 0x02, 0x01, 0xFF, 0xFF, 0xFF])),
        ),
        (
            "EBML lace: first size VINT truncated",
            with_head(&simple_block(&[0x81, 0, 0, 0x06, 0x01])), // flags 0x06 (EBML), count-1=1
        ),
        ("EBML lace: negative frame size from a bad delta", {
            with_head(&simple_block(&[0x81, 0, 0, 0x06, 0x01, 0x81, 0x80]))
        }),
        (
            "fixed lace: block size not divisible by frame count",
            with_head(&simple_block(&[0x81, 0, 0, 0x04, 0x02, 0xAA, 0xBB, 0xCC, 0xDD])),
        ),
        (
            "laced frame size overflow claim",
            with_head(&simple_block(&[0x81, 0, 0, 0x02, 0x01, 0xFF, 0xFF, 0x01])),
        ),
        ("BlockGroup with a truncated Block", {
            let mut bg = Vec::new();
            ebml::write_id(&mut bg, id::BLOCK_GROUP);
            let block_body = vec![0x81u8, 0, 0]; // track + rel-ts, no flags, truncated
            let mut block = Vec::new();
            ebml::write_id(&mut block, id::BLOCK);
            ebml::write_size(&mut block, block_body.len() as u64);
            block.extend_from_slice(&block_body);
            ebml::write_size(&mut bg, block.len() as u64);
            bg.extend_from_slice(&block);
            with_head(&bg)
        }),
    ];

    for (name, data) in &cases {
        feed_no_panic(data);
        for n in 0..data.len() {
            feed_no_panic(&data[..n]);
        }
        eprintln!("ok: {name}");
    }
}

#[test]
fn zero_track_number_is_a_hard_error() {
    let mut out = Vec::new();
    ebml::write_id(&mut out, id::SEGMENT);
    ebml::write_unknown_size(&mut out);
    ebml::write_id(&mut out, id::TRACKS);
    let at = ebml::reserve_size(&mut out);
    let b = out.len();
    ebml::write_id(&mut out, id::TRACK_ENTRY);
    let te = ebml::reserve_size(&mut out);
    let teb = out.len();
    ebml::write_uint(&mut out, id::TRACK_NUMBER, 0);
    ebml::write_string(&mut out, id::CODEC_ID, "A_FLAC");
    let tel = (out.len() - teb) as u64;
    ebml::patch_size(&mut out, te, tel);
    let l = (out.len() - b) as u64;
    ebml::patch_size(&mut out, at, l);

    let mut r = MatroskaReader::new();
    assert!(r.push(&out).is_err(), "a zero TrackNumber must be a hard error");
}
