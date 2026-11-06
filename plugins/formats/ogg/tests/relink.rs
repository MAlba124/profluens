//! A link made to an `oggdemux` pad mid-run is handed the stream's header packets first.

// A test's own fixtures and clock, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pf_ogg::{Mapping, OggDemux};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::clock::MockClock;
use profluens_core::command::ALL;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::Timestamp;

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
static FEED: ElementDesc = desc("feed", &[], &SRC_PADS);

#[derive(Clone, PartialEq, Eq, Debug)]
enum Ev {
    Buf { pts: Option<u64>, head: bool, bytes: Vec<u8> },
    Flush,
}

impl Ev {
    fn buf(&self) -> Option<(Option<u64>, bool, &[u8])> {
        match self {
            Ev::Buf { pts, head, bytes } => Some((*pts, *head, bytes)),
            Ev::Flush => None,
        }
    }
}

type Log = Arc<Mutex<Vec<Ev>>>;

struct Rec(Log);

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut evs = self.0.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            evs.push(Ev::Buf {
                pts: buf.pts.nanos(),
                head: buf.flags.contains(BufferFlags::HEADER),
                bytes: buf.memory.data().to_vec(),
            });
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

/// Pushes the file in small unstamped buffers, from a seek's byte after one.
struct Feed {
    data: Arc<Vec<u8>>,
    pos: usize,
}

impl Element for Feed {
    fn desc(&self) -> &'static ElementDesc {
        &FEED
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = 1024.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.pos = ctx.seek_target().map_or(0, |t| t.to_byte as usize);
        }
        Ok(())
    }
}

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// The Vorbis, FLAC and Opus fixtures.
fn fixtures() -> [PathBuf; 3] {
    [
        fixture("../../../fixtures/out/ogg_vorbis.ogg"),
        fixture("../../../fixtures/out/ogg_flac.oga"),
        fixture("tests/fixtures/celt-fb-stereo-128kbps.opus"),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edit {
    /// No edit, the reference run.
    None,
    /// A second link joins the pad beside the first.
    Join,
    /// The first link is cut and a new one made in the same commit.
    Relink,
    /// As `Relink`, a seek to `at` of the file's length following before the demuxer runs.
    RelinkSeek { at: (u64, u64) },
}

struct Run {
    first: Vec<Ev>,
    second: Vec<Ev>,
}

fn pass(run: &mut Stepped) -> bool {
    run.service();
    let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
    for g in &ready {
        run.pass(*g).expect("pass");
    }
    !ready.is_empty()
}

fn rec(p: &mut Pipeline) -> (ElementId, Log) {
    let log = Log::default();
    (p.add(Rec(Arc::clone(&log))), log)
}

/// Plays `path` through `oggdemux`, by stream when `streams`, and makes `edit` once the first
/// link has had `after` packets.
fn play(path: &Path, streams: bool, after: usize, edit: Edit) -> Run {
    let data = Arc::new(std::fs::read(path).expect("fixture"));
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
    let src = p.add(Feed { data: Arc::clone(&data), pos: 0 });
    let len = Some(data.len() as u64);
    let made = match streams {
        true => OggDemux::with_len(len).with_head(data[..data.len().min(64 << 10)].to_vec()),
        false => OggDemux::with_len(len),
    };
    let demux = p.add(made);
    p.link(src.src(SrcPad(0)), demux.sink(OggDemux::IN)).expect("feed ! oggdemux");
    let pad = match streams {
        true => {
            let srcs = p.preroll().expect("preroll").srcs;
            assert_eq!(srcs.len(), 1, "{}: one stream", path.display());
            srcs[0].pad
        }
        false => OggDemux::OUT,
    };
    let (a, first) = rec(&mut p);
    p.link(demux.src(pad), a.sink(SinkPad(0))).expect("oggdemux ! rec");
    let second = Log::default();
    let mut run = p.step(Arc::new(clock)).expect("step");
    if edit != Edit::None {
        while first.lock().unwrap().len() < after {
            assert!(pass(&mut run), "{}: the stream runs", path.display());
        }
        if matches!(edit, Edit::Relink | Edit::RelinkSeek { .. }) {
            let (link, ..) = p.link_into(a, SinkPad(0)).expect("the first link");
            p.unlink(link).expect("unlink");
        }
        let b = p.add(Rec(Arc::clone(&second)));
        p.link(demux.src(pad), b.sink(SinkPad(0))).expect("a link made mid-run");
        p.commit().expect("commit");
        if let Edit::RelinkSeek { at: (num, den) } = edit {
            let to_byte = data.len() as u64 * num / den;
            let at = Timestamp::ZERO;
            p.control().seek_bytes(at, &[(ALL, to_byte, at)]).expect("seek");
        }
    }
    while pass(&mut run) {}
    let what = path.display();
    assert!((0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit)), "{what}: ends");
    run.finish().expect("finish");
    let first = first.lock().unwrap().clone();
    let second = second.lock().unwrap().clone();
    Run { first, second }
}

fn heads_of(evs: &[Ev]) -> usize {
    let ident = evs.iter().find_map(Ev::buf).expect("a packet").2;
    let heads = Mapping::identify(ident).header_packets(ident) as usize;
    assert!(heads > 1, "a mapping of several header packets");
    heads
}

/// A media packet's stamp and bytes.
type Media = (Option<u64>, Vec<u8>);

/// The header packets `evs` opens with and the media after, asserting no header follows.
fn split_heads(evs: &[Ev], heads: usize, what: &str) -> (Vec<Vec<u8>>, Vec<Media>) {
    let bufs: Vec<_> = evs.iter().filter_map(Ev::buf).collect();
    assert!(bufs.len() > heads, "{what}: {} packets", bufs.len());
    for (i, (pts, head, _)) in bufs.iter().enumerate() {
        assert_eq!(*head, i < heads, "{what}: packet {i} flagged a header");
        if i < heads {
            assert_eq!(*pts, None, "{what}: header {i} unstamped");
        }
    }
    let first = evs.iter().position(|e| e.buf().is_some_and(|b| !b.1));
    let flushed = evs.iter().position(|e| *e == Ev::Flush);
    assert!(
        flushed.is_none_or(|f| first.is_none_or(|m| f < m)),
        "{what}: no flush behind the media"
    );
    let head = bufs[..heads].iter().map(|b| b.2.to_vec()).collect();
    let media = bufs[heads..].iter().map(|b| (b.0, b.2.to_vec())).collect();
    (head, media)
}

#[test]
fn a_link_that_joins_a_playing_pad_is_handed_the_headers_first() {
    for path in fixtures() {
        for streams in [true, false] {
            let what = format!("{} streams {streams}", path.display());
            let whole = play(&path, streams, 0, Edit::None).first;
            let heads = heads_of(&whole);
            let (head, all) = split_heads(&whole, heads, &what);
            let run = play(&path, streams, heads + 6, Edit::Join);
            let (played_head, played) = split_heads(&run.first, heads, &format!("{what} first"));
            assert_eq!(played_head, head, "{what}: the first link has the headers once");
            assert_eq!(played, all, "{what}: the first link plays on unchanged");
            let (joined_head, joined) = split_heads(&run.second, heads, &format!("{what} second"));
            assert_eq!(joined_head, head, "{what}: the joining link has the headers");
            assert!(!joined.is_empty() && joined.len() < all.len(), "{what}: {}", joined.len());
            assert_eq!(joined, all[all.len() - joined.len()..], "{what}: then what follows");
        }
    }
}

#[test]
fn a_link_made_in_place_of_a_cut_one_is_handed_the_headers_then_the_stamps_run_on() {
    for path in fixtures() {
        for streams in [true, false] {
            let what = format!("{} streams {streams}", path.display());
            let whole = play(&path, streams, 0, Edit::None).first;
            let heads = heads_of(&whole);
            let (head, all) = split_heads(&whole, heads, &what);
            let run = play(&path, streams, heads + 6, Edit::Relink);
            let (cut_head, cut) = split_heads(&run.first, heads, &format!("{what} first"));
            assert_eq!(cut_head, head, "{what}: the cut link had the headers");
            let (new_head, new) = split_heads(&run.second, heads, &format!("{what} second"));
            assert_eq!(new_head, head, "{what}: the new link has the headers");
            assert!(!new.is_empty(), "{what}: media follows");
            // what was in flight on the cut link goes with it
            assert_eq!(cut, all[..cut.len()], "{what}: the cut link played in order");
            assert!(cut.len() + new.len() <= all.len(), "{what}: nothing repeated");
            assert_eq!(new, all[all.len() - new.len()..], "{what}: then the stamps run on");
        }
    }
}

#[test]
fn a_link_made_as_a_seek_flushes_is_handed_the_headers_once() {
    for path in fixtures() {
        // the start re-reads the headers, the middle does not
        for at in [(0, 1), (1, 2)] {
            let what = format!("{} at {at:?}", path.display());
            let whole = play(&path, true, 0, Edit::None).first;
            let heads = heads_of(&whole);
            let (head, all) = split_heads(&whole, heads, &what);
            let run = play(&path, true, heads + 6, Edit::RelinkSeek { at });
            let (new_head, new) = split_heads(&run.second, heads, &format!("{what} second"));
            assert_eq!(new_head, head, "{what}: the new link has the headers");
            assert!(!new.is_empty(), "{what}: media follows");
            if at.0 == 0 {
                assert_eq!(new, all, "{what}: the whole stream again");
            }
        }
    }
}
