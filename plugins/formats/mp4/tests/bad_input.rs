//! Malformed and truncated MP4 input never panics, at resolution or while streaming.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_mp4::Mp4Reader;

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn ftyp() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"isom"); // major_brand
    body.extend_from_slice(&0u32.to_be_bytes()); // minor_version
    body.extend_from_slice(b"isom"); // compatible_brands[0]
    boxed(b"ftyp", &body)
}

fn resolve_no_panic(head: &[u8]) {
    let _ = Mp4Reader::new(head);
}

fn stream_no_panic(head: &[u8], tail: &[u8]) {
    let Ok(mut reader) = Mp4Reader::new(head) else {
        return; // head did not resolve, nothing to stream
    };
    reader.reset_stream();
    let mut file = head.to_vec();
    file.extend_from_slice(tail);
    // 1-byte chunks make nearly every sample straddle a chunk boundary.
    for b in &file {
        reader.push_bytes(std::slice::from_ref(b));
        while reader.next_sample().is_some() {}
    }
}

#[test]
fn malformed_heads_never_panic() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("single byte", vec![0x00]),
        ("all zeros", vec![0u8; 64]),
        ("all 0xFF", vec![0xFF; 64]),
        ("random-ish bytes", (0u8..128).map(|b| b.wrapping_mul(37)).collect()),
        ("ftyp only (no moov)", ftyp()),
        ("a box claiming a huge size but no data", {
            let mut v = 0x7FFF_FFFFu32.to_be_bytes().to_vec();
            v.extend_from_slice(b"moov");
            v
        }),
        ("largesize box claiming ~2^40 bytes, none present", {
            let mut v = 1u32.to_be_bytes().to_vec(); // size == 1, largesize follows
            v.extend_from_slice(b"moov");
            v.extend_from_slice(&(1u64 << 40).to_be_bytes());
            v
        }),
        ("box size smaller than its own header", {
            let mut v = 4u32.to_be_bytes().to_vec(); // size 4 < 8-byte header
            v.extend_from_slice(b"moov");
            v
        }),
        ("empty moov (no trak)", boxed(b"moov", &[])),
        ("moov with a truncated trak", {
            let mut moov = Vec::new();
            let mut trak = 200u32.to_be_bytes().to_vec();
            trak.extend_from_slice(b"trak");
            trak.extend_from_slice(&[0u8; 4]); // only 4 body bytes, not 192
            moov.extend_from_slice(&trak);
            boxed(b"moov", &moov)
        }),
        ("moov/trak/mdia/minf/stbl with a truncated stsz claiming a huge table", {
            let mut stsz = vec![0u8; 4]; // version+flags
            stsz.extend_from_slice(&0u32.to_be_bytes());
            stsz.extend_from_slice(&u32::MAX.to_be_bytes());
            let stbl = boxed(b"stbl", &boxed(b"stsz", &stsz));
            let minf = boxed(b"minf", &stbl);
            let mut mdia = boxed(b"mdhd", &{
                let mut m = vec![0u8; 4];
                m.extend_from_slice(&0u64.to_be_bytes()); // times
                m.extend_from_slice(&1000u32.to_be_bytes()); // timescale
                m.extend_from_slice(&0u32.to_be_bytes()); // duration
                m.extend_from_slice(&[0u8; 4]); // language + pre_defined
                m
            });
            mdia.extend_from_slice(&minf);
            let mdia = boxed(b"mdia", &mdia);
            let trak = boxed(b"trak", &mdia);
            boxed(b"moov", &trak)
        }),
        ("fragmented movie (moov with mvex)", {
            let mvex = boxed(b"mvex", &[]);
            boxed(b"moov", &mvex)
        }),
    ];

    for (name, head) in &cases {
        resolve_no_panic(head);
        for n in 0..head.len() {
            resolve_no_panic(&head[..n]);
        }
        eprintln!("ok (resolve): {name}");
    }
}

#[test]
fn a_fragmented_movie_with_no_track_is_refused_for_that() {
    let mvex = boxed(b"mvex", &[]);
    let moov = boxed(b"moov", &mvex);
    let mut head = ftyp();
    head.extend_from_slice(&moov);
    assert!(matches!(Mp4Reader::new(&head), Err(pf_mp4::Mp4Error::Missing(_))));
}

#[test]
fn streaming_over_bad_mdat_never_panics() {
    let file = fixture_bytes("tiny_h264.mp4");
    let head = head_through_moov(&file);

    assert!(Mp4Reader::new(&head).is_ok(), "the fixture head resolves");

    let tails: Vec<(&str, Vec<u8>)> = vec![
        ("no mdat at all", Vec::new()),
        ("a few garbage bytes", vec![0xAB; 16]),
        (
            "half the real mdat",
            file[head.len()..head.len() + (file.len() - head.len()) / 2].to_vec(),
        ),
        ("the real mdat (full)", file[head.len()..].to_vec()),
        ("all 0xFF where samples should be", vec![0xFF; 4096]),
    ];

    for (name, tail) in &tails {
        stream_no_panic(&head, tail);
        for n in 0..tail.len().min(300) {
            stream_no_panic(&head, &tail[..n]);
        }
        eprintln!("ok (stream): {name}");
    }
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

fn head_through_moov(file: &[u8]) -> Vec<u8> {
    let mut at = 0usize;
    while at + 8 <= file.len() {
        let size =
            u32::from_be_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]) as usize;
        if &file[at + 4..at + 8] == b"mdat" {
            return file[..at].to_vec();
        }
        let advance = if size == 0 {
            file.len() - at
        } else if size == 1 {
            u64::from_be_bytes(file[at + 8..at + 16].try_into().unwrap()) as usize
        } else {
            size
        };
        if advance == 0 {
            break;
        }
        at += advance;
    }
    file.to_vec()
}

/// Where the `nth` box of `kind` starts, its size field.
fn box_at(file: &[u8], kind: &[u8; 4], nth: usize) -> usize {
    let found = file.windows(4).enumerate().filter(|(_, w)| w == kind).nth(nth);
    found.map(|(at, _)| at - 4).unwrap_or_else(|| panic!("no {nth}th {kind:?}"))
}

#[test]
fn a_negative_version_0_composition_offset_is_read_signed() {
    use pf_mp4::reader::{Limits, Tables};
    let mut file = fixture_bytes("bframes_h264.mp4");
    let ctts = box_at(&file, b"ctts", 0);
    assert_eq!(file[ctts + 8], 0, "the fixture writes version 0");
    // the first run's sample_offset, after the box and full box heads, entry_count and count
    let at = ctts + 20;
    file[at..at + 4].copy_from_slice(&(-1000i32).to_be_bytes());
    let tables = Tables::resolve(&file, &[], Limits::default()).expect("the movie resolves");
    let first = tables.samples().iter().find(|s| s.track_index == 0).expect("a sample");
    assert_eq!(first.pts - first.dts, -1000, "not 2^32 - 1000");
}

#[test]
fn a_track_whose_sample_entries_do_not_resolve_is_skipped_and_the_rest_open() {
    use pf_mp4::reader::{Limits, Tables};
    let file = fixture_bytes("tiny_av.mp4");
    let whole = Tables::resolve(&file, &[], Limits::default()).expect("the fixture resolves");
    assert_eq!((whole.tracks().len(), whole.skipped_tracks()), (2, 0));
    // the second trak's stsd entry_count to 0
    let mut broken = file.clone();
    let at = box_at(&file, b"stsd", 1) + 12;
    broken[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
    let tables = Tables::resolve(&broken, &[], Limits::default()).expect("the video still opens");
    assert_eq!((tables.tracks().len(), tables.skipped_tracks()), (1, 1));
    assert_eq!(tables.tracks()[0].track_id, whole.tracks()[0].track_id);
    let video = whole.samples().iter().filter(|s| s.track_index == 0).count();
    assert_eq!(tables.samples().len(), video, "the video's samples, all of them");
    // with no track left the first track's own error is said
    let mut none = broken;
    let at = box_at(&file, b"stsd", 0) + 12;
    none[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
    let err = Tables::resolve(&none, &[], Limits::default()).err().map(|e| e.to_string());
    assert!(err.as_deref().is_some_and(|e| e.contains("stsd with no sample entries")), "{err:?}");
}
