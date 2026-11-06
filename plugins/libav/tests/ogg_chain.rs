//! `oggdemux ! libavspeexdec` and `oggdemux ! libavtheoradec` link on the families the demuxer
//! emits and decode as the system ffmpeg does. The streams are the ones `speex.rs` and
//! `theora.rs` make, by the same commands.

// Test code.
#![allow(clippy::disallowed_methods)]

mod support;

use std::sync::{Arc, Mutex};

use pf_libav::{LibavAudioDec, LibavTheoraDec};
use pf_ogg::OggDemux;
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::ALL;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::Outcome;
use profluens_core::time::Timestamp;
use profluens_elements::flow::Queue;

const SLOT: usize = 1024 * 1024;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static BYTE_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static CHUNKS: ElementDesc = ElementDesc {
    name: "chunksrc",
    sinks: &[],
    srcs: &BYTE_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// The file in chunks, the first stamped zero as a file source stamps it.
struct Chunks(Vec<Vec<u8>>, usize);

impl Element for Chunks {
    fn desc(&self) -> &'static ElementDesc {
        &CHUNKS
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let Some(chunk) = self.0.get(self.1) else {
            return Ok(Flow::Eos);
        };
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.as_mut_full()[..chunk.len()].copy_from_slice(chunk);
        buf.memory.set_len(chunk.len());
        buf.pts = if self.1 == 0 { Timestamp::ZERO } else { Timestamp::NONE };
        ctx.out(SrcPad(0)).push(buf);
        self.1 += 1;
        Ok(Flow::Ok)
    }
}

static PIXFMTS: [ValueDesc; 1] = [ValueDesc::Id("i420")];
static RAW_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Set(&PIXFMTS), preferred: None },
];
static VIDEO: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &RAW_FIELDS }];
static VIDEO_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &VIDEO }];
static AUDIO: [OfferDesc; 1] = [OfferDesc::any("audio/raw")];
static AUDIO_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &AUDIO }];

const fn sink_desc(sinks: &'static [PadDesc]) -> ElementDesc {
    ElementDesc {
        name: "recordsink",
        sinks,
        srcs: &[],
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static VIDEO_SINK: ElementDesc = sink_desc(&VIDEO_PADS);
static AUDIO_SINK: ElementDesc = sink_desc(&AUDIO_PADS);

type Got = Arc<Mutex<Vec<(Timestamp, Vec<u8>)>>>;

struct Record(&'static ElementDesc, Got);

impl Element for Record {
    fn desc(&self) -> &'static ElementDesc {
        self.0
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut got = self.1.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            got.push((buf.pts, buf.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }
}

/// What each stream of `file` decodes to through `oggdemux ! queue ! dec`, by family, the
/// demuxer in the streams mode an open builds, the file read in 4 KiB chunks.
fn run(file: &[u8]) -> Vec<(&'static str, Got)> {
    let mut p = Pipeline::new();
    p.set_pool(SLOT, 16);
    let src = p.add(Chunks(file.chunks(4096).map(<[u8]>::to_vec).collect(), 0));
    let head = file[..file.len().min(64 << 10)].to_vec();
    let demux = p.add(OggDemux::with_len(Some(file.len() as u64)).with_head(head));
    p.link(src.src(SrcPad(0)), demux.sink(OggDemux::IN)).expect("src ! demux");
    p.set_select_all(false);
    p.preroll().expect("preroll");
    let families: Vec<&'static str> = p.streams(demux).iter().map(|s| s.codec).collect();
    let mut out = Vec::new();
    for (n, family) in families.into_iter().enumerate() {
        let (dec, sink) = match family {
            "theora" => (p.add(LibavTheoraDec::new()), &VIDEO_SINK),
            "speex" => (p.add(LibavAudioDec::speex()), &AUDIO_SINK),
            _ => continue,
        };
        let got: Got = Arc::new(Mutex::new(Vec::new()));
        let pad = p.select(demux, n as u32).expect("select");
        let queue = p.add(Queue::new());
        let snk = p.add(Record(sink, Arc::clone(&got)));
        p.link(demux.src(pad), queue.sink(Queue::IN)).expect("demux ! queue");
        p.link(queue.src(Queue::OUT), dec.sink(SinkPad(0))).expect("queue ! dec");
        p.link(dec.src(SrcPad(0)), snk.sink(SinkPad(0))).expect("dec ! sink");
        out.push((family, got));
    }
    p.run().expect("run");
    while let Some(m) = p.bus().try_recv() {
        if let BusMessage::Warning { error, .. } = m {
            panic!("{error}");
        }
    }
    out
}

fn of<'a>(runs: &'a [(&'static str, Got)], family: &str) -> std::sync::MutexGuard<'a, Recorded> {
    let at = runs.iter().position(|r| r.0 == family).expect(family);
    runs[at].1.lock().unwrap()
}

type Recorded = Vec<(Timestamp, Vec<u8>)>;

fn speex_matches(got: &Recorded, want: &[u8]) {
    assert_eq!(got.first().map(|g| g.0), Some(Timestamp::ZERO));
    let pcm: Vec<u8> = got.iter().flat_map(|g| g.1.iter().copied()).collect();
    assert_eq!(pcm.len(), want.len(), "every sample once");
    let s = |b: &[u8]| -> Vec<i16> {
        b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect()
    };
    let worst = s(&pcm).into_iter().zip(s(want)).map(|(a, b)| (i32::from(a) - i32::from(b)).abs());
    assert!(worst.max().unwrap_or(0) <= 1, "within a step of ffmpeg");
}

/// Every picture as ffmpeg decodes it, stamped 1/15 s apart from zero.
fn theora_matches(got: &Recorded, want: &[u8]) {
    let frame = 160 * 120 * 3 / 2;
    assert_eq!(got.len(), want.len() / frame, "one picture a frame");
    for (k, (pts, bytes)) in got.iter().enumerate() {
        assert!(bytes[..] == want[k * frame..(k + 1) * frame], "frame {k} differs from ffmpeg");
        assert_eq!(*pts, Timestamp::from_nanos(k as u64 * 1_000_000_000 / 15), "frame {k}");
    }
}

const SPEEX: [&str; 8] = [
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:sample_rate=16000:duration=2",
    "-c:a",
    "libspeex",
    "-f",
    "ogg",
];

const THEORA: [&str; 10] = [
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=160x120:rate=15:duration=1",
    "-c:v",
    "libtheora",
    "-q:v",
    "5",
    "-f",
    "ogg",
];

#[test]
fn speex_decodes_through_the_demuxer_as_ffmpeg_does() {
    let Some(ogg) = support::make("tone.spx", &SPEEX) else { return };
    let Some(want) =
        support::oracle("tone.spx", "tone.spx.s16", &["-f", "s16le", "-c:a", "pcm_s16le"])
    else {
        return;
    };
    speex_matches(&of(&run(&ogg), "speex"), &want);
}

#[test]
fn theora_decodes_through_the_demuxer_as_ffmpeg_does() {
    let Some(ogg) = support::make("tiny.ogv", &THEORA) else { return };
    let Some(want) =
        support::oracle("tiny.ogv", "tiny.ogv.yuv", &["-f", "rawvideo", "-pix_fmt", "yuv420p"])
    else {
        return;
    };
    theora_matches(&of(&run(&ogg), "theora"), &want);
}

/// `ffmpeg -f lavfi -i testsrc2=size=160x120:rate=15:duration=1 -f lavfi -i
/// sine=frequency=440:sample_rate=16000:duration=1 -c:v libtheora -q:v 5 -c:a libspeex -f ogg
/// both.ogv`, the two streams interleaved.
#[test]
fn theora_and_speex_in_one_file_decode_side_by_side() {
    let mut args: Vec<&str> = THEORA[..4].to_vec();
    args.extend_from_slice(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=16000:duration=1",
    ]);
    args.extend_from_slice(&["-c:v", "libtheora", "-q:v", "5", "-c:a", "libspeex", "-f", "ogg"]);
    let Some(ogg) = support::make("both.ogv", &args) else { return };
    let Some(yuv) = support::oracle(
        "both.ogv",
        "both.ogv.yuv",
        &["-map", "0:v", "-f", "rawvideo", "-pix_fmt", "yuv420p"],
    ) else {
        return;
    };
    let Some(pcm) = support::oracle(
        "both.ogv",
        "both.ogv.s16",
        &["-map", "0:a", "-f", "s16le", "-c:a", "pcm_s16le"],
    ) else {
        return;
    };
    let runs = run(&ogg);
    assert_eq!(runs.len(), 2, "two streams");
    theora_matches(&of(&runs, "theora"), &yuv);
    speex_matches(&of(&runs, "speex"), &pcm);
}

static RANGES: ElementDesc = ElementDesc { name: "rangesrc", ..CHUNKS };

/// The file served by range from memory, as a file source a demuxer pulls from.
struct Ranges(Arc<Vec<u8>>);

impl Element for Ranges {
    fn desc(&self) -> &'static ElementDesc {
        &RANGES
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.offer_ranges(SrcPad(0));
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(mut ask) = ctx.next_ask(SrcPad(0)) {
            let from = (ask.offset as usize).min(self.0.len());
            let n = (ask.len as usize).min(self.0.len() - from);
            ask.buf.memory.as_mut_full()[..n].copy_from_slice(&self.0[from..from + n]);
            ask.buf.memory.set_len(n);
            ctx.serve(SrcPad(0), ask.tag, ask.buf);
        }
        if ctx.asks_over(SrcPad(0)) {
            return Ok(Flow::Eos);
        }
        ctx.wake_within(std::time::Duration::ZERO);
        Ok(Flow::Ok)
    }
}

/// What a decoder put out behind the last flush, and the flushes.
#[derive(Default)]
struct Behind {
    flushes: usize,
    got: Recorded,
}

struct RecordBehind(Arc<Mutex<Behind>>);

impl Element for RecordBehind {
    fn desc(&self) -> &'static ElementDesc {
        &AUDIO_SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut b = self.0.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            b.got.push((buf.pts, buf.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            let mut b = self.0.lock().unwrap();
            b.flushes += 1;
            b.got.clear();
        }
        Ok(())
    }
}

/// The Speex stream of `file` pulled through `oggdemux ! libavspeexdec` in the streams mode,
/// sought to `at` once the decoder put out a buffer. Panics on a bus warning.
fn seek_speex(file: &[u8], at: Timestamp) -> Behind {
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    p.set_pool(SLOT, 16);
    let src = p.add(Ranges(Arc::new(file.to_vec())));
    let head = file[..file.len().min(64 << 10)].to_vec();
    let demux = p.add(OggDemux::with_len(Some(file.len() as u64)).with_head(head));
    p.link(src.src(SrcPad(0)), demux.sink(OggDemux::IN)).expect("src ! demux");
    p.set_select_all(false);
    p.preroll().expect("preroll");
    let n = p.streams(demux).iter().position(|s| s.codec == "speex").expect("a speex stream");
    let pad = p.select(demux, n as u32).expect("select");
    let dec = p.add(LibavAudioDec::speex());
    let behind = Arc::new(Mutex::new(Behind::default()));
    let snk = p.add(RecordBehind(Arc::clone(&behind)));
    p.link(demux.src(pad), dec.sink(SinkPad(0))).expect("demux ! dec");
    p.link(dec.src(SrcPad(0)), snk.sink(SinkPad(0))).expect("dec ! sink");
    let control = p.control();
    let mut run = p.step(Arc::clone(&shared)).expect("step");
    let mut sought = false;
    for _ in 0..1_000_000u32 {
        if !sought && !behind.lock().unwrap().got.is_empty() {
            sought = true;
            control.seek_bytes(at, &[(ALL, 0, at)]).expect("seek");
        }
        let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
            match run.next_deadline() {
                Some(t) if t > clock.now() => clock.advance(Timestamp(t.0 - clock.now().0)),
                Some(_) => {}
                None => break,
            }
            continue;
        };
        let _ = run.pass(g).expect("pass");
    }
    let done = (0..run.groups()).all(|g| matches!(run.outcome(g), Some(Outcome::Exit)));
    assert!(done, "the run stalled");
    run.finish().expect("finish");
    while let Some(m) = p.bus().try_recv() {
        if let BusMessage::Warning { error, .. } = m {
            panic!("{error}");
        }
    }
    Arc::into_inner(behind).expect("the run over").into_inner().unwrap()
}

/// A seek pulled to the start or into the first page of audio reads the file again from byte 0.
/// Every header goes out flagged, the comment is never decoded as 20 ms of sound, so the decode
/// behind the flush is ffmpeg's from the seek's point, stamped from it.
#[test]
fn a_speex_seek_pulled_decodes_as_ffmpeg_does_from_its_point() {
    let both: Vec<&str> = THEORA[..4]
        .iter()
        .copied()
        .chain(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000:duration=1"])
        .chain(["-c:v", "libtheora", "-q:v", "5", "-c:a", "libspeex", "-f", "ogg"])
        .collect();
    for (name, args) in [("tone.spx", &SPEEX[..]), ("both.ogv", &both[..])] {
        let Some(ogg) = support::make(name, args) else { return };
        let s16 = format!("{name}.s16");
        let oracle = ["-map", "0:a", "-f", "s16le", "-c:a", "pcm_s16le"];
        let Some(want) = support::oracle(name, &s16, &oracle) else { return };
        for ms in [0u64, 500] {
            let at = Timestamp::from_nanos(ms * 1_000_000);
            let behind = seek_speex(&ogg, at);
            let what = format!("{name} at {ms} ms");
            assert_eq!(behind.flushes, 1, "{what}");
            assert_eq!(behind.got.first().map(|g| g.0), Some(at), "{what}: the first stamp");
            let from = (ms * RATE / 1000 * 2) as usize;
            speex_matches_from(&behind.got, &want[from..], &what);
        }
    }
}

const RATE: u64 = 16_000;

fn speex_matches_from(got: &Recorded, want: &[u8], what: &str) {
    let pcm: Vec<u8> = got.iter().flat_map(|g| g.1.iter().copied()).collect();
    assert_eq!(pcm.len(), want.len(), "{what}: every sample from the point once");
    let s = |b: &[u8]| -> Vec<i16> {
        b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect()
    };
    // the flushed decoder keeps its state, its first two packets differ from a fresh one's
    let worst = s(&pcm)
        .into_iter()
        .zip(s(want))
        .skip(640)
        .map(|(a, b)| (i32::from(a) - i32::from(b)).abs());
    assert!(worst.max().unwrap_or(0) <= 1, "{what}: within a step of ffmpeg");
}
