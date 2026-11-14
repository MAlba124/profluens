// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! MPSub parser, a port of the C `parse_mpsub`. See `specs/mpsub.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;

/// Parser for the MPSub subtitle format.
#[derive(Debug, Default)]
pub struct MpSub {
    lines: LineScanner,
    machine: Machine,
}

impl SubtitleFormat for MpSub {
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
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::Utf8
    }
}

#[derive(Debug, Default)]
struct Machine {
    state: u8,
    start_ns: u64,
    duration_ns: u64,
    buf: String,
}

impl Machine {
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        if self.state == 0 {
            if let Some((t1, t2)) = parse_two_floats(line) {
                self.state = 1;
                // start_time += duration + GST_SECOND * t1, in f32.
                let inc = self.duration_ns as f32 + GST_SECOND as f32 * t1;
                self.start_ns = (self.start_ns as f32 + inc) as u64;
                self.duration_ns = (GST_SECOND as f32 * t2) as u64;
            }
        } else {
            if !self.buf.is_empty() {
                self.buf.push('\n');
            }
            self.buf.push_str(line);
            if line.is_empty() {
                let text = std::mem::take(&mut self.buf);
                self.state = 0;
                cues.push(Cue::new(
                    self.start_ns,
                    Some(self.start_ns.saturating_add(self.duration_ns)),
                    text,
                ));
                // The driver's post-push `start_time += duration`, in `u64` like the C.
                self.start_ns = self.start_ns.saturating_add(self.duration_ns);
            }
        }
    }
}

fn parse_two_floats(line: &str) -> Option<(f32, f32)> {
    let b = line.as_bytes();
    let mut pos = 0;
    let t1 = read_float(b, &mut pos)?;
    let t2 = read_float(b, &mut pos)?;
    Some((t1, t2))
}

fn read_float(b: &[u8], pos: &mut usize) -> Option<f32> {
    while *pos < b.len() && b[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
    let start = *pos;

    if *pos < b.len() && (b[*pos] == b'+' || b[*pos] == b'-') {
        *pos += 1;
    }
    let mut digits = false;
    while *pos < b.len() && b[*pos].is_ascii_digit() {
        *pos += 1;
        digits = true;
    }
    if *pos < b.len() && b[*pos] == b'.' {
        *pos += 1;
        while *pos < b.len() && b[*pos].is_ascii_digit() {
            *pos += 1;
            digits = true;
        }
    }
    if !digits {
        *pos = start;
        return None;
    }
    if *pos < b.len() && (b[*pos] == b'e' || b[*pos] == b'E') {
        let save = *pos;
        *pos += 1;
        if *pos < b.len() && (b[*pos] == b'+' || b[*pos] == b'-') {
            *pos += 1;
        }
        let mut exp_digits = false;
        while *pos < b.len() && b[*pos].is_ascii_digit() {
            *pos += 1;
            exp_digits = true;
        }
        if !exp_digits {
            *pos = save;
        }
    }

    std::str::from_utf8(&b[start..*pos]).ok()?.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Cue> {
        MpSub::default().parse(body, &ParseContext::default()).unwrap()
    }

    #[test]
    fn output_format_is_utf8() {
        assert_eq!(MpSub::default().output_format(), OutputFormat::Utf8);
    }

    #[test]
    fn relative_timing_accumulates() {
        let body = "FORMAT=TIME\n2.0 3.0\nHello\n\n1.0 2.0\nWorld\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);

        assert_eq!(cues[0].start_ns, 2 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(5 * GST_SECOND));
        assert_eq!(cues[0].text, "Hello\n");

        assert_eq!(cues[1].start_ns, 8_999_999_488);
        assert_eq!(cues[1].end_ns, Some(10_999_999_488));
        assert_eq!(cues[1].text, "World\n");
    }

    #[test]
    fn c_parity_duration_is_counted_twice_per_cue() {
        // The double count, pinned. The format's own arithmetic would give 2 s, 6 s and 9 s, the C
        // 2, 9, 14.
        let body = "FORMAT=TIME\n2.0 3.0\nHello\n\n1.0 2.0\nWorld\n\n1.0 2.0\nThird\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].start_ns, 2 * GST_SECOND);
        assert_eq!(cues[1].start_ns, 8_999_999_488);
        assert_eq!(cues[2].start_ns, 13_999_998_976);
    }

    #[test]
    fn f32_accumulation_is_faithful() {
        let body = "2.0 3.0\nx\n\n1.5 2.0\ny\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[1].start_ns, 9_500_000_256);
        assert_eq!(cues[1].end_ns, Some(11_500_000_256));
    }

    #[test]
    fn multiline_text_preserves_trailing_newline() {
        let body = "0.0 4.0\nLine1\nLine2\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 0);
        assert_eq!(cues[0].end_ns, Some(4 * GST_SECOND));
        assert_eq!(cues[0].text, "Line1\nLine2\n");
    }

    #[test]
    fn header_lines_are_skipped() {
        let body = "FORMAT=TIME\n\n5.0 1.0\nHi\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 5 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(6 * GST_SECOND));
    }

    #[test]
    fn single_float_line_is_not_a_cue() {
        let body = "10\nnot text\n\n";
        let cues = parse(body);
        assert!(cues.is_empty());
    }

    #[test]
    fn fractional_offsets_and_durations() {
        let body = "1.5 0.5\nx\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 1_500_000_000);
        assert_eq!(cues[0].end_ns, Some(2_000_000_000));
    }

    #[test]
    fn unterminated_final_cue_is_dropped() {
        let body = "2.0 3.0\nHello\n";
        let cues = parse(body);
        assert!(cues.is_empty());
    }

    #[test]
    fn empty_body() {
        assert!(parse("").is_empty());
    }
}
