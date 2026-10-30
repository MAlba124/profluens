//! Frame QoS for clock-paced video sinks. A frame over one frame late at the front is dropped,
//! unless the picture has stood for [`STALL`]. Drops and the return to on time are told upstream.

use profluens_core::ctx::Ctx;
use profluens_core::feedback::{Feedback, Qos};
use profluens_core::id::SinkPad;
use profluens_core::time::Timestamp;

/// How long the picture may stand while late frames are dropped before one is shown anyway.
pub const STALL: Timestamp = Timestamp::from_millis(100);

/// More running time than this between two frames is a gap, not a frame.
const MAX_PERIOD: Timestamp = Timestamp::from_millis(250);

#[derive(Clone, Copy, Debug)]
pub struct FrameQos {
    last_at: Timestamp,
    period: Timestamp,
    shown_at: Timestamp,
    told_late: bool,
}

impl Default for FrameQos {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameQos {
    pub const fn new() -> Self {
        Self {
            last_at: Timestamp::NONE,
            period: Timestamp::NONE,
            shown_at: Timestamp::NONE,
            told_late: false,
        }
    }

    /// Judge the frame at the front once. How late it is when dropped, `None` when shown.
    /// `fps_ns` is a frame of the link's fps, zero for none.
    pub fn late(&mut self, ctx: &mut Ctx, pts: Timestamp, fps_ns: u64) -> Option<Timestamp> {
        if pts.is_none() {
            return None;
        }
        let at = ctx.running_time(pts);
        let spacing = match self.last_at.is_some() && at > self.last_at {
            true => Some(at.saturating_sub(self.last_at)).filter(|d| *d <= MAX_PERIOD),
            false => None,
        };
        self.last_at = at;
        if let Some(spacing) = spacing {
            self.period = spacing;
        }
        let period = match (self.period.is_some(), fps_ns > 0) {
            (true, _) => self.period,
            (false, true) => Timestamp(fps_ns),
            (false, false) => return None,
        };
        let lateness = ctx.lateness(pts).filter(|l| *l > period);
        self.tell(ctx, at, lateness);
        let lateness = lateness?;
        (!self.stood(ctx)).then_some(lateness)
    }

    fn tell(&mut self, ctx: &mut Ctx, at: Timestamp, lateness: Option<Timestamp>) {
        if lateness.is_none() && !self.told_late {
            return;
        }
        self.told_late = lateness.is_some();
        let qos = match lateness {
            Some(l) => {
                Qos { lateness: l.0.min(i64::MAX as u64) as i64, earliest: at.saturating_add(l) }
            }
            None => Qos { lateness: 0, earliest: Timestamp::ZERO },
        };
        ctx.publish_feedback(SinkPad(0), Feedback::Qos(qos));
    }

    pub fn period(&self) -> Timestamp {
        self.period
    }

    pub fn stood(&self, ctx: &Ctx) -> bool {
        let now = ctx.now();
        self.shown_at.is_some() && now.is_some() && now.saturating_sub(self.shown_at) >= STALL
    }

    /// A preroll is not judged, so the spacing counts from it.
    pub fn shown(&mut self, ctx: &Ctx, pts: Timestamp, preroll: bool) {
        if preroll {
            self.last_at = ctx.running_time(pts);
        }
        self.shown_at = ctx.now();
    }

    pub fn reset(&mut self) {
        (self.last_at, self.shown_at) = (Timestamp::NONE, Timestamp::NONE);
    }
}
