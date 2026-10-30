//! Golden snapshots of the WPT WebVTT corpus, each file pushed through the `subparse` element as
//! one document and its `subtitle/cue-ir` output read back as a `CueIr` view. The goldens come from
//! gst-subparse-rs with its pango lines removed.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo nextest run -p pf-subparse --test wptvtt_golden`, then
//! review the diff.

// Test code, fixtures are read from disk and snapshots live on the heap.
#![allow(clippy::disallowed_methods)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pf_harness::Harness;
use pf_subparse::SubParse;
use pf_subparse::ir::{
    Color, CueIr, FontSize, FontStyle, LayoutRec, LineAlign, LinePosition, PositionAlign,
    RubyPosition, SpanRec, StyleRec, TextAlign, WritingMode,
};
use profluens_core::time::Timestamp;

const WPTVTT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/wptvtt");
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/wptvtt");

fn run_file(path: &Path) -> Vec<(Timestamp, Timestamp, Vec<u8>)> {
    let bytes = std::fs::read(path).expect("a corpus file");
    let mut h = Harness::new(SubParse::new());
    h.fix_sink_format(SubParse::IN, "bytes", &[]);
    let buf = h.alloc(&bytes);
    h.push(buf).expect("push");
    let out = h.eos().expect("eos");
    out.iter()
        .map(|b| {
            CueIr::parse(b.memory.data()).expect("a valid cue-ir payload");
            (b.pts, b.duration, b.memory.data().to_vec())
        })
        .collect()
}

fn fmt_time(t: Timestamp) -> String {
    match t.nanos() {
        Some(ns) => format!("{}.{:03}", ns / 1_000_000_000, (ns / 1_000_000) % 1000),
        None => "none".to_owned(),
    }
}

fn fmt_color(c: Color) -> String {
    if c.a == 0xff {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
    }
}

fn fmt_size(s: FontSize) -> String {
    match s {
        FontSize::Points(p) => format!("{p}pt"),
        FontSize::Scale(f) => format!("scale({f})"),
        FontSize::FrameHeightPercent(p) => format!("{p}%fh"),
    }
}

fn style_parts(ir: &CueIr<'_>, s: &StyleRec) -> Vec<String> {
    let mut p = Vec::new();
    if let Some(f) = ir.style_family(s) {
        p.push(format!("family={f:?}"));
    }
    if let Some(v) = s.font_size() {
        p.push(format!("size={}", fmt_size(v)));
    }
    if let Some(v) = s.font_style() {
        let v = match v {
            FontStyle::Normal => "normal",
            FontStyle::Italic => "italic",
            FontStyle::Oblique => "oblique",
        };
        p.push(format!("style={v}"));
    }
    if let Some(v) = s.font_weight() {
        p.push(format!("weight={v}"));
    }
    if let Some(v) = s.underline() {
        p.push(format!("underline={v}"));
    }
    if let Some(v) = s.strikethrough() {
        p.push(format!("strikethrough={v}"));
    }
    if let Some(v) = s.foreground() {
        p.push(format!("fg={}", fmt_color(v)));
    }
    if let Some(v) = s.background() {
        p.push(format!("bg={}", fmt_color(v)));
    }
    if let Some(o) = s.outline() {
        p.push(format!("outline={}/{}", fmt_color(o.color), o.width));
    }
    if let Some(sh) = s.shadow() {
        p.push(format!("shadow={}/{},{},{}", fmt_color(sh.color), sh.dx, sh.dy, sh.blur));
    }
    if let Some(v) = s.letter_spacing() {
        p.push(format!("letter-spacing={v}"));
    }
    if let Some(v) = s.baseline_shift() {
        p.push(format!("baseline-shift={v}"));
    }
    if let Some((x, y)) = s.scale() {
        p.push(format!("glyph-scale={x},{y}"));
    }
    if let Some(l) = ir.style_language(s) {
        p.push(format!("lang={l}"));
    }
    p
}

fn layout_parts(l: &LayoutRec) -> Vec<String> {
    let mut p = Vec::new();
    match l.writing_mode() {
        WritingMode::HorizontalTb => {}
        WritingMode::VerticalRl => p.push("vertical-rl".to_owned()),
        WritingMode::VerticalLr => p.push("vertical-lr".to_owned()),
    }
    match l.line() {
        Some(LinePosition::Percent(pc)) => p.push(format!("line={pc}%")),
        Some(LinePosition::Line(n)) => p.push(format!("line={n}")),
        None => {}
    }
    if let Some(v) = l.line_align() {
        let v = match v {
            LineAlign::Start => "start",
            LineAlign::Center => "center",
            LineAlign::End => "end",
        };
        p.push(format!("line-align={v}"));
    }
    if let Some(v) = l.position() {
        p.push(format!("position={v}"));
    }
    if let Some(v) = l.position_align() {
        let v = match v {
            PositionAlign::Auto => "auto",
            PositionAlign::LineLeft => "line-left",
            PositionAlign::Center => "center",
            PositionAlign::LineRight => "line-right",
        };
        p.push(format!("position-align={v}"));
    }
    if let Some(v) = l.size() {
        p.push(format!("size={v}"));
    }
    if let Some(v) = l.align() {
        let v = match v {
            TextAlign::Start => "start",
            TextAlign::Center => "center",
            TextAlign::End => "end",
            TextAlign::Left => "left",
            TextAlign::Right => "right",
        };
        p.push(format!("align={v}"));
    }
    if let Some(v) = l.anchor() {
        p.push(format!("anchor={v:?}"));
    }
    if let Some((x, y)) = l.origin() {
        p.push(format!("origin={x},{y}"));
    }
    if let Some(m) = l.margins() {
        p.push(format!("margins={},{},{}", m.left, m.right, m.vertical));
    }
    if let Some(c) = l.background() {
        p.push(format!("bg={}", fmt_color(c)));
    }
    p
}

fn push_span(out: &mut String, ir: &CueIr<'_>, span: &SpanRec) {
    write!(out, "    span {:?}", ir.span_text(span)).unwrap();
    if let Some(v) = ir.span_voice(span) {
        write!(out, " voice={v:?}").unwrap();
    }
    if ir.span_class_count(span) > 0 {
        write!(out, " classes={}", ir.span_classes(span).collect::<Vec<_>>().join(".")).unwrap();
    }
    if let Some(r) = ir.span_ruby(span) {
        let pos = match r.position() {
            RubyPosition::Over => "over",
            RubyPosition::Under => "under",
        };
        write!(out, " ruby={:?}/{pos}", ir.ruby_text(r)).unwrap();
    }
    if let Some(ns) = span.reveal_ns() {
        write!(out, " reveal={ns}ns").unwrap();
    }
    let style = style_parts(ir, ir.span_style(span));
    if !style.is_empty() {
        write!(out, " [{}]", style.join(" ")).unwrap();
    }
    out.push('\n');
}

fn render_ir(out: &mut String, ir: &CueIr<'_>) {
    let layout = layout_parts(ir.layout());
    if !layout.is_empty() {
        writeln!(out, "  layout: {}", layout.join(" ")).unwrap();
    }
    let base = style_parts(ir, ir.base_style());
    if !base.is_empty() {
        writeln!(out, "  base: {}", base.join(" ")).unwrap();
    }
    for line in ir.lines() {
        writeln!(out, "  line:").unwrap();
        for span in ir.line_spans(line) {
            push_span(out, ir, span);
        }
    }
}

fn snapshot(path: &Path) -> String {
    let cues = run_file(path);
    if cues.is_empty() {
        return "(no cues)\n".to_owned();
    }
    let mut out = String::new();
    for (i, (pts, duration, bytes)) in cues.iter().enumerate() {
        let ir = CueIr::parse(bytes).expect("validated at capture");
        writeln!(out, "cue {i}: {} +{}", fmt_time(*pts), fmt_time(*duration)).unwrap();
        writeln!(out, "  text: {:?}", ir.plain_text()).unwrap();
        render_ir(&mut out, &ir);
    }
    out
}

fn corpus() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(WPTVTT)
        .expect("the corpus directory")
        .map(|e| e.expect("an entry").path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "vtt" || e == "webvtt"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 91, "the WPT corpus under {WPTVTT}");
    files
}

#[test]
fn wptvtt_matches_goldens() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut failures = Vec::new();
    for path in &corpus() {
        let got = snapshot(path);
        let name = path.file_name().expect("a file name").to_string_lossy();
        let golden = Path::new(GOLDEN).join(format!("{name}.txt"));
        if update {
            std::fs::write(&golden, &got).expect("write a golden");
            continue;
        }
        match std::fs::read_to_string(&golden) {
            Ok(want) if want == got => {}
            Ok(want) => failures.push(format!("{name}\n--- golden\n{want}--- current\n{got}")),
            Err(_) => failures.push(format!("{name}: no golden, run with UPDATE_GOLDEN=1")),
        }
    }
    assert!(failures.is_empty(), "{} golden mismatches\n\n{}", failures.len(), failures.join("\n"));
}
