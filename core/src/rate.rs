//! What a demuxer tells its source of the stream's bitrate, read off its index or measured.

use crate::ctx::Ctx;
use crate::feedback::{Bitrate, Feedback, RateKind};
use crate::id::SinkPad;
use crate::pipeline::Index;
use crate::time::Timestamp;

/// How far past the reader a local rate looks.
pub const WINDOW: Timestamp = Timestamp::from_secs(10);
/// How often a demuxer says the rate again as it reads on.
pub const SAY_EVERY: Timestamp = Timestamp::from_secs(2);
const SPAN_MIN: u64 = 1_000_000_000;
/// How far apart the marks of a measured rate are, and how many it keeps.
const MARK_EVERY: u64 = 500_000_000;
const MARKS: usize = 8;

fn bps(bytes: u64, ns: u64) -> Option<u64> {
    let rate = (u128::from(bytes) * 8 * 1_000_000_000).checked_div(u128::from(ns))?;
    (ns >= SPAN_MIN && rate > 0).then_some(rate.min(u128::from(u64::MAX)) as u64)
}

/// The rate at stream time `at`, the slope of the index over the window ahead, else length over
/// duration.
pub fn of_index(index: &Index, at: Timestamp) -> Option<Bitrate> {
    let (ns, e) = (at.nanos().unwrap_or(0), &index.entries);
    let last = e.len().saturating_sub(1);
    let from = e.partition_point(|x| x.time_ns <= ns).saturating_sub(1);
    let to = e.partition_point(|x| x.time_ns < ns.saturating_add(WINDOW.0)).min(last);
    // Past the last entry but one, nothing lies ahead to read a slope over.
    let (from, to) = match to > from {
        true => (from, to),
        false => (
            e.partition_point(|x| x.time_ns.saturating_add(WINDOW.0) < ns)
                .min(last.saturating_sub(1)),
            last,
        ),
    };
    let local = match (e.get(from), e.get(to)) {
        (Some(a), Some(b)) if b.byte > a.byte && b.time_ns > a.time_ns => {
            bps(b.byte - a.byte, b.time_ns - a.time_ns)
        }
        _ => None,
    };
    match local {
        Some(bits_per_second) => Some(Bitrate { bits_per_second, kind: RateKind::Local }),
        None => average(index),
    }
}

pub fn average(index: &Index) -> Option<Bitrate> {
    let bits_per_second = bps(index.file_len?, index.duration.nanos()?)?;
    Some(Bitrate { bits_per_second, kind: RateKind::Average })
}

/// When a demuxer says its rate next, at its first output, behind a flush and every [`SAY_EVERY`].
#[derive(Clone, Copy, Default, Debug)]
pub struct Teller {
    next: Option<u64>,
}

impl Teller {
    /// Say `rate` at where this pass's output stands, up `pad`, when it is due.
    pub fn tell(
        &mut self,
        ctx: &mut Ctx,
        pad: SinkPad,
        rate: impl FnOnce(Timestamp) -> Option<Bitrate>,
    ) {
        let at = ctx.newest_out();
        self.tell_at(ctx, pad, at, rate);
    }

    pub fn tell_at(
        &mut self,
        ctx: &mut Ctx,
        pad: SinkPad,
        at: Timestamp,
        rate: impl FnOnce(Timestamp) -> Option<Bitrate>,
    ) {
        let Some(at) = at.nanos() else { return };
        if self.next.is_some_and(|next| at < next) {
            return;
        }
        self.next = Some(at.saturating_add(SAY_EVERY.0));
        if let Some(b) = rate(Timestamp(at)) {
            ctx.publish_feedback(pad, Feedback::Bitrate(b));
        }
    }

    /// Behind a flush. The slot told the old stretch's rate, so the next output says it again.
    pub fn restart(&mut self) {
        self.next = None;
    }
}

/// A rate measured off the bytes a demuxer took in and how far its output went.
#[derive(Clone, Copy, Default, Debug)]
pub struct ByteRate {
    marks: [(u64, u64); MARKS],
    len: usize,
    head: usize,
}

impl ByteRate {
    /// `bytes` taken in so far, and the newest stamp put out `at`. Half a second apart at most.
    pub fn note(&mut self, bytes: u64, at: Timestamp) {
        let Some(ns) = at.nanos() else { return };
        if self.len > 0 {
            let (_, last) = self.marks[(self.head + self.len - 1) % MARKS];
            if ns < last.saturating_add(MARK_EVERY) {
                return;
            }
        }
        match self.len < MARKS {
            true => {
                self.marks[(self.head + self.len) % MARKS] = (bytes, ns);
                self.len += 1;
            }
            false => {
                self.marks[self.head] = (bytes, ns);
                self.head = (self.head + 1) % MARKS;
            }
        }
    }

    pub fn rate(&self) -> Option<Bitrate> {
        let (first, last) =
            (self.marks[self.head], self.marks[(self.head + self.len.max(1) - 1) % MARKS]);
        let bits_per_second = bps(last.0.checked_sub(first.0)?, last.1.checked_sub(first.1)?)?;
        Some(Bitrate { bits_per_second, kind: RateKind::Local })
    }

    pub fn clear(&mut self) {
        *self = ByteRate::default();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::pipeline::IndexEntry;

    const S: u64 = 1_000_000_000;

    // Test fixtures allocate freely.
    #[allow(clippy::disallowed_methods)]
    fn index(entries: &[(u64, u64)], len: Option<u64>, secs: u64) -> Index {
        let entries = entries.iter().map(|&(t, b)| IndexEntry::key(t * S, b)).collect();
        Index { entries, file_len: len, duration: Timestamp(secs * S), ..Index::default() }
    }

    #[test]
    fn an_index_says_its_slope_ahead_of_the_reader_and_its_average_without_entries() {
        // 1 MB a second for 20 s, then 4 MB a second.
        let steps: Vec<(u64, u64)> = (0..=40)
            .map(|t| (t, if t <= 20 { t } else { 20 + 4 * (t - 20) } * 1_000_000))
            .collect();
        let idx = index(&steps, Some(100_000_000), 40);
        assert_eq!(
            of_index(&idx, Timestamp(5 * S)),
            Some(Bitrate { bits_per_second: 8_000_000, kind: RateKind::Local })
        );
        assert_eq!(
            of_index(&idx, Timestamp(25 * S)).map(|b| b.bits_per_second),
            Some(32_000_000),
            "the swing ahead"
        );
        let end = of_index(&idx, Timestamp(40 * S));
        assert_eq!(
            end,
            Some(Bitrate { bits_per_second: 32_000_000, kind: RateKind::Local }),
            "the slope behind at the end"
        );
        let bare = index(&[], Some(10_000_000), 40);
        assert_eq!(
            of_index(&bare, Timestamp::ZERO),
            Some(Bitrate { bits_per_second: 2_000_000, kind: RateKind::Average })
        );
        assert_eq!(
            of_index(&index(&[], None, 40), Timestamp::ZERO),
            None,
            "nothing to read one off"
        );
    }

    #[test]
    fn a_measured_rate_needs_a_second_and_keeps_the_last_few() {
        let mut r = ByteRate::default();
        r.note(0, Timestamp::ZERO);
        r.note(10, Timestamp(S / 10));
        assert_eq!(r.rate(), None, "under a second");
        for k in 1..=20 {
            r.note(k * 125_000 / 2, Timestamp(k * S / 2));
        }
        assert_eq!(r.rate().map(|b| b.bits_per_second), Some(1_000_000));
        for k in 21..=40 {
            r.note(20 * 125_000 / 2 + (k - 20) * 125_000, Timestamp(k * S / 2));
        }
        assert_eq!(r.rate().map(|b| b.bits_per_second), Some(2_000_000));
        r.clear();
        assert_eq!(r.rate(), None);
    }
}
