//! Hand-written AVI (RIFF) container demuxer.

#![deny(unsafe_code)]

pub mod codec;
pub mod element;
pub mod idx1;
pub mod index;
pub mod riff;

pub use codec::{EMITS, family_for, offers_for, vol_prefix};
pub use element::AviDemux;
pub use idx1::{Idx1At, Idx1Body};
pub use index::AviIndex;
pub use riff::{
    AviError, AviHeader, ChunkHead, Idx1, Idx1Entry, MediaChunk, MoviLocation, MoviWalker, Stream,
    StreamKind, first_chunk_of, parse_idx1, probe_header,
};

use pf_registry::{Elements, Registry};
use profluens_core::pipeline::Index;

pub static ELEMENTS: Elements = &[&element::DEMUX_DESC];

/// A time to byte [`Index`] from a header prefix and the raw `idx1` payload.
pub fn build_seek_index(header: &[u8], idx1_payload: &[u8], file_len: u64) -> Option<Index> {
    let (avi_header, movi) = probe_header(header).ok()?;
    let movi = movi?;
    let video = avi_header.streams.iter().find(|s| matches!(s.kind, StreamKind::Video))?;
    let dur = video.sample_duration_ns()?;
    let idx = parse_idx1(idx1_payload, movi.movi_data_start, file_len);
    let mut index = idx.build_seek_index(video.index, dur);
    index.duration =
        avi_header.duration_ns().map_or(index.duration, profluens_core::time::Timestamp);
    Some(index)
}

/// Register this crate's elements for name-based lookup.
pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_exposes_the_demuxer() {
        let mut reg = Registry::new();
        register(&mut reg);
        assert_eq!(reg.names(), vec!["avidemux"], "the demuxer descriptor is registered");
        assert!(
            reg.get("avidemux").unwrap().make_default.is_none(),
            "avidemux is not name-constructible (needs the stream header bytes)"
        );
    }
}
