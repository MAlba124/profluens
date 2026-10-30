//! Re-exports `profluens_video::gpuframe` plus the probe of what this driver exports.

pub use profluens_video::gpuframe::*;

use crate::va::VaResult;

/// A failed 10-bit export leaves P010 out and a 10-bit stream goes to readback. Takes and closes
/// the display.
// Once per zero-copy decoder, at preroll.
#[allow(clippy::disallowed_methods)]
pub(crate) fn exported_formats(display: crate::va::Display) -> VaResult<Vec<DmabufFormat>> {
    use crate::va::{ExportedSurface, SurfaceFormat};
    let mut formats = Vec::with_capacity(2);
    let (fourcc, modifier) = ExportedSurface::probe_for(&display, SurfaceFormat::NV12)?;
    formats.push(DmabufFormat { fourcc, modifiers: vec![modifier] });
    if let Ok((fourcc, modifier)) = ExportedSurface::probe_for(&display, SurfaceFormat::P010) {
        formats.push(DmabufFormat { fourcc, modifiers: vec![modifier] });
    }
    Ok(formats)
}
