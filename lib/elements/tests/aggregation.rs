//! Fan-in. Two sources with interleaved PTS feed a muxer that merges them by PTS.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "ptssrc",
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

struct PtsSrc {
    marker: u8,
    start_ms: u64,
    step_ms: u64,
    count: usize,
    produced: usize,
}
impl PtsSrc {
    const OUT: SrcPad = SrcPad(0);

    fn new(marker: u8, start_ms: u64, step_ms: u64, count: usize) -> Self {
        Self { marker, start_ms, step_ms, count, produced: 0 }
    }
}
impl Element for PtsSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.produced >= self.count {
            return Ok(Flow::Eos);
        }
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok),
        };
        buf.memory.as_mut_full()[0] = self.marker;
        buf.memory.set_len(1);
        buf.pts = Timestamp::from_millis(self.start_ms + self.produced as u64 * self.step_ms);
        self.produced += 1;
        ctx.out(Self::OUT).push(buf);
        Ok(Flow::Ok)
    }
}

static MUX_SINKS: [PadDesc; 2] =
    [PadDesc { name: "sink0", offers: &OFFERS }, PadDesc { name: "sink1", offers: &OFFERS }];
static MUX_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static MUX_DESC: ElementDesc = ElementDesc {
    name: "interleavemux",
    sinks: &MUX_SINKS,
    srcs: &MUX_SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::All { by: profluens_core::element::AlignBy::RunningTime },
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct InterleaveMux {
    queues: [VecDeque<Buffer>; 2],
}
impl InterleaveMux {
    const INS: [SinkPad; 2] = [SinkPad(0), SinkPad(1)];
    const OUT: SrcPad = SrcPad(0);

    fn new() -> Self {
        Self { queues: [VecDeque::new(), VecDeque::new()] }
    }
    /// Emit while every open pad has a head, so the lowest-PTS head is safe to release.
    fn emit_ready(&mut self, ctx: &mut Ctx) {
        loop {
            let mut min: Option<(usize, Timestamp)> = None;
            let mut blocked = false;
            for (i, q) in self.queues.iter().enumerate() {
                let pad = SinkPad(i as u32);
                match q.front() {
                    Some(buf) => {
                        if min.is_none_or(|(_, p)| buf.pts < p) {
                            min = Some((i, buf.pts));
                        }
                    }
                    None => {
                        if !ctx.is_pad_closed(pad) {
                            blocked = true; // a live pad might still deliver an earlier PTS
                        }
                    }
                }
            }
            if blocked {
                break;
            }
            match min {
                Some((i, _)) => {
                    let buf = self.queues[i].pop_front().unwrap();
                    ctx.out(Self::OUT).push(buf);
                }
                None => break,
            }
        }
    }
}
impl Element for InterleaveMux {
    fn desc(&self) -> &'static ElementDesc {
        &MUX_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        for (i, pad) in [Self::INS[0], Self::INS[1]].into_iter().enumerate() {
            let mut batch = ctx.take_input_on(pad);
            while let Some(buf) = batch.pop_front() {
                self.queues[i].push_back(buf);
            }
        }
        self.emit_ready(ctx);
        Ok(Flow::Ok)
    }
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::Eos) {
            self.emit_ready(ctx); // both pads closed, drain the remainder in PTS order
        }
        Ok(())
    }
}

static REC_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static REC_DESC: ElementDesc = ElementDesc {
    name: "recordingsink",
    sinks: &REC_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Clone)]
struct Recorder(Arc<Mutex<Vec<(u64, u8)>>>);
struct RecordingSink {
    rec: Recorder,
}
impl RecordingSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for RecordingSink {
    fn desc(&self) -> &'static ElementDesc {
        &REC_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let pts_ms = buf.pts.nanos().unwrap_or(0) / 1_000_000;
            let marker = buf.memory.data().first().copied().unwrap_or(0xFF);
            self.rec.0.lock().unwrap().push((pts_ms, marker));
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn muxer_merges_two_streams_in_pts_order() {
    use profluens_core::pipeline::Pipeline;

    let n = 5;
    let rec = Recorder(Arc::new(Mutex::new(Vec::new())));

    let mut p = Pipeline::new();
    let a = p.add(PtsSrc::new(0, 0, 20, n));
    let b = p.add(PtsSrc::new(1, 10, 20, n));
    let mux = p.add(InterleaveMux::new());
    let sink = p.add(RecordingSink { rec: rec.clone() });
    p.link(a.src(PtsSrc::OUT), mux.sink(InterleaveMux::INS[0])).expect("a -> mux.sink0");
    p.link(b.src(PtsSrc::OUT), mux.sink(InterleaveMux::INS[1])).expect("b -> mux.sink1");
    p.link(mux.src(InterleaveMux::OUT), sink.sink(RecordingSink::IN)).expect("mux -> sink");
    p.run().expect("run");

    let got = rec.0.lock().unwrap().clone();
    let want: Vec<(u64, u8)> = (0..2 * n as u64).map(|k| (k * 10, (k % 2) as u8)).collect();
    assert_eq!(got, want, "muxer emitted a single PTS-ordered stream from two inputs");
}
