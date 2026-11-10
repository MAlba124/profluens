#![allow(clippy::disallowed_methods, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;

use pf_open::{Caps, OpenAt, Opened, Want};
use pf_tags::item::{FRONT_COVER, ItemTags, KIND_UNKNOWN, taken};
use pf_tags::{ScanConfig, Scanner};
use profluens_core::fetch::{Need, drive_slice, drive_slice_log};

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
    assert_eq!(read.get("TITLE"), Some("Tagged tones"), "the Info Title, no tag states one");
}

/// What ffmpeg writes from `args` as `name`, None when it cannot, the test then skips.
fn ffmpeg(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("pf_tags_opened_{}_{name}", std::process::id()));
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    let got = std::fs::read(&path).ok().filter(|_| ok);
    let _ = std::fs::remove_file(&path);
    if got.is_none() {
        eprintln!("skipped: ffmpeg could not write {name}");
    }
    got
}

/// The tags an open of `b` reads and its reads.
fn of_bytes(b: &[u8], caps: Caps) -> (ItemTags, Vec<Need>) {
    let mut log = Vec::new();
    let o = drive_slice_log(&mut OpenAt::new(b.len() as u64, Want::ALL, caps), b, &mut log);
    (pf_tags::of(&o.unwrap()), log)
}

/// What the scanner finds in `b` written as `name`.
fn scanned_bytes(name: &str, b: &[u8]) -> ItemTags {
    let p = pf_tags::fixture::scratch_dir("opened").join(name);
    std::fs::write(&p, b).unwrap();
    let mut out = ItemTags::default();
    let mut sc = Scanner::new(ScanConfig { in_flight: 1, ..ScanConfig::default() });
    sc.scan([p.clone()], |r| {
        if let Ok(r) = r.result {
            out = pf_tags::item::scanned(&r.tags);
        }
    });
    let _ = std::fs::remove_file(&p);
    out
}

/// `b` with its `LIST` chunk moved behind the rest, the chunk's length.
fn list_last(b: &[u8]) -> (Vec<u8>, usize) {
    let at = b.windows(4).position(|w| w == b"LIST").unwrap();
    let size = u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()) as usize;
    let n = 8 + size + (size & 1);
    let mut moved = b.to_vec();
    let list: Vec<u8> = moved.drain(at..at + n).collect();
    moved.extend(list);
    (moved, n)
}

/// A WAV file's RIFF INFO as ffmpeg writes it ahead of the data, and moved behind the data past
/// the window, reads as the scanner reads it. Behind the data it takes one read of the chunks.
#[test]
fn wav_info_before_or_behind_the_data() {
    let meta = [
        "title=Tone T",
        "album=Alb",
        "date=2024",
        "genre=Ambient",
        "artist=Art",
        "track=7",
        "comment=odd",
    ];
    let mut args = vec!["-f", "lavfi", "-i", "sine=duration=3:sample_rate=48000", "-ac", "2"];
    meta.iter().for_each(|m| args.extend(["-metadata", m]));
    let Some(front) = ffmpeg("info.wav", &args) else { return };
    assert!(front.len() > Caps::default().window, "the data past the window");
    let (t, log) = of_bytes(&front, Caps::default());
    let want = [
        ("TITLE", "Tone T"),
        ("ARTIST", "Art"),
        ("ALBUM", "Alb"),
        ("TRACKNUMBER", "7"),
        ("DATE", "2024"),
        ("GENRE", "Ambient"),
    ];
    let got: Vec<_> = t.tags.iter().take(6).map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(got, want);
    assert_eq!(t.get("COMMENT"), Some("odd"));
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(t, scanned_bytes("front.wav", &front));
    let (back, n) = list_last(&front);
    let (moved, log) = of_bytes(&back, Caps::default());
    assert_eq!(moved, t);
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1], Need { at: (back.len() - n) as u64, len: n }, "the chunks behind the data");
    let (small, _) = of_bytes(&back, Caps { window: 4096, ..Caps::default() });
    assert_eq!(small, t);
    assert_eq!(scanned_bytes("back.wav", &back), t);
    let (none, _) = of_bytes(&back, Caps { max_tags: 8, ..Caps::default() });
    assert_eq!(none, ItemTags::default(), "the chunks past the cap are not read");
}

/// An odd sized data chunk and its pad byte ahead of the INFO, a whole small file and one past
/// the window.
#[test]
fn wav_info_behind_an_odd_data_chunk() {
    use pf_tags::fixture::{fmt_chunk, info_chunk, riff, riff_chunk};
    let info = info_chunk(&[("INAM", "ab"), ("IART", "Art")]);
    for frames in [7, 300_001] {
        let f =
            riff(&[fmt_chunk(8_000, 1, 8), riff_chunk(b"data", &vec![0x80; frames]), info.clone()]);
        let (t, log) = of_bytes(&f, Caps::default());
        assert_eq!((t.get("TITLE"), t.get("ARTIST")), (Some("ab"), Some("Art")), "{frames}");
        assert_eq!(log.len(), if frames == 7 { 1 } else { 2 }, "{log:?}");
    }
}

/// A Matroska file's Info Title is its TITLE when no tag states one, the tag's when one does,
/// read with the head as the scanner reads it.
#[test]
fn a_matroska_segment_title_is_the_title_no_tag_states() {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "sine=duration=1",
        "-c:a",
        "pcm_s16le",
        "-metadata",
        "title=Segment title",
        "-metadata",
        "artist=Tag title",
    ];
    let Some(b) = ffmpeg("title.mkv", &args) else { return };
    // the scanner keeps the track's DURATION, the open leaves the statistics out
    let scanned = |name: &str, b: &[u8]| {
        let mut t = scanned_bytes(name, b).tags;
        t.retain(|t| t.0 != "DURATION");
        t
    };
    let (t, _) = of_bytes(&b, Caps::default());
    assert_eq!((t.get("TITLE"), t.get("ARTIST")), (Some("Segment title"), Some("Tag title")));
    assert_eq!(scanned("title.mkv", &b), t.tags);
    // the ARTIST tag renamed TITLE, NUL padded to the same length
    let name = [&[0x45, 0xA3, 0x86][..], b"ARTIST"].concat();
    let at = b.windows(name.len()).position(|w| w == name).unwrap() + 3;
    let mut tagged = b.clone();
    tagged[at..at + 6].copy_from_slice(b"TITLE\0");
    let (t, _) = of_bytes(&tagged, Caps::default());
    assert_eq!(t.get("TITLE"), Some("Tag title"));
    assert_eq!(t.tags.iter().filter(|(k, _)| k == "TITLE").count(), 1, "{:?}", t.tags);
    assert_eq!(scanned("tagged.mkv", &tagged), t.tags);
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
