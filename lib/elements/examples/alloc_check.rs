//! Steady-state allocations per buffer for the built-in elements, measured with a
//! counting global allocator. Subtract the `testsrc ! testsink` baseline to isolate one.

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
use profluens_core::pipeline::Pipeline;
use profluens_elements::flow::{PassThrough, Queue, Tee};
use profluens_elements::io::{FileSink, FileSrc};
use profluens_elements::testing::{TestSink, TestSrc};

// Mutates what it receives. Behind a `tee` that takes the copy-on-write branch.
static MUT_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static MUT_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &MUT_OFFERS }];
static MUT_DESC: ElementDesc = ElementDesc {
    name: "mutsink",
    sinks: &MUT_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
struct MutSink;
impl MutSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for MutSink {
    fn desc(&self) -> &'static ElementDesc {
        &MUT_DESC
    }
    fn process(&mut self, _c: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(mut buf) = inputs.pop() {
            buf.memory.as_mut_full()[0] ^= 1;
        }
        Ok(Flow::Ok)
    }
}

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
        if out.len() == 4 {
            break;
        }
    }
    if out.is_empty() { "<unknown>".to_string() } else { out.join(" \u{2190} ") }
}

// SAFETY: every request is delegated to `System`.
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

fn dump_sites() {
    if let Some(map) = STACKS.lock().unwrap().take() {
        let every = SAMPLE_EVERY.load(Ordering::Relaxed);
        let mut rows: Vec<(String, (usize, usize))> = map.into_iter().collect();
        rows.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        println!("  allocation sites (sampled 1/{every}):");
        for (site, (n, sz)) in rows.iter().take(18) {
            let site: String = site
                .split(" \u{2190} ")
                .map(|f| f.split_once('<').map_or(f, |(head, _)| head))
                .collect::<Vec<_>>()
                .join(" \u{2190} ");
            println!("    {:>8}  {:>8} B avg  {site}", n * every, sz / n.max(&1));
        }
    }
}

fn measure(
    name: &str,
    want: Option<&str>,
    build: impl FnOnce() -> (Pipeline, Box<dyn Fn(&Pipeline) -> u64>),
) {
    if let Some(w) = want
        && !name.contains(w)
    {
        return;
    }
    let (mut p, count) = build();
    let profile = std::env::var_os("PF_ALLOC_PROFILE").is_some();
    if profile {
        SAMPLE_EVERY.store(
            std::env::var("PF_ALLOC_SAMPLE").ok().and_then(|s| s.parse().ok()).unwrap_or(1),
            Ordering::Relaxed,
        );
        SAMPLING.store(true, Ordering::Relaxed);
    }
    let base = ALLOCS.load(Ordering::Relaxed);
    let base_bytes = BYTES.load(Ordering::Relaxed);
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    p.run().expect("run");
    SAMPLING.store(false, Ordering::Relaxed);
    let allocs = ALLOCS.load(Ordering::Relaxed) - base;
    let bytes = BYTES.load(Ordering::Relaxed) - base_bytes;
    let bufs = count(&p);
    println!(
        "{name:<38} {bufs:>8} buffers  {allocs:>8} allocs  {:>7.4} allocs/buf  {:>6} KiB peak-live  {:>8} KiB total",
        allocs as f64 / bufs.max(1) as f64,
        PEAK.load(Ordering::Relaxed) / 1024,
        bytes / 1024,
    );
    if profile {
        dump_sites();
    }
}

fn main() {
    let want = std::env::args().nth(1);
    let want = want.as_deref();
    // 64 MiB in 64 KiB slots so startup amortizes. `PF_ALLOC_MIB` scales it.
    let total: u64 =
        std::env::var("PF_ALLOC_MIB").ok().and_then(|s| s.parse().ok()).unwrap_or(64) << 20;
    #[allow(non_snake_case)]
    let TOTAL = total;
    const SLOT: usize = 64 << 10;
    const SLOTS: u32 = 16;

    measure("testsrc ! testsink (baseline)", want, || {
        let (sink, _stats) = TestSink::new();
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(TestSrc::new(TOTAL));
        let snk = p.add(sink);
        p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    measure("testsrc ! passthrough ! testsink", want, || {
        let (sink, _stats) = TestSink::new();
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(TestSrc::new(TOTAL));
        let pt = p.add(PassThrough::new());
        let snk = p.add(sink);
        p.link(src.src(TestSrc::OUT), pt.sink(PassThrough::IN)).unwrap();
        p.link(pt.src(PassThrough::OUT), snk.sink(TestSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    measure("testsrc ! queue ! testsink", want, || {
        let (sink, _stats) = TestSink::new();
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(TestSrc::new(TOTAL));
        let q = p.add(Queue::new());
        let snk = p.add(sink);
        p.link(src.src(TestSrc::OUT), q.sink(Queue::IN)).unwrap();
        p.link(q.src(Queue::OUT), snk.sink(TestSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    measure("testsrc ! tee(4) ! 4x testsink", want, || {
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(TestSrc::new(TOTAL));
        let t = p.add(Tee::new(4));
        p.link(src.src(TestSrc::OUT), t.sink(Tee::IN)).unwrap();
        for i in 0..4 {
            let (sink, _stats) = TestSink::new();
            let snk = p.add(sink);
            p.link(t.src(Tee::OUTS[i]), snk.sink(TestSink::IN)).unwrap();
        }
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    measure("testsrc ! mutsink (unshared)", want, || {
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(TestSrc::new(TOTAL));
        let m = p.add(MutSink);
        p.link(src.src(TestSrc::OUT), m.sink(MutSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    // Behind a tee the same mutation copy-on-writes a whole slot per buffer.
    measure("testsrc ! tee(2) ! mutsink + testsink", want, || {
        let (sink, _stats) = TestSink::new();
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(TestSrc::new(TOTAL));
        let t = p.add(Tee::new(2));
        let m = p.add(MutSink);
        let snk = p.add(sink);
        p.link(src.src(TestSrc::OUT), t.sink(Tee::IN)).unwrap();
        p.link(t.src(Tee::OUTS[0]), m.sink(MutSink::IN)).unwrap();
        p.link(t.src(Tee::OUTS[1]), snk.sink(TestSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    // With a dry pool every copy-on-write becomes an unpooled heap box.
    measure("testsrc ! tee(2) ! mutsink + testsink [tiny pool]", want, || {
        let (sink, _stats) = TestSink::new();
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, 2);
        let src = p.add(TestSrc::new(TOTAL));
        let t = p.add(Tee::new(2));
        let m = p.add(MutSink);
        let snk = p.add(sink);
        p.link(src.src(TestSrc::OUT), t.sink(Tee::IN)).unwrap();
        p.link(t.src(Tee::OUTS[0]), m.sink(MutSink::IN)).unwrap();
        p.link(t.src(Tee::OUTS[1]), snk.sink(TestSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    let inp = std::env::temp_dir().join("pf_alloc_check_in.bin");
    let outp = std::env::temp_dir().join("pf_alloc_check_out.bin");
    if std::fs::metadata(&inp).map(|m| m.len()).unwrap_or(0) != TOTAL {
        std::fs::write(&inp, vec![0x5Au8; TOTAL as usize]).unwrap();
    }

    measure("filesrc ! testsink", want, || {
        let (sink, _stats) = TestSink::new();
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(FileSrc::new(&inp));
        let snk = p.add(sink);
        p.link(src.src(FileSrc::OUT), snk.sink(TestSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    measure("filesrc ! filesink", want, || {
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(FileSrc::new(&inp));
        let snk = p.add(FileSink::new(&outp));
        p.link(src.src(FileSrc::OUT), snk.sink(FileSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    measure("filesrc ! queue ! filesink", want, || {
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, SLOTS);
        let src = p.add(FileSrc::new(&inp));
        let q = p.add(Queue::new());
        let snk = p.add(FileSink::new(&outp));
        p.link(src.src(FileSrc::OUT), q.sink(Queue::IN)).unwrap();
        p.link(q.src(Queue::OUT), snk.sink(FileSink::IN)).unwrap();
        (p, Box::new(move |p: &Pipeline| p.counters(src).buffers_out))
    });

    let _ = std::fs::remove_file(&outp);
}
