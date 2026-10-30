//! MPEG audio readers. The frame header, ID3v2/ID3v1/APEv2 tags, the no-decode duration probe
//! and the seek index of a bare MP3. Decoding goes through `pf-libav`.

pub mod frame;
pub mod id3;
pub mod index;
pub mod props;

pub use props::{AudioProps, TAG_FRAME_PROBE, TagFrame, XingToc, probe_props, tag_frame};
