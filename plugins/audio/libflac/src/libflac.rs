//! Safe owners of libFLAC's stream decoder and encoder. The callbacks run inside `process` on
//! the caller's thread against a heap `Feed` or `Sink` whose address is libFLAC's `client_data`.

use std::os::raw::c_void;
use std::ptr::NonNull;

use libflac_sys as sys;

use crate::decoder::StreamInfo;

#[derive(Default)]
pub(crate) struct Feed {
    pub buf: Vec<u8>,
    pub base: u64,
    pub fed: usize,
    pub limit: usize,
    pub starved: Option<sys::FLAC__StreamDecoderState>,
    /// A later write in the same call replaces an earlier one, so libFLAC's silence for a
    /// missing frame is not kept.
    pub pcm: Vec<i32>,
    pub first: u64,
    pub block: u32,
    pub channels: u32,
    pub frames: u32,
    pub errors: u32,
    pub single: bool,
    pub info: Option<StreamInfo>,
}

impl Feed {
    pub fn begin(&mut self) {
        self.starved = None;
        self.frames = 0;
        self.errors = 0;
    }

    fn halted(&self) -> bool {
        self.single && self.errors > 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Init {
    Alloc,
    Status(i32),
}

pub(crate) struct Decoder {
    raw: NonNull<sys::FLAC__StreamDecoder>,
    feed: NonNull<Feed>,
}

// SAFETY: owned solely here and used from one thread at a time, never shared.
unsafe impl Send for Decoder {}

impl Decoder {
    // One-time construction per stream.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Result<Self, Init> {
        // SAFETY: plain constructor, null on allocation failure.
        let raw = NonNull::new(unsafe { sys::FLAC__stream_decoder_new() }).ok_or(Init::Alloc)?;
        let feed =
            NonNull::from(Box::leak(Box::new(Feed { limit: usize::MAX, ..Feed::default() })));
        let me = Self { raw, feed };
        // SAFETY: fresh decoder, matching callbacks, and `Drop` frees `feed` after it.
        let status = unsafe {
            sys::FLAC__stream_decoder_init_stream(
                me.raw.as_ptr(),
                Some(read_cb),
                None,
                Some(tell_cb),
                None,
                Some(eof_cb),
                Some(write_cb),
                Some(metadata_cb),
                Some(error_cb),
                me.feed.as_ptr().cast::<c_void>(),
            )
        };
        if status != sys::FLAC__STREAM_DECODER_INIT_STATUS_OK {
            return Err(Init::Status(status));
        }
        Ok(me)
    }

    pub fn feed(&self) -> &Feed {
        // SAFETY: libFLAC touches the feed only inside `process`, which takes `&mut self`.
        unsafe { self.feed.as_ref() }
    }

    pub fn feed_mut(&mut self) -> &mut Feed {
        // SAFETY: as for `feed`, and `&mut self` makes this the only reference.
        unsafe { self.feed.as_mut() }
    }

    pub fn process(&mut self) -> bool {
        // SAFETY: live decoder, no Rust reference to the feed is held during the call.
        unsafe { sys::FLAC__stream_decoder_process_single(self.raw.as_ptr()) != 0 }
    }

    pub fn state(&self) -> sys::FLAC__StreamDecoderState {
        // SAFETY: a live decoder, a read of its state.
        unsafe { sys::FLAC__stream_decoder_get_state(self.raw.as_ptr()) }
    }

    pub fn position(&self) -> Option<u64> {
        let mut at = 0u64;
        // SAFETY: live decoder, valid out pointer, the tell callback only reads the feed.
        let ok =
            unsafe { sys::FLAC__stream_decoder_get_decode_position(self.raw.as_ptr(), &mut at) };
        (ok != 0).then_some(at)
    }

    pub fn flush(&mut self) -> bool {
        // SAFETY: a live decoder.
        unsafe { sys::FLAC__stream_decoder_flush(self.raw.as_ptr()) != 0 }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: both from `new`, freed once, decoder first since its finish may touch the feed.
        unsafe {
            sys::FLAC__stream_decoder_delete(self.raw.as_ptr());
            drop(Box::from_raw(self.feed.as_ptr()));
        }
    }
}

/// # Safety
/// `client_data` is the feed pointer given at init and no other reference to it is live.
unsafe fn feed<'a>(client_data: *mut c_void) -> &'a mut Feed {
    // SAFETY: the caller's contract.
    unsafe { &mut *client_data.cast::<Feed>() }
}

unsafe extern "C" fn read_cb(
    decoder: *const sys::FLAC__StreamDecoder,
    buffer: *mut sys::FLAC__byte,
    bytes: *mut usize,
    client_data: *mut c_void,
) -> sys::FLAC__StreamDecoderReadStatus {
    // SAFETY: libFLAC passes our feed and valid `buffer`/`bytes` pointers.
    let feed = unsafe { feed(client_data) };
    if feed.halted() {
        unsafe { *bytes = 0 };
        return sys::FLAC__STREAM_DECODER_READ_STATUS_ABORT;
    }
    let want = unsafe { *bytes };
    let end = feed.limit.min(feed.buf.len());
    let left = feed.buf.get(feed.fed..end).unwrap_or_default();
    if left.is_empty() {
        // SAFETY: a live decoder, a read of its state.
        feed.starved = Some(unsafe { sys::FLAC__stream_decoder_get_state(decoder) });
        unsafe { *bytes = 0 };
        return sys::FLAC__STREAM_DECODER_READ_STATUS_ABORT;
    }
    let n = want.min(left.len());
    // SAFETY: libFLAC's buffer holds `want` bytes, `n` of ours are copied into it.
    unsafe {
        std::ptr::copy_nonoverlapping(left.as_ptr(), buffer, n);
        *bytes = n;
    }
    feed.fed += n;
    sys::FLAC__STREAM_DECODER_READ_STATUS_CONTINUE
}

unsafe extern "C" fn tell_cb(
    _decoder: *const sys::FLAC__StreamDecoder,
    absolute_byte_offset: *mut u64,
    client_data: *mut c_void,
) -> sys::FLAC__StreamDecoderTellStatus {
    // SAFETY: libFLAC passes our feed and a valid out pointer, read shared only.
    let feed = unsafe { &*client_data.cast::<Feed>() };
    unsafe { *absolute_byte_offset = feed.base + feed.fed as u64 };
    sys::FLAC__STREAM_DECODER_TELL_STATUS_OK
}

unsafe extern "C" fn eof_cb(
    _decoder: *const sys::FLAC__StreamDecoder,
    _client_data: *mut c_void,
) -> sys::FLAC__bool {
    // The end is never said. A tail short of a frame waits for bytes that never come.
    0
}

unsafe extern "C" fn write_cb(
    _decoder: *const sys::FLAC__StreamDecoder,
    frame: *const sys::FLAC__Frame,
    buffer: *const *const i32,
    client_data: *mut c_void,
) -> sys::FLAC__StreamDecoderWriteStatus {
    // SAFETY: libFLAC passes our feed and a frame whose header it filled.
    let feed = unsafe { feed(client_data) };
    if feed.halted() {
        return sys::FLAC__STREAM_DECODER_WRITE_STATUS_ABORT;
    }
    let header = unsafe { &(*frame).header };
    let block = header.blocksize as usize;
    let nch = header.channels.min(sys::FLAC__MAX_CHANNELS) as usize;
    // SAFETY: libFLAC numbers every frame it writes by sample
    // (FLAC__FRAME_NUMBER_TYPE_SAMPLE_NUMBER).
    feed.first = unsafe { header.number.sample_number };
    feed.block = header.blocksize;
    feed.channels = nch as u32;
    feed.frames += 1;
    feed.pcm.clear();
    feed.pcm.resize(block * nch, 0);
    // SAFETY: `buffer` holds `channels` pointers, each to `blocksize` samples.
    let planes = unsafe { std::slice::from_raw_parts(buffer, nch) };
    if nch == 2 {
        let (l, r) = unsafe {
            (
                std::slice::from_raw_parts(planes[0], block),
                std::slice::from_raw_parts(planes[1], block),
            )
        };
        for (dst, (&a, &b)) in feed.pcm.as_chunks_mut::<2>().0.iter_mut().zip(l.iter().zip(r)) {
            *dst = [a, b];
        }
    } else {
        for (c, &plane) in planes.iter().enumerate() {
            let plane = unsafe { std::slice::from_raw_parts(plane, block) };
            let lane = feed.pcm.get_mut(c..).unwrap_or_default();
            for (dst, &s) in lane.iter_mut().step_by(nch).zip(plane) {
                *dst = s;
            }
        }
    }
    sys::FLAC__STREAM_DECODER_WRITE_STATUS_CONTINUE
}

unsafe extern "C" fn metadata_cb(
    _decoder: *const sys::FLAC__StreamDecoder,
    metadata: *const sys::FLAC__StreamMetadata,
    client_data: *mut c_void,
) {
    // SAFETY: our feed, and the union member is read only when the type is STREAMINFO.
    let feed = unsafe { feed(client_data) };
    let m = unsafe { &*metadata };
    if m.type_ == sys::FLAC__METADATA_TYPE_STREAMINFO {
        feed.info = Some(stream_info(&m.stream_info));
    }
}

unsafe extern "C" fn error_cb(
    _decoder: *const sys::FLAC__StreamDecoder,
    _status: sys::FLAC__StreamDecoderErrorStatus,
    client_data: *mut c_void,
) {
    // SAFETY: libFLAC passes our feed.
    let feed = unsafe { feed(client_data) };
    feed.errors += 1;
}

fn stream_info(s: &sys::FLAC__StreamMetadata_StreamInfo) -> StreamInfo {
    StreamInfo {
        min_block_size: s.min_blocksize,
        max_block_size: s.max_blocksize,
        min_frame_size: s.min_framesize,
        max_frame_size: s.max_framesize,
        sample_rate: s.sample_rate,
        channels: s.channels,
        bits_per_sample: s.bits_per_sample,
        total_samples: s.total_samples,
        md5: s.md5sum,
    }
}

#[cfg(feature = "encoder")]
#[derive(Default)]
pub(crate) struct Sink {
    pub out: Vec<u8>,
    pub info: Option<StreamInfo>,
    pub ends: Vec<usize>,
}

#[cfg(feature = "encoder")]
pub(crate) struct EncoderParams {
    pub rate: u32,
    pub channels: u32,
    pub bits: u32,
    pub block: u32,
    pub subset: bool,
}

#[cfg(feature = "encoder")]
pub(crate) struct Encoder {
    raw: NonNull<sys::FLAC__StreamEncoder>,
    sink: NonNull<Sink>,
}

// SAFETY: as for `Decoder`, owned solely here and reached only through `&mut self`.
#[cfg(feature = "encoder")]
unsafe impl Send for Encoder {}

#[cfg(feature = "encoder")]
const COMPRESSION_LEVEL: u32 = 5;

#[cfg(feature = "encoder")]
impl Encoder {
    // One-time construction per stream.
    #[allow(clippy::disallowed_methods)]
    pub fn new(p: &EncoderParams) -> Result<Self, Init> {
        // SAFETY: plain constructor, null on allocation failure.
        let raw = NonNull::new(unsafe { sys::FLAC__stream_encoder_new() }).ok_or(Init::Alloc)?;
        let sink = NonNull::from(Box::leak(Box::new(Sink::default())));
        let me = Self { raw, sink };
        let e = me.raw.as_ptr();
        // SAFETY: setters on an uninitialised encoder, matching callbacks, the sink outlives it.
        let status = unsafe {
            sys::FLAC__stream_encoder_set_streamable_subset(e, sys::FLAC__bool::from(p.subset));
            sys::FLAC__stream_encoder_set_channels(e, p.channels);
            sys::FLAC__stream_encoder_set_bits_per_sample(e, p.bits);
            sys::FLAC__stream_encoder_set_sample_rate(e, p.rate);
            sys::FLAC__stream_encoder_set_compression_level(e, COMPRESSION_LEVEL);
            sys::FLAC__stream_encoder_set_blocksize(e, p.block);
            sys::FLAC__stream_encoder_init_stream(
                e,
                Some(enc_write_cb),
                None,
                None,
                Some(enc_metadata_cb),
                me.sink.as_ptr().cast::<c_void>(),
            )
        };
        if status != sys::FLAC__STREAM_ENCODER_INIT_STATUS_OK {
            return Err(Init::Status(status));
        }
        Ok(me)
    }

    pub fn sink(&mut self) -> &mut Sink {
        // SAFETY: libFLAC touches the sink only inside calls taking `&mut self`.
        unsafe { self.sink.as_mut() }
    }

    pub fn process(&mut self, pcm: &[i32], frames: u32) -> bool {
        // SAFETY: a live encoder, `pcm` holds `frames * channels` samples.
        unsafe {
            sys::FLAC__stream_encoder_process_interleaved(self.raw.as_ptr(), pcm.as_ptr(), frames)
                != 0
        }
    }

    pub fn finish(&mut self) -> bool {
        // SAFETY: a live encoder.
        unsafe { sys::FLAC__stream_encoder_finish(self.raw.as_ptr()) != 0 }
    }

    pub fn state(&self) -> sys::FLAC__StreamEncoderState {
        // SAFETY: a live encoder, a read of its state.
        unsafe { sys::FLAC__stream_encoder_get_state(self.raw.as_ptr()) }
    }
}

#[cfg(feature = "encoder")]
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: both from `new`, freed once, encoder first since its delete may write the sink.
        unsafe {
            sys::FLAC__stream_encoder_delete(self.raw.as_ptr());
            drop(Box::from_raw(self.sink.as_ptr()));
        }
    }
}

#[cfg(feature = "encoder")]
unsafe extern "C" fn enc_write_cb(
    _encoder: *const sys::FLAC__StreamEncoder,
    buffer: *const sys::FLAC__byte,
    bytes: usize,
    samples: u32,
    _current_frame: u32,
    client_data: *mut c_void,
) -> sys::FLAC__StreamEncoderWriteStatus {
    // SAFETY: libFLAC passes our sink and `bytes` readable bytes at `buffer`.
    let sink = unsafe { &mut *client_data.cast::<Sink>() };
    let data = unsafe { std::slice::from_raw_parts(buffer, bytes) };
    sink.out.extend_from_slice(data);
    if samples > 0 {
        sink.ends.push(sink.out.len());
    }
    sys::FLAC__STREAM_ENCODER_WRITE_STATUS_OK
}

#[cfg(feature = "encoder")]
unsafe extern "C" fn enc_metadata_cb(
    _encoder: *const sys::FLAC__StreamEncoder,
    metadata: *const sys::FLAC__StreamMetadata,
    client_data: *mut c_void,
) {
    // SAFETY: libFLAC passes our sink and a STREAMINFO block.
    let sink = unsafe { &mut *client_data.cast::<Sink>() };
    let m = unsafe { &*metadata };
    if m.type_ == sys::FLAC__METADATA_TYPE_STREAMINFO {
        sink.info = Some(stream_info(&m.stream_info));
    }
}
