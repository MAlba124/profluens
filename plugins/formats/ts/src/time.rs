//! Unwraps 33-bit PTS, DTS and PCR base (H.222.0 2.4.3.5, 2.4.3.7) to the congruent point
//! nearest the last one. Feed one unwrapper per program so all timestamps share an epoch.

pub const TS_MODULUS: u64 = 1 << 33;
pub const PCR_MODULUS: u64 = TS_MODULUS * 300;
pub const TS_HZ: u64 = 90_000;
pub const PCR_HZ: u64 = 27_000_000;
/// A policy value, H.222.0 2.7.4 only bounds spacing within one stream (0.7 s).
pub const DEFAULT_MAX_STEP: u64 = 10 * TS_HZ;

#[inline]
pub fn pcr_to_90k(pcr: u64) -> u64 {
    pcr / 300
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    First,
    /// Not behind the previous value and in the same period.
    Forward,
    Backward,
    /// Ahead of the previous value across the modulus.
    Rollover,
    Discontinuity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unwrapped {
    /// Congruent to the raw value. The first maps to itself, so an early step back goes negative.
    pub ticks: i64,
    pub step: Step,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unwrapper {
    modulus: u64,
    max_step: u64,
    last: i64,
    primed: bool,
}

impl Unwrapper {
    /// `modulus` is [`TS_MODULUS`] or [`PCR_MODULUS`]. Both arguments are clamped to sane ranges.
    pub fn new(modulus: u64, max_step: u64) -> Self {
        let modulus = modulus.clamp(2, 1 << 48);
        Unwrapper { modulus, max_step: max_step.min(modulus / 2), last: 0, primed: false }
    }

    pub fn for_timestamps() -> Self {
        Self::new(TS_MODULUS, DEFAULT_MAX_STEP)
    }

    pub fn reset(&mut self) {
        self.primed = false;
        self.last = 0;
    }

    /// Treats `ticks` as the last unwrapped value, as after a seek to a known resume point.
    pub fn prime(&mut self, ticks: i64) {
        self.primed = true;
        self.last = ticks;
    }

    pub fn last(&self) -> Option<i64> {
        self.primed.then_some(self.last)
    }

    pub fn unwrap(&mut self, raw: u64) -> Unwrapped {
        let m = self.modulus;
        let raw = raw % m;
        if !self.primed {
            self.primed = true;
            self.last = raw as i64;
            return Unwrapped { ticks: self.last, step: Step::First };
        }
        let phase = self.last.rem_euclid(m as i64) as u64;
        // Distance going forward from the previous phase, in 0..m.
        let ahead = (raw + m - phase) % m;
        let (delta, step) = if ahead <= m / 2 {
            let step = if ahead > self.max_step {
                Step::Discontinuity
            } else if phase + ahead >= m {
                Step::Rollover
            } else {
                Step::Forward
            };
            (ahead as i64, step)
        } else {
            let behind = m - ahead;
            let step = if behind > self.max_step { Step::Discontinuity } else { Step::Backward };
            (-(behind as i64), step)
        };
        match self.last.checked_add(delta) {
            Some(ticks) => {
                self.last = ticks;
                Unwrapped { ticks, step }
            }
            // Only reachable through a long run of hostile half-modulus jumps. Start over.
            None => {
                self.last = raw as i64;
                Unwrapped { ticks: self.last, step: Step::Discontinuity }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_and_rollover() {
        let mut u = Unwrapper::for_timestamps();
        let start = TS_MODULUS - 9000;
        assert_eq!(u.unwrap(start), Unwrapped { ticks: start as i64, step: Step::First });
        assert_eq!(u.unwrap(start + 3000).step, Step::Forward);
        assert_eq!(u.unwrap(start + 6000).ticks, start as i64 + 6000);
        let r = u.unwrap(0);
        assert_eq!(r, Unwrapped { ticks: TS_MODULUS as i64, step: Step::Rollover });
        let r = u.unwrap(3000);
        assert_eq!(r, Unwrapped { ticks: TS_MODULUS as i64 + 3000, step: Step::Forward });
        assert_eq!(u.last(), Some(TS_MODULUS as i64 + 3000));
    }

    #[test]
    fn several_rollovers_stay_monotonic() {
        let mut u = Unwrapper::for_timestamps();
        let step = 3 * TS_HZ;
        let mut raw = 0u64;
        let mut prev = u.unwrap(raw).ticks;
        let mut rollovers = 0;
        for _ in 0..(3 * TS_MODULUS / step) {
            raw = (raw + step) % TS_MODULUS;
            let r = u.unwrap(raw);
            assert_eq!(r.ticks - prev, step as i64);
            assert_ne!(r.step, Step::Discontinuity);
            if r.step == Step::Rollover {
                rollovers += 1;
            }
            prev = r.ticks;
        }
        assert_eq!(rollovers, 2);
    }

    #[test]
    fn b_frame_reordering_is_not_a_rollover() {
        let pts = [138_000u64, 156_000, 144_000, 150_000, 174_000, 162_000, 168_000];
        let mut u = Unwrapper::for_timestamps();
        for (i, &p) in pts.iter().enumerate() {
            let r = u.unwrap(p);
            assert_eq!(r.ticks, p as i64);
            let expect = if i == 0 {
                Step::First
            } else if p < pts[i - 1] {
                Step::Backward
            } else {
                Step::Forward
            };
            assert_eq!(r.step, expect);
        }
    }

    #[test]
    fn reordering_across_the_modulus() {
        let mut u = Unwrapper::for_timestamps();
        let base = TS_MODULUS as i64;
        assert_eq!(u.unwrap(TS_MODULUS - 6000).step, Step::First);
        assert_eq!(u.unwrap(6000), Unwrapped { ticks: base + 6000, step: Step::Rollover });
        assert_eq!(
            u.unwrap(TS_MODULUS - 3000),
            Unwrapped { ticks: base - 3000, step: Step::Backward }
        );
        assert_eq!(u.unwrap(0), Unwrapped { ticks: base, step: Step::Rollover });
        assert_eq!(u.unwrap(12_000), Unwrapped { ticks: base + 12_000, step: Step::Forward });
    }

    #[test]
    fn backward_before_the_first_value_goes_negative() {
        let mut u = Unwrapper::for_timestamps();
        assert_eq!(u.unwrap(100).ticks, 100);
        assert_eq!(u.unwrap(TS_MODULUS - 200), Unwrapped { ticks: -200, step: Step::Backward });
        assert_eq!(u.unwrap(50), Unwrapped { ticks: 50, step: Step::Rollover });
    }

    #[test]
    fn a_restart_is_a_discontinuity_not_a_rollover() {
        let mut u = Unwrapper::for_timestamps();
        let hour = 3600 * TS_HZ;
        u.unwrap(hour);
        u.unwrap(hour + 3000);
        let r = u.unwrap(1000);
        assert_eq!(r.step, Step::Discontinuity);
        assert_eq!(r.ticks, 1000);
        assert_eq!(u.unwrap(4000), Unwrapped { ticks: 4000, step: Step::Forward });
        let r = u.unwrap(4000 + DEFAULT_MAX_STEP + 1);
        assert_eq!(r.step, Step::Discontinuity);
        let at = r.ticks as u64;
        assert_eq!(u.unwrap(at + DEFAULT_MAX_STEP).step, Step::Forward);
        assert_eq!(u.unwrap(at).step, Step::Backward);
    }

    #[test]
    fn reset_starts_over() {
        let mut u = Unwrapper::for_timestamps();
        u.unwrap(5);
        u.reset();
        assert_eq!(u.last(), None);
        assert_eq!(u.unwrap(7), Unwrapped { ticks: 7, step: Step::First });
    }

    #[test]
    fn pcr_scale() {
        let mut u = Unwrapper::new(PCR_MODULUS, 10 * PCR_HZ);
        let near_end = PCR_MODULUS - PCR_HZ;
        u.unwrap(near_end);
        let r = u.unwrap(PCR_HZ);
        assert_eq!(r, Unwrapped { ticks: (PCR_MODULUS + PCR_HZ) as i64, step: Step::Rollover });
        assert_eq!(pcr_to_90k(PCR_HZ), TS_HZ);
    }

    #[test]
    fn hostile_input_never_overflows() {
        let mut u = Unwrapper::new(u64::MAX, u64::MAX);
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            u.unwrap(x);
        }
        let mut u = Unwrapper::new(0, 0);
        u.unwrap(1);
        u.unwrap(u64::MAX);
    }
}
