//! Hand-written Matroska (RFC 9559) and EBML (RFC 8794) muxer and demuxer for profluens,
//! with no container or codec dependencies. The RFCs are checked in under `spec/`.

#![deny(unsafe_code)]

pub mod codec;
mod color_map;
pub mod config;
pub(crate) mod cued;
pub mod drain;
pub mod ebml;
pub mod element;
pub mod encoding;
pub mod error;
pub mod extras;
pub mod index;
pub mod inflate;
pub mod mux_multi;
pub mod parsed;
pub(crate) mod pull;
pub mod reader;
pub mod tags;
pub mod walk;
pub mod writer;

pub use codec::{
    EMITS, Lead, ReframeError, Reframer, config_depth, family_for, is_audio_family,
    is_video_family, lead_of, nal_head_from_config, track_family, track_offers,
};
pub use element::{MkvDemux, MkvMux};

pub fn track_colour(track: &Track) -> profluens_video::color::StreamColor {
    color_map::colour_of(track)
}
pub use encoding::{Coding, Refusal};
pub use error::{DemuxError, MkvError, MuxError, SinkPads};
pub use extras::{
    Attachment, Chapter, ChapterTitle, Chapters, Edition, Extras, MkvExtras, parse_attachments,
    parse_chapters,
};
pub use index::{CAPTION_REACH, audio_lead, cue_entries, key_track};
pub use inflate::InflateError;
pub use mux_multi::MkvMuxN;
pub use parsed::Parsed;
pub use reader::{
    Cue, CueIndex, CuedBlock, CuesAt, Frame, MatroskaReader, PTS_NONE, SeekHeadInfo, Track,
    cluster_timestamp, cued_block, cues_demand, cues_typical, parse_cue_tracks, parse_cues,
    parse_seek_head,
};
pub use writer::{
    APP_NAME, AudioConfig, CLUSTER_BLOCKS_MAX, CLUSTER_BYTES_MAX, ColourConfig,
    DEFAULT_TIMESTAMP_SCALE, MatroskaWriter, MuxOut, MuxPiece, TrackConfig, VideoConfig,
    WriteError,
};

use pf_registry::{Elements, Registry};

pub static ELEMENTS: Elements = &[&element::MUX_DESC, &mux_multi::MUXN_DESC, &element::DEMUX_DESC];

/// Register this crate's elements by name. `mkvdemux` has no default since it needs the header
/// bytes.
pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_exposes_both_elements() {
        let mut reg = Registry::new();
        register(&mut reg);
        assert_eq!(
            reg.names(),
            vec!["mkvdemux", "mkvmux", "mkvmuxn"],
            "all descriptors registered"
        );
        assert!(
            reg.get("mkvmux").unwrap().make_default.is_some(),
            "mkvmux is name-constructible (caps-driven default)"
        );
        assert!(
            reg.get("mkvmuxn").unwrap().make_default.is_some(),
            "mkvmuxn is name-constructible (caps-driven default)"
        );
        assert!(
            reg.get("mkvdemux").unwrap().make_default.is_none(),
            "mkvdemux not name-constructible"
        );
    }
}
