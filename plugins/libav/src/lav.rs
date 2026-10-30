//! The one unsafe module of pf-libav, a safe [`Decoder`], [`Frame`] and [`Encoder`] over
//! the `libav-sys` shim. Video pictures decode into slots of the element's pool ([`Supply`]).

use std::fmt;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use libav_sys as ffi;
use profluens_core::memory::Memory;
use profluens_video::PixelFormat;
use profluens_video::color::{
    ChromaSite, ContentLight, HdrStatic, Mastering, Matrix, Primaries, Range, TransferFn,
};
use profluens_video::decoder::{Color, HeldLayout, PlaneRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vp8,
    Vp9,
    Aac,
    Ac3,
    Mp3,
    Dts,
    TrueHd,
    Mlp,
    Mpeg2,
    Mpeg4,
    Alac,
    Flv1,
    Vp6,
    Vp6f,
    Vp6a,
    Vp7,
    Vorbis,
}

impl Codec {
    /// The libavcodec decoder name, for log fields.
    pub const fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Vp8 => "vp8",
            Codec::Vp9 => "vp9",
            Codec::Aac => "aac",
            Codec::Ac3 => "ac3",
            Codec::Mp3 => "mp3",
            Codec::Dts => "dts",
            Codec::TrueHd => "truehd",
            Codec::Mlp => "mlp",
            Codec::Mpeg2 => "mpeg2video",
            Codec::Mpeg4 => "mpeg4",
            Codec::Alac => "alac",
            Codec::Flv1 => "flv",
            Codec::Vp6 => "vp6",
            Codec::Vp6f => "vp6f",
            Codec::Vp6a => "vp6a",
            Codec::Vp7 => "vp7",
            Codec::Vorbis => "vorbis",
        }
    }

    fn raw(self) -> c_int {
        match self {
            Codec::H264 => ffi::PF_LAV_H264,
            Codec::Hevc => ffi::PF_LAV_HEVC,
            Codec::Vp8 => ffi::PF_LAV_VP8,
            Codec::Vp9 => ffi::PF_LAV_VP9,
            Codec::Aac => ffi::PF_LAV_AAC,
            Codec::Ac3 => ffi::PF_LAV_AC3,
            Codec::Mp3 => ffi::PF_LAV_MP3,
            Codec::Dts => ffi::PF_LAV_DTS,
            Codec::TrueHd => ffi::PF_LAV_TRUEHD,
            Codec::Mlp => ffi::PF_LAV_MLP,
            Codec::Mpeg2 => ffi::PF_LAV_MPEG2,
            Codec::Mpeg4 => ffi::PF_LAV_MPEG4,
            Codec::Alac => ffi::PF_LAV_ALAC,
            Codec::Flv1 => ffi::PF_LAV_FLV1,
            Codec::Vp6 => ffi::PF_LAV_VP6,
            Codec::Vp6f => ffi::PF_LAV_VP6F,
            Codec::Vp6a => ffi::PF_LAV_VP6A,
            Codec::Vp7 => ffi::PF_LAV_VP7,
            Codec::Vorbis => ffi::PF_LAV_VORBIS,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LavError {
    NoCodec,
    OutOfMemory,
    Argument,
    Av(i32),
}

impl LavError {
    fn from_code(code: c_int) -> Self {
        match code {
            ffi::PF_LAV_ERR_NO_CODEC => LavError::NoCodec,
            ffi::PF_LAV_ERR_NOMEM => LavError::OutOfMemory,
            ffi::PF_LAV_ERR_ARG => LavError::Argument,
            c => LavError::Av(c),
        }
    }

    /// The shim or AVERROR code, for log fields.
    pub const fn code(self) -> i32 {
        match self {
            LavError::NoCodec => ffi::PF_LAV_ERR_NO_CODEC,
            LavError::OutOfMemory => ffi::PF_LAV_ERR_NOMEM,
            LavError::Argument => ffi::PF_LAV_ERR_ARG,
            LavError::Av(c) => c,
        }
    }
}

impl fmt::Display for LavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LavError::NoCodec => f.write_str("codec not in this libavcodec build"),
            LavError::OutOfMemory => f.write_str("out of memory"),
            LavError::Argument => f.write_str("argument refused"),
            LavError::Av(code) => {
                let mut buf = [0 as c_char; 96];
                // SAFETY: av_strerror writes at most `len` bytes, NUL terminated.
                unsafe { ffi::pf_lav_strerror(*code, buf.as_mut_ptr(), buf.len()) };
                // SAFETY: NUL terminated within the buffer, as above.
                let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
                write!(f, "{} ({code})", s.to_str().unwrap_or("?"))
            }
        }
    }
}

impl fmt::Debug for LavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for LavError {}

struct Live {
    memory: Memory,
    base: usize,
    end: usize,
}

type Direct = *mut (dyn FnMut(usize) -> Option<Memory> + 'static);

struct DirectCell(Option<Direct>);

// SAFETY: set and cleared around one FFI call and only called within it, on the same thread.
unsafe impl Send for DirectCell {}

struct Shared {
    fed: Mutex<Vec<Memory>>,
    live: Mutex<Vec<Option<Live>>>,
    wanted: AtomicUsize,
    direct: Mutex<DirectCell>,
    short: Mutex<Option<(usize, usize)>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
pub struct Supply {
    shared: Arc<Shared>,
}

impl Default for Supply {
    fn default() -> Self {
        Self::new()
    }
}

impl Supply {
    // Once per codec, the empty registries.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        let shared = Shared {
            fed: Mutex::new(Vec::new()),
            live: Mutex::new(Vec::new()),
            wanted: AtomicUsize::new(0),
            direct: Mutex::new(DirectCell(None)),
            short: Mutex::new(None),
        };
        Supply { shared: Arc::new(shared) }
    }

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

    pub fn take_short(&self) -> Option<(usize, usize)> {
        lock(&self.shared.short).take()
    }

    pub fn for_each_held(&self, f: &mut dyn FnMut(&Memory)) {
        lock(&self.shared.fed).iter().for_each(&mut *f);
        lock(&self.shared.live).iter().flatten().for_each(|l| f(&l.memory));
    }

    pub fn live(&self) -> usize {
        lock(&self.shared.live).iter().flatten().count()
    }

    fn slot_of(&self, ptr: *const u8) -> Option<(Memory, usize)> {
        let p = ptr as usize;
        let live = lock(&self.shared.live);
        let l = live.iter().flatten().find(|l| l.base <= p && p < l.end)?;
        Some((l.memory.clone(), l.base))
    }

    fn cookie(&self) -> *mut c_void {
        Arc::as_ptr(&self.shared) as *mut c_void
    }
}

fn register<T>(registry: &Mutex<Vec<Option<T>>>, entry: T) {
    let mut live = lock(registry);
    match live.iter().position(Option::is_none) {
        Some(at) => live[at] = Some(entry),
        // Grows to the most libavcodec holds at once.
        #[allow(clippy::disallowed_methods)]
        None => live.push(Some(entry)),
    }
}

unsafe extern "C" fn acquire(opaque: *mut c_void, bytes: usize) -> *mut u8 {
    // SAFETY: `opaque` is the supply's `Shared`, alive until the decoder closes.
    let shared = unsafe { &*(opaque as *const Shared) };
    let fed = {
        let mut fed = lock(&shared.fed);
        fed.retain(|m| m.capacity() >= bytes);
        fed.pop()
    };
    let memory = fed.or_else(|| {
        let direct = lock(&shared.direct).0?;
        // SAFETY: the closure of the call in progress on this thread, live until it returns.
        unsafe { (*direct)(bytes) }
    });
    let Some(mut memory) = memory else {
        return ptr::null_mut();
    };
    if memory.capacity() < bytes {
        *lock(&shared.short) = Some((bytes, memory.capacity()));
        return ptr::null_mut();
    }
    let base = memory.as_mut_full().as_mut_ptr();
    memory.set_len(bytes);
    register(&shared.live, Live { memory, base: base as usize, end: base as usize + bytes });
    base
}

unsafe extern "C" fn release(opaque: *mut c_void, ptr: *mut u8) {
    // SAFETY: as in `acquire`.
    let shared = unsafe { &*(opaque as *const Shared) };
    let p = ptr as usize;
    let mut live = lock(&shared.live);
    if let Some(entry) =
        live.iter_mut().find(|l| l.as_ref().is_some_and(|l| l.base <= p && p < l.end))
    {
        *entry = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    Taken,
    Full,
    Ended,
}

// Lives for one match on the stack, a box would allocate per frame.
#[allow(clippy::large_enum_variant)]
pub enum Received {
    Frame(Frame),
    Again,
    Ended,
}

pub struct Parsed {
    pub used: usize,
    /// A packet the decoder refused is no parse error, the bytes behind it are still the stream.
    pub sent: Option<(Result<Sent, LavError>, i64)>,
}

pub struct Decoder {
    raw: *mut ffi::pf_lav_dec,
    supply: Option<Supply>,
}

// SAFETY: driven from one thread at a time and starts no thread of its own (thread_count 1).
unsafe impl Send for Decoder {}

pub const NONE: i64 = i64::MIN;

impl Decoder {
    pub fn open(
        codec: Codec,
        extradata: &[u8],
        supply: Option<&Supply>,
        parse: bool,
    ) -> Result<Decoder, LavError> {
        let slots = supply.map(|s| ffi::pf_lav_slots {
            opaque: s.cookie(),
            acquire: Some(acquire),
            release: Some(release),
        });
        let slots_ptr = slots.as_ref().map_or(ptr::null(), |s| s as *const ffi::pf_lav_slots);
        let mut raw = ptr::null_mut();
        // SAFETY: pointers live for the call, the shim copies extradata and slots.
        let code = unsafe {
            ffi::pf_lav_open(
                &mut raw,
                codec.raw(),
                extradata.as_ptr(),
                extradata.len(),
                slots_ptr,
                c_int::from(parse),
            )
        };
        if code != ffi::PF_LAV_OK || raw.is_null() {
            return Err(LavError::from_code(code));
        }
        Ok(Decoder { raw, supply: supply.cloned() })
    }

    fn with_direct<R>(
        &mut self,
        direct: &mut dyn FnMut(usize) -> Option<Memory>,
        body: impl FnOnce(*mut ffi::pf_lav_dec) -> R,
    ) -> R {
        let Some(supply) = &self.supply else {
            return body(self.raw);
        };
        struct Clear<'a>(&'a Shared);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                lock(&self.0.direct).0 = None;
            }
        }
        // SAFETY: only the lifetime is erased. `Clear` resets the pointer before `direct` drops.
        let erased: Direct = unsafe {
            std::mem::transmute::<*mut (dyn FnMut(usize) -> Option<Memory> + '_), Direct>(direct)
        };
        lock(&supply.shared.direct).0 = Some(erased);
        let _clear = Clear(&supply.shared);
        body(self.raw)
    }

    pub fn send(
        &mut self,
        data: &[u8],
        pts: i64,
        dts: i64,
        duration: i64,
        direct: &mut dyn FnMut(usize) -> Option<Memory>,
    ) -> Result<Sent, LavError> {
        // SAFETY: the decoder is open and `data` is live for the call, which copies it.
        let code = self.with_direct(direct, |raw| unsafe {
            ffi::pf_lav_send(raw, data.as_ptr(), data.len(), pts, dts, duration)
        });
        sent(code)
    }

    pub fn parse_send(
        &mut self,
        input: &[u8],
        pts: i64,
        direct: &mut dyn FnMut(usize) -> Option<Memory>,
    ) -> Result<Parsed, LavError> {
        let mut out: *const u8 = ptr::null();
        let (mut len, mut out_pts) = (0usize, NONE);
        // SAFETY: the decoder is open and the out pointers are live for the call.
        let used = unsafe {
            ffi::pf_lav_parse(
                self.raw,
                input.as_ptr(),
                input.len(),
                pts,
                &mut out,
                &mut len,
                &mut out_pts,
            )
        };
        if used < 0 {
            return Err(LavError::from_code(used));
        }
        if out.is_null() || len == 0 {
            return Ok(Parsed { used: used as usize, sent: None });
        }
        // SAFETY: the parser's packet stays valid until its next call, after this send.
        let code = self.with_direct(direct, |raw| unsafe {
            ffi::pf_lav_send(raw, out, len, out_pts, NONE, 0)
        });
        Ok(Parsed { used: used as usize, sent: Some((sent(code), out_pts)) })
    }

    pub fn drain(&mut self) -> Result<(), LavError> {
        // SAFETY: the decoder is open.
        let code = unsafe { ffi::pf_lav_drain(self.raw) };
        if code < 0 {
            return Err(LavError::from_code(code));
        }
        Ok(())
    }

    pub fn receive(
        &mut self,
        direct: &mut dyn FnMut(usize) -> Option<Memory>,
    ) -> Result<Received, LavError> {
        // SAFETY: a zeroed pf_lav_frame is a valid out parameter, the shim fills it.
        let mut raw: ffi::pf_lav_frame = unsafe { std::mem::zeroed() };
        // SAFETY: the decoder is open and `raw` is live for the call.
        let code = self.with_direct(direct, |d| unsafe { ffi::pf_lav_receive(d, &mut raw) });
        match code {
            ffi::PF_LAV_OK => Ok(Received::Frame(Frame { raw, supply: self.supply.clone() })),
            ffi::PF_LAV_AGAIN => Ok(Received::Again),
            ffi::PF_LAV_EOF => Ok(Received::Ended),
            c => Err(LavError::from_code(c)),
        }
    }

    pub fn flush(&mut self) {
        // SAFETY: the decoder is open.
        unsafe { ffi::pf_lav_flush(self.raw) }
    }

    pub fn skip_nonref(&mut self, on: bool) {
        // SAFETY: the decoder is open.
        unsafe { ffi::pf_lav_skip_nonref(self.raw, on as c_int) }
    }

    pub fn fallbacks(&self) -> u64 {
        // SAFETY: the decoder is open.
        unsafe { ffi::pf_lav_fallbacks(self.raw) }
    }
}

fn sent(code: c_int) -> Result<Sent, LavError> {
    match code {
        ffi::PF_LAV_OK => Ok(Sent::Taken),
        ffi::PF_LAV_AGAIN => Ok(Sent::Full),
        ffi::PF_LAV_EOF => Ok(Sent::Ended),
        c => Err(LavError::from_code(c)),
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: closed once. The supply outlives the close, which releases every held slot.
        unsafe { ffi::pf_lav_close(self.raw) }
    }
}

pub enum Encoded<'a> {
    Packet { data: &'a [u8], pts: i64, duration: i64 },
    Again,
    Ended,
}

pub struct Encoder {
    raw: *mut ffi::pf_lav_enc,
    channels: usize,
    frame: usize,
    delay: usize,
}

// SAFETY: driven from one thread at a time and starts no thread of its own (thread_count 1).
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn open(
        codec: Codec,
        rate: u32,
        channels: u8,
        bit_rate: u32,
        fast: bool,
    ) -> Result<Encoder, LavError> {
        let rate = c_int::try_from(rate).map_err(|_| LavError::Argument)?;
        // send() divides by the channel count.
        if channels == 0 {
            return Err(LavError::Argument);
        }
        let mut raw = ptr::null_mut();
        // SAFETY: an out pointer live for the call.
        let code = unsafe {
            ffi::pf_lav_enc_open(
                &mut raw,
                codec.raw(),
                rate,
                c_int::from(channels),
                i64::from(bit_rate),
                c_int::from(fast),
            )
        };
        if code != ffi::PF_LAV_OK || raw.is_null() {
            return Err(LavError::from_code(code));
        }
        // SAFETY: opened above.
        let (frame, delay) =
            unsafe { (ffi::pf_lav_enc_frame_size(raw), ffi::pf_lav_enc_delay(raw)) };
        Ok(Encoder {
            raw,
            channels: usize::from(channels),
            frame: frame.max(1) as usize,
            delay: delay.max(0) as usize,
        })
    }

    pub fn frame_size(&self) -> usize {
        self.frame
    }

    pub fn delay(&self) -> usize {
        self.delay
    }

    pub fn config(&self) -> &[u8] {
        let mut len = 0;
        // SAFETY: the encoder is open, the bytes live until close, which `&self` outlasts.
        let p = unsafe { ffi::pf_lav_enc_config(self.raw, &mut len) };
        if p.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: as above, `len` bytes at `p`.
        unsafe { std::slice::from_raw_parts(p, len) }
    }

    pub fn send(&mut self, pcm: &[f32], pts: i64) -> Result<Sent, LavError> {
        let samples = pcm.len() / self.channels;
        if samples == 0 || samples > self.frame || !pcm.len().is_multiple_of(self.channels) {
            return Err(LavError::Argument);
        }
        // SAFETY: the encoder is open and `pcm` holds `samples` frames of all channels.
        sent(unsafe { ffi::pf_lav_enc_send(self.raw, pcm.as_ptr(), samples as c_int, pts) })
    }

    pub fn drain(&mut self) -> Result<(), LavError> {
        // SAFETY: the encoder is open.
        let code = unsafe { ffi::pf_lav_enc_drain(self.raw) };
        if code < 0 {
            return Err(LavError::from_code(code));
        }
        Ok(())
    }

    pub fn receive(&mut self) -> Result<Encoded<'_>, LavError> {
        let (mut data, mut len, mut pts, mut duration) = (ptr::null(), 0usize, 0i64, 0i64);
        // SAFETY: the encoder is open and the out pointers live for the call.
        let code = unsafe {
            ffi::pf_lav_enc_receive(self.raw, &mut data, &mut len, &mut pts, &mut duration)
        };
        match code {
            ffi::PF_LAV_OK if !data.is_null() => {
                // SAFETY: the packet lives until the next receive or close, both take `&mut self`.
                let data = unsafe { std::slice::from_raw_parts(data, len) };
                Ok(Encoded::Packet { data, pts, duration })
            }
            ffi::PF_LAV_OK => Ok(Encoded::Again),
            ffi::PF_LAV_AGAIN => Ok(Encoded::Again),
            ffi::PF_LAV_EOF => Ok(Encoded::Ended),
            c => Err(LavError::from_code(c)),
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: open, closed once.
        unsafe { ffi::pf_lav_enc_close(self.raw) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Samples {
    U8,
    S16,
    S32,
    F32,
    F64,
}

pub struct Frame {
    raw: ffi::pf_lav_frame,
    supply: Option<Supply>,
}

// SAFETY: a counted reference, libavcodec's counts are atomic and slot release locks.
unsafe impl Send for Frame {}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: the handle came out of pf_lav_receive and is freed once.
        unsafe { ffi::pf_lav_frame_free(self.raw.handle) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Named(PixelFormat, HeldLayout),
    Other { bit_depth: u8, chroma_shift: (u8, u8) },
}

impl Frame {
    pub fn pts(&self) -> i64 {
        self.raw.pts
    }

    pub fn duration(&self) -> i64 {
        self.raw.duration
    }

    pub fn width(&self) -> u32 {
        self.raw.width.max(0) as u32
    }

    pub fn height(&self) -> u32 {
        self.raw.height.max(0) as u32
    }

    pub fn corrupt(&self) -> bool {
        self.raw.flags & ffi::PF_LAV_FRAME_CORRUPT != 0
    }

    pub fn shape(&self) -> Shape {
        let bit_depth = self.raw.bit_depth.clamp(0, 255) as u8;
        let chroma_shift = self.chroma_shift();
        match self.raw.pixfmt {
            ffi::PF_LAV_PIX_I420 => Shape::Named(PixelFormat::I420, HeldLayout::Native),
            ffi::PF_LAV_PIX_GRAY8 => Shape::Named(PixelFormat::Gray8, HeldLayout::Native),
            ffi::PF_LAV_PIX_I420_10 => Shape::Named(PixelFormat::P010, HeldLayout::Planar10),
            ffi::PF_LAV_PIX_YUV if chroma_shift.0 <= 1 && chroma_shift.1 <= 1 => {
                let pixfmt = if bit_depth > 8 { PixelFormat::P010 } else { PixelFormat::I420 };
                Shape::Named(pixfmt, HeldLayout::Reduce { chroma_shift, bits: bit_depth })
            }
            _ => Shape::Other { bit_depth, chroma_shift },
        }
    }

    fn chroma_shift(&self) -> (u8, u8) {
        (self.raw.chroma_shift_w.clamp(0, 8) as u8, self.raw.chroma_shift_h.clamp(0, 8) as u8)
    }

    pub fn plane(&self, index: usize) -> PlaneRef<'_> {
        let planes = match self.raw.pixfmt {
            ffi::PF_LAV_PIX_GRAY8 => 1,
            ffi::PF_LAV_PIX_I420 | ffi::PF_LAV_PIX_I420_10 | ffi::PF_LAV_PIX_YUV => 3,
            _ => 0,
        };
        if index >= planes {
            return PlaneRef::EMPTY;
        }
        let data = self.raw.data[index];
        let stride = self.raw.linesize[index];
        if data.is_null() || stride <= 0 {
            return PlaneRef::EMPTY;
        }
        let stride = stride as usize;
        let (w, h) = (self.width() as usize, self.height() as usize);
        // Chroma is luma shifted down, rounded up (AV_CEIL_RSHIFT).
        let (sw, sh) = match self.raw.pixfmt {
            ffi::PF_LAV_PIX_YUV => self.chroma_shift(),
            _ => (1, 1),
        };
        let (cols, rows) =
            if index == 0 { (w, h) } else { (w.div_ceil(1 << sw), h.div_ceil(1 << sh)) };
        let bytes = match self.raw.pixfmt {
            ffi::PF_LAV_PIX_I420_10 => 2,
            ffi::PF_LAV_PIX_YUV if self.raw.bit_depth > 8 => 2,
            _ => 1,
        };
        if cols == 0 || rows == 0 {
            return PlaneRef::EMPTY;
        }
        let len = (rows - 1) * stride + cols * bytes;
        // SAFETY: the frame holds its buffers, rows are `stride` apart with `cols` samples each.
        let data = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
        PlaneRef { data, stride }
    }

    pub fn placement(&self) -> Option<(Memory, [usize; 3], [usize; 3])> {
        if self.raw.in_slot == 0 {
            return None;
        }
        let supply = self.supply.as_ref()?;
        let (memory, _) = supply.slot_of(self.raw.data[0])?;
        let base = memory.data().as_ptr() as usize;
        let mut offsets = [0; 3];
        let mut strides = [0; 3];
        for i in 0..3 {
            let d = self.raw.data[i];
            if d.is_null() {
                continue;
            }
            offsets[i] = (d as usize).checked_sub(base).filter(|o| *o < memory.len())?;
            strides[i] = usize::try_from(self.raw.linesize[i]).ok()?;
        }
        Some((memory, offsets, strides))
    }

    pub fn slot_bytes(&self) -> Option<usize> {
        self.placement().map(|(m, _, _)| m.len())
    }

    pub fn color(&self) -> Color {
        let cp = |v: i32| u8::try_from(v).ok();
        let range = match self.raw.range {
            1 => Some(Range::Limited),
            2 => Some(Range::Full),
            _ => None,
        };
        let mut hdr = HdrStatic::default();
        if self.raw.flags & ffi::PF_LAV_FRAME_MASTERING != 0 {
            let p = self.raw.mastering_primaries;
            hdr.mastering = Mastering {
                primaries: [[p[0], p[1]], [p[2], p[3]], [p[4], p[5]]],
                white: self.raw.mastering_white,
                max_luminance: self.raw.mastering_max,
                min_luminance: self.raw.mastering_min,
            };
        }
        if self.raw.flags & ffi::PF_LAV_FRAME_LIGHT != 0 {
            hdr.light = ContentLight { max_cll: self.raw.max_cll, max_fall: self.raw.max_fall };
        }
        Color {
            matrix: cp(self.raw.matrix).and_then(Matrix::from_h273),
            range,
            transfer: cp(self.raw.transfer).and_then(TransferFn::from_h273),
            primaries: cp(self.raw.primaries).and_then(Primaries::from_h273),
            hdr,
            chroma: u32::try_from(self.raw.chroma_location.saturating_sub(1))
                .ok()
                .and_then(ChromaSite::from_h26x),
            dovi: None,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.raw.sample_rate.max(0) as u32
    }

    pub fn channels(&self) -> usize {
        self.raw.channels.max(0) as usize
    }

    pub fn samples(&self) -> usize {
        self.raw.samples.max(0) as usize
    }

    pub fn channel_mask(&self) -> u64 {
        self.raw.channel_mask
    }

    /// Only Vorbis sets it, on its first frame behind an open or a flush.
    pub fn skip(&self) -> usize {
        (self.raw.skip.max(0) as usize).min(self.samples())
    }

    pub fn sample_format(&self) -> Option<(Samples, bool)> {
        let planar = self.raw.sample_fmt & ffi::PF_LAV_PLANAR != 0;
        let s = match self.raw.sample_fmt & !ffi::PF_LAV_PLANAR {
            ffi::PF_LAV_SMP_U8 => Samples::U8,
            ffi::PF_LAV_SMP_S16 => Samples::S16,
            ffi::PF_LAV_SMP_S32 => Samples::S32,
            ffi::PF_LAV_SMP_FLT => Samples::F32,
            ffi::PF_LAV_SMP_DBL => Samples::F64,
            _ => return None,
        };
        Some((s, planar))
    }

    pub fn audio_plane(&self, index: usize) -> &[u8] {
        let Some((s, planar)) = self.sample_format() else {
            return &[];
        };
        let width = match s {
            Samples::U8 => 1,
            Samples::S16 => 2,
            Samples::S32 | Samples::F32 => 4,
            Samples::F64 => 8,
        };
        let planes = if planar { self.channels() } else { 1 };
        if index >= planes.min(8) {
            return &[];
        }
        let per = if planar { 1 } else { self.channels() };
        let data = self.raw.data[index];
        if data.is_null() {
            return &[];
        }
        let len = self.samples() * per * width;
        // SAFETY: each plane holds nb_samples per channel, packed ones all channels in plane 0.
        unsafe { std::slice::from_raw_parts(data as *const u8, len) }
    }
}

pub fn version() -> &'static str {
    // SAFETY: a static NUL-terminated string.
    unsafe { std::ffi::CStr::from_ptr(ffi::pf_lav_version()) }.to_str().unwrap_or("?")
}

// Test code.
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use profluens_core::memory::Pool;

    fn units() -> Vec<Vec<u8>> {
        let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ipb.h264"))
            .expect("fixture");
        let mut cuts = vec![0];
        let mut seen_vcl = false;
        let mut i = 0;
        while i + 4 < bytes.len() {
            if bytes[i..i + 3] == [0, 0, 1] {
                let t = bytes[i + 3] & 0x1F;
                let vcl = matches!(t, 1 | 5);
                let first = bytes[i + 4] & 0x80 != 0;
                let start = if i > 0 && bytes[i - 1] == 0 { i - 1 } else { i };
                if seen_vcl && (matches!(t, 6..=9) || (vcl && first)) {
                    cuts.push(start);
                    seen_vcl = false;
                }
                seen_vcl |= vcl;
                i += 3;
            } else {
                i += 1;
            }
        }
        cuts.push(bytes.len());
        cuts.windows(2).map(|w| bytes[w[0]..w[1]].to_vec()).collect()
    }

    #[test]
    fn pictures_decode_into_pool_slots() {
        let pool = Pool::new(1 << 20);
        let supply = Supply::new();
        let mut d = Decoder::open(Codec::H264, &[], Some(&supply), false).expect("open");
        let mut direct = |bytes: usize| {
            pool.resize(bytes.max(pool_slot(&pool)));
            Some(pool.acquire())
        };
        let mut frames = 0;
        let mut check = |f: Frame| {
            let (memory, offsets, strides) = f.placement().expect("in a slot");
            let (w, h) = (f.width() as usize, f.height() as usize);
            assert!(strides[0] >= w && strides[1] >= w / 2, "strides {strides:?}");
            assert!(
                offsets[2] + (h / 2 - 1) * strides[2] + w / 2 <= memory.len(),
                "planes inside the slot"
            );
            assert_eq!(f.plane(0).data.len(), (h - 1) * strides[0] + w);
            frames += 1;
        };
        for u in units() {
            assert_eq!(d.send(&u, NONE, NONE, 0, &mut direct), Ok(Sent::Taken));
            while let Ok(Received::Frame(f)) = d.receive(&mut direct) {
                check(f);
            }
        }
        d.drain().expect("drain");
        while let Ok(Received::Frame(f)) = d.receive(&mut direct) {
            check(f);
        }
        assert!(frames > 0);
        assert_eq!(d.fallbacks(), 0, "no picture in libavcodec's own memory");
        assert!(supply.live() > 0, "references held as slots");
        drop(d);
        assert_eq!(supply.live(), 0, "every slot released at close");
    }

    fn pool_slot(pool: &Pool) -> usize {
        pool.stats().slot_size as usize
    }

    #[test]
    fn a_dry_pool_falls_back_to_libavcodec_memory() {
        let supply = Supply::new();
        let mut d = Decoder::open(Codec::H264, &[], Some(&supply), false).expect("open");
        let mut none = |_: usize| None;
        let mut frames = 0;
        for u in units() {
            let _ = d.send(&u, NONE, NONE, 0, &mut none);
            while let Ok(Received::Frame(f)) = d.receive(&mut none) {
                assert!(f.placement().is_none());
                assert!(!f.plane(0).data.is_empty());
                frames += 1;
            }
        }
        assert!(frames > 0);
        assert!(d.fallbacks() > 0);
    }

    #[test]
    fn a_full_encoder_takes_the_frame_once_its_packets_are_received() {
        let mut e = Encoder::open(Codec::Aac, 48_000, 2, 0, false).expect("open");
        let frame = e.frame_size() as i64;
        let pcm = vec![0.25f32; e.frame_size() * 2];
        let mut pts = 0;
        let refused = loop {
            match e.send(&pcm, pts).expect("send") {
                Sent::Taken => pts += frame,
                s => break s,
            }
            assert!(pts < 16 * frame, "never full");
        };
        assert_eq!(refused, Sent::Full);
        let mut packets = 0;
        while let Ok(Encoded::Packet { .. }) = e.receive() {
            packets += 1;
        }
        assert!(packets > 0);
        assert_eq!(e.send(&pcm, pts), Ok(Sent::Taken), "the refused frame goes in on the resend");
        pts += frame;
        while let Ok(Encoded::Packet { .. }) = e.receive() {
            packets += 1;
        }
        e.drain().expect("drain");
        while let Ok(Encoded::Packet { .. }) = e.receive() {
            packets += 1;
        }
        // every frame and the priming one
        assert_eq!(packets, pts / frame + 1);
    }

    #[test]
    fn a_short_slot_is_reported() {
        let pool = Pool::new(1024);
        let supply = Supply::new();
        let mut d = Decoder::open(Codec::H264, &[], Some(&supply), false).expect("open");
        let mut small = |_: usize| Some(pool.acquire());
        for u in units().iter().take(3) {
            let _ = d.send(u, NONE, NONE, 0, &mut small);
            while let Ok(Received::Frame(_)) = d.receive(&mut small) {}
        }
        let (need, capacity) = supply.take_short().expect("reported");
        assert!(need > capacity && capacity == 1024);
    }
}
