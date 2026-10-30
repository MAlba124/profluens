//! `psdemux` played by `pf_fuzz`, pushed and pulled, with seeks. See `src/rig.rs`.
#![no_main]
// Fuzz target, not an element. libfuzzer's macro opens files.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;

#[global_allocator]
static HEAP: pf_sim::corrupt::Tracking = pf_sim::corrupt::Tracking;

fuzz_target!(|input: &[u8]| pf_fuzz::fuzz(input, &pf_fuzz::ps::TARGET));
