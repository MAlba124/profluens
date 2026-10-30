//! `oggdemux` pulling by range.

// A test's own fixtures, files and clock, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pf_ogg::{
    GRANULE_NONE, GranuleClock, Mapping, OGG_TAIL_FIRST, OGG_TAIL_LEN, OggDemux, PageHeader,
    demux_all, granule_clock,
};
use pf_sim::driver::reactor::{Faults, World};
use pf_sim::time::MS;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::Control;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::{Index, Pipeline};
use profluens_core::pull::Ask;
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];

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

static REC: ElementDesc = desc("rec", &SINK_PADS, &[]);
static TAIL_LAST: ElementDesc = desc("taillast", &[], &SRC_PADS);

#[derive(Clone, PartialEq, Eq)]
enum Ev {
    Buf { pts: Option<u64>, flags: u32, bytes: Vec<u8> },
    Flush,
}

impl std::fmt::Debug for Ev {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ev::Buf { pts, flags, bytes } => {
                let sum =
                    bytes.iter().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(*b as u32));
                write!(f, "Buf(pts {pts:?} flags {flags:#x} len {} sum {sum:08x})", bytes.len())
            }
            Ev::Flush => f.write_str("Flush"),
        }
    }
}

struct Rec {
    evs: Arc<Mutex<Vec<Ev>>>,
    seen: Arc<AtomicUsize>,
}

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut evs = self.evs.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            evs.push(Ev::Buf {
                pts: buf.pts.nanos(),
                flags: buf.flags.0,
                bytes: buf.memory.data().to_vec(),
            });
            self.seen.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.evs.lock().unwrap().push(Ev::Flush);
        }
        Ok(())
    }
}

struct TailLast {
    data: Arc<Vec<u8>>,
    tails: [(u64, u32); 2],
    held: Vec<(Ask, u32)>,
    tail_held: Option<Ask>,
    log: Arc<Mutex<TailLog>>,
}

#[derive(Default)]
struct TailLog {
    taken: Vec<bool>,
    lens: Vec<u32>,
    windows_before: usize,
    tail_served: bool,
}

impl TailLast {
    fn serve(ctx: &mut Ctx, data: &[u8], mut ask: Ask) {
        let from = (ask.offset as usize).min(data.len());
        let n = (ask.len as usize).min(data.len() - from);
        ask.buf.memory.as_mut_full()[..n].copy_from_slice(&data[from..from + n]);
        ask.buf.memory.set_len(n);
        ctx.serve(SrcPad(0), ask.tag, ask.buf);
    }
}

impl Element for TailLast {
    fn desc(&self) -> &'static ElementDesc {
        &TAIL_LAST
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.offer_ranges(SrcPad(0));
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        assert!(ctx.pulled(SrcPad(0)), "the demuxer pulls");
        let mut log = self.log.lock().unwrap();
        while let Some(ask) = ctx.next_ask(SrcPad(0)) {
            let tail = self.tails.contains(&(ask.offset, ask.len));
            log.taken.push(tail);
            log.lens.push(ask.len);
            if tail {
                self.tail_held = Some(ask);
            } else {
                let wait = (ask.offset / 4096 % 3) as u32;
                self.held.push((ask, wait));
            }
        }
        let mut i = 0;
        while i < self.held.len() {
            if self.held[i].1 > 0 {
                self.held[i].1 -= 1;
                i += 1;
                continue;
            }
            let (ask, _) = self.held.remove(i);
            Self::serve(ctx, &self.data, ask);
            log.windows_before += usize::from(!log.tail_served);
        }
        if self.held.is_empty() && ctx.asks_over(SrcPad(0)) {
            if let Some(ask) = self.tail_held.take() {
                Self::serve(ctx, &self.data, ask);
                log.tail_served = true;
                ctx.wake_within(Duration::ZERO);
                return Ok(Flow::Ok);
            }
            return Ok(Flow::Eos);
        }
        ctx.wake_within(Duration::ZERO);
        Ok(Flow::Ok)
    }
}

fn ffmpeg(name: &str, filter: &str, codec: &[&str]) -> Option<PathBuf> {
    let mut args = vec!["-f", "lavfi", "-i", filter];
    args.extend_from_slice(codec);
    ffmpeg_args(name, &args)
}

fn ffmpeg_args(name: &str, args: &[&str]) -> Option<PathBuf> {
    let mut hash = DefaultHasher::new();
    args.hash(&mut hash);
    let path = std::env::temp_dir().join(format!("pf_ogg_pull_{:016x}_{name}", hash.finish()));
    if path.exists() {
        return Some(path);
    }
    let part = std::env::temp_dir().join(format!("pf_ogg_pull_{}_{name}", std::process::id()));
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(args)
        .args(["-f", "ogg"])
        .arg(&part)
        .status()
        .is_ok_and(|s| s.success())
        && std::fs::rename(&part, &path).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&part);
        eprintln!("pull: ffmpeg could not make {name}, skipped");
    }
    ok.then_some(path)
}

fn fixtures() -> &'static [PathBuf] {
    static ALL: OnceLock<Vec<PathBuf>> = OnceLock::new();
    ALL.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut all = vec![root.join("ogg/tests/fixtures/celt-fb-stereo-128kbps.opus")];
        let noise = "anoisesrc=d=12:a=0.4:r=48000:seed=1";
        all.extend(ffmpeg("noise.opus", noise, &["-ac", "2", "-c:a", "libopus", "-b:a", "160k"]));
        all.extend(ffmpeg(
            "noise.oga",
            "anoisesrc=d=4:a=0.3:r=44100:seed=2",
            &["-ac", "2", "-c:a", "flac"],
        ));
        all.extend(ffmpeg("noise.ogg", noise, &["-ac", "2", "-c:a", "libvorbis", "-q:a", "4"]));
        all.retain(|p| p.exists());
        all
    })
}

fn grouped() -> &'static [PathBuf] {
    static ALL: OnceLock<Vec<PathBuf>> = OnceLock::new();
    ALL.get_or_init(|| {
        let (a, b) = ("anoisesrc=d=6:a=0.4:r=48000:seed=3", "sine=f=660:d=6:r=48000");
        let two = [
            "-f",
            "lavfi",
            "-i",
            a,
            "-f",
            "lavfi",
            "-i",
            b,
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-c:a",
            "libopus",
            "-metadata:s:a:0",
            "language=eng",
            "-metadata:s:a:1",
            "language=fra",
        ];
        let mixed = [
            "-f", "lavfi", "-i", a, "-f", "lavfi", "-i", b, "-map", "0:a", "-map", "1:a", "-c:a:0",
            "libopus", "-c:a:1", "flac",
        ];
        let mut all = Vec::new();
        all.extend(ffmpeg_args("two_opus.ogg", &two));
        all.extend(ffmpeg_args("opus_flac.ogg", &mixed));
        all
    })
}

fn is_opus(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "opus")
}

#[derive(Clone, Copy)]
enum Pools {
    Player,
    Tight,
    NoRoomForTail,
}

#[derive(Clone, Copy)]
enum Demux {
    Extent,
    Len(bool),
}

#[derive(Clone, Copy)]
struct Seek {
    after: usize,
    to_byte: u64,
    at: Timestamp,
}

#[derive(Clone, Copy)]
struct Opts {
    pull: bool,
    streams: bool,
    demux: Demux,
    pools: Pools,
    stepped: Option<u64>,
    seek: Option<Seek>,
}

const PUSH: Opts = Opts {
    pull: false,
    streams: false,
    demux: Demux::Extent,
    pools: Pools::Tight,
    stepped: None,
    seek: None,
};
const PULL: Opts = Opts {
    pull: true,
    streams: false,
    demux: Demux::Len(true),
    pools: Pools::Tight,
    stepped: None,
    seek: None,
};

struct Out {
    evs: Vec<Ev>,
    pads: Vec<(String, Vec<Ev>)>,
    streams: Vec<(&'static str, String)>,
    index: Option<Arc<Index>>,
}

fn tail_of(data: &[u8]) -> Vec<u8> {
    data[data.len() - OGG_TAIL_LEN.min(data.len())..].to_vec()
}

fn demuxer(data: &[u8], demux: Demux) -> OggDemux {
    let len = data.len() as u64;
    match demux {
        Demux::Extent => OggDemux::with_extent(Some(len), tail_of(data)),
        Demux::Len(known) => OggDemux::with_len(known.then_some(len)),
    }
}

fn step(
    mut run: Stepped,
    clock: &MockClock,
    mut seek: Option<(Seek, Arc<Control>)>,
    seen: &AtomicUsize,
    seed: u64,
) {
    let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    for _ in 0..4_000_000u32 {
        if seek.as_ref().is_some_and(|(s, _)| seen.load(Ordering::Relaxed) >= s.after) {
            let (s, control) = seek.take().unwrap();
            let _ = control.seek_bytes(s.at, &[(profluens_core::command::ALL, s.to_byte, s.at)]);
        }
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        if ready.is_empty() {
            match run.next_deadline() {
                Some(at) if at > clock.now() => clock.advance(Timestamp(at.0 - clock.now().0)),
                Some(_) => {}
                None => break,
            }
            continue;
        }
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let g = ready[(rng % ready.len() as u64) as usize];
        let _ = run.pass(g).expect("pass");
    }
    let stalled = (0..run.groups()).any(|g| !matches!(run.outcome(g), Some(Outcome::Exit)));
    assert!(!stalled, "the run stalled, seed {seed}");
    run.finish().expect("finish");
}

fn demux(path: &Path, opts: Opts) -> Out {
    let name = path.to_str().expect("a path in UTF-8");
    let data = std::fs::read(path).expect("fixture");
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = match opts.stepped {
        Some(_) => Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared)),
        None => pf_reactor::pipeline(),
    };
    if let Some(seed) = opts.stepped {
        let mut world = World::new(Faults { latency: (0, 3 * MS), split: true, ..Faults::NONE });
        world.file(name, data.clone());
        p.set_reactor_factory(World::factory(
            Arc::new(Mutex::new(world)),
            Arc::clone(&shared),
            seed,
        ));
    }
    let src = FileSrc::new(path);
    let src = p.add(if opts.pull { src } else { src.push_only() });
    let made = demuxer(&data, opts.demux);
    let made = match opts.streams {
        true => made.with_head(data[..data.len().min(64 << 10)].to_vec()),
        false => made,
    };
    let demux = p.add(made);
    match opts.pools {
        Pools::Player => {
            p.set_element_pool(src, 512 * 1024, 32);
            p.set_element_pool_bytes(demux, 256 * 1024, 24, 32 << 20);
        }
        Pools::Tight => {
            p.set_element_pool(src, 4096, 6);
            p.set_element_pool_bytes(demux, 4096, 6, 128 << 10);
        }
        Pools::NoRoomForTail => {
            p.set_element_pool(src, 4096, 6);
            p.set_element_pool_bytes(demux, 4096, 8, 12 << 10);
        }
    }
    let seen = Arc::new(AtomicUsize::new(0));
    p.link(src.src(FileSrc::OUT), demux.sink(OggDemux::IN)).expect("filesrc ! oggdemux");
    let pads: Vec<(String, (_, SrcPad))> = match opts.streams {
        true => p
            .preroll()
            .expect("preroll")
            .srcs
            .into_iter()
            .map(|a| (a.name, (a.element, a.pad)))
            .collect(),
        false => vec![("src".to_string(), demux.src(OggDemux::OUT))],
    };
    let streams = p.streams(demux).iter().map(|s| (s.codec, s.language.to_string())).collect();
    let mut recs = Vec::new();
    for (name, pad) in pads {
        let evs = Arc::new(Mutex::new(Vec::new()));
        let rec = p.add(Rec { evs: Arc::clone(&evs), seen: Arc::clone(&seen) });
        p.link(pad, rec.sink(SinkPad(0))).expect("oggdemux ! rec");
        recs.push((name, evs));
    }
    let taps = p.tap_handle();
    match opts.stepped {
        None => p.run().expect("run"),
        Some(seed) => {
            let seek = opts.seek.map(|s| (s, p.control()));
            let run = p.step(Arc::clone(&shared)).expect("step");
            step(run, &clock, seek, &seen, seed);
        }
    }
    let read_own = taps.pool_of(src).expect("the source's pool").acquires > 0;
    assert_eq!(read_own, !opts.pull, "{name} pulled {}", opts.pull);
    let pads: Vec<(String, Vec<Ev>)> =
        recs.into_iter().map(|(n, evs)| (n, evs.lock().unwrap().clone())).collect();
    let evs = pads.first().map(|(_, evs)| evs.clone()).unwrap_or_default();
    Out { evs, pads, streams, index: taps.index(demux) }
}

fn behind_flush(evs: &[Ev]) -> &[Ev] {
    let from = evs.iter().rposition(|e| *e == Ev::Flush).map_or(0, |at| at + 1);
    &evs[from..]
}

fn assert_same(push: &[Ev], pull: &[Ev], what: &str) {
    assert!(!push.is_empty(), "{what}: nothing put out");
    assert_eq!(
        push.len(),
        pull.len(),
        "{what}: {} packets pushed and {} pulled",
        push.len(),
        pull.len()
    );
    for (i, (a, b)) in push.iter().zip(pull).enumerate() {
        assert_eq!(a, b, "{what}: packet {i}");
    }
}

#[test]
fn a_pulled_demux_asks_the_tail_and_publishes_what_the_handed_tail_gave() {
    for path in fixtures() {
        for pools in [Pools::Player, Pools::Tight] {
            let what = path.display();
            let push = demux(path, Opts { pools, ..PUSH });
            let pull = demux(path, Opts { pools, ..PULL });
            assert_same(&push.evs, &pull.evs, &format!("{what}"));
            let index = push.index.as_deref().expect("published pushed");
            assert_eq!(pull.index.as_deref(), Some(index), "{what}: the index");
            assert!(index.duration.is_some(), "{what}: a duration, {index:?}");
            assert_eq!(index.file_len, Some(std::fs::metadata(path).unwrap().len()));
        }
    }
}

#[test]
fn the_header_packets_are_flagged_and_media_is_not() {
    for path in fixtures().iter().chain(grouped()) {
        let what = path.display();
        let out = demux(path, Opts { streams: true, ..PUSH });
        for (name, evs) in &out.pads {
            let bufs: Vec<(u32, &[u8])> = evs
                .iter()
                .filter_map(|e| {
                    if let Ev::Buf { flags, bytes, .. } = e {
                        Some((*flags, bytes.as_slice()))
                    } else {
                        None
                    }
                })
                .collect();
            let ident = bufs.first().map(|b| b.1).expect("a packet");
            let heads = Mapping::identify(ident).header_packets(ident) as usize;
            assert!(
                heads > 0 && bufs.len() > heads,
                "{what} {name}: {heads} headers of {}",
                bufs.len()
            );
            for (i, (flags, _)) in bufs.iter().enumerate() {
                assert_eq!(
                    flags & BufferFlags::HEADER.0 != 0,
                    i < heads,
                    "{what} {name}: packet {i}"
                );
            }
        }
    }
}

#[test]
fn reads_late_out_of_order_and_split_change_nothing() {
    for path in fixtures() {
        let push = demux(path, PUSH);
        for seed in 0..6 {
            let pull = demux(path, Opts { stepped: Some(seed), ..PULL });
            let what = format!("{} seed {seed}", path.display());
            assert_same(&push.evs, &pull.evs, &what);
            assert_eq!(pull.index, push.index, "{what}: the index");
        }
    }
}

fn last_on_page(data: &[u8], key: u32, granule: u64) -> Option<Vec<u8>> {
    let (mut at, mut page) = (0, None);
    while let Ok(h) = PageHeader::parse(&data[at..]) {
        if h.serial() == key && h.granule_position() == granule {
            page = Some(at);
        }
        at += h.len();
    }
    let from = demux_all(&data[page?..]);
    from.into_iter().rfind(|p| p.serial == key && p.granule == granule).map(|p| p.data)
}

fn landed(data: &[u8], serial: Option<u32>, target: u64) -> Vec<Ev> {
    let packets = demux_all(data);
    let key = packets[0].serial;
    let clock = granule_clock(&packets[0].data).expect("a clock");
    let preroll = if matches!(clock, GranuleClock::Opus { .. }) { 80_000_000 } else { 0 };
    let ns = |g: u64| clock.ns(g).unwrap();
    let page = packets
        .iter()
        .filter(|p| {
            p.serial == key
                && p.granule != GRANULE_NONE
                && ns(p.granule) <= target.saturating_sub(preroll)
        })
        .map(|p| p.granule)
        .max()
        .expect("a page ends by the target");
    let serial = serial.unwrap_or(key);
    let mut evs: Vec<Ev> = packets
        .iter()
        .filter(|p| p.serial == serial && (p.serial != key || p.granule > page))
        .map(|p| Ev::Buf { pts: None, flags: 0, bytes: p.data.clone() })
        .collect();
    if let Some(Ev::Buf { pts, .. }) = evs.first_mut() {
        *pts = Some(ns(page));
    }
    let vorbis = Mapping::identify(&packets[0].data) == Mapping::Vorbis;
    if let Some(lead) =
        last_on_page(data, key, page).filter(|_| vorbis && serial == key && !evs.is_empty())
    {
        evs.insert(0, Ev::Buf { pts: None, flags: 0, bytes: lead });
    }
    evs
}

#[test]
fn a_seek_part_way_through_restarts_a_push_at_its_byte_and_lands_a_pull_on_its_page() {
    for path in fixtures() {
        let data = std::fs::read(path).unwrap();
        let len = data.len() as u64;
        for at in [Timestamp(1_500_000_000), Timestamp(55_000_000), Timestamp(3_210_000_000)] {
            let seek = Some(Seek { after: 8, to_byte: len / 2, at });
            let want = landed(&data, None, at.0);
            for seed in 0..4 {
                let what = format!("{} at {at:?} seed {seed}", path.display());
                let push = demux(path, Opts { stepped: Some(seed), seek, ..PUSH });
                let pull = demux(path, Opts { stepped: Some(seed), seek, ..PULL });
                assert!(push.evs.contains(&Ev::Flush), "{what}: sought before the end");
                let push_behind = behind_flush(&push.evs);
                assert!(
                    matches!(push_behind.first(), Some(Ev::Buf { pts: Some(t), .. }) if *t == at.0),
                    "{what}"
                );
                if pull.evs.contains(&Ev::Flush) {
                    match want.is_empty() {
                        true => assert_eq!(behind_flush(&pull.evs), &[], "{what}"),
                        false => assert_same(&want, behind_flush(&pull.evs), &what),
                    }
                }
                assert_eq!(pull.index, push.index, "{what}: the index");
            }
        }
    }
}

#[test]
fn with_no_length_there_is_no_tail_and_the_stream_still_plays() {
    for path in fixtures() {
        let what = path.display();
        let push = demux(path, PUSH);
        let pull = demux(path, Opts { demux: Demux::Len(false), ..PULL });
        assert_same(&push.evs, &pull.evs, &format!("{what}"));
        let blind = demux(path, Opts { demux: Demux::Len(false), ..PUSH });
        let index = pull.index.as_deref().expect("published");
        assert_eq!(Some(index), blind.index.as_deref(), "{what}");
        assert_eq!(index.file_len, None);
        if is_opus(path) {
            assert!(index.duration.is_none(), "{what}: no tail, no duration, {index:?}");
        }
    }
}

#[test]
fn a_pool_too_small_for_the_tail_plays_without_a_duration() {
    for path in fixtures().iter().filter(|p| is_opus(p)) {
        let what = path.display();
        let push = demux(path, PUSH);
        let pull = demux(path, Opts { pools: Pools::NoRoomForTail, ..PULL });
        assert_same(&push.evs, &pull.evs, &format!("{what}"));
        let index = pull.index.as_deref().expect("published");
        assert!(index.duration.is_none(), "{what}: {index:?}");
        assert_eq!(index.file_len, push.index.as_deref().unwrap().file_len);
    }
}

#[test]
fn the_windows_do_not_wait_on_the_tail() {
    for path in fixtures() {
        let what = path.display();
        let data = Arc::new(std::fs::read(path).unwrap());
        let len = data.len() as u64;
        let log = Arc::new(Mutex::new(TailLog::default()));
        let clock = MockClock::new();
        let shared: Arc<dyn Clock> = Arc::new(clock.clone());
        let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
        p.set_pool(4096, 16);
        let src = p.add(TailLast {
            data: Arc::clone(&data),
            tails: tails(len),
            held: Vec::new(),
            tail_held: None,
            log: Arc::clone(&log),
        });
        let oggdemux = p.add(OggDemux::with_len(Some(len)));
        p.set_element_pool_bytes(oggdemux, 4096, 8, 128 << 10);
        let evs = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::new(AtomicUsize::new(0));
        let rec = p.add(Rec { evs: Arc::clone(&evs), seen: Arc::clone(&seen) });
        p.link(src.src(SrcPad(0)), oggdemux.sink(OggDemux::IN)).expect("src ! oggdemux");
        p.link(oggdemux.src(OggDemux::OUT), rec.sink(SinkPad(0))).expect("oggdemux ! rec");
        let taps = p.tap_handle();
        let run = p.step(Arc::clone(&shared)).expect("step");
        step(run, &clock, None, &seen, 1);
        let log = log.lock().unwrap();
        assert_eq!(log.taken.first(), Some(&true), "{what}: the tail is asked first");
        let asked = log.taken.iter().filter(|t| **t).count();
        assert!(asked == 1 || asked == 2 && !is_opus(path), "{what}: asked {asked} times");
        let windows = log.taken.len() - asked;
        assert!(
            windows > 1 && log.windows_before == windows,
            "{what}: {} of {windows}",
            log.windows_before
        );
        let push = demux(path, PUSH);
        assert_same(&push.evs, &evs.lock().unwrap(), &format!("{what}"));
        assert_eq!(taps.index(oggdemux), push.index, "{what}: the index");
    }
}

fn by_serial(data: &[u8]) -> Vec<(u32, Vec<Vec<u8>>)> {
    let mut out: Vec<(u32, Vec<Vec<u8>>)> = Vec::new();
    for p in demux_all(data) {
        match out.iter_mut().find(|(s, _)| *s == p.serial) {
            Some((_, v)) => v.push(p.data),
            None => out.push((p.serial, vec![p.data])),
        }
    }
    out
}

fn bytes_of(evs: &[Ev]) -> Vec<Vec<u8>> {
    evs.iter()
        .filter_map(|e| if let Ev::Buf { bytes, .. } = e { Some(bytes.clone()) } else { None })
        .collect()
}

#[test]
fn every_grouped_stream_has_its_pad_pushed_and_pulled_alike() {
    for path in grouped() {
        let what = path.display();
        let data = std::fs::read(path).unwrap();
        let want = by_serial(&data);
        assert_eq!(want.len(), 2, "{what}: two streams");
        let single = demux(path, PUSH);
        for pools in [Pools::Player, Pools::Tight] {
            let push = demux(path, Opts { pools, streams: true, ..PUSH });
            let pull = demux(path, Opts { pools, streams: true, ..PULL });
            assert_eq!(push.pads.len(), 2, "{what}: a pad per stream");
            let families: Vec<&str> = push.streams.iter().map(|s| s.0).collect();
            match path.to_str().unwrap().contains("two_opus") {
                true => {
                    assert_eq!(families, ["opus", "opus"], "{what}");
                    let langs: Vec<&str> = push.streams.iter().map(|s| s.1.as_str()).collect();
                    assert_eq!(langs, ["eng", "fra"], "{what}: the LANGUAGE comments");
                }
                false => assert_eq!(families, ["opus", "flac/ogg"], "{what}"),
            }
            for ((name, pushed), (_, pulled)) in push.pads.iter().zip(&pull.pads) {
                assert_same(pushed, pulled, &format!("{what} {name}"));
            }
            for ((name, evs), (serial, packets)) in push.pads.iter().zip(&want) {
                assert_eq!(*name, format!("src_{serial:08x}"), "{what}");
                assert_eq!(&bytes_of(evs), packets, "{what} {name}: the packets of its serial");
            }
            assert_same(&single.evs, &push.evs, &format!("{what} first stream"));
            assert_eq!(push.index, single.index, "{what}: the index");
            assert_eq!(pull.index, single.index, "{what}: the index pulled");
        }
    }
}

#[test]
fn behind_a_seek_every_pad_restarts_with_its_time() {
    for path in grouped() {
        let data = std::fs::read(path).unwrap();
        let len = data.len() as u64;
        let at = Timestamp(2_000_000_000);
        let seek = Some(Seek { after: 8, to_byte: len / 2, at });
        let first = landed(&data, None, at.0);
        let Some(Ev::Buf { pts: Some(page), .. }) = first.first().cloned() else {
            panic!("a landing")
        };
        for seed in 0..4 {
            let what = format!("{} seed {seed}", path.display());
            let push = demux(path, Opts { stepped: Some(seed), seek, streams: true, ..PUSH });
            let pull = demux(path, Opts { stepped: Some(seed), seek, streams: true, ..PULL });
            assert_eq!(pull.pads.len(), 2, "{what}");
            for (name, pushed) in &push.pads {
                assert!(pushed.contains(&Ev::Flush), "{what} {name}: sought before the end");
                let a = behind_flush(pushed);
                assert!(
                    matches!(a.first(), Some(Ev::Buf { pts: Some(t), .. }) if *t == at.0),
                    "{what} {name}"
                );
                let headed = a.iter().any(
                    |e| matches!(e, Ev::Buf { flags, .. } if flags & BufferFlags::HEADER.0 != 0),
                );
                assert!(!headed, "{what} {name}: no header packet behind a seek");
            }
            let (first_pad, other) = (&pull.pads[0].1, &pull.pads[1].1);
            assert_same(&first, behind_flush(first_pad), &format!("{what} first"));
            let b = behind_flush(other);
            assert!(
                matches!(b.first(), Some(Ev::Buf { pts: Some(t), .. }) if *t == page),
                "{what} other: {:?}",
                b.first()
            );
            let all = bytes_of(&landed(&data, Some(by_serial(&data)[1].0), 0));
            let tail = bytes_of(b);
            assert!(all.ends_with(&tail), "{what} other: its packets from the landing on");
            assert_eq!(pull.index, push.index, "{what}: the index");
        }
    }
}

fn tails(len: u64) -> [(u64, u32); 2] {
    [OGG_TAIL_FIRST, OGG_TAIL_LEN]
        .map(|want| (len - (want as u64).min(len), (want as u64).min(len) as u32))
}

#[test]
fn a_first_tail_of_no_page_is_asked_again_whole() {
    for path in fixtures().iter().filter(|p| is_opus(p)) {
        let what = path.display();
        let mut data = std::fs::read(path).unwrap();
        data.resize(data.len() + OGG_TAIL_FIRST + 4096, 0);
        let junk = std::env::temp_dir().join(format!(
            "pf_ogg_pull_{}_junk_{}",
            std::process::id(),
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&junk, &data).unwrap();
        let push = demux(&junk, PUSH);
        let pull = demux(&junk, PULL);
        let _ = std::fs::remove_file(&junk);
        assert_same(&push.evs, &pull.evs, &format!("{what}"));
        let index = push.index.as_deref().expect("published pushed");
        assert!(index.duration.is_some(), "{what}: a duration, {index:?}");
        assert_eq!(pull.index.as_deref(), Some(index), "{what}: the index");
    }
}

#[test]
fn told_the_rate_a_window_holds_a_quarter_second_of_it() {
    for path in fixtures() {
        let what = path.display();
        let data = Arc::new(std::fs::read(path).unwrap());
        let len = data.len() as u64;
        let log = Arc::new(Mutex::new(TailLog::default()));
        let clock = MockClock::new();
        let shared: Arc<dyn Clock> = Arc::new(clock.clone());
        let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
        p.set_pool(4096, 16);
        let src = p.add(TailLast {
            data: Arc::clone(&data),
            tails: tails(len),
            held: Vec::new(),
            tail_held: None,
            log: Arc::clone(&log),
        });
        let oggdemux = p.add(OggDemux::with_len(Some(len)));
        p.set_element_pool_bytes(oggdemux, 256 * 1024, 24, 32 << 20);
        let seen = Arc::new(AtomicUsize::new(0));
        let rec = p.add(Rec { evs: Arc::new(Mutex::new(Vec::new())), seen: Arc::clone(&seen) });
        p.link(src.src(SrcPad(0)), oggdemux.sink(OggDemux::IN)).expect("src ! oggdemux");
        p.link(oggdemux.src(OggDemux::OUT), rec.sink(SinkPad(0))).expect("oggdemux ! rec");
        let run = p.step(Arc::clone(&shared)).expect("step");
        step(run, &clock, None, &seen, 1);
        let duration =
            demux(path, PUSH).index.and_then(|i| i.duration.nanos()).expect("a duration");
        if duration < 4_000_000_000 {
            continue;
        }
        let paced = (u128::from(len) * 250_000_000 / u128::from(duration)) as u64;
        let paced = paced.clamp(4096, 256 * 1024);
        let log = log.lock().unwrap();
        let windows: Vec<u64> = log
            .taken
            .iter()
            .zip(&log.lens)
            .filter(|(t, _)| !**t)
            .map(|(_, l)| u64::from(*l))
            .collect();
        assert_eq!(windows.first(), Some(&(16u64 << 10).min(len)), "{what}: {windows:?}");
        let steady = windows[windows.len().saturating_sub(4)..windows.len() - 1].to_vec();
        assert!(
            !steady.is_empty() && steady.iter().all(|w| *w >= paced / 2 && *w <= paced * 2),
            "{what}: {steady:?} for {paced}"
        );
    }
}
