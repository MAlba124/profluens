//! `vpldec`, the VPL hardware decoder for H.264, H.265, VP9 and AV1.
//! Outputs `video/raw`, `video/vpl` surfaces or `video/gpu` dma-bufs, plus optional fused VPP
//! channels.

use std::collections::VecDeque;
use std::sync::Arc;

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;
use profluens_video::color::HdrStatic;
use profluens_video::decoder::Color;
use profluens_video::gpuframe::{FRAMES_OUT, GpuFrameChannel};

use crate::emit::{Mode, Output, Picture, RAW_OFFERS, VPL_OFFERS};
use crate::error::{MfxError, VplErrorKind as K, check, failed};
use crate::ffi;
use crate::format::{Layout, color_of_signal};
use crate::frames::{Geometry, SurfaceChannel};
use crate::group::{Joining, VplGroup};
use crate::hdr::HdrCarry;
use crate::park::{Park, Stall, park, stalled};
use crate::probe::{self, Runtime};
use crate::pts::{PtsMap, Stamp, to_ts90};
use crate::session::{Component, Owner, Session, Surface, SurfaceArray, attach, header};
use crate::stats::Meter;
use crate::vppcore::{self, Plan, Setup, Target, VppCore, out_info, out_pool, plan, trace_pool};
use profluens_core::log::Loggable;

pub const NAME: &str = "vpldec";
pub const MAX_CHANNELS: usize = 2;

static SINK_OFFERS: [OfferDesc; 4] = [
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
    OfferDesc::any("vp9"),
    OfferDesc::any("av1"),
];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
const fn pad(name: &'static str, offers: &'static [OfferDesc]) -> PadDesc {
    PadDesc { name, offers }
}
static SRC_RAW_0: [PadDesc; 1] = [pad("src", &RAW_OFFERS)];
static SRC_RAW_1: [PadDesc; 2] = [pad("src", &RAW_OFFERS), pad("vpp0", &RAW_OFFERS)];
static SRC_RAW_2: [PadDesc; 3] =
    [pad("src", &RAW_OFFERS), pad("vpp0", &RAW_OFFERS), pad("vpp1", &RAW_OFFERS)];
static SRC_VPL_0: [PadDesc; 1] = [pad("src", &VPL_OFFERS)];
static SRC_VPL_1: [PadDesc; 2] = [pad("src", &VPL_OFFERS), pad("vpp0", &VPL_OFFERS)];
static SRC_VPL_2: [PadDesc; 3] =
    [pad("src", &VPL_OFFERS), pad("vpp0", &VPL_OFFERS), pad("vpp1", &VPL_OFFERS)];
static SRC_GPU: [PadDesc; 1] = [pad("src", OfferDesc::PROBED)];

type MakeFn = fn() -> Box<dyn Element>;

const fn desc(srcs: &'static [PadDesc], make_default: Option<MakeFn>) -> ElementDesc {
    ElementDesc {
        name: NAME,
        sinks: &SINKS,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default,
        io: false,
        couplings: &[],
    }
}

// Cold, registry default.
#[allow(clippy::disallowed_methods)]
fn make_default() -> Box<dyn Element> {
    Box::new(VplDec::new())
}

static DESCS_RAW: [ElementDesc; 3] =
    [desc(&SRC_RAW_0, Some(make_default)), desc(&SRC_RAW_1, None), desc(&SRC_RAW_2, None)];
static DESCS_VPL: [ElementDesc; 3] =
    [desc(&SRC_VPL_0, None), desc(&SRC_VPL_1, None), desc(&SRC_VPL_2, None)];
static DESC_GPU: ElementDesc = desc(&SRC_GPU, None);

struct Pending {
    surface: Surface,
    /// The decoded picture a fallback VPP output was made from, kept until that output is done.
    _src: Option<Surface>,
    geom: Geometry,
    stamp: Stamp,
    preview: bool,
    submitted: u64,
}

/// A decoded picture a fallback VPP could not take yet.
struct Retry {
    surface: Surface,
    stamp: Stamp,
    preview: bool,
}

struct Channel {
    target: Target,
    out: Output,
    plan: Option<Plan>,
    /// The fallback VPP, opened on the channel's first picture.
    vpp: Option<VppCore>,
    joining: Joining,
    /// The fallback VPP refused this session, its pictures are skipped until a reopen.
    dead: bool,
    retry: Option<Retry>,
}

/// What opening a session did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Open {
    Ready,
    /// The join is busy, parked, the caller leaves its input where it is.
    Wait,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fused {
    Off,
    On,
    Fallback,
}

/// Why a unit or a drain stopped short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Park(Park),
    /// The queues out are at [`PENDING_CAP`], they free as pictures go out.
    Room,
}

enum Step {
    Done,
    /// The unit goes again from this offset.
    Held(u32, Wait),
    /// A new sequence the decoder cannot follow, drain and reopen.
    Reinit,
}

/// Where the decoder is in its stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Feed,
    /// Sending the null bitstream until the decoder gives up its last picture.
    Drain(Then),
    /// The end was drained, what is left goes out of the queues.
    Drained,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    /// Reopen on the held unit, which starts the new sequence.
    Reopen,
    End,
}

struct Held {
    buf: Buffer,
    offset: u32,
}

pub struct VplDec {
    owner: Option<Arc<Owner>>,
    group: Option<Arc<VplGroup>>,
    joining: Joining,
    async_depth: Option<u16>,
    out_budget: Option<u32>,
    meter: Meter,
    codec: Option<u32>,
    param: ffi::mfxVideoParam,
    info: ffi::mfxFrameInfo,
    inited: bool,
    fused: Fused,
    main: Output,
    channels: Vec<Channel>,
    channel_params: [ffi::mfxVideoChannelParam; MAX_CHANNELS],
    skip: [u32; 1],
    /// One queue per src pad so a full ring on one pad does not block another.
    pending: [VecDeque<Pending>; MAX_CHANNELS + 1],
    held: Option<Held>,
    phase: Phase,
    busy: Stall,
    stamps: PtsMap,
    hdr_carry: HdrCarry,
    hdr: HdrStatic,
    last_out: Option<Timestamp>,
    color: Color,
    fps: Option<(u32, u32)>,
    failed: bool,
    disabled: bool,
    ended: bool,
    fused_warned: bool,
    pictures: u64,
    /// Test hook, the next calls report the device busy.
    #[cfg(test)]
    inject_busy: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
}

// SAFETY: SchedHint::Active keeps the element on one thread, surfaces refcount atomically.
unsafe impl Send for VplDec {}

impl Default for VplDec {
    fn default() -> Self {
        Self::new()
    }
}

impl VplDec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::with_mode(Mode::Raw)
    }

    pub fn new_surfaces(channel: std::sync::Arc<SurfaceChannel>) -> Self {
        Self::with_mode(Mode::Vpl(channel))
    }

    pub fn new_zerocopy(channel: std::sync::Arc<GpuFrameChannel>) -> Self {
        Self::with_mode(Mode::Gpu(channel))
    }

    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    fn with_mode(mode: Mode) -> Self {
        VplDec {
            owner: None,
            group: None,
            joining: Joining::default(),
            async_depth: None,
            out_budget: None,
            meter: Meter::default(),
            codec: None,
            param: ffi::mfxVideoParam::zeroed(),
            info: ffi::mfxFrameInfo::default(),
            inited: false,
            fused: Fused::Off,
            main: Output::new(mode, Self::OUT, Self::IN),
            channels: Vec::with_capacity(MAX_CHANNELS),
            // SAFETY: zero is a value of every field.
            channel_params: unsafe { std::mem::zeroed() },
            skip: [0],
            pending: std::array::from_fn(|_| VecDeque::with_capacity(16)),
            held: None,
            phase: Phase::Feed,
            busy: Stall::default(),
            stamps: PtsMap::new(),
            hdr_carry: HdrCarry::new(),
            hdr: HdrStatic::default(),
            last_out: None,
            color: Color::default(),
            fps: None,
            failed: false,
            disabled: false,
            ended: false,
            fused_warned: false,
            pictures: 0,
            #[cfg(test)]
            inject_busy: None,
        }
    }

    pub fn with_channel(
        mut self,
        target: Target,
        channel: Option<std::sync::Arc<SurfaceChannel>>,
    ) -> Self {
        if self.channels.len() < MAX_CHANNELS && !self.main.is_gpu() {
            let pad = SrcPad(self.channels.len() as u32 + 1);
            let mode = match channel {
                Some(c) => Mode::Vpl(c),
                None => Mode::Raw,
            };
            self.channels.push(Channel {
                target,
                out: Output::new(mode, pad, Self::IN),
                plan: None,
                vpp: None,
                joining: Joining::default(),
                dead: false,
                retry: None,
            });
        }
        self
    }

    /// The chain's join group. Without one the session joins what `PF_VPL_JOIN` says.
    pub fn with_group(mut self, group: Arc<VplGroup>) -> Self {
        self.group = Some(group);
        self
    }

    /// H.264 and HEVC decodes in flight, 1 to 16. AV1 keeps its own depth. `PF_VPL_ASYNC`
    /// sets it for elements that name none.
    pub fn with_async_depth(mut self, depth: u16) -> Self {
        self.async_depth = Some(depth.clamp(1, MAX_ASYNC_DEPTH));
        self
    }

    /// Surfaces kept over the runtime's suggestion for `video/vpl` and `video/gpu` output,
    /// [`OUT_BUDGET`] unless set. Zero sizes it from what the consumer declared it holds.
    pub fn with_out_budget(mut self, n: u32) -> Self {
        self.out_budget = Some(n);
        self
    }

    pub fn meter(&self) -> Meter {
        self.meter
    }

    pub fn fused(&self) -> Option<bool> {
        match self.fused {
            Fused::Off => None,
            Fused::On => Some(true),
            Fused::Fallback => Some(false),
        }
    }

    pub fn frames(&self) -> u64 {
        self.main.frames
    }

    /// Pictures lost on the way out of the main pad.
    pub fn dropped(&self) -> u64 {
        self.main.dropped
    }

    fn warn(&self, ctx: &mut Ctx, kind: K) {
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: failed(ctx, NAME, kind) });
    }

    fn fail(&mut self, ctx: &mut Ctx, kind: K) {
        if std::mem::replace(&mut self.failed, true) {
            return;
        }
        self.clear_pending();
        self.held = None;
        let element = ctx.element();
        ctx.post(BusMessage::Error { element, error: failed(ctx, NAME, kind) });
    }

    fn runtime(&mut self, ctx: &mut Ctx) -> Option<&'static Runtime> {
        let rt = probe::runtime();
        if rt.is_none() && !self.disabled {
            self.disabled = true;
            self.warn(ctx, K::NoDevice);
        }
        rt
    }

    fn session(&self) -> Option<&Session> {
        self.owner.as_deref().map(|o| &o.session)
    }

    fn ensure_session(&mut self, ctx: &mut Ctx) -> Open {
        if self.owner.is_some() {
            return Open::Ready;
        }
        let Some(rt) = self.runtime(ctx) else {
            return Open::Failed;
        };
        match self.joining.open(rt.parent_for(self.group.as_ref()), || rt.session()) {
            Ok(Some(s)) => {
                self.owner = Some(Owner::new(s));
                self.busy.clear();
                Open::Ready
            }
            Ok(None) => self.join_busy(ctx),
            Err(e) => {
                self.fail(ctx, K::Session(e));
                Open::Failed
            }
        }
    }

    /// A join the runtime could not take yet is tried again on a short wake.
    fn join_busy(&mut self, ctx: &mut Ctx) -> Open {
        if self.busy.tick(ctx.steady()) {
            self.fail(ctx, K::Stalled(stalled(ffi::MFX_WRN_IN_EXECUTION, "MFXJoinSession")));
            return Open::Failed;
        }
        self.meter.park(Park::Busy);
        park(ctx, Park::Busy);
        Open::Wait
    }

    fn codec(&mut self, ctx: &Ctx) -> Option<u32> {
        if self.codec.is_none() {
            let family = ctx.negotiated_sink(Self::IN).and_then(|f| ctx.family_name(f.family));
            self.codec = family.and_then(probe::codec_of);
        }
        self.codec
    }

    fn read_header(&mut self, ctx: &mut Ctx, data: &[u8]) -> Result<bool, MfxError> {
        let Some(codec) = self.codec(ctx) else {
            return Ok(false);
        };
        let Some(session) = self.session() else {
            return Ok(false);
        };
        let mut p = ffi::mfxVideoParam::zeroed();
        p.IOPattern = ffi::MFX_IOPATTERN_OUT_VIDEO_MEMORY;
        // The AV1 DPB is 8 refs plus this depth. Depth 1 is a slot short and stalls on
        // MFX_WRN_DEVICE_BUSY.
        let depth = probe::runtime().map_or(1, |rt| rt.async_depth(self.async_depth));
        p.AsyncDepth = if codec == ffi::MFX_CODEC_AV1 { AV1_ASYNC_DEPTH } else { depth };
        p.dec_mut().CodecId = codec;
        let mut vsi = ffi::mfxExtVideoSignalInfo {
            Header: header::<ffi::mfxExtVideoSignalInfo>(ffi::MFX_EXTBUFF_VIDEO_SIGNAL_INFO),
            ..Default::default()
        };
        let mut list = [(&raw mut vsi).cast::<ffi::mfxExtBuffer>()];
        let nal = codec == ffi::MFX_CODEC_AVC || codec == ffi::MFX_CODEC_HEVC;
        if nal {
            attach(&mut p, &mut list);
        }
        let mut bs = bitstream(data, ffi::MFX_TIMESTAMP_UNKNOWN as u64, 0, true, codec);
        // SAFETY: the bitstream reads data for the call, p and vsi are live.
        let st = unsafe { ffi::MFXVideoDECODE_DecodeHeader(session.raw(), &mut bs, &mut p) };
        if st == ffi::MFX_ERR_MORE_DATA {
            return Ok(false);
        }
        check(st, "MFXVideoDECODE_DecodeHeader")?;
        attach(&mut p, &mut []);
        self.color = color_of_signal(&vsi, nal);
        let f = p.dec().FrameInfo;
        self.fps = (f.FrameRateExtN > 0 && f.FrameRateExtD > 0)
            .then_some((f.FrameRateExtN, f.FrameRateExtD));
        self.param = p;
        Ok(true)
    }

    fn init(&mut self, ctx: &mut Ctx) -> Result<(), MfxError> {
        let Some(owner) = self.owner.clone() else {
            return Err(MfxError { status: ffi::MFX_ERR_NOT_INITIALIZED, call: "init" });
        };
        let session = &owner.session;
        let mut request = ffi::mfxFrameAllocRequest::default();
        let mut p = self.param;
        // SAFETY: p is valid, request a valid out struct.
        let suggested =
            match unsafe { ffi::MFXVideoDECODE_QueryIOSurf(session.raw(), &mut p, &mut request) } {
                st if st >= 0 => u32::from(request.NumFrameSuggested),
                _ => 8,
            };
        let slots = ctx.pool_stats().max_slots.min(u64::from(u32::MAX)) as u32;
        let budget = self.main.budget(self.out_budget);
        let extra = out_surfaces(budget, slots);
        // The budget is preallocated, the pool grows on demand up to what can be out at once.
        let delta = out_ceiling(budget, slots, self.main.hold()).saturating_sub(extra);
        trace_pool(
            &*ctx,
            NAME,
            suggested,
            suggested.saturating_add(extra),
            &self.param.dec().FrameInfo,
        );
        log!(&*ctx, Level::Trace, "pool"; "on_the_fly={delta}");
        let mut hints = ffi::mfxExtAllocationHints {
            Header: header::<ffi::mfxExtAllocationHints>(ffi::MFX_EXTBUFF_ALLOCATION_HINTS),
            AllocationPolicy: ffi::MFX_ALLOCATION_LIMITED,
            NumberToPreAllocate: suggested.saturating_add(extra),
            DeltaToAllocateOnTheFly: delta,
            // Without a wait the runtime drops the picture behind MFX_ERR_MORE_DATA.
            Wait: POOL_WAIT_MS,
            ..Default::default()
        };
        let mut list = [(&raw mut hints).cast::<ffi::mfxExtBuffer>()];
        attach(&mut p, &mut list);
        let f = p.dec().FrameInfo;
        let layout = Layout::of_fourcc(f.FourCC).unwrap_or(Layout::Nv12);
        let rate = self.fps.unwrap_or((30, 1));
        for ch in &mut self.channels {
            ch.plan = plan(u32::from(f.CropW), u32::from(f.CropH), layout, ch.target);
        }
        if !self.channels.is_empty() {
            for (i, ch) in self.channels.iter().enumerate() {
                let Some(pl) = ch.plan else { continue };
                let cp = &mut self.channel_params[i];
                cp.VPP = out_info(pl, rate);
                cp.VPP.ChannelId = i as u16 + 1;
                cp.IOPattern = ffi::MFX_IOPATTERN_OUT_VIDEO_MEMORY;
            }
            let n = self.channels.len();
            let mut ptrs = [std::ptr::null_mut::<ffi::mfxVideoChannelParam>(); MAX_CHANNELS];
            for (i, cp) in self.channel_params.iter_mut().enumerate().take(n) {
                ptrs[i] = cp;
            }
            // SAFETY: p, its hint and the channel params outlive the call.
            let st = unsafe {
                ffi::MFXVideoDECODE_VPP_Init(session.raw(), &mut p, ptrs.as_mut_ptr(), n as u32)
            };
            if st >= 0 {
                self.fused = Fused::On;
                self.inited = true;
                owner.set(Component::DecoderVpp);
                return self.after_init(ctx);
            }
            if !self.fused_warned {
                self.fused_warned = true;
                self.warn(
                    ctx,
                    K::FusedInit(MfxError { status: st, call: "MFXVideoDECODE_VPP_Init" }),
                );
            }
            self.fused = Fused::Fallback;
        }
        // SAFETY: as above.
        let mut st = unsafe { ffi::MFXVideoDECODE_Init(session.raw(), &mut p) };
        if st == ffi::MFX_ERR_INVALID_VIDEO_PARAM || st == ffi::MFX_ERR_UNSUPPORTED {
            attach(&mut p, &mut []);
            // SAFETY: as above.
            st = unsafe { ffi::MFXVideoDECODE_Init(session.raw(), &mut p) };
        }
        check(st, "MFXVideoDECODE_Init")?;
        self.inited = true;
        owner.set(Component::Decoder);
        self.after_init(ctx)
    }

    /// The shape the runtime decodes into, read back after init.
    fn after_init(&mut self, ctx: &mut Ctx) -> Result<(), MfxError> {
        let Some(session) = self.session() else {
            return Ok(());
        };
        let mut p = ffi::mfxVideoParam::zeroed();
        // SAFETY: a valid out struct.
        let got = unsafe { ffi::MFXVideoDECODE_GetVideoParam(session.raw(), &mut p) };
        self.info = if got >= 0 { p.dec().FrameInfo } else { self.param.dec().FrameInfo };
        self.trace_info(&*ctx);
        Ok(())
    }

    /// After MFX_WRN_VIDEO_PARAM_CHANGED the shape, the colour and the rate are read again.
    fn refresh(&mut self, log: &dyn Loggable) {
        let (Some(session), Some(codec)) = (self.session(), self.codec) else {
            return;
        };
        let raw = session.raw();
        let mut p = ffi::mfxVideoParam::zeroed();
        let mut vsi = ffi::mfxExtVideoSignalInfo {
            Header: header::<ffi::mfxExtVideoSignalInfo>(ffi::MFX_EXTBUFF_VIDEO_SIGNAL_INFO),
            ..Default::default()
        };
        let mut list = [(&raw mut vsi).cast::<ffi::mfxExtBuffer>()];
        let nal = codec == ffi::MFX_CODEC_AVC || codec == ffi::MFX_CODEC_HEVC;
        if nal {
            attach(&mut p, &mut list);
        }
        // SAFETY: p and vsi are live for the call.
        let mut got = unsafe { ffi::MFXVideoDECODE_GetVideoParam(raw, &mut p) };
        let signal = nal && got >= 0;
        if nal && got < 0 {
            p = ffi::mfxVideoParam::zeroed();
            // SAFETY: a valid out struct.
            got = unsafe { ffi::MFXVideoDECODE_GetVideoParam(raw, &mut p) };
        }
        attach(&mut p, &mut []);
        if got < 0 {
            return;
        }
        let f = p.dec().FrameInfo;
        self.info = f;
        if f.FrameRateExtN > 0 && f.FrameRateExtD > 0 {
            self.fps = Some((f.FrameRateExtN, f.FrameRateExtD));
        }
        if signal {
            self.color = color_of_signal(&vsi, true);
        }
        self.trace_info(log);
    }

    fn trace_info(&self, log: &dyn Loggable) {
        let i = self.info;
        log!(log, Level::Trace, "init"; "fused={} info {}x{} crop {},{} {}x{} fourcc={:#x} depth={} shift={}",
                self.fused == Fused::On,
                i.Width,
                i.Height,
                i.CropX,
                i.CropY,
                i.CropW,
                i.CropH,
                i.FourCC,
                i.BitDepthLuma,
                i.Shift
        );
    }

    /// Opens channel `i`'s fallback VPP in the chain's group. A refusal is warned once and the
    /// channel skips its pictures until the decoder reopens.
    fn open_fallback(&mut self, ctx: &mut Ctx, i: usize) -> Open {
        let (Some(rt), Some(pl)) = (probe::runtime(), self.channels[i].plan) else {
            return Open::Failed;
        };
        if self.channels[i].dead {
            return Open::Failed;
        }
        let parent = rt.parent_for(self.group.as_ref());
        let session = match self.channels[i].joining.open(parent, || rt.session()) {
            Ok(Some(s)) => s,
            Ok(None) => return self.join_busy(ctx),
            Err(e) => {
                self.channels[i].dead = true;
                self.warn(ctx, K::Session(e));
                return Open::Failed;
            }
        };
        let color = Color { hdr: self.hdr, ..self.color };
        let mut info = self.info;
        info.CropX = 0;
        info.CropY = 0;
        let depth = rt.async_depth(self.async_depth);
        let pool = out_pool(self.channels[i].out.hold(), depth);
        match VppCore::new(rt, session, info, pl, &color, Setup { async_depth: depth, pool }, &*ctx)
        {
            Ok(v) => {
                // Its tasks read the decoder's surfaces, the decoder closes after it.
                if let Some(o) = &self.owner {
                    v.owner().keep(o);
                }
                self.channels[i].vpp = Some(v);
                Open::Ready
            }
            Err(e) => {
                self.channels[i].dead = true;
                self.warn(ctx, e);
                Open::Failed
            }
        }
    }

    fn close(&mut self) {
        self.clear_pending();
        self.held = None;
        self.close_session();
    }

    /// The pictures queued out keep their session open until they go.
    fn close_session(&mut self) {
        self.owner = None;
        self.joining.clear();
        self.inited = false;
        self.fused = Fused::Off;
        for ch in &mut self.channels {
            ch.vpp = None;
            ch.joining.clear();
            ch.dead = false;
            ch.retry = None;
        }
    }

    fn queue(
        &mut self,
        surface: Surface,
        src: Option<Surface>,
        pad: usize,
        stamp: Stamp,
        preview: bool,
        submitted: u64,
    ) {
        let i = surface.info();
        let geom = Geometry {
            coded_w: u32::from(i.Width),
            coded_h: u32::from(i.Height),
            disp_w: u32::from(i.CropW),
            disp_h: u32::from(i.CropH),
            crop_x: u32::from(i.CropX),
            crop_y: u32::from(i.CropY),
            fourcc: i.FourCC,
        };
        if pad <= self.channels.len() {
            self.pending[pad].push_back(Pending {
                surface,
                _src: src,
                geom,
                stamp,
                preview,
                submitted,
            });
        }
    }

    fn clear_pending(&mut self) {
        for q in &mut self.pending {
            q.clear();
        }
    }

    /// Every decode call adds at most one picture to each queue, so the queues never pass their
    /// capacity while each stays under the cap before the call.
    fn pending_full(&self) -> bool {
        self.pending.iter().any(|q| q.len() >= PENDING_CAP)
    }

    fn pending_empty(&self) -> bool {
        self.pending.iter().all(VecDeque::is_empty)
    }

    fn retry_pending(&self) -> bool {
        self.channels.iter().any(|c| c.retry.is_some())
    }

    /// `Some` when a channel's VPP could not take the picture and keeps it for a retry.
    fn fallback(
        &mut self,
        ctx: &mut Ctx,
        surface: &Surface,
        stamp: Stamp,
        preview: bool,
    ) -> Option<Wait> {
        let mut wait = None;
        for i in 0..self.channels.len() {
            let r = Retry { surface: surface.clone(), stamp, preview };
            wait = wait.or(self.fallback_one(ctx, i, r));
        }
        wait
    }

    /// One picture into channel `i`'s VPP, kept for a retry while the VPP cannot take it.
    fn fallback_one(&mut self, ctx: &mut Ctx, i: usize, r: Retry) -> Option<Wait> {
        if self.channels[i].vpp.is_none() {
            match self.open_fallback(ctx, i) {
                Open::Ready => {}
                Open::Wait => {
                    self.channels[i].retry = Some(r);
                    return Some(Wait::Park(Park::Busy));
                }
                Open::Failed => return None,
            }
        }
        let now = ctx.steady();
        let v = self.channels[i].vpp.as_mut()?;
        match v.process(Some(&r.surface), &*ctx) {
            Ok(vppcore::Processed::Out(out)) => {
                self.queue(out, Some(r.surface), i + 1, r.stamp, r.preview, now);
                None
            }
            Ok(vppcore::Processed::Kept) => None,
            Ok(vppcore::Processed::Busy) => {
                self.channels[i].retry = Some(r);
                Some(Wait::Park(Park::Busy))
            }
            Ok(vppcore::Processed::Wait) => {
                self.channels[i].retry = Some(r);
                Some(Wait::Park(Park::Pool))
            }
            Err(e) => {
                self.warn(ctx, e);
                None
            }
        }
    }

    /// `Some` while a channel still waits on its VPP.
    fn retry_channels(&mut self, ctx: &mut Ctx) -> Option<Wait> {
        let mut wait = None;
        for i in 0..self.channels.len() {
            if self.pending[i + 1].len() >= PENDING_CAP {
                wait = wait.or(self.channels[i].retry.as_ref().map(|_| Wait::Room));
                continue;
            }
            if let Some(r) = self.channels[i].retry.take() {
                wait = wait.or(self.fallback_one(ctx, i, r));
            }
        }
        wait
    }

    fn decode(
        &mut self,
        ctx: &mut Ctx,
        data: Option<&[u8]>,
        ts90: u64,
        mut offset: u32,
    ) -> Result<Step, Error> {
        const CALL: &str = "MFXVideoDECODE_DecodeFrameAsync";
        let Some(codec) = self.codec else {
            return Ok(Step::Done);
        };
        let mut tries = 0;
        loop {
            let Some(owner) = self.owner.clone() else {
                return Ok(Step::Done);
            };
            if self.pending_full() {
                return Ok(Step::Held(offset, Wait::Room));
            }
            let session = &owner.session;
            let mut bs = data.map(|d| bitstream(d, ts90, offset, true, codec));
            let bs_ptr = bs.as_mut().map_or(std::ptr::null_mut(), |b| b as *mut ffi::mfxBitstream);
            let mut out: *mut ffi::mfxFrameSurface1 = std::ptr::null_mut();
            let mut array: *mut ffi::mfxSurfaceArray = std::ptr::null_mut();
            let mut syncp: ffi::mfxSyncPoint = std::ptr::null_mut();
            let fused = self.fused == Fused::On;
            let nskip = u32::from(fused && !ctx.pad_linked(Self::OUT));
            let st = if self.injected_busy() {
                ffi::MFX_WRN_DEVICE_BUSY
            } else {
                // SAFETY: the session is initialized, data outlives the call, out pointers are
                // valid.
                unsafe {
                    if fused {
                        ffi::MFXVideoDECODE_VPP_DecodeFrameAsync(
                            session.raw(),
                            bs_ptr,
                            self.skip.as_mut_ptr(),
                            nskip,
                            &mut array,
                        )
                    } else {
                        ffi::MFXVideoDECODE_DecodeFrameAsync(
                            session.raw(),
                            bs_ptr,
                            std::ptr::null_mut(),
                            &mut out,
                            &mut syncp,
                        )
                    }
                }
            };
            if let Some(b) = &bs {
                offset = b.DataOffset;
            }
            log!(&*ctx, Level::Trace, "decode";
                "fused={fused} st={} offset={offset} len={} out={:?} array={:?}",
                MfxError { status: st, call: "" }.name(),
                data.map_or(0, |d| d.len()),
                out,
                array
            );
            match st {
                ffi::MFX_ERR_NONE => {
                    // SAFETY: the runtime handed out one reference each.
                    let got = unsafe {
                        (
                            Surface::from_raw(out).map(|s| s.owned_by(&owner)),
                            SurfaceArray::from_raw(array),
                        )
                    };
                    if let Some(w) = self.take_output(ctx, got.0, got.1, &owner, data.is_none()) {
                        return Ok(Step::Held(offset, w));
                    }
                    if data.is_none() {
                        continue;
                    }
                }
                ffi::MFX_ERR_MORE_DATA => return Ok(Step::Done),
                ffi::MFX_WRN_VIDEO_PARAM_CHANGED if tries < 1000 => {
                    tries += 1;
                    self.refresh(&*ctx);
                }
                ffi::MFX_ERR_MORE_SURFACE if tries < 1000 => tries += 1,
                // The device took nothing, the unit goes again on a timer.
                ffi::MFX_WRN_DEVICE_BUSY | ffi::MFX_WRN_IN_EXECUTION => {
                    return Ok(Step::Held(offset, Wait::Park(Park::Busy)));
                }
                // Held for a release. Dropping it makes AV1 emit its kept picture under the next
                // stamp.
                ffi::MFX_WRN_ALLOC_TIMEOUT_EXPIRED => {
                    return Ok(Step::Held(offset, Wait::Park(Park::Pool)));
                }
                // The runtime took the data despite the offset. Refeed only the stamp so it is not
                // decoded twice.
                ffi::MFX_ERR_MEMORY_ALLOC => {
                    let end = data.map_or(0, |d| d.len() as u32);
                    return Ok(Step::Held(end, Wait::Park(Park::Pool)));
                }
                ffi::MFX_ERR_INCOMPATIBLE_VIDEO_PARAM => return Ok(Step::Reinit),
                _ => {
                    self.fail(ctx, K::Decode(MfxError { status: st, call: CALL }));
                    return Ok(Step::Done);
                }
            }
            if data.is_some_and(|d| offset as usize >= d.len()) && st != ffi::MFX_ERR_NONE {
                return Ok(Step::Done);
            }
        }
    }

    #[cfg(test)]
    fn injected_busy(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.inject_busy.as_ref().is_some_and(|n| {
            n.try_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1)).is_ok()
        })
    }

    #[cfg(not(test))]
    fn injected_busy(&self) -> bool {
        false
    }

    /// At the drain the runtime repeats the last stamp on every held frame (AV1), so a drained
    /// frame takes the earliest stamp owed after the last picture out.
    fn stamp_of(&mut self, s: &Surface, draining: bool, log: &dyn Loggable) -> Stamp {
        let ts = s.timestamp();
        let stamp = self.stamps.take_or_next(ts, !draining, self.last_out);
        if stamp.pts.is_some() && self.last_out.is_none_or(|l| stamp.pts > l) {
            self.last_out = Some(stamp.pts);
        }
        log!(log, Level::Trace, "out"; "ts={ts} draining={draining} pts={:?}", stamp.pts);
        stamp
    }

    /// The pictures are queued as handed out, each goes out once the runtime finished it.
    /// `Some` when a fallback VPP keeps one for a retry.
    fn take_output(
        &mut self,
        ctx: &mut Ctx,
        single: Option<Surface>,
        array: Option<SurfaceArray>,
        owner: &std::sync::Arc<Owner>,
        draining: bool,
    ) -> Option<Wait> {
        let mut wait = None;
        let now = ctx.steady();
        if let Some(s) = single {
            let stamp = self.stamp_of(&s, draining, &*ctx);
            let preview = Self::preview_of(ctx, stamp);
            self.pictures += 1;
            if self.fused == Fused::Fallback {
                wait = self.fallback(ctx, &s, stamp, preview);
            }
            self.queue(s, None, 0, stamp, preview, now);
        }
        if let Some(mut a) = array {
            let mut stamp = None;
            for i in 0..a.len() {
                let Some(s) = a.take(i, owner) else { continue };
                let (st, preview) = match stamp {
                    Some(st) => st,
                    None => {
                        let st = self.stamp_of(&s, draining, &*ctx);
                        *stamp.insert((st, Self::preview_of(ctx, st)))
                    }
                };
                let pad = usize::from(s.info().ChannelId);
                if pad <= self.channels.len() {
                    self.queue(s, None, pad, st, preview, now);
                }
            }
            self.pictures += 1;
        }
        wait
    }

    fn preview_of(ctx: &mut Ctx, stamp: Stamp) -> bool {
        !stamp.only && ctx.before_presentation(stamp.pts, Timestamp::ZERO) && ctx.take_preview()
    }

    fn flush_pending(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        let mut all = true;
        for pad in 0..=self.channels.len() {
            all &= self.flush_pad(ctx, pad)?;
        }
        Ok(all)
    }

    fn flush_pad(&mut self, ctx: &mut Ctx, pad: usize) -> Result<bool, Error> {
        while let Some(p) = self.pending[pad].front() {
            if p.stamp.only || (!p.preview && ctx.before_presentation(p.stamp.pts, Timestamp::ZERO))
            {
                ctx.count_drop();
                self.pending[pad].pop_front();
                continue;
            }
            if !ctx.pad_linked(out_pad(pad)) {
                self.pending[pad].pop_front();
                continue;
            }
            if !ctx.room(out_pad(pad)) {
                return Ok(false);
            }
            let color = Color { hdr: self.hdr, ..self.color };
            let p = &self.pending[pad][0];
            let pic = Picture {
                surface: &p.surface,
                geom: p.geom,
                pts: p.stamp.pts,
                dts: Timestamp::NONE,
                duration: p.stamp.duration,
                flags: if p.preview { BufferFlags::PREVIEW } else { BufferFlags::empty() },
                color,
                fps: self.fps,
                submitted: p.submitted,
            };
            let pts = p.stamp.pts.0;
            let out = if pad == 0 { &mut self.main } else { &mut self.channels[pad - 1].out };
            let carry = &mut self.hdr_carry;
            let meter = &mut self.meter;
            if !out.emit(ctx, NAME, &pic, meter, &mut |b| carry.attach(pts, &mut b.memory))? {
                return Ok(false);
            }
            self.pending[pad].pop_front();
        }
        Ok(true)
    }

    /// `None` once the decoder gave up its last picture.
    fn drain(&mut self, ctx: &mut Ctx) -> Result<Option<Wait>, Error> {
        if !self.inited || self.failed {
            return Ok(None);
        }
        Ok(match self.decode(ctx, None, ffi::MFX_TIMESTAMP_UNKNOWN as u64, 0)? {
            Step::Held(_, w) => Some(w),
            Step::Done | Step::Reinit => None,
        })
    }

    fn drain_releases(&mut self) {
        self.main.drain_released();
        for ch in &mut self.channels {
            ch.out.drain_released();
        }
    }

    /// Stamps and HDR of a unit, once as it comes in.
    fn admit(&mut self, ctx: &mut Ctx, buf: &Buffer) {
        let only = buf.flags.contains(BufferFlags::DECODE_ONLY);
        self.stamps.put(to_ts90(buf.pts), Stamp { pts: buf.pts, duration: buf.duration, only });
        self.gather_hdr(ctx, buf);
    }

    /// `Some` when the unit waits, held or behind a drain.
    fn feed(&mut self, ctx: &mut Ctx, buf: Buffer, offset: u32) -> Result<Option<Wait>, Error> {
        let data = buf.memory.data();
        let ts90 = to_ts90(buf.pts);
        if !self.inited {
            match self.read_header(ctx, data) {
                Ok(true) => {
                    if let Err(e) = self.init(ctx) {
                        self.fail(ctx, K::Init(e));
                        return Ok(None);
                    }
                }
                Ok(false) => return Ok(None),
                Err(e) => {
                    self.warn(ctx, K::Header(e));
                    return Ok(None);
                }
            }
        }
        match self.decode(ctx, Some(data), ts90, offset)? {
            Step::Done => Ok(None),
            Step::Held(at, w) => {
                self.held = Some(Held { buf, offset: at });
                Ok(Some(w))
            }
            // The new sequence starts this unit, it goes again whole on the reopened decoder.
            Step::Reinit => {
                self.held = Some(Held { buf, offset: 0 });
                self.phase = Phase::Drain(Then::Reopen);
                Ok(None)
            }
        }
    }

    /// Parks on `w`, failing once the device stayed busy past the stall limit.
    fn wait(&mut self, ctx: &mut Ctx, w: Wait) {
        match w {
            Wait::Room => {}
            Wait::Park(Park::Busy) if self.busy.tick(ctx.steady()) => {
                let e = stalled(ffi::MFX_WRN_DEVICE_BUSY, "MFXVideoDECODE_DecodeFrameAsync");
                self.fail(ctx, K::Stalled(e));
            }
            Wait::Park(p) => {
                self.meter.park(p);
                park(ctx, p);
            }
        }
    }

    /// Takes units and drains until something waits. The end is held while anything is owed.
    fn run(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        loop {
            if self.failed {
                return Ok(());
            }
            self.flush_pending(ctx)?;
            if let Some(w) = self.retry_channels(ctx) {
                self.wait(ctx, w);
                break;
            }
            match self.phase {
                Phase::Drain(then) => match self.drain(ctx)? {
                    Some(w) => {
                        self.wait(ctx, w);
                        break;
                    }
                    None if then == Then::Reopen => {
                        self.busy.clear();
                        self.close_session();
                        self.phase = Phase::Feed;
                        match self.ensure_session(ctx) {
                            Open::Ready => continue,
                            // The held unit waits for the join, or failed set.
                            Open::Wait | Open::Failed => break,
                        }
                    }
                    None => {
                        self.busy.clear();
                        self.phase = Phase::Drained;
                        continue;
                    }
                },
                Phase::Drained => break,
                Phase::Feed => {}
            }
            if self.pending_full() || ctx.command_pending() {
                break;
            }
            let (buf, offset) = match self.held.take() {
                Some(h) => (h.buf, h.offset),
                None => match inputs.pop() {
                    Some(b) => {
                        self.admit(ctx, &b);
                        (b, 0)
                    }
                    // A harness may end with units still queued, they go first.
                    None if self.ended && ctx.input_len() == 0 => {
                        self.phase = Phase::Drain(Then::End);
                        continue;
                    }
                    None => break,
                },
            };
            match self.feed(ctx, buf, offset)? {
                Some(w) => {
                    self.wait(ctx, w);
                    break;
                }
                None => self.busy.clear(),
            }
        }
        self.flush_pending(ctx)?;
        if self.ended && !self.finished() {
            ctx.hold_end();
        }
        Ok(())
    }

    fn finished(&self) -> bool {
        self.failed
            || (self.phase == Phase::Drained
                && self.held.is_none()
                && !self.retry_pending()
                && self.pending_empty())
    }

    fn gather_hdr(&mut self, ctx: &mut Ctx, buf: &Buffer) {
        let changed = match self.codec {
            Some(ffi::MFX_CODEC_HEVC) => self.hdr_carry.gather(
                pf_hdr::h26x::Codec::H265,
                &buf.memory,
                buf.pts.0,
                &mut self.hdr,
            ),
            Some(ffi::MFX_CODEC_AVC) => self.hdr_carry.gather(
                pf_hdr::h26x::Codec::H264,
                &buf.memory,
                buf.pts.0,
                &mut self.hdr,
            ),
            Some(ffi::MFX_CODEC_AV1) => {
                self.hdr_carry.gather_av1(&buf.memory, buf.pts.0, &mut self.hdr)
            }
            _ => {
                self.hdr_carry.keep(&buf.memory, buf.pts.0);
                false
            }
        };
        if changed {
            let (m, l) = (self.hdr.mastering, self.hdr.light);
            log!(
                &*ctx,
                Level::Debug,
                "hdr_static",
                max_nits = m.max_luminance / 10_000,
                max_cll = l.max_cll
            );
        }
    }
}

fn out_pad(pad: usize) -> SrcPad {
    SrcPad(pad as u32)
}

/// Surfaces kept over the runtime's suggestion for surfaces handed on, unless set otherwise.
pub const OUT_BUDGET: u32 = 16;
const POOL_WAIT_MS: u32 = 20;
const AV1_ASYNC_DEPTH: u16 = 4;
/// The encoder's ring bounds the depth, the decoder takes the same bound.
pub const MAX_ASYNC_DEPTH: u16 = 16;
/// Pictures queued per pad before the decoder takes no more input. The queues hold 16.
pub const PENDING_CAP: usize = 8;

/// How many surfaces the runtime keeps over its own suggestion, per output mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Budget {
    /// Raw output copies each picture and lets go, so the queues bound it.
    Raw,
    /// `video/vpl`, the budget, or two over what the consumer declared it holds.
    Vpl(u32),
    /// `video/gpu`, the budget, at least one per pool slot that may carry a frame.
    Gpu(u32),
}

pub fn out_surfaces(budget: Budget, slots: u32) -> u32 {
    match budget {
        Budget::Raw => PENDING_CAP as u32 + 2,
        Budget::Vpl(n) => n,
        Budget::Gpu(n) => n.max(slots.saturating_add(1)),
    }
}

/// What a `video/vpl` consumer that declared nothing is taken to keep.
pub const UNDECLARED_HOLD: u32 = 4;
/// A parked retry and inputs held until the consumer's next pass, over its declared hold.
const HOLD_SLACK: u32 = 4;

/// The most surfaces out of the runtime's hands at once, over its own suggestion. The queues
/// out, the frames in flight to the consumer, one per pool slot or export, and what the consumer
/// keeps once it took them. A pool below this runs dry under a slow consumer, and the HEVC
/// decoder never recovers from a frame it could not get a surface for.
pub fn out_ceiling(budget: Budget, slots: u32, hold: Option<u32>) -> u32 {
    let queued = PENDING_CAP as u32;
    let carried = match budget {
        Budget::Raw => 2,
        Budget::Vpl(_) => {
            slots.saturating_add(hold.unwrap_or(UNDECLARED_HOLD)).saturating_add(HOLD_SLACK)
        }
        Budget::Gpu(_) => (FRAMES_OUT as u32).max(slots).saturating_add(1),
    };
    queued.saturating_add(carried)
}

/// The unit from `offset` on, an offset past the end reads as empty.
pub fn bitstream(
    data: &[u8],
    ts90: u64,
    offset: u32,
    complete: bool,
    codec: u32,
) -> ffi::mfxBitstream {
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let offset = offset.min(len);
    ffi::mfxBitstream {
        EncryptedData: std::ptr::null_mut(),
        ExtParam: std::ptr::null_mut(),
        NumExtParam: 0,
        reserved1: 0,
        CodecId: codec,
        DecodeTimeStamp: ffi::MFX_TIMESTAMP_UNKNOWN,
        TimeStamp: ts90,
        Data: data.as_ptr().cast_mut(),
        DataOffset: offset,
        DataLength: len - offset,
        MaxLength: len,
        PicStruct: 0,
        FrameType: 0,
        DataFlag: if complete { ffi::MFX_BITSTREAM_COMPLETE_FRAME } else { 0 },
        reserved2: 0,
    }
}

impl Element for VplDec {
    fn desc(&self) -> &'static ElementDesc {
        let n = self.channels.len();
        if self.main.is_gpu() {
            &DESC_GPU
        } else if self.main.is_raw() {
            &DESCS_RAW[n]
        } else {
            &DESCS_VPL[n]
        }
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let device = probe::runtime().map(|r| &*r.device);
        self.main.preroll(ctx, device)
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.main.start(ctx, NAME)?;
        for ch in &mut self.channels {
            ch.out.start(ctx, NAME)?;
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.failed {
            while inputs.pop().is_some() {
                ctx.count_drop();
            }
            return Ok(Flow::Ok);
        }
        self.meter.report(ctx.steady(), &*ctx);
        match self.ensure_session(ctx) {
            Open::Ready => {}
            // The units stay queued for the pass after the join.
            Open::Wait => return Ok(Flow::Ok),
            Open::Failed => {
                while inputs.pop().is_some() {}
                return Ok(Flow::Ok);
            }
        }
        self.drain_releases();
        self.run(ctx, &mut inputs)?;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart => {
                self.clear_pending();
                self.held = None;
                self.phase = Phase::Feed;
                self.busy.clear();
                self.stamps.clear();
                self.last_out = None;
                self.hdr_carry.clear();
                self.ended = false;
                self.main.flush();
                for ch in &mut self.channels {
                    ch.out.flush();
                    ch.retry = None;
                }
                // The runtime's AV1 Reset forgets the surfaces the DPB still holds and stalls after
                // a few seeks, so AV1 reopens on the next header.
                if self.codec == Some(ffi::MFX_CODEC_AV1) {
                    if self.inited {
                        self.close_session();
                    }
                } else if let (Some(raw), true) = (self.session().map(|s| s.raw()), self.inited) {
                    let mut p = self.param;
                    // SAFETY: initialized on this session, p the init parameters.
                    let st = unsafe {
                        if self.fused == Fused::On {
                            let n = self.channels.len();
                            let mut ptrs =
                                [std::ptr::null_mut::<ffi::mfxVideoChannelParam>(); MAX_CHANNELS];
                            for (i, cp) in self.channel_params.iter_mut().enumerate().take(n) {
                                ptrs[i] = cp;
                            }
                            ffi::MFXVideoDECODE_VPP_Reset(raw, &mut p, ptrs.as_mut_ptr(), n as u32)
                        } else {
                            ffi::MFXVideoDECODE_Reset(raw, &mut p)
                        }
                    };
                    if st < 0 {
                        self.close();
                    }
                }
            }
            // A parked unit or a drain is still owed, the end waits for them and the queues.
            Event::Eos => {
                self.ended = true;
                if !self.failed {
                    self.drain_releases();
                    self.run(ctx, &mut Inputs::empty())?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.meter.report_final(&*ctx);
        self.clear_pending();
        self.held = None;
        self.phase = Phase::Feed;
        self.busy.clear();
        // Outputs first so queued surfaces return while the decoder still exists.
        self.main.stop();
        for ch in &mut self.channels {
            ch.out.stop();
            ch.vpp = None;
        }
        self.close();
        self.codec = None;
        self.ended = false;
    }
}

#[cfg(test)]
// Hardware test, skips without the runtime.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use pf_harness::Harness;

    use super::*;

    const INTER_AV1: &[u8] = include_bytes!("../../../video/av1/tests/fixtures/inter_64x48.ivf");
    const FRAME_NS: u64 = 40_000_000;

    fn ivf_units(data: &[u8]) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut off = usize::from(u16::from_le_bytes([data[6], data[7]]));
        while off + 12 <= data.len() {
            let size = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            let Some(unit) = data.get(off + 12..off + 12 + size as usize) else { break };
            out.push(unit);
            off += 12 + size as usize;
        }
        out
    }

    /// Pictures out of `units`, the device made busy for `before_last` calls ahead of the last
    /// unit and `before_end` calls ahead of the end.
    fn decode(units: &[&[u8]], before_last: u32, before_end: u32) -> (usize, Vec<String>) {
        let busy = Arc::new(AtomicU32::new(0));
        let mut dec = VplDec::new();
        dec.inject_busy = Some(Arc::clone(&busy));
        let mut h = Harness::with_pool(dec, 1 << 20, 16).real_wakes();
        h.fix_sink_format(VplDec::IN, "av1", &[]);
        h.start().expect("start");
        let mut out = 0;
        for (i, u) in units.iter().enumerate() {
            if i + 1 == units.len() {
                busy.store(before_last, Ordering::Release);
            }
            let mut b = h.alloc(u);
            b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
            b.duration = Timestamp::from_nanos(FRAME_NS);
            h.push(b).expect("push");
            while h.pull(VplDec::OUT).is_some() {
                out += 1;
            }
        }
        busy.fetch_add(before_end, Ordering::AcqRel);
        out += h.eos().expect("eos").len();
        let said = h
            .bus_messages()
            .iter()
            .filter_map(|m| match m {
                BusMessage::Warning { error, .. } | BusMessage::Error { error, .. } => {
                    Some(error.to_string())
                }
                _ => None,
            })
            .collect();
        (out, said)
    }

    #[test]
    fn the_end_waits_for_a_unit_held_on_a_busy_device_and_a_busy_drain() {
        let test = "the_end_waits_for_a_unit_held_on_a_busy_device_and_a_busy_drain";
        if !probe::probe().is_some_and(|c| c.supports("av1")) {
            return probe::skip(test, "no VPL runtime decoding av1");
        }
        let units = ivf_units(INTER_AV1);
        let (all, said) = decode(&units, 0, 0);
        assert!(said.is_empty() && all == units.len(), "{test}: baseline {all} {said:?}");
        let (held, said) = decode(&units, 3, 0);
        assert!(said.is_empty(), "{test}: {said:?}");
        assert_eq!(held, all, "{test}: the last unit was held at the end");
        let (drained, said) = decode(&units, 0, 3);
        assert!(said.is_empty(), "{test}: {said:?}");
        assert_eq!(drained, all, "{test}: the drain went busy");
        let (both, said) = decode(&units, 2, 2);
        assert!(said.is_empty(), "{test}: {said:?}");
        assert_eq!(both, all, "{test}: both");
    }

    #[test]
    fn surfaces_kept_over_the_suggestion_scale_with_the_output() {
        assert_eq!(out_surfaces(Budget::Raw, 64), PENDING_CAP as u32 + 2, "raw copies out at once");
        assert_eq!(out_surfaces(Budget::Gpu(OUT_BUDGET), 64), 65, "one per pool slot handed on");
        assert_eq!(out_surfaces(Budget::Gpu(OUT_BUDGET), 4), OUT_BUDGET);
        assert_eq!(out_surfaces(Budget::Gpu(OUT_BUDGET), u32::MAX), u32::MAX);
        assert_eq!(out_surfaces(Budget::Vpl(4), 64), 4, "a surface consumer is sized directly");
        let vpl = Output::new(Mode::Vpl(SurfaceChannel::new()), SrcPad(0), SinkPad(0));
        assert_eq!(vpl.budget(None), Budget::Vpl(OUT_BUDGET), "sixteen unless set");
        assert_eq!(vpl.budget(Some(4)), Budget::Vpl(4));
        assert_eq!(vpl.budget(Some(0)), Budget::Vpl(OUT_BUDGET), "nothing declared yet");
        let Mode::Vpl(ch) = &vpl.mode else { unreachable!() };
        ch.declare_hold(2);
        assert_eq!(vpl.budget(Some(0)), Budget::Vpl(4), "the consumer's hold plus two");
        assert_eq!(vpl.budget(Some(9)), Budget::Vpl(9), "a set budget wins");
        assert_eq!(vpl.budget(None), Budget::Vpl(OUT_BUDGET));
        let raw = Output::new(Mode::Raw, SrcPad(0), SinkPad(0));
        assert_eq!(raw.budget(Some(4)), Budget::Raw);
    }

    #[test]
    fn the_pool_ceiling_covers_everything_that_can_be_out_at_once() {
        let cap = PENDING_CAP as u32;
        // The box chain, 16 header slots, a vplvpp at depth 1 declaring three.
        let vpl = out_ceiling(Budget::Vpl(OUT_BUDGET), 16, Some(3));
        assert_eq!(vpl, cap + 16 + 3 + HOLD_SLACK);
        assert!(
            vpl > out_surfaces(Budget::Vpl(OUT_BUDGET), 16),
            "the default budget is short of it"
        );
        assert_eq!(out_ceiling(Budget::Vpl(4), 16, None), cap + 16 + UNDECLARED_HOLD + HOLD_SLACK);
        assert_eq!(
            out_ceiling(Budget::Raw, 64, Some(9)),
            out_surfaces(Budget::Raw, 64),
            "raw copies out at once, nothing grows"
        );
        let gpu = out_ceiling(Budget::Gpu(OUT_BUDGET), 4, None);
        assert_eq!(gpu, cap + FRAMES_OUT as u32 + 1, "every export may be out");
        assert!(gpu >= out_surfaces(Budget::Gpu(OUT_BUDGET), 4));
        assert_eq!(out_ceiling(Budget::Vpl(1), u32::MAX, Some(u32::MAX)), u32::MAX);
    }

    #[test]
    fn a_unit_past_its_offset_reads_as_empty() {
        let data = [0u8; 10];
        let b = bitstream(&data, 7, 4, true, ffi::MFX_CODEC_AVC);
        assert_eq!((b.DataOffset, b.DataLength, b.MaxLength, b.TimeStamp), (4, 6, 10, 7));
        let b = bitstream(&data, 7, 11, false, ffi::MFX_CODEC_AVC);
        assert_eq!((b.DataOffset, b.DataLength, b.DataFlag), (10, 0, 0));
    }
}
