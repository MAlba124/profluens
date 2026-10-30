//! A `V_VP8` track muxed, demuxed and decoded in a real pipeline matches the reference decode
//! bit-exact.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libav::LibavVp8Dec as Vp8Dec;
use pf_mkv::ebml::id;
use pf_mkv::{MatroskaWriter, MkvDemux, TrackConfig};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

const W: u32 = 64;
const H: u32 = 64;
const FRAME_NS: u64 = 33_000_000;

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "bytesrc",
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

struct ByteSrc {
    data: Vec<u8>,
    chunk: usize,
    pos: usize,
}

impl ByteSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for ByteSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.pos = 0;
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let cap = buf.memory.capacity();
        let n = self.chunk.min(cap).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

static PIXFMTS: [ValueDesc; 1] = [ValueDesc::Id("i420")];
static RAW_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Set(&PIXFMTS), preferred: None },
];
static RAW_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &RAW_FIELDS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_OFFERS }];
static RECORD_DESC: ElementDesc = ElementDesc {
    name: "planerecordsink",
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

#[derive(Default)]
struct Recorded {
    frames: Vec<(Timestamp, Vec<u8>)>,
    format: Option<(i64, i64, String)>,
}

struct PlaneRecordSink {
    shared: Arc<Mutex<Recorded>>,
}

impl PlaneRecordSink {
    const IN: SinkPad = SinkPad(0);

    fn new() -> (Self, Arc<Mutex<Recorded>>) {
        let shared = Arc::new(Mutex::new(Recorded::default()));
        (Self { shared: Arc::clone(&shared) }, shared)
    }
}

impl Element for PlaneRecordSink {
    fn desc(&self) -> &'static ElementDesc {
        &RECORD_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.shared.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.frames.push((buf.pts, buf.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(_) = event {
            let f = ctx.negotiated_sink(Self::IN).expect("format installed").clone();
            let get = |name: &str| ctx.field_id(name).and_then(|id| f.get(id));
            let (Some(Value::Int(w)), Some(Value::Int(h)), Some(Value::Id(pf))) =
                (get("width"), get("height"), get("pixfmt"))
            else {
                panic!("announced format missing width/height/pixfmt");
            };
            let name = ctx.value_name(pf).expect("pixfmt name interned").to_string();
            self.shared.lock().unwrap().format = Some((w, h, name));
        }
        Ok(())
    }
}

const IVF: &[u8] = include_bytes!("fixtures/vp8-golden-update-cycle/input.ivf");
const YUV: &[u8] = include_bytes!("fixtures/vp8-golden-update-cycle/expected.yuv");

fn frames() -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut at = 32;
    while at + 12 <= IVF.len() {
        let size = u32::from_le_bytes([IVF[at], IVF[at + 1], IVF[at + 2], IVF[at + 3]]) as usize;
        at += 12;
        frames.push(IVF[at..at + size].to_vec());
        at += size;
    }
    frames
}

fn reference() -> Vec<Vec<u8>> {
    YUV.chunks((W * H * 3 / 2) as usize).map(<[u8]>::to_vec).collect()
}

fn mux_vp8(frames: &[Vec<u8>]) -> Vec<u8> {
    let track = TrackConfig::vp8(1, W, H);
    let mut w = MatroskaWriter::new(vec![track]);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    for (i, f) in frames.iter().enumerate() {
        // The frame tag low bit is clear on a keyframe (RFC 6386 section 9.1).
        w.write_frame(&mut out, 1, i as u64 * FRAME_NS, f, f[0] & 1 == 0).unwrap();
    }
    w.finalize(&mut out);
    out
}

fn header_prefix(stream: &[u8]) -> Vec<u8> {
    let cluster =
        stream.windows(4).position(|w| w == id::CLUSTER).expect("stream has at least one Cluster");
    stream[..cluster].to_vec()
}

fn run_playback(stream: Vec<u8>, chunk: usize) -> Arc<Mutex<Recorded>> {
    let header = header_prefix(&stream);
    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: stream, chunk, pos: 0 });
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(ByteSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 1, "one V_VP8 track → one src pad");

    let (sink, recorded) = PlaneRecordSink::new();
    let dec = p.add(Vp8Dec::new());
    let snk = p.add(sink);
    let ap = &added[0];
    p.link(ap.element.src(ap.pad), dec.sink(Vp8Dec::IN)).expect("demux pad -> vp8dec");
    p.link(dec.src(Vp8Dec::OUT), snk.sink(PlaneRecordSink::IN)).expect("vp8dec -> sink");

    p.run().expect("run");
    recorded
}

#[test]
fn vp8_in_mkv_decodes_bit_exact_with_pts() {
    let (frames, reference) = (frames(), reference());
    assert_eq!(frames.len(), 5);
    let stream = mux_vp8(&frames);

    let recorded = run_playback(stream, 500);
    let rec = recorded.lock().unwrap();

    assert_eq!(rec.frames.len(), 5, "every muxed VP8 frame decoded back");
    for (i, ((pts, planes), want)) in rec.frames.iter().zip(&reference).enumerate() {
        assert_eq!(
            *pts,
            Timestamp::from_nanos(i as u64 * FRAME_NS),
            "frame {i} pts survives the MKV round-trip"
        );
        assert_eq!(planes, want, "frame {i} planes bit-exact vs the reference decode");
    }

    let (w, h, pf) = rec.format.clone().expect("format announced downstream of the decoder");
    assert_eq!((w, h), (W as i64, H as i64));
    assert_eq!(pf, "i420");
}

#[test]
fn vp8_in_mkv_whole_stream_in_one_chunk() {
    let (frames, reference) = (frames(), reference());
    let stream = mux_vp8(&frames);

    let recorded = run_playback(stream.clone(), stream.len().max(1));
    let rec = recorded.lock().unwrap();
    let planes: Vec<&Vec<u8>> = rec.frames.iter().map(|f| &f.1).collect();
    assert!(planes == reference.iter().collect::<Vec<_>>(), "{} frames", planes.len());
}
