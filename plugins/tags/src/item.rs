use crate::Tags;
use crate::flacmeta;

pub use crate::chapters::{
    ChapterEdition, ChapterSource, ChapterTitle, ItemChapter, ItemChapters, MAX_CHAPTER_DEPTH,
    MAX_CHAPTER_TEXT, MAX_CHAPTER_TITLE, MAX_CHAPTER_TITLES, MAX_CHAPTERS, MAX_EDITIONS,
    MAX_LANGUAGE, RawChapter, bcp47, chapters, is_chapter_key, mkv_chapters, moov_chapters,
    normalise, vorbis_chapters,
};
use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

/// At most this many tags are kept.
pub const MAX_TAGS: usize = 64;
/// A value is cut to this many bytes, on a char boundary.
pub const MAX_VALUE: usize = 1024;
/// Longer keys are dropped, a TXXX description can be anything.
pub const MAX_KEY: usize = 64;
/// Keys and values together stay under this, well inside the 48 KiB a message holds.
pub const MAX_TEXT: usize = 32 << 10;
pub const MAX_PICTURES: usize = 8;
/// A larger picture is left out.
pub const MAX_PICTURE: usize = 16 << 20;
/// A stated MIME type is cut to this.
pub const MAX_MIME: usize = 64;
/// A Matroska Tags element past this is left out, the cap to read [`pf_mkv::walk::Meta`] with.
pub const MAX_MKV_TAGS: u64 = 4 << 20;
/// ID3v2 APIC picture type of the front cover, what a folder image is taken as.
pub const FRONT_COVER: u8 = 3;
/// The picture type when the container states none.
pub const KIND_UNKNOWN: u8 = 0xFF;

/// Keys kept first, in this order, the rest follow in file order.
const FIRST: [&str; 8] =
    ["TITLE", "ARTIST", "ALBUM", "ALBUMARTIST", "TRACKNUMBER", "DISCNUMBER", "DATE", "GENRE"];

/// The track statistics mkvmerge writes per track (its `_STATISTICS_TAGS` list), numbers no tv
/// shows, pf-mkv passes a track's tags on as the file's.
const STATISTICS: [&str; 4] = ["BPS", "DURATION", "NUMBER_OF_FRAMES", "NUMBER_OF_BYTES"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemPicture {
    /// ID3v2 APIC picture type, 3 front cover, 0xFF unknown.
    pub kind: u8,
    pub mime: String,
    /// Pixels, 0 unknown.
    pub width: u16,
    pub height: u16,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemTags {
    /// Canonical uppercase keys, keys may repeat.
    pub tags: Vec<(String, String)>,
    /// Front covers first, then file order.
    pub pictures: Vec<ItemPicture>,
}

impl ItemTags {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.tags.iter().find(|t| t.0 == key).map(|t| t.1.as_str())
    }

    /// Bytes of keys and values.
    pub fn text_len(&self) -> usize {
        self.tags.iter().map(|t| t.0.len() + t.1.len()).sum()
    }
}

/// `s` cut to at most `max` bytes on a char boundary.
pub fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The tags and pictures read, cut to the caps. Priority keys first, empty values and exact
/// repeats dropped, values cut, a tag the text budget has no room for left out.
// Owned result, once per item.
#[allow(clippy::disallowed_methods)]
pub fn capped(mut text: Vec<(String, String)>, mut pictures: Vec<ItemPicture>) -> ItemTags {
    // a YEAR comment, as some taggers write it, is the DATE a tv looks for
    if !text.iter().any(|t| t.0 == "DATE")
        && let Some(t) = text.iter_mut().find(|t| t.0 == "YEAR")
    {
        t.0 = "DATE".to_string();
    }
    let rank = |k: &str| FIRST.iter().position(|f| *f == k).unwrap_or(FIRST.len());
    // chapter comments are read as chapters
    let mut order: Vec<usize> = (0..text.len()).filter(|&i| !is_chapter_key(&text[i].0)).collect();
    order.sort_by_key(|&i| rank(&text[i].0));
    let mut out = ItemTags::default();
    let mut used = 0;
    for i in order {
        if out.tags.len() >= MAX_TAGS {
            break;
        }
        let (k, v) = &text[i];
        let v = cut(v.trim(), MAX_VALUE);
        if k.is_empty() || k.len() > MAX_KEY || v.is_empty() || used + k.len() + v.len() > MAX_TEXT
        {
            continue;
        }
        if out.tags.iter().any(|t| t.0 == *k && t.1 == v) {
            continue;
        }
        used += k.len() + v.len();
        out.tags.push((k.clone(), v.to_string()));
    }
    pictures.retain(|p| p.bytes.len() <= MAX_PICTURE && image_info(&p.bytes).is_some());
    pictures.sort_by_key(|p| p.kind != FRONT_COVER);
    pictures.truncate(MAX_PICTURES);
    out.pictures = pictures;
    out
}

/// The type and pixel size of an image by its bytes, JPEG by its first SOF, PNG by IHDR, WebP by
/// its VP8, VP8L or VP8X head, None for what is none of these.
pub fn image_info(b: &[u8]) -> Option<(&'static str, u16, u16)> {
    let u16be = |i: usize| b.get(i..i + 2).map_or(0, |x| u16::from_be_bytes([x[0], x[1]]));
    let u32be =
        |i: usize| b.get(i..i + 4).map_or(0, |x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]));
    let clamp = |v: u32| v.min(u32::from(u16::MAX)) as u16;
    match b {
        [0xFF, 0xD8, 0xFF, ..] => {
            let mut at = 2;
            while let (Some(&0xFF), Some(&m)) = (b.get(at), b.get(at + 1)) {
                if m == 0xFF {
                    at += 1;
                    continue;
                }
                // SOF0 to SOF15 less DHT, JPG and DAC
                if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                    return Some(("image/jpeg", u16be(at + 7), u16be(at + 5)));
                }
                if m == 0xD9 || m == 0xDA {
                    break;
                }
                let len = usize::from(u16be(at + 2));
                if len < 2 {
                    break;
                }
                at += 2 + len;
            }
            Some(("image/jpeg", 0, 0))
        }
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', ..] => {
            Some(("image/png", clamp(u32be(16)), clamp(u32be(20))))
        }
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => {
            let (w, h) = webp_size(b).unwrap_or((0, 0));
            Some(("image/webp", clamp(w), clamp(h)))
        }
        _ => None,
    }
}

/// The canvas of a WebP by its first chunk, the lossy frame header, the lossless header or the
/// extended header.
fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    let le = |i: usize, n: usize| {
        b.get(i..i + n).map(|x| x.iter().rev().fold(0u32, |a, &v| a << 8 | u32::from(v)))
    };
    match b.get(12..16)? {
        b"VP8 " if b.get(23..26)? == [0x9D, 0x01, 0x2A] => {
            Some((le(26, 2)? & 0x3FFF, le(28, 2)? & 0x3FFF))
        }
        b"VP8L" if *b.get(20)? == 0x2F => {
            let v = le(21, 4)?;
            Some(((v & 0x3FFF) + 1, (v >> 14 & 0x3FFF) + 1))
        }
        b"VP8X" => Some((le(24, 3)? + 1, le(27, 3)? + 1)),
        _ => None,
    }
}

/// A picture of `kind` when its bytes are a JPEG, PNG or WebP of at most [`MAX_PICTURE`]
/// bytes, its type and size from them.
// Owned result, once per picture.
#[allow(clippy::disallowed_methods)]
pub fn image(kind: u8, bytes: Vec<u8>) -> Option<ItemPicture> {
    if bytes.len() > MAX_PICTURE {
        return None;
    }
    let (mime, width, height) = image_info(&bytes)?;
    Some(ItemPicture { kind, mime: mime.to_string(), width, height, bytes })
}

/// A picture of `kind`, its type and size from its bytes where they tell, else as tagged.
/// Unchecked, [`capped`] drops what [`image`] refuses.
// Owned result, once per picture.
#[allow(clippy::disallowed_methods)]
pub fn picture(kind: u8, mime: &str, bytes: Vec<u8>) -> ItemPicture {
    let (mime, width, height) = match image_info(&bytes) {
        Some((m, w, h)) => (m.to_string(), w, h),
        None => (cut(mime, MAX_MIME).to_string(), 0, 0),
    };
    ItemPicture { kind, mime, width, height, bytes }
}

/// A TagSink collecting owned text, and pictures with the type their container states. At most
/// [`MAX_PICTURES`] front covers and as many others are kept, all [`capped`] can choose from.
#[derive(Debug, Default)]
pub struct Collect {
    pub text: Vec<(String, String)>,
    pub pictures: Vec<ItemPicture>,
    /// Pictures come from elsewhere, the sink drops them.
    pub no_pictures: bool,
}

impl Collect {
    /// What was collected, cut to the caps.
    pub fn finish(self) -> ItemTags {
        capped(self.text, self.pictures)
    }

    // One copy per picture kept.
    #[allow(clippy::disallowed_methods)]
    fn admit(&mut self, kind: u8, data: &[u8]) {
        if self.no_pictures || data.len() > MAX_PICTURE || image_info(data).is_none() {
            return;
        }
        let cover = kind == FRONT_COVER;
        if self.pictures.iter().filter(|p| (p.kind == FRONT_COVER) == cover).count() >= MAX_PICTURES
        {
            return;
        }
        self.pictures.extend(image(kind, data.to_vec()));
    }
}

impl TagSink for Collect {
    // Owned result, at most MAX_TAGS * 4 entries.
    #[allow(clippy::disallowed_methods)]
    fn text(&mut self, key: &str, value: &str) {
        if self.text.len() < MAX_TAGS * 4 && key.len() <= MAX_KEY && !is_chapter_key(key) {
            self.text.push((key.to_string(), cut(value.trim(), MAX_VALUE).to_string()));
        }
    }

    fn picture(&mut self, _mime: &str, data: &[u8]) {
        self.admit(KIND_UNKNOWN, data);
    }

    fn picture_typed(&mut self, kind: u8, _mime: &str, data: &[u8]) {
        self.admit(kind, data);
    }
}

/// Matroska text tags into a [`Collect`], keys uppercased, the track statistics left out.
struct MkvText<'c>(&'c mut Collect);

impl TagSink for MkvText<'_> {
    fn text(&mut self, key: &str, value: &str) {
        if key.len() > MAX_KEY {
            return;
        }
        let mut buf = [0u8; MAX_KEY];
        let up = &mut buf[..key.len()];
        up.copy_from_slice(key.as_bytes());
        up.make_ascii_uppercase();
        let Ok(key) = std::str::from_utf8(up) else { return };
        if key.starts_with("_STATISTICS") || STATISTICS.contains(&key) {
            return;
        }
        self.0.text(key, value);
    }

    fn picture(&mut self, _mime: &str, _data: &[u8]) {}
}

/// A PICTURE block body into `c`, sized as the block states when its bytes do not tell.
fn picture_block_into(c: &mut Collect, block: &[u8]) {
    let n = c.pictures.len();
    flacmeta::picture_into(block, c);
    if let Some(p) = c.pictures.get_mut(n)
        && p.width == 0
        && let Some((w, h)) = flacmeta::picture_size(block)
        && w > 0
    {
        let clamp = |v: u32| v.min(u32::from(u16::MAX)) as u16;
        (p.width, p.height) = (clamp(w), clamp(h));
    }
}

/// The tags of FLAC VORBIS_COMMENT and PICTURE block bodies, as pf-bare's RawTags spans them.
/// A comment that is no UTF-8 is kept, each invalid run as U+FFFD.
pub fn flac<'b>(
    comments: impl IntoIterator<Item = &'b [u8]>,
    pictures: impl IntoIterator<Item = &'b [u8]>,
) -> ItemTags {
    let mut c = Collect::default();
    comments.into_iter().for_each(|b| flacmeta::vorbis_comment_lossy_into(b, &mut c));
    pictures.into_iter().for_each(|b| picture_block_into(&mut c, b));
    c.finish()
}

/// The ID3v2 tag of an MPEG audio or ADTS file, its trailing APEv2 and ID3v1 tags only when
/// the ID3v2 tag holds no text, their pictures dropped.
pub fn id3(id3v2: &[u8], ape: &[u8], v1: &[u8], scratch: &Arena) -> ItemTags {
    let mut c = Collect::default();
    pf_mp3::id3::parse_v2(id3v2, scratch, &mut c);
    if c.text.is_empty() {
        c.no_pictures = true;
        pf_mp3::id3::parse_ape(ape, &mut c);
        pf_mp3::id3::parse_v1(v1, scratch, &mut c);
    }
    c.finish()
}

/// The `ilst` tags of a whole `moov` box, header included.
pub fn moov(moov: &[u8], scratch: &Arena) -> ItemTags {
    let mut c = Collect::default();
    pf_mp4::parse_moov_tags(moov, scratch, &mut c);
    c.finish()
}

/// The tags of a Vorbis comment body, the bytes after an Ogg comment packet's codec magic. A
/// comment that is no UTF-8 is kept as [`flac`] keeps it, pictures read as its blocks are.
pub fn ogg(comment: &[u8], scratch: &Arena) -> ItemTags {
    let mut c = Collect { no_pictures: true, ..Collect::default() };
    pf_ogg::parse_comment_body_lossy(comment, scratch, &mut c);
    c.no_pictures = false;
    pf_ogg::picture_blocks(comment, scratch).for_each(|b| picture_block_into(&mut c, b));
    c.finish()
}

/// The tags and image attachments of a Matroska file as its [`pf_mkv::walk::MetaAt`] read them.
/// All or nothing, a walk that stopped on an error gives none.
pub fn mkv(meta: pf_mkv::walk::Meta) -> ItemTags {
    if meta.error.is_some() {
        return ItemTags::default();
    }
    let mut c = Collect::default();
    pf_mkv::tags::parse_tags(&meta.tags, &mut MkvText(&mut c));
    c.pictures =
        meta.pictures.into_iter().map(|a| picture(KIND_UNKNOWN, &a.media_type, a.data)).collect();
    c.finish()
}

/// The tags and pictures of a file as its open read them, none when they were not wanted. A
/// Matroska file's pictures are copied, [`taken`] moves them.
pub fn of(o: &pf_open::Opened) -> ItemTags {
    match &o.meta {
        pf_open::Meta::Mkv(m) => mkv(m.clone()),
        meta => read(o, meta),
    }
}

/// [`of`] with the tag bytes moved out of the open.
pub fn taken(o: &mut pf_open::Opened) -> ItemTags {
    match std::mem::take(&mut o.meta) {
        pf_open::Meta::Mkv(m) => mkv(m),
        meta => read(o, &meta),
    }
}

/// [`taken`] with the [`chapters`] read before the meta moves.
pub fn taken_with_chapters(o: &mut pf_open::Opened) -> (ItemTags, ItemChapters) {
    let c = chapters(o);
    (taken(o), c)
}

fn read(o: &pf_open::Opened, meta: &pf_open::Meta) -> ItemTags {
    use pf_open::Meta;
    let scratch = Arena::default();
    match meta {
        Meta::None | Meta::Mkv(_) => ItemTags::default(),
        Meta::Bare { raw, flac: true } => {
            let span = |r| raw.span(r);
            flac(raw.comments.iter().map(span), raw.pictures.iter().map(span))
        }
        Meta::Bare { raw, flac: false } => id3(raw.id3v2(), &raw.ape, &raw.v1, &scratch),
        Meta::Moov(r) => {
            o.head.get(r.clone()).map_or_else(ItemTags::default, |m| moov(m, &scratch))
        }
        Meta::Ogg(s) => ogg(o.bytes(*s), &scratch),
    }
}

/// What a [`crate::Scanner`] found in a file, its pictures untyped.
// Owned result, once per item.
#[allow(clippy::disallowed_methods)]
pub fn scanned(tags: &Tags<'_>) -> ItemTags {
    let text = tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pictures = tags
        .pictures()
        .iter()
        .filter(|p| p.data.len() <= MAX_PICTURE)
        .map(|p| picture(KIND_UNKNOWN, p.mime, p.data.data().to_vec()))
        .collect();
    capped(text, pictures)
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests;
