//! `tsdemux` made as the player makes it, from the first segment of a stream.

// The head is copied out of the file, once per input.
#![allow(clippy::disallowed_methods)]

use pf_tsdemux::{AacHeads, TsDemux};

use crate::rig::{Bounds, Made, Target};

/// The gate's two demultiplexers of `DEMUX_HEAP`, the head probe's and the run's, and
/// eight times the input of heap, twice the input of output.
pub const TARGET: Target = Target {
    make,
    bounds: Bounds {
        heap_per_byte: 8,
        heap_per_head: 8,
        heap_fixed: 2 * DEMUX_HEAP,
        out_per_byte: 2,
        out_per_head: 0,
    },
};

/// A reassembly buffer per stream of 64 KiB, 32 streams, a unit of 4 MiB at most.
const DEMUX_HEAP: usize = 32 * (64 << 10) + (4 << 20) + (1 << 20);

/// A segment the player probes is a few seconds, a head that holds its PAT and PMT.
const HEAD: usize = 64 << 10;

/// The head is the first [`HEAD`] bytes, or for variant 1 the whole file. Variant 2 keeps
/// the AAC configs for a player, 3 plays program 1 and not the first one mapped.
fn make(file: &[u8], variant: u8) -> Option<Made> {
    let end = if variant == 1 { file.len() } else { file.len().min(HEAD) };
    let head = file[..end].to_vec();
    let demux = match variant {
        1 => TsDemux::new(head).with_len(file.len() as u64),
        2 => {
            let heads = AacHeads::of(&head);
            TsDemux::new(head).with_heads(heads)
        }
        3 => TsDemux::new(head).program(1),
        _ => TsDemux::new(head),
    };
    Some(Made {
        element: Box::new(demux),
        sink: TsDemux::IN,
        head_len: end,
        statics: &[],
        out_scale: 1,
    })
}
