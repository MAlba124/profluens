//! Matroska / WebM metadata, located through the SeekHead and parsed by [`pf_mkv::tags`].

use profluens_core::event::TagSink;

use pf_mkv::ebml::{self, id};
use pf_mkv::tags::{self, Extent, SegmentIndex};

use crate::Props;

/// First read size at a SeekHead position, which carries no length.
const REACH: u64 = 64 * 1024;

const MAX_META: u64 = 32 << 20;

/// The Info Title, RFC 9559 section 5.1.2.12.
const TITLE: &[u8] = &[0x7B, 0xA9];

type View<'a> = (&'a [u8], u64);

/// The next `(offset, len)` read this file wants, or `None` when all reachable metadata is held.
/// Callers stop once a request repeats.
pub(crate) fn plan(views: &[View<'_>], file_len: u64) -> Option<(u64, u64)> {
    let index = index_of(views)?;
    let prefix = &views[..1];

    let mut lo = u64::MAX;
    let mut hi = 0u64;
    let mut incomplete = false;
    for at in [index.info, index.tracks, index.tags, index.attachments].into_iter().flatten() {
        if at >= file_len {
            continue; // a stale or hostile SeekHead position, pointing off the end
        }
        if span(prefix, at).1 {
            continue; // already in hand, and always will be
        }
        // Anchored at `at` so the next read, which replaces this extent, is a superset of it.
        let (bytes, complete) = span(views, at);
        lo = lo.min(at);
        hi = hi.max(at.saturating_add(bytes));
        incomplete |= !complete;
    }
    if lo == u64::MAX || !incomplete {
        return None; // everything reachable is in hand
    }
    let len = hi.saturating_sub(lo).min(MAX_META);
    Some((lo, len.min(file_len.saturating_sub(lo))))
}

fn span(views: &[View<'_>], at: u64) -> (u64, bool) {
    match view_at(views, at).map(tags::element_extent) {
        Some(Extent::Complete { len }) => (len as u64, true),
        Some(Extent::Short { total }) => (total.min(MAX_META), false),
        Some(Extent::Invalid) | None => (REACH, false),
    }
}

fn view_at<'a>(views: &[View<'a>], at: u64) -> Option<&'a [u8]> {
    views
        .iter()
        .filter_map(|(bytes, base)| {
            let off = usize::try_from(at.checked_sub(*base)?).ok()?;
            bytes.get(off..).filter(|w| !w.is_empty())
        })
        .max_by_key(|w| w.len())
}

fn index_of(views: &[View<'_>]) -> Option<SegmentIndex> {
    views.iter().find(|(_, base)| *base == 0).and_then(|(bytes, _)| tags::index_segment(bytes))
}

pub(crate) fn parse(views: &[View<'_>], sink: &mut impl TagSink) -> Props {
    let mut props = Props::default();
    let Some(index) = index_of(views) else {
        return props;
    };

    if let Some(info) = index.info.and_then(|at| view_at(views, at)) {
        props.duration_ns = tags::info_duration_ns(info);
        props.duration_exact = props.duration_ns.is_some();
    }
    if let Some(tracks) = index.tracks.and_then(|at| view_at(views, at))
        && let Some((rate, channels)) = tags::audio_format(tracks)
    {
        props.sample_rate = (rate != 0).then_some(rate);
        props.channels = (channels != 0).then_some(channels);
    }
    let mut sink = Titled { sink, title: false };
    if let Some(t) = index.tags.and_then(|at| view_at(views, at)) {
        tags::parse_tags(t, &mut sink);
    }
    if !sink.title
        && let Some(title) = index.info.and_then(|at| view_at(views, at)).and_then(info_title)
    {
        sink.sink.text("TITLE", title);
    }
    if let Some(a) = index.attachments.and_then(|at| view_at(views, at)) {
        tags::parse_attachments(a, sink.sink);
    }
    props
}

/// The Segment's title, `info` the Info element from its head on.
pub(crate) fn info_title(info: &[u8]) -> Option<&str> {
    let h = ebml::read_element_header(info, 0).ok()?;
    if h.id != id::INFO {
        return None;
    }
    let body = info.get(h.data_start..h.data_end().ok()??.min(info.len()))?;
    let mut at = 0;
    while at < body.len() {
        let c = ebml::read_element_header(body, at).ok()?;
        let end = c.data_end().ok()??;
        if c.id == TITLE {
            let t = std::str::from_utf8(ebml::read_string(body.get(c.data_start..end)?)).ok()?;
            return Some(t.trim()).filter(|t| !t.is_empty());
        }
        at = end;
    }
    None
}

/// Passes tags on and notes whether one was a TITLE.
struct Titled<'s, S> {
    sink: &'s mut S,
    title: bool,
}

impl<S: TagSink> TagSink for Titled<'_, S> {
    fn text(&mut self, key: &str, value: &str) {
        self.title |= key == "TITLE";
        self.sink.text(key, value);
    }

    fn picture(&mut self, mime: &str, data: &[u8]) {
        self.sink.picture(mime, data);
    }

    fn picture_typed(&mut self, kind: u8, mime: &str, data: &[u8]) {
        self.sink.picture_typed(kind, mime, data);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use profluens_core::event::TagListSink;
    use profluens_core::memory::Pool;

    use crate::fixture::{mkv, mkv_trailing};

    fn run(views: &[View<'_>]) -> (Props, profluens_core::event::TagList) {
        let pool = Pool::bounded(65_536, 8);
        let mut sink = TagListSink::new(&pool);
        let props = parse(views, &mut sink);
        (props, sink.finish())
    }

    #[test]
    fn a_front_loaded_file_needs_no_second_read() {
        let f = mkv(48_000.0, 2, 3_000.0, &[("TITLE", "Zonderlig"), ("ARTIST", "Sonderlig")], None);
        let views = [(&f[..], 0u64)];
        assert_eq!(plan(&views, f.len() as u64), None, "the prefix holds everything");

        let (props, tags) = run(&views);
        assert_eq!(props.duration_ns, Some(3_000_000_000));
        assert!(props.duration_exact);
        assert_eq!(props.sample_rate, Some(48_000));
        assert_eq!(props.channels, Some(2));
        assert_eq!(tags.get("TITLE"), Some("Zonderlig"));
        assert_eq!(tags.get("ARTIST"), Some("Sonderlig"));
    }

    #[test]
    fn trailing_masters_are_reached_in_one_extra_read() {
        let art = vec![0x5Au8; 4_096];
        let f = mkv_trailing(
            44_100.0,
            2,
            1_500.0,
            &[("TITLE", "Far"), ("ALBUM", "Behind the clusters")],
            Some(("image/jpeg", &art)),
            200_000,
        );
        let prefix_len = 128 * 1024;
        let prefix: View = (&f[..prefix_len], 0);

        let (at, len) = plan(&[prefix], f.len() as u64).expect("a trailing read");
        assert!(at >= prefix_len as u64, "the request starts past what we hold");
        assert!(len <= MAX_META);

        let (props, tags) = run(&[prefix]);
        assert_eq!(props.duration_ns, Some(1_500_000_000));
        assert_eq!(props.sample_rate, Some(44_100));
        assert_eq!(tags.get("TITLE"), None, "the tags are not in the prefix");

        let extent: View = (&f[at as usize..(at + len) as usize], at);
        assert_eq!(plan(&[prefix, extent], f.len() as u64), None, "one read settles it");

        let (props, tags) = run(&[prefix, extent]);
        assert_eq!(props.duration_ns, Some(1_500_000_000));
        assert_eq!(tags.get("TITLE"), Some("Far"));
        assert_eq!(tags.get("ALBUM"), Some("Behind the clusters"));
        assert_eq!(tags.pictures().len(), 1);
        assert_eq!(&*tags.pictures()[0].mime, "image/jpeg");
        assert_eq!(tags.pictures()[0].data.data(), &art[..], "cover art is borrowed, not copied");
    }

    #[test]
    fn a_second_read_is_a_superset_of_the_first() {
        let art = vec![0x11u8; 200_000]; // far past REACH
        let f = mkv_trailing(
            48_000.0,
            1,
            10.0,
            &[("TITLE", "Big art")],
            Some(("image/png", &art)),
            4_096,
        );
        let prefix: View = (&f[..2_048], 0);

        let (at, len) = plan(&[prefix], f.len() as u64).expect("a trailing read");
        assert!(len <= REACH + 4_096, "with no headers in hand the first request is REACH-sized");
        let first: View = (&f[at as usize..(at + len) as usize], at);

        let (at2, len2) = plan(&[prefix, first], f.len() as u64).expect("the art needs more");
        assert_eq!(at2, at, "anchored at the same offset, the second read re-covers the tags");
        assert!(len2 > len && len2 > art.len() as u64, "and reaches past the art");

        let second: View = (&f[at2 as usize..(at2 + len2) as usize], at2);
        assert_eq!(plan(&[prefix, second], f.len() as u64), None, "two reads settle it");
        let (_, tags) = run(&[prefix, second]);
        assert_eq!(tags.get("TITLE"), Some("Big art"), "kept, though only the 2nd extent is held");
        assert_eq!(tags.pictures()[0].data.data().len(), art.len());
    }

    #[test]
    fn a_window_stopping_inside_an_element_reports_its_true_length() {
        let art = vec![0x22u8; 100_000];
        let f = mkv_trailing(48_000.0, 1, 10.0, &[("TITLE", "T")], Some(("image/png", &art)), 512);
        let prefix: View = (&f[..1_024], 0);
        let (at, len) = plan(&[prefix], f.len() as u64).expect("a trailing read");

        let stub: View = (&f[at as usize..at as usize + 8], at);
        let (next_at, next_len) = plan(&[prefix, stub], f.len() as u64).expect("more is needed");
        assert_eq!(next_at, at, "the same range, measured rather than guessed");
        assert_eq!(next_len, len);
    }

    /// A file of `tags` whose Info states `title`, no SeekHead, the masters in the prefix.
    fn titled(title: &[u8], tags: &[(&str, &str)]) -> Vec<u8> {
        use crate::fixture::{mkv_cluster, mkv_master, mkv_segment, mkv_tags, mkv_tracks};
        let info = mkv_master(id::INFO, &[mkv_master(TITLE, &[title.to_vec()])]);
        let mut level1 = vec![info, mkv_tracks(48_000.0, 2)];
        level1.extend((!tags.is_empty()).then(|| mkv_tags(tags)));
        level1.push(mkv_cluster(16));
        mkv_segment("matroska", &level1).0
    }

    #[test]
    fn the_info_title_is_the_title_no_tag_states() {
        let f = titled(b"Segment", &[("ARTIST", "Band")]);
        let (_, tags) = run(&[(&f[..], 0)]);
        assert_eq!((tags.get("TITLE"), tags.get("ARTIST")), (Some("Segment"), Some("Band")));
        let f = titled(b"Segment", &[("title", "Tagged")]);
        let (_, tags) = run(&[(&f[..], 0)]);
        assert_eq!(tags.iter().filter(|(k, _)| *k == "TITLE").count(), 1);
        assert_eq!(tags.get("TITLE"), Some("Tagged"));
        let f = titled(b" Padded \0\0", &[]);
        assert_eq!(run(&[(&f[..], 0)]).1.get("TITLE"), Some("Padded"), "NUL padded and trimmed");
        let f = titled(b"  ", &[]);
        assert_eq!(run(&[(&f[..], 0)]).1.get("TITLE"), None, "blank");
    }

    #[test]
    fn info_title_reads_the_info_alone() {
        let info = mkv_master_bytes(id::INFO, &[(TITLE, b"T")]);
        assert_eq!(info_title(&info), Some("T"));
        assert_eq!(info_title(&mkv_master_bytes(id::TRACKS, &[(TITLE, b"T")])), None, "not Info");
        let scale = mkv_master_bytes(id::INFO, &[(id::TIMESTAMP_SCALE, &[0x0F, 0x42, 0x40])]);
        assert_eq!(info_title(&scale), None);
        for n in 0..info.len() {
            let _ = info_title(&info[..n]);
        }
        assert_eq!(info_title(&info[..info.len() - 1]), None, "the title cut");
    }

    fn mkv_master_bytes(master: &[u8], kids: &[(&[u8], &[u8])]) -> Vec<u8> {
        use crate::fixture::mkv_master;
        let kids: Vec<Vec<u8>> = kids.iter().map(|(i, v)| mkv_master(i, &[v.to_vec()])).collect();
        mkv_master(master, &kids)
    }

    #[test]
    fn junk_is_declined() {
        for junk in [&b""[..], b"ID3\x04\x00", b"RIFF\x00\x00\x00\x00WAVE", &[0xFFu8; 64]] {
            let views = [(junk, 0u64)];
            assert_eq!(plan(&views, junk.len() as u64), None);
            let (props, tags) = run(&views);
            assert_eq!(props, Props::default());
            assert_eq!(tags.get("TITLE"), None);
            assert_eq!(tags.pictures().len(), 0);
        }
    }

    #[test]
    fn truncation_never_panics() {
        let f = mkv_trailing(
            48_000.0,
            2,
            42.0,
            &[("TITLE", "T"), ("PART_NUMBER", "3")],
            Some(("image/png", &[7u8; 300])),
            1_024,
        );
        let pool = Pool::bounded(4_096, 8);
        for n in 0..f.len() {
            let views = [(&f[..n], 0u64)];
            let _ = plan(&views, n as u64);
            let mut sink = TagListSink::new(&pool);
            let _ = parse(&views, &mut sink);
        }
    }
}
