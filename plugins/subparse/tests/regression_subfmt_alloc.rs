// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Allocation counts of the cue builder, linear growth on a cold one and none on a warm one.

// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pf_subparse::formats::WebVtt;
use pf_subparse::formats::ssa::framed_dialogue;
use pf_subparse::formats::webvtt::{
    MarkupScratch, cue_text_to_pango_markup, cue_text_to_pango_markup_into,
};
use pf_subparse::ir::{CueBuilder, CueIr, MAX_MARKUP_DEPTH};
use pf_subparse::ssastyle::{SsaStyles, dialogue_into};
use pf_subparse::vttcss::Stylesheet;
use pf_subparse::{CUE_SLOT, OutputFormat, ParseContext, SubtitleFormat};

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.with(Cell::get) {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        if ARMED.with(Cell::get) {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs(f: impl FnOnce()) -> u64 {
    ALLOCS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    ALLOCS.with(Cell::get)
}

#[test]
fn webvtt_nested_class_tags_allocate_linearly() {
    const DEPTH: usize = 1000;
    let body = format!("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n{}a\n\n", "<c.x>".repeat(DEPTH));
    let mut parser = WebVtt::default();
    let cues = parser.parse(&body, &ParseContext::default()).unwrap();
    assert_eq!(cues.len(), 1);
    let mut classes = 0;
    let mut b = CueBuilder::new();
    let n = allocs(|| {
        b.cue(&cues[0], OutputFormat::PangoMarkup, parser.stylesheet());
        let bytes = b.to_vec();
        let ir = CueIr::parse(&bytes).expect("valid");
        classes = ir.spans().iter().map(|s| ir.span_class_count(s)).sum();
    });
    let kept = DEPTH.min(MAX_MARKUP_DEPTH);
    assert_eq!(classes, kept, "the one span inherits every class up to the depth cap");
    let bound = 64 * DEPTH as u64;
    assert!(n < bound, "{n} allocations for {DEPTH} nested tags, bound {bound}");
}

const CUES: [&str; 4] = [
    "<v Fred><c.loud>Hi</c> <i>there</i></v>\n<ruby>base<rt>rt</rt></ruby> &lt;00:00:01.000&gt;late",
    "<font color=\"#ff0000\" face=\"Comic Sans\">red</font> and {\\an8}<b>bold</b>",
    "plain\n\nlines",
    "<span font_family=\"Arial\" size=\"large\" lang=\"en\">a</span><tt>b</tt><sub>c</sub>",
];

const ROWS: [&str; 2] = [
    "0,0,Default,,0,0,0,,{\\i1}it{\\i0}no{\\b1\\u1\\fnArial\\c&H0000FF&}bu{\\r}plain\\Nnext",
    "0,0,Default,,10,10,20,,{\\k50}la{\\k100}lala{\\kf25}end{\\an8\\pos(10,20)}",
];

fn every_cue(b: &mut CueBuilder, sheet: &Stylesheet, slot: &mut [u8]) -> usize {
    let mut read = 0;
    let mut touch = |b: &mut CueBuilder, slot: &mut [u8]| {
        let n = b.finish(slot).expect("fits");
        let ir = CueIr::parse(&slot[..n]).expect("valid");
        for i in 0..ir.line_count() {
            read += ir.line_text(i).map(str::len).sum::<usize>();
        }
    };
    for text in CUES {
        b.pango_markup(text, Some(sheet), Some("id"));
        touch(b, slot);
        b.srt_text(text);
        touch(b, slot);
        b.plain_text(text);
        touch(b, slot);
    }
    for row in ROWS {
        let d = framed_dialogue(row).expect("a row");
        dialogue_into(&d, &SsaStyles::default(), 1_000_000_000, b);
        touch(b, slot);
    }
    read
}

/// The Block path of `subparse`, parse to a validated view, allocates nothing once the builder's
/// scratch has grown to the cue shapes it sees.
#[test]
fn a_warm_builder_allocates_nothing_per_cue() {
    let sheet = Stylesheet::parse(
        "::cue(.loud) { color: red; font-family: Verdana } ::cue { font: italic 12pt Arial }",
    );
    let mut b = CueBuilder::new();
    let mut slot = vec![0u8; CUE_SLOT];
    for _ in 0..2 {
        every_cue(&mut b, &sheet, &mut slot);
    }
    let mut read = 0;
    let n = allocs(|| {
        read = every_cue(&mut b, &sheet, &mut slot);
    });
    assert!(read > 0);
    assert_eq!(n, 0, "a warm builder allocated {n} times");
}

const VTT_BLOCKS: [&str; 6] = [
    "<v Fred><c.loud>Hi</c> <i>there</i></v>\n<ruby>base<rt>rt</rt></ruby>",
    "<b><i>unclosed\n\n\n",
    "stray</u> closer</b> and <font color=red>gone</font> & \"quoted\" 'x'",
    "ctrl \u{1}\u{1f}\u{85}\u{9f} &lt;b&gt; <00:00:01.000>late",
    "<c.a.b.c><v.loud Jean-Luc>nested</v></c><unknown>x</unknown>",
    "plain",
];

/// The VTT Block path of `subparse`, markup pipeline to builder to a validated view, allocates
/// nothing once its scratch is warm, and says what the cold pipeline says.
#[test]
fn a_warm_vtt_block_allocates_nothing() {
    let mut scratch = MarkupScratch::default();
    let mut b = CueBuilder::new();
    let mut slot = vec![0u8; CUE_SLOT];
    let run = |scratch: &mut MarkupScratch, b: &mut CueBuilder, slot: &mut [u8]| {
        let mut read = 0;
        for body in VTT_BLOCKS {
            b.pango_markup(cue_text_to_pango_markup_into(body, scratch), None, None);
            let n = b.finish(slot).expect("fits");
            let ir = CueIr::parse(&slot[..n]).expect("valid");
            for i in 0..ir.line_count() {
                read += ir.line_text(i).map(str::len).sum::<usize>();
            }
        }
        read
    };
    for body in VTT_BLOCKS {
        assert_eq!(
            cue_text_to_pango_markup_into(body, &mut scratch),
            cue_text_to_pango_markup(body)
        );
    }
    for _ in 0..2 {
        run(&mut scratch, &mut b, &mut slot);
    }
    let mut read = 0;
    let n = allocs(|| read = run(&mut scratch, &mut b, &mut slot));
    assert!(read > 0);
    assert_eq!(n, 0, "a warm VTT block allocated {n} times");
}
