//! Per element counters. Parks by kind, time a finished poll was still owed, and submit to done
//! per frame. Printed under the element's trace level every [`PERIOD_NS`], pure otherwise.

use crate::park::Park;
use profluens_core::log;
use profluens_core::log::{Level, Loggable};

pub const PERIOD_NS: u64 = 3_000_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meter {
    /// MFX_WRN_DEVICE_BUSY or MFX_WRN_IN_EXECUTION parks.
    pub busy: u64,
    /// Dry pool parks.
    pub pool: u64,
    /// Frames whose submit to done was measured.
    pub frames: u64,
    /// Submit to done, summed over `frames`.
    pub latency_ns: u64,
    pub latency_max_ns: u64,
    /// Time between a poll that found the front frame still running and the poll that found it
    /// done, summed.
    pub wait_ns: u64,
    wait_since: Option<u64>,
    printed: Option<u64>,
    /// `frames` at the last print, for the rate of the period.
    frames_at: u64,
}

impl Meter {
    pub fn park(&mut self, p: Park) {
        match p {
            Park::Busy => self.busy += 1,
            Park::Pool => self.pool += 1,
        }
    }

    /// A poll of the front frame, `ready` once the runtime finished it.
    pub fn polled(&mut self, ready: bool, now: u64) {
        match (ready, self.wait_since) {
            (false, None) => self.wait_since = Some(now),
            (true, Some(since)) => {
                self.wait_ns += now.saturating_sub(since);
                self.wait_since = None;
            }
            _ => {}
        }
    }

    /// A frame submitted at `submitted` is done at `now`.
    pub fn done(&mut self, submitted: u64, now: u64) {
        let ns = now.saturating_sub(submitted);
        self.frames += 1;
        self.latency_ns += ns;
        self.latency_max_ns = self.latency_max_ns.max(ns);
    }

    pub fn mean_latency_ns(&self) -> u64 {
        self.latency_ns.checked_div(self.frames).unwrap_or(0)
    }

    /// `true` once a period passed since the last print, the first call starts the first period.
    pub fn due(&mut self, now: u64) -> bool {
        match self.printed {
            None => {
                self.printed = Some(now);
                false
            }
            Some(at) if now.saturating_sub(at) >= PERIOD_NS => true,
            Some(_) => false,
        }
    }

    /// The trace line once per period, through the element's `log`.
    pub fn report(&mut self, now: u64, log: &dyn Loggable) {
        if !log.enabled(Level::Trace) || !self.due(now) {
            return;
        }
        let period = self.frames - self.frames_at;
        self.frames_at = self.frames;
        self.printed = Some(now);
        log!(
            log,
            Level::Trace,
            "stats";
            "frames={} period={period} busy={} pool={} wait_ms={} latency_ms mean={} max={}",
            self.frames,
            self.busy,
            self.pool,
            self.wait_ns / 1_000_000,
            self.mean_latency_ns() / 1_000_000,
            self.latency_max_ns / 1_000_000
        );
    }

    /// The final line at stop, at trace level.
    pub fn report_final(&self, log: &dyn Loggable) {
        if log.enabled(Level::Trace) {
            log!(
                log,
                Level::Trace,
                "stats_final";
                "frames={} busy={} pool={} wait_ms={} latency_ms mean={} max={}",
                self.frames,
                self.busy,
                self.pool,
                self.wait_ns / 1_000_000,
                self.mean_latency_ns() / 1_000_000,
                self.latency_max_ns / 1_000_000
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use profluens_core::log::{Field, Text};

    /// A log that takes every level and keeps nothing.
    struct On;

    impl Loggable for On {
        fn enabled(&self, _: Level) -> bool {
            true
        }
        fn emit(&self, _: Level, _: &'static str, _: &[Field], _: Text) {}
    }

    #[test]
    fn waits_span_the_first_miss_to_the_hit_and_latency_runs_from_the_submit() {
        let mut m = Meter::default();
        m.polled(true, 10);
        assert_eq!(m.wait_ns, 0, "a frame found done waited nothing");
        m.polled(false, 100);
        m.polled(false, 150);
        m.polled(true, 400);
        assert_eq!(m.wait_ns, 300);
        m.polled(true, 500);
        assert_eq!(m.wait_ns, 300, "no run open");
        m.done(1_000, 4_000);
        m.done(5_000, 6_000);
        assert_eq!((m.frames, m.latency_ns, m.latency_max_ns), (2, 4_000, 3_000));
        assert_eq!(m.mean_latency_ns(), 2_000);
        m.done(9, 3);
        assert_eq!(m.latency_ns, 4_000, "a clock that runs back adds nothing");
        m.park(Park::Busy);
        m.park(Park::Busy);
        m.park(Park::Pool);
        assert_eq!((m.busy, m.pool), (2, 1));
    }

    #[test]
    fn a_report_is_due_once_a_period_after_the_first_look() {
        let mut m = Meter::default();
        assert!(!m.due(5));
        assert!(!m.due(5 + PERIOD_NS - 1));
        assert!(m.due(5 + PERIOD_NS));
        assert!(m.due(5 + PERIOD_NS), "due until printed");
        m.report(5 + PERIOD_NS, &On);
        assert!(!m.due(5 + PERIOD_NS + 1));
    }
}
