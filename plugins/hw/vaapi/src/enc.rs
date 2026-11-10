//! Codec-neutral plumbing of the VA-API encoders: session setup, raw frame upload, submit and sync,
//! and the ring of reconstruction surfaces, the references of a P picture and the one it codes.

use std::collections::VecDeque;

use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::error::Error;
use profluens_core::format::{FixedFormat, Value};
use profluens_core::id::SrcPad;
use profluens_core::io::{Completion, Handle, IoError, IoResult, OpenMode};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::error::{VaapiErrorKind as K, failed};
use crate::ffi;
use crate::gpuframe::DmabufFds;
use crate::va::{
    self, Buffer as VaBuffer, CodedMap, Config, Context, Display, ExportedSurface, SurfaceFormat,
    Surfaces, UploadImage, VaError, VaResult,
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

    /// Submits the held buffers on `input`, then drops them since VA consumed them.
    pub(crate) fn submit(
        &mut self,
        display: &Display,
        engine: &EncEngine,
        input: ffi::VASurfaceID,
    ) -> VaResult<()> {
        self.ids.clear();
        self.ids.extend(self.hold.iter().map(VaBuffer::id));
        let r = engine.submit_from(display, &self.ids, input);
        self.hold.clear();
        r
    }

    /// [`Self::submit`] without the wait, the picture then [`InFlight`].
    pub(crate) fn queue(&mut self, engine: &EncEngine, input: ffi::VASurfaceID) -> VaResult<()> {
        self.ids.clear();
        self.ids.extend(self.hold.iter().map(VaBuffer::id));
        let r = engine.queue(&self.ids, input);
        self.hold.clear();
        r
    }
}

/// How long a picture on the GPU waits for a poll, the encoder asks to run again this soon.
pub(crate) const POLL_WAKE: std::time::Duration = std::time::Duration::from_millis(1);

/// How long a blocking wait in the driver runs before it looks for a pending command.
pub(crate) const WAIT_STEP_NS: u64 = 1_000_000;

/// How an encoder waits for the oldest picture in flight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum How {
    /// Not at all, a timed wake asked for when it is not done.
    Poll,
    /// In the driver, in steps, until it is done or a command pends, for an encoder on its own
    /// thread. The thread sleeps in the kernel's wait instead of the scheduler's timer.
    Step,
    /// In the driver until it is done.
    Wait,
}

/// The picture the GPU codes while the encoder returns to its caller, so the next decode is
/// queued behind it. It holds the coded buffer the bytes land in and the producer's frame it
/// reads, until the bytes are read.
pub(crate) struct InFlight {
    pub coded: VaBuffer,
    /// The parameter sets go in front of the bytes, a driver without packed headers writes none.
    pub head: bool,
    pub stamp: Stamp,
    pub _frame: Option<crate::gpuframe::GpuFrame>,
    /// The picture's number since the session opened, what its fence op is tagged with.
    pub seq: u64,
    pub fence: Fence,
}

/// How a picture in flight is known to be done.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fence {
    /// No reactor, or one that refused, the driver is asked on a timer.
    Polled,
    /// The reactor polls the fence of the surface it writes, its completion wakes the encoder,
    /// so the picture asks for no timed wake.
    Pending,
    /// The fence signalled, the batch that writes the surface and the coded bytes is done,
    /// the driver need not be asked.
    Signalled,
}

/// Pictures an encoder keeps on the GPU at most, each holds a producer's frame and a coded buffer.
pub(crate) const DEPTH_MAX: usize = 4;

/// The pictures handed to the GPU and not yet read, oldest first, [`Self::depth`] at most. The
/// GPU codes them in order while the encoder's thread goes on, so the next picture is queued
/// behind one still coding and the engine never waits for the CPU.
pub(crate) struct Flight {
    ring: VecDeque<InFlight>,
    depth: usize,
}

impl Flight {
    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new(depth: usize) -> Self {
        Flight { ring: VecDeque::with_capacity(DEPTH_MAX), depth: depth.clamp(1, DEPTH_MAX) }
    }

    pub(crate) fn set_depth(&mut self, depth: usize) {
        self.depth = depth.clamp(1, DEPTH_MAX);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// No room for another picture until the oldest is read.
    pub(crate) fn full(&self) -> bool {
        self.ring.len() >= self.depth
    }

    pub(crate) fn push(&mut self, f: InFlight) {
        self.ring.push_back(f);
    }

    /// The oldest picture, the one the GPU finishes first.
    pub(crate) fn front(&self) -> Option<&InFlight> {
        self.ring.front()
    }

    pub(crate) fn pop(&mut self) -> Option<InFlight> {
        self.ring.pop_front()
    }

    /// Every picture, oldest first, the ring left empty.
    pub(crate) fn drain(&mut self) -> std::collections::vec_deque::Drain<'_, InFlight> {
        self.ring.drain(..)
    }

    /// The fence of picture `seq` signalled, or never will and it polls from here on.
    pub(crate) fn unfence(&mut self, seq: u64, signalled: bool) {
        if let Some(f) = self.ring.iter_mut().find(|f| f.seq == seq) {
            f.fence = if signalled { Fence::Signalled } else { Fence::Polled };
        }
    }
}

/// The timer slack the thread has while the driver reads a picture back.
const MAP_SLACK_NS: libc::c_ulong = 500_000;

/// The calling thread's timer slack raised to `ns`, what it was put back on drop.
pub(crate) struct Slack {
    was: libc::c_ulong,
}

impl Slack {
    pub(crate) fn raise(ns: libc::c_ulong) -> Slack {
        // SAFETY: both calls read or set a number of the calling thread, no pointer is passed.
        let was = unsafe { libc::prctl(libc::PR_GET_TIMERSLACK) }.max(0) as libc::c_ulong;
        unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, ns.max(was)) };
        Slack { was }
    }
}

impl Drop for Slack {
    fn drop(&mut self) {
        // SAFETY: as in `raise`.
        unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, self.was) };
    }
}

/// The coded picture mapped, see [`VaBuffer::map_coded`]. iHD's HEVC encoder writes the count
/// its status report compares in a batch of its own behind the picture's, on no buffer an
/// application can wait on, so a picture whose fence signalled can still read incomplete. The
/// driver then sleeps 10 us a turn until that batch ran, each turn two ioctls, 16000 turns a
/// second at the default slack of 50 us. The slack raised makes it 2000.
pub(crate) fn map_coded(b: &VaBuffer) -> VaResult<CodedMap<'_>> {
    let _slack = Slack::raise(MAP_SLACK_NS);
    b.map_coded()
}

/// How often the pictures read are told, at most.
const READS_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// What a session's pictures were read on and how long their maps took, told at info so one
/// run says whether the completion signal holds on a driver.
#[derive(Default)]
pub(crate) struct Reads {
    /// Pictures mapped by the state of their fence, [`Fence::Polled`], [`Fence::Pending`]
    /// (the driver said done before the fence did) and [`Fence::Signalled`].
    by_fence: [u32; 3],
    /// Of them, those mapped without asking, the map waited.
    waited: u32,
    /// Maps under 100 us, under 1 ms and the rest. A map of a picture that is done is a few us.
    map: [u32; 3],
    map_max_us: u32,
    told: Option<std::time::Instant>,
}

impl Reads {
    /// A picture mapped, the map began at `since`.
    pub(crate) fn mapped(&mut self, fence: Fence, how: How, since: std::time::Instant) {
        let us = since.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        self.by_fence[fence as usize] += 1;
        self.waited += u32::from(how == How::Wait);
        self.map[usize::from(us >= 100) + usize::from(us >= 1000)] += 1;
        self.map_max_us = self.map_max_us.max(us);
    }

    fn due(&mut self, now: std::time::Instant, close: bool) -> bool {
        let pictures = self.by_fence.iter().sum::<u32>() > 0;
        let told = *self.told.get_or_insert(now);
        let due = pictures && (close || now.duration_since(told) >= READS_EVERY);
        if due {
            self.told = Some(now);
        }
        due
    }

    /// Tells the counts since the session opened, every [`READS_EVERY`] and when it closes.
    pub(crate) fn tell(&mut self, ctx: &Ctx, fences: &Fences, close: bool) {
        if !self.due(crate::stats::mark(), close) {
            return;
        }
        let [polled, pending, signalled] = self.by_fence;
        let [fast, slow, stuck] = self.map;
        log!(
            ctx,
            Level::Info,
            "reads",
            polled = polled,
            pending = pending,
            signalled = signalled,
            waited = self.waited
        );
        log!(
            ctx,
            Level::Info,
            "read_maps",
            under_100us = fast,
            under_1ms = slow,
            over_1ms = stuck,
            max_us = self.map_max_us
        );
        log!(ctx, Level::Info, "read_fences", exports = fences.exports, off = fences.off);
        if close {
            *self = Reads::default();
        }
    }
}

/// What a completion's `user` says it is, in its top bits.
const USER_KIND: u64 = 3 << 62;
const USER_ADOPT: u64 = 1 << 62;
const USER_READY: u64 = 2 << 62;

/// An adopt's `user`, the session it was asked in and the surface it is for.
fn adopt_tag(epoch: u32, slot: usize) -> u64 {
    USER_ADOPT | u64::from(epoch) << 8 | (slot as u64 & 0xFF)
}

/// Where the fence of a reconstruction surface stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Watch {
    /// No picture wrote the surface yet, the driver may still give it other storage.
    Unused,
    /// Exported, the reactor's adopt of it pends.
    Adopting,
    /// The reactor's handle of its dma-buf.
    Adopted(Handle),
}

/// What a picture queued on a reconstruction surface does about its fence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ask {
    /// Nothing, the picture polls.
    Poll,
    /// The surface is exported now that a picture writes it, the picture polls.
    Export,
    /// The reactor polls the fence.
    Ready(Handle),
}

/// The fences of a session's reconstruction surfaces. Each is exported as a dma-buf and adopted
/// by the reactor once, then a picture's completion is polled on the one it writes, so the
/// encoder wakes when the GPU is done and not on a timer. Without a reactor, or one that refuses,
/// the pictures poll as before.
///
/// A surface is exported once its first picture is queued and not before. iHD gives the
/// reconstruction of a 10 bit or 4:2:2 or 4:4:4 HEVC picture other storage when the picture's
/// parameters first name it (`DdiMedia_ReplaceSurfaceWithVariant`), an export from before
/// that is of storage no picture writes, its fence reads signalled at once and the coded
/// buffer is mapped with the picture still coding, which the driver waits out in a sleep loop.
pub(crate) struct Fences {
    /// One a reconstruction surface, kept open for the session, empty until exported.
    fds: Vec<DmabufFds>,
    watch: Vec<Watch>,
    /// The fds of adopts a closed session left pending, by the adopt's tag. An adopted fd
    /// stays open until its op completes.
    parked: Vec<(u64, DmabufFds)>,
    /// Handles of adopts that completed after their session closed, closed on the next call.
    stale: Vec<Handle>,
    /// Sessions closed, what tells a pending adopt of an old one from this one's.
    epoch: u32,
    /// The reactor refused an op, nothing more is asked of it.
    off: bool,
    /// [`Self::off`] was logged.
    off_told: bool,
    /// Surfaces exported since the session opened.
    exports: u32,
    /// Pictures queued since the element was made, the next picture's number.
    next_seq: u64,
}

impl Fences {
    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new() -> Self {
        Fences {
            fds: Vec::with_capacity(REFS_MAX + 1),
            watch: Vec::with_capacity(REFS_MAX + 1),
            parked: Vec::with_capacity(REFS_MAX + 1),
            stale: Vec::with_capacity(REFS_MAX + 1),
            epoch: 0,
            off: false,
            off_told: false,
            exports: 0,
            next_seq: 0,
        }
    }

    /// A session opened on `surfaces` reconstruction surfaces, none exported yet. The last
    /// session's handles are closed first.
    pub(crate) fn open(&mut self, ctx: &mut Ctx, surfaces: usize) {
        self.close(ctx);
        self.reset(surfaces);
    }

    fn reset(&mut self, surfaces: usize) {
        self.fds.resize_with(surfaces, DmabufFds::default);
        self.exports = 0;
        self.watch.resize(surfaces, Watch::Unused);
    }

    /// The session is gone, the reactor's handles with it.
    pub(crate) fn close(&mut self, ctx: &mut Ctx) {
        self.retire();
        self.sweep(ctx);
    }

    /// Forgets the session. Its handles go to [`Self::stale`], the fds of its pending adopts
    /// are parked until those complete.
    fn retire(&mut self) {
        for (slot, (w, fds)) in self.watch.drain(..).zip(self.fds.drain(..)).enumerate() {
            match w {
                Watch::Adopted(h) => self.stale.push(h),
                Watch::Adopting => self.parked.push((adopt_tag(self.epoch, slot), fds)),
                Watch::Unused => {}
            }
        }
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn sweep(&mut self, ctx: &mut Ctx) {
        for h in self.stale.drain(..) {
            ctx.io().close(h, 0);
        }
    }

    /// A completion of the element. The number of the picture whose fence op ended, if it was
    /// one, and whether the fence signalled (else the op failed and the picture polls), the
    /// adopts noted on the way.
    pub(crate) fn completed(&mut self, c: &Completion) -> Option<(u64, bool)> {
        self.note(c.user, c.result)
    }

    fn note(&mut self, user: u64, result: IoResult) -> Option<(u64, bool)> {
        let refused = matches!(
            result,
            IoResult::Err(IoError::NoBackend | IoError::Unsupported | IoError::WrongKind)
        );
        self.off |= refused;
        match user & USER_KIND {
            USER_ADOPT => {
                let slot = (user & 0xFF) as usize;
                let handle = match result {
                    IoResult::File { handle, .. } => Some(handle),
                    _ => None,
                };
                let ours = user == adopt_tag(self.epoch, slot);
                match (self.watch.get_mut(slot), handle) {
                    (Some(w), Some(h)) if ours && *w == Watch::Adopting => *w = Watch::Adopted(h),
                    (_, h) => {
                        // an old session's, its fd was kept open for the op and goes now
                        self.parked.retain(|(tag, _)| *tag != user);
                        self.stale.extend(h);
                    }
                }
                None
            }
            USER_READY => Some((user & !USER_KIND, !matches!(result, IoResult::Err(_)))),
            _ => None,
        }
    }

    fn ask(&self, slot: usize) -> Ask {
        match self.watch.get(slot) {
            _ if self.off => Ask::Poll,
            Some(Watch::Unused) => Ask::Export,
            Some(&Watch::Adopted(h)) => Ask::Ready(h),
            Some(Watch::Adopting) | None => Ask::Poll,
        }
    }

    /// A picture queued, writing `surface`, the reconstruction `slot`. Its number, and whether
    /// a completion comes for it. The first picture on a surface polls, the surface is
    /// exported behind it.
    pub(crate) fn queued(
        &mut self,
        ctx: &mut Ctx,
        dpy: ffi::VADisplay,
        surface: ffi::VASurfaceID,
        slot: usize,
    ) -> (u64, Fence) {
        let seq = self.next_seq;
        self.next_seq += 1;
        #[cfg(test)]
        self.audit(dpy, surface, slot);
        self.sweep(ctx);
        if self.off && !self.off_told {
            self.off_told = true;
            log!(&*ctx, Level::Info, "fences_off", seq = seq);
        }
        match self.ask(slot) {
            Ask::Poll => {}
            Ask::Ready(handle) => {
                ctx.io().ready(handle, USER_READY | seq);
                return (seq, Fence::Pending);
            }
            Ask::Export => self.export(ctx, dpy, surface, slot),
        }
        (seq, Fence::Polled)
    }

    /// Notes in [`WATCHED`] whether the export held for `slot` is of the storage `surface` has
    /// now, the dma-buf of an object is one inode however often it is exported.
    #[cfg(test)]
    fn audit(&self, dpy: ffi::VADisplay, surface: ffi::VASurfaceID, slot: usize) {
        use std::os::fd::AsRawFd;
        let ino = |fd: std::os::fd::BorrowedFd<'_>| {
            // SAFETY: a zeroed stat is a valid out struct, fd is open while borrowed.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            (unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == 0).then_some(st.st_ino)
        };
        let Some(held) = self.fds.get(slot).and_then(|f| f.fd(0)).and_then(ino) else {
            return;
        };
        let Ok(now) = ExportedSurface::export_queued(dpy, surface) else {
            return;
        };
        let now = now.fds.fd(0).and_then(ino);
        let (live, stale) = WATCHED.get();
        WATCHED.set(if now == Some(held) { (live + 1, stale) } else { (live, stale + 1) });
    }

    /// Exports `surface` and hands it to the reactor. A driver that does not export turns the
    /// fences off.
    fn export(
        &mut self,
        ctx: &mut Ctx,
        dpy: ffi::VADisplay,
        surface: ffi::VASurfaceID,
        slot: usize,
    ) {
        let fds = ExportedSurface::export_queued(dpy, surface)
            .map(|mut e| std::mem::take(&mut e.fds))
            .unwrap_or_default();
        match fds.fd(0) {
            Some(fd) => {
                ctx.io().adopt(fd, OpenMode::Read, adopt_tag(self.epoch, slot));
                self.exports += 1;
                self.fds[slot] = fds;
                self.watch[slot] = Watch::Adopting;
            }
            None => self.off = true,
        }
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

/// Pushes `len` bytes on `pad`, `fill` writes them straight into a pool buffer. `false` when the
/// pool is dry, `fill` then not called.
pub(crate) fn send(
    ctx: &mut Ctx,
    pad: SrcPad,
    name: &'static str,
    len: usize,
    fill: &dyn Fn(&mut [u8]),
    st: Stamp,
) -> Result<bool, Error> {
    let Some(mut buf) = ctx.try_alloc() else {
        return Ok(false);
    };
    let capacity = buf.memory.capacity();
    if capacity < len {
        return Err(failed(ctx, name, K::PacketSlot { len, capacity }));
    }
    fill(&mut buf.memory.as_mut_full()[..len]);
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
    /// Pushes one frame of `len` bytes `fill` writes, `false` when no slot is free. A dry pool
    /// wakes the element on a return.
    fn send(&mut self, len: usize, fill: &dyn Fn(&mut [u8]), st: Stamp) -> Result<bool, Error>;
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
    fn send(&mut self, len: usize, fill: &dyn Fn(&mut [u8]), st: Stamp) -> Result<bool, Error> {
        send(self.ctx, self.pad, self.name, len, fill, st)
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
        let fill = |d: &mut [u8]| d.copy_from_slice(coded);
        if self.frames.is_empty() && out.send(coded.len(), &fill, stamp)? {
            return Ok(());
        }
        let next = self.spare.pop().unwrap_or_default();
        self.push(std::mem::replace(coded, next), stamp);
        Ok(())
    }

    /// Sends the coded picture in `map` behind `head` straight from the driver's buffer into a
    /// slot when nothing waits and one is free, else into staged storage. One copy either way.
    pub(crate) fn send_coded(
        &mut self,
        out: &mut impl Outlet,
        head: &[&[u8]],
        map: &CodedMap<'_>,
        stamp: Stamp,
    ) -> Result<(), Error> {
        let len = head.iter().map(|h| h.len()).sum::<usize>() + map.len();
        let fill = |d: &mut [u8]| {
            let mut rest = d;
            for h in head {
                let Some((to, after)) = rest.split_at_mut_checked(h.len()) else { return };
                to.copy_from_slice(h);
                rest = after;
            }
            map.copy_to(rest);
        };
        if self.frames.is_empty() && out.send(len, &fill, stamp)? {
            return Ok(());
        }
        let mut bytes = self.spare.pop().unwrap_or_default();
        bytes.clear();
        bytes.reserve(len);
        head.iter().for_each(|h| bytes.extend_from_slice(h));
        map.append_to(&mut bytes);
        self.push(bytes, stamp);
        Ok(())
    }

    /// Sends staged frames in order, `false` when the pool ran dry first.
    pub(crate) fn drain(&mut self, out: &mut impl Outlet) -> Result<bool, Error> {
        while let Some(front) = self.frames.front() {
            let fill = |d: &mut [u8]| d.copy_from_slice(&front.bytes);
            if !out.send(front.bytes.len(), &fill, front.stamp)? {
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

/// References an engine keeps at most.
pub(crate) const REFS_MAX: usize = 4;

/// L0 and L1 references a picture takes at most on `entrypoint`, VAConfigAttribEncMaxRefFrames
/// (low 16 bits L0, high 16 L1), 1 each when the driver does not say.
pub(crate) fn max_refs(
    display: &Display,
    profile: ffi::VAProfile,
    entrypoint: ffi::VAEntrypoint,
) -> (u32, u32) {
    match va::config_attribute(display, profile, entrypoint, ffi::VAConfigAttribEncMaxRefFrames) {
        Ok(Some(v)) => (v & 0xFFFF, v >> 16),
        _ => (1, 1),
    }
}

/// The spacing of the source's intra pictures, which tells its keyframe grid from scene cuts.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct IntraCadence {
    /// Pictures since the last intra one.
    since: u32,
    /// Pictures since the last intra one on the grid, or any before a grid is known.
    since_grid: u32,
    /// Pictures between the last two intra ones, 0 before two came.
    gap: u32,
    /// The source's keyframe interval, a gap that came twice in a row, 0 before one did.
    grid: u32,
    /// A picture came since the start or the last flush.
    begun: bool,
}

impl IntraCadence {
    /// Each picture in order. True for an intra one off the source's grid, a scene cut. One
    /// after a cut that keeps the grid's phase is on it.
    pub(crate) fn note(&mut self, intra: bool) -> bool {
        // the spacing counts from the start, an intra picture there is that start
        if !std::mem::replace(&mut self.begun, true) && intra {
            return false;
        }
        self.since = self.since.saturating_add(1);
        self.since_grid = self.since_grid.saturating_add(1);
        if !intra {
            return false;
        }
        // within an eighth, a source encoder lets its grid drift a picture or two
        let near = |a: u32, b: u32| b != 0 && a.abs_diff(b) <= (b / 8).max(1);
        let repeat = near(self.since, self.gap);
        if repeat {
            self.grid = self.since;
        }
        let on_grid = repeat || near(self.since, self.grid) || near(self.since_grid, self.grid);
        if on_grid || self.grid == 0 {
            self.since_grid = 0;
        }
        (self.gap, self.since) = (self.since, 0);
        !on_grid
    }

    /// A flush, what comes after starts the spacing over on the source's grid, which stays.
    pub(crate) fn reset(&mut self) {
        *self = IntraCadence { grid: self.grid, ..IntraCadence::default() };
    }
}

/// Field order is drop order, the teardown order libva wants.
pub(crate) struct EncEngine {
    /// Coded buffers read and free for the next picture, one a picture in flight at most. A
    /// buffer made and destroyed each picture is a driver allocation each picture.
    coded: Vec<VaBuffer>,
    /// Coded buffers made since the session opened.
    coded_made: u32,
    context: Context,
    _surfaces: Surfaces,
    /// The upload surface, made by the first raw picture. A zero-copy session codes from its
    /// producer's surfaces and never makes it.
    input: Option<Surfaces>,
    _config: Config,
    /// A ring, `recon[cur]` takes the picture coded, the `recon_n - 1` before it are references.
    recon: [ffi::VASurfaceID; REFS_MAX + 1],
    recon_n: usize,
    cur: usize,
    pub coded_w: u32,
    pub coded_h: u32,
    pub width: u32,
    pub height: u32,
    /// Of the input surface, which also sizes the coded buffer.
    format: SurfaceFormat,
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
        Self::new_packed(display, profile, entrypoint, rc_mode, format, width, height, align, 0, 1)
    }

    // Cold, session setup.
    #[allow(clippy::too_many_arguments, clippy::disallowed_methods)]
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
        refs: usize,
    ) -> VaResult<EncEngine> {
        let recon_n = refs.clamp(1, REFS_MAX) + 1;
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
        let surfaces = Surfaces::new(display, format, coded_w, coded_h, recon_n as u32)?;
        let context = Context::new(display, &config, coded_w as i32, coded_h as i32, &surfaces)?;
        let mut recon = [ffi::VA_INVALID_SURFACE; REFS_MAX + 1];
        recon[..recon_n].copy_from_slice(surfaces.ids());
        Ok(EncEngine {
            coded: Vec::with_capacity(DEPTH_MAX),
            coded_made: 0,
            context,
            _surfaces: surfaces,
            input: None,
            _config: config,
            recon,
            recon_n,
            cur: 0,
            coded_w,
            coded_h,
            width,
            height,
            format,
        })
    }

    /// The coded width and height.
    pub(crate) fn coded(&self) -> (u32, u32) {
        (self.coded_w, self.coded_h)
    }

    pub(crate) fn context(&self) -> &Context {
        &self.context
    }

    /// The upload surface, None until a raw picture made it.
    pub(crate) fn input(&self) -> Option<ffi::VASurfaceID> {
        self.input.as_ref().map(|s| s.ids()[0])
    }

    /// Surfaces the session made, its references and the upload surface once made.
    pub(crate) fn surfaces_made(&self) -> u32 {
        (self.recon_n + usize::from(self.input.is_some())) as u32
    }

    // Allocates once a session, at its first raw picture.
    #[allow(clippy::disallowed_methods)]
    fn upload_surface(&mut self, display: &Display) -> VaResult<ffi::VASurfaceID> {
        if let Some(id) = self.input() {
            return Ok(id);
        }
        let s = Surfaces::new(display, self.format, self.coded_w, self.coded_h, 1)?;
        let id = s.ids()[0];
        self.input = Some(s);
        Ok(id)
    }

    pub(crate) fn recon_cur(&self) -> ffi::VASurfaceID {
        self.recon[self.cur]
    }

    pub(crate) fn dpy(&self) -> ffi::VADisplay {
        self.context.display()
    }

    /// Which of [`Self::recon_ids`] the picture coded now writes.
    pub(crate) fn recon_slot(&self) -> usize {
        self.cur
    }

    /// The reconstruction surfaces, [`Self::recon_slot`] indexes them.
    pub(crate) fn recon_ids(&self) -> &[ffi::VASurfaceID] {
        &self.recon[..self.recon_n]
    }

    /// The picture coded last.
    pub(crate) fn recon_ref(&self) -> ffi::VASurfaceID {
        self.recon_back(0)
    }

    /// References kept, the pictures coded before the current one.
    pub(crate) fn refs(&self) -> usize {
        self.recon_n - 1
    }

    /// The picture coded `back + 1` pictures ago, `back` below [`Self::refs`].
    pub(crate) fn recon_back(&self, back: usize) -> ffi::VASurfaceID {
        let n = self.recon_n;
        self.recon[(self.cur + n - 1 - back % (n - 1)) % n]
    }

    pub(crate) fn advance_recon(&mut self) {
        self.cur = (self.cur + 1) % self.recon_n;
    }

    /// The picture in `data` on the upload surface, which it returns. Edges are replicated into
    /// the alignment padding so padded blocks predict cheaply.
    pub(crate) fn upload(
        &mut self,
        display: &Display,
        data: &[u8],
        fmt: RawPixFmt,
    ) -> VaResult<ffi::VASurfaceID> {
        let input = self.upload_surface(display)?;
        if fmt == RawPixFmt::P010 {
            return self.upload_p010(display, data, input).map(|()| input);
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (pad_w, pad_h) = (self.coded_w as usize, self.coded_h as usize);
        let (pad_cw, pad_ch) = (pad_w / 2, pad_h / 2);

        let mut img = UploadImage::acquire(display, input, self.coded_w, self.coded_h)?;

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

        img.finish().map(|()| input)
    }

    fn upload_p010(&self, display: &Display, data: &[u8], input: ffi::VASurfaceID) -> VaResult<()> {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (pad_w, pad_h) = (self.coded_w as usize, self.coded_h as usize);
        let (pad_cw, pad_ch) = (pad_w / 2, pad_h / 2);
        let mut img = UploadImage::acquire_for(
            display,
            input,
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
        self.queue(bufs, input)?;
        va::sync_surface(display, input)
    }

    /// The picture on `input` handed to the GPU, its coded buffer read when it is done.
    pub(crate) fn queue(&self, bufs: &[ffi::VABufferID], input: ffi::VASurfaceID) -> VaResult<()> {
        self.context.begin(input)?;
        let rendered = self.context.render(bufs);
        // vaEndPicture runs after a failed render too, so the context is not left mid-picture.
        rendered.and(self.context.end())
    }

    /// Raw size plus 4 KiB, output beyond the raw input means something is badly wrong.
    pub(crate) fn coded_buf_size(&self) -> u32 {
        self.coded_w * self.coded_h * self.format.bits_per_pixel / 8 + 4096
    }

    /// A coded buffer for the next picture, a free one or a new one. The size is the session's,
    /// a new picture size is a new session.
    pub(crate) fn take_coded(&mut self, display: &Display) -> VaResult<VaBuffer> {
        if let Some(b) = self.coded.pop() {
            return Ok(b);
        }
        let ty = ffi::VAEncCodedBufferType;
        let b = VaBuffer::new_empty(display, &self.context, ty, self.coded_buf_size())?;
        self.coded_made += 1;
        Ok(b)
    }

    /// `b` free again, its picture read and unmapped or never handed to the GPU. Only a buffer
    /// [`Self::take_coded`] gave out comes back, the encoders read every picture before a new
    /// session.
    pub(crate) fn put_coded(&mut self, b: VaBuffer) {
        #[cfg(test)]
        if FRESH_CODED.get() {
            return;
        }
        if self.coded.len() < DEPTH_MAX {
            self.coded.push(b);
        }
    }

    pub(crate) fn coded_made(&self) -> u32 {
        self.coded_made
    }
}

/// A picture in flight read and dropped, a flush or a stop, its buffer back to `engine`. iHD
/// fails every picture after a coded buffer destroyed unread. A failed submit goes through
/// here too, the map waits out whatever the driver queued before it failed.
pub(crate) fn discard_coded(engine: Option<&mut EncEngine>, b: VaBuffer) {
    let read = map_coded(&b).and_then(CodedMap::unmap);
    if let (Ok(()), Some(e)) = (read, engine) {
        e.put_coded(b);
    }
}

#[cfg(test)]
thread_local! {
    /// Pictures queued on a reconstruction surface already exported, those whose export is of
    /// the surface's storage and those whose is not.
    pub(crate) static WATCHED: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
    /// Every picture through a new coded buffer, the path before the reuse, for a test to
    /// compare against. The harness drives an element on the test's thread.
    pub(crate) static FRESH_CODED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
// Test fixtures, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use profluens_core::error::Error;
    use profluens_core::time::Timestamp;

    use profluens_core::io::{Handle, IoError, IoResult};

    use std::time::Duration;

    use super::{
        Ask, End, Fence, Fences, How, IntraCadence, Outlet, Reads, STAGED_MAX, Slack, Staged,
        Stamp, USER_READY, Watch, adopt_tag, align_up,
    };

    fn adopted(handle: Handle) -> IoResult {
        IoResult::File { handle, len: 0 }
    }

    #[test]
    fn a_surface_is_exported_by_its_first_picture_and_fenced_once_adopted() {
        let mut f = Fences::new();
        f.reset(3);
        assert_eq!(f.ask(1), Ask::Export, "no picture wrote it yet");
        f.watch[1] = Watch::Adopting;
        assert_eq!(f.ask(1), Ask::Poll, "the adopt pends");
        let h = Handle::new(7, 1);
        assert_eq!(f.note(adopt_tag(f.epoch, 1), adopted(h)), None);
        assert_eq!(f.ask(1), Ask::Ready(h));
        assert_eq!(f.ask(0), Ask::Export, "the others wait for their own picture");
        assert_eq!(f.ask(3), Ask::Poll, "no such surface");
    }

    #[test]
    fn an_adopt_of_a_closed_session_arms_nothing_and_is_closed() {
        let mut f = Fences::new();
        f.reset(2);
        (f.watch[0], f.watch[1]) = (Watch::Adopting, Watch::Adopted(Handle::new(1, 1)));
        let old = adopt_tag(f.epoch, 0);
        f.retire();
        assert_eq!(f.stale, [Handle::new(1, 1)], "the adopted handle is closed");
        assert_eq!(f.parked.len(), 1, "the fd of the pending adopt stays open");
        f.reset(2);
        f.watch[0] = Watch::Adopting;
        let late = Handle::new(2, 1);
        assert_eq!(f.note(old, adopted(late)), None);
        assert_eq!(f.watch[0], Watch::Adopting, "the new session's adopt still pends");
        assert!(f.parked.is_empty(), "its fd goes with the op");
        assert_eq!(f.stale, [Handle::new(1, 1), late]);
        let ours = Handle::new(3, 1);
        f.note(adopt_tag(f.epoch, 0), adopted(ours));
        assert_eq!(f.ask(0), Ask::Ready(ours));
    }

    #[test]
    fn a_failed_adopt_of_a_closed_session_frees_its_fd() {
        let mut f = Fences::new();
        f.reset(1);
        f.watch[0] = Watch::Adopting;
        let old = adopt_tag(f.epoch, 0);
        f.retire();
        f.note(old, IoResult::Err(IoError::Full));
        assert!(f.parked.is_empty() && f.stale.is_empty());
        assert!(!f.off, "a full reactor is no refusal");
    }

    #[test]
    fn a_reactor_that_refuses_turns_the_fences_off() {
        let mut f = Fences::new();
        f.reset(1);
        f.watch[0] = Watch::Adopting;
        f.note(adopt_tag(f.epoch, 0), IoResult::Err(IoError::NoBackend));
        assert_eq!(f.ask(0), Ask::Poll);
        f.watch[0] = Watch::Unused;
        assert_eq!(f.ask(0), Ask::Poll, "nothing more is exported");
    }

    #[test]
    fn the_slack_is_raised_for_the_map_and_put_back() {
        // SAFETY: reads a number of the calling thread.
        let slack = || unsafe { libc::prctl(libc::PR_GET_TIMERSLACK) } as libc::c_ulong;
        let was = slack();
        {
            let _raised = Slack::raise(700_000);
            assert_eq!(slack(), 700_000u64.max(was));
            let _lower = Slack::raise(1);
            assert_eq!(slack(), 700_000u64.max(was), "never lowered");
        }
        assert_eq!(slack(), was);
    }

    #[test]
    fn reads_count_by_fence_and_map_time_and_tell_every_ten_seconds() {
        let t0 = std::time::Instant::now();
        let mut r = Reads::default();
        assert!(!r.due(t0, true), "no picture, nothing to tell");
        r.mapped(Fence::Signalled, How::Poll, t0);
        r.mapped(Fence::Pending, How::Poll, t0 - Duration::from_micros(500));
        r.mapped(Fence::Polled, How::Wait, t0 - Duration::from_millis(3));
        assert_eq!((r.by_fence, r.waited), ([1, 1, 1], 1));
        assert_eq!(r.map, [1, 1, 1]);
        assert!(r.map_max_us >= 3000);
        assert!(!r.due(t0, false), "the first look starts the clock");
        assert!(!r.due(t0 + Duration::from_secs(9), false));
        assert!(r.due(t0 + Duration::from_secs(10), false));
        assert!(!r.due(t0 + Duration::from_secs(11), false));
        assert!(r.due(t0 + Duration::from_secs(11), true), "a close tells at once");
    }

    #[test]
    fn a_fence_completion_names_its_picture() {
        let mut f = Fences::new();
        assert_eq!(f.note(USER_READY | 41, IoResult::Ok(0)), Some((41, true)));
        assert_eq!(f.note(USER_READY | 42, IoResult::Err(IoError::BadHandle)), Some((42, false)));
        assert_eq!(f.note(9, IoResult::Ok(0)), None, "a close's completion");
    }

    /// The cut flags of intra pictures `gaps` pictures apart, the first after `gaps[0]`.
    fn cuts(gaps: &[u32]) -> Vec<bool> {
        let mut c = IntraCadence::default();
        let mut out = Vec::new();
        for &g in gaps {
            for _ in 1..g {
                assert!(!c.note(false), "a picture that is not intra is no cut");
            }
            out.push(c.note(true));
        }
        out
    }

    #[test]
    fn a_keyframe_grid_is_no_scene_cut_and_what_breaks_it_is() {
        // novocaine.mkv from 600 s, a 21 picture grid with cuts that keep its phase
        let gaps = [21, 21, 21, 21, 3, 19, 21, 21, 8, 19, 21, 21, 12, 9, 21];
        let want = [
            true, false, false, false, true, false, false, false, true, false, false, false, true,
            false, false,
        ];
        assert_eq!(cuts(&gaps), want);
        // sdr1080.mkv, cuts only, two alike by chance make a grid the next cut is off
        assert_eq!(cuts(&[113, 72, 149, 93, 93, 40]), [true, true, true, true, false, true]);
        let mut c = IntraCadence::default();
        for _ in 0..2 {
            (1..21).for_each(|_| assert!(!c.note(false)));
            c.note(true);
        }
        (1..21).for_each(|_| assert!(!c.note(false)));
        c.reset();
        assert!(!c.note(true), "after a flush an intra picture starts the spacing");
    }

    /// A seek lands on a keyframe, the first picture after the flush is intra. It starts the
    /// spacing and the grid learnt before the flush holds.
    #[test]
    fn a_seek_onto_a_keyframe_keeps_the_grid() {
        let mut c = IntraCadence::default();
        let run = |c: &mut IntraCadence, n: u32| -> Vec<bool> {
            let mut out = Vec::new();
            for _ in 0..n {
                out.push(c.note(true));
                (1..48).for_each(|_| assert!(!c.note(false)));
            }
            out
        };
        // from the start, the second keyframe gives the first gap, the third confirms it
        assert_eq!(run(&mut c, 4), [false, true, false, false]);
        for _ in 0..3 {
            c.reset();
            assert_eq!(run(&mut c, 3), [false, false, false], "no cut after a seek");
        }
        c.reset();
        assert!(!c.note(true));
        (1..20).for_each(|_| assert!(!c.note(false)));
        assert!(c.note(true), "off the grid after a seek is a cut");
    }

    /// A pool of `free` slots. Sent pts land in `out`, `holds` counts the passes the end was held.
    struct FakePool {
        free: u32,
        out: Vec<Timestamp>,
        holds: u32,
    }

    impl Outlet for FakePool {
        fn send(
            &mut self,
            _len: usize,
            _fill: &dyn Fn(&mut [u8]),
            st: Stamp,
        ) -> Result<bool, Error> {
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
