//! Documents pushed through the `subparse` element as raw bytes, so each crosses the charset
//! decoder and the format parser. The charset fixtures stand in for pysrt's (GPL-3.0, not copied),
//! built here from our own reference text in the same encodings.

// Test code, fixtures are read from disk or built on the heap.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use pf_harness::Harness;
use pf_subparse::encoding::{CP1252_HIGH, SNIFF_LIMIT};
use pf_subparse::ir::CueIr;
use pf_subparse::{Charset, SubParse};
use profluens_core::time::Timestamp;

const WPTVTT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/wptvtt");

/// A cue's timing and its validated `subtitle/cue-ir` bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Cue(Timestamp, Timestamp, Vec<u8>);

impl Cue {
    fn plain_text(&self) -> String {
        CueIr::parse(&self.2).expect("validated at capture").plain_text()
    }
}

fn run_pieces(el: SubParse, family: &str, pieces: &[&[u8]]) -> Vec<Cue> {
    let mut h = Harness::new(el);
    h.fix_sink_format(SubParse::IN, family, &[]);
    let mut out = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        let mut buf = h.alloc(piece);
        buf.pts = Timestamp::from_secs(i as u64);
        buf.duration = Timestamp::from_secs(1);
        h.push(buf).expect("push");
        out.extend(h.drain_outputs());
    }
    out.extend(h.eos().expect("eos"));
    out.iter()
        .map(|b| {
            CueIr::parse(b.memory.data()).expect("a valid cue-ir payload");
            Cue(b.pts, b.duration, b.memory.data().to_vec())
        })
        .collect()
}

fn run(bytes: &[u8]) -> Vec<Cue> {
    run_pieces(SubParse::new(), "bytes", &[bytes])
}

const CUES: [&str; 6] = [
    "\u{c9}V\u{c9}NEMENTS",
    "<i>Caf\u{e9} cr\u{e8}me, s\u{2019}il vous pla\u{ee}t\u{2026}</i>",
    "\u{201c}Na\u{ef}ve\u{201d} \u{2013} \u{fc}ber \u{20ac}20",
    "C\u{153}ur \u{e0} l\u{2019}ouvrage, gar\u{e7}on",
    "Line one\nLine two \u{e5}\u{f8}\u{e6}",
    "plain ascii",
];

fn stamp(s: usize) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Our reference SubRip, `count` cues cycling through [`CUES`].
fn reference(count: usize, eol: &str) -> String {
    let mut doc = String::new();
    for i in 0..count {
        let (start, end) = (stamp(i * 3 + 1), stamp(i * 3 + 3));
        let text = CUES[i % CUES.len()].replace('\n', eol);
        doc += &format!("{}{eol}{start},000 --> {end},500{eol}{text}{eol}{eol}", i + 1);
    }
    doc
}

fn cp1252(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| match u32::from(c) {
            n @ (0..0x80 | 0xa0..0x100) => n as u8,
            _ => 0x80 + CP1252_HIGH.iter().position(|&h| h == c).expect("a cp1252 char") as u8,
        })
        .collect()
}

fn variants(text: &str) -> [(&'static str, Vec<u8>); 5] {
    let mut utf8 = vec![0xef, 0xbb, 0xbf];
    utf8.extend_from_slice(text.as_bytes());
    let wide = |bom: &[u8], f: fn(u16) -> [u8; 2]| {
        let mut out = bom.to_vec();
        text.encode_utf16().for_each(|u| out.extend(f(u)));
        out
    };
    let wider = |bom: &[u8], f: fn(u32) -> [u8; 4]| {
        let mut out = bom.to_vec();
        text.chars().for_each(|c| out.extend(f(u32::from(c))));
        out
    };
    [
        ("bom-utf-8", utf8),
        ("bom-utf-16-le", wide(&[0xff, 0xfe], u16::to_le_bytes)),
        ("bom-utf-16-be", wide(&[0xfe, 0xff], u16::to_be_bytes)),
        ("bom-utf-32-le", wider(&[0xff, 0xfe, 0, 0], u32::to_le_bytes)),
        ("bom-utf-32-be", wider(&[0, 0, 0xfe, 0xff], u32::to_be_bytes)),
    ]
}

#[test]
fn pysrt_bom_variants_match_the_utf8_reference() {
    let text = reference(12, "\n");
    let want = run(text.as_bytes());
    assert_eq!(want.len(), 12);
    assert_eq!(want[0].plain_text(), "\u{c9}V\u{c9}NEMENTS");
    for (name, bytes) in variants(&text) {
        assert_eq!(run(&bytes), want, "{name} decodes to the reference cues");
        let pieces: Vec<&[u8]> = bytes.chunks(3).collect();
        assert_eq!(run_pieces(SubParse::new(), "bytes", &pieces), want, "{name} in 3 byte pushes");
    }
}

#[test]
fn pysrt_windows_1252_matches_the_utf8_reference() {
    let utf8 = reference(2500, "\n");
    let crlf = reference(2500, "\r\n");
    let legacy = cp1252(&crlf);
    assert!(legacy.len() > 2 * SNIFF_LIMIT, "the window fills and latches");
    let want = run(utf8.as_bytes());
    assert_eq!(want.len(), 2500);
    assert!(want.iter().any(|c| c.plain_text().contains("\u{c9}V\u{c9}NEMENTS")));
    assert_eq!(run(&legacy), want, "a large BOM-less cp1252 file");
    let small = cp1252(&reference(6, "\r\n"));
    assert_eq!(run(&small), want[..6], "a small one, decided at its end");
}

#[test]
fn a_named_charset_reads_a_file_with_no_mark() {
    let text = reference(6, "\n");
    let want = run(text.as_bytes());
    let le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let named =
        |c: Charset, bytes: &[u8]| run_pieces(SubParse::with_charset(Some(c)), "bytes", &[bytes]);
    assert_eq!(named(Charset::Utf16Le, &le), want);
    assert!(run(&le).is_empty(), "sniffing does not guess UTF-16");
    assert_eq!(named(Charset::Windows1252, &cp1252(&text)), want);
    let (_, bom_utf8) = variants(&text).into_iter().next().expect("the UTF-8 variant");
    assert_eq!(named(Charset::Utf16Le, &bom_utf8), want, "a mark beats the named charset");
}

#[test]
fn a_stray_byte_spoils_one_character_not_the_document() {
    let mut bytes = reference(6, "\n").into_bytes();
    let at = bytes.windows(5).position(|w| w == b"plain").expect("the ascii cue");
    bytes[at] = 0xff;
    let cues = run(&bytes);
    assert_eq!(cues.len(), 6);
    assert_eq!(cues[0].plain_text(), "\u{c9}V\u{c9}NEMENTS", "cues before it intact");
    assert_eq!(cues[5].plain_text(), "\u{fffd}lain ascii");
}

#[test]
fn blocks_are_decoded_with_the_running_counts() {
    let block = |s: &str| cp1252(s);
    let pieces = [block("caf\u{e9}"), block("\u{c9}t\u{e9} \u{e0} Paris"), block("ascii")];
    let pieces: Vec<&[u8]> = pieces.iter().map(Vec::as_slice).collect();
    let cues = run_pieces(SubParse::new(), "subtitle/srt", &pieces);
    let texts: Vec<String> = cues.iter().map(Cue::plain_text).collect();
    assert_eq!(texts, ["caf\u{e9}", "\u{c9}t\u{e9} \u{e0} Paris", "ascii"]);
    let utf8 = ["\u{e9}\u{e8}\u{ea}", "ok", "bad \u{e9}"].map(str::as_bytes);
    let mut damaged = utf8.map(<[u8]>::to_vec);
    damaged[2].push(0xff);
    let pieces: Vec<&[u8]> = damaged.iter().map(Vec::as_slice).collect();
    let cues = run_pieces(SubParse::new(), "subtitle/srt", &pieces);
    assert_eq!(cues[2].plain_text(), "bad \u{e9}\u{fffd}", "earlier UTF-8 outvotes the damage");
}

#[test]
fn wpt_vtt_corpus_smoke() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(WPTVTT)
        .expect("the corpus directory")
        .map(|e| e.expect("an entry").path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "vtt" || e == "webvtt"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 91);
    let mut total = 0;
    for path in &files {
        let bytes = std::fs::read(path).expect("a corpus file");
        let cues = run(&bytes);
        total += cues.len();
        for (name, variant) in variants(&String::from_utf8(bytes).expect("UTF-8")) {
            assert_eq!(run(&variant), cues, "{} as {name}", path.display());
        }
    }
    assert!(total > 0, "the corpus yields cues");
}
