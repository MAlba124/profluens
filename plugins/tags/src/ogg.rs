//! Ogg metadata for the Opus, Vorbis, FLAC and Speex mappings (RFC 3533).
//! Ogg states no duration except the last page's granule position, so a scan reads head and tail.

use pf_ogg::page::{CAPTURE_PATTERN, HEADER_FIXED_LEN, PageHeader};
use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

use crate::{Format, Props};

/// Pages one header walk may visit. Past this it is an attack, not a header chain.
const MAX_PAGES: usize = 192;

/// Page-spanning runs one packet may be gathered from, about 4 MiB of cover art.
const MAX_RUNS: usize = 64;

/// How much further to read when the header walk runs out of window.
const REACH: u64 = 512 * 1024;

pub(crate) const MAX_META: u64 = 4 << 20;

/// Header packets for Opus, Vorbis and Speex, the identification and comment headers.
const WANT_XIPH: usize = 2;

const MAX_FLAC_HEADERS: usize = 8;

const FLAC_MAP_HEADER: usize = 9;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Head {
    /// Serial of the stream the headers came from, so the tail times this stream only.
    pub serial: Option<u32>,
    pub props: Props,
    pre_skip: u16,
    /// Granule rate. Always 48 kHz for Opus, the stream's own rate otherwise.
    granule_rate: u32,
}

pub(crate) fn want(window: &[u8], body: u64, format: Format) -> usize {
    match format {
        Format::OggFlac => flac_header_packets(window, body),
        _ => WANT_XIPH,
    }
}

/// File offset the head window must reach to hold `want` packets. Walks page headers only.
pub(crate) fn plan(window: &[u8], body: u64, file_len: u64, want: usize) -> Option<u64> {
    let mut at = usize::try_from(body).ok()?;
    let mut done = 0usize;
    for _ in 0..MAX_PAGES {
        let Some(h) = window.get(at..).and_then(|r| r.get(..HEADER_FIXED_LEN)) else {
            return Some(reach(at as u64, file_len));
        };
        if h[..4] != CAPTURE_PATTERN {
            return None;
        }
        let n = h[26] as usize;
        let table_end = at + HEADER_FIXED_LEN + n;
        let Some(table) = window.get(at + HEADER_FIXED_LEN..table_end) else {
            return Some(reach(table_end as u64, file_len));
        };
        // A lacing value below 255 terminates a packet (RFC 3533 section 6).
        done += table.iter().filter(|&&b| b < 255).count();
        at = table_end + table.iter().map(|&b| b as usize).sum::<usize>();
        if done >= want {
            break;
        }
    }
    Some((at as u64).min(file_len))
}

fn reach(at: u64, file_len: u64) -> u64 {
    at.saturating_add(REACH).min(file_len)
}

pub(crate) fn scan(
    window: &[u8],
    body: u64,
    format: Format,
    arena: &Arena,
    sink: &mut impl TagSink,
) -> Head {
    let mut head = Head::default();
    match format {
        Format::OggOpus => {
            let mut ident = None;
            let serial = for_each_packet(window, body, arena, WANT_XIPH, |i, packet| {
                if i == 0 {
                    ident = pf_ogg::ident::parse_opus_head(packet);
                } else {
                    pf_ogg::comment::parse_opus_tags(packet, arena, sink);
                }
                true
            });
            head.serial = serial;
            if let Some(id) = ident {
                head.props.channels = Some(u32::from(id.channels));
                // Opus always decodes at 48 kHz regardless of `input_sample_rate` (RFC 7845 section
                // 5.1).
                head.props.sample_rate = Some(pf_ogg::duration::OPUS_GRANULE_RATE);
                head.pre_skip = id.pre_skip;
            }
            head.granule_rate = pf_ogg::duration::OPUS_GRANULE_RATE;
        }
        Format::OggVorbis => {
            let mut ident = None;
            let serial = for_each_packet(window, body, arena, WANT_XIPH, |i, packet| {
                if i == 0 {
                    ident = pf_ogg::ident::parse_vorbis_ident(packet);
                } else {
                    pf_ogg::comment::parse_vorbis_comments(packet, arena, sink);
                }
                true
            });
            head.serial = serial;
            if let Some(id) = ident {
                head.props.channels = Some(u32::from(id.channels));
                head.props.sample_rate = Some(id.sample_rate);
                head.granule_rate = id.sample_rate;
            }
        }
        Format::OggSpeex => {
            let mut ident = None;
            let serial = for_each_packet(window, body, arena, WANT_XIPH, |i, packet| {
                if i == 0 {
                    ident = pf_ogg::ident::parse_speex_head(packet);
                } else {
                    // Speex comment packet has no magic, type octet or framing bit (Speex manual
                    // section 7.3).
                    pf_ogg::comment::parse_comment_body(packet, arena, sink);
                }
                true
            });
            head.serial = serial;
            if let Some(id) = ident {
                head.props.channels = Some(u32::from(id.channels));
                head.props.sample_rate = Some(id.sample_rate);
                head.granule_rate = id.sample_rate;
            }
        }
        Format::OggFlac => {
            // Packets after the mapping header concatenate into a native FLAC metadata chain.
            let mut chain: Vec<u8, &Arena> = Vec::new_in(arena);
            let serial =
                for_each_packet(window, body, arena, want(window, body, format), |i, packet| {
                    let block = if i == 0 { packet.get(FLAC_MAP_HEADER..) } else { Some(packet) };
                    let Some(block) = block else { return false };
                    chain.extend_from_slice(block);
                    // The block header's top bit is the last-block flag (RFC 9639 section 8.1).
                    let header = if i == 0 { block.get(4) } else { block.first() };
                    header.is_none_or(|h| h & 0x80 == 0)
                });
            head.serial = serial;
            if let Some(si) = crate::flacmeta::stream_info(&chain) {
                head.props.channels = Some(u32::from(si.channels));
                if si.sample_rate > 0 {
                    head.props.sample_rate = Some(si.sample_rate);
                    head.granule_rate = si.sample_rate;
                    if si.total_samples > 0 {
                        // u128 since `total_samples` is a 36-bit field.
                        head.props.duration_ns = u64::try_from(
                            u128::from(si.total_samples) * 1_000_000_000
                                / u128::from(si.sample_rate),
                        )
                        .ok();
                        head.props.duration_exact = true;
                    }
                }
            }
            crate::flacmeta::parse_into(&chain, sink);
        }
        _ => {}
    }
    head
}

/// Duration from the last page's granule. Speex reports a floor, its encoder lookahead is not in
/// the file.
pub(crate) fn duration(tail: &[u8], head: &Head, format: Format) -> Option<u64> {
    let granule = pf_ogg::duration::last_granule(tail, head.serial)?;
    match format {
        Format::OggOpus => Some(pf_ogg::duration::opus_duration_ns(granule, head.pre_skip)),
        _ => pf_ogg::duration::vorbis_duration_ns(granule, head.granule_rate),
    }
}

fn flac_header_packets(window: &[u8], body: u64) -> usize {
    let fallback = MAX_FLAC_HEADERS;
    let Ok(at) = usize::try_from(body) else {
        return fallback;
    };
    let Some(h) = window.get(at..).and_then(|r| r.get(..HEADER_FIXED_LEN)) else {
        return fallback;
    };
    let payload = at + HEADER_FIXED_LEN + h[26] as usize;
    let Some(id) = window.get(payload..).and_then(|r| r.get(..FLAC_MAP_HEADER)) else {
        return fallback;
    };
    if id[..5] != *b"\x7fFLAC" {
        return fallback;
    }
    match u16::from_be_bytes([id[7], id[8]]) as usize {
        0 => fallback,
        n => (n + 1).min(MAX_FLAC_HEADERS),
    }
}

/// Walk packets of the first logical stream. A corrupt page or another serial ends the walk.
fn for_each_packet<'a>(
    window: &'a [u8],
    body: u64,
    arena: &'a Arena,
    limit: usize,
    mut visit: impl FnMut(usize, &'a [u8]) -> bool,
) -> Option<u32> {
    let mut at = usize::try_from(body).ok()?;
    let mut serial: Option<u32> = None;
    let mut runs = [(0usize, 0usize); MAX_RUNS];
    let mut n_runs = 0usize;
    let mut total = 0usize;
    let mut dropped = false;
    let mut index = 0usize;

    for _ in 0..MAX_PAGES {
        let Some(rest) = window.get(at..) else { break };
        let Ok(page) = PageHeader::parse(rest) else {
            break;
        };
        match serial {
            None => serial = Some(page.serial()),
            Some(s) if s != page.serial() => break,
            _ => {}
        }
        let mut off = at + HEADER_FIXED_LEN + page.n_segments();
        let mut start = off;
        let mut run = 0usize;
        for &lace in page.segment_table() {
            off += lace as usize;
            run += lace as usize;
            if lace == 255 {
                continue; // the packet continues into the next segment or page
            }
            if n_runs < MAX_RUNS {
                runs[n_runs] = (start, run);
                n_runs += 1;
                total += run;
            } else {
                dropped = true;
            }
            if !dropped && !visit(index, gather(window, &runs[..n_runs], total, arena)) {
                return serial;
            }
            index += 1;
            if index >= limit {
                return serial;
            }
            (n_runs, total, dropped) = (0, 0, false);
            start = off;
            run = 0;
        }
        if run > 0 {
            if n_runs < MAX_RUNS {
                runs[n_runs] = (start, run);
                n_runs += 1;
                total += run;
            } else {
                dropped = true;
            }
        }
        at += page.len();
    }
    serial
}

/// One packet as a slice, borrowed when it is a single run, copied into `arena` when it spans
/// pages.
fn gather<'a>(
    window: &'a [u8],
    runs: &[(usize, usize)],
    total: usize,
    arena: &'a Arena,
) -> &'a [u8] {
    if let [(start, len)] = *runs {
        return start.checked_add(len).and_then(|e| window.get(start..e)).unwrap_or(&[]);
    }
    let Ok(region) = arena.alloc_bytes(total) else {
        return &[];
    };
    let mut n = 0;
    for &(start, len) in runs {
        let Some(src) = start.checked_add(len).and_then(|e| window.get(start..e)) else {
            break;
        };
        region[n..n + len].copy_from_slice(src);
        n += len;
    }
    let out: &'a [u8] = region;
    &out[..n]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use profluens_core::event::{TagList, TagListSink};
    use profluens_core::memory::Pool;

    use crate::fixture::{ogg_flac, ogg_opus, ogg_speex, ogg_vorbis};

    fn run(f: &[u8], format: Format) -> (Props, TagList) {
        let pool = Pool::bounded(4096, 16);
        let arena = Arena::default();
        let mut sink = TagListSink::new(&pool);
        let mut head = scan(f, 0, format, &arena, &mut sink);
        if let Some(ns) = duration(f, &head, format) {
            head.props.duration_ns = Some(ns);
            head.props.duration_exact = true;
        }
        (head.props, sink.finish())
    }

    #[test]
    fn opus_head_tags_and_exact_duration() {
        let granule = 48_000 * 7 + 312;
        let f = ogg_opus(2, 312, 44_100, &["TITLE=Rain", "ARTIST=Someone"], None, granule);
        let (props, tags) = run(&f, Format::OggOpus);
        assert_eq!(props.channels, Some(2));
        assert_eq!(props.sample_rate, Some(48_000), "Opus decodes at 48 kHz (RFC 7845 §5.1)");
        assert_eq!(props.duration_ns, Some(7_000_000_000));
        assert!(props.duration_exact);
        assert_eq!(tags.get("TITLE"), Some("Rain"));
        assert_eq!(tags.get("ARTIST"), Some("Someone"));
    }

    #[test]
    fn vorbis_ident_tags_and_duration() {
        let f = ogg_vorbis(2, 44_100, &["TITLE=Vor", "ALBUM=Bis"], 44_100 * 3);
        let (props, tags) = run(&f, Format::OggVorbis);
        assert_eq!(props.channels, Some(2));
        assert_eq!(props.sample_rate, Some(44_100));
        assert_eq!(props.duration_ns, Some(3_000_000_000));
        assert_eq!(tags.get("TITLE"), Some("Vor"));
        assert_eq!(tags.get("ALBUM"), Some("Bis"));
    }

    #[test]
    fn speex_head_bare_comments_and_duration() {
        let f = ogg_speex(16_000, 1, &["TITLE=Spx", "ARTIST=Someone"], None, 16_000 * 3);
        let (props, tags) = run(&f, Format::OggSpeex);
        assert_eq!(props.sample_rate, Some(16_000));
        assert_eq!(props.channels, Some(1));
        assert_eq!(props.duration_ns, Some(3_000_000_000));
        assert!(props.duration_exact);
        assert_eq!(tags.get("TITLE"), Some("Spx"));
        assert_eq!(tags.get("ARTIST"), Some("Someone"));
    }

    #[test]
    fn a_speex_comment_carries_cover_art() {
        let art = vec![0x4Du8; 120_000];
        let f = ogg_speex(8_000, 2, &["TITLE=Art"], Some(("image/jpeg", &art)), 8_000);
        let want = want(&f, 0, Format::OggSpeex);
        assert_eq!(want, WANT_XIPH);
        let (props, tags) = run(&f, Format::OggSpeex);
        assert_eq!(props.channels, Some(2));
        assert_eq!(props.duration_ns, Some(1_000_000_000));
        assert_eq!(tags.get("TITLE"), Some("Art"));
        assert_eq!(tags.pictures()[0].data.data(), &art[..]);
    }

    #[test]
    fn ogg_flac_rebuilds_the_native_chain() {
        let f = ogg_flac(48_000, 1, 48_000 * 2, &["TITLE=Chain"], Some(("image/png", &[7u8; 900])));
        let (props, tags) = run(&f, Format::OggFlac);
        assert_eq!(props.sample_rate, Some(48_000));
        assert_eq!(props.channels, Some(1));
        assert_eq!(props.duration_ns, Some(2_000_000_000));
        assert_eq!(tags.get("TITLE"), Some("Chain"));
        assert_eq!(tags.pictures().len(), 1, "a PICTURE block in its own packet still lands");
        assert_eq!(tags.pictures()[0].data.data(), &[7u8; 900][..]);
    }

    #[test]
    fn a_packet_spanning_pages_is_gathered() {
        let art = vec![0x2Bu8; 150_000];
        let f = ogg_opus(2, 312, 48_000, &["TITLE=Big"], Some(("image/jpeg", &art)), 48_000 + 312);
        let want = want(&f, 0, Format::OggOpus);
        assert_eq!(plan(&f, 0, f.len() as u64, want), Some(f.len() as u64 - last_page_len(&f)));

        let (props, tags) = run(&f, Format::OggOpus);
        assert_eq!(props.duration_ns, Some(1_000_000_000));
        assert_eq!(tags.get("TITLE"), Some("Big"));
        assert_eq!(tags.pictures()[0].data.data(), &art[..]);
    }

    fn last_page_len(f: &[u8]) -> u64 {
        let mut at = 0usize;
        let mut last = 0usize;
        while let Ok(p) = PageHeader::parse(&f[at..]) {
            last = p.len();
            at += p.len();
            if at >= f.len() {
                break;
            }
        }
        last as u64
    }

    #[test]
    fn a_short_window_asks_for_more_and_never_regresses() {
        let art = vec![0x11u8; 150_000];
        let f = ogg_opus(1, 312, 48_000, &["TITLE=Short"], Some(("image/jpeg", &art)), 48_312);
        let len = f.len() as u64;
        for cut in [0usize, 1, 26, 27, 100, 4096, 65_536, f.len() / 2] {
            let cut = cut.min(f.len());
            if let Some(end) = plan(&f[..cut], 0, len, WANT_XIPH) {
                assert!(end > cut as u64 || end == len, "cut {cut}: plan {end} makes no progress");
            }
        }
    }

    #[test]
    fn truncation_never_panics() {
        let pool = Pool::bounded(4096, 16);
        let files = [
            (
                ogg_opus(2, 312, 48_000, &["TITLE=T"], Some(("image/png", &[1u8; 4000])), 96_312),
                Format::OggOpus,
            ),
            (ogg_vorbis(1, 8_000, &["TITLE=T"], 8_000), Format::OggVorbis),
            (ogg_flac(44_100, 2, 44_100, &["TITLE=T"], None), Format::OggFlac),
            (ogg_speex(16_000, 1, &["TITLE=T"], None, 16_000), Format::OggSpeex),
        ];
        for (f, format) in files {
            for n in (0..f.len()).step_by(13).chain(0..64) {
                let n = n.min(f.len());
                let arena = Arena::default();
                let mut sink = TagListSink::new(&pool);
                let head = scan(&f[..n], 0, format, &arena, &mut sink);
                let _ = duration(&f[..n], &head, format);
                let _ = plan(&f[..n], 0, n as u64, want(&f[..n], 0, format));
            }
        }
    }

    #[test]
    fn junk_is_not_a_page() {
        let junk: Vec<u8> = (0..4096).map(|i| (i * 37 % 256) as u8).collect();
        assert_eq!(plan(&junk, 0, junk.len() as u64, 2), None);
        let arena = Arena::default();
        let pool = Pool::bounded(4096, 4);
        let mut sink = TagListSink::new(&pool);
        let head = scan(&junk, 0, Format::OggOpus, &arena, &mut sink);
        assert!(head.serial.is_none());
        assert!(sink.finish().is_empty());
    }
}
