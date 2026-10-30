//! Dynamic pads and preroll. A demuxer adds one src pad per stream and each reaches its own sink.

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
use profluens_elements::testing::{FNV_OFFSET, TestSink, fold};

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

// Buffer j is filled with `j % streams`, its stream tag.
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "taggedsrc",
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

struct TaggedSrc {
    streams: usize,
    per_stream: usize,
    buf_size: usize,
    produced: usize,
}
impl TaggedSrc {
    const OUT: SrcPad = SrcPad(0);

    fn new(streams: usize, per_stream: usize, buf_size: usize) -> Self {
        Self { streams, per_stream, buf_size, produced: 0 }
    }
    fn total(&self) -> usize {
        self.streams * self.per_stream
    }
}
impl Element for TaggedSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.produced >= self.total() {
            return Ok(Flow::Eos);
        }
        let stream = (self.produced % self.streams) as u8;
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok),
        };
        let dst = buf.memory.as_mut_full();
        for b in dst.iter_mut().take(self.buf_size) {
            *b = stream;
        }
        buf.memory.set_len(self.buf_size);
        self.produced += 1;
        ctx.out(Self::OUT).push(buf);
        Ok(Flow::Ok)
    }
}

static DEMUX_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static DEMUX_DESC: ElementDesc = ElementDesc {
    name: "demux",
    sinks: &DEMUX_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Demux {
    streams: usize,
    pads: Vec<SrcPad>,
}
impl Demux {
    const IN: SinkPad = SinkPad(0);

    fn new(streams: usize) -> Self {
        Self { streams, pads: Vec::new() }
    }
}
impl Element for Demux {
    fn desc(&self) -> &'static ElementDesc {
        &DEMUX_DESC
    }
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        for i in 0..self.streams {
            let pad = ctx.add_src_pad(&format!("src{i}"), &OFFERS);
            self.pads.push(pad);
        }
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let Some(tag) = buf.memory.data().first().copied() else {
                continue;
            };
            let pad = self.pads[(tag as usize) % self.pads.len()];
            ctx.out(pad).push(buf);
        }
        Ok(Flow::Ok)
    }
}

fn repeated_hash(byte: u8, n: usize) -> u64 {
    let mut h = FNV_OFFSET;
    for _ in 0..n {
        h = fold(h, byte);
    }
    h
}

#[test]
fn demux_routes_each_discovered_stream_to_its_own_sink() {
    let (streams, per_stream, buf_size) = (2usize, 40usize, 500usize);
    let (sink0, st0) = TestSink::new();
    let (sink1, st1) = TestSink::new();

    let mut p = Pipeline::new();
    let src = p.add(TaggedSrc::new(streams, per_stream, buf_size));
    let demux = p.add(Demux::new(streams));
    let k0 = p.add(sink0);
    let k1 = p.add(sink1);
    p.link(src.src(TaggedSrc::OUT), demux.sink(Demux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 2, "demux exposed two runtime src pads");
    assert!(added.iter().all(|a| a.element == demux));
    p.link(added[0].element.src(added[0].pad), k0.sink(TestSink::IN)).expect("pad0 -> k0");
    p.link(added[1].element.src(added[1].pad), k1.sink(TestSink::IN)).expect("pad1 -> k1");

    p.run().expect("run");

    let per_stream_bytes = (per_stream * buf_size) as u64;
    assert!(st0.is_done() && st1.is_done(), "both sinks saw EOS");
    assert_eq!(st0.bytes(), per_stream_bytes, "sink 0 got its whole stream");
    assert_eq!(st1.bytes(), per_stream_bytes, "sink 1 got its whole stream");
    assert_eq!(
        st0.hash(),
        repeated_hash(0, per_stream * buf_size),
        "sink 0 received only stream-0 bytes, in order"
    );
    assert_eq!(
        st1.hash(),
        repeated_hash(1, per_stream * buf_size),
        "sink 1 received only stream-1 bytes, in order"
    );
    assert_ne!(st0.hash(), st1.hash(), "the two streams were demultiplexed distinctly");
}

#[test]
fn dump_dot_shows_the_settled_topology() {
    let (sink0, _s0) = TestSink::new();
    let (sink1, _s1) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(TaggedSrc::new(2, 1, 8));
    let demux = p.add(Demux::new(2));
    let k0 = p.add(sink0);
    let k1 = p.add(sink1);
    p.link(src.src(TaggedSrc::OUT), demux.sink(Demux::IN)).unwrap();
    let added = p.preroll().unwrap().srcs;
    p.link(added[0].element.src(added[0].pad), k0.sink(TestSink::IN)).unwrap();
    p.link(added[1].element.src(added[1].pad), k1.sink(TestSink::IN)).unwrap();

    let dot = p.dump_dot();
    assert!(dot.contains("demux") && dot.contains("taggedsrc"));
    assert!(dot.contains("e0 -> e1"), "src → demux:\n{dot}");
    assert!(dot.contains("e1 -> e2"), "demux → sink 0:\n{dot}");
    assert!(dot.contains("e1 -> e3"), "demux → sink 1:\n{dot}");
}
