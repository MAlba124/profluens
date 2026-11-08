//! Stream selection in `mkvdemux`. A switch while playing sends the source back for the new track
//! alone, and the tracks that play on put nothing out twice.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mkv::ebml;
use pf_mkv::{
    MatroskaWriter, MkvDemux, TrackConfig, parse_cue_tracks, parse_cues, parse_seek_head,
};
use profluens_core::batch::Inputs;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::{Ctx, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::log::{DebugSpec, Level, LogRecord, LogWriter};
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::Timestamp;
use profluens_core::topology::Leave;

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
    stamp: Timestamp,
    flushes: Arc<Mutex<Vec<u64>>>,
}

impl Element for Feed {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = 96.min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        buf.pts = std::mem::replace(&mut self.stamp, Timestamp::NONE);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let (Event::FlushStart, Some(target)) = (event, ctx.seek_target()) {
            (self.pos, self.stamp) = (target.to_byte as usize, target.at);
            self.flushes.lock().unwrap().push(target.to_byte);
        }
        Ok(())
    }
}

static SINK_OFFERS: [OfferDesc; 4] = [
    OfferDesc::any("vp8"),
    OfferDesc::any("aac"),
    OfferDesc::any("subtitle/srt"),
    OfferDesc::any("bytes"),
];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
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

type Taken = Arc<Mutex<Vec<([u8; 2], u64)>>>;

struct Rec(Taken);

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let bytes = buf.memory.data();
            if bytes != HEAD {
                self.0.lock().unwrap().push(([bytes[0], bytes[1]], buf.pts.0 / 1_000_000));
            }
        }
        Ok(Flow::Ok)
    }
}

const HEAD: [u8; 2] = [0x11, 0x90];
const END_MS: u64 = 2_400;

fn movie_with_text(captions: &[u64]) -> Vec<u8> {
    let first = TrackConfig::audio(2, "A_AAC", HEAD.to_vec(), 48_000.0, 2, 0);
    let second = TrackConfig::audio(3, "A_AAC", HEAD.to_vec(), 48_000.0, 2, 0);
    let text = TrackConfig::audio(4, "S_TEXT/UTF8", Vec::new(), 0.0, 0, 0);
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240), first, second, text]);
    w.enable_cues();
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    for ms in (0..END_MS).step_by(20) {
        if ms % 40 == 0 {
            w.write_frame(&mut out, 1, ms * 1_000_000, &[1, (ms / 40) as u8], ms % 400 == 0)
                .unwrap();
        }
        w.write_frame(&mut out, 2, ms * 1_000_000, &[2, (ms / 20) as u8], true).unwrap();
        w.write_frame(&mut out, 3, ms * 1_000_000, &[3, (ms / 20) as u8], true).unwrap();
        if captions.contains(&ms) {
            w.write_frame(&mut out, 4, ms * 1_000_000, &[4, (ms / 20) as u8], true).unwrap();
        }
    }
    w.finalize(&mut out);
    out
}

fn movie() -> Vec<u8> {
    let first = TrackConfig::audio(2, "A_AAC", HEAD.to_vec(), 48_000.0, 2, 0);
    let second = TrackConfig::audio(3, "A_AAC", HEAD.to_vec(), 48_000.0, 2, 0);
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240), first, second]);
    w.enable_cues();
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    for ms in (0..END_MS).step_by(20) {
        if ms % 40 == 0 {
            w.write_frame(&mut out, 1, ms * 1_000_000, &[1, (ms / 40) as u8], ms % 400 == 0)
                .unwrap();
        }
        w.write_frame(&mut out, 2, ms * 1_000_000, &[2, (ms / 20) as u8], true).unwrap();
        w.write_frame(&mut out, 3, ms * 1_000_000, &[3, (ms / 20) as u8], true).unwrap();
    }
    w.finalize(&mut out);
    out
}

fn header_of(stream: &[u8]) -> Vec<u8> {
    let cluster = stream.windows(4).position(|w| w == [0x1F, 0x43, 0xB6, 0x75]).expect("a Cluster");
    stream[..cluster].to_vec()
}

fn cues_of(stream: &[u8]) -> Vec<(u64, u64)> {
    let info = parse_seek_head(&header_of(stream)).expect("a SeekHead");
    let at = (info.segment_data_start + info.cues_pos.expect("it names the Cues")) as usize;
    let end = ebml::read_element_header(stream, at).unwrap().data_end().unwrap().unwrap();
    parse_cues(&stream[at..end], info.segment_data_start, info.timestamp_scale)
}

struct Rig {
    p: Pipeline,
    clock: MockClock,
    src: ElementId,
    demux: ElementId,
    flushes: Arc<Mutex<Vec<u64>>>,
}

/// Every track's cues of `stream`, as an open hands them to the demuxer.
fn every_cue(stream: &[u8]) -> Vec<pf_mkv::Cue> {
    let info = parse_seek_head(&header_of(stream)).expect("a SeekHead");
    let at = (info.segment_data_start + info.cues_pos.expect("it names the Cues")) as usize;
    parse_cue_tracks(&stream[at..], info.segment_data_start, info.timestamp_scale)
}

/// The events a run logged.
#[derive(Default)]
struct Events(Mutex<Vec<&'static str>>);

impl Events {
    fn count(&self, event: &str) -> usize {
        self.0.lock().unwrap().iter().filter(|e| **e == event).count()
    }
}

impl LogWriter for Events {
    fn record(&self, rec: &LogRecord) {
        self.0.lock().unwrap().push(rec.event);
    }

    fn line(&self, _level: Level, _msg: std::fmt::Arguments<'_>) {}
}

impl Rig {
    fn new(stream: &[u8]) -> Rig {
        Rig::with(stream, MkvDemux::new(header_of(stream)), None)
    }

    fn with(stream: &[u8], demux: MkvDemux, events: Option<Arc<Events>>) -> Rig {
        let clock = MockClock::starting_at(Timestamp::from_secs(1));
        let mut p = Pipeline::with(DebugSpec::parse("debug"), false, Arc::new(clock.clone()));
        if let Some(events) = events {
            p.set_log_writer(events);
        } else {
            p.set_debug(DebugSpec::default());
        }
        p.set_pool(4096, 32);
        p.set_select_all(false);
        let flushes = Arc::new(Mutex::new(Vec::new()));
        let feed = Feed {
            data: stream.to_vec(),
            pos: 0,
            stamp: Timestamp::NONE,
            flushes: Arc::clone(&flushes),
        };
        let (src, demux) = (p.add(feed), p.add(demux));
        p.link(src.src(SrcPad(0)), demux.sink(MkvDemux::IN)).expect("feed -> demux");
        assert!(p.preroll().expect("preroll").srcs.is_empty(), "describing tracks adds no pad");
        Rig { p, clock, src, demux, flushes }
    }

    fn play(&mut self, stream: u32) -> (ElementId, Taken) {
        let pad = self.p.select(self.demux, stream).expect("select");
        let taken = Taken::default();
        let sink = self.p.add(Rec(Arc::clone(&taken)));
        self.p.link(self.demux.src(pad), sink.sink(SinkPad(0))).expect("pad -> sink");
        (sink, taken)
    }

    fn run(&self, run: &mut Stepped, mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..2_000_000 {
            if done() {
                return false;
            }
            run.service();
            let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
                let Some(to) = run.next_deadline() else { break };
                self.clock.advance(Timestamp(to.0 - self.clock.now().0));
                continue;
            };
            run.pass(g).expect("pass");
        }
        (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
    }
}

fn stamps(taken: &Taken) -> Vec<u64> {
    taken.lock().unwrap().iter().map(|(_, ms)| *ms).collect()
}

fn every(step: usize, from: u64) -> Vec<u64> {
    (from..END_MS).step_by(step).collect()
}

#[test]
fn the_tracks_are_described_and_one_nobody_selected_is_read_past() {
    let stream = movie();
    let mut r = Rig::new(&stream);
    let streams = r.p.streams(r.demux);
    let shape: Vec<_> = streams.iter().map(|s| (s.kind, s.codec, &*s.name, s.default)).collect();
    assert_eq!(
        shape,
        [
            (StreamKind::Video, "vp8", "src_track1", true),
            (StreamKind::Audio, "aac", "src_track2", true),
            (StreamKind::Audio, "aac", "src_track3", true),
        ]
    );
    assert_eq!(&*streams[2].language, "eng", "the default a track that names none has");
    let ((_, video), (_, audio)) = (r.play(0), r.play(2));
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    assert!(r.run(&mut run, || false), "the run ends");
    run.finish().expect("finish");
    assert_eq!(stamps(&video), every(40, 0));
    assert_eq!(stamps(&audio), every(20, 0));
    assert!(
        audio.lock().unwrap().iter().all(|(bytes, _)| bytes[0] == 3),
        "the track that was selected"
    );
    let counters = r.p.counters(r.demux);
    assert_eq!(counters.drops, 0);
    let frames = END_MS / 40 + END_MS / 20;
    assert!(
        (frames..=frames + 2).contains(&counters.buffers_out),
        "the frames and a codec head a pad"
    );
}

#[test]
fn a_switch_sends_the_source_back_for_the_new_track_alone() {
    let stream = movie();
    let cues = cues_of(&stream);
    let mut r = Rig::new(&stream);
    let ((_, video), (old, first)) = (r.play(0), r.play(1));
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    let played = Arc::clone(&first);
    assert!(!r.run(&mut run, || { stamps(&played).last().is_some_and(|ms| *ms >= 1_000) }));
    let position = *stamps(&first).last().expect("played") * 1_000_000;
    let line = position - 21_333_334 - 1_000_000;
    let &(at, byte) =
        cues.iter().rev().find(|(time, _)| *time <= line).expect("a cue ahead of the line");
    let (_, second) = r.play(2);
    r.p.remove(old, Leave::Cut).expect("remove");
    let targets = [(r.src, byte, Timestamp(at)), (r.demux, byte, Timestamp(position))];
    r.p.commit_at(Timestamp(position), &targets).expect("commit");
    assert!(r.run(&mut run, || false), "the run ends");
    run.finish().expect("finish");

    assert_eq!(*r.flushes.lock().unwrap(), [byte], "the source went back once, to the cue");
    assert_eq!(stamps(&video), every(40, 0), "no picture is missing and none came twice");
    let from = (line / 20_000_000) * 20;
    assert_eq!(stamps(&second), every(20, from), "the new track from its line");
    assert!(second.lock().unwrap().iter().all(|(bytes, _)| bytes[0] == 3));
    let old = stamps(&first);
    assert_eq!(
        old,
        (0..=position / 1_000_000).step_by(20).collect::<Vec<_>>(),
        "the old track stopped where it was"
    );
}

#[test]
fn a_switch_that_names_no_target_starts_the_new_track_where_the_reader_is() {
    let stream = movie();
    let mut r = Rig::new(&stream);
    let ((_, video), (_, first)) = (r.play(0), r.play(1));
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    let played = Arc::clone(&first);
    assert!(!r.run(&mut run, || { stamps(&played).last().is_some_and(|ms| *ms >= 1_000) }));
    let (_, second) = r.play(2);
    r.p.commit().expect("commit");
    assert!(r.run(&mut run, || false), "the run ends");
    run.finish().expect("finish");
    assert!(r.flushes.lock().unwrap().is_empty(), "nothing went back");
    assert_eq!(stamps(&video), every(40, 0));
    assert_eq!(stamps(&first), every(20, 0), "the track that played plays on");
    let got = stamps(&second);
    assert!(got[0] >= 1_000 && got == every(20, got[0]), "whole from where it began, {got:?}");
}

#[test]
fn a_switch_keeps_the_captions_of_a_text_track_whose_last_lies_ahead_of_the_cue() {
    let stream = movie_with_text(&[100, 1_900, 2_200]);
    let cues = cues_of(&stream);
    let mut r = Rig::new(&stream);
    let ((_, video), (old, first), (_, text)) = (r.play(0), r.play(1), r.play(3));
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    let played = Arc::clone(&first);
    assert!(!r.run(&mut run, || { stamps(&played).last().is_some_and(|ms| *ms >= 1_000) }));
    assert!(stamps(&text).is_empty(), "the caption ahead of the switch waits for its end");
    let position = *stamps(&first).last().expect("played") * 1_000_000;
    let line = position - 21_333_334 - 1_000_000;
    let &(at, byte) =
        cues.iter().rev().find(|(time, _)| *time <= line).expect("a cue ahead of the line");
    assert!(at > 100_000_000, "the cue is behind the caption, so it is not read again");
    let (_, second) = r.play(2);
    r.p.remove(old, Leave::Cut).expect("remove");
    let targets = [(r.src, byte, Timestamp(at)), (r.demux, byte, Timestamp(position))];
    r.p.commit_at(Timestamp(position), &targets).expect("commit");
    assert!(r.run(&mut run, || false), "the run ends");
    run.finish().expect("finish");

    assert_eq!(*r.flushes.lock().unwrap(), [byte]);
    assert_eq!(stamps(&text), [100, 1_900, 2_200], "the captions behind the switch come");
    assert_eq!(stamps(&video), every(40, 0), "no picture is missing and none came twice");
    assert!(!stamps(&second).is_empty());
}

#[test]
fn a_switch_puts_out_a_caption_read_again_once() {
    let stream = movie_with_text(&[800, 1_900]);
    let cues = cues_of(&stream);
    let mut r = Rig::new(&stream);
    let ((_, _video), (old, first), (_, text)) = (r.play(0), r.play(1), r.play(3));
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    let played = Arc::clone(&first);
    assert!(!r.run(&mut run, || { stamps(&played).last().is_some_and(|ms| *ms >= 1_000) }));
    let position = *stamps(&first).last().expect("played") * 1_000_000;
    let line = position - 21_333_334 - 1_000_000;
    let &(at, byte) =
        cues.iter().rev().find(|(time, _)| *time <= line).expect("a cue ahead of the line");
    assert!(at <= 800_000_000, "the caption at 800 is read again");
    let (_, _second) = r.play(2);
    r.p.remove(old, Leave::Cut).expect("remove");
    let targets = [(r.src, byte, Timestamp(at)), (r.demux, byte, Timestamp(position))];
    r.p.commit_at(Timestamp(position), &targets).expect("commit");
    assert!(r.run(&mut run, || false), "the run ends");
    run.finish().expect("finish");
    assert_eq!(stamps(&text), [800, 1_900], "once each");
}

/// Keeps every buffer it takes until it is taken out, a sink whose link waits.
struct Hold(Taken, Vec<profluens_core::buffer::Buffer>);

impl Element for Hold {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let bytes = buf.memory.data();
            if bytes != HEAD {
                self.0.lock().unwrap().push(([bytes[0], bytes[1]], buf.pts.0 / 1_000_000));
            }
            self.1.push(buf);
        }
        Ok(Flow::Ok)
    }
}

/// An audio sink that keeps its frames dries the pool, and the demuxer parks the next frame it
/// read, of the audio or of the video. The audio moved to another sink with a restart starts
/// at its line, not at a frame it parked ahead, and the video loses no picture it parked. The
/// pool sizes park a frame of each.
#[test]
fn a_restart_drops_what_the_track_parked_and_the_other_tracks_keep_theirs() {
    let stream = movie();
    let cues = cues_of(&stream);
    for slots in [24, 25, 26, 27] {
        let mut r = Rig::new(&stream);
        r.p.set_pool(4096, slots);
        let (_, video) = r.play(0);
        let pad = r.p.select(r.demux, 1).expect("select");
        let held = Taken::default();
        let hold = r.p.add(Hold(Arc::clone(&held), Vec::new()));
        r.p.link(r.demux.src(pad), hold.sink(SinkPad(0))).expect("pad -> hold");
        let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
        assert!(!r.run(&mut run, || false), "the pool runs dry, {slots} slots");
        let position = *stamps(&held).last().expect("held") * 1_000_000;
        let line = position - 21_333_334 - 1_000_000;
        let &(at, byte) =
            cues.iter().rev().find(|(time, _)| *time <= line).expect("a cue ahead of the line");
        r.p.remove(hold, Leave::Cut).expect("remove");
        let moved = Taken::default();
        let sink = r.p.add(Rec(Arc::clone(&moved)));
        r.p.link(r.demux.src(pad), sink.sink(SinkPad(0))).expect("pad -> sink");
        let targets = [(r.src, byte, Timestamp(at)), (r.demux, byte, Timestamp(position))];
        r.p.commit_at(Timestamp(position), &targets).expect("commit");
        assert!(r.run(&mut run, || false), "the run ends");
        run.finish().expect("finish");
        assert_eq!(stamps(&video), every(40, 0), "every picture once, {slots} slots");
        let from = (line / 20_000_000) * 20;
        assert_eq!(stamps(&moved), every(20, from), "from its line, {slots} slots");
    }
}

/// The seek index of the Cues is computed once a set of linked tracks. A seek computes none, a
/// link that changes the set computes it once, and one back to a set seen lately none. The
/// captions are not computed again for any of them. What preroll logs is not handed to the
/// writer, so the counts start at the start.
#[test]
fn the_cued_entries_are_computed_once_a_linked_set() {
    let stream = movie_with_text(&[100, 1_900, 2_200]);
    let cues = cues_of(&stream);
    let events = Arc::new(Events::default());
    let demux = MkvDemux::new(header_of(&stream)).with_cues(every_cue(&stream), None);
    let mut r = Rig::with(&stream, demux, Some(Arc::clone(&events)));
    let ((_, _video), (_, first), (text, _)) = (r.play(0), r.play(1), r.play(3));
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    let past = |ms: u64| {
        let played = Arc::clone(&first);
        move || stamps(&played).last().is_some_and(|at| *at >= ms)
    };
    assert!(!r.run(&mut run, past(600)));
    let position = *stamps(&first).last().expect("played") * 1_000_000;
    let &(at, byte) = cues.iter().rev().find(|(time, _)| *time <= position / 2).expect("a cue");
    r.p.commit_at(Timestamp(at), &[(r.src, byte, Timestamp(at))]).expect("seek");
    assert!(!r.run(&mut run, past(1_000)));
    assert_eq!(*r.flushes.lock().unwrap(), [byte], "the seek went back");
    assert_eq!(events.count("entries"), 1, "the set at start, none for the seek");
    let entries = |r: &Rig| r.p.index(r.demux).expect("an index").entries.clone();
    let all = entries(&r);
    r.p.remove(text, Leave::Cut).expect("remove");
    r.p.commit().expect("commit");
    assert!(!r.run(&mut run, past(1_400)));
    assert_eq!(events.count("entries"), 2, "the set without the captions");
    assert_ne!(entries(&r), all, "the set without the captions is published");
    let (_, _again) = r.play(3);
    r.p.commit().expect("commit");
    assert!(r.run(&mut run, || false), "the run ends");
    run.finish().expect("finish");
    assert_eq!(events.count("relink"), 2, "the captions unlinked and linked again");
    assert_eq!(events.count("entries"), 2, "the set at start is kept");
    assert_eq!(entries(&r), all, "the kept set is published again");
    assert_eq!(events.count("captions"), 0, "the captions once at preroll");
}
