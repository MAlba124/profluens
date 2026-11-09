//! pf-mp4, a hand-written ISO-BMFF (ISO/IEC 14496-12) MP4 demuxer and muxer.
//! Spec editions behind the section citations are recorded in `spec/NOTES.md`.

#![deny(unsafe_code)]

pub mod boxes;
pub mod chapters;
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
pub use element::{EMITS, Mp4Demux};
pub use ilst::{MoovExtent, Mp4Props, locate_moov, parse_moov_tags, props_from_moov};
pub use mux::{Mode, Mp4Mux};
pub use reader::{
    Limits, Mp4Error, Mp4Reader, RemuxPool, ResolvedSample, Restart, Sample, SamplePayload, Tables,
    Track, TrackKind,
};

pub use pf_mkv::{ReframeError, Reframer, nal_head_from_config};

use pf_registry::{Elements, Registry};

pub static ELEMENTS: Elements = &[&element::DEMUX_DESC, &mux::DESC];

/// Registers `mp4demux` and `mp4mux`. Only `mp4mux` is constructible by name, `Mp4Demux`
/// needs the file head through `moov`.
pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_is_every_src_offer_family() {
        for o in &element::DEMUX_SRC_OFFERS {
            assert!(o.family == "bytes" || EMITS.contains(&o.family), "{}", o.family);
        }
        for (i, f) in EMITS.iter().enumerate() {
            assert!(element::DEMUX_SRC_OFFERS.iter().any(|o| o.family == *f), "{f} not offered");
            assert!(!EMITS[i + 1..].contains(f), "{f} twice");
        }
    }

    #[test]
    fn sample_entries_agree_with_the_fourcc_table() {
        // mp4a and mp4v wait on the esds, the rest pf-mp4 does not read yet.
        for &(family, cc) in &profluens_core::codec::FOURCCS {
            let read = family_for(cc);
            assert!(read == family || read == "bytes", "{family}: {read}");
        }
    }

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
