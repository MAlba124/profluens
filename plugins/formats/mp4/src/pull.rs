//! The samples a pulled `mp4demux` asked for, in file order. Each sample or run of small
//! neighbours is one range read, served in any order and paired by tag.

use std::collections::VecDeque;

use profluens_core::memory::Memory;
use profluens_core::pull::RangeError;

use crate::reader::Sample;

pub(crate) const SMALL: u32 = 64 << 10;
pub(crate) const RUN: u32 = 256 << 10;

pub(crate) enum State {
    Waiting,
    /// `true` when the buffer is the sample's own and may be reframed in place.
    Ready(Memory, bool),
    Empty,
    Dropped,
}

pub(crate) struct Planned {
    pub(crate) index: usize,
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
}

impl Plan {
    pub(crate) fn reset(&mut self) -> Option<usize> {
        let first = self.items.front().map(|p| p.index);
        self.items.clear();
        (self.held, self.ranges, self.asked_all) = (0, 0, false);
        first
    }

    pub(crate) fn push(&mut self, index: usize, sample: Sample, at: u32) {
        let state = if sample.size == 0 { State::Empty } else { State::Waiting };
        self.items.push_back(Planned { index, sample, tag: self.next_tag, at, last_of: 0, state });
    }

    pub(crate) fn asked(&mut self, charge: u32) {
        let last = self.items.back_mut().filter(|p| p.tag == self.next_tag && charge > 0);
        if let Some(last) = last {
            last.last_of = charge;
            (self.held, self.ranges) = (self.held + charge as u64, self.ranges + 1);
        }
        self.next_tag += 1;
    }

    pub(crate) fn unplan(&mut self) -> Option<usize> {
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
        let (want, got) = (last.at + last.sample.size, buf.len() as u32);
        let offset = self.items[from].sample.offset;
        if count == 1 && got == want {
            self.items[from].state = State::Ready(buf, true);
            return Ok(());
        }
        for p in self.items.range_mut(from..from + count).filter(|p| p.sample.size > 0) {
            let end = p.at as usize + p.sample.size as usize;
            p.state = match buf.slice(p.at as usize, p.sample.size as usize) {
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

    fn sample(offset: u64, size: u32) -> Sample {
        Sample { track_index: 0, offset, size, dts: 0, pts: 0, sync: true, hidden: false }
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
        plan.push(0, sample(100, 3), 0);
        plan.asked(3);
        plan.push(1, sample(103, 2), 0);
        plan.push(2, sample(105, 0), 2);
        plan.push(3, sample(105, 4), 2);
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

    #[test]
    fn a_short_range_drops_what_it_cut_and_says_where() {
        let pool = Pool::new(4096);
        let mut plan = Plan::default();
        plan.push(0, sample(10, 4), 0);
        plan.push(1, sample(14, 4), 4);
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
            plan.push(i, sample(10, 0), 0);
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
        plan.push(7, sample(10, 4), 0);
        plan.asked(4);
        plan.push(8, sample(14, 4), 0);
        assert_eq!(plan.unplan(), Some(8), "a range not asked walks back");
        assert_eq!(plan.reset(), Some(7));
        plan.served(0, filled(&pool, b"abcd")).expect("a late range is no error");
        assert!(plan.pop_ready().is_none() && plan.items.is_empty());
    }
}
