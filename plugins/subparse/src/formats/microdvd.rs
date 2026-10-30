// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! MicroDVD (`.sub`) parser, a port of the C `parse_mdvdsub`. See `specs/microdvd.md`.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;

/// Parser for the MicroDVD subtitle format.
#[derive(Debug, Default)]
pub struct MicroDvd {
    lines: LineScanner,
    fps: Option<(u64, u64)>,
}

impl SubtitleFormat for MicroDvd {
    fn parse_incremental(
        &mut self,
        body: &str,
        ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let fps = self.fps.get_or_insert(match ctx.fps {
            Some((n, d)) if n != 0 && d != 0 => (n as u64, d as u64),
            _ => (24000, 1001),
        });

        let mut cues = Vec::new();
        let mut consumed = self.lines.feed(body, |line| parse_line(line, fps, &mut cues));

        if at_eos {
            // The remainder is dropped like `get_next_line`, the C force-feeds no EOS flush for
            // MicroDVD.
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::PangoMarkup
    }
}

fn parse_line(line: &str, fps: &mut (u64, u64), cues: &mut Vec<Cue>) {
    let Some((start_frame, end_frame, text_off)) = parse_two_braced(line) else {
        return;
    };
    let text = &line[text_off..];

    // `{1}{1}` is the fps header, never a subtitle. Comma is a decimal separator.
    if start_frame == 1 && end_frame == 1 {
        if let Some((n, d)) = parse_fps(text) {
            *fps = (n, d);
        }
        return;
    }

    let (fps_n, fps_d) = *fps;
    // Truncating like gst_util_uint64_scale.
    let start_ns = scale(start_frame, GST_SECOND * fps_d, fps_n);
    let dur_ns = scale(end_frame.saturating_sub(start_frame), GST_SECOND * fps_d, fps_n);
    let end_ns = start_ns.saturating_add(dur_ns);

    // A `{s:...}` missing its brace makes the C drop the whole line.
    if let Some(markup) = build_markup(text) {
        cues.push(Cue::new(start_ns, Some(end_ns), markup));
    }
}

fn parse_two_braced(line: &str) -> Option<(u64, u64, usize)> {
    let b = line.as_bytes();
    let mut i = 0;
    let start = scan_braced_uint(b, &mut i)?;
    // The second '}' is not required. sscanf completed both %u before the literal can fail.
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    skip_c_space(b, &mut i);
    let end = scan_uint(b, &mut i)?;
    // No '}' left is UB in the C (strchr NULL + 1), dropping the line is the sane reading.
    let close = b[i..].iter().position(|&c| c == b'}')? + i;
    Some((start, end, close + 1))
}

fn scan_braced_uint(b: &[u8], i: &mut usize) -> Option<u64> {
    if b.get(*i) != Some(&b'{') {
        return None;
    }
    *i += 1;
    skip_c_space(b, i);
    let val = scan_uint(b, i)?;
    if b.get(*i) != Some(&b'}') {
        return None;
    }
    *i += 1;
    Some(val)
}

fn skip_c_space(b: &[u8], i: &mut usize) {
    while matches!(b.get(*i), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)) {
        *i += 1;
    }
}

fn scan_uint(b: &[u8], i: &mut usize) -> Option<u64> {
    let start = *i;
    let mut val: u64 = 0;
    while let Some(d @ b'0'..=b'9') = b.get(*i) {
        val = val.saturating_mul(10).saturating_add((d - b'0') as u64);
        *i += 1;
    }
    if *i == start { None } else { Some(val) }
}

/// `gst_util_uint64_scale`, 128-bit `val * num / denom` truncated. Saturates like upstream.
fn scale(val: u64, num: u64, denom: u64) -> u64 {
    if denom == 0 {
        return 0;
    }
    let scaled = (val as u128 * num as u128) / denom as u128;
    scaled.min(u64::MAX as u128) as u64
}

/// A `{1}{1}` header's fps. The C swaps `,` for `.`, runs `g_ascii_strtod` and accepts
/// `0.001 <= fps <= 1000.0`.
fn parse_fps(text: &str) -> Option<(u64, u64)> {
    let b = text.as_bytes();
    let mut i = 0;
    skip_c_space(b, &mut i);
    if matches!(b.get(i), Some(b'+' | b'-')) {
        if b[i] == b'-' {
            return None;
        }
        i += 1;
    }

    let mut int_part: u64 = 0;
    let mut int_digits = 0u32;
    while let Some(d @ b'0'..=b'9') = b.get(i) {
        int_part = int_part.saturating_mul(10).saturating_add((*d - b'0') as u64);
        int_digits += 1;
        i += 1;
    }

    let mut num: u64 = int_part;
    let mut den: u64 = 1;
    let mut frac_digits = 0u32;
    if matches!(b.get(i), Some(b'.' | b',')) {
        i += 1;
        while let (Some(d @ b'0'..=b'9'), true) = (b.get(i), frac_digits < 9) {
            num = num.saturating_mul(10).saturating_add((*d - b'0') as u64);
            den = den.saturating_mul(10);
            frac_digits += 1;
            i += 1;
        }
    }

    if int_digits == 0 && frac_digits == 0 {
        return None;
    }

    // An exponent only counts with at least one digit after the `e`, as strtod.
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        let negative = match b.get(j) {
            Some(b'+') => {
                j += 1;
                false
            }
            Some(b'-') => {
                j += 1;
                true
            }
            _ => false,
        };
        let mut exp: i32 = 0;
        let mut exp_digits = 0u32;
        while let Some(d @ b'0'..=b'9') = b.get(j) {
            exp = (exp.saturating_mul(10).saturating_add((*d - b'0') as i32)).min(MAX_EXP + 1);
            exp_digits += 1;
            j += 1;
        }
        if exp_digits > 0 {
            (num, den) = apply_exponent(num, den, if negative { -exp } else { exp })?;
        }
    }

    let value = num as f64 / den as f64;
    if !(0.001..=1000.0).contains(&value) {
        return None;
    }

    let g = gcd(num, den);
    Some((num / g, den / g))
}

const MAX_EXP: i32 = 40;
const MAX_DEN: u64 = 1_000_000_000;

fn apply_exponent(mut num: u64, mut den: u64, exp: i32) -> Option<(u64, u64)> {
    if num == 0 || exp.abs() > MAX_EXP {
        return None;
    }
    for _ in 0..exp.abs() {
        if exp > 0 {
            if den > 1 && den.is_multiple_of(10) {
                den /= 10;
            } else {
                num = num.checked_mul(10)?;
            }
        } else if num.is_multiple_of(10) {
            num /= 10;
        } else {
            den = den.checked_mul(10).filter(|d| *d <= MAX_DEN)?;
        }
    }
    Some((num, den))
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

fn build_markup(text: &str) -> Option<String> {
    let mut markup = String::with_capacity(text.len() + 16);
    let mut rest = text;

    loop {
        let mut italic = false;
        let mut bold = false;
        let mut fontsize: u64 = 0;

        if let Some(r) = rest.strip_prefix("{y:i}") {
            italic = true;
            rest = r;
        }
        if let Some(r) = rest.strip_prefix("{y:b}") {
            bold = true;
            rest = r;
        }
        if let Some(after) = rest.strip_prefix("{s:") {
            let b = after.as_bytes();
            let mut i = 0;
            skip_c_space(b, &mut i);
            if let Some(value) = scan_uint(b, &mut i) {
                // No closing brace makes the C drop the line.
                let close = rest.find('}')?;
                fontsize = value;
                rest = &rest[close + 1..];
            }
        }
        if let Some(r) = rest.strip_prefix('/') {
            italic = true;
            rest = r;
        }

        let (chunk_src, next) = match rest.find('|') {
            Some(i) => (&rest[..i], Some(i + 1)),
            None => (rest, None),
        };

        let mut chunk = String::with_capacity(chunk_src.len());
        markup_escape(chunk_src, &mut chunk);
        if chunk.ends_with('/') {
            chunk.pop();
        }

        markup.push_str("<span");
        if italic {
            markup.push_str(" style=\"italic\"");
        }
        if bold {
            markup.push_str(" weight=\"bold\"");
        }
        if fontsize != 0 {
            markup.push_str(" size=\"");
            push_u64(fontsize.saturating_mul(1000), &mut markup);
            markup.push('"');
        }
        markup.push('>');
        markup.push_str(&chunk);
        markup.push_str("</span>");

        match next {
            Some(off) => {
                markup.push('\n');
                rest = &rest[off..];
            }
            None => break,
        }
    }

    Some(markup)
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

fn push_u64(mut v: u64, out: &mut String) {
    if v == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = 0;
    while v > 0 {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for &d in buf[..n].iter().rev() {
        out.push(d as char);
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
        MicroDvd::default().parse(body, &ParseContext::default()).unwrap()
    }

    fn parse_fps_ctx(body: &str, fps: (u32, u32)) -> Vec<Cue> {
        MicroDvd::default().parse(body, &ParseContext { fps: Some(fps) }).unwrap()
    }

    #[test]
    fn c_parity_italics() {
        let body = "{1}{1}25.000 movie info: XVID  608x256 25.0fps 699.0 MB|\
/SubEdit b.4060(http://subedit.com.pl)/\n\
{100}{200}/italics/|not italics\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 4 * S);
        assert_eq!(cues[0].end_ns, Some(8 * S));
        assert_eq!(cues[0].text, "<span style=\"italic\">italics</span>\n<span>not italics</span>");
    }

    #[test]
    fn c_parity_fps_dot() {
        let body = "{1}{1}12.500\n{100}{200}- Hi, Eddie.|- Hiya, Scotty.\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 8 * S);
        assert_eq!(cues[0].end_ns, Some(16 * S));
        assert_eq!(cues[0].text, "<span>- Hi, Eddie.</span>\n<span>- Hiya, Scotty.</span>");
    }

    #[test]
    fn c_parity_fps_comma_separator() {
        let body = "{1}{1}12,500\n{100}{200}- Hi, Eddie.|- Hiya, Scotty.\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 8 * S);
        assert_eq!(cues[0].end_ns, Some(16 * S));
    }

    #[test]
    fn c_parity_apostrophe_escaped() {
        let body = "{1}{1}12.500\n\
{1250}{1350}- Cold enough for you?|- Well, I'm only faintly alive. It's 25 below\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 100 * S);
        assert_eq!(cues[0].end_ns, Some(108 * S));
        assert_eq!(
            cues[0].text,
            "<span>- Cold enough for you?</span>\n\
<span>- Well, I&apos;m only faintly alive. It&apos;s 25 below</span>"
        );
    }

    #[test]
    fn default_fps_is_24000_1001() {
        let cues = parse("{24000}{48000}hi\n");
        assert_eq!(cues[0].start_ns, 1001 * S);
        assert_eq!(cues[0].end_ns, Some(2002 * S));
    }

    #[test]
    fn ctx_fps_used_when_no_header() {
        let cues = parse_fps_ctx("{30}{60}hi\n", (30, 1));
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn header_overrides_ctx_fps() {
        let cues = parse_fps_ctx("{1}{1}10.000\n{10}{20}hi\n", (30, 1));
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn one_one_line_is_never_a_cue() {
        let cues = parse("{1}{1}not a number\n{25}{50}hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span>hi</span>");
    }

    #[test]
    fn out_of_range_fps_is_ignored() {
        let cues = parse_fps_ctx("{1}{1}5000\n{30}{60}hi\n", (30, 1));
        assert_eq!(cues[0].start_ns, S);
    }

    #[test]
    fn c_parity_fps_exponent() {
        let cues = parse("{1}{1}1e2\n{100}{200}hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));

        let cues = parse("{1}{1}1000.0e-1\n{100}{200}hi\n");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));

        let cues = parse("{1}{1}25e\n{25}{50}hi\n");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn c_parity_fps_trailing_zeros() {
        let cues = parse("{1}{1}25.000\n{25}{50}hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn huge_frame_numbers_saturate() {
        let cues = parse("{500000000000}{500000000001}x\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, u64::MAX);
        assert_eq!(cues[0].end_ns, Some(u64::MAX));
    }

    #[test]
    fn c_parity_junk_after_the_second_frame_number() {
        let cues = parse("{1}{1}25.000\n{25}{50 x}hello\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
        assert_eq!(cues[0].text, "<span>hello</span>");
    }

    #[test]
    fn a_line_with_no_second_closing_brace_is_dropped() {
        let cues = parse("{1}{1}25.000\n{25}{50\n{75}{100}ok\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span>ok</span>");
    }

    #[test]
    fn bold_and_italic_and_size() {
        let cues = parse("{1}{1}25.000\n{25}{50}{y:i}{y:b}{s:20}Hi\n");
        assert_eq!(cues[0].text, "<span style=\"italic\" weight=\"bold\" size=\"20000\">Hi</span>");
    }

    #[test]
    fn size_without_closing_brace_drops_line() {
        let cues = parse("{1}{1}25.000\n{25}{50}{s:20 broken\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn c_parity_size_with_space_before_the_digits() {
        let cues = parse("{1}{1}25.000\n{25}{50}{s: 20}Hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span size=\"20000\">Hi</span>");
    }

    #[test]
    fn trailing_slash_removed_without_leading_slash() {
        let cues = parse("{1}{1}25.000\n{25}{50}plain/\n");
        assert_eq!(cues[0].text, "<span>plain</span>");
    }

    #[test]
    fn escapes_markup_metacharacters() {
        let cues = parse("{1}{1}25.000\n{25}{50}a<b>&\"c\n");
        assert_eq!(cues[0].text, "<span>a&lt;b&gt;&amp;&quot;c</span>");
    }

    #[test]
    fn escapes_c1_controls_like_glib() {
        let cues = parse("{1}{1}25.000\n{25}{50}a\u{80}b\u{85}c\u{9f}d\u{7f}\n");
        assert_eq!(cues[0].text, "<span>a&#x80;b\u{85}c&#x9f;d&#x7f;</span>");
    }

    #[test]
    fn multiline_pipe_split() {
        let cues = parse("{1}{1}25.000\n{25}{50}one|two|three\n");
        assert_eq!(cues[0].text, "<span>one</span>\n<span>two</span>\n<span>three</span>");
    }

    #[test]
    fn utf8_passes_through() {
        let cues = parse("{1}{1}25.000\n{25}{50}café 日本語\n");
        assert_eq!(cues[0].text, "<span>café 日本語</span>");
    }

    #[test]
    fn non_microdvd_lines_are_skipped() {
        let cues = parse("this is not microdvd\n{1}{1}25.000\n{25}{50}ok\ngarbage\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span>ok</span>");
    }

    #[test]
    fn unterminated_final_line_is_dropped() {
        let cues = parse("{1}{1}25.000\n{25}{50}kept\n{75}{100}dropped");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span>kept</span>");

        let cues = parse("{1}{1}25.000\n{25}{50}kept\n{75}{100}dropped\n");
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[1].text, "<span>dropped</span>");
    }

    #[test]
    fn empty_body_yields_no_cues() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(MicroDvd::default().output_format(), OutputFormat::PangoMarkup);
    }
}
