//! `vaapih265dec`, the VA-API H.265 decode element. Annex-B access units in, I420 or P010 frames
//! out. The host parses headers and keeps the DPB and RPS (H.265 section 8.3), the driver decodes
//! slices.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::decproc::{self, Twins};
use crate::error::{Transient, VaapiErrorKind as K, Verdict, Warned, failed, log_fatal};
use crate::ffi;
use crate::gpuframe::{
    self, F_LAYOUT, F_MODIFIER, FAMILY_VIDEO_GPU, FrameToken, FrameTx, GPU_FRAME_HEADER_LEN,
    GpuFrame, GpuFrameChannel, GpuFrameHeader,
};
use crate::h265parse::{self, Nal, Pps, ScalingLists, SliceHeader, SliceType, Sps, to_raster};
use crate::probe;
use crate::va::{
    Buffer as VaBuffer, Config, Context, Display, ExportFn, ExportedSurface, MappedImage,
    SurfaceFormat, Surfaces,
};
use crate::vpp::Target;
use profluens_video::color::{self as colour, HdrStatic};
use profluens_video::decoder::{Color, color_fields};
use profluens_video::format::COLOR_OFFER_FIELDS;

const FAMILY: &str = "video/raw";
/// MaxSliceSegmentsPerPicture of level 6.2 (A.4.1 Table A.8).
const SLICES_MAX: u32 = 600;
const F_WIDTH: &str = "width";
const F_HEIGHT: &str = "height";
const F_PIXFMT: &str = "pixfmt";
const PIXFMT_I420: &str = "i420";
const PIXFMT_P010: &str = "p010";

static PIXFMT_VALUES: [ValueDesc; 2] = [ValueDesc::Id(PIXFMT_I420), ValueDesc::Id(PIXFMT_P010)];

static SRC_FIELDS: [FieldDesc; 13] = {
    let c = COLOR_OFFER_FIELDS;
    [
        FieldDesc { field: F_WIDTH, allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc { field: F_HEIGHT, allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc {
            field: F_PIXFMT,
            allowed: ConstraintDesc::Set(&PIXFMT_VALUES),
            preferred: None,
        },
        c[0],
        c[1],
        c[2],
        c[3],
        c[4],
        c[5],
        c[6],
        c[7],
        c[8],
        c[9],
    ]
};
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }];
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("h265/annexb")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

// Cold: make_default boxes one element when the registry builds the default, not per-frame.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "vaapih265dec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VaapiH265Dec::new())),
    io: false,
    couplings: &[],
};

static SRCS_ZEROCOPY: [PadDesc; 1] = [PadDesc { name: "src", offers: OfferDesc::PROBED }];
static DESC_ZEROCOPY: ElementDesc = ElementDesc {
    name: "vaapih265dec",
    sinks: &SINKS,
    srcs: &SRCS_ZEROCOPY,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Clone)]
struct DpbEntry {
    surface: ffi::VASurfaceID,
    poc: i32,
    long_term: bool,
    rps_flag: u32,
    in_rps: bool,
}

struct PendingOut {
    surface: ffi::VASurfaceID,
    /// What is exported, the surface or its twin, the geometry below is its.
    export: ffi::VASurfaceID,
    pts: Timestamp,
    duration: Timestamp,
    width: u32,
    height: u32,
    crop_x: u32,
    crop_y: u32,
    coded_w: u32,
    coded_h: u32,
    format: SurfaceFormat,
    color: Color,
    preview: bool,
    /// Every slice of it is I, the source's scene change most often.
    intra: bool,
}

struct VaState {
    // Field order is drop order, the context goes before its surfaces and its config.
    context: Context,
    /// The scaled twins of the surfaces when the decoder shrinks.
    twins: Option<Twins>,
    /// Decoded as Main 12, the extension picture and slice parameters.
    rext: bool,
    surfaces: Surfaces,
    _config: Config,
    format: SurfaceFormat,
    free: VecDeque<ffi::VASurfaceID>,
    width: u32,
    height: u32,
    crop_x: u32,
    crop_y: u32,
    coded_w: u32,
    coded_h: u32,
    count: u32,
}

/// The VA-API H.265 decode element.
pub struct VaapiH265Dec {
    device: Option<PathBuf>,
    // Field order is drop order, the sessions go before the display they live on.
    va: Option<VaState>,
    retired: Vec<VaState>,
    display: Option<Display>,

    sps: [Option<Arc<Sps>>; 16],
    pps: [Option<Arc<Pps>>; 64],
    scratch: AuScratch,

    dpb: Vec<DpbEntry>,
    poc: h265parse::PocState,
    output_queue: VecDeque<PendingOut>,
    output_key: std::collections::HashMap<ffi::VASurfaceID, (u32, i32)>,
    gop: u32,
    pending: Option<PendingOut>,

    skip_rasl: bool,
    awaiting_irap: bool,

    shape: Option<(u32, u32, u64, u32, Color)>,
    hdr: HdrStatic,
    side: crate::colour::HdrCarry,
    dims: Option<(u32, u32)>,
    alloc_stalled: bool,
    ended: bool,
    seq_end: bool,
    disabled: bool,
    failed: bool,
    transient: Transient,
    warned: Warned,
    emitted: u64,

    zerocopy: Option<Arc<GpuFrameChannel>>,
    tx: Option<FrameTx>,
    released: Vec<FrameToken>,
    next_token: FrameToken,
    /// Surface id -> token for frames exported and not yet released. These stay out of the free
    /// pool so a decode cannot tear the shown picture.
    in_flight: std::collections::HashMap<ffi::VASurfaceID, FrameToken>,
    export: ExportFn,
    link: Option<(u32, u64)>,
    only: bool,
    /// Pictures leave scaled into this box by the decode call, bars around them when fixed.
    shrink: Option<(Target, bool)>,
}

// SAFETY: an Active element runs on one scheduler thread after preroll, its display moves there
// with it and is never used from two threads at once.
unsafe impl Send for VaapiH265Dec {}

impl Default for VaapiH265Dec {
    fn default() -> Self {
        Self::new()
    }
}

impl VaapiH265Dec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::with_zerocopy(None)
    }

    /// Zero-copy mode, decoded surfaces are exported as dma-bufs into `channel` instead of read
    /// back.
    pub fn new_zerocopy(channel: Arc<GpuFrameChannel>) -> Self {
        Self::with_zerocopy(Some(channel))
    }

    /// Zero-copy pictures leave scaled into `target` by the decode call itself, placed in its box
    /// with bars when `fixed`, see [`crate::decproc`]. The driver must do decode processing for
    /// the stream's profile.
    pub fn with_shrink(mut self, target: Target, fixed: bool) -> Self {
        self.shrink = Some((target, fixed));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_export(mut self, export: ExportFn) -> Self {
        self.export = export;
        self
    }

    // Cold: constructor, runs once per element, DPB grows in-place afterward.
    #[allow(clippy::disallowed_methods)]
    fn with_zerocopy(zerocopy: Option<Arc<GpuFrameChannel>>) -> Self {
        VaapiH265Dec {
            only: false,
            device: None,
            display: None,
            va: None,
            retired: Vec::new(),
            sps: std::array::from_fn(|_| None),
            pps: std::array::from_fn(|_| None),
            dpb: Vec::new(),
            poc: h265parse::PocState::new(),
            output_queue: VecDeque::new(),
            output_key: std::collections::HashMap::new(),
            gop: 0,
            pending: None,
            skip_rasl: false,
            awaiting_irap: true,
            shape: None,
            hdr: HdrStatic::default(),
            side: crate::colour::HdrCarry::new(),
            dims: None,
            alloc_stalled: false,
            ended: false,
            seq_end: false,
            disabled: false,
            failed: false,
            transient: Transient::default(),
            warned: Warned::default(),
            emitted: 0,
            zerocopy,
            tx: None,
            released: Vec::new(),
            next_token: 1,
            in_flight: std::collections::HashMap::new(),
            export: ExportedSurface::export,
            link: None,
            scratch: AuScratch::default(),
            shrink: None,
        }
    }

    fn warn(&mut self, ctx: &mut Ctx, kind: K) {
        if !self.warned.note(ctx, kind) {
            return;
        }
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: failed(ctx, DESC.name, kind) });
    }

    fn picture_failed(&mut self, ctx: &mut Ctx, transient: &crate::va::VaError, kind: K) {
        match self.transient.judge(transient) {
            Verdict::Drop => self.warn(ctx, kind),
            Verdict::Fail => self.fail(ctx, kind),
        }
    }

    fn fail(&mut self, ctx: &mut Ctx, kind: K) {
        if std::mem::replace(&mut self.failed, true) {
            return;
        }
        self.pending = None;
        self.output_queue.clear();
        log_fatal(ctx, kind);
        let element = ctx.element();
        ctx.post(BusMessage::Error { element, error: failed(ctx, DESC.name, kind) });
    }

    fn ensure_display(&mut self, ctx: &mut Ctx) -> bool {
        if self.display.is_some() {
            return true;
        }
        let Some(caps) = probe::probe() else {
            if !self.disabled {
                self.disabled = true;
                self.warn(ctx, K::NoDevice);
            }
            return false;
        };
        match Display::open(&caps.device) {
            Ok(d) => {
                self.device = Some(caps.device.clone());
                self.display = Some(d);
                true
            }
            Err(e) => {
                if !self.disabled {
                    self.disabled = true;
                    self.warn(ctx, K::DisplayOpen(e));
                }
                false
            }
        }
    }

    // Cold: VA config/context/surface setup, done once per dimension at announce.
    #[allow(clippy::disallowed_methods)]
    fn ensure_va(&mut self, ctx: &mut Ctx, sps: &Sps, cvs: bool) -> bool {
        let width = sps.width().max(16);
        let height = sps.height().max(16);
        let (crop_x, crop_y) = sps.crop_origin();
        // Surfaces must cover whole CTBs (section 7.4.3.2.1). iHD vaCreateContext fails on the
        // cropped size.
        let ctb = 1u32 << sps.ctb_log2_size_y();
        let coded_w = (sps.pic_width_in_ctbs() * ctb).max(16);
        let coded_h = (sps.pic_height_in_ctbs() * ctb).max(16);
        let ten_bit = sps.is_ten_bit();
        let depth = sps.depth();
        let format = match depth {
            12 => SurfaceFormat::P012,
            10 => SurfaceFormat::P010,
            _ => SurfaceFormat::NV12,
        };
        // VA decodes the format range extensions profiles as Main 12
        let rext = sps.rext || depth > 10;
        let (refs, reorder) = (sps.ref_frames(), sps.reorder_frames());
        // Exported surfaces stay out of the free pool until released, this covers them and the
        // release-drain latency.
        const PRESENTATION_SLACK: u32 = 6;
        let extra = if self.zerocopy.is_some() { PRESENTATION_SLACK } else { 0 };
        let count = crate::va::decode_surface_count(refs, reorder, extra);
        if depth > 10 && self.zerocopy.is_none() {
            self.fail(ctx, unsupported(sps));
            return false;
        }
        // 12 bit pictures leave as P010 twins, a consumer never sees P012
        let twelve = (Target { max_width: 0, max_height: 0, depth: 10 }, false);
        let asked = self.shrink.or((depth > 10).then_some(twelve));
        let shrink = match asked.filter(|_| self.zerocopy.is_some()) {
            Some((t, fixed)) => match decproc::plan(width, height, depth.min(10), t, fixed) {
                Some(p) => Some(p),
                None => {
                    self.fail(ctx, K::ProcSize { width, height });
                    return false;
                }
            },
            None => None,
        };
        if let Some(va) = &self.va {
            if va.width == width
                && va.height == height
                && va.format == format
                && va.rext == rext
                && va.twins.as_ref().map(|t| t.plan) == shrink
                && (va.count >= count || !cvs)
            {
                return true;
            }
            log!(&*ctx, Level::Debug, "reinit", width = width, height = height, depth = depth);
            self.retire_session();
        }
        if ten_bit
            && !rext
            && !probe::probe().is_some_and(|caps| caps.decodes_profile("HEVCMain10"))
        {
            self.fail(ctx, K::NoMain10Profile);
            return false;
        }
        if rext && !probe::probe().is_some_and(|caps| caps.decodes_profile("HEVCMain12")) {
            self.fail(ctx, unsupported(sps));
            return false;
        }
        let Some(display) = &self.display else {
            return false;
        };
        log!(&*ctx, Level::Info, "surfaces", count = count, refs = refs, reorder = reorder);
        log!(
            &*ctx,
            Level::Info,
            "session",
            width = width,
            height = height,
            coded_w = coded_w,
            depth = depth
        );

        let profile = match (rext, ten_bit) {
            (true, _) => ffi::VAProfileHEVCMain12,
            (false, true) => ffi::VAProfileHEVCMain10,
            (false, false) => ffi::VAProfileHEVCMain,
        };
        let config = match shrink {
            Some(_) => Config::new_decode_processing(display, profile, format),
            None => Config::new_decode_for(display, profile, format),
        };
        let config = match config {
            Ok(c) => c,
            Err(e) => {
                self.fail(ctx, K::SessionSetup(e));
                return false;
            }
        };
        let surfaces = match Surfaces::new(display, format, coded_w, coded_h, count) {
            Ok(s) => s,
            Err(e) => {
                self.fail(ctx, K::SessionSetup(e));
                return false;
            }
        };
        let context =
            match Context::new(display, &config, coded_w as i32, coded_h as i32, &surfaces) {
                Ok(c) => c,
                Err(e) => {
                    self.fail(ctx, K::SessionSetup(e));
                    return false;
                }
            };
        // the decoded picture is the SPS's luma size, its conformance window the crop
        let full = (sps.pic_width_in_luma_samples, sps.pic_height_in_luma_samples);
        let crop = (crop_x, crop_y, width, height);
        let twins = match shrink.map(|p| Twins::new(display, p, count, crop, full)).transpose() {
            Ok(t) => t,
            Err(e) => {
                self.fail(ctx, e);
                return false;
            }
        };
        if let Some(t) = &twins {
            let (w, h) = (t.plan.width, t.plan.height);
            log!(&*ctx, Level::Info, "shrink", width = w, height = h, layout = t.kind().name);
        }
        let free = VecDeque::from(surfaces.ids().to_vec());
        self.va = Some(VaState {
            _config: config,
            context,
            rext,
            twins,
            format,
            surfaces,
            free,
            width,
            height,
            crop_x,
            crop_y,
            coded_w,
            coded_h,
            count,
        });
        self.dims = Some((width, height));
        true
    }

    fn acquire_surface(&mut self) -> Option<ffi::VASurfaceID> {
        self.va.as_mut().and_then(|va| va.free.pop_front())
    }

    fn surfaces_exhausted(&self) -> bool {
        self.va.as_ref().is_some_and(|va| va.free.is_empty())
    }

    fn release_surface(&mut self, id: ffi::VASurfaceID) {
        match &mut self.va {
            Some(va) if va.surfaces.ids().contains(&id) => {
                if !va.free.contains(&id) {
                    va.free.push_back(id);
                }
            }
            _ => self.prune_retired(),
        }
    }

    /// Destroyed at once, a release put a dead id on the new free list and vaBeginPicture failed
    /// with VA_STATUS_ERROR_INVALID_SURFACE. So the session lives until no holder names its
    /// surfaces.
    fn retire_session(&mut self) {
        self.dpb.clear();
        self.awaiting_irap = true;
        if let Some(va) = self.va.take() {
            self.retired.push(va);
        }
        self.prune_retired();
    }

    fn prune_retired(&mut self) {
        let mut at = 0;
        while at < self.retired.len() {
            if self.retired[at].surfaces.ids().iter().any(|&id| self.surface_referenced(id)) {
                at += 1;
            } else {
                self.retired.swap_remove(at);
            }
        }
    }

    fn surface_referenced(&self, id: ffi::VASurfaceID) -> bool {
        self.dpb.iter().any(|e| e.surface == id)
            || self.output_queue.iter().any(|o| o.surface == id)
            || self.pending.as_ref().is_some_and(|o| o.surface == id)
            || self.in_flight.contains_key(&id)
    }

    fn release_if_unreferenced(&mut self, id: ffi::VASurfaceID) {
        if !self.surface_referenced(id) {
            self.release_surface(id);
        }
    }

    fn decode_au(&mut self, ctx: &mut Ctx, au: &[u8], pts: Timestamp, duration: Timestamp) {
        let mut sc = std::mem::take(&mut self.scratch);
        self.decode_au_with(ctx, au, pts, duration, &mut sc);
        sc.clear();
        self.scratch = sc;
    }

    fn decode_au_with(
        &mut self,
        ctx: &mut Ctx,
        au: &[u8],
        pts: Timestamp,
        duration: Timestamp,
        sc: &mut AuScratch,
    ) {
        h265parse::split_nals_into(au, &mut sc.nals);
        self.seq_end =
            sc.nals.iter().any(|n| matches!(n.nal_type, h265parse::NAL_EOS | h265parse::NAL_EOB));

        for nal in &sc.nals {
            match nal.nal_type {
                h265parse::NAL_SPS => match h265parse::parse_sps(nal.raw(au)) {
                    Some(sps) if (sps.sps_id as usize) < 16 => {
                        let id = sps.sps_id as usize;
                        self.sps[id] = Some(Arc::new(sps));
                    }
                    _ => self.warned.note_name(ctx, "sps_unparsed", 0),
                },
                h265parse::NAL_PPS => match h265parse::parse_pps(nal.raw(au)) {
                    Some(pps) if (pps.pps_id as usize) < 64 => {
                        let id = pps.pps_id as usize;
                        self.pps[id] = Some(Arc::new(pps));
                    }
                    _ => self.warned.note_name(ctx, "pps_unparsed", 0),
                },
                _ => {}
            }
        }

        sc.slices.clear();
        sc.slices.extend(sc.nals.iter().filter(|n| h265parse::is_slice_nal(n.nal_type)).copied());
        if sc.slices.is_empty() {
            return; // parameter-set / SEI / AUD-only AU: nothing to output
        }
        let first = sc.slices[0];
        let nal_type = first.nal_type;
        let temporal_id = first.temporal_id;
        let is_idr = h265parse::is_idr(nal_type);
        let is_irap = h265parse::is_irap(nal_type);

        if self.awaiting_irap && !is_irap {
            log!(&*ctx, Level::Trace, "await_irap", pts = pts, nal_type = nal_type);
            return; // wait for a clean random-access point
        }
        if h265parse::is_rasl(nal_type) && self.skip_rasl {
            log!(&*ctx, Level::Trace, "skip_rasl", pts = pts);
            return;
        }

        let Some(pps_id) = peek_pps_id(first.raw(au), is_irap) else {
            self.warn(ctx, K::PpsIdUnreadable);
            return;
        };
        let Some(pps) = self.pps.get(pps_id as usize).and_then(|p| p.clone()) else {
            self.warn(ctx, K::UnknownPps);
            return;
        };
        let Some(sps) = self.sps.get(pps.sps_id as usize).and_then(|s| s.clone()) else {
            self.warn(ctx, K::UnknownSps);
            return;
        };

        let before = self.only || ctx.before_presentation(pts, Timestamp::ZERO);
        if before && h265parse::non_reference(&sc.nals, sps.max_sub_layers_minus1) {
            log!(&*ctx, Level::Trace, "skip_nonref", pts = pts);
            ctx.count_drop();
            return;
        }

        if !sps.supported() || pps.other_ext {
            self.fail(ctx, unsupported(&sps));
            return;
        }
        // 7.4.3.3, every tile is at least one CTB wide and high.
        let (columns, rows) = (pps.num_tile_columns_minus1 + 1, pps.num_tile_rows_minus1 + 1);
        if pps.tiles_enabled_flag
            && (columns > sps.pic_width_in_ctbs() || rows > sps.pic_height_in_ctbs())
        {
            self.warn(ctx, K::PpsTiles { columns, rows });
            return;
        }

        // Each segment codes at least one CTB, and A.4.1 bounds the count per picture.
        let most = (sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs()).min(SLICES_MAX);
        if sc.slices.len() > most as usize {
            self.warn(ctx, K::TooManySlices { slices: sc.slices.len(), most });
            return;
        }
        sc.headers.clear();
        for n in &sc.slices {
            let prev = sc.headers.last();
            let Some(sh) =
                h265parse::parse_slice_header_after(n.raw(au), n.nal_type, &sps, &pps, prev)
            else {
                self.warn(ctx, K::SliceSegment);
                return;
            };
            sc.headers.push(sh);
        }
        let sh0 = &sc.headers[0];

        let cvs = is_irap && (is_idr || h265parse::is_bla(nal_type) || self.awaiting_irap);
        if !self.ensure_va(ctx, &sps, cvs) {
            return;
        }

        let no_rasl_output = is_idr || h265parse::is_bla(nal_type) || self.awaiting_irap;
        if is_irap && no_rasl_output {
            self.reset_dpb();
            self.gop = self.gop.wrapping_add(1);
        }
        if is_irap {
            self.skip_rasl = no_rasl_output;
        }
        self.awaiting_irap = false;

        let poc = self.poc.compute(&sps, sh0, is_idr, is_irap && no_rasl_output, temporal_id);

        self.apply_rps(&sps, sh0, poc, is_idr);

        let Some(target) = self.acquire_surface() else {
            self.warn(ctx, K::NoFreeSurface);
            return;
        };

        let Some((export, width, height, crop_x, crop_y, coded_w, coded_h, format)) =
            self.va.as_ref().map(|v| match &v.twins {
                Some(t) => {
                    let twin = t.of(&v.surfaces, target).unwrap_or(target);
                    let (w, h) = (t.plan.width, t.plan.height);
                    (twin, w, h, 0, 0, t.coded.0, t.coded.1, t.kind().surface())
                }
                None => {
                    (target, v.width, v.height, v.crop_x, v.crop_y, v.coded_w, v.coded_h, v.format)
                }
            })
        else {
            return;
        };
        if let Err(e) = self.submit_picture(
            &sps,
            &pps,
            &sc.headers,
            &sc.slices,
            au,
            target,
            poc,
            nal_type,
            &mut sc.buffers,
        ) {
            self.picture_failed(ctx, &e, K::Submit(e));
            self.release_surface(target);
            return;
        }

        self.dpb.push(DpbEntry {
            surface: target,
            poc,
            long_term: false,
            rps_flag: 0,
            in_rps: true,
        });

        let preview = before && !self.only && sh0.pic_output_flag && ctx.take_preview();
        if before && sh0.pic_output_flag && !preview {
            log!(&*ctx, Level::Trace, "decode_only", pts = pts);
            ctx.count_drop();
        } else if sh0.pic_output_flag {
            self.output_queue.push_back(PendingOut {
                surface: target,
                export,
                pts,
                duration,
                width,
                height,
                crop_x,
                crop_y,
                coded_w,
                coded_h,
                format,
                color: Color { hdr: self.hdr, dovi: self.side.dovi, ..sps.colour },
                preview,
                intra: sc.headers.iter().all(|h| h.slice_type == SliceType::I),
            });
            self.reorder_output(target, poc);
        }
        self.maybe_bump(&sps);
    }

    fn apply_rps(&mut self, sps: &Sps, sh: &SliceHeader, cur_poc: i32, is_idr: bool) {
        for e in &mut self.dpb {
            (e.rps_flag, e.in_rps) = (0, false);
        }
        if is_idr {
            return;
        }

        let max_poc_lsb = sps.max_poc_lsb();

        let st = sh
            .st_rps
            .delta_poc_s0
            .iter()
            .zip(&sh.st_rps.used_s0)
            .map(|e| (e, ffi::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE));
        let st = st.chain(
            sh.st_rps
                .delta_poc_s1
                .iter()
                .zip(&sh.st_rps.used_s1)
                .map(|e| (e, ffi::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER)),
        );
        for ((delta, used), flag) in st {
            let ref_poc = cur_poc.wrapping_add(*delta);
            if let Some(e) = self.dpb.iter_mut().find(|e| e.poc == ref_poc) {
                (e.rps_flag, e.long_term, e.in_rps) = (if *used { flag } else { 0 }, false, true);
            }
        }
        for lt in &sh.long_term_refs {
            let target_lsb = lt.poc_lsb as i32;
            let found = if lt.delta_poc_msb_present {
                let cur_lsb = ((cur_poc % max_poc_lsb) + max_poc_lsb) % max_poc_lsb;
                // Eq. 8-5 in i64, the accumulated cycle times MaxPocLsb passes i32.
                let target = i64::from(cur_poc)
                    - i64::from(lt.delta_poc_msb_cycle) * i64::from(max_poc_lsb)
                    - i64::from(cur_lsb - target_lsb);
                self.dpb.iter_mut().find(|e| i64::from(e.poc) == target)
            } else {
                self.dpb
                    .iter_mut()
                    .find(|e| (e.poc % max_poc_lsb + max_poc_lsb) % max_poc_lsb == target_lsb)
            };
            if let Some(e) = found {
                let flag = if lt.used_by_curr_pic { ffi::VA_PICTURE_HEVC_RPS_LT_CURR } else { 0 };
                (e.rps_flag, e.long_term, e.in_rps) = (flag, true, true);
            }
        }

        while let Some(at) = self.dpb.iter().position(|e| !e.in_rps) {
            let e = self.dpb.remove(at);
            self.release_if_unreferenced(e.surface);
        }
    }

    fn reorder_output(&mut self, surface: ffi::VASurfaceID, poc: i32) {
        self.output_key.insert(surface, (self.gop, poc));
        let mut i = self.output_queue.len().saturating_sub(1);
        while i > 0 {
            let a = self.output_queue[i - 1].surface;
            let b = self.output_queue[i].surface;
            let ka = *self.output_key.get(&a).unwrap_or(&(u32::MAX, i32::MAX));
            let kb = *self.output_key.get(&b).unwrap_or(&(u32::MAX, i32::MAX));
            if ka > kb {
                self.output_queue.swap(i - 1, i);
                i -= 1;
            } else {
                break;
            }
        }
    }

    fn maybe_bump(&mut self, sps: &Sps) {
        let budget = if self.seq_end { 0 } else { sps.reorder_frames() as usize };
        while self.output_queue.len() > budget && self.pending.is_none() {
            if let Some(out) = self.output_queue.pop_front() {
                self.pending = Some(out);
            } else {
                break;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_picture(
        &mut self,
        sps: &Sps,
        pps: &Pps,
        headers: &[SliceHeader],
        slice_nals: &[Nal],
        au: &[u8],
        target: ffi::VASurfaceID,
        cur_poc: i32,
        nal_type: u8,
        keep: &mut Vec<VaBuffer>,
    ) -> Result<(), crate::va::VaError> {
        let (Some(display), Some(va)) = (&self.display, &self.va) else {
            return Err(crate::va::VaError { status: -1, ctx: "no session" });
        };
        let context = &va.context;

        let mut pic = zeroed_pic_param_hevc();
        pic.CurrPic = ffi::VAPictureHEVC {
            picture_id: target,
            pic_order_cnt: cur_poc,
            flags: 0,
            va_reserved: [0; ffi::VA_PADDING_LOW],
        };
        pic.ReferenceFrames = [ffi::VAPictureHEVC::invalid(); 15];
        let mut ref_index: [(ffi::VASurfaceID, u8); 15] = [(0, 0); 15];
        let mut ref_index_len = 0usize;
        for (i, e) in self.dpb.iter().filter(|e| e.in_rps).take(15).enumerate() {
            let mut flags = e.rps_flag;
            if e.long_term {
                flags |= ffi::VA_PICTURE_HEVC_LONG_TERM_REFERENCE;
            }
            pic.ReferenceFrames[i] = ffi::VAPictureHEVC {
                picture_id: e.surface,
                pic_order_cnt: e.poc,
                flags,
                va_reserved: [0; ffi::VA_PADDING_LOW],
            };
            ref_index[ref_index_len] = (e.surface, i as u8);
            ref_index_len += 1;
        }
        let ref_index = &ref_index[..ref_index_len];

        pic.pic_width_in_luma_samples = sps.pic_width_in_luma_samples as u16;
        pic.pic_height_in_luma_samples = sps.pic_height_in_luma_samples as u16;
        pic.pic_fields = pack_pic_fields(sps, pps);
        pic.sps_max_dec_pic_buffering_minus1 = sps.sps_max_dec_pic_buffering_minus1 as u8;
        pic.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8 as u8;
        pic.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8 as u8;
        pic.pcm_sample_bit_depth_luma_minus1 = sps.pcm_sample_bit_depth_luma_minus1 as u8;
        pic.pcm_sample_bit_depth_chroma_minus1 = sps.pcm_sample_bit_depth_chroma_minus1 as u8;
        pic.log2_min_luma_coding_block_size_minus3 =
            sps.log2_min_luma_coding_block_size_minus3 as u8;
        pic.log2_diff_max_min_luma_coding_block_size =
            sps.log2_diff_max_min_luma_coding_block_size as u8;
        pic.log2_min_transform_block_size_minus2 = sps.log2_min_transform_block_size_minus2 as u8;
        pic.log2_diff_max_min_transform_block_size =
            sps.log2_diff_max_min_transform_block_size as u8;
        pic.log2_min_pcm_luma_coding_block_size_minus3 =
            sps.log2_min_pcm_luma_coding_block_size_minus3 as u8;
        pic.log2_diff_max_min_pcm_luma_coding_block_size =
            sps.log2_diff_max_min_pcm_luma_coding_block_size as u8;
        pic.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra as u8;
        pic.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter as u8;
        pic.init_qp_minus26 = pps.init_qp_minus26 as i8;
        pic.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth as u8;
        pic.pps_cb_qp_offset = pps.pps_cb_qp_offset as i8;
        pic.pps_cr_qp_offset = pps.pps_cr_qp_offset as i8;
        pic.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2 as u8;
        pic.num_tile_columns_minus1 = pps.num_tile_columns_minus1 as u8;
        pic.num_tile_rows_minus1 = pps.num_tile_rows_minus1 as u8;
        // iHD does not derive uniform tile sizes and fails the picture with zeros.
        if pps.tiles_enabled_flag {
            let (uniform, cols, rows) =
                (pps.uniform_spacing_flag, pps.num_tile_columns_minus1, pps.num_tile_rows_minus1);
            let (w, h) = (sps.pic_width_in_ctbs(), sps.pic_height_in_ctbs());
            h265parse::tile_sizes_minus1(
                uniform,
                &pps.column_width_minus1,
                cols + 1,
                w,
                &mut pic.column_width_minus1,
            );
            h265parse::tile_sizes_minus1(
                uniform,
                &pps.row_height_minus1,
                rows + 1,
                h,
                &mut pic.row_height_minus1,
            );
        }
        pic.slice_parsing_fields = pack_slice_parsing_fields(sps, pps, nal_type, headers);
        pic.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4 as u8;
        pic.num_short_term_ref_pic_sets = sps.num_short_term_ref_pic_sets as u8;
        pic.num_long_term_ref_pic_sps = sps.num_long_term_ref_pics_sps as u8;
        pic.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1 as u8;
        pic.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1 as u8;
        pic.pps_beta_offset_div2 = pps.pps_beta_offset_div2 as i8;
        pic.pps_tc_offset_div2 = pps.pps_tc_offset_div2 as i8;
        pic.num_extra_slice_header_bits = pps.num_extra_slice_header_bits as u8;
        pic.st_rps_bits = headers[0].st_rps_bits;

        let pic_buf = match va.rext {
            true => {
                let ext = pic_ext(pic, sps, pps);
                VaBuffer::new_struct(display, context, ffi::VAPictureParameterBufferType, &ext)?
            }
            false => {
                VaBuffer::new_struct(display, context, ffi::VAPictureParameterBufferType, &pic)?
            }
        };

        let iq_buf = if sps.scaling_list_enabled_flag {
            let lists =
                pps.scaling_lists.or(sps.scaling_lists).unwrap_or_else(ScalingLists::default_lists);
            Some(VaBuffer::new_struct(
                display,
                context,
                ffi::VAIQMatrixBufferType,
                &iq_matrix(&lists),
            )?)
        } else {
            None
        };

        // every picture names its twin, iHD would write the last one's again
        let mut regions = va.twins.as_ref().map(|t| {
            let twin = t.of(&va.surfaces, target).unwrap_or(ffi::VA_INVALID_SURFACE);
            decproc::Regions::new(t, twin)
        });
        let proc_buf = match regions.as_mut() {
            Some(r) => Some(decproc::pipeline(display, context, target, r)?),
            None => None,
        };

        keep.clear();
        context.begin(target)?;
        let rendered = (|| {
            match &iq_buf {
                Some(iq) => context.render(&[pic_buf.id(), iq.id()])?,
                None => context.render(&[pic_buf.id()])?,
            }
            if let Some(p) = &proc_buf {
                context.render(&[p.id()])?;
            }
            let last = headers.len().saturating_sub(1);
            for (i, (sh, nal)) in headers.iter().zip(slice_nals.iter()).enumerate() {
                let sp = self.build_slice_param(sps, sh, nal.raw(au), i == last, ref_index);
                let ty = ffi::VASliceParameterBufferType;
                let sp_buf = match va.rext {
                    true => VaBuffer::new_struct(display, context, ty, &slice_ext(sp, sps, sh))?,
                    false => VaBuffer::new_struct(display, context, ty, &sp)?,
                };
                let data_buf = VaBuffer::new_data(display, context, nal.raw(au))?;
                context.render(&[sp_buf.id(), data_buf.id()])?;
                keep.push(sp_buf);
                keep.push(data_buf);
            }
            Ok(())
        })();
        // vaEndPicture runs after a failed render too, so the context is not left mid-picture.
        rendered.and(context.end())
    }

    fn build_slice_param(
        &self,
        sps: &Sps,
        sh: &SliceHeader,
        raw_nal: &[u8],
        last_slice: bool,
        ref_index: &[(ffi::VASurfaceID, u8)],
    ) -> ffi::VASliceParameterBufferHEVC {
        let (ref_pic_list, collocated_ref_idx) = self.build_ref_pic_list(sh, ref_index);
        let long_slice_flags = pack_long_slice_flags(sh, last_slice);

        let mut sp: ffi::VASliceParameterBufferHEVC = zeroed_slice_param_hevc();
        sp.slice_data_size = raw_nal.len() as u32;
        sp.slice_data_offset = 0;
        sp.slice_data_flag = ffi::VA_SLICE_DATA_FLAG_ALL;
        sp.slice_data_byte_offset = sh.slice_data_byte_offset;
        sp.slice_segment_address = sh.slice_segment_address;
        sp.RefPicList = ref_pic_list;
        sp.long_slice_flags = long_slice_flags;
        sp.collocated_ref_idx = collocated_ref_idx;
        sp.num_ref_idx_l0_active_minus1 = sh.num_ref_idx_l0_active_minus1 as u8;
        sp.num_ref_idx_l1_active_minus1 = sh.num_ref_idx_l1_active_minus1 as u8;
        sp.slice_qp_delta = sh.slice_qp_delta as i8;
        sp.slice_cb_qp_offset = sh.slice_cb_qp_offset as i8;
        sp.slice_cr_qp_offset = sh.slice_cr_qp_offset as i8;
        sp.slice_beta_offset_div2 = sh.slice_beta_offset_div2 as i8;
        sp.slice_tc_offset_div2 = sh.slice_tc_offset_div2 as i8;
        sp.five_minus_max_num_merge_cand = sh.five_minus_max_num_merge_cand as u8;
        sp.num_entry_point_offsets = sh.num_entry_point_offsets.min(u16::MAX as u32) as u16;
        sp.entry_offset_to_subset_array = 0;
        sp.slice_data_num_emu_prevn_bytes = sh.num_emu_prev_bytes;

        // VA wants the derived ChromaOffset (eq. 7-56), not the parsed delta_chroma_offset.
        let _ = sps;
        if let Some(t) = &sh.pred_weights {
            sp.luma_log2_weight_denom = t.luma_log2_weight_denom as u8;
            sp.delta_chroma_log2_weight_denom = t.delta_chroma_log2_weight_denom as i8;
            let chroma_denom =
                (t.luma_log2_weight_denom as i32 + t.delta_chroma_log2_weight_denom).clamp(0, 7);
            let derive_chroma_offset = |delta_w: i32, delta_off: i32| -> i8 {
                let chroma_weight = (1i32 << chroma_denom) + delta_w;
                let v = delta_off - ((128 * chroma_weight) >> chroma_denom) + 128;
                v.clamp(-128, 127) as i8
            };
            for (i, e) in t.l0.iter().take(15).enumerate() {
                sp.delta_luma_weight_l0[i] = e.delta_luma_weight as i8;
                sp.luma_offset_l0[i] = e.luma_offset as i8;
                for c in 0..2 {
                    sp.delta_chroma_weight_l0[i][c] = e.delta_chroma_weight[c] as i8;
                    sp.ChromaOffsetL0[i][c] =
                        derive_chroma_offset(e.delta_chroma_weight[c], e.chroma_offset[c]);
                }
            }
            for (i, e) in t.l1.iter().take(15).enumerate() {
                sp.delta_luma_weight_l1[i] = e.delta_luma_weight as i8;
                sp.luma_offset_l1[i] = e.luma_offset as i8;
                for c in 0..2 {
                    sp.delta_chroma_weight_l1[i][c] = e.delta_chroma_weight[c] as i8;
                    sp.ChromaOffsetL1[i][c] =
                        derive_chroma_offset(e.delta_chroma_weight[c], e.chroma_offset[c]);
                }
            }
        }
        sp
    }

    fn build_ref_pic_list(
        &self,
        sh: &SliceHeader,
        ref_index: &[(ffi::VASurfaceID, u8)],
    ) -> ([[u8; 15]; 2], u8) {
        let mut list = [[0xFFu8; 15]; 2];
        if !sh.slice_type.is_inter() {
            return (list, if sh.slice_temporal_mvp_enabled_flag { 0 } else { 0xFF });
        }

        let mut before: [(i32, u8); 16] = [(0, 0); 16];
        let mut before_len = 0usize;
        for e in self.dpb.iter().filter(|e| e.rps_flag == ffi::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE) {
            if let Some(&(_, i)) = ref_index.iter().find(|(s, _)| *s == e.surface)
                && before_len < before.len()
            {
                before[before_len] = (e.poc, i);
                before_len += 1;
            }
        }
        let before = &mut before[..before_len];
        before.sort_by_key(|a| std::cmp::Reverse(a.0)); // descending (nearest below cur first)
        let mut after: [(i32, u8); 16] = [(0, 0); 16];
        let mut after_len = 0usize;
        for e in self.dpb.iter().filter(|e| e.rps_flag == ffi::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER) {
            if let Some(&(_, i)) = ref_index.iter().find(|(s, _)| *s == e.surface)
                && after_len < after.len()
            {
                after[after_len] = (e.poc, i);
                after_len += 1;
            }
        }
        let after = &mut after[..after_len];
        after.sort_by_key(|a| a.0); // ascending (nearest above cur first)
        let mut long: [u8; 16] = [0; 16];
        let mut long_len = 0usize;
        for e in self.dpb.iter().filter(|e| e.rps_flag == ffi::VA_PICTURE_HEVC_RPS_LT_CURR) {
            if let Some(&(_, i)) = ref_index.iter().find(|(s, _)| *s == e.surface)
                && long_len < long.len()
            {
                long[long_len] = i;
                long_len += 1;
            }
        }
        let long = &long[..long_len];

        let mut temp0: [u8; 48] = [0; 48];
        let mut temp0_len = 0usize;
        for i in before
            .iter()
            .map(|(_, i)| *i)
            .chain(after.iter().map(|(_, i)| *i))
            .chain(long.iter().copied())
        {
            if temp0_len < temp0.len() {
                temp0[temp0_len] = i;
                temp0_len += 1;
            }
        }
        let temp0 = &temp0[..temp0_len];
        let mut temp1: [u8; 48] = [0; 48];
        let mut temp1_len = 0usize;
        for i in after
            .iter()
            .map(|(_, i)| *i)
            .chain(before.iter().map(|(_, i)| *i))
            .chain(long.iter().copied())
        {
            if temp1_len < temp1.len() {
                temp1[temp1_len] = i;
                temp1_len += 1;
            }
        }
        let temp1 = &temp1[..temp1_len];

        let n0 = (sh.num_ref_idx_l0_active_minus1 as usize + 1).min(15);
        fill_ref_list(&mut list[0], temp0, n0, &sh.ref_list_mods.l0, sh.ref_list_mods.l0_present);
        if sh.slice_type == SliceType::B {
            let n1 = (sh.num_ref_idx_l1_active_minus1 as usize + 1).min(15);
            fill_ref_list(
                &mut list[1],
                temp1,
                n1,
                &sh.ref_list_mods.l1,
                sh.ref_list_mods.l1_present,
            );
        }

        // The syntax element verbatim, not a ReferenceFrames index. Resolving it broke TMVP with
        // multiple refs.
        let collocated_ref_idx = if sh.slice_temporal_mvp_enabled_flag {
            (sh.collocated_ref_idx as u8).min(14)
        } else {
            0xFF
        };
        (list, collocated_ref_idx)
    }

    fn emit_pending(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        if self.zerocopy.is_some() {
            return self.emit_pending_zerocopy(ctx);
        }
        self.emit_pending_readback(ctx)
    }

    fn emit_pending_readback(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        if self.pending.is_none() {
            self.pending = self.output_queue.pop_front();
        }
        let Some(out) = self.pending.take() else {
            return Ok(true);
        };

        let width = out.width;
        let height = out.height;
        let (crop_x, crop_y) = (out.crop_x, out.crop_y);
        let ten_bit = out.format == SurfaceFormat::P010;
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        let bps = if ten_bit { 2 } else { 1 };
        let y_len = (width * height) as usize * bps;
        let c_len = (cw * ch) as usize * bps;
        let need = y_len + 2 * c_len;
        let (pixfmt, layout) = if ten_bit { (PIXFMT_P010, "P010") } else { (PIXFMT_I420, "I420") };

        let Some(mut buf) = ctx.try_alloc() else {
            if !self.alloc_stalled {
                self.alloc_stalled = true;
                let s = ctx.pool_stats();
                log!(
                    &*ctx,
                    Level::Debug,
                    "alloc_stall",
                    outstanding = s.outstanding,
                    max_slots = s.max_slots,
                    acquires = s.acquires,
                    recycles = s.recycles,
                );
            }
            self.pending = Some(out);
            return Ok(false);
        };
        self.alloc_stalled = false;
        if buf.memory.capacity() < need {
            self.recycle_output_surface(out.surface);
            let capacity = buf.memory.capacity();
            return Err(failed(
                ctx,
                DESC.name,
                K::FrameSlot { layout, width, height, need, capacity },
            ));
        }

        let Some(display) = &self.display else {
            return Ok(true);
        };

        let image = match MappedImage::acquire_for(
            display,
            out.surface,
            out.coded_w,
            out.coded_h,
            out.format,
        ) {
            Ok(im) => im,
            Err(e) => {
                self.picture_failed(ctx, &e, K::Readback(e));
                self.recycle_output_surface(out.surface);
                return Ok(true);
            }
        };
        if image.num_planes() < 2 {
            self.warn(ctx, K::NotNv12 { planes: image.num_planes() });
            drop(image);
            self.recycle_output_surface(out.surface);
            return Ok(true);
        }

        let shape = Some((width, height, 0, out.format.fourcc, out.color));
        if self.shape != shape {
            log!(&*ctx, Level::Debug, "announce", width = width, height = height);
            let mut fields = [(F_WIDTH, ValueDesc::Int(width as i64)); 3 + colour::FIELDS.len()];
            fields[1] = (F_HEIGHT, ValueDesc::Int(height as i64));
            fields[2] = (F_PIXFMT, ValueDesc::Id(pixfmt));
            let n = 3 + color_fields(ctx, Self::IN, out.color, &mut fields[3..]);
            ctx.announce_format(Self::OUT, FAMILY, &fields[..n]);
            self.shape = shape;
        }

        let cx = crop_x as usize * bps;
        let cx_c = (crop_x / 2) as usize * bps;
        let y_row = width as usize * bps;
        let c_row = cw as usize * 2 * bps;
        let dst = buf.memory.as_mut_full();
        {
            let (y_dst, chroma_dst) = dst.split_at_mut(y_len);
            for row in 0..height {
                let Some(src) = image.row(0, crop_y + row, cx + y_row) else {
                    drop(image);
                    self.recycle_output_surface(out.surface);
                    self.warn(ctx, K::PlaneOverrun { plane: "Y", row });
                    return Ok(true);
                };
                let d = &mut y_dst[row as usize * y_row..(row as usize + 1) * y_row];
                d.copy_from_slice(&src[cx..]);
            }
            let (cb_dst, cr_dst) = chroma_dst.split_at_mut(if ten_bit { 2 * c_len } else { c_len });
            for row in 0..ch {
                let Some(src) = image.row(1, crop_y / 2 + row, cx_c + c_row) else {
                    drop(image);
                    self.recycle_output_surface(out.surface);
                    self.warn(ctx, K::PlaneOverrun { plane: "CbCr", row });
                    return Ok(true);
                };
                let src = &src[cx_c..];
                if ten_bit {
                    let at = row as usize * c_row;
                    cb_dst[at..at + c_row].copy_from_slice(src);
                } else {
                    let cb = &mut cb_dst[(row * cw) as usize..((row + 1) * cw) as usize];
                    let cr = &mut cr_dst[(row * cw) as usize..((row + 1) * cw) as usize];
                    for x in 0..cw as usize {
                        cb[x] = src[2 * x];
                        cr[x] = src[2 * x + 1];
                    }
                }
            }
        }
        buf.memory.set_len(need);
        buf.pts = out.pts;
        buf.duration = out.duration;
        if out.preview {
            buf.flags |= BufferFlags::PREVIEW;
        }
        if out.intra {
            buf.flags |= BufferFlags::INTRA;
        }
        drop(image);
        log!(&*ctx, Level::Trace, "frame", pts = buf.pts);
        self.side.attach(buf.pts.0, &mut buf.memory);
        ctx.out(Self::OUT).push(buf);
        self.emitted += 1;
        self.transient.ok();

        self.recycle_output_surface(out.surface);
        Ok(true)
    }

    fn emit_pending_zerocopy(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        if self.surfaces_exhausted() && self.pending.is_none() && self.output_queue.is_empty() {
            return Ok(true); // nothing to emit
        }

        if self.pending.is_none() {
            self.pending = self.output_queue.pop_front();
        }
        let Some(out) = self.pending.take() else {
            return Ok(true);
        };

        let dpy = match &self.display {
            Some(d) => d.raw(),
            None => return Ok(true),
        };

        // a pending command, a seek's flush, cuts the wait short, the decoder runs again after it
        if !crate::va::wait_surface(dpy, out.export, || ctx.command_pending()) {
            self.pending = Some(out);
            ctx.wake_within(crate::enc::POLL_WAKE);
            return Ok(false);
        }
        let mut exported = match (self.export)(dpy, out.export) {
            Ok(e) => e,
            Err(e) => {
                // iHD fails `vaSyncSurface` now and then under rapid seeks, for one picture.
                self.warn(ctx, K::Export(e));
                self.recycle_output_surface(out.surface);
                return Ok(true);
            }
        };

        let same = self.link.filter(|(fourcc, _)| *fourcc == exported.drm_format);
        if let Some((_, negotiated)) = same.filter(|(_, m)| *m != exported.drm_modifier) {
            let kind = K::ModifierChanged { negotiated, exported: exported.drm_modifier };
            self.recycle_output_surface(out.surface);
            return Err(failed(ctx, DESC.name, kind));
        }
        let Some(layout) = gpuframe::layout_of(exported.drm_format) else {
            self.warn(ctx, K::NotNv12 { planes: exported.planes.len() as u32 });
            self.recycle_output_surface(out.surface);
            return Ok(true);
        };

        let shape =
            Some((out.width, out.height, exported.drm_modifier, exported.drm_format, out.color));
        if self.shape != shape {
            log!(&*ctx, Level::Debug, "announce_gpu", width = out.width, height = out.height);
            let mut fields =
                [(F_WIDTH, ValueDesc::Int(out.width as i64)); 4 + colour::FIELDS.len()];
            fields[1] = (F_HEIGHT, ValueDesc::Int(out.height as i64));
            fields[2] = (F_LAYOUT, ValueDesc::Id(layout));
            fields[3] = (F_MODIFIER, ValueDesc::Int(exported.drm_modifier as i64));
            let n = 4 + color_fields(ctx, Self::IN, out.color, &mut fields[4..]);
            ctx.announce_format(Self::OUT, FAMILY_VIDEO_GPU, &fields[..n]);
            self.shape = shape;
            self.link = Some((exported.drm_format, exported.drm_modifier));
            log!(&*ctx, Level::Info, "zero_copy", width = out.width, height = out.height);
        }

        let header = match ctx.try_alloc_sized(GPU_FRAME_HEADER_LEN) {
            Ok(buf) => buf,
            Err(e) => {
                self.recycle_output_surface(out.surface);
                return Err(failed(ctx, DESC.name, K::HeaderSlot { need: e.n, capacity: e.max }));
            }
        };
        let Some(mut buf) = header else {
            if !self.alloc_stalled {
                self.alloc_stalled = true;
                let s = ctx.pool_stats();
                log!(
                    &*ctx,
                    Level::Debug,
                    "alloc_stall",
                    outstanding = s.outstanding,
                    max_slots = s.max_slots,
                );
            }
            self.pending = Some(out);
            return Ok(false);
        };
        self.alloc_stalled = false;

        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);

        let header = GpuFrameHeader {
            token,
            coded_w: out.coded_w,
            coded_h: out.coded_h,
            disp_w: out.width,
            disp_h: out.height,
            crop_x: out.crop_x,
            crop_y: out.crop_y,
        };
        let dst = buf.memory.as_mut_full();
        if dst.len() < GPU_FRAME_HEADER_LEN {
            self.recycle_output_surface(out.surface);
            let (need, capacity) = (GPU_FRAME_HEADER_LEN, dst.len());
            return Err(failed(ctx, DESC.name, K::HeaderSlot { need, capacity }));
        }
        header.write_to(dst);
        buf.memory.set_len(GPU_FRAME_HEADER_LEN);
        buf.pts = out.pts;
        buf.duration = out.duration;
        if out.preview {
            buf.flags |= BufferFlags::PREVIEW;
        }
        if out.intra {
            buf.flags |= BufferFlags::INTRA;
        }

        // Pushed before the buffer so a fast sink cannot pop the token before it is marked in
        // flight.
        let frame = GpuFrame {
            token,
            fds: std::mem::take(&mut exported.fds),
            drm_format: exported.drm_format,
            coded_w: exported.coded_w,
            coded_h: exported.coded_h,
            disp_w: out.width,
            disp_h: out.height,
            crop_x: out.crop_x,
            crop_y: out.crop_y,
            planes: std::mem::take(&mut exported.planes),
            epoch: ctx.applied_seek_gen(),
            lease: None,
        };
        drop(exported);
        self.side.attach(buf.pts.0, &mut buf.memory);
        let sent = self.tx.as_mut().is_some_and(|tx| tx.push(frame, buf.memory.clone()));
        if !sent {
            self.warned.note_name(ctx, "channel_full", 0);
            self.recycle_output_surface(out.surface);
            return Ok(true);
        }
        self.in_flight.insert(out.surface, token);

        log!(&*ctx, Level::Trace, "gpu_frame", pts = buf.pts, token = token);
        ctx.out(Self::OUT).push(buf);
        self.emitted += 1;
        self.transient.ok();

        self.output_key.remove(&out.surface);
        Ok(true)
    }

    fn drain_presentation_releases(&mut self) {
        let Some(tx) = self.tx.as_mut() else { return };
        let mut released = std::mem::take(&mut self.released);
        tx.drain_released(&mut released);
        for token in released.drain(..) {
            let surface = self.in_flight.iter().find(|&(_, &t)| t == token).map(|(&s, _)| s);
            if let Some(surface) = surface {
                self.in_flight.remove(&surface);
                self.release_if_unreferenced(surface);
            }
        }
        self.released = released;
    }

    fn drop_before_point(&mut self, ctx: &Ctx) {
        let before = |o: &PendingOut| ctx.before_presentation(o.pts, Timestamp::ZERO);
        if self.pending.as_ref().is_some_and(before)
            && let Some(out) = self.pending.take()
        {
            ctx.count_drop();
            self.recycle_output_surface(out.surface);
        }
        let mut at = 0;
        while at < self.output_queue.len() {
            if !before(&self.output_queue[at]) {
                at += 1;
            } else if let Some(out) = self.output_queue.remove(at) {
                ctx.count_drop();
                self.recycle_output_surface(out.surface);
            }
        }
    }

    fn recycle_output_surface(&mut self, surface: ffi::VASurfaceID) {
        self.output_key.remove(&surface);
        self.release_if_unreferenced(surface);
    }

    fn reset_dpb(&mut self) {
        while let Some(e) = self.dpb.pop() {
            self.release_if_unreferenced(e.surface);
        }
    }

    fn drain_all(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        self.drain_presentation_releases();
        while self.pending.is_some() || !self.output_queue.is_empty() {
            if !self.emit_pending(ctx)? {
                ctx.hold_end();
                return Ok(false);
            }
            self.drain_presentation_releases();
        }
        Ok(true)
    }

    fn emit_over_budget(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        if self.pending.is_some() && !self.emit_pending(ctx)? {
            return Ok(false);
        }
        Ok(true)
    }
}

impl Element for VaapiH265Dec {
    fn desc(&self) -> &'static ElementDesc {
        if self.zerocopy.is_some() { &DESC_ZEROCOPY } else { &DESC }
    }

    // Cold, once at preroll.
    #[allow(clippy::disallowed_methods)]
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.zerocopy.is_none() {
            return Ok(());
        }
        // kept for the session, a display opened again on the element's thread is 2 to 5 ms
        // ahead of its first picture
        let display = if self.ensure_display(ctx) { self.display.as_ref() } else { None };
        let formats = match display.map(gpuframe::exported_formats) {
            Some(Ok(formats)) => formats,
            Some(Err(e)) => {
                self.warn(ctx, K::ExportProbe(e));
                Vec::new()
            }
            None => Vec::new(),
        };
        gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_src(Self::OUT, offers))
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let (Some(ch), None) = (&self.zerocopy, &self.tx) {
            self.tx = Some(ch.take_tx().ok_or_else(|| failed(ctx, DESC.name, K::ChannelTaken))?);
        }
        self.link = gpuframe::link_layout(ctx, ctx.negotiated_src(Self::OUT));
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let emitted = self.emitted;
        if self.failed {
            while inputs.pop().is_some() {
                ctx.count_drop();
            }
            return Ok(Flow::Ok);
        }
        if !self.ensure_display(ctx) {
            while inputs.pop().is_some() {}
            return Ok(Flow::Ok);
        }
        self.drain_presentation_releases();
        if self.ended && !self.drain_all(ctx)? {
            return Ok(Flow::Ok);
        }
        loop {
            if !self.emit_over_budget(ctx)? {
                return Ok(Flow::Ok);
            }
            while self.surfaces_exhausted() {
                if !self.in_flight.is_empty() {
                    ctx.await_recycle();
                    self.drain_presentation_releases();
                    if self.surfaces_exhausted() {
                        return Ok(Flow::Ok);
                    }
                    continue;
                }
                if self.pending.is_none() && self.output_queue.is_empty() {
                    break; // held by the DPB alone, decode_au's drop is the fuse
                }
                if !self.emit_pending(ctx)? {
                    return Ok(Flow::Ok);
                }
            }
            // a picture put out goes downstream now, the next decode would hold it back
            if ctx.command_pending() || self.emitted != emitted {
                return Ok(Flow::Ok);
            }
            let Some(inbuf) = inputs.pop() else { break };
            self.only = inbuf.flags.contains(BufferFlags::DECODE_ONLY);
            if self.side.gather(
                pf_hdr::h26x::Codec::H265,
                &inbuf.memory,
                inbuf.pts.0,
                &mut self.hdr,
            ) {
                let (m, l) = (self.hdr.mastering, self.hdr.light);
                log!(
                    &*ctx,
                    Level::Debug,
                    "hdr_sei",
                    max_nits = m.max_luminance / 10_000,
                    max_cll = l.max_cll
                );
            }
            self.decode_au(ctx, inbuf.memory.data(), inbuf.pts, inbuf.duration);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart => {
                let queued = self.output_queue.len() + usize::from(self.pending.is_some());
                log!(&*ctx, Level::Debug, "flush", queued = queued, dpb = self.dpb.len());
                self.warned.summary(ctx, "flush");
                self.side.clear();
                let outs: Vec<_> = self
                    .pending
                    .take()
                    .into_iter()
                    .chain(self.output_queue.drain(..))
                    .map(|o| o.surface)
                    .collect();
                self.output_key.clear();
                self.drain_presentation_releases();
                for id in outs {
                    self.release_if_unreferenced(id);
                }
                self.reset_dpb();
                self.poc.reset();
                self.skip_rasl = false;
                self.awaiting_irap = true; // resume at a clean random-access point
                self.alloc_stalled = false;
                self.ended = false;
            }
            Event::SkippedTo { to_time } => {
                log!(&*ctx, Level::Debug, "skip", to = *to_time);
                self.drop_before_point(ctx);
            }
            Event::Eos => {
                self.ended = true;
                self.drain_all(ctx)?;
                log!(&*ctx, Level::Info, "eos", emitted = self.emitted, failed = self.failed);
                self.warned.summary(ctx, "eos");
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        if let (Some(ch), Some(tx)) = (&self.zerocopy, self.tx.take()) {
            ch.put_tx(tx);
        }
        self.in_flight.clear();
        self.pending = None;
        self.output_queue.clear();
        self.dpb.clear();
        self.retired.clear();
        self.va = None;
        self.display = None;
    }
}

fn fill_ref_list(dst: &mut [u8; 15], temp: &[u8], active: usize, mods: &[u32], mods_present: bool) {
    if temp.is_empty() {
        return;
    }
    if mods_present && !mods.is_empty() {
        for (i, slot) in dst.iter_mut().take(active.min(15)).enumerate() {
            let idx = *mods.get(i).unwrap_or(&0) as usize;
            *slot = *temp.get(idx).unwrap_or(&temp[0]);
        }
    } else {
        for (i, slot) in dst.iter_mut().take(active.min(15)).enumerate() {
            *slot = temp[i % temp.len()];
        }
    }
}

#[derive(Default)]
struct AuScratch {
    nals: Vec<Nal>,
    slices: Vec<Nal>,
    headers: Vec<SliceHeader>,
    buffers: Vec<VaBuffer>,
}

impl AuScratch {
    fn clear(&mut self) {
        self.nals.clear();
        self.slices.clear();
        self.headers.clear();
        self.buffers.clear();
    }
}

fn peek_pps_id(nal: &[u8], is_irap: bool) -> Option<u32> {
    if nal.len() < 3 {
        return None;
    }
    let (body, body_len) = de_emulate_prefix(&nal[2..]);
    let mut r = crate::h264parse::BitReader::new(&body[..body_len]);
    let _first = r.flag();
    if is_irap {
        let _no_output = r.flag();
    }
    Some(r.ue())
}

fn de_emulate_prefix(body: &[u8]) -> ([u8; 16], usize) {
    let mut out = [0u8; 16];
    let mut out_len = 0usize;
    let mut zeros = 0;
    let mut i = 0;
    while i < body.len() && out_len < 16 {
        let b = body[i];
        if zeros >= 2 && b == 0x03 && i + 1 < body.len() && body[i + 1] <= 0x03 {
            zeros = 0;
            i += 1;
            continue;
        }
        out[out_len] = b;
        out_len += 1;
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    (out, out_len)
}

fn unsupported(sps: &Sps) -> K {
    K::UnsupportedSps {
        chroma_format_idc: sps.chroma_format_idc,
        bit_depth_luma: sps.bit_depth_luma_minus8 + 8,
        bit_depth_chroma: sps.bit_depth_chroma_minus8 + 8,
        separate_colour_plane: sps.separate_colour_plane_flag,
        extension: sps.has_extension,
    }
}

/// The picture parameters of a range extensions profile, `base` and the SPS's and PPS's range
/// extensions.
fn pic_ext(
    base: ffi::VAPictureParameterBufferHEVC,
    sps: &Sps,
    pps: &Pps,
) -> ffi::VAPictureParameterBufferHEVCExtension {
    let r = &pps.range;
    let fields = sps.range_ext
        | u32::from(r.cross_component_prediction_enabled_flag) << 9
        | u32::from(r.chroma_qp_offset_list_enabled_flag) << 10;
    let small = |v: u32| v.min(u32::from(u8::MAX)) as u8;
    let rext = ffi::VAPictureParameterBufferHEVCRext {
        range_extension_pic_fields: fields,
        diff_cu_chroma_qp_offset_depth: small(r.diff_cu_chroma_qp_offset_depth),
        chroma_qp_offset_list_len_minus1: small(r.chroma_qp_offset_list_len_minus1),
        log2_sao_offset_scale_luma: small(r.log2_sao_offset_scale_luma),
        log2_sao_offset_scale_chroma: small(r.log2_sao_offset_scale_chroma),
        log2_max_transform_skip_block_size_minus2: small(
            r.log2_max_transform_skip_block_size_minus2,
        ),
        cb_qp_offset_list: r.cb_qp_offset_list,
        cr_qp_offset_list: r.cr_qp_offset_list,
    };
    ffi::VAPictureParameterBufferHEVCExtension {
        base,
        rext,
        scc: ffi::VAPictureParameterBufferHEVCScc::zeroed(),
    }
}

/// ChromaOffset (7-56) of a delta weight and delta offset, `half` wpOffsetHalfRangeC.
fn chroma_offset(delta_w: i32, delta_off: i32, denom: i32, half: i32) -> i32 {
    let w = (1i32 << denom) + delta_w;
    (half + delta_off - ((half * w) >> denom)).clamp(-half, half - 1)
}

/// The slice parameters of a range extensions profile, `base` and the offsets at their full
/// width, wpOffsetHalfRangeC from high_precision_offsets_enabled_flag (7.4.7.3).
fn slice_ext(
    base: ffi::VASliceParameterBufferHEVC,
    sps: &Sps,
    sh: &SliceHeader,
) -> ffi::VASliceParameterBufferHEVCExtension {
    let mut rext = ffi::VASliceParameterBufferHEVCRext {
        slice_ext_flags: u32::from(sh.cu_chroma_qp_offset_enabled_flag),
        ..Default::default()
    };
    if let Some(t) = &sh.pred_weights {
        let high = sps.range_ext & h265parse::RANGE_EXT_HIGH_PRECISION_OFFSETS != 0;
        let half = if high { 1i32 << (sps.bit_depth_chroma_minus8 + 7) } else { 128 };
        let denom =
            (t.luma_log2_weight_denom as i32 + t.delta_chroma_log2_weight_denom).clamp(0, 7);
        let wide = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        for (i, e) in t.l0.iter().take(15).enumerate() {
            rext.luma_offset_l0[i] = wide(e.luma_offset);
            for c in 0..2 {
                let o = chroma_offset(e.delta_chroma_weight[c], e.chroma_offset[c], denom, half);
                rext.ChromaOffsetL0[i][c] = wide(o);
            }
        }
        for (i, e) in t.l1.iter().take(15).enumerate() {
            rext.luma_offset_l1[i] = wide(e.luma_offset);
            for c in 0..2 {
                let o = chroma_offset(e.delta_chroma_weight[c], e.chroma_offset[c], denom, half);
                rext.ChromaOffsetL1[i][c] = wide(o);
            }
        }
    }
    ffi::VASliceParameterBufferHEVCExtension { base, rext }
}

fn zeroed_pic_param_hevc() -> ffi::VAPictureParameterBufferHEVC {
    // SAFETY: plain POD, all-zero is valid.
    unsafe { std::mem::zeroed() }
}

fn zeroed_slice_param_hevc() -> ffi::VASliceParameterBufferHEVC {
    // SAFETY: plain POD, all-zero is valid.
    unsafe { std::mem::zeroed() }
}

fn iq_matrix(l: &ScalingLists) -> ffi::VAIQMatrixBufferHEVC {
    ffi::VAIQMatrixBufferHEVC {
        ScalingList4x4: l.list4x4.map(|m| to_raster(&m)),
        ScalingList8x8: l.list8x8.map(|m| to_raster(&m)),
        ScalingList16x16: l.list16x16.map(|m| to_raster(&m)),
        ScalingList32x32: l.list32x32.map(|m| to_raster(&m)),
        ScalingListDC16x16: l.dc16x16,
        ScalingListDC32x32: l.dc32x32,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

fn pack_pic_fields(sps: &Sps, pps: &Pps) -> u32 {
    let mut v = 0u32;
    v |= (sps.chroma_format_idc & 0x3) << ffi::HEVC_PIC_CHROMA_FORMAT_IDC_SHIFT;
    if sps.separate_colour_plane_flag {
        v |= ffi::HEVC_PIC_SEPARATE_COLOUR_PLANE_FLAG;
    }
    if sps.pcm_enabled_flag {
        v |= ffi::HEVC_PIC_PCM_ENABLED_FLAG;
    }
    if sps.scaling_list_enabled_flag {
        v |= ffi::HEVC_PIC_SCALING_LIST_ENABLED_FLAG;
    }
    if pps.transform_skip_enabled_flag {
        v |= ffi::HEVC_PIC_TRANSFORM_SKIP_ENABLED_FLAG;
    }
    if sps.amp_enabled_flag {
        v |= ffi::HEVC_PIC_AMP_ENABLED_FLAG;
    }
    if sps.strong_intra_smoothing_enabled_flag {
        v |= ffi::HEVC_PIC_STRONG_INTRA_SMOOTHING_ENABLED_FLAG;
    }
    if pps.sign_data_hiding_enabled_flag {
        v |= ffi::HEVC_PIC_SIGN_DATA_HIDING_ENABLED_FLAG;
    }
    if pps.constrained_intra_pred_flag {
        v |= ffi::HEVC_PIC_CONSTRAINED_INTRA_PRED_FLAG;
    }
    if pps.cu_qp_delta_enabled_flag {
        v |= ffi::HEVC_PIC_CU_QP_DELTA_ENABLED_FLAG;
    }
    if pps.weighted_pred_flag {
        v |= ffi::HEVC_PIC_WEIGHTED_PRED_FLAG;
    }
    if pps.weighted_bipred_flag {
        v |= ffi::HEVC_PIC_WEIGHTED_BIPRED_FLAG;
    }
    if pps.transquant_bypass_enabled_flag {
        v |= ffi::HEVC_PIC_TRANSQUANT_BYPASS_ENABLED_FLAG;
    }
    if pps.tiles_enabled_flag {
        v |= ffi::HEVC_PIC_TILES_ENABLED_FLAG;
    }
    if pps.entropy_coding_sync_enabled_flag {
        v |= ffi::HEVC_PIC_ENTROPY_CODING_SYNC_ENABLED_FLAG;
    }
    if pps.pps_loop_filter_across_slices_enabled_flag {
        v |= ffi::HEVC_PIC_PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED_FLAG;
    }
    if pps.loop_filter_across_tiles_enabled_flag {
        v |= ffi::HEVC_PIC_LOOP_FILTER_ACROSS_TILES_ENABLED_FLAG;
    }
    if sps.pcm_loop_filter_disabled_flag {
        v |= ffi::HEVC_PIC_PCM_LOOP_FILTER_DISABLED_FLAG;
    }
    v
}

fn pack_slice_parsing_fields(sps: &Sps, pps: &Pps, nal_type: u8, headers: &[SliceHeader]) -> u32 {
    let mut v = 0u32;
    if pps.lists_modification_present_flag {
        v |= ffi::HEVC_SLP_LISTS_MODIFICATION_PRESENT_FLAG;
    }
    if sps.long_term_ref_pics_present_flag {
        v |= ffi::HEVC_SLP_LONG_TERM_REF_PICS_PRESENT_FLAG;
    }
    if sps.sps_temporal_mvp_enabled_flag {
        v |= ffi::HEVC_SLP_SPS_TEMPORAL_MVP_ENABLED_FLAG;
    }
    if pps.cabac_init_present_flag {
        v |= ffi::HEVC_SLP_CABAC_INIT_PRESENT_FLAG;
    }
    if pps.output_flag_present_flag {
        v |= ffi::HEVC_SLP_OUTPUT_FLAG_PRESENT_FLAG;
    }
    if pps.dependent_slice_segments_enabled_flag {
        v |= ffi::HEVC_SLP_DEPENDENT_SLICE_SEGMENTS_ENABLED_FLAG;
    }
    if pps.pps_slice_chroma_qp_offsets_present_flag {
        v |= ffi::HEVC_SLP_PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT_FLAG;
    }
    if sps.sample_adaptive_offset_enabled_flag {
        v |= ffi::HEVC_SLP_SAMPLE_ADAPTIVE_OFFSET_ENABLED_FLAG;
    }
    if pps.deblocking_filter_override_enabled_flag {
        v |= ffi::HEVC_SLP_DEBLOCKING_FILTER_OVERRIDE_ENABLED_FLAG;
    }
    if pps.pps_deblocking_filter_disabled_flag {
        v |= ffi::HEVC_SLP_PPS_DISABLE_DEBLOCKING_FILTER_FLAG;
    }
    if pps.slice_segment_header_extension_present_flag {
        v |= ffi::HEVC_SLP_SLICE_SEGMENT_HEADER_EXTENSION_PRESENT_FLAG;
    }
    if h265parse::is_irap(nal_type) {
        v |= ffi::HEVC_SLP_RAP_PIC_FLAG;
    }
    if h265parse::is_idr(nal_type) {
        v |= ffi::HEVC_SLP_IDR_PIC_FLAG;
    }
    if headers.iter().all(|h| h.slice_type == SliceType::I) {
        v |= ffi::HEVC_SLP_INTRA_PIC_FLAG;
    }
    v
}

fn pack_long_slice_flags(sh: &SliceHeader, last_slice: bool) -> u32 {
    let mut v = 0u32;
    if last_slice {
        v |= ffi::HEVC_LSF_LAST_SLICE_OF_PIC;
    }
    if sh.dependent_slice_segment_flag {
        v |= ffi::HEVC_LSF_DEPENDENT_SLICE_SEGMENT_FLAG;
    }
    v |= (sh.slice_type.va_value() & 0x3) << ffi::HEVC_LSF_SLICE_TYPE_SHIFT;
    if sh.slice_sao_luma_flag {
        v |= ffi::HEVC_LSF_SLICE_SAO_LUMA_FLAG;
    }
    if sh.slice_sao_chroma_flag {
        v |= ffi::HEVC_LSF_SLICE_SAO_CHROMA_FLAG;
    }
    if sh.mvd_l1_zero_flag {
        v |= ffi::HEVC_LSF_MVD_L1_ZERO_FLAG;
    }
    if sh.cabac_init_flag {
        v |= ffi::HEVC_LSF_CABAC_INIT_FLAG;
    }
    if sh.slice_temporal_mvp_enabled_flag {
        v |= ffi::HEVC_LSF_SLICE_TEMPORAL_MVP_ENABLED_FLAG;
    }
    if sh.slice_deblocking_filter_disabled_flag {
        v |= ffi::HEVC_LSF_SLICE_DEBLOCKING_FILTER_DISABLED_FLAG;
    }
    if sh.collocated_from_l0_flag {
        v |= ffi::HEVC_LSF_COLLOCATED_FROM_L0_FLAG;
    }
    if sh.slice_loop_filter_across_slices_enabled_flag {
        v |= ffi::HEVC_LSF_SLICE_LOOP_FILTER_ACROSS_SLICES_ENABLED_FLAG;
    }
    v
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn fill_ref_list_cycles_to_active_count() {
        let mut dst = [0xFFu8; 15];
        let temp = [3u8, 5];
        fill_ref_list(&mut dst, &temp, 4, &[], false);
        assert_eq!(&dst[..4], &[3, 5, 3, 5]);
        assert_eq!(dst[4], 0xFF, "entries beyond active stay invalid");
    }

    #[test]
    fn fill_ref_list_applies_modifications() {
        let mut dst = [0xFFu8; 15];
        let temp = [10u8, 20, 30];
        fill_ref_list(&mut dst, &temp, 3, &[2, 0, 1], true);
        assert_eq!(&dst[..3], &[30, 10, 20]);
    }

    /// At wpOffsetHalfRangeC 128 the derivation is the 8 bit one, a wider range keeps what 128
    /// would clip.
    #[test]
    fn chroma_offsets_derive_at_any_half_range() {
        let old = |dw: i32, doff: i32, denom: i32| {
            let w = (1i32 << denom) + dw;
            (doff - ((128 * w) >> denom) + 128).clamp(-128, 127)
        };
        for (dw, doff, denom) in
            [(0, 0, 6), (-5, 40, 6), (20, -300, 3), (127, 511, 7), (-128, -512, 0)]
        {
            assert_eq!(
                chroma_offset(dw, doff, denom, 128),
                old(dw, doff, denom),
                "{dw} {doff} {denom}"
            );
        }
        assert_eq!(chroma_offset(0, 1000, 6, 2048), 1000, "12 bit high precision");
        assert_eq!(chroma_offset(0, 1000, 6, 128), 127);
    }

    #[test]
    fn long_slice_flags_carry_slice_type_and_last() {
        let mut sh = stub_sh(SliceType::B);
        sh.collocated_from_l0_flag = false;
        let v = pack_long_slice_flags(&sh, true);
        assert_ne!(v & ffi::HEVC_LSF_LAST_SLICE_OF_PIC, 0);
        assert_eq!((v >> ffi::HEVC_LSF_SLICE_TYPE_SHIFT) & 0x3, 0);
        assert_eq!(v & ffi::HEVC_LSF_COLLOCATED_FROM_L0_FLAG, 0);

        let sh_p = stub_sh(SliceType::P);
        let vp = pack_long_slice_flags(&sh_p, false);
        assert_eq!((vp >> ffi::HEVC_LSF_SLICE_TYPE_SHIFT) & 0x3, 1, "P slice_type == 1");
    }

    fn stub_sh(t: SliceType) -> SliceHeader {
        SliceHeader {
            first_slice_segment_in_pic_flag: true,
            no_output_of_prior_pics_flag: false,
            pps_id: 0,
            dependent_slice_segment_flag: false,
            slice_segment_address: 0,
            slice_type: t,
            pic_output_flag: true,
            pic_order_cnt_lsb: 0,
            st_rps: h265parse::ShortTermRps::default(),
            st_rps_bits: 0,
            long_term_refs: Default::default(),
            slice_temporal_mvp_enabled_flag: false,
            slice_sao_luma_flag: false,
            slice_sao_chroma_flag: false,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            ref_list_mods: h265parse::RefListMods::default(),
            mvd_l1_zero_flag: false,
            cabac_init_flag: false,
            collocated_from_l0_flag: true,
            collocated_ref_idx: 0,
            pred_weights: None,
            five_minus_max_num_merge_cand: 0,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            cu_chroma_qp_offset_enabled_flag: false,
            deblocking_filter_override_flag: false,
            slice_deblocking_filter_disabled_flag: false,
            slice_beta_offset_div2: 0,
            slice_tc_offset_div2: 0,
            slice_loop_filter_across_slices_enabled_flag: true,
            num_entry_point_offsets: 0,
            slice_data_byte_offset: 0,
            num_emu_prev_bytes: 0,
        }
    }
}
