//! `Tags` and `Attachments` (RFC 9559 sections 5.1.7, 5.1.8) parsed from a borrowed byte window,
//! with the front `SeekHead` locating masters written after the Clusters. Allocates nothing.

use profluens_core::event::TagSink;

use crate::ebml::{self, id};
use crate::reader::{Track, parse_info, read_uint};

/// `SimpleTag` nesting is unbounded (section 5.1.8.1.2), the cap bounds a crafted chain.
pub const MAX_NEST: usize = 6;

/// Levels >= this (TRACK, section 5.1.8.1.1.1) describe the file or its tracks, below them a part.
pub const TARGET_TRACK: u64 = 30;

pub const TARGET_DEFAULT: u64 = 50;

const MAX_KEY: usize = 64;

/// Absolute offsets of each master's ID octet, as far as a prefix reveals them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SegmentIndex {
    pub segment_data_start: u64,
    pub info: Option<u64>,
    pub tracks: Option<u64>,
    pub tags: Option<u64>,
    pub attachments: Option<u64>,
    pub chapters: Option<u64>,
}

impl SegmentIndex {
    pub fn lowest(&self) -> Option<u64> {
        [self.info, self.tracks, self.tags, self.attachments].into_iter().flatten().min()
    }
}

/// Where the Segment's metadata masters are in a `prefix` starting at byte 0. Elements in the
/// window win over `SeekHead` claims, and the first two `SeekHead`s in the window are read. One a
/// `SeekHead` points to past the window is not.
pub fn index_segment(prefix: &[u8]) -> Option<SegmentIndex> {
    let mut at = 0usize;
    let segment_data_start = loop {
        let h = ebml::read_element_header(prefix, at).ok()?;
        if h.id == id::SEGMENT {
            break h.data_start as u64;
        }
        let end = h.data_end().ok()??;
        if end <= at || end > prefix.len() {
            return None; // truncated before the Segment, or a malformed size
        }
        at = end;
    };

    let mut index = SegmentIndex { segment_data_start, ..SegmentIndex::default() };
    let mut seek_heads = 0usize;
    let mut at = segment_data_start as usize;
    // `seek_heads` bounds how many are read.
    while at < prefix.len() {
        let Ok(h) = ebml::read_element_header(prefix, at) else {
            break;
        };
        if h.id == id::CLUSTER {
            break; // frames begin; the front metadata is behind us
        }
        let Ok(Some(end)) = h.data_end() else { break };
        if end > prefix.len() || end <= at {
            break;
        }
        match h.id {
            i if i == id::INFO => index.info = Some(at as u64),
            i if i == id::TRACKS => index.tracks = Some(at as u64),
            i if i == id::TAGS => index.tags = Some(at as u64),
            i if i == id::ATTACHMENTS => index.attachments = Some(at as u64),
            i if i == id::CHAPTERS => index.chapters = Some(at as u64),
            i if i == id::SEEK_HEAD && seek_heads < 2 => {
                seek_heads += 1;
                index_seek_head(&prefix[h.data_start..end], segment_data_start, &mut index);
            }
            _ => {}
        }
        at = end;
    }
    Some(index)
}

fn index_seek_head(data: &[u8], segment_data_start: u64, index: &mut SegmentIndex) {
    let mut at = 0usize;
    while at < data.len() {
        let Ok(h) = ebml::read_element_header(data, at) else {
            return;
        };
        let Ok(Some(end)) = h.data_end() else { return };
        if end > data.len() || end <= at {
            return;
        }
        if h.id == id::SEEK
            && let Some((target, pos)) = seek_entry(&data[h.data_start..end])
        {
            let abs = segment_data_start.saturating_add(pos);
            match target {
                t if t == id::INFO => index.info.get_or_insert(abs),
                t if t == id::TRACKS => index.tracks.get_or_insert(abs),
                t if t == id::TAGS => index.tags.get_or_insert(abs),
                t if t == id::ATTACHMENTS => index.attachments.get_or_insert(abs),
                t if t == id::CHAPTERS => index.chapters.get_or_insert(abs),
                _ => &mut 0,
            };
        }
        at = end;
    }
}

fn seek_entry(data: &[u8]) -> Option<(&[u8], u64)> {
    let mut at = 0usize;
    let mut target = None;
    let mut pos = None;
    while at < data.len() {
        let h = ebml::read_element_header(data, at).ok()?;
        let end = h.data_end().ok()??;
        if end > data.len() || end <= at {
            return None;
        }
        let body = &data[h.data_start..end];
        if h.id == id::SEEK_ID {
            target = Some(body);
        } else if h.id == id::SEEK_POSITION {
            pos = Some(read_uint(body));
        }
        at = end;
    }
    Some((target?, pos?))
}

/// What a window holds of the element whose header begins at `window[0]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Extent {
    Complete { len: usize },
    Short { total: u64 },
    Invalid,
}

/// An unknown-size master (RFC 8794 section 6.2) is [`Extent::Invalid`], it has no length to read.
pub fn element_extent(window: &[u8]) -> Extent {
    let Ok(h) = ebml::read_element_header(window, 0) else {
        return Extent::Invalid;
    };
    let Ok(Some(end)) = h.data_end() else {
        return Extent::Invalid;
    };
    if end <= window.len() {
        Extent::Complete { len: end }
    } else {
        Extent::Short { total: end as u64 }
    }
}

/// The `Info` duration in ns (RFC 9559 section 5.1.2), `info` including its header.
pub fn info_duration_ns(info: &[u8]) -> Option<u64> {
    let h = ebml::read_element_header(info, 0).ok()?;
    if h.id != id::INFO {
        return None;
    }
    let end = h.data_end().ok()??;
    let body = info.get(h.data_start..end.min(info.len()))?;
    let (scale, ticks) = parse_info(body).ok()?;
    let scale = scale.unwrap_or(crate::writer::DEFAULT_TIMESTAMP_SCALE);
    let ticks = ticks?;
    let ns = ticks * scale as f64;
    (ns.is_finite() && ns >= 0.0 && ns < u64::MAX as f64).then(|| ns.round() as u64)
}

/// `(sample_rate, channels)` of the first audio track. Zero means not stated.
pub fn audio_format(tracks: &[u8]) -> Option<(u32, u32)> {
    let body = master_body(tracks, id::TRACKS)?;
    let mut found = None;
    for_each_child(body, |h, entry| {
        if h.id != id::TRACK_ENTRY || found.is_some() {
            return;
        }
        let mut is_audio = false;
        let mut rate = 0u32;
        let mut channels = 0u32;
        for_each_child(entry, |c, cbody| match c.id {
            i if i == id::TRACK_TYPE => is_audio = read_uint(cbody) == 2,
            i if i == id::AUDIO => {
                for_each_child(cbody, |a, abody| match a.id {
                    j if j == id::SAMPLING_FREQUENCY => {
                        let hz = match abody.len() {
                            4 => f32::from_bits(u32::from_be_bytes(
                                abody.try_into().unwrap_or_default(),
                            )) as f64,
                            8 => f64::from_bits(u64::from_be_bytes(
                                abody.try_into().unwrap_or_default(),
                            )),
                            _ => 0.0,
                        };
                        if hz.is_finite() && hz > 0.0 && hz < u32::MAX as f64 {
                            rate = hz as u32;
                        }
                    }
                    j if j == id::CHANNELS => {
                        channels = read_uint(abody).min(u32::MAX as u64) as u32
                    }
                    _ => {}
                });
            }
            _ => {}
        });
        if is_audio {
            found = Some((rate, channels));
        }
    });
    found
}

/// Parse a `Tags` element (RFC 9559 section 5.1.8) into `sink`, see [`emits_for_file`].
/// At album level `TITLE` and `ARTIST` become `ALBUM` and `ALBUMARTIST`, only when some `Tag`
/// declares a level below 50, since a lone untargeted block defaults to 50 but means the recording.
/// Nested `SimpleTag`s are flattened, parent first.
pub fn parse_tags(tags: &[u8], sink: &mut impl TagSink) {
    let Some(body) = master_body(tags, id::TAGS) else {
        return;
    };
    let album_scope = has_track_level(body);
    for_each_child(body, |h, tag| {
        if h.id == id::TAG {
            parse_tag(tag, album_scope, sink);
        }
    });
}

/// Sets each track's `bitrate` from the `BPS` statistics tag of a `Tags` element that names it
/// by its TrackUID, as mkvmerge writes them. A tag of no track is left out.
pub fn bitrates(tags: &[u8], tracks: &mut [Track]) {
    if let Some(body) = master_body(tags, id::TAGS) {
        bitrates_in(body, tracks);
    }
}

/// [`bitrates`] over the `Tags` element's data.
pub(crate) fn bitrates_in(body: &[u8], tracks: &mut [Track]) {
    for_each_child(body, |h, tag| {
        if h.id != id::TAG {
            return;
        }
        let mut bps = None;
        for_each_child(tag, |c, simple| {
            if c.id == id::SIMPLE_TAG {
                bps = bps.or_else(|| stat_bps(simple));
            }
        });
        let Some(bps) = bps else { return };
        for_each_child(tag, |c, targets| {
            if c.id != id::TARGETS {
                return;
            }
            for_each_child(targets, |t, uid| {
                let uid = read_uint(uid);
                if t.id != id::TAG_TRACK_UID || uid == 0 {
                    return;
                }
                for track in tracks.iter_mut().filter(|track| track.uid == uid) {
                    track.bitrate = bps;
                }
            });
        });
    });
}

/// The bits a second of a `BPS` SimpleTag, `BPS-eng` as some tools name it.
fn stat_bps(simple: &[u8]) -> Option<u64> {
    let (mut name, mut value) = (&[][..], &[][..]);
    for_each_child(simple, |h, body| match h.id {
        i if i == id::TAG_NAME => name = ebml::read_string(body),
        i if i == id::TAG_STRING => value = ebml::read_string(body),
        _ => {}
    });
    let bps = name == b"BPS" || name.starts_with(b"BPS-");
    let value = core::str::from_utf8(value).ok().filter(|_| bps)?;
    value.trim().parse::<u64>().ok().filter(|&v| v > 0)
}

fn has_track_level(body: &[u8]) -> bool {
    let mut found = false;
    for_each_child(body, |h, tag| {
        if h.id == id::TAG && target_of(tag).level < TARGET_DEFAULT {
            found = true;
        }
    });
    found
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Target {
    level: u64,
    pinned: bool,
}

impl Default for Target {
    fn default() -> Self {
        Self { level: TARGET_DEFAULT, pinned: false }
    }
}

/// A level below [`TARGET_TRACK`] or a chapter, edition or attachment UID fails it.
/// `TagTrackUID` does not, a scanner wants a track's tags.
pub fn emits_for_file(level: u64, pinned: bool) -> bool {
    level >= TARGET_TRACK && !pinned
}

fn target_of(tag: &[u8]) -> Target {
    let mut target = Target::default();
    for_each_child(tag, |h, targets| {
        if h.id != id::TARGETS {
            return;
        }
        for_each_child(targets, |c, body| match c.id {
            i if i == id::TARGET_TYPE_VALUE => target.level = read_uint(body),
            i if i == id::TAG_CHAPTER_UID
                || i == id::TAG_EDITION_UID
                || i == id::TAG_ATTACHMENT_UID =>
            {
                target.pinned |= read_uint(body) != 0;
            }
            _ => {}
        });
    });
    target
}

fn parse_tag(tag: &[u8], album_scope: bool, sink: &mut impl TagSink) {
    let target = target_of(tag);
    if !emits_for_file(target.level, target.pinned) {
        return;
    }
    let album = album_scope && target.level >= TARGET_DEFAULT;
    for_each_child(tag, |h, simple| {
        if h.id == id::SIMPLE_TAG {
            emit_simple_tag(simple, album, 0, sink);
        }
    });
}

fn emit_simple_tag(simple: &[u8], album: bool, depth: usize, sink: &mut impl TagSink) {
    let mut name: Option<&str> = None;
    let mut value: Option<&str> = None;
    for_each_child(simple, |h, body| match h.id {
        i if i == id::TAG_NAME => name = core::str::from_utf8(ebml::read_string(body)).ok(),
        i if i == id::TAG_STRING => value = core::str::from_utf8(ebml::read_string(body)).ok(),
        _ => {}
    });

    if let (Some(name), Some(value)) = (name, value)
        && !name.is_empty()
        && !value.is_empty()
    {
        let mut buf = [0u8; MAX_KEY];
        sink.text(canonical_key(name, album, &mut buf), value);
    }

    if depth + 1 < MAX_NEST {
        for_each_child(simple, |h, nested| {
            if h.id == id::SIMPLE_TAG {
                emit_simple_tag(nested, album, depth + 1, sink);
            }
        });
    }
}

fn canonical_key<'a>(name: &'a str, album: bool, buf: &'a mut [u8; MAX_KEY]) -> &'a str {
    match name {
        "TITLE" if album => return "ALBUM",
        "ARTIST" if album => return "ALBUMARTIST",
        "PART_NUMBER" => return if album { "DISCNUMBER" } else { "TRACKNUMBER" },
        "TOTAL_PARTS" => return if album { "DISCTOTAL" } else { "TRACKTOTAL" },
        "DATE_RELEASED" | "DATE_RECORDED" | "DATE_WRITTEN" | "DATE_DIGITIZED" => return "DATE",
        _ => {}
    }
    if name.len() > MAX_KEY || !name.bytes().any(|b| b.is_ascii_lowercase()) {
        return name; // already canonical, the common path, borrowed
    }
    for (dst, src) in buf.iter_mut().zip(name.bytes()) {
        *dst = src.to_ascii_uppercase();
    }
    core::str::from_utf8(&buf[..name.len()]).unwrap_or(name)
}

/// Parse `Attachments` (RFC 9559 section 5.1.7), emitting `image/*` files as pictures, `cover.*`
/// ones first.
pub fn parse_attachments(attachments: &[u8], sink: &mut impl TagSink) {
    let Some(body) = master_body(attachments, id::ATTACHMENTS) else {
        return;
    };
    for cover_pass in [true, false] {
        for_each_child(body, |h, file| {
            if h.id == id::ATTACHED_FILE {
                emit_attachment(file, cover_pass, sink);
            }
        });
    }
}

fn emit_attachment(file: &[u8], cover_pass: bool, sink: &mut impl TagSink) {
    let mut name: &str = "";
    let mut mime: Option<&str> = None;
    let mut data: Option<&[u8]> = None;
    for_each_child(file, |h, body| match h.id {
        i if i == id::FILE_NAME => {
            name = core::str::from_utf8(ebml::read_string(body)).unwrap_or("")
        }
        i if i == id::FILE_MEDIA_TYPE => mime = core::str::from_utf8(ebml::read_string(body)).ok(),
        i if i == id::FILE_DATA => data = Some(body),
        _ => {}
    });

    let (Some(mime), Some(data)) = (mime, data) else {
        return;
    };
    // Compared case-insensitively without a folded copy, which would allocate.
    if strip_prefix_ascii_ci(mime, "image/").is_none() || data.is_empty() {
        return;
    }
    if is_cover_name(name) == cover_pass {
        sink.picture(mime, data);
    }
}

/// A cover's file name by the Matroska attachment convention, `cover`, `small_cover`,
/// `cover_land` and `cover_port`, any extension and case.
pub fn is_cover_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let stem = strip_suffix_ascii_ci(stem, "_land")
        .or_else(|| strip_suffix_ascii_ci(stem, "_port"))
        .unwrap_or(stem);
    let stem = strip_prefix_ascii_ci(stem, "small_").unwrap_or(stem);
    stem.eq_ignore_ascii_case("cover")
}

fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

fn strip_suffix_ascii_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let at = s.len().checked_sub(suffix.len())?;
    let tail = s.get(at..)?;
    tail.eq_ignore_ascii_case(suffix).then(|| &s[..at])
}

pub(crate) fn master_body<'a>(data: &'a [u8], want: &[u8]) -> Option<&'a [u8]> {
    let h = ebml::read_element_header(data, 0).ok()?;
    if h.id != want {
        return None;
    }
    let end = match h.data_end() {
        Ok(Some(end)) => end.min(data.len()),
        _ => data.len(),
    };
    data.get(h.data_start..end)
}

/// A child that cannot be read stops the walk, so corruption costs only what follows it.
pub(crate) fn for_each_child<'a>(
    body: &'a [u8],
    mut f: impl FnMut(&ebml::ElementHeader<'a>, &'a [u8]),
) {
    let mut at = 0usize;
    while at < body.len() {
        let Ok(h) = ebml::read_element_header(body, at) else {
            return;
        };
        let Ok(Some(end)) = h.data_end() else { return };
        if end > body.len() || end <= at {
            return;
        }
        f(&h, &body[h.data_start..end]);
        at = end;
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn master(element_id: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = children.concat();
        let mut out = Vec::new();
        ebml::write_id(&mut out, element_id);
        ebml::write_size(&mut out, body.len() as u64);
        out.extend_from_slice(&body);
        out
    }

    fn uint(element_id: &[u8], v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        ebml::write_uint(&mut out, element_id, v);
        out
    }

    fn utf8(element_id: &[u8], v: &str) -> Vec<u8> {
        let mut out = Vec::new();
        ebml::write_string(&mut out, element_id, v);
        out
    }

    fn binary(element_id: &[u8], v: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ebml::write_binary(&mut out, element_id, v);
        out
    }

    fn simple_tag(name: &str, value: &str, nested: &[Vec<u8>]) -> Vec<u8> {
        let mut kids = vec![utf8(id::TAG_NAME, name), utf8(id::TAG_STRING, value)];
        kids.extend_from_slice(nested);
        master(id::SIMPLE_TAG, &kids)
    }

    fn targets(level: Option<u64>, track_uid: Option<u64>, chapter_uid: Option<u64>) -> Vec<u8> {
        let mut kids = Vec::new();
        if let Some(l) = level {
            kids.push(uint(id::TARGET_TYPE_VALUE, l));
        }
        if let Some(t) = track_uid {
            kids.push(uint(id::TAG_TRACK_UID, t));
        }
        if let Some(c) = chapter_uid {
            kids.push(uint(id::TAG_CHAPTER_UID, c));
        }
        master(id::TARGETS, &kids)
    }

    fn tag(targets: Vec<u8>, simples: &[Vec<u8>]) -> Vec<u8> {
        let mut kids = vec![targets];
        kids.extend_from_slice(simples);
        master(id::TAG, &kids)
    }

    fn tags_element(tags: &[Vec<u8>]) -> Vec<u8> {
        master(id::TAGS, tags)
    }

    fn attached_file(name: &str, mime: &str, data: &[u8]) -> Vec<u8> {
        master(
            id::ATTACHED_FILE,
            &[
                utf8(id::FILE_NAME, name),
                utf8(id::FILE_MEDIA_TYPE, mime),
                binary(id::FILE_DATA, data),
                uint(id::FILE_UID, 0x1234),
            ],
        )
    }

    fn info(scale: u64, duration_ticks: f64) -> Vec<u8> {
        let mut dur = Vec::new();
        ebml::write_f64(&mut dur, id::DURATION, duration_ticks);
        master(id::INFO, &[uint(id::TIMESTAMP_SCALE, scale), dur])
    }

    fn seek_head(entries: &[(&[u8], u64)]) -> Vec<u8> {
        let seeks: Vec<Vec<u8>> = entries
            .iter()
            .map(|(target, pos)| {
                master(id::SEEK, &[binary(id::SEEK_ID, target), uint(id::SEEK_POSITION, *pos)])
            })
            .collect();
        master(id::SEEK_HEAD, &seeks)
    }

    fn ebml_header() -> Vec<u8> {
        master(id::EBML, &[utf8(id::DOC_TYPE, "matroska"), uint(id::DOC_TYPE_VERSION, 4)])
    }

    fn segment_file(level1: &[Vec<u8>]) -> (Vec<u8>, u64) {
        let header = ebml_header();
        let body: Vec<u8> = level1.concat();
        let mut seg = Vec::new();
        ebml::write_id(&mut seg, id::SEGMENT);
        ebml::write_size(&mut seg, body.len() as u64);
        let data_start = (header.len() + seg.len()) as u64;
        let mut out = header;
        out.extend_from_slice(&seg);
        out.extend_from_slice(&body);
        (out, data_start)
    }

    fn tracks(rate: f64, channels: u64) -> Vec<u8> {
        let mut freq = Vec::new();
        ebml::write_f64(&mut freq, id::SAMPLING_FREQUENCY, rate);
        let audio = master(id::AUDIO, &[freq, uint(id::CHANNELS, channels)]);
        let entry =
            master(id::TRACK_ENTRY, &[uint(id::TRACK_NUMBER, 1), uint(id::TRACK_TYPE, 2), audio]);
        master(id::TRACKS, &[entry])
    }

    fn cluster(n: usize) -> Vec<u8> {
        master(id::CLUSTER, &[binary(id::SIMPLE_BLOCK, &vec![0u8; n])])
    }

    #[derive(Default)]
    struct Collect {
        text: Vec<(String, String)>,
        pictures: Vec<(String, usize)>,
    }

    impl Collect {
        fn get(&self, key: &str) -> Option<&str> {
            self.text.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
        }
        fn keys(&self) -> Vec<&str> {
            self.text.iter().map(|(k, _)| k.as_str()).collect()
        }
    }

    impl TagSink for Collect {
        fn text(&mut self, key: &str, value: &str) {
            self.text.push((key.to_string(), value.to_string()));
        }
        fn picture(&mut self, mime: &str, data: &[u8]) {
            self.pictures.push((mime.to_string(), data.len()));
        }
    }

    fn parse(tags: &[u8]) -> Collect {
        let mut c = Collect::default();
        parse_tags(tags, &mut c);
        c
    }

    #[test]
    fn a_flat_tag_block_maps_onto_canonical_keys() {
        let t = tags_element(&[tag(
            targets(None, None, None),
            &[
                simple_tag("TITLE", "Enough", &[]),
                simple_tag("ARTIST", "Fred again..", &[]),
                simple_tag("ALBUM", "Actual Life 3", &[]),
                simple_tag("DATE_RELEASED", "2022-10-28", &[]),
                simple_tag("PART_NUMBER", "7", &[]),
                simple_tag("TOTAL_PARTS", "12", &[]),
                simple_tag("GENRE", "Électronique", &[]),
            ],
        )]);
        let c = parse(&t);
        assert_eq!(c.get("TITLE"), Some("Enough"));
        assert_eq!(c.get("ARTIST"), Some("Fred again.."));
        assert_eq!(c.get("ALBUM"), Some("Actual Life 3"));
        assert_eq!(c.get("DATE"), Some("2022-10-28"), "DATE_RELEASED is the file's DATE");
        assert_eq!(c.get("TRACKNUMBER"), Some("7"), "PART_NUMBER at track scope");
        assert_eq!(c.get("TRACKTOTAL"), Some("12"));
        assert_eq!(c.get("GENRE"), Some("Électronique"), "non-ASCII UTF-8 passes through");
    }

    #[test]
    fn null_terminated_tag_strings_read_as_their_value() {
        let padded = master(
            id::SIMPLE_TAG,
            &[binary(id::TAG_NAME, b"TITLE\0"), binary(id::TAG_STRING, b"Enough\0\0")],
        );
        let t = tags_element(&[tag(targets(None, None, None), &[padded])]);
        let c = parse(&t);
        assert_eq!(c.get("TITLE"), Some("Enough"));
    }

    #[test]
    fn unknown_names_pass_through_uppercased() {
        let t = tags_element(&[tag(
            targets(None, None, None),
            &[
                simple_tag("ENCODER", "Lavf60.3.100", &[]),
                simple_tag("mood", "restless", &[]),
                simple_tag("Replaygain_Track_Gain", "-7.35 dB", &[]),
            ],
        )]);
        let c = parse(&t);
        assert_eq!(c.get("ENCODER"), Some("Lavf60.3.100"));
        assert_eq!(c.get("MOOD"), Some("restless"));
        assert_eq!(c.get("REPLAYGAIN_TRACK_GAIN"), Some("-7.35 dB"));
    }

    #[test]
    fn an_overlong_name_is_not_truncated() {
        let long = "x".repeat(MAX_KEY + 10);
        let t = tags_element(&[tag(targets(None, None, None), &[simple_tag(&long, "v", &[])])]);
        let c = parse(&t);
        assert_eq!(c.keys(), vec![long.as_str()], "passed through unfolded, not cut short");
    }

    #[test]
    fn a_lone_default_level_block_is_the_tracks_own() {
        let t = tags_element(&[tag(
            targets(None, None, None),
            &[simple_tag("TITLE", "Song", &[]), simple_tag("ARTIST", "Band", &[])],
        )]);
        let c = parse(&t);
        assert_eq!(c.get("TITLE"), Some("Song"));
        assert_eq!(c.get("ARTIST"), Some("Band"));
        assert_eq!(c.get("ALBUM"), None);
        assert_eq!(c.get("ALBUMARTIST"), None);
    }

    #[test]
    fn album_level_remaps_when_a_track_level_exists() {
        let t = tags_element(&[
            tag(
                targets(Some(50), None, None),
                &[
                    simple_tag("TITLE", "Actual Life 3", &[]),
                    simple_tag("ARTIST", "Fred again..", &[]),
                    simple_tag("PART_NUMBER", "2", &[]),
                    simple_tag("TOTAL_PARTS", "3", &[]),
                ],
            ),
            tag(
                targets(Some(30), None, None),
                &[
                    simple_tag("TITLE", "Enough", &[]),
                    simple_tag("ARTIST", "Fred again..", &[]),
                    simple_tag("PART_NUMBER", "7", &[]),
                ],
            ),
        ]);
        let c = parse(&t);
        assert_eq!(c.get("ALBUM"), Some("Actual Life 3"));
        assert_eq!(c.get("ALBUMARTIST"), Some("Fred again.."));
        assert_eq!(c.get("DISCNUMBER"), Some("2"), "PART_NUMBER at album level is the disc");
        assert_eq!(c.get("DISCTOTAL"), Some("3"));
        assert_eq!(c.get("TITLE"), Some("Enough"), "the track block keeps TITLE");
        assert_eq!(c.get("ARTIST"), Some("Fred again.."));
        assert_eq!(c.get("TRACKNUMBER"), Some("7"));
    }

    #[test]
    fn sub_track_and_chapter_scoped_tags_are_skipped() {
        let t = tags_element(&[
            tag(targets(Some(30), None, None), &[simple_tag("TITLE", "The recording", &[])]),
            tag(targets(Some(20), None, None), &[simple_tag("TITLE", "Movement II", &[])]),
            tag(targets(Some(10), None, None), &[simple_tag("TITLE", "A shot", &[])]),
            tag(targets(Some(30), None, Some(99)), &[simple_tag("TITLE", "Chapter 4", &[])]),
        ]);
        let c = parse(&t);
        assert_eq!(c.get("TITLE"), Some("The recording"));
        assert_eq!(c.keys().len(), 1, "only the unscoped track-level tag survives");
    }

    #[test]
    fn a_track_scoped_tag_is_kept() {
        let t = tags_element(&[
            tag(targets(None, None, None), &[simple_tag("ENCODER", "Lavf60.3.100", &[])]),
            tag(
                targets(None, Some(0x9A7B_C3D2), None),
                &[simple_tag("DURATION", "00:59:37.640000000", &[])],
            ),
        ]);
        let c = parse(&t);
        assert_eq!(c.get("ENCODER"), Some("Lavf60.3.100"));
        assert_eq!(c.get("DURATION"), Some("00:59:37.640000000"));
    }

    #[test]
    fn nested_simple_tags_are_flattened_parent_first() {
        let t = tags_element(&[tag(
            targets(None, None, None),
            &[simple_tag(
                "ARTIST",
                "The Beatles",
                &[
                    simple_tag("SORT_WITH", "Beatles, The", &[]),
                    simple_tag("INSTRUMENTS", "guitar", &[]),
                ],
            )],
        )]);
        let c = parse(&t);
        assert_eq!(
            c.keys(),
            vec!["ARTIST", "SORT_WITH", "INSTRUMENTS"],
            "parent precedes children"
        );
        assert_eq!(c.get("ARTIST"), Some("The Beatles"));
        assert_eq!(c.get("SORT_WITH"), Some("Beatles, The"));
    }

    #[test]
    fn nesting_is_depth_capped() {
        let mut inner = simple_tag("LEVEL", "deepest", &[]);
        for i in 0..MAX_NEST + 8 {
            inner = simple_tag("LEVEL", &format!("{i}"), &[inner]);
        }
        let t = tags_element(&[tag(targets(None, None, None), &[inner])]);
        let c = parse(&t);
        assert_eq!(c.keys().len(), MAX_NEST, "exactly the cap, and no recursion past it");
    }

    #[test]
    fn incomplete_or_non_utf8_pairs_emit_nothing() {
        let name_only = master(id::SIMPLE_TAG, &[utf8(id::TAG_NAME, "TITLE")]);
        let value_only = master(id::SIMPLE_TAG, &[utf8(id::TAG_STRING, "orphan")]);
        let empty_value = simple_tag("TITLE", "", &[]);
        let bad_utf8 = master(
            id::SIMPLE_TAG,
            &[utf8(id::TAG_NAME, "TITLE"), binary(id::TAG_STRING, &[0xFF, 0xFE])],
        );
        let t = tags_element(&[tag(
            targets(None, None, None),
            &[name_only, value_only, empty_value, bad_utf8, simple_tag("ARTIST", "real", &[])],
        )]);
        let c = parse(&t);
        assert_eq!(c.keys(), vec!["ARTIST"], "only the well-formed pair is emitted");
    }

    #[test]
    fn degenerate_tags_elements_emit_nothing() {
        assert!(parse(&tags_element(&[])).text.is_empty());
        assert!(parse(&tags_element(&[tag(targets(None, None, None), &[])])).text.is_empty());
        assert!(parse(&info(1_000_000, 5.0)).text.is_empty(), "wrong element ID");
        assert!(parse(&[]).text.is_empty());
        assert!(parse(&[0xFF, 0xFF, 0xFF]).text.is_empty());
    }

    fn attach(files: &[Vec<u8>]) -> Collect {
        let mut c = Collect::default();
        parse_attachments(&master(id::ATTACHMENTS, files), &mut c);
        c
    }

    #[test]
    fn attachments_emit_images_cover_first() {
        let c = attach(&[
            attached_file("banner.png", "image/png", &[1u8; 40]),
            attached_file("subtitles.srt", "text/plain", &[2u8; 10]),
            attached_file("cover.jpg", "image/jpeg", &[3u8; 100]),
            attached_file("DejaVuSans.ttf", "application/x-truetype-font", &[4u8; 20]),
        ]);
        assert_eq!(
            c.pictures,
            vec![("image/jpeg".to_string(), 100), ("image/png".to_string(), 40)],
            "cover.jpg first, then the other image; the font and the subtitles are not pictures"
        );
    }

    #[test]
    fn the_cover_naming_convention_is_matched_loosely() {
        for name in
            ["cover.jpg", "Cover.PNG", "small_cover.jpg", "cover_land.jpg", "COVER_PORT.png"]
        {
            assert!(is_cover_name(name), "{name} is a cover");
        }
        for name in ["banner.png", "coverart.jpg", "folder.jpg", "", "recover.png"] {
            assert!(!is_cover_name(name), "{name} is not");
        }
    }

    #[test]
    fn only_image_attachments_with_data_become_pictures() {
        let no_mime = master(id::ATTACHED_FILE, &[utf8(id::FILE_NAME, "cover.jpg")]);
        let no_data = master(
            id::ATTACHED_FILE,
            &[utf8(id::FILE_NAME, "cover.jpg"), utf8(id::FILE_MEDIA_TYPE, "image/jpeg")],
        );
        let empty = attached_file("cover.jpg", "image/jpeg", &[]);
        let c =
            attach(&[no_mime, no_data, empty, attached_file("a.webp", "IMAGE/WebP", &[9u8; 7])]);
        assert_eq!(c.pictures, vec![("IMAGE/WebP".to_string(), 7)]);
    }

    #[test]
    fn audio_format_reads_the_first_audio_track() {
        assert_eq!(audio_format(&tracks(48_000.0, 2)), Some((48_000, 2)));

        let video = master(id::TRACK_ENTRY, &[uint(id::TRACK_NUMBER, 1), uint(id::TRACK_TYPE, 1)]);
        let mut freq = Vec::new();
        ebml::write_f32(&mut freq, id::SAMPLING_FREQUENCY, 44_100.0);
        let audio_entry = master(
            id::TRACK_ENTRY,
            &[
                uint(id::TRACK_NUMBER, 2),
                uint(id::TRACK_TYPE, 2),
                master(id::AUDIO, &[freq, uint(id::CHANNELS, 6)]),
            ],
        );
        assert_eq!(
            audio_format(&master(id::TRACKS, &[video, audio_entry])),
            Some((44_100, 6)),
            "a 4-octet float reads too, and the video track is stepped over"
        );
    }

    #[test]
    fn audio_format_declines_what_it_cannot_read() {
        let video_only = master(id::TRACKS, &[master(id::TRACK_ENTRY, &[uint(id::TRACK_TYPE, 1)])]);
        assert_eq!(audio_format(&video_only), None);
        assert_eq!(audio_format(&master(id::TRACKS, &[])), None);
        assert_eq!(audio_format(&tags_element(&[])), None, "wrong element ID");
        assert_eq!(audio_format(&[]), None);

        for bad in [-48_000.0f64, f64::NAN, f64::INFINITY] {
            assert_eq!(audio_format(&tracks(bad, 2)), Some((0, 2)), "{bad} is not a rate");
        }
        let odd_width = master(
            id::TRACKS,
            &[master(
                id::TRACK_ENTRY,
                &[
                    uint(id::TRACK_TYPE, 2),
                    master(id::AUDIO, &[binary(id::SAMPLING_FREQUENCY, &[1, 2, 3])]),
                ],
            )],
        );
        assert_eq!(audio_format(&odd_width), Some((0, 0)));
    }

    #[test]
    fn index_segment_finds_the_masters_in_the_window() {
        let level1 = [
            seek_head(&[(id::INFO, 0), (id::TAGS, 0)]), // positions deliberately wrong
            info(1_000_000, 3_000.0),
            tracks(48_000.0, 2),
            tags_element(&[tag(targets(None, None, None), &[simple_tag("TITLE", "T", &[])])]),
            cluster(64),
        ];
        let (file, data_start) = segment_file(&level1);
        let ix = index_segment(&file).expect("a Segment");
        assert_eq!(ix.segment_data_start, data_start);

        let info_at = ix.info.expect("Info") as usize;
        let tags_at = ix.tags.expect("Tags") as usize;
        assert_eq!(&file[info_at..info_at + id::INFO.len()], id::INFO, "points at the Info header");
        assert_eq!(&file[tags_at..tags_at + id::TAGS.len()], id::TAGS);
        assert_eq!(ix.attachments, None, "the file has none");
        let tracks_at = ix.tracks.expect("Tracks") as usize;
        assert_eq!(audio_format(&file[tracks_at..]), Some((48_000, 2)));
        assert_eq!(info_duration_ns(&file[info_at..]), Some(3_000_000_000));
        assert_eq!(parse(&file[tags_at..]).get("TITLE"), Some("T"));
    }

    #[test]
    fn index_segment_reaches_trailing_masters_through_the_seek_head() {
        let filler = 200_000;
        let tags =
            tags_element(&[tag(targets(None, None, None), &[simple_tag("TITLE", "Far", &[])])]);
        let attachments =
            master(id::ATTACHMENTS, &[attached_file("cover.jpg", "image/jpeg", &[7u8; 5_000])]);

        // The SeekHead length depends on the positions it carries, so iterate to the fixed point.
        let (mut tags_pos, mut att_pos) = (0u64, 0u64);
        for _ in 0..8 {
            let head = [
                seek_head(&[(id::TAGS, tags_pos), (id::ATTACHMENTS, att_pos)]),
                info(1_000_000, 1.0),
                cluster(filler),
            ]
            .concat();
            tags_pos = head.len() as u64;
            att_pos = tags_pos + tags.len() as u64;
        }

        let level1 = [
            seek_head(&[(id::TAGS, tags_pos), (id::ATTACHMENTS, att_pos)]),
            info(1_000_000, 1.0),
            cluster(filler),
            tags.clone(),
            attachments.clone(),
        ];
        let (file, data_start) = segment_file(&level1);

        let prefix = &file[..64 * 1024];
        let ix = index_segment(prefix).expect("a Segment");
        assert_eq!(ix.segment_data_start, data_start);
        let tags_at = ix.tags.expect("Tags via the SeekHead") as usize;
        let att_at = ix.attachments.expect("Attachments via the SeekHead") as usize;
        assert!(tags_at > prefix.len(), "the tags really are past the prefix");
        assert_eq!(&file[tags_at..tags_at + tags.len()], &tags[..], "the position is exact");
        assert_eq!(&file[att_at..att_at + attachments.len()], &attachments[..]);

        assert_eq!(
            element_extent(&file[tags_at..tags_at + 8]),
            Extent::Short { total: tags.len() as u64 }
        );
        assert_eq!(element_extent(&file[tags_at..]), Extent::Complete { len: tags.len() });
        assert_eq!(parse(&file[tags_at..tags_at + tags.len()]).get("TITLE"), Some("Far"));

        let mut c = Collect::default();
        parse_attachments(&file[att_at..att_at + attachments.len()], &mut c);
        assert_eq!(c.pictures, vec![("image/jpeg".to_string(), 5_000)]);
    }

    #[test]
    fn info_duration_applies_the_timestamp_scale() {
        assert_eq!(info_duration_ns(&info(1_000_000, 3_000.0)), Some(3_000_000_000));
        assert_eq!(info_duration_ns(&info(100_000, 30_000.0)), Some(3_000_000_000));
        assert_eq!(info_duration_ns(&info(1_000_000, 1_234.5)), Some(1_234_500_000));
        assert_eq!(
            info_duration_ns(&master(id::INFO, &[uint(id::TIMESTAMP_SCALE, 1_000_000)])),
            None
        );
        assert_eq!(info_duration_ns(&tags_element(&[])), None);
        assert_eq!(info_duration_ns(&[]), None);
    }

    #[test]
    fn a_corrupt_duration_is_rejected() {
        for bad in [-1.0f64, f64::INFINITY, f64::NAN, f64::MAX] {
            assert_eq!(info_duration_ns(&info(1_000_000, bad)), None, "{bad} is not a duration");
        }
    }

    #[test]
    fn element_extent_measures_or_asks_for_more() {
        let t = tags_element(&[tag(targets(None, None, None), &[simple_tag("A", "b", &[])])]);
        assert_eq!(element_extent(&t), Extent::Complete { len: t.len() });
        let mut padded = t.clone();
        padded.extend_from_slice(&[0u8; 100]);
        assert_eq!(element_extent(&padded), Extent::Complete { len: t.len() });
        assert_eq!(element_extent(&t[..6]), Extent::Short { total: t.len() as u64 });
        assert_eq!(element_extent(&[]), Extent::Invalid);
        assert_eq!(element_extent(&[0x00, 0x01]), Extent::Invalid, "a VINT wider than 8 octets");

        let mut unknown = Vec::new();
        ebml::write_id(&mut unknown, id::SEGMENT);
        ebml::write_unknown_size(&mut unknown);
        assert_eq!(element_extent(&unknown), Extent::Invalid, "no declared end to read to");
    }

    #[test]
    fn index_segment_declines_what_is_not_a_segment() {
        assert_eq!(index_segment(&[]), None);
        assert_eq!(index_segment(b"ID3\x04\x00\x00\x00\x00\x00\x00"), None);
        let (file, _) = segment_file(&[info(1_000_000, 1.0)]);
        assert_eq!(index_segment(&file[..6]), None);
    }

    #[test]
    fn a_self_referential_seek_head_terminates() {
        let level1 = [seek_head(&[(id::SEEK_HEAD, 0), (id::TAGS, u64::MAX)]), info(1_000_000, 1.0)];
        let (file, _) = segment_file(&level1);
        let ix = index_segment(&file).expect("a Segment");
        assert_eq!(ix.tags, Some(u64::MAX));
        assert!(ix.info.is_some());
    }

    #[test]
    fn truncation_never_panics() {
        let level1 = [
            seek_head(&[(id::INFO, 0), (id::TAGS, 40), (id::ATTACHMENTS, 90)]),
            info(1_000_000, 4_242.0),
            tags_element(&[
                tag(
                    targets(Some(50), Some(7), None),
                    &[simple_tag("TITLE", "Album", &[simple_tag("SORT_WITH", "s", &[])])],
                ),
                tag(targets(Some(30), None, None), &[simple_tag("PART_NUMBER", "3", &[])]),
            ]),
            master(id::ATTACHMENTS, &[attached_file("cover.jpg", "image/jpeg", &[0xAB; 300])]),
            cluster(32),
        ];
        let (file, _) = segment_file(&level1);
        for n in 0..file.len() {
            let w = &file[..n];
            let _ = index_segment(w);
            let _ = element_extent(w);
            let _ = info_duration_ns(w);
            let mut c = Collect::default();
            parse_tags(w, &mut c);
            parse_attachments(w, &mut c);
        }
    }

    #[test]
    fn hostile_structures_never_panic() {
        let mut c = Collect::default();
        let mut huge = Vec::new();
        ebml::write_id(&mut huge, id::TAGS);
        ebml::write_size(&mut huge, ebml::MAX_VINT_DATA);
        huge.extend_from_slice(&[0u8; 4]);
        parse_tags(&huge, &mut c);
        parse_attachments(&huge, &mut c);

        let mut inner = Vec::new();
        ebml::write_id(&mut inner, id::TAG);
        ebml::write_size(&mut inner, 10_000);
        let mut outer = Vec::new();
        ebml::write_id(&mut outer, id::TAGS);
        ebml::write_size(&mut outer, inner.len() as u64);
        outer.extend_from_slice(&inner);
        parse_tags(&outer, &mut c);

        for a in 0u8..=255 {
            parse_tags(&[a], &mut c);
            parse_attachments(&[a], &mut c);
            let _ = index_segment(&[a]);
            let _ = element_extent(&[a]);
            let _ = info_duration_ns(&[a]);
            for b in [0u8, 1, 0x7F, 0x80, 0xFF] {
                parse_tags(&[a, b], &mut c);
                let _ = element_extent(&[a, b]);
            }
        }
        assert!(c.text.is_empty() && c.pictures.is_empty(), "nothing valid was in any of that");
    }
}
