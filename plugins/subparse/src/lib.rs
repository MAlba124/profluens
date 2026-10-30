// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! profluens-subparse (`pf-subparse`), text subtitle parsers, the flat `subtitle/cue-ir` cue
//! and the `subparse` element.

// The one audited exception is `pod`, the record casts of the wire layouts.
#![deny(unsafe_code)]

pub mod autodetect;
pub mod cue;
pub mod element;
pub mod encoding;
pub mod format;
pub mod formats;
pub mod ir;
pub mod pod;
pub mod ssastyle;
pub mod vttcss;

pub use cue::{Cue, CueSettings, OutputFormat, ParseContext, ParseError};
pub use element::{CUE_SLOT, MAX_FILE, SubParse, SubParseError, document_ir};
pub use encoding::{Charset, Decoder};
pub use format::{Format, LineScanner, Parsed, SubtitleFormat, parse_with};
pub use ir::{CUE_IR_FAMILY, CueBuilder, CueIr};

use pf_registry::Registry;
use profluens_core::element::Element;

/// Register this crate's element for name-based construction.
pub fn register(registry: &mut Registry) {
    registry.register(SubParse::new().desc());
}
