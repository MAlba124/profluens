//! Safe owners of the libopus encoder and decoders. libopus allocates nothing on the hot path.

use std::ffi::CStr;
use std::os::raw::c_int;

use libopus_sys as sys;

use crate::config::{Application, EncoderConfig, OPUS_RATE, Signal};
use crate::error::CodecError;

const OPUS_BAD_ARG: c_int = -1;

pub struct LibopusEncoder {
    st: *mut sys::OpusEncoder,
    channels: usize,
    frame_samples: usize,
    /// Copy of a frame that is not 2-aligned, a `bytes` source can split a sample.
    aligned: Vec<i16>,
}

// SAFETY: owned solely here, used only through `&mut self`, moved between threads, never shared.
unsafe impl Send for LibopusEncoder {}

impl LibopusEncoder {
    /// 48 kHz only, 1 or 2 channels.
    #[allow(clippy::disallowed_methods)] // one-time per-stream construction
    pub fn new(cfg: EncoderConfig) -> Result<Self, CodecError> {
        if cfg.sample_rate != OPUS_RATE {
            return Err(CodecError::Rate { rate: cfg.sample_rate });
        }
        let channels = cfg.channels as usize;
        if !(1..=2).contains(&channels) {
            return Err(CodecError::Channels { channels });
        }
        let application = match cfg.application {
            Application::Voip => sys::OPUS_APPLICATION_VOIP,
            Application::Audio => sys::OPUS_APPLICATION_AUDIO,
            Application::LowDelay => sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        };

        let mut err: c_int = 0;
        // SAFETY: plain constructor, the status lands in `err`.
        let st = unsafe {
            sys::opus_encoder_create(
                cfg.sample_rate as i32,
                channels as c_int,
                application,
                &mut err,
            )
        };
        if err != sys::OPUS_OK || st.is_null() {
            return Err(CodecError::Libopus { call: "opus_encoder_create", code: err });
        }

        let frame_samples = (cfg.sample_rate as usize * cfg.frame_ms_tenths as usize) / 10_000;
        let mut me = Self { st, channels, frame_samples, aligned: Vec::new() };
        // A refused setting fails `new`, a bitrate past `i32` saturates rather than wrap negative.
        let bitrate = i32::try_from(cfg.bitrate_bps).unwrap_or(i32::MAX);
        me.ctl_set(sys::OPUS_SET_BITRATE_REQUEST, bitrate)?;
        me.ctl_set(sys::OPUS_SET_VBR_REQUEST, i32::from(cfg.vbr))?;
        me.ctl_set(sys::OPUS_SET_VBR_CONSTRAINT_REQUEST, i32::from(cfg.vbr_constrained))?;
        me.ctl_set(sys::OPUS_SET_COMPLEXITY_REQUEST, i32::from(cfg.complexity.min(10)))?;
        me.ctl_set(
            sys::OPUS_SET_SIGNAL_REQUEST,
            match cfg.signal {
                Signal::Auto => sys::OPUS_AUTO,
                Signal::Voice => sys::OPUS_SIGNAL_VOICE,
                Signal::Music => sys::OPUS_SIGNAL_MUSIC,
            },
        )?;
        Ok(me)
    }

    fn ctl_set(&mut self, request: c_int, value: i32) -> Result<(), CodecError> {
        // SAFETY: every `SET_*` request takes exactly one `i32` vararg.
        let code = unsafe { sys::opus_encoder_ctl(self.st, request, value) };
        if code != sys::OPUS_OK {
            return Err(CodecError::Libopus { call: "opus_encoder_ctl", code });
        }
        Ok(())
    }

    /// Input bytes per frame, `channels * frame_samples * 2`.
    pub fn frame_bytes(&self) -> usize {
        self.channels * self.frame_samples * 2
    }

    /// One frame of interleaved `s16` LE bytes, passed without a copy when 2-aligned. Replaces
    /// `out`.
    pub fn encode(&mut self, frame: &[u8], out: &mut Vec<u8>) -> Result<(), CodecError> {
        if frame.len() != self.frame_bytes() {
            return Err(CodecError::FrameSize { len: frame.len(), expected: self.frame_bytes() });
        }
        // Headroom over the 1275 byte frame of RFC 6716 section 3.2.1.
        const CAP: usize = 4000;
        out.clear();
        out.reserve(CAP);

        #[cfg(target_endian = "big")]
        compile_error!("libopus s16 reinterpret assumes a little-endian target");
        let pcm: *const i16 = if (frame.as_ptr() as usize).is_multiple_of(2) {
            frame.as_ptr().cast::<i16>()
        } else {
            self.aligned.clear();
            let (pairs, _) = frame.as_chunks::<2>();
            self.aligned.extend(pairs.iter().map(|c| i16::from_le_bytes(*c)));
            self.aligned.as_ptr()
        };

        // SAFETY: CAP bytes reserved, `pcm` holds `frame_samples * channels` aligned `i16`.
        let n = unsafe {
            sys::opus_encode(
                self.st,
                pcm,
                self.frame_samples as c_int,
                out.as_mut_ptr(),
                CAP as i32,
            )
        };
        if n < 0 || n as usize > CAP {
            return Err(CodecError::Libopus { call: "opus_encode", code: n });
        }
        // SAFETY: opus_encode wrote `n` bytes, checked against CAP above.
        unsafe { out.set_len(n as usize) };
        Ok(())
    }

    pub fn reset(&mut self) {
        // SAFETY: OPUS_RESET_STATE consumes no vararg.
        unsafe { sys::opus_encoder_ctl(self.st, sys::OPUS_RESET_STATE) };
    }
}

impl Drop for LibopusEncoder {
    fn drop(&mut self) {
        // SAFETY: `st` came from opus_encoder_create and is freed exactly once here.
        unsafe { sys::opus_encoder_destroy(self.st) };
    }
}

/// 120 ms, RFC 6716 section 3.2.5.
const MAX_FRAME_SAMPLES: usize = 5760;

pub const DECODE_RATES: [u32; 5] = [48_000, 24_000, 16_000, 12_000, 8_000];

type Decode<St, T> = unsafe extern "C" fn(*mut St, *const u8, i32, *mut T, c_int, c_int) -> c_int;

fn decode_with<St, T>(
    st: *mut St,
    channels: usize,
    packet: &[u8],
    pcm: &mut Vec<T>,
    decode: Decode<St, T>,
    call: &'static str,
) -> Result<usize, CodecError> {
    let Ok(len) = i32::try_from(packet.len()) else {
        return Err(CodecError::Libopus { call, code: OPUS_BAD_ARG });
    };
    pcm.clear();
    pcm.reserve(channels * MAX_FRAME_SAMPLES);
    // SAFETY: a live decoder of `channels`, `pcm` has room for `channels * MAX_FRAME_SAMPLES`.
    let got = unsafe {
        decode(st, packet.as_ptr(), len, pcm.as_mut_ptr(), MAX_FRAME_SAMPLES as c_int, 0)
    };
    if got < 0 || got as usize > MAX_FRAME_SAMPLES {
        return Err(CodecError::Libopus { call, code: got });
    }
    // SAFETY: libopus wrote `channels * got` samples, `got` checked above.
    unsafe { pcm.set_len(channels * got as usize) };
    Ok(got as usize)
}

/// The width is fixed at creation from the `OpusHead` hint or the first TOC, so libopus
/// up or down mixes a mono frame inside a stereo stream.
pub struct LibopusDecoder {
    st: *mut sys::OpusDecoder,
    channels: usize,
    rate: u32,
    hint: usize,
}

// SAFETY: as for `LibopusEncoder`.
unsafe impl Send for LibopusDecoder {}

impl Default for LibopusDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl LibopusDecoder {
    pub fn new() -> Self {
        Self::at_rate(48_000)
    }

    /// A `rate` outside [`DECODE_RATES`] fails at the first packet.
    pub fn at_rate(rate: u32) -> Self {
        Self { st: std::ptr::null_mut(), channels: 0, rate, hint: 0 }
    }

    /// A width other than the open decoder's closes it, the next packet opens at `channels`.
    pub fn set_channels(&mut self, channels: usize) {
        if !(1..=2).contains(&channels) {
            return;
        }
        if !self.st.is_null() && self.channels != channels {
            // SAFETY: `st` came from opus_decoder_create and is freed exactly once.
            unsafe { sys::opus_decoder_destroy(self.st) };
            self.st = std::ptr::null_mut();
        }
        self.hint = channels;
    }

    /// Returns the output channel count.
    pub fn decode_packet_into(
        &mut self,
        packet: &[u8],
        pcm: &mut Vec<i16>,
    ) -> Result<u8, CodecError> {
        self.open(packet)?;
        decode_with(self.st, self.channels, packet, pcm, sys::opus_decode, "opus_decode")?;
        Ok(self.channels as u8)
    }

    pub fn decode_float_into(
        &mut self,
        packet: &[u8],
        pcm: &mut Vec<f32>,
    ) -> Result<u8, CodecError> {
        self.open(packet)?;
        decode_with(
            self.st,
            self.channels,
            packet,
            pcm,
            sys::opus_decode_float,
            "opus_decode_float",
        )?;
        Ok(self.channels as u8)
    }

    fn open(&mut self, packet: &[u8]) -> Result<(), CodecError> {
        // libopus would conceal an empty packet as a 120 ms loss.
        if packet.is_empty() {
            return Err(CodecError::EmptyPacket);
        }
        if !self.st.is_null() {
            return Ok(());
        }
        let channels = if self.hint != 0 {
            self.hint as c_int
        } else {
            // SAFETY: `packet` is non-empty and only its TOC byte is read.
            let ch = unsafe { sys::opus_packet_get_nb_channels(packet.as_ptr()) };
            if ch < 1 {
                return Err(CodecError::Libopus { call: "opus_packet_get_nb_channels", code: ch });
            }
            ch
        };
        let mut err: c_int = 0;
        // SAFETY: plain constructor, libopus checks the rate.
        let st = unsafe { sys::opus_decoder_create(self.rate as i32, channels, &mut err) };
        if err != sys::OPUS_OK || st.is_null() {
            return Err(CodecError::Libopus { call: "opus_decoder_create", code: err });
        }
        self.st = st;
        self.channels = channels as usize;
        Ok(())
    }

    pub fn reset(&mut self) {
        if !self.st.is_null() {
            // SAFETY: OPUS_RESET_STATE consumes no vararg.
            unsafe { sys::opus_decoder_ctl(self.st, sys::OPUS_RESET_STATE) };
        }
    }
}

impl Drop for LibopusDecoder {
    fn drop(&mut self) {
        if !self.st.is_null() {
            // SAFETY: `st` came from opus_decoder_create and is freed exactly once.
            unsafe { sys::opus_decoder_destroy(self.st) };
        }
    }
}

/// Output in the `OpusHead`'s channel order.
pub struct LibopusMsDecoder {
    st: *mut sys::OpusMSDecoder,
    channels: usize,
}

// SAFETY: as for `LibopusEncoder`.
unsafe impl Send for LibopusMsDecoder {}

impl LibopusMsDecoder {
    pub fn new(streams: u8, coupled: u8, mapping: &[u8]) -> Result<Self, CodecError> {
        Self::at_rate(48_000, streams, coupled, mapping)
    }

    pub fn at_rate(
        rate: u32,
        streams: u8,
        coupled: u8,
        mapping: &[u8],
    ) -> Result<Self, CodecError> {
        let channels = mapping.len();
        if channels == 0 || channels > 255 {
            return Err(CodecError::Libopus {
                call: "opus_multistream_decoder_create",
                code: OPUS_BAD_ARG,
            });
        }
        let mut err: c_int = 0;
        // SAFETY: `mapping` holds `channels` bytes, libopus validates the rest into `err`.
        let st = unsafe {
            sys::opus_multistream_decoder_create(
                rate as i32,
                channels as c_int,
                c_int::from(streams),
                c_int::from(coupled),
                mapping.as_ptr(),
                &mut err,
            )
        };
        if err != sys::OPUS_OK || st.is_null() {
            return Err(CodecError::Libopus { call: "opus_multistream_decoder_create", code: err });
        }
        Ok(Self { st, channels })
    }

    pub fn decode_packet_into(
        &mut self,
        packet: &[u8],
        pcm: &mut Vec<i16>,
    ) -> Result<u8, CodecError> {
        if packet.is_empty() {
            return Err(CodecError::EmptyPacket);
        }
        decode_with(
            self.st,
            self.channels,
            packet,
            pcm,
            sys::opus_multistream_decode,
            "opus_multistream_decode",
        )?;
        Ok(self.channels as u8)
    }

    pub fn decode_float_into(
        &mut self,
        packet: &[u8],
        pcm: &mut Vec<f32>,
    ) -> Result<u8, CodecError> {
        if packet.is_empty() {
            return Err(CodecError::EmptyPacket);
        }
        let call = "opus_multistream_decode_float";
        decode_with(self.st, self.channels, packet, pcm, sys::opus_multistream_decode_float, call)?;
        Ok(self.channels as u8)
    }

    pub fn reset(&mut self) {
        // SAFETY: OPUS_RESET_STATE consumes no vararg.
        unsafe { sys::opus_multistream_decoder_ctl(self.st, sys::OPUS_RESET_STATE) };
    }
}

impl Drop for LibopusMsDecoder {
    fn drop(&mut self) {
        // SAFETY: `st` came from opus_multistream_decoder_create and is freed once.
        unsafe { sys::opus_multistream_decoder_destroy(self.st) };
    }
}

pub fn strerror(code: c_int) -> &'static str {
    // SAFETY: a static NUL-terminated string for any code.
    let msg: &'static CStr = unsafe { CStr::from_ptr(sys::opus_strerror(code)) };
    msg.to_str().unwrap_or("unknown error")
}
