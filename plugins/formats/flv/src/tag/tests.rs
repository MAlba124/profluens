// Test bodies.
#![allow(clippy::disallowed_methods)]

use super::*;

fn tag(out: &mut Vec<u8>, kind: u8, ts: u32, body: &[u8]) -> u64 {
    let pos = out.len() as u64;
    let size = body.len() as u32;
    out.push(kind);
    out.extend_from_slice(&size.to_be_bytes()[1..]);
    out.extend_from_slice(&(ts & 0xFF_FFFF).to_be_bytes()[1..]);
    out.push((ts >> 24) as u8);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(body);
    out.extend_from_slice(&(size + 11).to_be_bytes());
    pos
}

fn file() -> (Vec<u8>, Vec<u64>) {
    let mut f = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
    let mut at = vec![tag(&mut f, SCRIPT, 0, b"\x02\x00\x0aonMetaData\x05")];
    for n in 0..20u32 {
        let video = [0x17 + if n % 5 == 0 { 0 } else { 0x10 }, 1, 0, 0, 0, n as u8, 0xAA];
        at.push(tag(&mut f, VIDEO, n * 40, &video));
        at.push(tag(&mut f, AUDIO, n * 40 + 1, &[0xAF, 1, n as u8, 0x55, 0x66]));
    }
    at.push(tag(&mut f, AUDIO, 0x0100_0002, &[0xAF, 1, 9]));
    (f, at)
}

type Seen = Vec<(u64, u8, u32, Vec<u8>)>;

fn read(data: &[u8], piece: usize, from: u64, expect: Expect) -> (Seen, Stats) {
    let mut r = Reader::new();
    r.expect = expect;
    r.restart(from);
    let mut seen = Vec::new();
    let mut sink = |t: Tag<'_>| seen.push((t.pos, t.header.kind, t.header.ts_ms, t.body.to_vec()));
    for chunk in data.chunks(piece.max(1)) {
        r.push(chunk, &mut sink);
    }
    r.finish(&mut sink);
    (seen, r.stats)
}

#[test]
fn every_tag_is_read_whatever_the_pieces() {
    let (f, at) = file();
    let (whole, stats) = read(&f, f.len(), 0, Expect::default());
    assert_eq!(whole.iter().map(|t| t.0).collect::<Vec<_>>(), at);
    assert_eq!((stats.resyncs, stats.skipped, stats.loose), (0, 0, 0));
    assert_eq!(whole.last().map(|t| t.2), Some(0x0100_0002));
    for piece in [1, 2, 7, 11, 15, 64, 300] {
        assert_eq!(read(&f, piece, 0, Expect::default()).0, whole, "pieces of {piece}");
    }
}

#[test]
fn a_header_is_read_as_a_header() {
    let h = file_header(b"FLV\x01\x05\x00\x00\x00\x09").unwrap();
    assert_eq!((h.version, h.audio, h.video, h.data_offset), (1, true, true, 9));
    assert!(file_header(b"FLV\x01\x05\x00\x00\x00").is_none());
    assert!(file_header(b"FLX\x01\x05\x00\x00\x00\x09").is_none());
}

#[test]
fn junk_between_tags_is_stepped_over() {
    let (f, at) = file();
    let (whole, _) = read(&f, f.len(), 0, Expect::default());
    let cut = at[7] as usize;
    let mut damaged = f[..cut].to_vec();
    damaged.extend_from_slice(&[0x09, 0xFF, 0x00, 0x00, 0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x17]);
    damaged.extend_from_slice(&[0xEE; 300]);
    damaged.extend_from_slice(&f[cut..]);
    let expect = Expect { audio: Some(10), video: Some(7) };
    for piece in [1, 9, 64, damaged.len()] {
        let (got, stats) = read(&damaged, piece, 0, expect);
        let tail: Vec<_> = got.iter().map(|t| (t.1, t.2, t.3.clone())).collect();
        let want: Vec<_> = whole.iter().map(|t| (t.1, t.2, t.3.clone())).collect();
        assert_eq!(tail, want, "pieces of {piece}");
        assert_eq!(stats.resyncs, 1);
        assert_eq!(stats.skipped, 311);
    }
}

#[test]
fn a_broken_size_loses_one_tag_and_no_more() {
    let (f, at) = file();
    let mut damaged = f.clone();
    // Tag 9's size points into the middle of tag 10.
    damaged[at[9] as usize + 3] = 0x30;
    let (got, stats) = read(&damaged, 4096, 0, Expect { audio: Some(10), video: Some(7) });
    let lost: Vec<u64> = at.iter().copied().filter(|p| !got.iter().any(|t| t.0 == *p)).collect();
    assert_eq!(lost, [at[9]]);
    assert_eq!(stats.resyncs, 1);
}

#[test]
fn back_pointers_of_zero_are_taken_on_the_next_header() {
    let (mut f, at) = file();
    for (n, p) in at.iter().enumerate() {
        let next = at.get(n + 1).map_or(f.len(), |n| *n as usize);
        f[next - 4..next].copy_from_slice(&[0, 0, 0, 0]);
        let _ = p;
    }
    let (got, stats) = read(&f, 100, 0, Expect::default());
    assert_eq!(got.len(), at.len());
    assert_eq!(stats.loose, at.len() as u64);
    // Resync needs two chained headers then.
    let (behind, _) = read(&f[at[5] as usize + 3..], 100, at[5] + 3, Expect::default());
    assert_eq!(behind.first().map(|t| t.0), Some(at[6]));
    assert_eq!(behind.len(), at.len() - 6);
}

#[test]
fn a_seek_lands_on_the_next_whole_tag() {
    let (f, at) = file();
    for from in [at[4], at[4] + 1, at[4] + 12, at[5] - 2] {
        let (got, _) = read(&f[from as usize..], 50, from, Expect::default());
        let first = at.iter().copied().find(|p| *p >= from).unwrap();
        assert_eq!(got.first().map(|t| t.0), Some(first), "from {from}");
        assert_eq!(got.len(), at.iter().filter(|p| **p >= from).count());
    }
}

#[test]
fn a_cut_last_tag_is_dropped() {
    let (f, at) = file();
    let end = at[at.len() - 1] as usize + 13;
    let (got, _) = read(&f[..end], 64, 0, Expect::default());
    assert_eq!(got.len(), at.len() - 1);
    assert!(read(b"", 1, 0, Expect::default()).0.is_empty());
    assert!(read(&f[..20], 3, 0, Expect::default()).0.is_empty());
}

#[test]
fn what_a_tag_body_starts_with_is_checked() {
    let e = Expect { audio: Some(10), video: Some(7) };
    assert!(e.fits(AUDIO, 0xAF) && !e.fits(AUDIO, 0x2F));
    assert!(e.fits(VIDEO, 0x17) && e.fits(VIDEO, 0x27) && !e.fits(VIDEO, 0x12));
    assert!(!e.fits(VIDEO, 0x07) && !e.fits(VIDEO, 0x67), "frame types 1 to 5");
    assert!(!e.fits(VIDEO, 0x91));
    let x = Expect { video: Some(ENHANCED), ..e };
    assert!(x.fits(VIDEO, 0x91) && !x.fits(VIDEO, 0x17));
    assert!(e.fits(SCRIPT, 2) && !e.fits(SCRIPT, 3) && !e.fits(7, 0));
}
