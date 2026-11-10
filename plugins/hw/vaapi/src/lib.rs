//! VA-API hardware video decode, encode and post-processing for profluens.
//! Hand-rolled FFI in [`ffi`], RAII wrappers in [`va`]. Elements register only when the probe finds
//! their entrypoint.

pub mod av1dec;
pub mod av1enc;
pub use profluens_video::bits as bitwriter;
pub mod colour;
pub mod decproc;
mod enc;
pub mod encshell;
pub mod error;
pub mod ffi;
pub mod gpuframe;
mod gpuin;
pub mod h264dec;
pub mod h264enc;
pub use profluens_video::h264parse;
pub mod h265dec;
pub mod h265enc;
pub use profluens_video::h265parse;
pub mod probe;
pub mod stats;
pub mod va;
pub mod vp8enc;
pub mod vp9;
pub mod vp9dec;
pub mod vp9enc;
mod vp9quant;
pub mod vpp;

#[cfg(test)]
mod coded_reuse;
#[cfg(test)]
mod enc_eos;
#[cfg(test)]
mod export_failure;
#[cfg(test)]
mod fence_storage;

pub use av1dec::VaapiAv1Dec;
pub use av1enc::VaapiAv1Enc;
pub use encshell::Settings;
pub use error::{VaapiError, VaapiErrorKind};
pub use gpuframe::{FrameToken, GpuFrame, GpuFrameChannel, GpuFrameHeader, Handoff};
pub use h264dec::VaapiH264Dec;
pub use h264enc::VaapiH264Enc;
pub use h265dec::VaapiH265Dec;
pub use h265enc::VaapiH265Enc;
pub use probe::{ConfigureError, ProbeConfig, VaCaps, configure, probe};
pub use stats::Stats;
pub use va::{ExportedPlane, ExportedSurface};
pub use vp8enc::VaapiVp8Enc;
pub use vp9dec::VaapiVp9Dec;
pub use vp9enc::VaapiVp9Enc;
pub use vpp::{Target, VaapiPostProc};

use pf_registry::{Elements, Raw, Registry};
use profluens_core::element::{Element, ElementDesc, PadDesc};

/// Every element this crate builds by name, whether or not the device runs it, see [`usable`].
pub static ELEMENTS: Elements = &[
    &h264dec::DESC,
    &h265dec::DESC,
    &vp9dec::DESC,
    &av1dec::DESC,
    &h264enc::DESC,
    &h265enc::DESC,
    &vp8enc::DESC,
    // Neither has run on hardware with the entrypoint yet (TODO.md).
    &vp9enc::DESC,
    &av1enc::DESC,
    &vpp::element::DESCS[0][0],
];

/// Whether the device `caps` describes runs `desc`, one of [`ELEMENTS`], by its first offers.
pub fn usable(caps: &VaCaps, desc: &ElementDesc) -> bool {
    let first = |pads: &[PadDesc]| pads.first().and_then(|p| p.offers.first()).map(|o| o.family);
    let (sink, src) = (first(desc.sinks).unwrap_or(""), first(desc.srcs).unwrap_or(""));
    match (Raw::of(sink), Raw::of(src)) {
        (Some(_), Some(_)) => caps.video_proc,
        (Some(_), None) => caps.supports_encode(src),
        (None, _) => caps.supports(sink),
    }
}

/// Register the elements the VA-API probe finds hardware for.
pub fn register(registry: &mut Registry) {
    if let Some(caps) = probe::probe() {
        for &desc in ELEMENTS.iter().filter(|d| usable(caps, d)) {
            registry.register(desc);
        }
    }
}

/// A hardware decoder for a negotiated decode family, `None` when unsupported.
// Cold: one boxing per element construction at autoplug time, not per-frame.
#[allow(clippy::disallowed_methods)]
pub fn video_decoder_for(family: &str) -> Option<Box<dyn Element>> {
    let caps = probe::probe()?;
    match family {
        "h264/annexb" if caps.supports("h264/annexb") => Some(Box::new(VaapiH264Dec::new())),
        "h265/annexb" if caps.supports("h265/annexb") => Some(Box::new(VaapiH265Dec::new())),
        "vp9" if caps.supports("vp9") => Some(Box::new(VaapiVp9Dec::new())),
        "av1" if caps.supports("av1") => Some(Box::new(VaapiAv1Dec::new())),
        _ => None,
    }
}

/// A zero-copy decoder of `family` whose pictures leave scaled into `target`'s box by the decode
/// call, bars around them when `fixed`. None where the driver does no decode processing for
/// every profile of the family or the decoder has none.
// Cold: one boxing per element construction, not per-frame.
#[allow(clippy::disallowed_methods)]
pub fn shrinking_decoder_for(
    family: &str,
    channel: std::sync::Arc<gpuframe::GpuFrameChannel>,
    target: vpp::Target,
    fixed: bool,
) -> Option<Box<dyn Element>> {
    let caps = probe::probe()?;
    if !caps.shrinks(family) {
        return None;
    }
    match family {
        "h264/annexb" => {
            Some(Box::new(VaapiH264Dec::new_zerocopy(channel).with_shrink(target, fixed)))
        }
        "h265/annexb" => {
            Some(Box::new(VaapiH265Dec::new_zerocopy(channel).with_shrink(target, fixed)))
        }
        "vp9" => Some(Box::new(VaapiVp9Dec::new_zerocopy(channel).with_shrink(target, fixed))),
        _ => None,
    }
}

/// A zero-copy decoder for a family, exporting dma-bufs into `channel`. `None` when unsupported.
// Cold: one boxing per element construction at autoplug time, not per-frame.
#[allow(clippy::disallowed_methods)]
pub fn video_decoder_zerocopy_for(
    family: &str,
    channel: std::sync::Arc<gpuframe::GpuFrameChannel>,
) -> Option<Box<dyn Element>> {
    let caps = probe::probe()?;
    match family {
        "h264/annexb" if caps.supports("h264/annexb") => {
            Some(Box::new(VaapiH264Dec::new_zerocopy(channel)))
        }
        "h265/annexb" if caps.supports("h265/annexb") => {
            Some(Box::new(VaapiH265Dec::new_zerocopy(channel)))
        }
        "vp9" if caps.supports("vp9") => Some(Box::new(VaapiVp9Dec::new_zerocopy(channel))),
        "av1" if caps.supports("av1") => Some(Box::new(VaapiAv1Dec::new_zerocopy(channel))),
        _ => None,
    }
}

#[cfg(test)]
// A caps fixture.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn usable_reads_the_probed_entrypoints() {
        let caps = VaCaps {
            device: std::path::PathBuf::new(),
            vendor: String::new(),
            version: (1, 0),
            decode_families: vec!["h264/annexb"],
            encode_families: vec!["av1"],
            profiles: vec![],
            video_proc: false,
        };
        let names: Vec<&str> =
            ELEMENTS.iter().filter(|d| usable(&caps, d)).map(|d| d.name).collect();
        assert_eq!(names, ["vaapih264dec", "vaapiav1enc"]);
    }
}
