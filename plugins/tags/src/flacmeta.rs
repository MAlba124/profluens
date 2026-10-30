//! FLAC metadata-block reading per RFC 9639. Tags, STREAMINFO, SEEKTABLE and audio start.
//! Best-effort, malformed metadata stops the walk. One leading ID3v2 tag is skipped.

use profluens_core::event::TagSink;

/// Tags parsed from a FLAC stream's metadata blocks.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct FlacTags {
    /// Vorbis comments as `(uppercased field name, value)`, in order, fields may repeat.
    pub comments: Vec<(String, String)>,
    /// Attached pictures as `(MIME type, image bytes)`.
    pub pictures: Vec<(String, Vec<u8>)>,
}

impl FlacTags {
    pub fn is_empty(&self) -> bool {
        self.comments.is_empty() && self.pictures.is_empty()
    }

    pub fn len(&self) -> usize {
        self.comments.len() + self.pictures.len()
    }
}

/// The container-level STREAMINFO fields (section 8.2). Read with [`stream_info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    /// Sample rate in Hz, a 20-bit field. 0 means a non-audio stream.
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    /// Total interchannel samples, a 36-bit field. 0 means unknown.
    pub total_samples: u64,
}

/// One `SEEKTABLE` entry (section 8.5), a sample number paired with a byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekPoint {
    pub sample: u64,
    /// Offset of the target frame's header, relative to [`audio_start`], not the file.
    pub byte_offset: u64,
    pub frame_samples: u16,
}

const STREAMINFO_LEN: usize = 34;

const SEEKTABLE_TYPE: u8 = 3;

const SEEK_POINT_LEN: usize = 18;

/// Sample number of a placeholder point. Padding, not a seek point.
const PLACEHOLDER_SAMPLE: u64 = u64::MAX;

const KEY_STACK_MAX: usize = 256;

/// Walk the metadata-block chain from `fLaC` and extract its tags.
// One-time header parsing.
#[allow(clippy::disallowed_methods)]
pub fn parse(bytes: &[u8]) -> FlacTags {
    let mut tags = FlacTags::default();
    parse_into(bytes, &mut OwningSink(&mut tags));
    tags
}

/// Zero-copy [`parse`]. Pushes every tag into `sink` borrowed from `bytes`, keys uppercased.
pub fn parse_into(bytes: &[u8], sink: &mut impl TagSink) {
    walk_blocks(bytes, |block_type, block| {
        match block_type {
            4 => vorbis_comment_into(block, sink),
            6 => picture_into(block, sink),
            _ => {}
        }
        true
    });
}

/// Read `STREAMINFO` from a buffer starting at `fLaC`. Takes the first one anywhere in the chain.
pub fn stream_info(bytes: &[u8]) -> Option<StreamInfo> {
    let mut found = None;
    walk_blocks(bytes, |block_type, block| {
        if block_type == 0 {
            found = parse_streaminfo(block);
            return false; // one STREAMINFO per stream, section 8.2
        }
        true
    });
    found
}

/// The `SEEKTABLE` points in encoder order, placeholders skipped. Allocates nothing.
pub fn seek_table(bytes: &[u8]) -> impl Iterator<Item = SeekPoint> + '_ {
    let mut table: &[u8] = &[];
    walk_blocks(bytes, |block_type, block| {
        if block_type == SEEKTABLE_TYPE {
            table = block;
            return false; // only one SEEKTABLE per stream, section 8.5
        }
        true
    });
    table.as_chunks::<SEEK_POINT_LEN>().0.iter().filter_map(|p| parse_seek_point(p))
}

/// Offset of the first audio frame, the anchor of every [`SeekPoint::byte_offset`].
/// `None` when `bytes` does not hold the whole chain, since a guess would shift every seek.
pub fn audio_start(bytes: &[u8]) -> Option<usize> {
    walk_blocks(bytes, |_, _| true)
}

fn parse_seek_point(p: &[u8]) -> Option<SeekPoint> {
    let be64 = |at: usize| -> Option<u64> {
        p.get(at..at + 8).and_then(|s| <[u8; 8]>::try_from(s).ok()).map(u64::from_be_bytes)
    };
    let sample = be64(0)?;
    if sample == PLACEHOLDER_SAMPLE {
        return None;
    }
    let byte_offset = be64(8)?;
    let frame_samples =
        p.get(16..18).and_then(|s| <[u8; 2]>::try_from(s).ok()).map(u16::from_be_bytes)?;
    Some(SeekPoint { sample, byte_offset, frame_samples })
}

/// Length of a leading ID3v2 tag including header and footer, or `None` (ID3v2.4.0 section 3.1).
pub fn id3v2_len(bytes: &[u8]) -> Option<usize> {
    crate::sniff::id3v2_len(bytes).and_then(|n| usize::try_from(n).ok())
}

fn marker_at(bytes: &[u8]) -> usize {
    id3v2_len(bytes).unwrap_or(0)
}

/// Returns the offset past the final block only when the chain ended at a last-block flag.
fn walk_blocks<'a>(bytes: &'a [u8], mut visit: impl FnMut(u8, &'a [u8]) -> bool) -> Option<usize> {
    let at = marker_at(bytes);
    if bytes.get(at..at.checked_add(4)?) != Some(b"fLaC".as_slice()) {
        return None;
    }
    let mut pos = at + 4;
    while pos + 4 <= bytes.len() {
        let header = bytes[pos];
        let last = header & 0x80 != 0;
        let block_type = header & 0x7f;
        let len = u32::from_be_bytes([0, bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        let start = pos + 4;
        let end = start.checked_add(len)?;
        let Some(block) = bytes.get(start..end) else {
            break;
        };
        let go_on = visit(block_type, block);
        pos = end;
        if last {
            return Some(pos);
        }
        if !go_on {
            break;
        }
    }
    None
}

fn vorbis_comment_into(block: &[u8], sink: &mut impl TagSink) {
    let le = |b: &[u8], p: usize| {
        b.get(p..p + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    let mut p = 0;
    let Some(vendor_len) = le(block, p) else {
        return;
    };
    p += 4 + vendor_len;
    let Some(count) = le(block, p) else { return };
    p += 4;
    let mut key_buf = [0u8; KEY_STACK_MAX];
    for _ in 0..count {
        let Some(clen) = le(block, p) else { break };
        p += 4;
        let Some(s) = block.get(p..p + clen) else {
            break;
        };
        p += clen;
        if let Ok(s) = std::str::from_utf8(s) {
            // Only the first '=' separates, so a value may itself contain '=' (section 8.6).
            if let Some((key, value)) = s.split_once('=') {
                sink.text(upper_key(key, &mut key_buf), value);
            }
        }
    }
}

/// A name longer than `buf` passes through unchanged. [`parse`]'s sink re-uppercases it.
fn upper_key<'k>(key: &'k str, buf: &'k mut [u8; KEY_STACK_MAX]) -> &'k str {
    let raw = key.as_bytes();
    if raw.len() > buf.len() || !raw.iter().any(u8::is_ascii_lowercase) {
        return key;
    }
    let out = &mut buf[..raw.len()];
    for (dst, &src) in out.iter_mut().zip(raw) {
        *dst = src.to_ascii_uppercase();
    }
    std::str::from_utf8(out).unwrap_or(key)
}

fn picture_into(block: &[u8], sink: &mut impl TagSink) {
    let be = |b: &[u8], p: usize| {
        b.get(p..p + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    let mut p = 4; // skip picture type
    let Some(mime_len) = be(block, p) else { return };
    p += 4;
    let Some(mime) = block.get(p..p + mime_len).and_then(|s| std::str::from_utf8(s).ok()) else {
        return;
    };
    p += mime_len;
    let Some(desc_len) = be(block, p) else { return };
    p += 4 + desc_len; // skip description
    p += 16; // width, height, colour depth, indexed-colour count, 4 bytes each
    let Some(data_len) = be(block, p) else { return };
    p += 4;
    let Some(data) = block.get(p..p + data_len) else {
        return;
    };
    sink.picture(mime, data);
}

fn parse_streaminfo(block: &[u8]) -> Option<StreamInfo> {
    let b = block.get(..STREAMINFO_LEN)?;
    // Bits 80..100: sample rate. Bytes 10, 11 and the high nibble of 12.
    let sample_rate = (b[10] as u32) << 12 | (b[11] as u32) << 4 | (b[12] as u32) >> 4;
    // Bits 100..103: channels - 1. Bits 103..108: bits per sample - 1 (straddling bytes 12/13).
    let channels = ((b[12] >> 1) & 0x07) + 1;
    let bits_per_sample = (((b[12] & 0x01) << 4) | (b[13] >> 4)) + 1;
    // Bits 108..144: total samples, the low nibble of byte 13 then four whole bytes.
    let total_samples =
        ((b[13] & 0x0F) as u64) << 32 | u32::from_be_bytes([b[14], b[15], b[16], b[17]]) as u64;
    Some(StreamInfo { sample_rate, channels, bits_per_sample, total_samples })
}

struct OwningSink<'t>(&'t mut FlacTags);

// One-time header parsing.
#[allow(clippy::disallowed_methods)]
impl TagSink for OwningSink<'_> {
    fn text(&mut self, key: &str, value: &str) {
        self.0.comments.push((key.to_ascii_uppercase(), value.to_string()));
    }

    fn picture(&mut self, mime: &str, data: &[u8]) {
        self.0.pictures.push((mime.to_string(), data.to_vec()));
    }
}

#[cfg(test)]
// Test fixtures.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn flac_with_comments(comments: &[&str]) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        out.push(0);
        out.extend_from_slice(&[0, 0, 34]);
        out.extend_from_slice(&[0u8; 34]);
        let mut body = Vec::new();
        let vendor = b"ref";
        body.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        body.extend_from_slice(vendor);
        body.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            body.extend_from_slice(&(c.len() as u32).to_le_bytes());
            body.extend_from_slice(c.as_bytes());
        }
        out.push(0x80 | 4);
        let len = body.len() as u32;
        out.extend_from_slice(&len.to_be_bytes()[1..]);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn parses_vorbis_comments() {
        let flac =
            flac_with_comments(&["TITLE=Set Theory", "ARTIST=Carbon Based Lifeforms", "DATE=2015"]);
        let tags = parse(&flac);
        assert_eq!(
            tags.comments,
            vec![
                ("TITLE".into(), "Set Theory".into()),
                ("ARTIST".into(), "Carbon Based Lifeforms".into()),
                ("DATE".into(), "2015".into()),
            ]
        );
        assert!(tags.pictures.is_empty());
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn lowercase_keys_are_uppercased_values_kept() {
        let tags = parse(&flac_with_comments(&["title=x", "Comment=hi=there"]));
        assert_eq!(tags.comments[0], ("TITLE".into(), "x".into()));
        assert_eq!(tags.comments[1], ("COMMENT".into(), "hi=there".into()));
    }

    #[test]
    fn non_flac_or_empty_is_empty() {
        assert!(parse(b"not a flac file").is_empty());
        assert!(parse(&[]).is_empty());
        assert!(parse(b"fLaC").is_empty()); // marker only, no blocks
    }

    fn picture_block_body(mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&(3u32).to_be_bytes()); // description length
        b.extend_from_slice(b"pic"); // description
        b.extend_from_slice(&[0u8; 16]); // width, height, colour depth, indexed colours
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn parses_pictures() {
        let mut flac = b"fLaC".to_vec();
        flac.push(0);
        flac.extend_from_slice(&[0, 0, 34]);
        flac.extend_from_slice(&[0u8; 34]);
        let body = picture_block_body("image/jpeg", b"\xFF\xD8\xFF\xE0jpeg-ish bytes");
        flac.push(0x80 | 6); // last-block, PICTURE
        flac.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        flac.extend_from_slice(&body);

        let tags = parse(&flac);
        assert_eq!(tags.pictures.len(), 1);
        assert_eq!(tags.pictures[0].0, "image/jpeg");
        assert_eq!(tags.pictures[0].1, b"\xFF\xD8\xFF\xE0jpeg-ish bytes");
        assert!(tags.comments.is_empty());
    }

    fn flac_with_comment_and_picture() -> Vec<u8> {
        let mut flac = b"fLaC".to_vec();
        flac.push(0);
        flac.extend_from_slice(&[0, 0, 34]);
        flac.extend_from_slice(&[0u8; 34]);
        let mut vc = Vec::new();
        vc.extend_from_slice(&3u32.to_le_bytes());
        vc.extend_from_slice(b"ref");
        vc.extend_from_slice(&1u32.to_le_bytes());
        vc.extend_from_slice(&("TITLE=X".len() as u32).to_le_bytes());
        vc.extend_from_slice(b"TITLE=X");
        flac.push(4);
        flac.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
        flac.extend_from_slice(&vc);
        let pic = picture_block_body("image/png", b"\x89PNG...");
        flac.push(0x80 | 6);
        flac.extend_from_slice(&(pic.len() as u32).to_be_bytes()[1..]);
        flac.extend_from_slice(&pic);
        flac
    }

    #[test]
    fn parses_comments_and_a_picture_together() {
        let tags = parse(&flac_with_comment_and_picture());
        assert_eq!(tags.comments, vec![("TITLE".to_string(), "X".to_string())]);
        assert_eq!(tags.pictures.len(), 1);
        assert_eq!(tags.pictures[0].0, "image/png");
        assert_eq!(tags.pictures[0].1, b"\x89PNG...");
    }

    struct Rec {
        text: Vec<(String, String)>,
        pictures: Vec<(String, Vec<u8>)>,
        picture_spans: Vec<(usize, usize)>,
        base: usize,
        len: usize,
    }

    impl Rec {
        fn over(input: &[u8]) -> Self {
            Self {
                text: Vec::new(),
                pictures: Vec::new(),
                picture_spans: Vec::new(),
                base: input.as_ptr() as usize,
                len: input.len(),
            }
        }
    }

    impl TagSink for Rec {
        fn text(&mut self, key: &str, value: &str) {
            self.text.push((key.to_string(), value.to_string()));
        }
        fn picture(&mut self, mime: &str, data: &[u8]) {
            let off = (data.as_ptr() as usize)
                .checked_sub(self.base)
                .expect("picture bytes must borrow the input, not a copy");
            assert!(off + data.len() <= self.len, "picture bytes must borrow the input");
            self.picture_spans.push((off, data.len()));
            self.pictures.push((mime.to_string(), data.to_vec()));
        }
    }

    #[test]
    fn parse_into_matches_parse_and_borrows_the_picture() {
        let flac = flac_with_comment_and_picture();
        let owned = parse(&flac);

        let mut rec = Rec::over(&flac);
        parse_into(&flac, &mut rec);

        assert_eq!(rec.text, owned.comments);
        assert_eq!(rec.pictures, owned.pictures);
        assert_eq!(rec.picture_spans.len(), 1);
        let (off, len) = rec.picture_spans[0];
        assert_eq!(&flac[off..off + len], &owned.pictures[0].1[..]);
    }

    #[test]
    fn parse_into_matches_parse_across_fixtures() {
        let fixtures = [
            flac_with_comments(&["TITLE=Set Theory", "artist=x", "ARTIST=y", "DATE=2015"]),
            flac_with_comments(&["title=x", "Comment=hi=there", "MixedCase=Value"]),
            flac_with_comments(&[]),
            flac_with_comment_and_picture(),
            b"not a flac file".to_vec(),
            b"fLaC".to_vec(),
            Vec::new(),
        ];
        for flac in fixtures {
            let owned = parse(&flac);
            let mut rec = Rec::over(&flac);
            parse_into(&flac, &mut rec);
            assert_eq!(rec.text, owned.comments);
            assert_eq!(rec.pictures, owned.pictures);
        }
    }

    #[test]
    fn parse_into_truncations_never_panic() {
        let flac = flac_with_comment_and_picture();
        for n in 0..flac.len() {
            let mut rec = Rec::over(&flac[..n]);
            parse_into(&flac[..n], &mut rec);
            assert_eq!(rec.text, parse(&flac[..n]).comments, "prefix of {n} bytes");
        }
    }

    #[test]
    fn oversized_field_name_survives_uncopied() {
        let key = "k".repeat(KEY_STACK_MAX + 1);
        let entry = format!("{key}=v");
        let tags = parse(&flac_with_comments(&[&entry]));
        assert_eq!(tags.comments, vec![(key.to_ascii_uppercase(), "v".to_string())]);
    }

    fn streaminfo_body(rate: u32, channels: u8, bits: u8, total: u64) -> Vec<u8> {
        let mut b = vec![0u8; 34];
        b[0..2].copy_from_slice(&4096u16.to_be_bytes()); // min block size
        b[2..4].copy_from_slice(&4096u16.to_be_bytes()); // max block size
        b[10] = (rate >> 12) as u8;
        b[11] = (rate >> 4) as u8;
        b[12] = ((rate << 4) as u8 & 0xF0) | ((channels - 1) << 1) | ((bits - 1) >> 4);
        b[13] = (((bits - 1) & 0x0F) << 4) | ((total >> 32) as u8 & 0x0F);
        b[14..18].copy_from_slice(&(total as u32).to_be_bytes());
        b
    }

    fn flac_with_streaminfo(body: &[u8]) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        out.push(0x80); // last block, type 0
        out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn reads_stream_info() {
        for (rate, channels, bits, total) in [
            (44_100u32, 2u8, 16u8, 9_876_543u64),
            (48_000, 1, 24, 0),
            (8_000, 8, 4, 1),
            (1_048_575, 5, 32, (1u64 << 36) - 1),
            (37_337, 3, 20, 5_000_000_000),
            (0, 1, 8, 0), // non-audio stream, section 8.2
        ] {
            let flac = flac_with_streaminfo(&streaminfo_body(rate, channels, bits, total));
            assert_eq!(
                stream_info(&flac),
                Some(StreamInfo {
                    sample_rate: rate,
                    channels,
                    bits_per_sample: bits,
                    total_samples: total
                }),
                "rate {rate} ch {channels} bits {bits} total {total}"
            );
        }
    }

    #[test]
    fn stream_info_missing_or_truncated() {
        assert_eq!(stream_info(b"not a flac file"), None);
        assert_eq!(stream_info(&[]), None);
        assert_eq!(stream_info(b"fLaC"), None);
        let mut no_info = b"fLaC".to_vec();
        no_info.push(0x80 | 4);
        no_info.extend_from_slice(&[0, 0, 4]);
        no_info.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(stream_info(&no_info), None);
        let flac = flac_with_streaminfo(&streaminfo_body(44_100, 2, 16, 1_000));
        for n in 0..flac.len() {
            assert_eq!(stream_info(&flac[..n]), None, "truncated to {n}");
        }
        assert!(stream_info(&flac).is_some());
        let mut short = b"fLaC".to_vec();
        short.push(0x80);
        short.extend_from_slice(&[0, 0, 20]);
        short.extend_from_slice(&[0u8; 20]);
        assert_eq!(stream_info(&short), None);
    }

    fn seek_point(sample: u64, offset: u64, frame_samples: u16) -> Vec<u8> {
        let mut p = sample.to_be_bytes().to_vec();
        p.extend_from_slice(&offset.to_be_bytes());
        p.extend_from_slice(&frame_samples.to_be_bytes());
        p
    }

    fn placeholder() -> Vec<u8> {
        seek_point(u64::MAX, 0xDEAD_BEEF, 0xFFFF)
    }

    fn flac_with_seektable(points: &[Vec<u8>], pad_len: usize) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        let mut block = |kind: u8, last: bool, body: &[u8]| {
            out.push(if last { 0x80 | kind } else { kind });
            out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            out.extend_from_slice(body);
        };
        block(0, false, &[0u8; STREAMINFO_LEN]);
        if !points.is_empty() {
            block(SEEKTABLE_TYPE, false, &points.concat());
        }
        block(1, true, &vec![0u8; pad_len]); // PADDING, last block
        out
    }

    #[test]
    fn seek_table_points_carry_sample_offset_and_frame_length() {
        let points = [
            seek_point(0, 0, 4096),
            seek_point(44_100, 12_345, 4096),
            seek_point(88_200, 25_000, 2048),
        ];
        let flac = flac_with_seektable(&points, 0);
        let got: Vec<SeekPoint> = seek_table(&flac).collect();
        assert_eq!(
            got,
            vec![
                SeekPoint { sample: 0, byte_offset: 0, frame_samples: 4096 },
                SeekPoint { sample: 44_100, byte_offset: 12_345, frame_samples: 4096 },
                SeekPoint { sample: 88_200, byte_offset: 25_000, frame_samples: 2048 },
            ]
        );
        assert!(got.windows(2).all(|w| w[0].sample < w[1].sample));
    }

    #[test]
    fn placeholder_points_are_skipped() {
        let points = [
            seek_point(0, 0, 4096),
            seek_point(44_100, 12_345, 4096),
            seek_point(88_200, 25_000, 4096),
            placeholder(),
            placeholder(),
            placeholder(),
        ];
        let got: Vec<SeekPoint> = seek_table(&flac_with_seektable(&points, 0)).collect();
        assert_eq!(got.len(), 3, "the three reserved slots are not seek points");
        assert!(got.iter().all(|p| p.byte_offset != 0xDEAD_BEEF));
        assert_eq!(got.last().unwrap().sample, 88_200);

        let all_pad = [placeholder(), placeholder()];
        assert_eq!(seek_table(&flac_with_seektable(&all_pad, 0)).count(), 0);
    }

    #[test]
    fn audio_start_is_the_end_of_the_metadata_chain() {
        let points = [seek_point(0, 0, 4096), seek_point(44_100, 9_000, 4096)];
        let flac = flac_with_seektable(&points, 64);
        assert_eq!(audio_start(&flac), Some(4 + 38 + 40 + 68));
        assert_eq!(audio_start(&flac), Some(flac.len()), "the frames start right after");

        let mut with_frames = flac.clone();
        with_frames.extend_from_slice(&[0xFF, 0xF8, 0x00, 0x00]);
        assert_eq!(audio_start(&with_frames), Some(flac.len()));

        assert_eq!(audio_start(&flac[..flac.len() - 1]), None);
        assert_eq!(audio_start(b"fLaC"), None);
        assert_eq!(audio_start(b"not a flac file"), None);
        assert_eq!(audio_start(&[]), None);
    }

    fn id3_tag(body: usize, footer: bool) -> Vec<u8> {
        let size = body as u32;
        let mut tag = b"ID3\x04\x00".to_vec();
        tag.push(if footer { 0x10 } else { 0 });
        tag.extend((0..4).rev().map(|i| ((size >> (7 * i)) & 0x7F) as u8));
        tag.resize(10 + body, 0xFF);
        if footer {
            tag.extend_from_slice(b"3DI\x04\x00\x10");
            tag.extend((0..4).rev().map(|i| ((size >> (7 * i)) & 0x7F) as u8));
        }
        tag
    }

    #[test]
    fn a_leading_id3v2_tag_is_skipped_by_every_reader() {
        let flac = flac_with_comment_and_picture();
        let bare = (parse(&flac), stream_info(&flac), audio_start(&flac));
        for footer in [false, true] {
            let tag = id3_tag(200, footer);
            assert_eq!(id3v2_len(&tag), Some(tag.len()));
            let tagged = [&tag[..], &flac[..]].concat();
            assert_eq!(parse(&tagged), bare.0);
            assert_eq!(stream_info(&tagged), bare.1);
            assert_eq!(
                audio_start(&tagged),
                bare.2.map(|n| n + tag.len()),
                "counted from the file's first byte"
            );
            for n in 0..tagged.len() {
                assert_eq!(audio_start(&tagged[..n]), None, "cut at {n}");
            }
        }
        let mut bad = id3_tag(16, false);
        bad[9] |= 0x80;
        assert_eq!(id3v2_len(&bad), None);
        assert_eq!(audio_start(&[&bad[..], &flac[..]].concat()), None);
        assert_eq!(audio_start(&id3_tag(16, false)), None);
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00"), None, "a short header");
        let twice = [&id3_tag(4, false)[..], &id3_tag(4, false)[..], &flac[..]].concat();
        assert_eq!(audio_start(&twice), None);
    }

    #[test]
    fn a_stream_without_a_seektable_has_an_empty_index() {
        assert_eq!(seek_table(&flac_with_seektable(&[], 16)).count(), 0);
        assert_eq!(seek_table(&flac_with_comments(&["TITLE=x"])).count(), 0);
        assert_eq!(seek_table(b"not a flac file").count(), 0);
        assert_eq!(seek_table(&[]).count(), 0);
    }

    #[test]
    fn a_ragged_or_truncated_table_yields_its_whole_points() {
        let mut body = [seek_point(0, 0, 4096), seek_point(44_100, 9_000, 4096)].concat();
        body.extend_from_slice(&[0xAB; 7]);
        let mut flac = b"fLaC".to_vec();
        flac.push(0);
        flac.extend_from_slice(&(STREAMINFO_LEN as u32).to_be_bytes()[1..]);
        flac.extend_from_slice(&[0u8; STREAMINFO_LEN]);
        flac.push(0x80 | SEEKTABLE_TYPE);
        flac.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        flac.extend_from_slice(&body);
        assert_eq!(seek_table(&flac).count(), 2);

        let full = flac_with_seektable(&[seek_point(0, 0, 4096), placeholder()], 8);
        for n in 0..=full.len() {
            let points: Vec<SeekPoint> = seek_table(&full[..n]).collect();
            assert!(points.len() <= 1, "cut at {n} yielded {points:?}");
            assert!(points.iter().all(|p| p.sample == 0));
            let _ = audio_start(&full[..n]);
            let _ = stream_info(&full[..n]);
        }
        let mut lying = b"fLaC".to_vec();
        lying.push(SEEKTABLE_TYPE);
        lying.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
        lying.extend_from_slice(&seek_point(0, 0, 4096));
        assert_eq!(seek_table(&lying).count(), 0);
        assert_eq!(audio_start(&lying), None);
    }
}
