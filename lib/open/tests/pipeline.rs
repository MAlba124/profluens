#![cfg(feature = "pipeline")]
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::expect_used
)]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pf_libav::LibavAudioDec;
use pf_libflac::{FlacDec, FlacEncoder, SampleFormat};
use pf_open::pipeline::{BuildError, DEMUX_BYTES, Out, demuxer, route};
use pf_open::{Caps, Kind, OpenAt, Opened, TrackKind, Want};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::fetch::drive_file_log;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::{Index, Pipeline, SeekMode, SeekPlan, Seekable};
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static DESC: ElementDesc = ElementDesc {
    name: "count",
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

/// Counts the buffers a track sends.
struct Count(Arc<AtomicU64>);

impl Element for Count {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }
}

fn path_of(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

/// `rel` opened through its file as a source opens it.
fn opened(rel: &str) -> Opened {
    opened_at(&path_of(rel))
}

fn opened_at(path: &Path) -> Opened {
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len();
    let mut log = Vec::new();
    drive_file_log(&mut OpenAt::new(len, Want::ALL, Caps::default()), &f, &mut log).unwrap()
}

/// Every audio and video track of `rel` routed to a counter and the file played to its end.
fn plays(rel: &str, kind: Kind) {
    plays_at(&path_of(rel), kind);
}

fn plays_at(path: &Path, kind: Kind) {
    let rel = path.display();
    let o = opened_at(path);
    assert_eq!(o.kind, kind, "{rel}");
    let mut p = pf_reactor::pipeline();
    p.set_select_all(false);
    let path = path.to_path_buf();
    let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap();
    p.preroll().unwrap();
    let mut counts = Vec::new();
    for t in o.tracks.iter().filter(|t| matches!(t.kind, TrackKind::Audio | TrackKind::Video)) {
        let out =
            route(&p, demux, &o, t).unwrap_or_else(|| panic!("{rel} track {} unrouted", t.id));
        let pad = out.pad(&mut p, demux).unwrap();
        let n = Arc::new(AtomicU64::new(0));
        let sink = p.add(Count(Arc::clone(&n)));
        p.link(demux.src(pad), sink.sink(SinkPad(0))).unwrap();
        counts.push((t.id, n));
    }
    assert!(!counts.is_empty(), "{rel}");
    p.run().unwrap();
    for (id, n) in counts {
        assert!(n.load(Ordering::Relaxed) > 0, "{rel} track {id} sent nothing");
    }
}

#[test]
fn every_container_plays_its_tracks_through_the_routes() {
    plays("plugins/formats/mp4/tests/fixtures/cues_last.mkv", Kind::Mkv);
    plays("lib/open/tests/fixtures/tone.webm", Kind::Mkv);
    plays("plugins/formats/mp4/tests/fixtures/tiny_av.mp4", Kind::Mp4);
    plays("plugins/formats/mp4/tests/fixtures/moov_last.mp4", Kind::Mp4);
    plays("plugins/formats/avi/tests/fixtures/tiny.avi", Kind::Avi);
    plays("lib/open/tests/fixtures/tone.opus", Kind::Ogg);
    plays("plugins/formats/flv/tests/fixtures/h264_aac.flv", Kind::Flv);
    plays("plugins/formats/bare/tests/fixtures/tone.flac", Kind::Flac);
    plays("plugins/formats/bare/tests/fixtures/tone.mp3", Kind::Mp3);
    plays("plugins/formats/bare/tests/fixtures/tone.aac", Kind::AdtsAac);
    plays("plugins/formats/ts/tests/fixtures/h264_aac.ts", Kind::Ts);
    plays("plugins/formats/ts/tests/fixtures/hevc_eac3_dvb.ts", Kind::Ts);
    plays("plugins/formats/tsdemux/tests/fixtures/dvbsub.ts", Kind::Ts);
    plays("plugins/formats/ps/tests/fixtures/mpeg2_ac3.vob", Kind::Ps);
    plays("plugins/formats/ps/tests/fixtures/lpcm16.vob", Kind::Ps);
    plays("plugins/formats/ps/tests/fixtures/mpeg1_mp2.mpg", Kind::Ps);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let made = |name: &str, bytes: Vec<u8>| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    };
    use pf_bare::fixture::{ac3_frame, dts_frame, eac3_frame, wav_file};
    let ac3 = made("plays.ac3", (0..200).flat_map(|_| ac3_frame(0, 192, false, 2)).collect());
    plays_at(&ac3, Kind::Ac3);
    let eac3 = made("plays.eac3", (0..200).flat_map(|_| eac3_frame(512, 0, 0)).collect());
    plays_at(&eac3, Kind::Eac3);
    plays_at(&made("plays.dts", (0..200).flat_map(|_| dts_frame(1006)).collect()), Kind::Dts);
    plays_at(&made("plays.wav", wav_file(48_000, 2, 16, false, 96_000, None, 0)), Kind::Wav);
    let corpus = path_of("fixtures/out");
    for (name, kind) in [
        ("ts_h264_aac.ts", Kind::Ts),
        ("ts_h265_ac3.ts", Kind::Ts),
        ("ps_mpeg1_mp2.mpg", Kind::Ps),
        ("ps_mpeg2_ac3.vob", Kind::Ps),
        ("audio.wav", Kind::Wav),
        ("audio.ac3", Kind::Ac3),
        ("audio.eac3", Kind::Eac3),
        ("audio.dts", Kind::Dts),
    ] {
        match corpus.join(name).exists() {
            true => plays_at(&corpus.join(name), kind),
            false => eprintln!("{name} not generated, skipped"),
        }
    }
}

/// A transport or program stream seeks ahead of what was read, the transport stream on the
/// duration the open found, the program stream by bisecting its packs, and lands on a key near
/// the target.
#[test]
fn a_transport_or_program_stream_seeks_where_nothing_was_read() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    for (name, bytes) in
        [("seek.ts", common::ts_file(30, 900_000)), ("seek.vob", common::ps_file(30, true, true))]
    {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        let o = opened_at(&path);
        let mut p = pf_reactor::pipeline();
        p.set_select_all(false);
        let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap();
        p.preroll().unwrap();
        let v = o.tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap();
        let pad = route(&p, demux, &o, v).unwrap().pad(&mut p, demux).unwrap();
        let got = Arc::new(Mutex::new(Pcm::default()));
        let ((first_tx, first_rx), (go_tx, go_rx)) = (channel(), channel());
        let sink = p.add(Collect { got: Arc::clone(&got), gate: Some((first_tx, go_rx)) });
        p.link(demux.src(pad), sink.sink(SinkPad(0))).unwrap();
        let ctl = p.control();
        let run = std::thread::spawn(move || {
            let r = p.run();
            (p, r)
        });
        first_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let to = Timestamp(20_000_000_000);
        let mut plan = None;
        for _ in 0..10_000 {
            if ctl.seekable() != Seekable::Unknown
                && ctl.plan_from(Timestamp::NONE, to, SeekMode::Keyframe) != SeekPlan::Deferred
            {
                plan = Some(ctl.seek_from(Timestamp::NONE, to, SeekMode::Keyframe).unwrap());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        go_tx.send(()).unwrap();
        let (p, r) = run.join().unwrap();
        r.unwrap();
        assert!(matches!(plan, Some(SeekPlan::Seek { .. })), "{name} {plan:?}");
        let ix = p.index(demux).unwrap();
        assert_eq!(ix.duration, Timestamp(o.duration_ns), "{name}");
        let g = got.lock().unwrap();
        let first = g.bufs[g.flushed..].iter().find(|b| b.0 != Timestamp::NONE).unwrap().0;
        // psdemux bisects to 5 s ahead of the target, the mux delay and a few GOPs back
        let back = if o.kind == Kind::Ps { 5_000_000_000 } else { 0 };
        let near = (to.0 - back).saturating_sub(1_000_000_000)..to.0 + 1_000_000_000;
        assert!(near.contains(&first.0), "{name} lands at {first:?}");
        let left = (o.duration_ns - first.0) / 40_000_000 + 2;
        assert!((g.bufs.len() - g.flushed) as u64 <= left, "{name} read on from where it landed");
    }
}

/// The Matroska demuxer publishes the Cues the open read, the AVI one the idx1 index.
#[test]
fn the_index_read_at_open_is_published() {
    for rel in [
        "plugins/formats/mp4/tests/fixtures/cues_last.mkv",
        "plugins/formats/avi/tests/fixtures/tiny.avi",
    ] {
        let o = opened(rel);
        let mut p = pf_reactor::pipeline();
        let path = path_of(rel);
        let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap();
        p.preroll().unwrap();
        let ix = p.index(demux).unwrap_or_else(|| panic!("{rel} published nothing"));
        assert!(!ix.entries.is_empty() && ix.complete, "{rel}");
    }
}

/// An MP4 run's demuxer holds the tables the open folded and folds none, a moov behind the
/// mdat too.
#[test]
fn an_mp4_run_takes_the_tables_of_the_open() {
    for rel in [
        "plugins/formats/mp4/tests/fixtures/tiny_av.mp4",
        "plugins/formats/mp4/tests/fixtures/moov_last.mp4",
    ] {
        let o = opened(rel);
        let pf_open::Index::Mp4(tables) = &o.index else { panic!("{rel} {:?}", o.index) };
        assert_eq!(Arc::strong_count(tables), 1);
        let path = path_of(rel);
        let mut runs = Vec::new();
        for n in 1..=2 {
            let mut p = pf_reactor::pipeline();
            let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap();
            p.preroll().unwrap();
            assert!(!p.streams(demux).is_empty(), "{rel}");
            assert_eq!(Arc::strong_count(tables), 1 + n, "{rel} run {n} holds the open's");
            runs.push(p);
        }
        drop(runs);
        assert_eq!(Arc::strong_count(tables), 1, "{rel}");
    }
}

/// A Matroska run's demuxer holds the head the open parsed and its Cues, and neither the head's
/// bytes nor a copy of them, so it parses nothing again.
#[test]
fn a_matroska_run_takes_the_head_the_open_parsed() {
    let rel = "plugins/formats/mp4/tests/fixtures/cues_last.mkv";
    let o = opened(rel);
    let pf_open::Index::Mkv(parsed) = &o.index else { panic!("{:?}", o.index) };
    assert!(!parsed.cues.is_empty() && !parsed.tracks.is_empty());
    assert_eq!((Arc::strong_count(parsed), Arc::strong_count(&o.head)), (1, 1));
    let path = path_of(rel);
    let mut runs = Vec::new();
    for n in 1..=2 {
        let mut p = pf_reactor::pipeline();
        let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap();
        p.preroll().unwrap();
        assert_eq!(p.streams(demux).len(), o.tracks.len());
        assert!(p.index(demux).is_some_and(|ix| ix.complete), "the open's Cues published");
        assert_eq!(Arc::strong_count(parsed), 1 + n, "run {n} holds the open's");
        assert_eq!(Arc::strong_count(&o.head), 1, "run {n} holds no head");
        runs.push(p);
    }
    drop(runs);
    assert_eq!(Arc::strong_count(parsed), 1);
}

#[test]
fn a_track_of_another_kind_or_number_has_no_route() {
    let rel = "plugins/formats/mp4/tests/fixtures/tiny_av.mp4";
    let o = opened(rel);
    let mut p = pf_reactor::pipeline();
    let path = path_of(rel);
    let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, true).unwrap();
    p.preroll().unwrap();
    let mut t = o.tracks[0];
    assert!(matches!(route(&p, demux, &o, &t), Some(Out::Stream(_))));
    t.id = 99;
    assert_eq!(route(&p, demux, &o, &t), None);
    let o = Opened { kind: Kind::Wav, index: pf_open::Index::None, ..o };
    let e = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap_err();
    assert!(matches!(e, BuildError::Unsupported(Kind::Wav)), "a bare kind without its head, {e}");
}

static PCM_DESC: ElementDesc = ElementDesc { name: "pcm", ..DESC };

/// What a run put out, the stamp, duration and bytes of each buffer, and how many came before
/// the last flush.
#[derive(Default)]
struct Pcm {
    bufs: Vec<(Timestamp, Timestamp, Vec<u8>)>,
    flushed: usize,
}

/// Collects PCM. Gated, it holds its first buffer until the test has sent its seek.
struct Collect {
    got: Arc<Mutex<Pcm>>,
    gate: Option<(Sender<()>, Receiver<()>)>,
}

impl Element for Collect {
    fn desc(&self) -> &'static ElementDesc {
        &PCM_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut any = false;
        while let Some(b) = inputs.pop() {
            self.got.lock().unwrap().bufs.push((b.pts, b.duration, b.memory.data().to_vec()));
            any = true;
        }
        if any && let Some((first, go)) = self.gate.take() {
            first.send(()).unwrap();
            go.recv().unwrap();
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            let mut g = self.got.lock().unwrap();
            g.flushed = g.bufs.len();
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    /// `filesrc ! decoder`, the decoder framing the bytes and publishing the index, as fhs HLS
    /// reads a bare file today.
    Today,
    /// pf-open's `baresrc ! decoder`.
    Open,
}

/// The decoder of a bare file, indexing the file of `len` bytes when it reads it from byte 0.
fn decoder(p: &mut Pipeline, kind: Kind, len: Option<u64>) -> (ElementId, SinkPad, SrcPad) {
    match (kind, len) {
        (Kind::Flac, None) => (p.add(FlacDec::new()), FlacDec::IN, FlacDec::OUT),
        (Kind::Flac, len) => (p.add(FlacDec::new().indexing(len)), FlacDec::IN, FlacDec::OUT),
        (Kind::Mp3, None) => (p.add(LibavAudioDec::mp3()), LibavAudioDec::IN, LibavAudioDec::OUT),
        (Kind::Mp3, len) => {
            (p.add(LibavAudioDec::mp3().indexing(len)), LibavAudioDec::IN, LibavAudioDec::OUT)
        }
        _ => (p.add(LibavAudioDec::aac()), LibavAudioDec::IN, LibavAudioDec::OUT),
    }
}

/// What a run gave after its seek, or all of it.
struct Played {
    first: Option<Timestamp>,
    end: Option<Timestamp>,
    pcm: Vec<u8>,
    index: Option<Arc<Index>>,
    plan: Option<SeekPlan>,
}

/// `path` played the `way` to its end, seeked to `to` as fhs HLS seeks once PCM comes out.
fn play(path: &Path, way: Way, to: Option<Timestamp>) -> Played {
    let o = opened_at(path);
    let file = File::open(path).unwrap();
    let mut p = pf_reactor::pipeline();
    p.set_select_all(false);
    let (dec, out, indexer) = match way {
        Way::Today => {
            let src = p.add(FileSrc::from_file(file, path));
            let (dec, inp, out) = decoder(&mut p, o.kind, Some(o.len));
            p.link(src.src(FileSrc::OUT), dec.sink(inp)).unwrap();
            p.set_element_pool_bytes(dec, 128 << 10, 64, DEMUX_BYTES);
            p.preroll().unwrap();
            (dec, out, dec)
        }
        Way::Open => {
            let demux = demuxer(&mut p, file, path, &o, false).unwrap();
            p.preroll().unwrap();
            let pad = route(&p, demux, &o, &o.tracks[0]).unwrap().pad(&mut p, demux).unwrap();
            let (dec, inp, out) = decoder(&mut p, o.kind, None);
            p.link(demux.src(pad), dec.sink(inp)).unwrap();
            (dec, out, demux)
        }
    };
    let got = Arc::new(Mutex::new(Pcm::default()));
    let ((first_tx, first_rx), (go_tx, go_rx)) = (channel(), channel());
    let gate = to.map(|_| (first_tx, go_rx));
    let sink = p.add(Collect { got: Arc::clone(&got), gate });
    p.link(dec.src(out), sink.sink(SinkPad(0))).unwrap();
    let ctl = p.control();
    let run = std::thread::spawn(move || {
        let r = p.run();
        (p, r)
    });
    let mut plan = None;
    if let Some(to) = to {
        first_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        // as fhs HLS asks, the plan first while the index is read
        for _ in 0..10_000 {
            if ctl.seekable() != Seekable::Unknown
                && ctl.plan_from(Timestamp::NONE, to, SeekMode::Accurate) != SeekPlan::Deferred
            {
                plan = Some(ctl.seek_from(Timestamp::NONE, to, SeekMode::Accurate).unwrap());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        go_tx.send(()).unwrap();
    }
    let (p, r) = run.join().unwrap();
    r.unwrap();
    let g = got.lock().unwrap();
    let after = &g.bufs[if to.is_some() { g.flushed } else { 0 }..];
    Played {
        first: after.first().map(|b| b.0),
        end: after.last().map(|b| b.0.saturating_add(b.1)),
        pcm: after.iter().flat_map(|b| b.2.iter().copied()).collect(),
        index: p.index(indexer),
        plan,
    }
}

const TONE_FLAC: &str = "plugins/formats/bare/tests/fixtures/tone.flac";
const TONE_MP3: &str = "plugins/formats/bare/tests/fixtures/tone.mp3";
const TONE_AAC: &str = "plugins/formats/bare/tests/fixtures/tone.aac";

/// A bare file the paths are compared on.
struct Bare {
    path: PathBuf,
    /// Today's seeks land on the samples of a whole decode. A FLAC without a SEEKTABLE restarts
    /// at an estimated byte and stamps the frame found there with the target, an MP3 without a
    /// Xing frame lands a frame off its line, and ADTS has no index.
    today_seeks: bool,
    /// The length is estimated, from a CBR line or the head's ADTS frames.
    estimated: bool,
}

/// A FLAC of `secs` seconds of a tone under noise, with a SEEKTABLE point every tenth frame
/// when `table`.
fn synth_flac(secs: u32, table: bool) -> Vec<u8> {
    const RATE: u32 = 44_100;
    let (mut enc, _) = FlacEncoder::new(RATE, 2, SampleFormat::S16).unwrap();
    let n = (RATE * secs) as usize;
    let mut pcm = Vec::with_capacity(n * 4);
    let mut x = 1u32;
    for i in 0..n {
        let t = i as f64 / f64::from(RATE);
        for c in 0..2 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let tone = 9_000.0 * (std::f64::consts::TAU * f64::from(220 + 110 * c) * t).sin();
            let noise = f64::from((x >> 20) as i32 - 2048);
            pcm.extend_from_slice(&((tone + noise) as i16).to_le_bytes());
        }
    }
    let mut frames = Vec::new();
    enc.encode_frames(&pcm, &mut frames).unwrap();
    let si = enc.finish_frames(&mut frames);
    let block = u64::from(u16::from_be_bytes([si[0], si[1]]));
    let mut f = b"fLaC".to_vec();
    f.extend_from_slice(&[if table { 0 } else { 0x80 }, 0, 0, 34]);
    f.extend_from_slice(&si);
    if table {
        let mut points = Vec::new();
        let mut off = 0u64;
        for (k, fr) in frames.iter().enumerate() {
            if k % 10 == 0 && k + 1 < frames.len() {
                points.extend_from_slice(&(k as u64 * block).to_be_bytes());
                points.extend_from_slice(&off.to_be_bytes());
                points.extend_from_slice(&(block as u16).to_be_bytes());
            }
            off += fr.len() as u64;
        }
        let len = points.len() as u32;
        f.extend_from_slice(&[0x83, (len >> 16) as u8, (len >> 8) as u8, len as u8]);
        f.extend_from_slice(&points);
    }
    frames.iter().for_each(|fr| f.extend_from_slice(fr));
    f
}

/// The audio frames of `rel`, past its tags and Xing frame, `times` over.
fn repeated(rel: &str, times: usize) -> Vec<u8> {
    let o = opened(rel);
    let pf_open::Index::Bare(h) = &o.index else { panic!("{rel} is not bare") };
    std::fs::read(path_of(rel)).unwrap()[h.first as usize..h.end as usize].repeat(times)
}

/// The tone fixtures and longer files made from them, written once.
fn bare_files() -> &'static [Bare] {
    static FILES: OnceLock<Vec<Bare>> = OnceLock::new();
    FILES.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let made = |name: &str, bytes: Vec<u8>| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let bare = |path, today_seeks, estimated| Bare { path, today_seeks, estimated };
        vec![
            bare(path_of(TONE_FLAC), false, false),
            bare(path_of(TONE_MP3), true, false),
            bare(path_of(TONE_AAC), false, false),
            bare(made("table.flac", synth_flac(20, true)), true, false),
            bare(made("bisect.flac", synth_flac(20, false)), false, false),
            // no Xing frame, so no gapless delay, and seeks on the CBR line
            bare(made("line.mp3", repeated(TONE_MP3, 12)), false, true),
            // short of the reader's 20 s walk, so a seek counts frames from the start
            bare(made("long.aac", repeated(TONE_AAC, 9)), false, true),
        ]
    })
}

/// The rate, bytes a sample frame and samples a codec frame of `path`, the decoders put out s16.
fn shape(path: &Path) -> (u32, usize, u32) {
    let o = opened_at(path);
    let pf_open::Index::Bare(h) = &o.index else { panic!("{} is not bare", path.display()) };
    (o.tracks[0].rate, usize::from(o.tracks[0].channels) * 2, h.spf)
}

/// Perceptual noise substitution draws from a generator a flush does not reseed (aacdec.c seeds
/// `random_state` at init only), so AAC after a seek differs from a fresh decode in those bands.
const AAC_NOISE: i32 = 8;

fn samples(pcm: &[u8]) -> Vec<i32> {
    pcm.as_chunks::<2>().0.iter().map(|c| i32::from(i16::from_le_bytes(*c))).collect()
}

/// How far apart two runs of s16 PCM are, the length difference, first and last differing sample
/// and the largest difference.
fn apart(got: &[u8], want: &[u8]) -> (i64, Option<usize>, Option<usize>, i32) {
    let n = got.len().min(want.len());
    let (g, w) = (samples(&got[..n]), samples(&want[..n]));
    let first = (0..g.len()).find(|&i| g[i] != w[i]);
    let last = (0..g.len()).rev().find(|&i| g[i] != w[i]);
    let worst = g.iter().zip(&w).map(|(a, b)| (a - b).abs()).max().unwrap_or(0);
    (got.len() as i64 - want.len() as i64, first, last, worst)
}

#[test]
fn a_bare_file_decodes_through_baresrc_as_through_its_decoder() {
    for f in bare_files() {
        let name = f.path.display();
        let (today, open) = (play(&f.path, Way::Today, None), play(&f.path, Way::Open, None));
        assert!(!open.pcm.is_empty(), "{name}");
        assert_eq!(open.first, Some(Timestamp::ZERO), "{name}");
        assert_eq!((open.first, open.end), (today.first, today.end), "{name}");
        assert_eq!(apart(&open.pcm, &today.pcm), (0, None, None, 0), "{name}");
    }
}

#[test]
fn a_seek_through_baresrc_lands_on_the_samples_of_a_whole_decode() {
    for f in bare_files() {
        let name = f.path.display();
        let (rate, stride, _) = shape(&f.path);
        let whole = play(&f.path, Way::Today, None);
        let end = whole.end.unwrap();
        let samples = whole.pcm.len() / stride;
        assert_eq!(samples as u64, profluens_audio::position_at(end, rate), "{name} is s16");
        let noise = if f.path.extension().is_some_and(|e| e == "aac") { AAC_NOISE } else { 0 };
        for to in [0, 13_000_000, end.0 / 2, end.0 - 10_000_000, end.0 + 5_000_000_000] {
            let to = Timestamp(to);
            let open = play(&f.path, Way::Open, Some(to));
            assert!(matches!(open.plan, Some(SeekPlan::Seek { .. })), "{name} at {to:?}");
            let at = (profluens_audio::position_at(to, rate) as usize).min(samples);
            let want = &whole.pcm[at * stride..];
            let first = (at < samples).then(|| profluens_audio::time_at(at as u64, rate));
            assert_eq!(open.first, first, "{name} at {to:?}");
            let (longer, _, _, worst) = apart(&open.pcm, want);
            assert!(longer == 0 && worst <= noise, "{name} at {to:?}, {longer} bytes, {worst} off");
            if f.today_seeks {
                let today = play(&f.path, Way::Today, Some(to));
                assert_eq!(today.first, open.first, "{name} at {to:?}");
                assert_eq!(apart(&today.pcm, &open.pcm), (0, None, None, 0), "{name} at {to:?}");
            }
        }
    }
}

/// HLS waits on an index that is not complete and plans its segments to the duration.
#[test]
fn the_index_baresrc_publishes_is_final_and_ends_with_the_pcm() {
    for f in bare_files() {
        let name = f.path.display();
        let (rate, _, spf) = shape(&f.path);
        // a frame, and a nanosecond for the two stamps rounded down
        let frame = profluens_audio::time_at(u64::from(spf), rate).0 + 1;
        let (today, open) = (play(&f.path, Way::Today, None), play(&f.path, Way::Open, None));
        let ix = open.index.unwrap_or_else(|| panic!("{name} published no index"));
        assert!(ix.complete && ix.entries.is_empty(), "{name}");
        let (got, end) = (ix.duration.0, open.end.unwrap().0);
        let off = if f.estimated { frame } else { 0 };
        assert!(got.abs_diff(end) <= off, "{name} lasts {got} ns, its PCM ends at {end}");
        if let Some(t) = today.index {
            assert!(t.duration.0.abs_diff(got) <= frame, "{name} {} ns today", t.duration.0);
        }
    }
}
