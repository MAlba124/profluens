//! The `video/gpu` input the encoders share.

use std::sync::Arc;

use profluens_core::ctx::Ctx;
use profluens_core::format::FixedFormat;

use crate::error::VaapiErrorKind as K;
use crate::ffi;
use crate::gpuframe::{FRAMES_OUT, FrameRx, GpuFrame, GpuFrameChannel};
use crate::va::Display;
use crate::vpp::element::read_link;
use crate::vpp::format::Kind;
use crate::vpp::proc::{Imported, dmabuf_key, import};

pub(crate) use crate::vpp::element::exported as offers;

const IMPORTS_MAX: usize = FRAMES_OUT;

#[derive(Clone, Copy)]
pub(crate) enum Input<'a> {
    Raw(&'a [u8]),
    Surface(ffi::VASurfaceID),
}

pub(crate) struct GpuIn {
    channel: Arc<GpuFrameChannel>,
    frames: Option<FrameRx>,
    pub link: Option<(Kind, u64)>,
    imports: Vec<Imported>,
    pub fps: (u32, u32),
}

impl GpuIn {
    // Cold: construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(channel: Arc<GpuFrameChannel>) -> Self {
        GpuIn {
            channel,
            frames: None,
            link: None,
            imports: Vec::with_capacity(IMPORTS_MAX),
            fps: (0, 1),
        }
    }

    pub fn start(&mut self, ctx: &Ctx, negotiated: Option<&FixedFormat>) -> Result<(), K> {
        self.link = read_link(ctx, negotiated).or(self.link);
        if self.frames.is_none() {
            self.frames = Some(self.channel.take_rx().ok_or(K::ReceiverTaken)?);
        }
        Ok(())
    }

    pub fn format_change(&mut self, ctx: &Ctx, f: &FixedFormat) {
        self.link = read_link(ctx, Some(f)).or(self.link);
    }

    pub fn flush(&mut self, ctx: &Ctx) {
        if let Some(r) = &mut self.frames {
            r.flush_before(ctx.applied_seek_gen());
        }
    }

    pub fn stop(&mut self) {
        self.imports.clear();
        if let Some(r) = self.frames.take() {
            self.channel.put_rx(r);
        }
    }

    pub fn take(&mut self, token: u64) -> Option<GpuFrame> {
        self.frames.as_mut()?.take(token)
    }

    pub fn imported(&mut self, display: &Display, frame: &GpuFrame) -> Result<ffi::VASurfaceID, K> {
        let key = dmabuf_key(frame)?;
        if let Some(i) = self.imports.iter().find(|i| i.key == key) {
            return Ok(i.id);
        }
        if self.imports.len() >= IMPORTS_MAX {
            // a picture in flight may still read one, the destroy waits for it
            for i in &self.imports {
                let _ = crate::va::sync_surface(display, i.id);
            }
            self.imports.clear();
        }
        let (kind, modifier) = self.link.ok_or(K::NoGpuLink)?;
        let i = import(display, frame, kind, modifier)?;
        let id = i.id;
        self.imports.push(i);
        Ok(id)
    }
}
