use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

use crate::flacmeta;
use crate::item::cut;

/// Editions kept, Matroska's alone has more than one.
pub const MAX_EDITIONS: usize = 16;
/// Chapters kept over all editions, a Vorbis comment numbers 1000 and a `chpl` 255.
pub const MAX_CHAPTERS: usize = 1024;
/// Nesting kept, a deeper chapter is left out with what it holds.
pub const MAX_CHAPTER_DEPTH: u8 = 8;
/// Titles kept of a chapter.
pub const MAX_CHAPTER_TITLES: usize = 8;
/// A title is cut to this many bytes, on a char boundary.
pub const MAX_CHAPTER_TITLE: usize = 256;
/// A longer language is taken as none, BCP 47 asks for 35 characters at least.
pub const MAX_LANGUAGE: usize = 35;
/// Title and language bytes of all chapters together, a title past it is left out.
pub const MAX_CHAPTER_TEXT: usize = 64 << 10;

/// Where an item's chapters came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChapterSource {
    #[default]
    None,
    /// The Matroska or WebM Chapters element.
    Matroska,
    /// A QuickTime chapter track, the text track a `tref/chap` names.
    ChapterTrack,
    /// The Nero `udta/chpl` box.
    Chpl,
    /// `CHAPTERxxx` Vorbis comments of Ogg or FLAC.
    VorbisComment,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapterTitle {
    /// BCP 47 as far as the container states it, empty when it states none or `und`.
    pub language: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemChapter {
    /// The container's UID, 0 when it states none.
    pub uid: u64,
    pub start_ns: u64,
    /// None when neither the container nor what follows the chapter tells.
    pub end_ns: Option<u64>,
    /// 0 for a chapter of the edition, one more per ChapterAtom it sits in.
    pub depth: u8,
    pub hidden: bool,
    /// A Matroska chapter a player skips when false.
    pub enabled: bool,
    pub titles: Vec<ChapterTitle>,
}

impl Default for ItemChapter {
    // Owned result, once per chapter.
    #[allow(clippy::disallowed_methods)]
    fn default() -> Self {
        ItemChapter {
            uid: 0,
            start_ns: 0,
            end_ns: None,
            depth: 0,
            hidden: false,
            enabled: true,
            titles: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapterEdition {
    pub uid: u64,
    /// Exactly one edition of an item is the default.
    pub default: bool,
    /// The chapters play in their order as segments of the timeline, they are not sorted.
    pub ordered: bool,
    pub hidden: bool,
    /// In pre-order, each chapter followed by the ones it holds, siblings by start unless
    /// ordered, ties in the container's order.
    pub chapters: Vec<ItemChapter>,
}

/// An item's chapters, no editions when it has none, else every edition with at least one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemChapters {
    pub source: ChapterSource,
    pub editions: Vec<ChapterEdition>,
}

impl ItemChapters {
    pub fn is_empty(&self) -> bool {
        self.editions.is_empty()
    }

    /// The default edition's chapters.
    pub fn default_chapters(&self) -> &[ItemChapter] {
        self.editions.iter().find(|e| e.default).map_or(&[], |e| &e.chapters)
    }
}

/// A chapter as a reader found it, for [`normalise`]. Its `depth` is ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawChapter {
    pub edition: usize,
    /// Index of the chapter it sits in, which comes before it.
    pub parent: Option<usize>,
    pub chapter: ItemChapter,
}

/// Raw chapters of `editions` as one form. Chapters past the caps, of a missing edition or
/// under a chapter left out are dropped, titles trimmed and cut, languages mapped to BCP 47 and
/// an end before its start taken as none. In an edition that is not ordered siblings are sorted
/// by start, ties in the given order, and a missing end is the next sibling's start, else the
/// end of the chapter it sits in, else `duration_ns` for the last of the edition.
// Owned result, once per item.
#[allow(clippy::disallowed_methods)]
pub fn normalise(
    source: ChapterSource,
    mut editions: Vec<ChapterEdition>,
    raw: Vec<RawChapter>,
    duration_ns: Option<u64>,
) -> ItemChapters {
    editions.truncate(MAX_EDITIONS);
    // the kept chapters, each with its edition and parent among them
    let mut kept: Vec<(usize, Option<usize>, ItemChapter)> = Vec::new();
    let mut new_of = vec![None; raw.len()];
    let mut text = 0usize;
    for (i, r) in raw.into_iter().enumerate() {
        if kept.len() == MAX_CHAPTERS || r.edition >= editions.len() {
            continue;
        }
        let parent = match r.parent {
            None => None,
            Some(p) => match new_of.get(p).copied().flatten().filter(|_| p < i) {
                Some(np) => Some(np),
                None => continue,
            },
        };
        let depth = parent.map_or(0, |p: usize| kept[p].2.depth + 1);
        if depth >= MAX_CHAPTER_DEPTH || parent.is_some_and(|p| kept[p].0 != r.edition) {
            continue;
        }
        let mut c = r.chapter;
        c.depth = depth;
        c.end_ns = c.end_ns.filter(|&e| e >= c.start_ns);
        c.titles = titles(std::mem::take(&mut c.titles), &mut text);
        new_of[i] = Some(kept.len());
        kept.push((r.edition, parent, c));
    }
    // siblings contiguous and in their order, then linked
    let ordered = |e: usize| editions[e].ordered;
    let mut order: Vec<usize> = (0..kept.len()).collect();
    order.sort_by_key(|&i| {
        let (e, p, c) = &kept[i];
        let start = if ordered(*e) { 0 } else { c.start_ns };
        (*e, p.map_or(0, |p| p + 1), start, i)
    });
    let mut first = vec![None; kept.len()];
    let mut next = vec![None; kept.len()];
    let mut roots = vec![None; editions.len()];
    for w in order.iter().rev() {
        let (e, p, _) = kept[*w];
        let head = match p {
            Some(p) => &mut first[p],
            None => &mut roots[e],
        };
        next[*w] = head.replace(*w);
    }
    for (e, edition) in editions.iter_mut().enumerate() {
        edition.chapters.clear();
        // pre-order by an explicit stack of the next sibling to visit
        let mut stack: Vec<Option<usize>> = vec![roots[e]];
        let mut ends: Vec<Option<u64>> = vec![duration_ns];
        while let Some(top) = stack.last_mut() {
            let Some(i) = *top else {
                stack.pop();
                ends.pop();
                continue;
            };
            *top = next[i];
            let mut c = std::mem::take(&mut kept[i].2);
            if c.end_ns.is_none() && !edition.ordered {
                let after = next[i].map(|n| kept[n].2.start_ns);
                let outer = ends.last().copied().flatten();
                c.end_ns = after.or(outer).filter(|&e| e >= c.start_ns);
            }
            ends.push(c.end_ns);
            stack.push(first[i]);
            edition.chapters.push(c);
        }
    }
    editions.retain(|e| !e.chapters.is_empty());
    let default = editions.iter().position(|e| e.default).unwrap_or(0);
    editions.iter_mut().enumerate().for_each(|(i, e)| e.default = i == default);
    let source = if editions.is_empty() { ChapterSource::None } else { source };
    ItemChapters { source, editions }
}

/// Titles trimmed, cut and deduplicated, at most [`MAX_CHAPTER_TITLES`] and what `used` of the
/// [`MAX_CHAPTER_TEXT`] budget leaves.
// Owned result, once per chapter.
#[allow(clippy::disallowed_methods)]
fn titles(given: Vec<ChapterTitle>, used: &mut usize) -> Vec<ChapterTitle> {
    let mut out: Vec<ChapterTitle> = Vec::new();
    for t in given {
        let text =
            cut(t.text.trim_matches(|c: char| c.is_whitespace() || c == '\0'), MAX_CHAPTER_TITLE);
        let language = profluens_core::language::bcp47(&t.language).as_str().to_string();
        if out.len() == MAX_CHAPTER_TITLES
            || text.is_empty()
            || *used + text.len() + language.len() > MAX_CHAPTER_TEXT
        {
            continue;
        }
        if out.iter().any(|o| o.text == text && o.language == language) {
            continue;
        }
        *used += text.len() + language.len();
        out.push(ChapterTitle { language, text: text.to_string() });
    }
    out
}

/// The duration as an end, none for 0.
fn known(duration_ns: u64) -> Option<u64> {
    (duration_ns > 0).then_some(duration_ns)
}

fn one_edition() -> Vec<ChapterEdition> {
    vec![ChapterEdition { default: true, ..ChapterEdition::default() }]
}

/// The chapters of a Matroska Chapters element, with the titles the Tags element gives them by
/// their UID after the ones the chapters carry.
// Owned result, once per item.
#[allow(clippy::disallowed_methods)]
pub fn mkv_chapters(chapters: &[u8], tags: &[u8], duration_ns: u64) -> ItemChapters {
    if chapters.is_empty() {
        return ItemChapters::default();
    }
    let parsed = pf_mkv::parse_chapters(chapters);
    let tagged = match tags.is_empty() {
        true => Vec::new(),
        false => pf_mkv::chapter_tag_titles(tags),
    };
    let editions = parsed
        .editions
        .iter()
        .map(|e| ChapterEdition {
            uid: e.uid,
            default: e.default,
            ordered: e.ordered,
            hidden: e.hidden,
            chapters: Vec::new(),
        })
        .collect();
    let title = |t: &pf_mkv::ChapterTitle| ChapterTitle {
        language: t.language.clone(),
        text: t.text.clone(),
    };
    let raw = parsed
        .chapters
        .iter()
        .take(MAX_CHAPTERS)
        .map(|c| {
            let own = c.titles.iter().map(title);
            let more =
                tagged.iter().filter(|t| c.uid != 0 && t.uid == c.uid).map(|t| title(&t.title));
            let chapter = ItemChapter {
                uid: c.uid,
                start_ns: c.start_ns,
                end_ns: c.end_ns,
                depth: c.depth,
                hidden: c.hidden,
                enabled: c.enabled,
                titles: own.chain(more).take(MAX_CHAPTER_TITLES).collect(),
            };
            RawChapter { edition: c.edition, parent: c.parent, chapter }
        })
        .collect();
    normalise(ChapterSource::Matroska, editions, raw, known(duration_ns))
}

/// The title of a QuickTime text sample, its 16 bit length then UTF-8, or UTF-16 after a
/// byte order mark.
// Owned result, once per chapter.
#[allow(clippy::disallowed_methods)]
fn sample_title(b: &[u8]) -> String {
    let Some(len) = b.get(..2).map(|l| usize::from(u16::from_be_bytes([l[0], l[1]]))) else {
        return String::new();
    };
    let text = &b[2..b.len().min(2 + len)];
    let utf16 = |t: &[u8], be: bool| {
        let units = t.as_chunks::<2>().0.iter().map(|&u| match be {
            true => u16::from_be_bytes(u),
            false => u16::from_le_bytes(u),
        });
        char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
    };
    match text {
        [0xFE, 0xFF, rest @ ..] => utf16(rest, true),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, false),
        _ => String::from_utf8_lossy(text).into_owned(),
    }
}

/// The chapters of a whole `moov` box and the chapter track its open read. The chapter track
/// wins over a `chpl`, it is the one QuickTime and Apple players read and it states each
/// chapter's end and its titles' language, the Nero box only starts and titles.
// Owned result, once per item.
#[allow(clippy::disallowed_methods)]
pub fn moov_chapters(
    moov: &[u8],
    track: &pf_mp4::chapters::ChapterTrack,
    duration_ns: u64,
) -> ItemChapters {
    let duration = known(duration_ns);
    if !track.samples.is_empty() && !track.bytes.is_empty() {
        let language = String::from_utf8_lossy(&track.language).into_owned();
        let raw = track
            .texts()
            .map(|(s, b)| {
                let text = sample_title(b);
                let titles = vec![ChapterTitle { language: language.clone(), text }];
                let chapter = ItemChapter {
                    start_ns: s.start_ns,
                    end_ns: Some(s.end_ns),
                    titles,
                    ..ItemChapter::default()
                };
                RawChapter { edition: 0, parent: None, chapter }
            })
            .collect();
        return normalise(ChapterSource::ChapterTrack, one_edition(), raw, duration);
    }
    let raw = pf_mp4::chapters::chpl(moov)
        .into_iter()
        .map(|e| {
            let text = String::from_utf8_lossy(e.title).into_owned();
            let chapter = ItemChapter {
                start_ns: e.start_ns,
                titles: vec![ChapterTitle { language: String::new(), text }],
                ..ItemChapter::default()
            };
            RawChapter { edition: 0, parent: None, chapter }
        })
        .collect();
    normalise(ChapterSource::Chpl, one_edition(), raw, duration)
}

/// Chapter numbers have at most this many digits.
const CHAPTER_DIGITS: usize = 4;

/// `CHAPTERxxx` comments by their number, start, name and language.
#[derive(Debug)]
struct VorbisChapters {
    found: Vec<(u32, Option<u64>, String, String)>,
    /// Where each number sits in `found`, a lookup per comment however many there are.
    slot: Vec<u16>,
}

/// The number and what follows it of a `CHAPTERxxx` key, uppercase as the parsers give it.
fn chapter_key(key: &str) -> Option<(u32, &str)> {
    let rest = key.strip_prefix("CHAPTER")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=CHAPTER_DIGITS).contains(&digits) {
        return None;
    }
    Some((rest[..digits].parse().ok()?, &rest[digits..]))
}

/// Whether a comment is one of the chapter ones [`vorbis_chapters`] reads, `CHAPTERxxx` and its
/// `NAME`, `URL` and `LANGUAGE`, any case.
pub fn is_chapter_key(key: &str) -> bool {
    let mut buf = [0u8; 24];
    let Some(up) = buf.get_mut(..key.len()) else { return false };
    up.copy_from_slice(key.as_bytes());
    up.make_ascii_uppercase();
    let up = std::str::from_utf8(up).unwrap_or_default();
    chapter_key(up).is_some_and(|(_, s)| matches!(s, "" | "NAME" | "URL" | "LANGUAGE"))
}

/// `HH:MM:SS.mmm` as nanoseconds, hours of any width and a fraction of up to 9 digits.
fn clock(v: &str) -> Option<u64> {
    let mut parts = v.trim().splitn(3, ':');
    let (h, m, s) = (parts.next()?, parts.next()?, parts.next()?);
    let (s, frac) = s.split_once('.').unwrap_or((s, ""));
    fn num(t: &str) -> Option<&str> {
        (!t.is_empty() && t.bytes().all(|c| c.is_ascii_digit())).then_some(t)
    }
    let (h, m, s): (u64, u64, u64) =
        (num(h)?.parse().ok()?, num(m)?.parse().ok()?, num(s)?.parse().ok()?);
    if m > 59 || s > 59 || frac.len() > 9 || !frac.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let frac = match frac {
        "" => 0,
        f => f.parse::<u64>().ok()? * 10u64.pow(9 - f.len() as u32),
    };
    h.checked_mul(3600)?.checked_add(m * 60 + s)?.checked_mul(1_000_000_000)?.checked_add(frac)
}

impl TagSink for VorbisChapters {
    // Owned result, at most MAX_CHAPTERS entries.
    #[allow(clippy::disallowed_methods)]
    fn text(&mut self, key: &str, value: &str) {
        let Some((n, what)) = chapter_key(key) else { return };
        let Some(slot) = self.slot.get_mut(n as usize) else { return };
        if *slot == u16::MAX {
            if self.found.len() == MAX_CHAPTERS {
                return;
            }
            *slot = self.found.len() as u16;
            self.found.push((n, None, String::new(), String::new()));
        }
        let f = &mut self.found[usize::from(*slot)];
        match what {
            "" => f.1 = clock(value),
            "NAME" => f.2 = cut(value, MAX_CHAPTER_TITLE).to_string(),
            "LANGUAGE" => f.3 = cut(value, MAX_LANGUAGE + 1).to_string(),
            _ => {}
        }
    }

    fn picture(&mut self, _mime: &str, _data: &[u8]) {}
}

/// The chapters of the Vorbis comment bodies of an Ogg stream or a FLAC file's blocks. A
/// chapter ends where the next starts, the last at the item's end, unknown without one.
// Owned result, once per item.
#[allow(clippy::disallowed_methods)]
pub fn vorbis_chapters<'b>(
    ogg: Option<&[u8]>,
    flac: impl IntoIterator<Item = &'b [u8]>,
    duration_ns: u64,
) -> ItemChapters {
    let slot = vec![u16::MAX; 10usize.pow(CHAPTER_DIGITS as u32)];
    let mut sink = VorbisChapters { found: Vec::new(), slot };
    if let Some(body) = ogg {
        pf_ogg::comment::parse_comment_body_lossy(body, &Arena::default(), &mut sink);
    }
    flac.into_iter().for_each(|b| flacmeta::vorbis_comment_lossy_into(b, &mut sink));
    sink.found.sort_by_key(|f| f.0);
    let raw = sink
        .found
        .into_iter()
        .filter_map(|(_, start, text, language)| {
            let titles = vec![ChapterTitle { language, text }];
            let chapter = ItemChapter { start_ns: start?, titles, ..ItemChapter::default() };
            Some(RawChapter { edition: 0, parent: None, chapter })
        })
        .collect();
    normalise(ChapterSource::VorbisComment, one_edition(), raw, known(duration_ns))
}

/// The chapters of a file as its open read them, none when its meta was not wanted. Call it
/// before [`crate::item::taken`], which moves the meta out. ID3v2 `CHAP` frames are not read.
pub fn chapters(o: &pf_open::Opened) -> ItemChapters {
    use pf_open::Meta;
    match &o.meta {
        Meta::None => ItemChapters::default(),
        Meta::Mkv(m) => mkv_chapters(&m.chapters, &m.tags, o.duration_ns),
        Meta::Moov(r) => o.head.get(r.clone()).map_or_else(ItemChapters::default, |m| {
            moov_chapters(m, &o.chapter_track, o.duration_ns)
        }),
        Meta::Ogg(s) => vorbis_chapters(Some(o.bytes(*s)), [], o.duration_ns),
        Meta::Bare { raw, flac: true, .. } => {
            vorbis_chapters(None, raw.comments.iter().map(|r| raw.span(r)), o.duration_ns)
        }
        Meta::Bare { flac: false, .. } => ItemChapters::default(),
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests;
