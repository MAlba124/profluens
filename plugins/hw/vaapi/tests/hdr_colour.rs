//! HEVC stream colour (VUI and mastering display SEI) over container colour in what
//! vaapih265dec announces, readback and zero-copy. Skips without a Main 10 device.

// Test code.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_harness::Harness;
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_video::color::{
    self, ContentLight, HdrStatic, Mastering, Matrix, Primaries, Range, StreamColor, TransferFn,
};
use profluens_video::format::COLOR_OFFER_FIELDS;

use pf_vaapi::VaapiH265Dec;
use pf_vaapi::gpuframe::{self, DmabufFormat, FrameRx, GpuFrameChannel, GpuFrameHeader};
use pf_vaapi::va::{Display, ExportedSurface, SurfaceFormat};

const HDR10: &[u8] = include_bytes!("fixtures/hdr10_128x128.hevc");
const FAMILY: &str = "h265/annexb";

fn container() -> StreamColor {
    StreamColor {
        transfer: Some(TransferFn::Bt709),
        primaries: Some(Primaries::Bt709),
        hdr: HdrStatic {
            mastering: Mastering {
                max_luminance: 10_000_000,
                min_luminance: 100,
                ..Default::default()
            },
            light: ContentLight { max_cll: 1000, max_fall: 400 },
        },
        ..Default::default()
    }
}

fn expected() -> StreamColor {
    StreamColor {
        matrix: Some(Matrix::Bt2020),
        range: Some(Range::Limited),
        transfer: Some(TransferFn::Pq),
        primaries: Some(Primaries::Bt2020),
        hdr: HdrStatic {
            mastering: Mastering {
                primaries: [[34000, 16000], [13250, 34500], [7500, 3000]],
                white: [15635, 16450],
                max_luminance: 40_000_000,
                min_luminance: 50,
            },
            light: ContentLight { max_cll: 1000, max_fall: 400 },
        },
        chroma: None,
        dovi: None,
    }
}

fn container_fields() -> Vec<(&'static str, ValueDesc)> {
    let mut out = [("", ValueDesc::Int(0)); color::FIELDS.len()];
    let n = container().write_fields(&mut out);
    out[..n].to_vec()
}

fn units() -> Vec<Vec<u8>> {
    let data = HDR10;
    let opens_picture = |at: usize| (data[at] >> 1) & 0x3F < 32 && data[at + 2] & 0x80 != 0;
    let mut cuts = vec![0];
    let (mut open, mut head) = (false, 0);
    for i in 0..data.len().saturating_sub(3) {
        if data[i..i + 3] != [0, 0, 1] {
            continue;
        }
        let code = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
        if opens_picture(i + 3) {
            if open {
                cuts.push(if head == usize::MAX { code } else { head });
            }
            (open, head) = (true, usize::MAX);
        } else if head == usize::MAX {
            head = code;
        }
    }
    let ends = cuts.iter().skip(1).copied().chain([data.len()]);
    let units: Vec<Vec<u8>> = cuts.iter().zip(ends).map(|(&a, b)| data[a..b].to_vec()).collect();
    assert_eq!(units.len(), 6);
    units
}

fn main10(test: &str) -> bool {
    let ok = pf_vaapi::probe()
        .is_some_and(|caps| caps.supports(FAMILY) && caps.decodes_profile("HEVCMain10"));
    if !ok {
        eprintln!("skip {test}, no VA-API device decodes HEVC Main 10");
    }
    ok
}

#[test]
fn the_vui_and_the_sei_state_the_colour_over_the_container_on_video_raw() {
    if !main10("readback") {
        return;
    }
    let mut h = Harness::with_pool(VaapiH265Dec::new(), 128 * 128 * 3, 8);
    h.fix_sink_format(VaapiH265Dec::IN, FAMILY, &container_fields());
    for (i, unit) in units().into_iter().enumerate() {
        let mut buf = h.alloc(&unit);
        buf.pts = Timestamp::from_nanos(i as u64 * 40_000_000);
        h.push(buf).expect("push");
        h.drain_outputs();
    }
    h.eos().expect("eos");
    let announced = h.announced().expect("a video/raw announcement");
    let v = h.vocabulary();
    assert_eq!(v.family_name(announced.family), Some("video/raw"));
    let got = read(v, &announced);
    assert_eq!(got, expected());
}

fn read(
    v: &profluens_core::format::Vocabulary,
    f: &profluens_core::format::FixedFormat,
) -> StreamColor {
    use profluens_core::format::Value;
    let mut c = StreamColor::default();
    let get = |field: &str| f.get(v.field_id(field)?);
    let name = |field: &str| match get(field)? {
        Value::Id(id) => v.value_name(id),
        _ => None,
    };
    c.matrix = name(color::FIELD_MATRIX).and_then(Matrix::from_caps_name);
    c.range = name(color::FIELD_RANGE).and_then(Range::from_caps_name);
    c.transfer = name(color::FIELD_TRANSFER).and_then(TransferFn::from_caps_name);
    c.primaries = name(color::FIELD_PRIMARIES).and_then(Primaries::from_caps_name);
    for field in &color::FIELDS[4..] {
        if let Some(Value::Int(x)) = get(field) {
            c.hdr.set_field(field, x);
        }
    }
    c
}

static SRC_FIELDS: [FieldDesc; 10] = COLOR_OFFER_FIELDS;
static SRC_OFFERS: [OfferDesc; 2] =
    [OfferDesc::any(FAMILY), OfferDesc { family: FAMILY, fields: &SRC_FIELDS }];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "unitsrc",
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

struct UnitSrc {
    units: Vec<Vec<u8>>,
    next: usize,
}

impl Element for UnitSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.next == 0 {
            ctx.announce_format(SrcPad(0), FAMILY, &container_fields());
        }
        let Some(unit) = self.units.get(self.next) else {
            return Ok(Flow::Eos);
        };
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.as_mut_full()[..unit.len()].copy_from_slice(unit);
        buf.memory.set_len(unit.len());
        buf.pts = Timestamp::from_nanos(self.next as u64 * 40_000_000);
        ctx.out(SrcPad(0)).push(buf);
        self.next += 1;
        Ok(Flow::Ok)
    }
}

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "coloursink",
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

struct ColourSink {
    formats: Vec<DmabufFormat>,
    channel: Arc<GpuFrameChannel>,
    frames: Option<FrameRx>,
    seen: Arc<Mutex<(Vec<StreamColor>, usize)>>,
}

impl Element for ColourSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        gpuframe::with_gpu_offers(&self.formats, |offers| ctx.offer_sink(SinkPad(0), offers))
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.frames = self.channel.take_rx();
        Ok(())
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            self.seen.lock().unwrap().0.push(StreamColor::read(ctx, f));
        }
        Ok(())
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let token = GpuFrameHeader::from_bytes(buf.memory.data()).expect("a header").token;
            if let Some(frame) = self.frames.as_mut().and_then(|frames| frames.take(token)) {
                drop(frame);
                self.seen.lock().unwrap().1 += 1;
            }
        }
        Ok(Flow::Ok)
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        if let Some(frames) = self.frames.take() {
            self.channel.put_rx(frames);
        }
    }
}

#[test]
fn the_same_colour_reaches_a_zero_copy_sink_on_video_gpu() {
    if !main10("zero-copy") {
        return;
    }
    let caps = pf_vaapi::probe().expect("probed");
    let display = Display::open(&caps.device).expect("the device");
    let mut formats = Vec::new();
    for layout in [SurfaceFormat::NV12, SurfaceFormat::P010] {
        if let Ok((fourcc, modifier)) = ExportedSurface::probe_for(&display, layout) {
            formats.push(DmabufFormat { fourcc, modifiers: vec![modifier] });
        }
    }
    drop(display);
    if formats.len() < 2 {
        eprintln!("skip zero-copy, the driver exports no P010 surface");
        return;
    }
    let channel = GpuFrameChannel::new();
    let seen = Arc::new(Mutex::new((Vec::new(), 0)));
    let mut p = Pipeline::new();
    p.set_pool(64 * 1024, 16);
    let src = p.add(UnitSrc { units: units(), next: 0 });
    let dec = p.add(VaapiH265Dec::new_zerocopy(Arc::clone(&channel)));
    let sink = p.add(ColourSink {
        formats,
        channel: Arc::clone(&channel),
        frames: None,
        seen: Arc::clone(&seen),
    });
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(VaapiH265Dec::IN)).expect("src -> decoder");
    p.link(dec.src(VaapiH265Dec::OUT), sink.sink(SinkPad(0))).expect("decoder -> sink");
    p.run().expect("the run ends");
    let (colours, frames) = seen.lock().unwrap().clone();
    assert_eq!(frames, 6, "every picture reached the sink");
    assert_eq!(colours.last(), Some(&expected()), "{colours:?}");
}

/// A suffix SEI NAL (type 40) of one HDR10+ message, H.265 D.2.6.
fn hdr10plus_suffix_sei(maxscl: u32) -> Vec<u8> {
    let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
    t35.extend(pf_hdr::hdr10plus::sample_payload([maxscl, 1, 1], 1000));
    let mut body = vec![4u8, t35.len() as u8];
    body.extend(t35);
    body.push(0x80);
    let (mut nal, mut zeros) = (vec![0, 0, 1, 40 << 1, 1], 0);
    for b in body {
        if zeros >= 2 && b <= 3 {
            nal.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        nal.push(b);
    }
    nal
}

#[test]
fn hdr10plus_rides_each_picture_on_video_raw() {
    if !main10("hdr10plus") {
        return;
    }
    let mut h = Harness::with_pool(VaapiH265Dec::new(), 128 * 128 * 3, 8);
    let mut out = Vec::new();
    for (i, mut unit) in units().into_iter().enumerate() {
        unit.extend(hdr10plus_suffix_sei(1000 * (i as u32 + 1)));
        let mut buf = h.alloc(&unit);
        buf.pts = Timestamp::from_nanos(i as u64 * 40_000_000);
        h.push(buf).expect("push");
        out.extend(h.drain_outputs());
    }
    out.extend(h.eos().expect("eos"));
    assert_eq!(out.len(), 6);
    for b in &out {
        let i = (b.pts.nanos().unwrap() / 40_000_000) as u32;
        let meta = pf_hdr::side::FrameMeta::read(b.memory.side()).expect("valid");
        assert_eq!(
            meta.hdr10plus().unwrap().unwrap().windows[0].maxscl[0],
            1000 * (i + 1),
            "picture {i}"
        );
    }
}
