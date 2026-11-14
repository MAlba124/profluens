// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! DKS parser, a port of the C `parse_dks`. See `specs/dks.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;

/// Parser for the DKS subtitle format.
#[derive(Debug, Default)]
pub struct Dks {
    lines: LineScanner,
    machine: Machine,
}

impl SubtitleFormat for Dks {
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
    buf: String,
}

impl Machine {
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        if self.state == 0 {
            if let Some((h, m, s)) = parse_timestamp(line) {
                self.start_ns = hms_to_ns(h, m, s);
                let text = skip_timestamp(line);
                if !text.is_empty() {
                    self.state = 1;
                    self.buf.push_str(text);
                }
            }
        } else {
            if let Some((h, m, s)) = parse_timestamp(line) {
                self.state = 0;
                let end_ns = hms_to_ns(h, m, s);
                let dur = end_ns.saturating_sub(self.start_ns);
                let text = unescape_newlines_br(&self.buf);
                self.buf.clear();
                cues.push(Cue::new(self.start_ns, Some(self.start_ns.saturating_add(dur)), text));
            }
        }
    }
}

fn hms_to_ns(h: u64, m: u64, s: u64) -> u64 {
    h.saturating_mul(3600)
        .saturating_add(m.saturating_mul(60))
        .saturating_add(s)
        .saturating_mul(GST_SECOND)
}

/// A leading `[%u:%u:%u]`. The `]` is not required, sscanf stops after the last conversion.
fn parse_timestamp(line: &str) -> Option<(u64, u64, u64)> {
    let b = line.as_bytes();
    let mut pos = 0;
    if !lit(b, &mut pos, b'[') {
        return None;
    }
    let h = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b':') {
        return None;
    }
    let m = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b':') {
        return None;
    }
    let s = read_uint(b, &mut pos)?;
    Some((h, m, s))
}

fn skip_timestamp(line: &str) -> &str {
    match line.find(']') {
        Some(i) => &line[i + 1..],
        None => "",
    }
}

fn unescape_newlines_br(text: &str) -> String {
    let b = text.as_bytes();
    if b.len() < 4 {
        return text.to_string();
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b.len() - i >= 4 && &b[i..i + 4] == b"[br]" {
            out.push(b'\n');
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).expect("ascii-only substitution preserves utf8")
}

fn read_uint(b: &[u8], pos: &mut usize) -> Option<u64> {
    while *pos < b.len() && b[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
    let mut val: u64 = 0;
    let mut n = 0;
    while *pos < b.len() && b[*pos].is_ascii_digit() {
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
        Dks::default().parse(body, &ParseContext::default()).unwrap()
    }

    #[test]
    fn output_format_is_utf8() {
        assert_eq!(Dks::default().output_format(), OutputFormat::Utf8);
    }

    #[test]
    fn c_parity_two_cues() {
        let body = concat!(
            "[00:00:07]THERE IS A PLACE ON EARTH WHERE IT[br]IS STILL THE MORNING OF LIFE...\n",
            "[00:00:12]\n",
            "[00:00:13]AND THE GREAT HERDS RUN FREE.[br]SO WHAT?!\n",
            "[00:00:15]\n",
        );
        let cues = parse(body);
        assert_eq!(cues.len(), 2);

        assert_eq!(cues[0].start_ns, 7 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(12 * GST_SECOND));
        assert_eq!(
            cues[0].text,
            "THERE IS A PLACE ON EARTH WHERE IT\nIS STILL THE MORNING OF LIFE..."
        );

        assert_eq!(cues[1].start_ns, 13 * GST_SECOND);
        assert_eq!(cues[1].end_ns, Some(15 * GST_SECOND));
        assert_eq!(cues[1].text, "AND THE GREAT HERDS RUN FREE.\nSO WHAT?!");
    }

    #[test]
    fn multi_digit_hours() {
        let cues = parse("[01:02:03]text\n[01:02:05]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, (3600 + 120 + 3) * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some((3600 + 120 + 5) * GST_SECOND));
    }

    #[test]
    fn start_line_with_empty_text_stays_in_state0() {
        let cues = parse("[00:00:05]\n[00:00:07]real text\n[00:00:10]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 7 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(10 * GST_SECOND));
        assert_eq!(cues[0].text, "real text");
    }

    #[test]
    fn non_timestamp_end_line_is_skipped() {
        let cues = parse("[00:00:01]hi\nnot a timestamp\n[00:00:04]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].end_ns, Some(4 * GST_SECOND));
        assert_eq!(cues[0].text, "hi");
    }

    #[test]
    fn end_before_start_does_not_panic() {
        let cues = parse("[00:00:10]x\n[00:00:05]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(10 * GST_SECOND));
    }

    #[test]
    fn text_shorter_than_br_marker() {
        let cues = parse("[00:00:01]x\n[00:00:02]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "x");
    }

    #[test]
    fn unterminated_final_line_drops_pending_cue() {
        let cues = parse("[00:00:01]pending\n[00:00:02]");
        assert!(cues.is_empty());
    }

    #[test]
    fn empty_body() {
        assert!(parse("").is_empty());
    }
}
