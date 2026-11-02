//! Vorbis comments, the metadata block Vorbis, Opus and FLAC share (Vorbis I section 5,
//! RFC 7845 section 5.2, RFC 9639 section 8.6).

use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

/// Cover art, a FLAC picture block (RFC 9639 section 8.8) base64-encoded into the value.
const PICTURE_FIELD: &str = "METADATA_BLOCK_PICTURE";

/// Longest field name uppercased on the stack. Longer ones fall back to the scratch arena, so
/// hostile input forces neither an allocation nor a truncation.
const KEY_STACK_MAX: usize = 64;

/// Parse the body of a Vorbis comment block, the bytes after any codec magic (Vorbis I 5.2.2.1).
pub fn parse_comment_body(body: &[u8], scratch: &Arena, sink: &mut impl TagSink) {
    comments_into(body, false, scratch, sink);
}

/// [`parse_comment_body`] keeping an entry that is no UTF-8, each invalid run as U+FFFD.
pub fn parse_comment_body_lossy(body: &[u8], scratch: &Arena, sink: &mut impl TagSink) {
    comments_into(body, true, scratch, sink);
}

/// The decoded `METADATA_BLOCK_PICTURE` blocks of a comment body in file order, unchecked.
pub fn picture_blocks<'a>(body: &'a [u8], scratch: &'a Arena) -> PictureBlocks<'a> {
    PictureBlocks { entries: Entries::new(body), scratch }
}

/// What [`picture_blocks`] returns, FLAC `PICTURE` block bodies (RFC 9639 section 8.8).
pub struct PictureBlocks<'a> {
    entries: Entries<'a>,
    scratch: &'a Arena,
}

impl<'a> Iterator for PictureBlocks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        for entry in self.entries.by_ref() {
            let Some(eq) = entry.iter().position(|&b| b == b'=') else { continue };
            if !entry[..eq].eq_ignore_ascii_case(PICTURE_FIELD.as_bytes()) {
                continue;
            }
            if let Some(block) = base64_decode(&entry[eq + 1..], self.scratch) {
                return Some(block);
            }
        }
        None
    }
}

/// The raw entries of a comment body, stopping at the first one cut short.
struct Entries<'a> {
    body: &'a [u8],
    pos: usize,
    left: usize,
}

impl<'a> Entries<'a> {
    fn new(body: &'a [u8]) -> Self {
        let count_at = le_u32(body, 0).and_then(|v| 4usize.checked_add(v));
        match count_at.and_then(|at| Some((at, le_u32(body, at)?))) {
            Some((at, left)) => Entries { body, pos: at + 4, left },
            None => Entries { body, pos: 0, left: 0 },
        }
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let entry = le_u32(self.body, self.pos).and_then(|len| {
            let at = self.pos + 4;
            Some((at + len, self.body.get(at..)?.get(..len)?))
        });
        let Some((next, entry)) = entry else {
            self.left = 0;
            return None;
        };
        self.pos = next;
        Some(entry)
    }
}

/// `raw` with each invalid run as U+FFFD, in the arena when it needs that.
fn lossy<'a>(raw: &'a [u8], scratch: &'a Arena) -> Option<&'a str> {
    if let Ok(s) = std::str::from_utf8(raw) {
        return Some(s);
    }
    const FFFD: &[u8] = "\u{FFFD}".as_bytes();
    let len: usize = raw
        .utf8_chunks()
        .map(|c| c.valid().len() + if c.invalid().is_empty() { 0 } else { FFFD.len() })
        .sum();
    let out = scratch.alloc_bytes(len).ok()?;
    let mut at = 0;
    for c in raw.utf8_chunks() {
        out[at..at + c.valid().len()].copy_from_slice(c.valid().as_bytes());
        at += c.valid().len();
        if !c.invalid().is_empty() {
            out[at..at + FFFD.len()].copy_from_slice(FFFD);
            at += FFFD.len();
        }
    }
    std::str::from_utf8(out).ok()
}

fn comments_into(body: &[u8], keep_invalid: bool, scratch: &Arena, sink: &mut impl TagSink) {
    let mut key_buf = [0u8; KEY_STACK_MAX];
    for entry in Entries::new(body) {
        // Split at the first '=' (Vorbis I section 5.2.2.1), a value may itself contain '='.
        let entry = match std::str::from_utf8(entry) {
            Ok(s) => s,
            Err(_) if keep_invalid => match lossy(entry, scratch) {
                Some(s) => s,
                None => continue,
            },
            Err(_) => continue,
        };
        let Some((raw_key, value)) = entry.split_once('=') else {
            continue;
        };
        let Some(key) = upper_key(raw_key.as_bytes(), &mut key_buf, scratch) else {
            continue;
        };

        if key == PICTURE_FIELD {
            if let Some(block) = base64_decode(value.as_bytes(), scratch)
                && let Some((kind, mime, data)) = parse_picture_block(block)
            {
                sink.picture_typed(kind, mime, data);
            }
            // A picture that will not decode is dropped, not emitted as base64 text.
            continue;
        }
        sink.text(key, value);
    }
}

/// Parse an Opus comment header packet (RFC 7845 section 5.2). False if the magic is absent.
pub fn parse_opus_tags(packet: &[u8], scratch: &Arena, sink: &mut impl TagSink) -> bool {
    let Some(body) = packet.strip_prefix(b"OpusTags") else {
        return false;
    };
    parse_comment_body(body, scratch, sink);
    true
}

/// Parse a Vorbis comment header packet (Vorbis I section 4.2.1). False if it is not one.
pub fn parse_vorbis_comments(packet: &[u8], scratch: &Arena, sink: &mut impl TagSink) -> bool {
    let Some(body) = packet.strip_prefix(b"\x03vorbis") else {
        return false;
    };
    parse_comment_body(body, scratch, sink);
    true
}

/// Comment lengths are little-endian (Vorbis I section 5.2.2.1), unlike FLAC's other fields.
fn le_u32(b: &[u8], pos: usize) -> Option<usize> {
    let s = b.get(pos..)?.get(..4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
}

fn upper_key<'k>(
    raw: &[u8],
    stack: &'k mut [u8; KEY_STACK_MAX],
    scratch: &'k Arena,
) -> Option<&'k str> {
    if raw.is_empty() || !raw.iter().all(|&b| (0x20..=0x7E).contains(&b) && b != b'=') {
        return None;
    }
    let buf: &'k mut [u8] = if raw.len() <= KEY_STACK_MAX {
        &mut stack[..raw.len()]
    } else {
        scratch.alloc_bytes(raw.len()).ok()?
    };
    for (dst, &src) in buf.iter_mut().zip(raw) {
        *dst = src.to_ascii_uppercase();
    }
    std::str::from_utf8(buf).ok()
}

fn base64_decode<'a>(input: &[u8], scratch: &'a Arena) -> Option<&'a [u8]> {
    let mut pad = 0;
    let mut n = input.len();
    while pad < 2 && n > 0 && input[n - 1] == b'=' {
        n -= 1;
        pad += 1;
    }
    let body = &input[..n];
    let rem = body.len() % 4;
    if rem == 1 || (pad > 0 && (rem + pad != 4)) {
        return None;
    }
    let out_len = body.len() / 4 * 3 + rem.saturating_sub(1);
    let out = scratch.alloc_bytes(out_len).ok()?;

    let full = body.len() / 4;
    let (main, tail) = out.split_at_mut(full * 3);
    let (groups, _) = body.as_chunks::<4>();
    let (outs, _) = main.as_chunks_mut::<3>();
    for (q, o) in groups.iter().zip(outs) {
        let a = DECODE[q[0] as usize] as u32;
        let b = DECODE[q[1] as usize] as u32;
        let c = DECODE[q[2] as usize] as u32;
        let d = DECODE[q[3] as usize] as u32;
        if (a | b | c | d) & 0xC0 != 0 {
            return None;
        }
        let v = (a << 18) | (b << 12) | (c << 6) | d;
        o[0] = (v >> 16) as u8;
        o[1] = (v >> 8) as u8;
        o[2] = v as u8;
    }

    // Trailing bits are dropped rather than checked for zero, which RFC 4648 section 3.5 leaves
    // optional.
    let rest = &body[full * 4..];
    if rest.len() >= 2 {
        let a = DECODE[rest[0] as usize] as u32;
        let b = DECODE[rest[1] as usize] as u32;
        let c = if rest.len() == 3 { DECODE[rest[2] as usize] as u32 } else { 0 };
        if (a | b | c) & 0xC0 != 0 {
            return None;
        }
        tail[0] = ((a << 2) | (b >> 4)) as u8;
        if rest.len() == 3 {
            tail[1] = ((b << 4) | (c >> 2)) as u8;
        }
    }
    Some(out)
}

/// Not a base64 character. Above 63, so OR-ing a group and testing `0xC0` catches any invalid byte.
const BAD: u8 = 0xFF;

const DECODE: [u8; 256] = {
    let mut t = [BAD; 256];
    let mut i = 0usize;
    while i < 26 {
        t[b'A' as usize + i] = i as u8;
        t[b'a' as usize + i] = (i + 26) as u8;
        i += 1;
    }
    let mut d = 0usize;
    while d < 10 {
        t[b'0' as usize + d] = (d + 52) as u8;
        d += 1;
    }
    t[b'+' as usize] = 62;
    t[b'/' as usize] = 63;
    t
};

/// Parse a FLAC `PICTURE` block (RFC 9639 section 8.8, Table 12) into `(type, media type, image
/// bytes)`, a type past 255 as 255.
fn parse_picture_block(block: &[u8]) -> Option<(u8, &str, &[u8])> {
    let be_u32 = |pos: usize| -> Option<usize> {
        let s = block.get(pos..)?.get(..4)?;
        Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    let kind = u8::try_from(be_u32(0)?).unwrap_or(u8::MAX);
    let mut p = 4;
    let mime_len = be_u32(p)?;
    p += 4;
    let mime = block.get(p..)?.get(..mime_len)?;
    if !mime.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
        return None;
    }
    let mime = std::str::from_utf8(mime).ok()?;
    p += mime_len;
    let desc_len = be_u32(p)?;
    p = p.checked_add(4)?.checked_add(desc_len)?.checked_add(16)?;
    let data_len = be_u32(p)?;
    p += 4;
    let data = block.get(p..)?.get(..data_len)?;
    Some((kind, mime, data))
}

#[cfg(test)]
// Tests build fixtures and record what the sink was handed.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec {
        text: Vec<(String, String)>,
        pictures: Vec<(String, Vec<u8>)>,
    }

    impl TagSink for Rec {
        fn text(&mut self, key: &str, value: &str) {
            self.text.push((key.to_string(), value.to_string()));
        }
        fn picture(&mut self, mime: &str, data: &[u8]) {
            self.pictures.push((mime.to_string(), data.to_vec()));
        }
    }

    fn body(vendor: &str, entries: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        b.extend_from_slice(vendor.as_bytes());
        b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            b.extend_from_slice(&(e.len() as u32).to_le_bytes());
            b.extend_from_slice(e.as_bytes());
        }
        b
    }

    fn picture_block(mime: &str, desc: &str, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes()); // type 3: front cover
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&(desc.len() as u32).to_be_bytes());
        b.extend_from_slice(desc.as_bytes());
        b.extend_from_slice(&[0u8; 16]); // width, height, depth, colours
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }

    fn base64_encode(data: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for chunk in data.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
            s.push(A[(n >> 18) as usize & 63] as char);
            s.push(A[(n >> 12) as usize & 63] as char);
            s.push(if chunk.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
            s.push(if chunk.len() > 2 { A[n as usize & 63] as char } else { '=' });
        }
        s
    }

    fn arena() -> Arena {
        Arena::default()
    }

    #[test]
    fn parses_entries_uppercases_keys_and_keeps_repeats() {
        let b = body(
            "reference libFLAC",
            &[
                "TITLE=Set Theory",
                "artist=Carbon Based Lifeforms",
                "ARTIST=Second Artist",
                "REPLAYGAIN_TRACK_GAIN=-7.18 dB",
                "Comment=hi=there",
            ],
        );
        let mut rec = Rec::default();
        parse_comment_body(&b, &arena(), &mut rec);
        assert_eq!(
            rec.text,
            vec![
                ("TITLE".to_string(), "Set Theory".to_string()),
                ("ARTIST".to_string(), "Carbon Based Lifeforms".to_string()),
                ("ARTIST".to_string(), "Second Artist".to_string()),
                ("REPLAYGAIN_TRACK_GAIN".to_string(), "-7.18 dB".to_string()),
                ("COMMENT".to_string(), "hi=there".to_string()),
            ]
        );
        assert!(rec.pictures.is_empty());
    }

    #[test]
    fn skips_malformed_entries_and_continues() {
        let entries: [&[u8]; 6] = [
            b"TITLE=ok",
            b"novalue",
            b"=novalue",
            b"BAD\x7FKEY=x",
            b"NAME=\xFF\xFE",
            b"ALBUM=fine",
        ];
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes());
        b.push(b'v');
        b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            b.extend_from_slice(&(e.len() as u32).to_le_bytes());
            b.extend_from_slice(e);
        }

        let mut rec = Rec::default();
        parse_comment_body(&b, &arena(), &mut rec);
        assert_eq!(
            rec.text,
            vec![
                ("TITLE".to_string(), "ok".to_string()),
                ("ALBUM".to_string(), "fine".to_string())
            ]
        );
    }

    #[test]
    fn tilde_field_name_accepted_rfc9639_range() {
        // Vorbis I stops at 0x7D, RFC 9639 section 8.6 allows 0x7E. We accept the union.
        let mut rec = Rec::default();
        parse_comment_body(&body("v", &["A~B=x"]), &arena(), &mut rec);
        assert_eq!(rec.text, vec![("A~B".to_string(), "x".to_string())]);
    }

    #[test]
    fn picture_round_trips_through_base64() {
        let jpeg = b"\xFF\xD8\xFF\xE0jpeg-ish bytes";
        let block = picture_block("image/jpeg", "front", jpeg);
        let entry = format!("METADATA_BLOCK_PICTURE={}", base64_encode(&block));
        let mut rec = Rec::default();
        parse_comment_body(&body("v", &["TITLE=x", &entry]), &arena(), &mut rec);
        assert_eq!(rec.text, vec![("TITLE".to_string(), "x".to_string())]);
        assert_eq!(rec.pictures.len(), 1);
        assert_eq!(rec.pictures[0].0, "image/jpeg");
        assert_eq!(rec.pictures[0].1, jpeg);
    }

    #[test]
    fn a_picture_states_its_type() {
        struct Kinds(Vec<u8>);
        impl TagSink for Kinds {
            fn text(&mut self, _key: &str, _value: &str) {}
            fn picture(&mut self, _mime: &str, _data: &[u8]) {
                self.0.push(0);
            }
            fn picture_typed(&mut self, kind: u8, _mime: &str, _data: &[u8]) {
                self.0.push(kind);
            }
        }
        let mut block = picture_block("image/png", "", b"\x89PNG");
        let mut entries = Vec::new();
        for kind in [3u32, 8, 300] {
            block[..4].copy_from_slice(&kind.to_be_bytes());
            entries.push(format!("METADATA_BLOCK_PICTURE={}", base64_encode(&block)));
        }
        let entries: Vec<&str> = entries.iter().map(String::as_str).collect();
        let mut k = Kinds(Vec::new());
        parse_comment_body(&body("v", &entries), &arena(), &mut k);
        assert_eq!(k.0, [3, 8, 255], "past 255 as 255");
    }

    fn raw_body(entries: &[&[u8]]) -> Vec<u8> {
        let mut b = 1u32.to_le_bytes().to_vec();
        b.push(b'v');
        b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            b.extend_from_slice(&(e.len() as u32).to_le_bytes());
            b.extend_from_slice(e);
        }
        b
    }

    #[test]
    fn the_lossy_parse_keeps_an_entry_that_is_no_utf8() {
        let b = raw_body(&[b"TITLE=Caf\xE9 \xFF\xFEx", b"ALBUM=ok"]);
        let mut rec = Rec::default();
        parse_comment_body(&b, &arena(), &mut rec);
        assert_eq!(rec.text, vec![("ALBUM".to_string(), "ok".to_string())], "the strict parse");
        let mut rec = Rec::default();
        parse_comment_body_lossy(&b, &arena(), &mut rec);
        assert_eq!(
            rec.text,
            vec![
                ("TITLE".to_string(), "Caf\u{FFFD} \u{FFFD}\u{FFFD}x".to_string()),
                ("ALBUM".to_string(), "ok".to_string())
            ]
        );
        for n in 0..b.len() {
            parse_comment_body_lossy(&b[..n], &arena(), &mut Rec::default());
        }
    }

    #[test]
    fn picture_blocks_come_decoded_in_order() {
        let one = picture_block("image/png", "", b"\x89PNG");
        let two = picture_block("image/jpeg", "", b"\xFF\xD8\xFF");
        let e1 = format!("METADATA_BLOCK_PICTURE={}", base64_encode(&one));
        let e2 = format!("metadata_block_picture={}", base64_encode(&two));
        let b = body("v", &["TITLE=x", &e1, "METADATA_BLOCK_PICTURE=!!", &e2]);
        let a = arena();
        let got: Vec<&[u8]> = picture_blocks(&b, &a).collect();
        assert_eq!(got, [one.as_slice(), two.as_slice()]);
        let a = arena();
        assert_eq!(picture_blocks(&b[..b.len() - 1], &a).count(), 1, "cut short");
    }

    #[test]
    fn picture_covers_all_base64_pad_variants() {
        let mut seen = Vec::new();
        for extra in 0..3usize {
            let data = b"IMG";
            let block = picture_block("image/png", &"d".repeat(extra), data);
            let b64 = base64_encode(&block);
            let pad: String = b64.chars().rev().take_while(|&c| c == '=').collect();
            let want_pad = match block.len() % 3 {
                0 => "",
                1 => "==",
                _ => "=",
            };
            assert_eq!(pad, want_pad, "block len {} encodes with pad {want_pad:?}", block.len());
            seen.push(pad);

            let entry = format!("METADATA_BLOCK_PICTURE={b64}");
            let mut rec = Rec::default();
            parse_comment_body(&body("v", &[&entry]), &arena(), &mut rec);
            assert_eq!(rec.pictures.len(), 1, "pad {want_pad:?} must decode");
            assert_eq!(rec.pictures[0].1, data);

            let entry = format!("METADATA_BLOCK_PICTURE={}", b64.trim_end_matches('='));
            let mut rec = Rec::default();
            parse_comment_body(&body("v", &[&entry]), &arena(), &mut rec);
            assert_eq!(rec.pictures.len(), 1, "unpadded pad {want_pad:?} must decode");
            assert_eq!(rec.pictures[0].1, data);
        }
        seen.sort_unstable();
        assert_eq!(seen, ["", "=", "=="], "all three RFC 4648 §4 pad shapes must be covered");
    }

    #[test]
    fn invalid_base64_skips_the_entry_and_parsing_continues() {
        let good = base64_encode(&picture_block("image/png", "", b"IMG"));
        for bad in [
            format!("{good}extra=chars"),            // '=' in the middle
            format!("{}!", &good[..good.len() - 1]), // non-alphabet byte
            good[..good.len() - 3].to_string(),      // remainder of 1 character
            "A".to_string(),                         // remainder of 1 character, whole input
            format!("{good}===="),                   // over-padded
        ] {
            let entry = format!("METADATA_BLOCK_PICTURE={bad}");
            let mut rec = Rec::default();
            parse_comment_body(
                &body("v", &["TITLE=before", &entry, "ALBUM=after"]),
                &arena(),
                &mut rec,
            );
            assert!(rec.pictures.is_empty(), "must not decode {bad:?}");
            assert_eq!(
                rec.text,
                vec![
                    ("TITLE".to_string(), "before".to_string()),
                    ("ALBUM".to_string(), "after".to_string())
                ],
                "the walk must continue past {bad:?}"
            );
        }
    }

    #[test]
    fn base64_decodes_rfc4648_test_vectors() {
        // RFC 4648 section 10 test vectors.
        let a = arena();
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_decode(encoded.as_bytes(), &a), Some(plain.as_bytes()), "{encoded}");
        }
    }

    #[test]
    fn table_decode_matches_bitwise_reference() {
        fn reference(input: &[u8]) -> Option<Vec<u8>> {
            let mut pad = 0;
            let mut n = input.len();
            while pad < 2 && n > 0 && input[n - 1] == b'=' {
                n -= 1;
                pad += 1;
            }
            let body = &input[..n];
            if body.contains(&b'=') {
                return None;
            }
            let rem = body.len() % 4;
            if rem == 1 || (pad > 0 && (rem + pad != 4)) {
                return None;
            }
            let mut out = Vec::new();
            let (mut acc, mut bits) = (0u32, 0u32);
            for &c in body {
                let v = match c {
                    b'A'..=b'Z' => c - b'A',
                    b'a'..=b'z' => c - b'a' + 26,
                    b'0'..=b'9' => c - b'0' + 52,
                    b'+' => 62,
                    b'/' => 63,
                    _ => return None,
                };
                acc = (acc << 6) | v as u32;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    out.push((acc >> bits) as u8);
                }
            }
            Some(out)
        }

        let alphabet: Vec<u8> =
            (b'A'..=b'Z').chain(b'a'..=b'z').chain(b'0'..=b'9').chain(*b"+/").collect();
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let odd = [b'=', b'.', b'-', b'\n', b' ', 0x00, 0xFF];

        let a = arena();
        for len in 0..40usize {
            for trial in 0..64 {
                let mut v: Vec<u8> =
                    (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
                if trial % 2 == 1 && len > 0 {
                    let at = (next() % len as u64) as usize;
                    v[at] = odd[(next() % odd.len() as u64) as usize];
                }
                let got = base64_decode(&v, &a).map(<[u8]>::to_vec);
                assert_eq!(got, reference(&v), "input {v:?}");
            }
        }
    }

    #[test]
    fn truncated_picture_block_is_dropped() {
        let block = picture_block("image/png", "d", b"IMGDATA");
        for cut in 0..block.len() {
            let entry = format!("METADATA_BLOCK_PICTURE={}", base64_encode(&block[..cut]));
            let mut rec = Rec::default();
            parse_comment_body(&body("v", &[&entry]), &arena(), &mut rec);
            assert!(rec.pictures.is_empty(), "truncated to {cut} bytes must not emit");
        }
    }

    #[test]
    fn truncations_never_panic() {
        let block = picture_block("image/jpeg", "front", b"IMG");
        let entry = format!("METADATA_BLOCK_PICTURE={}", base64_encode(&block));
        let full = body("reference", &["TITLE=Set Theory", "ARTIST=x", &entry]);
        for n in 0..full.len() {
            let mut rec = Rec::default();
            parse_comment_body(&full[..n], &arena(), &mut rec);
        }
        let mut lying = full.clone();
        lying[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        parse_comment_body(&lying, &arena(), &mut Rec::default());
        let mut lying = full.clone();
        let count_at = 4 + 9; // vendor "reference"
        lying[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        parse_comment_body(&lying, &arena(), &mut Rec::default());
    }

    #[test]
    fn long_field_name_uses_the_arena_not_the_stack() {
        let key = "k".repeat(KEY_STACK_MAX + 17);
        let entry = format!("{key}=v");
        let mut rec = Rec::default();
        parse_comment_body(&body("v", &[&entry]), &arena(), &mut rec);
        assert_eq!(rec.text, vec![(key.to_ascii_uppercase(), "v".to_string())]);
    }

    #[test]
    fn opus_tags_wrapper_needs_its_magic() {
        let mut packet = b"OpusTags".to_vec();
        packet.extend_from_slice(&body("libopus 1.4", &["TITLE=Opus"]));
        let mut rec = Rec::default();
        assert!(parse_opus_tags(&packet, &arena(), &mut rec));
        assert_eq!(rec.text, vec![("TITLE".to_string(), "Opus".to_string())]);

        let mut rec = Rec::default();
        assert!(!parse_opus_tags(b"OpusHead\0\0\0", &arena(), &mut rec));
        assert!(rec.text.is_empty());
        assert!(!parse_opus_tags(b"Opus", &arena(), &mut Rec::default()));
    }

    #[test]
    fn vorbis_comment_wrapper_needs_its_header_and_ignores_the_framing_bit() {
        let mut packet = b"\x03vorbis".to_vec();
        packet.extend_from_slice(&body("Xiph.Org libVorbis", &["TITLE=Vorbis"]));
        packet.push(0x01); // framing bit (Vorbis I section 5.2.2.1), ignored
        let mut rec = Rec::default();
        assert!(parse_vorbis_comments(&packet, &arena(), &mut rec));
        assert_eq!(rec.text, vec![("TITLE".to_string(), "Vorbis".to_string())]);

        let mut zeroed = packet.clone();
        *zeroed.last_mut().unwrap() = 0x00;
        let mut rec = Rec::default();
        assert!(parse_vorbis_comments(&zeroed, &arena(), &mut rec));
        assert_eq!(rec.text.len(), 1);

        assert!(!parse_vorbis_comments(b"\x01vorbis....", &arena(), &mut Rec::default()));
    }
}
