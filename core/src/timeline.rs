#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::time::{Rate, Timestamp};

/// Slots in the ring, two under loom so it wraps in two appends.
pub(crate) const PIECES: usize = if cfg!(loom) { 2 } else { 8 };

/// Set in the rate word of a piece that restarts the map. Nothing older counts.
const RESTART: u64 = 1 << 32;

const TRIES: usize = 8;

/// Stream time `stream_start` is running time `running_start`, at `rate` from there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Piece {
    pub(crate) stream_start: Timestamp,
    pub(crate) running_start: Timestamp,
    pub(crate) rate: Rate,
}

fn scale(at: Timestamp, d: i128, mul: u16, div: u16) -> i128 {
    at.0 as i128 + (d * mul as i128).div_euclid(div as i128)
}

impl Piece {
    /// The running time of stream time `t`. Signed, for a time before the piece.
    pub(crate) fn running(&self, t: u64) -> i128 {
        let d = t as i128 - self.stream_start.0 as i128;
        scale(self.running_start, d, self.rate.den(), self.rate.num())
    }

    pub(crate) fn stream(&self, r: u64) -> i128 {
        let d = r as i128 - self.running_start.0 as i128;
        scale(self.stream_start, d, self.rate.num(), self.rate.den())
    }
}

pub(crate) fn clamp(t: i128) -> Timestamp {
    Timestamp(t.clamp(0, Timestamp::MAX.0 as i128) as u64)
}

pub(crate) struct Timeline {
    /// Pieces written so far. Piece `k` is in slot `k % PIECES`.
    head: AtomicU64,
    /// Per piece its stream start, running start and rate with [`RESTART`].
    slots: [[AtomicU64; 3]; PIECES],
}

impl Timeline {
    pub(crate) fn new() -> Self {
        let slots = std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0)));
        Self { head: AtomicU64::new(0), slots }
    }

    /// Writer side. Publish `piece`. As a restart it is the oldest a reader maps through.
    pub(crate) fn push(&self, piece: Piece, restart: bool) {
        let seq = self.head.load(Ordering::Relaxed);
        let rate = piece.rate.pack() | if restart { RESTART } else { 0 };
        let words = [piece.stream_start.0, piece.running_start.0, rate];
        for (slot, word) in self.slots[seq as usize % PIECES].iter().zip(words) {
            slot.store(word, Ordering::Release);
        }
        self.head.store(seq + 1, Ordering::Release);
    }

    /// The newest piece starting at or before `at`, else the last restart or the oldest kept.
    pub(crate) fn find(&self, at: Timestamp, by_stream: bool) -> Piece {
        let zero = Timestamp::ZERO;
        let mut found = Piece { stream_start: zero, running_start: zero, rate: Rate::ONE };
        for _ in 0..TRIES {
            let head = self.head.load(Ordering::Acquire);
            let mut seq = head;
            while seq > 0 && head - seq < PIECES as u64 - 1 {
                seq -= 1;
                let [stream, running, rate] =
                    self.slots[seq as usize % PIECES].each_ref().map(|w| w.load(Ordering::Acquire));
                let (stream_start, running_start) = (Timestamp(stream), Timestamp(running));
                found = Piece { stream_start, running_start, rate: Rate::unpack(rate) };
                let start = if by_stream { stream_start } else { running_start };
                if rate & RESTART != 0 || start <= at {
                    break;
                }
            }
            // Words are Release-stored before `head`, so a reader that saw a rewritten word
            // retries.
            if self.head.load(Ordering::Acquire) - seq < PIECES as u64 {
                break;
            }
        }
        found
    }

    /// The running time of stream time `t`. Signed, and `t` is not the sentinel.
    pub(crate) fn running(&self, t: Timestamp) -> i128 {
        self.find(t, true).running(t.0)
    }

    pub(crate) fn stream(&self, r: Timestamp) -> Timestamp {
        if r.is_none() { r } else { clamp(self.find(r, false).stream(r.0)) }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn rate(num: i64, den: i64) -> Rate {
        Rate::new(num, den).unwrap()
    }

    fn piece(stream: u64, running: u64, r: Rate) -> Piece {
        Piece { stream_start: Timestamp(stream), running_start: Timestamp(running), rate: r }
    }

    fn append(t: &Timeline, stream: u64, r: Rate) {
        let running = clamp(t.find(Timestamp::MAX, true).running(stream));
        t.push(piece(stream, running.0, r), false);
    }

    const RATES: [(i64, i64); 9] =
        [(1, 1), (3, 2), (2, 3), (3, 4), (2, 1), (1, 4), (4, 1), (64, 1), (1, 64)];

    #[test]
    fn at_rate_one_the_map_is_the_identity_before_and_after_the_piece() {
        let t = Timeline::new();
        for at in [0, 1, 999, u64::MAX / 2, Timestamp::MAX.0] {
            assert_eq!(t.running(Timestamp(at)), at as i128, "no piece yet");
        }
        let five = 5_000_000_000;
        t.push(piece(five, five, Rate::ONE), true);
        for at in [0, 1, five - 1, five, five + 1, Timestamp::MAX.0] {
            assert_eq!(t.running(Timestamp(at)), at as i128);
            assert_eq!(t.stream(Timestamp(at)).0 as i128, at as i128);
        }
    }

    #[test]
    fn the_inverse_of_the_map_is_within_a_nanosecond_up_to_rate_one_and_the_rate_above() {
        let starts = [(0, 0), (7, 3), (1_000_000_007, 999_999_937), (u64::MAX / 4, u64::MAX / 8)];
        let offsets = [0u64, 1, 2, 3, 63, 64, 65, 1_000, 999_999_999, 86_400_000_000_007];
        for ((num, den), (stream, running)) in
            RATES.into_iter().flat_map(|r| starts.map(|s| (r, s)))
        {
            let p = piece(stream, running, rate(num, den));
            let bound = ((num + den - 1) / den).max(1) as i128;
            for d in offsets {
                let at = stream + d;
                let r = p.running(at);
                assert!(r >= running as i128, "{p:?} {at}");
                let back = p.stream(r as u64);
                assert!(back <= at as i128 && at as i128 - back <= bound, "{p:?} {at} {back}");
            }
        }
    }

    #[test]
    fn the_map_never_goes_back_inside_a_piece_or_across_a_join() {
        let t = Timeline::new();
        t.push(piece(0, 0, Rate::ONE), true);
        let mut at = 0u64;
        let mut last = -1i128;
        for (k, (num, den)) in RATES.into_iter().skip(1).take(PIECES - 2).enumerate() {
            for _ in 0..500 {
                let r = t.running(Timestamp(at));
                assert!(r >= last, "piece {k} at {at}, {r} after {last}");
                last = r;
                at += 333_333;
            }
            append(&t, at, rate(num, den));
        }
        assert_eq!(t.running(Timestamp(333_333)), 333_333);
        assert_eq!(t.stream(Timestamp(333_333)), Timestamp(333_333));
        assert!(t.stream(Timestamp::NONE).is_none());
    }

    #[test]
    fn a_piece_is_continuous_with_the_one_before_it() {
        let t = Timeline::new();
        t.push(piece(1_000, 1_000, Rate::ONE), true);
        append(&t, 4_000, rate(3, 2));
        append(&t, 10_000, rate(3, 4));
        assert_eq!(t.find(Timestamp::MAX, true), piece(10_000, 8_000, rate(3, 4)));
        let running = |at: u64| t.running(Timestamp(at));
        assert_eq!([running(1_000), running(3_999), running(4_000)], [1_000, 3_999, 4_000]);
        assert_eq!([running(7_000), running(9_999), running(10_000)], [6_000, 7_999, 8_000]);
        assert_eq!(running(13_000), 12_000);
        let stream = |at: u64| t.stream(Timestamp(at)).0 as i128;
        assert_eq!(
            [stream(3_999), stream(4_000), stream(6_000), stream(8_000)],
            [3_999, 4_000, 7_000, 10_000]
        );
        assert_eq!(stream(12_000), 13_000);
        append(&t, 10_000, rate(2, 1));
        assert_eq!((running(10_000), running(12_000)), (8_000, 9_000));
    }

    #[test]
    fn a_time_before_the_oldest_piece_maps_through_the_oldest() {
        let t = Timeline::new();
        t.push(piece(1_000, 1_000, rate(2, 1)), true);
        assert_eq!(t.running(Timestamp(0)), 500);
        assert_eq!(t.running(Timestamp(600)), 800);
        assert_eq!(clamp(t.running(Timestamp(0)) - 501), Timestamp::ZERO);
        for k in 1..=PIECES as u64 {
            append(&t, 1_000 * (k + 1), Rate::ONE);
        }
        let oldest = t.find(Timestamp(0), true);
        assert_eq!(
            oldest.stream_start,
            Timestamp(3_000),
            "the restart and the piece behind it are out of reach"
        );
        assert_eq!(t.find(Timestamp(0), false), oldest);
        t.push(piece(50, 50, rate(1, 2)), true);
        assert_eq!(t.running(Timestamp(40)), 30);
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;

    const STEP: u64 = 1_000;

    fn piece(k: u64) -> Piece {
        let rate = Rate::new(1, 2).unwrap();
        Piece { stream_start: Timestamp(k * STEP), running_start: Timestamp(k * STEP * 2), rate }
    }

    fn read(t: &Timeline, at: u64, last: &mut i128) {
        let p = t.find(Timestamp(at), true);
        assert_eq!(p, piece(p.stream_start.0 / STEP), "a torn piece");
        let r = p.running(at);
        assert_eq!(r, at as i128 * 2, "{at} mapped off the line every piece is on");
        assert!(r >= *last, "the map went back in running time");
        *last = r;
    }

    #[test]
    fn loom_timeline_two_readers_never_see_a_torn_piece_or_go_back() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(2);
        model.check(|| {
            let t = Arc::new(Timeline::new());
            t.push(piece(0), true);
            let readers: Vec<_> = (0..2u64)
                .map(|n| {
                    let t = Arc::clone(&t);
                    loom::thread::spawn(move || {
                        let mut last = -1;
                        for at in [n * 300, STEP + n] {
                            read(&t, at, &mut last);
                        }
                    })
                })
                .collect();
            for k in 1..=2 {
                t.push(piece(k), false);
            }
            readers.into_iter().for_each(|r| r.join().unwrap());
        });
    }
}
