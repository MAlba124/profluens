// Tests own their staging Vecs.
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::boxes::read_box_header;
use crate::frag::{parse_tfdt, parse_trun};

const MS: i64 = 1_000_000;

/// Top level boxes of `b` as (type, body).
fn boxes(b: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut at = 0;
    let mut v = Vec::new();
    while at < b.len() {
        let h = read_box_header(b, at).expect("box");
        v.push((h.kind, h.body(b)));
        at = h.end;
    }
    v
}

fn child<'a>(body: &'a [u8], kind: &[u8; 4]) -> &'a [u8] {
    boxes(body).into_iter().find(|(k, _)| k == kind).map(|(_, b)| b).expect("child")
}

/// (tfdt, rows of duration, size, offset, mdat length) of a segment.
fn read(seg: &[u8]) -> (u64, Vec<(u32, u32, i64)>, usize) {
    let top = boxes(seg);
    assert_eq!(&top[0].0, b"styp");
    let moof = top.iter().find(|(k, _)| k == b"moof").expect("moof").1;
    let traf = child(moof, b"traf");
    let tfdt = parse_tfdt(child(traf, b"tfdt")).expect("tfdt");
    let mut trun = parse_trun(child(traf, b"trun")).expect("trun");
    let rows = (0..trun.count)
        .map(|_| trun.row().expect("row"))
        .map(|r| (r.duration.unwrap_or(0), r.size.unwrap_or(0), r.offset))
        .collect();
    let mdat = top.iter().find(|(k, _)| k == b"mdat").expect("mdat").1.len();
    (tfdt, rows, mdat)
}

fn rows(pts: &[i64], dur: i64) -> Rows {
    let mut r = Rows::default();
    for (i, &p) in pts.iter().enumerate() {
        r.push(p, dur, i == 0, |d| d.extend_from_slice(&[i as u8; 10]));
    }
    r
}

#[test]
fn frames_in_order_have_no_offsets_and_end_on_the_next_segment() {
    let r = rows(&[2000 * MS, 2040 * MS, 2080 * MS], 40 * MS);
    let (mut out, mut s) = (Vec::new(), Scratch::default());
    segment(&mut out, 2, 90_000, &r, Some(2200 * MS), &mut s).expect("segment");
    let (tfdt, rows, mdat) = read(&out);
    assert_eq!(tfdt, 180_000);
    assert_eq!(
        rows,
        vec![(3600, 10, 0), (3600, 10, 0), (10_800, 10, 0)],
        "the last lasts to the next start"
    );
    assert_eq!(mdat, 30);
}

#[test]
fn reordered_frames_decode_on_sorted_stamps_with_signed_offsets() {
    // decode order I P B B, presented 0 3 1 2
    let r = rows(&[0, 120 * MS, 40 * MS, 80 * MS], 40 * MS);
    let (mut out, mut s) = (Vec::new(), Scratch::default());
    segment(&mut out, 1, 90_000, &r, None, &mut s).expect("segment");
    let (tfdt, rows, _) = read(&out);
    assert_eq!(tfdt, 0);
    let offsets: Vec<i64> = rows.iter().map(|r| r.2).collect();
    assert_eq!(offsets, vec![0, 7200, -3600, -3600]);
    assert!(rows.iter().all(|r| r.0 == 3600), "{rows:?}");
}

#[test]
fn the_same_samples_make_the_same_bytes() {
    let r = rows(&[4000 * MS, 4040 * MS], 40 * MS);
    let (mut a, mut b, mut s) = (Vec::new(), Vec::new(), Scratch::default());
    segment(&mut a, 3, 90_000, &r, Some(4080 * MS), &mut s).expect("a");
    segment(&mut b, 3, 90_000, &r, Some(4080 * MS), &mut Scratch::default()).expect("b");
    assert_eq!(a, b);
    assert_eq!(segment(&mut a, 1, 90_000, &Rows::default(), None, &mut s), Err(CmafError::Empty));
}

#[test]
fn chunks_after_the_first_carry_no_styp() {
    let r = rows(&[0, 40 * MS], 40 * MS);
    let (mut first, mut next, mut s) = (Vec::new(), Vec::new(), Scratch::default());
    chunk(&mut first, true, 1, 90_000, &r, Some(80 * MS), &mut s).expect("first");
    chunk(&mut next, false, 1, 90_000, &r, Some(80 * MS), &mut s).expect("next");
    assert_eq!(&first[..4], &24u32.to_be_bytes());
    assert_eq!(&first[24..], &next[..], "the same fragment behind the styp");
    assert_eq!(boxes(&next).iter().map(|b| b.0).collect::<Vec<_>>(), vec![*b"moof", *b"mdat"]);
}

#[test]
fn an_init_segment_is_ftyp_then_moov_with_mvex() {
    let mut out = vec![0xEE; 3];
    init(&mut out, &audio_head(Entry::Aac(vec![0x11, 0x90]), 48_000, 2));
    let top = boxes(&out);
    assert_eq!(top.iter().map(|b| b.0).collect::<Vec<_>>(), vec![*b"ftyp", *b"moov"]);
    assert!(boxes(top[1].1).iter().any(|(k, _)| k == b"mvex"));
}
