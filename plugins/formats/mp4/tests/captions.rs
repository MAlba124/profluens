// Test fixtures and a test rig, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::{Arc, Mutex};

use pf_mp4::{Mp4Demux, Mp4Reader};
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
use profluens_core::pipeline::{Pipeline, SeekMode};
use profluens_core::pull::Ask;
use profluens_core::sched::Stepped;
use profluens_core::time::Timestamp;

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

type Asked = Arc<Mutex<Vec<(u64, u32)>>>;

/// Serves every range asked from memory and keeps what was asked, or pushes the file from
/// byte `pushed` on.
struct MemSrc {
    file: Arc<[u8]>,
    asked: Asked,
    pushed: Option<usize>,
}

impl Element for MemSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.pushed.is_none() {
            ctx.offer_ranges(OUT);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(at) = self.pushed.as_mut() {
            if *at >= self.file.len() {
                return Ok(Flow::Eos);
            }
            let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
            let n = buf.memory.capacity().min(self.file.len() - *at);
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[*at..*at + n]);
            buf.memory.set_len(n);
            ctx.out(OUT).push(buf);
            *at += n;
            return Ok(Flow::Ok);
        }
        while let Some(Ask { offset, len, tag, mut buf }) = ctx.next_ask(OUT) {
            self.asked.lock().unwrap().push((offset, len));
            let from = (offset as usize).min(self.file.len());
            let to = (from + len as usize).min(self.file.len());
            buf.memory.as_mut_full()[..to - from].copy_from_slice(&self.file[from..to]);
            buf.memory.set_len(to - from);
            ctx.serve(OUT, tag, buf);
        }
        Ok(if ctx.asks_over(OUT) { Flow::Eos } else { Flow::Ok })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Frame(u64),
    Flush,
}

type Log = Arc<Mutex<Vec<(usize, Seen)>>>;

struct RecSink {
    pad: usize,
    out: Log,
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            if let (false, Some(pts)) = (buf.flags.contains(BufferFlags::HEADER), buf.pts.nanos()) {
                self.out.lock().unwrap().push((self.pad, Seen::Frame(pts)));
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.out.lock().unwrap().push((self.pad, Seen::Flush));
        }
        Ok(())
    }
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    boxed(kind, &[&[0u8; 4][..], body].concat())
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn tx3g(text: &str) -> Vec<u8> {
    let mut b = (text.len() as u16).to_be_bytes().to_vec();
    b.extend_from_slice(text.as_bytes());
    b
}

const PICTURES: u32 = 60;
const PICTURE: usize = 4096;
/// The captions in seconds, start and length. The track's samples between them are empty.
const CAPTIONS: [(u32, u32); 3] = [(2, 3), (30, 15), (50, 4)];

/// One `vp09` keyframe a second beside a `tx3g` track, a sample a chunk. A text sample is
/// stored behind the picture of the second it starts at.
fn film(ahead: bool) -> Vec<u8> {
    let mut vp09 = vec![0u8; 78];
    vp09[7] = 1;
    vp09[24..28].copy_from_slice(&[0, 64, 0, 48]);
    let mut entry = vec![0, 0, 0, 0, 0, 0, 0, 1];
    entry.extend([0u8; 30]);
    entry.extend(boxed(b"ftab", &[0, 1, 0, 1, 5, b'A', b'r', b'i', b'a', b'l']));
    let mut text: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    let mut at = 0;
    for (n, (start, len)) in CAPTIONS.into_iter().enumerate() {
        if start > at {
            text.push((at, start - at, tx3g("")));
        }
        text.push((start, len, tx3g(&format!("caption {n}"))));
        at = start + len;
    }
    text.push((at, PICTURES - at, tx3g("")));
    let stbl = |entry: &[u8], samples: &[(u32, u32)], offsets: &[u32], sync: bool| {
        let mut stts = be32(&[samples.len() as u32]);
        samples.iter().for_each(|(d, _)| stts.extend(be32(&[1, *d])));
        let mut stsz = be32(&[0, samples.len() as u32]);
        samples.iter().for_each(|(_, size)| stsz.extend(be32(&[*size])));
        let mut stco = be32(&[offsets.len() as u32]);
        stco.extend(be32(offsets));
        let mut stbl = [
            full(b"stsd", &[&be32(&[1])[..], entry].concat()),
            full(b"stts", &stts),
            full(b"stsz", &stsz),
            full(b"stsc", &be32(&[1, 1, 1, 1])),
            full(b"stco", &stco),
        ]
        .concat();
        if sync {
            let all: Vec<u32> = (1..=samples.len() as u32).collect();
            let mut stss = be32(&[all.len() as u32]);
            stss.extend(be32(&all));
            stbl.extend(full(b"stss", &stss));
        }
        stbl
    };
    let trak = |id: u32, handler: Option<&[u8; 4]>, dims: [u32; 2], stbl: Vec<u8>| {
        let mut mdhd = vec![0u8; 8];
        mdhd.extend(be32(&[1000, PICTURES * 1000, 0x55c4_0000]));
        let mut mdia = full(b"mdhd", &mdhd);
        if let Some(h) = handler {
            let mut hdlr = vec![0u8; 4];
            hdlr.extend_from_slice(h);
            hdlr.extend([0u8; 13]);
            mdia.extend(full(b"hdlr", &hdlr));
        }
        mdia.extend(boxed(b"minf", &boxed(b"stbl", &stbl)));
        let mut tkhd = vec![0u8; 8];
        tkhd.extend(be32(&[id, 0, PICTURES * 1000]));
        tkhd.extend([0u8; 52]);
        tkhd.extend(be32(&[dims[0] << 16, dims[1] << 16]));
        boxed(b"trak", &[full(b"tkhd", &tkhd), boxed(b"mdia", &mdia)].concat())
    };
    let build = |base: u32| {
        let (mut video_at, mut text_at, mut data) = (Vec::new(), Vec::new(), Vec::new());
        for (_, _, bytes) in text.iter().filter(|_| ahead) {
            text_at.push(base + data.len() as u32);
            data.extend_from_slice(bytes);
        }
        for n in 0..PICTURES {
            video_at.push(base + data.len() as u32);
            let mut picture = vec![b'V'];
            picture.extend(n.to_be_bytes());
            picture.resize(PICTURE, 0);
            data.extend(picture);
            for (_, _, bytes) in text.iter().filter(|t| !ahead && t.0 == n) {
                text_at.push(base + data.len() as u32);
                data.extend_from_slice(bytes);
            }
        }
        let mut mvhd = vec![0u8; 8];
        mvhd.extend(be32(&[1000, PICTURES * 1000]));
        mvhd.extend([0u8; 80]);
        let pictures = vec![(1000, PICTURE as u32); PICTURES as usize];
        let cues: Vec<(u32, u32)> = text.iter().map(|t| (t.1 * 1000, t.2.len() as u32)).collect();
        let moov = [
            full(b"mvhd", &mvhd),
            trak(1, None, [64, 48], stbl(&boxed(b"vp09", &vp09), &pictures, &video_at, true)),
            trak(2, Some(b"text"), [0, 0], stbl(&boxed(b"tx3g", &entry), &cues, &text_at, false)),
        ]
        .concat();
        ([boxed(b"ftyp", b"isom\0\0\0\0isom"), boxed(b"moov", &moov)].concat(), data)
    };
    let base = build(0).0.len() as u32 + 8;
    let (head, data) = build(base);
    [head, boxed(b"mdat", &data)].concat()
}

struct Ran {
    seen: Vec<(usize, Seen)>,
    /// The ranges asked of the source from the last step on, the switch or the seek.
    asked: Vec<(u64, u32)>,
    /// Where the index restarted the caption stream at the switch.
    restart: Option<(u64, Timestamp)>,
    /// The captions the index named when the run ended.
    captions: usize,
}

impl Ran {
    /// The stamps pad `n` saw behind its last flush.
    fn after_flush(&self, n: usize) -> Vec<u64> {
        let own: Vec<&Seen> = self.seen.iter().filter(|(p, _)| *p == n).map(|(_, s)| s).collect();
        let from = own.iter().rposition(|s| **s == Seen::Flush).map_or(0, |at| at + 1);
        own[from..].iter().filter_map(|s| s.stamp()).collect()
    }

    fn asked_below(&self, byte: u64) -> Vec<(u64, u32)> {
        self.asked.iter().copied().filter(|a| a.0 < byte).collect()
    }
}

impl Seen {
    fn stamp(&self) -> Option<u64> {
        match self {
            Seen::Frame(pts) => Some(*pts),
            Seen::Flush => None,
        }
    }
}

/// Plays `file` pulled with the video on. The caption stream is switched on at `text_ms` with
/// the restart the index names, then an exact seek to `seek_ms`, each where given.
fn play(file: &[u8], text_ms: Option<u64>, seek_ms: Option<u64>) -> Ran {
    run(file, false, text_ms, seek_ms)
}

fn run(file: &[u8], pushed: bool, text_ms: Option<u64>, seek_ms: Option<u64>) -> Ran {
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    p.set_select_all(false);
    let asked: Asked = Arc::default();
    let pushed = pushed.then_some(0);
    let src = p.add(MemSrc { file: Arc::from(file), asked: Arc::clone(&asked), pushed });
    let demux = p.add(Mp4Demux::new(file.to_vec()));
    // a few pictures, so the read stays near what plays
    p.set_element_pool_bytes(demux, 16 << 10, 64, 64 << 10);
    p.link(src.src(OUT), demux.sink(Mp4Demux::IN)).unwrap();
    p.preroll().unwrap();
    let out: Log = Arc::default();
    let link = |p: &mut Pipeline, n: u32| {
        let pad = p.select(demux, n).unwrap();
        let sink = p.add(RecSink { pad: n as usize, out: Arc::clone(&out) });
        p.link(demux.src(pad), sink.sink(SinkPad(0))).unwrap();
    };
    link(&mut p, 0);
    let mut run: Stepped = p.step(Arc::clone(&shared)).unwrap();
    let go = |run: &mut Stepped, until: Option<u64>| {
        let past = |seen: &[(usize, Seen)], ms: u64| {
            seen.iter().any(|(n, s)| *n == 0 && s.stamp().is_some_and(|t| t >= ms * MS))
        };
        for _ in 0..1_000_000u64 {
            if until.is_some_and(|ms| past(&out.lock().unwrap(), ms)) {
                return;
            }
            run.service();
            let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
                let Some(to) = run.next_deadline() else { return };
                if to > clock.now() {
                    clock.advance(Timestamp(to.0 - clock.now().0));
                }
                continue;
            };
            run.pass(g).expect("a pass");
        }
        panic!("the run did not end");
    };
    go(&mut run, Some(text_ms.unwrap_or(2_000)));
    let mut restart = None;
    if let Some(ms) = text_ms {
        let at = Timestamp(ms * MS);
        restart = p.index(demux).and_then(|ix| ix.restart_for(1, at));
        asked.lock().unwrap().clear();
        link(&mut p, 1);
        let targets: Vec<_> =
            restart.into_iter().flat_map(|(b, t)| [(src, b, t), (demux, b, t)]).collect();
        p.commit_at(at, &targets).unwrap();
        if seek_ms.is_some() {
            go(&mut run, Some(ms + 2_000));
        }
    }
    if let Some(ms) = seek_ms {
        asked.lock().unwrap().clear();
        p.control().seek(Timestamp(ms * MS), SeekMode::Accurate).unwrap();
    }
    go(&mut run, None);
    let captions = p.index(demux).map_or(0, |ix| ix.captions.len());
    let _ = run.finish();
    while let Some(m) = p.bus().try_recv() {
        if let BusMessage::Error { error, .. } = m {
            panic!("an error on the bus, {error}");
        }
    }
    drop(p);
    let seen = std::mem::take(&mut *out.lock().unwrap());
    let asked = std::mem::take(&mut *asked.lock().unwrap());
    Ran { seen, asked, restart, captions }
}

/// Where sample `n` of track `track` lies and its size.
fn sample_at(file: &[u8], track: usize, n: usize) -> (u64, u32) {
    let tables = Mp4Reader::new(file).expect("resolves").tables();
    let s = tables.samples().iter().filter(|s| s.track() == track).nth(n).expect("the sample");
    (s.offset, s.size)
}

const S: u64 = 1_000 * MS;

/// The caption up at the target was stored nine pictures ahead of the key. A pulled seek asks
/// its sample alone before the key, a pushed source restarts at it and reads those pictures.
#[test]
fn a_pulled_seek_with_a_text_track_on_reads_only_the_caption_up_before_the_key() {
    let file = film(false);
    let (key, caption) = (sample_at(&file, 0, 40).0, sample_at(&file, 1, 3));
    let ran = play(&file, Some(5_000), Some(40_500));
    assert_eq!(ran.asked_below(key), [caption]);
    assert_eq!(ran.after_flush(0).first(), Some(&(40 * S)));
    assert_eq!(ran.after_flush(1), [30 * S, 50 * S], "the caption up and the one behind it");
    let index = Mp4Reader::new(&file).expect("resolves").tables().seek_index(&|_| true);
    let entry = index.entries.iter().find(|e| e.time_ns == 40 * S).expect("an entry");
    assert_eq!((entry.byte, entry.restart), (key, caption.0));
    assert_eq!(key - caption.0, 9 * PICTURE as u64 + 11, "what a pushed restart reads");
}

/// A text track whose samples all lie ahead of the pictures. The seek asks the caption up and
/// the track's samples behind it, no picture before the key.
#[test]
fn a_pulled_seek_reads_a_caption_stored_ahead_of_every_picture_and_no_picture_before_the_key() {
    let file = film(true);
    let (key, caption) = (sample_at(&file, 0, 40).0, sample_at(&file, 1, 3));
    let ran = play(&file, Some(5_000), Some(40_500));
    let below = ran.asked_below(key);
    assert!(below.iter().all(|a| a.0 >= caption.0 && a.0 < sample_at(&file, 0, 0).0), "{below:?}");
    assert_eq!(below.iter().map(|a| a.1).sum::<u32>(), 11 + 2 + 11 + 2, "{below:?}");
    assert_eq!(ran.after_flush(0).first(), Some(&(40 * S)));
    assert_eq!(ran.after_flush(1), [30 * S, 50 * S]);
}

/// A text track switched on while the video plays restarts at the key of the position, its
/// caption up stored nine pictures before. Only that sample is asked before the key, and
/// every picture goes out once.
#[test]
fn a_text_track_switched_on_reads_its_caption_up_and_no_picture_again() {
    let file = film(false);
    let (key, caption) = (sample_at(&file, 0, 40).0, sample_at(&file, 1, 3));
    let ran = play(&file, Some(40_500), None);
    assert_eq!(ran.restart, Some((key, Timestamp(40 * S))));
    assert_eq!(ran.asked_below(key), [caption]);
    assert_eq!(ran.after_flush(1), [30 * S, 50 * S]);
    let pictures: Vec<u64> = (0..u64::from(PICTURES)).map(|n| n * S).collect();
    assert_eq!(ran.after_flush(0), pictures);
}

/// A text track stored ahead of every picture is switched on while the video plays. Its
/// samples lie before any byte the index can name. The restart is the key all the same, the
/// track is asked from its caption up on, and no picture is read again.
#[test]
fn a_text_track_stored_ahead_of_the_restart_is_read_from_its_caption_up_when_switched_on() {
    let file = film(true);
    let (first, key) = (sample_at(&file, 0, 0).0, sample_at(&file, 0, 40).0);
    let caption = sample_at(&file, 1, 3);
    let ran = play(&file, Some(40_500), None);
    assert_eq!(ran.restart, Some((key, Timestamp(40 * S))));
    let below = ran.asked_below(key);
    assert!(below.iter().all(|a| a.0 >= caption.0 && a.0 + u64::from(a.1) <= first), "{below:?}");
    assert_eq!(below.iter().map(|a| a.1).sum::<u32>(), 11 + 2 + 11 + 2, "{below:?}");
    assert_eq!(ran.after_flush(1), [30 * S, 50 * S], "the caption up and the one behind it");
    let pictures: Vec<u64> = (0..u64::from(PICTURES)).map(|n| n * S).collect();
    assert_eq!(ran.after_flush(0), pictures);
}

/// A pushed source reads nothing aside, so its index goes on naming the captions a restart
/// has to go back for. A pulled one names none.
#[test]
fn a_pushed_run_keeps_the_captions_in_its_index_and_a_pulled_one_names_none() {
    let file = film(false);
    assert_eq!(run(&file, true, None, None).captions, CAPTIONS.len() + 4, "every text sample");
    assert_eq!(play(&file, None, None).captions, 0);
}
