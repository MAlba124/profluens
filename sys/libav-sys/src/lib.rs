//! Static FFI to a subset of FFmpeg's libavcodec through the pf-owned shim `shim/pf_libav.h`.

#![allow(non_camel_case_types)]

use std::mem::{offset_of, size_of};
use std::os::raw::{c_char, c_int, c_void};

pub const FFMPEG_VERSION: &str = env!("LIBAV_SYS_FFMPEG");

pub const PF_LAV_OK: c_int = 0;
pub const PF_LAV_AGAIN: c_int = 1;
pub const PF_LAV_EOF: c_int = 2;
pub const PF_LAV_ERR_NO_CODEC: c_int = -0x5046_0001;
pub const PF_LAV_ERR_NOMEM: c_int = -0x5046_0002;
pub const PF_LAV_ERR_ARG: c_int = -0x5046_0003;

pub const PF_LAV_MAX_THREADS: c_int = 16;

pub const PF_LAV_H264: c_int = 1;
pub const PF_LAV_HEVC: c_int = 2;
pub const PF_LAV_VP8: c_int = 3;
pub const PF_LAV_VP9: c_int = 4;
pub const PF_LAV_AAC: c_int = 5;
pub const PF_LAV_AC3: c_int = 6;
pub const PF_LAV_EAC3: c_int = 7;
pub const PF_LAV_MP3: c_int = 8;
pub const PF_LAV_DTS: c_int = 9;
pub const PF_LAV_TRUEHD: c_int = 10;
pub const PF_LAV_MLP: c_int = 11;
pub const PF_LAV_MPEG2: c_int = 12;
pub const PF_LAV_VORBIS: c_int = 13;
pub const PF_LAV_MPEG4: c_int = 14;
pub const PF_LAV_ALAC: c_int = 15;
pub const PF_LAV_FLV1: c_int = 16;
pub const PF_LAV_VP6: c_int = 17;
pub const PF_LAV_VP6F: c_int = 18;
pub const PF_LAV_VP6A: c_int = 19;
pub const PF_LAV_VP7: c_int = 20;
pub const PF_LAV_THEORA: c_int = 21;
pub const PF_LAV_SPEEX: c_int = 22;

pub const PF_LAV_PIX_NONE: i32 = 0;
pub const PF_LAV_PIX_I420: i32 = 1;
pub const PF_LAV_PIX_GRAY8: i32 = 2;
pub const PF_LAV_PIX_I420_10: i32 = 3;
pub const PF_LAV_PIX_YUV: i32 = 4;
pub const PF_LAV_PIX_OTHER: i32 = 255;

pub const PF_LAV_SMP_NONE: i32 = 0;
pub const PF_LAV_SMP_U8: i32 = 1;
pub const PF_LAV_SMP_S16: i32 = 2;
pub const PF_LAV_SMP_S32: i32 = 3;
pub const PF_LAV_SMP_FLT: i32 = 4;
pub const PF_LAV_SMP_DBL: i32 = 5;
pub const PF_LAV_SMP_OTHER: i32 = 15;
pub const PF_LAV_PLANAR: i32 = 16;

pub const PF_LAV_FRAME_CORRUPT: i32 = 1;
pub const PF_LAV_FRAME_KEY: i32 = 2;
pub const PF_LAV_FRAME_MASTERING: i32 = 4;
pub const PF_LAV_FRAME_LIGHT: i32 = 8;

#[repr(C)]
pub struct pf_lav_slots {
    pub opaque: *mut c_void,
    pub acquire: Option<unsafe extern "C" fn(opaque: *mut c_void, bytes: usize) -> *mut u8>,
    pub release: Option<unsafe extern "C" fn(opaque: *mut c_void, ptr: *mut u8)>,
}

#[repr(C)]
pub struct pf_lav_frame {
    pub handle: *mut c_void,
    pub data: [*mut u8; 8],
    pub linesize: [i64; 8],
    pub in_slot: i64,
    pub pts: i64,
    pub duration: i64,
    pub width: i32,
    pub height: i32,
    pub pixfmt: i32,
    pub bit_depth: i32,
    pub chroma_shift_w: i32,
    pub chroma_shift_h: i32,
    pub primaries: i32,
    pub transfer: i32,
    pub matrix: i32,
    pub range: i32,
    pub flags: i32,
    pub mastering_primaries: [u16; 6],
    pub mastering_white: [u16; 2],
    pub mastering_max: u32,
    pub mastering_min: u32,
    pub max_cll: u16,
    pub max_fall: u16,
    pub sample_rate: i32,
    pub channels: i32,
    pub samples: i32,
    pub sample_fmt: i32,
    pub channel_mask: u64,
    pub chroma_location: i32,
    pub skip: i32,
}

const _: () = assert!(size_of::<pf_lav_frame>() == 264);
const _: () = assert!(offset_of!(pf_lav_frame, in_slot) == 136);
const _: () = assert!(offset_of!(pf_lav_frame, primaries) == 184);
const _: () = assert!(offset_of!(pf_lav_frame, mastering_primaries) == 204);
const _: () = assert!(offset_of!(pf_lav_frame, channel_mask) == 248);
const _: () = assert!(offset_of!(pf_lav_frame, chroma_location) == 256);
const _: () = assert!(offset_of!(pf_lav_frame, skip) == 260);
const _: () = assert!(size_of::<pf_lav_slots>() == 24);

#[repr(C)]
pub struct pf_lav_dec {
    _private: [u8; 0],
}

#[repr(C)]
pub struct pf_lav_enc {
    _private: [u8; 0],
}

unsafe extern "C" {
    pub fn pf_lav_open(
        out: *mut *mut pf_lav_dec,
        codec: c_int,
        extradata: *const u8,
        extradata_len: usize,
        slots: *const pf_lav_slots,
        parse: c_int,
        threads: c_int,
    ) -> c_int;
    pub fn pf_lav_close(d: *mut pf_lav_dec);
    pub fn pf_lav_send(
        d: *mut pf_lav_dec,
        data: *const u8,
        len: usize,
        pts: i64,
        dts: i64,
        duration: i64,
    ) -> c_int;
    pub fn pf_lav_drain(d: *mut pf_lav_dec) -> c_int;
    pub fn pf_lav_receive(d: *mut pf_lav_dec, f: *mut pf_lav_frame) -> c_int;
    pub fn pf_lav_frame_free(handle: *mut c_void);
    pub fn pf_lav_flush(d: *mut pf_lav_dec);
    pub fn pf_lav_parse(
        d: *mut pf_lav_dec,
        input: *const u8,
        len: usize,
        pts: i64,
        out: *mut *const u8,
        out_len: *mut usize,
        out_pts: *mut i64,
    ) -> c_int;
    pub fn pf_lav_fallbacks(d: *const pf_lav_dec) -> u64;
    pub fn pf_lav_skip_nonref(d: *mut pf_lav_dec, on: c_int);
    pub fn pf_lav_delay(d: *const pf_lav_dec) -> c_int;
    pub fn pf_lav_threads(d: *const pf_lav_dec) -> c_int;
    pub fn pf_lav_enc_open(
        out: *mut *mut pf_lav_enc,
        codec: c_int,
        sample_rate: c_int,
        channels: c_int,
        bit_rate: i64,
        fast: c_int,
    ) -> c_int;
    pub fn pf_lav_enc_close(e: *mut pf_lav_enc);
    pub fn pf_lav_enc_frame_size(e: *const pf_lav_enc) -> c_int;
    pub fn pf_lav_enc_delay(e: *const pf_lav_enc) -> c_int;
    pub fn pf_lav_enc_config(e: *const pf_lav_enc, len: *mut usize) -> *const u8;
    pub fn pf_lav_enc_send(e: *mut pf_lav_enc, pcm: *const f32, samples: c_int, pts: i64) -> c_int;
    pub fn pf_lav_enc_drain(e: *mut pf_lav_enc) -> c_int;
    pub fn pf_lav_enc_receive(
        e: *mut pf_lav_enc,
        data: *mut *const u8,
        len: *mut usize,
        pts: *mut i64,
        duration: *mut i64,
    ) -> c_int;
    pub fn pf_lav_strerror(code: c_int, buf: *mut c_char, len: usize);
    pub fn pf_lav_version() -> *const c_char;
    pub fn pf_lav_frame_size() -> usize;
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_layout_agrees_with_the_shim() {
        // SAFETY: a pure function of the shim.
        assert_eq!(unsafe { pf_lav_frame_size() }, size_of::<pf_lav_frame>());
        // SAFETY: a static NUL-terminated string.
        let v = unsafe { std::ffi::CStr::from_ptr(pf_lav_version()) };
        assert_eq!(v.to_str().ok(), Some(FFMPEG_VERSION));
    }

    #[test]
    fn every_codec_opens_single_threaded() {
        for codec in [
            PF_LAV_H264,
            PF_LAV_HEVC,
            PF_LAV_VP8,
            PF_LAV_VP9,
            PF_LAV_AAC,
            PF_LAV_AC3,
            PF_LAV_EAC3,
            PF_LAV_MP3,
            PF_LAV_DTS,
            PF_LAV_TRUEHD,
            PF_LAV_MLP,
            PF_LAV_MPEG2,
            PF_LAV_MPEG4,
            PF_LAV_FLV1,
            PF_LAV_VP6,
            PF_LAV_VP6F,
            PF_LAV_VP6A,
            PF_LAV_VP7,
        ] {
            let mut d = std::ptr::null_mut();
            // SAFETY: an out pointer and no extradata or slots.
            let rc =
                unsafe { pf_lav_open(&mut d, codec, std::ptr::null(), 0, std::ptr::null(), 0, 1) };
            assert_eq!(rc, PF_LAV_OK, "codec {codec}");
            // SAFETY: opened above, closed once.
            unsafe { pf_lav_close(d) };
        }
        let mut d = std::ptr::null_mut();
        let rc = unsafe { pf_lav_open(&mut d, 99, std::ptr::null(), 0, std::ptr::null(), 0, 1) };
        assert_eq!(rc, PF_LAV_ERR_NO_CODEC);
        assert!(d.is_null());
    }

    /// Both read their stream headers from extradata and refuse to open without them.
    #[test]
    fn theora_and_speex_are_built_and_open_on_their_headers() {
        for codec in [PF_LAV_THEORA, PF_LAV_SPEEX] {
            let mut d = std::ptr::null_mut();
            // SAFETY: an out pointer and no extradata or slots.
            let rc =
                unsafe { pf_lav_open(&mut d, codec, std::ptr::null(), 0, std::ptr::null(), 0, 1) };
            assert!(rc < 0 && rc != PF_LAV_ERR_NO_CODEC, "codec {codec}, {rc}");
            assert!(d.is_null());
        }
        // Speex manual Table 7.1, wideband mono at 16 kHz, 320 samples a frame.
        let mut head = [0u8; 80];
        head[..8].copy_from_slice(b"Speex   ");
        for (at, v) in [(32, 80), (36, 16_000), (40, 1), (44, 4), (48, 1), (56, 320), (64, 1)] {
            head[at..at + 4].copy_from_slice(&i32::to_le_bytes(v));
        }
        let mut d = std::ptr::null_mut();
        // SAFETY: an out pointer, the header outlives the call, closed below.
        let rc = unsafe {
            pf_lav_open(&mut d, PF_LAV_SPEEX, head.as_ptr(), head.len(), std::ptr::null(), 0, 1)
        };
        assert_eq!(rc, PF_LAV_OK);
        // SAFETY: opened above, closed once.
        unsafe { pf_lav_close(d) };
    }

    #[test]
    fn aac_encodes_a_tone_the_decoder_plays_back() {
        for fast in [0, 1] {
            aac_round_trip(fast);
        }
    }

    fn aac_round_trip(fast: c_int) {
        const RATE: i32 = 48_000;
        let mut e = std::ptr::null_mut();
        // SAFETY: an out pointer, closed at the end.
        assert_eq!(
            unsafe { pf_lav_enc_open(&mut e, PF_LAV_AAC, RATE, 2, 128_000, fast) },
            PF_LAV_OK
        );
        // SAFETY: opened above.
        let (n, delay) = unsafe { (pf_lav_enc_frame_size(e), pf_lav_enc_delay(e)) };
        assert_eq!((n, delay), (1024, 1024));
        let mut len = 0;
        // SAFETY: valid until close, read before it.
        let asc =
            unsafe { std::slice::from_raw_parts(pf_lav_enc_config(e, &mut len), len) }.to_vec();
        assert_eq!(asc, [0x11, 0x90, 0x56, 0xE5, 0x00]);
        let tone: Vec<f32> = (0..RATE as usize)
            .flat_map(|i| {
                let v = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / RATE as f32).sin();
                [v, v]
            })
            .collect();
        let mut packets: Vec<(i64, Vec<u8>)> = Vec::new();
        let take = |e: *mut pf_lav_enc, packets: &mut Vec<(i64, Vec<u8>)>| loop {
            let (mut data, mut len, mut pts, mut dur) = (std::ptr::null(), 0, 0, 0);
            // SAFETY: out pointers, the packet copied before the next call.
            let rc = unsafe { pf_lav_enc_receive(e, &mut data, &mut len, &mut pts, &mut dur) };
            if rc != PF_LAV_OK {
                return rc;
            }
            assert!(dur > 0 && dur <= 1024);
            // SAFETY: valid until the next receive.
            packets.push((pts, unsafe { std::slice::from_raw_parts(data, len) }.to_vec()));
        };
        for (k, frame) in tone.chunks(2 * n as usize).enumerate() {
            let samples = (frame.len() / 2) as c_int;
            // SAFETY: `samples` frames of two channels.
            let rc =
                unsafe { pf_lav_enc_send(e, frame.as_ptr(), samples, k as i64 * i64::from(n)) };
            assert_eq!(rc, PF_LAV_OK);
            assert_eq!(take(e, &mut packets), PF_LAV_AGAIN);
        }
        assert_eq!(unsafe { pf_lav_enc_drain(e) }, PF_LAV_OK);
        assert_eq!(take(e, &mut packets), PF_LAV_EOF);
        // SAFETY: opened above, closed once.
        unsafe { pf_lav_enc_close(e) };
        assert_eq!(packets[0].0, -1024, "the priming packet first");
        assert!(packets.windows(2).all(|w| w[1].0 - w[0].0 == 1024));

        let mut d = std::ptr::null_mut();
        // SAFETY: the encoder's AudioSpecificConfig as extradata.
        let rc = unsafe {
            pf_lav_open(&mut d, PF_LAV_AAC, asc.as_ptr(), asc.len(), std::ptr::null(), 0, 1)
        };
        assert_eq!(rc, PF_LAV_OK);
        let mut left = Vec::new();
        for (pts, p) in &packets {
            // SAFETY: a whole packet.
            assert_eq!(unsafe { pf_lav_send(d, p.as_ptr(), p.len(), *pts, *pts, 1024) }, PF_LAV_OK);
            // SAFETY: a zeroed frame the shim fills, freed through its handle.
            let mut f: pf_lav_frame = unsafe { std::mem::zeroed() };
            while unsafe { pf_lav_receive(d, &mut f) } == PF_LAV_OK {
                assert_eq!(f.sample_fmt, PF_LAV_SMP_FLT | PF_LAV_PLANAR);
                // SAFETY: plane 0 holds `samples` floats.
                let plane = unsafe {
                    std::slice::from_raw_parts(f.data[0] as *const f32, f.samples as usize)
                };
                left.extend_from_slice(plane);
                // SAFETY: the frame's own handle, freed once.
                unsafe { pf_lav_frame_free(f.handle) };
            }
        }
        // SAFETY: opened above, closed once.
        unsafe { pf_lav_close(d) };
        let mid = &left[2048..left.len() - 2048];
        let rms = (mid.iter().map(|v| v * v).sum::<f32>() / mid.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.03, "fast {fast} rms {rms}");
        let crossings = mid.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let per_s = crossings as f32 * RATE as f32 / mid.len() as f32;
        assert!((per_s - 2000.0).abs() < 20.0, "fast {fast}, {per_s} crossings a second");
    }

    #[test]
    fn the_encoder_refuses_what_it_cannot_make() {
        let mut e = std::ptr::null_mut();
        // SAFETY: an out pointer, nothing opened on refusal.
        assert_eq!(unsafe { pf_lav_enc_open(&mut e, PF_LAV_AAC, 48_000, 9, 0, 0) }, PF_LAV_ERR_ARG);
        assert_eq!(
            unsafe { pf_lav_enc_open(&mut e, PF_LAV_AC3, 48_000, 2, 0, 0) },
            PF_LAV_ERR_NO_CODEC
        );
        assert_eq!(unsafe { pf_lav_enc_open(&mut e, 99, 48_000, 2, 0, 0) }, PF_LAV_ERR_NO_CODEC);
        assert!(e.is_null());
    }
}
