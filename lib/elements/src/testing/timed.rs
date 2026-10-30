//! Timed test elements. [`TimedTestSink`] renders on the pipeline clock, so a
//! `MockClock` drives hours of virtual time without sleeping.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::clock::Clock;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

static SRC_DESC: ElementDesc = ElementDesc {
    name: "timedtestsrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// Emits `count` buffers with `pts = i * period`, then EOS. The payload is the index.
pub struct TimedTestSrc {
    count: u64,
    period: Timestamp,
    produced: u64,
}

impl TimedTestSrc {
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new(count: u64, period: Timestamp) -> Self {
        Self { count, period, produced: 0 }
    }
}

impl Element for TimedTestSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.produced >= self.count {
            return Ok(Flow::Eos);
        }
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok), // pool full, backpressure
        };
        let idx = self.produced;
        let bytes = idx.to_le_bytes();
        let dst = buf.memory.as_mut_full();
        let n = bytes.len().min(dst.len());
        dst[..n].copy_from_slice(&bytes[..n]);
        buf.memory.set_len(n);
        buf.pts = Timestamp::from_nanos(self.period.0.saturating_mul(idx));
        buf.duration = self.period;
        self.produced += 1;
        ctx.out(Self::OUT).push(buf);
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart)
            && let Some(t) = ctx.seek_target()
            && let (Some(ns), Some(period)) = (t.at.nanos(), self.period.nanos())
            && period > 0
        {
            self.produced = ns / period;
            log!(&*ctx, Level::Debug, "seek", at = t.at, index = self.produced);
        }
        Ok(())
    }
}

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

static SINK_DESC: ElementDesc = ElementDesc {
    name: "timedtestsink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// One rendered buffer and the running time it was released at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Render {
    pub pts: Timestamp,
    pub rendered_at: Timestamp,
}

struct SinkShared {
    // Locking per buffer breaks rule 8. Fine for a test sink only.
    renders: Mutex<Vec<Render>>,
    count: AtomicU64,
    done: AtomicBool,
}

pub struct TimedSinkStats(Arc<SinkShared>);

impl TimedSinkStats {
    pub fn renders(&self) -> Vec<Render> {
        self.0.renders.lock().unwrap().clone()
    }

    pub fn count(&self) -> u64 {
        self.0.count.load(Ordering::Acquire)
    }

    pub fn is_done(&self) -> bool {
        self.0.done.load(Ordering::Acquire)
    }
}

/// An active sink that holds each buffer until `ctx.due(pts)`, pacing the chain.
pub struct TimedTestSink {
    shared: Arc<SinkShared>,
    provided: Option<Arc<dyn Clock>>,
}

impl TimedTestSink {
    pub const IN: SinkPad = SinkPad(0);

    // Cold, one-time constructor of the shared record store.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> (Self, TimedSinkStats) {
        let shared = Arc::new(SinkShared {
            renders: Mutex::new(Vec::new()),
            count: AtomicU64::new(0),
            done: AtomicBool::new(false),
        });
        let stats = TimedSinkStats(Arc::clone(&shared));
        (Self { shared, provided: None }, stats)
    }

    /// Like [`new`](Self::new), but the sink also provides `clock` to the pipeline.
    pub fn providing(clock: Arc<dyn Clock>) -> (Self, TimedSinkStats) {
        let (mut sink, stats) = Self::new();
        sink.provided = Some(clock);
        (sink, stats)
    }
}

impl Element for TimedTestSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn provide_clock(&mut self) -> Option<Arc<dyn Clock>> {
        self.provided.clone()
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(front) = inputs.front() {
            if ctx.before_presentation(front.pts, Timestamp::ZERO) {
                log!(&*ctx, Level::Trace, "late", pts = front.pts);
                ctx.count_drop();
                inputs.pop();
                continue;
            }
            // `due` arms the wake that brings this sink back at the deadline.
            if !ctx.due(front.pts) {
                break;
            }
            let Some(buf) = inputs.pop() else { break };
            let render = Render { pts: buf.pts, rendered_at: ctx.now() };
            self.shared.renders.lock().unwrap().push(render);
            self.shared.count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(Flow::Ok)
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        log!(&*ctx, Level::Debug, "stop", rendered = self.shared.count.load(Ordering::Acquire));
        self.shared.done.store(true, Ordering::Release);
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use pf_harness::Harness;
    use profluens_core::buffer::Buffer;

    const MS: fn(u64) -> Timestamp = Timestamp::from_millis;

    fn stamped(h: &mut Harness, ms: u64) -> Buffer {
        let mut buf = h.alloc(&[0; 8]);
        buf.pts = MS(ms);
        buf
    }

    #[test]
    fn the_sink_drops_what_is_stamped_before_the_presentation_point() {
        let (sink, stats) = TimedTestSink::new();
        let mut h = Harness::new(sink);
        for ms in [10, 20] {
            let buf = stamped(&mut h, ms);
            h.push(buf).unwrap();
        }
        assert_eq!((h.input_len(), stats.count()), (2, 0), "both wait for the clock");
        h.clock().advance(MS(25));
        h.skip_to(MS(25)).unwrap();
        for ms in [30, 40] {
            let buf = stamped(&mut h, ms);
            h.push(buf).unwrap();
        }
        assert_eq!((h.input_len(), stats.count()), (2, 0), "dropped, and not rendered late");
        h.clock().advance(MS(5));
        h.crank().unwrap();
        assert_eq!(stats.renders(), [Render { pts: MS(30), rendered_at: MS(30) }]);
        let (sink, stats) = TimedTestSink::new();
        let mut h = Harness::new(sink);
        h.clock().advance(MS(30));
        h.skip_to(MS(30)).unwrap();
        let at_point = stamped(&mut h, 30);
        h.push(at_point).unwrap();
        let unstamped = h.alloc(&[0; 8]);
        h.push(unstamped).unwrap();
        assert_eq!((h.input_len(), stats.count()), (1, 1));
    }
}
