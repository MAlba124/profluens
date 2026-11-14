//! The one unsafe module of pf-av1, safe wrappers over dav1d's context and pictures.

use std::fmt;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use libdav1d_sys as ffi;
use profluens_core::memory::Memory;
use profluens_video::color::{
    ChromaSite, ContentLight, HdrStatic, Mastering, Matrix, Primaries, Range, TransferFn,
};
use profluens_video::decoder::{Color, HeldLayout, Planar, PlaneRef, place, place_gray};
use profluens_video::{PixelFormat, PixelFormats};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Worker threads, 0 for the host's logical core count.
    pub threads: u32,
    /// Frames decoded ahead, 0 for `ceil(sqrt(threads))`, 1 for the lowest latency.
    pub frame_delay: u32,
    pub frame_size_limit: u32,
}

/// `DAV1D_MAX_THREADS` and `DAV1D_MAX_FRAME_DELAY` (dav1d.h).
pub const MAX_THREADS: u32 = 256;
pub const MAX_FRAME_DELAY: u32 = 256;

/// `DAV1D_PICTURE_ALIGNMENT` (picture.h): every plane starts on it and a buffer is padded by it.
pub const PICTURE_ALIGNMENT: usize = 64;

/// A negative return of dav1d, by the errno it negates (`DAV1D_ERR` in `common.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dav1dError {
    Invalid,
    OutOfMemory,
    NoSequenceHeader,
    FrameTooLarge,
    Unsupported,
    Busy,
    Other(i32),
}

impl Dav1dError {
    fn from_code(code: c_int) -> Self {
        use ffi::layout::*;
        match -code {
            EINVAL => Dav1dError::Invalid,
            ENOMEM => Dav1dError::OutOfMemory,
            ENOENT => Dav1dError::NoSequenceHeader,
            ERANGE => Dav1dError::FrameTooLarge,
            ENOPROTOOPT => Dav1dError::Unsupported,
            EAGAIN => Dav1dError::Busy,
            _ => Dav1dError::Other(code),
        }
    }
}

impl fmt::Display for Dav1dError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dav1dError::Invalid => f.write_str("invalid data or argument (EINVAL)"),
            Dav1dError::OutOfMemory => f.write_str("out of memory (ENOMEM)"),
            Dav1dError::NoSequenceHeader => f.write_str("no sequence header (ENOENT)"),
            Dav1dError::FrameTooLarge => f.write_str("frame over the size limit (ERANGE)"),
            Dav1dError::Unsupported => f.write_str("bit depth not built (ENOPROTOOPT)"),
            Dav1dError::Busy => f.write_str("decoder busy (EAGAIN)"),
            Dav1dError::Other(code) => write!(f, "dav1d error {code}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenError {
    pub error: Dav1dError,
    pub settings: Settings,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dav1d_open failed, {} (threads {}, frame delay {})",
            self.error, self.settings.threads, self.settings.frame_delay
        )
    }
}

impl std::error::Error for OpenError {}

// Upper bound on send retries, the loop stops earlier once take() makes no progress.
const SEND_ROUNDS: u32 = 4096;

const EAGAIN: c_int = ffi::dav1d_err(ffi::layout::EAGAIN);
const EINVAL: c_int = ffi::dav1d_err(ffi::layout::EINVAL);

/// dav1d's default picture layout (`dav1d_default_picture_alloc` in src/picture.c).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub stride: [usize; 2],
    pub offsets: [usize; 3],
    pub chroma: bool,
    pub bytes: usize,
}

/// Geometry of a `w`x`h` picture, `None` for a size or layout no picture has.
pub fn geometry(w: i32, h: i32, layout: c_uint, bpc: i32) -> Option<Geometry> {
    if w <= 0 || h <= 0 || !(8..=16).contains(&bpc) {
        return None;
    }
    let (ss_hor, ss_ver, chroma) = match layout {
        ffi::DAV1D_PIXEL_LAYOUT_I400 => (1, 1, false),
        ffi::DAV1D_PIXEL_LAYOUT_I420 => (1, 1, true),
        ffi::DAV1D_PIXEL_LAYOUT_I422 => (1, 0, true),
        ffi::DAV1D_PIXEL_LAYOUT_I444 => (0, 0, true),
        _ => return None,
    };
    let hbd = usize::from(bpc > 8);
    let aligned_w = (w as usize).div_ceil(128) * 128;
    let aligned_h = (h as usize).div_ceil(128) * 128;
    let mut y_stride = aligned_w << hbd;
    let mut uv_stride = if chroma { y_stride >> ss_hor } else { 0 };
    if y_stride.is_multiple_of(1024) {
        y_stride += PICTURE_ALIGNMENT;
    }
    if uv_stride.is_multiple_of(1024) && chroma {
        uv_stride += PICTURE_ALIGNMENT;
    }
    let y_sz = y_stride.checked_mul(aligned_h)?;
    let uv_sz = uv_stride.checked_mul(aligned_h >> ss_ver)?;
    let bytes = y_sz.checked_add(2 * uv_sz)?.checked_add(PICTURE_ALIGNMENT)?;
    let offsets = if chroma { [0, y_sz, y_sz + uv_sz] } else { [0, 0, 0] };
    Some(Geometry { stride: [y_stride, uv_stride], offsets, chroma, bytes })
}

/// Slots fed ahead of a decode, the picture and its film grain or super resolution copy.
pub const SPARE: usize = 2;

enum Live {
    Slot(Memory),
    Libc(*mut u8),
}

// SAFETY: the libc pointer is owned by the entry alone and freed once, at release.
unsafe impl Send for Live {}

impl Drop for Live {
    fn drop(&mut self) {
        if let Live::Libc(p) = self {
            // SAFETY: a pointer `posix_memalign` gave, freed once here.
            unsafe { libc::free(*p as *mut c_void) }
        }
    }
}

struct Shared {
    fed: Mutex<Vec<Memory>>,
    pictures: Mutex<Vec<Option<Live>>>,
    units: Mutex<Vec<Option<Memory>>>,
    wanted: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where a decoder's pictures and units live.
#[derive(Clone)]
pub struct PictureSupply {
    shared: Arc<Shared>,
}

impl Default for PictureSupply {
    fn default() -> Self {
        Self::new()
    }
}

impl PictureSupply {
    // Once per codec.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        let shared = Shared {
            fed: Mutex::new(Vec::new()),
            pictures: Mutex::new(Vec::new()),
            units: Mutex::new(Vec::new()),
            wanted: AtomicUsize::new(0),
        };
        PictureSupply { shared: Arc::new(shared) }
    }

    /// Bytes to feed a slot of, zero before a shown picture named its size.
    pub fn wanted(&self) -> usize {
        self.shared.wanted.load(Ordering::Relaxed)
    }

    pub fn want(&self, bytes: usize) {
        self.shared.wanted.store(bytes, Ordering::Relaxed);
    }

    pub fn spare(&self) -> usize {
        lock(&self.shared.fed).len()
    }

    pub fn feed(&self, memory: Memory) {
        lock(&self.shared.fed).push(memory);
    }

    /// Every view of memory the decoder holds.
    pub fn for_each_held(&self, f: &mut dyn FnMut(&Memory)) {
        lock(&self.shared.fed).iter().for_each(&mut *f);
        for live in lock(&self.shared.pictures).iter().flatten() {
            if let Live::Slot(m) = live {
                f(m);
            }
        }
        lock(&self.shared.units).iter().flatten().for_each(&mut *f);
    }

    pub fn live(&self) -> (usize, usize) {
        let pictures = lock(&self.shared.pictures).iter().flatten().count();
        let units = lock(&self.shared.units).iter().flatten().count();
        (pictures, units)
    }

    fn cookie(&self) -> *mut c_void {
        Arc::as_ptr(&self.shared) as *mut c_void
    }

    fn picture(&self, index: usize) -> Option<Memory> {
        match lock(&self.shared.pictures).get(index.checked_sub(1)?)? {
            Some(Live::Slot(m)) => Some(m.clone()),
            _ => None,
        }
    }
}

fn register<T>(registry: &Mutex<Vec<Option<T>>>, entry: T) -> usize {
    let mut live = lock(registry);
    match live.iter().position(Option::is_none) {
        Some(at) => {
            live[at] = Some(entry);
            at
        }
        None => {
            // Grows to the peak held at once, then stays.
            #[allow(clippy::disallowed_methods)]
            live.push(Some(entry));
            live.len() - 1
        }
    }
}

fn libc_picture(bytes: usize) -> *mut u8 {
    let mut p: *mut c_void = ptr::null_mut();
    // SAFETY: posix_memalign(3) with a power of two alignment, the out pointer is ours.
    if unsafe { libc::posix_memalign(&mut p, PICTURE_ALIGNMENT, bytes) } != 0 {
        return ptr::null_mut();
    }
    p as *mut u8
}

unsafe extern "C" fn alloc_picture(pic: *mut ffi::Dav1dPicture, cookie: *mut c_void) -> c_int {
    // SAFETY: `cookie` is the supply's `Shared`, alive until `dav1d_close` returns.
    let (shared, p) = unsafe { (&*(cookie as *const Shared), &mut *pic) };
    let Some(geo) = geometry(p.p.w, p.p.h, p.p.layout, p.p.bpc) else {
        return EINVAL;
    };
    let fed = {
        let mut fed = lock(&shared.fed);
        fed.retain(|m| m.capacity() >= geo.bytes);
        fed.pop()
    };
    let (base, live) = match fed {
        Some(mut memory) => {
            let base = memory.as_mut_full().as_mut_ptr();
            memory.set_len(geo.bytes);
            (base, Live::Slot(memory))
        }
        None => {
            let base = libc_picture(geo.bytes);
            if base.is_null() {
                return ffi::dav1d_err(ffi::layout::ENOMEM);
            }
            (base, Live::Libc(base))
        }
    };
    if !(base as usize).is_multiple_of(PICTURE_ALIGNMENT) {
        return EINVAL;
    }
    // SAFETY: every offset is within the `geo.bytes` the memory holds.
    let plane = |at: usize| {
        if geo.chroma { (unsafe { base.add(at) }) as *mut c_void } else { ptr::null_mut() }
    };
    p.data = [base as *mut c_void, plane(geo.offsets[1]), plane(geo.offsets[2])];
    p.stride = [geo.stride[0] as isize, geo.stride[1] as isize];
    let index = register(&shared.pictures, live);
    p.allocator_data = (index + 1) as *mut c_void;
    0
}

unsafe extern "C" fn release_picture(pic: *mut ffi::Dav1dPicture, cookie: *mut c_void) {
    // SAFETY: as in `alloc_picture`, a picture it filled.
    let (shared, p) = unsafe { (&*(cookie as *const Shared), &*pic) };
    let index = p.allocator_data as usize;
    let mut live = lock(&shared.pictures);
    if let Some(entry) = index.checked_sub(1).and_then(|i| live.get_mut(i)) {
        *entry = None;
    }
}

unsafe extern "C" fn release_unit(buf: *const u8, cookie: *mut c_void) {
    // SAFETY: `cookie` is the supply's `Shared`, alive until `dav1d_close` releases every unit.
    let shared = unsafe { &*(cookie as *const Shared) };
    let mut units = lock(&shared.units);
    if let Some(entry) =
        units.iter_mut().find(|m| m.as_ref().is_some_and(|m| m.data().as_ptr() == buf))
    {
        *entry = None;
    }
}

pub struct Decoder {
    ctx: *mut ffi::Dav1dContext,
    frame_delay: u32,
    supply: PictureSupply,
}

// SAFETY: driven from one thread at a time, dav1d's workers synchronise internally.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn open(settings: &Settings, supply: &PictureSupply) -> Result<Decoder, OpenError> {
        let fail = |error| OpenError { error, settings: *settings };
        // SAFETY: dav1d_default_settings fills every field of the zeroed struct.
        unsafe {
            let mut s: ffi::Dav1dSettings = std::mem::zeroed();
            ffi::dav1d_default_settings(&mut s);
            s.n_threads = settings.threads.min(MAX_THREADS) as c_int;
            s.max_frame_delay = settings.frame_delay.min(MAX_FRAME_DELAY) as c_int;
            s.apply_grain = 1;
            // Every shown frame comes out, crate::tu::Last keeps the last of each unit.
            s.all_layers = 1;
            s.frame_size_limit = settings.frame_size_limit;
            s.logger.callback = None;
            s.allocator = ffi::Dav1dPicAllocator {
                cookie: supply.cookie(),
                alloc_picture_callback: Some(alloc_picture),
                release_picture_callback: Some(release_picture),
            };
            let frame_delay = ffi::dav1d_get_frame_delay(&s);
            if frame_delay < 0 {
                return Err(fail(Dav1dError::from_code(frame_delay)));
            }
            let mut ctx: *mut ffi::Dav1dContext = ptr::null_mut();
            let code = ffi::dav1d_open(&mut ctx, &s);
            if code < 0 || ctx.is_null() {
                return Err(fail(Dav1dError::from_code(code)));
            }
            Ok(Decoder { ctx, frame_delay: frame_delay as u32, supply: supply.clone() })
        }
    }

    pub fn frame_delay(&self) -> u32 {
        self.frame_delay
    }

    /// Feed one temporal unit and hand `emit` the pictures that came out. `serial` comes back
    /// on each picture of the unit, see [`Picture::serial`].
    pub fn decode(
        &mut self,
        unit: &[u8],
        stamps: Stamps,
        emit: impl FnMut(Picture),
    ) -> Result<(), Dav1dError> {
        let data = Data::create(unit, stamps)?;
        self.send(data, emit)
    }

    /// [`decode`](Self::decode) over the unit's own memory, held until dav1d drops it.
    pub fn decode_memory(
        &mut self,
        unit: &Memory,
        stamps: Stamps,
        emit: impl FnMut(Picture),
    ) -> Result<(), Dav1dError> {
        let data = Data::wrap(unit, &self.supply, stamps)?;
        self.send(data, emit)
    }

    fn send(&mut self, mut data: Data, mut emit: impl FnMut(Picture)) -> Result<(), Dav1dError> {
        // Keep draining while dav1d makes progress, a TU may hold many shown frames.
        let mut empty = 0;
        for _ in 0..SEND_ROUNDS {
            // SAFETY: both pointers are live. On 0 dav1d took the reference, else Drop releases it.
            let code = unsafe { ffi::dav1d_send_data(self.ctx, &mut data.raw) };
            if code == 0 {
                if let Some(picture) = self.take()? {
                    emit(picture);
                }
                return Ok(());
            }
            if code != EAGAIN {
                return Err(Dav1dError::from_code(code));
            }
            match self.take()? {
                Some(picture) => {
                    empty = 0;
                    emit(picture);
                }
                None if empty >= 1 => break,
                None => empty += 1,
            }
        }
        Err(Dav1dError::Busy)
    }

    pub fn drain(&mut self, mut emit: impl FnMut(Picture)) -> Result<(), Dav1dError> {
        let mut empty = 0;
        while empty < 2 {
            match self.take()? {
                Some(picture) => {
                    empty = 0;
                    emit(picture);
                }
                None => empty += 1,
            }
        }
        Ok(())
    }

    /// Drop every frame in flight and all reference state (`dav1d_flush`).
    pub fn flush(&mut self) {
        // SAFETY: the context is open.
        unsafe { ffi::dav1d_flush(self.ctx) }
    }

    fn take(&mut self) -> Result<Option<Picture>, Dav1dError> {
        // SAFETY: a zeroed Dav1dPicture is the documented out parameter, Drop releases it.
        unsafe {
            let mut raw: ffi::Dav1dPicture = std::mem::zeroed();
            let code = ffi::dav1d_get_picture(self.ctx, &mut raw);
            if code == 0 {
                Ok(Some(Picture { raw, supply: self.supply.clone() }))
            } else if code == EAGAIN {
                Ok(None)
            } else {
                Err(Dav1dError::from_code(code))
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: nothing of the context is used after this and the supply outlives it.
        unsafe { ffi::dav1d_close(&mut self.ctx) }
    }
}

/// What a unit's pictures carry back, dav1d's `Dav1dDataProps`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamps {
    pub timestamp: i64,
    pub duration: i64,
    /// Rides in `offset`, which dav1d copies to the pictures and never reads.
    pub serial: i64,
}

struct Data {
    raw: ffi::Dav1dData,
}

impl Data {
    fn create(unit: &[u8], stamps: Stamps) -> Result<Data, Dav1dError> {
        if unit.is_empty() {
            return Err(Dav1dError::Invalid);
        }
        // SAFETY: dav1d_data_create returns `unit.len()` writable bytes or null.
        unsafe {
            let mut raw: ffi::Dav1dData = std::mem::zeroed();
            let dst = ffi::dav1d_data_create(&mut raw, unit.len());
            if dst.is_null() {
                return Err(Dav1dError::OutOfMemory);
            }
            ptr::copy_nonoverlapping(unit.as_ptr(), dst, unit.len());
            stamp(&mut raw, stamps);
            Ok(Data { raw })
        }
    }

    fn wrap(unit: &Memory, supply: &PictureSupply, stamps: Stamps) -> Result<Data, Dav1dError> {
        let bytes = unit.data();
        if bytes.is_empty() {
            return Err(Dav1dError::Invalid);
        }
        let view = unit.clone();
        let (ptr, len) = (bytes.as_ptr(), bytes.len());
        let index = register(&supply.shared.units, view);
        // SAFETY: the registered view keeps the range readable until `release_unit`.
        unsafe {
            let mut raw: ffi::Dav1dData = std::mem::zeroed();
            let code =
                ffi::dav1d_data_wrap(&mut raw, ptr, len, Some(release_unit), supply.cookie());
            if code < 0 {
                lock(&supply.shared.units)[index] = None;
                return Err(Dav1dError::from_code(code));
            }
            stamp(&mut raw, stamps);
            Ok(Data { raw })
        }
    }
}

fn stamp(raw: &mut ffi::Dav1dData, s: Stamps) {
    raw.m.timestamp = s.timestamp;
    raw.m.duration = s.duration;
    raw.m.offset = s.serial;
}

impl Drop for Data {
    fn drop(&mut self) {
        // SAFETY: dav1d_data_unref is a no-op on a zeroed struct.
        unsafe { ffi::dav1d_data_unref(&mut self.raw) }
    }
}

pub struct Picture {
    raw: ffi::Dav1dPicture,
    supply: PictureSupply,
}

// SAFETY: the picture's refcount is atomic and the release callback thread-safe.
unsafe impl Send for Picture {}

impl Picture {
    pub fn width(&self) -> u32 {
        self.raw.p.w.max(0) as u32
    }

    pub fn height(&self) -> u32 {
        self.raw.p.h.max(0) as u32
    }

    pub fn bit_depth(&self) -> u8 {
        self.raw.p.bpc as u8
    }

    /// The `video/raw` format and plane layout on a link taking `takes`, `None` if none fits.
    pub fn pixfmt(&self, takes: PixelFormats) -> Option<(PixelFormat, HeldLayout)> {
        let bits = u8::try_from(self.raw.p.bpc).ok()?;
        let chroma_shift = match self.raw.p.layout {
            ffi::DAV1D_PIXEL_LAYOUT_I400 => return place_gray(bits, takes),
            ffi::DAV1D_PIXEL_LAYOUT_I420 => (1, 1),
            ffi::DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
            ffi::DAV1D_PIXEL_LAYOUT_I444 => (0, 0),
            _ => return None,
        };
        place(Planar { bits, chroma_shift }, takes)
    }

    pub fn layout_name(&self) -> &'static str {
        match self.raw.p.layout {
            ffi::DAV1D_PIXEL_LAYOUT_I400 => "monochrome",
            ffi::DAV1D_PIXEL_LAYOUT_I420 => "4:2:0",
            ffi::DAV1D_PIXEL_LAYOUT_I422 => "4:2:2",
            ffi::DAV1D_PIXEL_LAYOUT_I444 => "4:4:4",
            _ => "unknown",
        }
    }

    pub fn timestamp(&self) -> i64 {
        self.raw.m.timestamp
    }

    pub fn duration(&self) -> i64 {
        self.raw.m.duration
    }

    /// The serial of the unit the picture came of, see [`Stamps`].
    pub fn serial(&self) -> i64 {
        self.raw.m.offset
    }

    pub fn bytes(&self) -> Option<usize> {
        geometry(self.raw.p.w, self.raw.p.h, self.raw.p.layout, self.raw.p.bpc).map(|g| g.bytes)
    }

    /// Plane `index`, one byte a sample at 8 bits and a little-endian u16 deeper.
    pub fn plane(&self, index: usize) -> PlaneRef<'_> {
        if index > 2 {
            return PlaneRef::EMPTY;
        }
        let data = self.raw.data[index];
        let stride = self.raw.stride[index.min(1)];
        if data.is_null() || stride <= 0 {
            return PlaneRef::EMPTY;
        }
        let stride = stride as usize;
        let (w, h) = (self.width() as usize, self.height() as usize);
        let (ss_x, ss_y) = match self.raw.p.layout {
            ffi::DAV1D_PIXEL_LAYOUT_I420 => (1, 1),
            ffi::DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
            ffi::DAV1D_PIXEL_LAYOUT_I400 if index > 0 => return PlaneRef::EMPTY,
            _ => (0, 0),
        };
        let (cols, rows) =
            if index == 0 { (w, h) } else { ((w + ss_x) >> ss_x, (h + ss_y) >> ss_y) };
        let bytes = if self.raw.p.bpc > 8 { 2 } else { 1 };
        if cols == 0 || rows == 0 {
            return PlaneRef::EMPTY;
        }
        let len = (rows - 1) * stride + cols * bytes;
        // SAFETY: the picture holds its buffer and `alloc_picture` lays out `len` readable bytes.
        let data = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
        PlaneRef { data, stride }
    }

    /// The slot the picture lies in, with plane offsets and strides.
    pub fn placement(&self) -> Option<(Memory, [usize; 3], [usize; 3])> {
        let memory = self.supply.picture(self.raw.allocator_data as usize)?;
        let base = memory.data().as_ptr() as usize;
        let mut offsets = [0; 3];
        for (o, d) in offsets.iter_mut().zip(self.raw.data) {
            if !d.is_null() {
                *o = (d as usize).checked_sub(base).filter(|o| *o < memory.len())?;
            }
        }
        let [y, uv] = self.raw.stride;
        if y <= 0 || uv < 0 {
            return None;
        }
        Some((memory, offsets, [y as usize, uv as usize, uv as usize]))
    }

    /// The sequence header colour (AV1 6.4.2 `color_config`) as H.273 code points.
    pub fn color(&self) -> Color {
        let hdr = self.raw.seq_hdr;
        if hdr.is_null() {
            return Color::default();
        }
        // SAFETY: the picture holds its sequence header, whose prefix layout libdav1d-sys asserts.
        let head = unsafe { &*(hdr as *const ffi::Dav1dSequenceHeaderHead) };
        let cp = |v: u32| u8::try_from(v).ok();
        Color {
            matrix: cp(head.mtrx).and_then(Matrix::from_h273),
            range: Some(if head.color_range != 0 { Range::Full } else { Range::Limited }),
            transfer: cp(head.trc).and_then(TransferFn::from_h273),
            primaries: cp(head.pri).and_then(Primaries::from_h273),
            hdr: self.hdr(),
            chroma: ChromaSite::from_av1(head.chr),
            dovi: None,
        }
    }

    fn hdr(&self) -> HdrStatic {
        let mut hdr = HdrStatic::default();
        if !self.raw.content_light.is_null() {
            // SAFETY: a non-null `content_light` is a `Dav1dContentLightLevel` the picture holds.
            let cll = unsafe { &*(self.raw.content_light as *const Dav1dContentLightLevel) };
            hdr.light = ContentLight {
                max_cll: cll.max_content_light_level,
                max_fall: cll.max_frame_average_light_level,
            };
        }
        if !self.raw.mastering_display.is_null() {
            // SAFETY: as above, a `Dav1dMasteringDisplay` held by `mastering_display_ref`.
            let md = unsafe { &*(self.raw.mastering_display as *const Dav1dMasteringDisplay) };
            hdr.mastering = mastering_from_av1(md);
        }
        hdr
    }
}

#[repr(C)]
struct Dav1dContentLightLevel {
    max_content_light_level: u16,
    max_frame_average_light_level: u16,
}

#[repr(C)]
struct Dav1dMasteringDisplay {
    primaries: [[u16; 2]; 3],
    white_point: [u16; 2],
    max_luminance: u32,
    min_luminance: u32,
}

const _: () = assert!(std::mem::size_of::<Dav1dContentLightLevel>() == 4);
const _: () = assert!(std::mem::size_of::<Dav1dMasteringDisplay>() == 24);

fn mastering_from_av1(md: &Dav1dMasteringDisplay) -> Mastering {
    Mastering::from_av1(md.primaries, md.white_point, md.max_luminance, md.min_luminance)
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture came out of dav1d_get_picture and is released once.
        unsafe { ffi::dav1d_picture_unref(&mut self.raw) }
    }
}

pub fn version() -> &'static str {
    // SAFETY: a static NUL-terminated string.
    unsafe { std::ffi::CStr::from_ptr(ffi::dav1d_version()) }.to_str().unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;
    use profluens_core::memory::Pool;

    #[test]
    fn av1_mastering_display_converts_to_st2086_units() {
        let fx = |x: f64| (x * 65536.0).round() as u16;
        let md = Dav1dMasteringDisplay {
            primaries: [[fx(0.708), fx(0.292)], [fx(0.170), fx(0.797)], [fx(0.131), fx(0.046)]],
            white_point: [fx(0.3127), fx(0.3290)],
            max_luminance: 1000 << 8,
            min_luminance: (0.005 * 16384.0f64).round() as u32,
        };
        let m = mastering_from_av1(&md);
        assert_eq!(m.primaries, [[35400, 14600], [8500, 39850], [6550, 2300]]);
        assert_eq!(m.white, [15635, 16450]);
        assert_eq!(m.max_luminance, 10_000_000);
        assert_eq!(m.min_luminance, 50);
    }

    #[test]
    fn geometry_matches_dav1ds_default_allocator() {
        let g = geometry(1920, 1080, ffi::DAV1D_PIXEL_LAYOUT_I420, 8).expect("1080p");
        assert_eq!(g.stride, [1920, 960], "128 aligned width, chroma half");
        assert_eq!(g.offsets, [0, 1920 * 1152, 1920 * 1152 + 960 * 576], "128 aligned height");
        assert_eq!(g.bytes, 1920 * 1152 + 2 * 960 * 576 + 64, "padded by the alignment");
        let g = geometry(4096, 2160, ffi::DAV1D_PIXEL_LAYOUT_I420, 8).expect("4k");
        assert_eq!(
            g.stride,
            [4096 + 64, 2048 + 64],
            "a stride that is a multiple of 1024 is padded"
        );
        let g = geometry(1920, 1080, ffi::DAV1D_PIXEL_LAYOUT_I420, 10).expect("10 bit");
        assert_eq!(g.stride, [3840, 1920], "two bytes a sample");
        let g = geometry(64, 48, ffi::DAV1D_PIXEL_LAYOUT_I400, 8).expect("mono");
        assert_eq!(
            (g.chroma, g.stride, g.offsets, g.bytes),
            (false, [128, 0], [0, 0, 0], 128 * 128 + 64)
        );
        let g = geometry(64, 48, ffi::DAV1D_PIXEL_LAYOUT_I444, 8).expect("444");
        assert_eq!((g.stride, g.offsets[2]), ([128, 128], 2 * 128 * 128));
        assert!(geometry(0, 48, ffi::DAV1D_PIXEL_LAYOUT_I420, 8).is_none());
        assert!(geometry(64, 48, 7, 8).is_none(), "no such layout");
        assert!(geometry(64, 48, ffi::DAV1D_PIXEL_LAYOUT_I420, 4).is_none(), "no such depth");
    }

    fn blank(w: i32, h: i32) -> ffi::Dav1dPicture {
        // SAFETY: a zeroed Dav1dPicture is what dav1d fills before the callback.
        let mut pic: ffi::Dav1dPicture = unsafe { std::mem::zeroed() };
        pic.p = ffi::Dav1dPictureParameters { w, h, layout: ffi::DAV1D_PIXEL_LAYOUT_I420, bpc: 8 };
        pic
    }

    fn release(supply: &PictureSupply, index: usize) {
        let mut pic = blank(64, 48);
        pic.allocator_data = index as *mut c_void;
        // SAFETY: the callback reads `allocator_data` alone, of a picture the supply made.
        unsafe { release_picture(&mut pic, supply.cookie()) };
    }

    /// Drops a hand made picture's supply and skips its unref, no dav1d reference is behind it.
    fn unmade(picture: Picture) {
        let picture = std::mem::ManuallyDrop::new(picture);
        // SAFETY: read once, the picture itself is never dropped.
        drop(unsafe { std::ptr::read(&picture.supply) });
    }

    #[test]
    fn pictures_take_fed_slots_and_release_into_the_registry() {
        let supply = PictureSupply::new();
        let pool = Pool::new(1 << 20);
        let geo = geometry(64, 48, ffi::DAV1D_PIXEL_LAYOUT_I420, 8).expect("geometry");
        let mut pic = blank(64, 48);
        // SAFETY: a blank picture and the supply's cookie, as dav1d calls it.
        assert_eq!(unsafe { alloc_picture(&mut pic, supply.cookie()) }, 0);
        assert_eq!(supply.wanted(), 0, "a size is only wanted once a picture is shown");
        assert_eq!(supply.live(), (1, 0));
        assert!(!pic.data[0].is_null() && pic.allocator_data as usize == 1);
        assert_eq!(pic.stride, [128, 64]);
        let private = Picture { raw: pic, supply: supply.clone() };
        assert!(private.placement().is_none(), "libc memory is no slot");
        assert_eq!(private.bytes(), Some(geo.bytes));
        assert_eq!((private.plane(0).data.len(), private.plane(0).stride), (47 * 128 + 64, 128));
        assert_eq!((private.plane(1).data.len(), private.plane(2).stride), (23 * 64 + 32, 64));
        let mut n = 0;
        supply.for_each_held(&mut |_| n += 1);
        assert_eq!(n, 0, "nothing of a pool is held");
        unmade(private);
        release(&supply, 1);
        assert_eq!(supply.live(), (0, 0));
        supply.want(geo.bytes);
        let short = Pool::new(64).acquire();
        supply.feed(short);
        let slot = pool.acquire();
        let id = slot.slot_id();
        supply.feed(slot);
        assert_eq!(supply.spare(), 2);
        let mut pic = blank(64, 48);
        // SAFETY: as above.
        assert_eq!(unsafe { alloc_picture(&mut pic, supply.cookie()) }, 0);
        assert_eq!(supply.spare(), 0, "the short one went back, the fitting one was taken");
        let index = pic.allocator_data as usize;
        let placed = Picture { raw: pic, supply: supply.clone() };
        let (memory, offsets, strides) = placed.placement().expect("placed");
        assert_eq!((memory.slot_id(), offsets, strides), (id, geo.offsets, [128, 64, 64]));
        let mut n = 0;
        supply.for_each_held(&mut |m| n += usize::from(m.slot_id() == id));
        assert_eq!(n, 1, "counted among what the decoder holds");
        unmade(placed);
        release(&supply, index);
        assert_eq!(supply.live(), (0, 0));
        drop(memory);
        assert_eq!(pool.stats().outstanding, 0, "the slot is back in its pool");
    }

    #[test]
    fn a_wrapped_unit_is_held_until_released() {
        let supply = PictureSupply::new();
        let pool = Pool::new(256);
        let mut unit = pool.acquire();
        unit.as_mut_full()[..4].copy_from_slice(&[1, 2, 3, 4]);
        unit.set_len(4);
        let stamps = Stamps { timestamp: 7, duration: 1, serial: 9 };
        let data = Data::wrap(&unit, &supply, stamps).expect("wrapped");
        let m = data.raw.m;
        assert_eq!((data.raw.sz, m.timestamp, m.duration, m.offset), (4, 7, 1, 9));
        assert_eq!(data.raw.data, unit.data().as_ptr(), "read in place");
        assert_eq!(supply.live(), (0, 1));
        drop(unit);
        assert_eq!(pool.stats().outstanding, 1, "the held view keeps the slot out");
        drop(data);
        assert_eq!(supply.live(), (0, 0), "the last reference released the view");
        assert_eq!(pool.stats().outstanding, 0);
        let empty = Pool::new(256).acquire();
        assert_eq!(Data::wrap(&empty, &supply, stamps).err(), Some(Dav1dError::Invalid));
    }
}
