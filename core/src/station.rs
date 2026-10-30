use std::sync::atomic::AtomicU64 as Tally;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
#[cfg(loom)]
use loom::sync::{Condvar, Mutex};

#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex};

use crate::pipeline::SchedulerStats;

/// Store to load fence for loom, which treats SeqCst accesses as acquire and release.
#[inline(always)]
fn store_before_load() {
    #[cfg(loom)]
    fence(Ordering::SeqCst);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wake {
    Signal,
    Timeout,
}

/// One group's wake station, an eventcount with one parker and any number of signalers.
pub(crate) struct Station {
    epoch: AtomicU64,
    parked: AtomicBool,
    gate: Mutex<()>,
    cond: Condvar,
    pub(crate) bell: OnceLock<Arc<dyn Fn() + Send + Sync>>,
    parks: Tally,
    tick_expiries: Tally,
    late_wakes: Tally,
    empty_wakes: Tally,
    parked_ns: Tally,
}

impl Station {
    pub(crate) fn new() -> Station {
        Station {
            epoch: AtomicU64::new(0),
            parked: AtomicBool::new(false),
            gate: Mutex::new(()),
            cond: Condvar::new(),
            bell: OnceLock::new(),
            parks: Tally::new(0),
            tick_expiries: Tally::new(0),
            late_wakes: Tally::new(0),
            empty_wakes: Tally::new(0),
            parked_ns: Tally::new(0),
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Wake the parker after the change is published.
    pub(crate) fn signal(&self) {
        // Dekker with `park`, bump then load `parked`, so the parker sees the bump or is notified.
        self.epoch.fetch_add(1, Ordering::SeqCst);
        store_before_load();
        if self.parked.load(Ordering::SeqCst) {
            if let Some(ring) = self.bell.get() {
                return ring();
            }
            let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            self.cond.notify_one();
        }
    }

    /// Sleep until a signal or `timeout` unless the epoch already moved past `seen`.
    // Times the threaded driver's park for its idle accounting.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn park(
        &self,
        seen: u64,
        timeout: Option<Duration>,
        sleep: Option<&mut dyn FnMut(Duration) -> bool>,
    ) -> Wake {
        // Dekker with `signal`, raise `parked` then load the epoch.
        self.parked.store(true, Ordering::SeqCst);
        store_before_load();
        let guard =
            self.bell.get().is_none().then(|| self.gate.lock().unwrap_or_else(|e| e.into_inner()));
        let mut wake = Wake::Signal;
        if self.epoch.load(Ordering::SeqCst) == seen {
            let at = Instant::now();
            let timed_out = match (guard, timeout) {
                (None, t) => !sleep.is_some_and(|s| s(t.unwrap_or(Duration::MAX))),
                (Some(g), Some(t)) => {
                    self.cond.wait_timeout(g, t).is_ok_and(|(_, r)| r.timed_out())
                }
                (Some(g), None) => self.cond.wait(g).is_err(),
            };
            wake = if timed_out { Wake::Timeout } else { Wake::Signal };
            self.parks.fetch_add(1, Ordering::Relaxed);
            self.parked_ns.fetch_add(at.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        self.parked.store(false, Ordering::SeqCst);
        wake
    }

    pub(crate) fn count_tick_expiry(&self) {
        self.tick_expiries.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_late_wake(&self) {
        self.late_wakes.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_empty_wake(&self) {
        self.empty_wakes.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn stats(&self) -> SchedulerStats {
        SchedulerStats {
            parks: self.parks.load(Ordering::Relaxed),
            tick_expiries: self.tick_expiries.load(Ordering::Relaxed),
            late_wakes: self.late_wakes.load(Ordering::Relaxed),
            empty_wakes: self.empty_wakes.load(Ordering::Relaxed),
            parked_ns: self.parked_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone)]
pub struct Waker(pub(crate) Arc<Station>);

impl Waker {
    pub fn wake(&self) {
        self.0.signal();
    }

    pub fn set_bell(&self, bell: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.0.bell.set(bell);
    }
}

pub(crate) struct Room(AtomicBool);

impl Room {
    pub(crate) fn new() -> Room {
        Room(AtomicBool::new(false))
    }

    pub(crate) fn raise(&self) {
        // Pairs with the fence in `try_pop`, the consumer sees the flag or the retry sees room.
        self.0.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// Consumer side after a successful `try_pop`, true when the producer must be signaled.
    pub(crate) fn take(&self) -> bool {
        self.0.load(Ordering::Relaxed) && self.0.swap(false, Ordering::SeqCst)
    }
}

pub(crate) struct Starved {
    any: AtomicBool,
    list: Mutex<Vec<Arc<Station>>>,
}

impl Starved {
    // One empty list per pool, at pool construction.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new() -> Starved {
        Starved { any: AtomicBool::new(false), list: Mutex::new(Vec::new()) }
    }

    pub(crate) fn reserve(&self, groups: usize) {
        self.list.lock().unwrap_or_else(|e| e.into_inner()).reserve(groups);
    }

    pub(crate) fn enlist(&self, station: &Arc<Station>) {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        if !list.iter().any(|s| Arc::ptr_eq(s, station)) {
            list.push(Arc::clone(station));
        }
        self.any.store(true, Ordering::SeqCst);
    }

    pub(crate) fn wanted(&self) -> bool {
        self.any.load(Ordering::Relaxed)
    }

    pub(crate) fn wake(&self) {
        if !self.any.load(Ordering::SeqCst) {
            return;
        }
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        self.any.store(false, Ordering::SeqCst);
        for station in list.drain(..) {
            station.signal();
        }
    }
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn a_park_returns_at_once_when_the_epoch_moved() {
        let st = Station::new();
        let seen = st.epoch();
        st.signal();
        let at = Instant::now();
        assert_eq!(st.park(seen, Some(Duration::from_secs(30)), None), Wake::Signal);
        assert!(at.elapsed() < Duration::from_secs(5));
        assert_eq!(st.stats().parks, 0, "it never slept");
    }

    #[test]
    fn a_park_times_out_and_is_counted() {
        let st = Station::new();
        assert_eq!(st.park(st.epoch(), Some(Duration::from_millis(20)), None), Wake::Timeout);
        let s = st.stats();
        assert_eq!((s.parks, s.tick_expiries), (1, 0), "the driver decides what a tick is");
        assert!(s.parked_ns >= 15_000_000);
    }

    #[test]
    fn a_signal_from_another_thread_ends_the_sleep() {
        let st = Arc::new(Station::new());
        let seen = st.epoch();
        let signaler = {
            let st = Arc::clone(&st);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                st.signal();
            })
        };
        assert_eq!(st.park(seen, Some(Duration::from_secs(30)), None), Wake::Signal);
        signaler.join().unwrap();
    }

    #[test]
    fn room_is_taken_once_per_raise() {
        let room = Room::new();
        assert!(!room.take());
        room.raise();
        assert!(room.take());
        assert!(!room.take());
    }

    #[test]
    fn starved_signals_each_enlisted_station_once() {
        let starved = Starved::new();
        let (a, b) = (Arc::new(Station::new()), Arc::new(Station::new()));
        starved.wake();
        assert_eq!(a.epoch(), 0, "nobody enlisted, nobody signaled");
        starved.enlist(&a);
        starved.enlist(&a);
        starved.enlist(&b);
        starved.wake();
        starved.wake();
        assert_eq!((a.epoch(), b.epoch()), (1, 1));
    }
}

// Parks below pass no timeout, so a lost wake is a deadlock loom reports.
#[cfg(loom)]
mod loom_tests {
    use super::*;
    use crate::ring::spsc;
    use loom::sync::atomic::AtomicUsize;

    fn park_until(st: &Station, mut work: impl FnMut() -> bool) {
        loop {
            let seen = st.epoch();
            if work() {
                return;
            }
            st.park(seen, None, None);
        }
    }

    #[test]
    fn loom_station_one_signaler() {
        loom::model(|| {
            let st = Arc::new(Station::new());
            let work = Arc::new(AtomicUsize::new(0));
            let signaler = {
                let (st, work) = (Arc::clone(&st), Arc::clone(&work));
                loom::thread::spawn(move || {
                    work.store(1, Ordering::Release);
                    st.signal();
                })
            };
            park_until(&st, || work.load(Ordering::Acquire) == 1);
            signaler.join().unwrap();
        });
    }

    struct ModelBell {
        count: Mutex<u64>,
        cond: Condvar,
    }

    impl ModelBell {
        fn new() -> Arc<ModelBell> {
            Arc::new(ModelBell { count: Mutex::new(0), cond: Condvar::new() })
        }

        fn ring(&self) {
            *self.count.lock().unwrap() += 1;
            self.cond.notify_one();
        }

        fn sleep(&self) -> bool {
            let mut n = self.count.lock().unwrap();
            while *n == 0 {
                n = self.cond.wait(n).unwrap();
            }
            *n = 0;
            true
        }
    }

    fn park_in_until(st: &Station, bell: &ModelBell, mut work: impl FnMut() -> bool) {
        loop {
            let seen = st.epoch();
            if work() {
                return;
            }
            st.park(seen, None, Some(&mut |_| bell.sleep()));
        }
    }

    #[test]
    fn loom_station_parked_in_the_reactor_one_signaler() {
        loom::model(|| {
            let st = Arc::new(Station::new());
            let bell = ModelBell::new();
            let ring = Arc::clone(&bell);
            Waker(Arc::clone(&st)).set_bell(Arc::new(move || ring.ring()));
            let work = Arc::new(AtomicUsize::new(0));
            let signaler = {
                let (st, work) = (Arc::clone(&st), Arc::clone(&work));
                loom::thread::spawn(move || {
                    work.store(1, Ordering::Release);
                    st.signal();
                })
            };
            park_in_until(&st, &bell, || work.load(Ordering::Acquire) == 1);
            signaler.join().unwrap();
        });
    }

    #[test]
    fn loom_station_parked_in_the_reactor_two_signalers() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let st = Arc::new(Station::new());
            let bell = ModelBell::new();
            let ring = Arc::clone(&bell);
            Waker(Arc::clone(&st)).set_bell(Arc::new(move || ring.ring()));
            let work = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
            let spawn = |first: bool| {
                let (st, work) = (Arc::clone(&st), Arc::clone(&work));
                loom::thread::spawn(move || {
                    let cell = if first { &work.0 } else { &work.1 };
                    cell.store(1, Ordering::Release);
                    st.signal();
                })
            };
            let (a, b) = (spawn(true), spawn(false));
            park_in_until(&st, &bell, || {
                work.0.load(Ordering::Acquire) == 1 && work.1.load(Ordering::Acquire) == 1
            });
            a.join().unwrap();
            b.join().unwrap();
        });
    }

    // Raise with a swap, loom lets the parker's swap miss a later plain store.
    #[test]
    fn loom_reactor_flag_taken_by_a_swap_loses_no_wake() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let st = Arc::new(Station::new());
            let shared = Arc::new((AtomicBool::new(false), AtomicUsize::new(0)));
            let spawn = || {
                let (waker, shared) = (Waker(Arc::clone(&st)), Arc::clone(&shared));
                loom::thread::spawn(move || {
                    shared.1.fetch_add(1, Ordering::AcqRel);
                    shared.0.swap(true, Ordering::AcqRel);
                    waker.wake();
                })
            };
            let helpers = [spawn(), spawn()];
            let mut taken = 0;
            loop {
                let seen = st.epoch();
                if shared.0.swap(false, Ordering::AcqRel) {
                    taken += shared.1.swap(0, Ordering::AcqRel);
                }
                if taken == 2 {
                    break;
                }
                st.park(seen, None, None);
            }
            helpers.into_iter().for_each(|h| h.join().unwrap());
        });
    }

    #[test]
    fn loom_station_two_signalers() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let st = Arc::new(Station::new());
            let work = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
            let spawn = |first: bool| {
                let (st, work) = (Arc::clone(&st), Arc::clone(&work));
                loom::thread::spawn(move || {
                    let cell = if first { &work.0 } else { &work.1 };
                    cell.store(1, Ordering::Release);
                    st.signal();
                })
            };
            let (a, b) = (spawn(true), spawn(false));
            park_until(&st, || {
                work.0.load(Ordering::Acquire) == 1 && work.1.load(Ordering::Acquire) == 1
            });
            a.join().unwrap();
            b.join().unwrap();
        });
    }

    #[test]
    fn loom_room_wakes_a_staged_producer() {
        loom::model(|| {
            let (p, c) = spsc::<usize>(1);
            p.try_push(0).expect("an empty ring has room");
            let st = Arc::new(Station::new());
            let room = Arc::new(Room::new());
            let consumer = {
                let (st, room) = (Arc::clone(&st), Arc::clone(&room));
                loom::thread::spawn(move || {
                    assert_eq!(c.try_pop(), Some(0));
                    if room.take() {
                        st.signal();
                    }
                    c
                })
            };
            let mut staged = Some(1);
            park_until(&st, || {
                let Some(v) = staged.take() else { return true };
                let Err(v) = p.try_push(v) else { return true };
                room.raise();
                match p.try_push(v) {
                    Ok(()) => true,
                    Err(v) => {
                        staged = Some(v);
                        false
                    }
                }
            });
            let c = consumer.join().unwrap();
            assert_eq!(c.try_pop(), Some(1), "the staged value went in");
        });
    }

    #[test]
    fn loom_clock_jump_wakes_a_group_idle_on_a_deadline() {
        use crate::clock::{Clock, GroupWake, MockClock, Nap};
        use crate::command::Control;
        use crate::time::Timestamp;
        loom::model(|| {
            let st = Arc::new(Station::new());
            let shared = Control::new(Arc::new(std::sync::atomic::AtomicI64::new(0)));
            shared.install_stations(vec![Arc::clone(&st)]);
            let clock = MockClock::new();
            clock.attach(GroupWake::new(&shared));
            let deadline = Timestamp::from_millis(10);
            let jumper = {
                let clock = clock.clone();
                loom::thread::spawn(move || clock.advance(deadline))
            };
            park_until(&st, || match clock.nap(deadline) {
                Nap::Due => true,
                Nap::Signal => false,
                Nap::Sleep(_) => unreachable!("a virtual clock is never slept on"),
            });
            jumper.join().unwrap();
        });
    }

    #[test]
    fn loom_starved_wakes_on_a_returned_slot() {
        loom::model(|| {
            let st = Arc::new(Station::new());
            let starved = Arc::new(Starved::new());
            let free = Arc::new(Mutex::new(0u32));
            let freer = {
                let (starved, free) = (Arc::clone(&starved), Arc::clone(&free));
                loom::thread::spawn(move || {
                    *free.lock().unwrap() += 1;
                    starved.wake();
                    let stolen = {
                        let mut slots = free.lock().unwrap();
                        let there = *slots > 0;
                        if there {
                            *slots -= 1;
                        }
                        there
                    };
                    if stolen {
                        *free.lock().unwrap() += 1;
                        starved.wake();
                    }
                })
            };
            let acquire = || {
                let mut slots = free.lock().unwrap();
                let got = *slots > 0;
                if got {
                    *slots -= 1;
                }
                got
            };
            park_until(&st, || {
                if acquire() {
                    return true;
                }
                starved.enlist(&st);
                acquire()
            });
            freer.join().unwrap();
        });
    }
}
