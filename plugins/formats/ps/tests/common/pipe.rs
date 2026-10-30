//! A pushing source and recording sinks around `psdemux` in a pipeline.

#![allow(clippy::disallowed_methods)] // test rig

use std::sync::{Arc, Mutex};

use pf_ps::PsDemux;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::{Ctx, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "piecesrc",
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

pub struct PieceSrc {
    pub data: Vec<u8>,
    pub chunk: usize,
    pub at: usize,
}

impl Element for PieceSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.at >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let n = self.chunk.min(buf.memory.capacity()).min(self.data.len() - self.at);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.at += n;
        Ok(Flow::Ok)
    }
}

static ANY: [OfferDesc; 2] =
    [OfferDesc { family: "audio/raw", fields: &PCM_FIELDS }, OfferDesc::wildcard()];
static S16: [ValueDesc; 1] = [ValueDesc::Id("s16")];
static RAW_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: "sample", allowed: ConstraintDesc::Set(&S16), preferred: None }];
static PCM_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "sample", allowed: ConstraintDesc::Any, preferred: None },
];
static RAW: [OfferDesc; 3] = [
    OfferDesc::any("video/raw"),
    OfferDesc { family: "audio/raw", fields: &RAW_FIELDS },
    OfferDesc::wildcard(),
];
static REC_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static RAW_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW }];

const fn sink(name: &'static str, pads: &'static [PadDesc]) -> ElementDesc {
    ElementDesc {
        name,
        sinks: pads,
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

static REC_DESC: ElementDesc = sink("recsink", &REC_PADS);
static RAW_DESC: ElementDesc = sink("rawsink", &RAW_PADS);

/// Family, rate, channels and sample of a format change.
pub type Format = (String, Option<i64>, Option<i64>, Option<String>);

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub units: Vec<(Option<u64>, usize, u32)>,
    pub bytes: Vec<u8>,
    pub formats: Vec<Format>,
}

impl Recorded {
    pub fn keys(&self) -> impl Iterator<Item = &(Option<u64>, usize, u32)> {
        self.units.iter().filter(|u| u.2 & BufferFlags::KEYFRAME.0 != 0)
    }
}

pub struct RecSink {
    pub rec: Arc<Mutex<Recorded>>,
    pub raw: bool,
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        if self.raw { &RAW_DESC } else { &REC_DESC }
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().expect("rec");
        while let Some(buf) = inputs.pop() {
            rec.bytes.extend_from_slice(buf.memory.data());
            rec.units.push((buf.pts.nanos(), buf.memory.len(), buf.flags.0));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(format) = event {
            let get = |name: &str| ctx.field_id(name).and_then(|id| format.get(id));
            let int = |name: &str| match get(name) {
                Some(Value::Int(v)) => Some(v),
                _ => None,
            };
            let id = match get("sample") {
                Some(Value::Id(v)) => ctx.value_name(v).map(str::to_string),
                _ => None,
            };
            let family = ctx.family_name(format.family).unwrap_or("?").to_string();
            self.rec.lock().expect("rec").formats.push((family, int("rate"), int("channels"), id));
        }
        Ok(())
    }
}

pub type Out = Vec<(StreamKind, &'static str, Recorded)>;

pub fn recorder() -> (Arc<Mutex<Recorded>>, RecSink) {
    let rec = Arc::new(Mutex::new(Recorded::default()));
    (Arc::clone(&rec), RecSink { rec, raw: false })
}

pub enum Link {
    Rec,
    Through(ElementId, SinkPad, SrcPad),
    Not,
}

/// Every stream into a recorder, through a decoder or left unlinked, as `link` says by its index.
pub fn run_with(
    data: &[u8],
    demux: PsDemux,
    chunk: usize,
    link: &mut dyn FnMut(usize, &mut Pipeline) -> Link,
) -> Out {
    let mut p = Pipeline::new();
    let src = p.add(PieceSrc { data: data.to_vec(), chunk, at: 0 });
    let demux_id = p.add(demux);
    p.link(src.src(SrcPad(0)), demux_id.sink(PsDemux::IN)).expect("src to demux");
    p.set_select_all(false);
    p.preroll().expect("preroll");
    let streams: Vec<(StreamKind, &'static str)> =
        p.streams(demux_id).iter().map(|s| (s.kind, s.codec)).collect();
    let mut out = Vec::new();
    for n in 0..streams.len() {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let (through, raw) = match link(n, &mut p) {
            Link::Not => {
                out.push((n, rec));
                continue;
            }
            Link::Rec => (None, false),
            Link::Through(dec, sink_pad, src_pad) => (Some((dec, sink_pad, src_pad)), true),
        };
        let pad = p.select(demux_id, n as u32).expect("select");
        let s = p.add(RecSink { rec: Arc::clone(&rec), raw });
        match through {
            Some((dec, sink_pad, src_pad)) => {
                p.link(demux_id.src(pad), dec.sink(sink_pad)).expect("demux to decoder");
                p.link(dec.src(src_pad), s.sink(SinkPad(0))).expect("decoder to sink");
            }
            None => {
                p.link(demux_id.src(pad), s.sink(SinkPad(0))).expect("demux to sink");
            }
        }
        out.push((n, rec));
    }
    p.run().expect("the run ends");
    out.into_iter()
        .map(|(n, rec)| (streams[n].0, streams[n].1, rec.lock().expect("rec").clone()))
        .collect()
}

pub fn run(data: &[u8], chunk: usize) -> Out {
    run_with(data, PsDemux::new(data.to_vec()), chunk, &mut |_, _| Link::Rec)
}
