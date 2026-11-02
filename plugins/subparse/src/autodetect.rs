// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Subtitle format auto-detection, a hand port (no regex) of the C
//! `gst_sub_parse_data_format_autodetect`. Order and match rules follow the C.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::format::Format;

/// Sniff the format from the start of the decoded body, what the C `_autodetect` is handed.
pub fn detect(body: &str) -> Option<Format> {
    let body = body.strip_prefix('\u{feff}').unwrap_or(body);
    let b = body.as_bytes();

    if matches_microdvd(b) {
        return Some(Format::MicroDvd);
    }
    if matches_subrip(b) {
        return Some(Format::SubRip);
    }
    if matches_dks(b) {
        return Some(Format::Dks);
    }
    if matches_webvtt(b) {
        return Some(Format::WebVtt);
    }
    if matches_mpsub(b) {
        return Some(Format::MpSub);
    }
    if matches_sami(body) {
        return Some(Format::Sami);
    }
    if matches_tmplayer(b) {
        return Some(Format::TmPlayer);
    }
    if matches_mpl2(b) {
        return Some(Format::Mpl2);
    }
    if matches_subviewer(body) {
        return Some(Format::SubViewer);
    }
    if matches_qttext(body) {
        return Some(Format::QtText);
    }
    if matches_lrc(b) {
        return Some(Format::Lrc);
    }
    None
}

/// The SSA/ASS section headers, a separate element in the C.
const SSA_SECTIONS: [&str; 4] = ["[Script Info]", "[V4+ Styles]", "[V4 Styles]", "[Events]"];

/// How far into the body an SSA section header is looked for, in chars.
const SSA_HEAD: usize = 4096;

/// What the element parses a document as, [`detect`] then SSA/ASS by its section headers.
pub fn detect_document(body: &str) -> Option<Format> {
    detect(body).or_else(|| {
        let head = body.strip_prefix('\u{feff}').unwrap_or(body);
        let head = &head[..head.char_indices().nth(SSA_HEAD).map_or(head.len(), |(i, _)| i)];
        SSA_SECTIONS.iter().any(|s| head.contains(s)).then_some(Format::Ssa)
    })
}

/// PCRE `\s` and C `isspace`, the ASCII whitespace set.
#[inline]
fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn take_digits(b: &[u8], i: &mut usize, min: usize, max: usize) -> bool {
    let start = *i;
    let mut n = 0;
    while n < max && *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
        n += 1;
    }
    if n < min {
        *i = start;
        false
    } else {
        true
    }
}

fn take_spaces(b: &[u8], i: &mut usize, min: usize, max: usize) -> bool {
    let start = *i;
    let mut n = 0;
    while n < max && *i < b.len() && b[*i] == b' ' {
        *i += 1;
        n += 1;
    }
    if n < min {
        *i = start;
        false
    } else {
        true
    }
}

fn take_spaces_plus(b: &[u8], i: &mut usize) -> bool {
    take_spaces(b, i, 1, usize::MAX)
}

fn opt_space(b: &[u8], i: &mut usize) {
    if *i < b.len() && b[*i] == b' ' {
        *i += 1;
    }
}

fn lit(b: &[u8], i: &mut usize, ch: u8) -> bool {
    if *i < b.len() && b[*i] == ch {
        *i += 1;
        true
    } else {
        false
    }
}

fn one_of(b: &[u8], i: &mut usize, set: &[u8]) -> bool {
    if *i < b.len() && set.contains(&b[*i]) {
        *i += 1;
        true
    } else {
        false
    }
}

fn lit_str(b: &[u8], i: &mut usize, s: &[u8]) -> bool {
    if *i <= b.len() && b[*i..].starts_with(s) {
        *i += s.len();
        true
    } else {
        false
    }
}

/// C `%u` / `%Nu`, skip whitespace then read `1..=max` digits.
fn scan_uint(b: &[u8], i: &mut usize, max: Option<usize>, sign: bool) -> bool {
    let start = *i;
    while *i < b.len() && is_ws(b[*i]) {
        *i += 1;
    }
    let cap = max.unwrap_or(usize::MAX);
    let mut n = 0;
    if sign && n < cap && matches!(b.get(*i), Some(b'+' | b'-')) {
        *i += 1;
        n += 1;
    }
    let mut digits = 0;
    while n < cap && *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
        n += 1;
        digits += 1;
    }
    if digits == 0 {
        *i = start;
        false
    } else {
        true
    }
}

/// The C's MicroDVD regex `^\{[0-9]+\}\{[0-9]+\}`.
fn matches_microdvd(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'{')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b'}')
        && lit(b, &mut i, b'{')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b'}')
}

/// The C's SubRip regex `^[\s\n]*[\n]? {0,3}[ 0-9]{1,4}\s*(\x0d)?\x0a`, then a timestamp line.
fn matches_subrip(b: &[u8]) -> bool {
    let mut preamble = Preamble::default();
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' && preamble.ok() && srt_timestamp(b, i + 1) {
            return true;
        }
        if !(is_ws(c) || c.is_ascii_digit()) || !preamble.push(i, c) {
            return false;
        }
    }
    false
}

/// `preamble_ok` over a growing prefix, in linear time.
#[derive(Default)]
struct Preamble {
    first_digit: Option<usize>,
    last_gap: Option<usize>,
    space: bool,
    broken: bool,
}

impl Preamble {
    fn push(&mut self, i: usize, c: u8) -> bool {
        if c.is_ascii_digit() {
            let first = *self.first_digit.get_or_insert(i);
            if i - first >= 4 || self.last_gap.is_some_and(|gap| gap > first) {
                self.broken = true;
            }
        } else if c == b' ' {
            self.space = true;
        } else {
            self.last_gap = Some(i);
        }
        !self.broken
    }

    fn ok(&self) -> bool {
        !self.broken && (self.first_digit.is_some() || self.space)
    }
}

/// The preamble without the `\x0a`. Quadratic, the reference for [`Preamble`].
#[cfg(test)]
fn preamble_ok(r: &[u8]) -> bool {
    if !r.iter().all(|&c| is_ws(c) || c.is_ascii_digit()) {
        return false;
    }
    let n = r.len();
    for s in 0..n {
        if !r[..s].iter().all(|&c| is_ws(c)) {
            break;
        }
        for len in 1..=4 {
            let end = s + len;
            if end > n {
                break;
            }
            let block = &r[s..end];
            if block.iter().all(|&c| c == b' ' || c.is_ascii_digit())
                && r[end..].iter().all(|&c| is_ws(c))
            {
                return true;
            }
        }
    }
    false
}

fn srt_timestamp(b: &[u8], mut i: usize) -> bool {
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !lit(b, &mut i, b':') {
        return false;
    }
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !lit(b, &mut i, b':') {
        return false;
    }
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !one_of(b, &mut i, b",.") {
        return false;
    }
    take_spaces(b, &mut i, 0, 2);
    if !take_digits(b, &mut i, 1, 3) {
        return false;
    }
    if !take_spaces_plus(b, &mut i) || !lit_str(b, &mut i, b"-->") || !take_spaces_plus(b, &mut i) {
        return false;
    }
    if !take_digits(b, &mut i, 1, 2) || !lit(b, &mut i, b':') {
        return false;
    }
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !lit(b, &mut i, b':') {
        return false;
    }
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !one_of(b, &mut i, b",.") {
        return false;
    }
    take_spaces(b, &mut i, 0, 2);
    take_digits(b, &mut i, 1, 2)
}

/// The C's DKS regex `^\[[0-9]+:[0-9]+:[0-9]+\].*`.
fn matches_dks(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'[')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b':')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b':')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b']')
}

/// The C's WebVTT regex `^(\xef\xbb\xbf)?WEBVTT[\xa\xd\x20\x9]`.
fn matches_webvtt(b: &[u8]) -> bool {
    let mut i = 0;
    if b[i..].starts_with(&[0xEF, 0xBB, 0xBF]) {
        i += 3;
    }
    if !b[i..].starts_with(b"WEBVTT") {
        return false;
    }
    i += 6;
    matches!(b.get(i), Some(&(b'\n' | b'\r' | b' ' | b'\t')))
}

/// MPSub, `strncmp(str, "FORMAT=TIME", 11) == 0` in the C.
fn matches_mpsub(b: &[u8]) -> bool {
    b.starts_with(b"FORMAT=TIME")
}

/// SAMI, `<SAMI>` or `<sami>` but no mixed case, as the C `strstr` pair.
fn matches_sami(body: &str) -> bool {
    body.contains("<SAMI>") || body.contains("<sami>")
}

/// TMPlayer, the C's five `sscanf` probes collapsed to three checks.
fn matches_tmplayer(b: &[u8]) -> bool {
    tmp_hour_1(b) || tmp_hour_2(b) || tmp_hour_2_frac(b)
}

fn tmp_hour_1(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'0')
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
}

fn tmp_hour_2(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'0')
        && lit(b, &mut i, b'0')
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
}

fn tmp_hour_2_frac(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'0')
        && lit(b, &mut i, b'0')
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
        && lit(b, &mut i, b',')
        && scan_uint(b, &mut i, None, false)
}

/// MPL2, `sscanf(str, "[%u][%u]") == 2` in the C. `%u` takes a sign.
fn matches_mpl2(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'[')
        && scan_uint(b, &mut i, None, true)
        && lit(b, &mut i, b']')
        && lit(b, &mut i, b'[')
        && scan_uint(b, &mut i, None, true)
}

fn matches_subviewer(body: &str) -> bool {
    body.contains("[INFORMATION]")
}

fn matches_qttext(body: &str) -> bool {
    body.contains("{QTtext}")
}

/// LRC, `str[0] == '['` and every line but the last is good. The C skips the final split element.
fn matches_lrc(b: &[u8]) -> bool {
    if b.first() != Some(&b'[') {
        return false;
    }
    let mut lines = b.split(|&c| c == b'\n').peekable();
    while let Some(line) = lines.next() {
        if lines.peek().is_none() {
            break; // skip the final split element
        }
        if !lrc_line_good(line) {
            return false;
        }
    }
    true
}

fn lrc_line_good(line: &[u8]) -> bool {
    if lrc_timestamp(line, 2) || lrc_timestamp(line, 3) {
        return true;
    }
    matches!(line.last(), Some(&b']')) && line.contains(&b':')
}

/// `[%u:%02u.%0{ms}u]`. The `]` after the third `%u` does not affect the count.
fn lrc_timestamp(line: &[u8], ms_width: usize) -> bool {
    let mut i = 0;
    lit(line, &mut i, b'[')
        && scan_uint(line, &mut i, None, false)
        && lit(line, &mut i, b':')
        && scan_uint(line, &mut i, Some(2), false)
        && lit(line, &mut i, b'.')
        && scan_uint(line, &mut i, Some(ms_width), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_takes_ssa_by_its_sections() {
        let ass = "\u{feff}[Script Info]\nScriptType: v4.00+\n\n[Events]\nFormat: Layer, Start, End, Style, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,hi\n";
        assert_eq!(detect(ass), None, "not in the C typefinder");
        assert_eq!(detect_document(ass), Some(Format::Ssa));
        assert_eq!(detect_document("; comment\n[V4 Styles]\nFormat: Name\n"), Some(Format::Ssa));
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n[Events]\n";
        assert_eq!(detect_document(srt), Some(Format::SubRip), "the C cascade first");
        assert_eq!(detect_document("Just some text\n"), None);
        let late = format!("{}[Events]\n", "x".repeat(SSA_HEAD));
        assert_eq!(detect_document(&late), None, "a header past the head");
        let multibyte = format!("{}[Events]\n", "\u{e9}".repeat(SSA_HEAD - 8));
        assert_eq!(detect_document(&multibyte), Some(Format::Ssa), "the head counts chars");
    }

    #[test]
    fn detect_microdvd() {
        assert_eq!(
            detect("{1}{1}12.500\n{100}{200}- Hi, Eddie.|- Hiya, Scotty.\n"),
            Some(Format::MicroDvd)
        );
        assert_eq!(detect("{100}{200}/italics/|not italics\n"), Some(Format::MicroDvd));
    }

    #[test]
    fn detect_subrip() {
        assert_eq!(detect("1\n00:00:01,000 --> 00:00:02,000\nOne\n\n"), Some(Format::SubRip));
    }

    #[test]
    fn detect_subrip_with_loose_spacing() {
        assert_eq!(
            detect("1\n 0: 0:26, 26 --> 0: 0:28, 17\nI cant see.\n\n"),
            Some(Format::SubRip)
        );
        assert_eq!(detect("2\n00:00:03, 9 --> 00:00:04,0   \nThree\n\n"), Some(Format::SubRip));
    }

    #[test]
    fn detect_subrip_with_leading_blank_lines() {
        assert_eq!(detect("\n\n42\n00:00:01,000 --> 00:00:02,000\nHi\n"), Some(Format::SubRip));
    }

    #[test]
    fn detect_subrip_blank_line_between_index_and_timestamp() {
        assert_eq!(detect("1\n\n00:00:01,000 --> 00:00:02,000\nOne\n"), Some(Format::SubRip));
    }

    #[test]
    fn preamble_matches_its_reference_on_every_short_string() {
        const ALPHABET: [u8; 4] = *b" \n\t1";
        let mut buf = Vec::with_capacity(8);
        for len in 0..=8u32 {
            for mut code in 0..ALPHABET.len().pow(len) {
                buf.clear();
                for _ in 0..len {
                    buf.push(ALPHABET[code % ALPHABET.len()]);
                    code /= ALPHABET.len();
                }
                let mut preamble = Preamble::default();
                let mut alive = true;
                for i in 0..=buf.len() {
                    let prefix = &buf[..i];
                    let want = preamble_ok(prefix);
                    if alive {
                        assert_eq!(preamble.ok(), want, "prefix {prefix:?}");
                    } else {
                        assert!(!want, "push refused, yet {prefix:?} matches");
                    }
                    if alive && i < buf.len() {
                        alive = preamble.push(i, buf[i]);
                    }
                }
            }
        }
    }

    #[test]
    fn a_long_blank_preamble_is_linear() {
        let blank = "\n\n\t\x0c\r\n\x0b\n\x0c\r\r\n".repeat(64 * 1024 / 12);
        assert_eq!(detect(&blank), None);
        let srt = format!("{blank}1\n00:00:01,000 --> 00:00:02,000\nOne\n");
        assert_eq!(detect(&srt), Some(Format::SubRip));
    }

    #[test]
    fn detect_dks() {
        assert_eq!(
            detect("[00:00:07]THERE IS A PLACE ON EARTH[br]...\n[00:00:12]\n"),
            Some(Format::Dks)
        );
    }

    #[test]
    fn detect_webvtt() {
        assert_eq!(
            detect("WEBVTT FILE\n\n00:00:00.000 --> 00:00:02.000\nHi\n"),
            Some(Format::WebVtt)
        );
    }

    #[test]
    fn detect_webvtt_bare_header() {
        assert_eq!(detect("WEBVTT\n\nfoo"), Some(Format::WebVtt));
        assert_eq!(detect("WEBVTT\t"), Some(Format::WebVtt));
    }

    #[test]
    fn detect_webvtt_with_bom() {
        assert_eq!(detect("\u{feff}WEBVTT\n"), Some(Format::WebVtt));
    }

    #[test]
    fn detect_mpsub() {
        assert_eq!(detect("FORMAT=TIME\n\n0.00 3.00\nHello world\n"), Some(Format::MpSub));
    }

    #[test]
    fn detect_sami_uppercase() {
        assert_eq!(
            detect("<SAMI>\n<BODY>\n<SYNC Start=1000>Hi</SYNC>\n</BODY>\n</SAMI>\n"),
            Some(Format::Sami)
        );
    }

    #[test]
    fn detect_sami_lowercase() {
        assert_eq!(detect("<sami>\n<body></body>\n</sami>\n"), Some(Format::Sami));
    }

    #[test]
    fn detect_tmplayer_colon_style() {
        assert_eq!(
            detect("00:00:10:This is the Earth|when...\n00:00:13:\n"),
            Some(Format::TmPlayer)
        );
    }

    #[test]
    fn detect_tmplayer_equals_style() {
        assert_eq!(
            detect("00:00:10=This is the Earth|when...\n00:00:13=\n"),
            Some(Format::TmPlayer)
        );
        assert_eq!(
            detect("00:00:10,1=This is the Earth at a time\n00:00:13,1=\n"),
            Some(Format::TmPlayer)
        );
    }

    #[test]
    fn detect_tmplayer_short_hour() {
        assert_eq!(detect("0:00:10:This is the Earth|when...\n0:00:13:\n"), Some(Format::TmPlayer));
    }

    #[test]
    fn detect_mpl2() {
        assert_eq!(detect("[123][456] This is the Earth at a time|when...\n"), Some(Format::Mpl2));
    }

    #[test]
    fn detect_mpl2_sscanf_count_leniency() {
        assert_eq!(detect("[123][456 x]y\n"), Some(Format::Mpl2));
        assert_eq!(detect("[+123][456]y\n"), Some(Format::Mpl2));
        assert_eq!(detect("[123 x][456]y\n"), None);
    }

    #[test]
    fn detect_subviewer() {
        assert_eq!(
            detect("[INFORMATION]\n[TITLE]xxx\n[END INFORMATION]\n00:00:41.00,00:00:44.40\nHi\n"),
            Some(Format::SubViewer)
        );
    }

    #[test]
    fn detect_qttext() {
        assert_eq!(detect("{QTtext}{timeScale:100}\n[00:00:00.00]\nHello\n"), Some(Format::QtText));
    }

    #[test]
    fn detect_lrc() {
        assert_eq!(
            detect("[ar:123]\n[ti:Title]\n[al:Album]\n[00:02.23]Line 1\n"),
            Some(Format::Lrc)
        );
    }

    #[test]
    fn detect_lrc_timestamps_only() {
        assert_eq!(detect("[00:12.34]Lyric one\n[00:15.00]Lyric two\n"), Some(Format::Lrc));
        assert_eq!(detect("[00:06.123]Line 3\n[00:08.00]\n"), Some(Format::Lrc));
    }

    #[test]
    fn reject_empty() {
        assert_eq!(detect(""), None);
    }

    #[test]
    fn reject_plain_text() {
        assert_eq!(detect("Just some random text\nnot a subtitle at all\n"), None);
        assert_eq!(detect("Hello world"), None);
    }

    #[test]
    fn reject_incomplete_microdvd() {
        assert_eq!(detect("{1}foo\n"), None);
        assert_eq!(detect("{1}\n"), None);
    }

    #[test]
    fn reject_subrip_without_arrow() {
        assert_eq!(detect("1\n00:00:01,000\nOne\n"), None);
    }

    #[test]
    fn reject_webvtt_without_separator() {
        assert_eq!(detect("WEBVTTX\n"), None);
        assert_eq!(detect("WEBVTT"), None);
    }

    // Several probes could fire, the earliest in the C cascade wins.

    #[test]
    fn qttext_not_microdvd() {
        assert_eq!(detect("{QTtext}foo\n"), Some(Format::QtText));
    }

    #[test]
    fn dks_beats_lrc_and_subviewer() {
        assert_eq!(detect("[00:00:07]hello\n[00:00:12]\n"), Some(Format::Dks));
    }

    #[test]
    fn subviewer_beats_lrc() {
        assert_eq!(detect("[INFORMATION]\n[TITLE]x\n"), Some(Format::SubViewer));
    }

    #[test]
    fn mpl2_beats_dks_and_lrc() {
        assert_eq!(detect("[123][456]text\n"), Some(Format::Mpl2));
    }

    #[test]
    fn subrip_beats_tmplayer() {
        assert_eq!(detect("1\n00:00:01,000 --> 00:00:02,000\nOne\n"), Some(Format::SubRip));
    }

    #[test]
    fn lrc_is_last_resort_for_bracket_prefix() {
        assert_eq!(detect("[whatever:x]"), Some(Format::Lrc));
    }
}
