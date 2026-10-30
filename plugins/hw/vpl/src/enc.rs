//! `vplenc`, the VPL hardware H.264 and H.265 encoder, from `video/vpl` surfaces or uploaded
//! `video/raw`. Rate control prefers lookahead, then the extended BRC lookahead, then VBR, as Query
//! allows.

use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, ConstraintDesc, FieldDesc, OfferDesc, Value};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;
use profluens_video::decoder::Color;
use profluens_video::gpuframe::GpuFrameHeader;
use profluens_video::keys::at_key;

use crate::emit::VPL_OFFERS;
use crate::error::{MfxError, VplErrorKind as K, check, failed};
use crate::ffi;
use crate::format::{
    FAMILY_RAW, InFormat, Layout, RAW_IN_PIXFMTS, fields, is_hdr, read_format, signal_of_color,
};
use crate::frames::{F_HEIGHT, F_WIDTH, Geometry, SurfaceChannel, VplFrame};
use crate::group::{Joining, VplGroup};
use crate::input::{Input, upload};
use crate::nal;
use crate::park::{BUSY_WAKE, Park, Stall, park, stalled};
use crate::probe::{self, EncPlan, Runtime, frame_info};
use crate::pts::{PtsMap, Stamp, from_ts90, to_ts90};
use crate::session::{Component, Owner, Session, Surface, attach, header};
use crate::stats::Meter;
use crate::vppcore::trace_pool;
use profluens_core::log::Loggable;

pub const NAME: &str = "vplenc";
const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);
pub const FAM_H265: &str = "h265/annexb";
pub const FAM_H264: &str = "h264/annexb";

static RAW_IN_FIELDS: [FieldDesc; 14] = fields(&RAW_IN_PIXFMTS);
static RAW_IN_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY_RAW, fields: &RAW_IN_FIELDS }];
static RAW_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_IN_OFFERS }];
static VPL_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &VPL_OFFERS }];
static OUT_FIELDS: [FieldDesc; 2] = [
    FieldDesc { field: F_WIDTH, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_HEIGHT, allowed: ConstraintDesc::Any, preferred: None },
];
static SRC_OFFERS: [OfferDesc; 3] = [
    OfferDesc { family: FAM_H265, fields: &OUT_FIELDS },
    OfferDesc { family: FAM_H264, fields: &OUT_FIELDS },
    OfferDesc::any("bytes"),
];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_OFFERS_H265: [OfferDesc; 1] = [OfferDesc { family: FAM_H265, fields: &OUT_FIELDS }];
static SRC_OFFERS_H264: [OfferDesc; 1] = [OfferDesc { family: FAM_H264, fields: &OUT_FIELDS }];
static SRCS_H265: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS_H265 }];
static SRCS_H264: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS_H264 }];
static COUPLINGS: [Coupling; 2] = [Coupling::same(0, 0, F_WIDTH), Coupling::same(0, 0, F_HEIGHT)];

static PROPS: [PropDesc; 12] = [
    // kbit/s.
    PropDesc { name: "bitrate", allowed: Constraint::Any, live: true },
    // kbit/s, 0 is 1.5x the target.
    PropDesc { name: "max-bitrate", allowed: Constraint::Any, live: true },
    PropDesc { name: "gop", allowed: Constraint::Any, live: false },
    // 0 to 7, in a pyramid.
    PropDesc { name: "bframes", allowed: Constraint::Any, live: false },
    PropDesc { name: "lookahead", allowed: Constraint::Any, live: false },
    // auto, vbr, cbr, qvbr, la, la-hrd, icq, cqp.
    PropDesc { name: "rc", allowed: Constraint::Any, live: false },
    // QVBR and ICQ, 1 best to 51.
    PropDesc { name: "quality", allowed: Constraint::Any, live: false },
    PropDesc { name: "qp", allowed: Constraint::Any, live: false },
    // ms at the target bitrate, 0 is the runtime's.
    PropDesc { name: "hrd", allowed: Constraint::Any, live: true },
    // Any nonzero write makes the next frame an IDR.
    PropDesc { name: "force-keyframe", allowed: Constraint::Any, live: true },
    // 1 best quality to 7 best speed.
    PropDesc { name: "preset", allowed: Constraint::Any, live: false },
    // VDEnc path, 1 on, 0 off, -1 as probed.
    PropDesc { name: "low-power", allowed: Constraint::Any, live: false },
];

const fn desc(
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    make_default: Option<fn() -> Box<dyn Element>>,
) -> ElementDesc {
    ElementDesc {
        name: NAME,
        sinks,
        srcs,
        props: &PROPS,
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default,
        io: false,
        couplings: &COUPLINGS,
    }
}

// Cold, registry default.
#[allow(clippy::disallowed_methods)]
fn make_default() -> Box<dyn Element> {
    Box::new(VplEnc::new())
}

static DESC_RAW: ElementDesc = desc(&RAW_SINKS, &SRCS, Some(make_default));
static DESC_VPL: ElementDesc = desc(&VPL_SINKS, &SRCS, None);
static DESCS_FIXED: [[ElementDesc; 2]; 2] = [
    [desc(&RAW_SINKS, &SRCS_H265, None), desc(&RAW_SINKS, &SRCS_H264, None)],
    [desc(&VPL_SINKS, &SRCS_H265, None), desc(&VPL_SINKS, &SRCS_H264, None)],
];

/// Bitstreams in flight or done and waiting for a pool slot, in submit order.
pub const RING: usize = 16;
/// Inputs the runtime may still read, the deepest lookahead and pyramid with the ring behind.
const HELD_MAX: usize = 128;
/// The GOP of an encoder keyed by stamps, a backstop between keys far apart.
const KEYED_GOP: u16 = 256;

/// GopPicSize, GopOptFlag and AdaptiveI. Keyed by stamps the encoder adds no I frame of its own
/// but the backstop's.
fn gop_params(gop: u32, keyed: bool) -> (u16, u16, u16) {
    if keyed {
        let flags = ffi::MFX_GOP_CLOSED | ffi::MFX_GOP_STRICT;
        return (KEYED_GOP, flags, ffi::MFX_CODINGOPTION_OFF);
    }
    (gop.clamp(1, 1024) as u16, ffi::MFX_GOP_CLOSED, ffi::MFX_CODINGOPTION_ON)
}

/// The frame type a submit forces, an IDR when asked and a picture goes in, else the encoder's.
fn forced_type(force_idr: bool, input: bool) -> u16 {
    if force_idr && input {
        return ffi::MFX_FRAMETYPE_I | ffi::MFX_FRAMETYPE_REF | ffi::MFX_FRAMETYPE_IDR;
    }
    0
}

/// Ring positions, the oldest at `head`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fifo {
    head: usize,
    len: usize,
}

impl Fifo {
    pub fn len(self) -> usize {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn is_full(self) -> bool {
        self.len == RING
    }

    /// The slot the next push takes.
    pub fn next(self) -> Option<usize> {
        (!self.is_full()).then_some((self.head + self.len) % RING)
    }

    pub fn push(&mut self) -> Option<usize> {
        let at = self.next()?;
        self.len += 1;
        Some(at)
    }

    pub fn front(self) -> Option<usize> {
        (!self.is_empty()).then_some(self.head)
    }

    pub fn pop(&mut self) -> Option<usize> {
        let at = self.front()?;
        self.head = (self.head + 1) % RING;
        self.len -= 1;
        Some(at)
    }
}

/// One output bitstream. The runtime writes `bs` and `data` until `syncp` completes, so slots sit
/// in a box that never moves and `data` is never resized while in flight.
struct Slot {
    bs: ffi::mfxBitstream,
    syncp: ffi::mfxSyncPoint,
    done: bool,
    submitted: u64,
    data: Vec<u8>,
}

/// An input taken off the pad, its surface not made yet.
struct Waiting {
    buf: profluens_core::buffer::Buffer,
    frame: Option<VplFrame>,
}

/// What stays held once the encoder took a surface.
enum Hold {
    /// The session's own upload, the runtime keeps its own reference.
    Nothing,
    /// Another session's surface, kept so its producer stays open.
    Itself,
    /// An import, kept with the surface it shares memory with.
    With(Surface),
}

/// A session surface stamped for the encoder that the runtime could not take yet.
struct Retry {
    surface: Surface,
    preview: bool,
    hold: Hold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Run,
    /// Null surfaces until the runtime gives up its last frame, then the ring empties.
    Drain(Then),
    Drained,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    /// A new shape or a refused reset, the waiting input opens the next session.
    Reopen,
    End,
}

/// What opening a session for an input did.
enum Opened {
    Ready(Waiting),
    /// The old session drains first, the input waits.
    Drain,
    /// The join is busy, parked, the input waits.
    Wait,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Open {
    Ready,
    Wait,
    Failed,
}

/// What a submit did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Submit {
    /// A bitstream went into the ring.
    Out,
    /// Taken with nothing out yet, or for a null surface nothing left.
    More,
    Busy,
    /// The ring is full, it frees as packets go out.
    Full,
    /// The runtime would not read a foreign surface directly.
    Refused(MfxError),
    Failed,
}

/// Preview stamps, matched at output by pts since the runtime reorders.
struct Previews {
    slots: [u64; 8],
    next: usize,
}

impl Previews {
    const fn new() -> Previews {
        Previews { slots: [u64::MAX; 8], next: 0 }
    }

    fn put(&mut self, ts90: u64) {
        self.slots[self.next] = ts90;
        self.next = (self.next + 1) % self.slots.len();
    }

    fn take(&mut self, ts90: u64) -> bool {
        match self.slots.iter_mut().find(|t| **t == ts90) {
            Some(t) => {
                *t = u64::MAX;
                true
            }
            None => false,
        }
    }

    fn clear(&mut self) {
        self.slots = [u64::MAX; 8];
    }
}

fn candidates_eq(seen: &[(EncPlan, u32)], plan: EncPlan, bframes: u32) -> bool {
    seen.iter().any(|(p, b)| *p == plan && *b == bframes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rc {
    Auto,
    Vbr,
    Cbr,
    Qvbr,
    La,
    LaHrd,
    Icq,
    Cqp,
}

impl Rc {
    fn of_name(s: &str) -> Option<Rc> {
        Some(match s {
            "auto" => Rc::Auto,
            "vbr" => Rc::Vbr,
            "cbr" => Rc::Cbr,
            "qvbr" => Rc::Qvbr,
            "la" => Rc::La,
            "la-hrd" | "la_hrd" => Rc::LaHrd,
            "icq" => Rc::Icq,
            "cqp" => Rc::Cqp,
            _ => return None,
        })
    }

    fn mfx(self) -> u16 {
        match self {
            Rc::Auto | Rc::Vbr => ffi::MFX_RATECONTROL_VBR,
            Rc::Cbr => ffi::MFX_RATECONTROL_CBR,
            Rc::Qvbr => ffi::MFX_RATECONTROL_QVBR,
            Rc::La => ffi::MFX_RATECONTROL_LA,
            Rc::LaHrd => ffi::MFX_RATECONTROL_LA_HRD,
            Rc::Icq => ffi::MFX_RATECONTROL_ICQ,
            Rc::Cqp => ffi::MFX_RATECONTROL_CQP,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub bitrate_kbps: u32,
    pub max_kbps: u32,
    pub gop: u32,
    pub bframes: u32,
    pub lookahead: u16,
    pub rc: Rc,
    pub quality: u16,
    pub qp: u8,
    pub hrd_ms: u32,
    pub preset: u16,
    pub low_power: Option<bool>,
    /// Encodes in flight, 1 to [`RING`]. `None` takes `PF_VPL_ASYNC`, else 1.
    pub async_depth: Option<u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bitrate_kbps: 4000,
            max_kbps: 0,
            gop: 60,
            bframes: 3,
            lookahead: probe::LOOKAHEAD,
            rc: Rc::Auto,
            quality: 24,
            qp: 26,
            hrd_ms: 0,
            preset: ffi::MFX_TARGETUSAGE_BALANCED,
            low_power: None,
            async_depth: None,
        }
    }
}

impl Settings {
    /// Inputs the encoder may hold at once, declared to its producer. The lookahead, the
    /// pyramid, the depth in flight and one being handed over.
    pub fn hold(&self, async_depth: u16) -> u32 {
        u32::from(self.lookahead) + self.bframes.min(7) + 1 + u32::from(async_depth) + 1
    }
}

pub const RATE_MAX_KBPS: u32 = 500_000;
pub const HRD_MAX_MS: u32 = 10_000;

/// Rate fields in 16 bit words with the runtime's multiplier, as ffmpeg's qsvenc does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    pub target: u16,
    pub max: u16,
    pub buffer_kb: u16,
    pub initial_kb: u16,
    pub multiplier: u16,
}

/// Rates past [`RATE_MAX_KBPS`] and buffers past [`HRD_MAX_MS`] are clamped, as the props are.
pub fn rate(bitrate_kbps: u32, max_kbps: u32, hrd_ms: u32) -> Rate {
    let bitrate_kbps = bitrate_kbps.min(RATE_MAX_KBPS);
    let max_kbps = max_kbps.min(RATE_MAX_KBPS);
    let hrd_ms = hrd_ms.min(HRD_MAX_MS);
    let max =
        if max_kbps == 0 { bitrate_kbps + bitrate_kbps / 2 } else { max_kbps.max(bitrate_kbps) };
    let buffer_kb =
        if hrd_ms == 0 { 0 } else { (u64::from(max) * u64::from(hrd_ms) / 8_000).max(1) as u32 };
    let initial_kb = buffer_kb / 2;
    let largest = bitrate_kbps.max(max).max(buffer_kb);
    let multiplier = ((largest + 0x10000) / 0x10000).max(1);
    let w = |v: u32| (v / multiplier) as u16;
    Rate {
        target: w(bitrate_kbps),
        max: w(max),
        buffer_kb: w(buffer_kb),
        initial_kb: w(initial_kb),
        multiplier: multiplier as u16,
    }
}

struct Ext {
    co: ffi::mfxExtCodingOption,
    co2: ffi::mfxExtCodingOption2,
    co3: ffi::mfxExtCodingOption3,
    vsi: ffi::mfxExtVideoSignalInfo,
    mdcv: ffi::mfxExtMasteringDisplayColourVolume,
    cll: ffi::mfxExtContentLightLevelInfo,
    with_signal: bool,
    with_hdr: bool,
}

impl Ext {
    fn list(&mut self, out: &mut [*mut ffi::mfxExtBuffer; 6]) -> usize {
        let mut n = 0;
        for b in [
            (&raw mut self.co).cast::<ffi::mfxExtBuffer>(),
            (&raw mut self.co2).cast(),
            (&raw mut self.co3).cast(),
        ] {
            out[n] = b;
            n += 1;
        }
        if self.with_signal {
            out[n] = (&raw mut self.vsi).cast();
            n += 1;
        }
        if self.with_hdr {
            out[n] = (&raw mut self.mdcv).cast();
            out[n + 1] = (&raw mut self.cll).cast();
            n += 2;
        }
        n
    }
}

pub struct VplEnc {
    input: Input,
    family: Option<&'static str>,
    owner: Option<std::sync::Arc<Owner>>,
    group: Option<std::sync::Arc<VplGroup>>,
    joining: Joining,
    /// The Init candidate whose join is pending.
    opening: usize,
    meter: Meter,
    codec: u32,
    bytes_out: bool,
    settings: Settings,
    plan: Option<EncPlan>,
    param: ffi::mfxVideoParam,
    ext: Box<Ext>,
    inited: bool,
    key: Option<(Geometry, (u32, u32))>,
    in_fmt: Option<InFormat>,
    color: Color,
    stamps: PtsMap,
    previews: Previews,
    packet_size: usize,
    ring: Box<[Slot; RING]>,
    fifo: Fifo,
    headers: Vec<u8>,
    rc_dirty: bool,
    force_idr: bool,
    /// Sorted stamps, ns, each frame within the tolerance of one opens an IDR.
    keys: Option<(std::sync::Arc<[i64]>, i64)>,
    popped: u64,
    emitted: u64,
    ended: bool,
    phase: Phase,
    waiting: Option<Waiting>,
    retry: Option<Retry>,
    busy: Stall,
    sync_stall: Stall,
    /// Foreign inputs the runtime still reads with what they share memory with, kept so their
    /// producer stays open.
    held: Vec<(Surface, Option<Surface>)>,
    /// The component the last foreign input came from, kept by the encoder's own.
    producer: Option<std::sync::Arc<Owner>>,
    /// An earlier producer is kept, let go once the ring is empty.
    settle_due: bool,
    failed: bool,
    disabled: bool,
    warned_rc: bool,
    warned_out: bool,
    pub frames: u64,
}

// SAFETY: SchedHint::Active, one thread, the session its own.
unsafe impl Send for VplEnc {}

impl Default for VplEnc {
    fn default() -> Self {
        Self::new()
    }
}

impl VplEnc {
    pub const IN: SinkPad = IN;
    pub const OUT: SrcPad = OUT;

    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        VplEnc {
            input: Input::raw(),
            family: None,
            owner: None,
            group: None,
            joining: Joining::default(),
            opening: 0,
            meter: Meter::default(),
            codec: ffi::MFX_CODEC_HEVC,
            bytes_out: false,
            settings: Settings::default(),
            plan: None,
            param: ffi::mfxVideoParam::zeroed(),
            // SAFETY: integers and null pointers.
            ext: Box::new(unsafe { std::mem::zeroed() }),
            inited: false,
            key: None,
            in_fmt: None,
            color: Color::default(),
            stamps: PtsMap::new(),
            previews: Previews::new(),
            packet_size: 0,
            ring: Box::new(std::array::from_fn(|_| Slot {
                // SAFETY: zero is a value of every field.
                bs: unsafe { std::mem::zeroed() },
                syncp: std::ptr::null_mut(),
                done: false,
                submitted: 0,
                data: Vec::new(),
            })),
            fifo: Fifo::default(),
            headers: Vec::with_capacity(256),
            rc_dirty: false,
            force_idr: false,
            keys: None,
            popped: 0,
            emitted: 0,
            ended: false,
            phase: Phase::Run,
            waiting: None,
            retry: None,
            busy: Stall::default(),
            sync_stall: Stall::default(),
            held: Vec::with_capacity(HELD_MAX),
            producer: None,
            settle_due: false,
            failed: false,
            disabled: false,
            warned_rc: false,
            warned_out: false,
            frames: 0,
        }
    }

    pub fn new_surfaces(channel: std::sync::Arc<SurfaceChannel>) -> Self {
        let mut e = Self::new();
        e.input = Input::surfaces(channel).for_encode();
        e
    }

    pub fn with_family(mut self, family: &'static str) -> Self {
        self.family = Some(if family == FAM_H264 { FAM_H264 } else { FAM_H265 });
        self.codec = if family == FAM_H264 { ffi::MFX_CODEC_AVC } else { ffi::MFX_CODEC_HEVC };
        self
    }

    pub fn with_settings(mut self, settings: Settings) -> Self {
        self.settings = settings;
        self
    }

    pub fn with_bitrate(mut self, kbps: u32) -> Self {
        self.settings.bitrate_kbps = kbps;
        self
    }

    pub fn with_gop(mut self, gop: u32) -> Self {
        self.settings.gop = gop;
        self
    }

    /// Also force an IDR on every frame within `tol` ns of one of the sorted `keys`. The GOP is
    /// then a closed strict 256 frame backstop with no adaptive I frames.
    pub fn with_keys(mut self, keys: std::sync::Arc<[i64]>, tol: i64) -> Self {
        self.keys = Some((keys, tol.max(0)));
        self
    }

    /// The chain's join group. Without one the session joins what `PF_VPL_JOIN` says.
    pub fn with_group(mut self, group: std::sync::Arc<VplGroup>) -> Self {
        self.group = Some(group);
        self
    }

    pub fn plan(&self) -> Option<EncPlan> {
        self.plan
    }

    pub fn meter(&self) -> Meter {
        self.meter
    }

    fn depth(&self) -> u16 {
        let d = probe::runtime().map_or(1, |rt| rt.async_depth(self.settings.async_depth));
        d.min(RING as u16)
    }

    fn warn(ctx: &mut Ctx, kind: K) {
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: failed(ctx, NAME, kind) });
    }

    fn fail(&mut self, ctx: &mut Ctx, kind: K) {
        if std::mem::replace(&mut self.failed, true) {
            return;
        }
        self.release();
        self.waiting = None;
        let element = ctx.element();
        ctx.post(BusMessage::Error { element, error: failed(ctx, NAME, kind) });
    }

    fn runtime(&mut self, ctx: &mut Ctx) -> Option<&'static Runtime> {
        let rt = probe::runtime();
        if rt.is_none() && !self.disabled {
            self.disabled = true;
            Self::warn(ctx, K::NoDevice);
        }
        rt
    }

    fn pick_plan(&mut self, ctx: &mut Ctx, rt: &Runtime) -> Option<EncPlan> {
        let s = self.settings;
        let caps = &rt.caps;
        let plan = match s.rc {
            Rc::Auto => caps.encode_plan(self.codec, s.lookahead > 0),
            rc => {
                let low_power = s.low_power.unwrap_or_else(|| {
                    !caps.rc_taken(self.codec, false, rc.mfx())
                        && caps.rc_taken(self.codec, true, rc.mfx())
                });
                let lookahead =
                    if matches!(rc, Rc::La | Rc::LaHrd) { s.lookahead.max(10) } else { 0 };
                Some(EncPlan { low_power, rc: rc.mfx(), lookahead, ext_brc: false })
            }
        };
        let mut plan = plan?;
        if let Some(lp) = s.low_power {
            plan.low_power = lp;
        }
        if plan.lookahead > 0 && s.lookahead > 0 {
            plan.lookahead = plan.lookahead.min(s.lookahead.max(10));
        }
        log!(
            &*ctx,
            Level::Info,
            "encode_plan",
            low_power = plan.low_power,
            rc = probe::rc_name(plan.rc),
            lookahead = plan.lookahead,
            ext_brc = plan.ext_brc
        );
        Some(plan)
    }

    fn build(&mut self, geom: Geometry, fps: (u32, u32), plan: EncPlan, bframes: u32) {
        let s = self.settings;
        let on = ffi::MFX_CODINGOPTION_ON;
        let off = ffi::MFX_CODINGOPTION_OFF;
        let ten = geom.fourcc == ffi::MFX_FOURCC_P010;
        let mut p = ffi::mfxVideoParam::zeroed();
        p.AsyncDepth = self.depth();
        p.IOPattern = ffi::MFX_IOPATTERN_IN_VIDEO_MEMORY;
        let m = p.mfx_mut();
        m.CodecId = self.codec;
        m.CodecProfile = match (self.codec, ten) {
            (ffi::MFX_CODEC_HEVC, true) => ffi::MFX_PROFILE_HEVC_MAIN10,
            (ffi::MFX_CODEC_HEVC, false) => ffi::MFX_PROFILE_HEVC_MAIN,
            (_, true) => ffi::MFX_PROFILE_AVC_HIGH10,
            (_, false) => ffi::MFX_PROFILE_AVC_HIGH,
        };
        m.LowPower = if plan.low_power { on } else { off };
        m.TargetUsage = s.preset.clamp(1, 7);
        let (gop, gop_flags, adaptive_i) = gop_params(s.gop, self.keys.is_some());
        m.GopPicSize = gop;
        m.GopRefDist = (bframes.min(7) + 1) as u16;
        m.GopOptFlag = gop_flags;
        // Every I frame an IDR with its parameter sets.
        m.IdrInterval = u16::from(self.codec == ffi::MFX_CODEC_HEVC);
        m.RateControlMethod = plan.rc;
        let r = rate(s.bitrate_kbps.max(1), s.max_kbps, s.hrd_ms);
        match plan.rc {
            ffi::MFX_RATECONTROL_CQP => {
                m.InitialDelayInKB = u16::from(s.qp.clamp(1, 51));
                m.TargetKbps = u16::from(s.qp.clamp(1, 51));
                m.MaxKbps = u16::from(s.qp.clamp(1, 51).saturating_add(2));
            }
            ffi::MFX_RATECONTROL_ICQ => m.TargetKbps = s.quality.clamp(1, 51),
            _ => {
                m.TargetKbps = r.target;
                m.MaxKbps = r.max;
                m.BufferSizeInKB = r.buffer_kb;
                m.InitialDelayInKB = r.initial_kb;
                m.BRCParamMultiplier = r.multiplier;
            }
        }
        m.FrameInfo = frame_info(geom.fourcc, geom.disp_w as u16, geom.disp_h as u16);
        m.FrameInfo.Width = geom.coded_w as u16;
        m.FrameInfo.Height = geom.coded_h as u16;
        m.FrameInfo.CropX = geom.crop_x as u16;
        m.FrameInfo.CropY = geom.crop_y as u16;
        m.FrameInfo.FrameRateExtN = fps.0;
        m.FrameInfo.FrameRateExtD = fps.1;
        self.param = p;

        let e = &mut self.ext;
        e.co = ffi::mfxExtCodingOption {
            Header: header::<ffi::mfxExtCodingOption>(ffi::MFX_EXTBUFF_CODING_OPTION),
            ..Default::default()
        };
        e.co.NalHrdConformance = if s.hrd_ms > 0 { on } else { ffi::MFX_CODINGOPTION_UNKNOWN };
        e.co.VuiNalHrdParameters = if s.hrd_ms > 0 { on } else { ffi::MFX_CODINGOPTION_UNKNOWN };
        e.co.PicTimingSEI = off;
        e.co.AUDelimiter = off;
        e.co2 = ffi::mfxExtCodingOption2 {
            Header: header::<ffi::mfxExtCodingOption2>(ffi::MFX_EXTBUFF_CODING_OPTION2),
            ..Default::default()
        };
        e.co2.LookAheadDepth = plan.lookahead;
        e.co2.ExtBRC = if plan.ext_brc { on } else { ffi::MFX_CODINGOPTION_UNKNOWN };
        e.co2.MBBRC = on;
        e.co2.BRefType = if bframes > 0 { ffi::MFX_B_REF_PYRAMID } else { ffi::MFX_B_REF_OFF };
        e.co2.AdaptiveI = adaptive_i;
        e.co2.AdaptiveB = on;
        e.co2.RepeatPPS =
            if self.codec == ffi::MFX_CODEC_AVC { on } else { ffi::MFX_CODINGOPTION_UNKNOWN };
        e.co3 = ffi::mfxExtCodingOption3 {
            Header: header::<ffi::mfxExtCodingOption3>(ffi::MFX_EXTBUFF_CODING_OPTION3),
            ..Default::default()
        };
        e.co3.QVBRQuality =
            if plan.rc == ffi::MFX_RATECONTROL_QVBR { s.quality.clamp(1, 51) } else { 0 };
        e.vsi = signal_of_color(ffi::MFX_EXTBUFF_VIDEO_SIGNAL_INFO, &self.color);
        e.with_signal = e.vsi.ColourDescriptionPresent != 0 || e.vsi.VideoFullRange != 0;
        let hdr = self.color.hdr;
        e.with_hdr = self.codec == ffi::MFX_CODEC_HEVC && is_hdr(&self.color) && !hdr.is_empty();
        let m = hdr.mastering;
        e.mdcv = ffi::mfxExtMasteringDisplayColourVolume {
            Header: header::<ffi::mfxExtMasteringDisplayColourVolume>(
                ffi::MFX_EXTBUFF_MASTERING_DISPLAY_COLOUR_VOLUME,
            ),
            InsertPayloadToggle: ffi::MFX_PAYLOAD_IDR,
            DisplayPrimariesX: [m.primaries[1][0], m.primaries[2][0], m.primaries[0][0]],
            DisplayPrimariesY: [m.primaries[1][1], m.primaries[2][1], m.primaries[0][1]],
            WhitePointX: m.white[0],
            WhitePointY: m.white[1],
            MaxDisplayMasteringLuminance: m.max_luminance,
            MinDisplayMasteringLuminance: m.min_luminance,
            ..Default::default()
        };
        e.cll = ffi::mfxExtContentLightLevelInfo {
            Header: header::<ffi::mfxExtContentLightLevelInfo>(
                ffi::MFX_EXTBUFF_CONTENT_LIGHT_LEVEL_INFO,
            ),
            InsertPayloadToggle: ffi::MFX_PAYLOAD_IDR,
            MaxContentLightLevel: hdr.light.max_cll,
            MaxPicAverageLightLevel: hdr.light.max_fall,
            ..Default::default()
        };
    }

    // Cold, headers once per session.
    #[allow(clippy::disallowed_methods)]
    fn session(&self) -> Option<&Session> {
        self.owner.as_deref().map(|o| &o.session)
    }

    fn init(&mut self, ctx: &mut Ctx) -> Result<(), MfxError> {
        let Some(owner) = self.owner.clone() else {
            return Err(MfxError { status: ffi::MFX_ERR_NOT_INITIALIZED, call: "init" });
        };
        let session = &owner.session;
        let mut list = [std::ptr::null_mut::<ffi::mfxExtBuffer>(); 6];
        let n = self.ext.list(&mut list);
        let mut p = self.param;
        attach(&mut p, &mut list[..n]);
        let mut out = p;
        // SAFETY: p and its buffers are live, out shares them, the runtime corrects in place.
        let st = unsafe { ffi::MFXVideoENCODE_Query(session.raw(), &mut p, &mut out) };
        if ctx.enabled(Level::Trace) {
            let (i, o) = (p.mfx().FrameInfo, out.mfx().FrameInfo);
            log!(
                &*ctx,
                Level::Trace,
                "query";
                "st={} in {}x{} crop {}x{} fourcc={:#x} rc={} lp={} async={} -> {}x{} rc={} lp={} gop={} refdist={} la={} extbrc={} profile={} level={} async={}",
                MfxError { status: st, call: "" }.name(),
                i.Width,
                i.Height,
                i.CropW,
                i.CropH,
                i.FourCC,
                probe::rc_name(p.mfx().RateControlMethod),
                p.mfx().LowPower,
                p.AsyncDepth,
                o.Width,
                o.Height,
                probe::rc_name(out.mfx().RateControlMethod),
                out.mfx().LowPower,
                out.mfx().GopPicSize,
                out.mfx().GopRefDist,
                self.ext.co2.LookAheadDepth,
                self.ext.co2.ExtBRC,
                out.mfx().CodecProfile,
                out.mfx().CodecLevel,
                out.AsyncDepth
            );
        }
        check(st, "MFXVideoENCODE_Query")?;
        if st > 0 {
            log!(
                &*ctx,
                Level::Info,
                "encode_query",
                status = st,
                rc = probe::rc_name(out.mfx().RateControlMethod),
                gop_ref_dist = out.mfx().GopRefDist,
                lookahead = self.ext.co2.LookAheadDepth
            );
        }
        p = out;
        attach(&mut p, &mut list[..n]);
        self.param = p;
        attach(&mut self.param, &mut []);
        // SAFETY: as above.
        let st = unsafe { ffi::MFXVideoENCODE_Init(session.raw(), &mut p) };
        log!(&*ctx, Level::Trace, "init"; "st={}", MfxError { status: st, call: "" }.name());
        check(st, "MFXVideoENCODE_Init")?;
        self.inited = true;
        owner.set(Component::Encoder);
        if ctx.enabled(Level::Trace) {
            let mut q = self.param;
            let mut req = ffi::mfxFrameAllocRequest::default();
            // SAFETY: q valid, req a valid out struct.
            let st = unsafe { ffi::MFXVideoENCODE_QueryIOSurf(session.raw(), &mut q, &mut req) };
            let suggested = if st >= 0 { u32::from(req.NumFrameSuggested) } else { 0 };
            // The encoder gets no hint, its inputs come from the producer or its own upload pool.
            trace_pool(&*ctx, NAME, suggested, 0, &self.param.mfx().FrameInfo);
        }
        let mut sps = [0u8; 1024];
        let mut pps = [0u8; 256];
        let mut vps = [0u8; 256];
        let mut spspps = ffi::mfxExtCodingOptionSPSPPS {
            Header: header::<ffi::mfxExtCodingOptionSPSPPS>(ffi::MFX_EXTBUFF_CODING_OPTION_SPSPPS),
            SPSBuffer: sps.as_mut_ptr(),
            PPSBuffer: pps.as_mut_ptr(),
            SPSBufSize: sps.len() as u16,
            PPSBufSize: pps.len() as u16,
            SPSId: 0,
            PPSId: 0,
        };
        let mut vpsb = ffi::mfxExtCodingOptionVPS {
            Header: header::<ffi::mfxExtCodingOptionVPS>(ffi::MFX_EXTBUFF_CODING_OPTION_VPS),
            VPSBuffer: vps.as_mut_ptr(),
            VPSBufSize: vps.len() as u16,
            VPSId: 0,
            reserved: [0; 6],
        };
        let mut got = ffi::mfxVideoParam::zeroed();
        let mut hl: [*mut ffi::mfxExtBuffer; 2] =
            [(&raw mut spspps).cast(), (&raw mut vpsb).cast()];
        attach(&mut got, &mut hl[..if self.codec == ffi::MFX_CODEC_HEVC { 2 } else { 1 }]);
        // SAFETY: got and the header buffers are live for the call.
        let st = unsafe { ffi::MFXVideoENCODE_GetVideoParam(session.raw(), &mut got) };
        self.headers.clear();
        if st >= 0 {
            self.packet_size = usize::from(got.mfx().BufferSizeInKB)
                * usize::from(got.mfx().BRCParamMultiplier.max(1))
                * 1000;
            for (buf, len) in [
                (&vps[..], if self.codec == ffi::MFX_CODEC_HEVC { vpsb.VPSBufSize } else { 0 }),
                (&sps[..], spspps.SPSBufSize),
                (&pps[..], spspps.PPSBufSize),
            ] {
                let nal = &buf[..usize::from(len).min(buf.len())];
                if nal.is_empty() {
                    continue;
                }
                if !nal.starts_with(&[0, 0, 1]) && !nal.starts_with(&[0, 0, 0, 1]) {
                    self.headers.extend_from_slice(&[0, 0, 0, 1]);
                }
                self.headers.extend_from_slice(nal);
            }
        }
        if self.packet_size == 0 {
            let f = self.param.mfx().FrameInfo;
            self.packet_size = usize::from(f.Width) * usize::from(f.Height) * 3 / 2;
        }
        self.packet_size = self.packet_size.max(4096);
        self.size_ring();
        log!(
            &*ctx,
            Level::Info,
            "encoder",
            packet = self.packet_size,
            headers = self.headers.len()
        );
        Ok(())
    }

    /// Every slot holds a packet of the size the runtime names. Cold, once per session and on
    /// MFX_ERR_NOT_ENOUGH_BUFFER, never while a slot is in flight.
    #[allow(clippy::disallowed_methods)]
    fn size_ring(&mut self) {
        debug_assert!(self.fifo.is_empty(), "no slot in flight");
        for slot in self.ring.iter_mut() {
            if slot.data.len() < self.packet_size {
                slot.data = vec![0; self.packet_size];
            }
        }
    }

    /// Closes the session, the encoder first so it stops reading its inputs and writing the
    /// ring, then the foreign inputs it read.
    fn release(&mut self) {
        self.retry = None;
        self.input.reset_session();
        self.owner = None;
        self.joining.clear();
        self.opening = 0;
        self.inited = false;
        self.held.clear();
        self.producer = None;
        self.settle_due = false;
        self.fifo = Fifo::default();
        self.busy.clear();
        self.sync_stall.clear();
    }

    /// Opens a session for `geom` at `fps`. The caller drained and released any earlier one. A
    /// busy join parks and resumes at the same candidate on the next call.
    fn ensure(&mut self, ctx: &mut Ctx, geom: Geometry, fps: (u32, u32)) -> Open {
        debug_assert!(!self.inited && self.fifo.is_empty(), "the last session was drained");
        let Some(rt) = self.runtime(ctx) else {
            return Open::Failed;
        };
        let Some(first) = self.pick_plan(ctx, rt) else {
            self.fail(ctx, K::Unsupported(probe::family_of(self.codec).unwrap_or("?")));
            return Open::Failed;
        };
        // Query is lenient and Init strict, so try Init on the plan, then relaxations in order.
        let vbr = ffi::MFX_RATECONTROL_VBR;
        let plain = EncPlan { low_power: first.low_power, rc: vbr, lookahead: 0, ext_brc: false };
        let other = EncPlan { low_power: !first.low_power, rc: vbr, lookahead: 0, ext_brc: false };
        let candidates = [
            (first, self.settings.bframes),
            (plain, self.settings.bframes),
            (plain, 0),
            (other, self.settings.bframes),
            (other, 0),
        ];
        let mut last =
            MfxError { status: ffi::MFX_ERR_INVALID_VIDEO_PARAM, call: "MFXVideoENCODE_Init" };
        let resume = self.opening;
        for (i, (plan, bframes)) in candidates.into_iter().enumerate().skip(resume) {
            if i > 0 && candidates_eq(&candidates[..i], plan, bframes) {
                continue;
            }
            // A failed Init dirties the session and the next Query returns
            // MFX_ERR_UNDEFINED_BEHAVIOR.
            self.owner = None;
            self.inited = false;
            let parent = rt.parent_for(self.group.as_ref());
            match self.joining.open(parent, || rt.session()) {
                Ok(Some(s)) => self.owner = Some(Owner::new(s)),
                Ok(None) => {
                    if self.busy.tick(ctx.steady()) {
                        let e = stalled(ffi::MFX_WRN_IN_EXECUTION, "MFXJoinSession");
                        self.fail(ctx, K::Stalled(e));
                        return Open::Failed;
                    }
                    self.opening = i;
                    self.meter.park(Park::Busy);
                    park(ctx, Park::Busy);
                    return Open::Wait;
                }
                Err(e) => {
                    self.fail(ctx, K::Session(e));
                    return Open::Failed;
                }
            }
            self.opening = 0;
            self.busy.clear();
            self.build(geom, fps, plan, bframes);
            let r = self.init(ctx);
            log!(&*ctx, Level::Trace, "candidate";
                "{i} rc={} lp={} bframes={} -> {}",
                probe::rc_name(plan.rc),
                plan.low_power,
                bframes,
                r.as_ref().err().map_or("ok", |e| e.name())
            );
            match r {
                Ok(()) => {
                    self.plan = Some(plan);
                    self.key = Some((geom, fps));
                    self.rc_dirty = false;
                    if i > 0 {
                        log!(
                            &*ctx,
                            Level::Info,
                            "encode_fallback",
                            step = i,
                            rc = probe::rc_name(plan.rc),
                            low_power = plan.low_power,
                            bframes = bframes
                        );
                    }
                    return Open::Ready;
                }
                Err(e) => last = e,
            }
        }
        self.fail(ctx, K::Init(last));
        Open::Failed
    }

    /// `false` when the runtime refused the new rate and the session has to be reopened.
    fn reset(&mut self, ctx: &mut Ctx) -> bool {
        self.rc_dirty = false;
        let Some(plan) = self.plan else { return true };
        if matches!(plan.rc, ffi::MFX_RATECONTROL_CQP | ffi::MFX_RATECONTROL_ICQ) {
            if !self.warned_rc {
                self.warned_rc = true;
                Self::warn(ctx, K::BitrateOnFixedQuantizer);
            }
            return true;
        }
        let s = self.settings;
        let r = rate(s.bitrate_kbps.max(1), s.max_kbps, s.hrd_ms);
        let m = self.param.mfx_mut();
        m.TargetKbps = r.target;
        m.MaxKbps = r.max;
        m.BufferSizeInKB = r.buffer_kb;
        m.InitialDelayInKB = r.initial_kb;
        m.BRCParamMultiplier = r.multiplier;
        let Some(raw) = self.session().map(|s| s.raw()) else {
            return true;
        };
        let mut list = [std::ptr::null_mut::<ffi::mfxExtBuffer>(); 6];
        let n = self.ext.list(&mut list);
        let mut p = self.param;
        attach(&mut p, &mut list[..n]);
        // SAFETY: initialized on this session, p and its buffers live.
        let st = unsafe { ffi::MFXVideoENCODE_Reset(raw, &mut p) };
        log!(&*ctx, Level::Info, "bitrate", kbps = s.bitrate_kbps, status = st);
        if st < 0 {
            let e = MfxError { status: st, call: "MFXVideoENCODE_Reset" };
            log!(&*ctx, Level::Info, "reset_refused", status = e.status);
            return false;
        }
        true
    }

    /// One EncodeFrameAsync into the next ring slot. A null surface drains.
    fn submit(&mut self, ctx: &mut Ctx, surface: Option<&Surface>) -> Submit {
        const CALL: &str = "MFXVideoENCODE_EncodeFrameAsync";
        let Some(owner) = self.owner.clone() else {
            return Submit::Failed;
        };
        let Some(at) = self.fifo.next() else {
            return Submit::Full;
        };
        // A slot outgrown by MFX_ERR_NOT_ENOUGH_BUFFER grows once the ring is empty.
        if self.ring[at].data.len() < self.packet_size {
            if !self.fifo.is_empty() {
                return Submit::Full;
            }
            self.size_ring();
        }
        let mut ctrl = ffi::mfxEncodeCtrl::zeroed();
        ctrl.FrameType = forced_type(self.force_idr, surface.is_some());
        loop {
            let slot = &mut self.ring[at];
            // SAFETY: zero is a value of every field.
            slot.bs = unsafe { std::mem::zeroed() };
            slot.bs.Data = slot.data.as_mut_ptr();
            slot.bs.MaxLength = slot.data.len() as u32;
            slot.syncp = std::ptr::null_mut();
            slot.done = false;
            slot.submitted = ctx.steady();
            // The runtime copies the control in.
            let ctrl_ptr = if ctrl.FrameType != 0 { &raw mut ctrl } else { std::ptr::null_mut() };
            // SAFETY: initialized encoder, live surface, the slot stays put and unread until its
            // sync point completes.
            let st = unsafe {
                ffi::MFXVideoENCODE_EncodeFrameAsync(
                    owner.session.raw(),
                    ctrl_ptr,
                    surface.map_or(std::ptr::null_mut(), |s| s.raw()),
                    &mut slot.bs,
                    &mut slot.syncp,
                )
            };
            log!(&*ctx, Level::Trace, "encode";
                "st={} surface={} sync={:?} idr={}",
                MfxError { status: st, call: "" }.name(),
                surface.is_some(),
                slot.syncp,
                !ctrl_ptr.is_null()
            );
            match st {
                ffi::MFX_WRN_DEVICE_BUSY | ffi::MFX_WRN_IN_EXECUTION => return Submit::Busy,
                ffi::MFX_ERR_MORE_DATA => {
                    if surface.is_some() {
                        self.force_idr = false;
                    }
                    return Submit::More;
                }
                ffi::MFX_ERR_NOT_ENOUGH_BUFFER => {
                    self.packet_size = self.packet_size.saturating_mul(2);
                    if !self.fifo.is_empty() {
                        return Submit::Full;
                    }
                    self.size_ring();
                    continue;
                }
                st if st < 0 => {
                    let e = MfxError { status: st, call: CALL };
                    if surface.is_some() && self.input.wants_import(&e) {
                        return Submit::Refused(e);
                    }
                    self.fail(ctx, K::Encode(e));
                    return Submit::Failed;
                }
                _ => {}
            }
            if surface.is_some() {
                self.force_idr = false;
            }
            if self.ring[at].syncp.is_null() {
                // A warning with the input taken and nothing out, said once.
                if st != ffi::MFX_ERR_NONE && !std::mem::replace(&mut self.warned_out, true) {
                    Self::warn(ctx, K::Encode(MfxError { status: st, call: CALL }));
                }
                return Submit::More;
            }
            self.fifo.push();
            return Submit::Out;
        }
    }

    /// Polls the oldest bitstream and sends the done ones out in order. Waits ask for a wake.
    fn emit(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        while let Some(at) = self.fifo.front() {
            if !self.ring[at].done {
                let Some(owner) = self.owner.clone() else {
                    self.fifo = Fifo::default();
                    return Ok(());
                };
                match owner.session.sync(self.ring[at].syncp, 0) {
                    Ok(true) => {
                        let now = ctx.steady();
                        self.ring[at].done = true;
                        self.meter.polled(true, now);
                        self.meter.done(self.ring[at].submitted, now);
                        self.sync_stall.clear();
                    }
                    Ok(false) => {
                        self.meter.polled(false, ctx.steady());
                        if self.sync_stall.tick(ctx.steady()) {
                            let e =
                                stalled(ffi::MFX_WRN_IN_EXECUTION, "MFXVideoCORE_SyncOperation");
                            return Err(self.abort(ctx, K::Stalled(e)));
                        }
                        ctx.wake_within(BUSY_WAKE);
                        return Ok(());
                    }
                    Err(e) => return Err(self.abort(ctx, K::Sync(e))),
                }
            }
            // The scheduler drops a buffer pushed past a full ring.
            if !ctx.room(OUT) {
                return Ok(());
            }
            let bs = self.ring[at].bs;
            let (off, len) = (bs.DataOffset as usize, bs.DataLength as usize);
            let Some(data) = self.ring[at].data.get(off..off + len) else {
                let e = MfxError { status: ffi::MFX_ERR_UNDEFINED_BEHAVIOR, call: "mfxBitstream" };
                return Err(self.abort(ctx, K::Encode(e)));
            };
            let hevc = self.codec == ffi::MFX_CODEC_HEVC;
            let idr = bs.FrameType & (ffi::MFX_FRAMETYPE_IDR | ffi::MFX_FRAMETYPE_xIDR) != 0;
            let intra = bs.FrameType & (ffi::MFX_FRAMETYPE_I | ffi::MFX_FRAMETYPE_xI) != 0;
            let (key, prefix) = key_and_prefix(idr, intra, || nal::has_sps(data, hevc));
            let head = if prefix { self.headers.len() } else { 0 };
            let need = head + len;
            let Some(mut buf) = ctx
                .try_alloc_sized(need)
                .map_err(|e| failed(ctx, NAME, K::PacketSlot { need: e.n, capacity: e.max }))?
            else {
                return Ok(());
            };
            let dst = buf.memory.as_mut_full();
            dst[..head].copy_from_slice(&self.headers[..head]);
            dst[head..need].copy_from_slice(data);
            buf.memory.set_len(need);
            let stamp = self.stamps.take(bs.TimeStamp);
            let mut flags = if key { BufferFlags::KEYFRAME } else { BufferFlags::DELTA };
            if self.previews.take(bs.TimeStamp) {
                flags.insert(BufferFlags::PREVIEW);
            }
            log!(&*ctx, Level::Trace, "out";
                "ts={} pts={:?} type={:#x} key={key} prefix={prefix}",
                bs.TimeStamp, stamp.pts, bs.FrameType
            );
            self.emitted += 1;
            buf.pts = stamp.pts;
            // A decode time before zero has no Timestamp, it stays unknown.
            buf.dts = dts_of(bs.DecodeTimeStamp);
            buf.duration = stamp.duration;
            buf.flags = flags;
            ctx.out(OUT).push(buf);
            self.fifo.pop();
            self.frames += 1;
        }
        Ok(())
    }

    /// The session closes before the error goes up, so the runtime writes into the ring no more.
    fn abort(&mut self, ctx: &mut Ctx, kind: K) -> Error {
        self.failed = true;
        self.release();
        self.waiting = None;
        failed(ctx, NAME, kind)
    }

    /// Waits on `p`, failing once the device stayed busy past the stall limit.
    fn wait(&mut self, ctx: &mut Ctx, p: Park) {
        if p == Park::Busy && self.busy.tick(ctx.steady()) {
            let e = stalled(ffi::MFX_WRN_DEVICE_BUSY, "MFXVideoENCODE_EncodeFrameAsync");
            self.fail(ctx, K::Stalled(e));
            return;
        }
        self.meter.park(p);
        park(ctx, p);
    }

    /// Hands a stamped surface to the runtime, `false` when it waits in `retry`.
    fn send(&mut self, ctx: &mut Ctx, r: Retry) -> bool {
        match self.submit(ctx, Some(&r.surface)) {
            Submit::Out | Submit::More => {
                self.busy.clear();
                if r.preview {
                    self.force_idr = true;
                }
                match r.hold {
                    Hold::Nothing => {}
                    Hold::Itself => {
                        self.keep_producer(&r.surface);
                        self.held.push((r.surface, None));
                        self.input.mark_direct();
                    }
                    Hold::With(src) => {
                        self.keep_producer(&src);
                        self.held.push((r.surface, Some(src)));
                    }
                }
                self.let_go(&*ctx);
                true
            }
            Submit::Busy => {
                self.retry = Some(r);
                self.wait(ctx, Park::Busy);
                false
            }
            Submit::Full => {
                self.retry = Some(r);
                false
            }
            Submit::Refused(e) => {
                match self.imported(r) {
                    Ok(Some(r)) => {
                        log!(&*ctx, Level::Info, "encode_import", status = e.status);
                        return self.send(ctx, r);
                    }
                    Ok(None) => self.fail(ctx, K::Encode(e)),
                    Err(kind) => self.fail(ctx, kind),
                }
                true
            }
            Submit::Failed => true,
        }
    }

    /// A refused foreign surface again as an import, where the runtime offers imports.
    fn imported(&mut self, r: Retry) -> Result<Option<Retry>, K> {
        let (Hold::Itself, Some(owner)) = (&r.hold, self.owner.clone()) else {
            return Ok(None);
        };
        if !probe::probe().is_some_and(|c| c.import) {
            return Ok(None);
        }
        self.input.mark_indirect();
        let surface = self.input.for_session(&owner, &r.surface)?;
        surface.set_timestamp(r.surface.timestamp());
        Ok(Some(Retry { surface, preview: r.preview, hold: Hold::With(r.surface) }))
    }

    /// One null surface, `None` once the runtime is empty and `Some(false)` when it waits.
    fn drain_step(&mut self, ctx: &mut Ctx) -> Option<bool> {
        if !self.inited {
            return None;
        }
        match self.submit(ctx, None) {
            Submit::Out => {
                self.busy.clear();
                self.let_go(&*ctx);
                Some(true)
            }
            Submit::More | Submit::Failed | Submit::Refused(_) => None,
            Submit::Busy => {
                self.wait(ctx, Park::Busy);
                Some(false)
            }
            Submit::Full => Some(false),
        }
    }

    /// The encoder's tasks read `from` past its handles. Its component stays open until the
    /// encoder's closes or settles.
    fn keep_producer(&mut self, from: &Surface) {
        let (Some(p), Some(own)) = (from.owner(), &self.owner) else {
            return;
        };
        if self.producer.as_ref().is_some_and(|k| std::sync::Arc::ptr_eq(k, p)) {
            return;
        }
        own.keep(p);
        self.settle_due |= self.producer.is_some();
        self.producer = Some(std::sync::Arc::clone(p));
    }

    fn let_go(&mut self, log: &dyn Loggable) {
        self.held.retain(|(s, _)| s.locked() > 0);
        // Nothing in flight, no task of the encoder's still releases an earlier producer's input.
        if self.settle_due
            && self.fifo.is_empty()
            && let Some(own) = &self.owner
        {
            own.settle(self.producer.as_ref());
            self.settle_due = false;
        }
        if !self.held.is_empty() {
            log!(log, Level::Trace, "held", surfaces = self.held.len());
        }
    }

    /// The shape and rate of an input, or `None` before a format.
    fn shape_of(&self, w: &Waiting) -> Option<(Geometry, (u32, u32))> {
        let f = self.in_fmt?;
        let geom = match &w.frame {
            Some(frame) => frame.geom,
            None => {
                let layout: Layout = f.layout();
                Geometry {
                    coded_w: f.width.next_multiple_of(16),
                    coded_h: f.height.next_multiple_of(16),
                    disp_w: f.width,
                    disp_h: f.height,
                    crop_x: 0,
                    crop_y: 0,
                    fourcc: layout.fourcc(),
                }
            }
        };
        Some((geom, f.rate()))
    }

    /// The next input off the pad, its frame taken from the channel in token order.
    fn take_input(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Option<Waiting> {
        while let Some(buf) = inputs.pop() {
            self.popped += 1;
            if self.in_fmt.is_none() {
                Self::warn(ctx, K::FrameBeforeFormat);
                continue;
            }
            if !self.input.is_surfaces() {
                return Some(Waiting { buf, frame: None });
            }
            let Some(h) = GpuFrameHeader::from_bytes(buf.memory.data()) else {
                Self::warn(ctx, K::FrameHeader);
                continue;
            };
            let Some(frame) = self.input.take(h.token) else {
                Self::warn(ctx, K::NoFrame { token: h.token });
                continue;
            };
            return Some(Waiting { buf, frame: Some(frame) });
        }
        None
    }

    /// Opens the session for the input or asks for a drain first.
    fn open_for(&mut self, ctx: &mut Ctx, w: Waiting) -> Opened {
        let Some(key) = self.shape_of(&w) else {
            return Opened::Failed;
        };
        if self.inited && self.key != Some(key) {
            self.waiting = Some(w);
            self.phase = Phase::Drain(Then::Reopen);
            return Opened::Drain;
        }
        if self.inited && self.rc_dirty && !self.reset(ctx) {
            self.waiting = Some(w);
            self.phase = Phase::Drain(Then::Reopen);
            return Opened::Drain;
        }
        if !self.inited {
            self.rc_dirty = false;
            match self.ensure(ctx, key.0, key.1) {
                Open::Ready => {}
                Open::Wait => {
                    self.waiting = Some(w);
                    return Opened::Wait;
                }
                Open::Failed => return Opened::Failed,
            }
        }
        Opened::Ready(w)
    }

    /// The session surface for an input, stamped.
    fn stamp(&mut self, ctx: &mut Ctx, w: Waiting) -> Result<Option<Retry>, Error> {
        let Some(owner) = self.owner.clone() else {
            return Ok(None);
        };
        let (pts, duration) = (w.buf.pts, w.buf.duration);
        let preview = w.buf.flags.contains(BufferFlags::PREVIEW);
        let (surface, hold) = match &w.frame {
            Some(frame) => {
                let s = self
                    .input
                    .for_session(&owner, &frame.surface)
                    .map_err(|e| failed(ctx, NAME, e))?;
                let hold = if self.input.is_direct() {
                    Hold::Itself
                } else {
                    Hold::With(frame.surface.clone())
                };
                (s, hold)
            }
            None => {
                let Some(f) = self.in_fmt else { return Ok(None) };
                let mut raw: *mut ffi::mfxFrameSurface1 = std::ptr::null_mut();
                // SAFETY: the encoder is initialized, raw a valid out pointer.
                let st =
                    unsafe { ffi::MFXMemory_GetSurfaceForEncode(owner.session.raw(), &mut raw) };
                // SAFETY: one reference for us.
                let got = check(st, "MFXMemory_GetSurfaceForEncode")
                    .ok()
                    .and_then(|_| unsafe { Surface::from_raw(raw) });
                let Some(s) = got.map(|s| s.owned_by(&owner)) else {
                    let e = MfxError { status: st, call: "MFXMemory_GetSurfaceForEncode" };
                    Self::warn(ctx, K::NoSurface(e));
                    return Ok(None);
                };
                if let Err(e) = upload(&s, f, w.buf.memory.data()) {
                    Self::warn(ctx, e);
                    return Ok(None);
                }
                // The runtime keeps its own reference past the submit.
                (s, Hold::Nothing)
            }
        };
        let ts = to_ts90(pts);
        self.stamps.put(ts, Stamp { pts, duration, only: false });
        if preview {
            self.previews.put(ts);
            self.force_idr = true;
        }
        if let (Some((keys, tol)), Some(p)) = (&self.keys, pts.nanos())
            && at_key(keys, *tol, i64::try_from(p).unwrap_or(i64::MAX))
        {
            log!(&*ctx, Level::Debug, "forced_key", pts = pts, reason = "key_list");
            self.force_idr = true;
        }
        surface.set_timestamp(ts);
        log!(&*ctx, Level::Trace, "in"; "ts={ts} pts={pts:?} surface={:?}", surface.raw());
        Ok(Some(Retry { surface, preview, hold }))
    }

    fn finished(&self) -> bool {
        self.failed
            || self.disabled
            || (self.phase == Phase::Drained
                && self.fifo.is_empty()
                && self.waiting.is_none()
                && self.retry.is_none())
    }

    /// Feeds, drains and sends out until something waits, the end held while anything is owed.
    fn run(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        self.let_go(&*ctx);
        loop {
            if self.failed {
                return Ok(());
            }
            self.emit(ctx)?;
            if let Some(r) = self.retry.take()
                && !self.send(ctx, r)
            {
                break;
            }
            match self.phase {
                Phase::Drain(then) => match self.drain_step(ctx) {
                    Some(true) => continue,
                    Some(false) => break,
                    // Empty, the session closes once its last packet went out.
                    None if !self.fifo.is_empty() => break,
                    None => {
                        self.phase = if then == Then::Reopen { Phase::Run } else { Phase::Drained };
                        if then == Then::Reopen {
                            self.release();
                        }
                        continue;
                    }
                },
                Phase::Drained => break,
                Phase::Run => {}
            }
            if self.fifo.is_full() || ctx.command_pending() {
                break;
            }
            let next = match self.waiting.take() {
                Some(w) => w,
                None => match self.take_input(ctx, inputs) {
                    Some(w) => w,
                    // A harness may end with buffers still queued, they go first.
                    None if self.ended && ctx.input_len() == 0 => {
                        self.phase = Phase::Drain(Then::End);
                        continue;
                    }
                    None => break,
                },
            };
            let w = match self.open_for(ctx, next) {
                Opened::Ready(w) => w,
                Opened::Drain | Opened::Failed => continue,
                Opened::Wait => break,
            };
            let Some(r) = self.stamp(ctx, w)? else { continue };
            if !self.send(ctx, r) {
                break;
            }
        }
        self.emit(ctx)?;
        if self.ended && !self.finished() {
            ctx.hold_end();
        }
        Ok(())
    }

    fn discard(&mut self, inputs: &mut Inputs<'_>) {
        while let Some(b) = inputs.pop() {
            if let Some(h) = GpuFrameHeader::from_bytes(b.memory.data()) {
                drop(self.input.take(h.token));
            }
        }
    }
}

/// A packet is a keyframe on an IDR, or an I frame that carries its own SPS. An IDR without one
/// gets the session's parameter sets in front. The SPS scan runs once and only for intra frames.
pub fn key_and_prefix(idr: bool, intra: bool, has_sps: impl FnOnce() -> bool) -> (bool, bool) {
    if !idr && !intra {
        return (false, false);
    }
    let sps = has_sps();
    (idr || sps, idr && !sps)
}

/// The runtime's 90 kHz decode time, unknown when unset or before zero.
pub fn dts_of(ts: i64) -> Timestamp {
    if ts < 0 { Timestamp::NONE } else { from_ts90(ts as u64) }
}

impl Drop for VplEnc {
    /// The session closes before the ring it writes into goes.
    fn drop(&mut self) {
        self.release();
    }
}

impl Element for VplEnc {
    fn desc(&self) -> &'static ElementDesc {
        match self.family {
            Some(f) => {
                &DESCS_FIXED[usize::from(self.input.is_surfaces())][usize::from(f == FAM_H264)]
            }
            None if self.input.is_surfaces() => &DESC_VPL,
            None => &DESC_RAW,
        }
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let s = &mut self.settings;
        if let Some(Value::Int(v)) = ctx.prop("bitrate") {
            s.bitrate_kbps = v.clamp(1, 500_000) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("max-bitrate") {
            s.max_kbps = v.clamp(0, 500_000) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("gop") {
            s.gop = v.clamp(1, 1024) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("bframes") {
            s.bframes = v.clamp(0, 7) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("lookahead") {
            s.lookahead = v.clamp(0, 100) as u16;
        }
        if let Some(Value::Id(id)) = ctx.prop("rc")
            && let Some(rc) = ctx.value_name(id).and_then(Rc::of_name)
        {
            s.rc = rc;
        }
        if let Some(Value::Int(v)) = ctx.prop("quality") {
            s.quality = v.clamp(1, 51) as u16;
        }
        if let Some(Value::Int(v)) = ctx.prop("qp") {
            s.qp = v.clamp(1, 51) as u8;
        }
        if let Some(Value::Int(v)) = ctx.prop("hrd") {
            s.hrd_ms = v.clamp(0, 10_000) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("preset") {
            s.preset = v.clamp(1, 7) as u16;
        }
        if let Some(Value::Int(v)) = ctx.prop("low-power") {
            s.low_power = match v {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            };
        }
        let family = ctx
            .negotiated_src(OUT)
            .and_then(|f| ctx.family_name(f.family))
            .unwrap_or(self.family.unwrap_or(FAM_H265));
        self.codec = if family == FAM_H264 { ffi::MFX_CODEC_AVC } else { ffi::MFX_CODEC_HEVC };
        self.bytes_out = family == "bytes";
        let hold = self.settings.hold(self.depth());
        self.input.declare_hold(hold);
        self.input.start(ctx, NAME, ctx.negotiated_sink(IN))
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.failed || self.disabled {
            self.discard(&mut inputs);
            return Ok(Flow::Ok);
        }
        self.meter.report(ctx.steady(), &*ctx);
        if self.in_fmt.is_none()
            && let Some(f) = ctx.negotiated_sink(IN).cloned()
        {
            self.in_fmt = read_format(ctx, &f);
            self.color = Color::read(ctx, &f);
        }
        self.run(ctx, &mut inputs)?;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(f) => {
                self.in_fmt = read_format(ctx, f);
                self.color = Color::read(ctx, f);
                self.input.format_change(ctx, f);
            }
            Event::PropChanged { name: "bitrate", value: Value::Int(v) } => {
                if *v > 0 {
                    self.settings.bitrate_kbps = (*v).clamp(1, 500_000) as u32;
                    self.rc_dirty = true;
                } else {
                    Self::warn(ctx, K::BitrateZero);
                }
            }
            Event::PropChanged { name: "max-bitrate", value: Value::Int(v) } => {
                self.settings.max_kbps = (*v).clamp(0, 500_000) as u32;
                self.rc_dirty = true;
            }
            Event::PropChanged { name: "hrd", value: Value::Int(v) } => {
                self.settings.hrd_ms = (*v).clamp(0, 10_000) as u32;
                self.rc_dirty = true;
            }
            Event::PropChanged { name: "force-keyframe", value: Value::Int(v) } if *v != 0 => {
                self.force_idr = true
            }
            Event::FlushStart => {
                self.input.flush(ctx);
                self.stamps.clear();
                self.previews.clear();
                self.force_idr = true;
                self.ended = false;
                self.waiting = None;
                self.retry = None;
                self.busy.clear();
                // A drain for a new shape becomes a plain reopen on the next input.
                if self.phase != Phase::Run {
                    self.phase = Phase::Run;
                    self.release();
                }
                if !self.fifo.is_empty() {
                    // Bitstreams still in flight, closing the session stops the writes.
                    self.release();
                } else if let (Some(raw), true) = (self.session().map(|s| s.raw()), self.inited) {
                    let mut list = [std::ptr::null_mut::<ffi::mfxExtBuffer>(); 6];
                    let n = self.ext.list(&mut list);
                    let mut p = self.param;
                    attach(&mut p, &mut list[..n]);
                    // SAFETY: initialized on this session, the frames queued are dropped.
                    let st = unsafe { ffi::MFXVideoENCODE_Reset(raw, &mut p) };
                    if st < 0 {
                        self.release();
                    }
                }
            }
            // The tail comes out gated by room, the end waits for the ring.
            Event::Eos => {
                self.ended = true;
                if !self.failed && !self.disabled {
                    self.run(ctx, &mut Inputs::empty())?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        log!(&*ctx, Level::Trace, "stop";
            "popped={} frames={} emitted={} in_flight={}",
            self.popped,
            self.frames,
            self.emitted,
            self.fifo.len()
        );
        self.meter.report_final(&*ctx);
        self.waiting = None;
        // The encoder first so it lets go of its inputs.
        self.release();
        self.input.stop();
        self.phase = Phase::Run;
        self.key = None;
        self.in_fmt = None;
        self.ended = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_fit_sixteen_bits_through_the_multiplier() {
        let r = rate(4000, 0, 0);
        assert_eq!(r, Rate { target: 4000, max: 6000, buffer_kb: 0, initial_kb: 0, multiplier: 1 });
        let r = rate(80_000, 120_000, 2000);
        assert_eq!(r.multiplier, 2);
        assert_eq!((r.target, r.max), (40_000, 60_000));
        assert_eq!(r.buffer_kb, 15_000);
        assert_eq!(r.initial_kb, 7_500);
        let r = rate(1000, 500, 100);
        assert_eq!((r.target, r.max, r.buffer_kb), (1000, 1000, 12));
    }

    #[test]
    fn rates_past_the_props_clamp_instead_of_overflowing() {
        let r = rate(u32::MAX, u32::MAX, u32::MAX);
        assert_eq!(r, rate(RATE_MAX_KBPS, RATE_MAX_KBPS, HRD_MAX_MS));
        assert!(u32::from(r.target) * u32::from(r.multiplier) <= RATE_MAX_KBPS);
    }

    #[test]
    fn the_ring_hands_slots_out_and_back_in_order() {
        let mut f = Fifo::default();
        assert_eq!((f.front(), f.pop(), f.next()), (None, None, Some(0)));
        for i in 0..RING {
            assert_eq!(f.push(), Some(i));
        }
        assert!(f.is_full() && f.push().is_none() && f.next().is_none());
        assert_eq!((f.pop(), f.pop()), (Some(0), Some(1)));
        assert_eq!(f.push(), Some(0), "wraps to the slot freed first");
        assert_eq!((f.front(), f.len()), (Some(2), RING - 1));
        while f.pop().is_some() {}
        assert!(f.is_empty() && f.next() == Some(1));
    }

    #[test]
    fn only_intra_frames_scan_for_parameter_sets() {
        let never = || -> bool { panic!("a delta frame was scanned") };
        assert_eq!(key_and_prefix(false, false, never), (false, false));
        assert_eq!(key_and_prefix(true, true, || false), (true, true), "an IDR gets the headers");
        assert_eq!(key_and_prefix(true, true, || true), (true, false), "it carries its own");
        assert_eq!(key_and_prefix(false, true, || true), (true, false), "an I frame with an SPS");
        assert_eq!(key_and_prefix(false, true, || false), (false, false), "a plain I frame");
    }

    #[test]
    fn a_decode_time_before_zero_stays_unknown() {
        assert_eq!(dts_of(-3003), Timestamp::NONE);
        assert_eq!(dts_of(9), Timestamp::from_nanos(100_000));
        assert_eq!(dts_of(0), Timestamp::ZERO);
    }

    #[test]
    fn the_hold_covers_the_lookahead_the_pyramid_and_the_depth() {
        let s = Settings::default();
        assert_eq!(s.hold(1), 26, "lookahead 20, three B frames, one P, one in flight, one over");
        let s = Settings { lookahead: 0, bframes: 0, ..Settings::default() };
        assert_eq!(s.hold(4), 6);
        let s = Settings { bframes: 99, ..Settings::default() };
        assert_eq!(s.hold(1), 30, "the pyramid is capped at seven");
    }

    #[test]
    fn stamps_key_a_strict_backstop_gop_without_adaptive_i() {
        let (gop, flags, adaptive) = gop_params(48, true);
        assert_eq!(gop, KEYED_GOP);
        assert_eq!(flags, ffi::MFX_GOP_CLOSED | ffi::MFX_GOP_STRICT);
        assert_eq!(adaptive, ffi::MFX_CODINGOPTION_OFF);
        assert_eq!(gop_params(48, false), (48, ffi::MFX_GOP_CLOSED, ffi::MFX_CODINGOPTION_ON));
        assert_eq!(gop_params(0, false).0, 1);
        assert_eq!(gop_params(5000, false).0, 1024);
    }

    #[test]
    fn a_forced_idr_goes_on_a_picture_and_never_on_a_drain() {
        let idr = ffi::MFX_FRAMETYPE_I | ffi::MFX_FRAMETYPE_REF | ffi::MFX_FRAMETYPE_IDR;
        assert_eq!(forced_type(true, true), idr);
        assert_eq!(forced_type(true, false), 0, "a drain takes no frame type");
        assert_eq!(forced_type(false, true), 0, "the encoder picks");
        assert_eq!(forced_type(false, false), 0);
    }

    #[test]
    fn rate_control_names() {
        assert_eq!(Rc::of_name("la-hrd"), Some(Rc::LaHrd));
        assert_eq!(Rc::of_name("qvbr").map(Rc::mfx), Some(ffi::MFX_RATECONTROL_QVBR));
        assert_eq!(Rc::of_name("x"), None);
    }
}
