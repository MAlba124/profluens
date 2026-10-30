//! Scanned tags for the duration of the callback. Text lives in an arena, pictures in [`Memory`]
//! views.

use profluens_core::event::{TagList, TagListSink, TagSink};
use profluens_core::memory::{Memory, Pool};
use profluens_core::scratch::Arena;

/// Ceilings on what one file may put in a [`Tags`]. They bound arena growth from tiny-tag files.
pub(crate) const MAX_TEXT_TAGS: usize = 65_536;
pub(crate) const MAX_PICTURES: usize = 1_024;

/// An attached picture. A refcounted view of the read buffer when possible, not a copy.
pub struct PictureRef<'a> {
    pub mime: &'a str,
    pub data: Memory,
}

/// One file's tags in canonical uppercase Vorbis-comment keys, in file order. Keys may repeat.
pub struct Tags<'a> {
    text: Vec<(&'a str, &'a str), &'a Arena>,
    pictures: Vec<PictureRef<'a>, &'a Arena>,
}

impl<'a> Tags<'a> {
    pub(crate) fn new_in(arena: &'a Arena) -> Self {
        Self { text: Vec::new_in(arena), pictures: Vec::new_in(arena) }
    }

    /// First value for `key`, case-insensitively.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.text.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| *v)
    }

    /// All `(key, value)` pairs in file order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.text.iter().map(|(k, v)| (*k, *v))
    }

    pub fn pictures(&self) -> &[PictureRef<'a>] {
        &self.pictures
    }

    /// Number of text tags plus pictures.
    pub fn len(&self) -> usize {
        self.text.len() + self.pictures.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.pictures.is_empty()
    }

    /// Copy into core's owned [`TagList`], to keep the tags past the callback.
    pub fn to_tag_list(&self, pool: &Pool) -> TagList {
        let mut sink = TagListSink::new(pool);
        for (k, v) in self.iter() {
            sink.text(k, v);
        }
        for p in self.pictures() {
            sink.picture(p.mime, p.data.data());
        }
        sink.finish()
    }
}

pub(crate) struct ArenaSink<'a> {
    arena: &'a Arena,
    pool: &'a Pool,
    src: Option<&'a Memory>,
    tags: Tags<'a>,
    last_key: &'a str,
}

impl<'a> ArenaSink<'a> {
    pub(crate) fn new(arena: &'a Arena, pool: &'a Pool, src: Option<&'a Memory>) -> Self {
        Self { arena, pool, src, tags: Tags::new_in(arena), last_key: "" }
    }

    pub(crate) fn finish(self) -> Tags<'a> {
        self.tags
    }
}

impl<'a> TagSink for ArenaSink<'a> {
    fn text(&mut self, key: &str, value: &str) {
        if self.tags.text.len() >= MAX_TEXT_TAGS {
            return;
        }
        // Copy the `&'a Arena` out so interned strings keep the arena's lifetime.
        let arena: &'a Arena = self.arena;
        if key != self.last_key {
            let k = intern(arena, key);
            if k.is_empty() {
                return;
            }
            self.last_key = k;
        }
        let v = intern(arena, value);
        // A spent arena yields "" and a failed grow would abort, so drop the tag instead.
        if (v.is_empty() && !value.is_empty()) || self.tags.text.try_reserve(1).is_err() {
            return;
        }
        self.tags.text.push((self.last_key, v));
    }

    fn picture(&mut self, mime: &str, data: &[u8]) {
        if self.tags.pictures.len() >= MAX_PICTURES {
            return;
        }
        let arena: &'a Arena = self.arena;
        let mime = intern(arena, mime);
        let mem = match self.src.and_then(|src| view_of(src, data)) {
            Some(view) => view,
            None => {
                // A picture past the exact cap is left out.
                let Some(mut m) = self.pool.acquire_exact(data.len()) else { return };
                // A recycled buffer holds stale bytes and may be larger, so write the prefix then
                // trim.
                m.as_mut_full()[..data.len()].copy_from_slice(data);
                m.set_len(data.len());
                m
            }
        };
        if self.tags.pictures.try_reserve(1).is_err() {
            return;
        }
        self.tags.pictures.push(PictureRef { mime, data: mem });
    }
}

pub(crate) fn intern<'a>(arena: &'a Arena, s: &str) -> &'a str {
    let Ok(region) = arena.alloc_bytes(s.len()) else {
        return "";
    };
    region.copy_from_slice(s.as_bytes());
    std::str::from_utf8(region).unwrap_or("")
}

/// A zero-copy view of `data` if it lies inside `src`'s used bytes.
fn view_of(src: &Memory, data: &[u8]) -> Option<Memory> {
    let base = src.data();
    if data.is_empty() || base.is_empty() {
        return None;
    }
    let lo = base.as_ptr() as usize;
    let hi = lo.checked_add(base.len())?;
    let start = data.as_ptr() as usize;
    let end = start.checked_add(data.len())?;
    if start < lo || end > hi {
        return None;
    }
    src.slice(start - lo, data.len())
}

/// Decode `bytes` as UTF-8, or as Latin-1 into `scratch` when invalid. The Latin-1 path is
/// bounded by `scratch`.
pub(crate) fn decode_text<'s>(bytes: &'s [u8], scratch: &'s mut [u8]) -> &'s str {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s;
    }
    let mut n = 0;
    for &b in bytes {
        if b < 0x80 {
            if n + 1 > scratch.len() {
                break;
            }
            scratch[n] = b;
            n += 1;
        } else {
            if n + 2 > scratch.len() {
                break;
            }
            scratch[n] = 0xC0 | (b >> 6);
            scratch[n + 1] = 0x80 | (b & 0x3F);
            n += 2;
        }
    }
    std::str::from_utf8(&scratch[..n]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interned_text_survives_the_source() {
        let arena = Arena::default();
        let pool = Pool::bounded(4096, 4);
        let mut sink = ArenaSink::new(&arena, &pool, None);
        {
            let owned = String::from("Carbon Based Lifeforms");
            sink.text("ARTIST", &owned);
        }
        let tags = sink.finish();
        assert_eq!(tags.get("artist"), Some("Carbon Based Lifeforms"));
        assert_eq!(tags.get("TITLE"), None);
        assert_eq!(tags.len(), 1);
    }

    #[test]
    fn a_repeated_key_is_interned_once() {
        let arena = Arena::default();
        let pool = Pool::bounded(4096, 4);
        let mut sink = ArenaSink::new(&arena, &pool, None);
        for v in ["One", "Two", "Three"] {
            sink.text("ARTIST", v);
        }
        sink.text("TITLE", "T");
        sink.text("ARTIST", "Four");
        let tags = sink.finish();
        let keys: Vec<&str> = tags.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["ARTIST", "ARTIST", "ARTIST", "TITLE", "ARTIST"]);
        assert!(keys[..3].iter().all(|k| k.as_ptr() == keys[0].as_ptr()));
        assert_ne!(keys[4].as_ptr(), keys[0].as_ptr());
        assert_eq!(
            tags.iter().map(|(_, v)| v).collect::<Vec<_>>(),
            ["One", "Two", "Three", "T", "Four"]
        );
    }

    #[test]
    fn picture_inside_the_read_buffer_is_zero_copy() {
        let arena = Arena::default();
        let pool = Pool::bounded(4096, 4);
        let mut src = pool.acquire_exact(4096).expect("exact");
        src.as_mut_full()[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        src.set_len(8);
        let base = src.data().as_ptr() as usize;

        let mut sink = ArenaSink::new(&arena, &pool, Some(&src));
        let inner = &src.data()[2..6];
        sink.picture("image/png", inner);
        let tags = sink.finish();

        let p = &tags.pictures()[0];
        assert_eq!(p.mime, "image/png");
        assert_eq!(p.data.data(), b"NG\r\n");
        assert_eq!(p.data.data().as_ptr() as usize, base + 2);
    }

    #[test]
    fn picture_outside_the_read_buffer_is_copied() {
        let arena = Arena::default();
        let pool = Pool::bounded(4096, 4);
        let mut src = pool.acquire_exact(4096).expect("exact");
        src.set_len(8);
        let elsewhere = vec![7u8; 300];

        let mut sink = ArenaSink::new(&arena, &pool, Some(&src));
        sink.picture("image/jpeg", &elsewhere);
        let tags = sink.finish();

        let p = &tags.pictures()[0];
        assert_eq!(p.data.len(), 300);
        assert_eq!(p.data.data(), &elsewhere[..]);
        assert_ne!(p.data.data().as_ptr() as usize, elsewhere.as_ptr() as usize);
    }

    #[test]
    fn to_tag_list_materialises_an_owned_copy() {
        let arena = Arena::default();
        let pool = Pool::bounded(4096, 8);
        let mut sink = ArenaSink::new(&arena, &pool, None);
        sink.text("TITLE", "Enough");
        sink.picture("image/png", b"\x89PNG");
        let list = sink.finish().to_tag_list(&pool);
        assert_eq!(list.get("TITLE"), Some("Enough"));
        assert_eq!(list.pictures()[0].data.data(), b"\x89PNG");
    }

    #[test]
    fn latin1_text_is_transcoded_utf8_passes_through() {
        let mut scratch = [0u8; 64];
        assert_eq!(decode_text("Björk".as_bytes(), &mut scratch), "Björk");
        assert_eq!(decode_text(b"Bj\xf6rk", &mut scratch), "Björk");
        let mut tiny = [0u8; 3];
        assert_eq!(decode_text(b"Bj\xf6rk", &mut tiny), "Bj");
    }
}
