// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! QTtext parser, a port of `qttextparse.c`. See `specs/qttext.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;
const MIN_TO_NSEC: u64 = 60 * GST_SECOND;
const HOUR_TO_NSEC: u64 = 60 * MIN_TO_NSEC;
const CLOCK_TIME_NONE: u64 = u64::MAX;

/// Parser for the QTtext subtitle format.
#[derive(Debug, Default)]
pub struct QtText {
    lines: LineScanner,
    ctx: Option<Ctx>,
}

impl SubtitleFormat for QtText {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let Self { lines, ctx } = self;
        let ctx = ctx.get_or_insert_with(Ctx::new);

        let mut out = Vec::new();
        let mut consumed =
            lines.feed(body, at_eos, |line| ctx.parse_line(line.as_bytes(), &mut out));

        if at_eos {
            let tail = &body[consumed..];
            let tail = tail.strip_suffix('\r').unwrap_or(tail);
            ctx.parse_line(tail.as_bytes(), &mut out);
            consumed = body.len();
        }

        Ok(Parsed { cues: out, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::PangoMarkup
    }
}

#[derive(Debug)]
struct Ctx {
    timescale: i32,
    absolute: bool,
    start_time: u64,

    markup_open: bool,
    need_markup: bool,

    font: Option<String>,
    font_size: i32,
    bg_color: Option<String>,
    fg_color: Option<String>,

    bold: bool,
    italic: bool,

    buf: Option<String>,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            timescale: 1000,
            absolute: true,
            start_time: 0,
            markup_open: false,
            need_markup: false,
            font: None,
            font_size: 12,
            bg_color: None,
            fg_color: None,
            bold: false,
            italic: false,
            buf: None,
        }
    }

    fn parse_line(&mut self, line: &[u8], out: &mut Vec<Cue>) {
        let mut i = 0usize;
        let mut relative = RelativeHint::default();
        while i < line.len() {
            match line[i] {
                b'{' => {
                    if !self.parse_tag(line, &mut i, &mut relative) {
                        break;
                    }
                }
                b'[' => {
                    let ts = parse_timestamp(line, i, self.timescale);
                    if self.buf.is_some() {
                        let text = self.get_text();
                        let start = self.start_time;
                        // A timestamp before the block's start underflows in the C to about 584
                        // years. Open-ended here.
                        let end = if self.absolute {
                            (ts >= start).then_some(ts)
                        } else {
                            Some(start.saturating_add(ts))
                        };
                        out.push(Cue::new(start, end, text));
                    }
                    self.buf = None;
                    if ts != 0 {
                        if self.absolute {
                            self.start_time = ts;
                        } else {
                            self.start_time = self.start_time.wrapping_add(ts);
                        }
                    }
                    break;
                }
                b' ' | b'\t' => {
                    i += 1;
                }
                _ => {
                    self.parse_text(line, i);
                    break;
                }
            }
        }
    }

    fn parse_tag(&mut self, line: &[u8], i: &mut usize, relative: &mut RelativeHint) -> bool {
        debug_assert_eq!(line[*i], b'{');
        let close = match line[*i..].iter().position(|&c| c == b'}') {
            Some(p) => *i + p,
            None => return false, // error_out
        };
        let next_index = close + 1;
        *i += 1; // skip '{'
        let idx = *i;
        let tag = &line[idx..];

        if tag.starts_with(b"QTtext") {
        } else if tag.starts_with(b"font") {
            if let Some(s) = read_str(line, idx + 4, close) {
                self.font = Some(s);
                self.need_markup = true;
            }
        } else if tag.starts_with(b"size") {
            let aux = read_int(&line[idx + 4..]);
            self.font_size = if aux == 0 { 12 } else { aux };
            self.need_markup = true;
        } else if tag.starts_with(b"textColor") {
            if let Some((r, g, b)) = read_color(line, idx + 9) {
                self.fg_color = Some(make_color(r, g, b));
            }
            self.need_markup = true;
        } else if tag.starts_with(b"backColor") {
            match read_color(line, idx + 9) {
                Some((r, g, b)) => self.bg_color = Some(make_color(r, g, b)),
                None => self.bg_color = None, // failure disables the background
            }
            self.need_markup = true;
        } else if tag.starts_with(b"plain") {
            self.bold = false;
            self.italic = false;
            self.need_markup = true;
        } else if tag.starts_with(b"bold") {
            self.bold = true;
            self.italic = false;
            self.need_markup = true;
        } else if tag.starts_with(b"italic") {
            self.bold = false;
            self.italic = true;
            self.need_markup = true;
        } else if tag.starts_with(b"timescale") {
            let aux = read_int(&line[idx + 9..]);
            self.timescale = if aux == 0 { 1000 } else { aux };
        } else if tag.starts_with(b"timestamps") {
            // Upstream `string_match` is true when "relative" is not found before the brace, so
            // anything not explicitly `relative` is treated as relative.
            self.absolute = !relative.string_match(line, idx + 10, next_index);
        }

        *i = next_index; // move past '}'
        true
    }

    fn parse_text(&mut self, line: &[u8], index: usize) {
        self.prepare_text();
        let text = String::from_utf8_lossy(&line[index..]);
        self.buf.as_mut().expect("prepare_text always sets buf").push_str(text.as_ref());
    }

    fn prepare_text(&mut self) {
        match self.buf.as_mut() {
            Some(b) => b.push('\n'),
            None => self.buf = Some(String::with_capacity(256)),
        }

        if self.need_markup {
            if self.markup_open {
                self.buf.as_mut().unwrap().push_str("</span>");
            }
            self.open_markup();
            self.markup_open = true;
            // One span per style change. The C reopens it on every line, which made a long
            // font name times the line count of one cue.
            self.need_markup = false;
        }
    }

    fn open_markup(&mut self) {
        let mut s = String::from("<span");
        match &self.font {
            Some(f) => s.push_str(&format!(" font='{} {}'", f, self.font_size)),
            None => s.push_str(&format!(" font='{}'", self.font_size)),
        }
        if let Some(bg) = &self.bg_color {
            s.push_str(&format!(" bgcolor='{bg}'"));
        }
        if let Some(fg) = &self.fg_color {
            s.push_str(&format!(" color='{fg}'"));
        }
        if self.bold {
            s.push_str(" weight='bold'");
        }
        if self.italic {
            s.push_str(" style='italic'");
        }
        s.push('>');
        self.buf.as_mut().unwrap().push_str(&s);
    }

    fn get_text(&mut self) -> String {
        let mut buf = self.buf.take().expect("get_text called with pending buf");
        if self.markup_open {
            buf.push_str("</span>");
        }
        self.markup_open = false;
        buf
    }
}

fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn c_atoi(bytes: &[u8]) -> i32 {
    let mut i = 0;
    while i < bytes.len() && is_c_space(bytes[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let mut val: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        val = val.wrapping_mul(10).wrapping_add((bytes[i] - b'0') as i64);
        i += 1;
    }
    let val = if neg { val.wrapping_neg() } else { val };
    val as i32
}

fn read_int(bytes: &[u8]) -> i32 {
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx] != b':' && bytes[idx] != b'}' {
        idx += 1;
    }
    if idx >= bytes.len() || bytes[idx] != b':' {
        return 0;
    }
    idx += 1;
    c_atoi(&bytes[idx..])
}

fn read_str(line: &[u8], start: usize, end: usize) -> Option<String> {
    let mut idx = start;
    while idx < line.len() && line[idx] != b':' && line[idx] != b'}' {
        idx += 1;
    }
    if idx >= line.len() || line[idx] != b':' {
        return None;
    }
    idx += 1;
    while idx < line.len() && line[idx] == b' ' {
        idx += 1;
    }
    let stop = end.min(line.len()).max(idx);
    Some(String::from_utf8_lossy(&line[idx..stop]).into_owned())
}

fn read_color(line: &[u8], start: usize) -> Option<(i32, i32, i32)> {
    let mut idx = start;
    while idx < line.len() && line[idx] != b':' && line[idx] != b'}' {
        idx += 1;
    }
    if idx >= line.len() || line[idx] != b':' {
        return None;
    }
    idx += 1;
    let r = c_atoi(&line[idx..]);

    while idx < line.len() && line[idx] != b'}' && line[idx] != b',' {
        idx += 1;
    }
    if idx >= line.len() || line[idx] != b',' {
        return None;
    }
    idx += 1;
    let g = c_atoi(&line[idx..]);

    while idx < line.len() && line[idx] != b'}' && line[idx] != b',' {
        idx += 1;
    }
    if idx >= line.len() || line[idx] != b',' {
        return None;
    }
    idx += 1;
    let b = c_atoi(&line[idx..]);

    Some((r, g, b))
}

fn make_color(r: i32, g: i32, b: i32) -> String {
    let r = r / 256;
    let g = g / 256;
    let b = b / 256;
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Where the next `relative` sits at or past a line offset, searched forward only, so a line
/// of many `{timestamps}` tags scans itself once.
#[derive(Debug, Default)]
struct RelativeHint {
    from: usize,
    found: Option<Option<usize>>,
}

impl RelativeHint {
    /// `string_match`. A NULL `strstr` sorts below `upto` upstream, so not found yields `true`.
    fn string_match(&mut self, line: &[u8], start: usize, upto: usize) -> bool {
        let start = start.min(line.len());
        let hit = match self.found {
            Some(None) if start >= self.from => None,
            Some(Some(at)) if start >= self.from && at >= start => Some(at),
            _ => {
                let hit = find_subslice(&line[start..], b"relative").map(|pos| start + pos);
                self.from = start;
                self.found = Some(hit);
                hit
            }
        };
        hit.is_none_or(|at| at < upto)
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn uint64_scale(val: u64, num: u64, denom: u64) -> u64 {
    if denom == 0 {
        return 0;
    }
    ((val as u128 * num as u128) / denom as u128) as u64
}

fn scan_u32(line: &[u8], i: &mut usize) -> Option<u32> {
    while *i < line.len() && is_c_space(line[*i]) {
        *i += 1;
    }
    let mut neg = false;
    if *i < line.len() && (line[*i] == b'+' || line[*i] == b'-') {
        neg = line[*i] == b'-';
        *i += 1;
    }
    let dstart = *i;
    let mut val: u64 = 0;
    while *i < line.len() && line[*i].is_ascii_digit() {
        val = val.wrapping_mul(10).wrapping_add((line[*i] - b'0') as u64);
        *i += 1;
    }
    if *i == dstart {
        return None;
    }
    let v = val as u32;
    Some(if neg { v.wrapping_neg() } else { v })
}

/// `qttext_parse_timestamp`. A malformed timestamp is `0` like upstream.
fn parse_timestamp(line: &[u8], start: usize, timescale: i32) -> u64 {
    let mut i = start;
    if i >= line.len() || line[i] != b'[' {
        return 0;
    }
    i += 1;

    let hour = match scan_u32(line, &mut i) {
        Some(v) => v,
        None => return 0,
    };
    if i >= line.len() || line[i] != b':' {
        return 0;
    }
    i += 1;

    let min = match scan_u32(line, &mut i) {
        Some(v) => v,
        None => return 0,
    };
    if i >= line.len() || line[i] != b':' {
        return 0;
    }
    i += 1;

    let sec = match scan_u32(line, &mut i) {
        Some(v) => v,
        None => return 0,
    };

    let mut dec: u32 = 0;
    if i < line.len() && line[i] == b'.' {
        i += 1;
        if let Some(d) = scan_u32(line, &mut i) {
            dec = d;
        }
    }

    let mut timestamp = uint64_scale(dec as u64, GST_SECOND, timescale as u64);
    timestamp = timestamp.wrapping_add((sec as u64).wrapping_mul(GST_SECOND));
    match (min as u64).checked_mul(MIN_TO_NSEC) {
        Some(t) => timestamp = timestamp.wrapping_add(t),
        None => return CLOCK_TIME_NONE,
    }
    match (hour as u64).checked_mul(HOUR_TO_NSEC) {
        Some(t) => timestamp = timestamp.wrapping_add(t),
        None => return CLOCK_TIME_NONE,
    }
    timestamp
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = GST_SECOND;

    fn parse(body: &str) -> Vec<Cue> {
        QtText::default().parse(body, &ParseContext::default()).expect("qttext parse never errors")
    }

    #[test]
    fn emits_pango_markup() {
        assert_eq!(QtText::default().output_format(), OutputFormat::PangoMarkup);
    }

    #[test]
    fn basic_absolute_plain() {
        let cues = parse("{QTtext}\n[00:00:01.00]\nHello world\n[00:00:03.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(3 * S));
        assert_eq!(cues[0].text, "Hello world");
    }

    #[test]
    fn two_absolute_cues() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nfirst\n[00:00:02.00]\nsecond\n[00:00:05.00]\n");
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (0, Some(2 * S)));
        assert_eq!(cues[0].text, "first");
        assert_eq!((cues[1].start_ns, cues[1].end_ns), (2 * S, Some(5 * S)));
        assert_eq!(cues[1].text, "second");
    }

    #[test]
    fn multiline_plain_joined_with_newline() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nA\nB\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "A\nB");
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn leading_whitespace_is_stripped() {
        let cues = parse("{QTtext}\n[00:00:00.00]\n   Spaced\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Spaced");
    }

    #[test]
    fn last_block_without_trailing_timestamp_is_dropped() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nOnly line\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn malformed_timestamp_after_text_yields_an_open_ended_cue() {
        let cues = parse("{QTtext}\n[00:00:01.00]\nText\n[bad]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, None);
        assert_eq!(cues[0].text, "Text");

        let cues = parse("{QTtext}\n[00:00:05.00]\nText\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 5 * S);
        assert_eq!(cues[0].end_ns, None);
    }

    #[test]
    fn empty_and_header_only_produce_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("{QTtext}\n").is_empty());
        assert!(parse("{QTtext}{font:Sans}{size:18}\n").is_empty());
    }

    #[test]
    fn default_timescale_decimal() {
        let cues = parse("{QTtext}\n[00:00:00.500]\nHalf\n[00:00:01.000]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 500_000_000);
        assert_eq!(cues[0].end_ns, Some(1_000_000_000));
        assert_eq!(cues[0].text, "Half");
    }

    #[test]
    fn custom_timescale() {
        let cues = parse("{QTtext}{timescale:600}\n[00:00:00.300]\nTick\n[00:00:01.300]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 500_000_000);
        assert_eq!(cues[0].end_ns, Some(1_500_000_000));
        assert_eq!(cues[0].text, "Tick");
    }

    #[test]
    fn invalid_timescale_and_size_reset_to_defaults() {
        let cues = parse("{QTtext}{timescale:0}{size:0}\n[00:00:00.00]\nX\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].end_ns, Some(S));
        assert_eq!(cues[0].text, "<span font='12'>X</span>");
    }

    #[test]
    fn missing_decimal_is_forgiven() {
        let cues = parse("{QTtext}\n[00:00:01]\nNoDec\n[00:00:02]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn hours_and_minutes() {
        let cues = parse("{QTtext}\n[01:02:03.00]\nT\n[01:02:04.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 3600 * S + 2 * 60 * S + 3 * S);
        assert_eq!(cues[0].end_ns, Some(3600 * S + 2 * 60 * S + 4 * S));
    }

    #[test]
    fn relative_timestamps_are_durations() {
        let cues = parse(
            "{QTtext}{timestamps:relative}\n[00:00:01.00]\nFirst\n[00:00:02.00]\nSecond\n[00:00:03.00]\n",
        );
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (S, Some(3 * S)));
        assert_eq!(cues[0].text, "First");
        assert_eq!((cues[1].start_ns, cues[1].end_ns), (3 * S, Some(6 * S)));
        assert_eq!(cues[1].text, "Second");
    }

    #[test]
    fn timestamps_absolute_is_parsed_as_relative_quirk() {
        // The `string_match` quirk reads `{timestamps:absolute}` as relative.
        let cues = parse("{QTtext}{timestamps:absolute}\n[00:00:01.00]\nA\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(3 * S));
    }

    #[test]
    fn single_line_font_and_size_markup() {
        let cues = parse("{QTtext}{font:Sans}{size:18}\n[00:00:00.00]\nStyled\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span font='Sans 18'>Styled</span>");
    }

    #[test]
    fn multiline_markup_opens_one_span_per_style_change() {
        let cues = parse("{QTtext}{size:12}\n[00:00:00.00]\nA\nB\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span font='12'>A\nB</span>");
    }

    #[test]
    fn a_long_font_name_is_written_once_however_many_lines_follow() {
        let name = "A".repeat(100_000);
        let mut doc = format!("{{QTtext}}{{font:{name}}}\n[00:00:01.00]\n");
        for _ in 0..20_000 {
            doc.push_str("x\n");
        }
        doc.push_str("[00:00:02.00]\n");
        let cues = parse(&doc);
        assert_eq!(cues.len(), 1);
        assert!(cues[0].text.len() < name.len() + 20_000 * 2 + 64, "{}", cues[0].text.len());
    }

    #[test]
    fn a_line_of_many_timestamps_tags_is_scanned_once() {
        let line = "{timestamps:absolute}".repeat(200_000);
        let doc = format!("{{QTtext}}{line}\n[00:00:01.00]\nx\n[00:00:02.00]\n");
        let cues = parse(&doc);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "x");
    }

    #[test]
    fn colors_are_scaled_and_ordered() {
        let cues = parse(
            "{QTtext}{textColor:65535,0,0}{backColor:0,0,65535}\n[00:00:00.00]\nRed on blue\n[00:00:01.00]\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(
            cues[0].text,
            "<span font='12' bgcolor='#0000FF' color='#FF0000'>Red on blue</span>"
        );
    }

    #[test]
    fn full_attribute_order() {
        let cues = parse(
            "{QTtext}{font:Arial}{size:20}{textColor:65535,65535,0}{backColor:0,0,0}{bold}\n[00:00:00.00]\nAll\n[00:00:01.00]\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(
            cues[0].text,
            "<span font='Arial 20' bgcolor='#000000' color='#FFFF00' weight='bold'>All</span>"
        );
    }

    #[test]
    fn style_transitions_bold_italic_plain() {
        let cues = parse(
            "{QTtext}\n[00:00:00.00]\n{bold}Bold line\n{italic}Italic line\n{plain}Plain line\n[00:00:03.00]\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(
            cues[0].text,
            "<span font='12' weight='bold'>Bold line\n\
             </span><span font='12' style='italic'>Italic line\n\
             </span><span font='12'>Plain line</span>"
        );
    }

    #[test]
    fn back_color_failure_clears_background() {
        let cues = parse(
            "{QTtext}{backColor:255,255,255}{backColor:bad}\n[00:00:00.00]\nHi\n[00:00:01.00]\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span font='12'>Hi</span>");
    }

    #[test]
    fn unknown_tag_is_ignored() {
        let cues = parse("{QTtext}{wibble:42}\n[00:00:00.00]\nHi\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
    }

    #[test]
    fn malformed_tag_line_is_skipped_but_text_survives() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nHello\n{bad tag no close\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello");
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn bad_timestamp_still_flushes_pending_text() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nText\n[bad]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 0);
        assert_eq!(cues[0].end_ns, Some(0));
        assert_eq!(cues[0].text, "Text");
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let cues = parse("{QTtext}\r\n[00:00:01.00]\r\nCRLF\r\n[00:00:02.00]\r\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "CRLF");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn text_is_not_escaped() {
        let cues = parse("{QTtext}\n[00:00:00.00]\na < b & c\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "a < b & c");
    }

    #[test]
    fn utf8_text_is_preserved() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nПривет мир 🎬\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Привет мир 🎬");
    }

    #[test]
    fn make_color_divides_by_256() {
        assert_eq!(make_color(65535, 0, 0), "#FF0000");
        assert_eq!(make_color(0, 65535, 0), "#00FF00");
        assert_eq!(make_color(0, 0, 65535), "#0000FF");
        assert_eq!(make_color(256, 512, 768), "#010203");
    }

    #[test]
    fn timestamp_parsing_matches_reference() {
        assert_eq!(parse_timestamp(b"[00:00:00.00]", 0, 1000), 0);
        assert_eq!(parse_timestamp(b"[00:00:01.00]", 0, 1000), S);
        assert_eq!(parse_timestamp(b"[00:00:00.500]", 0, 1000), 500_000_000);
        assert_eq!(parse_timestamp(b"[00:01:00.00]", 0, 1000), 60 * S);
        assert_eq!(parse_timestamp(b"[garbage]", 0, 1000), 0);
        assert_eq!(parse_timestamp(b"[00:00]", 0, 1000), 0); // too few fields
    }
}
