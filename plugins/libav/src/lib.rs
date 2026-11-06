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
    LibavTheoraDec, LibavVp6Dec, LibavVp6aDec, LibavVp6fDec, LibavVp7Dec, LibavVp8Dec, LibavVp9Dec,
};

use pf_registry::{Elements, Registry};

pub static ELEMENTS: Elements = &[
    &video::H264_DESC,
    &video::H265_DESC,
    &video::VP8_DESC,
    &video::VP9_DESC,
    &video::MPEG2_DESC,
    &video::MPEG4_DESC,
    &video::FLV1_DESC,
    &video::VP6_DESC,
    &video::VP6F_DESC,
    &video::VP6A_DESC,
    &video::VP7_DESC,
    &video::THEORA_DESC,
    &audio::AAC_DESC,
    &audio::AC3_DESC,
    &audio::MP3_DESC,
    &audio::DTS_DESC,
    &audio::TRUEHD_DESC,
    &audio::MLP_DESC,
    &audio::VORBIS_DESC,
    &audio::ALAC_DESC,
    &audio::SPEEX_DESC,
    &aacenc::DESC,
];

pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}
