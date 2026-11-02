// Nightly allocator_api lets the scratch Arena back a Vec<T, &Arena>.
#![feature(allocator_api)]
// unsafe only in audited modules (memory, ring, publish, scratch), which opt in locally.
#![deny(unsafe_code)]

pub mod batch;
pub mod buffer;
pub mod bus;
pub mod clock;
pub mod command;
pub mod counters;
pub mod couple;
pub mod ctx;
pub mod element;
pub mod error;
pub mod event;
pub mod feedback;
pub mod fetch;
pub mod format;
pub mod id;
pub mod io;
pub mod language;
pub mod level;
pub mod log;
pub mod memory;
pub mod pipeline;
pub mod props;
pub mod publish;
pub mod pull;
pub mod rate;
pub mod ring;
pub mod sched;
pub mod scratch;
pub(crate) mod seeker;
pub(crate) mod slots;
pub(crate) mod solve;
pub(crate) mod station;
pub mod time;
pub(crate) mod timeline;
pub mod topology;
pub mod weave;
