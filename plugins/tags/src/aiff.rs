//! AIFF and AIFF-C metadata. `COMM` for properties, the IFF text chunks and an `ID3 ` chunk for
//! tags. RIFF-shaped but big-endian (AIFF-1.3 "File Structure", in-tree `spec/AIFF.md`).

use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

use crate::Props;
use crate::store::decode_text;

/// Scratch for the Latin-1 to UTF-8 fallback, sized as in `wav.rs`.
const TEXT_SCRATCH: usize = 1024;

const FORM_HEADER: u64 = 12;

/// `COMM` bytes needed. A plain AIFF `ckSize` of exactly 18 must not be rejected as short.
const COMM_LEN: usize = 18;

/// Chunk passes. `Tags::get` returns the first value per key, so ID3 must be emitted before the
/// text chunks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Pass {
    Id3,
    Text,
}

impl Pass {
    /// Precedence is the outer loop because tag chunks can straddle the prefix and one extent.
    pub(crate) const ORDER: [Pass; 2] = [Pass::Id3, Pass::Text];
}

#[cfg(test)]
pub(crate) fn walk(
    window: &[u8],
    base: u64,
    file_len: u64,
    props: &mut Props,
    arena: &Arena,
    sink: &mut impl TagSink,
) -> Option<u64> {
    let mut resume = None;
    for pass in Pass::ORDER {
        let r = walk_pass(window, base, file_len, props, pass, arena, sink);
        resume = resume.or(r);
    }
    resume
}

/// Where the walk ran out of bytes. Walks headers only, so ID3 frames never touch the arena.
pub(crate) fn plan(window: &[u8], base: u64, file_len: u64) -> Option<u64> {
    for_each_chunk(window, base, file_len, |_, _| {})
}

pub(crate) fn walk_pass(
    window: &[u8],
    base: u64,
    file_len: u64,
    props: &mut Props,
    which: Pass,
    arena: &Arena,
    sink: &mut impl TagSink,
) -> Option<u64> {
    let mut scratch = [0u8; TEXT_SCRATCH];
    for_each_chunk(window, base, file_len, |id, body| match (id, which) {
        (b"COMM", _) => comm(body, props),
        // Not in the Apple papers. A complete ID3v2 tag as a chunk body, as ffmpeg and iTunes
        // write.
        (b"ID3 " | b"id3 ", Pass::Id3) => pf_mp3::id3::parse_v2(body, arena, sink),
        (_, Pass::Text) => {
            if let Some(key) = text_key(id) {
                // Spec says pure ASCII with no terminator. Real files carry a NUL and Latin-1 or
                // UTF-8.
                let text = body.split(|b| *b == 0).next().unwrap_or(body);
                let text = decode_text(text, &mut scratch);
                if !text.is_empty() {
                    sink.text(key, text);
                }
            }
        }
        _ => {}
    })
}

fn for_each_chunk(
    window: &[u8],
    base: u64,
    file_len: u64,
    mut visit: impl FnMut(&[u8; 4], &[u8]),
) -> Option<u64> {
    let mut abs = if base == 0 { FORM_HEADER } else { base };
    let mut resume = None;

    while let Some(w) = abs.checked_sub(base).and_then(|d| usize::try_from(d).ok()) {
        let Some(header) = window.get(w..w + 8) else {
            resume = Some(abs);
            break;
        };
        let id: [u8; 4] = [header[0], header[1], header[2], header[3]];
        let size = u64::from(u32::from_be_bytes([header[4], header[5], header[6], header[7]]));
        let body_at = w + 8;

        // Parsed chunks need their whole body, so resume at a cut one. `SSND` is skipped by size.
        if parsed(&id) {
            if (window.len().saturating_sub(body_at) as u64) < size && abs + 8 + size <= file_len {
                resume = Some(abs);
                break;
            }
            if let Some(body) = window.get(body_at..).map(|t| &t[..t.len().min(size as usize)]) {
                visit(&id, body);
            }
        }

        // An odd chunk size carries a pad byte outside the size (AIFF-1.3 "File Structure").
        let advance = 8 + size + (size & 1);
        let Some(next) = abs.checked_add(advance) else {
            break;
        };
        // `next <= abs` guards a zero-size chunk looping forever.
        if next <= abs || next >= file_len {
            break;
        }
        abs = next;
    }

    resume.filter(|at| *at < file_len)
}

fn parsed(id: &[u8; 4]) -> bool {
    matches!(id, b"COMM" | b"ID3 " | b"id3 ") || text_key(id).is_some()
}

/// The Common Chunk (AIFF-1.3 "Common Chunk"). AIFF-C compression gates the frame count.
fn comm(body: &[u8], props: &mut Props) {
    let Some(c) = body.get(..COMM_LEN) else {
        return;
    };
    let channels = u16::from_be_bytes([c[0], c[1]]);
    if channels > 0 {
        props.channels = Some(u32::from(channels));
    }
    let frames = u64::from(u32::from_be_bytes([c[2], c[3], c[4], c[5]]));
    let Some(rate) =
        extended_rate(&[c[8], c[9], c[10], c[11], c[12], c[13], c[14], c[15], c[16], c[17]])
    else {
        return;
    };
    props.sample_rate = Some(rate);
    if !frame_count_is_frames(body.get(18..22)) {
        return;
    }
    // u128 since `frames * 1e9` overflows u64 past ~18G frames.
    props.duration_ns = u64::try_from(u128::from(frames) * 1_000_000_000 / u128::from(rate)).ok();
    props.duration_exact = true;
}

/// Whether `numSampleFrames` counts frames. Real `ima4` files store packets there, so only PCM
/// types are believed.
fn frame_count_is_frames(compression: Option<&[u8]>) -> bool {
    let Some(kind) = compression else {
        return true; // an 18-octet COMM: plain AIFF, no compression field exists
    };
    matches!(
        kind,
        b"NONE"
            | b"twos"
            | b"sowt"
            | b"raw "
            | b"in24"
            | b"42ni"
            | b"in32"
            | b"23ni"
            | b"fl32"
            | b"FL32"
            | b"fl64"
            | b"FL64"
    )
}

/// An 80-bit IEEE 754 extended big-endian float as whole hertz. `None` for implausible rates.
fn extended_rate(b: &[u8; 10]) -> Option<u32> {
    if b[0] & 0x80 != 0 {
        return None;
    }
    let exponent = (u32::from(b[0] & 0x7F) << 8) | u32::from(b[1]);
    let mantissa = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    // Exponent 0 is zero or a denormal, 0x7FFF is infinity or NaN.
    if exponent == 0 || exponent == 0x7FFF {
        return None;
    }
    let shift = exponent as i64 - 16383 - 63;
    let value: u128 = if shift >= 0 {
        if shift >= 64 {
            return None;
        }
        u128::from(mantissa) << shift
    } else {
        let s = (-shift) as u32;
        if s > 64 {
            return None; // rounds to zero
        }
        // Round half up. `s >= 1` here, so the half-ULP shift is valid.
        (u128::from(mantissa) + (1u128 << (s - 1))) >> s
    };
    u32::try_from(value).ok().filter(|&r| r > 0)
}

/// The four EA IFF 85 text chunks mapped to canonical Vorbis-comment keys.
fn text_key(id: &[u8; 4]) -> Option<&'static str> {
    Some(match id {
        b"NAME" => "TITLE",     // the name of the sampled sound
        b"AUTH" => "ARTIST",    // "the creator of a sampled sound"
        b"(c) " => "COPYRIGHT", // a copyright notice
        b"ANNO" => "COMMENT",   // many may exist, the first wins
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use profluens_core::event::{TagList, TagListSink};
    use profluens_core::memory::Pool;

    use crate::fixture::{aiff, aiff_chunk, comm_chunk, extended80, form, id3v2};

    fn run(bytes: &[u8]) -> (Props, TagList, Option<u64>) {
        run_windowed(bytes, bytes.len() as u64)
    }

    fn run_windowed(bytes: &[u8], file_len: u64) -> (Props, TagList, Option<u64>) {
        let pool = Pool::bounded(4096, 8);
        let arena = Arena::default();
        let mut sink = TagListSink::new(&pool);
        let mut props = Props::default();
        let resume = walk(bytes, 0, file_len, &mut props, &arena, &mut sink);
        (props, sink.finish(), resume)
    }

    #[test]
    fn the_four_worked_rates_decode_exactly() {
        for rate in [44_100u32, 48_000, 22_050, 8_000, 96_000, 11_025, 192_000, 1] {
            assert_eq!(extended_rate(&extended80(f64::from(rate))), Some(rate), "rate {rate}");
        }
        assert_eq!(extended_rate(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]), Some(44_100));
        assert_eq!(extended_rate(&[0x40, 0x0E, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]), Some(48_000));
        assert_eq!(extended_rate(&[0x40, 0x0D, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]), Some(22_050));
        assert_eq!(extended_rate(&[0x40, 0x0B, 0xFA, 0x00, 0, 0, 0, 0, 0, 0]), Some(8_000));
    }

    #[test]
    fn extended_edge_cases_yield_no_rate_and_never_panic() {
        let zero = [0u8; 10];
        assert_eq!(extended_rate(&zero), None, "+0 is not a sample rate");
        assert_eq!(extended_rate(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 1]), None);
        assert_eq!(extended_rate(&[0x7F, 0xFF, 0x80, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(extended_rate(&[0x7F, 0xFF, 0xC0, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(extended_rate(&[0xC0, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(extended_rate(&[0x40, 0x3F, 0x80, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(extended_rate(&[0x40, 0x63, 0x80, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(extended_rate(&[0x3F, 0x00, 0x80, 0, 0, 0, 0, 0, 0, 0]), None);
        for hi in 0..=u16::MAX {
            let [a, b] = hi.to_be_bytes();
            let _ = extended_rate(&[a, b, 0x80, 0, 0, 0, 0, 0, 0, 0]);
            let _ = extended_rate(&[a, b, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        }
    }

    #[test]
    fn props_and_text_chunks() {
        let f = aiff(
            b"AIFF",
            44_100,
            2,
            16,
            44_100, // exactly one second
            &[
                ("NAME", "Enough"),
                ("AUTH", "Solar Fields"),
                ("ANNO", "a note"),
                ("(c) ", "1988 Apple"),
            ],
            None,
        );
        let (props, tags, resume) = run(&f);
        assert_eq!(props.sample_rate, Some(44_100));
        assert_eq!(props.channels, Some(2));
        assert_eq!(props.duration_ns, Some(1_000_000_000));
        assert!(props.duration_exact, "a declared frame count is authoritative");
        assert_eq!(tags.get("TITLE"), Some("Enough"));
        assert_eq!(tags.get("ARTIST"), Some("Solar Fields"));
        assert_eq!(tags.get("COMMENT"), Some("a note"));
        assert_eq!(tags.get("COPYRIGHT"), Some("1988 Apple"));
        assert_eq!(resume, None);
    }

    #[test]
    fn aifc_with_a_pcm_compression_type_states_its_length() {
        for kind in [*b"NONE", *b"sowt", *b"twos", *b"fl32", *b"in24"] {
            let f = aiff(b"AIFC", 22_050, 1, 16, 11_025, &[("NAME", "Compressed")], Some(kind));
            let (props, tags, _) = run(&f);
            assert_eq!(props.sample_rate, Some(22_050));
            assert_eq!(props.channels, Some(1));
            assert_eq!(props.duration_ns, Some(500_000_000), "{:?}", kind);
            assert!(props.duration_exact);
            assert_eq!(tags.get("TITLE"), Some("Compressed"));
        }
    }

    /// A compressed type reports rate and channels but no duration.
    #[test]
    fn aifc_with_a_compressed_type_reports_no_duration() {
        for kind in [*b"ima4", *b"ulaw", *b"QDM2", *b"MAC3"] {
            let f = aiff(b"AIFC", 44_100, 2, 16, 2_068, &[("NAME", "Packets")], Some(kind));
            let (props, tags, _) = run(&f);
            assert_eq!(props.sample_rate, Some(44_100), "{:?}", kind);
            assert_eq!(props.channels, Some(2));
            assert_eq!(props.duration_ns, None, "{:?}: a packet count is not a frame count", kind);
            assert!(!props.duration_exact);
            assert_eq!(tags.get("TITLE"), Some("Packets"), "tags are unaffected");
        }
    }

    /// The ID3 chunk must beat the text chunks whichever order they sit in.
    #[test]
    fn id3_wins_over_the_text_chunks_in_either_order() {
        let tag = id3v2(3, &[("TIT2", "ID3 Title"), ("TALB", "ID3 Album")], None);
        for id3_first in [true, false] {
            let mut chunks = vec![comm_chunk(44_100, 2, 16, 44_100, None)];
            let text = aiff_chunk(b"NAME", b"Text Title");
            let id3 = aiff_chunk(b"ID3 ", &tag);
            if id3_first {
                chunks.push(id3);
                chunks.push(text);
            } else {
                chunks.push(text);
                chunks.push(id3);
            }
            let f = form(b"AIFF", &chunks);
            let (_, tags, _) = run(&f);
            assert_eq!(tags.get("TITLE"), Some("ID3 Title"), "id3_first={id3_first}");
            assert_eq!(tags.get("ALBUM"), Some("ID3 Album"));
        }
    }

    #[test]
    fn odd_sized_chunks_stay_word_aligned() {
        let f = aiff(b"AIFF", 8_000, 1, 8, 4_000, &[("NAME", "abc"), ("AUTH", "x")], None);
        let (props, tags, _) = run(&f);
        assert_eq!(tags.get("TITLE"), Some("abc"));
        assert_eq!(tags.get("ARTIST"), Some("x"), "the walk stayed aligned past the pad byte");
        assert_eq!(props.duration_ns, Some(500_000_000));
    }

    #[test]
    fn a_tag_after_the_audio_asks_to_resume_at_the_right_offset() {
        let tag = id3v2(3, &[("TIT2", "Tail Tag")], None);
        let id3 = aiff_chunk(b"ID3 ", &tag);
        let f = form(
            b"AIFF",
            &[
                comm_chunk(44_100, 2, 16, 44_100, None),
                aiff_chunk(b"SSND", &vec![0u8; 4096]),
                id3.clone(),
            ],
        );
        let head_len = f.len() - id3.len();
        let (props, tags, resume) = run_windowed(&f[..head_len], f.len() as u64);
        assert!(tags.is_empty(), "the tag is past the window");
        assert_eq!(props.duration_ns, Some(1_000_000_000), "props came from the head");
        assert_eq!(resume, Some(head_len as u64));
        assert_eq!(plan(&f[..head_len], 0, f.len() as u64), Some(head_len as u64));

        let pool = Pool::bounded(4096, 8);
        let arena = Arena::default();
        let mut sink = TagListSink::new(&pool);
        let mut p2 = Props::default();
        let again =
            walk(&f[head_len..], head_len as u64, f.len() as u64, &mut p2, &arena, &mut sink);
        assert_eq!(again, None);
        assert_eq!(sink.finish().get("TITLE"), Some("Tail Tag"));
    }

    #[test]
    fn a_body_straddling_the_window_end_asks_to_resume() {
        let name = aiff_chunk(b"NAME", b"Straddler");
        let f = form(
            b"AIFF",
            &[
                comm_chunk(48_000, 2, 24, 48_000, None),
                aiff_chunk(b"SSND", &[0u8; 64]),
                name.clone(),
            ],
        );
        let at = f.len() - name.len();
        let cut = at + 10; // header plus two body bytes
        let (_, tags, resume) = run_windowed(&f[..cut], f.len() as u64);
        assert_eq!(resume, Some(at as u64));
        assert!(tags.is_empty(), "a half-read text chunk yields nothing, not garbage");
    }

    #[test]
    fn a_bad_sample_rate_costs_the_duration_and_nothing_else() {
        let mut comm = vec![0u8; COMM_LEN];
        comm[0..2].copy_from_slice(&2u16.to_be_bytes());
        comm[2..6].copy_from_slice(&44_100u32.to_be_bytes());
        comm[6..8].copy_from_slice(&16u16.to_be_bytes());
        comm[8..18].copy_from_slice(&[0x7F, 0xFF, 0x80, 0, 0, 0, 0, 0, 0, 0]);
        let f = form(b"AIFF", &[aiff_chunk(b"COMM", &comm), aiff_chunk(b"NAME", b"Broken")]);
        let (props, tags, _) = run(&f);
        assert_eq!(props.channels, Some(2));
        assert_eq!(props.sample_rate, None);
        assert_eq!(props.duration_ns, None);
        assert!(!props.duration_exact);
        assert_eq!(tags.get("TITLE"), Some("Broken"));
    }

    #[test]
    fn latin1_text_is_transcoded() {
        let f = form(
            b"AIFF",
            &[comm_chunk(44_100, 2, 16, 44_100, None), aiff_chunk(b"AUTH", b"Bj\xf6rk")],
        );
        let (_, tags, _) = run(&f);
        assert_eq!(tags.get("ARTIST"), Some("Björk"));
    }

    #[test]
    fn absurd_chunk_size_does_not_hang_or_overflow() {
        let mut f = b"FORM\x00\x00\x00\x00AIFF".to_vec();
        f.extend_from_slice(b"SSND\xff\xff\xff\xff");
        f.extend_from_slice(&[0u8; 16]);
        let (_, tags, resume) = run(&f);
        assert!(tags.is_empty());
        assert_eq!(resume, None, "the claimed end is past EOF, nothing to resume");
        let mut z = b"FORM\x00\x00\x00\x00AIFF".to_vec();
        for _ in 0..64 {
            z.extend_from_slice(b"JUNK\x00\x00\x00\x00");
        }
        let (_, _, r) = run(&z);
        assert_eq!(r, None);
    }

    #[test]
    fn truncation_never_panics() {
        let f = aiff(
            b"AIFF",
            44_100,
            2,
            16,
            64,
            &[("NAME", "T"), ("AUTH", "A"), ("ANNO", "a comment"), ("(c) ", "2020 x")],
            None,
        );
        for n in 0..f.len() {
            let _ = run_windowed(&f[..n], n as u64);
            let _ = plan(&f[..n], 0, n as u64);
        }
    }

    #[test]
    fn single_byte_corruption_never_panics() {
        let base = aiff(b"AIFC", 48_000, 2, 24, 64, &[("NAME", "Corrupt me")], Some(*b"sowt"));
        for i in 0..base.len() {
            for bit in [0x01u8, 0x80, 0xFF] {
                let mut f = base.clone();
                f[i] ^= bit;
                let _ = run(&f);
                let _ = plan(&f, 0, f.len() as u64);
            }
        }
    }

    #[test]
    fn id3_chunk_with_a_picture_lands_from_either_position() {
        for tail in [false, true] {
            let f = crate::fixture::aiff_id3(
                44_100,
                2,
                4_410,
                &[("NAME", "Text Title")],
                &[("TIT2", "Tag Title"), ("TPE1", "Tag Artist")],
                tail,
            );
            let (props, tags, _) = run(&f);
            assert_eq!(props.duration_ns, Some(100_000_000), "4410 frames at 44.1 kHz");
            assert_eq!(tags.get("TITLE"), Some("Tag Title"), "tail={tail}");
            assert_eq!(tags.get("ARTIST"), Some("Tag Artist"));
        }
    }
}
