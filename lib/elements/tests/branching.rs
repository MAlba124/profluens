//! Fan-out. Each tee pad carries distinct data, so a mis-route fails loudly.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_elements::testing::{FNV_OFFSET, TestSink, TestSrc, fold, pattern_byte};

// Forwards to `src0` verbatim and to `src1` with every byte incremented.
static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 2] =
    [PadDesc { name: "src0", offers: &OFFERS }, PadDesc { name: "src1", offers: &OFFERS }];
static DESC: ElementDesc = ElementDesc {
    name: "tee",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Tee;

impl Tee {
    const IN: SinkPad = SinkPad(0);
    const OUTS: [SrcPad; 2] = [SrcPad(0), SrcPad(1)];
}

impl Element for Tee {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let bytes = buf.memory.data().to_vec();
            let n = bytes.len();

            let mut b0 = ctx.alloc();
            b0.memory.as_mut_full()[..n].copy_from_slice(&bytes);
            b0.memory.set_len(n);
            ctx.out(Self::OUTS[0]).push(b0);

            let mut b1 = ctx.alloc();
            let dst = b1.memory.as_mut_full();
            for (i, &x) in bytes.iter().enumerate() {
                dst[i] = x.wrapping_add(1);
            }
            b1.memory.set_len(n);
            ctx.out(Self::OUTS[1]).push(b1);
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn tee_routes_each_src_pad_to_its_own_sink() {
    let n = 300_007u64; // not a multiple of the buffer size, frames straddle buffers
    let (sink_a, stats_a) = TestSink::new();
    let (sink_b, stats_b) = TestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(n));
    let tee = p.add(Tee);
    let a = p.add(sink_a);
    let b = p.add(sink_b);
    p.link(src.src(TestSrc::OUT), tee.sink(Tee::IN)).expect("src->tee");
    p.link(tee.src(Tee::OUTS[0]), a.sink(TestSink::IN)).expect("tee.src0->a");
    p.link(tee.src(Tee::OUTS[1]), b.sink(TestSink::IN)).expect("tee.src1->b");
    p.run().expect("run");

    assert!(stats_a.is_done() && stats_b.is_done(), "both sinks saw EOS");
    assert_eq!(stats_a.bytes(), n, "sink A received the whole stream");
    assert_eq!(stats_b.bytes(), n, "sink B received the whole stream");

    let (mut ha, mut hb) = (FNV_OFFSET, FNV_OFFSET);
    for i in 0..n {
        ha = fold(ha, pattern_byte(i));
        hb = fold(hb, pattern_byte(i).wrapping_add(1));
    }
    assert_eq!(stats_a.hash(), ha, "src0 → sink A: pattern, intact and in order");
    assert_eq!(stats_b.hash(), hb, "src1 → sink B: pattern+1, intact and in order");
    assert_ne!(stats_a.hash(), stats_b.hash(), "the two pads route distinctly");
}

#[test]
fn dump_dot_shows_the_fan_out() {
    let (sink_a, _a) = TestSink::new();
    let (sink_b, _b) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TestSrc::new(0));
    let tee = p.add(Tee);
    let a = p.add(sink_a);
    let b = p.add(sink_b);
    p.link(src.src(TestSrc::OUT), tee.sink(Tee::IN)).unwrap();
    p.link(tee.src(Tee::OUTS[0]), a.sink(TestSink::IN)).unwrap();
    p.link(tee.src(Tee::OUTS[1]), b.sink(TestSink::IN)).unwrap();

    let dot = p.dump_dot();
    assert!(dot.contains("tee"));
    assert!(dot.contains("e1 -> e2"), "tee → sink A edge:\n{dot}");
    assert!(dot.contains("e1 -> e3"), "tee → sink B edge:\n{dot}");
}
