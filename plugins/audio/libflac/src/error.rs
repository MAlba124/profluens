use std::fmt;

use crate::decoder::DecodeError;
use crate::encoder::{EncodeError, SampleFormat};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlacError {
    Decode(DecodeError),
    NoStreamInfo,
    SlotTooSmall { capacity: usize, need: usize },
    Encode(EncodeError),
    EncoderNotStarted,
    FormatChanged { rate: u32, channels: u32, format: SampleFormat },
    MappingHeaderShort { len: usize, need: usize },
    MappingSignature,
}

impl fmt::Display for FlacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlacError::Decode(e) => write!(f, "flacdec: {e}"),
            FlacError::NoStreamInfo => f.write_str("flacdec: frame decoded without streaminfo"),
            FlacError::SlotTooSmall { capacity, need } => write!(
                f,
                "flacdec: pool slot of {capacity} bytes is smaller than one {need} byte frame"
            ),
            FlacError::Encode(e) => write!(f, "flacenc: {e}"),
            FlacError::EncoderNotStarted => f.write_str("flacenc: not started"),
            FlacError::FormatChanged { rate, channels, format } => write!(
                f,
                "flacenc: input format changed mid-stream (now {rate} Hz, {channels} ch, \
                 {format:?}) after STREAMINFO was written, unsupported"
            ),
            FlacError::MappingHeaderShort { len, need } => write!(
                f,
                "oggflac: first packet is {len} bytes, shorter than the {need}-byte \
                 FLAC-to-Ogg mapping header"
            ),
            FlacError::MappingSignature => f.write_str(
                "oggflac: first packet is not a FLAC-to-Ogg mapping header (missing \
                 0x7F \"FLAC\" signature)",
            ),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::BadMagic => f.write_str("missing or wrong fLaC marker"),
            DecodeError::BadStreamInfo => f.write_str("first metadata block is not a STREAMINFO"),
            DecodeError::UnexpectedEof => f.write_str("unexpected end of input"),
            DecodeError::TooLarge(what) => write!(f, "too large, {what}"),
            DecodeError::Memory => f.write_str("libFLAC out of memory"),
            DecodeError::Init(status) => write!(f, "libFLAC decoder init status {status}"),
            DecodeError::State(state) => write!(f, "libFLAC decoder stopped in state {state}"),
        }
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::UnsupportedParams(what) => write!(f, "unsupported parameters, {what}"),
            EncodeError::RaggedInput => {
                f.write_str("input is not a whole number of interchannel samples")
            }
            EncodeError::Memory => f.write_str("libFLAC out of memory"),
            EncodeError::Init(status) => write!(f, "libFLAC encoder init status {status}"),
            EncodeError::State(state) => write!(f, "libFLAC encoder stopped in state {state}"),
        }
    }
}

impl std::error::Error for FlacError {}
impl std::error::Error for DecodeError {}
impl std::error::Error for EncodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_element_and_carry_the_data() {
        let rows: [(FlacError, &str, &[&str]); 10] = [
            (FlacError::Decode(DecodeError::BadMagic), "flacdec: ", &["fLaC"]),
            (FlacError::Decode(DecodeError::State(7)), "flacdec: ", &["libFLAC", "state 7"]),
            (FlacError::NoStreamInfo, "flacdec: ", &["without streaminfo"]),
            (FlacError::SlotTooSmall { capacity: 8, need: 18 }, "flacdec: ", &["8 bytes", "18"]),
            (
                FlacError::Encode(EncodeError::UnsupportedParams("channels")),
                "flacenc: ",
                &["unsupported parameters", "channels"],
            ),
            (FlacError::Encode(EncodeError::Init(11)), "flacenc: ", &["init status 11"]),
            (FlacError::EncoderNotStarted, "flacenc: ", &["not started"]),
            (
                FlacError::FormatChanged { rate: 48_000, channels: 2, format: SampleFormat::S24 },
                "flacenc: ",
                &["48000 Hz", "2 ch", "S24"],
            ),
            (
                FlacError::MappingHeaderShort { len: 3, need: 9 },
                "oggflac: ",
                &["3 bytes", "9-byte"],
            ),
            (FlacError::MappingSignature, "oggflac: ", &["0x7F \"FLAC\""]),
        ];
        for (e, prefix, needles) in rows {
            let s = format!("{e}");
            assert!(s.starts_with(prefix), "{s}");
            assert!(!s.contains('\n') && !s.contains('\u{2014}'), "{s}");
            for n in needles {
                assert!(s.contains(n), "{s:?} should mention {n:?}");
            }
        }
    }
}
