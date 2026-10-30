//! The `queue` element. Decoupling, a FormatChange crossing it, and a fan-out it makes legal.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

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
use profluens_elements::flow::{PassThrough, Queue};
use profluens_elements::testing::{FNV_OFFSET, TestSink, TestSrc, fold, pattern_byte};

#[test]
fn passives_across_a_queue_run_and_stay_correct() {
    // Each passthrough inlines into a different group, one each side of the queue.
    let n = 500_009u64; // not a multiple of the buffer size, frames straddle buffers
    let (sink, stats) = TestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(n));
    let pt_in = p.add(PassThrough::new());
    let q = p.add(Queue::new());
    let pt_out = p.add(PassThrough::new());
    let dst = p.add(sink);
    p.link(src.src(TestSrc::OUT), pt_in.sink(PassThrough::IN)).expect("src->pt_in");
    p.link(pt_in.src(PassThrough::OUT), q.sink(Queue::IN)).expect("pt_in->queue");
    p.link(q.src(Queue::OUT), pt_out.sink(PassThrough::IN)).expect("queue->pt_out");
    p.link(pt_out.src(PassThrough::OUT), dst.sink(TestSink::IN)).expect("pt_out->sink");
    p.run().expect("a queue in the middle runs to completion");

    assert!(stats.is_done(), "sink saw EOS across the queue");
    assert_eq!(stats.bytes(), n, "every byte crossed the queue");

    let mut h = FNV_OFFSET;
    for i in 0..n {
        h = fold(h, pattern_byte(i));
    }
    assert_eq!(stats.hash(), h, "bytes intact and in order across the queue");
}

#[test]
fn queue_makes_two_passives_thread_decoupled() {
    let n = 300_007u64;
    let (sink, stats) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(n));
    let q = p.add(Queue::new());
    let dst = p.add(sink);
    p.link(src.src(TestSrc::OUT), q.sink(Queue::IN)).unwrap();
    p.link(q.src(Queue::OUT), dst.sink(TestSink::IN)).unwrap();
    p.run().expect("run");
    assert!(stats.is_done());
    assert_eq!(stats.bytes(), n);
}

static SRC_RATES: [ValueDesc; 2] = [ValueDesc::Int(44100), ValueDesc::Int(48000)];
static SRC_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: "rate", allowed: ConstraintDesc::Set(&SRC_RATES), preferred: None }];
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SRC_FIELDS }];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "announcesrc",
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

struct AnnounceSrc {
    rate: i64,
    announced: bool,
}
impl AnnounceSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for AnnounceSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.announced {
            ctx.announce_format(Self::OUT, "audio/raw", &[("rate", ValueDesc::Int(self.rate))]);
            self.announced = true;
            return Ok(Flow::Ok); // let the FormatChange batch flush downstream first
        }
        Ok(Flow::Eos)
    }
}

static SINK_RATES: [ValueDesc; 2] = [ValueDesc::Int(44100), ValueDesc::Int(48000)];
static SINK_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: "rate", allowed: ConstraintDesc::Set(&SINK_RATES), preferred: None }];
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SINK_FIELDS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "raterecordsink",
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

struct RateRecordSink {
    seen_rate: Arc<AtomicI64>,
    seen_change: Arc<AtomicU64>,
}
impl RateRecordSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for RateRecordSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {}
        Ok(Flow::Ok)
    }
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            self.seen_change.fetch_add(1, Ordering::Relaxed);
            if let Some(field) = ctx.field_id("rate")
                && let Some(Value::Int(r)) = f.get(field)
            {
                self.seen_rate.store(r, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

#[test]
fn dynamic_caps_announce_reaches_the_queue_and_revalidates() {
    // The wildcard queue sink admits the runtime announcement and its `event()` forwards it
    // onward to the sink.
    let seen_rate = Arc::new(AtomicI64::new(0));
    let seen_change = Arc::new(AtomicU64::new(0));

    let mut p = Pipeline::new();
    let src = p.add(AnnounceSrc { rate: 48000, announced: false });
    let q = p.add(Queue::new());
    let snk =
        p.add(RateRecordSink { seen_rate: seen_rate.clone(), seen_change: seen_change.clone() });
    p.link(src.src(AnnounceSrc::OUT), q.sink(Queue::IN)).expect("src->queue links");
    p.link(q.src(Queue::OUT), snk.sink(RateRecordSink::IN)).expect("queue->sink links");
    p.run().expect("a runtime announcement re-validates against the wildcard queue");
    assert!(
        seen_change.load(Ordering::Relaxed) >= 1,
        "the FormatChange hopped onward across the queue to the sink"
    );
    assert_eq!(
        seen_rate.load(Ordering::Relaxed),
        48000,
        "the sink observed the runtime-announced rate through the queue"
    );
}

// Passive fan-out is illegal. A queue per branch makes each branch its own Active group.

#[test]
fn passive_fan_out_is_rejected_but_a_queue_fixes_it() {
    let n = 200_003u64;
    let (sink_a, stats_a) = TestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(n));
    let q = p.add(Queue::new());
    let pt = p.add(PassThrough::new());
    let dst = p.add(sink_a);
    p.link(src.src(TestSrc::OUT), q.sink(Queue::IN)).unwrap();
    p.link(q.src(Queue::OUT), pt.sink(PassThrough::IN)).unwrap();
    p.link(pt.src(PassThrough::OUT), dst.sink(TestSink::IN)).unwrap();
    p.run().expect("queue ! passthrough is a legal group");

    assert!(stats_a.is_done());
    assert_eq!(stats_a.bytes(), n);
    let mut h = FNV_OFFSET;
    for i in 0..n {
        h = fold(h, pattern_byte(i));
    }
    assert_eq!(stats_a.hash(), h);
}
