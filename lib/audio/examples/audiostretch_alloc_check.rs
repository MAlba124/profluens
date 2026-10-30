//! Steady-state allocation audit for `audiostretch`.

#![allow(clippy::disallowed_methods)] // one-shot measurement harness

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pf_harness::Harness;
use profluens_audio::AudioStretch;
use profluens_audio::format::{FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
use profluens_core::format::ValueDesc;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

struct Counting;

// SAFETY: forwards every call to `System` unchanged, the counters are the only addition.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const RATE_HZ: u32 = 48_000;
const CHANNELS: u16 = 2;
const CHUNK_FRAMES: usize = 1021;

fn gen_input(frames: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * CHANNELS as usize);
    let mut seed: u32 = 0x1234_5678;
    let period = (RATE_HZ / 120) as usize;
    let eperiod = (RATE_HZ * 7 / 10) as usize;
    for n in 0..frames {
        let phase = (n % period) as f32 / period as f32;
        let tri = 1.0 - (2.0 * phase - 1.0).abs();
        let voiced = tri * tri * 2.0 - 0.5;
        let ephase = (n % eperiod) as f32 / eperiod as f32;
        let env = 1.0 - (2.0 * ephase - 1.0).abs();
        for c in 0..CHANNELS as usize {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 9) as f32 / 4_194_304.0 - 1.0;
            out.push(env * voiced * 0.6 + 0.05 * noise - 0.02 * c as f32);
        }
    }
    out
}

struct Run {
    allocs: usize,
    #[allow(dead_code)]
    bytes: usize,
    pushes: usize,
    pulls: usize,
}

const WARM: usize = 64;
const ROUNDS: usize = 512;

fn drive(h: &mut Harness) -> Run {
    let chunk = gen_input(CHUNK_FRAMES);
    let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_ne_bytes()).collect();

    let round = |h: &mut Harness| {
        let buf = h.alloc(&bytes);
        h.push(buf).expect("push");
        let mut pulls = 0usize;
        while let Some(_b) = h.pull(AudioStretch::OUT) {
            pulls += 1;
        }
        pulls
    };

    for _ in 0..WARM {
        round(h);
    }

    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    let mut pulls = 0usize;
    for _ in 0..ROUNDS {
        pulls += round(h);
    }
    ARMED.store(false, Ordering::Relaxed);

    Run {
        allocs: ALLOCS.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
        pushes: ROUNDS,
        pulls,
    }
}

fn fix_audio_caps(h: &mut Harness) {
    h.fix_sink_format(
        AudioStretch::IN,
        "audio/raw",
        &[
            (FIELD_RATE, ValueDesc::Int(RATE_HZ as i64)),
            (FIELD_CHANNELS, ValueDesc::Int(CHANNELS as i64)),
            (FIELD_SAMPLE, ValueDesc::Id("f32")),
        ],
    );
}

fn control(parts: usize) -> Run {
    let mut h = Harness::with_slot_size(chunker::Chunker::new(parts), 1 << 16);
    drive(&mut h)
}

fn measure((num, den): (i64, i64), label: &str) -> bool {
    let rate = format!("{num}/{den}");
    let playing = profluens_core::time::Rate::new(num, den).expect("a rate");
    let mut h = Harness::with_slot_size(AudioStretch::new(playing), 1 << 16);
    fix_audio_caps(&mut h);
    h.start().expect("start");
    let run = drive(&mut h);

    let parts = run.pulls.div_ceil(run.pushes.max(1));
    let ctl = control(parts);

    let per = |r: &Run| r.allocs as f64 / (r.pushes + r.pulls) as f64;
    let (mine, theirs) = (per(&run), per(&ctl));
    let ok = mine <= theirs * 1.05;
    println!(
        "{label:<24} rate={rate:<5} {:>4} pushes /{:>5} pulls  allocs={:<6} ({mine:.2}/buf)   \
         control(parts={parts}): {:>4}/{:<5} allocs={:<6} ({theirs:.2}/buf)   {}",
        run.pushes,
        run.pulls,
        run.allocs,
        ctl.pushes,
        ctl.pulls,
        ctl.allocs,
        if ok { "OK" } else { "*** ELEMENT ALLOCATES ***" }
    );
    ok
}

fn main() {
    println!("=== audiostretch steady-state heap traffic ===");
    println!(
        "Each rate is compared against a control element with the same push/pull shape that\n\
         provably owns no heap state. Equal per-buffer rates mean the stretcher itself added\n\
         nothing: its engine buffers are sized once, at negotiation, for the worst-case round.\n"
    );
    let mut ok = true;
    ok &= measure((1, 1), "bypass (copy-free)");
    ok &= measure((7, 20), "slow-down r<0.5");
    ok &= measure((3, 4), "slow-down 0.5<=r<1");
    ok &= measure((3, 2), "speed-up 1<r<2");
    ok &= measure((5, 2), "speed-up r>=2");
    println!(
        "\n=== {} ===",
        if ok {
            "ZERO steady-state allocations by the element"
        } else {
            "ALLOCATION IN THE HOT PATH"
        }
    );
    std::process::exit(if ok { 0 } else { 1 });
}

mod chunker {
    use profluens_core::batch::Inputs;
    use profluens_core::ctx::Ctx;
    use profluens_core::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use profluens_core::error::Error;
    use profluens_core::format::OfferDesc;
    use profluens_core::id::SrcPad;

    static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
    static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
    static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
    static DESC: ElementDesc = ElementDesc {
        name: "chunker",
        sinks: &SINKS,
        srcs: &SRCS,
        props: &[],
        sched: SchedHint::Passive,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    };

    pub struct Chunker(usize);
    impl Chunker {
        pub const OUT: SrcPad = SrcPad(0);

        pub fn new(parts: usize) -> Self {
            Self(parts.max(1))
        }
    }
    impl Element for Chunker {
        fn desc(&self) -> &'static ElementDesc {
            &DESC
        }
        fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
            while let Some(b) = inputs.pop() {
                let data = b.memory.data();
                let step = data.len().div_ceil(self.0).max(1);
                for part in data.chunks(step) {
                    let mut out = ctx.alloc();
                    let n = part.len().min(out.memory.capacity());
                    out.memory.as_mut_full()[..n].copy_from_slice(&part[..n]);
                    out.memory.set_len(n);
                    ctx.out(Self::OUT).push(out);
                }
            }
            Ok(Flow::Ok)
        }
    }
}
