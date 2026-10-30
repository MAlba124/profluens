//! `mkvdemux` made as the player makes it.

// Head copied once per input.
#![allow(clippy::disallowed_methods)]

use pf_mkv::MkvDemux;
use pf_mkv::encoding::Step;
use pf_mkv::reader::MatroskaReader;

use crate::rig::{Bounds, Made, Target};

pub const TARGET: Target = Target {
    make,
    bounds: Bounds {
        heap_per_byte: 192,
        heap_per_head: 8,
        heap_fixed: 0,
        out_per_byte: 3,
        out_per_head: 4,
    },
};

/// The head is everything before the first Cluster, at most 8 MiB (lib/play/src/head.rs).
fn make(file: &[u8], _variant: u8) -> Option<Made> {
    let cluster = file.windows(4).position(|w| w == pf_mkv::ebml::id::CLUSTER);
    let end = cluster.unwrap_or(file.len()).min(8 << 20);
    let element = Box::new(MkvDemux::new(file[..end].to_vec()));
    let out_scale = if inflates(&file[..end]) { DEFLATE_MAX_RATIO } else { 1 };
    Some(Made { element, sink: MkvDemux::IN, head_len: end, statics: &[], out_scale })
}

/// Deflate's ceiling, a 258 octet match costs a little over 2 bits (RFC 1951 3.2.5).
const DEFLATE_MAX_RATIO: usize = 1032;

/// Whether a track of the head is zlib coded, its frames may then expand up to the ratio.
fn inflates(head: &[u8]) -> bool {
    let mut reader = MatroskaReader::new();
    let _ = reader.push(head);
    reader
        .tracks()
        .iter()
        .any(|t| t.coding.transforms() && t.coding.frames.iter().any(|s| matches!(s, Step::Zlib)))
}
