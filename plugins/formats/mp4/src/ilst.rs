//! iTunes-style `ilst` metadata and no-decode props from a `moov`, allocation-free.
//! Box grammar per ISO/IEC 14496-12, the item scheme per Apple's QTFF Metadata chapter.

use core::fmt::Write as _;

use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

use crate::boxes::{self, FourCc, boxtype};

const UDTA: FourCc = boxes::fourcc(b"udta");
const META: FourCc = boxes::fourcc(b"meta");
const HDLR: FourCc = boxes::fourcc(b"hdlr");
const ILST: FourCc = boxes::fourcc(b"ilst");
const DATA: FourCc = boxes::fourcc(b"data");
const NAME: FourCc = boxes::fourcc(b"name");

/// The Mac OS Roman copyright octet prefixing Apple's QuickTime keys. One octet, so the
/// four-CC stays four octets (QTFF Metadata).
const COPYRIGHT: u8 = 0xA9;

// Well-known `data` type indicators (QTFF Metadata).
const TYPE_IMPLICIT: u32 = 0;
const TYPE_UTF8: u32 = 1;
const TYPE_JPEG: u32 = 13;
const TYPE_PNG: u32 = 14;
const TYPE_BE_SIGNED_INT: u32 = 21;

/// Caps the uppercased key so a crafted `name` box cannot force a large arena allocation.
const MAX_FREEFORM_KEY: usize = 128;

/// Where the `moov` box is, relative to a prefix the caller has already read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoovExtent {
    InPrefix {
        offset: usize,
        len: usize,
    },
    /// Re-read `file[offset..offset + len]` and call again. The range is the `moov` box or the
    /// unread tail where a trailing `moov` lives.
    Beyond {
        offset: u64,
        len: u64,
    },
    /// No `moov` and nothing left to read.
    NotFound,
}

/// Walks the top-level boxes of `prefix` (14496-12 4.2). `file_len` resolves `size == 0`
/// boxes and rejects a box running past the file.
pub fn locate_moov(prefix: &[u8], file_len: u64) -> MoovExtent {
    let mut at = 0u64;
    while at < file_len {
        let Ok(pos) = usize::try_from(at) else {
            return MoovExtent::NotFound;
        };
        match probe_box(prefix, pos, file_len) {
            Probe::Found(kind, total) => {
                // `total >= 8`, so `at` strictly increases and the loop cannot spin.
                let end = at.saturating_add(total);
                if end > file_len {
                    return MoovExtent::NotFound; // runs past the file: give up, don't guess
                }
                if kind == boxtype::MOOV {
                    return if end <= prefix.len() as u64 {
                        MoovExtent::InPrefix { offset: pos, len: total as usize }
                    } else {
                        MoovExtent::Beyond { offset: at, len: total }
                    };
                }
                at = end;
            }
            // A trailing `moov` lives in the unread rest. A caller already holding the whole file
            // would spin on another read.
            Probe::Unread => {
                return if file_len > prefix.len() as u64 {
                    MoovExtent::Beyond { offset: at, len: file_len - at }
                } else {
                    MoovExtent::NotFound
                };
            }
            Probe::Malformed => return MoovExtent::NotFound,
        }
    }
    MoovExtent::NotFound
}

enum Probe {
    Found(FourCc, u64),
    Unread,
    Malformed,
}

/// Unlike `boxes::read_box_header` this needs no body and resolves `size == 0` against the
/// file, not the buffer (4.2).
fn probe_box(prefix: &[u8], at: usize, file_len: u64) -> Probe {
    let Some(head) = prefix.get(at..at.saturating_add(8)) else {
        return Probe::Unread;
    };
    let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
    let kind: FourCc = [head[4], head[5], head[6], head[7]];
    let (header_len, total) = match size32 {
        1 => {
            let Some(ls) = prefix.get(at + 8..at.saturating_add(16)) else {
                return Probe::Unread;
            };
            (16u64, u64::from_be_bytes([ls[0], ls[1], ls[2], ls[3], ls[4], ls[5], ls[6], ls[7]]))
        }
        0 => match file_len.checked_sub(at as u64) {
            Some(n) => (8, n),
            None => return Probe::Malformed,
        },
        n => (8, n),
    };
    if total < header_len {
        return Probe::Malformed;
    }
    Probe::Found(kind, total)
}

/// Parses iTunes metadata in a complete `moov` box, header included, into `sink`.
pub fn parse_moov_tags(moov: &[u8], scratch: &Arena, sink: &mut impl TagSink) {
    let Some(body) = moov_body(moov) else { return };

    let via_udta = boxes::find_child(body, UDTA).ok().flatten().and_then(|u| {
        let udta = u.body(body);
        boxes::find_child(udta, META).ok().flatten().map(|m| m.body(udta))
    });
    let meta =
        via_udta.or_else(|| boxes::find_child(body, META).ok().flatten().map(|m| m.body(body)));
    let Some(meta) = meta else { return };
    let Some(ilst) = ilst_children(meta) else {
        return;
    };

    let _ = boxes::for_each_child(ilst, |item| {
        parse_item(item.kind, item.body(ilst), scratch, sink);
        Ok(())
    });
}

fn moov_body(moov: &[u8]) -> Option<&[u8]> {
    let h = boxes::read_box_header(moov, 0).ok()?;
    (h.kind == boxtype::MOOV).then(|| h.body(moov))
}

/// `meta` is a FullBox (8.11.1), so its children start four octets in. QuickTime-heritage
/// files omit the version and flags, so offset 0 is the fallback.
fn ilst_children(meta: &[u8]) -> Option<&[u8]> {
    for children in [meta.get(4..)?, meta] {
        if let Ok(Some(h)) = boxes::find_child(children, ILST) {
            return Some(h.body(children));
        }
    }
    None
}

fn parse_item(kind: FourCc, item: &[u8], scratch: &Arena, sink: &mut impl TagSink) {
    match &kind {
        b"trkn" => number_pair(item, "TRACKNUMBER", sink),
        b"disk" => number_pair(item, "DISCNUMBER", sink),
        b"covr" => cover(item, sink),
        b"----" => freeform(item, scratch, sink),
        // Legacy ID3v1 genre index. Skipped, files with it carry the text genre too.
        b"gnre" => {}
        _ => {
            if let Some(key) = text_key(kind) {
                text_item(item, key, sink);
            }
        }
    }
}

fn text_key(kind: FourCc) -> Option<&'static str> {
    Some(match kind {
        [COPYRIGHT, b'n', b'a', b'm'] => "TITLE",
        [COPYRIGHT, b'A', b'R', b'T'] => "ARTIST",
        [COPYRIGHT, b'a', b'l', b'b'] => "ALBUM",
        [b'a', b'A', b'R', b'T'] => "ALBUMARTIST",
        [COPYRIGHT, b'd', b'a', b'y'] => "DATE",
        [COPYRIGHT, b'g', b'e', b'n'] => "GENRE",
        [COPYRIGHT, b'w', b'r', b't'] => "COMPOSER",
        [COPYRIGHT, b'c', b'm', b't'] => "COMMENT",
        _ => return None,
    })
}

/// A `data` atom has a 4 octet type indicator and a 4 octet locale before the value (QTFF
/// Metadata).
fn for_each_data<F: FnMut(u32, &[u8])>(item: &[u8], mut f: F) {
    let _ = boxes::for_each_child(item, |h| {
        if h.kind == DATA {
            let d = h.body(item);
            if let Some(head) = d.get(..8) {
                let indicator = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
                if indicator >> 24 == 0 {
                    f(indicator & 0x00FF_FFFF, &d[8..]);
                }
            }
        }
        Ok(())
    });
}

/// Invalid UTF-8 is dropped rather than replaced.
fn text_item(item: &[u8], key: &'static str, sink: &mut impl TagSink) {
    for_each_data(item, |ty, payload| match ty {
        TYPE_UTF8 => {
            if let Ok(s) = core::str::from_utf8(payload) {
                sink.text(key, s);
            }
        }
        TYPE_BE_SIGNED_INT => {
            if let Some(n) = be_signed_int(payload) {
                let mut s = FixedStr::<24>::new();
                let _ = write!(s, "{n}");
                if let Some(v) = s.as_str() {
                    sink.text(key, v);
                }
            }
        }
        _ => {}
    });
}

/// The `trkn`/`disk` payload is 2 reserved octets, then index and total as u16 BE. Emitted as
/// `3/12` or `3`, and a zero index is dropped.
fn number_pair(item: &[u8], key: &'static str, sink: &mut impl TagSink) {
    for_each_data(item, |ty, payload| {
        if ty != TYPE_IMPLICIT {
            return;
        }
        let Some(b) = payload.get(..6) else { return };
        let index = u16::from_be_bytes([b[2], b[3]]);
        let total = u16::from_be_bytes([b[4], b[5]]);
        if index == 0 {
            return;
        }
        // Stack-formatted: "65535/65535" is 11 octets, so 16 cannot overflow.
        let mut s = FixedStr::<16>::new();
        let _ = if total == 0 { write!(s, "{index}") } else { write!(s, "{index}/{total}") };
        if let Some(v) = s.as_str() {
            sink.text(key, v);
        }
    });
}

/// Implicit-typed pictures are sniffed by magic, JPEG `FF D8` (T.81 B.1.1.3) or PNG `89 50 4E 47`.
fn cover(item: &[u8], sink: &mut impl TagSink) {
    for_each_data(item, |ty, payload| {
        let mime = match ty {
            TYPE_JPEG => "image/jpeg",
            TYPE_PNG => "image/png",
            TYPE_IMPLICIT => match payload {
                [0xFF, 0xD8, ..] => "image/jpeg",
                [0x89, 0x50, 0x4E, 0x47, ..] => "image/png",
                _ => return,
            },
            _ => return,
        };
        if !payload.is_empty() {
            sink.picture(mime, payload);
        }
    });
}

/// The `name` child, ASCII-uppercased, is the key. The `mean` namespace is not matched, since
/// taggers write ReplayGain under their own.
fn freeform(item: &[u8], scratch: &Arena, sink: &mut impl TagSink) {
    let Some(h) = boxes::find_child(item, NAME).ok().flatten() else {
        return;
    };
    let Some(name) = h.body(item).get(4..) else {
        return;
    };
    if name.is_empty() || name.len() > MAX_FREEFORM_KEY {
        return;
    }
    let Ok(buf) = scratch.alloc_bytes(name.len()) else {
        return;
    };
    buf.copy_from_slice(name);
    buf.make_ascii_uppercase();
    let Ok(key) = core::str::from_utf8(buf) else {
        return;
    };
    for_each_data(item, |ty, payload| {
        if ty == TYPE_UTF8
            && let Ok(v) = core::str::from_utf8(payload)
        {
            sink.text(key, v);
        }
    });
}

/// Apple writes type 21 as 1, 2, 4 or 8 octets, and the payload length is the width.
fn be_signed_int(payload: &[u8]) -> Option<i64> {
    Some(match payload.len() {
        1 => payload[0] as i8 as i64,
        2 => i16::from_be_bytes([payload[0], payload[1]]) as i64,
        4 => i32::from_be_bytes(payload.try_into().ok()?) as i64,
        8 => i64::from_be_bytes(payload.try_into().ok()?),
        _ => return None,
    })
}

/// Fixed-capacity alloc-free `write!` sink. Overflow is sticky, so a half-written number is
/// never emitted.
struct FixedStr<const N: usize> {
    buf: [u8; N],
    len: usize,
    ok: bool,
}

impl<const N: usize> FixedStr<N> {
    fn new() -> Self {
        Self { buf: [0; N], len: 0, ok: true }
    }

    fn as_str(&self) -> Option<&str> {
        self.ok.then(|| core::str::from_utf8(&self.buf[..self.len]).ok())?
    }
}

impl<const N: usize> core::fmt::Write for FixedStr<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        match self.buf.get_mut(self.len..self.len + s.len()) {
            Some(dst) => {
                dst.copy_from_slice(s.as_bytes());
                self.len += s.len();
                Ok(())
            }
            None => {
                self.ok = false;
                Err(core::fmt::Error)
            }
        }
    }
}

/// What a listing view shows without decoding. A fragmented file has no duration, a video-only
/// one no rate or channels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mp4Props {
    /// Presentation duration in nanoseconds.
    pub duration_ns: Option<u64>,
    /// Audio sample rate in Hz, from the sound track's sample entry.
    pub sample_rate: Option<u32>,
    /// Audio channel count, from the sound track's sample entry.
    pub channels: Option<u32>,
}

/// Best-effort duration, rate and channels from a complete `moov`. The edit list beats `mdhd`
/// only when they differ by more than one movie tick, a smaller gap is rounding. `mvhd` beats
/// the sound track only in a multi-track movie, where another track may be longer (8.2.2).
pub fn props_from_moov(moov: &[u8]) -> Mp4Props {
    let mut props = Mp4Props::default();
    let Some(body) = moov_body(moov) else {
        return props;
    };

    let mvhd = boxes::find_child(body, boxtype::MVHD)
        .ok()
        .flatten()
        .and_then(|h| boxes::parse_mvhd(h.body(body)).ok());
    let movie_ns = mvhd.and_then(|m| ticks_to_ns(m.duration, m.timescale));
    let tick_ns = mvhd.and_then(|m| movie_tick_ns(m.timescale));

    let mut soun: Option<(&[u8], &[u8])> = None;
    let mut tracks = 0usize;
    let _ = boxes::for_each_child(body, |h| {
        if h.kind == boxtype::TRAK {
            tracks += 1;
            let trak = h.body(body);
            if soun.is_none()
                && let Ok(Some(m)) = boxes::find_child(trak, boxtype::MDIA)
            {
                let mdia = m.body(trak);
                if handler_type(mdia) == Some(*b"soun") {
                    soun = Some((trak, mdia));
                }
            }
        }
        Ok(())
    });

    if let Some((trak, mdia)) = soun {
        let media_ns = boxes::find_child(mdia, boxtype::MDHD)
            .ok()
            .flatten()
            .and_then(|h| boxes::parse_mdhd(h.body(mdia)).ok())
            .and_then(|m| ticks_to_ns(m.duration, m.timescale));
        let edit_ns = mvhd.and_then(|m| edit_duration_ns(trak, m.timescale));
        props.duration_ns = pick_track_duration(media_ns, edit_ns, tick_ns);

        if let Some((rate, channels)) = audio_format(mdia) {
            props.sample_rate = (rate != 0).then_some(rate);
            props.channels = (channels != 0).then_some(channels);
        }
    }

    match props.duration_ns {
        Some(track) if tracks > 1 => {
            if let (Some(movie), Some(tick)) = (movie_ns, tick_ns)
                && movie > track.saturating_add(tick)
            {
                props.duration_ns = Some(movie);
            }
        }
        Some(_) => {}
        None => props.duration_ns = movie_ns,
    }
    props
}

/// One movie tick in ns, rounded up so the one-tick guard is inclusive.
fn movie_tick_ns(movie_timescale: u32) -> Option<u64> {
    (movie_timescale != 0).then(|| 1_000_000_000u64.div_ceil(movie_timescale as u64))
}

fn pick_track_duration(
    media_ns: Option<u64>,
    edit_ns: Option<u64>,
    tick_ns: Option<u64>,
) -> Option<u64> {
    match (media_ns, edit_ns) {
        (Some(media), Some(edit)) => {
            let tick = tick_ns.unwrap_or(0);
            Some(if media.abs_diff(edit) > tick { edit } else { media })
        }
        (media, edit) => media.or(edit),
    }
}

fn edit_duration_ns(trak: &[u8], movie_timescale: u32) -> Option<u64> {
    let edts = boxes::find_child(trak, boxtype::EDTS).ok()??;
    let edts = edts.body(trak);
    let elst = boxes::find_child(edts, boxtype::ELST).ok()??;
    ticks_to_ns(elst_presented_ticks(elst.body(edts))?, movie_timescale)
}

/// Empty edits (`media_time == -1`) are gaps and are skipped. `None` for a list that cannot
/// be trusted.
fn elst_presented_ticks(body: &[u8]) -> Option<u64> {
    let version = *body.first()?;
    let count = u32::from_be_bytes(body.get(4..8)?.try_into().ok()?) as usize;
    // An unknown version has an unknown entry width, so nothing can be walked.
    let width = match version {
        0 => 12usize,
        1 => 20usize,
        _ => return None,
    };
    let entries = body.get(8..)?;
    // `entry_count` is untrusted, so bound it by the bytes present.
    if count > entries.len() / width {
        return None;
    }

    let mut total = 0u64;
    let mut presented = 0usize;
    for i in 0..count {
        let e = entries.get(i * width..(i + 1) * width)?;
        let (segment_duration, media_time) = if version == 1 {
            (
                u64::from_be_bytes(e.get(0..8)?.try_into().ok()?),
                i64::from_be_bytes(e.get(8..16)?.try_into().ok()?),
            )
        } else {
            (
                u32::from_be_bytes(e.get(0..4)?.try_into().ok()?) as u64,
                i32::from_be_bytes(e.get(4..8)?.try_into().ok()?) as i64,
            )
        };
        if media_time != -1 {
            total = total.checked_add(segment_duration)?;
            presented += 1;
        }
    }
    (presented != 0).then_some(total)
}

fn handler_type(mdia: &[u8]) -> Option<FourCc> {
    let h = boxes::find_child(mdia, HDLR).ok()??;
    let b = h.body(mdia).get(8..12)?;
    Some([b[0], b[1], b[2], b[3]])
}

/// A version 2 QuickTime sound description stores the rate as `f64` at octet 32 and the
/// channels as u32 at octet 40 (8.5.2, 12.2.3).
fn audio_format(mdia: &[u8]) -> Option<(u32, u32)> {
    let minf = boxes::find_child(mdia, boxtype::MINF).ok()??;
    let minf = minf.body(mdia);
    let stbl = boxes::find_child(minf, boxtype::STBL).ok()??;
    let stbl = stbl.body(minf);
    let stsd = boxes::find_child(stbl, boxtype::STSD).ok()??;
    let entries = stsd.body(stbl).get(8..)?;
    let entry = boxes::read_box_header(entries, 0).ok()?;
    let e = entry.body(entries);

    let version = u16::from_be_bytes([*e.get(8)?, *e.get(9)?]);
    if version == 2 {
        let rate = f64::from_bits(u64::from_be_bytes(e.get(32..40)?.try_into().ok()?));
        let channels = u32::from_be_bytes(e.get(40..44)?.try_into().ok()?);
        let channels = channels.min(u32::from(profluens_audio::MAX_CHANNELS));
        let rate = if rate.is_finite() && rate > 0.0 { rate as u32 } else { 0 };
        return Some((rate, channels));
    }
    let channels = u16::from_be_bytes([*e.get(16)?, *e.get(17)?]) as u32;
    let rate = u32::from_be_bytes(e.get(24..28)?.try_into().ok()?) >> 16;
    Some((rate, channels))
}

/// `None` for a zero or all-ones duration, which means unknown (8.2.2, 8.4.2).
fn ticks_to_ns(duration: u64, timescale: u32) -> Option<u64> {
    if timescale == 0 || duration == 0 || duration == u32::MAX as u64 || duration == u64::MAX {
        return None;
    }
    u64::try_from(duration as u128 * 1_000_000_000 / timescale as u128).ok()
}

#[cfg(test)]
// Test-only heap use.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn full_bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 4];
        b.extend_from_slice(body);
        bx(kind, &b)
    }

    fn data_box(ty: u32, value: &[u8]) -> Vec<u8> {
        let mut b = ty.to_be_bytes().to_vec();
        b.extend_from_slice(&0u32.to_be_bytes()); // locale indicator
        b.extend_from_slice(value);
        bx(b"data", &b)
    }

    fn item(kind: &[u8; 4], ty: u32, value: &[u8]) -> Vec<u8> {
        bx(kind, &data_box(ty, value))
    }

    fn moov_with_items(items: &[Vec<u8>]) -> Vec<u8> {
        let mut ilst = Vec::new();
        for i in items {
            ilst.extend_from_slice(i);
        }
        let mut hdlr_body = vec![0u8; 4];
        hdlr_body.extend_from_slice(b"mdir");
        hdlr_body.extend_from_slice(b"appl");
        hdlr_body.extend_from_slice(&[0u8; 9]);
        let mut meta_body = bx(b"hdlr", &hdlr_body);
        meta_body.extend_from_slice(&bx(b"ilst", &ilst));
        let meta = full_bx(b"meta", &meta_body);
        bx(b"moov", &bx(b"udta", &meta))
    }

    #[derive(Default)]
    struct Collect {
        text: Vec<(String, String)>,
        pics: Vec<(String, Vec<u8>)>,
    }

    impl TagSink for Collect {
        fn text(&mut self, key: &str, value: &str) {
            self.text.push((key.to_string(), value.to_string()));
        }

        fn picture(&mut self, mime: &str, data: &[u8]) {
            self.pics.push((mime.to_string(), data.to_vec()));
        }
    }

    impl Collect {
        fn get(&self, key: &str) -> Option<&str> {
            self.text.iter().find(|(k, _)| k == key).map(|(_, v)| &**v)
        }
    }

    fn tags_of(moov: &[u8]) -> Collect {
        let scratch = Arena::default();
        let mut sink = Collect::default();
        parse_moov_tags(moov, &scratch, &mut sink);
        sink
    }

    #[test]
    fn meta_fullbox_offset_is_honoured() {
        let moov = moov_with_items(&[item(b"\xA9nam", TYPE_UTF8, b"Enough")]);

        // The control. A walk from offset 0 reads version/flags as size 0 and never sees `ilst`.
        let udta = boxes::find_child(&moov[8..], UDTA).unwrap().unwrap();
        let udta = udta.body(&moov[8..]);
        let meta = boxes::find_child(udta, META).unwrap().unwrap();
        let meta = meta.body(udta);
        assert!(
            !matches!(boxes::find_child(meta, ILST), Ok(Some(_))),
            "a non-FullBox walk of `meta` must NOT find `ilst`, that is the bug under test"
        );

        assert_eq!(tags_of(&moov).get("TITLE"), Some("Enough"));
    }

    #[test]
    fn quicktime_non_fullbox_meta_still_parses() {
        let ilst = bx(b"ilst", &item(b"\xA9nam", TYPE_UTF8, b"Mov"));
        let moov = bx(b"moov", &bx(b"udta", &bx(b"meta", &ilst)));
        assert_eq!(tags_of(&moov).get("TITLE"), Some("Mov"));
    }

    #[test]
    fn text_items_map_to_canonical_keys() {
        let moov = moov_with_items(&[
            item(b"\xA9nam", TYPE_UTF8, "Enough".as_bytes()),
            item(b"\xA9ART", TYPE_UTF8, "Fred again..".as_bytes()),
            item(b"\xA9alb", TYPE_UTF8, "Actual Life 3".as_bytes()),
            item(b"aART", TYPE_UTF8, "Various Artists".as_bytes()),
            item(b"\xA9day", TYPE_UTF8, "2021-10-29".as_bytes()),
            item(b"\xA9gen", TYPE_UTF8, "Électronique".as_bytes()),
            item(b"\xA9wrt", TYPE_UTF8, "F. Gibson".as_bytes()),
            item(b"\xA9cmt", TYPE_UTF8, "ripped".as_bytes()),
            item(b"stik", TYPE_IMPLICIT, &[1]), // unmapped id: skipped silently
        ]);
        let t = tags_of(&moov);
        assert_eq!(t.get("TITLE"), Some("Enough"));
        assert_eq!(t.get("ARTIST"), Some("Fred again.."));
        assert_eq!(t.get("ALBUM"), Some("Actual Life 3"));
        assert_eq!(t.get("ALBUMARTIST"), Some("Various Artists"));
        assert_eq!(t.get("DATE"), Some("2021-10-29"));
        assert_eq!(t.get("GENRE"), Some("Électronique"), "non-ASCII UTF-8 passes through");
        assert_eq!(t.get("COMPOSER"), Some("F. Gibson"));
        assert_eq!(t.get("COMMENT"), Some("ripped"));
        assert_eq!(t.text.len(), 8, "the unmapped `stik` item is skipped");
        assert!(t.pics.is_empty());
    }

    #[test]
    fn integer_typed_text_item_is_rendered_decimally() {
        let moov = moov_with_items(&[item(b"\xA9day", TYPE_BE_SIGNED_INT, &2015u16.to_be_bytes())]);
        assert_eq!(tags_of(&moov).get("DATE"), Some("2015"));
    }

    #[test]
    fn trkn_and_disk_emit_index_and_total() {
        let pair = |index: u16, total: u16| {
            let mut v = vec![0u8, 0];
            v.extend_from_slice(&index.to_be_bytes());
            v.extend_from_slice(&total.to_be_bytes());
            v.extend_from_slice(&[0, 0]);
            v
        };
        let moov = moov_with_items(&[
            item(b"trkn", TYPE_IMPLICIT, &pair(3, 12)),
            item(b"disk", TYPE_IMPLICIT, &pair(1, 2)),
        ]);
        let t = tags_of(&moov);
        assert_eq!(t.get("TRACKNUMBER"), Some("3/12"));
        assert_eq!(t.get("DISCNUMBER"), Some("1/2"));

        let moov = moov_with_items(&[item(b"trkn", TYPE_IMPLICIT, &pair(3, 0))]);
        assert_eq!(tags_of(&moov).get("TRACKNUMBER"), Some("3"));

        let moov = moov_with_items(&[item(b"trkn", TYPE_IMPLICIT, &pair(0, 12))]);
        assert!(tags_of(&moov).text.is_empty());
    }

    #[test]
    fn covr_pictures_round_trip_byte_exact() {
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\xff\x00\x01";
        let jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00";
        let moov = moov_with_items(&[
            item(b"covr", TYPE_PNG, png),
            item(b"covr", TYPE_JPEG, jpeg),
            item(b"covr", TYPE_IMPLICIT, png),
            item(b"covr", TYPE_IMPLICIT, jpeg),
            item(b"covr", TYPE_IMPLICIT, b"not an image"),
            item(b"covr", 42, png), // unknown type: skipped
        ]);
        let t = tags_of(&moov);
        assert_eq!(
            t.pics,
            vec![
                ("image/png".to_string(), png.to_vec()),
                ("image/jpeg".to_string(), jpeg.to_vec()),
                ("image/png".to_string(), png.to_vec()),
                ("image/jpeg".to_string(), jpeg.to_vec()),
            ]
        );
        assert!(t.text.is_empty());
    }

    #[test]
    fn freeform_replaygain_is_uppercased() {
        let mut ff = full_bx(b"mean", b"com.apple.iTunes");
        ff.extend_from_slice(&full_bx(b"name", b"replaygain_track_gain"));
        ff.extend_from_slice(&data_box(TYPE_UTF8, b"-7.35 dB"));
        let mut ff2 = full_bx(b"mean", b"org.example.tagger");
        ff2.extend_from_slice(&full_bx(b"name", b"replaygain_album_peak"));
        ff2.extend_from_slice(&data_box(TYPE_UTF8, b"0.988"));
        let moov = moov_with_items(&[bx(b"----", &ff), bx(b"----", &ff2)]);
        let t = tags_of(&moov);
        assert_eq!(t.get("REPLAYGAIN_TRACK_GAIN"), Some("-7.35 dB"));
        assert_eq!(
            t.get("REPLAYGAIN_ALBUM_PEAK"),
            Some("0.988"),
            "a non-Apple namespace still yields its key"
        );
    }

    #[test]
    fn gnre_is_skipped_but_gen_is_kept() {
        let moov = moov_with_items(&[
            item(b"gnre", TYPE_IMPLICIT, &18u16.to_be_bytes()),
            item(b"\xA9gen", TYPE_UTF8, b"Techno"),
        ]);
        let t = tags_of(&moov);
        assert_eq!(t.text, vec![("GENRE".to_string(), "Techno".to_string())]);
    }

    #[test]
    fn missing_metadata_emits_nothing() {
        assert!(tags_of(&[]).text.is_empty());
        assert!(tags_of(&bx(b"ftyp", b"M4A ")).text.is_empty());
        assert!(tags_of(&bx(b"moov", &bx(b"udta", b""))).text.is_empty());
        assert!(tags_of(&bx(b"moov", &bx(b"udta", &full_bx(b"meta", b"")))).text.is_empty());
    }

    #[test]
    fn locate_moov_in_prefix() {
        let ftyp = bx(b"ftyp", b"M4A \0\0\0\0");
        let moov = moov_with_items(&[item(b"\xA9nam", TYPE_UTF8, b"Front")]);
        let mut file = ftyp.clone();
        file.extend_from_slice(&moov);
        file.extend_from_slice(&bx(b"mdat", &[0u8; 64]));
        let file_len = file.len() as u64;

        let prefix = &file[..ftyp.len() + moov.len() + 16];
        let MoovExtent::InPrefix { offset, len } = locate_moov(prefix, file_len) else {
            panic!("moov is entirely inside the prefix");
        };
        assert_eq!((offset, len), (ftyp.len(), moov.len()));
        assert_eq!(tags_of(&prefix[offset..offset + len]).get("TITLE"), Some("Front"));
    }

    #[test]
    fn locate_moov_after_mdat_reports_beyond() {
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0");
        let mdat = bx(b"mdat", &vec![0u8; 4096]);
        let moov = moov_with_items(&[item(b"\xA9nam", TYPE_UTF8, b"Tail")]);
        let mut file = ftyp.clone();
        file.extend_from_slice(&mdat);
        file.extend_from_slice(&moov);
        let file_len = file.len() as u64;
        let moov_at = (ftyp.len() + mdat.len()) as u64;

        let got = locate_moov(&file[..moov_at as usize + 12], file_len);
        assert_eq!(got, MoovExtent::Beyond { offset: moov_at, len: moov.len() as u64 });

        let MoovExtent::Beyond { offset, len } = got else { unreachable!() };
        let re_read = &file[offset as usize..(offset + len) as usize];
        assert_eq!(tags_of(re_read).get("TITLE"), Some("Tail"));

        assert_eq!(
            locate_moov(&file[..64], file_len),
            MoovExtent::Beyond { offset: moov_at, len: file_len - moov_at }
        );
    }

    #[test]
    fn locate_moov_unread_tail_is_re_locatable() {
        let mdat = bx(b"mdat", &vec![0u8; 4096]);
        let moov = moov_with_items(&[item(b"\xA9nam", TYPE_UTF8, b"Loop")]);
        let free = bx(b"free", &[0u8; 32]);
        let mut file = mdat.clone();
        file.extend_from_slice(&moov);
        file.extend_from_slice(&free);
        let file_len = file.len() as u64;

        let tail_at = mdat.len() as u64;
        assert_eq!(
            locate_moov(&file[..16], file_len),
            MoovExtent::Beyond { offset: tail_at, len: file_len - tail_at },
            "the tail is `moov` + `free`, not just the box"
        );

        let tail = &file[tail_at as usize..];
        assert_eq!(
            locate_moov(tail, tail.len() as u64),
            MoovExtent::InPrefix { offset: 0, len: moov.len() }
        );
        assert_eq!(tags_of(&tail[..moov.len()]).get("TITLE"), Some("Loop"));
    }

    #[test]
    fn locate_moov_cut_short_by_the_prefix_is_beyond() {
        let ftyp = bx(b"ftyp", b"M4A \0\0\0\0");
        let moov = moov_with_items(&[item(b"\xA9nam", TYPE_UTF8, b"Cut")]);
        let mut file = ftyp.clone();
        file.extend_from_slice(&moov);
        let file_len = file.len() as u64;
        let prefix = &file[..ftyp.len() + 12]; // header of `moov` + a few octets
        assert_eq!(
            locate_moov(prefix, file_len),
            MoovExtent::Beyond { offset: ftyp.len() as u64, len: moov.len() as u64 }
        );
    }

    #[test]
    fn locate_moov_honours_64bit_largesize() {
        let payload = [0xAAu8; 32];
        let mut mdat = 1u32.to_be_bytes().to_vec();
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(&((16 + payload.len()) as u64).to_be_bytes());
        mdat.extend_from_slice(&payload);
        let moov = moov_with_items(&[item(b"\xA9nam", TYPE_UTF8, b"Large")]);
        let mut file = mdat.clone();
        file.extend_from_slice(&moov);
        let file_len = file.len() as u64;

        assert_eq!(
            locate_moov(&file, file_len),
            MoovExtent::InPrefix { offset: mdat.len(), len: moov.len() }
        );
        assert_eq!(
            locate_moov(&file[..16], file_len),
            MoovExtent::Beyond { offset: mdat.len() as u64, len: moov.len() as u64 }
        );
    }

    #[test]
    fn locate_moov_resolves_size_zero_against_the_file() {
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0");

        let mut open_mdat = 0u32.to_be_bytes().to_vec();
        open_mdat.extend_from_slice(b"mdat");
        open_mdat.extend_from_slice(&[7u8; 100]);
        let mut file = ftyp.clone();
        file.extend_from_slice(&open_mdat);
        assert_eq!(locate_moov(&file, file.len() as u64), MoovExtent::NotFound);

        let moov = moov_with_items(&[item(b"\xA9nam", TYPE_UTF8, b"Open")]);
        let mut open_moov = 0u32.to_be_bytes().to_vec();
        open_moov.extend_from_slice(b"moov");
        open_moov.extend_from_slice(&moov[8..]);
        let mut file = ftyp.clone();
        file.extend_from_slice(&open_moov);
        let file_len = file.len() as u64;
        assert_eq!(
            locate_moov(&file[..ftyp.len() + 10], file_len),
            MoovExtent::Beyond { offset: ftyp.len() as u64, len: open_moov.len() as u64 }
        );
    }

    #[test]
    fn locate_moov_rejects_junk_without_panicking() {
        assert_eq!(locate_moov(&[], 0), MoovExtent::NotFound);
        assert_eq!(locate_moov(b"not-an-mp4", 10), MoovExtent::NotFound);
        assert_eq!(locate_moov(&[0, 0, 0, 8, b'f'], 5), MoovExtent::NotFound);
        let mut short = 4u32.to_be_bytes().to_vec();
        short.extend_from_slice(b"ftyp");
        assert_eq!(locate_moov(&short, short.len() as u64), MoovExtent::NotFound);
        let mut huge = u32::MAX.to_be_bytes().to_vec();
        huge.extend_from_slice(b"free");
        assert_eq!(locate_moov(&huge, huge.len() as u64), MoovExtent::NotFound);
    }

    fn mdhd_body(version: u8, timescale: u32, duration: u64) -> Vec<u8> {
        let mut b = vec![version, 0, 0, 0];
        if version == 0 {
            b.extend_from_slice(&[0u8; 8]); // creation/modification time
            b.extend_from_slice(&timescale.to_be_bytes());
            b.extend_from_slice(&(duration as u32).to_be_bytes());
        } else {
            b.extend_from_slice(&[0u8; 16]);
            b.extend_from_slice(&timescale.to_be_bytes());
            b.extend_from_slice(&duration.to_be_bytes());
        }
        b.extend_from_slice(&[0x55, 0xC4, 0, 0]); // language + pre_defined
        b
    }

    fn mvhd_body(timescale: u32, duration: u32) -> Vec<u8> {
        let mut b = vec![0u8; 4];
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&timescale.to_be_bytes());
        b.extend_from_slice(&duration.to_be_bytes());
        b.extend_from_slice(&[0u8; 80]); // rate, volume, matrix, pre_defined, next_track_ID
        b
    }

    fn hdlr_body(handler: &[u8; 4]) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b.extend_from_slice(handler);
        b.extend_from_slice(&[0u8; 13]);
        b
    }

    fn audio_minf(entry: &[u8]) -> Vec<u8> {
        let mut stsd_body = vec![0u8; 4]; // FullBox version/flags
        stsd_body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        stsd_body.extend_from_slice(entry);
        let stbl = bx(b"stbl", &bx(b"stsd", &stsd_body));
        bx(b"minf", &stbl)
    }

    fn mp4a_entry(rate: u32, channels: u16) -> Vec<u8> {
        let mut ae = vec![0u8; 28];
        ae[16..18].copy_from_slice(&channels.to_be_bytes());
        ae[24..28].copy_from_slice(&(rate << 16).to_be_bytes()); // 16.16 fixed point
        bx(b"mp4a", &ae)
    }

    fn moov_with_track(
        mvhd: Option<Vec<u8>>,
        handler: &[u8; 4],
        mdhd: &[u8],
        minf: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let mut mdia = bx(b"hdlr", &hdlr_body(handler));
        mdia.extend_from_slice(&bx(b"mdhd", mdhd));
        if let Some(minf) = minf {
            mdia.extend_from_slice(&minf);
        }
        let mut body = mvhd.map(|m| bx(b"mvhd", &m)).unwrap_or_default();
        body.extend_from_slice(&bx(b"trak", &bx(b"mdia", &mdia)));
        bx(b"moov", &body)
    }

    #[test]
    fn props_duration_from_sound_track_mdhd_v0_and_v1() {
        let moov = moov_with_track(
            Some(mvhd_body(1000, 99_000)), // deliberately wrong: mdhd must win
            b"soun",
            &mdhd_body(0, 44_100, 441_000),
            None,
        );
        assert_eq!(props_from_moov(&moov).duration_ns, Some(10_000_000_000));

        let moov = moov_with_track(None, b"soun", &mdhd_body(1, 48_000, 5_400_000_000), None);
        assert_eq!(props_from_moov(&moov).duration_ns, Some(112_500_000_000_000));
    }

    #[test]
    fn props_falls_back_to_mvhd() {
        let moov = moov_with_track(
            Some(mvhd_body(600, 1_800)), // 3 s
            b"vide",
            &mdhd_body(0, 30_000, 90_000),
            None,
        );
        let p = props_from_moov(&moov);
        assert_eq!(p.duration_ns, Some(3_000_000_000));
        assert_eq!((p.sample_rate, p.channels), (None, None), "no sound track, no audio format");

        let moov =
            moov_with_track(Some(mvhd_body(600, 1_200)), b"soun", &mdhd_body(0, 44_100, 0), None);
        assert_eq!(props_from_moov(&moov).duration_ns, Some(2_000_000_000));

        let moov =
            moov_with_track(Some(mvhd_body(600, 0)), b"soun", &mdhd_body(0, 44_100, 0), None);
        assert_eq!(props_from_moov(&moov).duration_ns, None);
    }

    #[test]
    fn props_reads_rate_and_channels_from_the_sample_entry() {
        let moov = moov_with_track(
            None,
            b"soun",
            &mdhd_body(0, 48_000, 96_000),
            Some(audio_minf(&mp4a_entry(48_000, 2))),
        );
        let p = props_from_moov(&moov);
        assert_eq!(p.sample_rate, Some(48_000));
        assert_eq!(p.channels, Some(2));
        assert_eq!(p.duration_ns, Some(2_000_000_000));

        let mut alac = vec![0u8; 28];
        alac[16..18].copy_from_slice(&6u16.to_be_bytes());
        alac[24..28].copy_from_slice(&(44_100u32 << 16).to_be_bytes());
        let moov = moov_with_track(
            None,
            b"soun",
            &mdhd_body(0, 44_100, 44_100),
            Some(audio_minf(&bx(b"alac", &alac))),
        );
        let p = props_from_moov(&moov);
        assert_eq!((p.sample_rate, p.channels), (Some(44_100), Some(6)));
    }

    #[test]
    fn props_reads_a_version_2_sound_description() {
        let mut e = vec![0u8; 44];
        e[8..10].copy_from_slice(&2u16.to_be_bytes()); // version 2
        e[24..28].copy_from_slice(&(65_536u32 << 16).to_be_bytes()); // placeholder rate
        e[32..40].copy_from_slice(&192_000f64.to_bits().to_be_bytes());
        e[40..44].copy_from_slice(&8u32.to_be_bytes());
        let moov = moov_with_track(
            None,
            b"soun",
            &mdhd_body(0, 192_000, 192_000),
            Some(audio_minf(&bx(b"lpcm", &e))),
        );
        let p = props_from_moov(&moov);
        assert_eq!((p.sample_rate, p.channels), (Some(192_000), Some(8)));
    }

    #[test]
    fn props_of_junk_is_empty() {
        assert_eq!(props_from_moov(&[]), Mp4Props::default());
        assert_eq!(props_from_moov(&bx(b"ftyp", b"M4A ")), Mp4Props::default());
        assert_eq!(props_from_moov(&bx(b"moov", b"")), Mp4Props::default());
    }

    fn elst_body(version: u8, edits: &[(u64, i64)]) -> Vec<u8> {
        let mut b = vec![version, 0, 0, 0];
        b.extend_from_slice(&(edits.len() as u32).to_be_bytes());
        for &(segment, media_time) in edits {
            if version == 1 {
                b.extend_from_slice(&segment.to_be_bytes());
                b.extend_from_slice(&media_time.to_be_bytes());
            } else {
                b.extend_from_slice(&(segment as u32).to_be_bytes());
                b.extend_from_slice(&(media_time as i32).to_be_bytes());
            }
            b.extend_from_slice(&[0, 1, 0, 0]); // media_rate 1.0 (16.16)
        }
        b
    }

    fn moov_with_edits(
        movie_timescale: u32,
        movie_duration: u32,
        mdhd: &[u8],
        elst: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let mut mdia = bx(b"hdlr", &hdlr_body(b"soun"));
        mdia.extend_from_slice(&bx(b"mdhd", mdhd));
        let mut trak = Vec::new();
        if let Some(elst) = elst {
            trak.extend_from_slice(&bx(b"edts", &bx(b"elst", &elst)));
        }
        trak.extend_from_slice(&bx(b"mdia", &mdia));
        let mut body = bx(b"mvhd", &mvhd_body(movie_timescale, movie_duration));
        body.extend_from_slice(&bx(b"trak", &trak));
        bx(b"moov", &body)
    }

    #[test]
    fn elst_trimming_priming_wins_over_mdhd() {
        let moov = moov_with_edits(
            1_000,
            9_979,
            &mdhd_body(0, 48_000, 480_000),
            Some(elst_body(0, &[(9_979, 1_024)])),
        );
        assert_eq!(props_from_moov(&moov).duration_ns, Some(9_979_000_000));
    }

    #[test]
    fn elst_leading_empty_edit_contributes_no_duration() {
        let moov = moov_with_edits(
            1_000,
            10_479,
            &mdhd_body(0, 48_000, 480_000),
            Some(elst_body(0, &[(500, -1), (9_979, 1_024)])),
        );
        assert_eq!(props_from_moov(&moov).duration_ns, Some(9_979_000_000));

        let moov = moov_with_edits(
            1_000,
            500,
            &mdhd_body(0, 48_000, 480_000),
            Some(elst_body(0, &[(500, -1)])),
        );
        assert_eq!(props_from_moov(&moov).duration_ns, Some(10_000_000_000));
    }

    #[test]
    fn elst_version_1_reads_64_bit_fields() {
        let moov = moov_with_edits(
            1_000,
            0,
            &mdhd_body(1, 48_000, 240_000_000_000),
            Some(elst_body(1, &[(5_000_000_000, 1_024)])),
        );
        assert_eq!(props_from_moov(&moov).duration_ns, Some(5_000_000_000_000_000));

        let moov = moov_with_edits(
            1_000,
            0,
            &mdhd_body(1, 48_000, 240_000_000_000),
            Some(elst_body(1, &[(9_999, -1)])),
        );
        assert_eq!(props_from_moov(&moov).duration_ns, Some(5_000_000_000_000_000));
    }

    /// A one-entry `elst` that only restates the media duration rounded up. Under one tick
    /// apart, so the finer `mdhd` is kept.
    #[test]
    fn elst_that_only_restates_the_media_duration_keeps_the_finer_mdhd() {
        let moov = moov_with_edits(
            1_000,
            343_075,
            &mdhd_body(0, 44_100, 15_129_600),
            Some(elst_body(0, &[(343_075, 0)])),
        );
        assert_eq!(props_from_moov(&moov).duration_ns, Some(343_074_829_931));
    }

    #[test]
    fn absent_elst_leaves_the_duration_unchanged() {
        let with = moov_with_edits(1_000, 10_000, &mdhd_body(0, 48_000, 480_000), None);
        assert_eq!(props_from_moov(&with).duration_ns, Some(10_000_000_000));

        let empty_edts = {
            let mut mdia = bx(b"hdlr", &hdlr_body(b"soun"));
            mdia.extend_from_slice(&bx(b"mdhd", &mdhd_body(0, 48_000, 480_000)));
            let mut trak = bx(b"edts", b"");
            trak.extend_from_slice(&bx(b"mdia", &mdia));
            bx(b"moov", &bx(b"trak", &trak))
        };
        assert_eq!(props_from_moov(&empty_edts).duration_ns, Some(10_000_000_000));

        let no_entries =
            moov_with_edits(1_000, 10_000, &mdhd_body(0, 48_000, 480_000), Some(elst_body(0, &[])));
        assert_eq!(props_from_moov(&no_entries).duration_ns, Some(10_000_000_000));
    }

    #[test]
    fn hostile_elst_falls_back_to_the_media_header() {
        let mdhd = mdhd_body(0, 48_000, 480_000);
        let ten_s = Some(10_000_000_000);

        let mut v7 = elst_body(0, &[(9_979, 1_024)]);
        v7[0] = 7;
        assert_eq!(props_from_moov(&moov_with_edits(1_000, 0, &mdhd, Some(v7))).duration_ns, ten_s);

        let mut liar = elst_body(0, &[(9_979, 1_024)]);
        liar[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            props_from_moov(&moov_with_edits(1_000, 0, &mdhd, Some(liar))).duration_ns,
            ten_s
        );

        let overflow = elst_body(1, &[(u64::MAX, 0), (u64::MAX, 0)]);
        assert_eq!(
            props_from_moov(&moov_with_edits(1_000, 0, &mdhd, Some(overflow))).duration_ns,
            ten_s
        );

        let full = moov_with_edits(
            1_000,
            9_979,
            &mdhd,
            Some(elst_body(0, &[(500, -1), (9_979, 1_024), (7, 3)])),
        );
        for n in 0..full.len() {
            let _ = props_from_moov(&full[..n]);
        }
    }

    #[test]
    fn a_longer_video_track_sets_the_movie_duration() {
        let mut sound = bx(b"edts", &bx(b"elst", &elst_body(0, &[(427_731, 688)])));
        let mut mdia = bx(b"hdlr", &hdlr_body(b"soun"));
        mdia.extend_from_slice(&bx(b"mdhd", &mdhd_body(0, 48_000, 20_531_761)));
        sound.extend_from_slice(&bx(b"mdia", &mdia));

        let mut video = bx(b"edts", &bx(b"elst", &elst_body(0, &[(427_734, 2_048)])));
        let mut vmdia = bx(b"hdlr", &hdlr_body(b"vide"));
        vmdia.extend_from_slice(&bx(b"mdhd", &mdhd_body(0, 19_200, 8_212_480)));
        video.extend_from_slice(&bx(b"mdia", &vmdia));

        let mut body = bx(b"mvhd", &mvhd_body(1_000, 427_734));
        body.extend_from_slice(&bx(b"trak", &video));
        body.extend_from_slice(&bx(b"trak", &sound));
        let moov = bx(b"moov", &body);

        assert_eq!(props_from_moov(&moov).duration_ns, Some(427_734_000_000));
    }

    #[test]
    fn a_lone_sound_track_outranks_a_disagreeing_movie_header() {
        let moov = moov_with_edits(1_000, 99_000, &mdhd_body(0, 48_000, 480_000), None);
        assert_eq!(props_from_moov(&moov).duration_ns, Some(10_000_000_000));
    }

    #[test]
    fn truncation_never_panics() {
        let mut ff = full_bx(b"mean", b"com.apple.iTunes");
        ff.extend_from_slice(&full_bx(b"name", b"replaygain_track_gain"));
        ff.extend_from_slice(&data_box(TYPE_UTF8, b"-7.35 dB"));
        let mut trkn = vec![0u8, 0];
        trkn.extend_from_slice(&3u16.to_be_bytes());
        trkn.extend_from_slice(&12u16.to_be_bytes());
        trkn.extend_from_slice(&[0, 0]);

        let items = [
            item(b"\xA9nam", TYPE_UTF8, "Enough".as_bytes()),
            item(b"trkn", TYPE_IMPLICIT, &trkn),
            item(b"covr", TYPE_PNG, b"\x89PNG\r\n\x1a\n"),
            bx(b"----", &ff),
        ];
        let mut ilst = Vec::new();
        for i in &items {
            ilst.extend_from_slice(i);
        }
        let mut meta_body = bx(b"hdlr", &hdlr_body(b"mdir"));
        meta_body.extend_from_slice(&bx(b"ilst", &ilst));
        let udta = bx(b"udta", &full_bx(b"meta", &meta_body));

        let mut mdia = bx(b"hdlr", &hdlr_body(b"soun"));
        mdia.extend_from_slice(&bx(b"mdhd", &mdhd_body(0, 44_100, 441_000)));
        mdia.extend_from_slice(&audio_minf(&mp4a_entry(44_100, 2)));
        let mut moov_body = bx(b"mvhd", &mvhd_body(600, 6_000));
        moov_body.extend_from_slice(&bx(b"trak", &bx(b"mdia", &mdia)));
        moov_body.extend_from_slice(&udta);
        let moov = bx(b"moov", &moov_body);

        let t = tags_of(&moov);
        assert_eq!(t.get("TITLE"), Some("Enough"));
        assert_eq!(t.get("TRACKNUMBER"), Some("3/12"));
        assert_eq!(t.get("REPLAYGAIN_TRACK_GAIN"), Some("-7.35 dB"));
        assert_eq!(t.pics.len(), 1);
        let p = props_from_moov(&moov);
        assert_eq!(p.duration_ns, Some(10_000_000_000));
        assert_eq!((p.sample_rate, p.channels), (Some(44_100), Some(2)));

        let mut file = bx(b"ftyp", b"M4A \0\0\0\0");
        file.extend_from_slice(&moov);
        for n in 0..=file.len() {
            let _ = locate_moov(&file[..n], file.len() as u64);
        }
        for n in 0..=moov.len() {
            let cut = &moov[..n];
            let _ = tags_of(cut);
            let _ = props_from_moov(cut);
        }

        for n in 8..moov.len() {
            let mut cut = moov[..n].to_vec();
            cut[..4].copy_from_slice(&(moov.len() as u32).to_be_bytes());
            let _ = tags_of(&cut);
            let _ = props_from_moov(&cut);
        }
    }

    #[test]
    fn fixed_str_overflow_is_sticky() {
        let mut s = FixedStr::<4>::new();
        assert!(write!(s, "12").is_ok());
        assert_eq!(s.as_str(), Some("12"));
        assert!(write!(s, "34567").is_err());
        assert_eq!(s.as_str(), None, "an overflowed buffer yields nothing at all");
    }
}
