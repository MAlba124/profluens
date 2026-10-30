// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! SubRip (`.srt`) parser, a port of the C `parse_subrip`. See `specs/subrip.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

/// `allowed_srt_tags` in the C.
const ALLOWED_TAGS: [&[u8]; 3] = [b"i", b"b", b"u"];

const NS_PER_SECOND: u64 = 1_000_000_000;
const NS_PER_MSECOND: u64 = 1_000_000;

/// Parser for the SubRip (`.srt`) subtitle format.
#[derive(Debug, Default)]
pub struct SubRip {
    lines: LineScanner,
    machine: Machine,
    bom_checked: bool,
}

impl SubtitleFormat for SubRip {
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
        let mut consumed = skipped + lines.feed(rest, |line| machine.feed(line, &mut cues));

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
}

#[derive(Debug)]
struct Machine {
    state: State,
    /// The C's `state->start_time`, the previous cue's end. A time line ending before it is
    /// rejected.
    prev_end: u64,
    cur_start: u64,
    cur_end: u64,
    textbuf: String,
}

impl Default for Machine {
    fn default() -> Self {
        Machine { state: State::Id, prev_end: 0, cur_start: 0, cur_end: 0, textbuf: String::new() }
    }
}

impl Machine {
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        match self.state {
            State::Id => {
                if is_cue_id_line(line) {
                    self.state = State::Time;
                }
            }
            State::Time => match parse_timeline(line) {
                Some((ts_start, ts_end)) if self.prev_end <= ts_end => {
                    self.cur_start = ts_start;
                    self.cur_end = ts_end;
                    self.prev_end = ts_end;
                    self.state = State::Text;
                }
                _ => self.state = State::Id,
            },
            State::Text => {
                if !self.textbuf.is_empty() {
                    self.textbuf.push('\n');
                }
                self.textbuf.push_str(line);
                if line.is_empty() {
                    let text = markup_from_srt(&self.textbuf, &ALLOWED_TAGS);
                    // Source text for the cue-ir path, the pango transform keeps only <i>/<b>/<u>.
                    let raw = self.textbuf.trim_end_matches(['\n', '\r']).to_owned();
                    self.textbuf.clear();
                    // A reversed time line passes the guard, the C underflows the duration. The cue
                    // is clamped, the guard keeps the parsed end.
                    let end = self.cur_end.max(self.cur_start);
                    let mut cue = Cue::new(self.cur_start, Some(end), text);
                    cue.raw_text = Some(raw);
                    cues.push(cue);
                    self.state = State::Id;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Id,
    Time,
    Text,
}

/// Like the C's `g_ascii_strtoull`. An id if the number overflows (ERANGE) or consumes the line.
fn is_cue_id_line(line: &str) -> bool {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digit_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        return false;
    }
    if line[digit_start..i].parse::<u64>().is_err() {
        return true;
    }
    i == b.len()
}

fn parse_timeline(line: &str) -> Option<(u64, u64)> {
    let arrow = line.find(" --> ")?;
    // The start is the whole line (truncated at "-->" internally), like the C.
    let start = parse_subrip_time(line)?;
    let end = parse_subrip_time(&line[arrow + 5..])?;
    Some((start, end))
}

/// Port of `parse_subrip_time`.
fn parse_subrip_time(ts: &str) -> Option<u64> {
    let ts = ts.trim_start_matches(' ');
    let s = match ts.find("-->") {
        Some(i) => &ts[..i],
        None => ts,
    };
    // g_strchomp.
    let s = s.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    // g_strdelimit turns ' ' -> '0' and '.' -> ','.
    let munged: String = s
        .chars()
        .map(|c| match c {
            ' ' => '0',
            '.' => ',',
            other => other,
        })
        .collect();

    // sizeof("hhh:mm:ss,") == 11 guards against huge hours.
    let comma = munged.find(',')?;
    if comma > 11 {
        return None;
    }

    let mut ms_field = String::new();
    for c in munged[comma + 1..].chars().take(3) {
        ms_field.push(c);
    }
    while ms_field.len() < 3 {
        ms_field.push('0');
    }
    // The last conversion, so junk after its digits is not read (`,50A` -> 50 ms).
    let (msec, _) = scan_u(&ms_field)?;

    let mut parts = munged[..comma].split(':');
    let (hour, min, sec) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(m), Some(s), None) => (scan_u_field(h)?, scan_u_field(m)?, scan_u_field(s)?),
        (Some(m), Some(s), None, None) => (0, scan_u_field(m)?, scan_u_field(s)?),
        _ => return None,
    };

    // The C's `guint` fields wrap the seconds sum at 32 bits.
    let secs = hour.wrapping_mul(3600).wrapping_add(min.wrapping_mul(60)).wrapping_add(sec);
    Some(u64::from(secs) * NS_PER_SECOND + u64::from(msec) * NS_PER_MSECOND)
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

/// The C's escape, unescape-whitelist, drop-unknown, strip-newlines, balance.
fn markup_from_srt(text: &str, allowed: &[&[u8]]) -> String {
    let escaped = escape_markup(text);
    let mut bytes = unescape_formatting(escaped.as_bytes(), allowed);
    remove_unhandled_tags(&mut bytes);
    strip_trailing_newlines(&mut bytes);
    let bytes = fix_up_markup(bytes, allowed);
    // Transforms touch only ASCII tag boundaries, so UTF-8 stays intact.
    String::from_utf8(bytes).unwrap_or_default()
}

/// Port of `g_markup_escape_text`. Escapes `& < > ' "` and stray control chars.
fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            c if is_markup_control(c) => {
                out.push_str("&#x");
                push_lower_hex(&mut out, c as u32);
                out.push(';');
            }
            c => out.push(c),
        }
    }
    out
}

fn is_markup_control(c: char) -> bool {
    let c = c as u32;
    (0x1..=0x8).contains(&c)
        || (0xb..=0xc).contains(&c)
        || (0xe..=0x1f).contains(&c)
        || (0x7f..=0x84).contains(&c)
        || (0x86..=0x9f).contains(&c)
}

fn push_lower_hex(out: &mut String, mut v: u32) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    if v == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 8];
    let mut n = 0;
    while v != 0 {
        buf[n] = DIGITS[(v & 0xf) as usize];
        v >>= 4;
        n += 1;
    }
    for &d in buf[..n].iter().rev() {
        out.push(d as char);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Port of `subrip_unescape_formatting` without attributes.
fn unescape_formatting(input: &[u8], allowed: &[&[u8]]) -> Vec<u8> {
    if find(input, b"&lt;").is_none() {
        return input.to_vec();
    }

    let mut out = Vec::with_capacity(input.len());
    let mut p = 0;
    while p < input.len() {
        let lt = match find(&input[p..], b"&lt;") {
            None => {
                out.extend_from_slice(&input[p..]);
                break;
            }
            Some(rel) => p + rel,
        };
        out.extend_from_slice(&input[p..lt]);
        let after_lt = lt + 4;

        let gt = match find(&input[after_lt..], b"&gt;") {
            None => {
                out.extend_from_slice(&input[lt..]);
                break;
            }
            Some(rel) => after_lt + rel,
        };

        let mut ts = after_lt;
        let is_closing = ts < input.len() && input[ts] == b'/';
        if is_closing {
            ts += 1;
        }
        while ts < input.len() && (input[ts] == b' ' || input[ts] == b'\t') {
            ts += 1;
        }
        let name_start = ts;
        let mut te = ts;
        while te < input.len() && input[te].is_ascii_alphanumeric() {
            te += 1;
        }
        let name = &input[name_start..te];

        let is_allowed = allowed.contains(&name);
        if !is_allowed {
            out.extend_from_slice(&input[lt..gt + 4]);
            p = gt + 4;
            continue;
        }

        out.push(b'<');
        if is_closing {
            out.push(b'/');
        }
        out.extend_from_slice(name);
        out.push(b'>');
        p = gt + 4;
    }
    out
}

/// Port of `subrip_remove_unhandled_tags`. The `gt` cursor only moves forward, the C's rescan
/// is quadratic.
fn remove_unhandled_tags(buf: &mut Vec<u8>) {
    let src = std::mem::take(buf);
    let mut out = Vec::with_capacity(src.len());
    let mut gt = find(&src, b"&gt;");
    let mut pos = 0;

    while pos < src.len() {
        if src[pos..].starts_with(b"&lt;") {
            while let Some(g) = gt.filter(|&g| g < pos + 4) {
                gt = find(&src[g + 1..], b"&gt;").map(|rel| g + 1 + rel);
            }
            if let Some(g) = gt {
                let mut tag = pos + 4;
                if src.get(tag) == Some(&b'/') {
                    tag += 1;
                }
                if src.get(tag).is_some_and(|b| b.is_ascii_alphabetic()) {
                    // Resume after the tag, where the C's `--pos, ++pos` lands.
                    pos = g + 4;
                    continue;
                }
            }
        }
        out.push(src[pos]);
        pos += 1;
    }

    *buf = out;
}

/// Port of `strip_trailing_newlines`. Drops trailing '\n' but keeps >= 1 byte.
fn strip_trailing_newlines(buf: &mut Vec<u8>) {
    while buf.len() > 1 && buf[buf.len() - 1] == b'\n' {
        buf.pop();
    }
}

/// Port of `subrip_fix_up_markup` for the whitelist.
fn fix_up_markup(buf: Vec<u8>, allowed: &[&[u8]]) -> Vec<u8> {
    // The whitelist entry itself, so an open tag costs no allocation.
    let mut open_tags: Vec<&[u8]> = Vec::new();
    let mut cur = 0;
    let mut out: Option<Vec<u8>> = None;
    let mut copied = 0;

    while cur < buf.len() {
        let next_tag = match find(&buf[cur..], b"<") {
            None => break,
            Some(rel) => cur + rel,
        };

        let mut offset = 0usize;
        let mut is_closing = false;

        for &tag in allowed {
            let mut ts = next_tag + 1;
            is_closing = ts < buf.len() && buf[ts] == b'/';
            if is_closing {
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
            let name = &buf[name_start..te];

            if name.len() == tag.len() && name.eq_ignore_ascii_case(tag) {
                if matches!(buf.get(te), Some(b' ' | b'\t' | b'.')) {
                    while te < buf.len()
                        && buf[te] != b'>'
                        && (buf[te].is_ascii_alphanumeric()
                            || matches!(buf[te], b'.' | b' ' | b'\t' | b'(' | b')'))
                    {
                        te += 1;
                    }
                }
                if buf.get(te) == Some(&b'>') {
                    offset = te - (next_tag + 1);
                    if !is_closing {
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

        let tag_end = match find(&buf[next_tag..], b">") {
            Some(rel) => next_tag + rel,
            None => break,
        };
        let matches_last = match open_tags.last() {
            Some(last) => {
                let cmp = next_tag + 2;
                cmp + last.len() <= buf.len()
                    && buf[cmp..cmp + last.len()].eq_ignore_ascii_case(last)
            }
            None => false,
        };

        if !matches_last {
            out.get_or_insert_with(|| Vec::with_capacity(buf.len()))
                .extend_from_slice(&buf[copied..next_tag]);
            copied = tag_end + 1;
            cur = tag_end + 1;
            continue;
        }

        open_tags.pop();
        cur = tag_end + 1;
    }

    let mut buf = match out {
        Some(mut out) => {
            out.extend_from_slice(&buf[copied..]);
            out
        }
        None => buf,
    };
    while let Some(tag) = open_tags.pop() {
        buf.push(b'<');
        buf.push(b'/');
        buf.extend_from_slice(tag);
        buf.push(b'>');
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Cue> {
        SubRip::default().parse(body, &ParseContext::default()).expect("subrip parse is infallible")
    }

    fn one(body: &str) -> (u64, u64, String) {
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "expected exactly one cue for {body:?}");
        let c = &cues[0];
        (c.start_ns, c.end_ns.unwrap(), c.text.clone())
    }

    const SEC: u64 = NS_PER_SECOND;
    const MS: u64 = NS_PER_MSECOND;

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(SubRip::default().output_format(), OutputFormat::PangoMarkup);
    }

    #[test]
    fn raw_text_carries_the_source_verbatim() {
        let cues = parse(
            "1\n00:00:01,000 --> 00:00:02,000\n<font color=\"#ff0000\">Red</font> & <i>it</i>\nsecond line\n\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Red &amp; <i>it</i>\nsecond line");
        assert_eq!(
            cues[0].raw_text.as_deref(),
            Some("<font color=\"#ff0000\">Red</font> & <i>it</i>\nsecond line")
        );
    }

    #[test]
    fn basic_timing_and_text() {
        let (s, e, t) = one("1\n00:00:01,000 --> 00:00:02,000\nOne\n\n");
        assert_eq!((s, e, t.as_str()), (SEC, 2 * SEC, "One"));
    }

    #[test]
    fn hours_minutes_seconds() {
        let (s, e, _) = one("1\n01:02:03,004 --> 01:02:04,000\nx\n\n");
        assert_eq!(s, (3600 + 2 * 60 + 3) * SEC + 4 * MS);
        assert_eq!(e, (3600 + 2 * 60 + 4) * SEC);
    }

    #[test]
    fn minutes_seconds_only_defaults_hours_to_zero() {
        let (s, e, _) = one("1\n00:26,500 --> 00:28,000\nx\n\n");
        assert_eq!(s, 26 * SEC + 500 * MS);
        assert_eq!(e, 28 * SEC);
    }

    #[test]
    fn comma_or_dot_subsecond_separator() {
        let (s, e, _) = one("1\n00:00:01.250 --> 00:00:02.750\nx\n\n");
        assert_eq!((s, e), (SEC + 250 * MS, 2 * SEC + 750 * MS));
    }

    #[test]
    fn subsecond_digit_normalisation() {
        assert_eq!(one("1\n00:00:00,5 --> 00:00:01,0\nx\n\n").0, 500 * MS);
        assert_eq!(one("1\n00:00:00,05 --> 00:00:01,0\nx\n\n").0, 50 * MS);
        assert_eq!(one("1\n00:00:00,5000 --> 00:00:01,0\nx\n\n").0, 500 * MS);
    }

    #[test]
    fn spaces_become_zeros_in_timestamp() {
        let (s, e, t) = one("1\n 0: 0:26, 26 --> 0: 0:28, 17\nI cant see.\n\n");
        assert_eq!(s, 26 * SEC + 26 * MS);
        assert_eq!(e, 28 * SEC + 17 * MS);
        assert_eq!(t, "I cant see.");
    }

    #[test]
    fn extra_spaces_around_arrow() {
        let (s, e, _) = one("1\n00:00:02,5   --> 00:00:03,  5 \nTwo\n\n");
        assert_eq!((s, e), (2500 * MS, 3005 * MS));
    }

    #[test]
    fn short_fraction_followed_by_junk_reads_leading_digits() {
        let (s, e, t) = one("1\n00:00:01,000 --> 00:00:02,5 A:start\nOne\n\n");
        assert_eq!((s, e, t.as_str()), (SEC, 2 * SEC + 50 * MS, "One"));
        assert_eq!(parse_subrip_time("00:00:02,A"), None);
    }

    #[test]
    fn tab_in_timestamp_fields_is_skipped_like_sscanf() {
        assert_eq!(one("1\n\t00:00:01,000 --> 00:00:02,000\nOne\n\n").0, SEC);
        assert_eq!(one("1\n00:\t0:01,000 --> 00:00:02,000\nOne\n\n").0, SEC);
        assert_eq!(one("1\n00:00:01,\t5 --> 00:00:02,000\nOne\n\n").0, SEC + 50 * MS);
    }

    #[test]
    fn junk_inside_a_timestamp_field_rejects_the_cue() {
        let body = "1\n00:00:01x,000 --> 00:00:02,000\nJunk\n\n\
                    2\n00:00:03,000 --> 00:00:04,000\nGood\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Good");
        assert_eq!(parse_subrip_time("00:00:01x,000"), None);
        assert_eq!(parse_subrip_time("00:01x,000"), None);
    }

    #[test]
    fn signed_timestamp_field_matches_strtoul() {
        assert_eq!(parse_subrip_time("00:00:+1,000"), Some(SEC));
        assert_eq!(parse_subrip_time("00:00:-1,000"), Some(u64::from(u32::MAX) * SEC));
    }

    #[test]
    fn absurd_minute_field_wraps_at_32_bits() {
        assert_eq!(parse_subrip_time("99999999:00,000"), Some(1_705_032_644 * SEC));
        assert_eq!(parse_subrip_time("71582789:00,000"), Some(44 * SEC));
    }

    #[test]
    fn a_cue_ending_before_the_previous_one_is_dropped_with_its_text() {
        let body = "1\n00:00:05,000 --> 00:00:06,000\nA\n\n\
                    2\n00:00:01,000 --> 00:00:02,000\nB\n\n\
                    3\n00:00:07,000 --> 00:00:08,000\nC\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "the backwards cue is dropped: {cues:#?}");
        assert_eq!(cues[0].text, "A");
        assert_eq!(
            cues[1].text, "C",
            "B's TEXT goes with it rather than attaching to the next cue"
        );
        assert_eq!(cues[1].start_ns, 7 * SEC);
    }

    #[test]
    fn cues_sharing_an_end_time_both_survive() {
        let body = "1\n00:00:01,000 --> 00:00:05,000\nA\n\n\
                    2\n00:00:02,000 --> 00:00:05,000\nB\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "{cues:#?}");
        assert_eq!(cues[1].text, "B");
        assert_eq!(cues[1].end_ns, Some(5 * SEC));
    }

    #[test]
    fn reversed_timing_is_clamped_to_zero_length() {
        let cues = parse("1\n00:00:05,000 --> 00:00:02,000\nA\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 5 * SEC);
        assert_eq!(cues[0].end_ns, Some(5 * SEC));
        assert_eq!(cues[0].duration_ns(), Some(0));
    }

    #[test]
    fn cue_after_a_reversed_one_is_still_emitted() {
        let body = "1\n00:00:05,000 --> 00:00:02,000\nA\n\n\
                    2\n00:00:03,000 --> 00:00:04,000\nB\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        assert_eq!(cues[1].text, "B");
        assert_eq!((cues[1].start_ns, cues[1].end_ns), (3 * SEC, Some(4 * SEC)));
    }

    #[test]
    fn preserves_italic_markup() {
        assert_eq!(one("6\n00:00:06,000 --> 00:00:07,000\n<i>Six</i>\n\n").2, "<i>Six</i>");
    }

    #[test]
    fn closes_open_italic() {
        assert_eq!(one("7\n00:00:07,000 --> 00:00:08,000\n<i>Seven\n\n").2, "<i>Seven</i>");
    }

    #[test]
    fn closes_open_nested_tags_in_reverse() {
        assert_eq!(
            one("8\n00:00:08,000 --> 00:00:09,000\n<b><i>Eight\n\n").2,
            "<b><i>Eight</i></b>"
        );
    }

    #[test]
    fn stray_closing_tag_removed_to_empty() {
        assert_eq!(one("9\n00:00:09,000 --> 00:00:10,000\n</b>\n\n").2, "");
        assert_eq!(one("10\n00:00:10,000 --> 00:00:11,000\n</b></i>\n\n").2, "");
    }

    #[test]
    fn broken_markup_fixed_up() {
        assert_eq!(one("11\n00:00:11,000 --> 00:00:12,000\n<i>xyz</b></i>\n\n").2, "<i>xyz</i>");
        assert_eq!(one("12\n00:00:12,000 --> 00:00:13,000\n<i>xyz</b>\n\n").2, "<i>xyz</i>");
    }

    #[test]
    fn deeply_nested_italics_all_closed() {
        let inner = "<i>".repeat(33);
        let body = format!("13\n00:00:13,000 --> 00:00:14,000\n{inner}Keep them comiiiiiing\n\n");
        let expected = format!("{inner}Keep them comiiiiiing{}", "</i>".repeat(33));
        assert_eq!(one(&body).2, expected);
    }

    #[test]
    fn escapes_ampersand_but_keeps_allowed_markup() {
        assert_eq!(
            one("25\n00:03:00,000 --> 00:04:00,000\ngave <i>Rock & Roll</i> to\n\n").2,
            "gave <i>Rock &amp; Roll</i> to"
        );
        assert_eq!(
            one("26\n00:04:00,000 --> 00:05:00,000\n<i>Rock & Roll</i>\n\n").2,
            "<i>Rock &amp; Roll</i>"
        );
        assert_eq!(one("27\n00:06:00,000 --> 00:08:00,000\nRock & Roll\n\n").2, "Rock &amp; Roll");
    }

    #[test]
    fn font_and_unknown_tags_stripped_lt_escaped() {
        let body = "28\n00:10:00,000 --> 00:11:00,000\n\
                    <font \"#0000FF\"><joj>This is </xxx>in blue but <5</font>\n\n";
        assert_eq!(one(body).2, "This is in blue but &lt;5");
    }

    #[test]
    fn unhandled_tag_scan_with_a_distant_closing_marker() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<5<5<5 <font x>y>\n\n").2,
            "&lt;5&lt;5&lt;5 y&gt;"
        );
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<font <i <b\n\n").2,
            "&lt;font &lt;i &lt;b"
        );
    }

    #[test]
    fn closing_tag_with_space_recognised() {
        assert_eq!(
            one("29\n00:11:00,000 --> 00:12:00,000\n<i>italics</ i>\n\n").2,
            "<i>italics</i>"
        );
    }

    #[test]
    fn unrecognised_closing_tag_escaped_and_balanced() {
        assert_eq!(
            one("30\n00:12:00,000 --> 00:12:01,000\n<i>italics</ x>\n\n").2,
            "<i>italics&lt;/ x&gt;</i>"
        );
    }

    #[test]
    fn voice_tag_stripped() {
        assert_eq!(one("1\n00:00:01,000 --> 00:00:02,000\n<v>some text\n\n").2, "some text");
    }

    #[test]
    fn tag_with_attribute_dropped_keeps_bold() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<b.loud>some text\n\n").2,
            "<b>some text</b>"
        );
    }

    #[test]
    fn ruby_annotation_tags_stripped() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<ruby>base text<rt>annotation</rt></ruby>\n\n")
                .2,
            "base textannotation"
        );
    }

    #[test]
    fn multiple_cues() {
        let body = "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n\
                    2\n00:00:02,000 --> 00:00:03,000\nTwo\n\n\
                    3\n00:00:03,000 --> 00:00:04,000\nThree\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[1].text, "Two");
        assert_eq!(cues[2].text, "Three");
        assert_eq!(cues[1].start_ns, 2 * SEC);
        assert_eq!(cues[1].end_ns, Some(3 * SEC));
    }

    #[test]
    fn multi_line_text_joined_with_newline() {
        let (_, _, t) = one("1\n00:00:01,000 --> 00:00:02,000\nLine one\nLine two\n\n");
        assert_eq!(t, "Line one\nLine two");
    }

    #[test]
    fn cue_id_zero_is_valid() {
        assert_eq!(one("0\n00:00:01,000 --> 00:00:02,000\nOne\n\n").2, "One");
    }

    #[test]
    fn crlf_line_endings() {
        let (s, e, t) = one("1\r\n00:00:01,000 --> 00:00:02,000\r\nHello\r\n\r\n");
        assert_eq!((s, e, t.as_str()), (SEC, 2 * SEC, "Hello"));
    }

    #[test]
    fn leading_bom_stripped() {
        assert_eq!(
            one("\u{feff}1\n00:00:00,000 --> 00:00:03,50\nJust testing.\n\n").2,
            "Just testing."
        );
    }

    #[test]
    fn no_trailing_newline_still_emits() {
        let (s, e, t) = one("1\n00:00:01,000 --> 00:00:02,000\nLast cue, no newline at the end");
        assert_eq!((s, e), (SEC, 2 * SEC));
        assert_eq!(t, "Last cue, no newline at the end");
    }

    #[test]
    fn empty_cue_still_emitted() {
        let cues = parse("1\n00:00:01,000 --> 00:00:02,000\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "");
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (SEC, Some(2 * SEC)));
    }

    #[test]
    fn broken_end_timestamp_drops_cue_and_recovers() {
        let body = "2\n00:02:00,000 --> 00:03:0\n<v>some other text\n\n\
                    3\n00:00:03,000 --> 00:00:04,000\n<v>some more text\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "some more text");
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (3 * SEC, Some(4 * SEC)));
    }

    #[test]
    fn timeline_without_id_is_ignored() {
        let cues = parse("00:00:01,000 --> 00:00:02,000\nHello\n\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn non_numeric_id_line_ignored() {
        let cues = parse("not a number\nalso junk\n\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn empty_input_yields_no_cues() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n\n").is_empty());
    }

    #[test]
    fn apostrophe_and_quote_are_markup_escaped() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\ndon't \"stop\"\n\n").2,
            "don&apos;t &quot;stop&quot;"
        );
    }

    #[test]
    fn utf8_text_passes_through() {
        assert_eq!(one("1\n00:00:01,000 --> 00:00:02,000\ncafé — naïve\n\n").2, "café — naïve");
    }

    #[test]
    fn utf8_with_markup_boundaries_stay_valid() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<i>café</i> & <b>naïve\n\n").2,
            "<i>café</i> &amp; <b>naïve</b>"
        );
    }

    #[test]
    fn parse_subrip_time_examples() {
        assert_eq!(parse_subrip_time("00:00:01,000"), Some(SEC));
        assert_eq!(parse_subrip_time("01:00:00,000"), Some(3600 * SEC));
        assert_eq!(parse_subrip_time("00:03:00,50"), Some(3 * 60 * SEC + 500 * MS));
        assert_eq!(parse_subrip_time("00:03:0"), None);
        assert_eq!(parse_subrip_time("1234567890:00:00,000"), None);
    }

    #[test]
    fn is_cue_id_line_cases() {
        assert!(is_cue_id_line("1"));
        assert!(is_cue_id_line("0"));
        assert!(is_cue_id_line("  42"));
        assert!(!is_cue_id_line(""));
        assert!(!is_cue_id_line("1a"));
        assert!(!is_cue_id_line("00:00:01,000 --> 00:00:02,000"));
        assert!(!is_cue_id_line("hello"));
    }
}
