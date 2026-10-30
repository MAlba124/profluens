//! Steady-state allocation audit of an MP4 to MKV passthrough remux, split into construct,
//! preroll and stream phases. `PF_ALLOC_PROFILE=1` samples a backtrace per allocation.
//! cargo run --release -p pf-mp4 --example remux_alloc_check -- input.mp4

#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // one-shot measurement harness

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pf_mp4::{Mp4Demux, Mp4Reader, Track};
use profluens_elements::io::{FileSink, FileSrc};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static SAMPLING: AtomicBool = AtomicBool::new(false);
static SAMPLE_EVERY: AtomicUsize = AtomicUsize::new(1);
static STACKS: Mutex<Option<HashMap<String, (usize, usize)>>> = Mutex::new(None);

thread_local! {
    /// Capturing a backtrace allocates, so the sampler must not count or recurse into itself.
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

// SAFETY: delegates every request to `System` with the same layout.
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

fn read_head(path: &str) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut at = 0u64;
    loop {
        if at + 8 > len {
            return Err(std::io::Error::other("no mdat box (is this a faststart MP4?)"));
        }
        let mut hdr = [0u8; 8];
        f.seek(SeekFrom::Start(at))?;
        f.read_exact(&mut hdr)?;
        let size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        if &hdr[4..8] == b"mdat" {
            break;
        }
        let advance = match size {
            0 => len - at,
            1 => {
                let mut big = [0u8; 8];
                f.read_exact(&mut big)?;
                u64::from_be_bytes(big)
            }
            n => n,
        };
        if advance == 0 {
            return Err(std::io::Error::other("zero-size box while scanning for mdat"));
        }
        at += advance;
    }
    let mut head = vec![0u8; at as usize];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut head)?;
    Ok(head)
}

fn remuxable(t: &Track) -> Option<&'static str> {
    match &t.entry.kind {
        b"avc1" | b"avc3" if !t.entry.config_record.is_empty() => Some("h264/avcc"),
        b"hvc1" | b"hev1" if !t.entry.config_record.is_empty() => Some("h265/hvcc"),
        b"vp09" => Some("vp9"),
        _ if t.family() == "aac" => Some("aac"),
        _ => None,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        eprintln!("usage: remux_alloc_check <input.mp4> [output.mkv]");
        std::process::exit(2);
    };
    let output = args.next().unwrap_or_else(|| "/dev/null".to_string());

    if std::env::var_os("PF_ALLOC_PROFILE").is_some() {
        SAMPLE_EVERY.store(
            std::env::var("PF_ALLOC_SAMPLE").ok().and_then(|s| s.parse().ok()).unwrap_or(1),
            Ordering::Relaxed,
        );
    }

    let t0 = ALLOCS.load(Ordering::Relaxed);
    let head = read_head(&input).expect("read MP4 head (ftyp+moov)");
    let head_len = head.len();
    let probe = Mp4Reader::new(&head).expect("resolve MP4 tables");
    let picked: Vec<(String, &'static str)> = probe
        .tracks()
        .iter()
        .filter_map(|t| remuxable(t).map(|fam| (format!("src_track{}", t.track_id), fam)))
        .collect();
    assert!(!picked.is_empty(), "no remuxable track");
    let ntracks = probe.tracks().len();
    let npicked = picked.len();
    let tracks: Vec<usize> =
        (0..ntracks).filter(|&i| remuxable(&probe.tracks()[i]).is_some()).collect();
    let pool = probe.tables().remux_pool(&tracks);
    drop(probe);

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&input));
    let demux = p.add(Mp4Demux::passthrough(head));
    p.link(src.src(FileSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");
    let t1 = ALLOCS.load(Ordering::Relaxed);

    let added = p.preroll().expect("preroll");
    let mux = p.add(pf_mkv::MkvMux::multi(npicked));
    let sink = p.add(FileSink::new(&output));
    for (i, (pad_name, _)) in picked.iter().enumerate() {
        let ap = added.srcs.iter().find(|ap| &ap.name == pad_name).expect("picked pad at preroll");
        p.link(ap.element.src(ap.pad), mux.sink(pf_mkv::MkvMuxN::INS[i])).expect("demux -> mux");
    }
    p.link(mux.src(pf_mkv::MkvMuxN::OUT), sink.sink(FileSink::IN)).expect("mux -> sink");
    p.set_element_pool_bytes(demux, 64 << 10, 512, pool.bytes);
    p.set_element_reserve(demux, pool.largest as usize, pool.held);
    let tap = p.tap_handle();
    let t2 = ALLOCS.load(Ordering::Relaxed);

    if std::env::var_os("PF_ALLOC_PROFILE").is_some() {
        SAMPLING.store(true, Ordering::Relaxed);
    }
    p.run().expect("remux run");
    SAMPLING.store(false, Ordering::Relaxed);
    let t3 = ALLOCS.load(Ordering::Relaxed);

    let s_src = tap.snapshot(src).unwrap_or_default();
    let s_demux = tap.snapshot(demux).unwrap_or_default();
    let s_mux = tap.snapshot(mux).unwrap_or_default();

    let construct = t1 - t0;
    let preroll = t2 - t1;
    let stream = t3 - t2;
    let frames = s_demux.buffers_out.max(1) as f64;

    println!("input: {input}  ({} tracks, {npicked} remuxed, head {head_len} B)", ntracks);
    println!(
        "src   : {:>8} buffers out, {:>12} B out ({} batches)",
        s_src.buffers_out, s_src.bytes_out, s_src.batches_out
    );
    println!(
        "demux : {:>8} buffers out, {:>12} B out ({} batches)",
        s_demux.buffers_out, s_demux.bytes_out, s_demux.batches_out
    );
    println!(
        "mux   : {:>8} buffers out, {:>12} B out ({} batches)",
        s_mux.buffers_out, s_mux.bytes_out, s_mux.batches_out
    );
    println!();
    println!("PHASE                allocs   per demuxed frame");
    println!("construct+link  {construct:>10}   (one-time)");
    println!("preroll         {preroll:>10}   (one-time)");
    println!("stream          {stream:>10}   {:>8.3}", stream as f64 / frames);
    println!("TOTAL           {:>10}   {:>8.3}", t3 - t0, (t3 - t0) as f64 / frames);
    println!(
        "peak live heap: {:.2} MiB   total bytes allocated: {:.2} MiB",
        PEAK.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
        BYTES.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
    );
    if let Some(d) = tap.pool_of(demux) {
        println!(
            "demux pool: {} regions mapped, {} out at most, {:.1} of {:.1} MiB, {:.1} MiB mapped at the end",
            d.slot_allocations,
            d.high_water,
            d.bytes_high as f64 / (1024.0 * 1024.0),
            d.bytes_budget as f64 / (1024.0 * 1024.0),
            d.bytes_mapped as f64 / (1024.0 * 1024.0),
        );
    }
    println!(
        "SUMMARY frames={} srcbufs={} muxbufs={} construct={} preroll={} stream={}",
        s_demux.buffers_out, s_src.buffers_out, s_mux.buffers_out, construct, preroll, stream
    );

    if let Some(map) = STACKS.lock().unwrap().take() {
        let every = SAMPLE_EVERY.load(Ordering::Relaxed);
        let mut rows: Vec<(String, (usize, usize))> = map.into_iter().collect();
        rows.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        println!("\nstream-phase allocation sites (sampled 1/{every}):");
        for (i, (site, (n, sz))) in rows.iter().enumerate() {
            if i < 30 {
                let short: String = site
                    .split(" \u{2190} ")
                    .map(|f| f.split_once('<').map_or(f, |(head, _)| head))
                    .collect::<Vec<_>>()
                    .join(" \u{2190} ");
                println!(
                    "  {:>8}  {:>7.3}/frame  {:>8} B avg  {short}",
                    n * every,
                    (n * every) as f64 / frames,
                    sz / n.max(&1)
                );
            }
            println!("SITE\t{}\t{}\t{}", n * every, sz / n.max(&1), site);
        }
    }
}
