#[path = "link_leak.rs"]
mod link_leak;

#[path = "batch_alloc.rs"]
mod batch_alloc;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "errors.rs"]
mod errors;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "offers.rs"]
mod offers;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "format_change.rs"]
mod format_change;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "unlinked.rs"]
mod unlinked;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "sched_override.rs"]
mod sched_override;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "seek_plan.rs"]
mod seek_plan;

// Reads core/src off disk.
#[allow(clippy::disallowed_methods)]
#[path = "budget.rs"]
mod budget;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "sized.rs"]
mod sized;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "halt.rs"]
mod halt;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "topology.rs"]
mod topology;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "stepped.rs"]
mod stepped;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "pull.rs"]
mod pull;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "solve.rs"]
mod solve;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "resolve.rs"]
mod resolve;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "interleave.rs"]
mod interleave;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "bitrate.rs"]
mod bitrate;

// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
#[path = "wake.rs"]
mod wake;
