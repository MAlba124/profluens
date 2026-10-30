//! `mkvdemux` grows its pool for a track that starves behind another in a far-apart file.

// A test's own fixtures, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mkv::{MatroskaWriter, MkvDemux, TrackConfig};
use profluens_core::batch::Inputs;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::level::Limit;
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

const MS: u64 = 1_000_000;
const PICTURE: usize = 140 * 1024;
const END_MS: u64 = 4_000;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "feed",
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

struct Feed {
    data: Vec<u8>,
    pos: usize,
    ranges: bool,
}

impl Element for Feed {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.ranges {
            ctx.offer_ranges(SrcPad(0));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if ctx.pulled(SrcPad(0)) {
            while let Some(mut ask) = ctx.next_ask(SrcPad(0)) {
                let from = (ask.offset as usize).min(self.data.len());
                let n = (ask.len as usize).min(self.data.len() - from);
                ask.buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[from..from + n]);
                ask.buf.memory.set_len(n);
                ctx.serve(SrcPad(0), ask.tag, ask.buf);
            }
            return Ok(if ctx.asks_over(SrcPad(0)) { Flow::Eos } else { Flow::Ok });
        }
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = buf.memory.capacity().min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

static SINK_OFFERS: [OfferDesc; 3] =
    [OfferDesc::any("vp8"), OfferDesc::any("aac"), OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "paced",
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

struct Paced {
    first: Arc<Mutex<Option<Timestamp>>>,
}

impl Element for Paced {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(front) = inputs.front() {
            if front.pts.is_some() && !ctx.due(front.pts) {
                break;
            }
            self.first.lock().unwrap().get_or_insert(ctx.now());
            inputs.pop();
        }
        Ok(Flow::Ok)
    }
}

fn file(chunk: u64) -> Vec<u8> {
    let audio = TrackConfig::audio(2, "A_AAC", vec![0x11, 0x90], 48_000.0, 2, 0);
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240), audio]);
    w.enable_cues();
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    for from in (0..END_MS).step_by(chunk as usize) {
        for ms in (from..from + chunk).step_by(100) {
            w.write_frame(&mut out, 1, ms * MS, &vec![(ms / 100) as u8; PICTURE], ms % 1_000 == 0)
                .unwrap();
        }
        for ms in (from..from + chunk).step_by(20) {
            w.write_frame(&mut out, 2, ms * MS, &[2, (ms / 20) as u8], true).unwrap();
        }
    }
    w.finalize(&mut out);
    out
}

fn header_of(stream: &[u8]) -> Vec<u8> {
    let cluster = stream.windows(4).position(|w| w == [0x1F, 0x43, 0xB6, 0x75]).expect("a Cluster");
    stream[..cluster].to_vec()
}

fn play(chunk: u64, ranges: bool, whole: bool) -> (Option<Timestamp>, Option<Timestamp>, u64) {
    let file = file(chunk);
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    p.set_pool(64 * 1024, 32);
    let src = p.add(Feed { data: file.clone(), pos: 0, ranges });
    let demux = p.add(MkvDemux::new(header_of(&file)));
    p.set_element_pool_bytes(demux, 256 * 1024, 8, 32 << 20);
    p.link(src.src(SrcPad(0)), demux.sink(MkvDemux::IN)).expect("feed -> demux");
    let added = p.preroll().expect("preroll").srcs;
    let firsts: Vec<Arc<Mutex<Option<Timestamp>>>> = added.iter().map(|_| Arc::default()).collect();
    for (pad, first) in added.iter().zip(&firsts) {
        let sink = p.add(Paced { first: Arc::clone(first) });
        p.set_link_limit(sink, Limit::time(Timestamp(200 * MS)));
        p.set_queue_capacity(sink, 128);
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).expect("pad -> sink");
    }
    let tap = p.tap_handle();
    let mut run = p.step(Arc::clone(&shared)).expect("step");
    let until = Timestamp(1_000 * MS + END_MS * MS);
    for _ in 0..2_000_000 {
        if clock.now() >= until || (!whole && firsts.iter().all(|f| f.lock().unwrap().is_some())) {
            break;
        }
        run.service();
        if let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) {
            run.pass(g).expect("pass");
            continue;
        }
        let Some(to) = run.next_deadline() else { break };
        clock.advance(Timestamp(to.0.saturating_sub(clock.now().0).max(1)));
    }
    let slots = tap.pool_of(demux).map_or(0, |pool| pool.max_slots);
    run.control().stop();
    let first = |i: usize| *firsts[i].lock().unwrap();
    (first(0), first(1), slots)
}

fn starts_together(how: &str, ranges: bool) {
    let (video, audio, slots) = play(2_000, ranges, false);
    let video = video.unwrap_or_else(|| panic!("{how}: the pictures played"));
    let audio = audio
        .unwrap_or_else(|| panic!("{how}: no audio in {END_MS} ms, the pool held {slots} slots"));
    assert!(
        audio.0 - video.0 < 200 * MS,
        "{how}: the audio began {} ms behind the pictures, {slots} slots",
        (audio.0 - video.0) / MS
    );
    assert!(slots > 8, "{how}: the pool grew, to {slots}");
}

#[test]
fn a_pulled_far_apart_file_grows_the_pool_where_its_pictures_are_asked() {
    starts_together("pulled", true);
}

#[test]
fn a_pushed_far_apart_file_grows_the_pool_where_its_pictures_are_copied() {
    starts_together("pushed", false);
}

#[test]
fn an_interleaved_file_keeps_its_pool() {
    for ranges in [true, false] {
        let (video, audio, slots) = play(100, ranges, true);
        assert!(video.is_some() && audio.is_some(), "pulled {ranges}: both played");
        assert!(slots <= 12, "pulled {ranges}: the pool grew to {slots}");
    }
}
