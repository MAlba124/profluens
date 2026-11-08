//! The fixtures, their ffprobe packet tables and a pipeline that records what `flvdemux` puts out.

#![allow(clippy::disallowed_methods, dead_code)] // test fixtures

use std::sync::{Arc, Mutex};

use pf_flv::FlvDemux;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::{Ctx, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::{Index, Pipeline};

pub const MS: u64 = 1_000_000;

pub fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}.flv", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The file byte and type of each tag, walked by their sizes.
pub fn tags(data: &[u8]) -> Vec<(usize, u8)> {
    let mut at = 13;
    let mut out = Vec::new();
    while let Some(h) = data.get(at..at + 11) {
        out.push((at, h[0]));
        at += 15 + h[1..4].iter().fold(0usize, |n, b| n << 8 | usize::from(*b));
    }
    out
}

/// A row of `ffprobe -show_entries packet=stream_index,pts,dts,size,pos,flags`, times in ms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    pub stream: usize,
    pub pts: i64,
    pub dts: i64,
    pub size: usize,
    pub pos: u64,
    pub key: bool,
}

pub fn packets(name: &str) -> Vec<Packet> {
    let path = format!("{}/tests/fixtures/{name}.packets.csv", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    text.lines()
        .map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            let num = |s: &str| s.parse::<i64>().unwrap_or_else(|_| panic!("{line}"));
            Packet {
                stream: num(f[0]) as usize,
                pts: num(f[1]),
                dts: num(f[2]),
                size: num(f[3]) as usize,
                pos: num(f[4]) as u64,
                key: f[5].starts_with('K'),
            }
        })
        .collect()
}

pub fn of(rows: &[Packet], stream: usize) -> Vec<Packet> {
    rows.iter().copied().filter(|p| p.stream == stream).collect()
}

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

impl PieceSrc {
    pub const OUT: SrcPad = SrcPad(0);
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
        ctx.out(Self::OUT).push(buf);
        self.at += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let (Event::FlushStart, Some(target)) = (event, ctx.seek_target()) {
            self.at = target.to_byte as usize;
        }
        Ok(())
    }
}

static ANY: ConstraintDesc = ConstraintDesc::Any;
static FIELDS: [FieldDesc; 5] = [
    FieldDesc { field: "rate", allowed: ANY, preferred: None },
    FieldDesc { field: "channels", allowed: ANY, preferred: None },
    FieldDesc { field: "sample", allowed: ANY, preferred: None },
    FieldDesc { field: "width", allowed: ANY, preferred: None },
    FieldDesc { field: "height", allowed: ANY, preferred: None },
];
static REC_OFFERS: [OfferDesc; 9] = [
    OfferDesc { family: "h264/annexb", fields: &FIELDS },
    OfferDesc { family: "h265/annexb", fields: &FIELDS },
    OfferDesc { family: "av1", fields: &FIELDS },
    OfferDesc { family: "vp9", fields: &FIELDS },
    OfferDesc { family: "vp8", fields: &FIELDS },
    OfferDesc { family: "aac", fields: &FIELDS },
    OfferDesc { family: "mp3", fields: &FIELDS },
    OfferDesc { family: "audio/raw", fields: &FIELDS },
    OfferDesc::any("bytes"),
];
static REC_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &REC_OFFERS }];
static REC_DESC: ElementDesc = ElementDesc {
    name: "recsink",
    sinks: &REC_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Format {
    pub family: String,
    pub rate: Option<i64>,
    pub channels: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub units: Vec<(Option<u64>, usize, u32)>,
    pub bytes: Vec<u8>,
    pub formats: Vec<Format>,
}

impl Recorded {
    /// The units but for the configuration that leads them.
    pub fn frames(&self) -> Vec<(Option<u64>, usize, u32)> {
        self.units.iter().copied().filter(|u| u.2 & BufferFlags::HEADER.0 == 0).collect()
    }
}

pub struct RecSink {
    pub rec: Arc<Mutex<Recorded>>,
}

impl RecSink {
    pub const IN: SinkPad = SinkPad(0);
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &REC_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.bytes.extend_from_slice(buf.memory.data());
            rec.units.push((buf.pts.nanos(), buf.memory.len(), buf.flags.0));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(format) = event {
            let int = |name: &str| match ctx.field_id(name).and_then(|id| format.get(id)) {
                Some(Value::Int(v)) => Some(v),
                _ => None,
            };
            let family = ctx.family_name(format.family).unwrap_or("?").to_string();
            let (rate, channels) = (int("rate"), int("channels"));
            let (width, height) = (int("width"), int("height"));
            self.rec.lock().unwrap().formats.push(Format { family, rate, channels, width, height });
        }
        Ok(())
    }
}

/// Keeps every buffer it takes until it is taken out, a sink whose link waits.
pub struct Hold {
    pub rec: Arc<Mutex<Recorded>>,
    pub kept: Vec<profluens_core::buffer::Buffer>,
}

impl Element for Hold {
    fn desc(&self) -> &'static ElementDesc {
        &REC_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.units.push((buf.pts.nanos(), buf.memory.len(), buf.flags.0));
            self.kept.push(buf);
        }
        Ok(Flow::Ok)
    }
}

pub struct Out {
    pub streams: Vec<(StreamKind, &'static str, Recorded)>,
    pub warnings: Vec<String>,
    pub index: Option<Arc<Index>>,
}

/// `data` in pieces of `chunk` through `flvdemux` made from `head`, every stream recorded.
pub fn run(data: &[u8], head: &[u8], chunk: usize) -> Out {
    run_with(data, FlvDemux::new(head.to_vec()).with_len(data.len() as u64), chunk)
}

pub fn run_with(data: &[u8], demux: FlvDemux, chunk: usize) -> Out {
    let mut p = Pipeline::new();
    let src = p.add(PieceSrc { data: data.to_vec(), chunk, at: 0 });
    let demux = p.add(demux);
    p.link(src.src(PieceSrc::OUT), demux.sink(FlvDemux::IN)).expect("src to demux");
    p.set_select_all(true);
    let added = p.preroll().expect("preroll").srcs;
    let streams: Vec<(StreamKind, &'static str)> =
        p.streams(demux).iter().map(|s| (s.kind, s.codec)).collect();
    let pads: Vec<(ElementId, SrcPad)> = added.iter().map(|a| (a.element, a.pad)).collect();
    let mut recs = Vec::new();
    for (element, pad) in pads {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let sink = p.add(RecSink { rec: Arc::clone(&rec) });
        p.link(element.src(pad), sink.sink(RecSink::IN)).expect("pad to sink");
        recs.push(rec);
    }
    let tap = p.tap_handle();
    p.run().expect("the run ends");
    let mut warnings = Vec::new();
    while let Some(message) = p.bus().try_recv() {
        if let BusMessage::Warning { error, .. } = message {
            warnings.push(error.to_string());
        }
    }
    let streams = streams
        .into_iter()
        .zip(recs)
        .map(|((kind, codec), rec)| (kind, codec, rec.lock().unwrap().clone()))
        .collect();
    Out { streams, warnings, index: tap.index(demux) }
}

pub type Streams = Vec<(StreamKind, &'static str)>;

pub fn preroll(head: &[u8]) -> Result<(Streams, Vec<String>), String> {
    let mut p = Pipeline::new();
    let src = p.add(PieceSrc { data: head.to_vec(), chunk: 4096, at: 0 });
    let demux = p.add(FlvDemux::new(head.to_vec()));
    p.link(src.src(PieceSrc::OUT), demux.sink(FlvDemux::IN)).expect("src to demux");
    let result = p.preroll().map_err(|e| e.to_string());
    let mut warnings = Vec::new();
    while let Some(message) = p.bus().try_recv() {
        if let BusMessage::Warning { error, .. } = message {
            warnings.push(error.to_string());
        }
    }
    result?;
    Ok((p.streams(demux).iter().map(|s| (s.kind, s.codec)).collect(), warnings))
}
