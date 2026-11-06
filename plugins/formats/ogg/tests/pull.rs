//! `oggdemux` pulling by range.

// A test's own fixtures, files and clock, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pf_ogg::seek::{Bisect, Landing, Step};
use pf_ogg::{
    GRANULE_NONE, GranuleClock, Mapping, OGG_TAIL_FIRST, OGG_TAIL_LEN, OggDemux, Packet,
    PageHeader, demux_all, flac_frame_samples, granule_clock, opus_packet_samples,
    speex_packet_samples,
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

/// What the key stream puts out behind a seek pulled to `target`, from the page ending by it on.
/// A landing on a header page reads the file from its start, the headers flagged again.
fn landed(data: &[u8], target: u64) -> Vec<Ev> {
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
    if page == 0 {
        return from_head(&packets, key);
    }
    let from: Vec<&Packet> =
        packets.iter().filter(|p| p.serial == key && p.granule > page).collect();
    let stamps = stamps(&from, &packets[0].data, page, clock.stamp(page).unwrap());
    let mut evs: Vec<Ev> = from
        .iter()
        .zip(stamps)
        .map(|(p, pts)| Ev::Buf { pts, flags: 0, bytes: p.data.clone() })
        .collect();
    let vorbis = Mapping::identify(&packets[0].data) == Mapping::Vorbis;
    if let Some(lead) = last_on_page(data, key, page).filter(|_| vorbis && !evs.is_empty()) {
        evs.insert(0, Ev::Buf { pts: None, flags: 0, bytes: lead });
    }
    evs
}

/// The stream of `serial` read from byte 0 behind a seek, its headers flagged and unstamped, its
/// media stamped from where its first page's granule puts it, or the first at 0 where its packet
/// lengths are not read.
fn from_head(packets: &[Packet], serial: u32) -> Vec<Ev> {
    let mine: Vec<&Packet> = packets.iter().filter(|p| p.serial == serial).collect();
    let ident = &mine[0].data;
    let (head, media) = mine.split_at(Mapping::identify(ident).header_packets(ident) as usize);
    let stamps = match first_start(media, ident) {
        Some(start) => {
            stamps(media, ident, start, granule_clock(ident).unwrap().stamp(start).unwrap())
        }
        None => (0..media.len()).map(|i| (i == 0).then_some(0)).collect(),
    };
    let heads = head.iter().map(|p| Ev::Buf {
        pts: None,
        flags: BufferFlags::HEADER.0,
        bytes: p.data.clone(),
    });
    let media =
        media.iter().zip(stamps).map(|(p, pts)| Ev::Buf { pts, flags: 0, bytes: p.data.clone() });
    heads.chain(media).collect()
}

/// The samples of `packet` where the demuxer reads them, Opus, FLAC and Speex.
fn length(ident: &[u8], packet: &[u8]) -> Option<u32> {
    match Mapping::identify(ident) {
        Mapping::Opus => opus_packet_samples(packet),
        Mapping::Flac => flac_frame_samples(packet),
        Mapping::Speex => speex_packet_samples(ident),
        _ => None,
    }
}

/// The granule `packets[0]` starts at, its page's granule less the samples to it, 0 at the least.
fn first_start(packets: &[&Packet], ident: &[u8]) -> Option<u64> {
    let to_end = packets.iter().position(|p| p.last)?;
    let samples: Option<u64> =
        packets[..=to_end].iter().map(|p| length(ident, &p.data).map(u64::from)).sum();
    Some(packets[to_end].granule.saturating_sub(samples?))
}

/// The stamps of a stream's packets from a landing on the page ending at `page` stamped `at`, the
/// first at `at`, each after it later by the samples between, a page's granule ending its packets.
/// Only the first stamped for a mapping whose packet lengths the demuxer does not read.
fn stamps(packets: &[&Packet], ident: &[u8], page: u64, at: u64) -> Vec<Option<u64>> {
    let rate = match (Mapping::identify(ident), granule_clock(ident)) {
        (Mapping::Opus, _) => 48_000u128,
        (Mapping::Flac | Mapping::Speex, Some(GranuleClock::Rate(rate))) => u128::from(rate),
        _ => return (0..packets.len()).map(|i| (i == 0).then_some(at)).collect(),
    };
    let mut start = page;
    let mut out = Vec::with_capacity(packets.len());
    for p in packets {
        out.push(Some(at + (u128::from(start - page) * 1_000_000_000 / rate) as u64));
        start += u64::from(length(ident, &p.data).expect("a packet length"));
        if p.last && p.granule != GRANULE_NONE {
            start = p.granule;
        }
    }
    out
}

#[test]
fn a_seek_part_way_through_restarts_a_push_at_its_byte_and_lands_a_pull_on_its_page() {
    for path in fixtures() {
        let data = std::fs::read(path).unwrap();
        let len = data.len() as u64;
        for at in [Timestamp(1_500_000_000), Timestamp(55_000_000), Timestamp(3_210_000_000)] {
            let seek = Some(Seek { after: 8, to_byte: len / 2, at });
            let want = landed(&data, at.0);
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

#[test]
fn a_known_duration_asks_no_tail() {
    for path in fixtures() {
        let what = path.display();
        let data = Arc::new(std::fs::read(path).unwrap());
        let len = data.len() as u64;
        let push = demux(path, PUSH);
        let known = push.index.as_deref().and_then(|i| i.duration.nanos()).expect("timed");
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
        let oggdemux = p.add(OggDemux::with_len(Some(len)).with_duration(known));
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
        assert!(!log.taken.is_empty() && !log.taken.contains(&true), "{what}: {:?}", log.taken);
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

fn pts_of(evs: &[Ev]) -> Vec<Option<u64>> {
    evs.iter().filter_map(|e| if let Ev::Buf { pts, .. } = e { Some(*pts) } else { None }).collect()
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
        let first = landed(&data, at.0);
        let packets = demux_all(&data);
        let serial = by_serial(&data)[1].0;
        let mine: Vec<&Packet> = packets.iter().filter(|p| p.serial == serial).collect();
        let ident = &mine[0].data;
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
            let tail = bytes_of(b);
            let all: Vec<Vec<u8>> = mine.iter().map(|p| p.data.clone()).collect();
            assert!(all.ends_with(&tail), "{what} other: its packets from the landing on");
            // stamped from its own page granule, not the key stream's landing
            let from = &mine[mine.len() - tail.len()..];
            let start = first_start(from, ident).expect("a page end");
            let at = granule_clock(ident).unwrap().stamp(start).unwrap();
            assert_eq!(pts_of(b), stamps(from, ident, start, at), "{what} other");
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

/// Five seconds of 192 kHz stereo FLAC noise, about 4 Mbit/s, a quarter second of it over
/// the window's floor.
fn dense_flac() -> Option<PathBuf> {
    let noise = "anoisesrc=d=5:a=0.5:r=192000:seed=5";
    ffmpeg("dense.oga", noise, &["-ac", "2", "-c:a", "flac", "-sample_fmt", "s32"])
}

/// Told the rate, a window holds a quarter second of it, at least 64 KiB.
#[test]
fn told_the_rate_a_window_holds_a_quarter_second_of_it() {
    for path in fixtures().iter().cloned().chain(dense_flac()) {
        let path = &path;
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
        let paced = paced.clamp(64 << 10, 256 * 1024);
        let log = log.lock().unwrap();
        let windows: Vec<u64> = log
            .taken
            .iter()
            .zip(&log.lens)
            .filter(|(t, _)| !**t)
            .map(|(_, l)| u64::from(*l))
            .collect();
        assert_eq!(windows.first(), Some(&(16u64 << 10).min(len)), "{what}: {windows:?}");
        // past the first windows, the last short at the file's end
        let told: Vec<u64> = windows.iter().copied().skip_while(|w| *w == 16 << 10).collect();
        let steady = &told[..told.len().saturating_sub(1)];
        assert!(
            !steady.is_empty() && steady.iter().all(|w| *w >= paced / 2 && *w <= paced * 2),
            "{what}: {steady:?} for {paced}"
        );
    }
}

/// From the start nothing upstream stamps, the first media packet takes the time of where its
/// page's granule puts it and every packet after is stamped, pushed or pulled. Headers go out
/// unstamped.
#[test]
fn from_the_start_every_media_packet_is_stamped_from_the_granules() {
    for path in fixtures() {
        let what = path.display();
        let data = std::fs::read(path).unwrap();
        let packets = demux_all(&data);
        let ident = &packets[0].data;
        let clock = granule_clock(ident).expect("a clock");
        let mapping = Mapping::identify(ident);
        if !matches!(mapping, Mapping::Opus | Mapping::Flac) {
            continue;
        }
        let media: Vec<&Packet> =
            packets.iter().skip(mapping.header_packets(ident) as usize).collect();
        let to_end = media.iter().position(|p| p.last).expect("a page end");
        let samples: u64 = media[..=to_end]
            .iter()
            .map(|p| {
                let n = match mapping {
                    Mapping::Opus => opus_packet_samples(&p.data),
                    _ => flac_frame_samples(&p.data),
                };
                u64::from(n.expect("a packet length"))
            })
            .sum();
        let start = media[to_end].granule - samples;
        let want = stamps(&media, ident, start, clock.stamp(start).unwrap());
        for opts in [PUSH, PULL] {
            let out = demux(path, opts);
            let got: Vec<Option<u64>> = out
                .evs
                .iter()
                .filter_map(|e| match e {
                    Ev::Buf { pts, flags, .. } if flags & BufferFlags::HEADER.0 == 0 => Some(*pts),
                    Ev::Buf { pts, .. } => {
                        assert_eq!(*pts, None, "{what}: a header unstamped");
                        None
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(got, want, "{what} pulled {}", opts.pull);
        }
    }
}

/// A file of the generated corpus, `None` and a note where `cargo xtask fixtures` has not run.
fn corpus(name: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/out").join(name);
    if !path.exists() {
        eprintln!("pull: {name} not generated, skipped");
    }
    path.exists().then_some(path)
}

const SPEEX: (&str, u64) = ("ogg_speex.spx", 1_991_062_500);
const THEORA: (&str, u64) = ("ogg_theora_speex.ogv", 2_000_000_000);

/// Speex is timed in samples less its encoder's lookahead, Theora in frames, both from the last
/// page of the first stream.
#[test]
fn speex_and_theora_files_are_timed_from_their_last_granule() {
    for (name, ns) in [SPEEX, THEORA] {
        let Some(path) = corpus(name) else { continue };
        let data = std::fs::read(&path).unwrap();
        let first = demux_all(&data).swap_remove(0);
        let duration = pf_ogg::stream_duration_ns(&first.data, &tail_of(&data), first.serial);
        assert_eq!(duration, Some(ns), "{name}");
        let pulled = demux(&path, PULL).index.and_then(|i| i.duration.nanos());
        assert_eq!(pulled, Some(ns), "{name}: published");
    }
}

/// A pulled seek to the middle bisects by the granules. Speex lands on the page ending by the
/// target, its next packet within one of it and each after 20 ms on. Theora lands ahead of the
/// keyframe the target's frame decodes from, every frame stamped from it.
#[test]
fn a_speex_or_theora_seek_pulled_lands_by_its_granules() {
    const TARGET: u64 = 1_000_000_000;
    const SPEEX_PACKET: u64 = 20_000_000;
    for (name, _) in [SPEEX, THEORA] {
        let Some(path) = corpus(name) else { continue };
        let data = std::fs::read(&path).unwrap();
        let packets = demux_all(&data);
        let (key, ident) = (packets[0].serial, &packets[0].data);
        let clock = granule_clock(ident).expect("a clock");
        let theora = Mapping::identify(ident) == Mapping::Theora;
        let ns = |g: u64| clock.ns(g).expect("a time");
        let ends = |by: u64, below: u64| {
            packets
                .iter()
                .filter(|p| p.serial == key && p.granule < below && ns(p.granule) <= by)
                .map(|p| p.granule)
                .max()
        };
        let page = ends(TARGET, u64::MAX).expect("a page ends by the target");
        let page = match clock.keyframe_ns(page) {
            Some(key) => ends(key, page).expect("a page ends by the keyframe"),
            None => page,
        };
        let from: Vec<&Packet> =
            packets.iter().filter(|p| p.serial == key && p.granule > page).collect();
        let at = clock.stamp(page).expect("a stamp");
        let pts = match theora {
            true => (0..from.len()).map(|i| Some(at + i as u64 * 1_000_000_000 / 15)).collect(),
            false => stamps(&from, ident, page, at),
        };
        let want: Vec<Ev> = from
            .iter()
            .zip(pts)
            .map(|(p, pts)| Ev::Buf { pts, flags: 0, bytes: p.data.clone() })
            .collect();
        match theora {
            // the fixture's keyframes are 12 frames apart at 15 a second
            true => {
                assert_eq!(at, 800_000_000, "{name}");
                assert_eq!(from[0].data[0] & 0xC0, 0, "{name}: an intra frame first");
            }
            false => assert!(at <= TARGET && TARGET - at < SPEEX_PACKET, "{name}: {at}"),
        }
        let len = data.len() as u64;
        for seed in 0..4 {
            let what = format!("{name} seed {seed}");
            let seek = Some(Seek { after: 1, to_byte: len / 2, at: Timestamp(TARGET) });
            let pull = demux(&path, Opts { stepped: Some(seed), seek, ..PULL });
            assert!(pull.evs.contains(&Ev::Flush), "{what}: sought before the end");
            assert_same(&want, behind_flush(&pull.evs), &what);
        }
    }
}

/// Behind a seek every pad of a Theora file with Speex starts on its own time. Before the second
/// group of pictures the file is read again from byte 0, every stream's headers flagged, never a
/// Speex comment put out as audio. Past it the Speex pad is counted back from its own page
/// granule, not stamped with the Theora landing's time.
#[test]
fn behind_a_seek_a_theora_files_speex_pad_is_on_its_own_time() {
    let Some(path) = corpus(THEORA.0) else { return };
    let data = std::fs::read(&path).unwrap();
    let packets = demux_all(&data);
    let serials = by_serial(&data);
    let speex: Vec<&Packet> = packets.iter().filter(|p| p.serial == serials[1].0).collect();
    let ident = &speex[0].data;
    assert_eq!(Mapping::identify(ident), Mapping::Speex);
    let len = data.len() as u64;
    for (target, keyframe) in [(0, 0), (500_000_000, 0), (1_000_000_000, 12)] {
        for seed in 0..3 {
            let what = format!("target {target} seed {seed}");
            let seek = Some(Seek { after: 4, to_byte: len / 2, at: Timestamp(target) });
            let pull = demux(&path, Opts { stepped: Some(seed), seek, streams: true, ..PULL });
            assert!(pull.pads[0].1.contains(&Ev::Flush), "{what}: sought before the end");
            let (video, audio) = (behind_flush(&pull.pads[0].1), behind_flush(&pull.pads[1].1));
            let header =
                |e: &Ev| matches!(e, Ev::Buf { flags, .. } if flags & BufferFlags::HEADER.0 != 0);
            let comment = |e: &Ev| matches!(e, Ev::Buf { bytes, .. } if bytes.len() == 55 && bytes[0] == 0x0d);
            assert!(!audio.iter().any(|e| comment(e) && !header(e)), "{what}: a comment as audio");
            let heads = video.iter().take_while(|e| header(e)).count();
            assert_eq!(heads, if keyframe == 0 { 3 } else { 0 }, "{what}: the Theora headers");
            let pts = pts_of(&video[heads..]);
            let Some(Ev::Buf { bytes, .. }) = video.get(heads) else { panic!("{what}: no frame") };
            assert_eq!(bytes[0] & 0xC0, 0, "{what}: an intra frame first");
            for (i, t) in pts.iter().enumerate() {
                assert_eq!(
                    *t,
                    Some((keyframe + i as u64) * 1_000_000_000 / 15),
                    "{what}: frame {i}"
                );
            }
            if keyframe == 0 {
                assert_same(&from_head(&packets, serials[1].0), audio, &what);
                continue;
            }
            assert!(!audio.iter().any(header), "{what}: no Speex header past the headers");
            let from = &speex[speex.len() - audio.len()..];
            assert_eq!(bytes_of(audio), from.iter().map(|p| p.data.clone()).collect::<Vec<_>>());
            let start = first_start(from, ident).expect("a page end");
            let got = pts_of(audio);
            assert!(
                got[0].is_some_and(|t| t < 20_000_000),
                "{what}: {:?}, not the landing's",
                got[0]
            );
            assert_eq!(got, stamps(from, ident, start, start * 1_000_000_000 / 16_000), "{what}");
            assert_eq!(got[1].zip(got[0]).map(|(b, a)| b - a), Some(20_000_000), "{what}");
        }
    }
}

/// An audio file seeks by its granules, a target before its first page of audio ends reads it
/// again from byte 0 with its headers flagged.
#[test]
fn an_audio_seek_before_the_first_page_ends_reads_the_headers_again() {
    for name in ["ogg_vorbis.ogg", "ogg_flac.oga", SPEEX.0] {
        let Some(path) = corpus(name) else { continue };
        let data = std::fs::read(&path).unwrap();
        let len = data.len() as u64;
        let packets = demux_all(&data);
        let clock = granule_clock(&packets[0].data).expect("a clock");
        let first = packets.iter().map(|p| p.granule).filter(|g| *g > 0 && *g != GRANULE_NONE);
        let first = clock.ns(first.min().expect("a page of audio")).unwrap();
        for target in [0, 500_000_000, 1_000_000_000, 1_500_000_000] {
            let want = landed(&data, target);
            let head = matches!(want.first(), Some(Ev::Buf { flags, .. }) if *flags != 0);
            assert_eq!(head, target < first, "{name} {target}: from byte 0");
            for seed in 0..3 {
                let what = format!("{name} target {target} seed {seed}");
                let seek = Some(Seek { after: 2, to_byte: len / 2, at: Timestamp(target) });
                let pull = demux(&path, Opts { stepped: Some(seed), seek, ..PULL });
                assert!(pull.evs.contains(&Ev::Flush), "{what}: sought before the end");
                assert_same(&want, behind_flush(&pull.evs), &what);
            }
        }
    }
}

/// The key stream's timed pages, their first byte and granule, walked from byte 0.
fn timed_pages(data: &[u8], key: u32) -> Vec<(u64, u64)> {
    let (mut at, mut out) = (0, Vec::new());
    while let Ok(page) = PageHeader::parse(&data[at..]) {
        if page.serial() == key && page.granule_position() != GRANULE_NONE {
            out.push((at as u64, page.granule_position()));
        }
        at += page.len();
    }
    out
}

/// Where a seek to `target` lands read page by page, the rule [`Bisect`] keeps.
fn brute(pages: &[(u64, u64)], clock: GranuleClock, target: u64) -> Option<Landing> {
    let ns = |g: u64| clock.ns(g).expect("a time");
    let mut found = pages.iter().rev().find(|(_, g)| ns(*g) <= target).copied()?;
    if let Some(key) = clock.keyframe_ns(found.1) {
        let before = pages.iter().rev().find(|(at, g)| *at < found.0 && ns(*g) <= key);
        found = before.copied().unwrap_or(found);
    }
    Some(match found {
        (_, 0) => Landing::HEAD,
        (at, granule) => Landing { at, granule, ns: ns(granule) },
    })
}

/// The most probes a seek to each tenth of the file takes, the file's end time known or not,
/// every landing checked against the pages read in order.
fn most_probes(path: &Path, known: bool) -> u32 {
    let data = std::fs::read(path).unwrap();
    let packets = demux_all(&data);
    let (key, clock) = (packets[0].serial, granule_clock(&packets[0].data).expect("a clock"));
    let pages = timed_pages(&data, key);
    let end = clock.ns(pages.last().expect("a page").1).unwrap();
    let mut most = 0;
    for tenth in 0..=10 {
        let target = end / 10 * tenth;
        let mut b = Bisect::new(key, clock, target, data.len() as u64).ending(known.then_some(end));
        let mut probes = 0;
        let got = loop {
            match b.step() {
                Step::Land(l) => break l,
                Step::Probe { at, len } => {
                    probes += 1;
                    b.served(at, &data[at as usize..(at + u64::from(len)) as usize]);
                }
            }
        };
        assert_eq!(got, brute(&pages, clock, target), "{path:?} target {target}");
        most = most.max(probes);
    }
    most
}

/// A seek interpolates on the granules against the bytes. Every file takes no more probes than
/// plain bisection did, counted 2026-11-06, and ten minutes of Opus one or two with its end time
/// known, plain bisection 3 to 13.
#[test]
fn a_seek_lands_in_fewer_probes_than_a_bisection() {
    let before = [("ogg_vorbis.ogg", 1), ("ogg_flac.oga", 6), (SPEEX.0, 1), (THEORA.0, 5)];
    let corpus = before.into_iter().filter_map(|(name, most)| corpus(name).zip(Some(most)));
    let before = [("128kbps.opus", 4), ("noise.opus", 6), ("noise.oga", 11), ("noise.ogg", 6)];
    let made = fixtures().iter().filter_map(|path| {
        let name = path.file_name()?.to_str()?;
        before.iter().find(|(end, _)| name.ends_with(end)).map(|(_, most)| (path.clone(), *most))
    });
    for (path, before) in corpus.chain(made) {
        for known in [false, true] {
            let most = most_probes(&path, known);
            assert!(most <= before, "{path:?} end known {known}: {most} probes");
        }
    }
    // ten minutes of Opus at 24 kbit/s, 1.5 MB
    let noise = "anoisesrc=d=600:a=0.3:r=48000:seed=4";
    let Some(long) = ffmpeg("long.opus", noise, &["-ac", "1", "-c:a", "libopus", "-b:a", "24k"])
    else {
        return;
    };
    assert!(most_probes(&long, true) <= 2, "the end known");
    assert!(most_probes(&long, false) <= 4, "the end unknown");
}
