//! The FLAC encoder, libFLAC at `flac -5`. The header leaves out libFLAC's VORBIS_COMMENT so
//! it is the 42 bytes [`STREAMINFO_BODY_OFFSET`] counts on.

/// Interleaved signed little-endian PCM. S24 is packed, three bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleFormat {
    S8,
    S16,
    S24,
    S32,
}

impl SampleFormat {
    pub fn bits_per_sample(self) -> u32 {
        match self {
            SampleFormat::S8 => 8,
            SampleFormat::S16 => 16,
            SampleFormat::S24 => 24,
            SampleFormat::S32 => 32,
        }
    }

    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::S8 => 1,
            SampleFormat::S16 => 2,
            SampleFormat::S24 => 3,
            SampleFormat::S32 => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    UnsupportedParams(&'static str),
    RaggedInput,
    Memory,
    Init(i32),
    State(i32),
}

/// Offset of the STREAMINFO body in the header, after `fLaC` and the block header.
pub const STREAMINFO_BODY_OFFSET: usize = 8;

#[cfg(feature = "encoder")]
pub use imp::FlacEncoder;

#[cfg(feature = "encoder")]
mod imp {
    use libflac_sys as sys;

    use super::{EncodeError, STREAMINFO_BODY_OFFSET, SampleFormat};
    use crate::decoder::StreamInfo;
    use crate::libflac::{Encoder, EncoderParams, Init};

    const STREAMINFO_BODY_LEN: usize = 34;
    /// Largest block in the streamable subset at 48 kHz and below (RFC 9639 section 7).
    const BLOCK: u32 = 4608;
    const CHUNK: usize = 4096;

    pub struct FlacEncoder {
        enc: Encoder,
        rate: u32,
        channels: u32,
        format: SampleFormat,
        block: u32,
        scratch: Vec<i32>,
        cut: Vec<usize>,
    }

    impl FlacEncoder {
        /// Patch the header's STREAMINFO with what [`finish`](Self::finish) returns.
        pub fn new(
            sample_rate: u32,
            channels: u32,
            format: SampleFormat,
        ) -> Result<(Self, Vec<u8>), EncodeError> {
            Self::make(sample_rate, channels, format, BLOCK)
        }

        /// A forward-only encoder whose header is final, sizes and total samples 0 (unknown).
        pub fn new_streaming(
            sample_rate: u32,
            channels: u32,
            format: SampleFormat,
            declared_block: u32,
        ) -> Result<(Self, Vec<u8>), EncodeError> {
            Self::make(sample_rate, channels, format, declared_block.clamp(16, BLOCK))
        }

        // One-time construction per stream.
        #[allow(clippy::disallowed_methods)]
        fn make(
            rate: u32,
            channels: u32,
            format: SampleFormat,
            block: u32,
        ) -> Result<(Self, Vec<u8>), EncodeError> {
            if !(1..=8).contains(&channels) {
                return Err(EncodeError::UnsupportedParams("channels must be 1..=8"));
            }
            if rate == 0 || rate >= 1 << 20 {
                return Err(EncodeError::UnsupportedParams("sample rate out of u20 range"));
            }
            let mut p = EncoderParams {
                rate,
                channels,
                bits: format.bits_per_sample(),
                block,
                subset: true,
            };
            // A rate or depth outside the streamable subset is still FLAC.
            let mut enc = match Encoder::new(&p) {
                Err(Init::Status(sys::FLAC__STREAM_ENCODER_INIT_STATUS_NOT_STREAMABLE)) => {
                    p.subset = false;
                    Encoder::new(&p)
                }
                other => other,
            }
            .map_err(init_error)?;
            let written = std::mem::take(&mut enc.sink().out);
            enc.sink().ends.clear();
            let header = streaminfo_only(&written).ok_or(EncodeError::State(enc.state()))?;
            let me = Self {
                enc,
                rate,
                channels,
                format,
                block,
                scratch: Vec::with_capacity(CHUNK * channels as usize),
                cut: Vec::new(),
            };
            Ok((me, header))
        }

        pub fn channels(&self) -> u32 {
            self.channels
        }

        pub fn sample_rate(&self) -> u32 {
            self.rate
        }

        pub fn format(&self) -> SampleFormat {
            self.format
        }

        pub fn max_block_size(&self) -> u32 {
            self.block
        }

        pub fn encode_interleaved(
            &mut self,
            bytes: &[u8],
            out: &mut Vec<u8>,
        ) -> Result<(), EncodeError> {
            let width = self.format.bytes_per_sample();
            let stride = width * self.channels as usize;
            if !bytes.len().is_multiple_of(stride) {
                return Err(EncodeError::RaggedInput);
            }
            self.cut.clear();
            for chunk in bytes.chunks(CHUNK * stride) {
                self.scratch.clear();
                self.scratch.extend(chunk.chunks_exact(width).map(|s| read_le(s, width)));
                let frames = (chunk.len() / stride) as u32;
                if !self.enc.process(&self.scratch, frames) {
                    return Err(state_error(self.enc.state()));
                }
                self.drain(out);
            }
            Ok(())
        }

        // A fixture path, a frame a `Vec`.
        #[allow(clippy::disallowed_methods)]
        pub fn encode_frames(
            &mut self,
            bytes: &[u8],
            frames: &mut Vec<Vec<u8>>,
        ) -> Result<(), EncodeError> {
            let mut out = Vec::new();
            self.encode_interleaved(bytes, &mut out)?;
            split(&out, &self.cut, frames);
            Ok(())
        }

        // A fixture path, a frame a `Vec`.
        #[allow(clippy::disallowed_methods)]
        pub fn finish_frames(&mut self, frames: &mut Vec<Vec<u8>>) -> [u8; STREAMINFO_BODY_LEN] {
            let mut out = Vec::new();
            let body = self.finish(&mut out);
            split(&out, &self.cut, frames);
            body
        }

        fn drain(&mut self, out: &mut Vec<u8>) {
            let at = out.len();
            let sink = self.enc.sink();
            out.extend_from_slice(&sink.out);
            self.cut.extend(sink.ends.iter().map(|e| at + e));
            sink.out.clear();
            sink.ends.clear();
        }

        /// Encode what libFLAC holds back and return the final STREAMINFO body.
        pub fn finish(&mut self, out: &mut Vec<u8>) -> [u8; STREAMINFO_BODY_LEN] {
            let ok = self.enc.finish();
            self.cut.clear();
            self.drain(out);
            let info = self.enc.sink().info.take();
            debug_assert!(ok || info.is_none());
            info.map_or([0; STREAMINFO_BODY_LEN], |i| streaminfo_body(&i))
        }
    }

    // A fixture path, a frame a `Vec`.
    #[allow(clippy::disallowed_methods)]
    fn split(out: &[u8], ends: &[usize], frames: &mut Vec<Vec<u8>>) {
        let mut from = 0;
        for &end in ends {
            frames.push(out[from..end].to_vec());
            from = end;
        }
    }

    fn read_le(s: &[u8], width: usize) -> i32 {
        let mut v = [0u8; 4];
        v[4 - width..].copy_from_slice(s);
        i32::from_le_bytes(v) >> (8 * (4 - width))
    }

    // One-time, the header of a stream.
    #[allow(clippy::disallowed_methods)]
    fn streaminfo_only(written: &[u8]) -> Option<Vec<u8>> {
        let end = STREAMINFO_BODY_OFFSET + STREAMINFO_BODY_LEN;
        let mut header = written.get(..end)?.to_vec();
        if &header[..4] != b"fLaC" || header[4] & 0x7F != 0 {
            return None;
        }
        header[4] |= 0x80;
        Some(header)
    }

    fn streaminfo_body(i: &StreamInfo) -> [u8; STREAMINFO_BODY_LEN] {
        let mut b = [0u8; STREAMINFO_BODY_LEN];
        b[0..2].copy_from_slice(&(i.min_block_size as u16).to_be_bytes());
        b[2..4].copy_from_slice(&(i.max_block_size as u16).to_be_bytes());
        b[4..7].copy_from_slice(&i.min_frame_size.to_be_bytes()[1..]);
        b[7..10].copy_from_slice(&i.max_frame_size.to_be_bytes()[1..]);
        // 20 bits rate, 3 bits channels - 1, 5 bits depth - 1, 36 bits total samples.
        let packed = (u64::from(i.sample_rate) << 44)
            | (u64::from(i.channels.saturating_sub(1)) << 41)
            | (u64::from(i.bits_per_sample.saturating_sub(1)) << 36)
            | (i.total_samples & ((1 << 36) - 1));
        b[10..18].copy_from_slice(&packed.to_be_bytes());
        b[18..34].copy_from_slice(&i.md5);
        b
    }

    fn init_error(e: Init) -> EncodeError {
        match e {
            Init::Alloc => EncodeError::Memory,
            Init::Status(s) => EncodeError::Init(s),
        }
    }

    fn state_error(s: sys::FLAC__StreamEncoderState) -> EncodeError {
        if s == sys::FLAC__STREAM_ENCODER_MEMORY_ALLOCATION_ERROR {
            EncodeError::Memory
        } else {
            EncodeError::State(s)
        }
    }

    #[cfg(test)]
    // Tests build their PCM on the heap.
    #[allow(clippy::disallowed_methods)]
    mod tests {
        use super::*;

        #[test]
        fn frames_come_out_one_a_piece_and_join_to_the_stream() {
            let pcm: Vec<u8> =
                (0..30_000i32).flat_map(|i| ((i * 7 % 2000) as i16).to_le_bytes()).collect();
            let (mut a, _) =
                FlacEncoder::new_streaming(48_000, 1, SampleFormat::S16, 1000).expect("encoder");
            let (mut b, _) =
                FlacEncoder::new_streaming(48_000, 1, SampleFormat::S16, 1000).expect("encoder");
            let (mut whole, mut frames) = (Vec::new(), Vec::new());
            for chunk in pcm.chunks(3_002) {
                a.encode_interleaved(chunk, &mut whole).expect("encode");
                b.encode_frames(chunk, &mut frames).expect("encode");
            }
            a.finish(&mut whole);
            b.finish_frames(&mut frames);
            assert_eq!(frames.len(), 30, "30000 samples in blocks of 1000");
            assert_eq!(frames.concat(), whole);
            assert!(
                frames.iter().all(|f| f[0] == 0xFF && f[1] & 0xFE == 0xF8),
                "each starts on a sync code"
            );
        }

        #[test]
        fn samples_are_read_sign_extended() {
            assert_eq!(read_le(&[0x80], 1), -128);
            assert_eq!(read_le(&[0xFF, 0x7F], 2), 32767);
            assert_eq!(read_le(&[0x00, 0x00, 0x80], 3), -(1 << 23));
            assert_eq!(read_le(&[0xFF, 0xFF, 0xFF, 0xFF], 4), -1);
        }

        #[test]
        fn the_body_packs_like_the_header_libflac_wrote() {
            let (mut enc, header) =
                FlacEncoder::new(44_100, 2, SampleFormat::S16).expect("encoder");
            assert_eq!(header.len(), 42);
            assert_eq!(header[4], 0x80, "STREAMINFO is the last block");
            let pcm: Vec<u8> =
                (0..10_000i32).flat_map(|i| ((i % 300) as i16).to_le_bytes()).collect();
            let mut out = Vec::new();
            enc.encode_interleaved(&pcm, &mut out).expect("encode");
            let body = enc.finish(&mut out);
            let si = pf_tags::flacmeta::stream_info(&[&header[..8], &body[..]].concat())
                .expect("STREAMINFO");
            assert_eq!(
                (si.sample_rate, si.channels, si.bits_per_sample, si.total_samples),
                (44_100, 2, 16, 5_000)
            );
            assert_eq!(&body[..4], &[0x12, 0x00, 0x12, 0x00], "4608 both ways");
            assert_ne!(body[18..], [0; 16], "the MD5 is filled in");
            assert_eq!(enc.encode_interleaved(&[0; 3], &mut out), Err(EncodeError::RaggedInput));
        }
    }
}
