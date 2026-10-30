//! Allocations per Opus packet over a whole `filesrc ! flacdec ! opusenc ! sink` transcode
//! through the real scheduler.

#![allow(clippy::disallowed_methods)] // one-shot measurement harness

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;

use pf_libflac::FlacDec;
use pf_opus::OpusEnc;
use profluens_elements::io::FileSrc;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
/// A bump arena trades allocation count for retention, so the peak is watched too.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static SAMPLING: AtomicBool = AtomicBool::new(false);
static SAMPLE_EVERY: AtomicUsize = AtomicUsize::new(8);
static STACKS: Mutex<Option<HashMap<String, (usize, usize)>>> = Mutex::new(None);

thread_local! {
    static IN_SAMPLER: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

impl Counting {
    fn note(size: usize) {
        // The guard wraps the whole body, a backtrace allocates and must not be counted.
        let _ = IN_SAMPLER.try_with(|g| {
            if g.get() {
                return;
            }
            g.set(true);
            let n = ALLOCS.fetch_add(1, Ordering::Relaxed) + 1;
            BYTES.fetch_add(size, Ordering::Relaxed);
            PEAK.fetch_max(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
            if SAMPLING.load(Ordering::Relaxed)
                && n.is_multiple_of(SAMPLE_EVERY.load(Ordering::Relaxed))
            {
                let key = capture_site();
                if let Ok(mut guard) = STACKS.lock() {
                    let map = guard.get_or_insert_with(HashMap::new);
                    let e = map.entry(key).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += size;
                }
            }
            g.set(false);
        });
    }
}

const PLUMBING: &[&str] = &[
    "try_allocate_in",
    "allocate_in",
    "RawVec",
    "raw_vec",
    "exchange_malloc",
    "finish_grow",
    "grow_amortized",
    "grow_one",
    "do_reserve_and_handle",
    "reserve",
    "with_capacity",
    "from_iter",
    "to_vec",
    "into_vec",
    "extend_desugared",
    "SpecFrom",
    "SpecExtend",
    "spec_extend",
    "into_boxed_slice",
    "alloc::alloc",
    "alloc_impl",
    "grow_impl",
    "shrink_impl",
    "realloc_nonnull",
    "__rust",
];

const PLUMBING_EXACT: &[&str] = &["alloc", "realloc", "alloc_zeroed", "allocate", "grow", "shrink"];

fn capture_site() -> String {
    let bt = std::backtrace::Backtrace::force_capture().to_string();
    let mut frames: Vec<&str> = Vec::new();
    for line in bt.lines() {
        let t = line.trim_start();
        let Some((num, rest)) = t.split_once(": ") else { continue };
        if !num.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        frames.push(rest.trim());
    }
    let start = frames.iter().rposition(|f| f.ends_with("note") || f.contains("capture_site"));
    let tail = start.map_or(&frames[..], |i| &frames[i + 1..]);
    let mut out: Vec<&str> = Vec::new();
    for f in tail {
        if PLUMBING.iter().any(|p| f.contains(p)) || PLUMBING_EXACT.contains(f) {
            continue;
        }
        out.push(f);
        if out.len() == 4 {
            break;
        }
    }
    if out.is_empty() { "<unknown>".to_string() } else { out.join(" <- ") }
}

// SAFETY: delegates to `System`, adds a relaxed counter and a guarded backtrace capture.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        unsafe {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
            Self::note(l.size());
            System.alloc(l)
        }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe {
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
            System.dealloc(p, l)
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        unsafe {
            LIVE.fetch_add(new.wrapping_sub(l.size()), Ordering::Relaxed);
            Self::note(new);
            System.realloc(p, l, new)
        }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        unsafe {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
            Self::note(l.size());
            System.alloc_zeroed(l)
        }
    }
}

#[global_allocator]
static A: Counting = Counting;

static PKT: AtomicUsize = AtomicUsize::new(0);

struct CountSink;

static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any("opus"), OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "packetcountsink",
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

impl CountSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for CountSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {
            PKT.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }
}

fn main() {
    // The Active sink drains last, so the whole run is measured against the packet count.
    let flac = std::env::args().nth(1).unwrap_or_else(|| {
        let long = "/tmp/sc_long.flac";
        if std::path::Path::new(long).exists() {
            long.into()
        } else {
            "fixtures/out/audio.flac".into()
        }
    });
    if !std::path::Path::new(&flac).exists() {
        eprintln!("missing fixture {flac}");
        std::process::exit(2);
    }

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&flac));
    let dec = p.add(FlacDec::new());
    let enc = p.add(OpusEnc::new());
    let snk = p.add(CountSink);
    p.link(src.src(FileSrc::OUT), dec.sink(FlacDec::IN)).expect("filesrc->flacdec");
    p.link(dec.src(FlacDec::OUT), enc.sink(OpusEnc::IN)).expect("flacdec->opusenc");
    p.link(enc.src(OpusEnc::OUT), snk.sink(CountSink::IN)).expect("opusenc->sink");

    // Baselined right before the run so construction does not count.
    if std::env::var_os("PF_ALLOC_PROFILE").is_some() {
        SAMPLE_EVERY.store(
            std::env::var("PF_ALLOC_SAMPLE").ok().and_then(|s| s.parse().ok()).unwrap_or(1),
            Ordering::Relaxed,
        );
        SAMPLING.store(true, Ordering::Relaxed);
    }
    let base = ALLOCS.load(Ordering::Relaxed);
    p.run().expect("run");
    SAMPLING.store(false, Ordering::Relaxed);
    let total_allocs = ALLOCS.load(Ordering::Relaxed).saturating_sub(base);
    let pkts = PKT.load(Ordering::Relaxed);

    println!("fixture: {flac}");
    println!("Opus packets: {pkts}   run allocations: {total_allocs}");
    if pkts > 0 {
        println!(
            "FLAC->Opus transcode: {:.1} allocs/pkt  (flacdec decode + opusenc encode + scheduler; \
             one-time startup amortized)",
            total_allocs as f64 / pkts as f64
        );
    }

    if let Some(map) = STACKS.lock().unwrap().take() {
        let every = SAMPLE_EVERY.load(Ordering::Relaxed);
        let mut rows: Vec<(String, (usize, usize))> = map.into_iter().collect();
        rows.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        println!("\nallocation sites (sampled 1/{every}):");
        for (site, (n, sz)) in rows.iter().take(25) {
            let site: String = site
                .split(" \u{2190} ")
                .map(|f| f.split_once('<').map_or(f, |(head, _)| head))
                .collect::<Vec<_>>()
                .join(" \u{2190} ");
            println!("  {:>7}  {:>8} B avg  {site}", n * every, sz / n.max(&1));
        }
    }
}
