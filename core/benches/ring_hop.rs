// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use profluens_core::batch::{Batch, Inputs};
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{FormatId, SinkPad, SrcPad};
use profluens_core::memory::Pool;
use profluens_core::pipeline::Pipeline;
use profluens_core::ring::spsc;
use profluens_core::time::Timestamp;

const ITEMS_ST: usize = 5_000_000;
const ITEMS_PS: usize = 1_000_000;
const TRIALS: usize = 7;
const WARMUP: usize = 2;

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    if n % 2 == 1 { xs[n / 2] } else { (xs[n / 2 - 1] + xs[n / 2]) / 2.0 }
}

fn bench_same_thread() -> f64 {
    let mut samples = Vec::with_capacity(TRIALS);
    for trial in 0..(WARMUP + TRIALS) {
        let (p, c) = spsc::<usize>(1024);
        let start = Instant::now();
        for i in 0..ITEMS_ST {
            p.try_push(i).expect("depth-1 ring is never full");
            let _ = c.try_pop().expect("just pushed one");
        }
        let per_item = start.elapsed().as_nanos() as f64 / ITEMS_ST as f64;
        if trial >= WARMUP {
            samples.push(per_item);
        }
    }
    median(samples)
}

fn bench_ping_stream() -> f64 {
    let mut samples = Vec::with_capacity(TRIALS);
    for trial in 0..(WARMUP + TRIALS) {
        let (p, c) = spsc::<usize>(256);
        let start = Instant::now();
        let producer = std::thread::spawn(move || {
            for i in 0..ITEMS_PS {
                p.push(i).expect("consumer alive");
            }
        });
        let consumer = std::thread::spawn(move || {
            let mut acc = 0usize;
            for _ in 0..ITEMS_PS {
                acc = acc.wrapping_add(c.pop().expect("producer alive until N"));
            }
            acc
        });
        producer.join().unwrap();
        let acc = consumer.join().unwrap();
        assert_eq!(acc, ITEMS_PS.wrapping_mul(ITEMS_PS.wrapping_sub(1)) / 2);
        let per_item = start.elapsed().as_nanos() as f64 / ITEMS_PS as f64;
        if trial >= WARMUP {
            samples.push(per_item);
        }
    }
    median(samples)
}

const ITEMS_BATCH: usize = 4_000_000;

// Bench setup allocates the stash once, before the timed loop.
#[allow(clippy::disallowed_methods)]
fn bench_batch_push_pop(n: usize) -> f64 {
    let pool = Pool::bounded(64, n as u32);
    let mut stash: Vec<Buffer> = Vec::with_capacity(n);
    for i in 0..n {
        let mut memory = pool.acquire();
        memory.set_len(16);
        stash.push(Buffer {
            memory,
            pts: Timestamp::from_nanos(i as u64),
            dts: Timestamp::from_nanos(i as u64),
            duration: Timestamp::from_nanos(1),
            flags: BufferFlags::KEYFRAME,
            format: FormatId(0),
            sync: None,
        });
    }
    let mut batch = Batch::new(FormatId(0));
    let rounds = ITEMS_BATCH / n;
    let mut samples = Vec::with_capacity(TRIALS);
    for trial in 0..(WARMUP + TRIALS) {
        let mut acc = 0u64;
        let start = Instant::now();
        for _ in 0..rounds {
            for b in stash.drain(..) {
                batch.push(b);
            }
            let b = black_box(&mut batch);
            while let Some(buf) = b.pop_front() {
                acc = acc.wrapping_add(buf.pts.0).wrapping_add(buf.memory.len() as u64);
                stash.push(buf);
            }
        }
        let per_item = start.elapsed().as_nanos() as f64 / (rounds * n) as f64;
        black_box(acc);
        if trial >= WARMUP {
            samples.push(per_item);
        }
    }
    median(samples)
}

static ANY: [OfferDesc; 1] = [OfferDesc::any("bytes")];

type Pads = &'static [PadDesc];

const fn hop_desc(name: &'static str, sinks: Pads, srcs: Pads) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static HOP_SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &ANY }];
static HOP_SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static HOP_SRC: ElementDesc = hop_desc("hopsrc", &[], &HOP_SRC_PADS);
static HOP_SINK: ElementDesc = hop_desc("hopsink", &HOP_SINK_PADS, &[]);

struct HopSrc {
    left: u64,
    per: u64,
}

impl HopSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for HopSrc {
    fn desc(&self) -> &'static ElementDesc {
        &HOP_SRC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.left == 0 {
            return Ok(Flow::Eos);
        }
        for _ in 0..self.per.min(self.left) {
            let Some(mut buf) = ctx.try_alloc() else {
                break;
            };
            buf.memory.set_len(16);
            buf.pts = Timestamp::from_nanos(self.left);
            ctx.out(Self::OUT).push(buf);
            self.left -= 1;
        }
        Ok(Flow::Ok)
    }
}

struct HopSink {
    got: Arc<AtomicU64>,
}

impl HopSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for HopSink {
    fn desc(&self) -> &'static ElementDesc {
        &HOP_SINK
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut n = 0;
        while inputs.pop().is_some() {
            n += 1;
        }
        self.got.fetch_add(n, Ordering::Relaxed);
        Ok(Flow::Ok)
    }
}

static PASS_DESC: ElementDesc =
    ElementDesc { sched: SchedHint::Passive, ..hop_desc("hoppass", &HOP_SINK_PADS, &HOP_SRC_PADS) };
static PASS_SINK: ElementDesc =
    ElementDesc { sched: SchedHint::Passive, ..hop_desc("hoppsink", &HOP_SINK_PADS, &[]) };

struct HopPass;

impl Element for HopPass {
    fn desc(&self) -> &'static ElementDesc {
        &PASS_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            ctx.out(HopSrc::OUT).push(buf);
        }
        Ok(Flow::Ok)
    }
}

struct InlineSink {
    got: Arc<AtomicU64>,
}

impl Element for InlineSink {
    fn desc(&self) -> &'static ElementDesc {
        &PASS_SINK
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut n = 0;
        while inputs.pop().is_some() {
            n += 1;
        }
        self.got.fetch_add(n, Ordering::Relaxed);
        Ok(Flow::Ok)
    }
}

const PASSES: usize = 4;

// Bench setup builds one pipeline per trial, outside the per-batch path.
#[allow(clippy::disallowed_methods)]
fn bench_inline_hop(per: u64) -> (f64, f64) {
    let mut samples = Vec::with_capacity(TRIALS);
    for trial in 0..(WARMUP + TRIALS) {
        let got = Arc::new(AtomicU64::new(0));
        let mut p = Pipeline::new();
        let mut at = p.add(HopSrc { left: HOPS, per });
        p.set_pool(64, 4 * per as u32);
        for _ in 0..PASSES {
            let next = p.add(HopPass);
            p.link(at.src(HopSrc::OUT), next.sink(HopSink::IN)).expect("link");
            at = next;
        }
        let sink = p.add(InlineSink { got: Arc::clone(&got) });
        p.link(at.src(HopSrc::OUT), sink.sink(HopSink::IN)).expect("link");
        let start = Instant::now();
        p.run().expect("run");
        let per_hop = start.elapsed().as_nanos() as f64 / HOPS as f64 / (PASSES + 1) as f64;
        assert_eq!(got.load(Ordering::Relaxed), HOPS);
        if trial >= WARMUP {
            samples.push(per_hop);
        }
    }
    let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
    (min, median(samples))
}

const HOPS: u64 = 300_000;

// Bench setup builds one pipeline per trial, outside the per-batch path.
#[allow(clippy::disallowed_methods)]
fn bench_group_hop(cap: usize, per: u64) -> (f64, f64) {
    let mut samples = Vec::with_capacity(TRIALS);
    for trial in 0..(WARMUP + TRIALS) {
        let got = Arc::new(AtomicU64::new(0));
        let mut p = Pipeline::new();
        let src = p.add(HopSrc { left: HOPS, per });
        if per > 1 {
            p.set_pool(64, 4 * per as u32);
        }
        let sink = p.add(HopSink { got: Arc::clone(&got) });
        p.link(src.src(HopSrc::OUT), sink.sink(HopSink::IN)).expect("link");
        p.set_queue_capacity(sink, cap);
        let start = Instant::now();
        p.run().expect("run");
        let per_hop = start.elapsed().as_nanos() as f64 / HOPS as f64 * per as f64;
        assert_eq!(got.load(Ordering::Relaxed), HOPS);
        if trial >= WARMUP {
            samples.push(per_hop);
        }
    }
    let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
    (min, median(samples))
}

static HOP_SRC2_PADS: [PadDesc; 2] =
    [PadDesc { name: "a", offers: &ANY }, PadDesc { name: "b", offers: &ANY }];
static HOP_SRC2: ElementDesc = hop_desc("hopsrc2", &[], &HOP_SRC2_PADS);

struct StampSrc {
    left: u64,
    pads: u32,
    at: u64,
}

impl Element for StampSrc {
    fn desc(&self) -> &'static ElementDesc {
        if self.pads == 1 { &HOP_SRC } else { &HOP_SRC2 }
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.left == 0 {
            return Ok(Flow::Eos);
        }
        for pad in 0..self.pads {
            let Some(mut buf) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            buf.memory.set_len(16);
            (buf.pts, buf.duration) =
                (Timestamp::from_nanos(self.at), Timestamp::from_nanos(1_000));
            ctx.out(SrcPad(pad)).push(buf);
        }
        self.at += 1_000;
        self.left -= 1;
        Ok(Flow::Ok)
    }
}

// Bench setup builds one pipeline per trial, outside the per-batch path.
#[allow(clippy::disallowed_methods)]
fn bench_limited_hop(pads: u32) -> (f64, f64) {
    let mut samples = Vec::with_capacity(TRIALS);
    for trial in 0..(WARMUP + TRIALS) {
        let got = Arc::new(AtomicU64::new(0));
        let mut p = Pipeline::new();
        let src = p.add(StampSrc { left: HOPS, pads, at: 0 });
        p.set_pool(64, 64);
        for pad in 0..pads {
            let sink = p.add(HopSink { got: Arc::clone(&got) });
            p.link(src.src(SrcPad(pad)), sink.sink(HopSink::IN)).expect("link");
            p.set_link_limit(sink, profluens_core::level::Limit::time(Timestamp::from_secs(1)));
        }
        let start = Instant::now();
        p.run().expect("run");
        let per_hop = start.elapsed().as_nanos() as f64 / HOPS as f64;
        assert_eq!(got.load(Ordering::Relaxed), HOPS * u64::from(pads));
        if trial >= WARMUP {
            samples.push(per_hop);
        }
    }
    let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
    (min, median(samples))
}

fn main() {
    use std::io::Write;
    let only = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let wanted = |name: &str| only.as_deref().is_none_or(|o| name.contains(o));
    println!(
        "ring_hop microbench, {TRIALS} trials (median), {WARMUP} warmup \
         (same_thread {ITEMS_ST} items/trial, ping_stream {ITEMS_PS})"
    );
    std::io::stdout().flush().ok();

    if wanted("same_thread") {
        let st = bench_same_thread();
        println!("same_thread  try_push+try_pop : {st:>7.2} ns/item");
        std::io::stdout().flush().ok();
    }
    if wanted("ping_stream") {
        let ps = bench_ping_stream();
        println!("ping_stream  push->pop 2-thread: {ps:>7.2} ns/item");
        std::io::stdout().flush().ok();
    }
    if wanted("batch_push_pop") {
        println!("size_of::<Batch>() = {} bytes", std::mem::size_of::<Batch>());
        for n in [1usize, 8, 32] {
            let bp = bench_batch_push_pop(n);
            println!("batch_push_pop n={n:<2} warm shell : {bp:>7.2} ns/buffer");
            std::io::stdout().flush().ok();
        }
    }
    if wanted("group_hop") {
        for cap in [4usize, 256] {
            let (min, med) = bench_group_hop(cap, 1);
            println!(
                "group_hop cap={cap:<3} two groups    : {min:>7.2} min {med:>7.2} median ns/batch"
            );
            std::io::stdout().flush().ok();
        }
    }
    if wanted("inline_hop") {
        for per in [1u64, 32] {
            let (min, med) = bench_inline_hop(per);
            println!(
                "inline_hop {per:<2} a batch, one group : {min:>7.2} min {med:>7.2} median ns/buffer/hop"
            );
            std::io::stdout().flush().ok();
        }
    }
    if wanted("limited_hop") {
        for pads in [1u32, 2] {
            let (min, med) = bench_limited_hop(pads);
            println!(
                "limited_hop {pads} limited link(s), 1 s : {min:>7.2} min {med:>7.2} median ns/pass"
            );
            std::io::stdout().flush().ok();
        }
    }
    if wanted("group_rows") {
        let (min, med) = bench_group_hop(16, 32);
        println!("group_rows 32 a batch, two groups: {min:>7.2} min {med:>7.2} median ns/batch");
        std::io::stdout().flush().ok();
    }
}
