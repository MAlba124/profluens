//! Uncompressed audio sample tables that a hostile or unusual file declares.

// Test fixtures allocate freely.
#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use pf_mp4::Mp4Reader;

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    boxed(kind, &[&[0u8; 4][..], body].concat())
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// A QuickTime `twos` 8-bit mono sound entry, one octet a frame.
fn twos() -> Vec<u8> {
    let mut body = vec![0u8; 8];
    body.extend([0u8; 8]);
    body.extend(1u16.to_be_bytes());
    body.extend(8u16.to_be_bytes());
    body.extend([0u8; 4]);
    body.extend((8000u32 << 16).to_be_bytes());
    boxed(b"twos", &body)
}

/// A sound track whose tables are given raw, over one chunk at `offset`.
fn movie(stsz: &[u8], stts: &[u8], stsc: &[u8], frames: u32, offset: u32) -> Vec<u8> {
    let stbl = [
        full(b"stsd", &[&be32(&[1])[..], &twos()].concat()),
        full(b"stts", stts),
        full(b"stsz", stsz),
        full(b"stsc", stsc),
        full(b"stco", &be32(&[1, offset])),
    ]
    .concat();
    let mut mdhd = vec![0u8; 8];
    mdhd.extend(be32(&[8000, frames, 0]));
    let mut hdlr = vec![0u8; 4];
    hdlr.extend_from_slice(b"soun");
    hdlr.extend([0u8; 13]);
    let mut tkhd = vec![0u8; 8];
    tkhd.extend(be32(&[1, 0, frames]));
    tkhd.extend([0u8; 60]);
    let mdia = [full(b"mdhd", &mdhd), full(b"hdlr", &hdlr), boxed(b"minf", &boxed(b"stbl", &stbl))]
        .concat();
    let trak = boxed(b"trak", &[full(b"tkhd", &tkhd), boxed(b"mdia", &mdia)].concat());
    let mut mvhd = vec![0u8; 8];
    mvhd.extend(be32(&[1000, 0]));
    mvhd.extend([0u8; 80]);
    let moov = boxed(b"moov", &[full(b"mvhd", &mvhd), trak].concat());
    [boxed(b"ftyp", b"isom\0\0\0\0isom"), moov].concat()
}

#[test]
fn a_billion_pcm_frames_resolve_as_their_groups_in_bounded_time() {
    const FRAMES: u32 = 1 << 30;
    let head =
        movie(&be32(&[1, FRAMES]), &be32(&[1, FRAMES, 1]), &be32(&[1, 1, FRAMES, 1]), FRAMES, 4096);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = Mp4Reader::new(&head).expect("resolves");
        let tables = reader.tables();
        let n = tables.samples().len();
        let first = tables.samples()[0];
        let _ = tx.send((n, first.offset, first.size));
    });
    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok((n, offset, size)) => {
            assert_eq!(n, (FRAMES / 1024) as usize, "one group of 1024 frames a sample");
            assert_eq!((offset, size), (4096, 1024));
        }
        Err(_) => panic!("a billion frames walked one by one"),
    }
}

#[test]
fn a_pcm_track_with_a_per_sample_size_table_resolves() {
    let head = movie(&be32(&[0, 4, 2, 2, 2, 2]), &be32(&[1, 4, 2]), &be32(&[1, 1, 4, 1]), 4, 4096);
    let reader = Mp4Reader::new(&head).expect("a per-sample table folds one sample an entry");
    let tables = reader.tables();
    let sizes: Vec<_> = tables.samples().iter().map(|s| (s.offset, s.size)).collect();
    assert_eq!(sizes, [(4096, 2), (4098, 2), (4100, 2), (4102, 2)]);
    assert_eq!(tables.tracks()[0].entry.pcm.map(|p| p.frame), Some(1));
}
