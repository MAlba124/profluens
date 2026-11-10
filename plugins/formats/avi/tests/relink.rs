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

/// The video of the fixture played to one sink, moved to another mid-run with or without a
/// restart of the source at its keyframe, as a client's switch to a transcode does. What the
/// new sink got, and the VOL.
fn moved(push: bool, restart: bool) -> (Vec<Row>, Vec<u8>) {
    let data = std::fs::read(TINY).expect("the fixture");
    let vol = {
        let (_, movi) = pf_avi::probe_header(&data).expect("probe");
        let movi = movi.expect("movi").movi_data_start as usize;
        let first = pf_avi::riff::first_chunk_of(&data[movi..], 0).expect("a video chunk");
        pf_avi::vol_prefix(first).expect("a VOL").to_vec()
    };
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
        let index = p.index(demux).expect("the index");
        // a pushed file has no idx1, it restarts at the estimate
        let to = Timestamp(position);
        let (byte, at) = index.restart_for(0, to).or(index.resolve(to)).expect("a restart");
        p.remove(old, Leave::Cut).expect("remove");
        let sink = p.add(Rec { rows, pace });
        p.link(demux.src(pad), sink.sink(SinkPad(0))).expect("pad ! new sink");
        let targets = [(src, byte, at), (demux, byte, at)];
        let targets = if restart { &targets[..] } else { &[] };
        p.commit_at(Timestamp(position), targets).expect("commit");
        (position, byte, at)
    });
    let (position, byte, at) = edit.expect("the edit ran");
    let _ = run.join().expect("the run thread");
    eprintln!("moved at {position} ns, restart {byte} at {at:?}");
    let rows = new.lock().unwrap().clone();
    (rows, vol)
}

#[test]
fn a_restart_sends_the_vol_ahead_of_the_first_picture_read_again_and_not_again() {
    for push in [false, true] {
        let (rows, vol) = moved(push, true);
        assert!(rows.len() > 2, "push {push}, {} rows", rows.len());
        assert_eq!(heads(&rows), [0], "push {push}, the VOL once, ahead of the rest");
        assert_eq!(rows[0].2, vol, "push {push}");
        assert_eq!(rows[0].0, rows[1].0, "push {push}, stamped as the picture behind it");
        let key = rows[1].1.contains(BufferFlags::KEYFRAME);
        assert!(push || key, "the keyframe the index restarts at first");
    }
}

#[test]
fn a_pad_linked_again_without_a_restart_gets_the_vol_ahead_of_what_follows() {
    for push in [false, true] {
        let (rows, vol) = moved(push, false);
        assert!(rows.len() > 2, "push {push}, {} rows", rows.len());
        assert_eq!(heads(&rows), [0], "push {push}, the VOL once, ahead of the rest");
        assert_eq!(rows[0].2, vol, "push {push}");
        assert_eq!(rows[0].0, rows[1].0, "push {push}, stamped as the picture behind it");
    }
}
