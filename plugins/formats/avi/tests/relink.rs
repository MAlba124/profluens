// A test's own fixtures and threads, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pf_avi::AviDemux;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::time::Timestamp;
use profluens_core::topology::Leave;
use profluens_elements::io::FileSrc;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.avi");

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static SINK: ElementDesc = ElementDesc {
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

/// A buffer's pts, flags and bytes.
type Row = (Option<u64>, BufferFlags, Vec<u8>);
type Rows = Arc<Mutex<Vec<Row>>>;

struct Rec {
    rows: Rows,
    pace: Duration,
}

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut n = 0;
        let mut rows = self.rows.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rows.push((buf.pts.nanos(), buf.flags, buf.memory.data().to_vec()));
            n += 1;
        }
        drop(rows);
        std::thread::sleep(self.pace * n);
        Ok(Flow::Ok)
    }
}

fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let began = Instant::now();
    while !cond() {
        assert!(began.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn heads(rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.1.contains(BufferFlags::HEADER))
        .map(|r| r.0)
        .collect()
}

/// Each pad's buffers of the fixture played through, pulled.
fn whole() -> Vec<Vec<Row>> {
    let data = std::fs::read(TINY).expect("the fixture");
    let mut p = pf_reactor::pipeline();
    p.set_pool(1 << 20, 32);
    let src = p.add(FileSrc::new(TINY));
    let demux = AviDemux::new(data[..64 << 10].to_vec()).with_len(Some(data.len() as u64));
    let demux = p.add(demux);
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");
    let mut rows = Vec::new();
    for pad in &p.preroll().expect("preroll").srcs {
        let r = Rows::default();
        let sink = p.add(Rec { rows: Arc::clone(&r), pace: Duration::ZERO });
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).expect("pad ! sink");
        rows.push(r);
    }
    p.run().expect("run");
    rows.iter().map(|r| r.lock().unwrap().clone()).collect()
}

/// The video keyframes of `data` by its idx1, their time and chunk byte.
fn keyframes(data: &[u8]) -> Vec<(u64, u64)> {
    let (header, movi) = pf_avi::probe_header(data).expect("probe");
    let movi = movi.expect("movi");
    let at = movi.movi_end as usize;
    let size = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap()) as usize;
    let tick = header.streams[0].sample_duration_ns().expect("a frame rate");
    let video =
        data[at + 8..at + 8 + size].as_chunks::<16>().0.iter().filter(|e| &e[..4] == b"00dc");
    let key = |e: &&[u8; 16]| u32::from_le_bytes(e[4..8].try_into().unwrap()) & 0x10 != 0;
    let off = |e: &[u8; 16]| u64::from(u32::from_le_bytes(e[8..12].try_into().unwrap()));
    (0u64..)
        .zip(video)
        .filter(|(_, e)| key(e))
        .map(|(frame, e)| (frame * tick, movi.movi_data_start - 4 + off(e)))
        .collect()
}

/// What the new sink got, what the audio sink got, the VOL and the keyframe restarted at.
type Moved = (Vec<Row>, Vec<Row>, Vec<u8>, Option<(u64, Timestamp)>);

/// The video of the fixture played to one sink, moved to another mid-run with or without a
/// restart of the source at the keyframe up to where it played, as a client's switch to a
/// transcode does, the audio playing on.
fn moved(push: bool, restart: bool) -> Moved {
    let data = std::fs::read(TINY).expect("the fixture");
    let vol = {
        let (_, movi) = pf_avi::probe_header(&data).expect("probe");
        let movi = movi.expect("movi").movi_data_start as usize;
        let first = pf_avi::riff::first_chunk_of(&data[movi..], 0).expect("a video chunk");
        pf_avi::vol_prefix(first).expect("a VOL").to_vec()
    };
    let keys = keyframes(&data);
    let mut p = pf_reactor::pipeline();
    p.set_pool(1 << 20, 32);
    let src = match push {
        true => p.add(FileSrc::new(TINY).push_only()),
        false => p.add(FileSrc::new(TINY)),
    };
    let demux = AviDemux::new(data[..64 << 10].to_vec()).with_len(Some(data.len() as u64));
    let demux = p.add(demux);
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");
    let added = p.preroll().expect("preroll").srcs;
    let pace = Duration::from_millis(3);
    let mut sinks: Vec<(ElementId, SrcPad, Rows)> = Vec::new();
    for pad in &added {
        let rows = Rows::default();
        let sink = p.add(Rec { rows: Arc::clone(&rows), pace });
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).expect("pad ! sink");
        sinks.push((sink, pad.pad, rows));
    }
    let control = p.control();
    let run = std::thread::spawn(move || p.run());
    let (old, pad, played) = sinks.swap_remove(0);
    until("the video under way", || played.lock().unwrap().len() > 30);
    let new = Rows::default();
    let rows = Arc::clone(&new);
    let edit = control.edit(move |p| {
        let position = played.lock().unwrap().last().and_then(|r| r.0).expect("a stamp");
        let to = Timestamp(position);
        // a pushed file has no idx1 read, the keyframe is the test's
        let key = keys.iter().rev().find(|k| k.0 <= position).map(|k| (k.1, Timestamp(k.0)));
        let index = p.index(demux).expect("the index");
        let at = if push { key } else { index.restart_for(0, to) };
        assert_eq!(at, key, "the published keyframe");
        p.remove(old, Leave::Cut).expect("remove");
        let sink = p.add(Rec { rows, pace });
        p.link(demux.src(pad), sink.sink(SinkPad(0))).expect("pad ! new sink");
        let (byte, key) = at.expect("a keyframe");
        let targets = [(src, byte, key), (demux, byte, key)];
        let targets = if restart { &targets[..] } else { &[] };
        p.commit_at(to, targets).expect("commit");
        restart.then_some((byte, key))
    });
    let at = edit.expect("the edit ran");
    let _ = run.join().expect("the run thread");
    let rows = new.lock().unwrap().clone();
    let audio = sinks[0].2.lock().unwrap().clone();
    (rows, audio, vol, at)
}

#[test]
fn a_restart_sends_the_vol_ahead_of_the_keyframe_stamped_as_its_own_and_drops_the_audio_read_again()
{
    let all = whole();
    for push in [false, true] {
        let (rows, audio, vol, at) = moved(push, true);
        let (_, key) = at.expect("a restart");
        assert!(rows.len() > 2, "push {push}, {} rows", rows.len());
        assert_eq!(heads(&rows), [0], "push {push}, the VOL once, ahead of the rest");
        assert_eq!(rows[0].2, vol, "push {push}");
        assert_eq!(rows[0].0, rows[1].0, "push {push}, stamped as the picture behind it");
        assert!(rows[1].1.contains(BufferFlags::KEYFRAME), "push {push}, the keyframe first");
        assert_eq!(rows[1].0, key.nanos(), "push {push}, at its own time");
        let from = all[0].iter().position(|r| r.0 == key.nanos()).expect("the keyframe");
        assert_eq!(rows[1..], all[0][from..], "push {push}, each picture on from it once");
        assert_eq!(audio, all[1], "push {push}, the audio frames once, as played through");
    }
}

#[test]
fn a_pad_linked_again_without_a_restart_gets_the_vol_ahead_of_what_follows() {
    let all = whole();
    for push in [false, true] {
        let (rows, audio, vol, _) = moved(push, false);
        assert!(rows.len() > 2, "push {push}, {} rows", rows.len());
        assert_eq!(heads(&rows), [0], "push {push}, the VOL once, ahead of the rest");
        assert_eq!(rows[0].2, vol, "push {push}");
        assert_eq!(rows[0].0, rows[1].0, "push {push}, stamped as the picture behind it");
        let tail = &all[0][all[0].len() + 1 - rows.len()..];
        assert_eq!(rows[1..], *tail, "push {push}, the pictures on from where it was");
        assert_eq!(audio, all[1], "push {push}");
    }
}
