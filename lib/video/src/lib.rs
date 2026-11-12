//! POD video format descriptions and typed views over the opaque buffer, plus the
//! test video elements and the shared software decoder shell ([`decoder`]).
//! TODO(milestone 5+): frame views feeding the hand-written VP8/VP9/AV1 codecs.

#![allow(dead_code)]

pub mod annexb;
pub mod bits;
pub mod color;
pub mod decoder;
pub mod fixed;
pub mod format;
pub mod frame;
pub mod geometry;
pub mod gpuframe;
pub mod h264parse;
pub mod h265parse;
pub mod keys;
pub mod parse;
pub mod prefixed;
pub mod props;
pub mod qos;
pub mod rap;
mod simd;
pub mod sink;
pub mod testsrc;

pub use format::{
    FAMILY, FIELD_FPS, FIELD_HEIGHT, FIELD_PIXFMT, FIELD_WIDTH, PixelFormat, PixelFormats,
    RAW_ANY_OFFER, VideoFormat,
};
pub use frame::{VideoFrameMut, VideoFrameRef};
pub use geometry::{
    PlaneGeometry, frame_size, plane_count, plane_geometries, plane_geometry, plane_offset,
};
pub use parse::RawVideoParse;
pub use sink::{VideoCkSink, VideoCkSinkStats, VideoRender};
pub use testsrc::{VideoTestSrc, frame_pattern_byte};

use pf_registry::Registry;
use profluens_core::element::Element;

/// Register this crate's elements for name-based construction.
pub fn register(registry: &mut Registry) {
    registry.register(VideoTestSrc::new(props::DEFAULT_FORMAT, 0, 90).desc());
    registry.register(RawVideoParse::new(props::DEFAULT_FORMAT).desc());
    registry.register(VideoCkSink::new_element().desc());
}
