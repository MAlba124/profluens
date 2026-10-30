//! The pf-tags scanner's performance regression tool. `--cold` evicts the page cache before each
//! timed pass, `--once` runs bare passes for profilers.

// App-side harness setup.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pf_tags::{Format, ScanConfig, Scanner, scan_parallel};

const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "spx", "m4a", "m4b", "mp4", "wav", "aif", "aiff", "aifc",
    "ape", "wv", "mpc", "webm",
];

const PASSES: usize = 5;

const WARM_THREADS: &[usize] = &[1, 2, 4, 8];

const COLD_THREADS: &[usize] = &[1, 8];

fn main() {
    let mut root: Option<PathBuf> = None;
    let mut cold = false;
    let mut once = false;
    let mut cfg = ScanConfig::default();
    let mut passes: Option<usize> = None;
    let mut thread_list: Option<Vec<usize>> = None;
    let mut evict_only = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cold" => cold = true,
            "--once" => once = true,
            "--evict" => evict_only = true,
            "--in-flight" => {
                cfg.in_flight = args.next().and_then(|v| v.parse().ok()).unwrap_or(cfg.in_flight);
            }
            "--tail" => {
                cfg.tail_len = args.next().and_then(|v| v.parse().ok()).unwrap_or(cfg.tail_len);
            }
            "--passes" => passes = args.next().and_then(|v| v.parse().ok()),
            "--threads" => {
                if let Some(v) = args.next() {
                    let list: Vec<usize> =
                        v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                    if !list.is_empty() {
                        thread_list = Some(list);
                    }
                }
            }
            "-h" | "--help" => return usage(),
            other => root = Some(PathBuf::from(other)),
        }
    }
    let Some(root) = root else { return usage() };

    let mut paths = Vec::new();
    collect(&root, &mut paths);
    paths.sort();
    if paths.is_empty() {
        println!("no audio files under {}", root.display());
        return;
    }

    if evict_only {
        let before = cached_kib();
        let t0 = Instant::now();
        evict(&paths);
        println!(
            "evicted {} file(s) in {:.3} s   Cached: {} -> {} KiB  (freed {} KiB)",
            paths.len(),
            t0.elapsed().as_secs_f64(),
            before,
            cached_kib(),
            before.saturating_sub(cached_kib()),
        );
        return;
    }

    let acc = Acc::default();

    if once {
        let n = passes.unwrap_or(1).max(1);
        let mut total = Duration::ZERO;
        let rchar0 = rchar();
        for _ in 0..n {
            total += pass_single(&paths, cfg, &acc);
        }
        let read = rchar().saturating_sub(rchar0);
        let files = acc.files.load(Ordering::Relaxed);
        println!(
            "{} files x {n} pass(es)  {:.3} s  {:.0} files/sec  {:.1} KiB read/file  checksum {}",
            paths.len(),
            total.as_secs_f64(),
            files as f64 / total.as_secs_f64().max(f64::MIN_POSITIVE),
            read as f64 / (n as f64 * paths.len() as f64 * 1024.0),
            std::hint::black_box(acc.checksum()),
        );
        return;
    }

    let passes = passes.unwrap_or(PASSES).max(1);
    let total_bytes: u64 = paths.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
    println!("pf-tags bench_scan");
    println!("root      : {}", root.display());
    println!("reactor   : {:?}", Scanner::new(cfg).reactor_kind());
    println!("mode      : {}   passes {passes}", if cold { "cold" } else { "warm" });
    println!("in_flight : {}   tail {} B", cfg.slots(), cfg.tail_len);
    println!(
        "files     : {}   {:.2} GiB",
        paths.len(),
        total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    );
    println!("by ext    : {}", ext_census(&paths));

    let prime = Acc::default();
    pass_single(&paths, cfg, &prime);
    println!("by format : {}", prime.format_census());
    println!(
        "art       : {} picture(s), {:.2} MiB, {} file(s) over the {} KiB slot",
        prime.pics.load(Ordering::Relaxed),
        prime.pic_bytes.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
        prime.big_art.load(Ordering::Relaxed),
        pf_tags::DEFAULT_PREFIX / 1024,
    );

    let threads: &[usize] =
        thread_list.as_deref().unwrap_or(if cold { COLD_THREADS } else { WARM_THREADS });
    println!(
        "\n{:>8}{:>11}{:>11}{:>11}{:>11}   passes (ms)",
        "threads", "best ms", "med ms", "best f/s", "med f/s"
    );
    for &t in threads {
        let mut times: Vec<Duration> = Vec::with_capacity(passes);
        for _ in 0..passes {
            if cold {
                evict(&paths);
            }
            let dt = if t == 1 {
                pass_single(&paths, cfg, &acc)
            } else {
                pass_parallel(&paths, t, cfg, &acc)
            };
            times.push(dt);
        }
        report(t, paths.len(), &times);
    }

    println!("\nchecksum  : {}", std::hint::black_box(acc.checksum()));
}

fn usage() {
    println!(
        "usage: bench_scan [--cold] [--once] [--passes N] [--threads 1,2,4,8] [--in-flight N] [--tail B] <dir>"
    );
}

/// One pass through [`Scanner`], built inside the timed region to match [`scan_parallel`].
fn pass_single(paths: &[PathBuf], cfg: ScanConfig, acc: &Acc) -> Duration {
    let t0 = Instant::now();
    let mut sc = Scanner::new(cfg);
    sc.scan(paths.iter(), |out| acc.take(out));
    t0.elapsed()
}

fn pass_parallel(paths: &[PathBuf], threads: usize, cfg: ScanConfig, acc: &Acc) -> Duration {
    let owned = paths.to_vec();
    let t0 = Instant::now();
    scan_parallel(owned, threads, cfg, |out| acc.take(out));
    t0.elapsed()
}

fn report(threads: usize, files: usize, times: &[Duration]) {
    let mut ms: Vec<f64> = times.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    ms.sort_by(f64::total_cmp);
    let best = ms[0];
    let med = ms[ms.len() / 2];
    let fps = |m: f64| files as f64 / (m / 1e3).max(f64::MIN_POSITIVE);
    let raw: Vec<String> = times.iter().map(|d| format!("{:.1}", d.as_secs_f64() * 1e3)).collect();
    println!(
        "{threads:>8}{best:>11.2}{med:>11.2}{:>11.0}{:>11.0}   {}",
        fps(best),
        fps(med),
        raw.join(" ")
    );
}

fn evict(paths: &[PathBuf]) {
    for p in paths {
        let Ok(f) = std::fs::File::open(p) else {
            continue;
        };
        let _ = pf_reactor::fadvise(f.as_raw_fd(), 0, 0, pf_reactor::POSIX_FADV_DONTNEED);
    }
}

/// `rchar` from `/proc/self/io`. Reads 0 on io_uring, whose reads bypass the read syscalls.
fn rchar() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/self/io") else {
        return 0;
    };
    s.lines()
        .find_map(|l| l.strip_prefix("rchar:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

fn cached_kib() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    s.lines()
        .find_map(|l| l.strip_prefix("Cached:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[derive(Default)]
struct Acc {
    files: AtomicU64,
    failed: AtomicU64,
    tags: AtomicU64,
    pics: AtomicU64,
    pic_bytes: AtomicU64,
    big_art: AtomicU64,
    ns: AtomicU64,
    fmt: [AtomicU64; FORMATS.len()],
}

impl Acc {
    fn take(&self, out: pf_tags::ScanOutcome<'_>) {
        match out.result {
            Err(_) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
            }
            Ok(r) => {
                self.files.fetch_add(1, Ordering::Relaxed);
                self.fmt[fmt_index(r.format)].fetch_add(1, Ordering::Relaxed);
                self.tags.fetch_add(r.tags.len() as u64, Ordering::Relaxed);
                let pics = r.tags.pictures();
                self.pics.fetch_add(pics.len() as u64, Ordering::Relaxed);
                for p in pics {
                    self.pic_bytes.fetch_add(p.data.len() as u64, Ordering::Relaxed);
                    if p.data.len() > pf_tags::DEFAULT_PREFIX {
                        self.big_art.fetch_add(1, Ordering::Relaxed);
                    }
                }
                self.ns.fetch_add(r.props.duration_ns.unwrap_or(0), Ordering::Relaxed);
            }
        }
    }

    fn checksum(&self) -> u64 {
        self.files.load(Ordering::Relaxed)
            ^ self.failed.load(Ordering::Relaxed).rotate_left(8)
            ^ self.tags.load(Ordering::Relaxed).rotate_left(16)
            ^ self.pics.load(Ordering::Relaxed).rotate_left(24)
            ^ self.pic_bytes.load(Ordering::Relaxed).rotate_left(32)
            ^ self.ns.load(Ordering::Relaxed).rotate_left(40)
    }

    fn format_census(&self) -> String {
        let mut s = String::new();
        for (i, name) in FORMATS.iter().enumerate() {
            let n = self.fmt[i].load(Ordering::Relaxed);
            if n > 0 {
                s.push_str(&format!("{name} {n}  "));
            }
        }
        s.trim_end().to_string()
    }
}

const FORMATS: [&str; 14] = [
    "flac",
    "mp3",
    "mp4",
    "ogg/opus",
    "ogg/vorbis",
    "ogg/flac",
    "wav",
    "mkv",
    "unknown",
    "ogg/speex",
    "aiff",
    "ape",
    "wavpack",
    "musepack",
];

// Indices 0..=8 are frozen so runs over an existing library stay byte-identical.
fn fmt_index(f: Format) -> usize {
    match f {
        Format::Flac => 0,
        Format::Mp3 => 1,
        Format::Mp4 => 2,
        Format::OggOpus => 3,
        Format::OggVorbis => 4,
        Format::OggFlac => 5,
        Format::Wav => 6,
        Format::Mkv => 7,
        Format::Unknown => 8,
        Format::OggSpeex => 9,
        Format::Aiff => 10,
        Format::Ape => 11,
        Format::WavPack => 12,
        Format::Musepack => 13,
    }
}

fn ext_census(paths: &[PathBuf]) -> String {
    let mut counts: Vec<(usize, usize)> = AUDIO_EXT.iter().map(|_| (0, 0)).collect();
    for (i, e) in AUDIO_EXT.iter().enumerate() {
        counts[i] = (i, paths.iter().filter(|p| has_ext(p, e)).count());
    }
    counts.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    let mut s = String::new();
    for (i, n) in counts {
        if n > 0 {
            s.push_str(&format!("{} {n}  ", AUDIO_EXT[i]));
        }
    }
    s.trim_end().to_string()
}

fn has_ext(p: &Path, ext: &str) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// Recursive walk. Symlinked directories are not followed so a self-link cannot hang it.
fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return;
    };
    if md.is_dir() {
        let Ok(rd) = std::fs::read_dir(path) else {
            return;
        };
        for e in rd.flatten() {
            collect(&e.path(), out);
        }
    } else if md.is_file() && AUDIO_EXT.iter().any(|e| has_ext(path, e)) {
        out.push(path.to_path_buf());
    }
}
