// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Core types shared by every format parser.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

/// Text flavour a parser emits, mirroring subparse's `text/x-raw` `format` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Utf8,
    PangoMarkup,
}

/// Per-cue presentation settings. The first five are the C `ParserState` fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CueSettings {
    /// Line position, percent (`L:10%`, `line:10%`).
    pub line_position: Option<u8>,
    /// Text position, percent (`T:50%`, `position:50%`).
    pub text_position: Option<u8>,
    /// Text size, percent (`S:35%`, `size:35%`).
    pub text_size: Option<u8>,
    /// `D:` / `vertical:` value.
    pub vertical: Option<String>,
    /// `A:` / `align:` value.
    pub alignment: Option<String>,
    /// Modern `line:<int>`, 0-based from the start edge, negative from the end.
    pub line_number: Option<i32>,
    /// The modern `line:...,<align>` suffix, "start", "center" or "end".
    pub line_align: Option<String>,
    /// The modern `position:...,<align>` suffix, "line-left", "center" or "line-right".
    pub position_align: Option<String>,
}

/// A parsed subtitle cue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    /// Presentation start, nanoseconds.
    pub start_ns: u64,
    /// Presentation end (exclusive), nanoseconds.
    pub end_ns: Option<u64>,
    /// Plain UTF-8 or Pango markup per the parser's `output_format`.
    pub text: String,
    /// Optional presentation settings (WebVTT).
    pub settings: CueSettings,
    /// The WebVTT cue identifier, for `cue-ir` `::cue(#id)` selectors only.
    pub id: Option<String>,
    /// SSA/ASS extras (raw Text field, style name, margins), `cue-ir` only.
    pub ssa: Option<Box<crate::ssastyle::SsaDialogue>>,
    /// Source text before a lossy pango transform (SubRip keeps only `<i>/<b>/<u>`), for
    /// [`crate::ir::CueIr::from_srt_text`] only.
    pub raw_text: Option<String>,
}

impl Cue {
    /// Convenience constructor with default (empty) settings.
    pub fn new(start_ns: u64, end_ns: Option<u64>, text: impl Into<String>) -> Self {
        Cue {
            start_ns,
            end_ns,
            text: text.into(),
            settings: CueSettings::default(),
            id: None,
            ssa: None,
            raw_text: None,
        }
    }

    /// Duration in nanoseconds, if the cue has an end.
    pub fn duration_ns(&self) -> Option<u64> {
        self.end_ns.map(|e| e.saturating_sub(self.start_ns))
    }
}

/// Cue text bytes a parser collects before it stops copying, the rest of an unterminated cue is
/// read and dropped. Past [`crate::ir::CUE_LIMIT`] the cue is refused anyway, the margin keeps
/// markup heavy text that lays out short.
pub const CUE_TEXT_MAX: usize = 4 * crate::ir::CUE_LIMIT;

/// Append `s` to cue text `buf` up to [`CUE_TEXT_MAX`], cut on a char boundary.
pub(crate) fn push_capped(buf: &mut String, s: &str) {
    let mut n = s.len().min(CUE_TEXT_MAX.saturating_sub(buf.len()));
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    buf.push_str(&s[..n]);
}

/// Context a parser may need beyond the raw text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParseContext {
    /// `(num, den)` for frame-based formats (MicroDVD).
    pub fps: Option<(u32, u32)>,
}

/// A parse failure. Parsers skip malformed cues like the C, `Invalid` is input of another format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Invalid(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Invalid(msg) => write!(f, "invalid subtitle input: {msg}"),
        }
    }
}

impl std::error::Error for ParseError {}
