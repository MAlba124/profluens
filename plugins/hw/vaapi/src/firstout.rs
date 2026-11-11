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
        if std::mem::take(&mut self.fresh) {
            self.held = ctx.group_out().map(|n| (n, Instant::now()));
        }
    }

    /// Whether the next decode waits, a wake asked for when it does.
    pub(crate) fn holds(&mut self, ctx: &mut Ctx) -> bool {
        let Some((n, at)) = self.held else {
            return false;
        };
        if ctx.group_out() == Some(n) && at.elapsed() < WAIT {
            ctx.wake_within(crate::enc::POLL_WAKE);
            return true;
        }
        self.held = None;
        false
    }
}
