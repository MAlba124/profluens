//! A call the runtime could not take yet waits parked, never on a sleep. Busy retries on a short
//! timer, a dry surface pool on the next slot recycle with a timer behind it.

use std::time::Duration;

use profluens_core::ctx::Ctx;

use crate::error::MfxError;
use crate::ffi;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Park {
    /// MFX_WRN_DEVICE_BUSY or MFX_WRN_IN_EXECUTION from a submit.
    Busy,
    /// MFX_WRN_ALLOC_TIMEOUT_EXPIRED, no free runtime surface.
    Pool,
}

pub const BUSY_WAKE: Duration = Duration::from_millis(1);
/// The runtime says to retry an allocation timeout in a few ms, a recycle wakes sooner.
pub const POOL_WAKE: Duration = Duration::from_millis(5);
/// How long a busy device or a pending operation may hold up an element before it fails.
pub const STALL_NS: u64 = 5_000_000_000;

pub fn park(ctx: &mut Ctx, p: Park) {
    match p {
        Park::Busy => ctx.wake_within(BUSY_WAKE),
        Park::Pool => {
            ctx.await_recycle();
            ctx.wake_within(POOL_WAKE);
        }
    }
}

/// The start of a run of waits, cleared by progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stall {
    since: Option<u64>,
}

impl Stall {
    /// `true` once `now` is past [`STALL_NS`] from the first wait of the run.
    pub fn tick(&mut self, now: u64) -> bool {
        let since = *self.since.get_or_insert(now);
        now.saturating_sub(since) > STALL_NS
    }

    pub fn clear(&mut self) {
        self.since = None;
    }

    pub fn waiting(&self) -> bool {
        self.since.is_some()
    }
}

/// The error a stall reports, naming the status the runtime kept returning.
pub fn stalled(status: ffi::mfxStatus, call: &'static str) -> MfxError {
    MfxError { status, call }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stall_counts_from_its_first_wait_and_progress_clears_it() {
        let mut s = Stall::default();
        assert!(!s.waiting());
        assert!(!s.tick(1_000));
        assert!(!s.tick(1_000 + STALL_NS), "at the limit is not past it");
        assert!(s.tick(1_001 + STALL_NS));
        s.clear();
        assert!(!s.tick(10 * STALL_NS), "a new run starts at its own first wait");
        assert!(s.waiting());
        assert!(!s.tick(0), "a clock that runs back never trips");
    }
}
