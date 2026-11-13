//! `vaapivp9dec`, the VA-API VP9 decode element. The driver reads the compressed
//! header and adapts probabilities, so the whole frame goes as slice data.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use crate::vp9::{ColorConfig, ColorSpace, FrameType, MAX_SEGMENTS, Vp9FrameHeader};

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
use profluens_video::color::{self as colour, Matrix, Range};
use profluens_video::decoder::{COLOR_FIELDS, Color, color_fields};
use profluens_video::format::COLOR_OFFER_FIELDS;

use crate::decproc::{self, Twins};
use crate::error::{Transient, VaapiErrorKind as K, Verdict, Warned, failed, log_fatal};
use crate::ffi;
use crate::gpuframe::Handoff;
use crate::gpuframe::{
    self, F_LAYOUT, F_MODIFIER, FAMILY_VIDEO_GPU, FrameToken, FrameTx, GPU_FRAME_HEADER_LEN,
    GpuFrame, GpuFrameChannel, GpuFrameHeader,
};
use crate::probe;
use crate::stats::Stats;
use crate::va::{
    Buffer as VaBuffer, Config, Context, Display, ExportedSurface, ReadbackImage, SurfaceFormat,
    Surfaces, VaError,
};
use crate::vpp::Target;
pub use pf_bitstream::vp9::StreamState;
use pf_bitstream::vp9::{self as bs, NUM_SLOTS};
use pf_bitstream::{DpbError, Event as DpbEvent, Events, Slot};

const FAMILY: &str = "video/raw";
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
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "vp9", fields: &COLOR_FIELDS }];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

// Cold: make_default boxes one element when the registry builds the default.
#[allow(clippy::disallowed_methods)]
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "vaapivp9dec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VaapiVp9Dec::new())),
    io: false,
    couplings: &[],
};

static SRCS_ZEROCOPY: [PadDesc; 1] = [PadDesc { name: "src", offers: OfferDesc::PROBED }];
static DESC_ZEROCOPY: ElementDesc = ElementDesc {
    name: "vaapivp9dec",
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

const SUPERFRAME_RESERVE: usize = 2;

pub(crate) fn pic_fields(h: &Vp9FrameHeader) -> u32 {
    let flag = |b: bool, bit: u32| if b { bit } else { 0 };
    let cc = &h.color_config;
    let seg = &h.segmentation;
    let mut v = flag(cc.subsampling_x, ffi::VP9_PIC_SUBSAMPLING_X)
        | flag(cc.subsampling_y, ffi::VP9_PIC_SUBSAMPLING_Y)
        | flag(h.frame_type == FrameType::NonKeyFrame, ffi::VP9_PIC_FRAME_TYPE)
        | flag(h.show_frame, ffi::VP9_PIC_SHOW_FRAME)
        | flag(h.error_resilient_mode, ffi::VP9_PIC_ERROR_RESILIENT_MODE)
        | flag(h.intra_only, ffi::VP9_PIC_INTRA_ONLY)
        | flag(h.allow_high_precision_mv, ffi::VP9_PIC_ALLOW_HIGH_PRECISION_MV)
        | (u32::from(h.interpolation_filter & 7) << ffi::VP9_PIC_MCOMP_FILTER_TYPE_SHIFT)
        | flag(h.frame_parallel_decoding_mode, ffi::VP9_PIC_FRAME_PARALLEL_DECODING_MODE)
        | (u32::from(h.reset_frame_context & 3) << ffi::VP9_PIC_RESET_FRAME_CONTEXT_SHIFT)
        | flag(h.refresh_frame_context, ffi::VP9_PIC_REFRESH_FRAME_CONTEXT)
        | (u32::from(h.frame_context_idx & 3) << ffi::VP9_PIC_FRAME_CONTEXT_IDX_SHIFT)
        | flag(seg.enabled, ffi::VP9_PIC_SEGMENTATION_ENABLED)
        | flag(seg.enabled && seg.temporal_update, ffi::VP9_PIC_SEGMENTATION_TEMPORAL_UPDATE)
        | flag(seg.enabled && seg.update_map, ffi::VP9_PIC_SEGMENTATION_UPDATE_MAP)
        | flag(h.quantization.lossless, ffi::VP9_PIC_LOSSLESS_FLAG);
    if let Some(idx) = h.ref_frame_idx {
        let bias = h.ref_frame_sign_bias;
        v |= (u32::from(idx[0] & 7) << ffi::VP9_PIC_LAST_REF_FRAME_SHIFT)
            | flag(bias[0], ffi::VP9_PIC_LAST_REF_FRAME_SIGN_BIAS)
            | (u32::from(idx[1] & 7) << ffi::VP9_PIC_GOLDEN_REF_FRAME_SHIFT)
            | flag(bias[1], ffi::VP9_PIC_GOLDEN_REF_FRAME_SIGN_BIAS)
            | (u32::from(idx[2] & 7) << ffi::VP9_PIC_ALT_REF_FRAME_SHIFT)
            | flag(bias[2], ffi::VP9_PIC_ALT_REF_FRAME_SIGN_BIAS);
    }
    v
}

pub(crate) fn pic_param(
    h: &Vp9FrameHeader,
    state: &StreamState,
    refs: [ffi::VASurfaceID; NUM_SLOTS],
) -> ffi::VADecPictureParameterBufferVP9 {
    ffi::VADecPictureParameterBufferVP9 {
        frame_width: h.frame_width as u16,
        frame_height: h.frame_height as u16,
        reference_frames: refs,
        pic_fields: pic_fields(h),
        filter_level: h.loop_filter.level,
        sharpness_level: h.loop_filter.sharpness,
        log2_tile_rows: h.tile_info.tile_rows_log2,
        log2_tile_columns: h.tile_info.tile_cols_log2,
        frame_header_length_in_bytes: h.uncompressed_header_size_bytes as u8,
        first_partition_size: h.header_size_in_bytes,
        mb_segment_tree_probs: state.tree_probs,
        segment_pred_probs: state.pred_probs,
        profile: h.profile,
        bit_depth: h.color_config.bit_depth,
        va_reserved: [0; ffi::VA_PADDING_MEDIUM],
    }
}

/// Section 8.8.1 filter levels and section 8.6.1 dequantizer scales.
pub(crate) fn segment_param(
    h: &Vp9FrameHeader,
    state: &StreamState,
    segment: usize,
) -> ffi::VASegmentParameterVP9 {
    let g = state.segment(h, segment);
    let mut flags = 0u16;
    if let Some(r) = g.reference {
        flags |= ffi::VP9_SEG_REFERENCE_ENABLED | (u16::from(r) << ffi::VP9_SEG_REFERENCE_SHIFT);
    }
    if g.skipped {
        flags |= ffi::VP9_SEG_REFERENCE_SKIPPED;
    }
    ffi::VASegmentParameterVP9 {
        segment_flags: flags,
        filter_level: g.filter_level,
        luma_ac_quant_scale: g.luma_ac_quant_scale as i16,
        luma_dc_quant_scale: g.luma_dc_quant_scale as i16,
        chroma_ac_quant_scale: g.chroma_ac_quant_scale as i16,
        chroma_dc_quant_scale: g.chroma_dc_quant_scale as i16,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

/// With segmentation off only segment 0 is filled, as `va_dec_vp9.h` asks.
pub(crate) fn slice_param(
    h: &Vp9FrameHeader,
    state: &StreamState,
    len: usize,
) -> ffi::VASliceParameterBufferVP9 {
    // SAFETY: plain integers and arrays of them, all zero is valid.
    let zero: ffi::VASegmentParameterVP9 = unsafe { std::mem::zeroed() };
    let mut seg_param = [zero; MAX_SEGMENTS];
    let segments = if h.segmentation.enabled { MAX_SEGMENTS } else { 1 };
    for (i, p) in seg_param.iter_mut().enumerate().take(segments) {
        *p = segment_param(h, state, i);
    }
    ffi::VASliceParameterBufferVP9 {
        slice_data_size: len as u32,
        slice_data_offset: 0,
        slice_data_flag: ffi::VA_SLICE_DATA_FLAG_ALL,
        seg_param,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

pub(crate) fn stated_color(cc: &ColorConfig) -> Color {
    let matrix = match cc.color_space {
        ColorSpace::Bt601 | ColorSpace::Smpte170 => Some(Matrix::Bt601),
        ColorSpace::Bt709 => Some(Matrix::Bt709),
        ColorSpace::Bt2020 => Some(Matrix::Bt2020),
        ColorSpace::Rgb => Some(Matrix::Identity),
        ColorSpace::Unknown | ColorSpace::Smpte240 | ColorSpace::Reserved => None,
    };
    let range = Some(if cc.color_range_full { Range::Full } else { Range::Limited });
    Color { matrix, range, ..Color::default() }
}

fn supported(cc: &ColorConfig) -> bool {
    cc.subsampling_x && cc.subsampling_y && matches!(cc.bit_depth, 8 | 10)
}

pub(crate) fn frame_bytes(width: u32, height: u32, ten_bit: bool) -> usize {
    let (w, h) = (width as usize, height as usize);
    (w * h + 2 * w.div_ceil(2) * h.div_ceil(2)) << usize::from(ten_bit)
}

pub(crate) enum CopyFail {
    Va(VaError),
    Planes(u32),
    Overrun(&'static str, u32),
}

/// The whole surface is copied, iHD's `vaGetImage` of a smaller region gave wrong
/// chroma for 640x360 in 640x368 surfaces. NV12 to I420 stays on the CPU, an I420
/// image is slower at 4K.
pub(crate) fn copy_out(
    image: &mut ReadbackImage,
    line: &mut [u8],
    out: &PendingOut,
    dst: &mut [u8],
) -> Result<(), CopyFail> {
    let ten_bit = out.format == SurfaceFormat::P010;
    let bps = if ten_bit { 2 } else { 1 };
    let (cw, ch) = (out.width.div_ceil(2), out.height.div_ceil(2));
    let y_row = out.width as usize * bps;
    let c_row = cw as usize * 2 * bps;
    let view = image.read(out.surface, out.coded_w, out.coded_h).map_err(CopyFail::Va)?;
    if view.num_planes() < 2 {
        return Err(CopyFail::Planes(view.num_planes()));
    }
    let (y_dst, chroma_dst) = dst.split_at_mut(y_row * out.height as usize);
    for (row, d) in y_dst.chunks_exact_mut(y_row).enumerate() {
        let src = view.row(0, row as u32, y_row).ok_or(CopyFail::Overrun("Y", row as u32))?;
        d.copy_from_slice(src);
    }
    if ten_bit {
        for (row, d) in chroma_dst.chunks_exact_mut(c_row).enumerate() {
            let src =
                view.row(1, row as u32, c_row).ok_or(CopyFail::Overrun("CbCr", row as u32))?;
            d.copy_from_slice(src);
        }
        return Ok(());
    }
    let line = &mut line[..c_row];
    let (cb_dst, cr_dst) = chroma_dst.split_at_mut(cw as usize * ch as usize);
    let rows = cb_dst.chunks_exact_mut(cw as usize).zip(cr_dst.chunks_exact_mut(cw as usize));
    for (row, (cb, cr)) in rows.enumerate() {
        let src = view.row(1, row as u32, c_row).ok_or(CopyFail::Overrun("CbCr", row as u32))?;
        line.copy_from_slice(src);
        for ((u, v), &[a, b]) in cb.iter_mut().zip(cr.iter_mut()).zip(line.as_chunks::<2>().0) {
            (*u, *v) = (a, b);
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct PendingOut {
    pub surface: ffi::VASurfaceID,
    /// The surface that leaves, the decode surface's twin when the decoder shrinks.
    pub export: ffi::VASurfaceID,
    pub pts: Timestamp,
    pub duration: Timestamp,
    pub width: u32,
    pub height: u32,
    pub coded_w: u32,
    pub coded_h: u32,
    pub format: SurfaceFormat,
    pub color: Color,
    pub preview: bool,
}

struct VaState {
    // Field order is drop order, the context goes before its surfaces and its config.
    context: Context,
    surfaces: Surfaces,
    _config: Config,
    format: SurfaceFormat,
    free: VecDeque<ffi::VASurfaceID>,
    coded_w: u32,
    coded_h: u32,
    readback: Option<ReadbackImage>,
    /// The scaled twins of the surfaces when the decoder shrinks, and the frame size planned.
    twins: Option<(Twins, (u32, u32))>,
}

pub struct VaapiVp9Dec {
    // Field order is drop order, the session goes before the display it lives on.
    va: Option<VaState>,
    display: Option<Display>,
    device: Option<PathBuf>,

    dpb: Box<bs::Dpb>,
    /// the VA surface of each DPB slot
    surfaces: Vec<ffi::VASurfaceID>,
    /// per slot, held by a reference slot
    held: Vec<bool>,
    ranges: Vec<(usize, usize)>,
    output_queue: VecDeque<PendingOut>,
    side: crate::colour::HdrCarry,
    /// The mapping is uncached, byte reads from it are slow.
    row: Vec<u8>,

    shape: Option<(u32, u32, u64, u32, Color)>,
    alloc_stalled: bool,
    ended: bool,
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
    in_flight: Vec<(ffi::VASurfaceID, FrameToken)>,
    /// The first picture holds the next decode until its group put out.
    first: crate::firstout::FirstOut,
    link: Option<(u32, u64)>,
    only: bool,
    shrink: Option<(Target, bool)>,
}

// SAFETY: `SchedHint::Active`, the element runs on one scheduler thread and never
// shares its `Display`. The display preroll opens moves to that thread with the element.
unsafe impl Send for VaapiVp9Dec {}

impl Default for VaapiVp9Dec {
    fn default() -> Self {
        Self::new()
    }
}

impl VaapiVp9Dec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::with_zerocopy(None)
    }

    pub fn new_zerocopy(channel: Arc<GpuFrameChannel>) -> Self {
        Self::with_zerocopy(Some(channel))
    }

    /// The decoder's counters, pictures and waits for the GPU.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    // Cold: once per element. The lists are sized for their bound here.
    #[allow(clippy::disallowed_methods)]
    fn with_zerocopy(zerocopy: Option<Arc<GpuFrameChannel>>) -> Self {
        VaapiVp9Dec {
            only: false,
            va: None,
            display: None,
            device: None,
            dpb: Box::new(bs::Dpb::new()),
            surfaces: Vec::new(),
            held: Vec::new(),
            ranges: Vec::with_capacity(8),
            output_queue: VecDeque::with_capacity(16),
            side: crate::colour::HdrCarry::new(),
            row: Vec::new(),
            shape: None,
            alloc_stalled: false,
            ended: false,
            disabled: false,
            failed: false,
            transient: Transient::default(),
            warned: Warned::default(),
            emitted: 0,
            zerocopy,
            stats: Arc::default(),
            tx: None,
            released: Vec::with_capacity(16),
            next_token: 1,
            in_flight: Vec::with_capacity(16),
            first: crate::firstout::FirstOut::new(),
            link: None,
            shrink: None,
        }
    }

    /// Zero-copy pictures leave scaled into `target` by the decode call itself, placed in its box
    /// with bars when `fixed`, see [`crate::decproc`]. The driver must do decode processing for
    /// the stream's profile.
    pub fn with_shrink(mut self, target: Target, fixed: bool) -> Self {
        self.shrink = Some((target, fixed));
        self
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

    // Cold: VA setup, once per stream and per size or depth change.
    #[allow(clippy::disallowed_methods)]
    fn ensure_va(&mut self, ctx: &mut Ctx, h: &Vp9FrameHeader) -> bool {
        let ten_bit = h.color_config.bit_depth == 10;
        let format = if ten_bit { SurfaceFormat::P010 } else { SurfaceFormat::NV12 };
        let (w, hgt) = (h.frame_width, h.frame_height);
        let (mut coded_w, mut coded_h) = (w.next_multiple_of(16), hgt.next_multiple_of(16));
        let depth = if ten_bit { 10 } else { 8 };
        let shrink = match self.shrink.filter(|_| self.zerocopy.is_some()) {
            Some((t, fixed)) => match decproc::plan(w, hgt, depth, t, fixed) {
                Some(p) => decproc::needed(p, w, hgt, (0, 0), format).map(|p| (p, (w, hgt))),
                None => {
                    self.fail(ctx, K::ProcSize { width: w, height: hgt });
                    return false;
                }
            },
            None => None,
        };
        if let Some(va) = &self.va {
            let planned = va.twins.as_ref().map(|(t, at)| (t.plan, *at));
            if va.format == format && va.coded_w >= w && va.coded_h >= hgt && planned == shrink {
                return true;
            }
            if h.frame_type != FrameType::KeyFrame {
                self.warn(ctx, K::Vp9Resize { width: w, height: hgt });
                return false;
            }
            // Grown in one dimension only, so an alternating stream does not rebuild on every key
            // frame.
            if va.format == format {
                (coded_w, coded_h) = (coded_w.max(va.coded_w), coded_h.max(va.coded_h));
            }
            log!(&*ctx, Level::Debug, "reinit", width = w, height = hgt, ten_bit = ten_bit);
        }
        if ten_bit && !probe::probe().is_some_and(|caps| caps.decodes_profile("VP9Profile2")) {
            self.fail(ctx, K::NoVp9Profile2);
            return false;
        }
        self.teardown_session(ctx);
        let Some(display) = &self.display else {
            return false;
        };
        let count = crate::va::decode_surface_count(
            NUM_SLOTS as u32,
            SUPERFRAME_RESERVE as u32,
            self.zerocopy.is_some(),
        );
        log!(&*ctx, Level::Info, "surfaces", count = count, width = coded_w, height = coded_h);
        log!(
            &*ctx,
            Level::Info,
            "session",
            profile = h.profile,
            ten_bit = ten_bit,
            zero_copy = self.zerocopy.is_some()
        );
        let profile = if ten_bit { ffi::VAProfileVP9Profile2 } else { ffi::VAProfileVP9Profile0 };
        let readback_mode = self.zerocopy.is_none();
        let config = match shrink {
            Some(_) => Config::new_decode_processing(display, profile, format),
            None => Config::new_decode_for(display, profile, format),
        };
        let built = config.and_then(|config| {
            let surfaces = Surfaces::new(display, format, coded_w, coded_h, count)?;
            let context =
                Context::new(display, &config, coded_w as i32, coded_h as i32, &surfaces)?;
            let readback = match readback_mode {
                true => Some(ReadbackImage::new(
                    display,
                    format.fourcc,
                    format.bits_per_pixel,
                    coded_w,
                    coded_h,
                )?),
                false => None,
            };
            Ok((config, context, surfaces, readback))
        });
        let (config, context, surfaces, readback) = match built {
            Ok(b) => b,
            Err(e) => {
                self.fail(ctx, K::SessionSetup(e));
                return false;
            }
        };
        // a VP9 frame is its exact size, nothing cropped
        let made = shrink.map(|(p, at)| {
            Twins::new(display, p, count, (0, 0, w, hgt), (w, hgt)).map(|t| (t, at))
        });
        let twins = match made.transpose() {
            Ok(t) => t,
            Err(e) => {
                self.fail(ctx, e);
                return false;
            }
        };
        if let Some((t, _)) = &twins {
            let (w, h) = (t.plan.width, t.plan.height);
            log!(&*ctx, Level::Info, "shrink", width = w, height = h, layout = t.kind().name);
        }
        let made = if twins.is_some() { count } else { 0 };
        self.stats.made(count + made, made);
        let free = surfaces.ids().iter().copied().collect();
        self.row = if readback_mode { vec![0u8; coded_w as usize * 2] } else { Vec::new() };
        self.va = Some(VaState {
            _config: config,
            context,
            format,
            surfaces,
            free,
            coded_w,
            coded_h,
            readback,
            twins,
        });
        true
    }

    fn teardown_session(&mut self, ctx: &Ctx) {
        // the key frame refreshes every slot, the state it folded stays
        let mut ev = Events::new();
        self.dpb.drop_slots(&mut ev);
        self.surfaces.clear();
        self.held.clear();
        if !self.output_queue.is_empty() {
            log!(ctx, Level::Debug, "teardown_drop", queued = self.output_queue.len());
        }
        for _ in self.output_queue.drain(..) {
            ctx.count_drop();
        }
        self.in_flight.clear();
        self.va = None;
    }

    fn free_surfaces(&self) -> usize {
        self.va.as_ref().map_or(usize::MAX, |va| va.free.len())
    }

    fn acquire_surface(&mut self) -> Option<ffi::VASurfaceID> {
        self.va.as_mut().and_then(|va| va.free.pop_front())
    }

    fn surface_referenced(&self, id: ffi::VASurfaceID) -> bool {
        self.surfaces.iter().zip(&self.held).any(|(&s, &held)| held && s == id)
            || self.output_queue.iter().any(|o| o.surface == id)
            || self.in_flight.iter().any(|(s, _)| *s == id)
    }

    fn release_if_unreferenced(&mut self, id: ffi::VASurfaceID) {
        if self.surface_referenced(id) {
            return;
        }
        if let Some(va) = &mut self.va
            && va.surfaces.ids().contains(&id)
            && !va.free.contains(&id)
        {
            va.free.push_back(id);
        }
    }

    fn decode_packet(&mut self, ctx: &mut Ctx, unit: &[u8], pts: Timestamp, duration: Timestamp) {
        let mut ranges = std::mem::take(&mut self.ranges);
        crate::vp9::superframe::split_superframe_into(unit, &mut ranges);
        let last = ranges.len().saturating_sub(1);
        for (i, &(start, end)) in ranges.iter().enumerate() {
            let (p, d) =
                if i == last { (pts, duration) } else { (Timestamp::ZERO, Timestamp::ZERO) };
            self.decode_frame(ctx, &unit[start..end], p, d);
        }
        self.ranges = ranges;
    }

    fn decode_frame(&mut self, ctx: &mut Ctx, frame: &[u8], pts: Timestamp, duration: Timestamp) {
        let h = match self.dpb.parse(frame) {
            Ok(h) => h,
            Err(_) => {
                if !self.dpb.awaits_key() {
                    self.warn(ctx, K::Vp9Header);
                }
                self.dpb.await_key();
                return;
            }
        };
        if self.dpb.awaits_key() && h.frame_type != FrameType::KeyFrame {
            log!(&*ctx, Level::Trace, "await_key", pts = pts);
            return;
        }
        let mut ev = Events::new();
        if h.show_existing_frame {
            match self.dpb.begin(&h, &mut ev) {
                Ok(_) => self.apply(ctx, &ev, None, pts, duration),
                Err(_) => {
                    let slot = h.frame_to_show_map_idx.unwrap_or(0);
                    self.warn(ctx, K::EmptyRefSlot { slot });
                }
            }
            return;
        }
        if !supported(&h.color_config) {
            let cc = &h.color_config;
            self.fail(
                ctx,
                K::UnsupportedVp9 {
                    profile: h.profile,
                    bit_depth: cc.bit_depth,
                    subsampling_x: cc.subsampling_x,
                    subsampling_y: cc.subsampling_y,
                },
            );
            self.dpb.await_key();
            return;
        }
        // `frame_header_length_in_bytes` is a byte, and the compressed header is never empty
        // or past the frame (6.1, 6.3).
        let compressed = usize::from(h.header_size_in_bytes);
        if h.uncompressed_header_size_bytes > 255
            || compressed == 0
            || h.uncompressed_header_size_bytes + compressed > frame.len()
        {
            self.warn(ctx, K::Vp9Header);
            self.dpb.await_key();
            return;
        }
        // takes the frame's state, a slot it names empty refuses it
        let fate = match self.dpb.begin(&h, &mut ev) {
            Ok(fate) => fate,
            Err(e) => {
                let empty = |i: &&u8| self.dpb.slots()[usize::from(**i & 7)].is_none();
                match h.ref_frame_idx.as_ref().and_then(|idx| idx.iter().find(empty)) {
                    Some(&slot) if e == DpbError::MissingReference => {
                        self.warn(ctx, K::EmptyRefSlot { slot })
                    }
                    _ => self.warn(ctx, K::Dpb(e)),
                }
                self.dpb.await_key();
                return;
            }
        };
        if !self.ensure_va(ctx, &h) {
            self.abandon(ctx, &h, pts, duration);
            return;
        }
        let Some(target) = self.acquire_surface() else {
            self.warn(ctx, K::NoFreeSurface);
            self.abandon(ctx, &h, pts, duration);
            return;
        };
        if let Err(e) = self.submit(&h, frame, target) {
            self.picture_failed(ctx, &e, K::Submit(e));
            self.release_if_unreferenced(target);
            self.abandon(ctx, &h, pts, duration);
            return;
        }
        let slot = self.slot_of(target);
        self.held[usize::from(slot.0)] = true;
        self.dpb.end(&h, fate, slot, &mut ev);
        let cur = (slot, h.frame_width, h.frame_height);
        self.apply(ctx, &ev, Some(cur), pts, duration);
    }

    /// A frame the DPB began that was never decoded, the frames up to the next key frame are
    /// skipped and the slots it would have taken are let go.
    fn abandon(&mut self, ctx: &mut Ctx, h: &Vp9FrameHeader, pts: Timestamp, duration: Timestamp) {
        let mut ev = Events::new();
        self.dpb.abandon(h, &mut ev);
        self.apply(ctx, &ev, None, pts, duration);
    }

    /// Carries out what the DPB asked. A picture goes out with the current frame's timestamps,
    /// `cur` the frame decoded now and its size.
    fn apply(
        &mut self,
        ctx: &mut Ctx,
        ev: &Events,
        cur: Option<(Slot, u32, u32)>,
        pts: Timestamp,
        duration: Timestamp,
    ) {
        for e in ev.as_slice() {
            match *e {
                DpbEvent::Output(slot) => {
                    let size = match cur {
                        Some((s, w, h)) if s == slot => Some((w, h)),
                        _ => self
                            .dpb
                            .slots()
                            .iter()
                            .flatten()
                            .find(|r| r.slot == slot)
                            .map(|r| (r.width, r.height)),
                    };
                    if let Some((w, h)) = size {
                        let surface = self.surfaces[usize::from(slot.0)];
                        self.queue_output(ctx, surface, w, h, pts, duration);
                    }
                }
                DpbEvent::Release(slot) => {
                    let i = usize::from(slot.0);
                    self.held[i] = false;
                    self.release_if_unreferenced(self.surfaces[i]);
                }
            }
        }
    }

    /// The DPB's name of surface `id`, a slot per surface seen.
    fn slot_of(&mut self, id: ffi::VASurfaceID) -> Slot {
        let i = match self.surfaces.iter().position(|&s| s == id) {
            Some(i) => i,
            None => {
                self.surfaces.push(id);
                self.held.push(false);
                self.surfaces.len() - 1
            }
        };
        Slot(i as u16)
    }

    fn submit(
        &self,
        h: &Vp9FrameHeader,
        frame: &[u8],
        target: ffi::VASurfaceID,
    ) -> Result<(), VaError> {
        let (Some(display), Some(va)) = (&self.display, &self.va) else {
            return Ok(());
        };
        let mut refs = [ffi::VA_INVALID_SURFACE; NUM_SLOTS];
        for (r, s) in refs.iter_mut().zip(self.dpb.slots()) {
            if let Some(s) = s {
                *r = self.surfaces[usize::from(s.slot.0)];
            }
        }
        let state = self.dpb.state();
        let pic = pic_param(h, state, refs);
        let slice = slice_param(h, state, frame.len());
        let pic_buf =
            VaBuffer::new_struct(display, &va.context, ffi::VAPictureParameterBufferType, &pic)?;
        let slice_buf =
            VaBuffer::new_struct(display, &va.context, ffi::VASliceParameterBufferType, &slice)?;
        let data_buf = VaBuffer::new_data(display, &va.context, frame)?;
        // every picture names its twin, iHD would write the last one's again
        let mut regions = va.twins.as_ref().map(|(t, _)| {
            let twin = t.of(&va.surfaces, target).unwrap_or(ffi::VA_INVALID_SURFACE);
            decproc::Regions::new(t, twin)
        });
        let proc_buf = match regions.as_mut() {
            Some(r) => Some(decproc::pipeline(display, &va.context, target, r)?),
            None => None,
        };
        va.context.begin(target)?;
        let rendered = va
            .context
            .render(&[pic_buf.id()])
            .and_then(|()| match &proc_buf {
                Some(p) => va.context.render(&[p.id()]),
                None => Ok(()),
            })
            .and_then(|()| va.context.render(&[slice_buf.id(), data_buf.id()]));
        // vaEndPicture runs after a failed render too, so the context is not left mid-picture.
        rendered.and(va.context.end())
    }

    fn queue_output(
        &mut self,
        ctx: &mut Ctx,
        surface: ffi::VASurfaceID,
        width: u32,
        height: u32,
        pts: Timestamp,
        duration: Timestamp,
    ) {
        let before = self.only || ctx.before_presentation(pts, Timestamp::ZERO);
        let preview = before && !self.only && ctx.take_preview();
        if before && !preview {
            log!(&*ctx, Level::Trace, "decode_only", pts = pts);
            ctx.count_drop();
            return;
        }
        let Some(va) = &self.va else { return };
        let (export, width, height, coded_w, coded_h) = match &va.twins {
            Some((t, _)) => {
                let twin = t.of(&va.surfaces, surface).unwrap_or(surface);
                (twin, t.plan.width, t.plan.height, t.coded.0, t.coded.1)
            }
            None => (surface, width, height, va.coded_w, va.coded_h),
        };
        self.output_queue.push_back(PendingOut {
            surface,
            export,
            pts,
            duration,
            width,
            height,
            coded_w,
            coded_h,
            format: va.format,
            color: stated_color(&self.dpb.state().color),
            preview,
        });
    }

    fn emit_queued(&mut self, ctx: &mut Ctx, keep: usize) -> Result<bool, Error> {
        while self.output_queue.len() > keep {
            let Some(out) = self.output_queue.pop_front() else {
                break;
            };
            let sent = if self.zerocopy.is_some() {
                self.emit_zerocopy(ctx, out)?
            } else {
                self.emit_readback(ctx, out)?
            };
            if !sent {
                self.output_queue.push_front(out);
                return Ok(false);
            }
            self.drain_presentation_releases();
        }
        Ok(true)
    }

    fn announce_raw(&mut self, ctx: &mut Ctx, out: &PendingOut, pixfmt: &'static str) {
        let shape = Some((out.width, out.height, 0, out.format.fourcc, out.color));
        if self.shape == shape {
            return;
        }
        log!(&*ctx, Level::Debug, "announce", width = out.width, height = out.height);
        let mut fields = [(F_WIDTH, ValueDesc::Int(out.width as i64)); 3 + colour::FIELDS.len()];
        fields[1] = (F_HEIGHT, ValueDesc::Int(out.height as i64));
        fields[2] = (F_PIXFMT, ValueDesc::Id(pixfmt));
        let n = 3 + color_fields(ctx, Self::IN, out.color, &mut fields[3..]);
        ctx.announce_format(Self::OUT, FAMILY, &fields[..n]);
        self.shape = shape;
    }

    fn emit_readback(&mut self, ctx: &mut Ctx, out: PendingOut) -> Result<bool, Error> {
        let (width, height) = (out.width, out.height);
        let ten_bit = out.format == SurfaceFormat::P010;
        let need = frame_bytes(width, height, ten_bit);
        let (pixfmt, layout) = if ten_bit { (PIXFMT_P010, "P010") } else { (PIXFMT_I420, "I420") };

        if ctx.grow_slots(need) {
            log!(&*ctx, Level::Debug, "grow_slots", bytes = need);
        }
        let Some(mut buf) = ctx.try_alloc() else {
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
            return Ok(false);
        };
        self.alloc_stalled = false;
        if buf.memory.capacity() < need {
            self.release_if_unreferenced(out.surface);
            let capacity = buf.memory.capacity();
            return Err(failed(
                ctx,
                DESC.name,
                K::FrameSlot { layout, width, height, need, capacity },
            ));
        }
        let Some(image) = self.va.as_mut().and_then(|va| va.readback.as_mut()) else {
            return Ok(true);
        };
        let copied = copy_out(image, &mut self.row, &out, &mut buf.memory.as_mut_full()[..need]);
        if let Err(fail) = copied {
            self.release_if_unreferenced(out.surface);
            match fail {
                CopyFail::Va(e) => self.picture_failed(ctx, &e, K::Readback(e)),
                CopyFail::Planes(planes) => self.warn(ctx, K::NotNv12 { planes }),
                CopyFail::Overrun(plane, row) => self.warn(ctx, K::PlaneOverrun { plane, row }),
            }
            return Ok(true);
        }
        self.announce_raw(ctx, &out, pixfmt);
        buf.memory.set_len(need);
        buf.pts = out.pts;
        buf.duration = out.duration;
        if out.preview {
            buf.flags |= BufferFlags::PREVIEW;
        }
        log!(&*ctx, Level::Trace, "frame", pts = buf.pts);
        self.side.attach(buf.pts.0, &mut buf.memory);
        ctx.out(Self::OUT).push(buf);
        self.emitted += 1;
        self.first.sent(ctx);
        self.transient.ok();
        self.release_if_unreferenced(out.surface);
        Ok(true)
    }

    fn emit_zerocopy(&mut self, ctx: &mut Ctx, out: PendingOut) -> Result<bool, Error> {
        let Some(dpy) = self.display.as_ref().map(Display::raw) else {
            return Ok(true);
        };
        // a queued handoff exports the picture as it is, the dma-buf's fence orders the consumer
        let queued = self.zerocopy.as_ref().is_some_and(|c| c.handoff() == Handoff::Queued);
        // a pending command, a seek's flush, cuts the wait short, the decoder runs again after it
        if !queued {
            let t = crate::stats::mark();
            let done = crate::va::wait_surface(dpy, out.export, || ctx.command_pending());
            self.stats.waited(t);
            if !done {
                ctx.wake_within(crate::enc::POLL_WAKE);
                return Ok(false);
            }
        }
        let export = if queued { ExportedSurface::export_queued } else { ExportedSurface::export };
        let mut exported = match export(dpy, out.export) {
            Ok(e) => e,
            Err(e) => {
                self.warn(ctx, K::Export(e));
                self.release_if_unreferenced(out.surface);
                return Ok(true);
            }
        };
        let same = self.link.filter(|(fourcc, _)| *fourcc == exported.drm_format);
        if let Some((_, negotiated)) = same.filter(|(_, m)| *m != exported.drm_modifier) {
            self.release_if_unreferenced(out.surface);
            let kind = K::ModifierChanged { negotiated, exported: exported.drm_modifier };
            return Err(failed(ctx, DESC.name, kind));
        }
        let Some(layout) = gpuframe::layout_of(exported.drm_format) else {
            self.warn(ctx, K::NotNv12 { planes: exported.planes.len() as u32 });
            self.release_if_unreferenced(out.surface);
            return Ok(true);
        };
        let header = match ctx.try_alloc_sized(GPU_FRAME_HEADER_LEN) {
            Ok(Some(buf)) => buf,
            Ok(None) => {
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
                return Ok(false);
            }
            Err(e) => {
                self.release_if_unreferenced(out.surface);
                return Err(failed(ctx, DESC.name, K::HeaderSlot { need: e.n, capacity: e.max }));
            }
        };
        self.alloc_stalled = false;
        let mut buf = header;
        if buf.memory.as_mut_full().len() < GPU_FRAME_HEADER_LEN {
            self.release_if_unreferenced(out.surface);
            let capacity = buf.memory.as_mut_full().len();
            return Err(failed(
                ctx,
                DESC.name,
                K::HeaderSlot { need: GPU_FRAME_HEADER_LEN, capacity },
            ));
        }
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
        }

        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        let header = GpuFrameHeader {
            token,
            coded_w: out.coded_w,
            coded_h: out.coded_h,
            disp_w: out.width,
            disp_h: out.height,
            crop_x: 0,
            crop_y: 0,
        };
        header.write_to(buf.memory.as_mut_full());
        buf.memory.set_len(GPU_FRAME_HEADER_LEN);
        buf.pts = out.pts;
        buf.duration = out.duration;
        if out.preview {
            buf.flags |= BufferFlags::PREVIEW;
        }
        let frame = GpuFrame {
            token,
            fds: std::mem::take(&mut exported.fds),
            drm_format: exported.drm_format,
            coded_w: exported.coded_w,
            coded_h: exported.coded_h,
            disp_w: out.width,
            disp_h: out.height,
            crop_x: 0,
            crop_y: 0,
            planes: std::mem::take(&mut exported.planes),
            epoch: ctx.applied_seek_gen(),
            lease: None,
        };
        drop(exported);
        // Marked in flight before the buffer goes, so a fast sink cannot release first.
        self.side.attach(buf.pts.0, &mut buf.memory);
        let sent = self.tx.as_mut().is_some_and(|tx| tx.push(frame, buf.memory.clone()));
        if !sent {
            self.warned.note_name(ctx, "channel_full", 0);
            self.release_if_unreferenced(out.surface);
            return Ok(true);
        }
        self.in_flight.push((out.surface, token));
        log!(&*ctx, Level::Trace, "gpu_frame", pts = buf.pts, token = token);
        ctx.out(Self::OUT).push(buf);
        self.emitted += 1;
        self.first.sent(ctx);
        self.transient.ok();
        Ok(true)
    }

    fn drain_presentation_releases(&mut self) {
        let Some(tx) = self.tx.as_mut() else { return };
        let mut released = std::mem::take(&mut self.released);
        tx.drain_released(&mut released);
        for token in released.drain(..) {
            if let Some(at) = self.in_flight.iter().position(|(_, t)| *t == token) {
                let (surface, _) = self.in_flight.swap_remove(at);
                self.release_if_unreferenced(surface);
            }
        }
        self.released = released;
    }

    fn drop_before_point(&mut self, ctx: &Ctx) {
        let mut at = 0;
        while at < self.output_queue.len() {
            let o = self.output_queue[at];
            if !ctx.before_presentation(o.pts, Timestamp::ZERO) {
                at += 1;
                continue;
            }
            self.output_queue.remove(at);
            ctx.count_drop();
            self.release_if_unreferenced(o.surface);
        }
    }

    fn reset_stream(&mut self) {
        while let Some(o) = self.output_queue.pop_front() {
            self.release_if_unreferenced(o.surface);
        }
        let mut ev = Events::new();
        self.dpb.discard(&mut ev);
        for e in ev.as_slice() {
            if let DpbEvent::Release(slot) = *e {
                let i = usize::from(slot.0);
                self.held[i] = false;
                self.release_if_unreferenced(self.surfaces[i]);
            }
        }
    }

    fn drain_all(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        self.drain_presentation_releases();
        if !self.emit_queued(ctx, 0)? {
            ctx.hold_end();
            return Ok(false);
        }
        Ok(true)
    }

    fn surfaces_needed(&mut self, unit: &[u8]) -> usize {
        crate::vp9::superframe::split_superframe_into(unit, &mut self.ranges);
        self.ranges.len().clamp(1, SUPERFRAME_RESERVE)
    }
}

impl Element for VaapiVp9Dec {
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
            // The newest picture stays back a packet so its decode overlaps the previous readback.
            let keep = usize::from(inputs.front().is_some() && !self.first.fresh());
            if !self.emit_queued(ctx, keep)? {
                return Ok(Flow::Ok);
            }
            // a picture put out goes downstream now, the next decode would hold it back
            if ctx.command_pending() || self.emitted != emitted {
                return Ok(Flow::Ok);
            }
            if self.first.holds(ctx) {
                return Ok(Flow::Ok);
            }
            let Some(front) = inputs.front() else { break };
            let need = self.surfaces_needed(front.memory.data());
            // The one held back goes first, it may be what the sink waits for.
            if self.free_surfaces() < need && !self.emit_queued(ctx, 0)? {
                return Ok(Flow::Ok);
            }
            while self.free_surfaces() < need && !self.in_flight.is_empty() {
                ctx.await_recycle();
                self.drain_presentation_releases();
                if self.free_surfaces() < need {
                    return Ok(Flow::Ok);
                }
            }
            let Some(inbuf) = inputs.pop() else { break };
            self.only = inbuf.flags.contains(BufferFlags::DECODE_ONLY);
            self.side.keep(&inbuf.memory, inbuf.pts.0);
            self.decode_packet(ctx, inbuf.memory.data(), inbuf.pts, inbuf.duration);
        }
        self.emit_queued(ctx, 0)?;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", queued = self.output_queue.len());
                self.warned.summary(ctx, "flush");
                self.side.clear();
                self.drain_presentation_releases();
                self.reset_stream();
                self.alloc_stalled = false;
                self.ended = false;
                self.first.reset();
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
        self.output_queue.clear();
        *self.dpb = bs::Dpb::new();
        self.surfaces.clear();
        self.held.clear();
        self.first.reset();
        self.va = None;
        self.display = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitwriter::BitWriter;
    use pf_bitstream::vp9::{SEG_LVL_ALT_Q, ac_q, dc_q};

    fn signed(w: &mut BitWriter, v: i32, n: u32) {
        w.u(v.unsigned_abs(), n);
        w.flag(v < 0);
    }

    fn delta_q(w: &mut BitWriter, v: i32) {
        w.flag(v != 0);
        if v != 0 {
            signed(w, v, 4);
        }
    }

    fn prob(w: &mut BitWriter, p: u8) {
        w.flag(p != 255);
        if p != 255 {
            w.u(u32::from(p), 8);
        }
    }

    fn lf_delta(w: &mut BitWriter, d: Option<i32>) {
        w.flag(d.is_some());
        if let Some(d) = d {
            signed(w, d, 6);
        }
    }

    type LfDeltas = ([Option<i32>; 4], [Option<i32>; 2]);

    #[derive(Default)]
    struct Tail {
        lf_level: u32,
        sharpness: u32,
        lf_delta_enabled: bool,
        lf_deltas: Option<LfDeltas>,
        base_q: u32,
        dq: [i32; 3],
        segmentation: Option<Seg>,
        header_size: u32,
    }

    type Feature = (usize, usize, i32);

    struct Seg {
        tree: [u8; 7],
        pred: Option<[u8; 3]>,
        data: Option<(bool, &'static [Feature])>,
    }

    fn write_tail(w: &mut BitWriter, t: &Tail) {
        w.u(t.lf_level, 6);
        w.u(t.sharpness, 3);
        w.flag(t.lf_delta_enabled);
        if t.lf_delta_enabled {
            w.flag(t.lf_deltas.is_some());
        }
        if let Some((refs, modes)) = t.lf_deltas.filter(|_| t.lf_delta_enabled) {
            refs.into_iter().for_each(|d| lf_delta(w, d));
            modes.into_iter().for_each(|d| lf_delta(w, d));
        }
        w.u(t.base_q, 8);
        t.dq.into_iter().for_each(|d| delta_q(w, d));
        w.flag(t.segmentation.is_some());
        if let Some(seg) = &t.segmentation {
            w.flag(true);
            seg.tree.into_iter().for_each(|p| prob(w, p));
            w.flag(seg.pred.is_some());
            if let Some(pred) = seg.pred {
                pred.into_iter().for_each(|p| prob(w, p));
            }
            w.flag(seg.data.is_some());
            if let Some((abs, features)) = seg.data {
                w.flag(abs);
                const BITS: [u32; 4] = [8, 6, 2, 0];
                const SIGNED: [bool; 4] = [true, true, false, false];
                for i in 0..8 {
                    for j in 0..4 {
                        let v = features.iter().find(|f| f.0 == i && f.1 == j).map(|f| f.2);
                        w.flag(v.is_some());
                        if let Some(v) = v {
                            w.u(v.unsigned_abs(), BITS[j]);
                            if SIGNED[j] {
                                w.flag(v < 0);
                            }
                        }
                    }
                }
            }
        }
        w.flag(false);
        w.u(t.header_size, 16);
        while !w.bit_len().is_multiple_of(8) {
            w.flag(false);
        }
    }

    fn key_frame(profile: u32, width: u32, height: u32, t: &Tail) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(2, 2);
        w.u(profile & 1, 1);
        w.u(profile >> 1, 1);
        w.flag(false); // show_existing_frame
        w.flag(false); // frame_type KEY_FRAME
        w.flag(true); // show_frame
        w.flag(false); // error_resilient_mode
        w.u(0x49, 8);
        w.u(0x83, 8);
        w.u(0x42, 8);
        if profile >= 2 {
            w.flag(false); // ten_or_twelve_bit, 10
        }
        w.u(2, 3); // CS_BT_709
        w.flag(false); // color_range studio
        w.u(width - 1, 16);
        w.u(height - 1, 16);
        w.flag(false); // render_and_frame_size_different
        w.flag(true);
        w.flag(false);
        w.u(2, 2);
        write_tail(&mut w, t);
        w.into_bytes()
    }

    fn inter_frame(error_resilient: bool, t: &Tail) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(2, 2);
        w.u(0, 2); // profile 0
        w.flag(false); // show_existing_frame
        w.flag(true); // frame_type NON_KEY_FRAME
        w.flag(true); // show_frame, so no intra_only bit
        w.flag(error_resilient);
        if !error_resilient {
            w.u(1, 2); // reset_frame_context
        }
        w.u(0x04, 8);
        for (idx, bias) in [(0, false), (1, false), (2, true)] {
            w.u(idx, 3);
            w.flag(bias);
        }
        w.flag(false); // found_ref LAST
        w.flag(true); // found_ref GOLDEN
        w.flag(false); // render_and_frame_size_different
        w.flag(true); // allow_high_precision_mv
        w.flag(false); // is_filter_switchable
        w.u(2, 2); // literal_to_type[2] EIGHTTAP_SHARP
        if !error_resilient {
            w.flag(false);
            w.flag(true);
        }
        w.u(3, 2);
        write_tail(&mut w, t);
        w.into_bytes()
    }

    fn intra_only_frame(t: &Tail) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(2, 2);
        w.u(0, 2);
        w.flag(false);
        w.flag(true); // NON_KEY_FRAME
        w.flag(false); // show_frame
        w.flag(false); // error_resilient_mode
        w.flag(true); // intra_only
        w.u(3, 2); // reset_frame_context
        w.u(0x49, 8);
        w.u(0x83, 8);
        w.u(0x42, 8);
        w.u(0x81, 8);
        w.u(63, 16);
        w.u(47, 16);
        w.flag(false);
        w.flag(true);
        w.flag(false);
        w.u(1, 2);
        write_tail(&mut w, t);
        w.into_bytes()
    }

    fn slot(_surface: u32, width: u32, height: u32) -> Option<(u32, u32)> {
        Some((width, height))
    }

    /// The header as the element parses it against slots of these sizes.
    fn parse_header(
        frame: &[u8],
        slots: &[Option<(u32, u32)>; 8],
        state: &StreamState,
    ) -> Result<Vp9FrameHeader, crate::vp9::Error> {
        let mut dims = [(0u32, 0u32); 8];
        for (d, s) in dims.iter_mut().zip(slots) {
            if let Some(s) = s {
                *d = *s;
            }
        }
        let refs = crate::vp9::RefFrameState { ref_dims: &dims, color_config: state.color };
        crate::vp9::parse_uncompressed_header_with_refs(frame, Some(refs))
    }

    fn feature_key_frame() -> (Vp9FrameHeader, StreamState) {
        let t = Tail {
            lf_level: 36,
            sharpness: 2,
            lf_delta_enabled: true,
            lf_deltas: Some(([Some(1), None, Some(-2), None], [None, Some(3)])),
            base_q: 60,
            dq: [-3, 0, 2],
            segmentation: Some(Seg {
                tree: [10, 20, 255, 40, 50, 60, 70],
                pred: None,
                data: Some((false, &[(1, 0, -10), (2, 1, 5), (3, 2, 2), (4, 3, 0), (5, 0, -100)])),
            }),
            header_size: 777,
        };
        let bytes = key_frame(0, 352, 288, &t);
        let mut state = StreamState::new();
        let h = parse_header(&bytes, &[None; 8], &state).expect("key frame parses");
        state.fold(&h);
        (h, state)
    }

    #[test]
    fn a_key_frame_header_parses_to_its_fields() {
        let (h, _) = feature_key_frame();
        assert_eq!(h.frame_type, FrameType::KeyFrame);
        assert_eq!((h.frame_width, h.frame_height), (352, 288));
        assert_eq!(h.color_config.color_space, ColorSpace::Bt709);
        assert_eq!(h.refresh_frame_flags, 0xFF);
        assert_eq!(h.frame_context_idx, 0);
        assert!(h.refresh_frame_context && !h.frame_parallel_decoding_mode);
        assert_eq!((h.loop_filter.level, h.loop_filter.sharpness), (36, 2));
        assert_eq!(h.quantization.base_q_idx, 60);
        assert_eq!(h.header_size_in_bytes, 777);
        // 2+2+1+1+1+1 + 24 + 3+1 + 32 + 1 + 1+1+2 = 73 bits, then the tail.
        assert!(h.uncompressed_header_size_bytes > 9);
        assert_eq!(h.segmentation.tree_probs, Some([10, 20, 255, 40, 50, 60, 70]));
        assert_eq!(h.segmentation.pred_prob, Some([255; 3]));
    }

    #[test]
    fn pic_fields_pack_a_key_frame() {
        let (h, state) = feature_key_frame();
        let f = pic_fields(&h);
        let want = ffi::VP9_PIC_SUBSAMPLING_X
            | ffi::VP9_PIC_SUBSAMPLING_Y
            | ffi::VP9_PIC_SHOW_FRAME
            | ffi::VP9_PIC_REFRESH_FRAME_CONTEXT
            | ffi::VP9_PIC_SEGMENTATION_ENABLED
            | ffi::VP9_PIC_SEGMENTATION_UPDATE_MAP;
        assert_eq!(f, want, "{f:#010x}");
        let refs = [7, 7, 7, 7, 7, 7, 7, 7];
        let p = pic_param(&h, &state, refs);
        assert_eq!((p.frame_width, p.frame_height), (352, 288));
        assert_eq!((p.filter_level, p.sharpness_level, p.bit_depth, p.profile), (36, 2, 8, 0));
        assert_eq!(p.first_partition_size, 777);
        assert_eq!(usize::from(p.frame_header_length_in_bytes), h.uncompressed_header_size_bytes);
        assert_eq!(p.mb_segment_tree_probs, [10, 20, 255, 40, 50, 60, 70]);
    }

    #[test]
    fn loop_filter_levels_follow_the_deltas_and_the_segment_feature() {
        let (h, state) = feature_key_frame();
        assert_eq!(state.lf_ref_deltas, [1, 0, -2, -1]);
        assert_eq!(state.lf_mode_deltas, [0, 3]);
        assert_eq!(state.filter_levels(&h, 0), [[38, 38], [36, 42], [32, 38], [34, 40]]);
        assert_eq!(state.filter_levels(&h, 2), [[43, 43], [41, 47], [37, 43], [39, 45]]);
    }

    #[test]
    fn dequantizer_scales_follow_the_segment_qindex() {
        let (h, state) = feature_key_frame();
        assert_eq!(state.qindex(&h, 0), 60);
        assert_eq!(state.qindex(&h, 1), 50);
        assert_eq!(state.qindex(&h, 5), 0);
        let sp = slice_param(&h, &state, 1234);
        assert_eq!((sp.slice_data_size, sp.slice_data_offset), (1234, 0));
        for (i, q) in [(0, 60), (1, 50), (5, 0)] {
            let s = sp.seg_param[i];
            assert_eq!(s.luma_ac_quant_scale, ac_q(8, q));
            assert_eq!(s.luma_dc_quant_scale, dc_q(8, q - 3));
            assert_eq!(s.chroma_dc_quant_scale, dc_q(8, q));
            assert_eq!(s.chroma_ac_quant_scale, ac_q(8, q + 2));
        }
        assert_eq!(
            (sp.seg_param[0].luma_dc_quant_scale, sp.seg_param[0].luma_ac_quant_scale),
            (55, 67)
        );
        assert_eq!(
            sp.seg_param[3].segment_flags,
            ffi::VP9_SEG_REFERENCE_ENABLED | (2 << ffi::VP9_SEG_REFERENCE_SHIFT)
        );
        assert_eq!(sp.seg_param[4].segment_flags, ffi::VP9_SEG_REFERENCE_SKIPPED);
        assert_eq!(sp.seg_param[0].segment_flags, 0);
    }

    #[test]
    fn an_inter_frame_parses_against_the_slots_and_the_held_state() {
        let (_, mut state) = feature_key_frame();
        let mut slots = [slot(1, 352, 288); 8];
        slots[1] = slot(2, 320, 240);
        let t = Tail {
            lf_level: 10,
            lf_delta_enabled: true,
            lf_deltas: None,
            base_q: 100,
            header_size: 5,
            ..Tail::default()
        };
        let h = parse_header(&inter_frame(false, &t), &slots, &state).expect("inter frame parses");
        assert_eq!((h.frame_width, h.frame_height), (320, 240));
        assert_eq!(h.ref_frame_idx, Some([0, 1, 2]));
        assert_eq!(h.ref_frame_sign_bias, [false, false, true]);
        assert_eq!(h.interpolation_filter, 2);
        assert_eq!(
            (h.frame_context_idx, h.reset_frame_context, h.refresh_frame_flags),
            (3, 1, 0x04)
        );
        assert_eq!(h.color_config.color_space, ColorSpace::Bt709, "inherited from the key frame");
        let want = ffi::VP9_PIC_SUBSAMPLING_X
            | ffi::VP9_PIC_SUBSAMPLING_Y
            | ffi::VP9_PIC_FRAME_TYPE
            | ffi::VP9_PIC_SHOW_FRAME
            | ffi::VP9_PIC_ALLOW_HIGH_PRECISION_MV
            | (2 << ffi::VP9_PIC_MCOMP_FILTER_TYPE_SHIFT)
            | ffi::VP9_PIC_FRAME_PARALLEL_DECODING_MODE
            | (1 << ffi::VP9_PIC_RESET_FRAME_CONTEXT_SHIFT)
            | (3 << ffi::VP9_PIC_FRAME_CONTEXT_IDX_SHIFT)
            | (1 << ffi::VP9_PIC_GOLDEN_REF_FRAME_SHIFT)
            | (2 << ffi::VP9_PIC_ALT_REF_FRAME_SHIFT)
            | ffi::VP9_PIC_ALT_REF_FRAME_SIGN_BIAS;
        assert_eq!(pic_fields(&h), want, "{:#010x}", pic_fields(&h));
        state.fold(&h);
        assert_eq!(state.filter_levels(&h, 0), [[11, 11], [10, 13], [8, 11], [9, 12]]);
        assert_eq!(state.qindex(&h, 1), 100);
        assert_eq!(state.feature_data[1][SEG_LVL_ALT_Q], -10);
        let sp = slice_param(&h, &state, 9);
        assert_eq!(sp.seg_param[0].luma_ac_quant_scale, ac_q(8, 100));
        assert_eq!(sp.seg_param[1].luma_ac_quant_scale, 0, "only segment 0 with segmentation off");
    }

    #[test]
    fn error_resilience_resets_the_held_state() {
        let (_, mut state) = feature_key_frame();
        let slots = [slot(1, 352, 288); 8];
        let t = Tail {
            lf_level: 40,
            lf_delta_enabled: true,
            lf_deltas: Some(([None; 4], [None; 2])),
            ..Tail::default()
        };
        let h = parse_header(&inter_frame(true, &t), &slots, &state).expect("inter frame parses");
        assert!(h.error_resilient_mode);
        assert_eq!(h.frame_context_idx, 0, "section 6.2 forces context 0");
        assert_eq!(h.reset_frame_context, 0);
        assert!(h.frame_parallel_decoding_mode && !h.refresh_frame_context);
        state.fold(&h);
        assert_eq!(state.lf_ref_deltas, [1, 0, -1, -1]);
        assert_eq!(state.feature_enabled, [[false; 4]; 8]);
        assert!(!state.abs_or_delta_update);
    }

    #[test]
    fn an_intra_only_frame_takes_profile_0_defaults() {
        let (_, mut state) = feature_key_frame();
        let h = parse_header(&intra_only_frame(&Tail::default()), &[None; 8], &state)
            .expect("intra-only parses");
        assert!(h.intra_only && !h.show_frame);
        assert_eq!((h.frame_width, h.frame_height, h.refresh_frame_flags), (64, 48, 0x81));
        assert_eq!(h.color_config.color_space, ColorSpace::Bt601);
        assert_eq!(h.color_config.bit_depth, 8);
        assert_eq!(h.reset_frame_context, 3);
        assert_eq!(h.frame_context_idx, 0);
        assert_eq!(h.ref_frame_idx, None);
        let f = pic_fields(&h);
        assert_ne!(f & ffi::VP9_PIC_FRAME_TYPE, 0);
        assert_ne!(f & ffi::VP9_PIC_INTRA_ONLY, 0);
        assert_eq!(f & ffi::VP9_PIC_SHOW_FRAME, 0);
        assert_eq!((f >> ffi::VP9_PIC_RESET_FRAME_CONTEXT_SHIFT) & 3, 3);
        state.fold(&h);
        assert_eq!(state.color.color_space, ColorSpace::Bt601);
        assert_eq!(state.lf_ref_deltas, [1, 0, -1, -1]);
    }

    #[test]
    fn a_profile_2_key_frame_is_10_bit() {
        let t = Tail { base_q: 60, ..Tail::default() };
        let state = StreamState::new();
        let h = parse_header(&key_frame(2, 64, 64, &t), &[None; 8], &state).expect("parses");
        assert_eq!((h.profile, h.color_config.bit_depth), (2, 10));
        let sp = slice_param(&h, &state, 1);
        assert_eq!(sp.seg_param[0].luma_ac_quant_scale, ac_q(10, 60));
        assert_ne!(ac_q(10, 60), ac_q(8, 60));
        assert_eq!(pic_param(&h, &state, [0; 8]).bit_depth, 10);
    }

    #[test]
    fn show_existing_frame_names_its_slot() {
        let h = parse_header(&[0b1000_1101], &[None; 8], &StreamState::new()).expect("parses");
        assert!(h.show_existing_frame);
        assert_eq!(h.frame_to_show_map_idx, Some(5));
    }

    #[test]
    fn a_broken_header_is_an_error() {
        let state = StreamState::new();
        assert!(parse_header(&[0b0100_0000], &[None; 8], &state).is_err(), "frame marker 1");
        let key = key_frame(0, 64, 64, &Tail::default());
        assert!(parse_header(&key[..6], &[None; 8], &state).is_err(), "truncated");
        let mut bad_sync = key.clone();
        bad_sync[1] ^= 0x10;
        assert!(parse_header(&bad_sync, &[None; 8], &state).is_err(), "sync code");
    }

    #[test]
    fn level_zero_filters_nothing() {
        let t = Tail {
            lf_level: 0,
            segmentation: Some(Seg {
                tree: [255; 7],
                pred: None,
                data: Some((true, &[(0, 1, 20)])),
            }),
            ..Tail::default()
        };
        let mut state = StreamState::new();
        let h = parse_header(&key_frame(0, 64, 64, &t), &[None; 8], &state).expect("parses");
        state.fold(&h);
        assert_eq!(state.filter_levels(&h, 0), [[0; 2]; 4]);
    }

    #[test]
    fn absolute_segment_features_replace_the_frame_values() {
        let t = Tail {
            lf_level: 20,
            base_q: 90,
            segmentation: Some(Seg {
                tree: [255; 7],
                pred: Some([1, 2, 3]),
                data: Some((true, &[(6, 0, 30), (6, 1, 7)])),
            }),
            ..Tail::default()
        };
        let mut state = StreamState::new();
        let h = parse_header(&key_frame(0, 64, 64, &t), &[None; 8], &state).expect("parses");
        state.fold(&h);
        assert_eq!(state.qindex(&h, 6), 30);
        assert_eq!(state.qindex(&h, 0), 90);
        assert_eq!(state.filter_levels(&h, 6), [[7; 2]; 4]);
        assert_eq!(state.pred_probs, [1, 2, 3]);
        assert_ne!(pic_fields(&h) & ffi::VP9_PIC_SEGMENTATION_TEMPORAL_UPDATE, 0);
    }

    #[test]
    fn the_stated_colour_is_matrix_and_range() {
        let cc = ColorConfig {
            bit_depth: 10,
            color_space: ColorSpace::Bt2020,
            color_range_full: true,
            subsampling_x: true,
            subsampling_y: true,
        };
        let c = stated_color(&cc);
        assert_eq!(
            (c.matrix, c.range, c.transfer),
            (Some(Matrix::Bt2020), Some(Range::Full), None)
        );
        assert!(supported(&cc));
        assert!(!supported(&ColorConfig { subsampling_y: false, ..cc }));
        assert!(!supported(&ColorConfig { bit_depth: 12, ..cc }));
    }
}
