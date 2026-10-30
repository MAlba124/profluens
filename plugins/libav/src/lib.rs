//! FFmpeg's libavcodec decoders and AAC encoder as profluens elements, vendored via
//! [`libav_sys`]. The FFI is confined to [`lav`]. Each runs on the element's thread alone.

pub mod aacenc;
pub mod audio;
pub mod lav;
pub mod video;

pub use aacenc::{LavAacError, LibavAacEnc};
pub use audio::{LavAudioError, LibavAudioDec};
pub use video::{
    LavVideoError, LibavFlv1Dec, LibavH264Dec, LibavH265Dec, LibavMpeg2Dec, LibavMpeg4Dec,
    LibavVp6Dec, LibavVp6aDec, LibavVp6fDec, LibavVp7Dec, LibavVp8Dec, LibavVp9Dec,
};

use pf_registry::Registry;
use profluens_core::element::Element;

pub fn register(registry: &mut Registry) {
    registry.register(LibavH264Dec::new().desc());
    registry.register(LibavH265Dec::new().desc());
    registry.register(LibavVp8Dec::new().desc());
    registry.register(LibavVp9Dec::new().desc());
    registry.register(LibavMpeg2Dec::new().desc());
    registry.register(LibavMpeg4Dec::new().desc());
    registry.register(LibavFlv1Dec::new().desc());
    registry.register(LibavVp6Dec::new().desc());
    registry.register(LibavVp6fDec::new().desc());
    registry.register(LibavVp6aDec::new().desc());
    registry.register(LibavVp7Dec::new().desc());
    registry.register(LibavAudioDec::aac().desc());
    registry.register(LibavAudioDec::ac3().desc());
    registry.register(LibavAudioDec::mp3().desc());
    registry.register(LibavAudioDec::dts().desc());
    registry.register(LibavAudioDec::truehd().desc());
    registry.register(LibavAudioDec::mlp().desc());
    registry.register(LibavAudioDec::vorbis().desc());
    registry.register(LibavAudioDec::alac().desc());
    registry.register(LibavAacEnc::new().desc());
}
