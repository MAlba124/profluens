//! `vaapih264dec`, the VA-API H.264 decode element. Annex-B access units in, NV12 out.
//! The host parses slice headers and keeps the DPB and reference lists (H.264 8.2.4, 8.2.5).

use std::collections::VecDeque;
use std::path::PathBuf;

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

use std::sync::Arc;

use crate::decproc::{self, Twins};
use crate::error::{Transient, VaapiErrorKind as K, Verdict, Warned, failed, log_fatal};
use crate::ffi;
use crate::gpuframe::{
    self, F_LAYOUT, F_MODIFIER, FAMILY_VIDEO_GPU, FrameToken, FrameTx, GPU_FRAME_HEADER_LEN,
    GpuFrame, GpuFrameChannel, GpuFrameHeader, LAYOUT_NV12_DMABUF,
};
use crate::gpuframe::Handoff;
use crate::stats::Stats;
use crate::h264parse::{
    self, MmcoOp, Nal, Pps, PredWeightTable, RefListMod, SliceHeader, SliceType, Sps,
};
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
/// Slices per AU past any A.3 level bound. Each costs a header, a VA buffer and a render.
const SLICES_MAX: u32 = 4096;
const F_WIDTH: &str = "width";
const F_HEIGHT: &str = "height";
const F_PIXFMT: &str = "pixfmt";
const PIXFMT_NV12: &str = "nv12";

static PIXFMT_VALUES: [ValueDesc; 1] = [ValueDesc::Id(PIXFMT_NV12)];

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
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("h264/annexb")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

// Cold, once per default build.
#[allow(clippy::disallowed_methods)]
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "vaapih264dec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VaapiH264Dec::new())),
    io: false,
    couplings: &[],
};

static SRCS_ZEROCOPY: [PadDesc; 1] = [PadDesc { name: "src", offers: OfferDesc::PROBED }];
static DESC_ZEROCOPY: ElementDesc = ElementDesc {
    name: "vaapih264dec",
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
    frame_num: i32,
    frame_num_wrap: i32,
    poc: i32,
    top_poc: i32,
    bottom_poc: i32,
    long_term: bool,
    long_term_frame_idx: i32,
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
    color: Color,
    preview: bool,
    /// Every slice of it is I or SI, the source's scene change most often.
    intra: bool,
}

struct VaState {
    // Field order is drop order, the context goes before its surfaces and its config.
    context: Context,
    /// The scaled twins of the surfaces when the decoder shrinks.
    twins: Option<Twins>,
    surfaces: Surfaces,
    _config: Config,
    free: VecDeque<ffi::VASurfaceID>,
    width: u32,
    height: u32,
    crop_x: u32,
    crop_y: u32,
    coded_w: u32,
    coded_h: u32,
    count: u32,
}

/// The VA-API H.264 decode element.
pub struct VaapiH264Dec {
    device: Option<PathBuf>,
    // Field order is drop order, the sessions go before the display they live on.
    va: Option<VaState>,
    retired: Vec<VaState>,
    display: Option<Display>,

    sps: [Option<Arc<Sps>>; 32],
    pps: [Option<Arc<Pps>>; 256],
    scratch: AuScratch,

    dpb: Vec<DpbEntry>,
    poc: h264parse::PocState,
    output_queue: VecDeque<PendingOut>,
    /// (gop, POC) per surface. POC resets at every IDR (8.2.1), the gop keeps the key monotonic.
    output_key: std::collections::HashMap<ffi::VASurfaceID, (u32, i32)>,
    gop: u32,
    /// Decoding waits for an IDR or an all-intra picture after start and after a flush.
    awaiting_idr: bool,
    pending: Option<PendingOut>,

    shape: Option<(u32, u32, u64, Color)>,
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
    stats: Arc<Stats>,
    tx: Option<FrameTx>,
    released: Vec<FrameToken>,
    next_token: FrameToken,
    in_flight: std::collections::HashMap<ffi::VASurfaceID, FrameToken>,
    export: ExportFn,
    modifier: Option<u64>,
    only: bool,
    /// Pictures leave scaled into this box by the decode call, bars around them when fixed.
    shrink: Option<(Target, bool)>,
}

// SAFETY: an Active element runs on one dedicated thread after preroll, its display moves there
// with it and is never used from two threads at once.
unsafe impl Send for VaapiH264Dec {}

impl Default for VaapiH264Dec {
    fn default() -> Self {
        Self::new()
    }
}

impl VaapiH264Dec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::with_zerocopy(None)
    }

    /// Zero-copy mode. Decoded surfaces leave as DMA-BUF [`GpuFrame`]s on `channel`.
    pub fn new_zerocopy(channel: Arc<GpuFrameChannel>) -> Self {
        Self::with_zerocopy(Some(channel))
    }

    /// The decoder's counters, pictures and waits for the GPU.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
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

    // Cold, once per element.
    #[allow(clippy::disallowed_methods)]
    fn with_zerocopy(zerocopy: Option<Arc<GpuFrameChannel>>) -> Self {
        VaapiH264Dec {
            only: false,
            device: None,
            display: None,
            va: None,
            retired: Vec::new(),
            sps: std::array::from_fn(|_| None),
            pps: std::array::from_fn(|_| None),
            dpb: Vec::new(),
            poc: h264parse::PocState::new(),
            output_queue: VecDeque::new(),
            output_key: std::collections::HashMap::new(),
            gop: 0,
            awaiting_idr: true,
            pending: None,
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
            stats: Arc::default(),
            tx: None,
            released: Vec::new(),
            next_token: 1,
            in_flight: std::collections::HashMap::new(),
            export: ExportedSurface::export,
            modifier: None,
            scratch: AuScratch::default(),
            shrink: None,
        }
    }

    /// The PPS with its scaling matrix sized by the SPS it names, 4:2:0 when that is unknown.
    fn parse_pps(&self, nal: &[u8]) -> Option<h264parse::Pps> {
        let pps = h264parse::parse_pps(nal)?;
        let sps = self.sps.get(pps.seq_parameter_set_id as usize).and_then(|s| s.as_deref());
        match sps.map_or(1, |s| s.chroma_format_idc) {
            3 => h264parse::parse_pps_for(nal, 3),
            _ => Some(pps),
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

    // Cold, once per dimension.
    #[allow(clippy::disallowed_methods)]
    fn ensure_va(&mut self, ctx: &mut Ctx, sps: &Sps, idr: bool) -> bool {
        let width = sps.width().max(16);
        let height = sps.height().max(16);
        let (crop_x, crop_y) = sps.crop_origin();
        // Surfaces cover whole macroblocks (7.4.2.1.1). iHD fails vaCreateContext at
        // the cropped size with VA_STATUS_ERROR_ALLOCATION_FAILED.
        let coded_w = (sps.width_in_mbs() * 16).max(16);
        let coded_h = (sps.height_in_mbs() * 16).max(16);
        // Undersized, the exhaustion path emits below the reorder budget and frames display out of
        // order.
        let (refs, reorder) = (sps.max_num_ref_frames, sps.reorder_frames());
        // Exported surfaces stay out of the pool until dropped. The slack covers the shown frame
        // and release latency.
        const PRESENTATION_SLACK: u32 = 6;
        let extra = if self.zerocopy.is_some() { PRESENTATION_SLACK } else { 0 };
        let count = crate::va::decode_surface_count(refs, reorder, extra);
        // A new SPS takes effect only at an IDR (7.4.1.2.1), so an undersized session is rebuilt
        // there.
        let shrink = match self.shrink.filter(|_| self.zerocopy.is_some()) {
            Some((t, fixed)) => match decproc::plan(width, height, 8, t, fixed) {
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
                && va.twins.as_ref().map(|t| t.plan) == shrink
                && (va.count >= count || !idr)
            {
                return true;
            }
            log!(
                &*ctx,
                Level::Debug,
                "reinit",
                width = width,
                height = height,
                was_w = va.width,
                was_h = va.height
            );
            self.retire_session();
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
            coded_h = coded_h
        );

        let profile = ffi::VAProfileH264High; // High covers Main and CB for VLD on Intel.
        let config = match shrink {
            Some(_) => Config::new_decode_processing(display, profile, SurfaceFormat::NV12),
            None => Config::new_decode(display, profile),
        };
        let config = match config {
            Ok(c) => c,
            Err(e) => {
                self.fail(ctx, K::SessionSetup(e));
                return false;
            }
        };
        let surfaces = match Surfaces::new_nv12(display, coded_w, coded_h, count) {
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
        // the decoded picture is whole macroblocks, frame cropping the crop
        let crop = (crop_x, crop_y, width, height);
        let twins = match shrink
            .map(|p| Twins::new(display, p, count, crop, (coded_w, coded_h)))
            .transpose()
        {
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
            context,
            twins,
            surfaces,
            _config: config,
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

    /// Retired sessions live until no picture or sink frame names their surfaces. Destroyed early,
    /// a dead id lands on the new free list and vaBeginPicture fails with INVALID_SURFACE.
    fn retire_session(&mut self) {
        self.dpb.clear();
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

    fn acquire_surface(&mut self) -> Option<ffi::VASurfaceID> {
        self.va.as_mut().and_then(|va| va.free.pop_front())
    }

    fn surfaces_exhausted(&self) -> bool {
        self.va.as_ref().is_some_and(|va| va.free.is_empty())
    }

    fn release_surface(&mut self, id: ffi::VASurfaceID) {
        match &mut self.va {
            // To the back, a compositor may read a little past its release.
            Some(va) if va.surfaces.ids().contains(&id) => {
                if !va.free.contains(&id) {
                    va.free.push_back(id);
                }
            }
            _ => self.prune_retired(),
        }
    }

    fn surface_referenced(&self, id: ffi::VASurfaceID) -> bool {
        self.dpb.iter().any(|e| e.surface == id)
            || self.output_queue.iter().any(|o| o.surface == id)
            || self.pending.as_ref().is_some_and(|o| o.surface == id)
            || self.in_flight.contains_key(&id)
    }

    /// A surface still queued for output must not become a decode target.
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
        h264parse::split_nals_into(au, &mut sc.nals);
        self.seq_end = sc
            .nals
            .iter()
            .any(|n| matches!(n.unit_type, h264parse::NAL_END_SEQ | h264parse::NAL_END_STREAM));

        for nal in &sc.nals {
            match nal.unit_type {
                h264parse::NAL_SPS => match h264parse::parse_sps(nal.raw(au)) {
                    Some(sps) if (sps.seq_parameter_set_id as usize) < 32 => {
                        let id = sps.seq_parameter_set_id as usize;
                        self.sps[id] = Some(Arc::new(sps));
                    }
                    _ => self.warned.note_name(ctx, "sps_unparsed", 0),
                },
                h264parse::NAL_PPS => match self.parse_pps(nal.raw(au)) {
                    Some(pps) if (pps.pic_parameter_set_id as usize) < 256 => {
                        let id = pps.pic_parameter_set_id as usize;
                        self.pps[id] = Some(Arc::new(pps));
                    }
                    _ => self.warned.note_name(ctx, "pps_unparsed", 0),
                },
                _ => {}
            }
        }

        sc.slices.clear();
        let slice = |n: &&Nal| {
            matches!(n.unit_type, h264parse::NAL_SLICE_NON_IDR | h264parse::NAL_SLICE_IDR)
        };
        sc.slices.extend(sc.nals.iter().filter(slice).copied());
        if sc.slices.is_empty() {
            return;
        }

        let before = self.only || ctx.before_presentation(pts, Timestamp::ZERO);
        if before && h264parse::non_reference(&sc.nals) {
            log!(&*ctx, Level::Trace, "skip_nonref", pts = pts);
            ctx.count_drop();
            return;
        }

        let first = sc.slices[0];
        let pps_id = peek_pps_id(first.raw(au));
        let Some(pps) = pps_id.and_then(|id| self.pps.get(id as usize).and_then(|p| p.clone()))
        else {
            self.warn(ctx, K::UnknownPps);
            return;
        };
        let Some(sps) = self.sps.get(pps.seq_parameter_set_id as usize).and_then(|s| s.clone())
        else {
            self.warn(ctx, K::UnknownSps);
            return;
        };

        if sps.separate_colour_plane_flag
            || sps.bit_depth_luma_minus8 != 0
            || sps.bit_depth_chroma_minus8 != 0
            || sps.chroma_format_idc != 1
        {
            self.fail(
                ctx,
                K::UnsupportedSps {
                    chroma_format_idc: sps.chroma_format_idc,
                    bit_depth_luma: sps.bit_depth_luma_minus8 + 8,
                    bit_depth_chroma: sps.bit_depth_chroma_minus8 + 8,
                    separate_colour_plane: sps.separate_colour_plane_flag,
                    extension: false,
                },
            );
            return;
        }

        // Each slice codes at least one macroblock, and A.3 bounds the count per picture.
        let most = (sps.width_in_mbs() * sps.height_in_mbs()).min(SLICES_MAX);
        if sc.slices.len() > most as usize {
            self.warn(ctx, K::TooManySlices { slices: sc.slices.len(), most });
            return;
        }
        sc.headers.clear();
        for n in &sc.slices {
            let Some(sh) =
                h264parse::parse_slice_header(n.raw(au), n.unit_type, n.ref_idc, &sps, &pps)
            else {
                self.warn(ctx, K::SliceHeader);
                return;
            };
            if sh.field_pic_flag {
                self.fail(ctx, K::Interlaced);
                return;
            }
            sc.headers.push(sh);
        }
        let sh0 = &sc.headers[0];
        let is_idr = first.unit_type == h264parse::NAL_SLICE_IDR;
        let ref_idc = first.ref_idc;

        // A stream joined mid-GOP decodes from its first IDR or all-intra picture, as the
        // references before it are not there.
        if self.awaiting_idr {
            let intra =
                sc.headers.iter().all(|sh| matches!(sh.slice_type, SliceType::I | SliceType::Si));
            if !is_idr && !intra {
                log!(&*ctx, Level::Trace, "await_idr", pts = pts);
                return;
            }
            self.awaiting_idr = false;
        }

        if !self.ensure_va(ctx, &sps, is_idr) {
            return;
        }

        if is_idr {
            self.reset_dpb();
            self.poc.reset();
            self.gop = self.gop.wrapping_add(1);
        }

        let (top_poc, bottom_poc) = self.poc.compute(&sps, sh0, is_idr, ref_idc);
        let poc = top_poc.min(bottom_poc);
        // 8.2.1 and 7.4.3, an MMCO 5 picture is stored as frame_num 0 at order count 0.
        let mmco5 = !is_idr
            && ref_idc != 0
            && sh0.adaptive_ref_pic_marking_mode_flag
            && sh0.mmco_ops.iter().any(|op| op.op == 5);
        let (top_poc, bottom_poc, poc) =
            if mmco5 { (top_poc - poc, bottom_poc - poc, 0) } else { (top_poc, bottom_poc, poc) };

        let max_frame_num = sps.max_frame_num() as i32;
        let cur_frame_num = sh0.frame_num as i32;
        for e in &mut self.dpb {
            e.frame_num_wrap = if !e.long_term && e.frame_num > cur_frame_num {
                e.frame_num - max_frame_num
            } else {
                e.frame_num
            };
        }

        let Some(target) = self.acquire_surface() else {
            let (dpb, queued, out) =
                (self.dpb.len(), self.output_queue.len(), self.in_flight.len());
            log!(
                &*ctx,
                Level::Debug,
                "no_surface",
                dpb = dpb,
                queued = queued,
                pending = self.pending.is_some(),
                out = out
            );
            self.warn(ctx, K::NoFreeSurface);
            return;
        };

        let Some((export, width, height, crop_x, crop_y, coded_w, coded_h)) =
            self.va.as_ref().map(|v| match &v.twins {
                Some(t) => {
                    let twin = t.of(&v.surfaces, target).unwrap_or(target);
                    (twin, t.plan.width, t.plan.height, 0, 0, t.coded.0, t.coded.1)
                }
                None => (target, v.width, v.height, v.crop_x, v.crop_y, v.coded_w, v.coded_h),
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
            top_poc,
            bottom_poc,
            ref_idc,
            &mut sc.buffers,
        ) {
            self.picture_failed(ctx, &e, K::Submit(e));
            self.release_surface(target);
            return;
        }

        if ref_idc != 0 {
            self.mark_and_insert(
                &sps,
                sh0,
                target,
                if mmco5 { 0 } else { cur_frame_num },
                poc,
                top_poc,
                bottom_poc,
                is_idr,
            );
        }

        let preview = before && !self.only && ctx.take_preview();
        if before && !preview {
            log!(&*ctx, Level::Trace, "decode_only", pts = pts);
            ctx.count_drop();
            self.release_if_unreferenced(target);
            return;
        }

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
            color: Color { hdr: self.hdr, dovi: self.side.dovi, ..sps.colour },
            preview,
            intra: sc.headers.iter().all(|h| matches!(h.slice_type, SliceType::I | SliceType::Si)),
        });
        if mmco5 {
            // 8.2.1, the picture restarts the order count. Earlier pictures sort first.
            self.gop = self.gop.wrapping_add(1);
        }
        self.reorder_output(target, poc);

        self.maybe_bump(&sps);
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
        top_poc: i32,
        bottom_poc: i32,
        ref_idc: u8,
        keep: &mut Vec<VaBuffer>,
    ) -> Result<(), crate::va::VaError> {
        let (Some(display), Some(va)) = (&self.display, &self.va) else {
            return Err(crate::va::VaError { status: -1, ctx: "no session" });
        };
        let context = &va.context;

        let mut pic = zeroed_pic_param();
        pic.CurrPic = ffi::VAPictureH264 {
            picture_id: target,
            frame_idx: headers[0].frame_num,
            flags: if ref_idc != 0 { ffi::VA_PICTURE_H264_SHORT_TERM_REFERENCE } else { 0 },
            TopFieldOrderCnt: top_poc,
            BottomFieldOrderCnt: bottom_poc,
            va_reserved: [0; ffi::VA_PADDING_LOW],
        };
        // Order is not required, the driver matches by picture_id and frame_idx.
        pic.ReferenceFrames = [ffi::VAPictureH264::invalid(); 16];
        for (i, e) in self.dpb.iter().take(16).enumerate() {
            pic.ReferenceFrames[i] = ffi::VAPictureH264 {
                picture_id: e.surface,
                frame_idx: if e.long_term {
                    e.long_term_frame_idx as u32
                } else {
                    e.frame_num as u32
                },
                flags: if e.long_term {
                    ffi::VA_PICTURE_H264_LONG_TERM_REFERENCE
                } else {
                    ffi::VA_PICTURE_H264_SHORT_TERM_REFERENCE
                },
                TopFieldOrderCnt: e.top_poc,
                BottomFieldOrderCnt: e.bottom_poc,
                va_reserved: [0; ffi::VA_PADDING_LOW],
            };
        }
        pic.picture_width_in_mbs_minus1 = (sps.width_in_mbs() - 1) as u16;
        pic.picture_height_in_mbs_minus1 = (sps.height_in_mbs() - 1) as u16;
        pic.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8 as u8;
        pic.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8 as u8;
        pic.num_ref_frames = sps.max_num_ref_frames as u8;
        pic.seq_fields = pack_seq_fields(sps);
        pic.pic_init_qp_minus26 = pps.pic_init_qp_minus26 as i8;
        pic.chroma_qp_index_offset = pps.chroma_qp_index_offset as i8;
        pic.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset as i8;
        pic.pic_fields = pack_pic_fields(sps, pps, headers[0].field_pic_flag, ref_idc != 0);
        pic.frame_num = headers[0].frame_num as u16;

        let pic_buf =
            VaBuffer::new_struct(display, context, ffi::VAPictureParameterBufferType, &pic)?;

        let lists = h264parse::resolve_scaling(sps, pps);
        let iq = ffi::VAIQMatrixBufferH264 {
            ScalingList4x4: std::array::from_fn(|i| h264parse::raster_4x4(&lists.list4x4[i])),
            ScalingList8x8: std::array::from_fn(|i| h264parse::raster_8x8(&lists.list8x8[i])),
            va_reserved: [0; ffi::VA_PADDING_LOW],
        };
        let iq_buf = VaBuffer::new_struct(display, context, ffi::VAIQMatrixBufferType, &iq)?;

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
            context.render(&[pic_buf.id(), iq_buf.id()])?;
            if let Some(p) = &proc_buf {
                context.render(&[p.id()])?;
            }
            for (sh, nal) in headers.iter().zip(slice_nals.iter()) {
                let (list0, list1, n0, n1) = self.build_ref_lists(sps, sh, top_poc.min(bottom_poc));
                let sp = self.build_slice_param(sh, nal.raw(au), &list0, &list1, n0, n1);
                let sp_buf =
                    VaBuffer::new_struct(display, context, ffi::VASliceParameterBufferType, &sp)?;
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

    fn build_ref_lists(
        &self,
        _sps: &Sps,
        sh: &SliceHeader,
        cur_poc: i32,
    ) -> ([ffi::VAPictureH264; 32], [ffi::VAPictureH264; 32], usize, usize) {
        let (mut list0, mut list1) = default_ref_lists(&self.dpb, sh.slice_type, cur_poc);

        apply_ref_list_mods(&mut list0, &sh.ref_list_mods_l0, &self.dpb, sh.frame_num as i32, _sps);
        if sh.slice_type == SliceType::B {
            apply_ref_list_mods(
                &mut list1,
                &sh.ref_list_mods_l1,
                &self.dpb,
                sh.frame_num as i32,
                _sps,
            );
        }

        let n0 = (sh.num_ref_idx_l0_active_minus1 as usize + 1).min(32);
        let n1 = (sh.num_ref_idx_l1_active_minus1 as usize + 1).min(32);

        let va0 = to_va_list(&self.dpb, &list0);
        let va1 = to_va_list(&self.dpb, &list1);
        (va0, va1, n0, n1)
    }

    fn build_slice_param(
        &self,
        sh: &SliceHeader,
        raw_nal: &[u8],
        list0: &[ffi::VAPictureH264; 32],
        list1: &[ffi::VAPictureH264; 32],
        n0: usize,
        n1: usize,
    ) -> ffi::VASliceParameterBufferH264 {
        // Counts de-emulated bits from the NAL start including its header byte (va.h).
        let bit_offset = sh.header_bits_in_rbsp as u16 + 8;

        // The driver uses this table whenever weighted_pred_flag is set. Absent weights
        // default to 1 << denom, offset 0 (8.4.2.3.2).
        let mut w = WeightFill::default();
        if let Some(t) = &sh.pred_weights {
            w.fill(t, n0, n1);
        }

        ffi::VASliceParameterBufferH264 {
            slice_data_size: raw_nal.len() as u32,
            slice_data_offset: 0,
            slice_data_flag: ffi::VA_SLICE_DATA_FLAG_ALL,
            slice_data_bit_offset: bit_offset,
            first_mb_in_slice: sh.first_mb_in_slice as u16,
            slice_type: sh.slice_type.va_value(),
            direct_spatial_mv_pred_flag: sh.direct_spatial_mv_pred_flag as u8,
            num_ref_idx_l0_active_minus1: (n0.max(1) - 1) as u8,
            num_ref_idx_l1_active_minus1: (n1.max(1) - 1) as u8,
            cabac_init_idc: sh.cabac_init_idc as u8,
            slice_qp_delta: sh.slice_qp_delta as i8,
            disable_deblocking_filter_idc: sh.disable_deblocking_filter_idc as u8,
            slice_alpha_c0_offset_div2: sh.slice_alpha_c0_offset_div2 as i8,
            slice_beta_offset_div2: sh.slice_beta_offset_div2 as i8,
            RefPicList0: *list0,
            RefPicList1: *list1,
            luma_log2_weight_denom: w.luma_denom,
            chroma_log2_weight_denom: w.chroma_denom,
            luma_weight_l0_flag: w.l0_present,
            luma_weight_l0: w.luma_weight_l0,
            luma_offset_l0: w.luma_offset_l0,
            chroma_weight_l0_flag: w.l0_present,
            chroma_weight_l0: w.chroma_weight_l0,
            chroma_offset_l0: w.chroma_offset_l0,
            luma_weight_l1_flag: w.l1_present,
            luma_weight_l1: w.luma_weight_l1,
            luma_offset_l1: w.luma_offset_l1,
            chroma_weight_l1_flag: w.l1_present,
            chroma_weight_l1: w.chroma_weight_l1,
            chroma_offset_l1: w.chroma_offset_l1,
            va_reserved: [0; ffi::VA_PADDING_LOW],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mark_and_insert(
        &mut self,
        sps: &Sps,
        sh: &SliceHeader,
        surface: ffi::VASurfaceID,
        frame_num: i32,
        poc: i32,
        top_poc: i32,
        bottom_poc: i32,
        is_idr: bool,
    ) {
        let mut long_term = false;
        let mut long_term_frame_idx = -1;

        if is_idr {
            if sh.long_term_reference_flag {
                long_term = true;
                long_term_frame_idx = 0;
            }
        } else if sh.adaptive_ref_pic_marking_mode_flag {
            self.apply_mmco(sps, &sh.mmco_ops, frame_num, poc);
            for op in &sh.mmco_ops {
                if op.op == 6 {
                    long_term = true;
                    long_term_frame_idx = op.arg1 as i32;
                }
            }
            if long_term {
                // 8.2.5.4.6, the frame that held LongTermFrameIdx is unmarked first.
                self.remove_dpb_where(|e| {
                    e.long_term && e.long_term_frame_idx == long_term_frame_idx
                });
            }
        } else {
            let num_short = self.dpb.iter().filter(|e| !e.long_term).count();
            let num_long = self.dpb.iter().filter(|e| e.long_term).count();
            let max_refs = (sps.max_num_ref_frames as usize).max(1);
            if num_short + num_long >= max_refs {
                self.evict_smallest_frame_num_wrap(frame_num, sps);
            }
        }

        self.dpb.push(DpbEntry {
            surface,
            frame_num,
            frame_num_wrap: frame_num,
            poc,
            top_poc,
            bottom_poc,
            long_term,
            long_term_frame_idx,
        });
        // At most Max(max_num_ref_frames, 1) frames are marked (7.4.3.3). Past it the oldest
        // short-term goes.
        let max_refs = (sps.max_num_ref_frames as usize).max(1);
        while self.dpb.len() > max_refs
            && self.dpb.iter().any(|e| !e.long_term && e.surface != surface)
        {
            self.evict_smallest_frame_num_wrap(frame_num, sps);
        }
    }

    fn evict_smallest_frame_num_wrap(&mut self, cur_frame_num: i32, sps: &Sps) {
        let max_frame_num = sps.max_frame_num() as i32;
        let mut idx = None;
        let mut smallest = i32::MAX;
        for (i, e) in self.dpb.iter().enumerate() {
            if e.long_term {
                continue;
            }
            let wrap =
                if e.frame_num > cur_frame_num { e.frame_num - max_frame_num } else { e.frame_num };
            if wrap < smallest {
                smallest = wrap;
                idx = Some(i);
            }
        }
        if let Some(i) = idx {
            let e = self.dpb.remove(i);
            self.release_if_unreferenced(e.surface);
        }
    }

    fn apply_mmco(&mut self, _sps: &Sps, ops: &[MmcoOp], frame_num: i32, _poc: i32) {
        for op in ops {
            match op.op {
                1 => {
                    // picNumX = CurrPicNum - (diff + 1) (8.2.5.4.1), negative past a frame_num
                    // wrap.
                    let pic_num_x = frame_num - (op.arg1 as i32 + 1);
                    self.remove_dpb_where(|e| !e.long_term && e.frame_num_wrap == pic_num_x);
                }
                2 => {
                    let ltpn = op.arg1 as i32;
                    self.remove_dpb_where(|e| e.long_term && e.long_term_frame_idx == ltpn);
                }
                3 => {
                    let pic_num_x = frame_num - (op.arg1 as i32 + 1);
                    let lt = op.arg2 as i32;
                    // 8.2.5.4.3, the frame that held LongTermFrameIdx is unmarked first.
                    self.remove_dpb_where(|e| e.long_term && e.long_term_frame_idx == lt);
                    for e in &mut self.dpb {
                        if !e.long_term && e.frame_num_wrap == pic_num_x {
                            e.long_term = true;
                            e.long_term_frame_idx = lt;
                        }
                    }
                }
                4 => {
                    let max_lt = op.arg1 as i32 - 1;
                    self.remove_dpb_where(|e| e.long_term && e.long_term_frame_idx > max_lt);
                }
                5 => {
                    self.reset_dpb();
                    self.poc.reset();
                }
                _ => {}
            }
        }
    }

    fn reset_dpb(&mut self) {
        while let Some(e) = self.dpb.pop() {
            // An IDR resets the DPB while earlier frames may still await readback.
            self.release_if_unreferenced(e.surface);
        }
    }

    fn remove_dpb_where(&mut self, pred: impl Fn(&DpbEntry) -> bool) {
        while let Some(at) = self.dpb.iter().position(&pred) {
            let e = self.dpb.remove(at);
            self.release_if_unreferenced(e.surface);
        }
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
        let need = (width * height + width * height.div_ceil(2)) as usize;

        // Take the slot first. A surface mapped before a dry pool would be remapped on every spin.
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
                K::FrameSlot { layout: "NV12", width, height, need, capacity },
            ));
        }

        let Some(display) = &self.display else {
            return Ok(true);
        };

        let image = match MappedImage::acquire(
            display,
            out.surface,
            width + out.crop_x,
            height + out.crop_y,
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

        let shape = Some((width, height, 0, out.color));
        if self.shape != shape {
            log!(&*ctx, Level::Debug, "announce", width = width, height = height);
            let mut fields = [(F_WIDTH, ValueDesc::Int(width as i64)); 3 + colour::FIELDS.len()];
            fields[1] = (F_HEIGHT, ValueDesc::Int(height as i64));
            fields[2] = (F_PIXFMT, ValueDesc::Id(PIXFMT_NV12));
            let n = 3 + color_fields(ctx, Self::IN, out.color, &mut fields[3..]);
            ctx.announce_format(Self::OUT, FAMILY, &fields[..n]);
            self.shape = shape;
        }

        let dst = buf.memory.as_mut_full();
        let y_len = (width * height) as usize;
        let c_rows = height.div_ceil(2);
        let (crop_x, crop_y) = (out.crop_x as usize, out.crop_y);
        {
            let (y_dst, c_dst) = dst.split_at_mut(y_len);
            for row in 0..height {
                let Some(src) = image.row(0, row + crop_y, crop_x + width as usize) else {
                    drop(image);
                    self.recycle_output_surface(out.surface);
                    self.warn(ctx, K::PlaneOverrun { plane: "Y", row });
                    return Ok(true);
                };
                let d = &mut y_dst[(row * width) as usize..((row + 1) * width) as usize];
                d.copy_from_slice(&src[crop_x..]);
            }
            for row in 0..c_rows {
                let Some(src) = image.row(1, row + crop_y / 2, crop_x + width as usize) else {
                    drop(image);
                    self.recycle_output_surface(out.surface);
                    self.warn(ctx, K::PlaneOverrun { plane: "CbCr", row });
                    return Ok(true);
                };
                let d = &mut c_dst[(row * width) as usize..((row + 1) * width) as usize];
                d.copy_from_slice(&src[crop_x..]);
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

        // a queued handoff exports the picture as it is, the dma-buf's fence orders the consumer
        let queued = self.zerocopy.as_ref().is_some_and(|c| c.handoff() == Handoff::Queued);
        // a pending command, a seek's flush, cuts the wait short, the decoder runs again after it
        if !queued {
            let t = crate::stats::mark();
            let done = crate::va::wait_surface(dpy, out.export, || ctx.command_pending());
            self.stats.waited(t);
            if !done {
                self.pending = Some(out);
                ctx.wake_within(crate::enc::POLL_WAKE);
                return Ok(false);
            }
        }
        let export = if queued { ExportedSurface::export_queued } else { self.export };
        let mut exported = match export(dpy, out.export) {
            Ok(e) => e,
            Err(e) => {
                // The driver fails this sync now and then under rapid seeks.
                self.warn(ctx, K::Export(e));
                self.recycle_output_surface(out.surface);
                return Ok(true);
            }
        };

        if let Some(negotiated) = self.modifier.filter(|m| *m != exported.drm_modifier) {
            let kind = K::ModifierChanged { negotiated, exported: exported.drm_modifier };
            self.recycle_output_surface(out.surface);
            return Err(failed(ctx, DESC.name, kind));
        }

        let shape = Some((out.width, out.height, exported.drm_modifier, out.color));
        if self.shape != shape {
            log!(&*ctx, Level::Debug, "announce_gpu", width = out.width, height = out.height);
            let mut fields =
                [(F_WIDTH, ValueDesc::Int(out.width as i64)); 4 + colour::FIELDS.len()];
            fields[1] = (F_HEIGHT, ValueDesc::Int(out.height as i64));
            fields[2] = (F_LAYOUT, ValueDesc::Id(LAYOUT_NV12_DMABUF));
            fields[3] = (F_MODIFIER, ValueDesc::Int(exported.drm_modifier as i64));
            let n = 4 + color_fields(ctx, Self::IN, out.color, &mut fields[4..]);
            ctx.announce_format(Self::OUT, FAMILY_VIDEO_GPU, &fields[..n]);
            self.shape = shape;
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
                    max_slots = s.max_slots
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
}

impl Element for VaapiH264Dec {
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
        self.modifier = gpuframe::link_modifier(ctx, ctx.negotiated_src(Self::OUT));
        self.awaiting_idr = true;
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
            // An exhausted surface pool is backpressure, never a drop. Dropping would lose every AU
            // until the next IDR.
            while self.surfaces_exhausted() {
                // Surfaces out at the sink come back with their lease, which wakes this group.
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
                    return Ok(Flow::Ok); // pool dry, re-cranked on slot return
                }
            }
            // a picture put out goes downstream now, the next decode would hold it back
            if ctx.command_pending() || self.emitted != emitted {
                return Ok(Flow::Ok);
            }
            let Some(inbuf) = inputs.pop() else { break };
            self.only = inbuf.flags.contains(BufferFlags::DECODE_ONLY);
            if self.side.gather(
                pf_hdr::h26x::Codec::H264,
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
                self.awaiting_idr = true;
                self.warned.summary(ctx, "flush");
                self.side.clear();
                // Outputs drain first so reset_dpb sees their surfaces unreferenced and frees them.
                let outs: Vec<_> = self
                    .pending
                    .take()
                    .into_iter()
                    .chain(self.output_queue.drain(..))
                    .map(|o| o.surface)
                    .collect();
                self.output_key.clear();
                // A surface still on screen stays out across the flush.
                self.drain_presentation_releases();
                for id in outs {
                    self.release_if_unreferenced(id);
                }
                self.reset_dpb();
                self.poc.reset();
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

impl VaapiH264Dec {
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

#[derive(Default)]
struct WeightFill {
    luma_denom: u8,
    chroma_denom: u8,
    l0_present: u8,
    l1_present: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
}

impl WeightFill {
    fn fill(&mut self, t: &PredWeightTable, n0: usize, n1: usize) {
        self.luma_denom = t.luma_log2_weight_denom as u8;
        self.chroma_denom = t.chroma_log2_weight_denom as u8;
        let ld = 1i16 << t.luma_log2_weight_denom.min(7);
        let cd = 1i16 << t.chroma_log2_weight_denom.min(7);

        self.l0_present = 1;
        for i in 0..n0.min(32) {
            let e = t.l0.get(i).copied().unwrap_or_default();
            let (lw, lo) = e.luma.unwrap_or((ld as i32, 0));
            self.luma_weight_l0[i] = lw as i16;
            self.luma_offset_l0[i] = lo as i16;
            let ch = e.chroma.unwrap_or([(cd as i32, 0); 2]);
            for (c, &(w, o)) in ch.iter().enumerate() {
                self.chroma_weight_l0[i][c] = w as i16;
                self.chroma_offset_l0[i][c] = o as i16;
            }
        }
        if !t.l1.is_empty() {
            self.l1_present = 1;
            for i in 0..n1.min(32) {
                let e = t.l1.get(i).copied().unwrap_or_default();
                let (lw, lo) = e.luma.unwrap_or((ld as i32, 0));
                self.luma_weight_l1[i] = lw as i16;
                self.luma_offset_l1[i] = lo as i16;
                let ch = e.chroma.unwrap_or([(cd as i32, 0); 2]);
                for (c, &(w, o)) in ch.iter().enumerate() {
                    self.chroma_weight_l1[i][c] = w as i16;
                    self.chroma_offset_l1[i][c] = o as i16;
                }
            }
        }
    }
}

fn peek_pps_id(nal: &[u8]) -> Option<u32> {
    if nal.len() < 2 {
        return None;
    }
    let mut body = [0u8; 16];
    let n = h264parse::de_emulate(&nal[1..], &mut body);
    let mut r = h264parse::BitReader::new(&body[..n]);
    let _first_mb = r.ue();
    let _slice_type = r.ue();
    Some(r.ue())
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

#[derive(Clone, Copy)]
struct RefList {
    idx: [u8; 32],
    len: u8,
}

impl RefList {
    const EMPTY: RefList = RefList { idx: [0; 32], len: 0 };

    fn push(&mut self, i: usize) {
        if (self.len as usize) < self.idx.len() {
            self.idx[self.len as usize] = i as u8;
            self.len += 1;
        }
    }

    fn extend(&mut self, other: &RefList) {
        other.as_slice().iter().for_each(|&i| self.push(i as usize));
    }

    fn as_slice(&self) -> &[u8] {
        &self.idx[..self.len as usize]
    }

    fn len(&self) -> usize {
        self.len as usize
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn sorted<K: Ord>(
        dpb: &[DpbEntry],
        keep: impl Fn(&DpbEntry) -> bool,
        key: impl Fn(&DpbEntry) -> K,
    ) -> RefList {
        let mut list = RefList::EMPTY;
        dpb.iter().enumerate().filter(|(_, e)| keep(e)).for_each(|(i, _)| list.push(i));
        let n = list.len();
        list.idx[..n].sort_by_key(|&i| key(&dpb[i as usize]));
        list
    }
}

fn zeroed_pic_param() -> ffi::VAPictureParameterBufferH264 {
    // SAFETY: plain integer POD, all-zero is a valid empty state.
    unsafe { std::mem::zeroed() }
}

fn pack_seq_fields(sps: &Sps) -> u32 {
    let mut v = 0u32;
    v |= sps.chroma_format_idc & 0x3;
    v |= (sps.separate_colour_plane_flag as u32) << 2; // residual_colour_transform_flag
    v |= (sps.gaps_in_frame_num_value_allowed_flag as u32) << 3;
    v |= (sps.frame_mbs_only_flag as u32) << 4;
    v |= (sps.mb_adaptive_frame_field_flag as u32) << 5;
    v |= (sps.direct_8x8_inference_flag as u32) << 6;
    // Bit 7 MinLumaBiPredSize8x8 is level-derived, 0 is safe.
    v |= (sps.log2_max_frame_num_minus4 & 0xf) << 8;
    v |= (sps.pic_order_cnt_type & 0x3) << 12;
    v |= (sps.log2_max_pic_order_cnt_lsb_minus4 & 0xf) << 14;
    v |= (sps.delta_pic_order_always_zero_flag as u32) << 18;
    v
}

fn pack_pic_fields(sps: &Sps, pps: &Pps, field_pic: bool, is_ref: bool) -> u32 {
    let _ = sps;
    let mut v = 0u32;
    v |= pps.entropy_coding_mode_flag as u32;
    v |= (pps.weighted_pred_flag as u32) << 1;
    v |= (pps.weighted_bipred_idc & 0x3) << 2;
    v |= (pps.transform_8x8_mode_flag as u32) << 4;
    v |= (field_pic as u32) << 5;
    v |= (pps.constrained_intra_pred_flag as u32) << 6;
    v |= (pps.bottom_field_pic_order_in_frame_present_flag as u32) << 7; // pic_order_present_flag
    v |= (pps.deblocking_filter_control_present_flag as u32) << 8;
    v |= (pps.redundant_pic_cnt_present_flag as u32) << 9;
    v |= (is_ref as u32) << 10; // reference_pic_flag
    v
}

fn to_va_list(dpb: &[DpbEntry], list: &RefList) -> [ffi::VAPictureH264; 32] {
    let mut out = [ffi::VAPictureH264::invalid(); 32];
    for (i, &at) in list.as_slice().iter().enumerate() {
        let e = &dpb[at as usize];
        out[i] = ffi::VAPictureH264 {
            picture_id: e.surface,
            frame_idx: if e.long_term { e.long_term_frame_idx as u32 } else { e.frame_num as u32 },
            flags: if e.long_term {
                ffi::VA_PICTURE_H264_LONG_TERM_REFERENCE
            } else {
                ffi::VA_PICTURE_H264_SHORT_TERM_REFERENCE
            },
            TopFieldOrderCnt: e.top_poc,
            BottomFieldOrderCnt: e.bottom_poc,
            va_reserved: [0; ffi::VA_PADDING_LOW],
        };
    }
    out
}

fn default_ref_lists(dpb: &[DpbEntry], slice_type: SliceType, cur_poc: i32) -> (RefList, RefList) {
    use std::cmp::Reverse;
    let dpb = &dpb[..dpb.len().min(32)];
    let mut list0 = RefList::EMPTY;
    let mut list1 = RefList::EMPTY;
    let long = RefList::sorted(dpb, |e| e.long_term, |e| e.long_term_frame_idx);
    match slice_type {
        SliceType::P | SliceType::Sp => {
            let short = RefList::sorted(dpb, |e| !e.long_term, |e| Reverse(e.frame_num_wrap));
            list0.extend(&short);
            list0.extend(&long);
        }
        SliceType::B => {
            let less =
                RefList::sorted(dpb, |e| !e.long_term && e.poc < cur_poc, |e| Reverse(e.poc));
            let greater = RefList::sorted(dpb, |e| !e.long_term && e.poc > cur_poc, |e| e.poc);

            list0.extend(&less);
            list0.extend(&greater);
            list0.extend(&long);

            list1.extend(&greater);
            list1.extend(&less);
            list1.extend(&long);
            let same = |(a, b): (&u8, &u8)| dpb[*a as usize].surface == dpb[*b as usize].surface;
            if list1.len() > 1
                && list0.len() == list1.len()
                && list0.as_slice().iter().zip(list1.as_slice()).all(same)
            {
                list1.idx.swap(0, 1);
            }
        }
        SliceType::I | SliceType::Si => {}
    }
    (list0, list1)
}

/// 8.2.4.3 modifications, best-effort for short-term ops 0/1 and long-term op 2.
fn apply_ref_list_mods(
    list: &mut RefList,
    mods: &[RefListMod],
    dpb: &[DpbEntry],
    cur_frame_num: i32,
    sps: &Sps,
) {
    if mods.is_empty() {
        return;
    }
    let dpb = &dpb[..dpb.len().min(32)];
    let max_frame_num = sps.max_frame_num() as i32;
    let mut pred = cur_frame_num;
    let mut refined = RefList::EMPTY;
    for m in mods {
        match m.op {
            0 | 1 => {
                let abs_diff = m.value as i32 + 1;
                let mut pic_num = if m.op == 0 { pred - abs_diff } else { pred + abs_diff };
                if pic_num < 0 {
                    pic_num += max_frame_num;
                } else if pic_num >= max_frame_num {
                    pic_num -= max_frame_num;
                }
                pred = pic_num;
                if let Some(i) = dpb.iter().position(|e| !e.long_term && e.frame_num == pic_num) {
                    refined.push(i);
                }
            }
            2 => {
                let lt = m.value as i32;
                if let Some(i) = dpb.iter().position(|e| e.long_term && e.long_term_frame_idx == lt)
                {
                    refined.push(i);
                }
            }
            _ => {}
        }
    }
    if !refined.is_empty() {
        // A faithful 8.2.4.3 shifts in place. For short-term P/B this yields the same active
        // window.
        let mut new_list = refined;
        for &i in list.as_slice() {
            let surface = dpb[i as usize].surface;
            if !new_list.as_slice().iter().any(|&x| dpb[x as usize].surface == surface) {
                new_list.push(i as usize);
            }
        }
        *list = new_list;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS_1080P: [u8; 27] = [
        0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
    ];

    fn entry(surface: u32, frame_num: i32, poc: i32) -> DpbEntry {
        DpbEntry {
            surface,
            frame_num,
            frame_num_wrap: frame_num,
            poc,
            top_poc: poc,
            bottom_poc: poc,
            long_term: false,
            long_term_frame_idx: -1,
        }
    }

    #[test]
    fn p_slice_ref_list_is_descending_frame_num_wrap() {
        let dpb = vec![entry(10, 0, 0), entry(11, 2, 4), entry(12, 1, 2)];
        let (list0, list1) = default_ref_lists(&dpb, SliceType::P, 6);
        let order: Vec<i32> =
            list0.as_slice().iter().map(|&i| dpb[i as usize].frame_num_wrap).collect();
        assert_eq!(order, vec![2, 1, 0], "P list0 must descend by FrameNumWrap");
        assert!(list1.is_empty(), "P slices have no list1");
        let sps = h264parse::parse_sps(&SPS_1080P).expect("parses");
        let mut list0 = list0;
        apply_ref_list_mods(&mut list0, &[RefListMod { op: 0, value: 2 }], &dpb, 3, &sps);
        let order: Vec<i32> =
            list0.as_slice().iter().map(|&i| dpb[i as usize].frame_num_wrap).collect();
        assert_eq!(order, vec![0, 2, 1], "frame_num 0 named by abs_diff 3 goes first");
    }

    #[test]
    fn mmco_unmarks_by_pic_num_across_a_frame_num_wrap() {
        // MaxFrameNum 16. Past a frame_num wrap picNumX is negative (8.2.4.1, 8.2.5.4.1).
        let sps = h264parse::parse_sps(&SPS_1080P).expect("parses");
        assert_eq!(sps.max_frame_num(), 16);
        let mut dec = VaapiH264Dec::new();
        dec.dpb = vec![entry(1, 14, 0), entry(2, 15, 2), entry(3, 0, 4)];
        for e in &mut dec.dpb {
            e.frame_num_wrap = if e.frame_num > 1 { e.frame_num - 16 } else { e.frame_num };
        }
        let ops = [MmcoOp { op: 1, arg1: 2, arg2: 0 }, MmcoOp { op: 3, arg1: 1, arg2: 0 }];
        dec.apply_mmco(&sps, &ops, 1, 6);
        let left: Vec<(u32, bool)> = dec.dpb.iter().map(|e| (e.surface, e.long_term)).collect();
        assert_eq!(
            left,
            vec![(2, true), (3, false)],
            "frame_num 14 unmarked, frame_num 15 long-term"
        );
    }

    #[test]
    fn a_long_term_index_given_again_unmarks_its_old_holder() {
        let sps = h264parse::parse_sps(&SPS_1080P).expect("parses");
        let mut dec = VaapiH264Dec::new();
        dec.dpb = vec![entry(1, 2, 0), entry(2, 3, 2), entry(3, 4, 4)];
        dec.dpb[0].long_term = true;
        dec.dpb[0].long_term_frame_idx = 0;
        // MMCO 3 moves LongTermFrameIdx 0 from frame_num 2 to frame_num 3 (picNumX = 5 - 2).
        dec.apply_mmco(&sps, &[MmcoOp { op: 3, arg1: 1, arg2: 0 }], 5, 10);
        let left: Vec<(u32, bool, i32)> =
            dec.dpb.iter().map(|e| (e.surface, e.long_term, e.long_term_frame_idx)).collect();
        assert_eq!(left, vec![(2, true, 0), (3, false, -1)], "the old holder is gone");
    }

    #[test]
    fn b_slice_ref_lists_split_by_poc() {
        let dpb = vec![
            entry(1, 0, 0), // POC 0  (< cur)
            entry(2, 1, 2), // POC 2  (< cur)
            entry(3, 2, 8), // POC 8  (> cur)
            entry(4, 3, 6), // POC 6  (> cur)
        ];
        let cur_poc = 4;
        let (list0, list1) = default_ref_lists(&dpb, SliceType::B, cur_poc);
        let l0: Vec<i32> = list0.as_slice().iter().map(|&i| dpb[i as usize].poc).collect();
        let l1: Vec<i32> = list1.as_slice().iter().map(|&i| dpb[i as usize].poc).collect();
        assert_eq!(l0, vec![2, 0, 6, 8]);
        assert_eq!(l1, vec![6, 8, 2, 0]);
    }

    #[test]
    fn output_order_survives_idr_poc_reset() {
        // POC restarts at an IDR (8.2.1). The (gop, poc) key keeps the old GOP tail first.
        let mut dec = VaapiH264Dec::new();
        let push = |dec: &mut VaapiH264Dec, surface: u32, poc: i32| {
            dec.output_queue.push_back(PendingOut {
                surface,
                export: surface,
                pts: Timestamp::ZERO,
                duration: Timestamp::ZERO,
                width: 16,
                height: 16,
                crop_x: 0,
                crop_y: 0,
                coded_w: 16,
                coded_h: 16,
                color: Color::default(),
                preview: false,
                intra: false,
            });
            dec.reorder_output(surface, poc);
        };
        push(&mut dec, 1, 202);
        push(&mut dec, 2, 200);
        dec.gop = dec.gop.wrapping_add(1);
        push(&mut dec, 3, 0);
        push(&mut dec, 4, 2);
        let order: Vec<u32> = dec.output_queue.iter().map(|o| o.surface).collect();
        assert_eq!(
            order,
            vec![2, 1, 3, 4],
            "old GOP (POC-sorted) must drain before the new GOP's POC-0 IDR"
        );
    }
}
