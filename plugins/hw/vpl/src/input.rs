//! The `vplvpp` and `vplenc` input side, `video/vpl` surfaces by token or uploaded `video/raw`.
//! A foreign surface is used directly, or imported where the runtime wants that.

use std::sync::Arc;

use profluens_core::ctx::Ctx;
use profluens_core::error::Error;
use profluens_core::format::FixedFormat;
use profluens_video::format::PixelFormat;
use profluens_video::geometry::{frame_size, plane_geometry, plane_offset};
use profluens_video::gpuframe::{FRAMES_OUT, FrameToken};

use crate::error::{MfxError, VplErrorKind as K, failed};
use crate::ffi;
use crate::format::InFormat;
use crate::frames::{SurfaceChannel, SurfaceRx, VplFrame};
use std::sync::Arc as SyncArc;

use crate::session::{Owner, Surface};

/// An imported surface is found again by its VA surface and its shape, so a producer that
/// reopens and hands out the same ids at another size never meets a stale import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportKey {
    pub va: ffi::VASurfaceID,
    pub width: u16,
    pub height: u16,
    pub fourcc: u32,
}

pub struct Input {
    channel: Option<Arc<SurfaceChannel>>,
    rx: Option<SurfaceRx>,
    imports: Vec<(ImportKey, Surface)>,
    /// `None` until the first frame says.
    direct: Option<bool>,
    component: ffi::mfxSurfaceComponent,
}

impl Input {
    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    pub fn raw() -> Input {
        Input {
            channel: None,
            rx: None,
            imports: Vec::with_capacity(FRAMES_OUT),
            direct: None,
            component: ffi::MFX_SURFACE_COMPONENT_VPP_INPUT,
        }
    }

    pub fn surfaces(channel: Arc<SurfaceChannel>) -> Input {
        Input { channel: Some(channel), ..Input::raw() }
    }

    /// Imports land as encoder input instead of VPP input.
    pub fn for_encode(mut self) -> Input {
        self.component = ffi::MFX_SURFACE_COMPONENT_ENCODE;
        self
    }

    pub fn is_surfaces(&self) -> bool {
        self.channel.is_some()
    }

    /// Tells the producer how many of its surfaces this consumer keeps at once.
    pub fn declare_hold(&self, n: u32) {
        if let Some(ch) = &self.channel {
            ch.declare_hold(n);
        }
    }

    pub fn start(
        &mut self,
        ctx: &Ctx,
        element: &'static str,
        _negotiated: Option<&FixedFormat>,
    ) -> Result<(), Error> {
        if let (Some(ch), None) = (&self.channel, &self.rx) {
            self.rx = Some(ch.take_rx().ok_or_else(|| failed(ctx, element, K::ReceiverTaken))?);
        }
        Ok(())
    }

    /// A new shape upstream may come with reused VA surface ids.
    pub fn format_change(&mut self, _ctx: &Ctx, _f: &FixedFormat) {
        self.imports.clear();
    }

    /// Frames from before a seek wait in the channel with their buffers gone.
    pub fn flush(&mut self, ctx: &Ctx) {
        if let Some(r) = &mut self.rx {
            r.flush_before(ctx.applied_seek_gen());
        }
    }

    pub fn stop(&mut self) {
        self.imports.clear();
        if let (Some(ch), Some(rx)) = (&self.channel, self.rx.take()) {
            ch.put_rx(rx);
        }
    }

    pub fn take(&mut self, token: FrameToken) -> Option<VplFrame> {
        self.rx.as_mut()?.take(token)
    }

    pub fn reset_session(&mut self) {
        self.imports.clear();
    }

    /// A refusal of the very first foreign surface, before any went through directly.
    pub fn wants_import(&self, e: &MfxError) -> bool {
        self.channel.is_some()
            && self.direct.is_none()
            && matches!(
                e.status,
                ffi::MFX_ERR_INVALID_HANDLE
                    | ffi::MFX_ERR_UNDEFINED_BEHAVIOR
                    | ffi::MFX_ERR_UNSUPPORTED
                    | ffi::MFX_ERR_NULL_PTR
                    | ffi::MFX_ERR_LOCK_MEMORY
            )
    }

    pub fn mark_indirect(&mut self) {
        self.direct = Some(false);
    }

    pub fn mark_direct(&mut self) {
        if self.direct.is_none() {
            self.direct = Some(true);
        }
    }

    pub fn is_direct(&self) -> bool {
        self.direct != Some(false)
    }

    pub fn for_session(&mut self, owner: &SyncArc<Owner>, surface: &Surface) -> Result<Surface, K> {
        if self.direct != Some(false) {
            return Ok(surface.clone());
        }
        let i = surface.info();
        let key = ImportKey {
            va: surface.native_handle().map_err(K::NativeHandle)?,
            width: i.Width,
            height: i.Height,
            fourcc: i.FourCC,
        };
        if let Some((_, s)) = self.imports.iter().find(|(k, _)| *k == key) {
            return Ok(s.clone());
        }
        if self.imports.len() >= FRAMES_OUT {
            self.imports.clear();
        }
        let imported =
            owner.session.import(surface, self.component).map_err(K::Import)?.owned_by(owner);
        self.imports.push((key, imported.clone()));
        Ok(imported)
    }
}

/// i420's chroma is interleaved.
pub fn upload(surface: &Surface, f: InFormat, data: &[u8]) -> Result<(), K> {
    let need = frame_size(f.pixfmt, f.width, f.height);
    let short = K::ShortFrame { len: data.len(), need };
    if data.len() < need {
        return Err(short);
    }
    let geom = |p| {
        plane_geometry(f.pixfmt, f.width, f.height, p)
            .zip(plane_offset(f.pixfmt, f.width, f.height, p))
            .ok_or(short)
    };
    let mut m = surface.map_write().map_err(K::Map)?;
    let (yg, yo) = geom(0)?;
    for r in 0..yg.height {
        let src = data.get(yo + r * yg.stride..yo + (r + 1) * yg.stride).ok_or(short)?;
        let row = r as u32;
        m.row_mut(0, row, yg.stride)
            .ok_or(K::PlaneOverrun { plane: "Y", row })?
            .copy_from_slice(src);
    }
    let (cg, co) = geom(1)?;
    if f.pixfmt == PixelFormat::I420 {
        let (vg, vo) = geom(2)?;
        for r in 0..cg.height {
            let u = data.get(co + r * cg.stride..co + (r + 1) * cg.stride).ok_or(short)?;
            let v = data.get(vo + r * vg.stride..vo + (r + 1) * vg.stride).ok_or(short)?;
            let row = r as u32;
            let dst =
                m.row_mut(1, row, cg.stride * 2).ok_or(K::PlaneOverrun { plane: "CbCr", row })?;
            for ((d, &u), &v) in dst.as_chunks_mut::<2>().0.iter_mut().zip(u).zip(v) {
                *d = [u, v];
            }
        }
    } else {
        for r in 0..cg.height {
            let src = data.get(co + r * cg.stride..co + (r + 1) * cg.stride).ok_or(short)?;
            let row = r as u32;
            m.row_mut(1, row, cg.stride)
                .ok_or(K::PlaneOverrun { plane: "CbCr", row })?
                .copy_from_slice(src);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_first_refusal_of_a_foreign_surface_asks_for_an_import() {
        let refused = MfxError { status: ffi::MFX_ERR_UNDEFINED_BEHAVIOR, call: "x" };
        let other = MfxError { status: ffi::MFX_ERR_DEVICE_FAILED, call: "x" };
        let mut raw = Input::raw();
        assert!(!raw.wants_import(&refused), "an uploaded surface is the session's own");
        raw.channel = Some(SurfaceChannel::new());
        assert!(raw.wants_import(&refused) && !raw.wants_import(&other));
        raw.mark_direct();
        assert!(!raw.wants_import(&refused), "proven direct, a later error is not the surface");
        raw.direct = None;
        raw.mark_indirect();
        assert!(!raw.wants_import(&refused) && !raw.is_direct(), "imports already");
    }
}
