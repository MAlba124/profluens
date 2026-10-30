//! MP4 / M4A metadata. Finds `moov` through the box walk and hands it to [`pf_mp4::ilst`].

use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

use crate::Props;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plan {
    Here { at: usize, len: usize },
    Need { at: u64, len: u64 },
    None,
}

/// Walk `window`, which starts at absolute offset `base` on a top-level box boundary.
/// `locate_moov` measures from its slice start, so answers are rebased here.
pub(crate) fn locate(window: &[u8], base: u64, file_len: u64) -> Plan {
    let Some(remaining) = file_len.checked_sub(base) else {
        return Plan::None;
    };
    match pf_mp4::ilst::locate_moov(window, remaining) {
        pf_mp4::ilst::MoovExtent::InPrefix { offset, len } => Plan::Here { at: offset, len },
        pf_mp4::ilst::MoovExtent::Beyond { offset, len } => Plan::Need { at: base + offset, len },
        pf_mp4::ilst::MoovExtent::NotFound => Plan::None,
    }
}

/// Parse the `moov` at `window[at..at + len]`. Cover art stays borrowed from `window`.
pub(crate) fn parse(
    window: &[u8],
    at: usize,
    len: usize,
    arena: &Arena,
    sink: &mut impl TagSink,
) -> Props {
    let mut props = Props::default();
    let Some(moov) = at.checked_add(len).and_then(|end| window.get(at..end)) else {
        return props;
    };
    let p = pf_mp4::ilst::props_from_moov(moov);
    props.duration_ns = p.duration_ns;
    props.duration_exact = p.duration_ns.is_some();
    props.sample_rate = p.sample_rate;
    props.channels = p.channels;
    pf_mp4::ilst::parse_moov_tags(moov, arena, sink);
    props
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use profluens_core::event::TagListSink;
    use profluens_core::memory::Pool;

    use crate::fixture::m4a;

    fn run(f: &[u8]) -> (Props, profluens_core::event::TagList) {
        let pool = Pool::bounded(4096, 8);
        let arena = Arena::default();
        let mut sink = TagListSink::new(&pool);
        let props = match locate(f, 0, f.len() as u64) {
            Plan::Here { at, len } => parse(f, at, len, &arena, &mut sink),
            other => panic!("expected the moov in the prefix, got {other:?}"),
        };
        (props, sink.finish())
    }

    #[test]
    fn faststart_moov_is_found_in_the_prefix() {
        let f = m4a(
            44_100,
            2,
            44_100 * 3,
            &[("©nam", "Title"), ("©ART", "Artist")],
            Some(7),
            None,
            true,
        );
        let (props, tags) = run(&f);
        assert_eq!(props.sample_rate, Some(44_100));
        assert_eq!(props.channels, Some(2));
        assert_eq!(props.duration_ns, Some(3_000_000_000));
        assert!(props.duration_exact);
        assert_eq!(tags.get("TITLE"), Some("Title"));
        assert_eq!(tags.get("ARTIST"), Some("Artist"));
        assert_eq!(tags.get("TRACKNUMBER"), Some("7"));
    }

    #[test]
    fn a_trailing_moov_asks_for_the_unread_range() {
        let f = m4a(48_000, 1, 48_000, &[("©nam", "Tail")], None, None, false);
        let head = 64.min(f.len());
        let Plan::Need { at, len } = locate(&f[..head], 0, f.len() as u64) else {
            panic!("a trailing moov must report a range to read")
        };
        assert!(at >= head as u64, "the range starts past what we already hold");

        let chunk = &f[at as usize..(at + len) as usize];
        let Plan::Here { at: rel, len } = locate(chunk, at, f.len() as u64) else {
            panic!("the re-read range must contain the moov")
        };
        let pool = Pool::bounded(4096, 8);
        let arena = Arena::default();
        let mut sink = TagListSink::new(&pool);
        let props = parse(chunk, rel, len, &arena, &mut sink);
        assert_eq!(props.duration_ns, Some(1_000_000_000));
        assert_eq!(sink.finish().get("TITLE"), Some("Tail"));
    }

    #[test]
    fn cover_art_is_borrowed_from_the_window() {
        let art = vec![0x5Au8; 2_048];
        let f = m4a(44_100, 2, 44_100, &[("©nam", "Art")], None, Some(("image/jpeg", &art)), true);
        let (_, tags) = run(&f);
        assert_eq!(tags.pictures().len(), 1);
        assert_eq!(&*tags.pictures()[0].mime, "image/jpeg");
        assert_eq!(tags.pictures()[0].data.data(), &art[..]);
    }

    #[test]
    fn truncation_never_panics() {
        let f = m4a(
            44_100,
            2,
            44_100,
            &[("©nam", "T")],
            Some(2),
            Some(("image/png", &[3u8; 500])),
            true,
        );
        let pool = Pool::bounded(4096, 8);
        for n in 0..f.len() {
            let arena = Arena::default();
            let mut sink = TagListSink::new(&pool);
            if let Plan::Here { at, len } = locate(&f[..n], 0, n as u64) {
                let _ = parse(&f[..n], at, len, &arena, &mut sink);
            }
        }
    }
}
