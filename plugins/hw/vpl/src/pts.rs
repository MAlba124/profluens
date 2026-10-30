//! The runtime's 90 kHz stamps paired back with the exact pts and duration a frame came with.

use profluens_core::time::Timestamp;

use crate::ffi;

pub fn to_ts90(pts: Timestamp) -> u64 {
    match pts.nanos() {
        Some(ns) => ns / 100_000 * 9 + (ns % 100_000) * 9 / 100_000,
        None => ffi::MFX_TIMESTAMP_UNKNOWN as u64,
    }
}

/// Past the largest time a stamp saturates there.
pub fn from_ts90(ts: u64) -> Timestamp {
    if ts == ffi::MFX_TIMESTAMP_UNKNOWN as u64 {
        return Timestamp::NONE;
    }
    let ns = (u128::from(ts) * 100_000 + 4) / 9;
    Timestamp::from_nanos(ns.min(u128::from(Timestamp::MAX.0)) as u64)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub pts: Timestamp,
    pub duration: Timestamp,
    pub only: bool,
}

const SLOTS: usize = 128;

pub struct PtsMap {
    /// `None` is an empty slot, so an unknown stamp (u64::MAX) is a key like any other.
    slots: [(Option<u64>, Stamp); SLOTS],
    next: usize,
}

impl PtsMap {
    pub const fn new() -> PtsMap {
        PtsMap {
            slots: [(
                None,
                Stamp { pts: Timestamp::NONE, duration: Timestamp::NONE, only: false },
            ); SLOTS],
            next: 0,
        }
    }

    pub fn put(&mut self, ts90: u64, stamp: Stamp) {
        self.slots[self.next] = (Some(ts90), stamp);
        self.next = (self.next + 1) % SLOTS;
    }

    pub fn take(&mut self, ts90: u64) -> Stamp {
        if let Some(slot) = self.slots.iter_mut().find(|(t, _)| *t == Some(ts90)) {
            let s = slot.1;
            slot.0 = None;
            return s;
        }
        Stamp { pts: from_ts90(ts90), duration: Timestamp::NONE, only: false }
    }

    /// The earliest stamp owed after `last`, decoders output in presentation order.
    /// A frame fed twice leaves a stale entry, skipped when at or before `last`.
    pub fn take_next(&mut self, last: Option<Timestamp>) -> Option<Stamp> {
        let owed = |i: &usize| {
            self.slots[*i].0.is_some() && last.is_none_or(|l| self.slots[*i].1.pts > l)
        };
        let i = (0..SLOTS).filter(owed).min_by_key(|&i| self.slots[i].1.pts)?;
        self.slots[i].0 = None;
        Some(self.slots[i].1)
    }

    /// A drain is not trusted, the runtime repeats the last bitstream's stamp on every held frame.
    pub fn take_or_next(&mut self, ts90: u64, trust: bool, last: Option<Timestamp>) -> Stamp {
        if trust && self.slots.iter().any(|(t, _)| *t == Some(ts90)) {
            return self.take(ts90);
        }
        self.take_next(last).unwrap_or_else(|| self.take(ts90))
    }

    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.0 = None;
        }
    }
}

impl Default for PtsMap {
    fn default() -> Self {
        PtsMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip_within_a_tick_and_the_map_restores_the_exact_pts() {
        let pts = Timestamp::from_nanos(1_234_567_891);
        let ts = to_ts90(pts);
        assert_eq!(ts, 111_111);
        let back = from_ts90(ts).nanos().unwrap();
        assert!((back as i64 - 1_234_567_891).abs() < 12_000);
        assert_eq!(to_ts90(Timestamp::NONE), ffi::MFX_TIMESTAMP_UNKNOWN as u64);
        assert_eq!(from_ts90(ffi::MFX_TIMESTAMP_UNKNOWN as u64), Timestamp::NONE);
        assert_eq!(from_ts90(u64::MAX - 1), Timestamp::MAX, "saturates, never wraps");
        let mut m = PtsMap::new();
        m.put(ts, Stamp { pts, duration: Timestamp::from_nanos(40_000_000), only: true });
        assert_eq!(
            m.take(ts),
            Stamp { pts, duration: Timestamp::from_nanos(40_000_000), only: true }
        );
        assert_eq!(m.take(ts).pts, from_ts90(ts), "taken once, then rescaled");
        for i in 0..SLOTS as u64 + 1 {
            m.put(
                i,
                Stamp { pts: Timestamp::from_nanos(i), duration: Timestamp::NONE, only: false },
            );
        }
        assert_eq!(m.take(0).pts, from_ts90(0), "the oldest was forgotten");
        assert_eq!(m.take(5).pts, Timestamp::from_nanos(5));
    }

    #[test]
    fn a_drain_takes_the_frames_still_owed_in_presentation_order() {
        let ms = |v: u64| Timestamp::from_nanos(v * 1_000_000);
        let mut m = PtsMap::new();
        for v in [0u64, 0, 4400, 4440, 4480] {
            m.put(to_ts90(ms(v)), Stamp { pts: ms(v), duration: ms(40), only: false });
        }
        assert_eq!(m.take(to_ts90(ms(0))).pts, ms(0));
        assert_eq!(m.take(to_ts90(ms(4400))).pts, ms(4400));
        assert_eq!(
            m.take_or_next(to_ts90(ms(4480)), false, Some(ms(4400))).pts,
            ms(4440),
            "the earliest owed after the last out"
        );
        assert_eq!(m.take_or_next(to_ts90(ms(4480)), false, Some(ms(4440))).pts, ms(4480));
        assert_eq!(
            m.take_or_next(to_ts90(ms(4480)), false, Some(ms(4480))).pts,
            from_ts90(to_ts90(ms(4480))),
            "nothing owed after it, rescaled"
        );
        assert_eq!(m.take_next(None).map(|s| s.pts), Some(ms(0)));
        m.put(to_ts90(ms(100)), Stamp { pts: ms(100), duration: ms(40), only: false });
        m.put(to_ts90(ms(140)), Stamp { pts: ms(140), duration: ms(40), only: false });
        assert_eq!(m.take_or_next(to_ts90(ms(140)), true, None).pts, ms(140));
        assert_eq!(m.take_or_next(to_ts90(ms(999)), true, None).pts, ms(100));
        assert_eq!(m.take_next(None), None, "nothing owed");
    }

    #[test]
    fn a_unit_without_a_pts_is_kept_apart_from_the_empty_slots() {
        let unknown = to_ts90(Timestamp::NONE);
        assert_eq!(unknown, u64::MAX);
        let ms = |v: u64| Timestamp::from_nanos(v * 1_000_000);
        let mut m = PtsMap::new();
        let none = m.take(unknown);
        assert_eq!((none.pts, none.duration, none.only), (Timestamp::NONE, Timestamp::NONE, false));
        assert_eq!(m.take_or_next(unknown, true, None).pts, Timestamp::NONE, "nothing put");
        m.put(to_ts90(ms(40)), Stamp { pts: ms(40), duration: ms(40), only: true });
        m.put(unknown, Stamp { pts: Timestamp::NONE, duration: ms(40), only: false });
        let s = m.take_or_next(unknown, true, None);
        assert_eq!((s.pts, s.duration, s.only), (Timestamp::NONE, ms(40), false), "its own stamp");
        let s = m.take_or_next(unknown, true, None);
        assert_eq!(s.pts, ms(40), "an unknown stamp with none put takes the earliest owed");
        assert_eq!(m.take_next(None), None, "both went out once");
        m.put(unknown, Stamp { pts: Timestamp::NONE, duration: ms(40), only: false });
        m.clear();
        assert_eq!(m.take(unknown).duration, Timestamp::NONE, "cleared");
    }
}
