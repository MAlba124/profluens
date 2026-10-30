use std::fmt;

/// `Rate` and `Channels` are the encoder limits, 48 kHz and 1 or 2 channels. `EmptyPacket` is
/// RFC 6716 section 3.4 R1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    Rate { rate: u32 },
    Channels { channels: usize },
    FrameSize { len: usize, expected: usize },
    EmptyPacket,
    Libopus { call: &'static str, code: i32 },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Rate { rate } => {
                write!(f, "libopus backend requires 48 kHz, got {rate}")
            }
            CodecError::Channels { channels } => {
                write!(f, "libopus backend supports 1 or 2 channels, got {channels}")
            }
            CodecError::FrameSize { len, expected } => {
                write!(f, "frame is {len} bytes, expected {expected}")
            }
            CodecError::EmptyPacket => f.write_str("empty packet, not even a TOC byte"),
            CodecError::Libopus { call, code } => {
                write!(f, "{call} failed with code {code} ({})", crate::libopus::strerror(*code))
            }
        }
    }
}

impl std::error::Error for CodecError {}

impl CodecError {
    /// A static tag for a log field.
    pub const fn reason(&self) -> &'static str {
        match self {
            CodecError::Rate { .. } => "rate",
            CodecError::Channels { .. } => "channels",
            CodecError::FrameSize { .. } => "frame_size",
            CodecError::EmptyPacket => "empty_packet",
            CodecError::Libopus { call, .. } => call,
        }
    }
}

/// The two dropped variants are warnings, the rest fatal. Only mapping families 0 and 1 decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpusError {
    PacketDropped(CodecError),
    FrameRejected(CodecError),
    InputFormatUnknown,
    UnsupportedChannelMapping { family: u8, channels: u8 },
    DecoderSetup(CodecError),
}

impl fmt::Display for OpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpusError::PacketDropped(e) => write!(f, "opusdec: dropping undecodable packet, {e}"),
            OpusError::FrameRejected(e) => {
                write!(f, "opusenc: dropping frame the encoder rejected, {e}")
            }
            OpusError::InputFormatUnknown => {
                f.write_str("opusenc: PCM arrived before the input format was known")
            }
            OpusError::UnsupportedChannelMapping { family, channels } => write!(
                f,
                "opusdec: channel mapping family {family} with {channels} channels is not \
                 supported, only families 0 and 1"
            ),
            OpusError::DecoderSetup(e) => write!(f, "opusdec: cannot build the decoder, {e}"),
        }
    }
}

impl std::error::Error for OpusError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_carry_the_variant_data() {
        let rows: [(CodecError, &[&str]); 5] = [
            (CodecError::Rate { rate: 44_100 }, &["48 kHz", "44100"]),
            (CodecError::Channels { channels: 6 }, &["got 6"]),
            (CodecError::FrameSize { len: 100, expected: 3840 }, &["100 bytes", "3840"]),
            (CodecError::EmptyPacket, &["empty packet"]),
            (CodecError::Libopus { call: "opus_decode", code: -4 }, &["opus_decode", "-4"]),
        ];
        for (e, needles) in rows {
            let s = format!("{}", OpusError::PacketDropped(e));
            assert!(s.starts_with("opusdec: "), "{s}");
            assert!(!s.contains('\n') && !s.contains('\u{2014}'), "{s}");
            for n in needles {
                assert!(s.contains(n), "{s:?} should mention {n:?}");
            }
        }
        let s = format!("{}", OpusError::InputFormatUnknown);
        assert!(s.contains("before the input format was known"), "{s}");
        let s = format!("{}", OpusError::UnsupportedChannelMapping { family: 255, channels: 6 });
        assert!(s.contains("family 255") && s.contains("6 channels"), "{s}");
        let e = CodecError::Libopus { call: "opus_multistream_decoder_create", code: -1 };
        let s = format!("{}", OpusError::DecoderSetup(e));
        assert!(s.starts_with("opusdec: cannot build") && s.contains("decoder_create"), "{s}");
    }

    #[test]
    fn libopus_code_renders_its_message() {
        // OPUS_INVALID_PACKET.
        let s = format!("{}", CodecError::Libopus { call: "opus_decode", code: -4 });
        assert!(s.contains("corrupted stream"), "{s}");
    }
}
