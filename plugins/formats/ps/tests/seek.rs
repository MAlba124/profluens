//! Seeks in a VOB ffmpeg writes, pulled with the bisection and pushed from the estimate. Skipped
//! without ffmpeg. A restart for one track, on a fixture and on that VOB.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)] // test fixtures and rig

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pf_ps::{PsDemux, TAIL_LEN};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::{Index, Pipeline, SeekMode, Serves};
use profluens_core::sched::Outcome;
use profluens_core::time::Timestamp;

struct Film {
    bytes: Arc<[u8]>,
    /// Keyframe stamps in stream time.
    keys: Vec<u64>,
    end: u64,
}

fn film() -> Option<&'static Film> {
    static FILM: OnceLock<Option<Film>> = OnceLock::new();
    FILM.get_or_init(make).as_ref()
}

fn make() -> Option<Film> {
    let dir = std::env::temp_dir().join(format!("pf-ps-seek-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path: PathBuf = dir.join("long.vob");
    let made = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=96x64:rate=25:duration=20"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=20"])
        .args(["-c:v", "mpeg2video", "-b:v", "200k", "-g", "12", "-bf", "2"])
        .args(["-c:a", "mp2", "-b:a", "64k", "-output_ts_offset", "3", "-f", "vob"])
        .arg(&path)
        .status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("skipped, no ffmpeg");
        return None;
    }
    let probe = Command::new("ffprobe")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-show_entries", "packet=stream_index,pts,dts,flags"])
        .args(["-of", "csv=p=0"])
        .arg(&path)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let rows: Vec<(u32, Option<i64>, Option<i64>, bool)> = String::from_utf8(probe.stdout)
        .ok()?
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Some((
                f[0].parse().ok()?,
                f[1].parse().ok(),
                f[2].parse().ok(),
                f.get(3)?.starts_with('K'),
            ))
        })
        .collect();
    let first = |s: u32| rows.iter().find(|r| r.0 == s).and_then(|r| r.2.or(r.1));
    let origin = (0..2).filter_map(first).min()?;
    let ns = |t: i64| ((t - origin) as u64) * 100_000 / 9;
    let keys: Vec<u64> =
        rows.iter().filter(|r| r.0 == 0 && r.3).filter_map(|r| r.1).map(ns).collect();
    let end = rows.iter().filter_map(|r| r.1).max().map(ns)?;
    Some(Film { bytes: Arc::from(std::fs::read(&path).ok()?), keys, end })
}

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

static SRC: ElementDesc = desc("memsrc", &[], &SRC_PADS);
static SINK: ElementDesc = desc("recsink", &SINK_PADS, &[]);

/// Serves ranges when pulled, else pushes from where a seek's byte lands.
struct MemSrc {
    file: Arc<[u8]>,
    pull: bool,
    pos: usize,
}

impl Element for MemSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        if self.pull {
            ctx.offer_ranges(SrcPad(0));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if ctx.pulled(SrcPad(0)) {
            while let Some(ask) = ctx.next_ask(SrcPad(0)) {
                let mut buf = ask.buf;
                let from = (ask.offset as usize).min(self.file.len());
                let n = (ask.len as usize).min(self.file.len() - from).min(buf.memory.capacity());
                buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[from..from + n]);
                buf.memory.set_len(n);
                ctx.serve(SrcPad(0), ask.tag, buf);
            }
            return Ok(if ctx.asks_over(SrcPad(0)) { Flow::Eos } else { Flow::Ok });
        }
        if self.pos >= self.file.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let n = 4096.min(buf.memory.capacity()).min(self.file.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) && !ctx.pulled(SrcPad(0)) {
            let t = ctx.seek_target().unwrap_or(SeekTarget { to_byte: 0, at: Timestamp::ZERO });
            self.pos = (t.to_byte as usize).min(self.file.len());
        }
        Ok(())
    }
}

type Seen = Arc<Mutex<Vec<(Option<u64>, u32)>>>;

struct RecSink {
    seen: Seen,
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.seen.lock().expect("seen").push((buf.pts.nanos(), buf.flags.0));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.seen.lock().expect("seen").push((None, FLUSHED));
        }
        Ok(())
    }
}

/// A row standing for the flush, no flag a buffer carries.
const FLUSHED: u32 = 1 << 31;

/// The video units before and after a seek to `to` once 20 are out.
fn seek(
    film: &Film,
    demux: PsDemux,
    pull: bool,
    to: u64,
    mode: SeekMode,
) -> (usize, Vec<(Option<u64>, u32)>) {
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    let src = p.add(MemSrc { file: Arc::clone(&film.bytes), pull, pos: 0 });
    let demux = p.add(demux);
    p.set_element_pool_bytes(demux, 64 << 10, 16, 4 << 20);
    p.link(src.src(SrcPad(0)), demux.sink(PsDemux::IN)).expect("src to demux");
    p.set_select_all(false);
    p.preroll().expect("preroll");
    let pad = p.select(demux, 0).expect("the video");
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let sink = p.add(RecSink { seen: Arc::clone(&seen) });
    p.link(demux.src(pad), sink.sink(SinkPad(0))).expect("demux to sink");
    let mut run = p.step(Arc::clone(&shared)).expect("step");
    let control = p.control();
    let mut at = None;
    for _ in 0..2_000_000 {
        if at.is_none() && seen.lock().expect("seen").len() >= 20 {
            at = Some(seen.lock().expect("seen").len());
            control.seek(Timestamp(to), mode).expect("seekable");
        }
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        let Some(&g) = ready.first() else {
            match run.next_deadline() {
                Some(t) if t > clock.now() => clock.advance(Timestamp(t.0 - clock.now().0)),
                Some(_) => {}
                None => break,
            }
            continue;
        };
        run.pass(g).expect("a pass");
    }
    let ended = (0..run.groups()).all(|g| matches!(run.outcome(g), Some(Outcome::Exit)));
    assert!(ended, "the run ends");
    let _ = run.finish();
    std::thread::sleep(Duration::ZERO);
    let seen = seen.lock().expect("seen").clone();
    at.expect("seeked");
    let flushed = seen.iter().rposition(|u| u.1 == FLUSHED).expect("flushed");
    (flushed + 1, seen)
}

fn key(u: &(Option<u64>, u32)) -> bool {
    u.1 & BufferFlags::KEYFRAME.0 != 0
}

#[test]
fn a_pulled_seek_bisects_to_the_keyframe_before_the_target() {
    let Some(f) = film() else { return };
    let len = f.bytes.len() as u64;
    for to in [12_000_000_000u64, 3_100_000_000, 17_500_000_000] {
        let (at, seen) = seek(
            f,
            PsDemux::new(f.bytes[..256 << 10].to_vec()).with_len(Some(len)),
            true,
            to,
            SeekMode::Keyframe,
        );
        let after = &seen[at..];
        let lead = *after.first().expect("units after the seek");
        assert!(key(&lead), "{to}: the first unit is a key");
        let ns = lead.0.expect("stamped");
        assert!(f.keys.contains(&ns), "{to}: {ns} is a keyframe of the file");
        assert!(ns <= to && ns + 6_000_000_000 > to, "{to}: landed at {ns}");
        let frames = (f.end - ns) / 40_000_000 + 13;
        assert!(after.len() as u64 <= frames, "{to}: read on from the landing only");
    }
}

#[test]
fn a_pushed_seek_starts_at_a_keyframe_near_the_estimate() {
    let Some(f) = film() else { return };
    let len = f.bytes.len() as u64;
    let tail = f.bytes[f.bytes.len() - TAIL_LEN..].to_vec();
    let demux = PsDemux::new(f.bytes[..256 << 10].to_vec()).with_extent(len, tail);
    let (at, seen) = seek(f, demux, false, 10_000_000_000, SeekMode::Keyframe);
    let lead = *seen.get(at).expect("units after the seek");
    assert!(key(&lead), "the first unit is a key");
    let ns = lead.0.expect("stamped");
    assert!(f.keys.contains(&ns), "{ns} is a keyframe of the file");
    assert!(ns.abs_diff(10_000_000_000) < 4_000_000_000, "landed at {ns}");
    assert!(f.end > 19_000_000_000);
}

/// Keeps every buffer it takes past the first `skip` until it is taken out, a sink whose link
/// waits.
struct Hold {
    seen: Seen,
    kept: Vec<profluens_core::buffer::Buffer>,
    skip: usize,
}

impl Element for Hold {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            self.seen.lock().expect("seen").push((buf.pts.nanos(), buf.flags.0));
            self.kept.push(buf);
        }
        Ok(Flow::Ok)
    }
}

/// Passes every ready group until none is, false when the run did not end.
fn step_through(run: &mut profluens_core::sched::Stepped) -> bool {
    loop {
        run.service();
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        if ready.is_empty() {
            break;
        }
        for g in ready {
            run.pass(g).expect("pass");
        }
    }
    (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
}

type Units = Vec<(Option<u64>, u32)>;

/// Every unit of the video and of the audio as a whole run reads them, and the index it grew.
fn whole(file: &Arc<[u8]>) -> (Units, Units, Arc<Index>) {
    let mut p = Pipeline::new();
    let src = p.add(MemSrc { file: Arc::clone(file), pull: false, pos: 0 });
    let demux = p.add(PsDemux::new(file.to_vec()));
    p.link(src.src(SrcPad(0)), demux.sink(PsDemux::IN)).expect("src to demux");
    p.set_select_all(false);
    p.preroll().expect("preroll");
    let (video, audio): (Seen, Seen) = Default::default();
    for (stream, seen) in [(0, &video), (1, &audio)] {
        let pad = p.select(demux, stream).expect("select");
        let sink = p.add(RecSink { seen: Arc::clone(seen) });
        p.link(demux.src(pad), sink.sink(SinkPad(0))).expect("demux to sink");
    }
    p.run().expect("the run ends");
    let index = p.index(demux).expect("an index grown to the end");
    let (video, audio) = (video.lock().unwrap().clone(), audio.lock().unwrap().clone());
    (video, audio, index)
}

/// The audio of `file`, stream 1 under key `sound`, moved with a restart once the pool ran dry
/// with `slots` of it held past the first `skip`, pushed and pulled. Where each restarted, and
/// the first keyframe.
fn restart(file: &Arc<[u8]>, sound: u16, skip: usize, cases: &[(bool, u32)]) -> (Vec<u64>, u64) {
    use profluens_core::topology::Leave;
    let (video_all, audio_all, index) = whole(file);
    let (_, units) = common::units(file, sound, 4096);
    assert_eq!(audio_all.len(), units.len(), "a unit a PES");
    let mut restarts = Vec::new();
    for (&(pull, slots), twice) in cases.iter().flat_map(|c| [(c, false), (c, true)]) {
        let clock = MockClock::starting_at(Timestamp::from_secs(1));
        let shared: Arc<dyn Clock> = Arc::new(clock.clone());
        let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
        let src = p.add(MemSrc { file: Arc::clone(file), pull, pos: 0 });
        let len = Some(file.len() as u64);
        let demux = p.add(PsDemux::new(file.to_vec()).with_len(len));
        p.set_element_pool_bytes(demux, 64 << 10, slots, 4 << 20);
        p.link(src.src(SrcPad(0)), demux.sink(PsDemux::IN)).expect("src to demux");
        p.set_select_all(false);
        p.preroll().expect("preroll");
        let (video, held, mut moved): (Seen, Seen, Seen) = Default::default();
        let pad = p.select(demux, 0).expect("the video");
        let sink = p.add(RecSink { seen: Arc::clone(&video) });
        p.link(demux.src(pad), sink.sink(SinkPad(0))).expect("demux to sink");
        let pad = p.select(demux, 1).expect("the audio");
        let hold = p.add(Hold { seen: Arc::clone(&held), kept: Vec::new(), skip });
        p.link(demux.src(pad), hold.sink(SinkPad(0))).expect("demux to hold");
        let mut run = p.step(Arc::clone(&shared)).expect("step");
        let case = format!("pulled {pull}, {slots} slots, moved twice {twice}");
        assert!(!step_through(&mut run), "the pool runs dry, {case}");

        let position = held.lock().unwrap().last().and_then(|u| u.0).expect("held");
        let (byte, at) = index.restart_for(1, Timestamp(position)).expect("a keyframe");
        p.remove(hold, Leave::Cut).expect("remove");
        let sink = p.add(RecSink { seen: Arc::clone(&moved) });
        p.link(demux.src(pad), sink.sink(SinkPad(0))).expect("demux to sink");
        p.commit_at(Timestamp(position), &[(src, byte, at), (demux, byte, at)]).expect("commit");
        if twice {
            // The audio moves on again before the video read past what it put out.
            while p.changing() {
                run.service();
                if let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) {
                    run.pass(g).expect("pass");
                }
            }
            p.remove(sink, Leave::Cut).expect("remove");
            moved = Arc::default();
            let sink = p.add(RecSink { seen: Arc::clone(&moved) });
            p.link(demux.src(pad), sink.sink(SinkPad(0))).expect("demux to sink");
            let targets = [(src, byte, at), (demux, byte, at)];
            p.commit_at(Timestamp(position), &targets).expect("commit");
        }
        assert!(step_through(&mut run), "the run ends, {case}");
        let _ = run.finish();

        assert_eq!(*video.lock().unwrap(), video_all, "every picture once, {case}");
        let from = units.iter().position(|u| u.offset >= byte).expect("a unit behind");
        let moved = moved.lock().unwrap();
        let moved: Vec<_> = moved.iter().copied().filter(|u| u.1 != FLUSHED).collect();
        // A new run on a new sink, whether it starts at a break of the stream does not matter.
        let mut want = audio_all[from..].to_vec();
        want.iter_mut().take(1).for_each(|u| u.1 &= !BufferFlags::DISCONT.0);
        assert_eq!(moved, want, "on from the restart, {case}");
        restarts.push(byte);
    }
    (restarts, index.entries.first().expect("a keyframe").byte)
}

/// An audio sink that keeps its units dries the pool, and the demuxer parks the next unit it
/// read, of the audio or of the video. The audio moved to another sink with a restart starts at
/// the first unit behind the restart, not at a unit it parked ahead, and the video loses no
/// picture it parked or had in progress and gets none twice, also with the audio moved on again
/// right behind. Pushed and pulled, the restart reads from its byte, with no bisection. The pool
/// sizes park a unit of each.
#[test]
fn a_restart_drops_what_the_track_parked_and_the_other_track_keeps_its_own() {
    let file: Arc<[u8]> = Arc::from(common::fixture("mpeg2_mp2.vob"));
    let cases = [(false, 1), (false, 2), (false, 3), (false, 4), (true, 1), (true, 2)];
    restart(&file, 0xC000, 0, &cases);
    let Some(f) = film() else { return };
    let cases: Vec<(bool, u32)> =
        [false, true].into_iter().flat_map(|pull| (6..14).map(move |n| (pull, n))).collect();
    let (restarts, first) = restart(&f.bytes, 0xC000, 0, &cases);
    assert!(restarts.iter().all(|byte| *byte > first), "past the first GOP, {restarts:?}");
}

/// Two VOBs joined, the clock going back at the join, and the audio moved with a restart past
/// it. The video goes on on the line the join made, not on the file's.
#[test]
fn a_restart_past_a_clock_break_keeps_the_other_track_on_its_line() {
    let one = common::fixture("mpeg2_mp2.vob");
    let file: Arc<[u8]> = Arc::from([one.as_slice(), &one].concat());
    let (_, audio, _) = whole(&file);
    let (restarts, _) = restart(&file, 0xC000, audio.len() / 2 + 3, &[(false, 1), (true, 1)]);
    assert!(restarts.iter().all(|byte| *byte >= one.len() as u64), "past the join, {restarts:?}");
}
