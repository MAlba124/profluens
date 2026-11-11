use std::time::{Duration, Instant};

use profluens_core::ctx::Ctx;

/// The longest the first picture holds the next decode back, an encoder behind it that wants
/// more pictures before its first would wait for good.
const WAIT: Duration = Duration::from_millis(250);

/// A decoder's first picture after its start or a flush, the one a seek waits for. Until the
/// decoder's group puts out more no access unit is decoded. On iHD a decode submitted before
/// that picture's encode is done runs ahead of it, 20 ms a seek of a 1080p AV1 film.
pub(crate) struct FirstOut {
    fresh: bool,
    /// The group's output count as the picture went out and when.
    held: Option<(u64, Instant)>,
}

impl FirstOut {
    pub(crate) const fn new() -> Self {
        FirstOut { fresh: true, held: None }
    }

    /// The stream starts over, a flush or a stop.
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// No picture went out since the reset.
    pub(crate) fn fresh(&self) -> bool {
        self.fresh
    }

    /// A picture went out.
    pub(crate) fn sent(&mut self, ctx: &Ctx) {
        self.sent_at(ctx.group_out(), crate::stats::mark());
    }

    /// Whether the next decode waits, a wake asked for when it does.
    pub(crate) fn holds(&mut self, ctx: &mut Ctx) -> bool {
        let holds = self.holds_at(ctx.group_out(), crate::stats::mark());
        if holds {
            ctx.wake_within(crate::enc::POLL_WAKE);
        }
        holds
    }

    fn sent_at(&mut self, out: Option<u64>, now: Instant) {
        if std::mem::take(&mut self.fresh) {
            self.held = out.map(|n| (n, now));
        }
    }

    fn holds_at(&mut self, out: Option<u64>, now: Instant) -> bool {
        let Some((n, at)) = self.held else {
            return false;
        };
        if out == Some(n) && now.duration_since(at) < WAIT {
            return true;
        }
        self.held = None;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_until_the_group_puts_out() {
        let t = crate::stats::mark();
        let mut g = FirstOut::new();
        assert!(g.fresh() && !g.holds_at(Some(4), t));
        g.sent_at(Some(4), t);
        assert!(!g.fresh());
        assert!(g.holds_at(Some(4), t + WAIT / 2));
        assert!(!g.holds_at(Some(5), t + WAIT / 2));
        // let go for good, the count standing still again holds nothing
        assert!(!g.holds_at(Some(5), t + WAIT / 2));
    }

    #[test]
    fn only_the_first_picture_holds() {
        let t = crate::stats::mark();
        let mut g = FirstOut::new();
        g.sent_at(Some(1), t);
        assert!(!g.holds_at(Some(2), t));
        g.sent_at(Some(2), t);
        assert!(!g.holds_at(Some(2), t));
    }

    #[test]
    fn a_stuck_group_is_let_go_after_the_wait() {
        let t = crate::stats::mark();
        let mut g = FirstOut::new();
        g.sent_at(Some(0), t);
        assert!(g.holds_at(Some(0), t + WAIT - Duration::from_millis(1)));
        assert!(!g.holds_at(Some(0), t + WAIT));
        assert!(!g.holds_at(Some(0), t));
    }

    #[test]
    fn nothing_holds_outside_a_run() {
        let t = crate::stats::mark();
        let mut g = FirstOut::new();
        g.sent_at(None, t);
        assert!(!g.holds_at(None, t));
    }

    #[test]
    fn a_reset_arms_the_next_first_picture() {
        let t = crate::stats::mark();
        let mut g = FirstOut::new();
        g.sent_at(Some(3), t);
        g.reset();
        assert!(g.fresh() && !g.holds_at(Some(3), t));
        g.sent_at(Some(7), t);
        assert!(g.holds_at(Some(7), t));
    }
}
