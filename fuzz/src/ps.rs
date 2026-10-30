//! `psdemux` made as the player makes it, from the head of a program stream.

// Head and tail copied once per input.
#![allow(clippy::disallowed_methods)]

use pf_ps::{PsDemux, TAIL_LEN};

use crate::rig::{Bounds, Made, Target};

/// The demuxers of preroll, the run and the tail, the video units' reserves of 32 streams.
pub const TARGET: Target = Target {
    make,
    bounds: Bounds {
        heap_per_byte: 8,
        heap_per_head: 8,
        heap_fixed: 3 * DEMUX_HEAP + 32 * (256 << 10) + (1 << 20),
        out_per_byte: 2,
        out_per_head: 0,
    },
};

/// The carry of one demuxer, the largest PES packet.
const DEMUX_HEAP: usize = 64 << 10;

/// A DVD's first VOBU and then some.
const HEAD: usize = 256 << 10;

/// 0 is told the length, 1 not, 2 is handed the tail too, 3 reads its head from the whole file.
fn make(file: &[u8], variant: u8) -> Option<Made> {
    let end = if variant == 3 { file.len() } else { file.len().min(HEAD) };
    let demux = PsDemux::new(file[..end].to_vec());
    let len = file.len() as u64;
    let demux = match variant {
        1 => demux,
        2 => demux.with_extent(len, file[file.len().saturating_sub(TAIL_LEN)..].to_vec()),
        _ => demux.with_len(Some(len)),
    };
    Some(Made {
        element: Box::new(demux),
        sink: PsDemux::IN,
        head_len: end,
        statics: &[],
        out_scale: 1,
    })
}
