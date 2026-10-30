//! The seeded corruption rig behind every crate's untrusted-input gate. A case is a pure
//! function of one seed, and its printed [`Op`]s rebuild the input without it. Env vars are
//! `PF_FUZZ_ITERS`, `_SEED`, `_CASE`, `_DUMP`, `_TIMEOUT_SECS` and `_SHOW_TOLERATED`.

// Test support, built on the heap and written with std.
#![allow(clippy::disallowed_methods)]

mod chunk;
mod corpus;
mod env;
mod gate;
mod heap;
mod op;
mod sites;

#[cfg(feature = "harness")]
pub mod harness;

pub use chunk::{Chunking, Chunks};
pub use corpus::{Corpus, Input};
pub use env::{base_seed, iterations};
pub use gate::{
    Case, Summary, Violation, check_heap, check_output, check_steps, corpus_gate,
    corpus_gate_quiet, gate, heap_bounded, sweep_gate,
};
pub use heap::{CLASS_REGION_COST, Tracking, heap_during, heap_growth, heap_mark};
pub use op::{EXTREMES, Op, Site, apply, corrupt};
pub use sites::{mutate, noise, prefix_then_garbage, sweep};
