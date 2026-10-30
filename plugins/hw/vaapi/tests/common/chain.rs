//! Shared helpers for the vaapipostproc chain tests.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc, VaapiH265Dec, VaapiH265Enc};

pub const FRAME_NS: u64 = 40_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn family(self) -> &'static str {
        match self {
            Codec::H264 => "h264/annexb",
            Codec::H265 => "h265/annexb",
        }
    }

    fn opens_picture(self, data: &[u8], at: usize) -> bool {
        if at + 2 >= data.len() {
            return false;
        }
        match self {
            Codec::H264 => matches!(data[at] & 0x1F, 1 | 5) && data[at + 1] & 0x80 != 0,
            Codec::H265 => (data[at] >> 1) & 0x3F < 32 && data[at + 2] & 0x80 != 0,
        }
    }

    pub fn units(self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut cuts = vec![0];
        let (mut open, mut head) = (false, 0);
        for i in 0..data.len().saturating_sub(3) {
            if data[i..i + 3] != [0, 0, 1] {
                continue;
            }
            let code = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            if self.opens_picture(data, i + 3) {
                if open {
                    cuts.push(if head == usize::MAX { code } else { head });
                }
                (open, head) = (true, usize::MAX);
            } else if head == usize::MAX {
                head = code;
            }
        }
        let ends = cuts.iter().skip(1).copied().chain([data.len()]);
        cuts.iter().zip(ends).map(|(&a, b)| data[a..b].to_vec()).collect()
    }

    pub fn decoder(self, channel: Option<&Arc<GpuFrameChannel>>) -> Box<dyn Element> {
        match (self, channel) {
            (Codec::H264, Some(c)) => Box::new(VaapiH264Dec::new_zerocopy(Arc::clone(c))),
            (Codec::H265, Some(c)) => Box::new(VaapiH265Dec::new_zerocopy(Arc::clone(c))),
            (Codec::H264, None) => Box::new(VaapiH264Dec::new()),
            (Codec::H265, None) => Box::new(VaapiH265Dec::new()),
        }
    }

    pub fn encoder(self, channel: Option<&Arc<GpuFrameChannel>>) -> Box<dyn Element> {
        match (self, channel) {
            (Codec::H264, Some(c)) => {
                Box::new(VaapiH264Enc::new_zerocopy(Arc::clone(c)).with_qp(26).with_gop(30))
            }
            (Codec::H265, Some(c)) => {
                Box::new(VaapiH265Enc::new_zerocopy(Arc::clone(c)).with_qp(26).with_gop(30))
            }
            (Codec::H264, None) => Box::new(VaapiH264Enc::new().with_qp(26).with_gop(30)),
            (Codec::H265, None) => Box::new(VaapiH265Enc::new().with_qp(26).with_gop(30)),
        }
    }
}

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    inputs: InputPolicy,
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static STREAMS: [OfferDesc; 3] =
    [OfferDesc::any("h264/annexb"), OfferDesc::any("h265/annexb"), OfferDesc::any("av1")];
static TAKEN: [OfferDesc; 3] =
    [OfferDesc::any("h264/annexb"), OfferDesc::any("h265/annexb"), OfferDesc::any("video/raw")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &STREAMS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &TAKEN }];
static SRC_DESC: ElementDesc = desc("unitsrc", &[], &SRC_PADS, InputPolicy::None);
static SINK_DESC: ElementDesc = desc("collect", &SINK_PADS, &[], InputPolicy::Single);

pub struct UnitSrc {
    pub units: Vec<Vec<u8>>,
    pub next: usize,
}

impl UnitSrc {
    pub fn new(units: Vec<Vec<u8>>) -> UnitSrc {
        UnitSrc { units, next: 0 }
    }
}

impl Element for UnitSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let Some(unit) = self.units.get(self.next) else {
            return Ok(Flow::Eos);
        };
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.as_mut_full()[..unit.len()].copy_from_slice(unit);
        buf.memory.set_len(unit.len());
        buf.pts = Timestamp::from_nanos(self.next as u64 * FRAME_NS);
        ctx.out(SrcPad(0)).push(buf);
        self.next += 1;
        Ok(Flow::Ok)
    }
}

pub struct Collect(pub mpsc::Sender<(Timestamp, Vec<u8>)>);

impl Element for Collect {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(b) = inputs.pop() {
            let _ = self.0.send((b.pts, b.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }
}

pub fn warnings(p: &Pipeline) -> Vec<String> {
    let mut w = Vec::new();
    while let Some(m) = p.bus().try_recv() {
        if let BusMessage::Warning { error, .. } = m {
            w.push(format!("{error}"));
        }
    }
    w.retain(|w| !w.starts_with("format announcement 'video/gpu'"));
    w
}

pub fn decode(codec: Codec, stream: &[(Timestamp, Vec<u8>)]) -> (Vec<Vec<u8>>, Vec<String>) {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(stream.iter().map(|u| u.1.clone()).collect()));
    let dec = p.add_boxed(codec.decoder(None));
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("decoder to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.run().expect("the run ends");
    let warnings = warnings(&p);
    drop(p);
    (rx.try_iter().map(|f| f.1).collect(), warnings)
}
