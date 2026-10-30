use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::command::Control;
use crate::time::Timestamp;

/// How the driver may wait for a deadline on a clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nap {
    Due,
    /// Sleep this long at most, then ask again.
    Sleep(Duration),
    /// Time moves only when the clock is moved, which wakes the groups.
    Signal,
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;

    /// How to wait for `deadline`, a reading of this clock.
    fn nap(&self, deadline: Timestamp) -> Nap;

    /// Takes the handle that wakes a run's groups, kept only by a [`Nap::Signal`] clock.
    fn attach(&self, _wake: GroupWake) {}
}

/// Wakes every group of one pipeline, called by a clock after it jumps.
#[derive(Clone)]
pub struct GroupWake(Weak<Control>);

impl GroupWake {
    pub(crate) fn new(shared: &Arc<Control>) -> GroupWake {
        GroupWake(Arc::downgrade(shared))
    }

    pub fn wake(&self) -> bool {
        match self.0.upgrade() {
            Some(shared) => {
                shared.signal_all();
                true
            }
            None => false,
        }
    }
}

/// Monotonic wall clock whose `now()` is the time since it was made.
pub struct InstantClock {
    base: Instant,
}

impl InstantClock {
    // The wall clock is where real time enters a run.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self { base: Instant::now() }
    }
}

impl Default for InstantClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for InstantClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_nanos(u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }

    fn nap(&self, deadline: Timestamp) -> Nap {
        match deadline.0.saturating_sub(self.now().0) {
            0 => Nap::Due,
            left => Nap::Sleep(Duration::from_nanos(left)),
        }
    }
}

/// Test clock with virtual time that only [`advance`](Self::advance) moves, shared by clones.
#[derive(Clone)]
pub struct MockClock(Arc<MockShared>);

struct MockShared {
    now_ns: AtomicU64,
    wakes: Mutex<Vec<GroupWake>>,
}

impl MockClock {
    pub fn new() -> Self {
        Self::starting_at(Timestamp::ZERO)
    }

    // One per test clock.
    #[allow(clippy::disallowed_methods)]
    pub fn starting_at(start: Timestamp) -> Self {
        Self(Arc::new(MockShared {
            now_ns: AtomicU64::new(start.0),
            wakes: Mutex::new(Vec::new()),
        }))
    }

    /// Moves virtual time forward by `by`, then wakes every group running on this clock.
    pub fn advance(&self, by: Timestamp) {
        let add = by.nanos().unwrap_or(0);
        let mut now = self.0.now_ns.load(Ordering::SeqCst);
        while let Err(seen) = self.0.now_ns.compare_exchange(
            now,
            now.saturating_add(add).min(Timestamp::MAX.0),
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            now = seen;
        }
        // Time is published before the signal, the order a wake station asks of its signalers.
        let mut wakes = self.0.wakes.lock().unwrap_or_else(|e| e.into_inner());
        wakes.retain(GroupWake::wake);
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MockClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_nanos(self.0.now_ns.load(Ordering::SeqCst))
    }

    fn nap(&self, deadline: Timestamp) -> Nap {
        if self.now() >= deadline { Nap::Due } else { Nap::Signal }
    }

    fn attach(&self, wake: GroupWake) {
        let mut wakes = self.0.wakes.lock().unwrap_or_else(|e| e.into_inner());
        if !wakes.iter().any(|w| w.0.ptr_eq(&wake.0)) {
            wakes.push(wake);
        }
    }
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::station::Station;
    use std::sync::atomic::AtomicI64;

    fn shared_with(stations: &[Arc<Station>]) -> Arc<Control> {
        let shared = Control::new(Arc::new(AtomicI64::new(0)));
        shared.install_stations(stations.to_vec());
        shared
    }

    #[test]
    fn mock_now_reflects_advance_and_clones_share_time() {
        let a = MockClock::starting_at(Timestamp::from_secs(1));
        let b = a.clone();
        a.advance(Timestamp::from_millis(10));
        b.advance(Timestamp::from_millis(5));
        assert_eq!(a.now(), Timestamp::from_millis(1015));
        assert_eq!(b.now(), a.now());
        a.advance(Timestamp::NONE);
        assert_eq!(a.now(), Timestamp::from_millis(1015), "NONE moves nothing");
        a.advance(Timestamp::MAX);
        assert_eq!(a.now(), Timestamp::MAX, "saturates below the sentinel");
    }

    #[test]
    fn mock_nap_is_due_or_waits_for_a_signal() {
        let clock = MockClock::starting_at(Timestamp::from_millis(100));
        assert_eq!(clock.nap(Timestamp::from_millis(50)), Nap::Due);
        assert_eq!(clock.nap(Timestamp::from_millis(100)), Nap::Due, "equal is reached");
        assert_eq!(clock.nap(Timestamp::from_millis(101)), Nap::Signal);
        assert_eq!(clock.nap(Timestamp::NONE), Nap::Signal);
    }

    #[test]
    fn mock_advance_signals_every_attached_pipeline_once_each() {
        let (a, b) = (Arc::new(Station::new()), Arc::new(Station::new()));
        let (first, second) = (shared_with(&[Arc::clone(&a)]), shared_with(&[Arc::clone(&b)]));
        let clock = MockClock::new();
        clock.advance(Timestamp::from_millis(1));
        assert_eq!(a.epoch(), 0, "nobody attached, nobody signaled");
        clock.attach(GroupWake::new(&first));
        clock.attach(GroupWake::new(&first));
        clock.clone().attach(GroupWake::new(&second));
        clock.advance(Timestamp::from_millis(1));
        assert_eq!((a.epoch(), b.epoch()), (1, 1), "attaching twice is one signal");
        drop(second);
        clock.advance(Timestamp::from_millis(1));
        assert_eq!((a.epoch(), b.epoch()), (2, 1), "a pipeline that is gone is forgotten");
        assert_eq!(clock.0.wakes.lock().unwrap().len(), 1);
    }

    #[test]
    fn instant_now_is_monotonic_and_starts_near_zero() {
        let clock = InstantClock::new();
        let (a, b) = (clock.now(), clock.now());
        assert!(b >= a && a < Timestamp::from_secs(1));
    }

    #[test]
    fn instant_nap_is_the_time_left() {
        let clock = InstantClock::new();
        assert_eq!(clock.nap(Timestamp::ZERO), Nap::Due);
        match clock.nap(clock.now().saturating_add(Timestamp::from_secs(2))) {
            Nap::Sleep(left) => {
                assert!(left <= Duration::from_secs(2) && left > Duration::from_secs(1))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn clock_is_object_safe() {
        let clock: Box<dyn Clock> = Box::new(MockClock::starting_at(Timestamp::from_millis(20)));
        assert_eq!(clock.now(), Timestamp::from_millis(20));
        assert_eq!(clock.nap(Timestamp::from_millis(10)), Nap::Due);
    }
}
