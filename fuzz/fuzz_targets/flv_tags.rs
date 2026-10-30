//! The pf-flv tag reader, AMF0 `onMetaData` and tag headers on raw bytes, no element. Input is a
//! knob byte, its low 3 bits the piece size, then bits for the expectation and a restart, then
//! the bytes. Tags must not depend on how the input is cut and must be the file's own bytes.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_flv::codec;
use pf_flv::tag::{self, Expect, Reader, TAG_HEADER, Tag};
use pf_flv::{amf, index};

const PIECES: [usize; 8] = [1, 3, 7, 11, 64, 1000, 4096, usize::MAX];
const EXPECT: [Expect; 4] = [
    Expect { audio: None, video: None },
    Expect { audio: Some(10), video: Some(7) },
    Expect { audio: Some(2), video: Some(tag::ENHANCED) },
    Expect { audio: Some(0), video: Some(12) },
];

#[derive(Debug, PartialEq, Eq)]
struct Seen {
    pos: u64,
    kind: u8,
    ts: u32,
    len: usize,
}

fn read(b: &[u8], piece: usize, from: usize, expect: Expect) -> (Vec<Seen>, tag::Stats) {
    let mut r = Reader::new();
    r.expect = expect;
    r.restart(from as u64);
    let mut seen = Vec::new();
    let mut sink = |t: Tag<'_>| {
        let at = t.pos as usize;
        let size = t.header.size as usize;
        assert_eq!(t.body.len(), size, "a body other than its size");
        assert_eq!(b.get(at + TAG_HEADER..at + TAG_HEADER + size), Some(t.body), "not the file's");
        assert_eq!(tag::TagHeader::parse(&b[at..]), Some(t.header), "a header not at its byte");
        assert!(seen.last().is_none_or(|l: &Seen| l.pos < t.pos), "tags out of order");
        parse(&t, b.len());
        seen.push(Seen { pos: t.pos, kind: t.header.kind, ts: t.header.ts_ms, len: size });
    };
    for chunk in b[from..].chunks(piece.min(b.len()).max(1)) {
        r.push(chunk, &mut sink);
    }
    r.finish(&mut sink);
    assert_eq!(r.stats.tags, seen.len() as u64);
    (seen, r.stats)
}

fn parse(t: &Tag<'_>, len: usize) {
    match t.header.kind {
        tag::SCRIPT => meta(t.body, len),
        tag::VIDEO => {
            let Some(v) = codec::video_tag(t.body) else { return };
            assert!(v.from <= v.to && v.to <= t.body.len(), "{v:?}");
            assert!((-(1 << 23)..1 << 23).contains(&v.cts));
            let _ = codec::nal4_whole(&t.body[v.from..v.to]);
        }
        tag::AUDIO => {
            let Some(a) = codec::audio_tag(t.body) else { return };
            let payload = t.body.get(a.from..).unwrap_or_default();
            if let Some((rate, _)) = codec::aac_form(payload) {
                assert!(rate > 0);
            }
            if let Some((rate, ch)) = codec::mp3_form(payload) {
                assert!((8_000..=48_000).contains(&rate) && matches!(ch, 1 | 2));
            }
            let (rate, ch, _) = codec::pcm_form(a.flags);
            assert!(rate >= 5_512 && matches!(ch, 1 | 2));
        }
        _ => {}
    }
}

fn meta(body: &[u8], len: usize) {
    let Ok(m) = amf::on_meta_data(body) else { return };
    for array in [m.filepositions, m.times].into_iter().flatten() {
        assert!(array.at <= body.len() && array.count as usize <= body.len());
        assert!(amf::numbers(body, array).count() <= array.count as usize);
    }
    let entries = index::from_meta(body, &m, 0, Some(len as u64));
    assert!(entries.len() <= index::MAX_KEYS);
    for w in entries.windows(2) {
        assert!(w[0].time_ns < w[1].time_ns && w[0].byte < w[1].byte, "entries out of order");
    }
    assert!(entries.iter().all(|e| e.byte < len as u64 && e.byte >= 9));
}

fuzz_target!(|data: &[u8]| {
    let Some((&knob, b)) = data.split_first() else { return };
    meta(b, b.len());
    let _ = codec::video_tag(b);
    let _ = codec::audio_tag(b);
    let expect = EXPECT[usize::from(knob >> 3 & 3)];
    let (whole, stats) = read(b, usize::MAX, 0, expect);
    let (cut, cut_stats) = read(b, PIECES[usize::from(knob & 7)], 0, expect);
    assert_eq!(cut, whole, "pieces of {} read otherwise", PIECES[usize::from(knob & 7)]);
    assert_eq!(cut_stats, stats);
    if let Some(n) = whole.len().checked_sub(1) {
        assert!(whole[n].pos as usize + whole[n].len + TAG_HEADER <= b.len());
    }
    // A seek lands anywhere, and what comes out from there is whole tags of the file too.
    if knob & 0x20 != 0 && !b.is_empty() {
        let from = usize::from(knob >> 6) * b.len() / 4;
        let (behind, _) = read(b, PIECES[usize::from(knob & 7)], from, expect);
        assert!(behind.iter().all(|t| t.pos >= from as u64));
    }
});
