// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! MPL2 parser, a port of the C `mpl2_parse_line`. See `specs/mpl2.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const DECISECOND_NS: u64 = 100_000_000;

/// Parser for the MPL2 subtitle format.
#[derive(Debug, Default)]
pub struct Mpl2 {
    lines: LineScanner,
}

impl SubtitleFormat for Mpl2 {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let mut cues = Vec::new();
        let mut consumed = self.lines.feed(body, at_eos, |line| parse_line(line, &mut cues));

        if at_eos {
            let tail = &body[consumed..];
            let tail = tail.strip_suffix('\r').unwrap_or(tail);
            parse_line(tail, &mut cues);
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::PangoMarkup
    }
}

fn parse_line(line: &str, cues: &mut Vec<Cue>) {
    let Some((dc_start, dc_stop)) = parse_timestamps(line) else {
        return;
    };

    let Some(text) = text_after_two_brackets(line) else {
        return;
    };

    let start_ns = dc_start.saturating_mul(DECISECOND_NS);
    // An end before the start would make a zero-length cue that never shows.
    let end_ns = dc_stop.saturating_mul(DECISECOND_NS).max(start_ns);

    let markup = build_markup(text);
    cues.push(Cue::new(start_ns, Some(end_ns), markup));
}

/// The C's `sscanf (line, "[%u][%u]") != 2`. The final `]` cannot change the count.
fn parse_timestamps(line: &str) -> Option<(u64, u64)> {
    let b = line.as_bytes();
    let mut i = 0;
    let start = scan_bracketed_uint(b, &mut i, true)?;
    let stop = scan_bracketed_uint(b, &mut i, false)?;
    Some((start, stop))
}

/// One `[%u` field. A `-` wraps to garbage in the C, here it reads as 0.
fn scan_bracketed_uint(b: &[u8], i: &mut usize, need_close: bool) -> Option<u64> {
    if b.get(*i) != Some(&b'[') {
        return None;
    }
    *i += 1;
    while matches!(b.get(*i), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)) {
        *i += 1;
    }
    let negative = match b.get(*i) {
        Some(b'+') => {
            *i += 1;
            false
        }
        Some(b'-') => {
            *i += 1;
            true
        }
        _ => false,
    };
    let start = *i;
    let mut val: u64 = 0;
    while let Some(d @ b'0'..=b'9') = b.get(*i) {
        val = val.saturating_mul(10).saturating_add((d - b'0') as u64);
        *i += 1;
    }
    if *i == start {
        return None;
    }
    if need_close {
        if b.get(*i) != Some(&b']') {
            return None;
        }
        *i += 1;
    }
    Some(if negative { 0 } else { val })
}

fn text_after_two_brackets(line: &str) -> Option<&str> {
    let first = line.find(']')?;
    let second = line[first + 1..].find(']')? + first + 1;
    Some(&line[second + 1..])
}

fn build_markup(text: &str) -> String {
    let mut markup = String::with_capacity(text.len() + 8);
    let mut rest = text;

    loop {
        // Only spaces and tabs, per the C.
        rest = rest.trim_start_matches([' ', '\t']);

        let italic = rest.starts_with('/');
        if italic {
            markup.push_str("<i>");
            rest = &rest[1..];
        }

        let (chunk_src, next) = match rest.find('|') {
            Some(i) => (&rest[..i], Some(i + 1)),
            None => (rest, None),
        };

        markup_escape(chunk_src, &mut markup);

        if italic {
            markup.push_str("</i>");
        }

        match next {
            Some(off) => {
                markup.push('\n');
                rest = &rest[off..];
            }
            None => break,
        }
    }

    strip_ascii_ws(&markup).to_string()
}

fn strip_ascii_ws(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}'))
}

fn markup_escape(s: &str, out: &mut String) {
    for c in s.chars() {
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
                    out.push_str("&#x");
                    push_hex(u, out);
                    out.push(';');
                } else {
                    out.push(c);
                }
            }
        }
    }
}

fn push_hex(mut v: u32, out: &mut String) {
    if v == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 8];
    let mut n = 0;
    while v > 0 {
        buf[n] = char::from_digit(v & 0xf, 16).unwrap() as u8;
        v >>= 4;
        n += 1;
    }
    for &d in buf[..n].iter().rev() {
        out.push(d as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn parse(body: &str) -> Vec<Cue> {
        Mpl2::default().parse(body, &ParseContext::default()).unwrap()
    }

    #[test]
    fn c_parity_full() {
        let body = "[123][456] This is the Earth at a time|when the dinosaurs roamed\n\
[1234][5678]a lush and fertile planet.\n\
[12345][27890] /Italic|Normal\n\
[32345][37890]/Italic|/Italic\n\
[42345][47890] Normal|/Italic";
        let cues = parse(body);
        assert_eq!(cues.len(), 5);

        assert_eq!(cues[0].start_ns, 123 * S / 10);
        assert_eq!(cues[0].end_ns, Some(456 * S / 10));
        assert_eq!(cues[0].text, "This is the Earth at a time\nwhen the dinosaurs roamed");

        assert_eq!(cues[1].start_ns, 1234 * S / 10);
        assert_eq!(cues[1].end_ns, Some(5678 * S / 10));
        assert_eq!(cues[1].text, "a lush and fertile planet.");

        assert_eq!(cues[2].start_ns, 12345 * S / 10);
        assert_eq!(cues[2].end_ns, Some(27890 * S / 10));
        assert_eq!(cues[2].text, "<i>Italic</i>\nNormal");

        assert_eq!(cues[3].text, "<i>Italic</i>\n<i>Italic</i>");

        assert_eq!(cues[4].start_ns, 42345 * S / 10);
        assert_eq!(cues[4].end_ns, Some(47890 * S / 10));
        assert_eq!(cues[4].text, "Normal\n<i>Italic</i>");
    }

    #[test]
    fn deciseconds_timing() {
        let cues = parse("[5][15]hi\n");
        assert_eq!(cues[0].start_ns, 500_000_000);
        assert_eq!(cues[0].end_ns, Some(1_500_000_000));
    }

    #[test]
    fn escapes_metacharacters() {
        let cues = parse("[1][2]a<b>&'c\n");
        assert_eq!(cues[0].text, "a&lt;b&gt;&amp;&apos;c");
    }

    #[test]
    fn escapes_c1_controls_like_glib() {
        let cues = parse("[1][2]a\u{80}b\u{85}c\u{9f}d\u{7f}\n");
        assert_eq!(cues[0].text, "a&#x80;b\u{85}c&#x9f;d&#x7f;");
    }

    #[test]
    fn trailing_whitespace_stripped() {
        let cues = parse("[1][2]text   \n");
        assert_eq!(cues[0].text, "text");
    }

    #[test]
    fn optional_space_after_bracket() {
        assert_eq!(parse("[1][2] hi\n")[0].text, "hi");
        assert_eq!(parse("[1][2]hi\n")[0].text, "hi");
    }

    #[test]
    fn empty_text_yields_empty_cue() {
        let cues = parse("[1][2]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "");
    }

    #[test]
    fn c_parity_junk_after_the_second_timestamp() {
        let cues = parse("[123][456 x]y\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 123 * S / 10);
        assert_eq!(cues[0].end_ns, Some(456 * S / 10));
        assert_eq!(cues[0].text, "y");
    }

    #[test]
    fn c_parity_signed_timestamp() {
        let cues = parse("[+123][456]y\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 123 * S / 10);
        assert_eq!(cues[0].end_ns, Some(456 * S / 10));
        assert_eq!(cues[0].text, "y");

        let cues = parse("[-123][456]y\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 0);
        assert_eq!(cues[0].end_ns, Some(456 * S / 10));
    }

    #[test]
    fn missing_first_closing_bracket_is_not_a_record() {
        assert!(parse("[123 x][456]y\n").is_empty());
    }

    #[test]
    fn malformed_lines_skipped() {
        let cues = parse("not mpl2\n[1][2]ok\n[bad]\n[3][4]two\n");
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "ok");
        assert_eq!(cues[1].text, "two");
    }

    #[test]
    fn empty_body_yields_no_cues() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(Mpl2::default().output_format(), OutputFormat::PangoMarkup);
    }

    #[test]
    fn an_end_before_the_start_is_clamped_to_the_start() {
        let cues = parse("[50][10]x\n");
        assert_eq!(cues.len(), 1);
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (5 * S, Some(5 * S)));
    }
}
