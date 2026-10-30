//! A format announced between two pushes in one `process()` reaches downstream between
//! exactly those two buffers.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static SRC_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None }];
static OFFERS: [OfferDesc; 1] = [OfferDesc { family: "midtest", fields: &SRC_FIELDS }];

static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "annsrc",
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

struct AnnSrc {
    emitted: bool,
}

impl AnnSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for AnnSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.emitted {
            return Ok(Flow::Eos);
        }
        self.emitted = true;
        for i in 0..5u8 {
            if i == 3 {
                ctx.announce_format(Self::OUT, "midtest", &[("rate", ValueDesc::Int(999))]);
            }
            let mut buf = ctx.alloc();
            buf.memory.as_mut_full()[0] = i;
            buf.memory.set_len(1);
            ctx.out(Self::OUT).push(buf);
        }
        Ok(Flow::Ok)
    }
}

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "ordersink",
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

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Item {
    Buf(u8),
    Rate(i64),
}

struct OrderSink {
    seen: Arc<Mutex<Vec<Item>>>,
}

impl OrderSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for OrderSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut seen = self.seen.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            seen.push(Item::Buf(buf.memory.data()[0]));
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            let rate = ctx
                .field_id("rate")
                .and_then(|id| f.get(id))
                .and_then(|v| match v {
                    Value::Int(n) => Some(n),
                    _ => None,
                })
                .unwrap_or(-1);
            self.seen.lock().unwrap().push(Item::Rate(rate));
        }
        Ok(())
    }
}

#[test]
fn mid_batch_announce_lands_between_the_right_buffers() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(AnnSrc { emitted: false });
    let snk = p.add(OrderSink { seen: Arc::clone(&seen) });
    p.link(src.src(AnnSrc::OUT), snk.sink(OrderSink::IN)).expect("link");
    p.run().expect("run");

    let got = seen.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![Item::Buf(0), Item::Buf(1), Item::Buf(2), Item::Rate(999), Item::Buf(3), Item::Buf(4),],
        "the FormatChange splits the batch exactly at its announcement position"
    );
}

#[test]
fn mid_batch_position_survives_a_forwarding_transport() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(AnnSrc { emitted: false });
    let q = p.add(profluens_elements::flow::Queue::new());
    let snk = p.add(OrderSink { seen: Arc::clone(&seen) });
    p.link(src.src(AnnSrc::OUT), q.sink(profluens_elements::flow::Queue::IN)).expect("src!q");
    p.link(q.src(profluens_elements::flow::Queue::OUT), snk.sink(OrderSink::IN)).expect("q!snk");
    p.run().expect("run");

    let got = seen.lock().unwrap().clone();
    let rate_pos = got.iter().position(|i| matches!(i, Item::Rate(999))).expect("change arrived");
    let bufs_before: Vec<u8> = got[..rate_pos]
        .iter()
        .filter_map(|i| match i {
            Item::Buf(b) => Some(*b),
            _ => None,
        })
        .collect();
    assert_eq!(bufs_before, vec![0, 1, 2], "exactly the pre-announce buffers precede the change");
    let bufs_after: Vec<u8> = got[rate_pos..]
        .iter()
        .filter_map(|i| match i {
            Item::Buf(b) => Some(*b),
            _ => None,
        })
        .collect();
    assert_eq!(bufs_after, vec![3, 4], "exactly the post-announce buffers follow it");
}
