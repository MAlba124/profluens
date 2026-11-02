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
