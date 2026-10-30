//! Hand-written FFI to libopus 1.6.1 (RFC 6716), built static by `build.rs`, which asserts every
//! constant against the headers. The safe wrappers live in `pf-opus`.

#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int};

#[repr(C)]
pub struct OpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
pub struct OpusDecoder {
    _private: [u8; 0],
}

#[repr(C)]
pub struct OpusMSDecoder {
    _private: [u8; 0],
}

pub const OPUS_OK: c_int = 0;

pub const OPUS_APPLICATION_VOIP: c_int = 2048;
pub const OPUS_APPLICATION_AUDIO: c_int = 2049;
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;

pub const OPUS_SIGNAL_VOICE: c_int = 3001;
pub const OPUS_SIGNAL_MUSIC: c_int = 3002;

pub const OPUS_AUTO: c_int = -1000;
pub const OPUS_BITRATE_MAX: c_int = -1;

pub const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
pub const OPUS_GET_BITRATE_REQUEST: c_int = 4003;
pub const OPUS_SET_VBR_REQUEST: c_int = 4006;
pub const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
pub const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
pub const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
pub const OPUS_SET_VBR_CONSTRAINT_REQUEST: c_int = 4020;
pub const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
pub const OPUS_RESET_STATE: c_int = 4028;

unsafe extern "C" {
    pub fn opus_get_version_string() -> *const c_char;
    pub fn opus_strerror(error: c_int) -> *const c_char;

    pub fn opus_encoder_get_size(channels: c_int) -> c_int;
    pub fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    pub fn opus_encode(
        st: *mut OpusEncoder,
        pcm: *const i16,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;
    pub fn opus_encode_float(
        st: *mut OpusEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;
    /// A `SET_*` request takes one `i32`, a `GET_*` one a `*mut i32`.
    pub fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    pub fn opus_encoder_destroy(st: *mut OpusEncoder);

    pub fn opus_packet_get_nb_channels(data: *const u8) -> c_int;

    pub fn opus_decoder_get_size(channels: c_int) -> c_int;
    pub fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    pub fn opus_decode(
        st: *mut OpusDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub fn opus_decode_float(
        st: *mut OpusDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub fn opus_decoder_ctl(st: *mut OpusDecoder, request: c_int, ...) -> c_int;
    pub fn opus_decoder_destroy(st: *mut OpusDecoder);

    /// `mapping` holds `channels` bytes, the RFC 7845 section 5.1.1 table.
    pub fn opus_multistream_decoder_create(
        fs: i32,
        channels: c_int,
        streams: c_int,
        coupled_streams: c_int,
        mapping: *const u8,
        error: *mut c_int,
    ) -> *mut OpusMSDecoder;
    pub fn opus_multistream_decode(
        st: *mut OpusMSDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub fn opus_multistream_decode_float(
        st: *mut OpusMSDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub fn opus_multistream_decoder_ctl(st: *mut OpusMSDecoder, request: c_int, ...) -> c_int;
    pub fn opus_multistream_decoder_destroy(st: *mut OpusMSDecoder);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_links() {
        // SAFETY: a static NUL-terminated string.
        let v = unsafe { std::ffi::CStr::from_ptr(opus_get_version_string()) };
        let s = v.to_str().unwrap();
        assert!(s.contains("libopus"), "unexpected version string: {s:?}");
        println!("linked: {s}");
    }

    #[test]
    fn linked_library_is_the_pinned_release() {
        // SAFETY: a static NUL-terminated string.
        let v = unsafe { std::ffi::CStr::from_ptr(opus_get_version_string()) };
        assert_eq!(v.to_str().unwrap(), "libopus 1.6.1");
    }

    #[test]
    fn multistream_decoder_links() {
        // RFC 7845 section 5.1.1.2 5.1 layout, 4 streams, 2 coupled.
        let mapping = [0u8, 4, 1, 2, 3, 5];
        unsafe {
            let mut err: c_int = 0;
            let st = opus_multistream_decoder_create(48_000, 6, 4, 2, mapping.as_ptr(), &mut err);
            assert_eq!(err, OPUS_OK, "create: {}", strerror(err));
            assert!(!st.is_null());
            let mut out = vec![0i16; 960 * 6];
            let got = opus_multistream_decode(st, std::ptr::null(), 0, out.as_mut_ptr(), 960, 0);
            assert_eq!(got, 960, "{}", strerror(got));
            assert_eq!(opus_multistream_decoder_ctl(st, OPUS_RESET_STATE), OPUS_OK);
            opus_multistream_decoder_destroy(st);
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        const FS: i32 = 48_000;
        const CH: c_int = 2;
        const FRAME: c_int = 960; // 20 ms @ 48 kHz

        unsafe {
            let mut err: c_int = 0;
            let enc = opus_encoder_create(FS, CH, OPUS_APPLICATION_AUDIO, &mut err);
            assert_eq!(err, OPUS_OK, "encoder_create: {}", strerror(err));
            assert!(!enc.is_null());
            assert_eq!(opus_encoder_ctl(enc, OPUS_SET_BITRATE_REQUEST, 96_000i32), OPUS_OK);

            let mut pcm = vec![0i16; (FRAME as usize) * CH as usize];
            for i in 0..FRAME as usize {
                let t = i as f64 / FS as f64;
                let s = (0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 16384.0) as i16;
                pcm[i * 2] = s;
                pcm[i * 2 + 1] = s;
            }

            let mut packet = vec![0u8; 4000];
            let n = opus_encode(enc, pcm.as_ptr(), FRAME, packet.as_mut_ptr(), packet.len() as i32);
            assert!(n > 1, "encode returned {n} ({})", strerror(n));
            println!("encoded 20ms stereo @96k -> {n} bytes");

            let dec = opus_decoder_create(FS, CH, &mut err);
            assert_eq!(err, OPUS_OK, "decoder_create: {}", strerror(err));
            let mut out = vec![0i16; (FRAME as usize) * CH as usize];
            let got = opus_decode(dec, packet.as_ptr(), n, out.as_mut_ptr(), FRAME, 0);
            assert_eq!(got, FRAME, "decoded {got} samples/ch ({})", strerror(got));

            opus_encoder_destroy(enc);
            opus_decoder_destroy(dec);
        }
    }

    fn strerror(code: c_int) -> String {
        // SAFETY: a static NUL-terminated string for any code.
        unsafe { std::ffi::CStr::from_ptr(opus_strerror(code)) }.to_string_lossy().into_owned()
    }
}
