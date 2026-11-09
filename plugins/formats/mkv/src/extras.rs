//! Attachments (RFC 9559 section 5.1.6) and Chapters (section 5.1.7) parsed from untrusted bytes
//! and published on an [`MkvExtras`] handle. Ordered chapters and linked segments are read as flags
//! only.

use std::sync::Arc;

use profluens_core::publish::{Publisher, Reader, published};

use crate::ebml::{self, id};
use crate::reader::read_uint;
use crate::tags::{for_each_child, master_body, next_child};

pub const MAX_NEST: u8 = 8;
pub const MAX_ATTACHMENTS: usize = 1024;
pub const MAX_CHAPTERS: usize = 4096;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attachment {
    pub uid: u64,
    pub name: String,
    pub media_type: String,
    pub description: String,
    pub data: Vec<u8>,
}

impl Attachment {
    pub fn is_font(&self) -> bool {
        let m = self.media_type.as_bytes();
        let starts = |p: &[u8]| m.len() >= p.len() && m[..p.len()].eq_ignore_ascii_case(p);
        let typed = starts(b"font/")
            || starts(b"application/x-truetype-font")
            || starts(b"application/vnd.ms-opentype")
            || starts(b"application/x-font");
        let n = self.name.as_bytes();
        let ends = |s: &[u8]| n.len() >= s.len() && n[n.len() - s.len()..].eq_ignore_ascii_case(s);
        typed || ends(b".ttf") || ends(b".otf") || ends(b".ttc")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edition {
    pub uid: u64,
    pub default: bool,
    pub hidden: bool,
    pub ordered: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapterTitle {
    pub text: String,
    pub language: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chapter {
    pub edition: usize,
    pub parent: Option<usize>,
    pub depth: u8,
    pub uid: u64,
    pub string_uid: String,
    pub start_ns: u64,
    pub end_ns: Option<u64>,
    pub hidden: bool,
    pub enabled: bool,
    pub linked: bool,
    pub titles: Vec<ChapterTitle>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chapters {
    pub editions: Vec<Edition>,
    pub chapters: Vec<Chapter>,
}

impl Chapters {
    pub fn default_edition(&self) -> Option<usize> {
        self.editions.iter().position(|e| e.default).or((!self.editions.is_empty()).then_some(0))
    }
}

// Cold path, once per file.
#[allow(clippy::disallowed_methods)]
pub fn parse_attachments(element: &[u8]) -> Vec<Attachment> {
    let mut out = Vec::new();
    let Some(body) = master_body(element, id::ATTACHMENTS) else {
        return out;
    };
    for_each_child(body, |h, file| {
        if h.id != id::ATTACHED_FILE || out.len() == MAX_ATTACHMENTS {
            return;
        }
        let mut a = Attachment::default();
        for_each_child(file, |h, v| match h.id {
            i if i == id::FILE_UID => a.uid = read_uint(v),
            i if i == id::FILE_NAME => a.name = text(v),
            i if i == id::FILE_MEDIA_TYPE => a.media_type = text(v),
            i if i == id::FILE_DESCRIPTION => a.description = text(v),
            i if i == id::FILE_DATA => a.data = v.to_vec(),
            _ => {}
        });
        out.push(a);
    });
    out
}

// Cold path, once per file.
#[allow(clippy::disallowed_methods)]
pub fn parse_chapters(element: &[u8]) -> Chapters {
    let mut out = Chapters::default();
    let Some(body) = master_body(element, id::CHAPTERS) else {
        return out;
    };
    for_each_child(body, |h, entry| {
        if h.id != id::EDITION_ENTRY {
            return;
        }
        let edition = out.editions.len();
        let mut e = Edition::default();
        for_each_child(entry, |h, v| match h.id {
            i if i == id::EDITION_UID => e.uid = read_uint(v),
            i if i == id::EDITION_FLAG_DEFAULT => e.default = read_uint(v) != 0,
            i if i == id::EDITION_FLAG_HIDDEN => e.hidden = read_uint(v) != 0,
            i if i == id::EDITION_FLAG_ORDERED => e.ordered = read_uint(v) != 0,
            _ => {}
        });
        out.editions.push(e);
        atoms(entry, edition, &mut out.chapters);
    });
    out
}

/// The ChapterAtoms of an edition and the ones nested in them, depth first, at most
/// [`MAX_NEST`] deep.
// Cold path, once per file.
#[allow(clippy::disallowed_methods)]
fn atoms(entry: &[u8], edition: usize, out: &mut Vec<Chapter>) {
    // per open level, its body, where its next child starts and the chapter it is under
    let mut open = [(&[][..], 0usize, None); MAX_NEST as usize];
    open[0] = (entry, 0, None);
    let mut n = 1;
    while n > 0 {
        let (body, at, parent) = open[n - 1];
        let Some((h, atom, next)) = next_child(body, at) else {
            n -= 1;
            continue;
        };
        open[n - 1].1 = next;
        if h.id != id::CHAPTER_ATOM || out.len() == MAX_CHAPTERS {
            continue;
        }
        let depth = (n - 1) as u8;
        let mut c = Chapter { edition, parent, depth, enabled: true, ..Chapter::default() };
        for_each_child(atom, |h, v| match h.id {
            i if i == id::CHAPTER_UID => c.uid = read_uint(v),
            i if i == id::CHAPTER_STRING_UID => c.string_uid = text(v),
            i if i == id::CHAPTER_TIME_START => c.start_ns = read_uint(v),
            i if i == id::CHAPTER_TIME_END => c.end_ns = Some(read_uint(v)),
            i if i == id::CHAPTER_FLAG_HIDDEN => c.hidden = read_uint(v) != 0,
            i if i == id::CHAPTER_FLAG_ENABLED => c.enabled = read_uint(v) != 0,
            i if i == ebml::id::CHAPTER_DISPLAY => c.titles.push(title(v)),
            i if i == CHAPTER_SEGMENT_UUID => c.linked = !v.is_empty(),
            _ => {}
        });
        out.push(c);
        if n < MAX_NEST as usize {
            open[n] = (atom, 0, Some(out.len() - 1));
            n += 1;
        }
    }
}

const CHAPTER_SEGMENT_UUID: &[u8] = &[0x6E, 0x67];

/// A title the Tags give a chapter, a `TITLE` SimpleTag of a Tag whose Targets name its UID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapterTagTitle {
    pub uid: u64,
    pub title: ChapterTitle,
}

/// The chapter titles of a Tags element, at most [`MAX_CHAPTERS`]. A Tag naming several
/// chapters titles each, a SimpleTag with no language is `und` as RFC 9559 defaults it.
// Cold path, once per file.
#[allow(clippy::disallowed_methods)]
pub fn chapter_tag_titles(element: &[u8]) -> Vec<ChapterTagTitle> {
    let mut out = Vec::new();
    let Some(body) = master_body(element, id::TAGS) else {
        return out;
    };
    let mut uids = [0u64; 8];
    for_each_child(body, |h, tag| {
        if h.id != id::TAG {
            return;
        }
        let mut n = 0;
        for_each_child(tag, |h, targets| {
            if h.id == id::TARGETS {
                for_each_child(targets, |h, v| {
                    let uid = read_uint(v);
                    if h.id == id::TAG_CHAPTER_UID && uid != 0 && n < uids.len() {
                        uids[n] = uid;
                        n += 1;
                    }
                });
            }
        });
        for_each_child(tag, |h, simple| {
            if h.id != id::SIMPLE_TAG {
                return;
            }
            let Some(title) = tag_title(simple) else { return };
            for &uid in &uids[..n] {
                if out.len() < MAX_CHAPTERS {
                    out.push(ChapterTagTitle { uid, title: title.clone() });
                }
            }
        });
    });
    out
}

/// A SimpleTag's string when it is named `TITLE`, with its language.
// Cold path, once per chapter.
#[allow(clippy::disallowed_methods)]
fn tag_title(simple: &[u8]) -> Option<ChapterTitle> {
    let (mut named, mut bcp47) = (false, false);
    let mut t = ChapterTitle { text: String::new(), language: String::from("und") };
    for_each_child(simple, |h, v| match h.id {
        i if i == id::TAG_NAME => named = ebml::read_string(v).eq_ignore_ascii_case(b"TITLE"),
        i if i == id::TAG_STRING => t.text = text(v),
        i if i == id::TAG_LANGUAGE_BCP47 => (t.language, bcp47) = (text(v), true),
        i if i == id::TAG_LANGUAGE && !bcp47 => t.language = text(v),
        _ => {}
    });
    (named && !t.text.is_empty()).then_some(t)
}

// Cold path, once per chapter.
#[allow(clippy::disallowed_methods)]
fn title(display: &[u8]) -> ChapterTitle {
    let mut t = ChapterTitle { text: String::new(), language: String::from("eng") };
    let (mut bcp47, mut first) = (false, true);
    for_each_child(display, |h, v| match h.id {
        i if i == id::CHAP_STRING => t.text = text(v),
        i if i == id::CHAP_LANGUAGE_BCP47 => (t.language, bcp47) = (text(v), true),
        i if i == id::CHAP_LANGUAGE && !bcp47 && first => (t.language, first) = (text(v), false),
        _ => {}
    });
    t
}

// Cold path, once per field.
#[allow(clippy::disallowed_methods)]
fn text(v: &[u8]) -> String {
    String::from_utf8_lossy(ebml::read_string(v)).into_owned()
}

#[derive(Clone, Debug, Default)]
pub struct Extras {
    pub attachments: Arc<[Attachment]>,
    pub chapters: Arc<Chapters>,
    pub settled: bool,
}

/// A handle on what a `mkvdemux` publishes of its attachments and chapters. Take it before the
/// pipeline.
#[derive(Clone)]
pub struct MkvExtras {
    inner: Reader<Extras>,
}

impl MkvExtras {
    // Cold path, once per element.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new() -> (MkvExtras, Publisher<Extras>) {
        let (publisher, inner) = published(Extras::default());
        (MkvExtras { inner }, publisher)
    }

    pub fn attachments(&self) -> Arc<[Attachment]> {
        self.inner.read(|e| Arc::clone(&e.attachments))
    }

    pub fn chapters(&self) -> Arc<Chapters> {
        self.inner.read(|e| Arc::clone(&e.chapters))
    }

    pub fn settled(&self) -> bool {
        self.inner.read(|e| e.settled)
    }

    // Cold path, once per renderer.
    #[allow(clippy::disallowed_methods)]
    pub fn fonts(&self) -> Vec<Attachment> {
        self.attachments().iter().filter(|a| a.is_font()).cloned().collect()
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::ebml::{write_binary, write_string, write_uint};

    fn master(element_id: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        write_binary(&mut out, element_id, &children.concat());
        out
    }
    fn uint(element_id: &[u8], v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        write_uint(&mut out, element_id, v);
        out
    }
    fn utf8(element_id: &[u8], v: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_string(&mut out, element_id, v);
        out
    }

    fn display(text: &str, lang: &str) -> Vec<u8> {
        master(id::CHAPTER_DISPLAY, &[utf8(id::CHAP_STRING, text), utf8(id::CHAP_LANGUAGE, lang)])
    }

    fn atom(uid: u64, start: u64, end: Option<u64>, rest: &[Vec<u8>]) -> Vec<u8> {
        let mut c = vec![uint(id::CHAPTER_UID, uid), uint(id::CHAPTER_TIME_START, start)];
        c.extend(end.map(|e| uint(id::CHAPTER_TIME_END, e)));
        c.extend(rest.iter().cloned());
        master(id::CHAPTER_ATOM, &c)
    }

    fn simple(name: &str, value: &str, lang: Option<&str>) -> Vec<u8> {
        let mut c = vec![utf8(id::TAG_NAME, name), utf8(id::TAG_STRING, value)];
        c.extend(lang.map(|l| utf8(id::TAG_LANGUAGE, l)));
        master(id::SIMPLE_TAG, &c)
    }

    #[test]
    fn titles_of_tags_naming_chapters_are_found() {
        let targets = |uids: &[u64]| {
            let mut c = vec![uint(id::TARGET_TYPE_VALUE, 30)];
            c.extend(uids.iter().map(|&u| uint(id::TAG_CHAPTER_UID, u)));
            master(id::TARGETS, &c)
        };
        let both = master(
            id::TAG,
            &[
                targets(&[7, 9]),
                simple("TITLE", "Vorspann", Some("ger")),
                simple("ARTIST", "x", None),
            ],
        );
        let bcp = master(
            id::SIMPLE_TAG,
            &[
                utf8(id::TAG_NAME, "title"),
                utf8(id::TAG_LANGUAGE_BCP47, "pt-BR"),
                utf8(id::TAG_STRING, "Abertura"),
                utf8(id::TAG_LANGUAGE, "por"),
            ],
        );
        let one = master(id::TAG, &[targets(&[9]), bcp]);
        let file = master(id::TAG, &[targets(&[]), simple("TITLE", "Album", None)]);
        let got = chapter_tag_titles(&master(id::TAGS, &[both, one, file]));
        let got: Vec<(u64, &str, &str)> =
            got.iter().map(|t| (t.uid, t.title.text.as_str(), t.title.language.as_str())).collect();
        assert_eq!(got, [(7, "Vorspann", "ger"), (9, "Vorspann", "ger"), (9, "Abertura", "pt-BR")]);
        assert!(chapter_tag_titles(b"\x1a\x45").is_empty());
    }

    #[test]
    fn attachments_keep_every_field_and_fonts_are_told_apart() {
        let file = |name: &str, mime: &str, data: &[u8], uid: u64| {
            let mut d = Vec::new();
            write_binary(&mut d, id::FILE_DATA, data);
            master(
                id::ATTACHED_FILE,
                &[
                    utf8(id::FILE_NAME, name),
                    utf8(id::FILE_MEDIA_TYPE, mime),
                    d,
                    uint(id::FILE_UID, uid),
                ],
            )
        };
        let el = master(
            id::ATTACHMENTS,
            &[
                file("Gentium.ttf", "application/x-truetype-font", b"\0\x01\0\0", 7),
                file("cover.jpg", "image/jpeg", b"\xff\xd8", 8),
                file("Sans.OTF", "application/octet-stream", b"OTTO", 9),
                file("Serif", "font/woff2", b"wOF2", 10),
            ],
        );
        let got = parse_attachments(&el);
        assert_eq!(got.len(), 4);
        assert_eq!(
            got[0],
            Attachment {
                uid: 7,
                name: "Gentium.ttf".into(),
                media_type: "application/x-truetype-font".into(),
                description: String::new(),
                data: b"\0\x01\0\0".to_vec(),
            }
        );
        let fonts: Vec<bool> = got.iter().map(Attachment::is_font).collect();
        assert_eq!(fonts, [true, false, true, true]);
        assert_eq!(parse_attachments(&el[..el.len() - 3]).len(), 3);
        assert!(parse_attachments(b"\x1a\x45\xdf\xa3\x80").is_empty(), "not Attachments");
    }

    #[test]
    fn chapters_keep_editions_nesting_titles_and_flags() {
        let nested = atom(3, 2_000_000_000, None, &[display("Part A", "eng")]);
        let fr = master(
            id::CHAPTER_DISPLAY,
            &[
                utf8(id::CHAP_STRING, "Ouverture"),
                utf8(id::CHAP_LANGUAGE, "fre"),
                utf8(id::CHAP_LANGUAGE_BCP47, "fr-CA"),
            ],
        );
        let first = atom(1, 0, Some(1_000_000_000), &[display("Opening", "eng"), fr]);
        let second = atom(
            2,
            1_000_000_000,
            Some(5_000_000_000),
            &[uint(id::CHAPTER_FLAG_HIDDEN, 1), nested],
        );
        let main = master(id::EDITION_ENTRY, &[uint(id::EDITION_UID, 11), first, second]);
        let ordered = master(
            id::EDITION_ENTRY,
            &[
                uint(id::EDITION_UID, 12),
                uint(id::EDITION_FLAG_DEFAULT, 1),
                uint(id::EDITION_FLAG_ORDERED, 1),
                atom(4, 0, Some(9), &[]),
            ],
        );
        let c = parse_chapters(&master(id::CHAPTERS, &[main, ordered]));
        assert_eq!(c.editions.len(), 2);
        assert_eq!(c.editions[1], Edition { uid: 12, default: true, hidden: false, ordered: true });
        assert_eq!(c.default_edition(), Some(1));
        let uids: Vec<(u64, usize, Option<usize>, u8)> =
            c.chapters.iter().map(|ch| (ch.uid, ch.edition, ch.parent, ch.depth)).collect();
        assert_eq!(uids, [(1, 0, None, 0), (2, 0, None, 0), (3, 0, Some(1), 1), (4, 1, None, 0)]);
        assert_eq!(c.chapters[0].end_ns, Some(1_000_000_000));
        assert_eq!(
            c.chapters[0].titles[1],
            ChapterTitle { text: "Ouverture".into(), language: "fr-CA".into() }
        );
        assert!(c.chapters[1].hidden && c.chapters[1].enabled);
        assert_eq!((c.chapters[2].start_ns, c.chapters[2].end_ns), (2_000_000_000, None));
        assert_eq!(c.chapters[2].titles[0].text, "Part A");
    }

    #[test]
    fn a_title_with_no_language_is_english() {
        let c = parse_chapters(&master(
            id::CHAPTERS,
            &[master(
                id::EDITION_ENTRY,
                &[atom(1, 0, None, &[master(id::CHAPTER_DISPLAY, &[utf8(id::CHAP_STRING, "x")])])],
            )],
        ));
        assert_eq!(c.chapters[0].titles[0].language, "eng");
    }

    #[test]
    fn nesting_is_capped_and_garbage_parses_to_nothing() {
        let mut deep = atom(99, 0, None, &[]);
        for uid in (0..40).rev() {
            deep = atom(uid, 0, None, &[deep]);
        }
        let c = parse_chapters(&master(id::CHAPTERS, &[master(id::EDITION_ENTRY, &[deep])]));
        assert_eq!(c.chapters.len(), MAX_NEST as usize);
        assert_eq!(c.chapters.last().unwrap().depth, MAX_NEST - 1);
        let mut seed = 1u64;
        for len in 0..200 {
            let junk: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (seed >> 56) as u8
                })
                .collect();
            let _ = parse_chapters(&junk);
            let _ = parse_attachments(&junk);
        }
    }

    #[test]
    fn the_handle_reads_what_was_published() {
        let (handle, mut publisher) = MkvExtras::new();
        assert!(handle.attachments().is_empty() && !handle.settled());
        let font = Attachment { name: "a.ttf".into(), ..Attachment::default() };
        let img = Attachment {
            name: "b.png".into(),
            media_type: "image/png".into(),
            ..Attachment::default()
        };
        let attachments: Arc<[Attachment]> = vec![font.clone(), img].into();
        publisher.publish(Extras { attachments, chapters: Arc::default(), settled: true });
        let other = handle.clone();
        assert!(other.settled());
        assert_eq!(other.attachments().len(), 2);
        assert_eq!(handle.fonts(), [font]);
    }
}
