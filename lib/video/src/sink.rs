//! [`VideoCkSink`], a test sink that renders each frame on the pipeline clock and records its
//! checksum.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::id::SinkPad;
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::format::RAW_ANY_OFFER;

// `bytes` too, so it can follow a byte producer in a smoke test.
use profluens_core::format::OfferDesc;
static OFFERS: [OfferDesc; 2] = [RAW_ANY_OFFER[0], OfferDesc::any("bytes")];

static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

// Cold, once per registry-created element.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "videocksink",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VideoCkSink::new_element())),
    io: false,
    couplings: &[],
};

/// One rendered frame. `rendered_at >= pts` always holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoRender {
    pub pts: Timestamp,
    pub checksum: u64,
    pub rendered_at: Timestamp,
}

/// FNV-1a over a frame's bytes.
pub fn checksum(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(PRIME);
    }
    h
}

struct SinkShared {
    // Locked once a frame, which only a test sink may do.
    renders: Mutex<Vec<VideoRender>>,
    count: AtomicU64,
    done: AtomicBool,
    flushes: AtomicU64,
}

/// A handle to a [`VideoCkSink`]'s results, readable after `run()` returns.
pub struct VideoCkSinkStats(Arc<SinkShared>);

impl VideoCkSinkStats {
    pub fn renders(&self) -> Vec<VideoRender> {
        self.0.renders.lock().unwrap().clone()
    }

    pub fn count(&self) -> u64 {
        self.0.count.load(Ordering::Acquire)
    }

    pub fn is_done(&self) -> bool {
        self.0.done.load(Ordering::Acquire)
    }

    pub fn flushes(&self) -> u64 {
        self.0.flushes.load(Ordering::Acquire)
    }
}

pub struct VideoCkSink {
    shared: Arc<SinkShared>,
}

impl VideoCkSink {
    pub const IN: SinkPad = SinkPad(0);

    /// Returns the sink and a stats handle to read after `run()`.
    // Cold constructor.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> (Self, VideoCkSinkStats) {
        let shared = Arc::new(SinkShared {
            renders: Mutex::new(Vec::new()),
            count: AtomicU64::new(0),
            done: AtomicBool::new(false),
            flushes: AtomicU64::new(0),
        });
        let stats = VideoCkSinkStats(Arc::clone(&shared));
        (Self { shared }, stats)
    }

    /// The sink alone for the registry, which cannot hand a stats handle back.
    pub fn new_element() -> Self {
        Self::new().0
    }
}

impl Element for VideoCkSink {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(front) = inputs.front() {
            // Before the presentation point, dropped and counted, never late.
            if ctx.before_presentation(front.pts, Timestamp::ZERO) {
                log!(&*ctx, Level::Trace, "late", pts = front.pts);
                ctx.count_drop();
                inputs.pop();
                continue;
            }
            // An early frame stays in the input, `due` armed the wake for its deadline.
            if !ctx.due(front.pts) {
                break;
            }
            let Some(buf) = inputs.pop() else { break };
            let render = VideoRender {
                pts: buf.pts,
                checksum: checksum(buf.memory.data()),
                rendered_at: ctx.now(),
            };
            self.shared.renders.lock().unwrap().push(render);
            self.shared.count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            log!(
                &*ctx,
                Level::Debug,
                "flush",
                rendered = self.shared.count.load(Ordering::Acquire)
            );
            self.shared.renders.lock().unwrap().clear();
            self.shared.count.store(0, Ordering::Release);
            self.shared.flushes.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.shared.done.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sink_drops_what_is_stamped_before_the_presentation_point() {
        use pf_harness::Harness;
        let ms = Timestamp::from_millis;
        let (sink, stats) = VideoCkSink::new();
        let mut h = Harness::new(sink);
        let frame = |h: &mut Harness, at: u64| {
            let mut buf = h.alloc(&[at as u8; 16]);
            buf.pts = ms(at);
            h.push(buf).unwrap();
        };
        frame(&mut h, 40);
        frame(&mut h, 80);
        assert_eq!((h.input_len(), stats.count()), (2, 0), "both wait for the clock");
        // A skip to 100 ms. Running time reads the point, so both would be due, and late.
        h.clock().advance(ms(100));
        h.skip_to(ms(100)).unwrap();
        frame(&mut h, 120);
        assert_eq!((h.input_len(), stats.count()), (1, 0), "dropped, and not rendered late");
        h.clock().advance(ms(20));
        h.crank().unwrap();
        let want =
            VideoRender { pts: ms(120), checksum: checksum(&[120; 16]), rendered_at: ms(120) };
        assert_eq!(stats.renders(), [want]);
        assert_eq!(stats.flushes(), 0, "a skip is no flush, the record stands");
    }

    #[test]
    fn checksum_is_stable_and_content_sensitive() {
        assert_eq!(checksum(b"abc"), checksum(b"abc"));
        assert_ne!(checksum(b"abc"), checksum(b"abd"));
        assert_eq!(checksum(&[]), 0xcbf2_9ce4_8422_2325);
    }
}
