//! `testsink` folds received bytes into an FNV-1a hash readable through [`TestSinkStats`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use super::{FNV_OFFSET, fold};

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

// Cold, one box per registry-created element.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "testsink",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(TestSink::new().0)),
    io: false,
    couplings: &[],
};

struct Shared {
    hash: AtomicU64,
    bytes: AtomicU64,
    done: AtomicBool,
}

pub struct TestSinkStats(Arc<Shared>);

impl TestSinkStats {
    pub fn hash(&self) -> u64 {
        self.0.hash.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.0.bytes.load(Ordering::Relaxed)
    }

    pub fn is_done(&self) -> bool {
        self.0.done.load(Ordering::Relaxed)
    }
}

pub struct TestSink {
    shared: Arc<Shared>,
    hash: u64,
    bytes: u64,
    paced: bool,
}

impl TestSink {
    pub const IN: SinkPad = SinkPad(0);

    pub fn new() -> (Self, TestSinkStats) {
        let shared = Arc::new(Shared {
            hash: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            done: AtomicBool::new(false),
        });
        let stats = TestSinkStats(Arc::clone(&shared));
        (Self { shared, hash: FNV_OFFSET, bytes: 0, paced: false }, stats)
    }

    /// Like [`new`](Self::new), but each buffer is taken on the pipeline clock.
    pub fn paced() -> (Self, TestSinkStats) {
        let (sink, stats) = Self::new();
        (Self { paced: true, ..sink }, stats)
    }
}

impl Element for TestSink {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(front) = inputs.front() {
            if self.paced && ctx.before_presentation(front.pts, Timestamp::ZERO) {
                log!(&*ctx, Level::Trace, "late", pts = front.pts);
                ctx.count_drop();
                inputs.pop();
                continue;
            }
            if self.paced && !ctx.due(front.pts) {
                break;
            }
            let Some(buf) = inputs.pop() else { break };
            let data = buf.memory.data();
            for &b in data {
                self.hash = fold(self.hash, b);
            }
            self.bytes += data.len() as u64;
        }
        Ok(Flow::Ok)
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        log!(&*ctx, Level::Debug, "stop", bytes = self.bytes, hash = self.hash);
        self.shared.hash.store(self.hash, Ordering::Relaxed);
        self.shared.bytes.store(self.bytes, Ordering::Relaxed);
        self.shared.done.store(true, Ordering::Relaxed);
    }
}
