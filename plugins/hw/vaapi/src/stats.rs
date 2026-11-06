//! Counters a stage keeps while it runs, read through an `Arc` by the bench and the tests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// What a stage did, its pictures and the time its thread stood waiting for the GPU.
#[derive(Default, Debug)]
pub struct Stats {
    /// Pictures through.
    pub frames: AtomicU64,
    /// Blocking waits for the GPU.
    pub waits: AtomicU64,
    /// Time in them, ns.
    pub wait_ns: AtomicU64,
    /// Passes that found the GPU busy and asked for a timed wake instead of waiting.
    pub polls: AtomicU64,
    /// Surfaces imported from dma-bufs.
    pub imports: AtomicU64,
    /// Coded buffers an encoder made, one a picture in flight a session, reused after.
    pub coded_bufs: AtomicU64,
    /// VA surfaces a stage made, a decoder's with their twins, an encoder's input and references.
    pub surfaces: AtomicU64,
    /// Of those, the twins a decoder scales into.
    pub twins: AtomicU64,
}

/// The counters at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub frames: u64,
    pub waits: u64,
    pub wait_ns: u64,
    pub polls: u64,
    pub imports: u64,
    pub coded_bufs: u64,
    pub surfaces: u64,
    pub twins: u64,
}

/// When a wait starts, for its length alone, nothing is scheduled on it.
#[allow(clippy::disallowed_methods)]
pub fn mark() -> Instant {
    Instant::now()
}

impl Stats {
    /// A blocking wait that began at `since` ended now.
    pub fn waited(&self, since: Instant) {
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.wait_ns.fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn polled(&self) {
        self.polls.fetch_add(1, Ordering::Relaxed);
    }

    /// A picture went through.
    pub fn frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    /// A session made `surfaces` surfaces, `twins` of them twins.
    pub fn made(&self, surfaces: u32, twins: u32) {
        self.surfaces.fetch_add(u64::from(surfaces), Ordering::Relaxed);
        self.twins.fetch_add(u64::from(twins), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            frames: self.frames.load(Ordering::Relaxed),
            waits: self.waits.load(Ordering::Relaxed),
            wait_ns: self.wait_ns.load(Ordering::Relaxed),
            polls: self.polls.load(Ordering::Relaxed),
            imports: self.imports.load(Ordering::Relaxed),
            coded_bufs: self.coded_bufs.load(Ordering::Relaxed),
            surfaces: self.surfaces.load(Ordering::Relaxed),
            twins: self.twins.load(Ordering::Relaxed),
        }
    }
}
