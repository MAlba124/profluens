// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! LRC (lyrics) parser, a port of the C `parse_lrc`. See `specs/lrc.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;
const GST_MSECOND: u64 = 1_000_000;

/// Parser for the LRC lyric subtitle format.
#[derive(Debug, Default)]
pub struct Lrc {
    lines: LineScanner,
}

impl SubtitleFormat for Lrc {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let mut cues = Vec::new();
        let mut consumed = self.lines.feed(body, at_eos, |line| {
            if let Some(cue) = parse_line(line) {
                cues.push(cue);
            }
        });

        if at_eos {
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::Utf8
    }
}

fn parse_line(line: &str) -> Option<Cue> {
    let b = line.as_bytes();
    if b.first() != Some(&b'[') {
        return None;
    }

    let mut pos = 1; // past '['
    let m = read_uint(b, &mut pos, None)?;
    if !lit(b, &mut pos, b':') {
        return None;
    }
    let s = read_uint(b, &mut pos, Some(2))?;
    if !lit(b, &mut pos, b'.') {
        return None;
    }
    let c = read_uint(b, &mut pos, Some(3))?;

    let bracket = line.find(']')?;
    // Upstream quirk, ']' at byte 9 (the canonical "[mm:ss.ff]") means centiseconds, any other
    // offset milliseconds.
    let milli = if bracket == 9 { 10 } else { 1 };

    let start_ns = (m.saturating_mul(60).saturating_mul(GST_SECOND))
        .saturating_add(s.saturating_mul(GST_SECOND))
        .saturating_add(c.saturating_mul(milli).saturating_mul(GST_MSECOND));

    let text = &line[bracket + 1..];
    Some(Cue::new(start_ns, None, text))
}

fn read_uint(b: &[u8], pos: &mut usize, max_width: Option<usize>) -> Option<u64> {
    while *pos < b.len() && b[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
    let mut val: u64 = 0;
    let mut n = 0;
    while *pos < b.len() && b[*pos].is_ascii_digit() {
        if max_width.is_some_and(|w| n >= w) {
            break;
        }
        val = val.saturating_mul(10).saturating_add((b[*pos] - b'0') as u64);
        *pos += 1;
        n += 1;
    }
    if n == 0 { None } else { Some(val) }
}

fn lit(b: &[u8], pos: &mut usize, c: u8) -> bool {
    if *pos < b.len() && b[*pos] == c {
        *pos += 1;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Cue> {
        Lrc::default().parse(body, &ParseContext::default()).unwrap()
    }

    #[test]
    fn output_format_is_utf8() {
        assert_eq!(Lrc::default().output_format(), OutputFormat::Utf8);
    }

    #[test]
    fn two_digit_fraction_is_centiseconds() {
        let cues = parse("[01:02.34]Hello\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 62 * GST_SECOND + 340 * GST_MSECOND);
        assert_eq!(cues[0].end_ns, None);
        assert_eq!(cues[0].text, "Hello");
    }

    #[test]
    fn three_digit_fraction_is_milliseconds() {
        let cues = parse("[01:02.345]Hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 62 * GST_SECOND + 345 * GST_MSECOND);
        assert_eq!(cues[0].text, "Hi");
    }

    #[test]
    fn multi_digit_minutes_shift_the_fraction_unit() {
        let cues = parse("[123:04.05]late\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 123 * 60 * GST_SECOND + 4 * GST_SECOND + 5 * GST_MSECOND);
    }

    #[test]
    fn empty_lyric_text_is_kept() {
        let cues = parse("[00:10.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "");
        assert_eq!(cues[0].start_ns, 10 * GST_SECOND);
    }

    #[test]
    fn multiple_lines() {
        let cues = parse("[00:01.00]one\n[00:02.00]two\n[00:03.00]three\n");
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].text, "one");
        assert_eq!(cues[1].text, "two");
        assert_eq!(cues[2].text, "three");
        assert_eq!(cues[1].start_ns, 2 * GST_SECOND);
    }

    #[test]
    fn non_bracket_lines_are_skipped() {
        let cues = parse("ti:Title\n[00:05.00]sing\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "sing");
    }

    #[test]
    fn bracket_line_without_close_is_skipped() {
        let cues = parse("[00:05.00 no close\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn text_after_timestamp_may_contain_brackets() {
        let cues = parse("[00:00.50]a]b\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "a]b");
    }

    #[test]
    fn unterminated_final_line_is_dropped() {
        let cues = parse("[00:01.00]kept\n[00:02.00]dropped");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "kept");
    }

    #[test]
    fn crlf_is_tolerated() {
        let cues = parse("[00:01.00]win\r\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "win");
    }

    #[test]
    fn empty_body() {
        assert!(parse("").is_empty());
    }
}
