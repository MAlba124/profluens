#[cfg(loom)]
use loom::sync::{
    Mutex,
    atomic::{AtomicU8, AtomicU64, Ordering::Relaxed, Ordering::SeqCst, fence},
};
#[cfg(not(loom))]
use std::sync::{
    Mutex,
    atomic::{AtomicU8, AtomicU64, Ordering::Relaxed, Ordering::SeqCst, fence},
};

use crate::time::Timestamp;

const NONE: u64 = u64::MAX;

/// What a level holds. `time` is zero where no stamp says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Held {
    pub bytes: u64,
    pub time: Timestamp,
}

impl Default for Held {
    fn default() -> Held {
        Held { bytes: 0, time: Timestamp::ZERO }
    }
}

/// A bound in both measures. `u64::MAX` bytes and [`Timestamp::NONE`] are no bound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limit {
    pub bytes: u64,
    pub time: Timestamp,
}

impl Limit {
    pub const NONE: Limit = Limit { bytes: NONE, time: Timestamp::NONE };

    pub const fn time(time: Timestamp) -> Limit {
        Limit { bytes: NONE, time }
    }

    pub const fn bytes(bytes: u64) -> Limit {
        Limit { bytes, time: Timestamp::NONE }
    }

    pub fn reached(&self, held: Held) -> bool {
        held.bytes >= self.bytes || (self.time.is_some() && held.time >= self.time)
    }
}

/// Which side of its marks a level is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fill {
    /// At or under the low mark, until the high mark is reached.
    Low,
    /// At or over the high mark, until the low mark is reached.
    High,
}

#[repr(align(64))]
#[derive(Default)]
struct Fed {
    bytes: AtomicU64,
    // Times are stream time plus one, zero is none.
    from: AtomicU64,
    to: AtomicU64,
    stretch: AtomicU64,
    ended: AtomicU8,
}

#[repr(align(64))]
#[derive(Default)]
struct Taken {
    bytes: AtomicU64,
    at: AtomicU64,
    stretch: AtomicU64,
}

#[repr(align(64))]
struct Bounds {
    max_bytes: AtomicU64,
    max_time: AtomicU64,
    low: [AtomicU64; 2],
    high: [AtomicU64; 2],
    fill: AtomicU8,
}

/// How much a link or store holds, written lock-free by its feeder and taker.
pub struct Gauge {
    fed: Fed,
    taken: Taken,
    bounds: Bounds,
    // Held while a link's end judges and says a crossing, so they are said in order.
    say: Mutex<()>,
}

impl Default for Gauge {
    fn default() -> Gauge {
        let none = || AtomicU64::new(NONE);
        let bounds = Bounds {
            max_bytes: none(),
            max_time: none(),
            low: [none(), none()],
            high: [none(), none()],
            fill: AtomicU8::new(0),
        };
        Gauge { fed: Fed::default(), taken: Taken::default(), bounds, say: Mutex::new(()) }
    }
}

fn stamp(t: Timestamp) -> u64 {
    t.0.wrapping_add(1)
}

impl Gauge {
    /// Feeder. `bytes` more were put in, covering stream time `from` to `to`.
    pub fn feed(&self, bytes: u64, from: Timestamp, to: Timestamp) {
        let fed = &self.fed;
        fed.bytes.store(fed.bytes.load(Relaxed).wrapping_add(bytes), Relaxed);
        if to.is_some() {
            if fed.from.load(Relaxed) == 0 {
                fed.from.store(stamp(if from.is_some() { from } else { to }), Relaxed);
            }
            fed.to.store(stamp(to), Relaxed);
        }
    }

    /// Feeder. What follows is stretch `stretch` of the stream, behind a seek.
    pub fn restart(&self, stretch: u64) {
        self.fed.ended.store(0, Relaxed);
        self.fed.from.store(0, Relaxed);
        self.fed.to.store(0, Relaxed);
        self.fed.stretch.store(stretch, Relaxed);
    }

    /// Feeder. Nothing more comes in this stretch, so a link's marks read high.
    pub fn end(&self) {
        self.fed.ended.store(1, Relaxed);
    }

    /// Where the feeder stands in stream time, `NONE` before a stamped row in this stretch.
    pub fn fed_to(&self) -> Timestamp {
        Timestamp(self.fed.to.load(Relaxed).wrapping_sub(1))
    }

    pub fn ended(&self) -> bool {
        self.fed.ended.load(Relaxed) != 0
    }

    /// Taker. It dropped what it held for seek stretch `stretch`, false if already there.
    pub fn restarted(&self, stretch: u64) -> bool {
        let moved = self.taken.stretch.load(Relaxed) != stretch;
        if moved {
            self.taken.at.store(0, Relaxed);
            self.taken.stretch.store(stretch, Relaxed);
        }
        moved
    }

    pub fn reset(&self) {
        self.restart(0);
        self.taken.stretch.store(0, Relaxed);
        self.fed.bytes.store(0, Relaxed);
        self.take(0, Timestamp::NONE);
    }

    /// Taker. `bytes` is the count of all it took so far, `at` where it stands.
    pub fn take(&self, bytes: u64, at: Timestamp) {
        self.taken.bytes.store(bytes, Relaxed);
        self.taken.at.store(stamp(at), Relaxed);
        // Dekker with a resting feeder (`station::Room`). Only a limited level rests a feeder.
        if self.limit() != Limit::NONE {
            fence(SeqCst);
        }
    }

    pub fn held(&self) -> Held {
        let bytes = self.fed.bytes.load(Relaxed).wrapping_sub(self.taken.bytes.load(Relaxed));
        let (from, to) = (self.fed.from.load(Relaxed), self.fed.to.load(Relaxed));
        let (fed, taken) = (self.fed.stretch.load(Relaxed), self.taken.stretch.load(Relaxed));
        // The ends pass a seek one after the other, so a stretch mismatch picks the side behind.
        let at = match fed.cmp(&taken) {
            std::cmp::Ordering::Greater => from,
            std::cmp::Ordering::Less => to,
            std::cmp::Ordering::Equal => self.taken.at.load(Relaxed).max(from),
        };
        let time = if from == 0 || to == 0 { 0 } else { to.saturating_sub(at) };
        // A count read behind the taker's is a batch in flight, not 2^64 bytes.
        Held { bytes: if bytes > u64::MAX / 2 { 0 } else { bytes }, time: Timestamp(time) }
    }

    pub fn limit(&self) -> Limit {
        let b = &self.bounds;
        Limit { bytes: b.max_bytes.load(Relaxed), time: Timestamp(b.max_time.load(Relaxed)) }
    }

    pub fn set_limit(&self, limit: Limit) {
        self.bounds.max_bytes.store(limit.bytes, Relaxed);
        self.bounds.max_time.store(limit.time.0, Relaxed);
    }

    /// The byte bound alone. The app sets it while the tail grows the time bound.
    pub fn set_limit_bytes(&self, bytes: u64) {
        self.bounds.max_bytes.store(bytes, Relaxed);
    }

    /// The time bound alone, the one the tail grows.
    pub fn set_limit_time(&self, time: Timestamp) {
        self.bounds.max_time.store(time.0, Relaxed);
    }

    /// Set the low and high marks. A level with none never says a [`Fill`].
    pub fn set_marks(&self, low: Limit, high: Limit) {
        let b = &self.bounds;
        for (cell, mark) in [(&b.low, low), (&b.high, high)] {
            cell[0].store(mark.bytes, Relaxed);
            cell[1].store(mark.time.0, Relaxed);
        }
    }

    pub fn full(&self) -> bool {
        let limit = self.limit();
        limit != Limit::NONE && limit.reached(self.held())
    }

    /// How much stream time the feeder may still put in. `NONE` with no time limit.
    pub fn room(&self) -> Timestamp {
        match self.limit().time {
            Timestamp::NONE => Timestamp::NONE,
            max => max.saturating_sub(self.held().time),
        }
    }

    fn marks(&self) -> (Limit, Limit) {
        let mark = |m: &[AtomicU64; 2]| Limit {
            bytes: m[0].load(Relaxed),
            time: Timestamp(m[1].load(Relaxed)),
        };
        (mark(&self.bounds.low), mark(&self.bounds.high))
    }

    pub fn marked(&self) -> bool {
        self.marks().1 != Limit::NONE
    }

    /// The side of the marks crossed since last asked. One of two racing callers gets it.
    pub fn crossed(&self) -> Option<Fill> {
        let (low, high) = self.marks();
        if high == Limit::NONE {
            return None;
        }
        let held = self.held();
        let now = if high.reached(held) {
            HIGH
        } else if under(low, held) {
            LOW
        } else {
            return None;
        };
        let was = self.bounds.fill.swap(now, Relaxed);
        (was != now).then_some(if now == HIGH { Fill::High } else { Fill::Low })
    }

    /// A link's end judges its side of the marks and says a crossing with `say`, under a lock so
    /// the last said is current.
    pub fn judge(&self, rising: bool, mut say: impl FnMut(Fill, Held)) {
        // Dekker with the other end, which stores the fill, fences and judges this side again.
        fence(SeqCst);
        if self.crossing(rising).is_none() {
            return;
        }
        let _said = self.say.lock().unwrap_or_else(|e| e.into_inner());
        for side in [rising, !rising] {
            let Some((fill, held)) = self.crossing(side) else {
                return;
            };
            self.bounds.fill.store(if fill == Fill::High { HIGH } else { LOW }, Relaxed);
            say(fill, held);
            fence(SeqCst);
        }
    }

    fn crossing(&self, rising: bool) -> Option<(Fill, Held)> {
        let (low, high) = self.marks();
        if high == Limit::NONE {
            return None;
        }
        let (fed, taken) = (self.fed.stretch.load(Relaxed), self.taken.stretch.load(Relaxed));
        let (ended, fill, held) =
            (self.fed.ended.load(Relaxed) != 0, self.bounds.fill.load(Relaxed), self.held());
        match rising {
            true if fed >= taken && fill != HIGH && (ended || high.reached(held)) => {
                Some((Fill::High, held))
            }
            false if fed == taken && fill != LOW && !ended && under(low, held) => {
                Some((Fill::Low, held))
            }
            _ => None,
        }
    }
}

const LOW: u8 = 1;
const HIGH: u8 = 2;

fn under(low: Limit, held: Held) -> bool {
    match low == Limit::NONE {
        true => held.bytes == 0,
        false => {
            (low.bytes == NONE || held.bytes <= low.bytes)
                && (low.time.is_none() || held.time <= low.time)
        }
    }
}

/// One range a store holds in input bytes and stream time, ends exclusive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Range {
    pub bytes: (u64, u64),
    pub time: (Timestamp, Timestamp),
}

/// How many ranges a store says.
pub const RANGES: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RangeSet {
    ranges: [Range; RANGES],
    len: usize,
}

impl RangeSet {
    pub fn as_slice(&self) -> &[Range] {
        &self.ranges[..self.len]
    }
}

/// A store's ranges, a seqlock whose count is odd while written.
#[derive(Default)]
pub struct Ranges {
    seq: AtomicU64,
    len: AtomicU64,
    cells: [[AtomicU64; 4]; RANGES],
}

impl Ranges {
    /// Writer, one thread. Replaces what was said.
    pub fn write(&self, ranges: &[Range]) {
        self.seq.fetch_add(1, SeqCst);
        let n = ranges.len().min(RANGES);
        for (cell, r) in self.cells.iter().zip(&ranges[..n]) {
            for (c, v) in cell.iter().zip([r.bytes.0, r.bytes.1, r.time.0.0, r.time.1.0]) {
                c.store(v, SeqCst);
            }
        }
        self.len.store(n as u64, SeqCst);
        self.seq.fetch_add(1, SeqCst);
    }

    /// Reader. Retries a torn read a few times, then says nothing.
    pub fn read(&self) -> RangeSet {
        let none = Range { bytes: (0, 0), time: (Timestamp::NONE, Timestamp::NONE) };
        let mut set = RangeSet { ranges: [none; RANGES], len: 0 };
        for _ in 0..if cfg!(loom) { 1 } else { 8 } {
            let seq = self.seq.load(SeqCst);
            let n = (self.len.load(SeqCst) as usize).min(RANGES);
            for (r, cell) in set.ranges.iter_mut().zip(&self.cells).take(n) {
                let [a, b, c, d] = [0, 1, 2, 3].map(|i| cell[i].load(SeqCst));
                *r = Range { bytes: (a, b), time: (Timestamp(c), Timestamp(d)) };
            }
            if seq.is_multiple_of(2) && self.seq.load(SeqCst) == seq {
                set.len = n;
                break;
            }
        }
        set
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    const fn ms(n: u64) -> Timestamp {
        Timestamp::from_millis(n)
    }

    #[test]
    fn a_level_holds_what_was_fed_and_not_taken_in_both_measures() {
        let l = Gauge::default();
        assert_eq!(l.held(), Held::default());
        l.feed(100, ms(0), ms(20));
        l.feed(100, ms(20), ms(40));
        assert_eq!(l.held(), Held { bytes: 200, time: ms(40) });
        l.take(100, ms(20));
        assert_eq!(l.held(), Held { bytes: 100, time: ms(20) });
        l.take(200, ms(40));
        assert_eq!(l.held(), Held::default());
    }

    #[test]
    fn rows_with_no_stamp_move_bytes_and_no_time() {
        let l = Gauge::default();
        l.feed(64, Timestamp::NONE, Timestamp::NONE);
        assert_eq!(l.held(), Held { bytes: 64, time: Timestamp::ZERO });
        l.set_limit(Limit::time(ms(10)));
        assert!(!l.full(), "no time is known, so the time limit does not hold it");
        l.set_limit(Limit::bytes(64));
        assert!(l.full());
    }

    #[test]
    fn a_restart_measures_from_the_first_row_behind_it() {
        let l = Gauge::default();
        l.feed(10, ms(0), ms(500));
        l.take(10, ms(500));
        l.restart(1);
        l.feed(10, ms(9_000), ms(9_020));
        assert_eq!(l.held().time, ms(20), "from the first row fed, not from the old place");
        assert!(l.restarted(2));
        assert_eq!(l.held().time, Timestamp::ZERO);
        l.restart(2);
        l.feed(10, ms(100), ms(120));
        assert_eq!(l.held().time, ms(20), "the taker stands in it, at no place yet");
        l.take(20, ms(110));
        assert_eq!(l.held().time, ms(10));
        assert!(!l.restarted(2), "the same stretch again is no seek");
        assert_eq!(l.held().time, ms(10));
    }

    #[test]
    fn a_limit_rests_the_feeder_until_the_taker_stands_where_it_fed_to() {
        let l = Gauge::default();
        assert!(!l.full());
        assert_eq!(l.room(), Timestamp::NONE);
        l.set_limit(Limit::time(ms(50)));
        l.feed(8, ms(0), ms(40));
        assert!(!l.full());
        assert_eq!(l.room(), ms(10));
        l.feed(8, ms(40), ms(80));
        assert!(l.full());
        assert_eq!(l.room(), Timestamp::ZERO);
        l.take(16, ms(80));
        assert!(!l.full());
        l.feed(0, ms(80), ms(10_000));
        assert_eq!(l.held(), Held { bytes: 0, time: ms(9_920) });
        assert!(l.full());
        l.take(16, ms(10_000));
        assert!(!l.full());
    }

    #[test]
    fn a_crossing_is_said_once_and_only_with_marks() {
        let l = Gauge::default();
        l.feed(1, ms(0), ms(900));
        assert_eq!(l.crossed(), None, "no marks");
        l.set_marks(Limit::time(ms(100)), Limit::time(ms(800)));
        assert_eq!(l.crossed(), Some(Fill::High));
        assert_eq!(l.crossed(), None);
        l.take(0, ms(500));
        assert_eq!(l.crossed(), None, "between the marks it stays what it was");
        l.take(0, ms(850));
        assert_eq!(l.crossed(), Some(Fill::Low));
        assert_eq!(l.crossed(), None);
        l.feed(1, ms(900), ms(1_700));
        assert_eq!(l.crossed(), Some(Fill::High));
    }

    #[allow(clippy::disallowed_methods)]
    #[test]
    fn a_links_feeder_says_high_and_its_taker_low_in_the_stretch_both_stand_in() {
        let l = Gauge::default();
        let said = std::cell::RefCell::new(Vec::new());
        let say = |fill: Fill, _: Held| said.borrow_mut().push(fill);
        let heard = || std::mem::take(&mut *said.borrow_mut());
        l.feed(1, ms(0), ms(900));
        l.judge(true, say);
        assert!(!l.marked() && heard().is_empty(), "no marks");
        l.set_marks(Limit::time(ms(100)), Limit::time(ms(800)));
        l.judge(false, say);
        assert_eq!(heard(), [], "the taker does not say high");
        l.judge(true, say);
        l.judge(true, say);
        assert_eq!(heard(), [Fill::High]);
        l.take(1, ms(850));
        l.judge(true, say);
        assert_eq!(heard(), [], "the feeder does not say low");
        l.judge(false, say);
        assert_eq!(heard(), [Fill::Low]);
        l.end();
        l.judge(true, say);
        l.take(1, ms(900));
        l.judge(false, say);
        assert_eq!(heard(), [Fill::High], "an end reads high, and stays so to the last take");
        l.restart(1);
        l.feed(1, ms(0), ms(20));
        l.judge(false, say);
        assert_eq!(heard(), [], "the taker is a stretch behind");
        assert!(l.restarted(1));
        l.judge(false, say);
        assert_eq!(heard(), [Fill::Low], "the end was the old stretch's");
    }

    #[test]
    fn the_ranges_read_are_the_ranges_last_written() {
        let r = Ranges::default();
        assert!(r.read().as_slice().is_empty());
        let a = Range { bytes: (0, 100), time: (ms(0), ms(500)) };
        let b = Range { bytes: (400, 900), time: (Timestamp::NONE, Timestamp::NONE) };
        r.write(&[a, b]);
        assert_eq!(r.read().as_slice(), [a, b]);
        r.write(&[b]);
        assert_eq!(r.read().as_slice(), [b]);
        r.write(&[a; RANGES + 3]);
        assert_eq!(r.read().as_slice().len(), RANGES, "the first of them");
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;
    use crate::station::{Room, Station};
    use loom::sync::Arc;

    #[test]
    fn loom_a_feeder_resting_on_a_limit_is_woken_by_the_take() {
        loom::model(|| {
            let level = Arc::new(Gauge::default());
            level.set_limit(Limit::bytes(1));
            level.feed(1, Timestamp::NONE, Timestamp::NONE);
            let (room, station) = (Arc::new(Room::new()), Arc::new(Station::new()));
            let feeder = {
                let (level, room, station) =
                    (Arc::clone(&level), Arc::clone(&room), Arc::clone(&station));
                loom::thread::spawn(move || {
                    loop {
                        let seen = station.epoch();
                        let rests = level.full() && {
                            room.raise();
                            level.full()
                        };
                        if !rests {
                            level.feed(1, Timestamp::NONE, Timestamp::NONE);
                            return;
                        }
                        station.park(seen, None, None);
                    }
                })
            };
            level.take(1, Timestamp::NONE);
            if room.take() {
                station.signal();
            }
            feeder.join().unwrap();
            assert_eq!(level.held().bytes, 1);
        });
    }

    // The feeder grows its own limit where it would rest. The take still wakes it.
    #[test]
    fn loom_a_feeder_whose_limit_grows_between_its_looks_is_still_woken() {
        loom::model(|| {
            let t = Timestamp;
            let level = Arc::new(Gauge::default());
            level.set_limit(Limit::time(t(10)));
            level.feed(1, t(0), t(10));
            let (room, station) = (Arc::new(Room::new()), Arc::new(Station::new()));
            let feeder = {
                let (level, room, station) =
                    (Arc::clone(&level), Arc::clone(&room), Arc::clone(&station));
                loom::thread::spawn(move || {
                    let (mut grown, mut to) = (false, 10);
                    loop {
                        let seen = station.epoch();
                        if level.full() && !std::mem::replace(&mut grown, true) {
                            level.set_limit(Limit::time(t(20)));
                            continue;
                        }
                        let rests = level.full() && {
                            room.raise();
                            level.full()
                        };
                        if rests {
                            station.park(seen, None, None);
                            continue;
                        }
                        assert!(level.held().time <= t(20), "{:?}", level.held());
                        level.feed(1, t(to), t(to + 10));
                        to += 10;
                        if to == 30 {
                            return;
                        }
                    }
                })
            };
            level.take(1, t(10));
            if room.take() {
                station.signal();
            }
            feeder.join().unwrap();
            assert!(level.held().time <= t(20));
        });
    }

    // The app bounds the bytes while the tail grows the time limit. Neither undoes the other.
    #[test]
    fn loom_a_byte_bound_and_a_grown_time_limit_set_at_once_both_hold() {
        loom::model(|| {
            let level = Arc::new(Gauge::default());
            level.set_limit(Limit::time(Timestamp(10)));
            let app = {
                let level = Arc::clone(&level);
                loom::thread::spawn(move || level.set_limit_bytes(64))
            };
            level.set_limit_time(Timestamp(20));
            app.join().unwrap();
            assert_eq!(level.limit(), Limit { bytes: 64, time: Timestamp(20) });
        });
    }

    // Both ends judge at once. The last said is where the level ends, no side said twice running.
    fn crossings_race(start: (u64, u64), feed: u64, take: u64) -> (Held, Vec<Fill>) {
        let level = Arc::new(Gauge::default());
        level.set_marks(Limit::bytes(1), Limit::bytes(2));
        let said = Arc::new(loom::sync::Mutex::new(Vec::new()));
        let say = |said: &Arc<loom::sync::Mutex<Vec<Fill>>>| {
            let said = Arc::clone(said);
            move |fill, _| said.lock().unwrap().push(fill)
        };
        level.feed(start.0, Timestamp::NONE, Timestamp::NONE);
        level.take(start.1, Timestamp::NONE);
        level.judge(start.0 > start.1, say(&said));
        let taker = {
            let (level, say) = (Arc::clone(&level), say(&said));
            loom::thread::spawn(move || {
                level.take(take, Timestamp::NONE);
                level.judge(false, say);
            })
        };
        level.feed(feed, Timestamp::NONE, Timestamp::NONE);
        level.judge(true, say(&said));
        taker.join().unwrap();
        let said = said.lock().unwrap().clone();
        (level.held(), said)
    }

    #[test]
    fn loom_a_crossing_said_last_is_the_side_the_level_ends_on() {
        loom::model(|| {
            let (held, said) = crossings_race((2, 0), 2, 2);
            assert_eq!(held.bytes, 2);
            assert_eq!(said.last(), Some(&Fill::High), "{said:?}");
            assert!(said.windows(2).all(|w| w[0] != w[1]), "{said:?}");
        });
        loom::model(|| {
            let (held, said) = crossings_race((0, 0), 2, 2);
            assert_eq!(held.bytes, 0);
            assert_eq!(said.last(), Some(&Fill::Low), "{said:?}");
            assert!(said.windows(2).all(|w| w[0] != w[1]), "{said:?}");
        });
    }

    #[test]
    fn loom_a_range_read_is_one_write_whole() {
        loom::model(|| {
            let ranges = Arc::new(Ranges::default());
            let one = |n: u64| Range { bytes: (n, n), time: (Timestamp(n), Timestamp(n)) };
            ranges.write(&[one(1)]);
            let writer = {
                let ranges = Arc::clone(&ranges);
                loom::thread::spawn(move || ranges.write(&[one(2)]))
            };
            let got = ranges.read();
            let rows = got.as_slice();
            assert!(rows.is_empty() || rows == [one(1)] || rows == [one(2)], "{rows:?}");
            writer.join().unwrap();
        });
    }
}
