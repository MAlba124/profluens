//! pf-vpl, Intel VPL (libvpl 2.x) hardware decode, scale and encode.
//! Elements share one VA display and joined sessions, so surfaces pass between them uncopied.
//! `PF_VPL_JOIN=global|none` picks the parent of elements without a [`VplGroup`], `PF_VPL_ASYNC=N`
//! their async depth. An element at `trace` level in `PROFLUENS_DEBUG` (`vplenc:trace`) prints its
//! calls, pools and counters to stderr.

pub mod dec;
pub mod emit;
pub mod enc;
pub mod error;
pub mod ffi;
pub mod format;
pub mod frames;
pub mod group;
pub mod hdr;
pub mod input;
pub mod layout;
pub mod nal;
pub mod park;
pub mod probe;
pub mod pts;
pub mod session;
pub mod stats;
pub mod va;
pub mod vppcore;
pub mod vppel;

pub use dec::VplDec;
pub use enc::{Rc, Settings, VplEnc};
pub use error::{MfxError, VplError, VplErrorKind};
pub use frames::{SurfaceChannel, VplFrame};
pub use group::{Join, VplGroup};
pub use probe::{ConfigureError, ProbeConfig, VplCaps, configure, probe};
pub use vppcore::{Plan, Rect, Target, plan_fixed};
pub use vppel::VplVpp;

use pf_registry::Registry;
use profluens_core::element::Element;

/// Each element registers only when the runtime takes its work.
pub fn register(registry: &mut Registry) {
    let Some(caps) = probe::probe() else { return };
    if !caps.decode_families.is_empty() {
        registry.register(VplDec::new().desc());
    }
    if !caps.encode_families.is_empty() {
        registry.register(VplEnc::new().desc());
    }
    registry.register(VplVpp::new(Target::default()).desc());
}

// Cold, autoplug construction.
#[allow(clippy::disallowed_methods)]
pub fn video_decoder_for(family: &str) -> Option<Box<dyn Element>> {
    let caps = probe::probe()?;
    caps.supports(family).then(|| Box::new(VplDec::new()) as Box<dyn Element>)
}

// Cold, autoplug construction.
#[allow(clippy::disallowed_methods)]
pub fn video_decoder_zerocopy_for(
    family: &str,
    channel: std::sync::Arc<profluens_video::gpuframe::GpuFrameChannel>,
) -> Option<Box<dyn Element>> {
    let caps = probe::probe()?;
    caps.supports(family).then(|| Box::new(VplDec::new_zerocopy(channel)) as Box<dyn Element>)
}
