// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Subtitle bytes to UTF-8 text. A byte-order mark decides outright. Otherwise clean UTF-8 is
//! released as it comes, and from the first illegal sequence on the bytes wait in a window until
//! the stream or unit ends or [`SNIFF_LIMIT`] of them pile up. Then cp1252 is chosen only when
//! illegal sequences outnumber valid multibyte ones over the whole stream so far, else the window
//! is UTF-8 with each damaged sequence as U+FFFD. A sequence cut by the end of a unit counts as
//! illegal, a cp1252 cue often ends in one. A decision from a full window latches, one
//! forced early by the end of a unit holds for that unit only. UTF-16 and UTF-32 need a mark or
//! an explicit [`Charset`].

/// Window bytes that vote and that latch the decision once they pile up.
pub const SNIFF_LIMIT: usize = 64 * 1024;

const MAX_BOM: usize = 4;

const REPLACEMENT: char = char::REPLACEMENT_CHARACTER;

/// cp1252 0x80 to 0x9F. The five undefined slots read as their C1 controls, as WHATWG has them.
pub const CP1252_HIGH: [char; 32] = [
    '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}', '\u{8f}',
    '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{17e}', '\u{178}',
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Charset {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
    Windows1252,
}

impl Charset {
    pub const ALL: [Charset; 6] = [
        Charset::Utf8,
        Charset::Utf16Le,
        Charset::Utf16Be,
        Charset::Utf32Le,
        Charset::Utf32Be,
        Charset::Windows1252,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Charset::Utf8 => "utf-8",
            Charset::Utf16Le => "utf-16le",
            Charset::Utf16Be => "utf-16be",
            Charset::Utf32Le => "utf-32le",
            Charset::Utf32Be => "utf-32be",
            Charset::Windows1252 => "windows-1252",
        }
    }

    /// A label, case-insensitive. Latin-1 names read as cp1252, as WHATWG has them.
    pub fn from_label(label: &str) -> Option<Charset> {
        const ALIASES: [(&str, Charset); 13] = [
            ("utf-8", Charset::Utf8),
            ("utf8", Charset::Utf8),
            ("utf-16le", Charset::Utf16Le),
            ("utf16le", Charset::Utf16Le),
            ("utf-16be", Charset::Utf16Be),
            ("utf16be", Charset::Utf16Be),
            ("utf-32le", Charset::Utf32Le),
            ("utf-32be", Charset::Utf32Be),
            ("windows-1252", Charset::Windows1252),
            ("cp1252", Charset::Windows1252),
            ("iso-8859-1", Charset::Windows1252),
            ("latin1", Charset::Windows1252),
            ("us-ascii", Charset::Windows1252),
        ];
        let label = label.trim();
        ALIASES.iter().find(|(name, _)| name.eq_ignore_ascii_case(label)).map(|&(_, c)| c)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Sniff,
    Utf8,
    Utf16 { be: bool },
    Utf32 { be: bool },
    Cp1252 { repair: bool },
}

impl Mode {
    const fn of(c: Charset) -> Mode {
        match c {
            Charset::Utf8 => Mode::Utf8,
            Charset::Utf16Le => Mode::Utf16 { be: false },
            Charset::Utf16Be => Mode::Utf16 { be: true },
            Charset::Utf32Le => Mode::Utf32 { be: false },
            Charset::Utf32Be => Mode::Utf32 { be: true },
            Charset::Windows1252 => Mode::Cp1252 { repair: false },
        }
    }
}

/// One stream's decoder. Feed it the unconsumed tail plus new bytes, it appends text to `out`
/// and says how many bytes it consumed.
#[derive(Clone, Debug)]
pub struct Decoder {
    mode: Mode,
    forced: Option<Charset>,
    bom_checked: bool,
    releasing: bool,
    fresh: bool,
    multibyte: u64,
    illegal: u64,
}

impl Decoder {
    pub const fn new(forced: Option<Charset>) -> Decoder {
        Decoder {
            mode: Mode::Sniff,
            forced,
            bom_checked: false,
            releasing: true,
            fresh: true,
            multibyte: 0,
            illegal: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Decoder::new(self.forced);
    }

    /// The charset a mark, the caller or a full window settled on.
    pub fn charset(&self) -> Option<Charset> {
        match self.mode {
            Mode::Sniff => None,
            Mode::Utf8 => Some(Charset::Utf8),
            Mode::Utf16 { be } => Some(if be { Charset::Utf16Be } else { Charset::Utf16Le }),
            Mode::Utf32 { be } => Some(if be { Charset::Utf32Be } else { Charset::Utf32Le }),
            Mode::Cp1252 { .. } => Some(Charset::Windows1252),
        }
    }

    /// Decode what is safe of `bytes`. With `last` the unit ends here, so nothing is held and a
    /// cut sequence is damage. `bytes[returned..]` is for the next call.
    pub fn decode(&mut self, bytes: &[u8], last: bool, out: &mut String) -> usize {
        let start = out.len();
        let used = self.step(bytes, last, out);
        if self.fresh && out.len() > start {
            self.fresh = false;
            if out[start..].starts_with('\u{feff}') {
                out.replace_range(start..start + '\u{feff}'.len_utf8(), "");
            }
        }
        used
    }

    fn step(&mut self, bytes: &[u8], last: bool, out: &mut String) -> usize {
        if !self.bom_checked {
            if bytes.len() < MAX_BOM && !last {
                return 0;
            }
            self.bom_checked = true;
            let (mode, skip) = match sniff_bom(bytes) {
                Some(found) => found,
                None => (self.forced.map_or(Mode::Sniff, Mode::of), 0),
            };
            self.mode = mode;
            return skip + self.step(&bytes[skip..], last, out);
        }
        match self.mode {
            Mode::Sniff => self.sniff(bytes, last, out),
            mode => decode_as(mode, bytes, last, out),
        }
    }

    fn sniff(&mut self, bytes: &[u8], last: bool, out: &mut String) -> usize {
        let mut used = 0;
        if self.releasing {
            let (valid, flaw) = first_run(bytes);
            out.push_str(valid);
            self.multibyte += count_multibyte(valid.as_bytes());
            used = valid.len();
            self.releasing = flaw != Flaw::Illegal;
        }
        let window = &bytes[used..];
        let full = !self.releasing && window.len() >= SNIFF_LIMIT;
        if !last && !full {
            return used;
        }
        let mode = self.decide(window, last);
        let n = decode_as(mode, window, last, out);
        if full {
            self.mode = mode;
        } else {
            self.releasing = true;
        }
        used + n
    }

    fn decide(&mut self, window: &[u8], last: bool) -> Mode {
        let prefix = &window[..window.len().min(SNIFF_LIMIT)];
        let s = utf8_stats(prefix);
        let cut_at_end = s.truncated && last && prefix.len() == window.len();
        self.multibyte += s.multibyte;
        self.illegal += s.illegal + u64::from(cut_at_end);
        if self.illegal > self.multibyte { Mode::Cp1252 { repair: true } } else { Mode::Utf8 }
    }
}

pub fn decode_all(bytes: &[u8], forced: Option<Charset>, out: &mut String) {
    let used = Decoder::new(forced).decode(bytes, true, out);
    debug_assert_eq!(used, bytes.len());
}

/// UTF-32LE is tested before UTF-16LE, its mark starts with UTF-16LE's.
fn sniff_bom(bytes: &[u8]) -> Option<(Mode, usize)> {
    const MARKS: [(&[u8], Mode); 5] = [
        (&[0xff, 0xfe, 0x00, 0x00], Mode::Utf32 { be: false }),
        (&[0x00, 0x00, 0xfe, 0xff], Mode::Utf32 { be: true }),
        (&[0xef, 0xbb, 0xbf], Mode::Utf8),
        (&[0xff, 0xfe], Mode::Utf16 { be: false }),
        (&[0xfe, 0xff], Mode::Utf16 { be: true }),
    ];
    MARKS.iter().find(|(mark, _)| bytes.starts_with(mark)).map(|&(mark, mode)| (mode, mark.len()))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flaw {
    None,
    Illegal,
    Incomplete,
}

fn incomplete(invalid: &[u8]) -> bool {
    std::str::from_utf8(invalid).is_err_and(|e| e.error_len().is_none())
}

fn first_run(bytes: &[u8]) -> (&str, Flaw) {
    let mut chunks = bytes.utf8_chunks();
    let Some(c) = chunks.next() else {
        return ("", Flaw::None);
    };
    let flaw = if c.invalid().is_empty() {
        Flaw::None
    } else if chunks.next().is_none() && incomplete(c.invalid()) {
        Flaw::Incomplete
    } else {
        Flaw::Illegal
    };
    (c.valid(), flaw)
}

fn count_multibyte(valid: &[u8]) -> u64 {
    valid.iter().filter(|&&b| b >= 0xc0).count() as u64
}

#[derive(Default, PartialEq, Eq, Debug)]
struct Stats {
    multibyte: u64,
    illegal: u64,
    truncated: bool,
}

fn utf8_stats(bytes: &[u8]) -> Stats {
    let mut s = Stats::default();
    let mut chunks = bytes.utf8_chunks().peekable();
    while let Some(c) = chunks.next() {
        s.multibyte += count_multibyte(c.valid().as_bytes());
        if c.invalid().is_empty() {
            continue;
        }
        if chunks.peek().is_none() && incomplete(c.invalid()) {
            s.truncated = true;
        } else {
            s.illegal += 1;
        }
    }
    s
}

fn decode_as(mode: Mode, bytes: &[u8], last: bool, out: &mut String) -> usize {
    match mode {
        Mode::Sniff | Mode::Utf8 => decode_utf8(bytes, last, out),
        Mode::Utf16 { be } => decode_utf16(be, bytes, last, out),
        Mode::Utf32 { be } => decode_utf32(be, bytes, last, out),
        Mode::Cp1252 { repair } => decode_cp1252(repair, bytes, out),
    }
}

fn decode_utf8(bytes: &[u8], last: bool, out: &mut String) -> usize {
    let mut chunks = bytes.utf8_chunks().peekable();
    while let Some(c) = chunks.next() {
        out.push_str(c.valid());
        if c.invalid().is_empty() {
            continue;
        }
        if chunks.peek().is_none() && !last && incomplete(c.invalid()) {
            return bytes.len() - c.invalid().len();
        }
        out.push(REPLACEMENT);
    }
    bytes.len()
}

fn decode_utf16(be: bool, bytes: &[u8], last: bool, out: &mut String) -> usize {
    let unit = |i: usize| {
        let pair = [bytes[i], bytes[i + 1]];
        u32::from(if be { u16::from_be_bytes(pair) } else { u16::from_le_bytes(pair) })
    };
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let u = unit(i);
        let (c, width) = match u {
            0xd800..=0xdbff if i + 4 > bytes.len() => {
                if !last {
                    return i;
                }
                (REPLACEMENT, 2)
            }
            0xd800..=0xdbff => match unit(i + 2) {
                lo @ 0xdc00..=0xdfff => {
                    let scalar = 0x10000 + ((u - 0xd800) << 10) + (lo - 0xdc00);
                    (char::from_u32(scalar).unwrap_or(REPLACEMENT), 4)
                }
                _ => (REPLACEMENT, 2),
            },
            _ => (char::from_u32(u).unwrap_or(REPLACEMENT), 2),
        };
        out.push(c);
        i += width;
    }
    if i < bytes.len() && !last {
        return i;
    }
    if i < bytes.len() {
        out.push(REPLACEMENT);
    }
    bytes.len()
}

fn decode_utf32(be: bool, bytes: &[u8], last: bool, out: &mut String) -> usize {
    let (units, tail) = bytes.as_chunks::<4>();
    for &u in units {
        let raw = if be { u32::from_be_bytes(u) } else { u32::from_le_bytes(u) };
        out.push(char::from_u32(raw).unwrap_or(REPLACEMENT));
    }
    if tail.is_empty() {
        return bytes.len();
    }
    if !last {
        return bytes.len() - tail.len();
    }
    out.push(REPLACEMENT);
    bytes.len()
}

/// A guessed cp1252 shows its undefined slots as U+FFFD rather than C1 controls.
fn decode_cp1252(repair: bool, bytes: &[u8], out: &mut String) -> usize {
    let mut rest = bytes;
    while !rest.is_empty() {
        let ascii = rest.iter().position(|b| !b.is_ascii()).unwrap_or(rest.len());
        let (run, tail) = rest.split_at(ascii);
        out.push_str(std::str::from_utf8(run).unwrap_or_default());
        let Some((&b, tail)) = tail.split_first() else {
            break;
        };
        out.push(match b {
            0x81 | 0x8d | 0x8f | 0x90 | 0x9d if repair => REPLACEMENT,
            0x80..=0x9f => CP1252_HIGH[usize::from(b - 0x80)],
            _ => char::from(b),
        });
        rest = tail;
    }
    bytes.len()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn chunked(bytes: &[u8], cuts: &[usize], forced: Option<Charset>) -> String {
        let mut d = Decoder::new(forced);
        let (mut pending, mut out, mut at) = (Vec::new(), String::new(), 0);
        for &cut in cuts.iter().chain([&bytes.len()]) {
            let cut = cut.min(bytes.len());
            pending.extend_from_slice(&bytes[at..cut.max(at)]);
            at = cut.max(at);
            let used = d.decode(&pending, at == bytes.len(), &mut out);
            pending.drain(..used);
        }
        assert!(pending.is_empty(), "bytes left after the last call");
        out
    }

    fn whole(bytes: &[u8]) -> String {
        let mut out = String::new();
        decode_all(bytes, None, &mut out);
        out
    }

    fn cp1252(text: &str) -> Vec<u8> {
        text.chars()
            .map(|c| match u32::from(c) {
                n @ (0..0x80 | 0xa0..0x100) => n as u8,
                _ => 0x80 + CP1252_HIGH.iter().position(|&h| h == c).expect("in cp1252") as u8,
            })
            .collect()
    }

    fn with_bom(bom: &[u8], text: &str, unit: fn(u32) -> Vec<u8>, wide: bool) -> Vec<u8> {
        let mut out = bom.to_vec();
        if wide {
            text.chars().for_each(|c| out.extend(unit(u32::from(c))));
        } else {
            text.encode_utf16().for_each(|u| out.extend(unit(u32::from(u))));
        }
        out
    }

    const TEXT: &str = "\u{c9}V\u{c9}NEMENTS \u{2018}caf\u{e9}\u{2019} \u{20ac}5 \u{1f600}";

    #[test]
    fn each_mark_decides_and_utf32le_is_not_read_as_utf16le() {
        let le16 = |u: u32| (u as u16).to_le_bytes().to_vec();
        let be16 = |u: u32| (u as u16).to_be_bytes().to_vec();
        let le32 = |u: u32| u.to_le_bytes().to_vec();
        let be32 = |u: u32| u.to_be_bytes().to_vec();
        let mut utf8 = vec![0xef, 0xbb, 0xbf];
        utf8.extend_from_slice(TEXT.as_bytes());
        let cases = [
            (utf8, Charset::Utf8),
            (with_bom(&[0xff, 0xfe], TEXT, le16, false), Charset::Utf16Le),
            (with_bom(&[0xfe, 0xff], TEXT, be16, false), Charset::Utf16Be),
            (with_bom(&[0xff, 0xfe, 0, 0], TEXT, le32, true), Charset::Utf32Le),
            (with_bom(&[0, 0, 0xfe, 0xff], TEXT, be32, true), Charset::Utf32Be),
        ];
        for (bytes, want) in &cases {
            let mut d = Decoder::new(Some(Charset::Windows1252));
            let mut out = String::new();
            assert_eq!(d.decode(bytes, true, &mut out), bytes.len());
            assert_eq!(out, TEXT, "{want:?}");
            assert_eq!(d.charset(), Some(*want), "a mark beats the forced charset");
            for cut in 1..bytes.len() {
                assert_eq!(chunked(bytes, &[cut], None), TEXT, "{want:?} cut at {cut}");
            }
        }
    }

    #[test]
    fn a_short_stream_still_sniffs_its_mark() {
        assert_eq!(whole(&[0xff, 0xfe]), "");
        assert_eq!(whole(&[0xff, 0xfe, b'A', 0]), "A");
        assert_eq!(whole(&[0xef, 0xbb, 0xbf]), "");
        assert_eq!(whole(b"ab"), "ab");
        assert_eq!(chunked(&[0, 0, 0xfe, 0xff, 0, 0, 0, b'A'], &[1, 2, 3], None), "A");
    }

    #[test]
    fn a_second_mark_in_the_text_is_dropped_once() {
        let three = "\u{feff}\u{feff}\u{feff}x".as_bytes();
        assert_eq!(whole(three), "\u{feff}x");
        assert_eq!(chunked(three, &[4, 5], None), "\u{feff}x");
    }

    #[test]
    fn a_character_split_across_pushes_is_held_not_damaged() {
        let doc = "1\n00:00:01,000 --> 00:00:02,000\n\u{c9}t\u{e9} \u{1f600} \u{65e5}\n";
        let bytes = doc.as_bytes();
        for cut in 1..bytes.len() {
            assert_eq!(chunked(bytes, &[cut], None), doc, "cut at {cut}");
        }
        let every: Vec<usize> = (1..bytes.len()).collect();
        assert_eq!(chunked(bytes, &every, None), doc, "one byte a push");
        let mut d = Decoder::new(None);
        let mut out = String::new();
        assert_eq!(d.decode(b"ab\xe2\x82", false, &mut out), 2, "the cut euro waits");
        assert_eq!(out, "ab");
        assert_eq!(d.decode(&[0xe2, 0x82, 0xac], false, &mut out), 3);
        assert_eq!(out, "ab\u{20ac}");
    }

    #[test]
    fn a_split_surrogate_pair_and_a_split_unit_are_held() {
        let le16 = |u: u32| (u as u16).to_le_bytes().to_vec();
        let bytes = with_bom(&[0xff, 0xfe], "a\u{1f600}b", le16, false);
        for cut in 1..bytes.len() {
            assert_eq!(chunked(&bytes, &[cut], None), "a\u{1f600}b", "cut at {cut}");
        }
    }

    #[test]
    fn damage_is_repaired_where_it_is() {
        assert_eq!(whole(b"caf\xc3\xa9 \xff ok \xc3\xa9"), "caf\u{e9} \u{fffd} ok \u{e9}");
        assert_eq!(whole(b"end \xc3\xa9\xe2\x82"), "end \u{e9}\u{fffd}", "a cut tail at eos");
        assert_eq!(
            whole(b"\xe2\x82A\xc3\xa9\xc3\xa9"),
            "\u{fffd}A\u{e9}\u{e9}",
            "one per sequence"
        );
        let le16 = [0xff, 0xfe, 0x00, 0xdc, b'A', 0, 0x3d, 0xd8, b'B', 0, 0x41];
        assert_eq!(whole(&le16), "\u{fffd}A\u{fffd}B\u{fffd}", "lone surrogates and a cut unit");
        let le32 = [0xff, 0xfe, 0, 0, 0, 0xd8, 0, 0, 0, 0, 0x11, 0, b'A', 0, 0, 0, 1];
        assert_eq!(whole(&le32), "\u{fffd}\u{fffd}A\u{fffd}", "non scalars and a cut unit");
    }

    #[test]
    fn the_statistics_pick_cp1252_only_when_illegal_outnumbers_multibyte() {
        let legacy = cp1252("1\r\n\u{c9}V\u{c9}NEMENTS \u{2018}caf\u{e9}\u{2019} \u{20ac}\r\n");
        assert_eq!(
            whole(&legacy),
            "1\r\n\u{c9}V\u{c9}NEMENTS \u{2018}caf\u{e9}\u{2019} \u{20ac}\r\n"
        );
        let damaged = "\u{e9}\u{e9}\u{e9} x \u{e8}".as_bytes().iter().chain(b"\xe9").copied();
        let damaged: Vec<u8> = damaged.collect();
        assert_eq!(whole(&damaged), "\u{e9}\u{e9}\u{e9} x \u{e8}\u{fffd}", "damaged UTF-8");
        assert_eq!(whole(b"\xc3\xa9 \xe9 \xe9 "), "\u{e9} \u{e9} \u{e9} ", "two against one");
        assert_eq!(whole(b"\x81\x8d\x93"), "\u{fffd}\u{fffd}\u{201c}", "guessed slots repaired");
        let mut out = String::new();
        decode_all(b"\x81\x93", Some(Charset::Windows1252), &mut out);
        assert_eq!(out, "\u{81}\u{201c}", "a named cp1252 keeps its slots");
    }

    #[test]
    fn released_multibyte_votes_and_the_window_is_bounded() {
        let mut bytes = "\u{e9}".repeat(3).into_bytes();
        bytes.extend_from_slice(b" \xe9 \xe9");
        assert_eq!(
            whole(&bytes),
            "\u{e9}\u{e9}\u{e9} \u{fffd} \u{fffd}",
            "the released three outvote one"
        );
        let mut long = b"\xe9".to_vec();
        long.extend(std::iter::repeat_n(b'a', SNIFF_LIMIT));
        long.extend_from_slice("\u{e9}".repeat(4).as_bytes());
        let text = whole(&long);
        assert!(text.starts_with("\u{e9}a"), "the window past the limit does not vote");
        assert!(text.ends_with("\u{c3}\u{a9}"), "and is decoded with the window's charset");
        for cut in [1, 2, SNIFF_LIMIT, SNIFF_LIMIT + 1, SNIFF_LIMIT + 2] {
            assert_eq!(chunked(&long, &[cut], None), text, "cut at {cut}");
        }
        let mut d = Decoder::new(None);
        let mut out = String::new();
        assert_eq!(d.decode(&long[..SNIFF_LIMIT - 1], false, &mut out), 0, "the window waits");
        assert_eq!(d.decode(&long, false, &mut out), long.len(), "a full window latches");
        assert_eq!(d.charset(), Some(Charset::Windows1252));
    }

    #[test]
    fn a_unit_end_decides_for_that_unit_and_the_counts_carry() {
        let mut d = Decoder::new(None);
        let mut out = String::new();
        d.decode("\u{e9}\u{e9}".as_bytes(), true, &mut out);
        d.decode(b"\xe9", true, &mut out);
        assert_eq!(out, "\u{e9}\u{e9}\u{fffd}", "two multibyte outvote one illegal");
        assert_eq!(d.charset(), None, "a unit end does not latch");
        d.decode(b"\xe9 \xe8 \xe9 ", true, &mut out);
        assert_eq!(out, "\u{e9}\u{e9}\u{fffd}\u{e9} \u{e8} \u{e9} ", "now illegal leads");
        d.decode("\u{e9}".as_bytes(), true, &mut out);
        assert!(out.ends_with('\u{e9}'), "a clean unit is released as UTF-8");
    }

    #[test]
    fn labels_resolve_case_insensitively() {
        for c in Charset::ALL {
            assert_eq!(Charset::from_label(&c.label().to_ascii_uppercase()), Some(c));
        }
        assert_eq!(Charset::from_label(" Latin1 "), Some(Charset::Windows1252));
        assert_eq!(Charset::from_label("koi8-r"), None);
    }
}
