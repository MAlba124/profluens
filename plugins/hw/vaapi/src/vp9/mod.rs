//! VP9 uncompressed header (spec section 6.2) and Annex B superframe split, allocation free.

pub mod bitreader;
pub mod header;
pub mod superframe;

pub use header::{
    ColorConfig, ColorSpace, FrameType, LoopFilterParams, MAX_SEGMENTS, QuantizationParams,
    RefFrameState, SEG_LVL_MAX, SEGMENTATION_FEATURE_BITS, SEGMENTATION_FEATURE_SIGNED,
    SegmentationParams, TileInfo, Vp9FrameHeader, parse_uncompressed_header,
    parse_uncompressed_header_with_refs,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    UnexpectedEof,
    InvalidBitstream,
    Unsupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedEof => f.write_str("vp9: ran out of bits in the header"),
            Self::InvalidBitstream => f.write_str("vp9: bitstream constraint violated"),
            Self::Unsupported => f.write_str("vp9: header path not supported"),
        }
    }
}

impl std::error::Error for Error {}
