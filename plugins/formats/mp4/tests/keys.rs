// Tests own their fixtures and clock.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pf_mp4::{Keyed, Limits, Mp4Demux, Mp4Reader, Sample, Tables, TrackKind};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::{Index, Reach, SeekMode, SeekPlan};
use profluens_core::pull::Ask;
use profluens_core::time::Timestamp;
use profluens_video::rap::{NalSyntax, random_access};

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];

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

static SRC: ElementDesc = desc("holdsrc", &[], &SRC_PADS);
static REC: ElementDesc = desc("rec", &SINK_PADS, &[]);

/// Serves ranges from memory, those at or past `hold` once it is lifted.
struct HoldSrc {
    data: Arc<Vec<u8>>,
    hold: Arc<AtomicU64>,
    held: Vec<Ask>,
}

impl HoldSrc {
    fn serve(&self, ctx: &mut Ctx, ask: Ask) {
        let Ask { offset, len, tag, mut buf } = ask;
        let from = (offset as usize).min(self.data.len());
        let to = (from + len as usize).min(self.data.len());
        buf.memory.as_mut_full()[..to - from].copy_from_slice(&self.data[from..to]);
        buf.memory.set_len(to - from);
        ctx.serve(SrcPad(0), tag, buf);
    }
}

impl Element for HoldSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.offer_ranges(SrcPad(0));
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(ask) = ctx.next_ask(SrcPad(0)) {
            self.held.push(ask);
        }
        let hold = self.hold.load(Ordering::Acquire);
        let (go, keep): (Vec<Ask>, Vec<Ask>) =
            std::mem::take(&mut self.held).into_iter().partition(|a| a.offset < hold);
        self.held = keep;
        for ask in go {
            self.serve(ctx, ask);
        }
        if self.held.is_empty() && ctx.asks_over(SrcPad(0)) {
            return Ok(Flow::Eos);
        }
        ctx.wake_within(Duration::from_millis(1));
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.held.clear();
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Buf { pts: u64, key: bool },
    Flush,
}

struct Rec(Arc<Mutex<Vec<Ev>>>);

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut evs = self.0.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            if buf.flags.contains(BufferFlags::HEADER) {
                continue;
            }
            let (pts, key) = (buf.pts.nanos().unwrap(), buf.flags.contains(BufferFlags::KEYFRAME));
            evs.push(Ev::Buf { pts, key });
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.0.lock().unwrap().push(Ev::Flush);
        }
        Ok(())
    }
}

fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let began = Instant::now();
    while !cond() {
        assert!(began.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// What ffmpeg writes for `args` with the `moov` behind the `mdat`, None without it.
fn ffmpeg(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    static MADE: AtomicU64 = AtomicU64::new(0);
    let n = MADE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("pf_mp4_keys_{}_{n}_{name}", std::process::id()));
    let ok = std::process::Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    let file = ok.then(|| std::fs::read(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    if file.is_none() {
        eprintln!("ffmpeg could not make {name}, skipped");
    }
    file
}

/// 4 s at 24 fps beside AAC, `video` naming the encoder and its options.
fn movie(name: &str, video: &[&str]) -> Option<Vec<u8>> {
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc2=d=4:s=160x96:r=24",
        "-f",
        "lavfi",
        "-i",
        "sine=d=4:r=48000",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-pix_fmt",
        "yuv420p",
    ];
    args.extend_from_slice(video);
    args.extend_from_slice(&["-c:a", "aac"]);
    ffmpeg(name, &args)
}

fn hevc_open_gops() -> Option<Vec<u8>> {
    let x265 = "open-gop=1:keyint=24:min-keyint=24:scenecut=0:log-level=error";
    movie("hevc.mp4", &["-c:v", "libx265", "-x265-params", x265])
}

/// Top level atoms of `file`, type, offset and size.
fn atoms(file: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let (mut at, mut out) = (0usize, Vec::new());
    while at + 8 <= file.len() {
        let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        out.push((file[at + 4..at + 8].try_into().unwrap(), at, size));
        at += size;
    }
    out
}

/// The bytes before the `mdat` with the `moov` behind it appended, the head the demuxer takes.
fn head_of(file: &[u8]) -> Vec<u8> {
    let all = atoms(file);
    let mdat = all.iter().find(|a| &a.0 == b"mdat").unwrap().1;
    let moov = all.iter().find(|a| &a.0 == b"moov").unwrap();
    assert!(moov.1 > mdat, "ffmpeg puts the moov last");
    [&file[..mdat], &file[moov.1..moov.1 + moov.2]].concat()
}

/// `file` with sample `n` of the one `stss` added, the atoms around it grown by four octets. The
/// `moov` is last, so no chunk offset moves.
fn with_sync(file: &[u8], n: u32) -> Vec<u8> {
    let moov = atoms(file).into_iter().find(|a| &a.0 == b"moov").unwrap().1;
    let at = moov + file[moov..].windows(4).position(|w| w == b"stss").unwrap() - 4;
    assert_eq!(file[at + 8..].windows(4).position(|w| w == b"stss"), None, "one stss");
    let mut out = file.to_vec();
    let count = u32::from_be_bytes(file[at + 12..at + 16].try_into().unwrap()) as usize;
    let rows: Vec<u32> = (0..count)
        .map(|k| u32::from_be_bytes(file[at + 16 + 4 * k..at + 20 + 4 * k].try_into().unwrap()))
        .collect();
    let place = rows.partition_point(|&r| r < n);
    out.splice(at + 16 + 4 * place..at + 16 + 4 * place, n.to_be_bytes());
    out[at + 12..at + 16].copy_from_slice(&(count as u32 + 1).to_be_bytes());
    // every atom from the moov down to the stss holds it
    let mut from = 0usize;
    while from < at + 8 {
        let size = u32::from_be_bytes(out[from..from + 4].try_into().unwrap());
        if from + size as usize > at {
            out[from..from + 4].copy_from_slice(&(size + 4).to_be_bytes());
            let kind = &out[from + 4..from + 8];
            from += match kind {
                b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => 8,
                _ => break,
            };
        } else {
            from += size as usize;
        }
    }
    out
}

struct Video {
    tables: Tables,
    /// The video track's samples in decode order, and their indices in the tables.
    samples: Vec<(usize, Sample)>,
}

impl Video {
    fn of(file: &[u8]) -> Video {
        let tables = Tables::resolve(&head_of(file), &[], Limits::default()).unwrap();
        let track = tables.tracks().iter().position(|t| t.kind() == TrackKind::Video).unwrap();
        let samples = tables
            .samples()
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, s)| s.track() == track)
            .collect();
        Video { tables, samples }
    }

    fn ns(&self, s: &Sample) -> u64 {
        self.tables.tracks()[s.track()].stamp_ns(s.pts)
    }

    fn rap(&self, file: &[u8], s: &Sample) -> Option<bool> {
        let data = &file[s.offset as usize..s.offset as usize + s.size as usize];
        random_access(NalSyntax::Hevc, 4, data)
    }
}

struct Run {
    video: Vec<Ev>,
    audio: Vec<Ev>,
    index: Arc<Index>,
    /// The index published when the seek was planned on it.
    planned: Option<Arc<Index>>,
}

/// `file` pulled through `mp4demux`, the reads at or past `hold` held until a seek to `to`
/// planned on the published index is sent.
fn run(file: &[u8], hold: u64, to: Option<u64>) -> Run {
    let mut p = pf_reactor::pipeline();
    let hold = Arc::new(AtomicU64::new(hold));
    let data = Arc::new(file.to_vec());
    let src = p.add(HoldSrc { data, hold: Arc::clone(&hold), held: Vec::new() });
    let demux = p.add(Mp4Demux::new(head_of(file)));
    p.link(src.src(SrcPad(0)), demux.sink(Mp4Demux::IN)).unwrap();
    let pads = p.preroll().unwrap().srcs;
    assert_eq!(pads.len(), 2, "video and audio");
    let recs: Vec<Arc<Mutex<Vec<Ev>>>> = pads.iter().map(|_| Arc::default()).collect();
    for (pad, rec) in pads.iter().zip(&recs) {
        let sink = p.add(Rec(Arc::clone(rec)));
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).unwrap();
    }
    let (control, tap) = (p.control(), p.tap_handle());
    let running = std::thread::spawn(move || p.run());
    let mut planned = None;
    if let Some(to) = to {
        until("a picture", || !recs[0].lock().unwrap().is_empty());
        let index = tap.index(demux).unwrap();
        let at = Timestamp(to);
        let plan = index.plan(at, SeekMode::Accurate, Timestamp::NONE, None, Reach::Group);
        let SeekPlan::Seek { to_byte, key, to_time } = plan else { panic!("{plan:?}") };
        control.seek_bytes(to_time, &[(profluens_core::command::ALL, to_byte, key)]).unwrap();
        until("the flush", || recs[0].lock().unwrap().contains(&Ev::Flush));
        planned = Some(index);
    }
    hold.store(u64::MAX, Ordering::Release);
    running.join().unwrap().unwrap();
    let take = |n: usize| std::mem::take(&mut *recs[n].lock().unwrap());
    Run { video: take(0), audio: take(1), index: tap.index(demux).unwrap(), planned }
}

fn bufs(evs: &[Ev]) -> Vec<(u64, bool)> {
    evs.iter()
        .filter_map(|e| match *e {
            Ev::Buf { pts, key } => Some((pts, key)),
            Ev::Flush => None,
        })
        .collect()
}

fn behind_flush(evs: &[Ev]) -> &[Ev] {
    &evs[evs.iter().rposition(|e| *e == Ev::Flush).unwrap() + 1..]
}

fn times(index: &Index) -> Vec<u64> {
    index.entries.iter().map(|e| e.time_ns).collect()
}

/// x265 marks its CRAs in the `stss`, each a random access point, and a sample of a trailing
/// picture added there is put out as no keyframe and leaves the index once read. A seek just
/// past it lands on it, steps back to the CRA before it, and the audio goes on once.
#[test]
fn hevc_keys_come_from_the_nal_types_and_a_landing_on_a_false_one_steps_back() {
    let Some(clean) = hevc_open_gops() else { return };
    let v = Video::of(&clean);
    let syncs: Vec<usize> = (0..v.samples.len()).filter(|&k| v.samples[k].1.sync).collect();
    assert!(syncs.len() >= 3, "{syncs:?}");
    for &k in &syncs {
        assert_eq!(v.rap(&clean, &v.samples[k].1), Some(true), "sample {} is an IRAP", k + 1);
    }
    let false_at = (syncs[1] + 8..syncs[2])
        .find(|&k| v.rap(&clean, &v.samples[k].1) == Some(false))
        .expect("a trailing picture between the CRAs");
    let file = with_sync(&clean, false_at as u32 + 1);
    let v = Video::of(&file);
    let (cra, fake) = (v.samples[syncs[1]].1, v.samples[false_at].1);
    assert!(fake.sync, "the stss names it");
    let (cra_ns, fake_ns) = (v.ns(&cra), v.ns(&fake));
    let iraps: Vec<u64> = syncs.iter().map(|&k| v.ns(&v.samples[k].1)).collect();
    assert!(times(&v.tables.seek_index(&|_| false)).contains(&fake_ns), "the index offers it");

    let played = run(&file, u64::MAX, None);
    let pictures = bufs(&played.video);
    assert_eq!(pictures.len(), v.samples.len());
    for (pts, key) in &pictures {
        assert_eq!(*key, iraps.contains(pts), "picture at {pts}");
    }
    assert_eq!(times(&played.index), iraps, "the false key left the index once read");

    let sought = run(&file, fake.offset, Some(fake_ns + 1));
    let planned = sought.planned.as_deref().unwrap();
    assert!(times(planned).contains(&fake_ns), "the seek was planned on the false key");
    let after = bufs(behind_flush(&sought.video));
    assert_eq!(after.first(), Some(&(cra_ns, true)), "decoding starts at the CRA before");
    assert!(after.contains(&(fake_ns, false)), "the false key goes out as a picture");
    assert_eq!(
        after.iter().filter(|b| b.1).count(),
        iraps.iter().filter(|t| **t >= cra_ns).count()
    );
    let audio = bufs(behind_flush(&sought.audio));
    assert!(!audio.is_empty() && audio.windows(2).all(|w| w[0].0 < w[1].0), "{audio:?}");
    assert_eq!(times(&sought.index), iraps, "the false key left the index");
}

/// libx264's IDR pictures are its `stss`, the flags and the index stand as the tables say.
#[test]
fn h264_idr_keys_and_the_index_stand() {
    let Some(file) = movie("h264.mp4", &["-c:v", "libx264", "-g", "24"]) else { return };
    let v = Video::of(&file);
    let keys: Vec<u64> = v.samples.iter().filter(|s| s.1.sync).map(|s| v.ns(&s.1)).collect();
    assert!(keys.len() >= 4, "{keys:?}");
    let played = run(&file, u64::MAX, None);
    let pictures = bufs(&played.video);
    assert_eq!(pictures.len(), v.samples.len());
    for (pts, key) in &pictures {
        assert_eq!(*key, keys.contains(pts), "picture at {pts}");
    }
    let tables = v.tables.seek_index(&|_| false);
    assert_eq!((&played.index.entries, played.index.duration), (&tables.entries, tables.duration));
    let third = v.samples.iter().filter(|s| s.1.sync).nth(2).unwrap().1;
    let sought = run(&file, third.offset, Some(v.ns(&third) + 1));
    let after = bufs(behind_flush(&sought.video));
    assert_eq!(after.first(), Some(&(v.ns(&third), true)), "decoding starts at the IDR");
}

/// A pushed source cannot go back, a restart on a false key waits for the next random access
/// point and puts nothing of its track out before it.
#[test]
fn a_pushed_restart_on_a_false_key_waits_for_the_next_random_access_point() {
    let Some(clean) = hevc_open_gops() else { return };
    let v = Video::of(&clean);
    let syncs: Vec<usize> = (0..v.samples.len()).filter(|&k| v.samples[k].1.sync).collect();
    let false_at =
        (syncs[1] + 8..syncs[2]).find(|&k| v.rap(&clean, &v.samples[k].1) == Some(false)).unwrap();
    let file = with_sync(&clean, false_at as u32 + 1);
    let v = Video::of(&file);
    let (fake, next) = (v.samples[false_at].1, v.samples[syncs[2]].1);
    let mut reader = Mp4Reader::new(&head_of(&file)).unwrap();
    let track = fake.track();
    let r = reader.seek_stream(fake.offset, Some(v.ns(&fake)), v.ns(&fake));
    reader.push_bytes(&file[r.to_byte as usize..]);
    let mut out = Vec::new();
    while let Some(s) = reader.next_sample() {
        if s.track_index == track {
            out.push((s.pts, reader.keyed(track, None, s.sync, s.payload.data(), false)));
        }
    }
    assert_eq!(out[0], (fake.pts, Keyed::Skip), "the false key is held back");
    let first = out.iter().position(|o| o.1 != Keyed::Skip).unwrap();
    assert_eq!(out[first], (next.pts, Keyed::Put { key: true }), "the next CRA");
    assert!(out[first + 1..].iter().all(|o| matches!(o.1, Keyed::Put { .. })));
}
