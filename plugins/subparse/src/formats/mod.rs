// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Format parsers, one std-only module per format.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

pub mod dks;
pub mod lrc;
pub mod microdvd;
pub mod mpl2;
pub mod mpsub;
pub mod qttext;
pub mod sami;
pub mod ssa;
pub mod subrip;
pub mod subviewer;
pub mod tmplayer;
pub mod webvtt;

pub use dks::Dks;
pub use lrc::Lrc;
pub use microdvd::MicroDvd;
pub use mpl2::Mpl2;
pub use mpsub::MpSub;
pub use qttext::QtText;
pub use sami::Sami;
pub use ssa::Ssa;
pub use subrip::SubRip;
pub use subviewer::SubViewer;
pub use tmplayer::TmPlayer;
pub use webvtt::WebVtt;
