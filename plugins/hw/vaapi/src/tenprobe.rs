use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

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

use crate::VaapiH265Dec;
use crate::gpuframe::GpuFrameChannel;
use crate::vpp::Target;

/// One Main 10 keyframe of 256x256, parameter sets ahead of it.
static KEYFRAME: &[u8] = include_bytes!("tenprobe.hevc");

static OUT: [OfferDesc; 1] = [OfferDesc::any("h265/annexb")];
static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OUT }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];

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

static SRC: ElementDesc = desc("tenprobesrc", &[], &SRC_PADS, InputPolicy::None);
static SINK: ElementDesc = desc("tenprobesink", &SINK_PADS, &[], InputPolicy::Single);

/// The keyframe once, then the end.
struct Keyframe {
    sent: bool,
}

impl Element for Keyframe {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.sent {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.as_mut_full()[..KEYFRAME.len()].copy_from_slice(KEYFRAME);
        buf.memory.set_len(KEYFRAME.len());
        ctx.out(SrcPad(0)).push(buf);
        self.sent = true;
        Ok(Flow::Ok)
    }
}

static SEEN: AtomicBool = AtomicBool::new(false);

struct Seen;

impl Element for Seen {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {
            SEEN.store(true, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }
}

/// Whether this device scales a 10 bit HEVC picture in the decode call, found by decoding one.
/// The profile lists decode processing on GPUs whose driver then fails every Main 10 picture
/// with it (iHD 26.1 on Gen9, `vaEndPicture` 0x17), Gen12 scales them.
pub fn shrinks_ten() -> bool {
    static KNOWN: OnceLock<bool> = OnceLock::new();
    *KNOWN.get_or_init(|| decoded().unwrap_or(false))
}

fn decoded() -> Option<bool> {
    let caps = crate::probe::probe()?;
    if !caps.shrinks("h265/annexb") || !caps.decodes_profile("HEVCMain10") {
        return Some(false);
    }
    let mut p = Pipeline::new();
    p.set_pool(64 * 1024, 4);
    let src = p.add(Keyframe { sent: false });
    let half = Target { max_width: 128, max_height: 128, depth: 10 };
    let dec = p.add(VaapiH265Dec::new_zerocopy(GpuFrameChannel::new()).with_shrink(half, true));
    let sink = p.add(Seen);
    p.preroll().ok()?;
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).ok()?;
    p.link(dec.src(SrcPad(0)), sink.sink(SinkPad(0))).ok()?;
    p.set_element_pool(dec, 64 * 1024, 8);
    p.run().ok()?;
    let mut clean = true;
    while let Some(m) = p.bus().try_recv() {
        clean &= !matches!(m, BusMessage::Error { .. });
    }
    Some(clean && SEEN.load(Ordering::Relaxed))
}
