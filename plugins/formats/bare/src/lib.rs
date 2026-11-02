//! pf-bare, audio files with no container, native FLAC, MPEG audio and ADTS AAC. Opening reads
//! the head and the tail, a [`Reader`] finds the frames as a session reads forward and seeks
//! on them, `baresrc` puts them out a frame per buffer. Tags are left to the caller, the opener
//! hands back the tag bytes it read on the way.

#![deny(unsafe_code)]

pub mod element;
pub mod fixture;
pub mod flac;
pub mod head;
pub mod reader;

pub use element::{BareSrc, BareSrcError};
pub use head::{BareHead, BareKind, Codec, RawTags, open};
pub use reader::{Reader, Unit};

use pf_registry::Registry;
use std::fmt;

/// Reads into the buffer at a file offset, returns the bytes read, 0 at the end.
pub type ReadAt<'a> = dyn FnMut(u64, &mut [u8]) -> std::io::Result<usize> + 'a;

/// Everything an opener or a reader refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareError {
    /// No stream of the kind the file was opened as.
    NoStreams,
    /// A stream of a form not carried.
    Unsupported(&'static str),
    /// A codec head that does not parse.
    BadHead(&'static str),
    /// Past a size the reader holds.
    TooLarge,
    /// The file is broken at a byte offset.
    Malformed(u64),
    /// The caller's read failed.
    Io(std::io::ErrorKind),
}

impl fmt::Display for BareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BareError::NoStreams => write!(f, "no streams"),
            BareError::Unsupported(what) => write!(f, "unsupported {what}"),
            BareError::BadHead(fam) => write!(f, "bad {fam} head"),
            BareError::TooLarge => write!(f, "too large"),
            BareError::Malformed(at) => write!(f, "malformed at byte {at}"),
            BareError::Io(k) => write!(f, "read failed, {k}"),
        }
    }
}

impl std::error::Error for BareError {}

/// Register this crate's elements for name-based lookup.
pub fn register(registry: &mut Registry) {
    registry.register(&element::REGISTRY_DESC);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn register_exposes_the_source() {
        let mut reg = Registry::new();
        register(&mut reg);
        assert_eq!(reg.names(), vec!["baresrc"]);
        let d = reg.get("baresrc").unwrap();
        assert!(d.make_default.is_none(), "baresrc needs the file and its head");
        assert_eq!(d.srcs[0].offers.len(), 3, "flac, mp3 and aac");
    }
}
