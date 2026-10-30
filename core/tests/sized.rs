use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ONE_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static ONE_OUT: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
) -> ElementDesc {
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

static DEMUX: ElementDesc = desc("zdemux", &[], &ONE_OUT);
static SINK: ElementDesc = desc("zsink", &ONE_IN, &[]);

const SIZES: [usize; 3] = [1000, 3000, 60_000];
const SAMPLES: u64 = 300;
const BUDGET: u64 = 4 * 64 * 1024;
const HOLD: usize = 4;

struct Demux {
    next: u64,
    carried: Option<usize>,
    starved: Arc<AtomicU64>,
}

impl Element for Demux {
    fn desc(&self) -> &'static ElementDesc {
        &DEMUX
    }

    fn process(&mut self, ctx: &mut Ctx, _: Inputs<'_>) -> Result<Flow, Error> {
        while self.next < SAMPLES {
            let n = self.carried.unwrap_or(SIZES[self.next as usize % SIZES.len()]);
            let Some(mut buf) = ctx.try_alloc_sized(n).expect("within the budget") else {
                self.carried = Some(n);
                self.starved.fetch_add(1, Ordering::Relaxed);
                return Ok(Flow::Ok);
            };
            self.carried = None;
            assert!(buf.memory.capacity() >= n);
            buf.memory.as_mut_full()[..8].copy_from_slice(&self.next.to_le_bytes());
            buf.memory.set_len(n);
            ctx.out(OUT).push(buf);
            self.next += 1;
        }
        Ok(Flow::Eos)
    }
}

struct Sink {
    held: Vec<Buffer>,
    seen: Arc<AtomicU64>,
}

impl Element for Sink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let n = u64::from_le_bytes(buf.memory.data()[..8].try_into().unwrap());
            assert_eq!(n, self.seen.fetch_add(1, Ordering::Relaxed), "in order and whole");
            assert_eq!(buf.memory.len(), SIZES[n as usize % SIZES.len()], "one sample, one buffer");
            self.held.push(buf);
            if self.held.len() == HOLD {
                self.held.clear();
            }
        }
        Ok(Flow::Ok)
    }
}

fn run(p: &mut Pipeline) -> Result<(), Error> {
    let (stop, done) = (p.control(), Arc::new(AtomicBool::new(false)));
    let over = Arc::clone(&done);
    let dog = std::thread::spawn(move || {
        for _ in 0..1_000 {
            if over.load(Ordering::Acquire) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.stop();
        true
    });
    let result = p.run();
    done.store(true, Ordering::Release);
    assert!(!dog.join().expect("watchdog"), "the run did not end by itself");
    result
}

#[test]
fn a_producer_of_sized_buffers_is_bounded_by_bytes_and_resumes_on_release() {
    let (starved, seen) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let mut p = Pipeline::new();
    let demux = p.add(Demux { next: 0, carried: None, starved: Arc::clone(&starved) });
    let sink = p.add(Sink { held: Vec::new(), seen: Arc::clone(&seen) });
    p.link(demux.src(OUT), sink.sink(IN)).expect("link");
    p.set_element_pool_bytes(demux, 64, 1024, BUDGET);
    let tap = p.tap_handle();
    run(&mut p).expect("the run ends");
    assert_eq!(seen.load(Ordering::Relaxed), SAMPLES, "every sample arrived");
    assert!(starved.load(Ordering::Relaxed) > 0, "the budget stopped the producer at least once");
    let pool = tap.pool_of(demux).expect("the producer's pool");
    assert_eq!(pool.bytes_budget, BUDGET);
    assert!(
        pool.bytes_high <= BUDGET,
        "{} bytes out at most, over a budget of {BUDGET}",
        pool.bytes_high
    );
    assert!(
        pool.bytes_high > BUDGET / 2,
        "{} bytes out at most, the budget was the bound",
        pool.bytes_high
    );
    assert_eq!(pool.bytes_out, 0, "every byte came back");
    assert_eq!(pool.acquires, 0, "no slot was taken for a sample");
    assert!(
        pool.high_water > 0 && pool.high_water < 1024,
        "{} sized buffers out at most, counted",
        pool.high_water
    );
}
