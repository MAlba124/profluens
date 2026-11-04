//! The `video/gpu` frame descriptor a zero-copy decoder emits in place of pixels.
//! The buffer bytes carry a token and geometry, the dmabuf fds go out of band through
//! [`GpuFrameChannel`]. Dropping a [`GpuFrame`] closes its fds and releases its surface through its
//! [`Lease`].

use std::ops::Deref;
use std::os::fd::BorrowedFd;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::ctx::Ctx;
use profluens_core::format::{
    ConstraintSpec, FieldSpec, FixedFormat, MAX_SET_VALUES, OfferSpec, Value, ValueSpec,
};
use profluens_core::memory::Memory;
use profluens_core::ring::{Consumer, Producer, spsc};

use crate::fixed::FixedList;

/// drm_fourcc.h `DRM_FORMAT_NV12`, Y then interleaved CbCr. Same as `VA_FOURCC_NV12`.
pub const DRM_FORMAT_NV12: u32 = 0x3231_564E;
/// drm_fourcc.h `DRM_FORMAT_P010`, 16-bit words with the ten significant bits high.
pub const DRM_FORMAT_P010: u32 = 0x3031_3050;
pub const DRM_FORMAT_YUV420: u32 = 0x3231_5559;
/// drm_fourcc.h `DRM_FORMAT_MOD_INVALID`. An importer then leaves the modifier attributes out.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// drm_fourcc.h `DRM_FORMAT_MOD_LINEAR`, the one layout a CPU writes.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportedPlane {
    pub object_index: u32,
    pub offset: u32,
    pub pitch: u32,
}

/// Leads the `video/gpu` payload so a mis-wired software sink fails loudly.
pub const GPU_FRAME_MAGIC: u32 = 0x5343_4750; // "SCGP"
pub const GPU_FRAME_VERSION: u32 = 1;

/// Distinct from `video/raw` so it only links to a consumer that opts in.
pub const FAMILY_VIDEO_GPU: &str = "video/gpu";
pub const F_LAYOUT: &str = "layout";
pub const LAYOUT_NV12_DMABUF: &str = "nv12-dmabuf";
pub const LAYOUT_P010_DMABUF: &str = "p010-dmabuf";
pub const LAYOUT_I420_DMABUF: &str = "i420-dmabuf";
pub const F_WIDTH: &str = "width";
pub const F_HEIGHT: &str = "height";

/// The DRM modifier of every frame on a `video/gpu` link, `DRM_FORMAT_MOD_INVALID` for none.
pub const F_MODIFIER: &str = "modifier";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifiers: Vec<u64>,
}

pub fn layout_of(fourcc: u32) -> Option<&'static str> {
    match fourcc {
        DRM_FORMAT_NV12 => Some(LAYOUT_NV12_DMABUF),
        DRM_FORMAT_P010 => Some(LAYOUT_P010_DMABUF),
        DRM_FORMAT_YUV420 => Some(LAYOUT_I420_DMABUF),
        _ => None,
    }
}

impl DmabufFormat {
    /// Group `(fourcc, modifier)` pairs by carried layout. Also returns how many fell past
    /// [`MAX_SET_VALUES`].
    // Once per device probe, at preroll.
    #[allow(clippy::disallowed_methods)]
    pub fn group(pairs: impl IntoIterator<Item = (u32, u64)>) -> (Vec<DmabufFormat>, usize) {
        let (mut formats, mut dropped) = (Vec::<DmabufFormat>::new(), 0);
        for (fourcc, modifier) in pairs {
            if layout_of(fourcc).is_none() {
                continue;
            }
            let at = formats.iter().position(|f| f.fourcc == fourcc).unwrap_or(formats.len());
            if at == formats.len() {
                formats.push(DmabufFormat { fourcc, modifiers: Vec::new() });
            }
            let modifiers = &mut formats[at].modifiers;
            if modifiers.contains(&modifier) {
                continue;
            }
            match modifiers.len() < MAX_SET_VALUES {
                true => modifiers.push(modifier),
                false => dropped += 1,
            }
        }
        (formats, dropped)
    }
}

/// Hand `f` one `video/gpu` offer per layout, with the `video/raw` colour fields so colour resolves
/// here too.
// Once per probed pad, at preroll.
#[allow(clippy::disallowed_methods)]
pub fn with_gpu_offers<R>(formats: &[DmabufFormat], f: impl FnOnce(&[OfferSpec<'_>]) -> R) -> R {
    let carried: Vec<(&str, &DmabufFormat)> =
        formats.iter().filter_map(|d| Some((layout_of(d.fourcc)?, d))).collect();
    let sets: Vec<Vec<ValueSpec<'_>>> = carried
        .iter()
        .map(|(_, d)| d.modifiers.iter().map(|&m| ValueSpec::Int(m as i64)).collect())
        .collect();
    let any = |field| FieldSpec { field, allowed: ConstraintSpec::Any, preferred: None };
    let c = crate::format::COLOR_OFFER_FIELDS;
    let fields: Vec<[FieldSpec<'_>; 14]> = carried
        .iter()
        .zip(&sets)
        .map(|((layout, _), set)| {
            let layout = ConstraintSpec::Eq(ValueSpec::Id(layout));
            [
                any(F_WIDTH),
                any(F_HEIGHT),
                FieldSpec { field: F_LAYOUT, allowed: layout, preferred: None },
                FieldSpec { field: F_MODIFIER, allowed: ConstraintSpec::Set(set), preferred: None },
                c[0],
                c[1],
                c[2],
                c[3],
                c[4],
                c[5],
                c[6],
                c[7],
                c[8],
                c[9],
            ]
        })
        .collect();
    let offers: Vec<OfferSpec<'_>> =
        fields.iter().map(|f| OfferSpec { family: FAMILY_VIDEO_GPU, fields: f }).collect();
    f(&offers)
}

pub fn link_modifier(ctx: &Ctx, format: Option<&FixedFormat>) -> Option<u64> {
    let format = format.filter(|f| ctx.family_name(f.family) == Some(FAMILY_VIDEO_GPU))?;
    match format.get(ctx.field_id(F_MODIFIER)?)? {
        Value::Int(bits) => Some(bits as u64),
        _ => None,
    }
}

pub fn link_layout(ctx: &Ctx, format: Option<&FixedFormat>) -> Option<(u32, u64)> {
    let modifier = link_modifier(ctx, format)?;
    let format = format?;
    let Value::Id(layout) = format.get(ctx.field_id(F_LAYOUT)?)? else {
        return None;
    };
    let fourcc = match ctx.value_name(layout)? {
        LAYOUT_NV12_DMABUF => DRM_FORMAT_NV12,
        LAYOUT_P010_DMABUF => DRM_FORMAT_P010,
        LAYOUT_I420_DMABUF => DRM_FORMAT_YUV420,
        _ => return None,
    };
    Some((fourcc, modifier))
}

pub type FrameToken = u64;

/// drm_fourcc.h `DRM_FORMAT_MAX_PLANES`. A frame has at most one fd per plane.
pub const MAX_PLANES: usize = 4;

pub type Planes = FixedList<ExportedPlane, MAX_PLANES>;

/// Up to [`MAX_PLANES`] owned dmabuf fds held inline. Each closes once, on [`DmabufFds::close`]
/// or drop. Not `Clone`, a copy is [`DmabufFds::try_clone`], which dups.
#[derive(Default)]
pub struct DmabufFds {
    fds: FixedList<RawFd, MAX_PLANES>,
}

impl DmabufFds {
    /// Take ownership of `fd`. `false` when full, `fd` is then closed.
    ///
    /// # Safety
    /// `fd` is a live fd the caller owns and does not use or close after.
    pub unsafe fn push(&mut self, fd: RawFd) -> bool {
        if self.fds.push(fd) {
            return true;
        }
        // SAFETY: owned per the contract and refused, so closed here once.
        unsafe { close_raw_fd(fd) };
        false
    }

    pub fn fd(&self, object: u32) -> Option<BorrowedFd<'_>> {
        let fd = *self.fds.get(object as usize)?;
        // SAFETY: fds close only in `close`, which takes `&mut self`.
        (fd >= 0).then(|| unsafe { BorrowedFd::borrow_raw(fd) })
    }

    /// A close-on-exec dup of each fd. A failed dup closes the dups made and returns its errno.
    pub fn try_clone(&self) -> Result<DmabufFds, i32> {
        let mut out = DmabufFds::default();
        for &fd in self.fds.iter() {
            // SAFETY: F_DUPFD_CLOEXEC reads no memory, a bad fd fails with EBADF.
            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                // `out` drops here and closes the dups made so far.
                return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
            }
            // Same capacity as `self`, so it fits.
            out.fds.push(dup);
        }
        Ok(out)
    }

    /// Close every fd and empty the list. A second call closes nothing.
    pub fn close(&mut self) {
        for &fd in self.fds.iter().filter(|&&fd| fd >= 0) {
            // SAFETY: owned here, and the clear below keeps it from closing twice.
            unsafe { close_raw_fd(fd) };
        }
        self.fds.clear();
    }
}

impl Drop for DmabufFds {
    fn drop(&mut self) {
        self.close();
    }
}

impl Deref for DmabufFds {
    type Target = [RawFd];

    fn deref(&self) -> &[RawFd] {
        &self.fds
    }
}

impl std::fmt::Debug for DmabufFds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fds.fmt(f)
    }
}

/// The dup'd DMA-BUF fds and geometry of a frame. Dropping it closes the fds and returns the
/// surface.
pub struct GpuFrame {
    pub token: FrameToken,
    pub fds: DmabufFds,
    /// The modifier is fixed on the link ([`link_modifier`]), not per frame.
    pub drm_format: u32,
    /// Coded size, the plane geometry is at this size.
    pub coded_w: u32,
    pub coded_h: u32,
    pub disp_w: u32,
    pub disp_h: u32,
    pub crop_x: u32,
    pub crop_y: u32,
    pub planes: Planes,
    pub epoch: u64,
    pub lease: Option<Lease>,
}

/// Frames of one decoder out at once, a bit each in one word.
pub const FRAMES_OUT: usize = 64;

/// Dropping the lease is the release. It sets the slot's bit and returns the pool slot, waking a
/// parked decoder.
pub struct Lease {
    slot: u8,
    back: Arc<AtomicU64>,
    _slot_of_pool: Memory,
}

impl Drop for Lease {
    fn drop(&mut self) {
        // The bit first. The pool slot follows when the fields drop.
        self.back.fetch_or(1 << self.slot, Ordering::Release);
    }
}

impl GpuFrame {
    pub fn fd(&self, object: u32) -> Option<BorrowedFd<'_>> {
        self.fds.fd(object)
    }

    pub fn close_fds(&mut self) {
        self.fds.close();
    }
}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        // The fds close before the lease returns the surface.
        self.close_fds();
    }
}

/// Close a raw fd on an error path, ignoring the result.
///
/// # Safety
/// `fd` must be a live, owned file descriptor closed exactly once.
pub unsafe fn close_raw_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

/// The decoder's end of a [`GpuFrameChannel`]. Each frame takes one of [`FRAMES_OUT`] bits, its
/// [`Lease`] sets it on return.
pub struct FrameTx {
    frames: Producer<GpuFrame>,
    /// A reset takes a new word, so a lease from before it sets a bit nobody reads.
    back: Arc<AtomicU64>,
    out: u64,
    tokens: [FrameToken; FRAMES_OUT],
}

impl FrameTx {
    /// Hand `frame` to the sink, leased with `slot_of_pool`. `false` and dropped when
    /// [`FRAMES_OUT`] are out or the ring is full.
    pub fn push(&mut self, mut frame: GpuFrame, slot_of_pool: Memory) -> bool {
        let slot = (!self.out).trailing_zeros() as usize;
        if slot >= FRAMES_OUT {
            return false;
        }
        frame.lease = Some(Lease {
            slot: slot as u8,
            back: Arc::clone(&self.back),
            _slot_of_pool: slot_of_pool,
        });
        let token = frame.token;
        if let Err(refused) = self.frames.try_push(frame) {
            // The refused lease set a bit of no frame out, so take it back.
            drop(refused);
            self.back.fetch_and(!(1 << slot), Ordering::AcqRel);
            return false;
        }
        (self.out, self.tokens[slot]) = (self.out | 1 << slot, token);
        true
    }

    pub fn drain_released(&mut self, tokens: &mut Vec<FrameToken>) {
        if self.back.load(Ordering::Acquire) == 0 {
            return;
        }
        let back = self.back.swap(0, Ordering::AcqRel) & self.out;
        self.out &= !back;
        tokens.extend(
            (0..FRAMES_OUT).filter(|slot| back >> slot & 1 == 1).map(|slot| self.tokens[slot]),
        );
    }

    /// Forget every frame that is out. Old leases still return their pool slots.
    // One word a reset.
    #[allow(clippy::disallowed_methods)]
    pub fn reset(&mut self) {
        (self.back, self.out) = (Arc::new(AtomicU64::new(0)), 0);
    }
}

pub struct FrameRx {
    frames: Consumer<GpuFrame>,
    ahead: Option<GpuFrame>,
}

impl FrameRx {
    fn next(&mut self) -> Option<GpuFrame> {
        self.ahead.take().or_else(|| self.frames.try_pop())
    }

    /// The frame of `token`, if handed over. Lower tokens met on the way lost their buffer and are
    /// dropped.
    pub fn take(&mut self, token: FrameToken) -> Option<GpuFrame> {
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

    /// Drop frames from before seek `epoch`, freeing their pool slots at once.
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

/// When a producer hands a frame on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    /// Once the GPU wrote it, the producer waited for the picture.
    Done,
    /// As soon as the picture is queued. The dma-buf carries the write's fence, so a consumer
    /// on the same GPU reads behind it and never waits for the CPU. A CPU reader calls
    /// [`wait_readable`] first.
    Queued,
}

/// Waits until every writer of `fds` is done, `poll` on each dma-buf. False on a timeout, an
/// error as errno. `timeout_ms` below zero waits for good.
pub fn wait_readable(fds: &DmabufFds, timeout_ms: i32) -> Result<bool, i32> {
    for &fd in fds.iter() {
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd of a live fd the frame owns, the call writes only revents.
        let n = unsafe { libc::poll(&mut p, 1, timeout_ms) };
        if n < 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        if n == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Hands [`GpuFrame`]s from a zero-copy decoder to its sink over an SPSC ring.
/// Nothing is sent back, dropping a frame releases its surface, so a surface is never reused while
/// held. Ends are taken at `start()` and put back at `stop()`, the only lock. With both back the
/// ring is dropped.
pub struct GpuFrameChannel {
    ends: Mutex<(Option<FrameTx>, Option<FrameRx>)>,
    handoff: Handoff,
}

impl GpuFrameChannel {
    /// A channel whose frames are handed on once written, [`Handoff::Done`].
    pub fn new() -> Arc<Self> {
        Self::with_handoff(Handoff::Done)
    }

    /// A channel whose frames are handed on as queued, [`Handoff::Queued`], for a consumer on
    /// the same GPU.
    pub fn queued() -> Arc<Self> {
        Self::with_handoff(Handoff::Queued)
    }

    // Cold, one channel per zero-copy pipeline.
    #[allow(clippy::disallowed_methods)]
    pub fn with_handoff(handoff: Handoff) -> Arc<Self> {
        let (frames, taken) = spsc(FRAMES_OUT);
        let tx =
            FrameTx { frames, back: Arc::new(AtomicU64::new(0)), out: 0, tokens: [0; FRAMES_OUT] };
        Arc::new(Self {
            ends: Mutex::new((Some(tx), Some(FrameRx { frames: taken, ahead: None }))),
            handoff,
        })
    }

    /// When the producer hands a frame on.
    pub fn handoff(&self) -> Handoff {
        self.handoff
    }

    fn ends(&self) -> std::sync::MutexGuard<'_, (Option<FrameTx>, Option<FrameRx>)> {
        self.ends.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn take_tx(&self) -> Option<FrameTx> {
        self.ends().0.take()
    }

    pub fn take_rx(&self) -> Option<FrameRx> {
        self.ends().1.take()
    }

    pub fn put_tx(&self, mut tx: FrameTx) {
        tx.reset();
        let mut ends = self.ends();
        ends.0 = Some(tx);
        Self::settle(&mut ends);
    }

    pub fn put_rx(&self, rx: FrameRx) {
        let mut ends = self.ends();
        ends.1 = Some(rx);
        Self::settle(&mut ends);
    }

    fn settle(ends: &mut (Option<FrameTx>, Option<FrameRx>)) {
        if let (Some(_), Some(rx)) = (&ends.0, &mut ends.1) {
            rx.clear();
        }
    }
}

/// The 40 byte little-endian `video/gpu` payload, token and display geometry. No fds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuFrameHeader {
    pub token: FrameToken,
    pub coded_w: u32,
    pub coded_h: u32,
    pub disp_w: u32,
    pub disp_h: u32,
    pub crop_x: u32,
    pub crop_y: u32,
}

pub const GPU_FRAME_HEADER_LEN: usize = 4 + 4 + 8 + 6 * 4;

impl GpuFrameHeader {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; GPU_FRAME_HEADER_LEN];
        self.write_to(&mut b);
        b
    }

    pub fn write_to(&self, out: &mut [u8]) {
        let words =
            [self.coded_w, self.coded_h, self.disp_w, self.disp_h, self.crop_x, self.crop_y];
        out[0..4].copy_from_slice(&GPU_FRAME_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&GPU_FRAME_VERSION.to_le_bytes());
        out[8..16].copy_from_slice(&self.token.to_le_bytes());
        for (i, w) in words.iter().enumerate() {
            out[16 + 4 * i..20 + 4 * i].copy_from_slice(&w.to_le_bytes());
        }
    }

    pub fn from_bytes(b: &[u8]) -> Option<GpuFrameHeader> {
        if b.len() < GPU_FRAME_HEADER_LEN {
            return None;
        }
        let rd_u32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let rd_u64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        if rd_u32(0) != GPU_FRAME_MAGIC || rd_u32(4) != GPU_FRAME_VERSION {
            return None;
        }
        Some(GpuFrameHeader {
            token: rd_u64(8),
            coded_w: rd_u32(16),
            coded_h: rd_u32(20),
            disp_w: rd_u32(24),
            disp_h: rd_u32(28),
            crop_x: rd_u32(32),
            crop_y: rd_u32(36),
        })
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = GpuFrameHeader {
            token: 0xDEAD_BEEF_1234_5678,
            coded_w: 1920,
            coded_h: 1088,
            disp_w: 1920,
            disp_h: 1080,
            crop_x: 0,
            crop_y: 0,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), GPU_FRAME_HEADER_LEN);
        assert_eq!(GpuFrameHeader::from_bytes(&bytes), Some(h));
    }

    #[test]
    fn header_rejects_bad_magic_and_short() {
        let mut bytes = GpuFrameHeader {
            token: 1,
            coded_w: 4,
            coded_h: 4,
            disp_w: 4,
            disp_h: 4,
            crop_x: 0,
            crop_y: 0,
        }
        .to_bytes();
        assert!(GpuFrameHeader::from_bytes(&bytes[..GPU_FRAME_HEADER_LEN - 1]).is_none());
        bytes[0] ^= 0xFF; // corrupt the magic
        assert!(GpuFrameHeader::from_bytes(&bytes).is_none());
    }

    fn frame(token: FrameToken, epoch: u64) -> GpuFrame {
        GpuFrame {
            token,
            fds: DmabufFds::default(),
            drm_format: DRM_FORMAT_NV12,
            coded_w: 16,
            coded_h: 16,
            disp_w: 16,
            disp_h: 16,
            crop_x: 0,
            crop_y: 0,
            planes: Planes::default(),
            epoch,
            lease: None,
        }
    }

    fn released(tx: &mut FrameTx) -> Vec<FrameToken> {
        let mut tokens = Vec::new();
        tx.drain_released(&mut tokens);
        tokens
    }

    #[test]
    fn a_frame_is_taken_once_by_its_token_and_dropping_it_is_the_release() {
        let pool = profluens_core::memory::Pool::bounded(64, 4);
        let ch = GpuFrameChannel::new();
        let (mut tx, mut rx) =
            (ch.take_tx().expect("the decoder's end"), ch.take_rx().expect("the sink's end"));
        assert!(
            ch.take_tx().is_none() && ch.take_rx().is_none(),
            "an end is one element's for the run"
        );
        assert!(rx.take(7).is_none());
        assert!(tx.push(frame(7, 0), pool.try_acquire().expect("a slot")));
        assert!(tx.push(frame(8, 0), pool.try_acquire().expect("a slot")));
        assert_eq!(pool.in_use().0, 2, "each frame holds the slot it was leased with");
        let seven = rx.take(7).expect("token 7");
        assert!(rx.take(7).is_none(), "taken once");
        assert!(released(&mut tx).is_empty(), "held, so not released");
        drop(seven);
        assert_eq!((released(&mut tx), pool.in_use().0), (vec![7], 1), "the drop is the release");
        assert!(released(&mut tx).is_empty());
        drop(rx.take(8));
        assert_eq!((released(&mut tx), pool.in_use().0), (vec![8], 0));
    }

    #[test]
    fn a_frame_whose_buffer_never_came_is_dropped_on_the_way_to_a_later_one() {
        let pool = profluens_core::memory::Pool::bounded(64, 8);
        let ch = GpuFrameChannel::new();
        let (mut tx, mut rx) = (ch.take_tx().unwrap(), ch.take_rx().unwrap());
        for token in 1..=4 {
            assert!(tx.push(frame(token, 0), pool.try_acquire().unwrap()));
        }
        let three = rx.take(3).expect("token 3");
        assert_eq!(released(&mut tx), [1, 2]);
        assert!(rx.take(2).is_none());
        assert_eq!(rx.take(4).map(|f| f.token), Some(4));
        drop(three);
        assert_eq!((released(&mut tx), pool.in_use().0), (vec![3, 4], 0));
    }

    #[test]
    fn a_flush_drops_the_frames_from_before_the_seek_and_keeps_the_ones_behind_it() {
        let pool = profluens_core::memory::Pool::bounded(64, 8);
        let ch = GpuFrameChannel::new();
        let (mut tx, mut rx) = (ch.take_tx().unwrap(), ch.take_rx().unwrap());
        assert!(tx.push(frame(1, 0), pool.try_acquire().unwrap()));
        assert!(tx.push(frame(2, 0), pool.try_acquire().unwrap()));
        tx.reset();
        assert!(tx.push(frame(3, 1), pool.try_acquire().unwrap()));
        rx.flush_before(1);
        assert_eq!(pool.in_use().0, 1, "the pool slots of the two frames before the seek are back");
        assert!(released(&mut tx).is_empty(), "and their bits were of the stretch before");
        drop(rx.take(3).expect("the frame behind the seek"));
        assert_eq!((released(&mut tx), pool.in_use().0), (vec![3], 0));
    }

    #[test]
    fn no_more_frames_go_out_than_there_are_bits_and_a_stopped_run_holds_nothing() {
        let pool = profluens_core::memory::Pool::bounded(64, FRAMES_OUT as u32 + 1);
        let ch = GpuFrameChannel::new();
        let (mut tx, rx) = (ch.take_tx().unwrap(), ch.take_rx().unwrap());
        for token in 0..FRAMES_OUT as u64 {
            assert!(tx.push(frame(token, 0), pool.try_acquire().unwrap()));
        }
        assert!(!tx.push(frame(99, 0), pool.try_acquire().unwrap()), "every bit is out");
        assert_eq!(pool.in_use().0, FRAMES_OUT as u64, "the refused frame took no slot with it");
        ch.put_rx(rx);
        assert_eq!(pool.in_use().0, FRAMES_OUT as u64);
        ch.put_tx(tx);
        assert_eq!(pool.in_use().0, 0);
        let mut tx = ch.take_tx().expect("for the next run");
        assert!(released(&mut tx).is_empty(), "the stopped run's frames are forgotten");
        assert!(tx.push(frame(100, 0), pool.try_acquire().unwrap()));
        assert_eq!(ch.take_rx().expect("for the next run").take(100).map(|f| f.token), Some(100));
    }

    const NV12: u32 = DRM_FORMAT_NV12;
    const P010: u32 = DRM_FORMAT_P010;
    const XR24: u32 = 0x3432_5258;

    #[test]
    fn pairs_group_by_layout_in_the_order_seen_without_repeats() {
        let invalid = DRM_FORMAT_MOD_INVALID;
        let pairs =
            [(XR24, 0), (NV12, 9), (XR24, 1), (NV12, invalid), (P010, 9), (NV12, 9), (NV12, 2)];
        let (formats, dropped) = DmabufFormat::group(pairs);
        assert_eq!(
            formats,
            [
                DmabufFormat { fourcc: NV12, modifiers: vec![9, invalid, 2] },
                DmabufFormat { fourcc: P010, modifiers: vec![9] },
            ]
        );
        assert_eq!(dropped, 0);
    }

    #[test]
    fn the_three_carried_layouts_have_their_names_and_the_rest_none() {
        assert_eq!(layout_of(NV12), Some(LAYOUT_NV12_DMABUF));
        assert_eq!(layout_of(P010), Some(LAYOUT_P010_DMABUF));
        assert_eq!(layout_of(DRM_FORMAT_YUV420), Some(LAYOUT_I420_DMABUF));
        assert_eq!(layout_of(XR24), None);
        with_gpu_offers(
            &[
                DmabufFormat { fourcc: P010, modifiers: vec![3] },
                DmabufFormat { fourcc: NV12, modifiers: vec![3] },
            ],
            |offers| {
                let layouts: Vec<_> = offers
                    .iter()
                    .map(|o| match o.fields[2].allowed {
                        ConstraintSpec::Eq(ValueSpec::Id(name)) => name,
                        _ => panic!("a layout name"),
                    })
                    .collect();
                assert_eq!(
                    layouts,
                    [LAYOUT_P010_DMABUF, LAYOUT_NV12_DMABUF],
                    "one offer a layout, in order"
                );
            },
        );
    }

    #[test]
    fn modifiers_past_the_set_limit_are_counted_not_offered() {
        let pairs = (0..MAX_SET_VALUES as u64 + 3).map(|m| (NV12, m));
        let (formats, dropped) = DmabufFormat::group(pairs);
        assert_eq!(formats[0].modifiers.len(), MAX_SET_VALUES);
        assert_eq!(formats[0].modifiers[MAX_SET_VALUES - 1], MAX_SET_VALUES as u64 - 1);
        assert_eq!(dropped, 3);
    }

    #[test]
    fn a_layout_offers_its_own_modifiers_by_bit_pattern() {
        let vendor = 0x8100_0000_0000_0002_u64;
        let formats = [
            DmabufFormat { fourcc: XR24, modifiers: vec![0] },
            DmabufFormat { fourcc: NV12, modifiers: vec![vendor, 1] },
        ];
        with_gpu_offers(&formats, |offers| {
            assert_eq!(offers.len(), 1, "only the layout the path carries");
            let (o, f) = (&offers[0], offers[0].fields);
            assert_eq!(o.family, FAMILY_VIDEO_GPU);
            let names: Vec<&str> = f.iter().map(|f| f.field).collect();
            assert_eq!(names[..4], [F_WIDTH, F_HEIGHT, F_LAYOUT, F_MODIFIER]);
            assert_eq!(names[4..], crate::color::FIELDS, "the colour rides the zero-copy link too");
            assert!(matches!(f[2].allowed, ConstraintSpec::Eq(ValueSpec::Id(LAYOUT_NV12_DMABUF))));
            let ConstraintSpec::Set(mods) = f[3].allowed else { panic!("a set") };
            let bits: Vec<u64> = mods
                .iter()
                .map(|v| match v {
                    ValueSpec::Int(i) => *i as u64,
                    other => panic!("an int, got {other:?}"),
                })
                .collect();
            assert_eq!(bits, [vendor, 1], "lossless and in order");
        });
        with_gpu_offers(&[], |offers| assert!(offers.is_empty()));
    }

    /// A nonblocking pipe. The write end is the fd under test, the read end hits EOF once every
    /// copy of the write end is closed.
    fn pipe() -> (RawFd, RawFd) {
        let mut p = [0; 2];
        // SAFETY: two ints out.
        assert_eq!(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) }, 0);
        (p[0], p[1])
    }

    /// Whether every write end of the pipe whose read end is `r` is closed.
    fn closed(r: RawFd) -> bool {
        let mut b = 0u8;
        // SAFETY: one byte into a local.
        let n = unsafe { libc::read(r, (&raw mut b).cast(), 1) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        match n {
            0 => true,
            -1 if errno == Some(libc::EAGAIN) => false,
            other => panic!("read {other}, errno {errno:?}"),
        }
    }

    /// Put the write end of `park` on the number `freed` had. A stray second close of `freed`
    /// then shows as EOF on `park`'s read end. nextest runs each test in its own process, so no
    /// other thread takes the number in between.
    fn park(freed: RawFd, (_, w): (RawFd, RawFd)) {
        // SAFETY: plain fd calls on numbers this test owns.
        unsafe {
            assert_eq!(libc::dup2(w, freed), freed);
            libc::close(w);
        }
    }

    fn close_all(fds: &[RawFd]) {
        for &fd in fds {
            // SAFETY: the test's own fds, closed once.
            unsafe { libc::close(fd) };
        }
    }

    fn owning(ws: &[RawFd]) -> DmabufFds {
        let mut fds = DmabufFds::default();
        for &w in ws {
            // SAFETY: a pipe end this test made and hands over.
            assert!(unsafe { fds.push(w) });
        }
        fds
    }

    #[test]
    fn dmabuf_fds_close_each_once_on_close_and_not_again_on_drop() {
        let ((ra, wa), (rb, wb), (rc, wc)) = (pipe(), pipe(), pipe());
        let parking = pipe();
        let mut fds = owning(&[wa, wb, wc]);
        assert_eq!(&*fds, &[wa, wb, wc]);
        assert!(!closed(ra) && !closed(rb) && !closed(rc));
        assert_eq!(fds.fd(1).map(|f| std::os::fd::AsRawFd::as_raw_fd(&f)), Some(wb));
        fds.close();
        assert!(closed(ra) && closed(rb) && closed(rc), "every fd closed");
        assert!(fds.is_empty() && fds.fd(0).is_none());
        park(wa, parking);
        fds.close();
        drop(fds);
        assert!(!closed(parking.0), "neither a second close nor the drop closed it again");
        close_all(&[wa, ra, rb, rc, parking.0]);
    }

    #[test]
    fn dmabuf_fds_close_on_drop() {
        let ((ra, wa), (rb, wb)) = (pipe(), pipe());
        let parking = pipe();
        let fds = owning(&[wa, wb]);
        drop(fds);
        assert!(closed(ra) && closed(rb));
        park(wb, parking);
        assert!(!closed(parking.0));
        close_all(&[wb, ra, rb]);
        assert!(closed(parking.0), "the parked fd was open until now");
        close_all(&[parking.0]);
    }

    #[test]
    fn a_full_list_refuses_and_closes_the_fd() {
        let pipes: [(RawFd, RawFd); MAX_PLANES + 1] = std::array::from_fn(|_| pipe());
        let mut fds = owning(&pipes[..MAX_PLANES].iter().map(|p| p.1).collect::<Vec<_>>());
        let (r, w) = pipes[MAX_PLANES];
        // SAFETY: handed over, refused or not.
        assert!(!unsafe { fds.push(w) }, "a fifth fd fits nowhere");
        assert!(closed(r), "the refused fd is closed, not leaked");
        assert_eq!(fds.len(), MAX_PLANES);
        assert!(pipes[..MAX_PLANES].iter().all(|&(r, _)| !closed(r)));
        drop(fds);
        assert!(pipes.iter().all(|&(r, _)| closed(r)));
        close_all(&pipes.map(|p| p.0));
    }

    #[test]
    fn a_clone_dups_and_each_side_closes_only_its_own() {
        let ((ra, wa), (rb, wb)) = (pipe(), pipe());
        let fds = owning(&[wa, wb]);
        let copy = fds.try_clone().expect("a dup of each");
        assert_eq!(copy.len(), 2);
        assert!(copy.iter().zip(fds.iter()).all(|(c, o)| c != o), "new numbers");
        // SAFETY: F_GETFD on the clone's own fds.
        assert!(
            copy.iter().all(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } == libc::FD_CLOEXEC)
        );
        drop(fds);
        assert!(!closed(ra) && !closed(rb), "the clone keeps both open");
        drop(copy);
        assert!(closed(ra) && closed(rb));
        close_all(&[ra, rb]);
    }

    #[test]
    fn a_failed_clone_closes_the_dups_it_made() {
        let (ra, wa) = pipe();
        let mut fds = owning(&[wa]);
        // SAFETY: -1 is no fd, the dup of it fails and close skips it.
        assert!(unsafe { fds.push(-1) });
        assert_eq!(fds.try_clone().err(), Some(libc::EBADF));
        drop(fds);
        assert!(closed(ra), "the dup of the first fd did not leak");
        close_all(&[ra]);
    }

    #[test]
    fn a_frame_closes_its_fds_once_whether_taken_refused_or_closed_early() {
        let pool = profluens_core::memory::Pool::bounded(64, FRAMES_OUT as u32 + 2);
        let ch = GpuFrameChannel::new();
        let (mut tx, mut rx) = (ch.take_tx().unwrap(), ch.take_rx().unwrap());
        let ((ra, wa), (rb, wb), (rc, wc)) = (pipe(), pipe(), pipe());
        let parking = pipe();
        let with = |token, w| {
            let mut f = frame(token, 0);
            f.fds = owning(&[w]);
            f
        };

        assert!(tx.push(with(1, wa), pool.try_acquire().unwrap()));
        let mut one = rx.take(1).expect("token 1");
        assert!(!closed(ra), "open while the sink holds the frame");
        one.close_fds();
        assert!(closed(ra));
        park(wa, parking);
        drop(one);
        assert!(!closed(parking.0), "the drop after close_fds closed nothing");

        assert!(tx.push(with(2, wb), pool.try_acquire().unwrap()));
        rx.flush_before(1);
        assert!(closed(rb), "a flushed frame closes its fds");
        assert_eq!(released(&mut tx), [1, 2]);

        for token in 10..10 + FRAMES_OUT as u64 {
            assert!(tx.push(frame(token, 0), pool.try_acquire().unwrap()));
        }
        assert!(!tx.push(with(99, wc), pool.try_acquire().unwrap()));
        assert!(closed(rc), "a refused frame closes its fds");
        close_all(&[wa, ra, rb, rc, parking.0]);
    }
}
