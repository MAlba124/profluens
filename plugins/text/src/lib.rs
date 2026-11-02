//! profluens-text (`pf-text`), subtitle rendering. The bitmap subtitle decoders (PGS, DVB,
//! VobSub), the flat `subtitle/bitmap` payload they write, and `suboverlay`, a latest-wins
//! compositor that never blocks the video.

// The one exception is the `Pod` marker on the two bitmap records.
#![deny(unsafe_code)]

pub mod bitmap;
pub mod dvbsubdec;
pub mod font;
pub mod font_data;
pub mod overlay;
pub mod pgs;
pub mod pgsdec;
pub mod vobsubdec;

pub use bitmap::{BitmapBuilder, BitmapHeader, BitmapRect, BitmapRef};
pub use dvbsubdec::{DVB_FAMILY, DvbSubDec};
pub use overlay::SubtitleOverlay;
pub use pf_subparse::CUE_IR_FAMILY;
pub use pgsdec::{PGS_FAMILY, PgsDec};
pub use vobsubdec::{VOBSUB_FAMILY, VobSubDec};

/// The bitmap-subtitle family the PGS, DVB and VobSub decoders emit, see [`bitmap`].
pub const BITMAP_FAMILY: &str = "subtitle/bitmap";

use pf_registry::{Elements, Registry};

pub static ELEMENTS: Elements =
    &[&pgsdec::DESC, &dvbsubdec::DESC, &vobsubdec::DESC, &overlay::DESC];

/// Register this crate's elements for name-based construction.
pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}
