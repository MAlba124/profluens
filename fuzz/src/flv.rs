//! `flvdemux` made as the player makes it, from a prefix of the file.

// Head copied once per input.
#![allow(clippy::disallowed_methods)]

use pf_flv::FlvDemux;

use crate::rig::{Bounds, Made, Target};

pub const TARGET: Target = Target {
    make,
    bounds: Bounds {
        heap_per_byte: 8,
        heap_per_head: 4,
        heap_fixed: 2 << 20,
        out_per_byte: 3,
        out_per_head: 0,
    },
};

/// The player's prefix. Variant 1 is the whole file without its length, 2 a short head that may
/// cut `onMetaData`, 3 the header and the first tag, so the streams come from the metadata.
fn make(file: &[u8], variant: u8) -> Option<Made> {
    let end = match variant {
        1 => file.len(),
        2 => file.len().min(4096),
        3 => first_tag_end(file),
        _ => file.len().min(pf_flv::HEAD),
    };
    let demux = FlvDemux::new(file[..end].to_vec());
    let element = match variant {
        1 => Box::new(demux),
        _ => Box::new(demux.with_len(file.len() as u64)),
    };
    Some(Made { element, sink: FlvDemux::IN, head_len: end, statics: &[], out_scale: 1 })
}

fn first_tag_end(file: &[u8]) -> usize {
    let size = file.get(14..17).map_or(0, |s| s.iter().fold(0, |n, b| n << 8 | usize::from(*b)));
    (13 + 15 + size).min(file.len())
}
