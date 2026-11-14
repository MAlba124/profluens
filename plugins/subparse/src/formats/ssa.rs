// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! SSA / ASS parser, a port of the C `ssaparse`. See `specs/ssa.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};
use crate::ssastyle::{SsaDialogue, SsaEvent, SsaStyles};

/// Parser for the SSA/ASS subtitle format (whole-file `[Events]`).
#[derive(Debug)]
pub struct Ssa {
    lines: LineScanner,
    machine: Machine,
    bom_checked: bool,
}

impl Default for Ssa {
    fn default() -> Self {
        Ssa { lines: LineScanner::new(), machine: Machine::default(), bom_checked: false }
    }
}

impl SubtitleFormat for Ssa {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let Self { lines, machine, bom_checked } = self;

        let mut skipped = 0usize;
        if !*bom_checked {
            if body.is_empty() && !at_eos {
                return Ok(Parsed::default());
            }
            *bom_checked = true;
            if body.starts_with('\u{feff}') {
                skipped = '\u{feff}'.len_utf8();
            }
        }
        let rest = &body[skipped..];

        let mut cues = Vec::new();
        let mut consumed = skipped + lines.feed(rest, at_eos, |line| machine.feed(line, &mut cues));

        if at_eos {
            let tail = &body[consumed..];
            if !tail.is_empty() {
                machine.feed(tail, &mut cues);
            }
            consumed = body.len();
            lines.reset();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        // gstssaparse.c advertises `text/x-raw, format=pango-markup`.
        OutputFormat::PangoMarkup
    }

    fn ssa_styles(&self) -> Option<&SsaStyles> {
        Some(&self.machine.styles)
    }
}

#[derive(Debug)]
struct Machine {
    in_events: bool,
    col_start: usize,
    col_end: usize,
    col_text: Option<usize>,
    col_style: Option<usize>,
    col_margin_l: Option<usize>,
    col_margin_r: Option<usize>,
    col_margin_v: Option<usize>,
    styles: SsaStyles,
}

impl Default for Machine {
    fn default() -> Self {
        Machine {
            in_events: false,
            col_start: 1,
            col_end: 2,
            col_text: Some(9),
            col_style: Some(3),
            col_margin_l: Some(5),
            col_margin_r: Some(6),
            col_margin_v: Some(7),
            styles: SsaStyles::default(),
        }
    }
}

impl Machine {
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            return;
        }

        self.styles.feed_line(trimmed);

        if let Some(rest) = trimmed.strip_prefix('[') {
            let name = match rest.split_once(']') {
                Some((name, _)) => name,
                None => rest.trim_end(),
            };
            self.in_events = name.eq_ignore_ascii_case("events");
            return;
        }

        if !self.in_events {
            return;
        }

        let Some((keyword, value)) = split_keyword(trimmed) else {
            return;
        };

        if keyword.eq_ignore_ascii_case("Format") {
            self.col_text = None;
            self.col_style = None;
            self.col_margin_l = None;
            self.col_margin_r = None;
            self.col_margin_v = None;
            for (i, n) in value.split(',').map(str::trim).enumerate() {
                if n.eq_ignore_ascii_case("Start") {
                    self.col_start = i;
                } else if n.eq_ignore_ascii_case("End") {
                    self.col_end = i;
                } else if n.eq_ignore_ascii_case("Text") {
                    self.col_text = Some(i);
                } else if n.eq_ignore_ascii_case("Style") {
                    self.col_style = Some(i);
                } else if n.eq_ignore_ascii_case("MarginL") {
                    self.col_margin_l = Some(i);
                } else if n.eq_ignore_ascii_case("MarginR") {
                    self.col_margin_r = Some(i);
                } else if n.eq_ignore_ascii_case("MarginV") {
                    self.col_margin_v = Some(i);
                }
            }
        } else if keyword.eq_ignore_ascii_case("Dialogue")
            && let Some(cue) = self.parse_dialogue(value)
        {
            cues.push(cue);
        }
    }

    fn parse_dialogue(&self, value: &str) -> Option<Cue> {
        let col_text = self.col_text?;
        // At most `col_text + 1` fields so Text keeps its commas.
        let fields: Vec<&str> = value.splitn(col_text + 1, ',').collect();
        if fields.len() < col_text + 1 {
            return None; // missing fields, skip like the C's lenient recovery
        }
        let start = parse_ass_time(fields.get(self.col_start)?.trim())?;
        let end = parse_ass_time(fields.get(self.col_end)?.trim())?.max(start);
        let raw_text = fields[col_text];
        let text = strip_to_pango_markup(raw_text);

        let field = |col: Option<usize>| col.and_then(|i| fields.get(i)).map(|f| f.trim());
        let margin =
            |col: Option<usize>| field(col).and_then(|f| f.parse::<u32>().ok()).unwrap_or(0);
        let mut cue = Cue::new(start, Some(end), text);
        cue.ssa = Some(Box::new(SsaDialogue {
            raw_text: raw_text.to_owned(),
            style: field(self.col_style).unwrap_or_default().to_owned(),
            margin_l: margin(self.col_margin_l),
            margin_r: margin(self.col_margin_r),
            margin_v: margin(self.col_margin_v),
        }));
        Some(cue)
    }
}

/// Convert a container-framed dialogue row to Pango markup, a byte-for-byte port of
/// `gst_ssa_parse_push_line`.
pub fn dialogue_to_pango_markup(line: &str) -> Option<String> {
    let text_field = skip_commas(line, 8)?;
    Some(strip_to_pango_markup(text_field))
}

/// The [`SsaEvent`] extras of the same framed row layout, borrowed from it, `None` exactly when
/// [`dialogue_to_pango_markup`] is. Allocates nothing.
pub fn framed_dialogue(line: &str) -> Option<SsaEvent<'_>> {
    let mut fields = line.splitn(9, ',');
    let mut field = || fields.next();
    let (_, _, style, _, l, r, v, _, raw_text) =
        (field()?, field()?, field()?, field()?, field()?, field()?, field()?, field()?, field()?);
    Some(SsaEvent {
        raw_text,
        style: style.trim(),
        margin_l: l.trim().parse().unwrap_or(0),
        margin_r: r.trim().parse().unwrap_or(0),
        margin_v: v.trim().parse().unwrap_or(0),
    })
}

/// Turn a raw *Text field* into Pango markup exactly as the C `ssaparse`
/// (`gst_ssa_parse_remove_override_codes` then `g_markup_printf_escaped`).
pub fn strip_to_pango_markup(text_field: &str) -> String {
    let (stripped, translate) = remove_override_blocks(text_field);
    let mut out = String::with_capacity(stripped.len() + 8);
    if translate {
        translate_and_escape(&stripped, &mut out);
    } else {
        for ch in stripped.chars() {
            escape_char(ch, &mut out);
        }
    }
    out
}

fn remove_override_blocks(text: &str) -> (String, bool) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    // '{' and '}' never appear as UTF-8 continuation bytes.
    while let Some(rel) = find_byte(&bytes[i..], b'{') {
        let open = i + rel;
        match find_byte(&bytes[open..], b'}') {
            Some(erel) => {
                let close = open + erel;
                out.push_str(&text[i..open]);
                i = close + 1; // drop '{' .. '}' inclusive
            }
            None => {
                out.push_str(&text[i..]);
                return (out, false);
            }
        }
    }
    out.push_str(&text[i..]);
    (out, true)
}

/// Translates `\N`/`\n`/`\h` and escapes in one pass. Equivalent to the C's three `strstr`
/// passes since no escape produces a new backslash.
fn translate_and_escape(s: &str, out: &mut String) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' | b'N' => {
                    out.push(' ');
                    out.push('\n');
                    i += 2;
                    continue;
                }
                b'h' => {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    continue;
                }
                _ => {
                    out.push('\\');
                    i += 1;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        escape_char(ch, out);
        i += ch.len_utf8();
    }
}

/// GLib `g_markup_escape_text` for one char.
fn escape_char(c: char, out: &mut String) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '\'' => out.push_str("&apos;"),
        '"' => out.push_str("&quot;"),
        _ => {
            let u = c as u32;
            if (0x1..=0x8).contains(&u)
                || (0xb..=0xc).contains(&u)
                || (0xe..=0x1f).contains(&u)
                || (0x7f..=0x84).contains(&u)
                || (0x86..=0x9f).contains(&u)
            {
                push_control_ref(u, out);
            } else {
                out.push(c);
            }
        }
    }
}

fn push_control_ref(u: u32, out: &mut String) {
    out.push_str("&#x");
    let mut started = false;
    let mut shift = 28;
    loop {
        let nibble = (u >> shift) & 0xf;
        if nibble != 0 || started || shift == 0 {
            started = true;
            out.push(char::from_digit(nibble, 16).unwrap());
        }
        if shift == 0 {
            break;
        }
        shift -= 4;
    }
    out.push(';');
}

fn find_byte(hay: &[u8], b: u8) -> Option<usize> {
    hay.iter().position(|&x| x == b)
}

fn skip_commas(s: &str, n: usize) -> Option<&str> {
    let mut rest = s;
    for _ in 0..n {
        let idx = rest.find(',')?;
        rest = &rest[idx + 1..];
    }
    Some(rest)
}

fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(':')?;
    let key = line[..idx].trim();
    let val = line[idx + 1..].trim_start_matches(' ');
    Some((key, val))
}

fn parse_ass_time(s: &str) -> Option<u64> {
    let mut parts = s.splitn(3, ':');
    let h: u64 = parts.next()?.trim().parse().ok()?;
    let m: u64 = parts.next()?.trim().parse().ok()?;
    let sec_field = parts.next()?.trim();
    if m >= 60 {
        return None;
    }
    let (sec_str, frac_str) = match sec_field.split_once('.') {
        Some((a, b)) => (a, b),
        None => (sec_field, ""),
    };
    let sec: u64 = sec_str.parse().ok()?;
    if sec >= 60 {
        return None;
    }
    let frac_ns = frac_to_ns(frac_str);
    // Uncapped hours saturate, and the fraction add can overflow on its own.
    Some(
        h.saturating_mul(3600)
            .saturating_add(m.saturating_mul(60))
            .saturating_add(sec)
            .saturating_mul(1_000_000_000)
            .saturating_add(frac_ns),
    )
}

/// A decimal fraction right-padded or truncated to 9 digits of ns, "5" and "50" are 500 ms.
fn frac_to_ns(frac: &str) -> u64 {
    let mut buf = [b'0'; 9];
    for (i, b) in frac.bytes().take(9).enumerate() {
        if b.is_ascii_digit() {
            buf[i] = b;
        } else {
            break;
        }
    }
    std::str::from_utf8(&buf).unwrap().parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn strips_single_override_block() {
        assert_eq!(strip_to_pango_markup("{\\i1}Hello{\\i0}"), "Hello");
    }

    #[test]
    fn strips_multiple_override_blocks() {
        assert_eq!(strip_to_pango_markup("a{\\b1}b{\\b0}c"), "abc");
    }

    #[test]
    fn strips_leading_and_trailing_blocks() {
        assert_eq!(strip_to_pango_markup("{\\an8}text"), "text");
        assert_eq!(strip_to_pango_markup("text{\\r}"), "text");
    }

    #[test]
    fn empty_override_block_removed() {
        assert_eq!(strip_to_pango_markup("a{}b"), "ab");
    }

    #[test]
    fn stray_close_brace_before_open_is_kept() {
        // strchr('}') starts at the '{', so an earlier '}' stays.
        assert_eq!(strip_to_pango_markup("a}b{\\i1}c"), "a}bc");
    }

    #[test]
    fn unmatched_brace_keeps_remainder_and_skips_escapes() {
        // The C returns early, "\N" stays literal.
        assert_eq!(strip_to_pango_markup("a{\\i1 no close \\Nb"), "a{\\i1 no close \\Nb");
    }

    #[test]
    fn unmatched_brace_after_a_valid_block() {
        assert_eq!(strip_to_pango_markup("{\\b1}ok{oops\\Ntail"), "ok{oops\\Ntail");
    }

    #[test]
    fn newline_escape_uppercase() {
        assert_eq!(strip_to_pango_markup("Line1\\NLine2"), "Line1 \nLine2");
    }

    #[test]
    fn newline_escape_lowercase() {
        assert_eq!(strip_to_pango_markup("Line1\\nLine2"), "Line1 \nLine2");
    }

    #[test]
    fn hard_space_escape() {
        assert_eq!(strip_to_pango_markup("a\\hb"), "a  b");
    }

    #[test]
    fn leading_newline_escape_has_space_before_newline() {
        assert_eq!(strip_to_pango_markup("\\Nx"), " \nx");
    }

    #[test]
    fn unknown_backslash_escape_kept_verbatim() {
        assert_eq!(strip_to_pango_markup("a\\db"), "a\\db");
    }

    #[test]
    fn backslash_at_end_kept() {
        assert_eq!(strip_to_pango_markup("abc\\"), "abc\\");
    }

    #[test]
    fn double_backslash_before_n() {
        assert_eq!(strip_to_pango_markup("\\\\N"), "\\ \n");
    }

    #[test]
    fn escapes_angle_brackets_and_amp() {
        assert_eq!(strip_to_pango_markup("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn escapes_quotes_with_named_refs() {
        assert_eq!(strip_to_pango_markup("'\""), "&apos;&quot;");
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(strip_to_pango_markup("\u{7}"), "&#x7;");
        assert_eq!(strip_to_pango_markup("\u{7f}"), "&#x7f;");
    }

    #[test]
    fn tab_and_newline_pass_through() {
        assert_eq!(strip_to_pango_markup("a\tb"), "a\tb");
    }

    #[test]
    fn combined_override_escape_and_markup() {
        assert_eq!(
            strip_to_pango_markup("{\\i1}<i>x & y</i>\\Nz"),
            "&lt;i&gt;x &amp; y&lt;/i&gt; \nz"
        );
    }

    #[test]
    fn utf8_text_preserved() {
        assert_eq!(strip_to_pango_markup("café — 日本語"), "café — 日本語");
    }

    #[test]
    fn dialogue_extracts_text_after_eight_commas() {
        let line = "0,0,Default,,0,0,0,,{\\i1}Hello world{\\i0}";
        assert_eq!(dialogue_to_pango_markup(line).unwrap(), "Hello world");
    }

    #[test]
    fn dialogue_preserves_commas_in_text() {
        let line = "1,0,Default,,0,0,0,,Hello, world, again";
        assert_eq!(dialogue_to_pango_markup(line).unwrap(), "Hello, world, again");
    }

    #[test]
    fn dialogue_translates_newline_in_text() {
        let line = "0,0,Default,,0,0,0,,First\\NSecond";
        assert_eq!(dialogue_to_pango_markup(line).unwrap(), "First \nSecond");
    }

    #[test]
    fn dialogue_missing_fields_returns_none() {
        assert_eq!(dialogue_to_pango_markup("0,0,Default"), None);
        assert_eq!(dialogue_to_pango_markup(""), None);
    }

    #[test]
    fn dialogue_exactly_eight_commas_empty_text() {
        assert_eq!(dialogue_to_pango_markup("0,0,d,,0,0,0,,").unwrap(), "");
    }

    #[test]
    fn parses_events_with_format_line() {
        let body = "\
[Script Info]
Title: Test

[V4+ Styles]
Format: Name, Fontname, Fontsize
Style: Default,Arial,20

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello {\\i1}world{\\i0}
Dialogue: 0,0:00:03.50,0:00:05.00,Default,,0,0,0,,Second\\Nline
Comment: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,ignored
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(3 * S));
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[1].start_ns, 3 * S + 500_000_000);
        assert_eq!(cues[1].end_ns, Some(5 * S));
        assert_eq!(cues[1].text, "Second \nline");
    }

    #[test]
    fn styles_format_line_does_not_affect_columns() {
        let body = "\
[V4+ Styles]
Format: Name, Fontname, Fontsize
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:02.00,0:00:04.00,Default,,0,0,0,,ok
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 2 * S);
        assert_eq!(cues[0].text, "ok");
    }

    #[test]
    fn parses_events_without_format_line_uses_defaults() {
        let body = "\
[Events]
Dialogue: 0,0:00:02.00,0:00:04.00,Default,,0,0,0,,Text here
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 2 * S);
        assert_eq!(cues[0].end_ns, Some(4 * S));
        assert_eq!(cues[0].text, "Text here");
    }

    #[test]
    fn dialogue_before_events_section_is_ignored() {
        let body = "\
[Script Info]
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,nope
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn ssa_v4_hmmss_hundredths_parses() {
        let body = "\
[Events]
Format: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: Marked=0,0:00:00.05,0:00:10.00,Default,,0000,0000,0000,,Hi
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 50_000_000); // 0.05 s
        assert_eq!(cues[0].end_ns, Some(10 * S));
    }

    #[test]
    fn malformed_time_line_is_skipped() {
        let body = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,not-a-time,0:00:03.00,Default,,0,0,0,,bad
Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,good
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "good");
    }

    #[test]
    fn dialogue_missing_field_in_file_is_skipped() {
        let body = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.00,Default
Dialogue: 0,0:00:04.00,0:00:06.00,Default,,0,0,0,,fine
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "fine");
    }

    #[test]
    fn styles_and_dialogue_extras_are_collected() {
        let body = "\
[Script Info]
PlayResX: 640
PlayResY: 480

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, Alignment
Style: Top,Arial,32,&H0000FFFF,8

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.00,Top,,12,0,48,,{\\i1}Hello
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello");

        let d = cues[0].ssa.as_deref().expect("dialogue extras attached");
        assert_eq!(d.raw_text, "{\\i1}Hello");
        assert_eq!(d.style, "Top");
        assert_eq!((d.margin_l, d.margin_r, d.margin_v), (12, 0, 48));

        let styles = p.ssa_styles().expect("SSA always exposes a registry");
        assert_eq!(styles.play_res(), (640.0, 480.0));
        let style = styles.lookup("Top").expect("style collected");
        assert_eq!(style.font_size, Some(32.0));
        assert_eq!(style.alignment, Some(8));
    }

    #[test]
    fn custom_event_format_resolves_style_and_margins() {
        let body = "\
[Events]
Format: Start, End, MarginV, Style, Text
Dialogue: 0:00:01.00,0:00:02.00,24,Alt,Hi there
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        let d = cues[0].ssa.as_deref().unwrap();
        assert_eq!(d.style, "Alt");
        assert_eq!(d.margin_v, 24);
        assert_eq!(d.margin_l, 0);
        assert_eq!(d.raw_text, "Hi there");
    }

    #[test]
    fn non_ssa_body_yields_no_cues() {
        let mut p = Ssa::default();
        let cues =
            p.parse("just some random text\nnot subtitles", &ParseContext::default()).unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(Ssa::default().output_format(), OutputFormat::PangoMarkup);
    }

    #[test]
    fn hours_over_one_digit() {
        assert_eq!(parse_ass_time("10:00:00.00"), Some(10 * 3600 * S));
    }

    #[test]
    fn huge_hour_saturates_and_does_not_panic() {
        assert_eq!(parse_ass_time("5124095:00:00.00"), Some(5_124_095 * 3600 * S));
        assert_eq!(parse_ass_time("5124096:00:00.00"), Some(u64::MAX));
    }

    #[test]
    fn fraction_add_saturates_and_does_not_panic() {
        assert_eq!(parse_ass_time("5124095:34:33.9"), Some(u64::MAX));
    }

    #[test]
    fn format_without_text_column_drops_dialogue() {
        let body = "\
[Events]
Format: Start, End
Dialogue: 0:00:01.00,0:00:02.00
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn format_with_text_last_keeps_commas() {
        let body = "\
[Events]
Format: Start, End, Text
Dialogue: 0:00:01.00,0:00:02.00,Hello, world, again
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
        assert_eq!(cues[0].text, "Hello, world, again");
    }

    #[test]
    fn format_with_text_not_last_takes_rest_of_line() {
        let body = "\
[Events]
Format: Start, End, Text, Effect
Dialogue: 0:00:01.00,0:00:02.00,Hello, world
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello, world");
    }

    #[test]
    fn format_with_timing_after_text_drops_dialogue() {
        let body = "\
[Events]
Format: Text, Start, End
Dialogue: Hello,0:00:01.00,0:00:02.00
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn format_trailing_comma_tolerated() {
        let body = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text,
Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello");
    }

    #[test]
    fn leading_bom_is_stripped() {
        let body = "\u{feff}[Events]\n\
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hi\n";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
    }

    #[test]
    fn section_header_with_trailing_content_is_recognized() {
        let body = "\
[Events] ; a comment
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hi
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
    }

    #[test]
    fn section_exit_with_trailing_content_is_recognized() {
        let body = "\
[Events]
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,kept
[Fonts] ; a comment
Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,dropped
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "kept");
    }

    #[test]
    fn scanner_resets_after_eos_tail_flush() {
        let ctx = ParseContext::default();
        let mut p = Ssa::default();
        let head = "[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,unterminated-tail";
        let parsed = p.parse_incremental(head, &ctx, true).unwrap();
        assert_eq!(parsed.consumed, head.len());
        assert_eq!(parsed.cues.len(), 1);

        let next = "Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,a\n\
                    Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,b\n";
        let parsed = p.parse_incremental(next, &ctx, false).unwrap();
        assert_eq!(parsed.cues.len(), 2);
        assert_eq!(parsed.cues[0].text, "a");
        assert_eq!(parsed.cues[1].text, "b");
    }

    #[test]
    fn a_dialogue_ending_before_it_starts_is_clamped() {
        let doc = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                   Dialogue: 0,0:00:05.00,0:00:01.00,Default,,0,0,0,,x\n";
        let cues = Ssa::default().parse(doc, &ParseContext::default()).expect("ssa parse");
        assert_eq!(cues.len(), 1);
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (5 * S, Some(5 * S)));
    }
}
