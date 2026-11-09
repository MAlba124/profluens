// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! WebVTT parser, a port of the C subparse `parse_webvtt`. See `specs/webvtt.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, CueSettings, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};
use crate::vttcss::Stylesheet;

const GST_SECOND: u64 = 1_000_000_000;
const GST_MSECOND: u64 = 1_000_000;

/// Inline tags the C keeps (`allowed_vtt_tags`). Anything else is stripped.
const ALLOWED_VTT_TAGS: &[&str] = &["i", "b", "c", "u", "v", "ruby", "rt"];

/// Parser for the WebVTT subtitle format.
#[derive(Debug, Default)]
pub struct WebVtt {
    lines: LineScanner,
    machine: Machine,
}

impl SubtitleFormat for WebVtt {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let Self { lines, machine } = self;

        let mut cues = Vec::new();
        let mut consumed = lines.feed(body, |line| machine.feed(line, &mut cues));

        if at_eos {
            let tail = &body[consumed..];
            let tail = tail.strip_suffix('\r').unwrap_or(tail);
            machine.feed(tail, &mut cues);
            machine.feed("", &mut cues);
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::PangoMarkup
    }

    fn stylesheet(&self) -> Option<&Stylesheet> {
        (!self.machine.sheet.is_empty()).then_some(&self.machine.sheet)
    }
}

/// The C's `ParserState.state`. `CollectStyle` is ours, the C ignores non-timing lines while
/// seeking.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum St {
    #[default]
    SeekTiming,
    CollectStyle,
    CollectText,
}

#[derive(Debug, Default)]
struct Machine {
    state: St,
    /// `ParserState.start_time`, the previous end. Timing lines need `start_time <= ts_end`, a
    /// weak monotonicity guard.
    prev_end: u64,
    cur_start: u64,
    /// The end as parsed, what the guard compares against. May precede `cur_start`.
    cur_end: u64,
    cur_settings: CueSettings,
    cur_id: Option<String>,
    pending_id: Option<String>,
    /// `STYLE` blocks are only valid before the first cue (WPT
    /// `embedded_style_invalid_format.vtt`).
    saw_cue: bool,
    style_buf: String,
    sheet: Stylesheet,
    buf: String,
    markup: MarkupScratch,
}

impl Machine {
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        match self.state {
            St::SeekTiming => self.feed_seek(line),
            St::CollectStyle => {
                if line.is_empty() {
                    self.end_style_block();
                } else if line.contains("-->") {
                    // A timing line ends an unfinished style block and starts its cue, as the
                    // spec's collector does.
                    self.end_style_block();
                    self.feed_seek(line);
                } else {
                    self.style_buf.push_str(line);
                    self.style_buf.push('\n');
                }
            }
            St::CollectText => {
                if line.contains("-->") {
                    // A later `-->` line ends the block and starts the next, as the spec's
                    // collector rewinds. The one deliberate break from the C,
                    // which would show the timestamps as cue text.
                    self.finish_cue(cues);
                    self.feed_seek(line);
                } else {
                    if !self.buf.is_empty() {
                        self.buf.push('\n');
                    }
                    self.buf.push_str(line);
                    if line.is_empty() {
                        self.finish_cue(cues);
                    }
                }
            }
        }
    }

    fn finish_cue(&mut self, cues: &mut Vec<Cue>) {
        let text = finalize_into(&self.buf, &mut self.markup).to_string();
        // A reversed time line passes the guard. The guard gets the parsed end, only the cue is
        // clamped.
        let mut cue = Cue::new(self.cur_start, Some(self.cur_end.max(self.cur_start)), text);
        cue.settings = std::mem::take(&mut self.cur_settings);
        cue.id = self.cur_id.take();
        cues.push(cue);
        self.prev_end = self.cur_end; // element: start_time += duration
        self.buf.clear();
        self.state = St::SeekTiming;
    }

    fn feed_seek(&mut self, line: &str) {
        if let Some((ts_start, ts_end, settings)) = parse_timing_line(line)
            && self.prev_end <= ts_end
        {
            self.cur_start = ts_start;
            self.cur_end = ts_end;
            self.cur_settings = match settings {
                Some(s) => parse_cue_settings(s),
                None => CueSettings::default(),
            };
            self.cur_id = self.pending_id.take();
            self.saw_cue = true;
            self.buf.clear();
            self.state = St::CollectText;
        } else if !self.saw_cue && line == "STYLE" {
            // A line exactly "STYLE" (case-sensitive) before the first cue. The C ignores it.
            self.style_buf.clear();
            self.pending_id = None;
            self.state = St::CollectStyle;
        } else {
            // Ignored while seeking like the C, remembered as the candidate cue id.
            self.pending_id =
                if line.is_empty() || line.contains("-->") { None } else { Some(line.to_owned()) };
        }
    }

    fn end_style_block(&mut self) {
        if !self.style_buf.is_empty() {
            self.sheet.push_css(&self.style_buf);
            self.style_buf.clear();
        }
        self.state = St::SeekTiming;
    }
}

/// `start --> end [settings]`. Settings are past the first space after the end, as in the C.
fn parse_timing_line(line: &str) -> Option<(u64, u64, Option<&str>)> {
    let idx = line.find(" --> ")?;
    // parse_subrip_time truncates the start at "-->" itself, like the C.
    let after = &line[idx + 5..];
    let ts_start = parse_subrip_time(line)?;
    let ts_end = parse_subrip_time(after)?;
    let settings = after.find(' ').map(|sp| &after[sp + 1..]);
    Some((ts_start, ts_end, settings))
}

/// Port of the C `parse_subrip_time`, shared by SRT/VTT.
fn parse_subrip_time(ts_string: &str) -> Option<u64> {
    let trimmed = ts_string.trim_start_matches(' ');

    // g_strlcpy into a 128-byte buffer, keeping at most 127 bytes (char-safe).
    let mut end = trimmed.len().min(127);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = trimmed[..end].to_string();

    if let Some(pos) = s.find("-->") {
        s.truncate(pos);
    }

    let s = s.trim_end_matches(|c: char| c.is_ascii_whitespace());

    // g_strdelimit turns ' ' into '0' and '.' into ','.
    let mut s: String = s
        .chars()
        .map(|c| match c {
            ' ' => '0',
            '.' => ',',
            other => other,
        })
        .collect();

    let comma = s.find(',')?;

    // Guard against absurdly long hour fields, `(p - s) > sizeof("hhh:mm:ss,")` in the C.
    if comma > 11 {
        return None;
    }

    let frac: String = {
        let f = &s[comma + 1..];
        if f.len() > 3 {
            f.chars().take(3).collect()
        } else {
            let mut f = f.to_string();
            while f.len() < 3 {
                f.push('0');
            }
            f
        }
    };
    s.truncate(comma); // keep the "HH:MM:SS" (or "MM:SS") part

    let (hour, min, sec) = parse_hms(&s)?;
    // The fraction is the last conversion, so junk after its digits is not read. Cue settings
    // munged in by the ' ' -> '0' step then cannot break a short fraction.
    let (msec, _) = scan_u(&frac)?;

    // The C's `guint` fields wrap the seconds sum at 32 bits. A far-future cue would otherwise
    // suppress every later one via the monotonic guard.
    let secs = hour.wrapping_mul(3600).wrapping_add(min.wrapping_mul(60)).wrapping_add(sec);
    Some(u64::from(secs) * GST_SECOND + u64::from(msec) * GST_MSECOND)
}

/// sscanf `"%u:%u:%u"` (three fields) falling back to `"%u:%u"` (hours = 0).
fn parse_hms(s: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [h, m, sec] => Some((scan_u_field(h)?, scan_u_field(m)?, scan_u_field(sec)?)),
        [m, sec] => Some((0, scan_u_field(m)?, scan_u_field(sec)?)),
        _ => None,
    }
}

/// One `sscanf` `%u`. Overflow to `ULONG_MAX` and the `guint` truncation are reproduced.
fn scan_u(s: &str) -> Option<(u32, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_scanf_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if matches!(b.get(i), Some(b'+' | b'-')) {
        neg = b[i] == b'-';
        i += 1;
    }
    let digits = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits {
        return None;
    }
    let mag = s[digits..i].parse::<u64>().unwrap_or(u64::MAX);
    let val = if neg { mag.wrapping_neg() } else { mag };
    Some((val as u32, &s[i..]))
}

/// A `%u` followed by a format literal. `00:00:01x,000` fails like in the C.
fn scan_u_field(s: &str) -> Option<u32> {
    let (val, rest) = scan_u(s)?;
    rest.is_empty().then_some(val)
}

/// C `isspace`. Spaces are already `0`s here, tabs and the rest survive.
fn is_scanf_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Port of `parse_webvtt_cue_settings` plus the modern `name:value` syntax the C does not know.
pub(crate) fn parse_cue_settings(settings: &str) -> CueSettings {
    let mut cs = CueSettings::default();

    for tok in settings.split([' ', '\t']) {
        let Some(&first) = tok.as_bytes().first() else {
            continue; // empty token (consecutive separators)
        };
        if let Some((key, value)) = tok.split_once(':')
            && !value.is_empty()
        {
            match key {
                "line" => {
                    let (value, align) = match value.split_once(',') {
                        Some((v, a)) => (v, Some(a)),
                        None => (value, None),
                    };
                    if let Some(pct) = value.strip_suffix('%') {
                        if let Some(v) = parse_percent(pct) {
                            cs.line_position = Some(v);
                        }
                    } else if let Ok(v) = value.parse::<i32>() {
                        cs.line_number = Some(v);
                    }
                    if let Some(a) = align.filter(|a| !a.is_empty()) {
                        cs.line_align = Some(a.to_string());
                    }
                    continue;
                }
                "position" => {
                    let (value, align) = match value.split_once(',') {
                        Some((v, a)) => (v, Some(a)),
                        None => (value, None),
                    };
                    if let Some(v) = value.strip_suffix('%').and_then(parse_percent) {
                        cs.text_position = Some(v);
                    }
                    if let Some(a) = align.filter(|a| !a.is_empty()) {
                        cs.position_align = Some(a.to_string());
                    }
                    continue;
                }
                "size" => {
                    if let Some(v) = value.strip_suffix('%').and_then(parse_percent) {
                        cs.text_size = Some(v);
                    }
                    continue;
                }
                "align" => {
                    cs.alignment = Some(value.to_string());
                    continue;
                }
                "vertical" => {
                    cs.vertical = Some(value.to_string());
                    continue;
                }
                "region" => continue,
                _ => {} // fall through to the one-letter C forms
            }
        }
        match first {
            b'T' => {
                if let Some(v) = scan_short_after(tok, "T:") {
                    cs.text_position = Some(v as u8);
                }
            }
            b'S' => {
                if let Some(v) = scan_short_after(tok, "S:") {
                    cs.text_size = Some(v as u8);
                }
            }
            b'L' => {
                if tok.ends_with('%')
                    && let Some(v) = scan_short_after(tok, "L:")
                {
                    cs.line_position = Some(v as u8);
                }
            }
            b'D' => {
                if tok.len() > 2 && tok.is_char_boundary(2) {
                    cs.vertical = Some(tok[2..].to_string());
                }
            }
            b'A' if tok.len() > 2 && tok.is_char_boundary(2) => {
                cs.alignment = Some(tok[2..].to_string());
            }
            _ => {}
        }
    }

    cs
}

fn parse_percent(s: &str) -> Option<u8> {
    let v = s.parse::<f32>().ok()?;
    (v.is_finite() && (0.0..=100.0).contains(&v)).then(|| v.round() as u8)
}

fn scan_short_after(tok: &str, prefix: &str) -> Option<i16> {
    let rest = tok.strip_prefix(prefix)?;
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut val: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        val = val.wrapping_mul(10).wrapping_add((bytes[i] - b'0') as i64);
        i += 1;
    }
    if i == start {
        return None;
    }
    if neg {
        val = -val;
    }
    Some(val as i16)
}

/// The cue text pipeline's buffers. Kept across cues, a warm one runs it without allocating.
#[derive(Debug, Default)]
pub struct MarkupScratch {
    out: String,
    tmp: String,
    open: Vec<&'static str>,
}

/// One container-framed cue body to Pango markup, the pipeline a whole file's cue text takes.
/// A cue in WebM's `D_WEBVTT` block form, every `subtitle/vtt` buffer's, as its identifier,
/// settings and text. A block short of its two line feeds is all text.
pub fn vtt_block(b: &str) -> (&str, &str, &str) {
    fn cut(l: &str) -> &str {
        l.strip_suffix('\r').unwrap_or(l)
    }
    match b.split_once('\n').and_then(|(id, r)| Some((id, r.split_once('\n')?))) {
        Some((id, (settings, text))) => (cut(id), cut(settings), text),
        None => ("", "", b),
    }
}

pub fn cue_text_to_pango_markup(text: &str) -> String {
    finalize_text(text)
}

/// [`cue_text_to_pango_markup`] into `s`, the result borrowed from it.
pub fn cue_text_to_pango_markup_into<'s>(text: &str, s: &'s mut MarkupScratch) -> &'s str {
    finalize_into(text, s)
}

fn finalize_text(buf: &str) -> String {
    let mut s = MarkupScratch::default();
    finalize_into(buf, &mut s);
    s.out
}

/// The C's cue text pipeline, `g_markup_escape_text`, `subrip_unescape_formatting`,
/// `subrip_remove_unhandled_tags`, `strip_trailing_newlines`, `subrip_fix_up_markup`. Each
/// stage reads `out` and writes `tmp`, then the two swap.
fn finalize_into<'s>(buf: &str, s: &'s mut MarkupScratch) -> &'s str {
    markup_escape_text(buf, &mut s.out);
    if unescape_formatting(&s.out, &mut s.tmp, ALLOWED_VTT_TAGS, true) {
        std::mem::swap(&mut s.out, &mut s.tmp);
    }
    if remove_unhandled_tags(&s.out, &mut s.tmp) {
        std::mem::swap(&mut s.out, &mut s.tmp);
    }
    strip_trailing_newlines(&mut s.out);
    fix_up_markup(&s.out, &mut s.tmp, &mut s.open, ALLOWED_VTT_TAGS);
    std::mem::swap(&mut s.out, &mut s.tmp);
    &s.out
}

/// Port of GLib's `g_markup_escape_text`.
fn markup_escape_text(input: &str, out: &mut String) {
    use std::fmt::Write;
    out.clear();
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => {
                let u = c as u32;
                let control = (0x1..=0x8).contains(&u)
                    || (0xb..=0xc).contains(&u)
                    || (0xe..=0x1f).contains(&u)
                    || (0x7f..=0x84).contains(&u)
                    || (0x86..=0x9f).contains(&u);
                if control {
                    // Writing to a String cannot fail.
                    let _ = write!(out, "&#x{u:x};");
                } else {
                    out.push(c);
                }
            }
        }
    }
}

/// The C keeps only `[alnum . space tab ( )]` in tag attributes, truncating `Jean-Luc`.
/// Here only controls and the brackets end a run.
fn is_tag_attr_byte(b: u8) -> bool {
    b >= 0x20 && b != 0x7f && b != b'<' && b != b'>'
}

/// Port of `subrip_unescape_formatting`, `false` when `src` has no tag and `out` is untouched.
fn unescape_formatting(src: &str, out: &mut String, allowed: &[&str], allows_attrs: bool) -> bool {
    if !src.contains("&lt;") {
        return false;
    }
    out.clear();
    let mut p = 0usize;

    loop {
        let rest = &src[p..];
        let lt_rel = match rest.find("&lt;") {
            Some(i) => i,
            None => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..lt_rel]);
        let lt = p + lt_rel + 4; // just past "&lt;"

        let gt_rel = match src[lt..].find("&gt;") {
            Some(i) => i,
            None => {
                out.push_str(&src[lt - 4..]);
                break;
            }
        };
        let gt = lt + gt_rel; // start of "&gt;"
        let region = &src[lt..gt];
        let rb = region.as_bytes();

        let is_closing = rb.first() == Some(&b'/');
        let mut ts = if is_closing { 1 } else { 0 };
        while ts < rb.len() && (rb[ts] == b' ' || rb[ts] == b'\t') {
            ts += 1;
        }
        let mut te = ts;
        while te < rb.len() && rb[te].is_ascii_alphanumeric() {
            te += 1;
        }
        let name = &region[ts..te];

        let allowed_match = allowed.contains(&name);
        if !allowed_match {
            out.push_str(&src[lt - 4..gt + 4]);
            p = gt + 4;
            continue;
        }

        out.push('<');
        if is_closing {
            out.push('/');
        }
        out.push_str(name);
        if allows_attrs {
            let mut ae = te;
            while ae < rb.len() && is_tag_attr_byte(rb[ae]) {
                ae += 1;
            }
            out.push_str(&region[te..ae]);
        }
        out.push('>');
        p = gt + 4;
    }
    true
}

/// Port of `subrip_remove_unhandled_tags`, `false` when `src` has no tag and `out` is untouched.
/// The `gt` cursor only moves forward, the C's rescan is quadratic.
fn remove_unhandled_tags(src: &str, out: &mut String) -> bool {
    if !src.contains("&lt;") {
        return false;
    }
    out.clear();
    let bytes = src.as_bytes();
    let mut gt = src.find("&gt;");
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"&lt;") {
            while let Some(g) = gt.filter(|&g| g < i + 4) {
                gt = src[g + 1..].find("&gt;").map(|rel| g + 1 + rel);
            }
            if let Some(g) = gt {
                let mut t = i + 4;
                if bytes.get(t) == Some(&b'/') {
                    t += 1;
                }
                if bytes.get(t).is_some_and(|b| b.is_ascii_alphabetic()) {
                    i = g + 4; // drop the whole tag
                    continue;
                }
            }
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    true
}

/// Port of `strip_trailing_newlines`. Drops trailing `\n` while length > 1.
fn strip_trailing_newlines(txt: &mut String) {
    while txt.len() > 1 && txt.as_bytes()[txt.len() - 1] == b'\n' {
        txt.pop();
    }
}

/// Port of `subrip_fix_up_markup` from `src` into `out`. Stray closers are skipped by copying,
/// removing in place was quadratic. Every cut is at an ASCII `<` or past a `>`.
fn fix_up_markup(
    src: &str,
    out: &mut String,
    open_tags: &mut Vec<&'static str>,
    allowed: &[&'static str],
) {
    let buf = src.as_bytes();
    out.clear();
    open_tags.clear();
    let mut cur = 0usize;
    let mut copied = 0usize;

    while let Some(rel) = buf[cur..].iter().position(|&b| b == b'<') {
        let next_tag = cur + rel;

        let mut offset = 0usize;
        let mut is_closing = false;

        for &tag in allowed {
            let mut ts = next_tag + 1;
            let closing = buf.get(ts) == Some(&b'/');
            if closing {
                ts += 1;
            }
            while ts < buf.len() && (buf[ts] == b' ' || buf[ts] == b'\t') {
                ts += 1;
            }
            let name_start = ts;
            let mut te = ts;
            while te < buf.len() && buf[te].is_ascii_alphanumeric() {
                te += 1;
            }
            let name_len = te - name_start;

            let name_matches =
                tag.len() == name_len && tag.as_bytes().eq_ignore_ascii_case(&buf[name_start..te]);
            if name_matches {
                if te < buf.len() && (buf[te] == b' ' || buf[te] == b'\t' || buf[te] == b'.') {
                    while te < buf.len() && is_tag_attr_byte(buf[te]) {
                        te += 1;
                    }
                }
                if te < buf.len() && buf[te] == b'>' {
                    offset = te - (next_tag + 1);
                    is_closing = closing;
                    // The allowed names are lowercase already.
                    if !closing {
                        open_tags.push(tag);
                    }
                    break;
                }
            }
            offset = 0;
        }

        if offset == 0 {
            cur = next_tag + 1;
            continue;
        }
        if !is_closing {
            cur = next_tag + offset;
            continue;
        }

        let tag_end = match buf[next_tag..].iter().position(|&b| b == b'>') {
            Some(rel) => next_tag + rel,
            None => {
                cur = next_tag + 1;
                continue;
            }
        };

        let matches_last = match open_tags.last() {
            Some(last) => {
                let cmp = next_tag + 2;
                let ll = last.len();
                cmp + ll <= buf.len() && buf[cmp..cmp + ll].eq_ignore_ascii_case(last.as_bytes())
            }
            None => false,
        };

        if !matches_last {
            out.push_str(&src[copied..next_tag]);
            copied = tag_end + 1;
            cur = tag_end + 1;
            continue;
        }

        open_tags.pop();
        cur = tag_end + 1;
    }

    out.push_str(&src[copied..]);
    for t in open_tags.iter().rev() {
        out.push_str("</");
        out.push_str(t);
        out.push('>');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = GST_SECOND;

    fn parse(body: &str) -> Vec<Cue> {
        WebVtt::default().parse(body, &ParseContext::default()).unwrap()
    }

    fn vtt(chunk: &str) -> String {
        format!("WEBVTT FILE\n{chunk}")
    }

    fn one(chunk: &str) -> Cue {
        let cues = parse(&vtt(chunk));
        assert_eq!(cues.len(), 1, "expected exactly one cue for {chunk:?}");
        cues.into_iter().next().unwrap()
    }

    fn assert_cue(chunk: &str, from: u64, to: u64, text: &str) {
        let c = one(chunk);
        assert_eq!(c.start_ns, from, "start for {chunk:?}");
        assert_eq!(c.end_ns, Some(to), "end for {chunk:?}");
        assert_eq!(c.text, text, "text for {chunk:?}");
    }

    #[test]
    fn reversed_timing_saturates_and_does_not_panic() {
        let c = one("\n00:00:05.000 --> 00:00:02.000\nx");
        assert_eq!(c.start_ns, 5 * S);
        assert_eq!(c.end_ns, Some(5 * S));
        assert_eq!(c.text, "x");
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(WebVtt::default().output_format(), OutputFormat::PangoMarkup);
    }

    #[test]
    fn c_webvtt_input_cue_settings_are_ignored_for_text() {
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 D:vertical T:50%\nOne\n\n", S, 2 * S, "One");
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 D:vertical   T:50%\nOne\n\n", S, 2 * S, "One");
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 D:vertical\tT:50%\nOne\n\n", S, 2 * S, "One");
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 D:vertical-lr\nOne\n\n", S, 2 * S, "One");
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 L:-123\nOne\n\n", S, 2 * S, "One");
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 L:123\nOne\n\n", S, 2 * S, "One");
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 L:12%\nOne\n\n", S, 2 * S, "One");
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 L:12% S:35% A:start\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 A:middle\nOne\n\n", S, 2 * S, "One");
        assert_cue("1\n00:00:01.000 --> 00:00:02.000 A:end\nOne\n\n", S, 2 * S, "One");
    }

    #[test]
    fn c_webvtt_input_escaping() {
        assert_cue("1\n00:00:01.000 --> 00:00:02.000\nOne & Two\n\n", S, 2 * S, "One &amp; Two");
        assert_cue("1\n00:00:01.000 --> 00:00:02.000\nOne < Two\n\n", S, 2 * S, "One &lt; Two");
    }

    #[test]
    fn c_webvtt_input_markup_tags() {
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<v Spoke>Live long and prosper\n\n",
            S,
            2 * S,
            "<v Spoke>Live long and prosper</v>",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<v The Joker>HAHAHA\n\n",
            S,
            2 * S,
            "<v The Joker>HAHAHA</v>",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<c.someclass>some text\n\n",
            S,
            2 * S,
            "<c.someclass>some text</c>",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<b.loud>some text\n\n",
            S,
            2 * S,
            "<b.loud>some text</b>",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<ruby>base text<rt>annotation</rt></ruby>\n\n",
            S,
            2 * S,
            "<ruby>base text<rt>annotation</rt></ruby>",
        );
    }

    #[test]
    fn c_webvtt_input_inline_timestamps_stay_escaped() {
        assert_cue(
            "1\n00:00:01.000 --> 00:00:03.000\nOne... <00:00:00,200>Two... <00:00:00,500>Three...\n\n",
            S,
            3 * S,
            "One... &lt;00:00:00,200&gt;Two... &lt;00:00:00,500&gt;Three...",
        );
    }

    #[test]
    fn c_webvtt_input_multiline() {
        assert_cue(
            "1\n00:00:02.000 --> 00:00:03.000\nHello\nWorld\n\n",
            2 * S,
            3 * S,
            "Hello\nWorld",
        );
    }

    #[test]
    fn c_webvtt_input_combined_stream() {
        let chunks = [
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical T:50%\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical   T:50%\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical\tT:50%\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical-lr\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 L:-123\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 L:123\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 L:12%\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 L:12% S:35% A:start\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 A:middle\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 A:end\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\nOne & Two\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\nOne < Two\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<v Spoke>Live long and prosper\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<v The Joker>HAHAHA\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<c.someclass>some text\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<b.loud>some text\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<ruby>base text<rt>annotation</rt></ruby>\n\n",
            "1\n00:00:01.000 --> 00:00:03.000\nOne... <00:00:00,200>Two... <00:00:00,500>Three...\n\n",
            "1\n00:00:02.000 --> 00:00:03.000\nHello\nWorld\n\n",
        ];
        let body: String = chunks.iter().map(|c| vtt(c)).collect();
        let cues = parse(&body);
        assert_eq!(cues.len(), chunks.len());
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[10].text, "One &amp; Two");
        assert_eq!(cues[16].text, "<ruby>base text<rt>annotation</rt></ruby>");
        assert_eq!(cues[17].start_ns, S);
        assert_eq!(cues[17].end_ns, Some(3 * S));
        assert_eq!(cues[18].text, "Hello\nWorld");
    }

    #[test]
    fn c_webvtt_input1_no_hour() {
        assert_cue(
            "1\n00:01.000 --> 00:02.000 D:vertical T:50%\nNo hour component\n\n",
            S,
            2 * S,
            "No hour component",
        );
    }

    #[test]
    fn c_webvtt_input2_no_trailing_newline() {
        assert_cue(
            "1\n00:00:01,000 --> 00:00:02,000\nLast cue, no newline at the end",
            S,
            2 * S,
            "Last cue, no newline at the end",
        );
    }

    #[test]
    fn c_webvtt_input3_broken_closing_tags() {
        assert_cue(
            "1\n00:00:00,000 --> 00:00:01,000\n<ruby>Hello!</ruby>World!\n\n",
            0,
            S,
            "<ruby>Hello!</ruby>World!",
        );
        assert_cue(
            "1\n00:00:01,000 --> 00:00:02,000\n<ruby>Hello!</i></ruby>World!\n\n",
            S,
            2 * S,
            "<ruby>Hello!</ruby>World!",
        );
        assert_cue(
            "1\n00:00:02,000 --> 00:00:03,000\n<i>World!</ruby></i>Hello!\n\n",
            2 * S,
            3 * S,
            "<i>World!</i>Hello!",
        );
    }

    fn settings_of(chunk: &str) -> CueSettings {
        one(chunk).settings
    }

    #[test]
    fn settings_vertical_and_text_position() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 D:vertical T:50%\nOne\n\n");
        assert_eq!(cs.vertical.as_deref(), Some("vertical"));
        assert_eq!(cs.text_position, Some(50));
        assert_eq!(cs.line_position, None);
        assert_eq!(cs.text_size, None);
        assert_eq!(cs.alignment, None);
    }

    #[test]
    fn settings_vertical_lr() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 D:vertical-lr\nOne\n\n");
        assert_eq!(cs.vertical.as_deref(), Some("vertical-lr"));
    }

    #[test]
    fn settings_line_percent_vs_line_number() {
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 L:12%\nOne\n\n").line_position,
            Some(12)
        );
        assert_eq!(settings_of("00:00:01.000 --> 00:00:02.000 L:123\nOne\n\n").line_position, None);
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 L:-123\nOne\n\n").line_position,
            None
        );
    }

    #[test]
    fn settings_size_alignment_combo() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 L:12% S:35% A:start\nOne\n\n");
        assert_eq!(cs.line_position, Some(12));
        assert_eq!(cs.text_size, Some(35));
        assert_eq!(cs.alignment.as_deref(), Some("start"));
    }

    #[test]
    fn settings_alignment_values() {
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 A:middle\nOne\n\n").alignment.as_deref(),
            Some("middle")
        );
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 A:end\nOne\n\n").alignment.as_deref(),
            Some("end")
        );
    }

    #[test]
    fn settings_separators_tab_and_multispace() {
        let tab = settings_of("00:00:01.000 --> 00:00:02.000 D:vertical\tT:50%\nOne\n\n");
        assert_eq!(tab.vertical.as_deref(), Some("vertical"));
        assert_eq!(tab.text_position, Some(50));

        let multi = settings_of("00:00:01.000 --> 00:00:02.000 D:vertical   T:50%\nOne\n\n");
        assert_eq!(multi.vertical.as_deref(), Some("vertical"));
        assert_eq!(multi.text_position, Some(50));
    }

    #[test]
    fn settings_absent_are_all_none() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000\nOne\n\n");
        assert_eq!(cs, CueSettings::default());
    }

    #[test]
    fn modern_settings_are_parsed() {
        let cs = settings_of(
            "00:00:01.000 --> 00:00:02.000 align:center position:50%,line-left size:35% line:10%,end vertical:rl\nOne\n\n",
        );
        assert_eq!(cs.alignment.as_deref(), Some("center"));
        assert_eq!(cs.text_position, Some(50));
        assert_eq!(cs.position_align.as_deref(), Some("line-left"));
        assert_eq!(cs.text_size, Some(35));
        assert_eq!(cs.line_position, Some(10));
        assert_eq!(cs.line_align.as_deref(), Some("end"));
        assert_eq!(cs.vertical.as_deref(), Some("rl"));
    }

    #[test]
    fn modern_line_number_form() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 line:-1\nOne\n\n");
        assert_eq!(cs.line_number, Some(-1));
        assert_eq!(cs.line_position, None);
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 line:3,start\nOne\n\n");
        assert_eq!(cs.line_number, Some(3));
        assert_eq!(cs.line_align.as_deref(), Some("start"));
    }

    #[test]
    fn modern_settings_do_not_change_cue_text_or_timing() {
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 align:center position:50%\nOne\n\n",
            S,
            2 * S,
            "One",
        );
    }

    #[test]
    fn modern_percent_values_are_validated() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 position:150% size:abc%\nOne\n\n");
        assert_eq!(cs.text_position, None);
        assert_eq!(cs.text_size, None);
    }

    #[test]
    fn old_single_letter_forms_still_parse() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 T:50% align:end\nOne\n\n");
        assert_eq!(cs.text_position, Some(50));
        assert_eq!(cs.alignment.as_deref(), Some("end"));
    }

    fn parse_keeping_parser(body: &str) -> (Vec<Cue>, WebVtt) {
        let mut p = WebVtt::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        (cues, p)
    }

    #[test]
    fn style_block_is_collected_and_cues_are_untouched() {
        let body = "WEBVTT\n\nSTYLE\n::cue { color: lime }\n\n00:00:01.000 --> 00:00:02.000\n<i>Hi</i>\n\n";
        let (cues, p) = parse_keeping_parser(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<i>Hi</i>");
        assert_eq!(cues[0].start_ns, S);
        let sheet = p.stylesheet().expect("stylesheet collected");
        assert_eq!(sheet.rules().len(), 1);
    }

    #[test]
    fn multiple_style_blocks_accumulate() {
        let body = "WEBVTT\n\nSTYLE\n::cue { color: green }\n\nSTYLE\n::cue { background: green }\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n";
        let (_, p) = parse_keeping_parser(body);
        assert_eq!(p.stylesheet().unwrap().rules().len(), 2);
    }

    #[test]
    fn style_lines_must_match_exactly_and_precede_cues() {
        for bad in ["STYLE Invalid", " STYLE", "style", "S T Y L E", "STYLE {"] {
            let body = format!(
                "WEBVTT\n\n{bad}\n::cue {{ color: red }}\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n"
            );
            let (cues, p) = parse_keeping_parser(&body);
            assert_eq!(cues.len(), 1, "{bad:?} must not eat the cue");
            assert!(p.stylesheet().is_none(), "{bad:?} must not collect CSS");
        }
        let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\n\nSTYLE\n::cue { color: red }\n\n00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let (cues, p) = parse_keeping_parser(body);
        assert_eq!(cues.len(), 2);
        assert!(p.stylesheet().is_none());
    }

    #[test]
    fn timing_line_terminates_a_style_block() {
        let body = "WEBVTT\n\nSTYLE\n::cue { color: lime }\n00:00:01.000 --> 00:00:02.000\nOne\n\n";
        let (cues, p) = parse_keeping_parser(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "One");
        assert_eq!(p.stylesheet().unwrap().rules().len(), 1);
    }

    #[test]
    fn empty_style_block_collects_nothing() {
        let body =
            "WEBVTT\n\nSTYLE\n\n::cue { color: red }\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n";
        let (cues, p) = parse_keeping_parser(body);
        assert_eq!(cues.len(), 1);
        assert!(p.stylesheet().is_none());
    }

    #[test]
    fn cue_identifier_is_captured() {
        let cues = parse(&vtt(
            "greeting\n00:00:01.000 --> 00:00:02.000\nHi\n\n00:00:03.000 --> 00:00:04.000\nAnon\n\n",
        ));
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].id.as_deref(), Some("greeting"));
        assert_eq!(cues[1].id, None);
    }

    #[test]
    fn note_line_directly_before_timing_becomes_the_id() {
        let cues = parse(&vtt("NOTE a comment\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n"));
        assert_eq!(cues[0].id, None);
    }

    #[test]
    fn stylesheet_survives_incremental_chunking() {
        let body = "WEBVTT\n\nSTYLE\n::cue(b) { color: red }\n\n00:00:01.000 --> 00:00:02.000\n<b>One</b>\n\n";
        let mut p = WebVtt::default();
        let mut buf = String::new();
        let mut cues = Vec::new();
        for (i, ch) in body.char_indices() {
            buf.push_str(&body[i..i + ch.len_utf8()]);
            let parsed = p.parse_incremental(&buf, &ParseContext::default(), false).unwrap();
            cues.extend(parsed.cues);
            buf.drain(..parsed.consumed);
        }
        let parsed = p.parse_incremental(&buf, &ParseContext::default(), true).unwrap();
        cues.extend(parsed.cues);

        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<b>One</b>");
        assert_eq!(p.stylesheet().unwrap().rules().len(), 1);
    }

    const WPT_URLS_NOTE: &str = "NOTE\n\
        Background for Voice1 should apply.\n\
        The other two backgrounds should not render because non-data URLs are not supported.";

    const WPT_URLS_STYLE: &str = "STYLE\n\
        ::cue(v[voice=Voice1])\n\
        {\n    \
            background: url(data:image/gif;base64,R0lGODlhEAAQAMQAAORHHOVSKudfOulrSOp3WOyDZu6QdvCchPGolfO0o/XBs/fNwfjZ0frl3/zy7////wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACH5BAkAABAALAAAAAAQABAAAAVVICSOZGlCQAosJ6mu7fiyZeKqNKToQGDsM8hBADgUXoGAiqhSvp5QAnQKGIgUhwFUYLCVDFCrKUE1lBavAViFIDlTImbKC5Gm2hB0SlBCBMQiB0UjIQA7)\n\
        }\n\
        ::cue(b)\n\
        {\n    \
            background: url(\"support/background.png\")\n\
        }\n\
        ::cue(i) {\n    \
            background: url(\"support/background.png\");\n    \
            background: -webkit-image-set(url(\"support/background.png\") 1x, url(\"support/background.png\") 2x);\n\
        }";

    const WPT_URLS_CUE1: &str = "00:00:00.000 --> 00:00:05.000\n\
        <v Voice1>This <i>is</i> a <b>test</b> subtitle";
    const WPT_URLS_CUE2: &str = "00:00:00.000 --> 00:00:05.000\n\
        <v Voice2>Here <i>is</i> a <b>second</b> subtitle";

    fn assert_wpt_urls_cues(cues: &[Cue]) {
        assert_eq!(cues.len(), 2, "expected exactly two cues, got {cues:?}");
        for c in cues {
            assert_eq!(c.start_ns, 0);
            assert_eq!(c.end_ns, Some(5 * S));
            assert!(
                !c.text.contains("--&gt;") && !c.text.contains("-->"),
                "a timing line leaked into the cue text: {:?}",
                c.text
            );
            assert!(
                !c.text.contains("00:00:00.000"),
                "a timing line leaked into the cue text: {:?}",
                c.text
            );
        }
        assert_eq!(cues[0].text, "<v Voice1>This <i>is</i> a <b>test</b> subtitle</v>");
        assert_eq!(cues[1].text, "<v Voice2>Here <i>is</i> a <b>second</b> subtitle</v>");
    }

    #[test]
    fn wpt_embedded_style_urls_run_together() {
        let body = format!(
            "WEBVTT\n{WPT_URLS_NOTE}\n{WPT_URLS_STYLE}\n{WPT_URLS_CUE1}\n{WPT_URLS_CUE2}\n"
        );
        let (cues, p) = parse_keeping_parser(&body);
        assert_wpt_urls_cues(&cues);
        assert!(p.stylesheet().is_none());
    }

    #[test]
    fn wpt_embedded_style_urls_blank_line_separated() {
        let body = format!(
            "WEBVTT\n\n{WPT_URLS_NOTE}\n\n{WPT_URLS_STYLE}\n\n{WPT_URLS_CUE1}\n\n{WPT_URLS_CUE2}\n"
        );
        let (cues, p) = parse_keeping_parser(&body);
        assert_wpt_urls_cues(&cues);
        assert!(p.stylesheet().is_none());
    }

    #[test]
    fn style_block_extent_survives_urls_and_braces() {
        let style = format!("{WPT_URLS_STYLE}\n::cue(b) {{ color: lime }}");
        let body = format!("WEBVTT\n{WPT_URLS_NOTE}\n{style}\n{WPT_URLS_CUE1}\n{WPT_URLS_CUE2}\n");
        let (cues, p) = parse_keeping_parser(&body);
        assert_wpt_urls_cues(&cues);
        let sheet = p.stylesheet().expect("the last rule is applicable");
        assert_eq!(sheet.rules().len(), 1);
    }

    #[test]
    fn timing_line_ends_the_cue_being_collected() {
        let body =
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\n00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[1].text, "Two");
        assert_eq!(cues[1].start_ns, 3 * S);
    }

    #[test]
    fn resynchronisation_keeps_the_identifier_rules() {
        let body = "WEBVTT\n\nid1\n00:00:01.000 --> 00:00:02.000\nOne\nid2\n\
                    00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].id.as_deref(), Some("id1"));
        assert_eq!(cues[0].text, "One\nid2");
        assert_eq!(cues[1].id, None);
        assert_eq!(cues[1].text, "Two");
    }

    #[test]
    fn unparsable_arrow_line_ends_the_cue_and_eats_its_block() {
        let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\n\
                    broken --> line\ndropped\n\n00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[1].text, "Two");
    }

    #[test]
    fn back_to_back_timing_lines_leave_the_first_cue_empty() {
        let body =
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[1].text, "Two");
    }

    #[test]
    fn multi_line_cue_text_is_still_joined() {
        let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\nTwo\nThree\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "One\nTwo\nThree");
    }

    #[test]
    fn timestamp_dot_and_comma_both_accepted() {
        assert_cue(
            "00:00:01.500 --> 00:00:02.250\nX\n\n",
            1_500 * GST_MSECOND,
            2_250 * GST_MSECOND,
            "X",
        );
        assert_cue(
            "00:00:01,500 --> 00:00:02,250\nX\n\n",
            1_500 * GST_MSECOND,
            2_250 * GST_MSECOND,
            "X",
        );
    }

    #[test]
    fn timestamp_short_fraction_is_right_padded() {
        assert_cue(
            "00:00:01,5 --> 00:00:02,25\nX\n\n",
            1_500 * GST_MSECOND,
            2_250 * GST_MSECOND,
            "X",
        );
    }

    #[test]
    fn timestamp_hours_component() {
        assert_cue(
            "01:02:03.004 --> 01:02:04.005\nX\n\n",
            (3600 + 2 * 60 + 3) * S + 4 * GST_MSECOND,
            (3600 + 2 * 60 + 4) * S + 5 * GST_MSECOND,
            "X",
        );
    }

    #[test]
    fn short_fraction_with_cue_settings_keeps_the_cue() {
        // The ' ' -> '0' step munges settings into the fraction, so the C reports 1.000 -> 2.050.
        for tail in ["A:start", "align:middle", "T:50%", "L:12% S:35% A:start"] {
            assert_cue(
                &format!("1\n00:00:01.000 --> 00:00:02.5 {tail}\nOne\n\n"),
                S,
                2 * S + 50 * GST_MSECOND,
                "One",
            );
        }
        assert_cue("1\n00:01.000 --> 00:02.5 A:start\nOne\n\n", S, 2 * S + 50 * GST_MSECOND, "One");
        let cs = settings_of("00:00:01.000 --> 00:00:02.5 A:start\nOne\n\n");
        assert_eq!(cs.alignment.as_deref(), Some("start"));
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.500 A:start\nOne\n\n",
            S,
            2 * S + 500 * GST_MSECOND,
            "One",
        );
    }

    #[test]
    fn tab_in_timestamp_fields_is_skipped_like_sscanf() {
        assert_cue("\t00:00:01,000 --> 00:00:02,000\nOne\n\n", S, 2 * S, "One");
        assert_cue("00:\t0:01,000 --> 00:00:02,000\nOne\n\n", S, 2 * S, "One");
        assert_cue("00:00:01,\t5 --> 00:00:02,000\nOne\n\n", S + 50 * GST_MSECOND, 2 * S, "One");
    }

    #[test]
    fn junk_inside_a_timestamp_field_rejects_the_cue() {
        let body = vtt(
            "1\n00:00:01x,000 --> 00:00:02,000\nJunk\n\n00:00:03.000 --> 00:00:04.000\nGood\n\n",
        );
        let cues = parse(&body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        assert_eq!(cues[0].text, "Good");
        assert_eq!(parse_subrip_time("00:00:01x,000"), None);
        assert_eq!(parse_subrip_time("00:01x,000"), None);
    }

    #[test]
    fn signed_timestamp_field_matches_strtoul() {
        assert_eq!(parse_subrip_time("00:00:+1,000"), Some(S));
        assert_eq!(parse_subrip_time("00:00:-1,000"), Some(u64::from(u32::MAX) * S));
    }

    #[test]
    fn absurd_minute_field_wraps_at_32_bits() {
        assert_eq!(parse_subrip_time("99999999:00,000"), Some(1_705_032_644 * S));
        assert_eq!(parse_subrip_time("71582789:00,000"), Some(44 * S));
    }

    #[test]
    fn cue_after_a_reversed_one_is_still_emitted() {
        let body =
            "WEBVTT\n\n00:00:05.000 --> 00:00:02.000\nA\n\n00:00:03.000 --> 00:00:04.000\nB\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        assert_eq!(cues[0].text, "A");
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (5 * S, Some(5 * S)));
        assert_eq!(cues[1].text, "B");
        assert_eq!((cues[1].start_ns, cues[1].end_ns), (3 * S, Some(4 * S)));
    }

    #[test]
    fn unhandled_tag_scan_with_a_distant_closing_marker() {
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\n<5<5<5 <font x>y>\n\n",
            S,
            2 * S,
            "&lt;5&lt;5&lt;5 y&gt;",
        );
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\n<font <i <b\n\n",
            S,
            2 * S,
            "&lt;font &lt;i &lt;b",
        );
    }

    #[test]
    fn broken_timestamp_without_comma_is_skipped() {
        let body = vtt(
            "2\n00:02:00,000 --> 00:03:0\nsome other text\n\n3\n00:00:03,000 --> 00:00:04,000\nsome more text\n\n",
        );
        let cues = parse(&body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 3 * S);
        assert_eq!(cues[0].end_ns, Some(4 * S));
        assert_eq!(cues[0].text, "some more text");
    }

    #[test]
    fn empty_body_yields_no_cues() {
        assert!(parse("").is_empty());
        assert!(parse("WEBVTT\n\n").is_empty());
        assert!(parse("WEBVTT FILE\n\n\n").is_empty());
    }

    #[test]
    fn header_and_cue_id_lines_are_ignored() {
        let body =
            "WEBVTT\n\nNOTE this is a comment\n\ngreeting\n00:00:00.000 --> 00:00:01.000\nHi\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
        assert_eq!(cues[0].start_ns, 0);
        assert_eq!(cues[0].end_ns, Some(S));
    }

    #[test]
    fn crlf_line_endings() {
        let body = "WEBVTT\r\n\r\n00:00:01.000 --> 00:00:02.000\r\nHi\r\n\r\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
    }

    #[test]
    fn monotonic_guard_drops_backwards_cue() {
        let body =
            "WEBVTT\n\n00:00:05.000 --> 00:00:06.000\nA\n\n00:00:01.000 --> 00:00:02.000\nB\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "A");
        assert_eq!(cues[0].start_ns, 5 * S);
    }

    #[test]
    fn two_cues_in_one_body() {
        let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nFirst\n\n00:00:02.000 --> 00:00:03.000\nSecond\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "First");
        assert_eq!(cues[1].text, "Second");
        assert_eq!(cues[1].start_ns, 2 * S);
        assert_eq!(cues[1].end_ns, Some(3 * S));
    }

    #[test]
    fn disallowed_tag_is_stripped() {
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\n<font color=\"red\">hi</font>\n\n",
            S,
            2 * S,
            "hi",
        );
    }

    #[test]
    fn unclosed_allowed_tag_is_auto_closed() {
        assert_cue("00:00:01.000 --> 00:00:02.000\n<i>italic\n\n", S, 2 * S, "<i>italic</i>");
    }

    #[test]
    fn double_quote_and_apostrophe_escape() {
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\nsay \"hi\" it's ok\n\n",
            S,
            2 * S,
            "say &quot;hi&quot; it&apos;s ok",
        );
    }

    #[test]
    fn duration_helper() {
        let c = one("00:00:01.000 --> 00:00:02.500\nX\n\n");
        assert_eq!(c.duration_ns(), Some(1_500 * GST_MSECOND));
    }
}
