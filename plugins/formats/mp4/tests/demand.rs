// Test fixtures allocate freely.
#![allow(clippy::disallowed_methods)]

use pf_mp4::Mp4Reader;
use profluens_core::memory::{Demand, Known};

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![version, 0, 0, 0];
    payload.extend_from_slice(body);
    boxed(kind, &payload)
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn trak(id: u32, handler: &[u8; 4], sizes: &[u32], offset: u32) -> Vec<u8> {
    let mut tkhd = vec![0u8; 8];
    tkhd.extend(be32(&[id, 0, 0]));
    tkhd.extend([0u8; 60]);
    let mut hdlr = vec![0u8; 4];
    hdlr.extend_from_slice(handler);
    hdlr.extend([0u8; 13]);
    let mut stsd = be32(&[1]);
    stsd.extend(boxed(b"raw ", &[0u8; 8]));
    let mut stsz = be32(&[0, sizes.len() as u32]);
    stsz.extend(be32(sizes));
    let n = sizes.len() as u32;
    let stbl = [
        full(b"stsd", 0, &stsd),
        full(b"stts", 0, &be32(&[1, n, 1])),
        full(b"stsz", 0, &stsz),
        full(b"stsc", 0, &be32(&[1, 1, n, 1])),
        full(b"stco", 0, &be32(&[1, offset])),
    ]
    .concat();
    let mut mdhd = vec![0u8; 8];
    mdhd.extend(be32(&[1000, n, 0]));
    let mdia =
        [full(b"mdhd", 0, &mdhd), full(b"hdlr", 0, &hdlr), boxed(b"minf", &boxed(b"stbl", &stbl))]
            .concat();
    boxed(b"trak", &[full(b"tkhd", 0, &tkhd), boxed(b"mdia", &mdia)].concat())
}

fn movie(traks: &[Vec<u8>]) -> Vec<u8> {
    let mut mvhd = vec![0u8; 8];
    mvhd.extend(be32(&[1000, 0]));
    mvhd.extend([0u8; 80]);
    let moov = boxed(b"moov", &[vec![full(b"mvhd", 0, &mvhd)], traks.to_vec()].concat().concat());
    [boxed(b"ftyp", b"isom\0\0\0\0isom"), moov].concat()
}

#[test]
fn a_track_says_its_largest_sample_and_its_peak_window() {
    let video = [10_000, 50_000, 20_000, 70_000, 30_000, 5_000];
    let audio = [400, 400, 900, 400];
    let head = movie(&[trak(1, b"vide", &video, 4096), trak(2, b"soun", &audio, 1 << 20)]);
    let tables = Mp4Reader::new(&head).expect("the movie resolves").tables();
    let d = tables.demand(0, 2);
    assert_eq!(d, Demand { known: Known::Samples, largest: 70_000, peak: 100_000, window: 2 });
    assert_eq!(tables.demand(0, 1).peak, 70_000, "a window of one is the largest");
    assert_eq!(tables.demand(0, 100).peak, 185_000, "a window past the track is all of it");
    let a = tables.demand(1, 2);
    assert_eq!((a.largest, a.peak), (900, 1300), "the other track's samples are its own");
    let both = d.and(a);
    assert_eq!((both.known, both.largest, both.peak), (Known::Samples, 70_000, 101_300));
    assert_eq!(Demand::unknown().and(d), d, "nothing known adds nothing");
    assert_eq!(tables.most_within(0, 100_000), 3, "20, 70 and 5 thousand at the end");
    assert_eq!(tables.most_within(0, 69_999), 2, "no 70 thousand");
    assert_eq!(tables.most_within(0, 1), 0);
    assert_eq!(tables.most_within(1, u64::MAX), 4);
}
