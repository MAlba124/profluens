//! `testsrc` generates a reproducible byte pattern, then EOS.

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{Constraint, OfferDesc, Value};
use profluens_core::id::SrcPad;
use profluens_core::log;
use profluens_core::log::Level;

use super::pattern_byte;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

/// Total bytes to produce. Read once in `start()`.
static PROPS: [PropDesc; 1] = [PropDesc { name: "total", allowed: Constraint::Any, live: false }];

// Cold, one box per registry-created element.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "testsrc",
    sinks: &[],
    srcs: &PADS,
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(TestSrc::new(0))),
    io: false,
    couplings: &[],
};

pub struct TestSrc {
    total: u64,
    produced: u64,
    /// `process()` may run again after `Flow::Eos`. Log the transition once.
    eos_logged: bool,
}

impl TestSrc {
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new(total_bytes: u64) -> Self {
        Self { total: total_bytes, produced: 0, eos_logged: false }
    }
}

impl Element for TestSrc {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(n)) = ctx.prop("total")
            && n >= 0
        {
            self.total = n as u64;
        }
        log!(&*ctx, Level::Debug, "start", total = self.total);
        self.eos_logged = false;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.produced >= self.total {
            if !self.eos_logged {
                self.eos_logged = true;
                log!(&*ctx, Level::Info, "eos", produced = self.produced);
            }
            return Ok(Flow::Eos);
        }
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok), // pool full, backpressure
        };
        let cap = buf.memory.capacity() as u64;
        let n = cap.min(self.total - self.produced);
        let dst = buf.memory.as_mut_full();
        for j in 0..n {
            dst[j as usize] = pattern_byte(self.produced + j);
        }
        buf.memory.set_len(n as usize);
        self.produced += n;
        ctx.out(Self::OUT).push(buf);
        Ok(Flow::Ok)
    }
}
