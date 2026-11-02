//! pf-tags, a parallel zero-copy tag and metadata scanner.
//! Parsing is total. Malformed or hostile input yields fewer tags, never a panic.

#![deny(unsafe_code)]
#![feature(allocator_api)]

mod aiff;
mod ape;
pub mod fixture;
mod flac;
pub mod flacmeta;
pub mod item;
mod mkv;
mod mp3;
mod mp4;
mod mpc;
mod ogg;
mod parallel;
mod scanner;
mod sniff;
mod store;
mod tail;
mod wav;
mod wv;

// Parser-level corruption gate, installs the rig allocator for the unit-test binary.
#[cfg(test)]
mod gate;
#[cfg(test)]
#[path = "../tests/inputs/mod.rs"]
mod inputs;

pub use parallel::{scan_parallel, scan_parallel_with};
pub use scanner::{ReactorKind, Scanner};
pub use sniff::Format;
pub use store::{PictureRef, Tags};

/// Default prefix-read size and default pool slot size. Covers most headers in one read.
pub const DEFAULT_PREFIX: usize = 128 * 1024;

/// Ceiling on concurrent in-flight files. Stays well inside pf-reactor's 256-op queue.
pub const MAX_IN_FLIGHT: usize = 32;

/// Scan tuning.
#[derive(Clone, Copy, Debug)]
pub struct ScanConfig {
    /// Files (and therefore reactor ops) in flight at once. Clamped to `1..=`[`MAX_IN_FLIGHT`].
    pub in_flight: usize,
    /// Bytes read when metadata sits at the end of the file.
    pub tail_len: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self { in_flight: 16, tail_len: 64 * 1024 }
    }
}

impl ScanConfig {
    pub fn slots(&self) -> usize {
        self.in_flight.clamp(1, MAX_IN_FLIGHT)
    }
}

/// Filesystem failure for one path. Malformed content is not an error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanError {
    Open(std::io::ErrorKind),
    Io(std::io::ErrorKind),
    /// The op was cancelled or the reactor stalled with ops outstanding.
    Cancelled,
}

/// One file's outcome. Borrowed until the callback returns, then the arena is reset.
pub struct ScanOutcome<'a> {
    pub path: &'a std::path::Path,
    pub result: Result<ScanResult<'a>, ScanError>,
}

pub struct ScanResult<'a> {
    pub format: Format,
    pub file: FileMeta,
    pub props: Props,
    pub tags: Tags<'a>,
}

/// Filesystem facts from the one `stat(2)` per file.
#[derive(Clone, Copy, Debug)]
pub struct FileMeta {
    pub len: u64,
    pub mtime: std::time::SystemTime,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Props {
    pub duration_ns: Option<u64>,
    /// Whether `duration_ns` came from a sample count rather than a bitrate estimate.
    pub duration_exact: bool,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
}
