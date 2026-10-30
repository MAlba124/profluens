//! Per-element output pools, observed as the capacity of received buffers.

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
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

static SRC_DESC: ElementDesc = ElementDesc {
    name: "capsrc",
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

struct CapSrc {
    n: usize,
    sent: usize,
}

impl CapSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for CapSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.sent >= self.n {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.set_len(1);
        ctx.out(Self::OUT).push(buf);
        self.sent += 1;
        Ok(Flow::Ok)
    }
}

static SINK_DESC: ElementDesc = ElementDesc {
    name: "capsink",
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

struct CapSink {
    caps: Arc<Mutex<Vec<usize>>>,
}

impl CapSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for CapSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut caps = self.caps.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            caps.push(buf.memory.capacity());
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn per_element_pools_serve_their_own_slot_sizes() {
    let mut p = Pipeline::new();
    p.set_pool(1024, 16);

    let a = p.add(CapSrc { n: 5, sent: 0 });
    let caps_a = Arc::new(Mutex::new(Vec::new()));
    let sink_a = p.add(CapSink { caps: Arc::clone(&caps_a) });
    p.link(a.src(CapSrc::OUT), sink_a.sink(CapSink::IN)).expect("a!sink_a");

    let b = p.add(CapSrc { n: 5, sent: 0 });
    let caps_b = Arc::new(Mutex::new(Vec::new()));
    let sink_b = p.add(CapSink { caps: Arc::clone(&caps_b) });
    p.link(b.src(CapSrc::OUT), sink_b.sink(CapSink::IN)).expect("b!sink_b");

    p.set_element_pool(a, 4096, 8);
    p.run().expect("run");

    let ca = caps_a.lock().unwrap();
    let cb = caps_b.lock().unwrap();
    assert_eq!(ca.len(), 5);
    assert_eq!(cb.len(), 5);
    assert!(ca.iter().all(|&c| c == 4096), "override pool served A: {ca:?}");
    assert!(cb.iter().all(|&c| c == 1024), "default pool served B: {cb:?}");
}
