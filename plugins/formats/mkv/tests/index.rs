//! The seek index `mkvdemux` publishes, from read Cues, grown from Clusters, or handed over.

// A test's own fixtures and threads, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pf_mkv::ebml::{self, id};
use pf_mkv::{MatroskaWriter, MkvDemux, TrackConfig, parse_cue_tracks, parse_seek_head};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::{Index, IndexEntry, Pipeline, Reach, SeekMode, SeekPlan};
use profluens_core::pull::Ask;
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

const MS: u64 = 1_000_000;

const AAC_HEAD: [u8; 2] = [0x11, 0x90];

fn film(ms: u64, gop: u64, audio: bool, cues: bool) -> Vec<u8> {
    let mut tracks = vec![TrackConfig::vp8(1, 320, 240)];
    if audio {
        tracks.push(TrackConfig::audio(2, "A_AAC", AAC_HEAD.to_vec(), 48_000.0, 2, 0));
    }
    let mut w = MatroskaWriter::new(tracks);
    if cues {
        w.enable_cues();
    }
    w.set_duration_ns(ms * MS);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    for t in (0..ms).step_by(20) {
        if t % 40 == 0 {
            w.write_frame(&mut out, 1, t * MS, &[1, (t / 40) as u8], t % gop == 0).unwrap();
        }
        if audio {
            w.write_frame(&mut out, 2, t * MS, &[2, (t / 20) as u8], true).unwrap();
        }
    }
    w.finalize(&mut out);
    sized(out)
}

fn sized(file: Vec<u8>) -> Vec<u8> {
    let at = file.windows(4).position(|w| w == id::SEGMENT).expect("a Segment") + 4;
    assert_eq!(file[at], 0xFF, "the writer leaves the size unknown");
    let size = (file.len() - at - 1) as u64;
    let mut out = file[..at].to_vec();
    out.push(0x01);
    out.extend_from_slice(&size.to_be_bytes()[1..]);
    out.extend_from_slice(&file[at + 1..]);
    out
}

fn clusters(file: &[u8]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for at in 0..file.len().saturating_sub(4) {
        if &file[at..at + 4] != id::CLUSTER {
            continue;
        }
        let Ok(h) = ebml::read_element_header(file, at) else {
            continue;
        };
        let Ok(t) = ebml::read_element_header(file, h.data_start) else {
            continue;
        };
        if t.id != id::TIMESTAMP {
            continue;
        }
        let v = &file[t.data_start..t.data_start + t.size.unwrap() as usize];
        out.push((at as u64, v.iter().fold(0u64, |a, b| a << 8 | u64::from(*b)) * MS));
    }
    out
}

fn header(file: &[u8]) -> Vec<u8> {
    file[..file.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster")].to_vec()
}

fn cues_at(file: &[u8]) -> u64 {
    let info = parse_seek_head(&header(file)).expect("a Segment");
    info.segment_data_start + info.cues_pos.expect("Cues")
}

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SINK: ElementDesc = ElementDesc {
    name: "rec",
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
static SRC: ElementDesc = ElementDesc {
    name: "memsrc",
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

#[derive(Default)]
struct Seen {
    rows: Vec<Option<u64>>,
    lengths: Vec<usize>,
}

struct Rec {
    seen: Arc<Mutex<Seen>>,
    index: Option<(profluens_core::counters::TapHandle, ElementId)>,
}

impl Rec {
    const IN: SinkPad = SinkPad(0);
}

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut seen = self.seen.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            if buf.memory.data() != AAC_HEAD {
                seen.rows.push(buf.pts.nanos());
            }
        }
        if let Some((tap, demux)) = &self.index {
            let n = tap.index(*demux).map_or(0, |index| index.entries.len());
            if seen.lengths.last() != Some(&n) {
                seen.lengths.push(n);
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.seen.lock().unwrap().rows.push(None);
        }
        Ok(())
    }
}

struct MemSrc {
    data: Vec<u8>,
    chunk: usize,
    pos: usize,
    pull: Option<Pulled>,
}

/// The offset and length of each ask served.
type Asks = Arc<Mutex<Vec<(u64, u32)>>>;

struct Pulled {
    held_at: u64,
    held: Vec<Ask>,
    release: Arc<AtomicBool>,
    end: Arc<AtomicBool>,
    /// Bytes served, when counted.
    served: Option<Arc<AtomicU64>>,
    /// Each ask's offset and length, when kept.
    asks: Option<Asks>,
}

impl MemSrc {
    const OUT: SrcPad = SrcPad(0);

    fn serve(&self, ctx: &mut Ctx, ask: Ask, pull: &Pulled) {
        let Ask { offset, len, tag, mut buf } = ask;
        let served = pull.served.as_deref();
        if let Some(asks) = &pull.asks {
            asks.lock().unwrap().push((offset, len));
        }
        let from = (offset as usize).min(self.data.len());
        let to = (from + len as usize).min(self.data.len());
        if let Some(s) = served {
            s.fetch_add((to - from) as u64, Ordering::Relaxed);
        }
        buf.memory.as_mut_full()[..to - from].copy_from_slice(&self.data[from..to]);
        buf.memory.set_len(to - from);
        ctx.serve(Self::OUT, tag, buf);
    }
}

impl Element for MemSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.pull.is_some() {
            ctx.offer_ranges(Self::OUT);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(mut pull) = self.pull.take() {
            let released = pull.release.load(Ordering::Acquire);
            while let Some(ask) = ctx.next_ask(Self::OUT) {
                match ask.offset == pull.held_at && !released {
                    true => pull.held.push(ask),
                    false => self.serve(ctx, ask, &pull),
                }
            }
            if released {
                for ask in std::mem::take(&mut pull.held) {
                    self.serve(ctx, ask, &pull);
                }
            }
            let over = pull.held.is_empty()
                && ctx.asks_over(Self::OUT)
                && pull.end.load(Ordering::Acquire);
            self.pull = Some(pull);
            return Ok(if over { Flow::Eos } else { Flow::Ok });
        }
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = self.chunk.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if !matches!(event, Event::FlushStart) {
            return Ok(());
        }
        match &mut self.pull {
            Some(pull) => pull.held.clear(),
            None => self.pos = ctx.seek_target().map_or(0, |t| t.to_byte as usize),
        }
        Ok(())
    }
}

fn build(
    p: &mut Pipeline,
    src: ElementId,
    demux: MkvDemux,
    link: &dyn Fn(usize) -> bool,
) -> (ElementId, Vec<Arc<Mutex<Seen>>>) {
    let demux = p.add(demux);
    p.link(src.src(SrcPad(0)), demux.sink(MkvDemux::IN)).expect("src ! demux");
    let added = p.preroll().expect("preroll").srcs;
    let mut seen = Vec::new();
    for (n, pad) in added.iter().enumerate().filter(|(n, _)| link(*n)) {
        let rec = Arc::new(Mutex::new(Seen::default()));
        let index = (n == 0).then(|| (p.tap_handle(), demux));
        let sink = p.add(Rec { seen: Arc::clone(&rec), index });
        p.link(pad.element.src(pad.pad), sink.sink(Rec::IN)).expect("pad ! sink");
        seen.push(rec);
    }
    (demux, seen)
}

fn pulled(name: &str, file: &[u8], link: &dyn Fn(usize) -> bool) -> Arc<Index> {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mkv_index");
    std::fs::create_dir_all(&dir).expect("a tmp dir");
    let path = dir.join(name);
    std::fs::write(&path, file).expect("write the fixture");
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(path.to_str().expect("utf-8")));
    let (demux, _) = build(&mut p, src, MkvDemux::new(header(file)), link);
    p.run().expect("run");
    p.index(demux).expect("published")
}

#[test]
fn the_published_index_is_the_cued_keyframes_and_where_each_restarts() {
    let file = film(1_600, 400, true, true);
    let at = clusters(&file);
    let times: Vec<u64> = at.iter().map(|c| c.1).collect();
    assert_eq!(times, [0, 400 * MS, 800 * MS, 1_200 * MS], "a Cluster a keyframe");
    // AAC's short pre-roll beside the video restarts at the keyframe, alone it goes back
    let keys: Vec<IndexEntry> =
        at.iter().map(|&(byte, time)| IndexEntry::key(time, byte)).collect();
    let index = pulled("cued_av.mkv", &file, &|_| true);
    assert_eq!(index.entries, keys);
    assert!(index.complete, "every keyframe is cued");
    assert_eq!((index.file_len, index.duration), (Some(file.len() as u64), Timestamp(1_600 * MS)));
    let index = pulled("cued_a.mkv", &file, &|n| n == 1);
    let back: Vec<IndexEntry> = (0..4)
        .map(|n| {
            let (byte, time_ns) = at[n];
            let (restart, restart_ns) = at[n.saturating_sub(1)];
            IndexEntry { time_ns, byte, restart, restart_ns }
        })
        .collect();
    assert_eq!(index.entries, back);
    let index = pulled("cued_v.mkv", &file, &|n| n == 0);
    assert_eq!(index.entries, keys);
}

/// Cues of more than 4 MB, as a file of many subtitle tracks cues every block of each, are read.
#[test]
fn large_cues_are_read_whole() {
    let file = film(9_000_000, 40, false, true);
    assert!(file.len() as u64 - cues_at(&file) > 4 << 20, "Cues over 4 MB");
    let index = pulled("large_cues.mkv", &file, &|_| true);
    assert!(index.complete, "the Cues were read");
    assert_eq!(index.entries.len(), 225_000);
}

#[test]
fn a_cueless_file_grows_its_index_from_the_clusters_read() {
    let file = film(1_600, 400, true, false);
    assert!(parse_seek_head(&header(&file)).is_some_and(|info| info.cues_pos.is_none()));
    let index = pulled("cueless.mkv", &file, &|_| true);
    let keys: Vec<IndexEntry> =
        clusters(&file).iter().map(|&(byte, time)| IndexEntry::key(time, byte)).collect();
    assert_eq!(index.entries, keys);
    assert!(!index.complete);
    assert_eq!(index.file_len, Some(file.len() as u64));
}

#[test]
fn a_pushed_stream_grows_its_index_and_publishes_it_as_it_goes() {
    let file = film(8_000, 40, false, true);
    let at = clusters(&file);
    assert_eq!(at.len(), 200);
    let mut p = Pipeline::new();
    let src = p.add(MemSrc { data: file.clone(), chunk: 96, pos: 0, pull: None });
    let (demux, seen) = build(&mut p, src, MkvDemux::new(header(&file)), &|_| true);
    assert_eq!(
        p.index(demux).map(|index| index.entries.len()),
        Some(0),
        "published at preroll, empty"
    );
    p.run().expect("run");
    let index = p.index(demux).expect("published");
    let keys: Vec<IndexEntry> =
        at.iter().map(|&(byte, time)| IndexEntry::key(time, byte)).collect();
    assert_eq!(index.entries, keys);
    assert!(!index.complete);
    let lengths = seen[0].lock().unwrap().lengths.clone();
    let grown = lengths.windows(2).all(|w| w[1] == 200 || (w[1] >= w[0] + 64 && w[1] < w[0] + 72));
    assert!(
        grown && lengths.len() > 2 && lengths.last() == Some(&200),
        "published as it grew, {lengths:?}"
    );
}

#[test]
fn cues_handed_over_are_published_at_preroll() {
    let file = film(1_600, 400, false, true);
    let info = parse_seek_head(&header(&file)).expect("a Segment");
    let cues = parse_cue_tracks(
        &file[cues_at(&file) as usize..],
        info.segment_data_start,
        info.timestamp_scale,
    );
    let mut p = Pipeline::new();
    let src = p.add(MemSrc { data: file.clone(), chunk: 4096, pos: 0, pull: None });
    let (demux, _) = build(
        &mut p,
        src,
        MkvDemux::new(header(&file)).with_cues(cues, Some(cues_at(&file))),
        &|_| true,
    );
    let index = p.index(demux).expect("published at preroll");
    let keys: Vec<IndexEntry> =
        clusters(&file).iter().map(|&(byte, time)| IndexEntry::key(time, byte)).collect();
    assert_eq!((&index.entries, index.complete), (&keys, true));
    p.run().expect("run");
    assert_eq!(p.index(demux).expect("published").entries, keys, "nothing grown over the Cues");
}

fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let began = Instant::now();
    while !cond() {
        assert!(began.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_seek_before_the_cues_are_in_lands_on_the_estimate_and_the_next_exactly() {
    let file = film(8_000, 1_000, false, true);
    let at = clusters(&file);
    let (release, end) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let held = Pulled {
        held_at: cues_at(&file),
        held: Vec::new(),
        release: Arc::clone(&release),
        end: Arc::clone(&end),
        served: None,
        asks: None,
    };
    let mut p = pf_reactor::pipeline();
    let src = p.add(MemSrc { data: file.clone(), chunk: 0, pos: 0, pull: Some(held) });
    let (demux, seen) = build(&mut p, src, MkvDemux::new(header(&file)), &|_| true);
    let (control, tap) = (p.control(), p.tap_handle());
    let run = std::thread::spawn(move || p.run());
    let rows = |seen: &Arc<Mutex<Seen>>| seen.lock().unwrap().rows.clone();
    until("the pictures", || rows(&seen[0]).len() >= 200);
    let index = tap.index(demux).expect("published");
    assert!(!index.complete && index.entries.is_empty(), "{index:?}");
    let target = Timestamp(5_500 * MS);
    let plan = index.plan(target, SeekMode::Keyframe, Timestamp::NONE, None, Reach::Group);
    let SeekPlan::Seek { to_byte, key, to_time } = plan else { panic!("{plan:?}") };
    assert_eq!((key, to_byte), (target, file.len() as u64 * 55 / 80), "the estimate");
    let _ = control.seek_bytes(to_time, &[(profluens_core::command::ALL, to_byte, key)]);
    let landed = at.iter().find(|c| c.0 >= to_byte).expect("a Cluster behind the estimate").1;
    let first = |seen: &Arc<Mutex<Seen>>, n: usize| {
        let rows = rows(seen);
        let flush = rows.iter().enumerate().filter(|(_, r)| r.is_none()).nth(n).map(|(i, _)| i)?;
        rows.get(flush + 1).copied().flatten()
    };
    until("the first seek", || first(&seen[0], 0).is_some());
    assert_eq!(first(&seen[0], 0), Some(landed), "the keyframe behind the estimated byte");
    assert_ne!(landed, 5_000 * MS, "which is not the target's");
    release.store(true, Ordering::Release);
    until("the Cues", || tap.index(demux).is_some_and(|index| index.complete));
    let index = tap.index(demux).expect("published");
    assert_eq!(index.entries.len(), 8);
    let plan =
        index.plan(Timestamp(2_500 * MS), SeekMode::Accurate, Timestamp::NONE, None, Reach::Group);
    assert_eq!(
        plan,
        SeekPlan::Seek {
            to_byte: at[2].0,
            key: Timestamp(2_000 * MS),
            to_time: Timestamp(2_500 * MS)
        }
    );
    let SeekPlan::Seek { to_byte, key, to_time } = plan else { unreachable!() };
    let _ = control.seek_bytes(to_time, &[(profluens_core::command::ALL, to_byte, key)]);
    until("the second seek", || first(&seen[0], 1).is_some());
    assert_eq!(first(&seen[0], 1), Some(2_000 * MS), "the keyframe at two seconds");
    end.store(true, Ordering::Release);
    run.join().expect("the run thread").expect("the run ends");
}

/// The frames past the read window of a track no pad takes are walked past unread. PCM's
/// 64 KB blocks beside a small video, with the video alone only the 16 KB window that finds
/// each block's head is read of it, every picture still out, past the start's lead of 512 KB.
/// With the PCM linked it is all read.
#[test]
fn an_unlinked_tracks_large_frames_are_not_read() {
    let tracks = vec![
        TrackConfig::vp8(1, 320, 240),
        TrackConfig::audio(2, "A_PCM/INT/LIT", Vec::new(), 48_000.0, 2, 16),
    ];
    let mut w = MatroskaWriter::new(tracks);
    w.enable_cues();
    w.set_duration_ns(2_000 * MS);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let pcm = vec![7u8; 64 << 10];
    for t in (0..2_000).step_by(20) {
        if t % 40 == 0 {
            w.write_frame(&mut out, 1, t * MS, &[1, (t / 40) as u8], t % 1_000 == 0).unwrap();
        }
        w.write_frame(&mut out, 2, t * MS, &pcm, true).unwrap();
    }
    w.finalize(&mut out);
    let file = sized(out);
    let run = |link: &dyn Fn(usize) -> bool| {
        let served = Arc::new(AtomicU64::new(0));
        let end = Arc::new(AtomicBool::new(true));
        let pulled = Pulled {
            held_at: u64::MAX,
            held: Vec::new(),
            release: Arc::new(AtomicBool::new(true)),
            end: Arc::clone(&end),
            served: Some(Arc::clone(&served)),
            asks: None,
        };
        let mut p = pf_reactor::pipeline();
        let src = p.add(MemSrc { data: file.clone(), chunk: 0, pos: 0, pull: Some(pulled) });
        let (_, seen) = build(&mut p, src, MkvDemux::new(header(&file)), link);
        p.run().expect("run");
        let pictures = seen[0].lock().unwrap().rows.iter().filter(|r| r.is_some()).count();
        (served.load(Ordering::Relaxed), pictures)
    };
    let (video_only, pictures) = run(&|n| n == 0);
    assert_eq!(pictures, 50, "every picture");
    let lead = 512 << 10;
    assert!(
        video_only < file.len() as u64 * 3 / 10 + lead,
        "{video_only} of {} bytes read",
        file.len()
    );
    let (both, _) = run(&|_| true);
    assert!(both >= file.len() as u64 * 9 / 10, "{both} of {} bytes read", file.len());
}

/// A seek's first read holds its Cluster's head and the keyframe, one ask the size of the
/// keyframes seen, so the file is read once there instead of a small window and the block apart.
#[test]
fn a_seeks_first_read_holds_the_keyframe_of_its_cluster() {
    const KEY: usize = 300 << 10;
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240)]);
    w.enable_cues();
    w.set_duration_ns(8_000 * MS);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let key = vec![9u8; KEY];
    for t in (0..8_000).step_by(40) {
        let first = t % 1_000 == 0;
        let frame: &[u8] = if first { &key } else { &[1, 2] };
        w.write_frame(&mut out, 1, t * MS, frame, first).unwrap();
    }
    w.finalize(&mut out);
    let file = sized(out);
    let at = clusters(&file);
    assert_eq!(at.len(), 8, "a Cluster a keyframe");
    let asks = Arc::new(Mutex::new(Vec::new()));
    let pulled = Pulled {
        held_at: u64::MAX,
        held: Vec::new(),
        release: Arc::new(AtomicBool::new(true)),
        end: Arc::new(AtomicBool::new(false)),
        served: None,
        asks: Some(Arc::clone(&asks)),
    };
    let mut p = pf_reactor::pipeline();
    let src = p.add(MemSrc { data: file.clone(), chunk: 0, pos: 0, pull: Some(pulled) });
    let (demux, seen) = build(&mut p, src, MkvDemux::new(header(&file)), &|_| true);
    let (control, tap) = (p.control(), p.tap_handle());
    let run = std::thread::spawn(move || p.run());
    let rows = |seen: &Arc<Mutex<Seen>>| seen.lock().unwrap().rows.clone();
    until("the pictures", || rows(&seen[0]).len() >= 200);
    until("the Cues", || tap.index(demux).is_some_and(|index| index.complete));
    asks.lock().unwrap().clear();
    let flushes = |rows: &[Option<u64>]| rows.iter().filter(|r| r.is_none()).count();
    let before = flushes(&rows(&seen[0]));
    let index = tap.index(demux).expect("published");
    let plan =
        index.plan(Timestamp(5_000 * MS), SeekMode::Keyframe, Timestamp::NONE, None, Reach::Group);
    let SeekPlan::Seek { to_byte, key, to_time } = plan else { panic!("{plan:?}") };
    assert_eq!(to_byte, at[5].0, "the Cluster at five seconds");
    let _ = control.seek_bytes(to_time, &[(profluens_core::command::ALL, to_byte, key)]);
    until("the keyframe", || {
        let rows = rows(&seen[0]);
        flushes(&rows) > before && rows.last().is_some_and(Option::is_some)
    });
    let asked = asks.lock().unwrap().clone();
    let (offset, len) = asked.first().copied().expect("an ask");
    assert_eq!(offset, at[5].0, "from the Cluster, {asked:?}");
    assert!(len as usize > KEY && offset + u64::from(len) < at[6].0 + (512 << 10), "{asked:?}");
    let inside = |&&(o, _): &&(u64, u32)| o > offset && o < offset + KEY as u64;
    assert!(!asked.iter().skip(1).any(|a| inside(&a)), "the keyframe read once, {asked:?}");
    control.stop();
    run.join().expect("the run thread").expect("the run ends");
}

/// A held start asks nothing until its seek, which then reads from its own Cluster, and a seek
/// to the start releases it from the first Cluster.
#[test]
fn a_held_start_reads_nothing_until_its_seek() {
    let file = film(4_000, 1_000, true, true);
    for target in [2_000 * MS, 0] {
        let asks = Arc::new(Mutex::new(Vec::new()));
        let pulled = Pulled {
            held_at: u64::MAX,
            held: Vec::new(),
            release: Arc::new(AtomicBool::new(true)),
            end: Arc::new(AtomicBool::new(false)),
            served: None,
            asks: Some(Arc::clone(&asks)),
        };
        let mut p = pf_reactor::pipeline();
        let src = p.add(MemSrc { data: file.clone(), chunk: 0, pos: 0, pull: Some(pulled) });
        let demux = MkvDemux::new(header(&file)).with_cues(cues_of(&file), Some(cues_at(&file)));
        let (demux, seen) = build(&mut p, src, demux.with_held_start(), &|_| true);
        let (control, tap) = (p.control(), p.tap_handle());
        let run = std::thread::spawn(move || p.run());
        std::thread::sleep(Duration::from_millis(50));
        assert!(asks.lock().unwrap().is_empty(), "nothing read before the seek");
        assert!(seen[0].lock().unwrap().rows.iter().all(Option::is_none), "nor put out");
        let index = tap.index(demux).expect("published at preroll");
        let plan =
            index.plan(Timestamp(target), SeekMode::Keyframe, Timestamp::NONE, None, Reach::Group);
        let SeekPlan::Seek { to_byte, key, to_time } = plan else { panic!("{plan:?}") };
        let _ = control.seek_bytes(to_time, &[(profluens_core::command::ALL, to_byte, key)]);
        until("a picture", || seen[0].lock().unwrap().rows.iter().any(Option::is_some));
        let first = asks.lock().unwrap().first().copied().expect("an ask");
        assert_eq!(first.0, to_byte, "read from the seek's Cluster");
        let rows = seen[0].lock().unwrap().rows.clone();
        assert_eq!(rows.iter().flatten().next(), Some(&target), "the seek's keyframe first");
        control.stop();
        run.join().expect("the run thread").expect("the run ends");
    }
}

fn cues_of(file: &[u8]) -> Vec<pf_mkv::Cue> {
    let info = parse_seek_head(&header(file)).expect("a Segment");
    parse_cue_tracks(&file[cues_at(file) as usize..], info.segment_data_start, info.timestamp_scale)
}
