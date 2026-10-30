//! Output pushed to an unlinked src pad is dropped and counted, never accumulated.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 2] =
    [PadDesc { name: "a", offers: &OFFERS }, PadDesc { name: "b", offers: &OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "twopadsrc",
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

struct TwoPadSrc {
    n: usize,
    sent: usize,
}

impl TwoPadSrc {
    const A: SrcPad = SrcPad(0);
    const B: SrcPad = SrcPad(1);
}

impl Element for TwoPadSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.sent >= self.n {
            return Ok(Flow::Eos);
        }
        for pad in [Self::A, Self::B] {
            let Some(mut buf) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            buf.memory.set_len(1);
            buf.memory.as_mut_full()[0] = pad.0 as u8;
            ctx.out(pad).push(buf);
        }
        self.sent += 1;
        Ok(Flow::Ok)
    }
}

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "bytesink",
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

struct ByteSink {
    seen: Arc<Mutex<Vec<u8>>>,
}

impl ByteSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for ByteSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut seen = self.seen.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            seen.push(buf.memory.data()[0]);
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn unlinked_pad_output_is_dropped_and_counted() {
    const ROUNDS: usize = 50;
    let seen = Arc::new(Mutex::new(Vec::new()));

    let mut p = Pipeline::new();
    let src = p.add(TwoPadSrc { n: ROUNDS, sent: 0 });
    let snk = p.add(ByteSink { seen: Arc::clone(&seen) });
    p.link(src.src(TwoPadSrc::A), snk.sink(ByteSink::IN)).expect("link pad a only");

    let tap = p.tap_handle();
    p.run().expect("run to EOS despite the unlinked pad");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), ROUNDS, "the linked track arrived in full");
    assert!(seen.iter().all(|&b| b == 0), "only pad-a bytes reached the sink");

    let s = tap.snapshot(src).expect("source counters");
    assert_eq!(s.drops, ROUNDS as u64, "every pad-b buffer was dropped by policy and counted");
    assert_eq!(s.buffers_out as usize, 2 * ROUNDS, "produced on both pads");
}
