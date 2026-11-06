//! The reads a pulled playback asks of a high bitrate file, with every track or audio alone.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pf_mkv::MkvDemux;
use pf_mkv::ebml::id;
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pull::Ask;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SRC: ElementDesc = ElementDesc {
    name: "readsrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static DROP_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static DROP: ElementDesc = ElementDesc {
    name: "drop",
    sinks: &DROP_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// As `pf_open` sizes a demuxer's pool.
const POOL_SLOT: usize = 128 << 10;
const POOL_SLOTS: u32 = 1024;
const DEMUX_BYTES: u64 = 64 << 20;

#[derive(Default)]
struct Log {
    asks: Vec<(u64, u32)>,
    /// The asks waiting and their bytes each time the source ran.
    rounds: Vec<(u32, u64)>,
}

struct ReadSrc {
    data: Arc<Vec<u8>>,
    log: Arc<Mutex<Log>>,
}

impl Element for ReadSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.offer_ranges(SrcPad(0));
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut asks = Vec::new();
        while let Some(ask) = ctx.next_ask(SrcPad(0)) {
            asks.push(ask);
        }
        if !asks.is_empty() {
            let mut log = self.log.lock().unwrap();
            let bytes = asks.iter().map(|a| u64::from(a.len)).sum();
            log.rounds.push((asks.len() as u32, bytes));
            log.asks.extend(asks.iter().map(|a| (a.offset, a.len)));
        }
        for Ask { offset, len, tag, mut buf } in asks {
            let from = (offset as usize).min(self.data.len());
            let to = (from + len as usize).min(self.data.len());
            buf.memory.as_mut_full()[..to - from].copy_from_slice(&self.data[from..to]);
            buf.memory.set_len(to - from);
            ctx.serve(SrcPad(0), tag, buf);
        }
        Ok(if ctx.asks_over(SrcPad(0)) { Flow::Eos } else { Flow::Ok })
    }
}

struct Drop(Arc<Mutex<u64>>);

impl Element for Drop {
    fn desc(&self) -> &'static ElementDesc {
        &DROP
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {
            *self.0.lock().unwrap() += 1;
        }
        Ok(Flow::Ok)
    }
}

/// Keeps each buffer's bytes.
struct Keep(Arc<Mutex<Vec<Vec<u8>>>>);

impl Element for Keep {
    fn desc(&self) -> &'static ElementDesc {
        &DROP
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.0.lock().unwrap().push(buf.memory.data().to_vec());
        }
        Ok(Flow::Ok)
    }
}

/// What a playback read.
#[derive(Debug, Default)]
struct Reads {
    count: usize,
    /// Bytes served, none past the end of the file.
    bytes: u64,
    largest: u32,
    /// Stretches of the file between its first and last read never read, their count and bytes.
    gaps: (usize, u64),
    /// Bytes served more than once.
    again: u64,
    /// The most asks and ask bytes out at once.
    depth: (u32, u64),
    mean_depth: f64,
    buffers: u64,
}

impl Reads {
    fn of(log: &Log, file_len: u64, buffers: u64) -> Reads {
        let mut r = Reads { count: log.asks.len(), buffers, ..Reads::default() };
        let mut spans: Vec<(u64, u64)> = log
            .asks
            .iter()
            .map(|&(o, l)| (o.min(file_len), (o + u64::from(l)).min(file_len)))
            .collect();
        for &(o, e) in &spans {
            r.bytes += e - o;
        }
        r.largest = log.asks.iter().map(|a| a.1).max().unwrap_or(0);
        spans.sort_unstable();
        let mut covered = 0;
        let mut end: Option<u64> = None;
        for (o, e) in spans {
            match end {
                Some(x) if o > x => {
                    r.gaps = (r.gaps.0 + 1, r.gaps.1 + (o - x));
                    covered += e - o;
                }
                Some(x) => covered += e.saturating_sub(x),
                None => covered += e - o,
            }
            end = Some(end.map_or(e, |x| x.max(e)));
        }
        r.again = r.bytes - covered;
        for &(n, bytes) in &log.rounds {
            r.depth = (r.depth.0.max(n), r.depth.1.max(bytes));
        }
        let rounds = log.rounds.len().max(1) as f64;
        r.mean_depth = log.rounds.iter().map(|(n, _)| f64::from(*n)).sum::<f64>() / rounds;
        r
    }

    fn print(&self, what: &str, seconds: f64) {
        let mean = self.bytes / self.count.max(1) as u64;
        println!(
            "{what}: {} reads, {:.1}/s of media, mean {} KiB, largest {} KiB, {:.1} MiB read, \
             {:.1} MiB of it again, gaps {} ({:.1} MiB), depth max {} ({} KiB) mean {:.2}, \
             {} buffers out",
            self.count,
            self.count as f64 / seconds,
            mean >> 10,
            self.largest >> 10,
            self.bytes as f64 / (1 << 20) as f64,
            self.again as f64 / (1 << 20) as f64,
            self.gaps.0,
            self.gaps.1 as f64 / (1 << 20) as f64,
            self.depth.0,
            self.depth.1 >> 10,
            self.mean_depth,
            self.buffers,
        );
    }
}

fn header(file: &[u8]) -> Vec<u8> {
    file[..file.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster")].to_vec()
}

/// Plays `file` pulled with the tracks numbered in `tracks` linked.
fn play(file: &Arc<Vec<u8>>, tracks: &[u64]) -> Reads {
    let mut p = pf_reactor::pipeline();
    let log = Arc::new(Mutex::new(Log::default()));
    let src = p.add(ReadSrc { data: Arc::clone(file), log: Arc::clone(&log) });
    let demux = p.add(MkvDemux::new(header(file)));
    p.link(src.src(SrcPad(0)), demux.sink(MkvDemux::IN)).expect("src ! demux");
    p.set_element_pool_bytes(demux, POOL_SLOT, POOL_SLOTS, DEMUX_BYTES);
    let pads = p.preroll().expect("preroll").srcs;
    let seen = Arc::new(Mutex::new(0));
    for (n, pad) in pads.iter().enumerate().filter(|(n, _)| tracks.contains(&(*n as u64 + 1))) {
        let sink = p.add(Drop(Arc::clone(&seen)));
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0)))
            .unwrap_or_else(|e| panic!("pad {n}: {e}"));
    }
    p.run().expect("run");
    let buffers = *seen.lock().unwrap();
    Reads::of(&log.lock().unwrap(), file.len() as u64, buffers)
}

/// 1080p H.264 at about 26 Mbit/s as track 1, Opus as track 2, as ffmpeg interleaves them.
fn film(seconds: u32) -> Option<Arc<Vec<u8>>> {
    let path = std::env::temp_dir().join(format!("pf_mkv_pull_reads_{seconds}s_v1.mkv"));
    if let Ok(data) = std::fs::read(&path) {
        return Some(Arc::new(data));
    }
    // tests make it at once, each its own copy until the rename
    static MADE: AtomicU32 = AtomicU32::new(0);
    let n = MADE.fetch_add(1, Ordering::Relaxed);
    let made = path.with_extension(format!("{}.{n}.mkv", std::process::id()));
    let d = format!("duration={seconds}");
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size=1920x1080:rate=24:{d}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:sample_rate=48000:{d}"))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-crf", "10"])
        .args(["-c:a", "libopus", "-b:a", "128k", "-ac", "2"])
        .arg(&made)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: ffmpeg could not make the film");
        let _ = std::fs::remove_file(&made);
        return None;
    }
    std::fs::rename(&made, &path).expect("the film in place");
    Some(Arc::new(std::fs::read(&path).expect("the film")))
}

/// Prints the reads of the file at `PF_MKV_READS` or a 60 s film.
#[test]
#[ignore = "a measurement, run with --ignored --nocapture"]
fn report_the_reads() {
    let file = match std::env::var_os("PF_MKV_READS") {
        Some(path) => Arc::new(std::fs::read(PathBuf::from(path)).expect("the file")),
        None => match film(60) {
            Some(f) => f,
            None => return,
        },
    };
    let seconds = 60.0;
    play(&file, &[1, 2]).print("video and audio", seconds);
    play(&file, &[2]).print("audio alone", seconds);
}

// Before the chain of windows the 60 s film took 2874 reads of 74 KiB on average with every
// track, one window out at a time, and 1442 reads of 16 KiB with audio alone. With the chain but
// each frame past a window read alone it took 201 reads, 12.8 MiB of them twice.
#[test]
fn a_high_rate_film_is_read_in_few_large_reads_some_out_at_once() {
    let Some(file) = film(10) else { return };
    let r = play(&file, &[1, 2]);
    r.print("video and audio", 10.0);
    assert!(r.buffers >= 240 + 500, "every frame out, {} buffers", r.buffers);
    assert!(r.count <= 32, "{} reads for 10 s", r.count);
    assert!(r.bytes / r.count as u64 >= 512 << 10, "{r:?}");
    assert_eq!(r.gaps, (0, 0));
    assert!(r.again < 1 << 20, "frames past a window joined from the chain, {r:?}");
    assert!(r.depth.0 >= 3, "windows ahead with the one walked, {r:?}");
}

#[test]
fn audio_alone_reads_the_video_between_through() {
    let Some(file) = film(10) else { return };
    let r = play(&file, &[2]);
    r.print("audio alone", 10.0);
    assert!(r.buffers >= 500, "every audio frame out, {} buffers", r.buffers);
    assert!(r.count <= 30, "{} reads for 10 s", r.count);
    assert_eq!(r.gaps, (0, 0), "the video frames are under the gap jumped");
    assert!(r.depth.0 >= 3, "windows ahead with the one walked, {r:?}");
}

/// A frame past the walked window is joined from the window's tail, the chain's windows and a
/// read of any rest. Each comes out as written, whichever of those held it.
#[test]
fn frames_past_a_window_come_out_byte_for_byte() {
    use pf_mkv::{MatroskaWriter, TrackConfig};
    // across the small first windows, held by the chain of 2 MiB ones, and past all of it
    let sizes = [100, 40_000, 300, 700_000, 5_000, 3 << 20, 200, 9 << 20, 1_000, 1_500_000, 50];
    let frames: Vec<Vec<u8>> = sizes
        .iter()
        .enumerate()
        .map(|(k, &n)| {
            let mut x = k as u32 + 1;
            let mut next = || {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8
            };
            (0..n).map(|_| next()).collect()
        })
        .collect();
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240)]);
    let mut out = Vec::new();
    w.write_header(&mut out).expect("header");
    for (k, f) in frames.iter().enumerate() {
        w.write_frame(&mut out, 1, k as u64 * 40_000_000, f, k == 0).expect("frame");
    }
    w.finalize(&mut out);
    let file = Arc::new(out);
    let mut p = pf_reactor::pipeline();
    let log = Arc::new(Mutex::new(Log::default()));
    let src = p.add(ReadSrc { data: Arc::clone(&file), log: Arc::clone(&log) });
    let demux = p.add(MkvDemux::new(header(&file)));
    p.link(src.src(SrcPad(0)), demux.sink(MkvDemux::IN)).expect("src ! demux");
    p.set_element_pool_bytes(demux, POOL_SLOT, POOL_SLOTS, DEMUX_BYTES);
    let pads = p.preroll().expect("preroll").srcs;
    let kept = Arc::new(Mutex::new(Vec::new()));
    let sink = p.add(Keep(Arc::clone(&kept)));
    p.link(pads[0].element.src(pads[0].pad), sink.sink(SinkPad(0))).expect("pad ! keep");
    p.run().expect("run");
    let r = Reads::of(&log.lock().unwrap(), file.len() as u64, 0);
    let kept = kept.lock().unwrap();
    assert_eq!(kept.len(), frames.len(), "every frame out, {r:?}");
    for (k, (got, want)) in kept.iter().zip(&frames).enumerate() {
        assert!(got == want, "frame {k} of {} bytes came out as {}", want.len(), got.len());
    }
    assert!(r.again < 1 << 20, "{r:?}");
}
