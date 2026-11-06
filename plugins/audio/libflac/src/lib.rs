//! pf-libflac, the FLAC plugin backed by libFLAC 1.5.0 from [`libflac_sys`].

#![deny(unsafe_code)]

mod decoder;
mod encoder;
mod error;
mod flacdec;
#[cfg(feature = "encoder")]
mod flacenc;
pub mod index;
#[allow(unsafe_code)]
mod libflac;
mod oggflac;

pub use decoder::{DecodeError, FlacDecoder, StreamDecoder, StreamInfo};
#[cfg(feature = "encoder")]
pub use encoder::FlacEncoder;
pub use encoder::{EncodeError, SampleFormat};
pub use error::FlacError;
pub use flacdec::FlacDec;
#[cfg(feature = "encoder")]
pub use flacenc::FlacEnc;
pub use oggflac::{OggFlacDec, OggFlacDeframe};
pub use pf_tags::flacmeta::FlacTags;

use pf_registry::{Elements, Registry};

#[cfg(feature = "encoder")]
pub static ELEMENTS: Elements =
    &[&flacenc::DESC, &flacdec::DESC, &oggflac::DESC, &oggflac::DEC_DESC];
#[cfg(not(feature = "encoder"))]
pub static ELEMENTS: Elements = &[&flacdec::DESC, &oggflac::DESC, &oggflac::DEC_DESC];

pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}

/// Where a two-pass caller writes the STREAMINFO body from [`FlacEncoder::finish`].
pub const fn streaminfo_offset() -> usize {
    encoder::STREAMINFO_BODY_OFFSET
}
