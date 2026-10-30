//! MP3 metadata. The leading ID3v2 tag and the duration probe. The trailing APEv2/ID3v1 pair is
//! [`crate::tail`]'s.

use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

use crate::Props;

/// Audio past the ID3v2 tag the head window reaches. The Xing/VBRI header sits inside the first
/// frame, at most 1441 bytes.
const PROBE_BYTES: u64 = 8 * 1024;

pub(crate) fn v2_len(window: &[u8]) -> u64 {
    pf_mp3::id3::v2_total_len(window).map_or(0, |n| n as u64)
}

/// One max-size MPEG-1 Layer III frame plus room to resync.
const MIN_AUDIO: u64 = 2 * 1024;

pub(crate) fn head_end(window: &[u8], file_len: u64) -> u64 {
    v2_len(window).saturating_add(PROBE_BYTES).min(file_len)
}

/// Whether `window` is too short for tags and duration. Not `len < head_end`, which would
/// re-read small files for nothing.
pub(crate) fn head_short(window: &[u8], file_len: u64) -> bool {
    v2_len(window).saturating_add(MIN_AUDIO).min(file_len) > window.len() as u64
}

pub(crate) fn emit_v2(head: &[u8], v2: u64, arena: &Arena, sink: &mut impl TagSink) {
    if v2 == 0 {
        return;
    }
    let end = usize::try_from(v2).unwrap_or(usize::MAX).min(head.len());
    pf_mp3::id3::parse_v2(&head[..end], arena, sink);
}

pub(crate) fn props(head: &[u8], v2: u64, audio_len: u64) -> Props {
    let mut props = Props::default();
    let Some(audio) = usize::try_from(v2).ok().and_then(|at| head.get(at..)) else {
        return props;
    };
    let Some(a) = pf_mp3::props::probe_props(audio, audio_len) else {
        return props;
    };
    props.duration_ns = a.duration_ns;
    props.duration_exact = a.exact;
    props.sample_rate = Some(a.sample_rate);
    props.channels = Some(a.channels);
    props
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use profluens_core::event::TagListSink;
    use profluens_core::memory::Pool;

    use crate::fixture::{id3v2, mp3, mp3_frames};

    #[test]
    fn head_end_covers_the_tag_and_the_first_frame() {
        let tag = id3v2(3, &[("TIT2", "T")], None);
        let f = [tag.clone(), mp3_frames(44_100, 128, 4)].concat();
        assert_eq!(v2_len(&f), tag.len() as u64);
        assert_eq!(
            head_end(&f, f.len() as u64),
            (tag.len() as u64 + PROBE_BYTES).min(f.len() as u64)
        );
        let bare = mp3_frames(44_100, 128, 4);
        assert_eq!(v2_len(&bare), 0);
    }

    #[test]
    fn only_a_window_missing_the_tag_or_the_first_frame_asks_for_more() {
        let big = mp3(
            44_100,
            128,
            6,
            &[("TIT2", "T")],
            Some(("image/jpeg", &[7u8; 20_000])),
            false,
            None,
            &[],
        );
        let len = big.len() as u64;
        assert!(head_short(&big[..4096], len));
        assert!(!head_short(&big, len), "the whole file is never short");
        let small = mp3_frames(44_100, 128, 6); // ~2.5 KiB
        assert!(!head_short(&small, small.len() as u64));
        assert!(head_short(&small[..1024], small.len() as u64));
    }

    #[test]
    fn tags_and_props_from_a_whole_file() {
        let f = mp3(
            44_100,
            128,
            20,
            &[("TIT2", "Head Tag"), ("TPE1", "An Artist")],
            None,
            false,
            Some(("Tail Title", "Tail Artist", "Tail Album", "1999", 3)),
            &[],
        );
        let pool = Pool::bounded(4096, 8);
        let arena = Arena::default();
        let mut sink = TagListSink::new(&pool);
        let v2 = v2_len(&f);
        emit_v2(&f, v2, &arena, &mut sink);
        let t = crate::tail::measure(&f, 0, f.len() as u64);
        crate::tail::emit(&f, 0, f.len() as u64, t, &arena, &mut sink);
        let tags = sink.finish();

        assert_eq!(tags.get("TITLE"), Some("Head Tag"));
        assert_eq!(tags.get("ARTIST"), Some("An Artist"));
        assert_eq!(tags.get("ALBUM"), Some("Tail Album"));
        assert_eq!(tags.get("DATE"), Some("1999"));

        let p = props(&f, v2, f.len() as u64 - v2 - t.total());
        assert_eq!(p.sample_rate, Some(44_100));
        assert_eq!(p.channels, Some(2));
        assert!(p.duration_ns.is_some());
    }

    #[test]
    fn a_xing_header_makes_the_duration_exact() {
        let f = mp3(44_100, 128, 20, &[("TIT2", "X")], None, true, None, &[]);
        let v2 = v2_len(&f);
        let p = props(&f, v2, f.len() as u64 - v2);
        assert!(p.duration_exact, "a Xing frame count is authoritative");
        assert_eq!(p.duration_ns, Some(20 * 1152 * 1_000_000_000 / 44_100));
    }

    #[test]
    fn truncation_never_panics() {
        let f = mp3(
            44_100,
            128,
            6,
            &[("TIT2", "T")],
            Some(("image/png", &[0x11u8; 300])),
            false,
            Some(("t", "a", "r", "2001", 1)),
            &[("REPLAYGAIN_TRACK_GAIN", "0.00 dB")],
        );
        let pool = Pool::bounded(4096, 8);
        for n in 0..f.len() {
            let arena = Arena::default();
            let mut sink = TagListSink::new(&pool);
            let v2 = v2_len(&f[..n]);
            emit_v2(&f[..n], v2, &arena, &mut sink);
            let t = crate::tail::measure(&f[..n], 0, n as u64);
            crate::tail::emit(&f[..n], 0, n as u64, t, &arena, &mut sink);
            let _ = props(&f[..n], v2, (n as u64).saturating_sub(v2));
            let _ = head_end(&f[..n], n as u64);
        }
    }
}
