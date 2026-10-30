//! Where a decoded or processed surface goes, one [`Output`] per src pad.

use std::sync::Arc;

use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::ctx::Ctx;
use profluens_core::error::Error;
use profluens_core::format::{OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;
use profluens_video::color as colour;
use profluens_video::decoder::{Color, color_fields};
use profluens_video::gpuframe::{
    self, DmabufFormat, F_LAYOUT, F_MODIFIER, FAMILY_VIDEO_GPU, FRAMES_OUT, FrameToken, FrameTx,
    GPU_FRAME_HEADER_LEN, GpuFrame, GpuFrameChannel,
};

use crate::dec::{Budget, OUT_BUDGET};
use crate::error::{VplErrorKind as K, failed};
use crate::ffi;
use crate::format::{self, FAMILY_RAW, Layout, RAW_OUT_PIXFMTS, VPL_PIXFMTS, fields};
use crate::frames::{
    F_FPS, F_HEIGHT, F_PIXFMT, F_WIDTH, FAMILY_VIDEO_VPL, Geometry, SurfaceChannel, SurfaceTx,
    VplFrame,
};
use crate::park::{BUSY_WAKE, Stall, stalled};
use crate::session::Surface;
use crate::stats::Meter;
use crate::va::{self, Device, Exported};

static RAW_FIELDS: [profluens_core::format::FieldDesc; 14] = fields(&RAW_OUT_PIXFMTS);
static VPL_FIELDS: [profluens_core::format::FieldDesc; 14] = fields(&VPL_PIXFMTS);
pub static RAW_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY_RAW, fields: &RAW_FIELDS }];
pub static VPL_OFFERS: [OfferDesc; 1] =
    [OfferDesc { family: FAMILY_VIDEO_VPL, fields: &VPL_FIELDS }];

pub enum Mode {
    Raw,
    Vpl(Arc<SurfaceChannel>),
    Gpu(Arc<GpuFrameChannel>),
}

impl Mode {
    pub fn offers(&self) -> &'static [OfferDesc] {
        match self {
            Mode::Raw => &RAW_OFFERS,
            Mode::Vpl(_) => &VPL_OFFERS,
            Mode::Gpu(_) => OfferDesc::PROBED,
        }
    }
}

pub struct Picture<'a> {
    pub surface: &'a Surface,
    pub geom: Geometry,
    pub pts: Timestamp,
    pub dts: Timestamp,
    pub duration: Timestamp,
    pub flags: BufferFlags,
    pub color: Color,
    pub fps: Option<(u32, u32)>,
    /// `ctx.steady()` when the runtime took the call that makes it, for the meter.
    pub submitted: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Shape {
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    color: Color,
    fps: Option<(u32, u32)>,
}

/// What an emit did with a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sent {
    Out,
    /// Lost on the way, counted as a drop.
    Dropped,
    /// Backpressure or not done yet, the caller keeps it.
    Kept,
}

/// Byte offsets of copying the display window of a mapped 4:2:0 surface out, nv12 to i420 and
/// p010 as it is. Chroma rows interleave Cb and Cr, two samples per two luma columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawCopy {
    /// Luma bytes before the window in a source row.
    pub y_skip: usize,
    /// Chroma bytes before the window in a source row.
    pub c_skip: usize,
    /// Luma bytes of one row out.
    pub y_row: usize,
    /// Interleaved chroma bytes read from one source row.
    pub c_row: usize,
    pub y_len: usize,
    /// One chroma plane out, Cb or Cr.
    pub c_len: usize,
    /// First chroma source row.
    pub c_top: u32,
    pub cw: u32,
    pub ch: u32,
}

pub fn raw_copy(layout: Layout, g: Geometry) -> RawCopy {
    let bps = layout.bps();
    let (w, h) = (g.disp_w, g.disp_h);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let y_row = w as usize * bps;
    RawCopy {
        y_skip: g.crop_x as usize * bps,
        c_skip: (g.crop_x / 2) as usize * 2 * bps,
        y_row,
        c_row: cw as usize * 2 * bps,
        y_len: y_row * h as usize,
        c_len: (cw * ch) as usize * bps,
        c_top: g.crop_y / 2,
        cw,
        ch,
    }
}

pub struct Output {
    pub(crate) mode: Mode,
    pad: SrcPad,
    sink: SinkPad,
    vpl_tx: Option<SurfaceTx>,
    gpu_tx: Option<FrameTx>,
    in_flight: Vec<(FrameToken, Surface)>,
    released: Vec<FrameToken>,
    next_token: FrameToken,
    shape: Option<Shape>,
    link: Option<(u32, u64)>,
    alloc_stalled: bool,
    /// Waiting on the runtime to finish the front picture.
    stall: Stall,
    drop_warned: bool,
    pub frames: u64,
    pub dropped: u64,
}

impl Output {
    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(mode: Mode, pad: SrcPad, sink: SinkPad) -> Output {
        Output {
            mode,
            pad,
            sink,
            vpl_tx: None,
            gpu_tx: None,
            in_flight: Vec::with_capacity(FRAMES_OUT),
            released: Vec::with_capacity(FRAMES_OUT),
            next_token: 1,
            shape: None,
            link: None,
            alloc_stalled: false,
            stall: Stall::default(),
            drop_warned: false,
            frames: 0,
            dropped: 0,
        }
    }

    pub fn is_gpu(&self) -> bool {
        matches!(self.mode, Mode::Gpu(_))
    }

    pub fn is_raw(&self) -> bool {
        matches!(self.mode, Mode::Raw)
    }

    /// What the `video/vpl` consumer declared it holds, `None` for other modes or nothing yet.
    pub fn hold(&self) -> Option<u32> {
        match &self.mode {
            Mode::Vpl(ch) => ch.hold(),
            _ => None,
        }
    }

    /// The surfaces to keep over the runtime's suggestion. `set` is the element's budget, zero
    /// asks for the consumer's hold plus two, `None` is [`OUT_BUDGET`].
    pub fn budget(&self, set: Option<u32>) -> Budget {
        let fixed = match set {
            Some(0) | None => None,
            Some(n) => Some(n),
        };
        match &self.mode {
            Mode::Raw => Budget::Raw,
            Mode::Vpl(ch) => Budget::Vpl(fixed.unwrap_or(match (set, ch.hold()) {
                (Some(0), Some(h)) => h.saturating_add(2),
                _ => OUT_BUDGET,
            })),
            Mode::Gpu(_) => Budget::Gpu(fixed.unwrap_or(OUT_BUDGET)),
        }
    }

    // Cold, once at preroll.
    #[allow(clippy::disallowed_methods)]
    pub fn preroll(&mut self, ctx: &mut Ctx, device: Option<&Device>) -> Result<(), Error> {
        if !self.is_gpu() {
            return Ok(());
        }
        let mut formats = Vec::with_capacity(2);
        if let Some(d) = device {
            for fourcc in [ffi::VA_FOURCC_NV12, ffi::VA_FOURCC_P010] {
                if let Ok((drm, modifier)) = va::probe_export(d, fourcc) {
                    formats.push(DmabufFormat { fourcc: drm, modifiers: vec![modifier] });
                }
            }
        }
        gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_src(self.pad, offers))
    }

    pub fn start(&mut self, ctx: &Ctx, element: &'static str) -> Result<(), Error> {
        match &self.mode {
            Mode::Raw => {}
            Mode::Vpl(ch) => {
                if self.vpl_tx.is_none() {
                    self.vpl_tx =
                        Some(ch.take_tx().ok_or_else(|| failed(ctx, element, K::ChannelTaken))?);
                }
            }
            Mode::Gpu(ch) => {
                if self.gpu_tx.is_none() {
                    self.gpu_tx =
                        Some(ch.take_tx().ok_or_else(|| failed(ctx, element, K::ChannelTaken))?);
                }
                self.link = gpuframe::link_layout(ctx, ctx.negotiated_src(self.pad));
            }
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        match &self.mode {
            Mode::Raw => {}
            Mode::Vpl(ch) => {
                if let Some(tx) = self.vpl_tx.take() {
                    ch.put_tx(tx);
                }
            }
            Mode::Gpu(ch) => {
                if let Some(tx) = self.gpu_tx.take() {
                    ch.put_tx(tx);
                }
            }
        }
        self.in_flight.clear();
        self.shape = None;
        self.alloc_stalled = false;
        self.stall.clear();
        self.drop_warned = false;
    }

    pub fn flush(&mut self) {
        self.alloc_stalled = false;
        self.stall.clear();
    }

    /// Once per `process` in gpu mode.
    pub fn drain_released(&mut self) {
        let Some(tx) = self.gpu_tx.as_mut() else {
            return;
        };
        tx.drain_released(&mut self.released);
        for token in self.released.drain(..) {
            if let Some(at) = self.in_flight.iter().position(|(t, _)| *t == token) {
                self.in_flight.swap_remove(at);
            }
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// `Ok(false)` keeps the picture, on backpressure or while the runtime still makes it. A wake
    /// is asked for either way. The meter takes the wait and the submit to done time.
    pub fn emit(
        &mut self,
        ctx: &mut Ctx,
        element: &'static str,
        pic: &Picture<'_>,
        meter: &mut Meter,
        side: &mut dyn FnMut(&mut Buffer),
    ) -> Result<bool, Error> {
        if !self.done(ctx, element, pic, meter)? {
            return Ok(false);
        }
        let sent = match &self.mode {
            Mode::Raw => self.emit_raw(ctx, element, pic, side)?,
            Mode::Vpl(_) => self.emit_vpl(ctx, element, pic, side)?,
            Mode::Gpu(_) => self.emit_gpu(ctx, element, pic, side)?,
        };
        match sent {
            Sent::Out => self.frames += 1,
            Sent::Dropped => {
                self.dropped += 1;
                ctx.count_drop();
            }
            Sent::Kept => return Ok(false),
        }
        // Counted as it leaves, a picture kept on backpressure is polled done again later.
        meter.done(pic.submitted, ctx.steady());
        Ok(true)
    }

    /// Polls the surface, a picture goes out only once the runtime finished it.
    fn done(
        &mut self,
        ctx: &mut Ctx,
        element: &'static str,
        pic: &Picture<'_>,
        meter: &mut Meter,
    ) -> Result<bool, Error> {
        match pic.surface.ready() {
            Ok(true) => {
                meter.polled(true, ctx.steady());
                self.stall.clear();
                Ok(true)
            }
            Ok(false) => {
                meter.polled(false, ctx.steady());
                if self.stall.tick(ctx.steady()) {
                    let e =
                        stalled(ffi::MFX_WRN_IN_EXECUTION, "mfxFrameSurfaceInterface.Synchronize");
                    return Err(failed(ctx, element, K::Stalled(e)));
                }
                ctx.wake_within(BUSY_WAKE);
                Ok(false)
            }
            Err(e) => Err(failed(ctx, element, K::Sync(e))),
        }
    }

    /// The first loss until stop is posted, the rest only counted.
    fn lose(&mut self, ctx: &mut Ctx, element: &'static str, kind: K) -> Sent {
        if !std::mem::replace(&mut self.drop_warned, true) {
            let el = ctx.element();
            ctx.post(profluens_core::bus::BusMessage::Warning {
                element: el,
                error: failed(ctx, element, kind),
            });
        }
        Sent::Dropped
    }

    fn stall(&mut self, ctx: &Ctx) {
        if !self.alloc_stalled {
            self.alloc_stalled = true;
            let s = ctx.pool_stats();
            log!(
                ctx,
                Level::Debug,
                "alloc_stall",
                outstanding = s.outstanding,
                max_slots = s.max_slots
            );
        }
    }

    fn announce(&mut self, ctx: &mut Ctx, pic: &Picture<'_>, modifier: u64) {
        let g = pic.geom;
        let shape = Shape {
            width: g.disp_w,
            height: g.disp_h,
            fourcc: g.fourcc,
            modifier,
            color: pic.color,
            fps: pic.fps,
        };
        if self.shape == Some(shape) {
            return;
        }
        self.shape = Some(shape);
        let layout = Layout::of_fourcc(g.fourcc).unwrap_or(Layout::Nv12);
        let mut out = [(F_WIDTH, ValueDesc::Int(i64::from(g.disp_w))); 4 + colour::FIELDS.len()];
        out[1] = (F_HEIGHT, ValueDesc::Int(i64::from(g.disp_h)));
        let (family, fixed) = match &self.mode {
            Mode::Raw => {
                out[2] = (F_PIXFMT, ValueDesc::Id(format::raw_name(layout)));
                out[3] = (
                    F_FPS,
                    ValueDesc::Rat(
                        pic.fps.map_or(0, |r| r.0 as i32),
                        pic.fps.map_or(1, |r| r.1 as i32),
                    ),
                );
                (FAMILY_RAW, if pic.fps.is_some() { 4 } else { 3 })
            }
            Mode::Vpl(_) => {
                out[2] = (F_PIXFMT, ValueDesc::Id(layout.name()));
                out[3] = (
                    F_FPS,
                    ValueDesc::Rat(
                        pic.fps.map_or(0, |r| r.0 as i32),
                        pic.fps.map_or(1, |r| r.1 as i32),
                    ),
                );
                (FAMILY_VIDEO_VPL, if pic.fps.is_some() { 4 } else { 3 })
            }
            Mode::Gpu(_) => {
                let name = gpuframe::layout_of(g.fourcc).unwrap_or(gpuframe::LAYOUT_NV12_DMABUF);
                out[2] = (F_LAYOUT, ValueDesc::Id(name));
                out[3] = (F_MODIFIER, ValueDesc::Int(modifier as i64));
                (FAMILY_VIDEO_GPU, 4)
            }
        };
        let n = fixed + color_fields(ctx, self.sink, pic.color, &mut out[fixed..]);
        log!(&*ctx, Level::Debug, "announce", width = g.disp_w, height = g.disp_h);
        ctx.announce_format(self.pad, family, &out[..n]);
    }

    fn stamp(buf: &mut Buffer, pic: &Picture<'_>) {
        buf.pts = pic.pts;
        buf.dts = pic.dts;
        buf.duration = pic.duration;
        buf.flags = pic.flags;
    }

    fn emit_raw(
        &mut self,
        ctx: &mut Ctx,
        element: &'static str,
        pic: &Picture<'_>,
        side: &mut dyn FnMut(&mut Buffer),
    ) -> Result<Sent, Error> {
        let g = pic.geom;
        let layout = Layout::of_fourcc(g.fourcc)
            .ok_or_else(|| failed(ctx, element, K::Layout { fourcc: g.fourcc }))?;
        let (w, h) = (g.disp_w, g.disp_h);
        let need = format::raw_frame_len(layout, w, h);
        let too_large = |ctx: &Ctx| K::FrameSlot {
            layout: layout.name(),
            width: w,
            height: h,
            need,
            capacity: ctx.slot_size(),
        };
        let Some(mut buf) =
            ctx.try_alloc_sized(need).map_err(|_| failed(ctx, element, too_large(ctx)))?
        else {
            self.stall(ctx);
            return Ok(Sent::Kept);
        };
        self.alloc_stalled = false;
        let mapped = pic.surface.map_read().map_err(|e| failed(ctx, element, K::Map(e)))?;
        let RawCopy { y_skip: cx, c_skip: cx_c, y_row, c_row, y_len, c_len, c_top, cw, ch } =
            raw_copy(layout, g);
        let dst = buf.memory.as_mut_full();
        let (y_dst, chroma) = dst.split_at_mut(y_len);
        for row in 0..h {
            let src = mapped
                .row(0, g.crop_y + row, cx + y_row)
                .ok_or_else(|| failed(ctx, element, K::PlaneOverrun { plane: "Y", row }))?;
            y_dst[row as usize * y_row..(row as usize + 1) * y_row].copy_from_slice(&src[cx..]);
        }
        let (cb, cr) = chroma.split_at_mut(if layout == Layout::P010 { 2 * c_len } else { c_len });
        for row in 0..ch {
            let src = mapped
                .row(1, c_top + row, cx_c + c_row)
                .ok_or_else(|| failed(ctx, element, K::PlaneOverrun { plane: "CbCr", row }))?;
            let src = &src[cx_c..];
            if layout == Layout::P010 {
                let at = row as usize * c_row;
                cb[at..at + c_row].copy_from_slice(src);
            } else {
                let (b, r) = (
                    &mut cb[(row * cw) as usize..((row + 1) * cw) as usize],
                    &mut cr[(row * cw) as usize..((row + 1) * cw) as usize],
                );
                for (i, pair) in src.as_chunks::<2>().0.iter().enumerate() {
                    b[i] = pair[0];
                    r[i] = pair[1];
                }
            }
        }
        drop(mapped);
        buf.memory.set_len(need);
        Self::stamp(&mut buf, pic);
        side(&mut buf);
        self.announce(ctx, pic, 0);
        ctx.out(self.pad).push(buf);
        Ok(Sent::Out)
    }

    fn emit_vpl(
        &mut self,
        ctx: &mut Ctx,
        element: &'static str,
        pic: &Picture<'_>,
        side: &mut dyn FnMut(&mut Buffer),
    ) -> Result<Sent, Error> {
        let Some(mut buf) = ctx
            .try_alloc_sized(GPU_FRAME_HEADER_LEN)
            .map_err(|e| failed(ctx, element, K::HeaderSlot { need: e.n, capacity: e.max }))?
        else {
            self.stall(ctx);
            return Ok(Sent::Kept);
        };
        self.alloc_stalled = false;
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        pic.geom.header(token).write_to(&mut buf.memory.as_mut_full()[..GPU_FRAME_HEADER_LEN]);
        buf.memory.set_len(GPU_FRAME_HEADER_LEN);
        Self::stamp(&mut buf, pic);
        side(&mut buf);
        let frame = VplFrame::new(token, pic.surface.clone(), pic.geom, ctx.applied_seek_gen());
        let tx = self.vpl_tx.as_mut().ok_or_else(|| failed(ctx, element, K::ChannelTaken))?;
        // A full channel frees as the consumer takes frames and their slots come back.
        if !tx.push(frame, buf.memory.clone()) {
            ctx.await_recycle();
            return Ok(Sent::Kept);
        }
        self.announce(ctx, pic, 0);
        log!(&*ctx, Level::Trace, "vpl_frame", pts = buf.pts, token = token);
        ctx.out(self.pad).push(buf);
        Ok(Sent::Out)
    }

    fn emit_gpu(
        &mut self,
        ctx: &mut Ctx,
        element: &'static str,
        pic: &Picture<'_>,
        side: &mut dyn FnMut(&mut Buffer),
    ) -> Result<Sent, Error> {
        if self.in_flight.len() >= FRAMES_OUT {
            ctx.await_recycle();
            return Ok(Sent::Kept);
        }
        let va_id =
            pic.surface.native_handle().map_err(|e| failed(ctx, element, K::NativeHandle(e)))?;
        let dpy =
            pic.surface.device_handle().map_err(|e| failed(ctx, element, K::NativeHandle(e)))?;
        let exported = match Exported::export(dpy, va_id) {
            Ok(e) => e,
            Err(e) => return Ok(self.lose(ctx, element, K::Export(e))),
        };
        if let Some((fourcc, negotiated)) =
            self.link.filter(|(f, m)| *f == exported.drm_format && *m != exported.drm_modifier)
        {
            let _ = fourcc;
            return Err(failed(
                ctx,
                element,
                K::ModifierChanged { negotiated, exported: exported.drm_modifier },
            ));
        }
        let Some(mut buf) = ctx
            .try_alloc_sized(GPU_FRAME_HEADER_LEN)
            .map_err(|e| failed(ctx, element, K::HeaderSlot { need: e.n, capacity: e.max }))?
        else {
            self.stall(ctx);
            return Ok(Sent::Kept);
        };
        self.alloc_stalled = false;
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        let g = pic.geom;
        g.header(token).write_to(&mut buf.memory.as_mut_full()[..GPU_FRAME_HEADER_LEN]);
        buf.memory.set_len(GPU_FRAME_HEADER_LEN);
        Self::stamp(&mut buf, pic);
        side(&mut buf);
        // The export's fds move into the frame, no dup. A frame the channel refuses closes them
        // and the next try exports again.
        let mut exported = exported;
        let frame = GpuFrame {
            token,
            fds: std::mem::take(&mut exported.fds),
            drm_format: exported.drm_format,
            coded_w: exported.coded_w,
            coded_h: exported.coded_h,
            disp_w: g.disp_w,
            disp_h: g.disp_h,
            crop_x: g.crop_x,
            crop_y: g.crop_y,
            planes: exported.planes.clone(),
            epoch: ctx.applied_seek_gen(),
            lease: None,
        };
        let modifier = exported.drm_modifier;
        drop(exported);
        let tx = self.gpu_tx.as_mut().ok_or_else(|| failed(ctx, element, K::ChannelTaken))?;
        // The frame's fds close with it, the picture waits for the consumer to free a place.
        if !tx.push(frame, buf.memory.clone()) {
            ctx.await_recycle();
            return Ok(Sent::Kept);
        }
        self.in_flight.push((token, pic.surface.clone()));
        self.link = self.link.or(Some((g.fourcc, modifier)));
        self.announce(ctx, pic, modifier);
        log!(&*ctx, Level::Trace, "gpu_frame", pts = buf.pts, token = token);
        ctx.out(self.pad).push(buf);
        Ok(Sent::Out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(w: u32, h: u32, crop_x: u32, crop_y: u32) -> Geometry {
        Geometry {
            coded_w: (w + crop_x).next_multiple_of(16),
            coded_h: (h + crop_y).next_multiple_of(16),
            disp_w: w,
            disp_h: h,
            crop_x,
            crop_y,
            fourcc: 0,
        }
    }

    #[test]
    fn the_chroma_window_starts_two_bytes_a_sample_pair_in() {
        let c = raw_copy(Layout::Nv12, geom(320, 240, 16, 8));
        assert_eq!((c.y_skip, c.c_skip, c.c_top), (16, 16, 4), "Cb Cr pairs at x 8 of 160");
        assert_eq!((c.y_row, c.c_row, c.cw, c.ch), (320, 320, 160, 120));
        assert_eq!((c.y_len, c.c_len), (320 * 240, 160 * 120));
        let c = raw_copy(Layout::P010, geom(320, 240, 16, 8));
        assert_eq!((c.y_skip, c.c_skip, c.c_row), (32, 32, 640), "16 bit samples");
        assert_eq!((c.y_len, c.c_len), (320 * 240 * 2, 160 * 120 * 2));
        let c = raw_copy(Layout::Nv12, geom(7, 5, 3, 3));
        assert_eq!((c.y_skip, c.c_skip, c.c_top, c.cw, c.ch), (3, 2, 1, 4, 3), "odd sizes");
    }

    #[test]
    fn the_copy_fills_the_frame_it_is_sized_for() {
        for layout in [Layout::Nv12, Layout::P010] {
            for (w, h) in [(2, 2), (7, 5), (1920, 1080), (3840, 2160)] {
                let c = raw_copy(layout, geom(w, h, 0, 0));
                assert_eq!(c.y_len + 2 * c.c_len, format::raw_frame_len(layout, w, h));
                assert_eq!(c.c_row, 2 * c.cw as usize * layout.bps());
                assert_eq!(c.c_row * c.ch as usize, 2 * c.c_len, "{layout:?} {w}x{h}");
            }
        }
    }
}
