//! Shared hardware test helpers. Every test skips without the runtime.
#![allow(dead_code, clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::mpsc;

use pf_harness::Harness;
pub use pf_vpl::probe::skip;
use pf_vpl::{VplCaps, VplDec, probe};
use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
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

pub const FRAME_NS: u64 = 40_000_000;
pub const SLOT: usize = 8 * 1024 * 1024;

pub const IPB_HEVC: &[u8] = include_bytes!("../../../vaapi/tests/fixtures/ipb_320x240.hevc");
pub const P_HEVC: &[u8] = include_bytes!("../../../vaapi/tests/fixtures/p_320x240.hevc");
pub const REFS6_HEVC: &[u8] = include_bytes!("../../../vaapi/tests/fixtures/refs6_320x240.hevc");
pub const HDR10_HEVC: &[u8] = include_bytes!("../../../vaapi/tests/fixtures/hdr10_128x128.hevc");
pub const IPB_H264: &[u8] = include_bytes!("../../../vaapi/tests/fixtures/ipb_320x240.h264");
pub const P_H264: &[u8] = include_bytes!("../../../vaapi/tests/fixtures/p_320x240.h264");
pub const FEATURES_VP9: &[u8] =
    include_bytes!("../../../vaapi/tests/fixtures/features_512x288.ivf");
pub const TILES_AV1: &[u8] =
    include_bytes!("../../../../video/av1/tests/fixtures/tiles_256x128.ivf");
pub const INTER_AV1: &[u8] = include_bytes!("../../../../video/av1/tests/fixtures/inter_64x48.ivf");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    H264,
    H265,
    Vp9,
    Av1,
}

impl Codec {
    pub fn family(self) -> &'static str {
        match self {
            Codec::H264 => "h264/annexb",
            Codec::H265 => "h265/annexb",
            Codec::Vp9 => "vp9",
            Codec::Av1 => "av1",
        }
    }

    fn opens_picture(self, data: &[u8], at: usize) -> bool {
        if at + 2 >= data.len() {
            return false;
        }
        match self {
            Codec::H264 => matches!(data[at] & 0x1F, 1 | 5) && data[at + 1] & 0x80 != 0,
            Codec::H265 => (data[at] >> 1) & 0x3F < 32 && data[at + 2] & 0x80 != 0,
            _ => false,
        }
    }

    pub fn units(self, data: &[u8]) -> Vec<Vec<u8>> {
        match self {
            Codec::Vp9 | Codec::Av1 => return ivf_units(data),
            _ => {}
        }
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

    pub fn software(self) -> Box<dyn Element> {
        match self {
            Codec::H264 => Box::new(pf_libav::LibavH264Dec::new()),
            Codec::H265 => Box::new(pf_libav::LibavH265Dec::new()),
            Codec::Vp9 => Box::new(pf_libav::LibavVp9Dec::new()),
            Codec::Av1 => Box::new(pf_av1::Av1Dec::new()),
        }
    }
}

pub fn ivf_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    if data.len() < 32 || &data[..4] != b"DKIF" {
        return frames;
    }
    let mut off = usize::from(u16::from_le_bytes([data[6], data[7]]));
    while off + 12 <= data.len() {
        let size =
            u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        let start = off + 12;
        let Some(frame) = data.get(start..start + size) else {
            break;
        };
        frames.push(frame.to_vec());
        off = start + size;
    }
    frames
}

pub fn hw(test: &str) -> Option<&'static VplCaps> {
    match probe() {
        Some(caps) => Some(caps),
        None => {
            skip(
                test,
                "no VPL hardware runtime (the dispatcher found no implementation for this GPU, Gen12 and later only)",
            );
            None
        }
    }
}

pub fn hw_decodes(test: &str, codec: Codec) -> Option<&'static VplCaps> {
    let caps = hw(test)?;
    if !caps.supports(codec.family()) {
        skip(test, &format!("the runtime does not decode {}", codec.family()));
        return None;
    }
    Some(caps)
}

pub struct Decoded {
    pub frames: Vec<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub pixfmt: String,
    pub warnings: Vec<String>,
}

pub fn decode_with(decoder: Box<dyn Element>, family: &str, units: &[Vec<u8>]) -> Decoded {
    let mut h = Harness::with_slot_size(BoxedElement(decoder), SLOT).real_wakes();
    h.fix_sink_format(SinkPad(0), family, &[]);
    h.start().expect("start");
    let mut frames = Vec::new();
    for (i, u) in units.iter().enumerate() {
        let mut buf = h.alloc(u);
        buf.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        buf.duration = Timestamp::from_nanos(FRAME_NS);
        h.push(buf).expect("push");
        while let Some(f) = h.pull(SrcPad(0)) {
            frames.push(f.memory.data().to_vec());
        }
    }
    frames.extend(h.eos().expect("eos").into_iter().map(|f| f.memory.data().to_vec()));
    let mut warnings = Vec::new();
    for m in h.bus_messages() {
        match m {
            BusMessage::Warning { error, .. } | BusMessage::Error { error, .. } => {
                warnings.push(format!("{error}"))
            }
            _ => {}
        }
    }
    let (mut width, mut height, mut pixfmt) = (0, 0, String::new());
    if let Some(a) = h.announced() {
        let v = h.vocabulary();
        let int = |name: &str| match a.get(v.field_id(name).unwrap()) {
            Some(profluens_core::format::Value::Int(x)) => x as u32,
            _ => 0,
        };
        width = int("width");
        height = int("height");
        if let Some(profluens_core::format::Value::Id(id)) = a.get(v.field_id("pixfmt").unwrap()) {
            pixfmt = v.value_name(id).unwrap().to_string();
        }
    }
    Decoded { frames, width, height, pixfmt, warnings }
}

pub struct BoxedElement(pub Box<dyn Element>);

impl Element for BoxedElement {
    fn desc(&self) -> &'static ElementDesc {
        self.0.desc()
    }
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.0.preroll(ctx)
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.0.start(ctx)
    }
    fn process(&mut self, ctx: &mut Ctx, inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.0.process(ctx, inputs)
    }
    fn event(&mut self, ctx: &mut Ctx, event: &profluens_core::event::Event) -> Result<(), Error> {
        self.0.event(ctx, event)
    }
    fn stop(&mut self, ctx: &mut Ctx) {
        self.0.stop(ctx)
    }
}

pub fn decode_hw(codec: Codec, units: &[Vec<u8>]) -> Decoded {
    decode_with(Box::new(VplDec::new()), codec.family(), units)
}

pub fn decode_sw(codec: Codec, units: &[Vec<u8>]) -> Decoded {
    decode_with(codec.software(), codec.family(), units)
}

pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "planes of one length");
    let mse: f64 =
        a.iter().zip(b).map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2)).sum::<f64>()
            / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

/// p010 words, the ten bits high.
pub fn psnr16(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let words = |p: &[u8]| {
        p.as_chunks::<2>()
            .0
            .iter()
            .map(|c| f64::from(u16::from_le_bytes(*c) >> 6))
            .collect::<Vec<f64>>()
    };
    let (a, b) = (words(a), words(b));
    let mse: f64 = a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (1023.0 * 1023.0 / mse).log10() }
}

pub fn ssim_luma(a: &[u8], b: &[u8], width: usize, height: usize) -> f64 {
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let (mut sum, mut n) = (0.0, 0usize);
    for y in (0..height.saturating_sub(7)).step_by(4) {
        for x in (0..width.saturating_sub(7)).step_by(4) {
            let (mut ma, mut mb) = (0.0, 0.0);
            for j in 0..8 {
                for i in 0..8 {
                    ma += f64::from(a[(y + j) * width + x + i]);
                    mb += f64::from(b[(y + j) * width + x + i]);
                }
            }
            ma /= 64.0;
            mb /= 64.0;
            let (mut va, mut vb, mut cov) = (0.0, 0.0, 0.0);
            for j in 0..8 {
                for i in 0..8 {
                    let da = f64::from(a[(y + j) * width + x + i]) - ma;
                    let db = f64::from(b[(y + j) * width + x + i]) - mb;
                    va += da * da;
                    vb += db * db;
                    cov += da * db;
                }
            }
            let (va, vb, cov) = (va / 63.0, vb / 63.0, cov / 63.0);
            sum += (2.0 * ma * mb + c1) * (2.0 * cov + c2)
                / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            n += 1;
        }
    }
    if n == 0 { 1.0 } else { sum / n as f64 }
}

pub fn mean_psnr(a: &[Vec<u8>], b: &[Vec<u8>], width: usize, height: usize) -> (f64, f64) {
    let y_len = width * height;
    let (mut y, mut c, mut n) = (0.0, 0.0, 0);
    for (fa, fb) in a.iter().zip(b) {
        y += psnr(&fa[..y_len], &fb[..y_len]).min(100.0);
        c += psnr(&fa[y_len..], &fb[y_len..]).min(100.0);
        n += 1;
    }
    if n == 0 { (0.0, 0.0) } else { (y / n as f64, c / n as f64) }
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

static STREAM_H264: [OfferDesc; 1] = [OfferDesc::any("h264/annexb")];
static STREAM_H265: [OfferDesc; 1] = [OfferDesc::any("h265/annexb")];
static STREAM_VP9: [OfferDesc; 1] = [OfferDesc::any("vp9")];
static STREAM_AV1: [OfferDesc; 1] = [OfferDesc::any("av1")];
static TAKEN: [OfferDesc; 4] = [
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
    OfferDesc::any("video/raw"),
    OfferDesc::any("bytes"),
];
static TAKEN_H264: [OfferDesc; 1] = [OfferDesc::any("h264/annexb")];
static TAKEN_H265: [OfferDesc; 1] = [OfferDesc::any("h265/annexb")];
static SRC_H264: [PadDesc; 1] = [PadDesc { name: "src", offers: &STREAM_H264 }];
static SRC_H265: [PadDesc; 1] = [PadDesc { name: "src", offers: &STREAM_H265 }];
static SRC_VP9: [PadDesc; 1] = [PadDesc { name: "src", offers: &STREAM_VP9 }];
static SRC_AV1: [PadDesc; 1] = [PadDesc { name: "src", offers: &STREAM_AV1 }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &TAKEN }];
static SINK_PADS_H264: [PadDesc; 1] = [PadDesc { name: "sink", offers: &TAKEN_H264 }];
static SINK_PADS_H265: [PadDesc; 1] = [PadDesc { name: "sink", offers: &TAKEN_H265 }];
static SRC_DESCS: [ElementDesc; 4] = [
    desc("unitsrc", &[], &SRC_H264, InputPolicy::None),
    desc("unitsrc", &[], &SRC_H265, InputPolicy::None),
    desc("unitsrc", &[], &SRC_VP9, InputPolicy::None),
    desc("unitsrc", &[], &SRC_AV1, InputPolicy::None),
];
static SINK_DESC: ElementDesc = desc("collect", &SINK_PADS, &[], InputPolicy::Single);
static SINK_DESC_H264: ElementDesc = desc("collect", &SINK_PADS_H264, &[], InputPolicy::Single);
static SINK_DESC_H265: ElementDesc = desc("collect", &SINK_PADS_H265, &[], InputPolicy::Single);

pub struct UnitSrc {
    pub units: Vec<Vec<u8>>,
    pub next: usize,
    pub codec: Codec,
}

impl UnitSrc {
    pub fn new(codec: Codec, units: Vec<Vec<u8>>) -> UnitSrc {
        UnitSrc { units, next: 0, codec }
    }
}

impl Element for UnitSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESCS[match self.codec {
            Codec::H264 => 0,
            Codec::H265 => 1,
            Codec::Vp9 => 2,
            Codec::Av1 => 3,
        }]
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
        buf.duration = Timestamp::from_nanos(FRAME_NS);
        ctx.out(SrcPad(0)).push(buf);
        self.next += 1;
        Ok(Flow::Ok)
    }
}

pub struct Collect(
    pub mpsc::Sender<(Timestamp, profluens_core::buffer::BufferFlags, Vec<u8>)>,
    pub Option<Codec>,
);

impl Collect {
    pub fn only(
        tx: mpsc::Sender<(Timestamp, profluens_core::buffer::BufferFlags, Vec<u8>)>,
        codec: Codec,
    ) -> Collect {
        Collect(tx, Some(codec))
    }
}

impl Element for Collect {
    fn desc(&self) -> &'static ElementDesc {
        match self.1 {
            Some(Codec::H264) => &SINK_DESC_H264,
            Some(Codec::H265) => &SINK_DESC_H265,
            _ => &SINK_DESC,
        }
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(b) = inputs.pop() {
            let _ = self.0.send((b.pts, b.flags, b.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }
}

pub fn warnings(p: &Pipeline) -> Vec<String> {
    let mut w = Vec::new();
    while let Some(m) = p.bus().try_recv() {
        match m {
            BusMessage::Warning { error, .. } => w.push(format!("warning {error}")),
            BusMessage::Error { error, .. } => w.push(format!("error {error}")),
            _ => {}
        }
    }
    w.retain(|w| !w.contains("format announcement"));
    w
}

pub fn copy_into(h: &mut Harness, b: &Buffer) -> Buffer {
    let mut c = h.alloc(b.memory.data());
    c.pts = b.pts;
    c.dts = b.dts;
    c.duration = b.duration;
    c.flags = b.flags;
    c
}
