use super::*;
use pf_mkv::ebml::{id, write_binary, write_string, write_uint};

fn titled(start_ns: u64, end_ns: Option<u64>, text: &str) -> ItemChapter {
    let titles = vec![ChapterTitle { language: String::new(), text: text.into() }];
    ItemChapter { start_ns, end_ns, titles, ..ItemChapter::default() }
}

fn raw(edition: usize, parent: Option<usize>, c: ItemChapter) -> RawChapter {
    RawChapter { edition, parent, chapter: c }
}

fn edition(ordered: bool) -> ChapterEdition {
    ChapterEdition { ordered, ..ChapterEdition::default() }
}

/// Each chapter's depth, first title and span.
fn shape(e: &ChapterEdition) -> Vec<(u8, &str, u64, Option<u64>)> {
    e.chapters
        .iter()
        .map(|c| (c.depth, c.titles.first().map_or("", |t| t.text.as_str()), c.start_ns, c.end_ns))
        .collect()
}

#[test]
fn siblings_sort_by_start_ties_keep_their_order_and_children_follow_their_parent() {
    let chapters = vec![
        raw(0, None, titled(50, None, "c")),
        raw(0, None, titled(10, Some(30), "a")),
        raw(0, Some(1), titled(25, None, "a.2")),
        raw(0, Some(1), titled(12, None, "a.1")),
        raw(0, None, titled(50, None, "c tie")),
        raw(0, Some(4), titled(60, Some(55), "end before start")),
    ];
    let got = normalise(ChapterSource::Matroska, vec![edition(false)], chapters, Some(100));
    assert_eq!(got.source, ChapterSource::Matroska);
    let e = &got.editions[0];
    assert!(e.default);
    assert_eq!(
        shape(e),
        [
            (0, "a", 10, Some(30)),
            (1, "a.1", 12, Some(25)),
            (1, "a.2", 25, Some(30)),
            (0, "c", 50, Some(50)),
            (0, "c tie", 50, Some(100)),
            (1, "end before start", 60, Some(100)),
        ]
    );
    assert_eq!(got.default_chapters().len(), 6);
}

#[test]
fn an_end_is_unknown_without_a_duration_and_ordered_editions_keep_their_order() {
    let flat = vec![raw(0, None, titled(5, None, "x")), raw(0, None, titled(0, None, "y"))];
    let got = normalise(ChapterSource::Chpl, vec![edition(false)], flat.clone(), None);
    assert_eq!(shape(&got.editions[0]), [(0, "y", 0, Some(5)), (0, "x", 5, None)]);
    let got = normalise(ChapterSource::Matroska, vec![edition(true)], flat, Some(9));
    assert_eq!(shape(&got.editions[0]), [(0, "x", 5, None), (0, "y", 0, None)], "as stated");
    let past = vec![raw(0, None, titled(50, None, "past the end"))];
    let got = normalise(ChapterSource::Chpl, vec![edition(false)], past, Some(10));
    assert_eq!(got.editions[0].chapters[0].end_ns, None, "no end before the start");
}

#[test]
fn editions_keep_one_default_and_empty_ones_go() {
    let mut eds = vec![edition(false), edition(false), edition(true), edition(false)];
    eds[2].default = true;
    eds[3].default = true;
    eds[2].uid = 7;
    let chapters = vec![
        raw(1, None, titled(0, None, "one")),
        raw(2, None, titled(0, None, "two")),
        raw(3, None, titled(0, None, "three")),
        raw(9, None, titled(0, None, "no such edition")),
    ];
    let got = normalise(ChapterSource::Matroska, eds, chapters, None);
    let flags: Vec<(bool, u64)> = got.editions.iter().map(|e| (e.default, e.uid)).collect();
    assert_eq!(flags, [(false, 0), (true, 7), (false, 0)]);
    assert_eq!(got.default_chapters()[0].titles[0].text, "two");
    let none = normalise(ChapterSource::Chpl, vec![edition(false)], Vec::new(), None);
    assert!(none.is_empty() && none.source == ChapterSource::None);
    let got = normalise(
        ChapterSource::Chpl,
        vec![edition(false); 2],
        vec![raw(1, None, titled(0, None, "z"))],
        None,
    );
    assert!(got.editions[0].default, "the first left is the default when none says");
}

#[test]
fn caps_bound_chapters_depth_titles_and_text() {
    let many: Vec<RawChapter> =
        (0..MAX_CHAPTERS as u64 + 50).map(|i| raw(0, None, titled(i, None, "t"))).collect();
    let got = normalise(ChapterSource::VorbisComment, vec![edition(false)], many, None);
    assert_eq!(got.editions[0].chapters.len(), MAX_CHAPTERS);
    // a chain deeper than the cap, and a child of a chapter left out
    let mut deep: Vec<RawChapter> =
        (0..20usize).map(|i| raw(0, i.checked_sub(1), titled(i as u64, None, "d"))).collect();
    deep.push(raw(0, Some(15), titled(99, None, "under a dropped one")));
    deep.push(raw(0, Some(40), titled(99, None, "parent past it")));
    let got = normalise(ChapterSource::Matroska, vec![edition(false)], deep, None);
    let c = &got.editions[0].chapters;
    assert_eq!(c.len(), MAX_CHAPTER_DEPTH as usize);
    assert_eq!(c.last().map(|c| c.depth), Some(MAX_CHAPTER_DEPTH - 1));
    let long = "é".repeat(MAX_CHAPTER_TITLE);
    let lots: Vec<ChapterTitle> = (0..20)
        .map(|i| ChapterTitle { language: "ger".into(), text: format!("  {i}{long}\0") })
        .collect();
    let mut one = titled(0, None, "");
    one.titles = lots.clone();
    one.titles.push(lots[0].clone());
    let got =
        normalise(ChapterSource::Matroska, vec![edition(false)], vec![raw(0, None, one)], None);
    let t = &got.editions[0].chapters[0].titles;
    assert_eq!(t.len(), MAX_CHAPTER_TITLES);
    assert!(t.iter().all(|t| t.text.len() <= MAX_CHAPTER_TITLE && t.language == "de"));
    assert!(t[0].text.starts_with("0é"), "trimmed, cut on a char boundary");
    let budget: Vec<RawChapter> = (0..MAX_CHAPTERS as u64)
        .map(|i| {
            let mut c = titled(i, None, "");
            c.titles = lots[..4].to_vec();
            raw(0, None, c)
        })
        .collect();
    let got = normalise(ChapterSource::Matroska, vec![edition(false)], budget, None);
    let text: usize = got.editions[0]
        .chapters
        .iter()
        .flat_map(|c| &c.titles)
        .map(|t| t.text.len() + t.language.len())
        .sum();
    assert!(text <= MAX_CHAPTER_TEXT, "{text}");
    assert_eq!(got.editions[0].chapters.len(), MAX_CHAPTERS, "chapters stay, untitled");
    let eds = vec![edition(false); MAX_EDITIONS + 4];
    let all: Vec<RawChapter> = (0..eds.len()).map(|e| raw(e, None, titled(0, None, "e"))).collect();
    assert_eq!(normalise(ChapterSource::Matroska, eds, all, None).editions.len(), MAX_EDITIONS);
}

#[test]
fn languages_are_bcp_47() {
    for (given, want) in [
        ("ger", "de"),
        ("deu", "de"),
        ("fre", "fr"),
        ("ENG", "en"),
        ("chi", "zh"),
        ("scc", "sr"),
        ("fil", "fil"),
        ("haw", "haw"),
        ("fr-CA", "fr-CA"),
        ("FRA-ca", "fr-ca"),
        ("en", "en"),
        ("und", ""),
        ("", ""),
        (" spa\0", "es"),
        ("e", ""),
        ("en_US", ""),
        ("123", ""),
        ("abcdefghi", ""),
        ("de-1996-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", ""),
    ] {
        assert_eq!(bcp47(given), want, "{given:?}");
    }
    assert!(ISO639_1.windows(2).all(|w| w[0].0 < w[1].0), "sorted for the search");
}

fn el(element: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    write_binary(&mut out, element, &children.concat());
    out
}
fn uint(element: &[u8], v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    write_uint(&mut out, element, v);
    out
}
fn utf8(element: &[u8], v: &str) -> Vec<u8> {
    let mut out = Vec::new();
    write_string(&mut out, element, v);
    out
}
fn display(text: &str, lang: &str, bcp: Option<&str>) -> Vec<u8> {
    let mut c = vec![utf8(id::CHAP_STRING, text), utf8(id::CHAP_LANGUAGE, lang)];
    c.extend(bcp.map(|b| utf8(id::CHAP_LANGUAGE_BCP47, b)));
    el(id::CHAPTER_DISPLAY, &c)
}
fn atom(uid: u64, start: u64, end: Option<u64>, rest: &[Vec<u8>]) -> Vec<u8> {
    let mut c = vec![uint(id::CHAPTER_UID, uid), uint(id::CHAPTER_TIME_START, start)];
    c.extend(end.map(|e| uint(id::CHAPTER_TIME_END, e)));
    c.extend(rest.iter().cloned());
    el(id::CHAPTER_ATOM, &c)
}

/// Two editions, the second default and ordered, nesting, hidden and disabled chapters.
fn mkv_chapters_element() -> Vec<u8> {
    let nested = [
        atom(21, 900, None, &[display("Teil B", "ger", None)]),
        atom(20, 400, None, &[display("Teil A", "ger", None)]),
    ];
    let main = el(
        id::EDITION_ENTRY,
        &[
            uint(id::EDITION_UID, 1),
            atom(3, 2000, None, &[display("Credits", "eng", None)]),
            atom(
                2,
                400,
                Some(2000),
                &[display("Middle", "eng", None), nested[0].clone(), nested[1].clone()],
            ),
            atom(
                1,
                0,
                None,
                &[
                    display("Opening", "eng", None),
                    display("Ouverture", "fre", Some("fr-CA")),
                    uint(id::CHAPTER_FLAG_HIDDEN, 1),
                    uint(id::CHAPTER_FLAG_ENABLED, 0),
                ],
            ),
        ],
    );
    let ordered = el(
        id::EDITION_ENTRY,
        &[
            uint(id::EDITION_UID, 2),
            uint(id::EDITION_FLAG_DEFAULT, 1),
            uint(id::EDITION_FLAG_ORDERED, 1),
            atom(9, 2000, Some(2500), &[]),
            atom(8, 0, Some(400), &[]),
        ],
    );
    el(id::CHAPTERS, &[main, ordered])
}

#[test]
fn matroska_editions_nesting_flags_and_languages() {
    let tags = el(
        id::TAGS,
        &[el(
            id::TAG,
            &[
                el(id::TARGETS, &[uint(id::TARGET_TYPE_VALUE, 30), uint(id::TAG_CHAPTER_UID, 3)]),
                el(
                    id::SIMPLE_TAG,
                    &[
                        utf8(id::TAG_NAME, "TITLE"),
                        utf8(id::TAG_STRING, "Abspann"),
                        utf8(id::TAG_LANGUAGE, "ger"),
                    ],
                ),
            ],
        )],
    );
    let got = mkv_chapters(&mkv_chapters_element(), &tags, 3000);
    assert_eq!(got.source, ChapterSource::Matroska);
    let main = &got.editions[0];
    assert!(!main.default && !main.ordered && main.uid == 1);
    assert_eq!(
        shape(main),
        [
            (0, "Opening", 0, Some(400)),
            (0, "Middle", 400, Some(2000)),
            (1, "Teil A", 400, Some(900)),
            (1, "Teil B", 900, Some(2000)),
            (0, "Credits", 2000, Some(3000)),
        ]
    );
    let open = &main.chapters[0];
    assert!(open.hidden && !open.enabled && open.uid == 1);
    let langs: Vec<(&str, &str)> =
        open.titles.iter().map(|t| (t.language.as_str(), t.text.as_str())).collect();
    assert_eq!(langs, [("en", "Opening"), ("fr-CA", "Ouverture")], "BCP 47 wins");
    let credits: Vec<&str> = main.chapters[4].titles.iter().map(|t| t.language.as_str()).collect();
    assert_eq!(credits, ["en", "de"], "the tag's title after the chapter's own");
    let ordered = &got.editions[1];
    assert!(ordered.default && ordered.ordered);
    let uids: Vec<u64> = ordered.chapters.iter().map(|c| c.uid).collect();
    assert_eq!(uids, [9, 8], "an ordered edition plays as stored");
    assert!(ordered.chapters[0].titles.is_empty());
    assert_eq!(got.default_chapters()[0].uid, 9);
    assert!(mkv_chapters(&[], &tags, 0).is_empty());
}

#[test]
fn a_cut_or_mangled_matroska_element_never_panics() {
    let c = mkv_chapters_element();
    for n in 0..c.len() {
        let got = mkv_chapters(&c[..n], &[], 0);
        assert!(got.editions.iter().all(|e| !e.chapters.is_empty()));
    }
    let mut seed = 7u64;
    let mut bad = c.clone();
    for b in &mut bad {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        if seed >> 61 == 0 {
            *b = (seed >> 33) as u8;
        }
    }
    let _ = mkv_chapters(&bad, &bad, u64::MAX);
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

fn chpl_moov(titles: &[(u64, &str)]) -> Vec<u8> {
    let mut b = vec![1, 0, 0, 0, 0, 0, 0, 0, titles.len() as u8];
    for (start, t) in titles {
        b.extend_from_slice(&(start / 100).to_be_bytes());
        b.push(t.len() as u8);
        b.extend_from_slice(t.as_bytes());
    }
    boxed(b"moov", &boxed(b"udta", &boxed(b"chpl", &b)))
}

fn sample(text: &[u8]) -> Vec<u8> {
    let mut b = (text.len() as u16).to_be_bytes().to_vec();
    b.extend_from_slice(text);
    // the encd atom ffmpeg writes after the text
    b.extend_from_slice(&[0, 0, 0, 12, b'e', b'n', b'c', b'd', 0, 0, 1, 0]);
    b
}

#[test]
fn an_mp4_chapter_track_wins_over_its_chpl() {
    use pf_mp4::chapters::{ChapterSample, ChapterTrack};
    let moov = chpl_moov(&[(0, "Nero one"), (2_000_000_000, "Nero two")]);
    let only_chpl = moov_chapters(&moov, &ChapterTrack::default(), 5_000_000_000);
    assert_eq!(only_chpl.source, ChapterSource::Chpl);
    assert_eq!(
        shape(&only_chpl.editions[0]),
        [
            (0, "Nero one", 0, Some(2_000_000_000)),
            (0, "Nero two", 2_000_000_000, Some(5_000_000_000))
        ]
    );
    let utf16: Vec<u8> =
        [0xFE, 0xFF].into_iter().chain("Zwöi".encode_utf16().flat_map(u16::to_be_bytes)).collect();
    let texts = [sample(b"Eins"), sample(&utf16), sample(b""), vec![0, 40, b'c']];
    let mut samples = Vec::new();
    let mut t = 0;
    for b in &texts {
        samples.push(ChapterSample { start_ns: t, end_ns: t + 1000, at: 0, len: b.len() as u32 });
        t += 1000;
    }
    let track = ChapterTrack { track_id: 2, language: *b"ger", samples, bytes: texts.concat() };
    let got = moov_chapters(&moov, &track, 0);
    assert_eq!(got.source, ChapterSource::ChapterTrack);
    let c = &got.editions[0].chapters;
    let titles: Vec<Option<(&str, &str)>> = c
        .iter()
        .map(|c| c.titles.first().map(|t| (t.language.as_str(), t.text.as_str())))
        .collect();
    assert_eq!(titles, [Some(("de", "Eins")), Some(("de", "Zwöi")), None, Some(("de", "c"))]);
    assert_eq!((c[1].start_ns, c[1].end_ns), (1000, Some(2000)));
    let unread = ChapterTrack { bytes: Vec::new(), ..track };
    assert_eq!(moov_chapters(&moov, &unread, 0).source, ChapterSource::Chpl, "nothing read");
    assert!(moov_chapters(&boxed(b"moov", &[]), &ChapterTrack::default(), 0).is_empty());
}

fn comments(entries: &[&str]) -> Vec<u8> {
    let mut b = 4u32.to_le_bytes().to_vec();
    b.extend_from_slice(b"test");
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        b.extend_from_slice(&(e.len() as u32).to_le_bytes());
        b.extend_from_slice(e.as_bytes());
    }
    b
}

#[test]
fn vorbis_comment_chapters_end_at_the_next_and_the_last_at_the_duration() {
    let body = comments(&[
        "TITLE=Book",
        "CHAPTER002=00:01:00.5",
        "chapter002name=Second",
        "CHAPTER001=00:00:00.000",
        "CHAPTER001NAME=First",
        "CHAPTER001URL=http://example.invalid",
        "CHAPTER003=1:00:00.000000001",
        "CHAPTER003LANGUAGE=ger",
        "CHAPTER003NAME=Third",
        "CHAPTER004=00:61:00.000",
        "CHAPTER004NAME=a bad clock, left out",
        "CHAPTER005NAME=no start, left out",
    ]);
    let got = vorbis_chapters(Some(&body), [], 3_700_000_000_000);
    assert_eq!(got.source, ChapterSource::VorbisComment);
    let e = &got.editions[0];
    assert_eq!(
        shape(e),
        [
            (0, "First", 0, Some(60_500_000_000)),
            (0, "Second", 60_500_000_000, Some(3_600_000_000_001)),
            (0, "Third", 3_600_000_000_001, Some(3_700_000_000_000)),
        ]
    );
    assert_eq!(e.chapters[2].titles[0].language, "de");
    let flac = vorbis_chapters(None, [&body[..]], 0);
    assert_eq!(flac.editions[0].chapters[2].end_ns, None, "no duration, no end");
    assert!(vorbis_chapters(Some(&comments(&["TITLE=x"])), [], 0).is_empty());
    for (key, is) in [
        ("CHAPTER001", true),
        ("chapter12name", true),
        ("CHAPTER0001URL", true),
        ("CHAPTER001LANGUAGE", true),
        ("CHAPTER", false),
        ("CHAPTER00001", false),
        ("CHAPTERS", false),
        ("CHAPTER001NOTE", false),
        ("TITLE", false),
    ] {
        assert_eq!(is_chapter_key(key), is, "{key}");
    }
}

#[test]
fn hostile_vorbis_comments_are_capped() {
    let entries: Vec<String> = (0..MAX_CHAPTERS + 200)
        .flat_map(|i| {
            [
                format!("CHAPTER{:04}=00:00:{:02}.000", i % 10_000, i % 60),
                format!("CHAPTER{:04}NAME={}", i, "x".repeat(400)),
            ]
        })
        .collect();
    let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
    let body = comments(&refs);
    let got = vorbis_chapters(Some(&body), [], 0);
    let c = &got.editions[0].chapters;
    assert_eq!(c.len(), MAX_CHAPTERS);
    let titles: Vec<usize> = c.iter().flat_map(|c| &c.titles).map(|t| t.text.len()).collect();
    assert!(titles.iter().all(|&n| n == MAX_CHAPTER_TITLE), "cut");
    assert_eq!(titles.len(), MAX_CHAPTER_TEXT / MAX_CHAPTER_TITLE, "the rest untitled");
    // overlapping and equal starts stay, in number order
    let body = comments(&[
        "CHAPTER1=00:00:05.000",
        "CHAPTER2=00:00:05.000",
        "CHAPTER1NAME=a",
        "CHAPTER2NAME=b",
    ]);
    let got = vorbis_chapters(Some(&body), [], 0);
    assert_eq!(
        shape(&got.editions[0]),
        [(0, "a", 5_000_000_000, Some(5_000_000_000)), (0, "b", 5_000_000_000, None)]
    );
    for n in 0..body.len() {
        let _ = vorbis_chapters(Some(&body[..n]), [&body[..n]], 1);
    }
}
