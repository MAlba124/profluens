// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Cue-ir reproductions, what the IR builder makes of real parser output.

// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use pf_subparse::formats::{Ssa, SubRip, WebVtt};
use pf_subparse::ir::{Color, CueBuilder, CueIr, SpanRec};
use pf_subparse::ssastyle::dialogue_into;
use pf_subparse::{OutputFormat, ParseContext, SubtitleFormat};

fn srt_ir(text: &str) -> Vec<u8> {
    let body = format!("1\n00:00:01,000 --> 00:00:02,000\n{text}\n\n");
    let cues = SubRip::default().parse(&body, &ParseContext::default()).unwrap();
    assert_eq!(cues.len(), 1, "{body:?}");
    let mut b = CueBuilder::new();
    b.cue(&cues[0], OutputFormat::PangoMarkup, None);
    b.to_vec()
}

fn spans<'a>(ir: &CueIr<'a>) -> impl Iterator<Item = &'a SpanRec> + 'a {
    let ir = *ir;
    ir.lines().iter().flat_map(move |l| ir.line_spans(l).iter())
}

#[test]
fn srt_ir_literal_lt_does_not_swallow_the_next_tag() {
    const CASES: &[(&str, &str)] = &[
        ("I <3 you, <i>really</i>", "I <3 you, really"),
        ("I <3 New York\n<i>(music)</i>", "I <3 New York\n(music)"),
        ("<-- <b>back</b>", "<-- back"),
    ];
    let mut bad = Vec::new();
    for &(text, want) in CASES {
        let bytes = srt_ir(text);
        let ir = CueIr::parse(&bytes).expect("valid");
        let got = ir.plain_text();
        let newline_in_span = spans(&ir).any(|s| ir.span_text(s).contains('\n'));
        if got != want || newline_in_span {
            bad.push(format!(
                "{text:?}: plain {got:?} want {want:?}, lines {}, newline in span {newline_in_span}",
                ir.line_count()
            ));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn webvtt_ir_keeps_bg_classes_and_voice_names() {
    let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n\
                <c.yellow.bg_blue>warn</c> <v Jean-Luc>hi</v> <v Zo\u{eb}>yo</v>\n\n";
    let mut parser = WebVtt::default();
    let cues = parser.parse(body, &ParseContext::default()).unwrap();
    assert_eq!(cues.len(), 1);
    let mut b = CueBuilder::new();
    b.cue(&cues[0], OutputFormat::PangoMarkup, parser.stylesheet());
    let bytes = b.to_vec();
    let ir = CueIr::parse(&bytes).expect("valid");
    let span = |t: &str| spans(&ir).find(|s| ir.span_text(s) == t);

    let mut bad = Vec::new();
    match span("warn") {
        Some(s)
            if ir.span_style(s).background() == Some(Color::rgb(0, 0, 0xff))
                && ir.span_classes(s).any(|c| c == "bg_blue") => {}
        other => bad.push(format!("warn: {other:?}")),
    }
    for (text, voice) in [("hi", "Jean-Luc"), ("yo", "Zo\u{eb}")] {
        match span(text) {
            Some(s) if ir.span_voice(s) == Some(voice) => {}
            other => bad.push(format!("{text}: want voice {voice:?}, got {other:?}")),
        }
    }
    assert!(bad.is_empty(), "markup {:?}\n{bad:#?}", cues[0].text);
}

#[test]
fn ssa_ir_pbo_does_not_end_drawing_mode() {
    let body = "[Events]\n\
                Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,\
                {\\an7\\p1\\pbo0}m 0 0 l 100 0 100 100 0 100{\\p0}Sign\n";
    let mut parser = Ssa::default();
    let cues = parser.parse(body, &ParseContext::default()).unwrap();
    assert_eq!(cues.len(), 1);
    let d = cues[0].ssa.as_deref().expect("dialogue extras");
    let mut b = CueBuilder::new();
    dialogue_into(&d.event(), parser.ssa_styles().unwrap(), cues[0].start_ns, &mut b);
    let bytes = b.to_vec();
    assert_eq!(CueIr::parse(&bytes).expect("valid").plain_text(), "Sign", "raw {:?}", d.raw_text);
}
