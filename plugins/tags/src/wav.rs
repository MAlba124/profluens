//! RIFF/WAVE metadata. The chunk walk is windowed and reports where to resume past the window.

use profluens_core::event::TagSink;

use crate::Props;
use crate::store::decode_text;

const TEXT_SCRATCH: usize = 1024;

const RIFF_HEADER: u64 = 12;

#[derive(Clone, Copy, Default)]
pub(crate) struct Carry {
    byte_rate: Option<u32>,
    data_len: Option<u64>,
    exact: bool,
}

/// Walk the chunks in `window`, which starts at absolute offset `base` on a chunk boundary.
/// Returns the offset to resume at when the window ran out with file left.
pub(crate) fn walk(
    window: &[u8],
    base: u64,
    file_len: u64,
    props: &mut Props,
    sink: &mut impl TagSink,
) -> Option<u64> {
    walk_carrying(window, base, file_len, &mut Carry::default(), props, sink)
}

pub(crate) fn walk_carrying(
    window: &[u8],
    base: u64,
    file_len: u64,
    carry: &mut Carry,
    props: &mut Props,
    sink: &mut impl TagSink,
) -> Option<u64> {
    let mut abs = if base == 0 { RIFF_HEADER } else { base };
    let Carry { mut byte_rate, mut data_len, mut exact } = *carry;
    let mut resume: Option<u64> = None;

    while let Some(w) = abs.checked_sub(base).and_then(|d| usize::try_from(d).ok()) {
        let Some(header) = window.get(w..w + 8) else {
            resume = Some(abs);
            break;
        };
        let id: [u8; 4] = [header[0], header[1], header[2], header[3]];
        let size = u64::from(u32::from_le_bytes([header[4], header[5], header[6], header[7]]));
        let body_at = w + 8;
        let body = window.get(body_at..).map(|t| &t[..t.len().min(size as usize)]);

        // A parsed chunk needs its whole body, so resume at it when the window cuts it short.
        let full = (window.len().saturating_sub(body_at) as u64) >= size;
        if matches!(&id, b"fmt " | b"LIST") && !full && abs + 8 + size <= file_len {
            resume = Some(abs);
            break;
        }

        match &id {
            b"fmt " => {
                if let Some(f) = body.filter(|b| b.len() >= 16) {
                    let tag = u16::from_le_bytes([f[0], f[1]]);
                    props.channels = Some(u32::from(u16::from_le_bytes([f[2], f[3]])));
                    props.sample_rate = Some(u32::from_le_bytes([f[4], f[5], f[6], f[7]]));
                    byte_rate = Some(u32::from_le_bytes([f[8], f[9], f[10], f[11]]));
                    // `nAvgBytesPerSec` is exact only for PCM, float and extensible.
                    exact = matches!(tag, 0x0001 | 0x0003 | 0xFFFE);
                }
            }
            b"data" => data_len = Some(size.min(file_len.saturating_sub(abs + 8))),
            b"LIST" => {
                if let Some(l) = body.filter(|b| b.len() >= 4 && &b[..4] == b"INFO") {
                    info(&l[4..], sink);
                }
            }
            _ => {}
        }

        // An odd chunk size carries a pad byte outside the size.
        let advance = 8 + size + (size & 1);
        let Some(next) = abs.checked_add(advance) else {
            break;
        };
        if next <= abs || next >= file_len {
            break;
        }
        abs = next;
    }

    if let (Some(len), Some(rate)) = (data_len, byte_rate.filter(|r| *r > 0)) {
        // u128 since `len * 1e9` overflows u64.
        props.duration_ns = u64::try_from(u128::from(len) * 1_000_000_000 / u128::from(rate)).ok();
        props.duration_exact = exact;
    }
    *carry = Carry { byte_rate, data_len, exact };

    resume.filter(|at| *at < file_len)
}

/// The subchunks of a `LIST INFO` body behind its form.
pub(crate) fn info(body: &[u8], sink: &mut impl TagSink) {
    let mut p = 0usize;
    let mut scratch = [0u8; TEXT_SCRATCH];
    while let Some(h) = body.get(p..p + 8) {
        let id: [u8; 4] = [h[0], h[1], h[2], h[3]];
        let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        let Some(raw) = body.get(p + 8..p + 8 + size) else {
            break;
        };
        if let Some(key) = key_for(&id) {
            let text = raw.split(|b| *b == 0).next().unwrap_or(raw);
            let text = decode_text(text, &mut scratch);
            if !text.is_empty() {
                sink.text(key, text);
            }
        }
        let Some(next) = p.checked_add(8 + size + (size & 1)) else {
            break;
        };
        if next <= p {
            break;
        }
        p = next;
    }
}

/// `ITRK` is unregistered but universal. `IPRT` is its alias.
fn key_for(id: &[u8; 4]) -> Option<&'static str> {
    Some(match id {
        b"INAM" => "TITLE",                 // Name
        b"IART" => "ARTIST",                // Artist
        b"IPRD" => "ALBUM",                 // Product (the collection the file belongs to)
        b"ITRK" | b"IPRT" => "TRACKNUMBER", // Track / Part
        b"IGNR" => "GENRE",                 // Genre
        b"ICRD" => "DATE",                  // Creation date
        b"ICMT" => "COMMENT",               // Comments
        b"ISFT" => "ENCODER",               // Software that created the file
        b"ICOP" => "COPYRIGHT",             // Copyright
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use profluens_core::event::{TagList, TagListSink};
    use profluens_core::memory::Pool;

    use crate::fixture::{fmt_chunk, info_chunk, riff, riff_chunk as chunk};

    fn run(bytes: &[u8]) -> (Props, TagList, Option<u64>) {
        let pool = Pool::bounded(4096, 4);
        let mut sink = TagListSink::new(&pool);
        let mut props = Props::default();
        let resume = walk(bytes, 0, bytes.len() as u64, &mut props, &mut sink);
        (props, sink.finish(), resume)
    }

    #[test]
    fn props_and_inline_info() {
        let data = vec![0u8; 44100 * 4]; // exactly 1 s of 44.1 kHz stereo s16
        let f = riff(&[
            fmt_chunk(44_100, 2, 16),
            info_chunk(&[("INAM", "Enough"), ("IART", "Solar Fields")]),
            chunk(b"data", &data),
        ]);
        let (props, tags, resume) = run(&f);
        assert_eq!(props.sample_rate, Some(44_100));
        assert_eq!(props.channels, Some(2));
        assert_eq!(props.duration_ns, Some(1_000_000_000));
        assert!(props.duration_exact);
        assert_eq!(tags.get("TITLE"), Some("Enough"));
        assert_eq!(tags.get("ARTIST"), Some("Solar Fields"));
        assert_eq!(resume, None);
    }

    #[test]
    fn odd_sized_chunks_stay_word_aligned() {
        let f = riff(&[
            fmt_chunk(8_000, 1, 8),
            info_chunk(&[("INAM", "ab"), ("IGNR", "x")]), // "ab\0" is odd, so a pad byte
            chunk(b"data", &[0u8; 8_000]),
        ]);
        let (props, tags, _) = run(&f);
        assert_eq!(tags.get("TITLE"), Some("ab"));
        assert_eq!(tags.get("GENRE"), Some("x"));
        assert_eq!(props.duration_ns, Some(1_000_000_000));
    }

    #[test]
    fn info_after_data_asks_to_resume_at_the_right_offset() {
        let data = vec![0u8; 4096];
        let info = info_chunk(&[("INAM", "Tail")]);
        let f = riff(&[fmt_chunk(44_100, 2, 16), chunk(b"data", &data), info.clone()]);
        let head_len = f.len() - info.len();
        let (props, tags, resume) = {
            let pool = Pool::bounded(4096, 4);
            let mut sink = TagListSink::new(&pool);
            let mut props = Props::default();
            let r = walk(&f[..head_len], 0, f.len() as u64, &mut props, &mut sink);
            (props, sink.finish(), r)
        };
        assert!(tags.is_empty(), "tags are past the window");
        assert!(props.duration_ns.is_some(), "props came from the head");
        assert_eq!(resume, Some(head_len as u64));

        let pool = Pool::bounded(4096, 4);
        let mut sink = TagListSink::new(&pool);
        let mut p2 = Props::default();
        let again = walk(&f[head_len..], head_len as u64, f.len() as u64, &mut p2, &mut sink);
        assert_eq!(again, None);
        assert_eq!(sink.finish().get("TITLE"), Some("Tail"));
    }

    #[test]
    fn a_list_body_straddling_the_window_end_asks_to_resume() {
        let info = info_chunk(&[("INAM", "Straddler")]);
        let f = riff(&[fmt_chunk(44_100, 2, 16), chunk(b"data", &[0u8; 64]), info.clone()]);
        let list_at = f.len() - info.len();
        let cut = list_at + 12;
        let pool = Pool::bounded(4096, 4);
        let mut sink = TagListSink::new(&pool);
        let mut props = Props::default();
        let resume = walk(&f[..cut], 0, f.len() as u64, &mut props, &mut sink);
        assert_eq!(resume, Some(list_at as u64));
        assert!(sink.finish().is_empty(), "a half-read LIST yields nothing, not garbage");

        let mut sink = TagListSink::new(&pool);
        let mut p2 = Props::default();
        walk(&f[list_at..], list_at as u64, f.len() as u64, &mut p2, &mut sink);
        assert_eq!(sink.finish().get("TITLE"), Some("Straddler"));
    }

    #[test]
    fn truncation_never_panics() {
        let f = riff(&[
            fmt_chunk(48_000, 2, 24),
            info_chunk(&[("INAM", "T"), ("ICMT", "a comment")]),
            chunk(b"data", &[0u8; 1000]),
        ]);
        for n in 0..f.len() {
            let pool = Pool::bounded(4096, 4);
            let mut sink = TagListSink::new(&pool);
            let mut props = Props::default();
            let _ = walk(&f[..n], 0, n as u64, &mut props, &mut sink);
        }
    }

    #[test]
    fn absurd_chunk_size_does_not_hang_or_overflow() {
        let mut f = b"RIFF\x00\x00\x00\x00WAVE".to_vec();
        f.extend_from_slice(b"data\xff\xff\xff\xff");
        f.extend_from_slice(&[0u8; 16]);
        let (_, tags, resume) = run(&f);
        assert!(tags.is_empty());
        assert_eq!(resume, None, "the claimed end is past EOF, nothing to resume");
    }

    #[test]
    fn latin1_info_text() {
        let f = riff(&[
            fmt_chunk(44_100, 2, 16),
            {
                let mut b = b"INFO".to_vec();
                b.extend_from_slice(&chunk(b"IART", b"Bj\xf6rk\0"));
                chunk(b"LIST", &b)
            },
            chunk(b"data", &[0u8; 4]),
        ]);
        let (_, tags, _) = run(&f);
        assert_eq!(tags.get("ARTIST"), Some("Björk"));
    }
}
