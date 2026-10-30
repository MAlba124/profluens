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
pub use oggflac::OggFlacDeframe;
pub use pf_tags::flacmeta::FlacTags;

use pf_registry::Registry;
use profluens_core::element::Element;

pub fn register(registry: &mut Registry) {
    #[cfg(feature = "encoder")]
    registry.register(FlacEnc::new(44_100, 2, SampleFormat::S16).desc());
    registry.register(FlacDec::new().desc());
    registry.register(OggFlacDeframe::new().desc());
}

/// Where a two-pass caller writes the STREAMINFO body from [`FlacEncoder::finish`].
pub const fn streaminfo_offset() -> usize {
    encoder::STREAMINFO_BODY_OFFSET
}
