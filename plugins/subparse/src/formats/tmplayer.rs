// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! TMPlayer parser, a port of the C `tmplayer_parse_line`. See `specs/tmplayer.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;
const MAX_DURATION: u64 = 5 * GST_SECOND;

/// Parser for the TMPlayer subtitle format.
#[derive(Debug, Default)]
pub struct TmPlayer {
    lines: LineScanner,
    machine: State,
}

impl SubtitleFormat for TmPlayer {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let Self { lines, machine } = self;

        let mut cues = Vec::new();
        let mut consumed = lines.feed(body, at_eos, |line| machine.feed(line, &mut cues));

        if at_eos {
            let tail = &body[consumed..];
            let tail = tail.strip_suffix('\r').unwrap_or(tail);
            machine.feed(tail, &mut cues);
            machine.flush_eos(&mut cues);
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::Utf8
    }
}

#[derive(Debug, Default)]
struct State {
    start_time: u64,
    buf: String,
    /// The C's `state->state`. Closing the previous unit needs `line_num > 0`.
    line_num: u64,
}

impl State {
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        let line_num = self.line_num;
        self.line_num += 1;

        if let Some((ts, l, text)) = parse_multiline(line) {
            self.handle_timestamped(ts, l, text, line_num, cues);
        } else if let Some((ts, text)) = parse_single(line) {
            self.handle_timestamped(ts, 1, text, line_num, cues);
        } else if line.is_empty() && !self.buf.is_empty() {
            let start = self.start_time;
            self.emit(start, None, cues);
        }
    }

    fn handle_timestamped(
        &mut self,
        ts: u64,
        l: u64,
        text: &str,
        line_num: u64,
        cues: &mut Vec<Cue>,
    ) {
        let end_of_unit = text.is_empty() || (l == 1 && !self.buf.is_empty());

        if end_of_unit {
            if self.start_time < ts && line_num > 0 {
                let d = ts - self.start_time;
                let start = self.start_time;
                self.emit(start, Some(d), cues);
                // Advance by the unclamped duration so the next start lands on `ts`.
                self.start_time = start + d;
                if !text.is_empty() {
                    self.buf.push_str(text);
                }
            }
        } else {
            if l > 1 {
                self.buf.push('\n');
            }
            self.buf.push_str(text);
            self.start_time = ts;
        }
    }

    fn emit(&mut self, start: u64, dur: Option<u64>, cues: &mut Vec<Cue>) {
        let text = self.buf.replace('|', "\n");
        self.buf.clear();
        let end = dur.map(|d| start.saturating_add(d.min(MAX_DURATION)));
        cues.push(Cue::new(start, end, text));
    }

    fn flush_eos(&mut self, cues: &mut Vec<Cue>) {
        if !self.buf.is_empty() {
            let start = self.start_time;
            self.emit(start, None, cues);
        }
    }
}

fn parse_multiline(line: &str) -> Option<(u64, u64, &str)> {
    let b = line.as_bytes();
    let mut i = 0;
    let h = scan_uint(b, &mut i, 0)?;
    expect(b, &mut i, b':')?;
    let m = scan_uint(b, &mut i, 2)?;
    expect(b, &mut i, b':')?;
    let s = scan_uint(b, &mut i, 2)?;
    expect(b, &mut i, b',')?;
    let l = scan_uint(b, &mut i, 0)?;
    if b.get(i) != Some(&b'=') {
        return None;
    }
    i += 1;
    Some((to_ns(h, m, s), l, &line[i..]))
}

/// `HH:MM:SS<sep>text`. The C finds the text with `strchr (line + 6, divc)`, the first separator
/// at or after byte 6.
fn parse_single(line: &str) -> Option<(u64, &str)> {
    let b = line.as_bytes();
    let mut i = 0;
    let h = scan_uint(b, &mut i, 0)?;
    expect(b, &mut i, b':')?;
    let m = scan_uint(b, &mut i, 2)?;
    expect(b, &mut i, b':')?;
    let s = scan_uint(b, &mut i, 2)?;
    let sep = *b.get(i)?;
    if sep != b':' && sep != b'=' {
        return None;
    }
    // A matching line is at least six ASCII bytes (`0:0:0:`), so byte 6 is a char boundary.
    let text = match b.iter().skip(6).position(|&c| c == sep) {
        Some(rel) => &line[6 + rel + 1..],
        None => "",
    };
    Some((to_ns(h, m, s), text))
}

fn to_ns(h: u64, m: u64, s: u64) -> u64 {
    h.saturating_mul(3600)
        .saturating_add(m.saturating_mul(60))
        .saturating_add(s)
        .saturating_mul(GST_SECOND)
}

fn expect(b: &[u8], i: &mut usize, c: u8) -> Option<()> {
    if b.get(*i) == Some(&c) {
        *i += 1;
        Some(())
    } else {
        None
    }
}

fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn scan_uint(b: &[u8], i: &mut usize, max_digits: usize) -> Option<u64> {
    let entry = *i;
    while matches!(b.get(*i), Some(&c) if is_c_space(c)) {
        *i += 1;
    }
    let start = *i;
    let mut val: u64 = 0;
    let mut n = 0;
    while let Some(d @ b'0'..=b'9') = b.get(*i) {
        if max_digits != 0 && n == max_digits {
            break;
        }
        val = val.saturating_mul(10).saturating_add((d - b'0') as u64);
        *i += 1;
        n += 1;
    }
    if *i == start {
        *i = entry;
        None
    } else {
        Some(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn parse(body: &str) -> Vec<Cue> {
        TmPlayer::default().parse(body, &ParseContext::default()).unwrap()
    }

    #[test]
    fn huge_hour_saturates_and_does_not_panic() {
        let cues = parse("5555001070:0:00=hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, u64::MAX);
    }

    #[test]
    fn c_parity_multiline() {
        let body = "00:00:10,1=This is the Earth at a time\n\
00:00:10,2=when the dinosaurs roamed...\n\
00:00:13,1=\n\
00:00:14,1=a lush and fertile planet.\n\
00:00:16,1=\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
        assert_eq!(cues[0].text, "This is the Earth at a time\nwhen the dinosaurs roamed...");
        assert_eq!(cues[1].start_ns, 14 * S);
        assert_eq!(cues[1].end_ns, Some(16 * S));
        assert_eq!(cues[1].text, "a lush and fertile planet.");
    }

    #[test]
    fn c_parity_multiline_with_bogus_lines() {
        let body = "00:00:10,1=This is the Earth at a time\n\
Yooboo wabahablablahuguug bogus line hello test 1-2-3-4\n\
00:00:10,2=when the dinosaurs roamed...\n\
00:00:13,1=\n\
00:00:14,1=a lush and fertile planet.\n\
00:00:16,1=\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "This is the Earth at a time\nwhen the dinosaurs roamed...");
        assert_eq!(cues[1].text, "a lush and fertile planet.");
    }

    #[test]
    fn c_parity_style1_colon_two_digit_hour() {
        let body = "00:00:10:This is the Earth at a time|when the dinosaurs roamed...\n\
00:00:13:\n\
00:00:14:a lush and fertile planet.\n\
00:00:16:\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
        assert_eq!(cues[0].text, "This is the Earth at a time\nwhen the dinosaurs roamed...");
        assert_eq!(cues[1].start_ns, 14 * S);
        assert_eq!(cues[1].end_ns, Some(16 * S));
    }

    #[test]
    fn c_parity_style2_equals() {
        let body = "00:00:10=This is the Earth at a time|when the dinosaurs roamed...\n\
00:00:13=\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
    }

    #[test]
    fn c_parity_style3_one_digit_hour_colon() {
        let body = "0:00:10:This is the Earth at a time|when the dinosaurs roamed...\n\
0:00:13:\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
        assert_eq!(cues[0].text, "This is the Earth at a time\nwhen the dinosaurs roamed...");
    }

    #[test]
    fn c_parity_style4_one_digit_hour_equals() {
        let body = "0:00:10=This is the Earth at a time|when the dinosaurs roamed...\n\
0:00:13=\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
    }

    #[test]
    fn c_parity_style4_with_bogus_lines() {
        let body = [
            "0:00:10=This is the Earth at a time|when the dinosaurs roamed...",
            "# This is a bogus line with a comment and should just be skipped",
            "0:00:13=",
            "0:00:14=a lush and fertile planet.",
            "                                                            ",
            "0:00:16=",
            "",
        ]
        .join("\n");
        let cues = parse(&body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "This is the Earth at a time\nwhen the dinosaurs roamed...");
        assert_eq!(cues[1].text, "a lush and fertile planet.");
    }

    #[test]
    fn c_parity_style3b_clamp_and_final_flush() {
        let body = "0:00:10:This is the Earth at a time|when the dinosaurs roamed...\n\
0:00:14:a lush and fertile planet.\n\
0:00:16:And they liked it a lot.\n\
0:00:30:Last line.";
        let cues = parse(body);
        assert_eq!(cues.len(), 4);

        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(14 * S));

        assert_eq!(cues[1].start_ns, 14 * S);
        assert_eq!(cues[1].end_ns, Some(16 * S));

        assert_eq!(cues[2].start_ns, 16 * S);
        assert_eq!(cues[2].end_ns, Some((16 + 5) * S));
        assert_eq!(cues[2].text, "And they liked it a lot.");

        assert_eq!(cues[3].start_ns, 30 * S);
        assert_eq!(cues[3].end_ns, None);
        assert_eq!(cues[3].text, "Last line.");
    }

    #[test]
    fn c_parity_whitespace_inside_the_timestamp() {
        let body = "00: 0:10:Hello|there\n00:00:13:\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
        assert_eq!(cues[0].text, "Hello\nthere");
    }

    #[test]
    fn c_parity_text_starts_at_the_first_separator_from_byte_six() {
        assert!(parse("0:0:1:text\n").is_empty());

        let cues = parse("100:00:10:text\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 360_010 * S);
        assert_eq!(cues[0].end_ns, None);
        assert_eq!(cues[0].text, "10:text");
    }

    #[test]
    fn final_unterminated_line_is_flushed() {
        let cues = parse("0:00:05:hello");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 5 * S);
        assert_eq!(cues[0].end_ns, None);
        assert_eq!(cues[0].text, "hello");
    }

    #[test]
    fn pipe_becomes_newline() {
        let cues = parse("0:00:01:a|b|c\n0:00:02:\n");
        assert_eq!(cues[0].text, "a\nb\nc");
    }

    #[test]
    fn no_escaping_of_markup_chars() {
        let cues = parse("0:00:01:a<b>&'c\n0:00:02:\n");
        assert_eq!(cues[0].text, "a<b>&'c");
    }

    #[test]
    fn empty_body_yields_no_cues() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn only_bogus_lines_yield_no_cues() {
        assert!(parse("hello\nworld\n").is_empty());
    }

    #[test]
    fn output_format_is_utf8() {
        assert_eq!(TmPlayer::default().output_format(), OutputFormat::Utf8);
    }
}
