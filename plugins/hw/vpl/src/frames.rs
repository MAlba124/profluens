//! The `video/vpl` hand over. A [`VplFrame`] goes on a [`SurfaceChannel`] and a header buffer
//! on the pad, paired by token. Dropping the frame releases the surface and the header slot.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::memory::Memory;
use profluens_core::ring::{Consumer, Producer, spsc};
use profluens_video::gpuframe::{FRAMES_OUT, FrameToken, GpuFrameHeader};

use crate::session::Surface;

/// Only VPL elements offer it, so a link of it never meets a CPU consumer.
pub const FAMILY_VIDEO_VPL: &str = "video/vpl";
pub const F_WIDTH: &str = "width";
pub const F_HEIGHT: &str = "height";
pub const F_PIXFMT: &str = "pixfmt";
pub const F_FPS: &str = "fps";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub coded_w: u32,
    pub coded_h: u32,
    pub disp_w: u32,
    pub disp_h: u32,
    pub crop_x: u32,
    pub crop_y: u32,
    pub fourcc: u32,
}

impl Geometry {
    pub fn header(&self, token: FrameToken) -> GpuFrameHeader {
        GpuFrameHeader {
            token,
            coded_w: self.coded_w,
            coded_h: self.coded_h,
            disp_w: self.disp_w,
            disp_h: self.disp_h,
            crop_x: self.crop_x,
            crop_y: self.crop_y,
        }
    }
}

pub struct VplFrame {
    pub token: FrameToken,
    pub surface: Surface,
    pub geom: Geometry,
    pub epoch: u64,
    slot: Option<Memory>,
}

impl VplFrame {
    pub fn new(token: FrameToken, surface: Surface, geom: Geometry, epoch: u64) -> VplFrame {
        VplFrame { token, surface, geom, epoch, slot: None }
    }
}

pub struct SurfaceTx {
    frames: Producer<VplFrame>,
}

impl SurfaceTx {
    /// `false` when the ring is full and the frame is dropped.
    pub fn push(&mut self, mut frame: VplFrame, slot: Memory) -> bool {
        frame.slot = Some(slot);
        self.frames.try_push(frame).is_ok()
    }
}

pub struct SurfaceRx {
    frames: Consumer<VplFrame>,
    ahead: Option<VplFrame>,
}

impl SurfaceRx {
    fn next(&mut self) -> Option<VplFrame> {
        self.ahead.take().or_else(|| self.frames.try_pop())
    }

    /// Tokens rise, so a frame with a lower token lost its header and is dropped.
    pub fn take(&mut self, token: FrameToken) -> Option<VplFrame> {
        while let Some(frame) = self.next() {
            match frame.token.cmp(&token) {
                std::cmp::Ordering::Less => drop(frame),
                std::cmp::Ordering::Equal => return Some(frame),
                std::cmp::Ordering::Greater => {
                    self.ahead = Some(frame);
                    return None;
                }
            }
        }
        None
    }

    pub fn flush_before(&mut self, epoch: u64) {
        while let Some(frame) = self.next() {
            if frame.epoch >= epoch {
                self.ahead = Some(frame);
                return;
            }
        }
    }

    pub fn clear(&mut self) {
        while self.next().is_some() {}
    }
}

pub struct SurfaceChannel {
    ends: Mutex<(Option<SurfaceTx>, Option<SurfaceRx>)>,
    /// Surfaces the consumer holds at most, declared before the producer's init. Zero is none.
    hold: AtomicU32,
}

impl SurfaceChannel {
    // Once per chain.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Arc<SurfaceChannel> {
        let (frames, taken) = spsc(FRAMES_OUT);
        Arc::new(SurfaceChannel {
            ends: Mutex::new((
                Some(SurfaceTx { frames }),
                Some(SurfaceRx { frames: taken, ahead: None }),
            )),
            hold: AtomicU32::new(0),
        })
    }

    /// The consumer says how many surfaces it keeps at once, the producer sizes its pool by it.
    pub fn declare_hold(&self, n: u32) {
        self.hold.store(n, Ordering::Release);
    }

    pub fn hold(&self) -> Option<u32> {
        match self.hold.load(Ordering::Acquire) {
            0 => None,
            n => Some(n),
        }
    }

    fn ends(&self) -> std::sync::MutexGuard<'_, (Option<SurfaceTx>, Option<SurfaceRx>)> {
        self.ends.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn take_tx(&self) -> Option<SurfaceTx> {
        self.ends().0.take()
    }

    pub fn take_rx(&self) -> Option<SurfaceRx> {
        self.ends().1.take()
    }

    pub fn put_tx(&self, tx: SurfaceTx) {
        let mut ends = self.ends();
        ends.0 = Some(tx);
        Self::settle(&mut ends);
    }

    pub fn put_rx(&self, rx: SurfaceRx) {
        let mut ends = self.ends();
        ends.1 = Some(rx);
        Self::settle(&mut ends);
    }

    fn settle(ends: &mut (Option<SurfaceTx>, Option<SurfaceRx>)) {
        if let (Some(_), Some(rx)) = (&ends.0, &mut ends.1) {
            rx.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use profluens_video::gpuframe::GPU_FRAME_HEADER_LEN;

    #[test]
    fn the_header_carries_the_geometry_and_the_token() {
        let g = Geometry {
            coded_w: 1920,
            coded_h: 1088,
            disp_w: 1920,
            disp_h: 1080,
            crop_x: 0,
            crop_y: 0,
            fourcc: 0,
        };
        let h = g.header(7);
        let mut bytes = [0u8; GPU_FRAME_HEADER_LEN];
        h.write_to(&mut bytes);
        let back = GpuFrameHeader::from_bytes(&bytes).unwrap();
        assert_eq!((back.token, back.coded_h, back.disp_h), (7, 1088, 1080));
    }

    #[test]
    fn both_ends_are_taken_once_and_come_back() {
        let ch = SurfaceChannel::new();
        let tx = ch.take_tx().unwrap();
        let rx = ch.take_rx().unwrap();
        assert!(ch.take_tx().is_none() && ch.take_rx().is_none());
        ch.put_tx(tx);
        ch.put_rx(rx);
        assert!(ch.take_tx().is_some() && ch.take_rx().is_some());
        assert_eq!(ch.hold(), None);
        ch.declare_hold(5);
        assert_eq!(ch.hold(), Some(5));
    }
}
