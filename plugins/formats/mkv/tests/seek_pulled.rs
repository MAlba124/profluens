// Test code and a test rig, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use pf_mkv::ebml::id;
use pf_mkv::index::PRE_ROLL_BYTES;
use pf_mkv::{
    MatroskaReader, MatroskaWriter, MkvDemux, Parsed, TrackConfig, parse_cue_tracks,
    parse_seek_head,
};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Index;
use profluens_core::pipeline::{Pipeline, SeekMode};
use profluens_core::time::Timestamp;
use profluens_video::rap::{NalSyntax, length_size, random_access};

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

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
const OUT: SrcPad = SrcPad(0);

/// The source's place in what was seen, its asks and flushes.
const SOURCE: usize = usize::MAX;

struct MemSrc {
    file: Arc<[u8]>,
    out: Arc<Mutex<Vec<(usize, Seen)>>>,
}

impl Element for MemSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.offer_ranges(OUT);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(ask) = ctx.next_ask(OUT) {
            let profluens_core::pull::Ask { offset, len, tag, mut buf } = ask;
            self.out.lock().unwrap().push((SOURCE, Seen::Ask { offset, len }));
            let from = offset.min(self.file.len() as u64) as usize;
            let n = (len as usize).min(self.file.len() - from).min(buf.memory.capacity());
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[from..from + n]);
            buf.memory.set_len(n);
            ctx.serve(OUT, tag, buf);
        }
        Ok(if ctx.asks_over(OUT) { Flow::Eos } else { Flow::Ok })
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.out.lock().unwrap().push((SOURCE, Seen::Flush));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Frame { pts: Option<u64>, key: bool, len: usize },
    Flush,
    Eos,
    Ask { offset: u64, len: u32 },
}

struct RecSink {
    pad: usize,
    out: Arc<Mutex<Vec<(usize, Seen)>>>,
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            if buf.flags.contains(BufferFlags::HEADER) {
                continue;
            }
            let key = buf.flags.contains(BufferFlags::KEYFRAME);
            let seen = Seen::Frame { pts: buf.pts.nanos(), key, len: buf.memory.len() };
            self.out.lock().unwrap().push((self.pad, seen));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        let seen = match event {
            Event::FlushStart => Seen::Flush,
            Event::Eos => Seen::Eos,
            _ => return Ok(()),
        };
        self.out.lock().unwrap().push((self.pad, seen));
        Ok(())
    }
}

/// Whether to seek, by what the sinks saw so far.
type When = dyn Fn(&[(usize, Seen)]) -> bool;

/// What a run saw, in the order the sinks saw it.
struct Ran {
    seen: Vec<(usize, Seen)>,
    index: Option<Arc<Index>>,
}

impl Ran {
    /// The stamps a pad put out after the last flush.
    fn after_flush(&self, pad: usize) -> Vec<(u64, bool)> {
        let from = self.seen.iter().rposition(|(p, s)| *p == pad && *s == Seen::Flush);
        self.seen[from.map_or(0, |i| i + 1)..]
            .iter()
            .filter(|(p, _)| *p == pad)
            .filter_map(|(_, s)| match s {
                Seen::Frame { pts: Some(pts), key, .. } => Some((*pts, *key)),
                _ => None,
            })
            .collect()
    }
}

fn head_of(file: &[u8]) -> Vec<u8> {
    let cl = file.windows(4).position(|w| w == id::CLUSTER).unwrap_or(file.len());
    file[..cl].to_vec()
}

/// The head parsed with its Cues as an open hands them over, none when the file has none.
fn parsed(file: &[u8]) -> Parsed {
    let head = head_of(file);
    let mut p = Parsed::of(&head).expect("a head");
    let info = parse_seek_head(&head).expect("a Segment");
    if let Some(at) = info.cues_pos.map(|c| c + info.segment_data_start) {
        let cues =
            parse_cue_tracks(&file[at as usize..], info.segment_data_start, info.timestamp_scale);
        p.cues = Arc::from(cues);
        p.cued_at = Some(at);
    }
    p
}

/// Plays `file` pulled with only the pads `link` names linked, and once `when` holds of what
/// was seen, seeks to `seek`.
fn play(
    file: &[u8],
    link: &dyn Fn(usize) -> bool,
    seek: Option<(Timestamp, SeekMode)>,
    when: &When,
) -> Ran {
    let seeks: Vec<(Timestamp, SeekMode, &When)> =
        seek.into_iter().map(|(to, mode)| (to, mode, when)).collect();
    play_seeks(file, link, &seeks)
}

/// [`play`] with a seek for each condition in turn.
fn play_seeks(
    file: &[u8],
    link: &dyn Fn(usize) -> bool,
    seeks: &[(Timestamp, SeekMode, &When)],
) -> Ran {
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    let out = Arc::new(Mutex::new(Vec::new()));
    let src = p.add(MemSrc { file: Arc::from(file), out: Arc::clone(&out) });
    let demux = p.add(MkvDemux::with_parsed(Arc::new(parsed(file))).without_extras());
    p.set_element_pool(src, 1 << 20, 8);
    p.set_element_pool_bytes(demux, 128 << 10, 1024, 64 << 20);
    p.link(src.src(OUT), demux.sink(MkvDemux::IN)).unwrap();
    let added = p.preroll().unwrap();
    for (n, a) in added.srcs.iter().enumerate().filter(|(n, _)| link(*n)) {
        let sink = p.add(RecSink { pad: n, out: Arc::clone(&out) });
        p.link(demux.src(a.pad), sink.sink(SinkPad(0))).unwrap();
    }
    let mut run = p.step(Arc::clone(&shared)).unwrap();
    let control = p.control();
    let mut next = 0;
    for passes in 0u64.. {
        assert!(passes < 20_000_000, "the run did not end");
        if let Some((to, mode, when)) = seeks.get(next)
            && when(&out.lock().unwrap())
        {
            control.seek(*to, *mode).unwrap();
            next += 1;
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
        run.pass(ready[passes as usize % ready.len()]).expect("a pass");
    }
    let _ = run.finish();
    let index = p.index(demux);
    while let Some(m) = p.bus().try_recv() {
        if let BusMessage::Error { error, .. } = m {
            panic!("an error on the bus, {error}");
        }
    }
    drop(p);
    let seen = Arc::try_unwrap(out).unwrap().into_inner().unwrap();
    Ran { seen, index }
}

fn ffmpeg(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mkv_restart");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(name);
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .current_dir(&dir)
        .args(["-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: ffmpeg could not make {name}");
        return None;
    }
    std::fs::read(&path).ok()
}

/// Every frame of `track` in file order as `(pts, keyframe flag, bytes)`.
fn frames_of(file: &[u8], track: u64) -> Vec<(u64, bool, Vec<u8>)> {
    let mut r = MatroskaReader::new();
    let mut out = Vec::new();
    for piece in file.chunks(64 << 10) {
        r.push(piece).expect("a clean file");
        while let Some(f) = r.next_frame() {
            if f.track_number == track {
                out.push((f.pts_ns, f.keyframe, f.data.clone()));
            }
        }
    }
    out
}

/// The x265 film remuxed with the container's keyframe flags set by `flag` of each picture's
/// index and whether its NAL types make it a random access point, cued at every flagged one.
fn reflagged(
    film: &[u8],
    cues: bool,
    flag: &dyn Fn(usize, bool) -> bool,
) -> (Vec<u8>, Vec<(u64, bool)>) {
    let p = parsed(film);
    let t = &p.tracks[0];
    let len = length_size(NalSyntax::Hevc, &t.codec_private);
    let config = TrackConfig::video(1, "V_MPEGH/ISO/HEVC", t.codec_private.clone(), 96, 64);
    let mut w = MatroskaWriter::new(vec![config]);
    if cues {
        w.enable_cues();
    }
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let mut pictures = Vec::new();
    for (i, (pts, _, data)) in frames_of(film, t.track_number).into_iter().enumerate() {
        let rap = random_access(NalSyntax::Hevc, len, &data).expect("readable");
        w.write_frame(&mut out, 1, pts, &data, flag(i, rap)).unwrap();
        pictures.push((pts, rap));
    }
    w.finalize(&mut out);
    (sized(out), pictures)
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

fn open_gop_film() -> Option<Vec<u8>> {
    ffmpeg(
        "open_gop.mkv",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=96x64:rate=24:d=6",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "open-gop=1:keyint=24:min-keyint=24:scenecut=0:log-level=error",
        ],
    )
}

/// The picture half way between the random access points at 2 s and 3 s carries a false
/// keyframe flag and a cue, the one at 4 s none.
fn false_keyed(cues: bool) -> Option<(Vec<u8>, u64, u64)> {
    let film = open_gop_film()?;
    let probe = reflagged(&film, cues, &|_, rap| rap).1;
    let mut order: Vec<usize> = (0..probe.len()).collect();
    order.sort_by_key(|&i| probe[i].0);
    let trail = order.iter().copied().find(|&i| probe[i].0 >= 2_500 * MS).expect("a picture");
    assert!(!probe[trail].1, "a trailing picture");
    let unflagged = probe.iter().position(|&(pts, rap)| rap && pts == 4 * S);
    let (file, _) = reflagged(&film, cues, &|i, rap| (rap && Some(i) != unflagged) || i == trail);
    Some((file, probe[trail].0, 2 * S))
}

#[test]
fn a_trailing_picture_flagged_key_goes_out_as_a_delta_and_an_unflagged_cra_as_a_key() {
    let Some((file, trail, _)) = false_keyed(true) else { return };
    let ran = play(&file, &|_| true, None, &|_| false);
    let out = ran.after_flush(0);
    let key = |t: u64| out.iter().find(|f| f.0 == t).map(|f| f.1);
    assert_eq!(key(trail), Some(false), "the false keyframe at {trail}");
    assert_eq!(key(4 * S), Some(true), "the unflagged CRA");
    assert_eq!(key(0), Some(true));
    assert_eq!(out.iter().filter(|f| f.1).count(), 6, "a key a second");
}

#[test]
fn an_exact_seek_past_a_false_key_decodes_from_the_random_access_point_before_it() {
    let Some((file, trail, cra)) = false_keyed(true) else { return };
    let cued = parsed(&file).cues.iter().any(|c| c.time_ns == trail);
    assert!(cued, "the false keyframe is cued");
    let target = Timestamp(trail + 100 * MS);
    let ran = play(&file, &|_| true, Some((target, SeekMode::Accurate)), &|_| true);
    let out = ran.after_flush(0);
    assert_eq!(out.first(), Some(&(cra, true)), "the CRA before the false keyframe first");
    assert!(out.iter().any(|f| f.0 == trail && !f.1), "the false keyframe as a delta");
    assert!(ran.index.as_ref().is_some_and(|i| i.entries.iter().all(|e| e.time_ns != trail)));
}

#[test]
fn a_grown_index_takes_random_access_points_by_their_nal_types() {
    let Some((file, trail, _)) = false_keyed(false) else { return };
    assert!(parsed(&file).cues.is_empty());
    let ran = play(&file, &|_| true, None, &|_| false);
    let index = ran.index.expect("a grown index");
    let times: Vec<u64> = index.entries.iter().map(|e| e.time_ns).collect();
    assert!(!times.contains(&trail), "no false keyframe in {times:?}");
    assert!(times.contains(&(3 * S)) && times.contains(&(5 * S)), "the CRAs in {times:?}");
}

/// 10 s of pictures, a keyframe each 2 s, beside 120 s of sound.
fn short_video() -> Option<Vec<u8>> {
    ffmpeg(
        "short_video.mkv",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=96x64:rate=25:d=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:d=120",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "50",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            // else the encoder's last delayed pictures go 10 s before the end of the file
            "-max_interleave_delta",
            "0",
        ],
    )
}

/// Where in what the sinks saw pad `pad` ended, and the first frame of `of` stamped `at` or
/// later.
fn eos_at(ran: &Ran, pad: usize) -> Option<usize> {
    ran.seen.iter().rposition(|(p, s)| *p == pad && *s == Seen::Eos)
}

fn first_from(ran: &Ran, pad: usize, at: u64) -> Option<usize> {
    ran.seen.iter().position(|(p, s)| {
        *p == pad && matches!(s, Seen::Frame { pts: Some(pts), .. } if *pts >= at)
    })
}

#[test]
fn a_track_that_ends_early_ends_its_pad_while_the_others_play_on() {
    let Some(file) = short_video() else { return };
    let ran = play(&file, &|_| true, None, &|_| false);
    let video_end = eos_at(&ran, 0).expect("the video ended");
    // ended 30 s of stream past its last picture, the sinks see it some passes later
    let sound = first_from(&ran, 1, 90 * S).expect("sound at 90 s");
    assert!(video_end < sound, "the video ended long before the end of the file");
    let video = ran.after_flush(0);
    assert!(video.last().is_some_and(|f| f.0 >= 9_900 * MS), "every picture went out");
    let last = ran.after_flush(1).last().map(|f| f.0).unwrap_or(0);
    assert!(last >= 119 * S, "the sound played on to {last}");
}

#[test]
fn a_seek_past_a_tracks_end_ends_its_pad_before_the_target() {
    let Some(file) = short_video() else { return };
    let ran = play(&file, &|_| true, Some((Timestamp(60 * S), SeekMode::Accurate)), &|_| true);
    let video_end = eos_at(&ran, 0).expect("the video ended");
    let sound = first_from(&ran, 1, 60 * S).expect("sound from the target");
    assert!(video_end < sound, "the video ended before the sound at the target went out");
    assert!(ran.after_flush(1).len() > 1000, "the sound flows");
}

#[test]
fn a_seek_back_after_a_track_ended_plays_it_again() {
    let Some(file) = short_video() else { return };
    let ended: &When = &|seen| seen.iter().any(|(p, s)| *p == 0 && *s == Seen::Eos);
    let ran = play_seeks(&file, &|_| true, &[(Timestamp(5 * S), SeekMode::Accurate, ended)]);
    let video = ran.after_flush(0);
    assert_eq!(video.first(), Some(&(4 * S, true)), "the video again from its keyframe");
    assert!(video.last().is_some_and(|f| f.0 >= 9_900 * MS), "to its end");
    assert!(ran.after_flush(1).last().is_some_and(|f| f.0 >= 119 * S));
}

/// 10 s of 32 KiB pictures, a keyframe each 2 s, beside 20 ms Opus packets, so a group is far
/// over what a long pre-roll may step back.
fn wide_groups() -> Vec<u8> {
    const OPUS_HEAD: &[u8; 19] = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00";
    let mut opus = TrackConfig::audio(2, "A_OPUS", OPUS_HEAD.to_vec(), 48_000.0, 2, 0);
    opus.seek_pre_roll_ns = 80 * MS;
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240), opus]);
    w.enable_cues();
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let picture = vec![0x5A; 32 << 10];
    for ms in (0..10_000u64).step_by(20) {
        if ms % 40 == 0 {
            w.write_frame(&mut out, 1, ms * MS, &picture, ms % 2_000 == 0).unwrap();
        }
        w.write_frame(&mut out, 2, ms * MS, &[0xFC, (ms / 20) as u8], true).unwrap();
    }
    w.finalize(&mut out);
    out
}

/// A seek beside Opus reads from the keyframe's Cluster, not the group before it, and the
/// sound starts at the picture.
#[test]
fn a_seek_beside_opus_reads_from_the_keyframes_cluster() {
    let file = wide_groups();
    let playing: &When = &|seen| seen.iter().any(|(p, s)| *p == 0 && *s != Seen::Flush);
    let ran = play(&file, &|_| true, Some((Timestamp(5_300 * MS), SeekMode::Keyframe)), playing);
    let &(key, flagged) = ran.after_flush(0).first().expect("pictures after the seek");
    assert!(flagged, "a keyframe first");
    let index = ran.index.as_ref().expect("an index");
    let at = index.entries.iter().position(|e| e.time_ns == key).expect("the keyframe's entry");
    let (e, before) = (index.entries[at], index.entries[at.checked_sub(1).expect("one before")]);
    assert!(e.byte - before.byte > PRE_ROLL_BYTES, "a group over the cap");
    assert_eq!((e.restart, e.restart_ns), (e.byte, e.time_ns), "no step back");

    let last = |pad: usize| ran.seen.iter().rposition(|(p, s)| *p == pad && *s == Seen::Flush);
    let flushed = last(SOURCE).expect("the source flushed");
    let video = last(0).expect("the video flushed");
    let picture = video
        + ran.seen[video..]
            .iter()
            .position(|(p, s)| *p == 0 && matches!(s, Seen::Frame { .. }))
            .expect("a picture");
    let asks: Vec<(u64, u32)> = ran.seen[flushed..picture]
        .iter()
        .filter_map(|(_, s)| match s {
            Seen::Ask { offset, len } => Some((*offset, *len)),
            _ => None,
        })
        .collect();
    assert!(!asks.is_empty(), "the seek read");
    assert!(asks.iter().all(|a| a.0 >= e.byte), "nothing of the group before, {asks:?}");

    let sound = ran.after_flush(1).first().expect("sound after the seek").0;
    assert!((key..=key + 20 * MS).contains(&sound), "sound at {sound}, the picture at {key}");
}
