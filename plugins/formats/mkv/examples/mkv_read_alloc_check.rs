//! Steady-state allocation audit of the Matroska read path in raw, recycle and pipeline modes.
//!     cargo run --release -p pf-mkv --example mkv_read_alloc_check -- file.mkv [mode]

#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // one-shot measurement harness

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pf_mkv::{MatroskaReader, MkvDemux};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_elements::io::FileSrc;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static SAMPLING: AtomicBool = AtomicBool::new(false);
static SAMPLE_EVERY: AtomicUsize = AtomicUsize::new(1);
static STACKS: Mutex<Option<HashMap<String, (usize, usize)>>> = Mutex::new(None);

thread_local! {
    static IN_SAMPLER: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

impl Counting {
    fn note(size: usize) {
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
        let Some((num, rest)) = t.split_once(": ") else {
            continue;
        };
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
        if out.len() == 6 {
            break;
        }
    }
    if out.is_empty() { "<unknown>".to_string() } else { out.join(" \u{2190} ") }
}

// SAFETY: delegates every request to `System`, adding only a counter and a guarded backtrace.
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

static SINK_OFFERS: [OfferDesc; 6] = [
    OfferDesc::any("h264"),
    OfferDesc::any("h265"),
    OfferDesc::any("aac"),
    OfferDesc::any("flac"),
    OfferDesc::any("vp8"),
    OfferDesc::any("bytes"),
];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "buffercountsink",
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

fn run_raw(path: &str, recycle: bool, chunk: usize) -> (usize, u64, u64) {
    let mut f = std::fs::File::open(path).expect("open");
    let mut r = MatroskaReader::new();
    let mut buf = vec![0u8; chunk];
    let setup = ALLOCS.load(Ordering::Relaxed);
    if std::env::var_os("PF_ALLOC_PROFILE").is_some() {
        SAMPLING.store(true, Ordering::Relaxed);
    }
    let (mut frames, mut bytes) = (0u64, 0u64);
    loop {
        let n = f.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        r.push(&buf[..n]).expect("parse");
        while let Some(fr) = r.next_frame() {
            frames += 1;
            bytes += fr.data.len() as u64;
            std::hint::black_box(fr.pts_ns);
            if recycle {
                r.recycle(fr.data);
            }
        }
    }
    SAMPLING.store(false, Ordering::Relaxed);
    (ALLOCS.load(Ordering::Relaxed) - setup, frames, bytes)
}

fn run_pipeline(path: &str) -> (usize, usize, u64, u64) {
    let mut f = std::fs::File::open(path).expect("open");
    let mut header = vec![0u8; 1 << 20];
    let n = f.read(&mut header).expect("read head");
    header.truncate(n);

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(path));
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(FileSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    let snk: Vec<_> = added
        .iter()
        .map(|ap| {
            let s = p.add(CountSink);
            p.link(ap.element.src(ap.pad), s.sink(CountSink::IN)).expect("demux -> sink");
            s
        })
        .collect();
    p.set_element_pool(src, 1 << 20, 96);
    let tap = p.tap_handle();
    let setup = ALLOCS.load(Ordering::Relaxed);

    if std::env::var_os("PF_ALLOC_PROFILE").is_some() {
        SAMPLING.store(true, Ordering::Relaxed);
    }
    p.run().expect("run");
    SAMPLING.store(false, Ordering::Relaxed);
    let stream = ALLOCS.load(Ordering::Relaxed) - setup;

    let s = tap.snapshot(demux).unwrap_or_default();
    let _ = snk;
    (stream, snk.len(), s.buffers_out, s.bytes_out)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: mkv_read_alloc_check <file.mkv> [mode]");
    let mode = std::env::args().nth(2).unwrap_or_else(|| "raw".into());
    let chunk: usize =
        std::env::var("PF_CHUNK").ok().and_then(|s| s.parse().ok()).unwrap_or(8 << 20);

    if std::env::var_os("PF_ALLOC_PROFILE").is_some() {
        SAMPLE_EVERY.store(
            std::env::var("PF_ALLOC_SAMPLE").ok().and_then(|s| s.parse().ok()).unwrap_or(1),
            Ordering::Relaxed,
        );
    }

    let t0 = ALLOCS.load(Ordering::Relaxed);
    let (stream, frames, bytes) = match mode.as_str() {
        "raw" => run_raw(&path, false, chunk),
        "recycle" => run_raw(&path, true, chunk),
        "pipeline" => {
            let (s, pads, f, b) = run_pipeline(&path);
            println!("pads: {pads}");
            (s, f, b)
        }
        other => panic!("unknown mode {other} (raw | recycle | pipeline)"),
    };
    let total = ALLOCS.load(Ordering::Relaxed) - t0;
    let setup = total - stream;
    let fr = frames.max(1) as f64;

    println!("input: {path}  mode: {mode}  (chunk {chunk} B)");
    println!("frames: {frames}   frame bytes: {bytes}");
    println!("PHASE                allocs   per frame");
    println!("setup           {setup:>10}   (one-time)");
    println!("stream          {stream:>10}   {:>8.3}", stream as f64 / fr);
    println!("TOTAL           {total:>10}   {:>8.3}", total as f64 / fr);
    println!(
        "peak live heap: {:.2} MiB   total bytes allocated: {:.2} MiB",
        PEAK.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
        BYTES.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
    );
    println!("SUMMARY frames={frames} setup={setup} stream={stream}");

    if let Some(map) = STACKS.lock().unwrap().take() {
        let every = SAMPLE_EVERY.load(Ordering::Relaxed);
        let mut rows: Vec<(String, (usize, usize))> = map.into_iter().collect();
        rows.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        println!("\nstream-phase allocation sites (sampled 1/{every}):");
        for (i, (site, (n, sz))) in rows.iter().enumerate() {
            if i < 25 {
                let short: String = site
                    .split(" \u{2190} ")
                    .map(|f| f.split_once('<').map_or(f, |(h, _)| h))
                    .collect::<Vec<_>>()
                    .join(" \u{2190} ");
                println!(
                    "  {:>8}  {:>7.3}/frame  {:>8} B avg  {short}",
                    n * every,
                    (n * every) as f64 / fr,
                    sz / n.max(&1)
                );
            }
            println!("SITE\t{}\t{}\t{}", n * every, sz / n.max(&1), site);
        }
    }
}
