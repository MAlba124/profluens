//! POD audio formats, typed PCM views and the audio glue elements.

#![feature(allocator_api)] // per-`process()` arena scratch
#![allow(dead_code)]

pub mod bytes_in;
pub mod chop;
pub mod convert;
pub mod convert_element;
pub mod downmix_element;
pub mod error;
pub mod format;
pub mod gain;
pub mod join;
pub mod layout;
pub mod quality;
pub mod resample;
pub mod resample_element;
pub mod restart;
pub mod stamp;
pub mod stretch;
pub mod trim;
pub mod wav;

pub use chop::{AudioChop, CHOP};
pub use convert::{
    convert_interleaved, convert_interleaved_vec, converted_len, downmix_layout_to_stereo,
    downmix_to_stereo, remap_channels,
};
pub use convert_element::AudioConvert;
pub use downmix_element::AudioDownmix;
pub use error::{AudioError, AudioErrorKind};
pub use format::{
    AudioFormat, AudioFrameRef, DELAY_FIELDS, FAMILY, FIELD_CHANNELS, FIELD_DELAY, FIELD_RATE,
    FIELD_SAMPLE, MAX_CHANNELS, MAX_SAMPLE_RATE, RAW_ANY_OFFER, SampleFormat, SampleFormats,
    announce_raw, link_samples, negotiated_audio_format,
};
pub use gain::{AudioGain, GAIN_MAX, GAIN_MIN, RAMP_MS};
pub use join::AudioJoin;
pub use layout::{
    FIELD_LAYOUT, LAYOUT_FIELD, default_layout, layout_or_default, negotiated_layout,
};
pub use resample::{ChannelResampler, PolyphaseFilter, gcd, output_len};
pub use resample_element::AudioResample;
pub use restart::{Lead, RESTARTS, Restart, lead_of};
pub use stamp::{push_pcm, take_pcm};
pub use stretch::{AudioStretch, RATE_MAX, RATE_MIN, rate_supported};
pub use trim::{
    Place, Rebase, Trim, Trimmed, declared_delay, delay_frames, frames_before, place, position_at,
    span_at, time_at, trim_frames, trim_pcm,
};
pub use wav::{
    WAV_GRID_NS, WAV_MAX_ENTRIES, WavError, WavHeader, WavParse, parse_wav_header, wav_index,
    write_pcm_wav,
};

use pf_registry::Registry;
use profluens_core::element::Element;

/// The audio converters an inserter may chain, a format converter then a resampler.
pub static CONVERT: pf_registry::convert::ConvertPolicy = pf_registry::convert::ConvertPolicy {
    converters: &[(FAMILY, &[&convert_element::DESC, &resample_element::DESC])],
};

/// Register this crate's elements for name-based construction.
pub fn register(registry: &mut Registry) {
    registry.register(WavParse::new().desc());
    registry.register(AudioConvert::new(SampleFormat::S16).desc());
    registry.register(AudioResample::new(48_000).desc());
    registry.register(AudioDownmix::new().desc());
    registry.register(AudioStretch::new(profluens_core::time::Rate::ONE).desc());
    registry.register(AudioGain::new(1.0).desc());
    registry.register(AudioJoin::new().desc());
    registry.register(AudioChop::new().desc());
}
