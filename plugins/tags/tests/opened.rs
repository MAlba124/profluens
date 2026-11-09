#![allow(clippy::disallowed_methods, clippy::unwrap_used)]

use std::path::PathBuf;

use pf_open::{Caps, OpenAt, Opened, Want};
use pf_tags::item::{FRONT_COVER, ItemTags, KIND_UNKNOWN, taken};
use pf_tags::{ScanConfig, Scanner};
use profluens_core::fetch::drive_slice;

fn path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

fn opened(rel: &str, want: Want) -> Opened {
    let b = std::fs::read(path(rel)).unwrap();
    drive_slice(&mut OpenAt::new(b.len() as u64, want, Caps::default()), &b).unwrap()
}

fn tags(rel: &str) -> ItemTags {
    let o = opened(rel, Want::ALL);
    let got = pf_tags::of(&o);
    let mut o = o;
    assert_eq!(taken(&mut o), got, "{rel} moved or copied the same");
    assert_eq!(pf_tags::of(&o), ItemTags::default(), "nothing left once taken");
    got
}

#[test]
fn flac_comments_and_cover() {
    let t = tags("plugins/formats/bare/tests/fixtures/tone.flac");
    assert_eq!(t.get("TITLE"), Some("Tone"));
    assert_eq!(t.get("ARTIST"), Some("Box"));
    assert_eq!(t.get("ALBUM"), Some("Fixtures"));
    assert_eq!(t.get("TRACKNUMBER"), Some("3"));
    assert_eq!(t.tags[0].0, "TITLE", "the priority keys lead");
    let p = &t.pictures[..];
    assert_eq!(p.len(), 1);
    assert_eq!(
        (p[0].kind, p[0].mime.as_str(), p[0].width, p[0].height),
        (FRONT_COVER, "image/png", 32, 24)
    );
}

#[test]
fn mp3_id3_tag_and_its_typed_picture() {
    let t = tags("plugins/formats/bare/tests/fixtures/tone.mp3");
    assert_eq!(t.get("TITLE"), Some("Tone"));
    assert_eq!(t.pictures.len(), 1);
    assert_eq!(
        (t.pictures[0].kind, t.pictures[0].width),
        (FRONT_COVER, 32),
        "typed by its APIC frame"
    );
}

#[test]
fn m4a_ilst_and_its_untyped_cover() {
    let t = tags("lib/open/tests/fixtures/tone.m4a");
    assert_eq!(t.get("TITLE"), Some("Tone"));
    assert_eq!(t.get("ARTIST"), Some("Box"));
    let p = &t.pictures[..];
    assert_eq!(p.len(), 1);
    assert_eq!(
        (p[0].kind, p[0].mime.as_str(), p[0].width, p[0].height),
        (KIND_UNKNOWN, "image/png", 32, 24),
        "covr says no type"
    );
}

/// A Matroska item's tags read with its head are the scanner's, the text tags at the end of the
/// file and the cover beside a font in the head, less the track's DURATION.
#[test]
fn a_matroskas_tags_are_the_scanners() {
    let rel = "lib/open/tests/fixtures/tagged.mkv";
    let read = tags(rel);
    let mut scanned = ItemTags::default();
    let mut sc = Scanner::new(ScanConfig { in_flight: 1, ..ScanConfig::default() });
    sc.scan([path(rel)], |out| {
        if let Ok(r) = out.result {
            scanned = pf_tags::item::scanned(&r.tags);
        }
    });
    let mut want = scanned.tags.clone();
    want.retain(|t| t.0 != "DURATION");
    assert_eq!(read.tags, want);
    assert!(
        read.tags.iter().any(|t| t == &("ARTIST".to_string(), "The Box".to_string())),
        "{:?}",
        read.tags
    );
    assert_eq!(read.pictures.len(), 1, "the cover, not the font");
    assert_eq!(read.pictures, scanned.pictures);
}

#[test]
fn ogg_comments() {
    let t = tags("lib/open/tests/fixtures/tone.opus");
    let o = opened("lib/open/tests/fixtures/tone.opus", Want::ALL);
    assert!(matches!(o.meta, pf_open::Meta::Ogg(_)), "{:?}", o.meta);
    assert_eq!(t.get("ENCODER"), Some("Lavc62.28.102 libopus"));
}

#[test]
fn no_meta_wanted_is_no_tags() {
    for rel in [
        "plugins/formats/bare/tests/fixtures/tone.flac",
        "lib/open/tests/fixtures/tagged.mkv",
        "lib/open/tests/fixtures/tone.m4a",
    ] {
        let o = opened(rel, Want::TRACKS);
        assert_eq!(pf_tags::of(&o), ItemTags::default(), "{rel}");
    }
}

/// Each chapter's start and end in ms and its first title with its language.
fn chapter_rows(c: &pf_tags::item::ItemChapters) -> Vec<(u64, Option<u64>, String, String)> {
    c.default_chapters()
        .iter()
        .map(|c| {
            let t = c.titles.first().cloned().unwrap_or_default();
            (c.start_ns / 1_000_000, c.end_ns.map(|e| e / 1_000_000), t.text, t.language)
        })
        .collect()
}

/// The chapter fixtures `cargo xtask fixtures` writes, skipped when not generated.
#[test]
fn chapters_of_every_container_are_one_form() {
    use pf_tags::item::{ChapterSource, chapters, taken_with_chapters};
    let row = |s: u64, e: u64, t: &str, l: &str| (s, Some(e), t.to_string(), l.to_string());
    let want = |de: &str| {
        [row(0, 1250, "Intro", ""), row(1250, 3250, "Mittelstück", ""), row(3250, 5000, "Ende", de)]
    };
    for (name, source, de) in [
        // ffmpeg writes ChapLanguage und and the language as a LANGUAGE tag, no title
        ("chapters.mkv", ChapterSource::Matroska, ""),
        ("chapters.mp4", ChapterSource::ChapterTrack, ""),
        ("chapters.mov", ChapterSource::ChapterTrack, ""),
        // ffmpeg passes the chapter's language on as a comment
        ("chapters.opus", ChapterSource::VorbisComment, "de"),
        ("chapters.flac", ChapterSource::VorbisComment, ""),
    ] {
        let rel = format!("fixtures/out/{name}");
        if !path(&rel).exists() {
            eprintln!("{name} not generated, skipped");
            continue;
        }
        let mut o = opened(&rel, Want::ALL);
        let c = chapters(&o);
        assert_eq!((c.source, c.editions.len()), (source, 1), "{name}");
        let mut rows = chapter_rows(&c);
        // the last ends at the item's end, which an Opus stream's pre-skip moves by a few ms
        if let Some(r) = rows.last_mut() {
            assert!(r.1.is_some_and(|e| e.abs_diff(5000) < 30), "{name} {rows:?}");
            r.1 = Some(5000);
        }
        assert_eq!(rows, want(de), "{name}");
        let tags = pf_tags::of(&o);
        assert!(tags.tags.iter().all(|t| !t.0.starts_with("CHAPTER")), "{name} {:?}", tags.tags);
        assert_eq!(taken_with_chapters(&mut o), (tags, c), "{name}");
        assert!(chapters(&opened(&rel, Want::TRACKS)).is_empty(), "{name} no meta wanted");
    }
}
