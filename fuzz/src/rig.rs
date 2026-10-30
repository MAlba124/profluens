//! The rig behind the demuxer fuzz targets. An input is a [`CONTROL`] block then the file,
//! played through `memsrc ! demux ! sink...`. It fails on a panic, a stall, or a broken bound.
//! `PF_FUZZ_SAY=1` prints what each input did, to set the bounds by.

// Test rig, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pf_sim::corrupt::{CLASS_REGION_COST, heap_growth, heap_mark};
use pf_sim::prng::Prng;
use profluens_core::batch::Inputs;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::ALL;
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::{Pipeline, SeekMode, Serves};
use profluens_core::pull::Ask;
use profluens_core::sched::Outcome;
use profluens_core::time::Timestamp;

pub const CONTROL: usize = 16;

const CHUNKS: [usize; 8] = [1, 7, 64, 512, 4096, 16 << 10, 64 << 10, 1 << 20];

/// A file is pushed in about twice this many chunks at most. One byte a pass over a large
/// file spends the whole time budget on the driver.
const PUSHES: usize = 4096;

/// `(src slot, src slots, demux slot, demux slots, demux bytes)`.
const POOLS: [(usize, u32, usize, u32, u64); 4] = [
    (4096, 6, 4096, 6, 128 << 10),
    (512, 2, 1024, 4, 64 << 10),
    (64 << 10, 8, 64 << 10, 16, 2 << 20),
    (512 << 10, 32, 256 << 10, 24, 32 << 20),
];

const PIPELINE_SLACK: usize = 8 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seek {
    pub after: u64,
    pub kind: SeekKind,
    pub frac: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekKind {
    Keyframe,
    Accurate,
    Byte,
}

#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub pull: bool,
    pub variant: u8,
    pub pools: usize,
    pub chunk_max: usize,
    pub delay_max: u8,
    /// Bit `i` links pad `i % 8`. Zero links every pad.
    pub pads: u8,
    pub seeks: [Option<Seek>; 2],
    pub seed: u64,
}

fn seek_of(kind: u8, after: [u8; 2], frac: [u8; 2]) -> Option<Seek> {
    let kind = match kind & 3 {
        1 => SeekKind::Keyframe,
        2 => SeekKind::Accurate,
        3 => SeekKind::Byte,
        _ => return None,
    };
    Some(Seek {
        after: u64::from(u16::from_le_bytes(after) % 512),
        kind,
        frac: u16::from_le_bytes(frac),
    })
}

impl Plan {
    pub fn decode(control: &[u8]) -> Plan {
        let mut c = [0u8; CONTROL];
        let n = control.len().min(CONTROL);
        c[..n].copy_from_slice(&control[..n]);
        Plan {
            pull: c[0] & 1 != 0,
            variant: (c[0] >> 1) & 3,
            pools: usize::from((c[0] >> 3) & 3),
            chunk_max: CHUNKS[usize::from(c[1] & 7)],
            delay_max: (c[1] >> 3) & 7,
            pads: c[2],
            seeks: [
                seek_of(c[3], [c[4], c[5]], [c[6], c[7]]),
                seek_of(c[3] >> 2, [c[8], c[9]], [c[10], c[11]]),
            ],
            seed: u64::from(u32::from_le_bytes([c[12], c[13], c[14], c[15]])),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub heap_per_byte: usize,
    pub heap_per_head: usize,
    pub heap_fixed: usize,
    pub out_per_byte: usize,
    pub out_per_head: usize,
}

pub struct Made {
    pub element: Box<dyn Element>,
    pub sink: SinkPad,
    pub head_len: usize,
    pub statics: &'static [SrcPad],
    /// Times `out_per_byte` for this input, 1 unless its coding may legitimately expand.
    pub out_scale: usize,
}

#[derive(Clone, Copy)]
pub struct Target {
    pub make: fn(&[u8], u8) -> Option<Made>,
    pub bounds: Bounds,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Broke {
    Stalled { passes: u64, outcomes: String },
    Passes { taken: u64, limit: u64 },
    Heap { grew: usize, limit: usize },
    Output { bytes: u64, limit: u64 },
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Stalled { passes, outcomes } => {
                write!(f, "the run stalled after {passes} passes, {outcomes}")
            }
            Broke::Passes { taken, limit } => write!(f, "{taken} passes taken, limit {limit}"),
            Broke::Heap { grew, limit } => write!(f, "the heap grew {grew} bytes, limit {limit}"),
            Broke::Output { bytes, limit } => write!(f, "{bytes} bytes put out, limit {limit}"),
        }
    }
}

impl std::error::Error for Broke {}

#[derive(Clone, Copy, Debug, Default)]
pub struct Ran {
    pub passes: u64,
    pub buffers: u64,
    pub bytes: u64,
    pub seeks: u64,
    pub failed: bool,
    pub grew: usize,
}

fn say() -> bool {
    static SAY: OnceLock<bool> = OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];

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

static SRC: ElementDesc = desc("memsrc", &[], &SRC_PADS);
static SINK: ElementDesc = desc("countsink", &SINK_PADS, &[]);

const OUT: SrcPad = SrcPad(0);

struct MemSrc {
    file: Arc<[u8]>,
    pull: bool,
    pos: usize,
    stamp: Timestamp,
    chunk_max: usize,
    delay_max: u8,
    prng: Prng,
    held: Vec<(Ask, u8)>,
}

impl MemSrc {
    fn serve(&mut self, ctx: &mut Ctx) -> Flow {
        while let Some(ask) = ctx.next_ask(OUT) {
            let wait = self.prng.below(u64::from(self.delay_max) + 1) as u8;
            self.held.push((ask, wait));
        }
        let mut i = 0;
        while i < self.held.len() {
            if self.held[i].1 > 0 {
                self.held[i].1 -= 1;
                i += 1;
                continue;
            }
            let (ask, _) = self.held.swap_remove(i);
            let Ask { offset, len, tag, mut buf } = ask;
            let from = offset.min(self.file.len() as u64) as usize;
            let n = (len as usize).min(self.file.len() - from).min(buf.memory.capacity());
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[from..from + n]);
            buf.memory.set_len(n);
            ctx.serve(OUT, tag, buf);
        }
        if !self.held.is_empty() {
            ctx.wake_within(Duration::ZERO);
            return Flow::Ok;
        }
        if ctx.asks_over(OUT) {
            return Flow::Eos;
        }
        Flow::Ok
    }
}

impl Element for MemSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        if self.pull {
            ctx.offer_ranges(OUT);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if ctx.pulled(OUT) {
            return Ok(self.serve(ctx));
        }
        if self.pos >= self.file.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let want = self.prng.range(1, self.chunk_max as u64 + 1) as usize;
        let n = want.min(buf.memory.capacity()).min(self.file.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        buf.pts = std::mem::replace(&mut self.stamp, Timestamp::NONE);
        ctx.out(OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if !matches!(event, Event::FlushStart) {
            return Ok(());
        }
        if ctx.pulled(OUT) {
            self.held.clear();
            return Ok(());
        }
        let t = ctx.seek_target().unwrap_or(SeekTarget { to_byte: 0, at: Timestamp::ZERO });
        self.pos = t.to_byte.min(self.file.len() as u64) as usize;
        self.stamp = t.at;
        Ok(())
    }
}

struct CountSink {
    buffers: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

impl Element for CountSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let data = buf.memory.data();
            let sum = data.iter().fold(0u8, |h, b| h.wrapping_add(*b));
            std::hint::black_box(sum);
            self.buffers.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }
}

/// Panics with the [`Broke`] on a broken bound, which is how libFuzzer is told.
pub fn fuzz(input: &[u8], target: &Target) {
    let (control, file) = input.split_at(input.len().min(CONTROL));
    let plan = Plan::decode(control);
    match run(&plan, file, target) {
        Ok(ran) if say() => eprintln!("pf-fuzz {plan:?} len {} {ran:?}", file.len()),
        Ok(_) => {}
        Err(broke) => panic!("{broke}, {plan:?}, {} file bytes", file.len()),
    }
}

pub fn run(plan: &Plan, file: &[u8], target: &Target) -> Result<Ran, Broke> {
    let mark = heap_mark();
    let mut ran = Ran::default();
    let bounds = target.bounds;
    let Some(made) = (target.make)(file, plan.variant) else {
        return Ok(ran);
    };
    let (head_len, out_scale) = (made.head_len, made.out_scale.max(1) as u64);
    let (src_slot, src_slots, demux_slot, demux_slots, demux_bytes) = POOLS[plan.pools];

    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    let src = p.add(MemSrc {
        file: Arc::from(file),
        pull: plan.pull,
        pos: 0,
        stamp: Timestamp::NONE,
        chunk_max: plan.chunk_max.max(file.len() / PUSHES),
        delay_max: plan.delay_max,
        prng: Prng::new(plan.seed).substream(1),
        held: Vec::new(),
    });
    let demux = p.add_boxed(made.element);
    p.set_element_pool(src, src_slot, src_slots);
    p.set_element_pool_bytes(demux, demux_slot, demux_slots, demux_bytes);
    if p.link(src.src(OUT), demux.sink(made.sink)).is_err() {
        return Ok(ran);
    }
    let Ok(added) = p.preroll() else {
        return Ok(ran);
    };
    let pads: Vec<SrcPad> =
        made.statics.iter().copied().chain(added.srcs.iter().map(|a| a.pad)).collect();
    if pads.is_empty() {
        return Ok(ran);
    }
    let (buffers, bytes) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let mut linked = 0;
    for (i, pad) in pads.iter().enumerate() {
        let picked = plan.pads == 0
            || plan.pads & (1 << (i % 8)) != 0
            || (linked == 0 && i + 1 == pads.len());
        if !picked {
            continue;
        }
        let sink = p.add(CountSink { buffers: Arc::clone(&buffers), bytes: Arc::clone(&bytes) });
        if p.link(demux.src(*pad), sink.sink(SinkPad(0))).is_ok() {
            linked += 1;
        }
    }
    let Ok(mut run) = p.step(Arc::clone(&shared)) else {
        return Ok(ran);
    };
    let control = p.control();

    let total = (file.len() + head_len) as u64;
    let seeks = plan.seeks.iter().flatten().count() as u64;
    let limit = 20_000 + 64 * total * (1 + seeks);
    let mut prng = Prng::new(plan.seed).substream(2);
    let mut ready: Vec<usize> = Vec::with_capacity(8);
    let mut next_seek = 0;
    loop {
        if let Some(Some(seek)) = plan.seeks.get(next_seek)
            && buffers.load(Ordering::Relaxed) >= seek.after
        {
            let frac = u64::from(seek.frac);
            let span = control.duration().filter(|d| d.is_some()).map_or(10_000_000_000, |d| d.0);
            let at = Timestamp(((u128::from(span) * u128::from(frac)) / 65_535) as u64);
            let _ = match seek.kind {
                SeekKind::Keyframe => control.seek(at, SeekMode::Keyframe).map(drop),
                SeekKind::Accurate => control.seek(at, SeekMode::Accurate).map(drop),
                SeekKind::Byte => {
                    let to_byte = file.len() as u64 * frac / 65_535;
                    control.seek_bytes(at, &[(ALL, to_byte, at)]).map(drop)
                }
            };
            ran.seeks += 1;
            next_seek += 1;
        }
        ran.passes += 1;
        if ran.passes > limit {
            return Err(Broke::Passes { taken: ran.passes, limit });
        }
        ready.clear();
        ready.extend((0..run.groups()).filter(|g| run.ready(*g)));
        if ready.is_empty() {
            match run.next_deadline() {
                Some(at) if at > clock.now() => clock.advance(Timestamp(at.0 - clock.now().0)),
                Some(_) => {}
                None => break,
            }
            continue;
        }
        let g = ready[prng.below(ready.len() as u64) as usize];
        if run.pass(g).is_err() {
            ran.failed = true;
            break;
        }
    }
    let ended = (0..run.groups()).all(|g| matches!(run.outcome(g), Some(Outcome::Exit)));
    if !ran.failed && !ended {
        let each = (0..run.groups()).map(|g| {
            format!("group {g} {:?} {:?} holds {}", run.elements(g), run.outcome(g), run.held(g))
        });
        return Err(Broke::Stalled {
            passes: ran.passes,
            outcomes: each.collect::<Vec<_>>().join(", "),
        });
    }
    let _ = run.finish();
    drop(p);
    ran.buffers = buffers.load(Ordering::Relaxed);
    ran.bytes = bytes.load(Ordering::Relaxed);

    let (len, head) = (file.len() as u64, head_len as u64);
    let once =
        bounds.out_per_byte as u64 * out_scale * len + bounds.out_per_head as u64 * head + 64;
    let out_limit = once * (1 + ran.seeks);
    if ran.bytes > out_limit {
        return Err(Broke::Output { bytes: ran.bytes, limit: out_limit });
    }
    ran.grew = heap_growth(mark);
    let pools =
        src_slot * src_slots as usize + demux_slot * demux_slots as usize + demux_bytes as usize;
    let heap_limit = (bounds.heap_per_byte + 1) * file.len()
        + (bounds.heap_per_head + 1) * head_len
        + bounds.heap_fixed
        + pools
        + 8 * CLASS_REGION_COST
        + PIPELINE_SLACK;
    if ran.grew > heap_limit {
        return Err(Broke::Heap { grew: ran.grew, limit: heap_limit });
    }
    Ok(ran)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_control_block_decodes_as_zeros() {
        let plan = Plan::decode(&[]);
        assert!(!plan.pull && plan.seeks == [None, None] && plan.chunk_max == 1);
        let plan = Plan::decode(&[1, 7, 0, 0b1011, 3, 0, 0xff, 0xff, 0, 0, 0, 0, 9, 0, 0, 0]);
        assert!(plan.pull && plan.chunk_max == 1 << 20 && plan.seed == 9);
        assert_eq!(plan.seeks[0], Some(Seek { after: 3, kind: SeekKind::Byte, frac: 0xffff }));
        assert_eq!(plan.seeks[1].map(|s| s.kind), Some(SeekKind::Accurate));
    }
}
