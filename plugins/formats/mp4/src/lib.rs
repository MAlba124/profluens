//! pf-mp4, a hand-written ISO-BMFF (ISO/IEC 14496-12) MP4 demuxer and muxer.
//! Spec editions behind the section citations are recorded in `spec/NOTES.md`.

#![deny(unsafe_code)]

pub mod boxes;
pub mod cmaf;
pub mod codec;
pub mod edit;
pub mod element;
pub mod es;
pub mod frag;
pub mod ilst;
pub mod mux;
pub(crate) mod pull;
pub mod reader;
pub mod writer;

pub use codec::{SampleEntry, family_for};
pub use element::Mp4Demux;
pub use ilst::{MoovExtent, Mp4Props, locate_moov, parse_moov_tags, props_from_moov};
pub use mux::{Mode, Mp4Mux};
pub use reader::{
    Limits, Mp4Error, Mp4Reader, RemuxPool, ResolvedSample, Restart, Sample, SamplePayload, Tables,
    Track, TrackKind,
};

pub use pf_mkv::{ReframeError, Reframer, nal_head_from_config};

use pf_registry::Registry;
use profluens_core::element::Element;

/// Registers `mp4demux` and `mp4mux`. Only `mp4mux` is constructible by name, `Mp4Demux`
/// needs the file head through `moov`.
// Cold path, once at registration.
#[allow(clippy::disallowed_methods)]
pub fn register(registry: &mut Registry) {
    registry.register(Mp4Demux::new(Vec::new()).desc());
    registry.register(Mp4Mux::new(Mode::Progressive).desc());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_exposes_the_demuxer_and_the_muxer() {
        let mut reg = Registry::new();
        register(&mut reg);
        assert_eq!(reg.names(), vec!["mp4demux", "mp4mux"], "both descriptors registered");
        assert!(
            reg.get("mp4demux").unwrap().make_default.is_none(),
            "mp4demux not name-constructible"
        );
        assert!(reg.get("mp4mux").unwrap().make_default.is_some(), "mp4mux name-constructible");
    }
}
