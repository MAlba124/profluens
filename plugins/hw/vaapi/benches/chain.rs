//! The GPU chain on a real file, timed: `filesrc ! demux ! vaapidec ! [vaapipostproc !] vaapienc`,
//! each variant of handoff, in-flight depth and scheduling in turn. Hand-rolled, `main` is the
//! harness. Skips without a VA device.
//!
//! `cargo bench -p pf-vaapi --bench chain -- FILE [--frames N] [--codec h264|h265] [--vpp WxH]
//! [--kbps K] [--qp Q] [--variants done1t,queued2i,...] [--repeat R] [--no-warm]`
//!
//! A variant is the handoff (`done`, the producer waits for each picture, or `queued`, the
//! dma-buf's fence orders the consumer), the encoder's in-flight depth (1 to 4) and the
//! scheduling (`t`, each stage on its own thread and the encoder polling with timed wakes, `b`,
//! the same with the encoder waiting in the driver, or `i`, the chain inline on the decoder's).
//! Each row gives the frames coded, the rate once the first is out, the CPU share, the GPU's
//! mean clock, the time the decoder and the encoder stood waiting for the GPU, the encoder's
//! timed wakes, the first picture's latency and a hash of the coded bytes, equal across variants
//! at a fixed quantizer when the GPU ordered every read behind its write.

// A bench's own fixtures, files and threads, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pf_open::pipeline::{demuxer, route};
use pf_open::{Caps, OpenAt, Opened, TrackDesc, TrackKind, Want};
use pf_vaapi::gpuframe::{GpuFrameChannel, Handoff};
use pf_vaapi::stats::{Snapshot, Stats};
use pf_vaapi::vpp::{Target, VaapiPostProc};
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc, VaapiH265Dec, VaapiH265Enc};
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::command::Control;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::fetch::drive_file;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sched {
    /// Each stage on its own thread, the encoder polling with timed wakes.
    Threads,
    /// Each stage on its own thread, the encoder waiting in the driver.
    Blocking,
    /// The chain inline on the decoder's thread.
    Inline,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Variant {
    handoff: Handoff,
    depth: usize,
    sched: Sched,
}

impl Variant {
    /// `done1t`, `queued3i`.
    fn parse(s: &str) -> Option<Variant> {
        let (handoff, rest) = match s.strip_prefix("queued") {
            Some(r) => (Handoff::Queued, r),
            None => (Handoff::Done, s.strip_prefix("done")?),
        };
        let (depth, sched) = rest.split_at(rest.len().checked_sub(1)?);
        let sched = match sched {
            "t" => Sched::Threads,
            "b" => Sched::Blocking,
            "i" => Sched::Inline,
            _ => return None,
        };
        Some(Variant { handoff, depth: depth.parse().ok().filter(|d| (1..=4).contains(d))?, sched })
    }

    fn name(&self) -> String {
        let h = match self.handoff {
            Handoff::Done => "done",
            Handoff::Queued => "queued",
        };
        let s = match self.sched {
            Sched::Threads => "thr",
            Sched::Blocking => "blk",
            Sched::Inline => "inl",
        };
        format!("{h:<6} d{} {s}", self.depth)
    }
}

const DEFAULT_VARIANTS: &str = "done1t,queued1t,queued2t,queued3t,queued1b,queued2b,queued3b,done1i,queued1i,queued2i,queued3i";

struct Args {
    file: PathBuf,
    frames: u64,
    codec: &'static str,
    vpp: Option<(u32, u32)>,
    kbps: u32,
    qp: u8,
    /// The encoder's quality level, 1 best to 7 fastest.
    tu: u32,
    variants: Vec<Variant>,
    repeat: usize,
    warm: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: chain FILE [--frames N] [--codec h264|h265] [--vpp WxH] [--kbps K] [--qp Q] \
         [--tu 1..7] [--variants {DEFAULT_VARIANTS}] [--repeat R] [--no-warm]"
    );
    std::process::exit(2);
}

fn args() -> Args {
    let mut a = Args {
        file: PathBuf::new(),
        frames: 600,
        codec: "h264/annexb",
        vpp: None,
        kbps: 0,
        qp: 26,
        tu: 4,
        variants: Vec::new(),
        repeat: 1,
        warm: true,
    };
    let mut variants = DEFAULT_VARIANTS.to_string();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--frames" => a.frames = value().parse().unwrap_or_else(|_| usage()),
            "--codec" => {
                a.codec = match value().as_str() {
                    "h264" => "h264/annexb",
                    "h265" => "h265/annexb",
                    _ => usage(),
                }
            }
            "--vpp" => {
                let v = value();
                let (w, h) = v.split_once('x').unwrap_or_else(|| usage());
                a.vpp = Some((
                    w.parse().unwrap_or_else(|_| usage()),
                    h.parse().unwrap_or_else(|_| usage()),
                ));
            }
            "--kbps" => a.kbps = value().parse().unwrap_or_else(|_| usage()),
            "--qp" => a.qp = value().parse().unwrap_or_else(|_| usage()),
            "--tu" => a.tu = value().parse().unwrap_or_else(|_| usage()),
            "--variants" => variants = value(),
            "--repeat" => a.repeat = value().parse().unwrap_or_else(|_| usage()),
            "--no-warm" => a.warm = false,
            // cargo bench passes these through
            "--bench" | "--nocapture" => {}
            s if s.starts_with("--") => usage(),
            s => a.file = PathBuf::from(s),
        }
    }
    if a.file.as_os_str().is_empty() {
        usage();
    }
    a.variants =
        variants.split(',').map(|v| Variant::parse(v.trim()).unwrap_or_else(|| usage())).collect();
    a
}

/// FNV-1a over every coded byte in order.
fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// What the sink saw, shared with the bench.
#[derive(Default)]
struct Seen {
    frames: u64,
    bytes: u64,
    hash: u64,
    first: Option<Instant>,
    last: Option<Instant>,
}

static STREAMS: [OfferDesc; 2] = [OfferDesc::any("h264/annexb"), OfferDesc::any("h265/annexb")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &STREAMS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "count",
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

/// Counts and hashes the coded frames, stops the pipeline at `limit`.
struct Count {
    seen: Arc<Mutex<Seen>>,
    limit: u64,
    control: Arc<Control>,
    stopped: bool,
}

impl Element for Count {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(b) = inputs.pop() {
            // the stop takes a moment, frames past the limit count for nothing
            if seen.frames >= self.limit {
                continue;
            }
            let now = Instant::now();
            seen.first.get_or_insert(now);
            seen.last = Some(now);
            seen.frames += 1;
            seen.bytes += b.memory.data().len() as u64;
            seen.hash = fnv(seen.hash, b.memory.data());
            if seen.frames >= self.limit && !self.stopped {
                self.stopped = true;
                self.control.stop();
            }
        }
        Ok(Flow::Ok)
    }
}

/// The GPU's actual clock, sampled while a run goes.
struct Clock {
    act: PathBuf,
    max: u32,
}

impl Clock {
    fn find() -> Option<Clock> {
        for n in 0..8 {
            let card = PathBuf::from(format!("/sys/class/drm/card{n}"));
            for (act, max) in [
                ("gt_act_freq_mhz", "gt_max_freq_mhz"),
                ("gt/gt0/rps_act_freq_mhz", "gt/gt0/rps_max_freq_mhz"),
            ] {
                let act = card.join(act);
                if let Ok(m) = std::fs::read_to_string(card.join(max)) {
                    let max = m.trim().parse().unwrap_or(0);
                    if std::fs::read_to_string(&act).is_ok() {
                        return Some(Clock { act, max });
                    }
                }
            }
        }
        None
    }
}

/// GPU engine time of one DRM client, ns, as i915 reports it in the file's fdinfo.
#[derive(Clone, Copy, Default)]
struct EngineNs {
    video: u64,
    enhance: u64,
    render: u64,
    copy: u64,
}

/// The engine time of the process's DRM clients. Each stage opens its own display, so each is a
/// client, and a client's counters go with its file when the stage stops, so the sampler keeps
/// the last reading of each client id and sums them at the end.
#[derive(Default)]
struct Engines {
    /// By client id, in the order first seen.
    clients: Vec<(u64, EngineNs)>,
}

impl Engines {
    /// Reads every open render node's fdinfo.
    fn sample(&mut self) {
        let Ok(fds) = std::fs::read_dir("/proc/self/fd") else {
            return;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if !target.starts_with("/dev/dri/") {
                continue;
            }
            let info = PathBuf::from("/proc/self/fdinfo").join(fd.file_name());
            let Ok(text) = std::fs::read_to_string(info) else {
                continue;
            };
            let (mut id, mut ns) = (None, EngineNs::default());
            for line in text.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.trim();
                let n = || {
                    value.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                };
                match key {
                    "drm-client-id" => id = Some(n()),
                    "drm-engine-video" => ns.video = n(),
                    "drm-engine-video-enhance" => ns.enhance = n(),
                    "drm-engine-render" => ns.render = n(),
                    "drm-engine-copy" => ns.copy = n(),
                    _ => {}
                }
            }
            let Some(id) = id else {
                continue;
            };
            match self.clients.iter_mut().find(|(c, _)| *c == id) {
                Some((_, last)) => *last = ns,
                None => self.clients.push((id, ns)),
            }
        }
    }

    /// Over every client seen.
    fn total(&self) -> EngineNs {
        self.clients.iter().fold(EngineNs::default(), |t, (_, e)| EngineNs {
            video: t.video + e.video,
            enhance: t.enhance + e.enhance,
            render: t.render + e.render,
            copy: t.copy + e.copy,
        })
    }
}

struct Sampled {
    mhz: f64,
    top: f64,
    engines: Engines,
}

/// Mean MHz over the samples and the share of them at the top clock, with the engine time.
fn sample_clock(clock: Option<&Clock>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Sampled> {
    let (act, max) = match clock {
        Some(c) => (Some(c.act.clone()), c.max),
        None => (None, 0),
    };
    std::thread::spawn(move || {
        let mut samples: Vec<u32> = Vec::new();
        let mut engines = Engines::default();
        while !stop.load(Ordering::Relaxed) {
            if let Some(act) = &act
                && let Some(mhz) =
                    std::fs::read_to_string(act).ok().and_then(|s| s.trim().parse().ok())
            {
                samples.push(mhz);
            }
            engines.sample();
            std::thread::sleep(Duration::from_millis(2));
        }
        engines.sample();
        if samples.is_empty() {
            return Sampled { mhz: 0.0, top: 0.0, engines };
        }
        let mean = samples.iter().map(|&m| f64::from(m)).sum::<f64>() / samples.len() as f64;
        let top =
            samples.iter().filter(|&&m| max > 0 && m >= max).count() as f64 / samples.len() as f64;
        Sampled { mhz: mean, top, engines }
    })
}

/// The process's CPU time so far, user plus system, from `/proc/self/stat` at the usual 100 ticks
/// a second.
fn cpu_time() -> Duration {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return Duration::ZERO;
    };
    let Some(after) = stat.rfind(')') else {
        return Duration::ZERO;
    };
    let fields: Vec<&str> = stat[after + 1..].split_whitespace().collect();
    let tick = |i: usize| fields.get(i).and_then(|f| f.parse::<u64>().ok()).unwrap_or(0);
    Duration::from_millis((tick(11) + tick(12)) * 10)
}

struct Opened2 {
    opened: Opened,
    track: TrackDesc,
}

fn open(path: &Path, family: &str) -> Opened2 {
    let f = File::open(path).unwrap_or_else(|e| {
        eprintln!("{}: {e}", path.display());
        std::process::exit(1);
    });
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let opened = drive_file(&mut OpenAt::new(len, Want::TRACKS, Caps::default()), &f)
        .unwrap_or_else(|e| {
            eprintln!("{}: {e}", path.display());
            std::process::exit(1);
        });
    let Some(track) = opened.tracks.iter().find(|t| t.kind == TrackKind::Video).copied() else {
        eprintln!("{}: no video track", path.display());
        std::process::exit(1);
    };
    if !matches!(track.family, "h264/annexb" | "h265/annexb") {
        eprintln!("{}: the video is {}, want H.264 or HEVC", path.display(), track.family);
        std::process::exit(1);
    }
    let _ = family;
    Opened2 { opened, track }
}

struct Run {
    seen: Seen,
    dec: Snapshot,
    vpp: Option<Snapshot>,
    enc: Snapshot,
    wall: Duration,
    cpu: Duration,
    mhz: f64,
    top: f64,
    engines: Engines,
    warnings: Vec<String>,
}

fn run(a: &Args, o: &Opened2, v: Variant, frames: u64, clock: Option<&Clock>) -> Run {
    let file = File::open(&a.file).expect("the file opens");
    let mut p = pf_reactor::pipeline();
    p.set_pool(256 * 1024, 16);
    let demux = demuxer(&mut p, file, &a.file, &o.opened, false).expect("a demuxer");
    let (ca, cb) =
        (GpuFrameChannel::with_handoff(v.handoff), GpuFrameChannel::with_handoff(v.handoff));
    // the encoder takes from the decoder's channel unless vaapipostproc sits between
    let enc_in = if a.vpp.is_some() { Arc::clone(&cb) } else { Arc::clone(&ca) };
    let (dec, dec_stats): (Box<dyn Element>, Arc<Stats>) = match o.track.family {
        "h264/annexb" => {
            let d = VaapiH264Dec::new_zerocopy(Arc::clone(&ca));
            let s = d.stats();
            (Box::new(d), s)
        }
        _ => {
            let d = VaapiH265Dec::new_zerocopy(Arc::clone(&ca));
            let s = d.stats();
            (Box::new(d), s)
        }
    };
    let vpp = a.vpp.map(|(w, h)| {
        let target = Target { max_width: w, max_height: h, depth: 8 };
        let v = VaapiPostProc::new_zerocopy(target, Arc::clone(&ca))
            .with_fixed_size()
            .with_gpu_output(Arc::clone(&cb));
        let s = v.stats();
        (v, s)
    });
    let fps = if o.track.frame_ns > 0 {
        (1_000_000_000, o.track.frame_ns.min(u64::from(u32::MAX)) as u32)
    } else {
        (30, 1)
    };
    let (enc, enc_stats): (Box<dyn Element>, Arc<Stats>) = match a.codec {
        "h265/annexb" => {
            let e = VaapiH265Enc::new_zerocopy(enc_in)
                .with_fps(fps.0, fps.1)
                .with_gop(120)
                .with_qp(a.qp)
                .with_bitrate(a.kbps)
                .with_low_power(true)
                .with_quality_level(a.tu)
                .with_depth(v.depth)
                .with_blocking_waits(v.sched == Sched::Blocking);
            let s = e.stats();
            (Box::new(e), s)
        }
        _ => {
            let e = VaapiH264Enc::new_zerocopy(enc_in)
                .with_fps(fps.0, fps.1)
                .with_gop(120)
                .with_qp(a.qp)
                .with_bitrate(a.kbps)
                .with_low_power(true)
                .with_quality_level(a.tu)
                .with_depth(v.depth)
                .with_blocking_waits(v.sched == Sched::Blocking);
            let s = e.stats();
            (Box::new(e), s)
        }
    };
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control = p.control();
    let sink = Count { seen: Arc::clone(&seen), limit: frames, control, stopped: false };

    let dec = p.add_boxed(dec);
    let vpp = vpp.map(|(v, s)| (p.add(v), s));
    let enc = p.add_boxed(enc);
    let sink = p.add(sink);
    p.set_element_pool(dec, 512 << 10, 32);
    if let Some((vpp, _)) = &vpp {
        p.set_element_pool(*vpp, 256 << 10, 16);
    }
    p.set_element_pool(enc, 4 << 20, 8);
    if v.sched == Sched::Inline {
        if let Some((vpp, _)) = &vpp {
            p.set_element_sched(*vpp, SchedHint::Passive);
        }
        p.set_element_sched(enc, SchedHint::Passive);
    }
    p.preroll().expect("preroll");
    let out = route(&p, demux, &o.opened, &o.track).expect("the video track's pad");
    let pad = out.pad(&mut p, demux).expect("the pad");
    p.link(demux.src(pad), dec.sink(SinkPad(0))).expect("demux to decoder");
    match &vpp {
        Some((vpp, _)) => {
            p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
            p.link(vpp.src(VaapiPostProc::OUT), enc.sink(SinkPad(0))).expect("vpp to encoder");
        }
        None => {
            p.link(dec.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("decoder to encoder");
        }
    }
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = sample_clock(clock, Arc::clone(&stop));
    let (t0, cpu0) = (Instant::now(), cpu_time());
    seen.lock().unwrap_or_else(|e| e.into_inner()).first = None;
    let started = Instant::now();
    p.run().expect("the run ends");
    let wall = started.elapsed();
    let cpu = cpu_time().saturating_sub(cpu0);
    let _ = t0;
    stop.store(true, Ordering::Relaxed);
    let Sampled { mhz, top, engines } =
        sampler.join().unwrap_or(Sampled { mhz: 0.0, top: 0.0, engines: Engines::default() });
    let mut warnings = Vec::new();
    while let Some(m) = p.bus().try_recv() {
        if let BusMessage::Warning { error, .. } = m {
            let w = format!("{error}");
            if !w.starts_with("format announcement 'video/gpu'") {
                warnings.push(w);
            }
        }
    }
    let vpp_stats = vpp.as_ref().map(|(_, s)| s.snapshot());
    drop(p);
    let seen = std::mem::take(&mut *seen.lock().unwrap_or_else(|e| e.into_inner()));
    Run {
        seen,
        dec: dec_stats.snapshot(),
        vpp: vpp_stats,
        enc: enc_stats.snapshot(),
        wall,
        cpu,
        mhz,
        top,
        engines,
        warnings,
    }
}

fn ms(d: u64) -> f64 {
    d as f64 / 1e6
}

fn main() {
    let a = args();
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip: no VA-API device");
        return;
    };
    let o = open(&a.file, a.codec);
    if !caps.supports(o.track.family) || !caps.supports_encode(a.codec) {
        eprintln!(
            "skip: the device decodes {} {} and encodes {} {}",
            o.track.family,
            caps.supports(o.track.family),
            a.codec,
            caps.supports_encode(a.codec)
        );
        return;
    }
    let clock = Clock::find();
    println!(
        "{} {}x{} {} to {}{}, {} frames, {}{}",
        a.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        o.track.width,
        o.track.height,
        o.track.family,
        a.codec,
        a.vpp.map(|(w, h)| format!(" through vaapipostproc at {w}x{h}")).unwrap_or_default(),
        a.frames,
        if a.kbps > 0 { format!("{} kbit/s", a.kbps) } else { format!("qp {}", a.qp) },
        clock
            .as_ref()
            .map(|c| format!(", clock {} (max {} MHz)", c.act.display(), c.max))
            .unwrap_or_default(),
    );
    println!(
        "quality level {}, engine ms a frame from fdinfo (vcs video, vecs enhance, rcs render), \
         vcs_split by client in start order (decoder, vaapipostproc, encoder)",
        a.tu
    );
    if a.warm {
        let _ = run(&a, &o, a.variants[0], a.frames.min(120), clock.as_ref());
    }
    println!(
        "{:<14} {:>6} {:>7} {:>8} {:>5} {:>7} {:>5} {:>6} {:>6} {:>6} {:>16} {:>9} {:>9} {:>9} {:>6} {:>8} {:>16}",
        "variant",
        "frames",
        "fps",
        "ms/frame",
        "cpu%",
        "gpu_mhz",
        "top%",
        "vcs",
        "vecs",
        "rcs",
        "vcs_split",
        "dec_wt_ms",
        "vpp_wt_ms",
        "enc_wt_ms",
        "polls",
        "first_ms",
        "kbit/f hash"
    );
    let mut first_hash = None;
    for _ in 0..a.repeat {
        for &v in &a.variants {
            let r = run(&a, &o, v, a.frames, clock.as_ref());
            let s = &r.seen;
            let (fps, per) = match (s.first, s.last) {
                (Some(f), Some(l)) if s.frames > 1 && l > f => {
                    let secs = (l - f).as_secs_f64();
                    ((s.frames - 1) as f64 / secs, secs * 1e3 / (s.frames - 1) as f64)
                }
                _ => (0.0, 0.0),
            };
            let first_ms =
                s.first.map(|f| (f - (Instant::now() - r.wall)).as_secs_f64() * 1e3).unwrap_or(0.0);
            let cpu = if r.wall.is_zero() {
                0.0
            } else {
                r.cpu.as_secs_f64() / r.wall.as_secs_f64() * 100.0
            };
            let same = match first_hash {
                None => {
                    first_hash = Some(s.hash);
                    ' '
                }
                Some(h) if h == s.hash => '=',
                Some(_) => '!',
            };
            let n = s.frames.max(1);
            let per_frame = |ns: u64| ms(ns / n);
            let e = r.engines.total();
            let split: Vec<String> = r
                .engines
                .clients
                .iter()
                .map(|(_, c)| format!("{:.2}", per_frame(c.video)))
                .collect();
            // the coded size, the quality side of a setting's cost at one qp
            let kbit = s.bytes as f64 * 8.0 / 1000.0 / n as f64;
            println!(
                "{:<14} {:>6} {:>7.1} {:>8.2} {:>5.0} {:>7.0} {:>5.0} {:>6.2} {:>6.2} {:>6.2} {:>16} {:>9.1} {:>9.1} {:>9.1} {:>6} {:>8.0} {:>6.0} {:016x}{}",
                v.name(),
                s.frames,
                fps,
                per,
                cpu,
                r.mhz,
                r.top * 100.0,
                per_frame(e.video),
                per_frame(e.enhance),
                per_frame(e.render),
                split.join("+"),
                ms(r.dec.wait_ns),
                r.vpp.map(|v| ms(v.wait_ns)).unwrap_or(0.0),
                ms(r.enc.wait_ns),
                r.enc.polls,
                first_ms,
                kbit,
                s.hash,
                same,
            );
            for w in &r.warnings {
                println!("    warning: {w}");
            }
        }
    }
}
