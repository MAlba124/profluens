// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! The parser trait and the format registry.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};

/// What one [`SubtitleFormat::parse_incremental`] call produced. `consumed` is always a `char`
/// boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Cues completed by this call, in order.
    pub cues: Vec<Cue>,
    /// Leading bytes of `body` the caller must now drop.
    pub consumed: usize,
}

/// A subtitle format parser over a decoded UTF-8 body, std only. A parser is single use: after a
/// call with `at_eos` its line scanner and state are spent, the next document takes a fresh one
/// from [`Format::parser`].
pub trait SubtitleFormat {
    /// Parse as much of `body` as forms complete records, leaving an incomplete trailing one. With
    /// `at_eos` the parser finalises it and must consume all of `body`.
    fn parse_incremental(
        &mut self,
        body: &str,
        ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError>;

    /// `parse_incremental(body, ctx, true)`. Call it on a fresh parser.
    fn parse(&mut self, body: &str, ctx: &ParseContext) -> Result<Vec<Cue>, ParseError> {
        Ok(self.parse_incremental(body, ctx, true)?.cues)
    }

    /// The text flavour this parser emits.
    fn output_format(&self) -> OutputFormat;

    /// The WebVTT `STYLE` sheet declared so far. Only the `cue-ir` path reads it.
    fn stylesheet(&self) -> Option<&crate::vttcss::Stylesheet> {
        None
    }

    /// The SSA/ASS style registry collected so far, `Some` for SSA only.
    fn ssa_styles(&self) -> Option<&crate::ssastyle::SsaStyles> {
        None
    }
}

/// Hands out complete lines of a growing body and remembers how far it looked, so a body without
/// line ends is not rescanned per call. CR, LF and CRLF each end a line, as WebVTT asks.
#[derive(Debug, Default)]
pub struct LineScanner {
    scanned: usize,
}

impl LineScanner {
    /// A scanner positioned at the start of a stream.
    pub fn new() -> Self {
        LineScanner { scanned: 0 }
    }

    /// Call `line` for each complete line, its terminator removed. A CR closing `body` may be the
    /// first half of a CRLF, so it waits for the next call unless `at_eos`.
    pub fn feed<F>(&mut self, body: &str, at_eos: bool, mut line: F) -> usize
    where
        F: FnMut(&str),
    {
        let bytes = body.as_bytes();
        let mut consumed = 0usize;
        // CR and LF are never UTF-8 continuation bytes, so a byte search is safe.
        let mut search = self.scanned.min(bytes.len());
        let mut searched = bytes.len();

        while let Some(rel) = bytes[search..].iter().position(|&b| b == b'\n' || b == b'\r') {
            let at = search + rel;
            let next = match bytes.get(at + 1) {
                _ if bytes[at] == b'\n' => at + 1,
                Some(b'\n') => at + 2,
                Some(_) => at + 1,
                None if at_eos => at + 1,
                None => {
                    searched = at;
                    break;
                }
            };
            line(&body[consumed..at]);
            consumed = next;
            search = next;
        }

        self.scanned = searched.max(consumed) - consumed;
        consumed
    }

    /// Reset to the start of a stream.
    pub fn reset(&mut self) {
        self.scanned = 0;
    }
}

/// The formats autodetection recognises, `GST_SUB_PARSE_FORMAT_*` in the C.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    SubRip,
    MicroDvd,
    Mpl2,
    SubViewer,
    Sami,
    TmPlayer,
    MpSub,
    QtText,
    Lrc,
    Dks,
    WebVtt,
    Ssa,
}

impl Format {
    /// Every format compiled in, kept beside the enum so table-driven tests cover a new variant
    /// automatically.
    pub const ALL: &'static [Format] = &[
        Format::SubRip,
        Format::MicroDvd,
        Format::Mpl2,
        Format::SubViewer,
        Format::Sami,
        Format::TmPlayer,
        Format::MpSub,
        Format::QtText,
        Format::Lrc,
        Format::Dks,
        Format::WebVtt,
        Format::Ssa,
    ];

    /// A fresh parser at the start of a stream.
    pub fn parser(self) -> Box<dyn SubtitleFormat + Send> {
        use crate::formats::*;
        match self {
            Format::SubRip => Box::<SubRip>::default(),
            Format::MicroDvd => Box::<MicroDvd>::default(),
            Format::Mpl2 => Box::<Mpl2>::default(),
            Format::SubViewer => Box::<SubViewer>::default(),
            Format::Sami => Box::<Sami>::default(),
            Format::TmPlayer => Box::<TmPlayer>::default(),
            Format::MpSub => Box::<MpSub>::default(),
            Format::QtText => Box::<QtText>::default(),
            Format::Lrc => Box::<Lrc>::default(),
            Format::Dks => Box::<Dks>::default(),
            Format::WebVtt => Box::<WebVtt>::default(),
            Format::Ssa => Box::<Ssa>::default(),
        }
    }

    /// The sink media type, matching the C typefind's `*_CAPS` one-for-one.
    pub fn media_type(self) -> &'static str {
        match self {
            Format::Sami => "application/x-subtitle-sami",
            Format::TmPlayer => "application/x-subtitle-tmplayer",
            Format::Mpl2 => "application/x-subtitle-mpl2",
            Format::Dks => "application/x-subtitle-dks",
            Format::QtText => "application/x-subtitle-qttext",
            Format::Lrc => "application/x-subtitle-lrc",
            Format::WebVtt => "application/x-subtitle-vtt",
            Format::Ssa => "application/x-ssa",
            Format::SubRip => "application/x-subtitle",
            Format::MicroDvd => "application/x-subtitle",
            Format::MpSub => "application/x-subtitle",
            Format::SubViewer => "application/x-subtitle",
        }
    }
}

/// Run a specific format's parser, without detection.
pub fn parse_with(format: Format, body: &str, ctx: &ParseContext) -> Result<Vec<Cue>, ParseError> {
    format.parser().parse(body, ctx)
}

#[cfg(test)]
mod tests {
    use super::{Format, LineScanner};

    /// Feed `chunks` as a growing body, dropping what is consumed, the last one at eos.
    fn lines(chunks: &[&str]) -> Vec<String> {
        let mut s = LineScanner::new();
        let (mut buf, mut out) = (String::new(), Vec::new());
        for (i, c) in chunks.iter().enumerate() {
            buf.push_str(c);
            let n = s.feed(&buf, i + 1 == chunks.len(), |l| out.push(l.to_owned()));
            buf.drain(..n);
        }
        out.push(format!("rest {buf:?}"));
        out
    }

    #[test]
    fn cr_lf_and_crlf_each_end_a_line() {
        assert_eq!(lines(&["a\rb\r\nc\nd"]), ["a", "b", "c", "rest \"d\""]);
        assert_eq!(lines(&["a\r\r\nb\n\n"]), ["a", "", "b", "", "rest \"\""]);
    }

    #[test]
    fn a_crlf_split_across_calls_is_one_line_end() {
        assert_eq!(lines(&["a\r", "\nb\r", "\r", ""]), ["a", "b", "", "rest \"\""]);
        assert_eq!(lines(&["a\r", "", "\n", ""]), ["a", "rest \"\""]);
    }

    #[test]
    fn a_cr_closing_the_stream_ends_its_line() {
        assert_eq!(lines(&["a\rb\r"]), ["a", "b", "rest \"\""]);
        assert_eq!(lines(&["a", "b\r", "c"]), ["ab", "rest \"c\""]);
    }

    #[test]
    fn media_type_matches_c_static_caps() {
        assert_eq!(Format::SubRip.media_type(), "application/x-subtitle");
        assert_eq!(Format::MicroDvd.media_type(), "application/x-subtitle");
        assert_eq!(Format::MpSub.media_type(), "application/x-subtitle");
        assert_eq!(Format::SubViewer.media_type(), "application/x-subtitle");
        assert_eq!(Format::Sami.media_type(), "application/x-subtitle-sami");
        assert_eq!(Format::TmPlayer.media_type(), "application/x-subtitle-tmplayer");
        assert_eq!(Format::Mpl2.media_type(), "application/x-subtitle-mpl2");
        assert_eq!(Format::Dks.media_type(), "application/x-subtitle-dks");
        assert_eq!(Format::QtText.media_type(), "application/x-subtitle-qttext");
        assert_eq!(Format::Lrc.media_type(), "application/x-subtitle-lrc");
        assert_eq!(Format::WebVtt.media_type(), "application/x-subtitle-vtt");
        assert_eq!(Format::Ssa.media_type(), "application/x-ssa");
    }
}
