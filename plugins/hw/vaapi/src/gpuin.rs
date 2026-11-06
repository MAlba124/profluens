//! The `video/gpu` input the encoders share.

use std::sync::Arc;

use profluens_core::ctx::Ctx;
use profluens_core::format::FixedFormat;
use profluens_core::time::Timestamp;

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
    /// Frames pass at most this often, numerator and denominator, (0, 1) for every one.
    pub max_fps: (u32, u32),
    /// When the last frame passed, ns.
    kept_ns: Option<u64>,
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
            max_fps: (0, 1),
            kept_ns: None,
        }
    }

    /// Whether the frame at `pts` passes the rate gate, `must` passes whatever the time. A
    /// frame passes once three quarters of the gate's period went by since the last one, so
    /// a source at twice the rate keeps every other frame and a slower one keeps them all.
    pub fn keep(&mut self, pts: Timestamp, must: bool) -> bool {
        let (num, den) = self.max_fps;
        let Some(at) = pts.nanos().filter(|_| num > 0) else {
            return true;
        };
        let period = u64::from(den.max(1)) * 1_000_000_000 / u64::from(num);
        let due = match self.kept_ns {
            Some(k) if at > k => at - k >= period - period / 4,
            _ => true,
        };
        if must || due {
            self.kept_ns = Some(at);
        }
        must || due
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
        self.kept_ns = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(num: u32, den: u32) -> GpuIn {
        let mut g = GpuIn::new(GpuFrameChannel::new());
        g.max_fps = (num, den);
        g
    }

    #[test]
    fn a_gate_at_half_the_rate_keeps_every_other_frame() {
        let mut g = gate(15, 1);
        let kept: Vec<bool> = (0..8u64).map(|i| g.keep(Timestamp(i * 33_333_333), false)).collect();
        assert_eq!(kept, [true, false, true, false, true, false, true, false]);
    }

    #[test]
    fn a_source_below_the_gate_passes_whole_and_a_must_frame_always() {
        let mut g = gate(30, 1);
        assert!((0..5u64).all(|i| g.keep(Timestamp(i * 41_666_666), false)));
        let mut g = gate(15, 1);
        assert!(g.keep(Timestamp(0), false));
        assert!(!g.keep(Timestamp(33_333_333), false));
        assert!(g.keep(Timestamp(40_000_000), true));
        // the must frame restarts the cadence
        assert!(!g.keep(Timestamp(73_333_333), false));
        assert!(g.keep(Timestamp(106_666_666), false));
    }

    #[test]
    fn no_gate_and_a_stamp_going_back_pass() {
        let mut g = gate(0, 1);
        assert!(g.keep(Timestamp(0), false) && g.keep(Timestamp(1), false));
        let mut g = gate(15, 1);
        assert!(g.keep(Timestamp(1_000_000_000), false));
        assert!(g.keep(Timestamp(0), false));
        assert!(g.keep(Timestamp::NONE, false));
    }
}
