//! ID3v2, ID3v1 and APEv2 tag parsing over `&[u8]` into a [`TagSink`], with no heap.
//! Callers emit v2, then APE, then v1, since `TagList::get` returns the first value.
//! Spec references are id3v2.4.0-structure, id3v2.4.0-frames, id3v2.3.0, id3v2-00 and APEv2.

use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

const V2_HEADER_LEN: usize = 10;

/// Longest `TXXX` description taken as a key. v2.4 repeats the key per value, so a long one
/// grows quadratically.
const MAX_USER_KEY: usize = 128;

const TITLE: &str = "TITLE";
const ARTIST: &str = "ARTIST";
const ALBUM: &str = "ALBUM";
const ALBUMARTIST: &str = "ALBUMARTIST";
const TRACKNUMBER: &str = "TRACKNUMBER";
const DISCNUMBER: &str = "DISCNUMBER";
const GENRE: &str = "GENRE";
const DATE: &str = "DATE";
const COMPOSER: &str = "COMPOSER";
const COMMENT: &str = "COMMENT";

/// Total byte length of the ID3v2 tag `prefix` starts with, footer included (v2.4 3.1, 3.4).
/// Rejects a non-synchsafe size, so a chance "ID3" inside audio is not skipped over.
pub fn v2_total_len(prefix: &[u8]) -> Option<usize> {
    let hdr = prefix.get(..V2_HEADER_LEN)?;
    if &hdr[..3] != b"ID3" {
        return None;
    }
    if hdr[3] == 0xFF || hdr[4] == 0xFF {
        return None;
    }
    let size = synchsafe(&hdr[6..10])?;
    let footer = if hdr[5] & 0x10 != 0 { V2_HEADER_LEN } else { 0 };
    Some(V2_HEADER_LEN + size as usize + footer)
}

/// Parse a whole ID3v2 tag (v2.2, v2.3, v2.4) into `sink`. Never fails, a corrupt tag ends the
/// walk.
pub fn parse_v2(tag: &[u8], scratch: &Arena, sink: &mut impl TagSink) {
    let Some(hdr) = tag.get(..V2_HEADER_LEN) else {
        return;
    };
    if &hdr[..3] != b"ID3" {
        return;
    }
    let major = hdr[3];
    if !matches!(major, 2..=4) {
        return;
    }
    let flags = hdr[5];
    let Some(size) = synchsafe(&hdr[6..10]) else {
        return;
    };
    let end = V2_HEADER_LEN.saturating_add(size as usize).min(tag.len());
    let mut body = &tag[V2_HEADER_LEN..end];

    // v2.2 3.1 says to ignore the whole tag when the undefined compression bit is set.
    if major == 2 && flags & 0x40 != 0 {
        return;
    }
    let tag_unsync = flags & 0x80 != 0;
    // v2.2 and v2.3 unsynchronise the whole body (6.1). v2.4 does it per frame (4.1.2).
    if tag_unsync && major < 4 {
        body = deunsync(body, scratch);
    }
    if major >= 3 && flags & 0x40 != 0 {
        let Some(sz) = body.get(..4) else { return };
        let skip = if major == 4 {
            // v2.4's extended-header size is synchsafe and counts its own four bytes.
            match synchsafe(sz) {
                Some(n) => n as usize,
                None => return,
            }
        } else {
            // v2.3's is a plain 32-bit size that excludes the four size bytes themselves.
            (be32(sz) as usize).saturating_add(4)
        };
        let Some(rest) = body.get(skip..) else { return };
        body = rest;
    }

    let (id_len, size_len, flag_len) = if major == 2 { (3, 3, 0) } else { (4, 4, 2) };
    let frame_hdr = id_len + size_len + flag_len;
    let mut p = 0usize;
    while p + frame_hdr <= body.len() {
        let id = &body[p..p + id_len];
        // Zero padding follows the last frame (3.1).
        if id[0] == 0 {
            break;
        }
        let sz_bytes = &body[p + id_len..p + id_len + size_len];
        let size = match major {
            4 => match synchsafe(sz_bytes) {
                Some(n) => n as usize,
                None => break,
            },
            3 => be32(sz_bytes) as usize,
            _ => be24(sz_bytes) as usize,
        };
        let fflags = if flag_len == 2 { [body[p + 8], body[p + 9]] } else { [0, 0] };
        let start = p + frame_hdr;
        let Some(mut data) = start.checked_add(size).and_then(|e| body.get(start..e)) else {
            break; // size runs past the tag, truncated or corrupt
        };
        p = start + size;

        // Frame flags, v2.3 3.3.1 and v2.4 4.1.
        let (compressed, encrypted, grouped, frame_unsync, has_dli) = if major == 4 {
            (
                fflags[1] & 0x08 != 0,
                fflags[1] & 0x04 != 0,
                fflags[1] & 0x40 != 0,
                fflags[1] & 0x02 != 0,
                fflags[1] & 0x01 != 0,
            )
        } else {
            (fflags[1] & 0x80 != 0, fflags[1] & 0x40 != 0, fflags[1] & 0x20 != 0, false, false)
        };
        // Compressed, encrypted and grouped frames are rare and skipped.
        if compressed || encrypted || grouped {
            continue;
        }
        if major == 4 && (frame_unsync || tag_unsync) {
            data = deunsync(data, scratch);
        }
        if has_dli {
            // v2.4 4.1.2 data-length indicator.
            let Some(rest) = data.get(4..) else { continue };
            data = rest;
        }

        match frame_kind(id) {
            // Multiple values per text frame are v2.4 only (4.2).
            Some(Kind::Text(key)) => text_frame(key, data, major == 4, scratch, sink),
            Some(Kind::Comment) => comment_frame(data, scratch, sink),
            Some(Kind::UserText) => user_text_frame(data, major == 4, scratch, sink),
            Some(Kind::Picture) => picture_frame(data, id_len == 3, sink),
            None => {}
        }
    }
}

enum Kind {
    Text(&'static str),
    Comment,
    UserText,
    Picture,
}

static TEXT_FRAMES: [(&[u8], &str); 19] = [
    (b"TIT2", TITLE),
    (b"TT2", TITLE),
    (b"TPE1", ARTIST),
    (b"TP1", ARTIST),
    (b"TALB", ALBUM),
    (b"TAL", ALBUM),
    (b"TPE2", ALBUMARTIST),
    (b"TP2", ALBUMARTIST),
    (b"TRCK", TRACKNUMBER),
    (b"TRK", TRACKNUMBER),
    (b"TPOS", DISCNUMBER),
    (b"TPA", DISCNUMBER),
    // 4.2.3 `TCON` passes through raw, legacy `(17)` references are not expanded.
    (b"TCON", GENRE),
    (b"TCO", GENRE),
    (b"TDRC", DATE),
    (b"TYER", DATE),
    (b"TYE", DATE),
    (b"TCOM", COMPOSER),
    (b"TCM", COMPOSER),
];

fn frame_kind(id: &[u8]) -> Option<Kind> {
    if id == b"COMM" || id == b"COM" {
        return Some(Kind::Comment);
    }
    if id == b"TXXX" || id == b"TXX" {
        return Some(Kind::UserText);
    }
    if id == b"APIC" || id == b"PIC" {
        return Some(Kind::Picture);
    }
    TEXT_FRAMES.iter().find(|(f, _)| *f == id).map(|&(_, key)| Kind::Text(key))
}

fn text_frame(key: &str, data: &[u8], multi: bool, scratch: &Arena, sink: &mut impl TagSink) {
    let Some((&enc, text)) = data.split_first() else {
        return;
    };
    emit_values(key, enc, text, multi, scratch, sink);
}

fn emit_values(
    key: &str,
    enc: u8,
    mut rest: &[u8],
    multi: bool,
    scratch: &Arena,
    sink: &mut impl TagSink,
) {
    loop {
        let (value, after) = split_terminated(rest, enc);
        if !value.is_empty()
            && let Some(s) = decode_text(enc, value, scratch)
            && !s.is_empty()
        {
            sink.text(key, s);
        }
        if !multi || after.is_empty() {
            break;
        }
        rest = after;
    }
}

fn comment_frame(data: &[u8], scratch: &Arena, sink: &mut impl TagSink) {
    let Some(&enc) = data.first() else { return };
    let Some(rest) = data.get(4..) else { return }; // skip the encoding byte + language
    let (_description, text) = split_terminated(rest, enc);
    if let Some(s) = decode_text(enc, text, scratch)
        && !s.is_empty()
    {
        sink.text(COMMENT, s);
    }
}

fn user_text_frame(data: &[u8], multi: bool, scratch: &Arena, sink: &mut impl TagSink) {
    let Some((&enc, rest)) = data.split_first() else {
        return;
    };
    let (description, value) = split_terminated(rest, enc);
    let Some(description) = decode_text(enc, description, scratch) else {
        return;
    };
    if description.len() > MAX_USER_KEY {
        return;
    }
    let Some(key) = upper_ascii(description, scratch) else {
        return;
    };
    if key.is_empty() {
        return;
    }
    emit_values(key, enc, value, multi, scratch, sink);
}

fn picture_frame(data: &[u8], v22: bool, sink: &mut impl TagSink) {
    let Some((&enc, rest)) = data.split_first() else {
        return;
    };
    let (mime, rest) = if v22 {
        let Some(format) = rest.get(..3) else { return };
        let mime = if format.eq_ignore_ascii_case(b"JPG") {
            "image/jpeg"
        } else if format.eq_ignore_ascii_case(b"PNG") {
            "image/png"
        } else {
            return; // `-->` (a URL link) and anything else: no image bytes to hand over
        };
        (mime, &rest[3..])
    } else {
        let (mime, rest) = split_terminated(rest, 0);
        let Ok(mime) = std::str::from_utf8(mime) else {
            return;
        };
        if mime == "-->" {
            return;
        }
        (mime, rest)
    };
    let Some((&kind, rest)) = rest.split_first() else { return };
    let (_description, image) = split_terminated(rest, enc);
    if image.is_empty() {
        return;
    }
    sink.picture_typed(kind, mime, image);
}

fn deunsync<'a>(src: &[u8], scratch: &'a Arena) -> &'a [u8] {
    let Ok(out) = scratch.alloc_bytes(src.len()) else {
        return &[];
    };
    let mut n = 0;
    let mut i = 0;
    while i < src.len() {
        out[n] = src[i];
        n += 1;
        // Consuming both bytes makes an encoded `$FF $00 $00` come back as `$FF $00`.
        i += if src[i] == 0xFF && src.get(i + 1) == Some(&0x00) { 2 } else { 1 };
    }
    let out: &'a [u8] = out;
    &out[..n]
}

fn decode_text<'a>(enc: u8, raw: &'a [u8], scratch: &'a Arena) -> Option<&'a str> {
    match enc {
        0 => latin1(raw, scratch),
        // UTF-16 with BOM. RFC 2781 4.3 makes big-endian the default when the BOM is missing.
        1 => {
            let (big_endian, body) = match raw.get(..2) {
                Some([0xFF, 0xFE]) => (false, &raw[2..]),
                Some([0xFE, 0xFF]) => (true, &raw[2..]),
                _ => (true, raw),
            };
            Some(utf16(body, big_endian, scratch))
        }
        2 => Some(utf16(raw, true, scratch)),
        3 => std::str::from_utf8(raw).ok(),
        _ => None,
    }
}

fn latin1<'a>(raw: &'a [u8], scratch: &'a Arena) -> Option<&'a str> {
    if raw.is_ascii() {
        return std::str::from_utf8(raw).ok();
    }
    let out = scratch.alloc_bytes(raw.len() * 2).ok()?;
    let mut n = 0;
    for &b in raw {
        n += char::from(b).encode_utf8(&mut out[n..]).len();
    }
    let out: &'a [u8] = out;
    std::str::from_utf8(&out[..n]).ok()
}

/// UTF-16 to UTF-8 into `scratch`. Unpaired surrogates become U+FFFD (Unicode 3.9).
fn utf16<'a>(raw: &[u8], big_endian: bool, scratch: &'a Arena) -> &'a str {
    let unit = |i: usize| -> u16 {
        let (a, b) = (raw[i], raw[i + 1]);
        if big_endian { u16::from_be_bytes([a, b]) } else { u16::from_le_bytes([a, b]) }
    };
    // At most 3 UTF-8 bytes per code unit, for a BMP scalar or half a surrogate pair.
    let Ok(out) = scratch.alloc_bytes((raw.len() / 2) * 3 + 4) else {
        return "";
    };
    let mut n = 0;
    let mut i = 0;
    while i + 1 < raw.len() {
        let u = unit(i);
        i += 2;
        let cp = if (0xD800..0xDC00).contains(&u) {
            let lo = if i + 1 < raw.len() { unit(i) } else { 0 };
            if (0xDC00..0xE000).contains(&lo) {
                i += 2;
                0x1_0000u32 + ((u32::from(u) - 0xD800) << 10) + (u32::from(lo) - 0xDC00)
            } else {
                0xFFFD
            }
        } else if (0xDC00..0xE000).contains(&u) {
            0xFFFD
        } else {
            u32::from(u)
        };
        let c = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
        n += c.encode_utf8(&mut out[n..]).len();
    }
    let out: &'a [u8] = out;
    std::str::from_utf8(&out[..n]).unwrap_or("")
}

/// Split at the first terminator, one `$00` or a code-unit aligned `$00 $00` for UTF-16 (4).
fn split_terminated(data: &[u8], enc: u8) -> (&[u8], &[u8]) {
    if enc == 1 || enc == 2 {
        let mut i = 0;
        while i + 1 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 {
                return (&data[..i], &data[i + 2..]);
            }
            i += 2;
        }
        (data, &data[data.len()..])
    } else {
        match data.iter().position(|&b| b == 0) {
            Some(i) => (&data[..i], &data[i + 1..]),
            None => (data, &data[data.len()..]),
        }
    }
}

fn upper_ascii<'a>(s: &str, scratch: &'a Arena) -> Option<&'a str> {
    let out = scratch.alloc_bytes(s.len()).ok()?;
    out.copy_from_slice(s.as_bytes());
    out.make_ascii_uppercase();
    let out: &'a [u8] = out;
    std::str::from_utf8(out).ok()
}

/// 4-byte synchsafe integer (6.2). `None` if any high bit is set.
fn synchsafe(b: &[u8]) -> Option<u32> {
    let b = b.get(..4)?;
    if b.iter().any(|&x| x & 0x80 != 0) {
        return None;
    }
    Some(
        (u32::from(b[0]) << 21)
            | (u32::from(b[1]) << 14)
            | (u32::from(b[2]) << 7)
            | u32::from(b[3]),
    )
}

fn be32(b: &[u8]) -> u32 {
    b.get(..4).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// v2.2 frame size (id3v2-00 3.2).
fn be24(b: &[u8]) -> u32 {
    b.get(..3).map_or(0, |s| u32::from_be_bytes([0, s[0], s[1], s[2]]))
}

const V1_LEN: usize = 128;

/// Parse a trailing ID3v1 or v1.1 tag from the last 128 bytes of `tail` into `sink`.
pub fn parse_v1(tail: &[u8], scratch: &Arena, sink: &mut impl TagSink) {
    if tail.len() < V1_LEN {
        return;
    }
    let t = &tail[tail.len() - V1_LEN..];
    if &t[..3] != b"TAG" {
        return;
    }
    if let Some(s) = v1_field(&t[3..33], scratch) {
        sink.text(TITLE, s);
    }
    if let Some(s) = v1_field(&t[33..63], scratch) {
        sink.text(ARTIST, s);
    }
    if let Some(s) = v1_field(&t[63..93], scratch) {
        sink.text(ALBUM, s);
    }
    if let Some(s) = v1_field(&t[93..97], scratch) {
        sink.text(DATE, s);
    }
    // v1.1 track number when comment[28] is 0 and comment[29] is not.
    let (comment, track) =
        if t[125] == 0 && t[126] != 0 { (&t[97..125], Some(t[126])) } else { (&t[97..127], None) };
    if let Some(s) = v1_field(comment, scratch) {
        sink.text(COMMENT, s);
    }
    if let Some(n) = track {
        let mut buf = [0u8; 3];
        sink.text(TRACKNUMBER, decimal(n, &mut buf));
    }
    if let Some(genre) = v1_genre(t[127]) {
        sink.text(GENRE, genre);
    }
}

fn v1_field<'a>(raw: &'a [u8], scratch: &'a Arena) -> Option<&'a str> {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let field = &raw[..end];
    let last = field.iter().rposition(|&b| b != b' ')?;
    latin1(&field[..=last], scratch)
}

fn decimal(n: u8, buf: &mut [u8; 3]) -> &str {
    let mut i = buf.len();
    let mut v = n;
    loop {
        i -= 1;
        buf[i] = b'0' + v % 10;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[i..]).unwrap_or("")
}

/// The ID3v1 genre name of `index`, `None` past the table. MP4 `gnre` items index it too.
pub fn v1_genre(index: u8) -> Option<&'static str> {
    GENRES.get(index as usize).copied()
}

/// ID3v1 genre names. 0..=79 are the original list, 80..=147 the Winamp extension.
/// Index 133 is published as `Afro-Punk`, as modern taggers do.
static GENRES: [&str; 148] = [
    "Blues",
    "Classic Rock",
    "Country",
    "Dance",
    "Disco",
    "Funk",
    "Grunge",
    "Hip-Hop",
    "Jazz",
    "Metal",
    "New Age",
    "Oldies",
    "Other",
    "Pop",
    "R&B",
    "Rap",
    "Reggae",
    "Rock",
    "Techno",
    "Industrial",
    "Alternative",
    "Ska",
    "Death Metal",
    "Pranks",
    "Soundtrack",
    "Euro-Techno",
    "Ambient",
    "Trip-Hop",
    "Vocal",
    "Jazz+Funk",
    "Fusion",
    "Trance",
    "Classical",
    "Instrumental",
    "Acid",
    "House",
    "Game",
    "Sound Clip",
    "Gospel",
    "Noise",
    "AlternRock",
    "Bass",
    "Soul",
    "Punk",
    "Space",
    "Meditative",
    "Instrumental Pop",
    "Instrumental Rock",
    "Ethnic",
    "Gothic",
    "Darkwave",
    "Techno-Industrial",
    "Electronic",
    "Pop-Folk",
    "Eurodance",
    "Dream",
    "Southern Rock",
    "Comedy",
    "Cult",
    "Gangsta",
    "Top 40",
    "Christian Rap",
    "Pop/Funk",
    "Jungle",
    "Native American",
    "Cabaret",
    "New Wave",
    "Psychadelic",
    "Rave",
    "Showtunes",
    "Trailer",
    "Lo-Fi",
    "Tribal",
    "Acid Punk",
    "Acid Jazz",
    "Polka",
    "Retro",
    "Musical",
    "Rock & Roll",
    "Hard Rock",
    "Folk",
    "Folk-Rock",
    "National Folk",
    "Swing",
    "Fast Fusion",
    "Bebob",
    "Latin",
    "Revival",
    "Celtic",
    "Bluegrass",
    "Avantgarde",
    "Gothic Rock",
    "Progressive Rock",
    "Psychedelic Rock",
    "Symphonic Rock",
    "Slow Rock",
    "Big Band",
    "Chorus",
    "Easy Listening",
    "Acoustic",
    "Humour",
    "Speech",
    "Chanson",
    "Opera",
    "Chamber Music",
    "Sonata",
    "Symphony",
    "Booty Bass",
    "Primus",
    "Porn Groove",
    "Satire",
    "Slow Jam",
    "Club",
    "Tango",
    "Samba",
    "Folklore",
    "Ballad",
    "Power Ballad",
    "Rhythmic Soul",
    "Freestyle",
    "Duet",
    "Punk Rock",
    "Drum Solo",
    "A capella",
    "Euro-House",
    "Dance Hall",
    "Goa",
    "Drum & Bass",
    "Club-House",
    "Hardcore",
    "Terror",
    "Indie",
    "BritPop",
    "Afro-Punk",
    "Polsk Punk",
    "Beat",
    "Christian Gangsta Rap",
    "Heavy Metal",
    "Black Metal",
    "Crossover",
    "Contemporary Christian",
    "Christian Rock",
    "Merengue",
    "Salsa",
    "Thrash Metal",
    "Anime",
    "Jpop",
    "Synthpop",
];

const APE_FOOTER_LEN: usize = 32;
const APE_PREAMBLE: &[u8; 8] = b"APETAGEX";
const APE_KEY_MAX: usize = 255;

/// Total length of the APEv2 tail whose footer is the last 32 bytes of `last32`, header included.
/// The caller strips a trailing ID3v1 tag first.
pub fn ape_tail_len(last32: &[u8]) -> Option<usize> {
    let f = last32.get(last32.len().checked_sub(APE_FOOTER_LEN)?..)?;
    if &f[..8] != APE_PREAMBLE {
        return None;
    }
    let version = le32(&f[8..12]);
    if version != 1000 && version != 2000 {
        return None;
    }
    let size = le32(&f[12..16]) as usize;
    if size < APE_FOOTER_LEN {
        return None;
    }
    let flags = le32(&f[20..24]);
    if flags & (1 << 29) != 0 {
        return None; // this block is a *header*, so we are not at the tag's end
    }
    let header = if flags & (1 << 31) != 0 { APE_FOOTER_LEN } else { 0 };
    Some(size + header)
}

/// Parse an APEv2 tag, the block [`ape_tail_len`] measured, into `sink`. Only text items are tags.
pub fn parse_ape(tag: &[u8], sink: &mut impl TagSink) {
    if tag.len() < APE_FOOTER_LEN {
        return;
    }
    let start = if tag.len() >= 2 * APE_FOOTER_LEN
        && &tag[..8] == APE_PREAMBLE
        && le32(&tag[20..24]) & (1 << 29) != 0
    {
        APE_FOOTER_LEN
    } else {
        0
    };
    let Some(items) = tag.get(start..tag.len() - APE_FOOTER_LEN) else {
        return;
    };

    let mut key_buf = [0u8; APE_KEY_MAX];
    let mut p = 0usize;
    while p + 8 <= items.len() {
        let value_len = le32(&items[p..p + 4]) as usize;
        let flags = le32(&items[p + 4..p + 8]);
        let rest = &items[p + 8..];
        let Some(key_len) = rest.iter().position(|&b| b == 0) else {
            break;
        };
        let key = &rest[..key_len];
        let Some(value) =
            (key_len + 1).checked_add(value_len).and_then(|e| rest.get(key_len + 1..e))
        else {
            break; // value runs past the item area, truncated or corrupt
        };
        // Advances by at least 9 each iteration, so the walk terminates.
        p += 8 + key_len + 1 + value_len;

        if (flags >> 1) & 0x3 != 0 {
            continue; // not a text item
        }
        let Some(slot) = key_buf.get_mut(..key_len) else {
            continue;
        };
        if key.is_empty() || !key.iter().all(|&b| (0x20..0x7F).contains(&b)) {
            continue;
        }
        slot.copy_from_slice(key);
        slot.make_ascii_uppercase();
        let Ok(key) = std::str::from_utf8(slot) else {
            continue;
        };
        let key = ape_alias(key);
        for value in value.split(|&b| b == 0) {
            if value.is_empty() {
                continue;
            }
            if let Ok(s) = std::str::from_utf8(value) {
                sink.text(key, s);
            }
        }
    }
}

fn ape_alias(key: &str) -> &str {
    match key {
        "YEAR" => DATE,
        "TRACK" => TRACKNUMBER,
        "DISC" => DISCNUMBER,
        "ALBUM ARTIST" => ALBUMARTIST,
        _ => key,
    }
}

fn le32(b: &[u8]) -> u32 {
    b.get(..4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
