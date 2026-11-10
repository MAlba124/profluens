//! The grown seek index against `ffprobe` keyframes of a stream ffmpeg writes, skipped without it.

#![allow(clippy::disallowed_methods)] // test fixtures

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pf_tsdemux::TsDemux;
use pf_tsdemux::index::GROWN_EVERY;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::clock::MockClock;
use profluens_core::counters::TapHandle;
use profluens_core::ctx::{Ctx, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::{Index, Pipeline, Reach, SeekMode, SeekPlan};
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

const VIDEO_PID: u16 = 0x100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    byte: u64,
    ns: u64,
}

struct Stream {
    path: PathBuf,
    bytes: Vec<u8>,
    keys: Vec<Key>,
}

fn ns(ticks: i64, origin: i64) -> u64 {
    (i128::from(ticks) * 100_000 / 9 - i128::from(origin) * 100_000 / 9).max(0) as u64
}

fn stream() -> Option<&'static Stream> {
    static STREAM: OnceLock<Option<Stream>> = OnceLock::new();
    STREAM.get_or_init(make).as_ref()
}

fn make() -> Option<Stream> {
    let dir = std::env::temp_dir().join(format!("pf-tsdemux-index-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("keys.ts");
    let made = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=size=64x64:rate=25:duration=24"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=24"])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p"])
        .args(["-g", "4", "-keyint_min", "4", "-sc_threshold", "0", "-bf", "0"])
        // Constant rate, so every window of the rate test reads the file average.
        .args(["-c:a", "aac", "-b:a", "32k", "-muxrate", "400k", "-f", "mpegts"])
        .arg(&path)
        .status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("skipped, no ffmpeg with libx264 and aac");
        return None;
    }
    let probe = Command::new("ffprobe")
        .env_remove("LD_LIBRARY_PATH")
        .args([
            "-v",
            "error",
            "-show_entries",
            "packet=stream_index,pts,dts,pos,flags",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(probe.stdout).ok()?;
    let rows: Vec<(u32, i64, i64, u64, bool)> = text
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            let int = |n: usize| f.get(n).and_then(|s| s.parse::<i64>().ok());
            Some((int(0)? as u32, int(1)?, int(2)?, int(3)? as u64, f.get(4)?.starts_with('K')))
        })
        .collect();
    let first = |stream: u32| rows.iter().find(|r| r.0 == stream).map(|r| r.2);
    let origin = (0..2).filter_map(first).min()?;
    let keys = rows
        .iter()
        .filter(|r| r.0 == 0 && r.4)
        .map(|r| Key { byte: r.3, ns: ns(r.1, origin) })
        .collect();
    let bytes = std::fs::read(&path).ok()?;
    Some(Stream { path, bytes, keys })
}

static ANY: ConstraintDesc = ConstraintDesc::Any;
static AAC_FIELDS: [FieldDesc; 2] = [
    FieldDesc { field: "rate", allowed: ANY, preferred: None },
    FieldDesc { field: "channels", allowed: ANY, preferred: None },
];
static OFFERS: [OfferDesc; 2] =
    [OfferDesc::any("h264/annexb"), OfferDesc { family: "aac", fields: &AAC_FIELDS }];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static DESC: ElementDesc = ElementDesc {
    name: "recsink",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct Seen {
    units: Vec<(Option<u64>, u32)>,
    published: Vec<Arc<Index>>,
}

struct Rec {
    seen: Arc<Mutex<Seen>>,
    tap: Option<(TapHandle, ElementId)>,
    pace: Duration,
}

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let mut seen = self.seen.lock().unwrap();
            seen.units.push((buf.pts.nanos(), buf.flags.0));
            if let Some(index) = self.tap.as_ref().and_then(|(tap, el)| tap.index(*el))
                && !seen.published.last().is_some_and(|last| Arc::ptr_eq(last, &index))
            {
                seen.published.push(index);
            }
            drop(seen);
            std::thread::sleep(self.pace);
        }
        Ok(Flow::Ok)
    }
}

fn run(s: &Stream, pace: Duration, seeks: &[(u64, Timestamp)]) -> (Seen, Option<Arc<Index>>) {
    let mut p = pf_reactor::pipeline();
    p.set_pool(32 * 1024, 32);
    let src = p.add(FileSrc::new(&s.path));
    let head = s.bytes[..s.bytes.len().min(64 * 1024)].to_vec();
    let demux = p.add(TsDemux::new(head));
    p.link(src.src(FileSrc::OUT), demux.sink(TsDemux::IN)).unwrap();
    p.set_select_all(true);
    let added = p.preroll().unwrap().srcs;
    let kinds: Vec<StreamKind> = p.streams(demux).iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [StreamKind::Video, StreamKind::Audio]);
    let seen = Arc::new(Mutex::new(Seen::default()));
    for (n, pad) in added.iter().enumerate() {
        let rec = match n {
            0 => Rec { seen: Arc::clone(&seen), tap: Some((p.tap_handle(), demux)), pace },
            _ => {
                Rec { seen: Arc::new(Mutex::new(Seen::default())), tap: None, pace: Duration::ZERO }
            }
        };
        let sink = p.add(rec);
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).unwrap();
    }
    let (control, tap) = (p.control(), p.tap_handle());
    let run = std::thread::spawn(move || p.run());
    for &(byte, at) in seeks {
        let before = seen.lock().unwrap().units.len();
        let mut waited = 0;
        while seen.lock().unwrap().units.len() < before + 8 && waited < 4_000 {
            std::thread::sleep(Duration::from_millis(1));
            waited += 1;
        }
        let _ = control.seek_bytes(at, &[(profluens_core::command::ALL, byte, at)]);
    }
    run.join().unwrap().unwrap();
    let seen = std::mem::take(&mut *seen.lock().unwrap());
    (seen, tap.index(demux))
}

fn keyframe_at(bytes: &[u8], byte: u64) -> bool {
    let Some(p) = bytes.get(byte as usize..byte as usize + 188) else { return false };
    let pid = u16::from(p[1] & 0x1F) << 8 | u16::from(p[2]);
    let (start, field) = (p[1] & 0x40 != 0, (p[3] >> 4) & 0x03);
    let adaptation = matches!(field, 2 | 3) && p[4] > 0;
    let random_access = adaptation && p[5] & 0x40 != 0;
    let payload = &p[if adaptation || field == 2 { 5 + usize::from(p[4]) } else { 4 }..];
    let idr = payload.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1F == 5);
    p[0] == 0x47 && pid == VIDEO_PID && start && (random_access || idr)
}

fn check(s: &Stream, index: &Index, seen: &Seen, stamped: bool) {
    assert_eq!((index.duration, index.stream, index.grouped), (Timestamp::NONE, Some(0), true));
    let keys: std::collections::HashMap<u64, u64> = s.keys.iter().map(|k| (k.byte, k.ns)).collect();
    for e in &index.entries {
        assert_eq!(keys.get(&e.byte), Some(&e.time_ns), "{e:?} is no keyframe ffprobe lists");
        assert!(keyframe_at(&s.bytes, e.byte), "{e:?}");
        assert_eq!((e.restart, e.restart_ns), (e.byte, e.time_ns));
        let left =
            seen.units.iter().any(|u| u.0 == Some(e.time_ns) && u.1 & BufferFlags::KEYFRAME.0 != 0);
        assert!(left || !stamped, "{e:?} is no stamp a keyframe left with");
    }
    assert!(index.entries.windows(2).all(|w| w[0].time_ns < w[1].time_ns && w[0].byte < w[1].byte));
}

#[test]
fn every_keyframe_read_is_an_entry_published_every_64() {
    let Some(s) = stream() else { return };
    assert!(s.keys.len() >= 140, "{} keyframes", s.keys.len());
    let (seen, index) = run(s, Duration::ZERO, &[]);
    let index = index.expect("published");
    check(s, &index, &seen, true);
    assert!(index.complete, "read whole from the start");
    assert!(seen.published.iter().all(|i| !i.complete || i.entries.len() == s.keys.len()));
    let got: Vec<Key> = index.entries.iter().map(|e| Key { byte: e.byte, ns: e.time_ns }).collect();
    assert_eq!(got, s.keys, "every keyframe, in order");
    let mut lens: Vec<usize> = seen.published.iter().map(|i| i.entries.len()).collect();
    let every: Vec<usize> =
        (1..=s.keys.len() / 64).map(|n| n * 64).filter(|n| *n < s.keys.len()).collect();
    if lens.last() == Some(&s.keys.len()) {
        lens.pop();
    }
    assert_eq!(lens, every);
    for published in &seen.published {
        assert_eq!(
            published.entries[..],
            index.entries[..published.entries.len()],
            "a publication is what was read"
        );
    }
}

#[test]
fn behind_a_seek_bytes_count_from_its_target_and_nothing_doubles() {
    let Some(s) = stream() else { return };
    let k = &s.keys;
    // Into keyframe 100's packet a little past its start, then back to keyframe 10's own byte.
    let seeks = [(k[100].byte + 37, Timestamp(k[100].ns)), (k[10].byte, Timestamp(k[10].ns))];
    let (seen, index) = run(s, Duration::from_millis(1), &seeks);
    let index = index.expect("published");
    check(s, &index, &seen, false);
    assert!(!index.complete, "a stretch was skipped");
    let got: std::collections::HashSet<Key> =
        index.entries.iter().map(|e| Key { byte: e.byte, ns: e.time_ns }).collect();
    assert!(k[10..].iter().all(|key| got.contains(key)), "{} of {}", got.len(), k.len());
    assert_eq!(got.len(), index.entries.len());
    // The second flush published what was read behind the first, none of the stretch jumped over.
    let jumped = seen.published.iter().find(|i| {
        let past = i.entries.iter().any(|e| e.byte > k[100].byte);
        past && !i.entries.iter().any(|e| e.byte == k[50].byte)
    });
    let lens: Vec<usize> = seen.published.iter().map(|i| i.entries.len()).collect();
    let jumped = jumped.unwrap_or_else(|| panic!("no publication between the seeks in {lens:?}"));
    check(s, jumped, &seen, false);
}

fn pass(run: &mut Stepped) -> bool {
    run.service();
    let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
    for g in &ready {
        run.pass(*g).expect("pass");
    }
    !ready.is_empty()
}

/// The keyframe ffprobe lists at or before `ns`.
fn floor(s: &Stream, ns: u64) -> Key {
    *s.keys.iter().rev().find(|k| k.ns <= ns).expect("a keyframe before")
}

/// Past the last entry published the keyframe is unknown, a link there is told so and a seek
/// there is estimated from the target. Once the end is read the index is whole and names it.
#[test]
fn past_the_published_entries_the_keyframe_is_unknown_until_read() {
    let Some(s) = stream() else { return };
    assert!(s.keys.len() > 2 * GROWN_EVERY, "{} keyframes", s.keys.len());
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
    p.set_pool(32 * 1024, 32);
    let src = p.add(Told { data: s.bytes.clone(), pos: 0, rates: Arc::default() });
    let head = s.bytes[..s.bytes.len().min(64 * 1024)].to_vec();
    let demux = TsDemux::new(head).with_len(s.bytes.len() as u64).with_duration(24_000_000_000);
    let demux = p.add(demux);
    p.link(src.src(SrcPad(0)), demux.sink(TsDemux::IN)).unwrap();
    p.set_select_all(true);
    for pad in p.preroll().unwrap().srcs {
        let sink = p.add(Rec { seen: Arc::default(), tap: None, pace: Duration::ZERO });
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).unwrap();
    }
    let (tap, control) = (p.tap_handle(), p.control());
    let mut run = p.step(Arc::new(clock)).unwrap();
    let grown = loop {
        if let Some(ix) = tap.index(demux).filter(|ix| !ix.entries.is_empty()) {
            break ix;
        }
        assert!(pass(&mut run), "the run ends before the index is published");
    };
    assert_eq!((grown.entries.len(), grown.complete), (GROWN_EVERY, false));
    let last = *grown.entries.last().unwrap();
    let ahead = s.keys[GROWN_EVERY + 40];
    let target = Timestamp(ahead.ns + 10_000_000);
    assert_eq!(grown.restart_for(0, target), grown.resolve(target), "resolved as a seek there");
    assert_eq!(grown.keyframe(target), None);
    let key = |ix: &Index| ix.plan(target, SeekMode::Keyframe, Timestamp::NONE, None, Reach::Group);
    let SeekPlan::Seek { to_byte, key: at, to_time } = key(&grown) else { panic!("a seek") };
    assert_eq!((at, to_time), (target, target), "estimated, not the last entry");
    assert!(to_byte > last.byte && to_byte < s.bytes.len() as u64, "{to_byte}");
    assert_eq!(control.plan_from(Timestamp::NONE, target, SeekMode::Keyframe), key(&grown));
    let timeless = Index { duration: Timestamp::NONE, ..(*grown).clone() };
    let from_entry = SeekPlan::Seek { to_byte: last.byte, key: Timestamp(last.time_ns), to_time };
    assert_eq!(key(&timeless), from_entry, "no estimate, read on from the entry");
    let within = Timestamp(s.keys[GROWN_EVERY / 2].ns);
    let half = Some((s.keys[GROWN_EVERY / 2].byte, within));
    assert_eq!(grown.restart_for(0, within), half, "inside what was published");

    while pass(&mut run) {}
    assert!((0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit)), "the run ends");
    run.finish().unwrap();
    let whole = tap.index(demux).expect("published");
    assert_eq!((whole.entries.len(), whole.complete), (s.keys.len(), true));
    let real = Some((ahead.byte, Timestamp(ahead.ns)));
    assert_eq!(whole.restart_for(0, target), real);
    let to = Timestamp(ahead.ns);
    assert_eq!(key(&whole), SeekPlan::Seek { to_byte: ahead.byte, key: to, to_time: to });
    let end = s.keys.last().unwrap();
    let past = Timestamp(end.ns + 50_000_000);
    assert_eq!(floor(s, past.0), *end);
    assert_eq!(whole.restart_for(0, past), Some((end.byte, Timestamp(end.ns))), "the last");
}

static TOLD_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static TOLD_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &TOLD_OFFERS }];
static TOLD: ElementDesc = ElementDesc {
    name: "told",
    sinks: &[],
    srcs: &TOLD_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Told {
    data: Vec<u8>,
    pos: usize,
    rates: Arc<Mutex<Vec<profluens_core::feedback::Bitrate>>>,
}

impl Element for Told {
    fn desc(&self) -> &'static ElementDesc {
        &TOLD
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let n = 4096.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(profluens_core::id::SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &profluens_core::event::Event) -> Result<(), Error> {
        use profluens_core::feedback::Feedback;
        if let profluens_core::event::Event::Feedback { feedback: Feedback::Bitrate(b), .. } = event
        {
            self.rates.lock().unwrap().push(*b);
        }
        Ok(())
    }
}

#[test]
fn the_rate_is_measured_off_the_key_streams_units() {
    let Some(s) = stream() else { return };
    let rates = Arc::new(Mutex::new(Vec::new()));
    let mut p = pf_reactor::pipeline();
    p.set_pool(32 * 1024, 32);
    let src = p.add(Told { data: s.bytes.clone(), pos: 0, rates: Arc::clone(&rates) });
    let demux = p.add(TsDemux::new(s.bytes[..s.bytes.len().min(64 * 1024)].to_vec()));
    p.link(src.src(profluens_core::id::SrcPad(0)), demux.sink(TsDemux::IN)).unwrap();
    p.set_select_all(true);
    for pad in p.preroll().unwrap().srcs {
        let sink = p.add(Rec { seen: Arc::default(), tap: None, pace: Duration::ZERO });
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).unwrap();
    }
    p.run().unwrap();
    let rates = rates.lock().unwrap().clone();
    let want = s.bytes.len() as u64 * 8 / 24;
    assert!(rates.len() >= 4, "a rate every two seconds, {rates:?}");
    let off = |b: &profluens_core::feedback::Bitrate| b.bits_per_second.abs_diff(want) * 50 > want;
    assert!(!rates.iter().any(off), "{want} b/s, {rates:?}");
}
