//! Drivers feeding a case to one core element through its `Harness`. Needs the `harness` feature.

use pf_harness::Harness;
use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{FormatId, SrcPad};
use profluens_core::memory::{Pool, PoolConfig};
use profluens_core::time::Timestamp;

use super::chunk::Chunking;
use super::gate::{Case, Violation, check_steps};
use super::heap::CLASS_REGION_COST;

static SHIM_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SHIM_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SHIM_OFFERS }];
static SHIM_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SHIM_OFFERS }];
static SHIM_DESC: ElementDesc = ElementDesc {
    name: "fuzzshim",
    sinks: &SHIM_SINKS,
    srcs: &SHIM_SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc {
        min: Timestamp::ZERO,
        max: Timestamp::ZERO,
        is_live: false,
        jitter: Timestamp::ZERO,
    },
    make_default: None,
    io: false,
    couplings: &[],
};

/// Presents a demuxer to the harness as one sink and one src pad. The harness never
/// calls `preroll`, so `start` runs it and every stream is funneled to the one src pad.
pub struct Shim<E> {
    pub inner: E,
}

/// Left on the added pads, output would hold pool slots for good.
fn funnel(ctx: &mut Ctx) {
    for pad in 1..ctx.src_pad_count() {
        if ctx.output_on(SrcPad(pad)).is_empty() {
            continue;
        }
        let mut batch = ctx.take_output(SrcPad(pad));
        ctx.output_on(SrcPad(0)).append(&mut batch);
    }
}

impl<E: Element> Element for Shim<E> {
    fn desc(&self) -> &'static ElementDesc {
        &SHIM_DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.inner.preroll(ctx)?;
        for stream in 0..ctx.take_streams().len() {
            ctx.set_stream_pad(stream as u32, SrcPad(0));
        }
        let started = self.inner.start(ctx);
        funnel(ctx);
        started
    }

    fn process(&mut self, ctx: &mut Ctx, inputs: Inputs<'_>) -> Result<Flow, Error> {
        let flow = self.inner.process(ctx, inputs);
        funnel(ctx);
        flow
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        let done = self.inner.event(ctx, event);
        funnel(ctx);
        done
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.inner.stop(ctx)
    }
}

/// The case's pool as `(slot bytes, slots)`. Small on purpose, to exercise backpressure.
pub fn pool_shape(knob: u64) -> (usize, u32) {
    let slot = [64usize, 256, 4096, 65536][(knob & 3) as usize];
    let slots = [1u32, 2, 8, 64][((knob >> 2) & 3) as usize];
    (slot, slots)
}

/// A pool class region (`profluens_core::memory`, 256 KiB), and the most classes a case touches.
const CLASS_REGION: u64 = 256 << 10;
const CLASSES: usize = 4;

/// Slots together and at least one class region, so every corpus sample is served whole.
pub fn pool_budget(knob: u64) -> u64 {
    let (slot, slots) = pool_shape(knob);
    (slot as u64 * slots as u64).max(CLASS_REGION)
}

pub fn pool_config(knob: u64) -> PoolConfig {
    let (slot, slots) = pool_shape(knob);
    PoolConfig { bytes: pool_budget(knob), ..PoolConfig::heap(slot, slots) }
}

/// Heap a harness pool of this shape may hold, plus fixed slack for the harness and bus.
pub fn pool_allowance(knob: u64) -> usize {
    let (slot, slots) = pool_shape(knob);
    slot * slots as usize + pool_budget(knob) as usize + CLASSES * CLASS_REGION_COST + (512 << 10)
}

/// Input from outside the element's pool, so input never competes with output for slots.
/// Panics past the exact cap, a case's own input.
#[allow(clippy::expect_used)]
pub fn input_buffer(bytes: &[u8]) -> Buffer {
    let mut memory =
        Pool::new(1).acquire_exact(bytes.len()).expect("case input under the exact cap");
    memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
    memory.set_len(bytes.len());
    Buffer {
        memory,
        pts: Timestamp::NONE,
        dts: Timestamp::NONE,
        duration: Timestamp::NONE,
        flags: BufferFlags::empty(),
        format: FormatId(0),
        sync: None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fed {
    pub steps: u64,
    pub out_buffers: usize,
    pub out_bytes: usize,
    /// An element error ends its stream and is a pass for corrupt input.
    pub failed: bool,
}

fn count(fed: &mut Fed, outputs: &[Buffer]) -> usize {
    fed.out_buffers += outputs.len();
    fed.out_bytes += outputs.iter().map(|buffer| buffer.memory.len()).sum::<usize>();
    outputs.len()
}

pub fn collect(harness: &mut Harness, fed: &mut Fed) -> usize {
    let taken = count(fed, &harness.drain_outputs());
    let _ = harness.bus_messages();
    taken
}

/// Crank until nothing more to emit. Fails with `Steps` past `limit`, cumulative over the case.
pub fn settle(harness: &mut Harness, fed: &mut Fed, limit: u64) -> Result<(), Violation> {
    collect(harness, fed);
    let mut idle = 0;
    while idle < 2 && !fed.failed {
        fed.steps += 1;
        check_steps(fed.steps, limit)?;
        if harness.crank().is_err() {
            fed.failed = true;
        }
        idle = if collect(harness, fed) == 0 { idle + 1 } else { 0 };
    }
    Ok(())
}

/// One buffer per chunk, cranked dry after each, then end of stream. The step limit grows
/// by `16 + 4 * chunk length` per chunk plus `4 * extra` for output owed beyond the input.
pub fn feed_bytes(
    element: impl Element + 'static,
    case: &Case,
    flush_before: Option<usize>,
    extra: usize,
) -> Result<Fed, Violation> {
    let mut harness = Harness::with_pool_config(Shim { inner: element }, pool_config(case.knob));
    let mut fed = Fed::default();
    let mut limit = 4 * extra as u64;
    if harness.start().is_err() {
        fed.failed = true;
        return Ok(fed);
    }
    for (index, chunk) in case.chunking.chunks(&case.data).enumerate() {
        if flush_before == Some(index) {
            let flushed = harness.push_event(Event::FlushStart).is_err()
                || harness.push_event(Event::FlushStop).is_err();
            fed.failed |= flushed;
        }
        if fed.failed {
            break;
        }
        fed.steps += 1;
        if harness.push(input_buffer(chunk)).is_err() {
            fed.failed = true;
        }
        limit += 16 + 4 * chunk.len() as u64;
        settle(&mut harness, &mut fed, limit)?;
    }
    if !fed.failed {
        finish(&mut harness, &mut fed);
    }
    collect(&mut harness, &mut fed);
    Ok(fed)
}

fn finish(harness: &mut Harness, fed: &mut Fed) {
    match harness.eos() {
        Ok(tail) => {
            count(fed, &tail);
        }
        Err(_) => fed.failed = true,
    }
}

/// Panics past `2 * pieces + bytes + 64` process calls, which is how a livelock fails.
pub fn drive(
    harness: &mut Harness,
    bytes: &[u8],
    chunking: Chunking,
    flush_before: Option<usize>,
) -> Fed {
    // Not collected, since a slice per one-byte chunk would dwarf the element's own heap.
    let count = chunking.pieces(bytes).count();
    let mut pieces = chunking.pieces(bytes);
    drive_each(harness, &mut pieces, count, bytes.len(), flush_before, &|_, _| {})
}

/// [`drive`] over caller-cut pieces. `stamp` sets timestamps or flags on each buffer.
pub fn drive_pieces(
    harness: &mut Harness,
    pieces: &[&[u8]],
    flush_before: Option<usize>,
    stamp: &dyn Fn(usize, &mut Buffer),
) -> Fed {
    let total: usize = pieces.iter().map(|piece| piece.len()).sum();
    let count = pieces.len();
    drive_each(harness, &mut pieces.iter().copied(), count, total, flush_before, stamp)
}

fn push_dry(
    harness: &mut Harness,
    buffer: Buffer,
    fed: &mut Fed,
    budget: u64,
    total: usize,
) -> bool {
    let mut flow = harness.push(buffer);
    loop {
        fed.steps += 1;
        assert!(
            fed.steps <= budget,
            "element still producing after {budget} process calls on {total} input bytes"
        );
        if flow.is_err() {
            return false;
        }
        if count(fed, &harness.drain_outputs()) == 0 {
            return true;
        }
        flow = harness.crank();
    }
}

fn drive_each<'a>(
    harness: &mut Harness,
    pieces: &mut dyn Iterator<Item = &'a [u8]>,
    count_of: usize,
    total: usize,
    flush_before: Option<usize>,
    stamp: &dyn Fn(usize, &mut Buffer),
) -> Fed {
    let mut fed = Fed::default();
    let budget = (2 * count_of + total + 64) as u64;
    for (index, piece) in pieces.enumerate() {
        if flush_before == Some(index) {
            let flushed = harness
                .push_event(Event::FlushStart)
                .and_then(|()| harness.push_event(Event::FlushStop));
            if flushed.is_err() {
                fed.failed = true;
                return fed;
            }
            count(&mut fed, &harness.drain_outputs());
        }
        let mut buffer = input_buffer(piece);
        stamp(index, &mut buffer);
        if !push_dry(harness, buffer, &mut fed, budget, total) {
            fed.failed = true;
            return fed;
        }
    }
    finish(harness, &mut fed);
    fed
}
