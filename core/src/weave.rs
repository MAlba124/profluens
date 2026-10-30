//! A tail's time-limited links grow to the interleave of its tracks, so a far-ahead track never
//! starves another.

use crate::level::Limit;
use crate::sched::DownLink;
use crate::time::Timestamp;

/// The most a tail's links grow to. Past it a far-apart file rests the tail.
pub const MAX_INTERLEAVE: Timestamp = Timestamp::from_secs(5);
/// What a full link grows by while a sibling starves.
pub(crate) const GROW: Timestamp = Timestamp::from_millis(100);
/// The progress the least link makes before the limit lowers, at most twice this.
const SETTLE: Timestamp = Timestamp::from_secs(1);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Full {
    Rest,
    Pass,
    /// A sibling starves and it is at the cap, holding this much. Said once a stretch.
    Capped(Timestamp),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Weave {
    pub(crate) timed: usize,
    /// What every timed link holds past its own limit, `ZERO` while nothing grew.
    pub(crate) value: Timestamp,
    pub(crate) at: Timestamp,
    warned: bool,
}

impl Default for Weave {
    fn default() -> Weave {
        Weave { timed: 0, value: Timestamp::ZERO, at: Timestamp::NONE, warned: false }
    }
}

pub(crate) fn timed(d: &DownLink) -> bool {
    d.limited && d.base.is_some() && !d.sparse
}

/// A timed link drained whose next data lies behind what `full` holds past its own limit.
fn starving(d: &DownLink, full: &DownLink) -> bool {
    let empty = timed(d)
        && d.staged.is_none()
        && !d.level.ended()
        && !d.data.is_closed()
        && d.level.held().bytes == 0;
    let (at, ahead) = (d.level.fed_to(), full.level.fed_to());
    empty && (at.is_none() || at.saturating_add(full.base) < ahead)
}

impl Weave {
    /// Whether a timed link starves behind another that holds data.
    pub(crate) fn behind(&self, links: &[DownLink]) -> bool {
        let fed = |d: &&DownLink| timed(d) && d.level.held().bytes > 0;
        self.timed >= 2 && links.iter().filter(fed).any(|d| links.iter().any(|o| starving(o, d)))
    }

    pub(crate) fn recount(&mut self, links: &[DownLink]) {
        self.timed = links.iter().filter(|d| timed(d)).count();
    }

    pub(crate) fn restart(&mut self) {
        (self.at, self.warned) = (Timestamp::NONE, false);
    }

    pub(crate) fn limit_of(&self, d: &DownLink) -> Timestamp {
        d.base.saturating_add(self.value).min(MAX_INTERLEAVE.max(d.base))
    }

    pub(crate) fn join(&self, d: &DownLink) {
        if timed(d) {
            d.level.set_limit(Limit { time: self.limit_of(d), ..d.level.limit() });
        }
    }

    fn set(&mut self, value: Timestamp, at: Timestamp, links: &[DownLink]) {
        (self.value, self.at) = (value.min(MAX_INTERLEAVE), at);
        links.iter().for_each(|d| self.join(d));
    }

    /// Whether the tail rests on full link `j`. A timed one grows while a sibling starves.
    pub(crate) fn full(&mut self, j: usize, links: &[DownLink]) -> Full {
        let d = &links[j];
        let held = d.level.held();
        if held.bytes >= d.level.limit().bytes {
            return Full::Rest;
        }
        if d.sparse {
            return Full::Pass;
        }
        let sibling = || links.iter().enumerate().any(|(k, o)| k != j && starving(o, d));
        if self.timed < 2 || !timed(d) || !sibling() {
            return Full::Rest;
        }
        let to = held.time.saturating_add(GROW);
        if to > MAX_INTERLEAVE.max(d.base) {
            return match std::mem::replace(&mut self.warned, true) {
                false => Full::Capped(held.time),
                true => Full::Rest,
            };
        }
        self.set(self.value.max(to.saturating_sub(d.base)), Timestamp::NONE, links);
        Full::Pass
    }

    /// Measures the links' spread, raising at once and lowering under three quarters (multiqueue's
    /// rule).
    pub(crate) fn measure(&mut self, links: &[DownLink]) {
        if self.timed < 2 {
            return;
        }
        let (mut low, mut high, mut pressed) = (Timestamp::NONE, Timestamp::ZERO, false);
        for d in links.iter().filter(|d| timed(d) && !d.level.ended()) {
            let to = d.level.fed_to();
            if to.is_none() {
                return;
            }
            if to >= high {
                (high, pressed) = (to, d.level.full());
            }
            low = low.min(to);
        }
        if low.is_none() {
            return;
        }
        let want = Timestamp((high.0 - low.0).saturating_mul(3) / 2).min(MAX_INTERLEAVE);
        if want > self.value && pressed {
            self.set(want, low, links);
        } else if self.at.is_none() {
            self.at = low;
        } else if want.0 < self.value.0 / 4 * 3
            && low.0 >= self.at.0.saturating_add(2 * self.value.min(SETTLE).0)
        {
            self.set(want, low, links);
        }
    }
}
