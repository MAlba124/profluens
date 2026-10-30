//! `oggdemux` made as the player makes it, and as a pipeline that names it does.

// Head copied once per input.
#![allow(clippy::disallowed_methods)]

use pf_ogg::OggDemux;
use pf_ogg::duration::OGG_TAIL_LEN;

use crate::rig::{Bounds, Made, Target};

pub const TARGET: Target = Target {
    make,
    bounds: Bounds {
        heap_per_byte: 6,
        heap_per_head: 8,
        heap_fixed: 1 << 20,
        out_per_byte: 1,
        out_per_head: 0,
    },
};

/// `player.rs`, `OGG_HEAD`.
const HEAD: usize = 64 << 10;

/// 0 is the player's, 1 has no length, 2 the registry's default, 3 is handed the tail too.
fn make(file: &[u8], variant: u8) -> Option<Made> {
    if variant == 2 {
        return Some(Made {
            element: Box::new(OggDemux::new()),
            sink: OggDemux::IN,
            head_len: 0,
            statics: &[OggDemux::OUT],
            out_scale: 1,
        });
    }
    let end = file.len().min(HEAD);
    let head = file[..end].to_vec();
    let len = Some(file.len() as u64);
    let demux = match variant {
        1 => OggDemux::with_len(None),
        3 => OggDemux::with_extent(len, file[file.len().saturating_sub(OGG_TAIL_LEN)..].to_vec()),
        _ => OggDemux::with_len(len),
    };
    Some(Made {
        element: Box::new(demux.with_head(head)),
        sink: OggDemux::IN,
        head_len: end,
        statics: &[],
        out_scale: 1,
    })
}
