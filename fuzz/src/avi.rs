//! `avidemux` made as the player makes it.

// Head copied once per input.
#![allow(clippy::disallowed_methods)]

use pf_avi::AviDemux;
use pf_avi::riff::MAX_STREAMS;

use crate::rig::{Bounds, Made, Target};

pub const TARGET: Target = Target {
    make,
    bounds: Bounds {
        heap_per_byte: 6,
        heap_per_head: 6,
        heap_fixed: 256 * MAX_STREAMS + (1 << 20),
        out_per_byte: 1,
        out_per_head: 0,
    },
};

const HEAD: usize = 16 << 20;

/// The head is the file through the `movi` four-CC. Odd variants are not told the length.
fn make(file: &[u8], variant: u8) -> Option<Made> {
    let window = &file[..file.len().min(HEAD)];
    let end = window.windows(4).position(|w| w == b"movi")? + 4;
    let len = (variant & 1 == 0).then_some(file.len() as u64);
    let element = Box::new(AviDemux::new(file[..end].to_vec()).with_len(len));
    Some(Made { element, sink: AviDemux::IN, head_len: end, statics: &[], out_scale: 1 })
}
