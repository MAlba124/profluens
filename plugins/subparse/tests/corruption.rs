//! The seeded corruption gate for pf-subparse, over the parsers, the cue IR wire and the element.

// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use pf_sim::corrupt::harness::drive;
use pf_sim::corrupt::{
    Chunking, Corpus, Input, Tracking, corpus_gate, heap_bounded, heap_during, iterations,
};
use pf_subparse::autodetect;
use pf_subparse::formats::ssa::framed_dialogue;
use pf_subparse::formats::webvtt::cue_text_to_pango_markup;
use pf_subparse::ir::{CueBuilder, CueIr, EncodeError, HEADER_LEN};
use pf_subparse::ssastyle::{SsaStyles, dialogue_into};
use pf_subparse::{CUE_SLOT, Format, OutputFormat, ParseContext, SubParse, document_ir};

#[global_allocator]
static HEAP: Tracking = Tracking;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

const SRT: &str = "1\n00:00:01,000 --> 00:00:03,500\n<i>Hello</i> <b>world</b>\n\n\
2\n00:00:04,000 --> 00:00:05,000 X1:10 X2:20\n<font color=\"red\">line one</font>\n\
line two \u{65e5}\u{672c}\u{8a9e}\n\n3\n01:02:03,004 --> 01:02:05,006\n5 < 6 and {\\an8}top\n";

const VTT: &str = "WEBVTT - title\n\nNOTE a comment\nover two lines\n\nSTYLE\n\
::cue { color: red }\n\ncue-1\n00:01.000 --> 00:03.000 line:90% align:center\n\
<v Bob>hi</v> <c.yellow>there</c>\n\n00:00:04.500 --> 00:00:06.000\n\
<00:00:05.000>karaoke <b>word</b>\n";

const ASS: &str = "[Script Info]\nTitle: t\nScriptType: v4.00+\n\n[V4+ Styles]\n\
Format: Name, Fontname, Fontsize\nStyle: Default,Arial,20\n\n[Events]\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,{\\i1}hello{\\i0}\\Nsecond, third\n\
Dialogue: 0,0:00:04.00,0:00:05.00,Default,,0,0,0,,{\\pos(10,20)\\t({\\fs20})}moving\\htext\n\
Comment: 0,0:00:04.00,0:00:05.00,Default,,0,0,0,,ignored\n";

const MKV_ASS: &str = "0,0,Default,,0,0,0,,{\\b1}bold{\\b0} caption\\Nsecond";

fn text_fields(bytes: &[u8]) -> Vec<usize> {
    let mut at = vec![0];
    for (i, w) in bytes.windows(3).enumerate() {
        if w == b"-->" {
            at.extend([i.saturating_sub(13), i.saturating_sub(4), i, i + 4, i + 13]);
        }
        if w[0] == b'\n' || w[0] == b'\r' {
            at.push(i + 1);
        }
    }
    at.retain(|&a| a < bytes.len());
    at.sort_unstable();
    at.dedup();
    at
}

struct Fixture {
    name: &'static str,
    bytes: Vec<u8>,
    fields: Vec<usize>,
}

fn text_fixtures() -> Vec<Fixture> {
    let make = |name, bytes: Vec<u8>| Fixture { name, fields: text_fields(&bytes), bytes };
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(ASS.as_bytes());
    vec![
        make("srt", SRT.as_bytes().to_vec()),
        make("srt_crlf", SRT.replace('\n', "\r\n").into_bytes()),
        make("srt_lone_cr", SRT.replace('\n', "\r").into_bytes()),
        make("vtt", VTT.as_bytes().to_vec()),
        make("vtt_crlf", VTT.replace('\n', "\r\n").into_bytes()),
        make("ass", ASS.as_bytes().to_vec()),
        make("ass_bom", bom),
        make("mkv_ass_block", MKV_ASS.as_bytes().to_vec()),
    ]
}

fn inputs(fixtures: &[Fixture]) -> Vec<Input<'_>> {
    fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &f.fields }).collect()
}

const TEXT_TOKENS: &[&[u8]] = &[
    b"\x80",
    b"\xC3",
    b"\xE6\x97",
    b"\xF0\x9F\x98",
    b"\xC0\xAF",
    b"\xE0\x80\xAF",
    b"\xF0\x82\x82\xAC",
    b"\xED\xA0\x80",
    b"\xF4\x90\x80\x80",
    b"\xFF",
    b"A",
    b"<",
    b">",
    b"{",
    b"}",
    b"<b>",
    b"<i>",
    b"<font color=\"#ff0000\">",
    b"<c.loud>",
    b"<v Speaker>",
    b"<00:00:01.000>",
    b"{\\b1}",
    b"{\\t(",
    b"\\N",
    b"\\h",
    b"\\",
    b"99999999999:99:99,999 --> 99999999999:99:99.999\n",
    b"-00:00:01,000 --> -00:00:02,000\n",
    b"00:00:05,000 --> 00:00:01,000\n",
    b"00:00:05.000 --> 00:00:01.000\n",
    b"00:00:01 --> \n",
    b" --> ",
    b"-->-->-->",
    b"00:00:00,18446744073709551616 --> 00:00:00,99999999999999999999999\n",
    b"18446744073709551615:00:00.00",
    b"0:00:00.1844674407370955162,",
    b"6000000:00:00,000",
    b":",
    b",",
    b".",
    b"9",
    b"\r\n",
    b"\r",
    b"\n\n",
    b"\xEF\xBB\xBF",
    b"\0",
    b"[Events]\n",
    b"Format: Text, Start, End\n",
    b"Format:",
    b"Dialogue: ",
    b"Dialogue: 0,0:00:00.00,0:00:01.00,D,,0,0,0,,x\n",
    b"WEBVTT\n\n",
    b"NOTE ",
    b"STYLE\n",
    b"REGION\n",
    b"1\n00:00:00,000 --> 00:00:01,000\nx\n\n",
];

const MIN_CUE_BYTES: usize = 13;

/// The builder's cue validates, re-lays out to the same bytes and walks without allocating.
fn check_wire(what: &str, b: &mut CueBuilder, slot: &mut [u8]) {
    match b.finish(slot) {
        Ok(n) => {
            let view = CueIr::parse(&slot[..n]).unwrap_or_else(|e| panic!("{what}: {e}"));
            let ((), grew) = heap_during(|| {
                walk(&view);
            });
            assert_eq!(grew, 0, "{what}: the view allocated");
            let mut again = CueBuilder::new();
            again.copy_from(&view);
            let bytes = again.to_vec();
            let back = CueIr::parse(&bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!(
                back.same_as(&view),
                "{what}: a re-laid cue reads differently\n{}\n{}",
                view.dump(),
                back.dump()
            );
            again.copy_from(&back);
            assert_eq!(again.to_vec(), bytes, "{what}: the re-laid form is stable");
        }
        Err(EncodeError::DoesNotFit { capacity }) => assert_eq!(capacity, slot.len()),
        Err(EncodeError::TooMany) => panic!("{what}: a document cue past the u32 counts"),
    }
}

fn check_parsers(source: &str, slot: &mut [u8]) {
    let mut b = CueBuilder::new();
    b.srt_text(source);
    check_wire("srt block", &mut b, slot);
    b.pango_markup(&cue_text_to_pango_markup(source), None, None);
    check_wire("vtt block", &mut b, slot);
    if let Some(d) = framed_dialogue(source) {
        dialogue_into(&d, &SsaStyles::default(), 0, &mut b);
        check_wire("ass block", &mut b, slot);
    }
    let Some(format) = autodetect::detect(source) else {
        return;
    };
    let mut parser = format.parser();
    let Ok(cues) = parser.parse(source, &ParseContext::default()) else {
        return;
    };
    assert!(
        cues.len() <= source.len() / MIN_CUE_BYTES + 1,
        "{format:?} gave {} cues from {} bytes",
        cues.len(),
        source.len()
    );
    for cue in &cues {
        document_ir(cue, &*parser, &mut b);
        check_wire("document cue", &mut b, slot);
    }
}

fn parsers_target(bytes: &[u8], _: Chunking) {
    let mut slot = vec![0u8; CUE_SLOT];
    check_parsers(&String::from_utf8_lossy(bytes), &mut slot);
    let valid = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""),
    };
    check_parsers(valid, &mut slot);
}

#[test]
fn text_parsers_survive_corruption() {
    let fixtures = text_fixtures();
    let gate = Corpus {
        name: "text_parsers",
        inputs: &inputs(&fixtures),
        tokens: TEXT_TOKENS,
        iters: iterations(500),
        sweep: 24,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(256 * KIB, 512, parsers_target));
}

fn wire_fixture(name: &'static str, b: &mut CueBuilder) -> Fixture {
    let bytes = b.to_vec();
    CueIr::parse(&bytes).expect("a builder payload validates");
    let mut fields: Vec<usize> = (0..HEADER_LEN).step_by(2).collect();
    // The six section offsets, then the first records of each section.
    for field in (32..56).step_by(4) {
        let off = u32::from_le_bytes([
            bytes[field],
            bytes[field + 1],
            bytes[field + 2],
            bytes[field + 3],
        ]);
        fields.extend((0..4).map(|k| off as usize + 8 * k));
    }
    fields.retain(|&a| a < bytes.len());
    fields.sort_unstable();
    fields.dedup();
    Fixture { name, bytes, fields }
}

fn wire_fixtures() -> Vec<Fixture> {
    let srt = autodetect::detect(SRT).expect("srt");
    let srt_cues = srt.parser().parse(SRT, &ParseContext::default()).expect("parses");
    let vtt = Format::WebVtt;
    let mut vtt_parser = vtt.parser();
    let vtt_cues = vtt_parser.parse(VTT, &ParseContext::default()).expect("parses");
    let d = framed_dialogue(MKV_ASS).expect("a row");
    let mut karaoke = String::from("0,0,Default,,0,0,0,,");
    for i in 0..200 {
        karaoke.push_str(&format!("{{\\k{}}}la", 5 + i % 7));
    }
    let k = framed_dialogue(&karaoke).expect("a row");
    let mut b = CueBuilder::new();
    let empty = wire_fixture("empty", &mut b);
    b.cue(&srt_cues[1], OutputFormat::PangoMarkup, None);
    let srt_font = wire_fixture("srt_font", &mut b);
    b.cue(&vtt_cues[0], OutputFormat::PangoMarkup, vtt_parser.stylesheet());
    let vtt_voice_ruby = wire_fixture("vtt_voice_ruby", &mut b);
    dialogue_into(&d, &SsaStyles::default(), 0, &mut b);
    let ass_block = wire_fixture("ass_block", &mut b);
    dialogue_into(&k, &SsaStyles::default(), 1_000_000_000, &mut b);
    let ass_karaoke = wire_fixture("ass_karaoke", &mut b);
    vec![empty, srt_font, vtt_voice_ruby, ass_block, ass_karaoke]
}

const WIRE_TOKENS: &[&[u8]] = &[
    b"\0",
    b"\xff",
    b"\xff\xff\xff\xff",
    b"\x00\x00\x00\x80",
    b"\x01\x00\x00\x00",
    b"\x44\x00\x00\x00",
    b"CUIR",
    b"\x80",
    b"\xc3",
];

/// Every string the view hands out, summed, so a reference past the region shows as a length.
fn walk(view: &CueIr<'_>) -> usize {
    let mut bytes = 0;
    std::hint::black_box((view.layout(), view.base_style()));
    for line in view.lines() {
        for span in view.line_spans(line) {
            bytes += view.span_text(span).len();
            bytes += view.span_voice(span).map_or(0, str::len);
            bytes += view.span_classes(span).map(str::len).sum::<usize>();
            if let Some(r) = view.span_ruby(span) {
                bytes += view.ruby_text(r).len();
            }
            let style = view.span_style(span);
            bytes += view.style_family(style).map_or(0, str::len);
            bytes += view.style_language(style).map_or(0, str::len);
        }
    }
    bytes
}

fn wire_target(bytes: &[u8], _: Chunking) {
    // An unaligned copy must be refused, an aligned one judged on its content.
    let mut shifted = vec![0u8; bytes.len() + 1];
    shifted[1..].copy_from_slice(bytes);
    assert!(CueIr::parse(&shifted[1..]).is_err() || bytes.as_ptr().addr() % 8 == 1);
    let ((), grew) = heap_during(|| {
        if let Ok(view) = CueIr::parse(bytes) {
            let read = walk(&view);
            assert!(read <= bytes.len(), "{read} bytes of strings read from {}", bytes.len());
        }
    });
    assert_eq!(grew, 0, "the view allocated {grew} bytes");
    if let Ok(view) = CueIr::parse(bytes) {
        let mut b = CueBuilder::new();
        b.copy_from(&view);
        let again = b.to_vec();
        let back = CueIr::parse(&again).expect("a re-laid cue validates");
        assert!(back.same_as(&view), "a re-laid cue reads differently");
        b.copy_from(&back);
        assert_eq!(b.to_vec(), again, "a second lay out is byte stable");
    }
}

#[test]
fn cue_ir_wire_refuses_corruption_without_panicking_or_allocating() {
    let fixtures = wire_fixtures();
    let gate = Corpus {
        name: "cue_ir_wire",
        inputs: &inputs(&fixtures),
        tokens: WIRE_TOKENS,
        iters: iterations(600),
        sweep: 32,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(64 * KIB, 8, wire_target));
}

const FAMILIES: [&str; 4] = ["subtitle/srt", "subtitle/vtt", "subtitle/ass", "bytes"];

const MAX_PIECES: usize = 4096;

fn coarse(bytes: &[u8], chunking: Chunking) -> Chunking {
    if chunking.pieces(bytes).count() <= MAX_PIECES {
        chunking
    } else {
        Chunking::Fixed(bytes.len().div_ceil(MAX_PIECES))
    }
}

fn subparse_target(bytes: &[u8], chunking: Chunking) {
    let chunking = coarse(bytes, chunking);
    let cues = chunking.pieces(bytes).count();
    for (n, family) in FAMILIES.iter().enumerate() {
        let flush = (n == bytes.len() % 4).then_some(1 + bytes.len() % 3);
        let mut h = Harness::new(SubParse::new());
        h.fix_sink_format(SubParse::IN, family, &[]);
        let driven = drive(&mut h, bytes, chunking, flush);
        assert!(!driven.failed, "subparse has no failure path, family {family}");
        if *family == "bytes" {
            assert!(
                driven.out_buffers <= bytes.len() / MIN_CUE_BYTES + 1,
                "{} cues from a {} byte document",
                driven.out_buffers,
                bytes.len()
            );
        } else {
            assert!(
                driven.out_buffers <= cues,
                "{} cues out of {cues} in, family {family}",
                driven.out_buffers
            );
        }
        assert!(
            driven.out_bytes <= driven.out_buffers * CUE_SLOT,
            "{family} gave {} bytes in {} cues",
            driven.out_bytes,
            driven.out_buffers
        );
    }
}

#[test]
fn subparse_element_survives_corruption() {
    let mut fixtures = text_fixtures();
    fixtures.retain(|f| ["srt", "vtt_crlf", "ass_bom", "mkv_ass_block"].contains(&f.name));
    for fixture in &mut fixtures {
        fixture.fields = fixture.fields.iter().copied().step_by(4).collect();
    }
    let gate = Corpus {
        name: "subparse_element",
        inputs: &inputs(&fixtures),
        tokens: TEXT_TOKENS,
        iters: iterations(300),
        sweep: 8,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(4 * MIB, 16, subparse_target));
}

#[test]
fn fixtures_are_valid() {
    let count = |doc: &str| {
        let format = autodetect::detect(doc).expect("a known format");
        format.parser().parse(doc, &ParseContext::default()).expect("parses").len()
    };
    assert_eq!(count(SRT), 3);
    assert_eq!(count(VTT), 2);
    assert!(autodetect::detect(ASS).is_none(), "SSA is not in the C typefinder");
    assert_eq!(autodetect::detect_document(ASS), Some(Format::Ssa));
    let ass = Format::Ssa.parser().parse(ASS, &ParseContext::default()).expect("parses");
    assert_eq!(ass.len(), 2);
    let d = framed_dialogue(MKV_ASS).expect("a row");
    let mut b = CueBuilder::new();
    dialogue_into(&d, &SsaStyles::default(), 0, &mut b);
    let bytes = b.to_vec();
    assert_eq!(CueIr::parse(&bytes).expect("valid").plain_text(), "bold caption\nsecond");
}
