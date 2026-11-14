//! pf-ogg, a hand-written Ogg container (RFC 3533) reader, writer, tag scanner and elements.

#![deny(unsafe_code)]

pub mod chain;
pub mod comment;
pub mod crc;
pub mod duration;
pub mod element;
pub mod ident;
pub mod mux_multi;
pub mod page;
pub mod reader;
pub mod seek;
pub mod streams;
pub mod writer;

pub use chain::{ChainScan, Link, Links};
pub use comment::{
    PictureBlocks, parse_comment_body, parse_comment_body_lossy, parse_opus_tags,
    parse_vorbis_comments, picture_blocks,
};
pub use crc::{Crc32, crc32};
pub use duration::{
    GranuleClock, OGG_TAIL_FIRST, OGG_TAIL_LEN, OPUS_GRANULE_RATE, granule_clock, last_granule,
    opus_duration_ns, stream_duration_ns, vorbis_duration_ns,
};
pub use element::{DEFAULT_SERIAL, DemuxError, OggDemux, OggMux};
pub use ident::{
    EMITS, Mapping, OpusHead, SPEEX_MAGIC, SpeexHead, TheoraIdent, VorbisIdent, flac_frame_samples,
    opus_packet_samples, parse_opus_head, parse_speex_head, parse_theora_ident, parse_vorbis_ident,
    speex_packet_samples,
};
pub use mux_multi::{MuxError, OggMuxN};

use pf_registry::{Elements, Registry};

pub static ELEMENTS: Elements = &[&element::DEMUX_DESC, &element::MUX_DESC, &mux_multi::DESC];

/// Register this crate's elements for name-based construction.
pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}
pub use page::{
    CAPTURE_PATTERN, GRANULE_NONE, HEADER_FIXED_LEN, MAX_PAGE_SIZE, MAX_SEGMENTS, PageError,
    PageHeader, STREAM_STRUCTURE_VERSION, flags as header_flags, page_crc, write_page,
};
pub use reader::{OggReader, Packet, demux_all};
pub use streams::{HeadStream, scan_head};
pub use writer::{OggWriter, WriteError, mux_packets};
