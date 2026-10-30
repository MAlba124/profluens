//! Codec-neutral plumbing of the VA-API encoders: session setup, raw frame upload, submit and sync,
//! and the two rotated reconstruction surfaces of the IDR/P single-reference GOP.

use std::collections::VecDeque;

use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::error::Error;
use profluens_core::format::{FixedFormat, Value};
use profluens_core::id::SrcPad;
use profluens_core::time::Timestamp;

use crate::error::{VaapiErrorKind as K, failed};
use crate::ffi;
use crate::va::{
    self, Buffer as VaBuffer, Config, Context, Display, SurfaceFormat, Surfaces, UploadImage,
    VaError, VaResult,
};

pub(crate) const RAW_FAMILY: &str = "video/raw";
pub(crate) const F_WIDTH: &str = "width";
pub(crate) const F_HEIGHT: &str = "height";
pub(crate) const F_PIXFMT: &str = "pixfmt";
pub(crate) const F_FPS: &str = "fps";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RawPixFmt {
    Nv12,
    I420,
    P010,
}

impl RawPixFmt {
    pub(crate) fn from_caps_name(s: &str) -> Option<RawPixFmt> {
        match s {
            "nv12" => Some(RawPixFmt::Nv12),
            "i420" => Some(RawPixFmt::I420),
            "p010" => Some(RawPixFmt::P010),
            _ => None,
        }
    }

    pub(crate) fn bytes(self) -> usize {
        if self == RawPixFmt::P010 { 2 } else { 1 }
    }

    pub(crate) fn surface(self) -> SurfaceFormat {
        if self == RawPixFmt::P010 { SurfaceFormat::P010 } else { SurfaceFormat::NV12 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct InFormat {
    pub width: u32,
    pub height: u32,
    pub pixfmt: RawPixFmt,
    pub fps: (u32, u32),
}

impl InFormat {
    pub(crate) fn frame_bytes(&self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        (w * h + 2 * cw * ch) * self.pixfmt.bytes()
    }
}

pub(crate) fn read_in_format(ctx: &Ctx, f: &FixedFormat) -> Option<InFormat> {
    let int = |field: &str| -> Option<i64> {
        ctx.field_id(field).and_then(|id| f.get(id)).and_then(|v| match v {
            Value::Int(i) => Some(i),
            _ => None,
        })
    };
    let width = u32::try_from(int(F_WIDTH)?).ok()?;
    let height = u32::try_from(int(F_HEIGHT)?).ok()?;
    if !dims_in_range(width, height) {
        return None;
    }
    let pix = ctx
        .field_id(F_PIXFMT)
        .and_then(|id| f.get(id))
        .and_then(|v| match v {
            Value::Id(vid) => ctx.value_name(vid),
            _ => None,
        })
        .and_then(RawPixFmt::from_caps_name)?;
    let fps = ctx
        .field_id(F_FPS)
        .and_then(|id| f.get(id))
        .and_then(|v| match v {
            Value::Rat(num, den) if num > 0 && den > 0 => Some((num as u32, den as u32)),
            _ => None,
        })
        .unwrap_or((0, 1));
    Some(InFormat { width, height, pixfmt: pix, fps })
}

/// Past any VA-API encoder's picture size, and what the header and size arithmetic is safe for.
pub(crate) const DIM_MAX: u32 = 16384;

pub(crate) fn dims_in_range(width: u32, height: u32) -> bool {
    (1..=DIM_MAX).contains(&width) && (1..=DIM_MAX).contains(&height)
}

pub(crate) fn align_up(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

/// Parameter buffers a picture submits at most.
const PARAMS_MAX: usize = 16;
/// Coded frames staged while the output pool is dry, at most this many.
pub(crate) const STAGED_MAX: usize = 32;

/// The storage each picture reuses. Parameter buffers live until the submit, the coded bytes
/// until they go out.
pub(crate) struct FrameBufs {
    pub hold: Vec<VaBuffer>,
    ids: Vec<ffi::VABufferID>,
    pub coded: Vec<u8>,
    /// The length-prefixed rewrite of `coded`, swapped in after.
    pub conv: Vec<u8>,
}

impl FrameBufs {
    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new() -> Self {
        FrameBufs {
            hold: Vec::with_capacity(PARAMS_MAX),
            ids: Vec::with_capacity(PARAMS_MAX),
            coded: Vec::new(),
            conv: Vec::new(),
        }
    }

    pub(crate) fn param<T: Copy>(
        &mut self,
        display: &Display,
        engine: &EncEngine,
        type_: ffi::VABufferType,
        value: &T,
    ) -> VaResult<()> {
        self.hold.push(VaBuffer::new_struct(display, engine.context(), type_, value)?);
        Ok(())
    }

    /// A packed header, its parameter buffer then its data. `bits` counts from the start of `data`.
    pub(crate) fn packed(
        &mut self,
        display: &Display,
        engine: &EncEngine,
        type_: u32,
        data: &[u8],
        bits: u32,
        has_ep: bool,
    ) -> VaResult<()> {
        let hdr = ffi::VAEncPackedHeaderParameterBuffer {
            type_,
            bit_length: bits,
            has_emulation_bytes: has_ep as u8,
            va_reserved: [0; ffi::VA_PADDING_LOW],
        };
        self.param(display, engine, ffi::VAEncPackedHeaderParameterBufferType, &hdr)?;
        let ty = ffi::VAEncPackedHeaderDataBufferType;
        self.hold.push(VaBuffer::new_typed_bytes(display, engine.context(), ty, data)?);
        Ok(())
    }

    /// Submits the held buffers on `input`, the engine's upload surface for `None`, then drops
    /// them since VA consumed them.
    pub(crate) fn submit(
        &mut self,
        display: &Display,
        engine: &EncEngine,
        input: Option<ffi::VASurfaceID>,
    ) -> VaResult<()> {
        self.ids.clear();
        self.ids.extend(self.hold.iter().map(VaBuffer::id));
        let r = engine.submit_from(display, &self.ids, input.unwrap_or(engine.input));
        self.hold.clear();
        r
    }
}

/// Timing and flags of one coded frame.
#[derive(Clone, Copy)]
pub(crate) struct Stamp {
    pub pts: Timestamp,
    pub duration: Timestamp,
    pub key: bool,
    pub preview: bool,
}

/// A coded frame held while the output pool is dry.
pub(crate) struct OutFrame {
    pub bytes: Vec<u8>,
    pub stamp: Stamp,
}

/// Copies `bytes` into a pool buffer and pushes it on `pad`. `false` when the pool is dry.
pub(crate) fn send(
    ctx: &mut Ctx,
    pad: SrcPad,
    name: &'static str,
    bytes: &[u8],
    st: Stamp,
) -> Result<bool, Error> {
    let Some(mut buf) = ctx.try_alloc() else {
        return Ok(false);
    };
    let (len, capacity) = (bytes.len(), buf.memory.capacity());
    if capacity < len {
        return Err(failed(ctx, name, K::PacketSlot { len, capacity }));
    }
    buf.memory.as_mut_full()[..len].copy_from_slice(bytes);
    buf.memory.set_len(len);
    (buf.pts, buf.duration) = (st.pts, st.duration);
    buf.flags = if st.key { BufferFlags::KEYFRAME } else { BufferFlags::DELTA };
    if st.preview {
        buf.flags.insert(BufferFlags::PREVIEW);
    }
    ctx.out(pad).push(buf);
    Ok(true)
}

/// Where staged frames go, an element's src pad or a fake pool in tests.
pub(crate) trait Outlet {
    /// Pushes one frame, `false` when no slot is free. A dry pool wakes the element on a return.
    fn send(&mut self, bytes: &[u8], st: Stamp) -> Result<bool, Error>;
    /// Keeps the end of the stream back for this pass.
    fn hold_end(&mut self);
}

/// The src pad `pad` of the element `name`.
pub(crate) struct PadOut<'a> {
    pub ctx: &'a mut Ctx,
    pub pad: SrcPad,
    pub name: &'static str,
}

impl Outlet for PadOut<'_> {
    fn send(&mut self, bytes: &[u8], st: Stamp) -> Result<bool, Error> {
        send(self.ctx, self.pad, self.name, bytes, st)
    }

    fn hold_end(&mut self) {
        self.ctx.hold_end();
    }
}

/// Where the end of the stream stands for the staged frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    Open,
    /// Eos came with frames staged, the end is held each pass until they are out.
    Draining,
}

/// Coded frames waiting on the output pool, in order, with the storage of sent ones for reuse.
pub(crate) struct Staged {
    frames: VecDeque<OutFrame>,
    spare: Vec<Vec<u8>>,
    end: End,
}

impl Staged {
    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new() -> Self {
        Staged {
            frames: VecDeque::with_capacity(STAGED_MAX),
            spare: Vec::with_capacity(STAGED_MAX),
            end: End::Open,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.frames.len()
    }

    /// No room for another frame, input waits upstream.
    pub(crate) fn full(&self) -> bool {
        self.frames.len() >= STAGED_MAX
    }

    #[cfg(test)]
    fn end(&self) -> End {
        self.end
    }

    /// Stages a frame built aside, a codec config record say.
    pub(crate) fn push(&mut self, bytes: Vec<u8>, stamp: Stamp) {
        self.frames.push_back(OutFrame { bytes, stamp });
    }

    /// Sends `coded` straight out when nothing waits and a slot is free, else stages it and
    /// leaves a spare storage in `coded`.
    pub(crate) fn send_or_stage(
        &mut self,
        out: &mut impl Outlet,
        coded: &mut Vec<u8>,
        stamp: Stamp,
    ) -> Result<(), Error> {
        if self.frames.is_empty() && out.send(coded, stamp)? {
            return Ok(());
        }
        let next = self.spare.pop().unwrap_or_default();
        self.push(std::mem::replace(coded, next), stamp);
        Ok(())
    }

    /// Sends staged frames in order, `false` when the pool ran dry first.
    pub(crate) fn drain(&mut self, out: &mut impl Outlet) -> Result<bool, Error> {
        while let Some(front) = self.frames.front() {
            if !out.send(&front.bytes, front.stamp)? {
                return Ok(false);
            }
            if let Some(f) = self.frames.pop_front() {
                self.recycle(f.bytes);
            }
        }
        Ok(true)
    }

    /// On Eos. `false` while frames remain, the end is then held until `resume` sends them.
    pub(crate) fn finish(&mut self, out: &mut impl Outlet) -> Result<bool, Error> {
        self.end = End::Draining;
        self.resume(out)
    }

    /// Each pass. `true` when the stream may go on, `false` while the end is held for frames
    /// still staged.
    pub(crate) fn resume(&mut self, out: &mut impl Outlet) -> Result<bool, Error> {
        if self.end == End::Open {
            return Ok(true);
        }
        if self.drain(out)? {
            self.end = End::Open;
            return Ok(true);
        }
        out.hold_end();
        Ok(false)
    }

    /// Drops every staged frame, a flush or a stop.
    pub(crate) fn discard(&mut self) {
        while let Some(f) = self.frames.pop_front() {
            self.recycle(f.bytes);
        }
        self.end = End::Open;
    }

    fn recycle(&mut self, mut bytes: Vec<u8>) {
        if self.spare.len() < self.spare.capacity() {
            bytes.clear();
            self.spare.push(bytes);
        }
    }
}

/// Field order is drop order, the teardown order libva wants.
pub(crate) struct EncEngine {
    context: Context,
    _surfaces: Surfaces,
    _config: Config,
    input: ffi::VASurfaceID,
    recon: [ffi::VASurfaceID; 2],
    cur: usize,
    pub coded_w: u32,
    pub coded_h: u32,
    pub width: u32,
    pub height: u32,
    /// Of the input surface format, which sizes the coded buffer.
    bits_per_pixel: u32,
}

impl EncEngine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        display: &Display,
        profile: ffi::VAProfile,
        entrypoint: ffi::VAEntrypoint,
        rc_mode: u32,
        format: SurfaceFormat,
        width: u32,
        height: u32,
        align: u32,
    ) -> VaResult<EncEngine> {
        Self::new_packed(display, profile, entrypoint, rc_mode, format, width, height, align, 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_packed(
        display: &Display,
        profile: ffi::VAProfile,
        entrypoint: ffi::VAEntrypoint,
        rc_mode: u32,
        format: SurfaceFormat,
        width: u32,
        height: u32,
        align: u32,
        packed: u32,
    ) -> VaResult<EncEngine> {
        let coded_w = align_up(width.max(2), align);
        let coded_h = align_up(height.max(2), align);
        let config = Config::new_encode_with(
            display,
            profile,
            entrypoint,
            format.rt_format,
            rc_mode,
            packed,
        )?;
        let surfaces = Surfaces::new(display, format, coded_w, coded_h, 3)?;
        let context = Context::new(display, &config, coded_w as i32, coded_h as i32, &surfaces)?;
        let ids = surfaces.ids();
        let (input, recon) = (ids[0], [ids[1], ids[2]]);
        Ok(EncEngine {
            context,
            _surfaces: surfaces,
            _config: config,
            input,
            recon,
            cur: 0,
            coded_w,
            coded_h,
            width,
            height,
            bits_per_pixel: format.bits_per_pixel,
        })
    }

    pub(crate) fn context(&self) -> &Context {
        &self.context
    }

    pub(crate) fn input(&self) -> ffi::VASurfaceID {
        self.input
    }

    pub(crate) fn recon_cur(&self) -> ffi::VASurfaceID {
        self.recon[self.cur]
    }

    pub(crate) fn recon_ref(&self) -> ffi::VASurfaceID {
        self.recon[1 - self.cur]
    }

    pub(crate) fn advance_recon(&mut self) {
        self.cur = 1 - self.cur;
    }

    /// Edges are replicated into the alignment padding so padded blocks predict cheaply.
    pub(crate) fn upload(&self, display: &Display, data: &[u8], fmt: RawPixFmt) -> VaResult<()> {
        if fmt == RawPixFmt::P010 {
            return self.upload_p010(display, data);
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (pad_w, pad_h) = (self.coded_w as usize, self.coded_h as usize);
        let (pad_cw, pad_ch) = (pad_w / 2, pad_h / 2);

        let mut img = UploadImage::acquire(display, self.input, self.coded_w, self.coded_h)?;

        for row in 0..pad_h {
            let src_row = row.min(h - 1);
            let Some(dst) = img.row_mut(0, row as u32, pad_w) else {
                return Err(VaError { status: -1, ctx: "upload row past the image" });
            };
            let src = &data[src_row * w..src_row * w + w];
            dst[..w].copy_from_slice(src);
            let edge = src[w - 1];
            dst[w..].fill(edge);
        }

        let y_len = w * h;
        for row in 0..pad_ch {
            let src_row = row.min(ch - 1);
            let Some(dst) = img.row_mut(1, row as u32, pad_cw * 2) else {
                return Err(VaError { status: -1, ctx: "upload row past the image" });
            };
            match fmt {
                RawPixFmt::Nv12 => {
                    let src = &data[y_len + src_row * cw * 2..y_len + src_row * cw * 2 + cw * 2];
                    dst[..cw * 2].copy_from_slice(src);
                }
                RawPixFmt::I420 => {
                    let cb = &data[y_len + src_row * cw..y_len + src_row * cw + cw];
                    let cr_base = y_len + cw * ch;
                    let cr = &data[cr_base + src_row * cw..cr_base + src_row * cw + cw];
                    for i in 0..cw {
                        dst[i * 2] = cb[i];
                        dst[i * 2 + 1] = cr[i];
                    }
                }
                RawPixFmt::P010 => {}
            }
            let (last_cb, last_cr) = (dst[cw * 2 - 2], dst[cw * 2 - 1]);
            for i in cw..pad_cw {
                dst[i * 2] = last_cb;
                dst[i * 2 + 1] = last_cr;
            }
        }

        img.finish()
    }

    fn upload_p010(&self, display: &Display, data: &[u8]) -> VaResult<()> {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (pad_w, pad_h) = (self.coded_w as usize, self.coded_h as usize);
        let (pad_cw, pad_ch) = (pad_w / 2, pad_h / 2);
        let mut img = UploadImage::acquire_for(
            display,
            self.input,
            self.coded_w,
            self.coded_h,
            SurfaceFormat::P010,
        )?;
        for row in 0..pad_h {
            let at = row.min(h - 1) * w * 2;
            let Some(dst) = img.row_mut(0, row as u32, pad_w * 2) else {
                return Err(VaError { status: -1, ctx: "upload row past the image" });
            };
            let src = &data[at..at + w * 2];
            dst[..w * 2].copy_from_slice(src);
            let edge = [src[w * 2 - 2], src[w * 2 - 1]];
            dst[w * 2..].as_chunks_mut::<2>().0.iter_mut().for_each(|d| *d = edge);
        }
        let y_bytes = w * h * 2;
        for row in 0..pad_ch {
            let at = y_bytes + row.min(ch - 1) * cw * 4;
            let Some(dst) = img.row_mut(1, row as u32, pad_cw * 4) else {
                return Err(VaError { status: -1, ctx: "upload row past the image" });
            };
            let src = &data[at..at + cw * 4];
            dst[..cw * 4].copy_from_slice(src);
            let edge = [src[cw * 4 - 4], src[cw * 4 - 3], src[cw * 4 - 2], src[cw * 4 - 1]];
            dst[cw * 4..].as_chunks_mut::<4>().0.iter_mut().for_each(|d| *d = edge);
        }
        img.finish()
    }

    pub(crate) fn submit_from(
        &self,
        display: &Display,
        bufs: &[ffi::VABufferID],
        input: ffi::VASurfaceID,
    ) -> VaResult<()> {
        self.context.begin(input)?;
        let rendered = self.context.render(bufs);
        // vaEndPicture runs after a failed render too, so the context is not left mid-picture.
        rendered.and(self.context.end())?;
        va::sync_surface(display, input)
    }

    /// Raw size plus 4 KiB, output beyond the raw input means something is badly wrong.
    pub(crate) fn coded_buf_size(&self) -> u32 {
        self.coded_w * self.coded_h * self.bits_per_pixel / 8 + 4096
    }
}

#[cfg(test)]
// Test fixtures, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use profluens_core::error::Error;
    use profluens_core::time::Timestamp;

    use super::{End, Outlet, STAGED_MAX, Staged, Stamp, align_up};

    /// A pool of `free` slots. Sent pts land in `out`, `holds` counts the passes the end was held.
    struct FakePool {
        free: u32,
        out: Vec<Timestamp>,
        holds: u32,
    }

    impl Outlet for FakePool {
        fn send(&mut self, _bytes: &[u8], st: Stamp) -> Result<bool, Error> {
            if self.free == 0 {
                return Ok(false);
            }
            self.free -= 1;
            self.out.push(st.pts);
            Ok(true)
        }

        fn hold_end(&mut self) {
            self.holds += 1;
        }
    }

    fn stamp(pts: u64) -> Stamp {
        Stamp { pts: Timestamp(pts), duration: Timestamp::NONE, key: false, preview: false }
    }

    fn staged(n: u64) -> Staged {
        let mut s = Staged::new();
        for i in 0..n {
            s.push(vec![i as u8; 8], stamp(i));
        }
        s
    }

    #[test]
    fn an_end_on_a_dry_pool_holds_until_every_frame_is_out() {
        let mut s = staged(5);
        let mut pool = FakePool { free: 0, out: Vec::new(), holds: 0 };
        assert!(!s.finish(&mut pool).unwrap(), "nothing fits at the end");
        assert_eq!((s.end(), pool.holds), (End::Draining, 1));
        let mut passes = 0;
        loop {
            pool.free += 2;
            passes += 1;
            if s.resume(&mut pool).unwrap() {
                break;
            }
            assert_eq!(pool.holds, passes + 1, "held each pass a frame waits");
        }
        assert_eq!(pool.out, (0..5).map(Timestamp).collect::<Vec<_>>(), "all of them, in order");
        assert_eq!((s.len(), s.end(), passes), (0, End::Open, 3));
        assert!(s.resume(&mut pool).unwrap(), "an open end passes");
        assert_eq!(pool.holds, 3, "and holds nothing");
    }

    #[test]
    fn an_end_with_room_passes_at_once() {
        let mut s = staged(3);
        let mut pool = FakePool { free: 3, out: Vec::new(), holds: 0 };
        assert!(s.finish(&mut pool).unwrap());
        assert_eq!((s.len(), s.end(), pool.holds), (0, End::Open, 0));
    }

    #[test]
    fn a_flush_drops_the_staged_frames_and_the_held_end() {
        let mut s = staged(4);
        let mut pool = FakePool { free: 1, out: Vec::new(), holds: 0 };
        assert!(!s.finish(&mut pool).unwrap());
        s.discard();
        assert_eq!((s.len(), s.end()), (0, End::Open));
        assert!(s.resume(&mut pool).unwrap(), "nothing held after a flush");
        assert_eq!(pool.out, [Timestamp(0)]);
    }

    #[test]
    fn a_frame_waits_behind_the_staged_ones() {
        let mut s = staged(1);
        let mut pool = FakePool { free: 1, out: Vec::new(), holds: 0 };
        let mut coded = vec![9u8; 8];
        s.send_or_stage(&mut pool, &mut coded, stamp(1)).unwrap();
        assert_eq!((s.len(), pool.out.len()), (2, 0), "staged behind, the slot left alone");
        assert!(!s.drain(&mut pool).unwrap() && pool.out == [Timestamp(0)]);
        pool.free = 1;
        assert!(s.drain(&mut pool).unwrap());
        assert_eq!(pool.out, [Timestamp(0), Timestamp(1)]);
    }

    #[test]
    fn sent_storage_is_reused_for_the_next_frame_staged() {
        let mut s = staged(STAGED_MAX as u64);
        assert!(s.full());
        let mut pool = FakePool { free: u32::MAX, out: Vec::new(), holds: 0 };
        assert!(s.drain(&mut pool).unwrap());
        let mut coded = vec![1u8; 8];
        pool.free = 0;
        s.send_or_stage(&mut pool, &mut coded, stamp(0)).unwrap();
        assert!(coded.is_empty() && coded.capacity() >= 8, "a spare handed back");
    }

    #[test]
    fn align_rounds_up() {
        assert_eq!(align_up(1920, 16), 1920);
        assert_eq!(align_up(1080, 16), 1088);
        assert_eq!(align_up(1, 16), 16);
        assert_eq!(align_up(33, 32), 64);
    }
}
