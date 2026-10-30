// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Superlinear scans left next to the ones `tag_scan_linearity.rs` pins.

// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use pf_subparse::formats::{SubRip, WebVtt};
use pf_subparse::ir::{CueBuilder, CueIr, Names};
use pf_subparse::ssastyle::{SsaEvent, SsaStyles, dialogue_into};
use pf_subparse::vttcss::MAX_RULES;
use pf_subparse::{OutputFormat, ParseContext, SubtitleFormat};

const BUDGET: Duration = Duration::from_secs(8);

fn within(what: &str, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        f();
        let _ = tx.send(started.elapsed());
    });
    match rx.recv_timeout(BUDGET) {
        Ok(took) => eprintln!("{what} took {took:?}"),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what} not done within {BUDGET:?}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
    }
}

fn stray_closers() -> String {
    "</i>".repeat(1 << 20)
}

#[test]
fn subrip_stray_closing_tags_are_linear() {
    let body = format!("1\n00:00:01,000 --> 00:00:02,000\n{}\n\n", stray_closers());
    within("SubRip 4 MB of </i>", move || {
        let cues = SubRip::default().parse(&body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert!(!cues[0].text.contains('<'), "every stray closer is dropped");
    });
}

#[test]
fn webvtt_stray_closing_tags_are_linear() {
    let body = format!("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n{}\n\n", stray_closers());
    within("WebVTT 4 MB of </i>", move || {
        let cues = WebVtt::default().parse(&body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert!(!cues[0].text.contains('<'), "every stray closer is dropped");
    });
}

#[test]
fn webvtt_style_block_of_stray_braces_is_linear() {
    let body =
        format!("WEBVTT\n\nSTYLE\n{}\n\n00:00:01.000 --> 00:00:02.000\nx\n\n", "}".repeat(200_000));
    within("WebVTT STYLE of 200k '}'", move || {
        let cues = WebVtt::default().parse(&body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "x");
    });
}

#[test]
fn ssa_unknown_tags_with_open_parens_are_linear() {
    let raw_text = format!("{{{}}}x", "\\(".repeat(150_000));
    within("SSA override block of 150k \\(", move || {
        let d = SsaEvent {
            raw_text: &raw_text,
            style: "Default",
            margin_l: 0,
            margin_r: 0,
            margin_v: 0,
        };
        let mut b = CueBuilder::new();
        dialogue_into(&d, &SsaStyles::default(), 0, &mut b);
        let bytes = b.to_vec();
        assert_eq!(pf_subparse::CueIr::parse(&bytes).expect("valid").plain_text(), "x");
    });
}

fn one_mb(unit: &str) -> String {
    let mut s = String::with_capacity((1 << 20) + unit.len());
    while s.len() < 1 << 20 {
        s.push_str(unit);
    }
    s
}

#[test]
fn ir_markup_of_escaped_angle_brackets_and_ampersands_is_linear() {
    within("IR 1 MB of &lt;5 and &", move || {
        for unit in ["&lt;5", "&&&", "&lt;00:00:01"] {
            let body = format!("{}&gt;", one_mb(unit));
            let mut b = CueBuilder::new();
            b.pango_markup(&body, None, None);
            assert!(b.overflowed(), "{unit}: a 1 MB cue is refused, not built");
        }
    });
}

#[test]
fn ir_nested_markup_with_text_at_every_level_is_bounded() {
    within("IR 64k nested <c.a>x and stray closers", move || {
        let mut b = CueBuilder::new();
        b.pango_markup(&"<c.a>x".repeat(1 << 16), None, None);
        assert!(b.overflowed());
        b.srt_text(&format!("{}{}", "<i>".repeat(1 << 16), "</q>".repeat(1 << 16)));
        assert!(!b.overflowed(), "no text, nothing laid out");
        assert!(CueIr::parse(&b.to_vec()).is_ok());
    });
}

#[test]
fn ir_distinct_styles_past_the_slot_stop_the_work() {
    within("IR 200k distinct font colours", move || {
        let mut text = String::new();
        for i in 0..200_000u32 {
            text.push_str(&format!("<font color=#{i:06x}>a</font>"));
        }
        let mut b = CueBuilder::new();
        b.srt_text(&text);
        assert!(b.overflowed());
    });
}

#[test]
fn names_intern_of_many_distinct_strings_is_linear() {
    within("Names 300k distinct", move || {
        let mut names = Names::default();
        let first = names.intern("f0");
        for i in 0..300_000 {
            names.intern(&format!("f{i}"));
        }
        assert_eq!(names.intern("f0"), first);
        assert_eq!(names.get(first), "f0");
    });
}

#[test]
fn ssa_dialogue_of_many_font_names_is_refused_in_linear_time() {
    within("SSA 300k \\fn names", move || {
        let mut text = String::from("{");
        for i in 0..300_000 {
            text.push_str(&format!("\\fn{i}"));
        }
        text.push_str("}x");
        let d = SsaEvent { raw_text: &text, style: "Default", margin_l: 0, margin_r: 0, margin_v: 0 };
        let mut b = CueBuilder::new();
        dialogue_into(&d, &SsaStyles::default(), 0, &mut b);
        assert!(b.overflowed());
    });
}

#[test]
fn ssa_style_lookups_against_many_styles_are_linear() {
    within("SSA 4k styles x 100k \\r", move || {
        let mut header = String::from("[V4+ Styles]\nFormat: Name, Fontname\n");
        for i in 0..4096 {
            header.push_str(&format!("Style: s{i},Arial\n"));
        }
        let reg = SsaStyles::parse(&header);
        let text = format!("{{{}}}x", "\\rS4095\\rnone".repeat(100_000));
        let d = SsaEvent { raw_text: &text, style: "s1", margin_l: 0, margin_r: 0, margin_v: 0 };
        let mut b = CueBuilder::new();
        dialogue_into(&d, &reg, 0, &mut b);
        assert_eq!(CueIr::parse(&b.to_vec()).expect("valid").plain_text(), "x");
    });
}

#[test]
fn webvtt_style_rules_are_capped_and_the_cascade_stays_cheap() {
    within("WebVTT 100k ::cue rules x 2k cues", move || {
        let mut body = String::from("WEBVTT\n\nSTYLE\n");
        for i in 0..100_000 {
            body.push_str(&format!("::cue(.c{i}) {{ color: red }}\n"));
        }
        body.push('\n');
        for i in 0..2_000u32 {
            let (h, m) = (i / 60, i % 60);
            body.push_str(&format!("{h:02}:{m:02}:00.000 --> {h:02}:{m:02}:00.500\n<c.c1>x</c>\n\n"));
        }
        let mut parser = WebVtt::default();
        let cues = parser.parse(&body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 2_000);
        let sheet = parser.stylesheet().expect("a sheet");
        assert_eq!(sheet.rules().len(), MAX_RULES);
        let mut b = CueBuilder::new();
        for cue in &cues {
            b.cue(cue, OutputFormat::PangoMarkup, Some(sheet));
            assert!(!b.overflowed());
        }
    });
}
