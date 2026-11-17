//! The samples a pulled `mp4demux` asked for, in file order. A sample and the small neighbours
//! behind it are one range read, served in any order and paired by tag. A source that reads in
//! order takes each range one turn through the reactor, so there the ranges are few and long.

use std::collections::VecDeque;

use profluens_core::memory::Memory;
use profluens_core::pull::RangeError;

use crate::reader::{Sample, SampleId};

/// The largest sample that shares a range and is copied out of it. A larger one is read alone
/// and reframed where it lies.
pub(crate) const SMALL: u32 = 64 << 10;
/// The most a range of several samples reads.
pub(crate) const RUN: u32 = 256 << 10;
/// A source that reads in order is asked ranges of at most one part in this many of what was
/// asked since the restart, where the samples are copied out of them. The reads behind a seek
/// stay the size of its samples, so the next seek waits for no more than it did, and a long read
/// comes to [`RUN`] within 16 MiB. With a part in 8 a USB disk read three times the bytes behind
/// a seek, and a seek 150 ms after it took 16 ms against 14.
const RAMP: u64 = 64;
/// The most bytes between two samples of a range a source reads in order, a track that is off
/// stored there. Reading them costs less than a read of its own.
pub(crate) const GAP: u32 = 64 << 10;

pub(crate) enum State {
    Waiting,
    /// `true` when the buffer is the sample's own and may be reframed in place.
    Ready(Memory, bool),
    Empty,
    Dropped,
}

pub(crate) struct Planned {
    pub(crate) index: SampleId,
    pub(crate) sample: Sample,
    pub(crate) tag: u64,
    pub(crate) at: u32,
    /// The pool charge of its range when it is the last sample of it, else zero.
    pub(crate) last_of: u32,
    pub(crate) state: State,
}

#[derive(Default)]
pub(crate) struct Plan {
    pub(crate) items: VecDeque<Planned>,
    pub(crate) next_tag: u64,
    pub(crate) held: u64,
    pub(crate) ranges: u32,
    pub(crate) asked_all: bool,
    pub(crate) warned: bool,
    /// The pool charge of the ranges asked since the last reset.
    asked: u64,
}

impl Plan {
    pub(crate) fn reset(&mut self) -> Option<SampleId> {
        let first = self.items.front().map(|p| p.index);
        self.items.clear();
        (self.held, self.ranges, self.asked_all, self.asked) = (0, 0, false, 0);
        first
    }

    /// The most a range read in order and copied out of reads now, see [`RAMP`].
    pub(crate) fn run(&self) -> u32 {
        (self.asked / RAMP).min(u64::from(RUN)) as u32
    }

    /// Plans `first` and the samples behind it that share its range, each `small`, no more than
    /// `gap` past the one before and the range within `room`. Gives the range's length and the
    /// sample taken from `next` that did not join, for the walk to go back to.
    pub(crate) fn join(
        &mut self,
        (index, first): (SampleId, Sample),
        (room, gap): (u32, u32),
        small: impl Fn(&Sample) -> bool,
        mut next: impl FnMut() -> Option<(SampleId, Sample)>,
    ) -> (u32, Option<SampleId>) {
        self.push(index, first, 0);
        let room = u64::from(room);
        let mut len = first.size();
        if !small(&first) {
            return (len, None);
        }
        while let Some((i, s)) = next() {
            let past = s.offset().checked_sub(first.offset() + u64::from(len));
            let at = match past {
                Some(past) if past <= u64::from(gap) && small(&s) => u64::from(len) + past,
                _ => return (len, Some(i)),
            };
            if at + u64::from(s.size()) > room {
                return (len, Some(i));
            }
            self.push(i, s, at as u32);
            len = at as u32 + s.size();
        }
        (len, None)
    }

    pub(crate) fn push(&mut self, index: SampleId, sample: Sample, at: u32) {
        let state = if sample.size() == 0 { State::Empty } else { State::Waiting };
        self.items.push_back(Planned { index, sample, tag: self.next_tag, at, last_of: 0, state });
    }

    pub(crate) fn asked(&mut self, charge: u32) {
        let last = self.items.back_mut().filter(|p| p.tag == self.next_tag && charge > 0);
        if let Some(last) = last {
            last.last_of = charge;
            (self.held, self.ranges) = (self.held + charge as u64, self.ranges + 1);
            self.asked += u64::from(charge);
        }
        self.next_tag += 1;
    }

    pub(crate) fn unplan(&mut self) -> Option<SampleId> {
        let mut first = None;
        while self.items.back().is_some_and(|p| p.tag == self.next_tag) {
            first = self.items.pop_back().map(|p| p.index);
        }
        first
    }

    pub(crate) fn drop_range(&mut self) {
        for p in self.items.iter_mut().rev().take_while(|p| p.tag == self.next_tag) {
            p.state = State::Dropped;
        }
        self.next_tag += 1;
    }

    pub(crate) fn served(&mut self, tag: u64, buf: Memory) -> Result<(), RangeError> {
        let from = self.items.partition_point(|p| p.tag < tag);
        let count = self.items.range(from..).take_while(|p| p.tag == tag).count();
        let Some(last) = count.checked_sub(1).map(|k| &self.items[from + k]) else {
            return Ok(());
        };
        let (want, got) = (last.at + last.sample.size(), buf.len() as u32);
        let offset = self.items[from].sample.offset();
        if count == 1 && got == want {
            self.items[from].state = State::Ready(buf, true);
            return Ok(());
        }
        for p in self.items.range_mut(from..from + count).filter(|p| p.sample.size() > 0) {
            let end = p.at as usize + p.sample.size() as usize;
            p.state = match buf.slice(p.at as usize, p.sample.size() as usize) {
                Some(view) if end <= got as usize => State::Ready(view, false),
                _ => State::Dropped,
            };
        }
        if got < want {
            return Err(RangeError::Short { offset, want, got });
        }
        Ok(())
    }

    pub(crate) fn pop_ready(&mut self) -> Option<Planned> {
        if self.items.front().is_none_or(|p| matches!(p.state, State::Waiting)) {
            return None;
        }
        let p = self.items.pop_front()?;
        if p.last_of > 0 {
            (self.held, self.ranges) = (self.held - p.last_of as u64, self.ranges - 1);
        }
        Some(p)
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use profluens_core::memory::Pool;

    fn id(n: u32) -> SampleId {
        SampleId::new(0, 0, n)
    }

    fn sample(offset: u64, size: u32) -> Sample {
        Sample::new(0, offset, size, 0, 0, true).expect("a sample")
    }

    fn filled(pool: &Pool, bytes: &[u8]) -> Memory {
        let mut m = pool.acquire_exact(bytes.len().max(1)).expect("exact");
        m.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
        m.set_len(bytes.len());
        m
    }

    fn data(p: &Planned) -> Vec<u8> {
        match &p.state {
            State::Ready(m, _) => m.data().to_vec(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn a_run_of_small_samples_is_sliced_and_waits_for_the_ranges_before_it() {
        let pool = Pool::new(4096);
        let mut plan = Plan::default();
        plan.push(id(0), sample(100, 3), 0);
        plan.asked(3);
        plan.push(id(1), sample(103, 2), 0);
        plan.push(id(2), sample(105, 0), 2);
        plan.push(id(3), sample(105, 4), 2);
        plan.asked(6);
        assert_eq!((plan.held, plan.ranges), (9, 2));
        plan.served(1, filled(&pool, b"abcdef")).expect("whole");
        assert!(plan.pop_ready().is_none(), "the range before is not in");
        plan.served(0, filled(&pool, b"xyz")).expect("whole");
        let out: Vec<_> = std::iter::from_fn(|| plan.pop_ready()).collect();
        assert_eq!(
            out.iter().map(data).collect::<Vec<_>>(),
            [b"xyz".to_vec(), b"ab".to_vec(), Vec::new(), b"cdef".to_vec()]
        );
        assert!(
            matches!(out[0].state, State::Ready(_, true)),
            "a sample of its own range may be reframed in place"
        );
        assert!(matches!(out[1].state, State::Ready(_, false)));
        assert!(matches!(out[2].state, State::Empty));
        assert_eq!((plan.held, plan.ranges), (0, 0));
    }

    /// The range `plan` joins from the head of `walk`, its length and how much of the walk is left.
    fn joined(plan: &mut Plan, walk: &[(u64, u32)], small: u32, room: u32) -> (u32, usize) {
        let mut all =
            walk.iter().enumerate().map(|(n, &(at, size))| (id(n as u32), sample(at, size)));
        let (first, before) = (all.next().expect("a first sample"), plan.items.len());
        let (len, past) = plan.join(first, (room, GAP), |s| s.size() <= small, || all.next());
        let left = walk.len() - (plan.items.len() - before);
        assert_eq!(past.is_some(), left > 0 && walk[0].1 <= small, "one not joined is given back");
        (len, left)
    }

    #[test]
    fn samples_of_any_track_join_over_short_gaps_up_to_the_room() {
        let pool = Pool::new(1 << 16);
        let k = 1 << 10;
        let mut plan = Plan::default();
        // a gap behind the second, then one past the most read through
        let walk = [(100, 30 * k), (100 + 30 * u64::from(k), 500), (100 + 32 * u64::from(k), k)];
        let far =
            [walk.to_vec(), vec![(100 + 33 * u64::from(k) + u64::from(GAP) + 1, 10)]].concat();
        assert_eq!(joined(&mut plan, &far, SMALL, RUN), (33 * k, 1), "the far one starts its own");
        plan.asked(33 * k);
        let bytes: Vec<u8> = (0..33 * k).map(|n| (n % 251) as u8).collect();
        plan.served(0, filled(&pool, &bytes)).expect("whole");
        for (at, size) in [(0, 30 * k), (30 * k, 500), (32 * k, k)] {
            let p = plan.pop_ready().expect("ready");
            assert_eq!(data(&p), bytes[at as usize..(at + size) as usize], "the sample at {at}");
        }

        let mut plan = Plan::default();
        assert_eq!(
            joined(&mut plan, &walk, SMALL, 31 * k),
            (30 * k + 500, 1),
            "no room for the third"
        );
        let mut plan = Plan::default();
        let big = [(0, SMALL + 1), (u64::from(SMALL) + 1, 10)];
        assert_eq!(joined(&mut plan, &big, SMALL, RUN), (SMALL + 1, 1), "a large one reads alone");
        let mut plan = Plan::default();
        let behind = [(0, 10), (10, SMALL + 1)];
        assert_eq!(joined(&mut plan, &behind, SMALL, RUN), (10, 1), "and joins no range");
        let mut plan = Plan::default();
        let back = [(500, 10), (400, 10)];
        assert_eq!(joined(&mut plan, &back, SMALL, RUN), (10, 1), "nor one stored before");
        let mut plan = Plan::default();
        let (first, mut rest) = ((id(0), sample(0, 10)), [(id(1), sample(11, 10))].into_iter());
        let (len, past) = plan.join(first, (RUN, 0), |_| true, || rest.next());
        assert_eq!((len, past), (10, Some(id(1))), "nor one past a gap where none is read");
    }

    #[test]
    fn a_range_reads_a_share_of_what_its_restart_asked_before_it() {
        const FRAME: u32 = 4096;
        let frames: Vec<(u64, u32)> = (0..20_000).map(|n| (n * u64::from(FRAME), FRAME)).collect();
        let mut plan = Plan::default();
        for round in 0..2 {
            let (mut left, mut asked, mut lens) = (&frames[..], 0u64, Vec::new());
            while asked < 2 * RAMP * u64::from(RUN) {
                let run = plan.run();
                let (len, rest) = joined(&mut plan, left, SMALL, run);
                assert!(u64::from(len) <= (asked / RAMP).max(u64::from(FRAME)), "{len} at {asked}");
                plan.asked(len);
                asked += u64::from(len);
                lens.push(len);
                left = &left[left.len() - rest..];
            }
            let alone = lens.iter().take_while(|len| **len == FRAME).count();
            assert_eq!(alone as u64, 2 * RAMP, "round {round}, each alone until two fit the share");
            assert!(lens.windows(2).all(|w| w[0] <= w[1]), "round {round}");
            assert_eq!(lens.last(), Some(&RUN), "round {round}");
            plan.reset();
        }
    }

    #[test]
    fn a_short_range_drops_what_it_cut_and_says_where() {
        let pool = Pool::new(4096);
        let mut plan = Plan::default();
        plan.push(id(0), sample(10, 4), 0);
        plan.push(id(1), sample(14, 4), 4);
        plan.asked(8);
        let err = plan.served(0, filled(&pool, b"abcdef")).unwrap_err();
        assert_eq!(err, RangeError::Short { offset: 10, want: 8, got: 6 });
        let out: Vec<_> = std::iter::from_fn(|| plan.pop_ready()).collect();
        assert_eq!(data(&out[0]), b"abcd");
        assert!(matches!(out[1].state, State::Dropped));
    }

    #[test]
    fn a_sample_of_no_bytes_holds_no_range() {
        let mut plan = Plan::default();
        for i in 0..8 {
            plan.push(id(i), sample(10, 0), 0);
            plan.asked(0);
        }
        assert_eq!((plan.held, plan.ranges), (0, 0), "a range never asked is never let go");
        assert_eq!(std::iter::from_fn(|| plan.pop_ready()).count(), 8);
        assert_eq!((plan.held, plan.ranges), (0, 0));
    }

    #[test]
    fn a_reset_forgets_the_plan_and_a_late_range_is_dropped() {
        let pool = Pool::new(4096);
        let mut plan = Plan::default();
        plan.push(id(7), sample(10, 4), 0);
        plan.asked(4);
        plan.push(id(8), sample(14, 4), 0);
        assert_eq!(plan.unplan(), Some(id(8)), "a range not asked walks back");
        assert_eq!(plan.reset(), Some(id(7)));
        plan.served(0, filled(&pool, b"abcd")).expect("a late range is no error");
        assert!(plan.pop_ready().is_none() && plan.items.is_empty());
    }
}
