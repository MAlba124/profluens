//! pf-opus, the Opus (RFC 6716, RFC 7845) decoder and encoder elements over `libopus-sys`.

mod config;
mod error;
pub mod head;
pub mod libopus;
#[doc(hidden)]
pub mod multistream;
mod opusdec;
mod opusenc;

pub use config::{Application, EncoderConfig, OPUS_RATE, Signal};
pub use error::{CodecError, OpusError};
pub use head::{OpusHead, OpusHeadError, apply_output_gain, apply_output_gain_f32};
pub use opusdec::OpusDec;
pub use opusenc::OpusEnc;

use pf_registry::{Elements, Registry};

pub static ELEMENTS: Elements = &[&opusdec::DESC, &opusenc::DESC];

pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}
