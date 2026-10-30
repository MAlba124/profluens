//! The trailing APEv2 then ID3v1 tag pair, shared by MP3, Monkey's Audio, WavPack and Musepack.
//! An APEv2 footer is looked for at `file_len - 128` too, since it sits in front of ID3v1.

use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

pub(crate) const V1_LEN: u64 = 128;

const APE_FOOTER: u64 = 32;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Trailers {
    pub v1: u64,
    pub ape: u64,
}

impl Trailers {
    /// Trailing bytes that are metadata, not stream.
    pub(crate) fn total(self) -> u64 {
        self.v1 + self.ape
    }
}

/// Measure the trailers in `tail`, which starts at file offset `base` and must end at EOF.
pub(crate) fn measure(tail: &[u8], base: u64, file_len: u64) -> Trailers {
    let mut t = Trailers::default();
    if file_len.checked_sub(base) != Some(tail.len() as u64) {
        return t;
    }
    let rel = |abs: u64| -> Option<usize> {
        usize::try_from(abs.checked_sub(base)?).ok().filter(|&d| d <= tail.len())
    };

    if let Some(at) = file_len.checked_sub(V1_LEN).and_then(&rel)
        && tail[at..].starts_with(b"TAG")
    {
        t.v1 = V1_LEN;
    }
    let ape_end = file_len - t.v1;
    if let Some(at) = rel(ape_end).filter(|&at| at as u64 >= APE_FOOTER) {
        // `ape_tail_len` reads the last 32 bytes of its slice, so the slice ends where the tag
        // would.
        if let Some(len) = pf_mp3::id3::ape_tail_len(&tail[..at]).filter(|&n| n as u64 <= ape_end) {
            t.ape = len as u64;
        }
    }
    t
}

/// Emit after the head's tags. APEv2 then ID3v1, so earlier richer values win on `get`.
pub(crate) fn emit(
    tail: &[u8],
    base: u64,
    file_len: u64,
    t: Trailers,
    arena: &Arena,
    sink: &mut impl TagSink,
) {
    let rel = |abs: u64| -> Option<usize> { usize::try_from(abs.checked_sub(base)?).ok() };
    if t.ape > 0 {
        let end = file_len - t.v1;
        if let Some(block) = rel(end - t.ape).zip(rel(end)).and_then(|(s, e)| tail.get(s..e)) {
            pf_mp3::id3::parse_ape(block, sink);
        }
    }
    if t.v1 > 0
        && let Some(block) = rel(file_len - V1_LEN).and_then(|s| tail.get(s..))
    {
        pf_mp3::id3::parse_v1(block, arena, sink);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use profluens_core::event::TagListSink;
    use profluens_core::memory::Pool;

    use crate::fixture::{ape_tag, id3v1, mp3_frames};

    #[test]
    fn finds_ape_in_front_of_id3v1() {
        let ape = ape_tag(&[("REPLAYGAIN_TRACK_GAIN", "-3.21 dB")]);
        let f = [mp3_frames(44_100, 128, 4), ape.clone(), id3v1("T", "A", "R", "2020", 7)].concat();
        let len = f.len() as u64;
        let t = measure(&f, 0, len);
        assert_eq!(t, Trailers { v1: V1_LEN, ape: ape.len() as u64 });
        assert_eq!(t.total(), V1_LEN + ape.len() as u64);

        let base = len - 512;
        assert_eq!(measure(&f[(base as usize)..], base, len), t);
        assert_eq!(measure(&f[..64], 0, len), Trailers::default());
    }

    #[test]
    fn a_bare_stream_has_no_trailers() {
        let f = mp3_frames(48_000, 128, 8);
        assert_eq!(measure(&f, 0, f.len() as u64), Trailers::default());
    }

    #[test]
    fn ape_alone_and_v1_alone() {
        let ape = ape_tag(&[("TITLE", "Ape Only")]);
        let f = [mp3_frames(44_100, 128, 2), ape.clone()].concat();
        let len = f.len() as u64;
        assert_eq!(measure(&f, 0, len), Trailers { v1: 0, ape: ape.len() as u64 });

        let g = [mp3_frames(44_100, 128, 2), id3v1("V1", "A", "R", "1999", 2)].concat();
        assert_eq!(measure(&g, 0, g.len() as u64), Trailers { v1: V1_LEN, ape: 0 });
    }

    #[test]
    fn emits_ape_then_v1() {
        let f = [
            mp3_frames(44_100, 128, 2),
            ape_tag(&[("TITLE", "Ape Title"), ("ALBUM", "Ape Album")]),
            id3v1("V1 Title", "V1 Artist", "V1 Album", "1997", 5),
        ]
        .concat();
        let len = f.len() as u64;
        let pool = Pool::bounded(4096, 8);
        let arena = Arena::default();
        let mut sink = TagListSink::new(&pool);
        let t = measure(&f, 0, len);
        emit(&f, 0, len, t, &arena, &mut sink);
        let tags = sink.finish();
        assert_eq!(tags.get("TITLE"), Some("Ape Title"));
        assert_eq!(tags.get("ALBUM"), Some("Ape Album"));
        assert_eq!(tags.get("ARTIST"), Some("V1 Artist"));
        assert_eq!(tags.get("DATE"), Some("1997"));
    }

    #[test]
    fn truncation_never_panics() {
        let f = [
            mp3_frames(44_100, 128, 3),
            ape_tag(&[("TITLE", "T"), ("REPLAYGAIN_TRACK_GAIN", "0.00 dB")]),
            id3v1("t", "a", "r", "2001", 1),
        ]
        .concat();
        let pool = Pool::bounded(4096, 8);
        for n in 0..f.len() {
            let arena = Arena::default();
            let mut sink = TagListSink::new(&pool);
            let t = measure(&f[..n], 0, n as u64);
            emit(&f[..n], 0, n as u64, t, &arena, &mut sink);
        }
    }
}
