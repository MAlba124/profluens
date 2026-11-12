//! `vaapih265enc`, VA-API H.265 encode. NV12/I420 in, Annex-B or HVCC out.
//! IDR + P only, up to [`REFS`] references a P picture, an SPS short-term RPS for each count of
//! them, headers authored here and sent packed.

use std::path::PathBuf;
use std::sync::Arc;

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
use profluens_core::format::{Constraint, ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::bitwriter::{BitWriter, annexb_nal};
use crate::colour;
use crate::enc::{self, EncEngine, FrameBufs, InFormat, PadOut, RawPixFmt, Staged, Stamp};
use crate::encshell;
use crate::encside::{self, Sides};
use crate::error::{VaapiErrorKind as K, Warned, failed};
use crate::ffi;
use crate::gpuframe::{self, GpuFrameChannel, GpuFrameHeader};
use crate::gpuin::{self, GpuIn, Input};
use crate::h264parse;
use crate::probe;
use crate::stats::Stats;
use crate::va::{self, Buffer as VaBuffer, Display, SurfaceFormat};
use pf_hdr::rpu::Fit;
use profluens_video::color::StreamColor;

const FAM_ANNEXB: &str = "h265/annexb";
const FAM_HVCC: &str = "h265/hvcc";

// NAL unit types (H.265 Table 7-1).
const NAL_TRAIL_R: u8 = 1;
const NAL_IDR_W_RADL: u8 = 19;
const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PREFIX_SEI: u8 = 39;
const NAL_PPS: u8 = 34;

static PIXFMT_VALUES: [ValueDesc; 3] =
    [ValueDesc::Id("nv12"), ValueDesc::Id("i420"), ValueDesc::Id("p010")];

static SINK_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: enc::F_WIDTH, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: enc::F_HEIGHT, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: enc::F_PIXFMT,
        allowed: ConstraintDesc::Set(&PIXFMT_VALUES),
        preferred: None,
    },
    FieldDesc { field: enc::F_FPS, allowed: ConstraintDesc::Any, preferred: None },
];
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: enc::RAW_FAMILY, fields: &SINK_FIELDS }];

static OUT_FIELDS: [FieldDesc; 2] = [
    FieldDesc { field: enc::F_WIDTH, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: enc::F_HEIGHT, allowed: ConstraintDesc::Any, preferred: None },
];
static SRC_OFFERS: [OfferDesc; 3] = [
    OfferDesc { family: FAM_ANNEXB, fields: &OUT_FIELDS },
    OfferDesc { family: FAM_HVCC, fields: &OUT_FIELDS },
    OfferDesc::any("bytes"),
];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINKS_GPU: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

static PROPS: [PropDesc; 5] = [
    PropDesc { name: "qp", allowed: Constraint::Any, live: false },
    PropDesc { name: "bitrate", allowed: Constraint::Any, live: true },
    // Keyframe period, 1..=256 so POC lsb never wraps within a GOP.
    PropDesc { name: "gop", allowed: Constraint::Any, live: false },
    PropDesc { name: "force-keyframe", allowed: Constraint::Any, live: true },
    PropDesc { name: "hrd", allowed: Constraint::Any, live: true },
];

// Cold, once per default build.
#[allow(clippy::disallowed_methods)]
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "vaapih265enc",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VaapiH265Enc::new())),
    // not required, the fences (`enc::Fences`) use a reactor when the pipeline has one and the
    // pictures poll on a timer when it has none
    io: false,
    couplings: &[Coupling::same(0, 0, enc::F_WIDTH), Coupling::same(0, 0, enc::F_HEIGHT)],
};

static DESC_GPU: ElementDesc = ElementDesc { sinks: &SINKS_GPU, make_default: None, ..DESC };

fn ten_bit(fmt: InFormat) -> bool {
    fmt.pixfmt == RawPixFmt::P010
}

const LEVELS: [(u8, u32, u64); 8] = [
    (60, 122_880, 3_686_400),      // 2
    (63, 245_760, 7_372_800),      // 2.1
    (90, 552_960, 16_588_800),     // 3
    (93, 983_040, 33_177_600),     // 3.1
    (120, 2_228_224, 66_846_720),  // 4
    (123, 2_228_224, 133_693_440), // 4.1
    (150, 8_912_896, 267_386_880), // 5
    (153, 8_912_896, 534_773_760), // 5.1
];

fn pick_level(coded_w: u32, coded_h: u32, fps: (u32, u32)) -> u8 {
    let ps = (coded_w * coded_h) as u64;
    let fps_int = if fps.0 > 0 { fps.0.div_ceil(fps.1) } else { 30 } as u64;
    let sr = ps * fps_int;
    for (idc, max_ps, max_sr) in LEVELS {
        if ps <= max_ps as u64 && sr <= max_sr {
            return idc;
        }
    }
    156 // 5.2
}

/// References a P picture takes at most, VDEnc's low delay B takes three (iHD 26).
pub const REFS: u32 = 3;

/// The quantizer a session starts at, its PPS's init_qp.
pub const DEFAULT_QP: u8 = 26;

/// The entrypoints a session tries in order, VDEnc first when asked or alone, the shader
/// encoder after it. VDEnc-only parts offer EncSliceLP alone.
fn entrypoints(enc: bool, lp: bool, low_power: bool) -> [Option<ffi::VAEntrypoint>; 2] {
    match (enc, lp && (low_power || !enc)) {
        (true, true) => [Some(ffi::VAEntrypointEncSliceLP), Some(ffi::VAEntrypointEncSlice)],
        (false, true) => [Some(ffi::VAEntrypointEncSliceLP), None],
        (true, false) => [Some(ffi::VAEntrypointEncSlice), None],
        (false, false) => [None, None],
    }
}

/// References a session keeps, [`REFS`] within the driver's L0, and its L1 too where inter
/// pictures are low delay B (both lists the previous pictures).
fn session_refs(r: probe::EncRefs) -> u32 {
    REFS.min(r.l0).min(if r.gpb { r.l1 } else { r.l0 }).max(1)
}

/// The VPS, SPS and PPS (Annex B) a session for `width` x `height` pictures writes, known
/// before one opens, from the probe. `qp`, `brc`, `low_power` as the session has them, `color`
/// the one [`VaapiH265Enc::with_colour`] pins. A session the driver refuses on its first
/// entrypoint writes the next one's. None without a probe or an encoder of the profile.
#[allow(clippy::too_many_arguments)]
pub fn parameter_sets(
    width: u32,
    height: u32,
    fps: (u32, u32),
    ten: bool,
    qp: u8,
    brc: bool,
    low_power: bool,
    color: &StreamColor,
) -> Option<Vec<u8>> {
    let name = if ten { "HEVCMain10" } else { "HEVCMain" };
    let row = probe::probe()?.profiles.iter().find(|p| p.name == name).copied()?;
    let entrypoint = entrypoints(row.enc, row.enc_lp, low_power)[0]?;
    let lp = entrypoint == ffi::VAEntrypointEncSliceLP;
    let refs = session_refs(row.enc_refs[usize::from(lp)]);
    let ctb_log2 = if lp { 6 } else { 5 };
    let (width, height) = (width.max(2), height.max(2));
    let coded = (enc::align_up(width, 16), enc::align_up(height, 16));
    let pixfmt = if ten { RawPixFmt::P010 } else { RawPixFmt::Nv12 };
    let fmt = InFormat { width, height, pixfmt, fps };
    let level_idc = pick_level(coded.0, coded.1, fps);
    let vps = write_vps(level_idc, ten, refs);
    let sps = write_sps(coded, fmt, level_idc, ctb_log2, refs, colour::vui_colour(color));
    Some(concat_nals(&vps, &sps, &write_pps(qp, brc, ctb_log2, refs)))
}

struct Session {
    engine: EncEngine,
    vps_nal: Vec<u8>,
    sps_nal: Vec<u8>,
    pps_nal: Vec<u8>,
    /// VPS, SPS and PPS back to back, what an IDR carries.
    seq_nals: Vec<u8>,
    /// The packed slice header, rewritten each frame.
    slice_header: BitWriter,
    packed_mask: u32,
    gop_pos: u32,
    ctu_count: u32,
    /// CTB size, 32 on EncSlice and 64 on EncSliceLP.
    ctb_log2: u32,
    level_idc: u8,
    ten: bool,
    /// The colour the SPS states.
    color: StreamColor,
    /// The HDR SEI NAL each IDR carries after its parameter sets.
    sei: Option<Vec<u8>>,
    /// Inter pictures are coded as B slices with both lists the previous pictures.
    gpb: bool,
}

/// VA-API H.265 encoder element.
pub struct VaapiH265Enc {
    device: Option<PathBuf>,
    // Field order is drop order, what holds the display's handle goes before it.
    flight: enc::Flight,
    gpu: Option<GpuIn>,
    session: Option<Session>,
    display: Option<Display>,

    qp: u8,
    bitrate_kbps: u32,
    gop_len: u32,
    hrd_ms: u32,
    rc_dirty: bool,
    warned_rc_mode: bool,

    low_power: bool,
    /// The driver's quality level, 1 best to 7 fastest.
    quality: u32,
    in_fmt: Option<InFormat>,
    /// The input's colour, stated in the VUI, its HDR metadata as SEI.
    color: StreamColor,
    hvcc: bool,
    bytes_out: bool,
    bufs: FrameBufs,
    staged: Staged,
    cadence: enc::IntraCadence,
    disabled: bool,
    warned: Warned,
    frames: u64,
    /// The picture size a zero-copy session opens at in `start`.
    ahead: Option<(u32, u32)>,
    /// The colour the VUI states in place of the input's, its HDR metadata still the input's.
    pinned: Option<StreamColor>,
    /// The source picture's size and its quarter turns clockwise to the output, see
    /// [`Self::with_dovi`].
    dovi: Option<((u32, u32), u8)>,
    stats: Arc<Stats>,
    /// Waits for a picture in the driver, the encoder has its own thread.
    blocking: bool,
    fences: enc::Fences,
    reads: enc::Reads,
    sides: Sides,
}

// SAFETY: the element runs on one thread at a time, its group's after preroll, its own when
// Active or its upstream's when Passive. The display moves with it and is never used from two
// threads at once, which libva permits as nothing in it is thread affine.
unsafe impl Send for VaapiH265Enc {}

#[cfg(test)]
impl crate::enc_eos::Stage for VaapiH265Enc {
    fn staged(&mut self) -> &mut Staged {
        &mut self.staged
    }
}

impl Default for VaapiH265Enc {
    fn default() -> Self {
        Self::new()
    }
}

impl VaapiH265Enc {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        VaapiH265Enc {
            device: None,
            flight: enc::Flight::new(1),
            stats: Arc::default(),
            blocking: false,
            fences: enc::Fences::new(),
            reads: enc::Reads::default(),
            gpu: None,
            display: None,
            session: None,
            qp: DEFAULT_QP,
            bitrate_kbps: 0,
            gop_len: 60,
            hrd_ms: 0,
            rc_dirty: false,
            warned_rc_mode: false,
            low_power: true,
            quality: encshell::QUALITY_BALANCED,
            in_fmt: None,
            color: StreamColor::default(),
            hvcc: false,
            bytes_out: false,
            bufs: FrameBufs::new(),
            staged: Staged::new(),
            cadence: enc::IntraCadence::default(),
            disabled: false,
            warned: Warned::default(),
            frames: 0,
            ahead: None,
            pinned: None,
            dovi: None,
            sides: Sides::new(),
        }
    }

    /// `video/gpu` in, NV12 or P010 frames of the producer on `channel`. P010 encodes as Main 10.
    pub fn new_zerocopy(channel: Arc<GpuFrameChannel>) -> Self {
        VaapiH265Enc { gpu: Some(GpuIn::new(channel)), ..Self::new() }
    }

    /// The frame rate of a `video/gpu` input, which its format does not state.
    pub fn with_fps(mut self, num: u32, den: u32) -> Self {
        if let Some(g) = &mut self.gpu {
            g.fps = (num, den.max(1));
        }
        self
    }

    /// Frames of a `video/gpu` input pass at most this often, the rest are dropped.
    pub fn with_max_fps(mut self, num: u32, den: u32) -> Self {
        if let Some(g) = &mut self.gpu {
            g.max_fps = (num, den.max(1));
        }
        self
    }

    /// Constant-QP quantizer (0..51). The `qp` prop overrides.
    pub fn with_qp(mut self, qp: u8) -> Self {
        self.qp = qp.min(51);
        self
    }

    /// Keyframe (IDR) period in frames (1 = all-intra). The `gop` prop overrides.
    pub fn with_gop(mut self, gop: u32) -> Self {
        self.gop_len = gop.clamp(1, 256);
        self
    }

    /// Target bitrate in kbit/s (0 = CQP). The `bitrate` prop overrides.
    pub fn with_bitrate(mut self, kbps: u32) -> Self {
        self.bitrate_kbps = kbps;
        self
    }

    /// HRD buffer depth in ms at the target bitrate (0 = off). The `hrd` prop overrides.
    /// Opens the session for `width` x `height` pictures in `start`, beside the demuxer and the
    /// decoder, instead of on the first picture, at the depth the link names. A first picture of
    /// another size, depth or colour reopens it.
    /// The colour the VUI states whatever the input announces, so a head written before the
    /// session (see [`parameter_sets`]) is the SPS it writes. The SEI keeps the input's HDR
    /// metadata.
    pub fn with_colour(mut self, color: StreamColor) -> Self {
        self.pinned = Some(color);
        self
    }

    /// Writes each picture's Dolby Vision RPU behind its slices as a single layer's, profile 8,
    /// where the VUI states PQ or HLG. `from` is the source picture the RPUs were written for
    /// and `turns` its quarter turns clockwise to the output, what their active area is fitted
    /// by. The stream's configuration record is the caller's to state.
    pub fn with_dovi(mut self, from: (u32, u32), turns: u8) -> Self {
        self.dovi = Some((from, turns));
        self
    }

    pub fn with_ahead(mut self, width: u32, height: u32) -> Self {
        self.ahead = Some((width, height));
        self
    }

    pub fn with_hrd_ms(mut self, ms: u32) -> Self {
        self.hrd_ms = ms.min(10_000);
        self
    }

    /// Pictures kept on the GPU at once, 1 to 4. With more than one the next picture is queued
    /// behind one still coding, so the engine never waits for the CPU between them.
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.flight.set_depth(depth);
        self
    }

    /// Waits for a picture in the driver, in steps that let a seek's flush through, instead of
    /// asking for a timed wake. For an encoder on its own thread (Active), never one on its
    /// producer's (Passive), which would stall the producer.
    pub fn with_blocking_waits(mut self, on: bool) -> Self {
        self.blocking = on;
        self
    }

    /// The encoder's counters, pictures, polls and waits for the GPU.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Encodes on VDEnc (EncSliceLP) where the driver has it, on by default. The shader
    /// encoder (EncSlice) loads the render engine past what a 1080p60 stream leaves free.
    pub fn with_low_power(mut self, on: bool) -> Self {
        self.low_power = on;
        self
    }

    /// The driver's quality level, 1 best to 7 fastest, VPL's balanced 4 without one. Takes
    /// effect from the next IDR.
    pub fn with_quality_level(mut self, level: u32) -> Self {
        self.quality = level.clamp(1, 7);
        self
    }

    fn warn(&mut self, ctx: &mut Ctx, kind: K) {
        if !self.warned.note(ctx, kind) {
            return;
        }
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: failed(ctx, DESC.name, kind) });
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

    fn ensure_session(&mut self, ctx: &mut Ctx, fmt: InFormat) -> bool {
        if let Some(s) = &self.session {
            if s.engine.width == fmt.width
                && s.engine.height == fmt.height
                && s.ten == ten_bit(fmt)
                && s.color == self.color
            {
                return true;
            }
            log!(
                &*ctx,
                Level::Debug,
                "reinit",
                width = fmt.width,
                height = fmt.height,
                ten_bit = ten_bit(fmt)
            );
            self.reads.tell(ctx, &self.fences, true);
            self.fences.close(ctx);
            self.session = None;
        }
        let Some(display) = &self.display else {
            return false;
        };
        if !fmt.width.is_multiple_of(2) || !fmt.height.is_multiple_of(2) {
            // Conformance-window offsets count 2-sample units for 4:2:0 (7.4.3.2.1).
            self.warn(ctx, K::OddDimensions { width: fmt.width, height: fmt.height });
            return false;
        }

        let ten = ten_bit(fmt);
        let (profile, name) = match ten {
            true => (ffi::VAProfileHEVCMain10, "HEVCMain10"),
            false => (ffi::VAProfileHEVCMain, "HEVCMain"),
        };
        let row =
            probe::probe().and_then(|caps| caps.profiles.iter().find(|p| p.name == name).copied());
        let (enc, lp) = row.map_or((true, false), |p| (p.enc, p.enc_lp));
        // the shader encoder after VDEnc, VDEnc refuses a small picture
        let order = entrypoints(enc, lp, self.low_power);
        if order[0].is_none() {
            self.warn(ctx, K::NoEncodeEntrypoint { profile: name });
            return false;
        }
        let rc_mode = if self.bitrate_kbps > 0 { ffi::VA_RC_VBR } else { ffi::VA_RC_CQP };
        let (mut opened, mut refused) = (None, None);
        for entrypoint in order.into_iter().flatten() {
            let packed_mask = va::config_attribute(
                display,
                profile,
                entrypoint,
                ffi::VAConfigAttribEncPackedHeaders,
            )
            .ok()
            .flatten()
            .unwrap_or(0);
            // a driver that takes no P slice wants low delay B, both lists the previous pictures
            // (as ffmpeg's p_to_gpb)
            let r = probe::EncRefs::query(display, profile, entrypoint);
            let (gpb, refs) = (r.gpb, session_refs(r));
            // Aligned to 16, a multiple of MinCbSizeY (7.4.3.2.1). Partial edge CTBs are legal.
            let engine = EncEngine::new_packed(
                display,
                profile,
                entrypoint,
                rc_mode,
                fmt.pixfmt.surface(),
                fmt.width,
                fmt.height,
                16,
                packed_mask,
                refs as usize,
            );
            match engine {
                Ok(e) => {
                    opened = Some((e, entrypoint, packed_mask, gpb));
                    break;
                }
                Err(e) => refused = Some(e),
            }
        }
        let Some((engine, entrypoint, packed_mask, gpb)) = opened else {
            if let Some(e) = refused {
                self.warn(ctx, K::SessionSetup(e));
            }
            return false;
        };
        if let Some(e) = refused {
            log!(&*ctx, Level::Info, "low_power_refused", status = e.status);
        }
        log!(&*ctx, Level::Debug, "packed_headers", mask = packed_mask);
        let refs = engine.refs() as u32;
        let ctb_log2 = if entrypoint == ffi::VAEntrypointEncSliceLP { 6 } else { 5 };

        let level_idc = pick_level(engine.coded_w, engine.coded_h, fmt.fps);
        let brc = self.bitrate_kbps > 0;
        let vps_nal = write_vps(level_idc, ten, refs);
        let vui = self.pinned.as_ref().unwrap_or(&self.color);
        let fit = self.dovi.map(|(from, turns)| Fit { from, turns, to: (fmt.width, fmt.height) });
        self.sides.rule = encside::keeps(vui, fit);
        let colour = colour::vui_colour(vui);
        let sps_nal = write_sps(engine.coded(), fmt, level_idc, ctb_log2, refs, colour);
        let sei = colour::hdr_sei_rbsp(&self.color.hdr)
            .map(|r| annexb_nal(&nal_header(NAL_PREFIX_SEI), &r));
        let pps_nal = write_pps(self.qp, brc, ctb_log2, refs);
        let ctu_count =
            engine.coded_w.div_ceil(1 << ctb_log2) * engine.coded_h.div_ceil(1 << ctb_log2);
        let (w, h) = (fmt.width, fmt.height);
        log!(
            &*ctx,
            Level::Info,
            "session",
            width = w,
            height = h,
            level = level_idc,
            ten_bit = ten
        );
        let lp = entrypoint == ffi::VAEntrypointEncSliceLP;
        log!(&*ctx, Level::Info, "entrypoint", low_power = lp, gpb = gpb, refs = refs);
        self.stats.made(engine.surfaces_made(), 0);
        let (kbps, qp, gop) = (self.bitrate_kbps, self.qp, self.gop_len);
        log!(
            &*ctx,
            Level::Info,
            "rate_control",
            cqp = !brc,
            bitrate_kbps = kbps,
            qp = qp,
            gop = gop
        );

        self.session = Some(Session {
            engine,
            seq_nals: concat_nals(&vps_nal, &sps_nal, &pps_nal),
            vps_nal,
            sps_nal,
            pps_nal,
            slice_header: BitWriter::new(),
            packed_mask,
            gop_pos: 0,
            ctu_count,
            ctb_log2,
            level_idc,
            ten,
            color: self.color,
            sei,
            gpb,
        });
        if let Some(s) = &self.session {
            self.fences.open(ctx, s.engine.recon_ids().len());
        }
        if self.bytes_out {
            ctx.announce_format(Self::OUT, "bytes", &[]);
        } else {
            let family = if self.hvcc { FAM_HVCC } else { FAM_ANNEXB };
            ctx.announce_format(
                Self::OUT,
                family,
                &[
                    (enc::F_WIDTH, ValueDesc::Int(fmt.width as i64)),
                    (enc::F_HEIGHT, ValueDesc::Int(fmt.height as i64)),
                ],
            );
        }
        if let (true, Some(s)) = (self.hvcc, self.session.as_ref()) {
            let bytes = hvcc_record(&s.vps_nal, &s.sps_nal, &s.pps_nal, s.level_idc, s.ten);
            let stamp = Stamp {
                pts: Timestamp::NONE,
                duration: Timestamp::NONE,
                key: true,
                preview: false,
            };
            self.staged.push(bytes, stamp);
        }
        true
    }

    fn pad(ctx: &mut Ctx) -> PadOut<'_> {
        PadOut { ctx, pad: Self::OUT, name: DESC.name }
    }

    fn drain_pending(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        self.staged.drain(&mut Self::pad(ctx))
    }

    /// Reads the oldest picture in flight once the GPU is done with it and sends it, waiting
    /// for it as `how` says. False while it is still being coded, a wake asked for.
    fn finish(&mut self, ctx: &mut Ctx, how: enc::How) -> Result<bool, Error> {
        let Some(f) = self.flight.front() else {
            return Ok(true);
        };
        let fence = f.fence;
        if how != enc::How::Wait {
            let done = match how {
                // a signalled fence is the answer, the driver is not asked
                enc::How::Poll if fence == enc::Fence::Signalled => Ok(true),
                enc::How::Poll => f.coded.idle(),
                _ => {
                    let t = crate::stats::mark();
                    let done = loop {
                        match f.coded.wait(enc::WAIT_STEP_NS) {
                            Ok(false) if !ctx.command_pending() => {}
                            r => break r,
                        }
                    };
                    self.stats.waited(t);
                    done
                }
            };
            match done {
                Ok(true) => {}
                Ok(false) => {
                    // a fenced picture's completion wakes the encoder, the rest poll
                    if fence != enc::Fence::Pending {
                        self.stats.polled();
                        ctx.wake_within(enc::POLL_WAKE);
                    }
                    return Ok(false);
                }
                // read by the map below, iHD fails every picture after a coded buffer destroyed
                // unread
                Err(e) => self.warn(ctx, K::Encode(e)),
            }
        }
        let Some(f) = self.flight.pop() else {
            return Ok(true);
        };
        // the map waits for the GPU
        let t = crate::stats::mark();
        let map = enc::map_coded(&f.coded);
        if how == enc::How::Wait {
            self.stats.waited(t);
        }
        self.reads.mapped(fence, how, t);
        self.reads.tell(ctx, &self.fences, false);
        self.stats.frame();
        let Ok(map) = map.map_err(|e| self.warn(ctx, K::Encode(e))) else {
            // its buffer goes, the session makes another
            return Ok(true);
        };
        let head: &[u8] = match (f.head, &self.session) {
            (true, Some(s)) => &s.seq_nals,
            _ => &[],
        };
        let sei = self.session.as_ref().and_then(|s| s.sei.as_deref()).filter(|_| f.stamp.key);
        log!(&*ctx, Level::Trace, "encoded", bytes = head.len() + map.len(), key = f.stamp.key);
        self.frames += 1;
        let dropped = self.sides.dropped;
        if self.hvcc || sei.is_some() || self.sides.has(f.seq) {
            let b = &mut self.bufs;
            b.coded.clear();
            b.coded.extend_from_slice(head);
            map.append_to(&mut b.coded);
            self.sides.dress(f.seq, sei, &mut b.coded);
            if self.hvcc {
                b.conv.clear();
                annexb_to_hvcc(&b.coded, &mut b.conv);
                std::mem::swap(&mut b.coded, &mut b.conv);
            }
            self.staged.send_or_stage(&mut Self::pad(ctx), &mut b.coded, f.stamp)?;
        } else {
            self.staged.send_coded(&mut Self::pad(ctx), &[head], &map, f.stamp)?;
        }
        let d = self.sides.dropped;
        if dropped == Default::default() && d != dropped {
            // the first drop alone, the counts go out again at the end of stream
            log!(
                &*ctx,
                Level::Info,
                "side_dropped",
                windows = d.windows,
                malformed = d.malformed,
                room = d.room,
                rpu = d.rpu
            );
        }
        match (map.unmap(), &mut self.session) {
            (Ok(()), Some(s)) => s.engine.put_coded(f.coded),
            (Ok(()), None) => {}
            (Err(e), _) => self.warn(ctx, K::Encode(e)),
        }
        Ok(true)
    }

    /// Reads every picture the GPU is done with, in order, a wake asked for the first it is not.
    fn finish_ready(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.flight.reap(self.session.as_mut().map(|s| &mut s.engine), false);
        while !self.flight.is_empty() && self.finish(ctx, enc::How::Poll)? {}
        Ok(())
    }

    /// With no input left, reads what is done. A full ring on an encoder with its own thread
    /// waits for its oldest picture in the driver, the engine busy with the rest. One with
    /// room keeps it for the next input, which must not wait behind a picture coding, and
    /// polls for the pictures in flight.
    fn finish_idle(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.blocking && self.flight.full() {
            self.finish(ctx, enc::How::Step)?;
            return Ok(());
        }
        self.finish_ready(ctx)
    }

    /// How the ring full waits for the oldest picture.
    fn room(&self) -> enc::How {
        if self.blocking { enc::How::Step } else { enc::How::Poll }
    }

    /// Reads every picture in flight, waiting for each.
    fn finish_all(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        while !self.flight.is_empty() {
            self.finish(ctx, enc::How::Wait)?;
        }
        self.flight.reap(self.session.as_mut().map(|s| &mut s.engine), true);
        Ok(())
    }

    /// Whether the session codes pictures of `fmt` as it is.
    fn session_fits(&self, fmt: InFormat) -> bool {
        self.session.as_ref().is_some_and(|s| {
            s.engine.width == fmt.width
                && s.engine.height == fmt.height
                && s.ten == ten_bit(fmt)
                && s.color == self.color
        })
    }

    /// Hands the picture to the GPU, it goes out on [`Self::finish`]. `frame` is the producer's
    /// picture the GPU reads, held until then, `side` its side records.
    #[allow(clippy::too_many_arguments)]
    fn encode_frame(
        &mut self,
        ctx: &mut Ctx,
        input: Input<'_>,
        frame: Option<crate::gpuframe::GpuFrame>,
        side: &[u8],
        pts: Timestamp,
        duration: Timestamp,
        preview: bool,
        intra: bool,
    ) -> Result<(), Error> {
        let Some(fmt) = self.in_fmt else {
            return Ok(());
        };
        let result = {
            let (Some(display), Some(s)) = (&self.display, &mut self.session) else {
                return Ok(());
            };
            if preview {
                log!(&*ctx, Level::Debug, "forced_key", pts = pts, reason = "preview");
                s.gop_pos = 0;
            }
            // a source intra picture off its keyframe grid is a scene change, a P there smears
            // for frames, a quarter GOP from the last IDR at least
            let cut = self.cadence.note(intra);
            if cut && s.gop_pos >= (self.gop_len / 4).max(1) {
                log!(&*ctx, Level::Debug, "forced_key", pts = pts, reason = "intra");
                s.gop_pos = 0;
            }
            let is_idr = s.gop_pos == 0;
            let rc = RcUpdate { dirty: self.rc_dirty, hrd_ms: self.hrd_ms, quality: self.quality };
            let (qp, kbps, gop) = (self.qp, self.bitrate_kbps, self.gop_len);
            let made = s.engine.coded_made();
            let surfaces = s.engine.surfaces_made();
            let r = encode_one(display, s, fmt, qp, kbps, gop, is_idr, rc, input, &mut self.bufs);
            let made = u64::from(s.engine.coded_made() - made);
            self.stats.coded_bufs.fetch_add(made, std::sync::atomic::Ordering::Relaxed);
            self.stats.made(s.engine.surfaces_made() - surfaces, 0);
            r.map(|q| (is_idr, q))
        };
        let (is_idr, (coded, head)) = match result {
            Ok(v) => v,
            Err(e) => {
                self.warn(ctx, K::Encode(e));
                return Ok(());
            }
        };
        self.rc_dirty = false; // the RC/HRD update rode this frame's submission

        let gop_len = self.gop_len.clamp(1, 256);
        let Some(s) = self.session.as_mut() else {
            return Ok(());
        };
        let (slot, recon, dpy) = (s.engine.recon_slot(), s.engine.recon_cur(), s.engine.dpy());
        s.engine.advance_recon();
        s.gop_pos += 1;
        if preview || s.gop_pos >= gop_len {
            s.gop_pos = 0;
        }
        let (seq, fence) = self.fences.queued(ctx, dpy, recon, slot);
        self.sides.keep(seq, side);
        let stamp = Stamp { pts, duration, key: is_idr, preview };
        self.flight.push(enc::InFlight { coded, head, stamp, _frame: frame, seq, fence });
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct RcUpdate {
    dirty: bool,
    hrd_ms: u32,
    quality: u32,
}

/// Hands one picture to the GPU, its coded buffer and whether the parameter sets go in front.
#[allow(clippy::too_many_arguments)]
fn encode_one(
    display: &Display,
    s: &mut Session,
    fmt: InFormat,
    qp: u8,
    bitrate_kbps: u32,
    gop_len: u32,
    is_idr: bool,
    rc: RcUpdate,
    input: Input<'_>,
    bufs: &mut FrameBufs,
) -> va::VaResult<(VaBuffer, bool)> {
    let surface = match input {
        Input::Raw(data) => s.engine.upload(display, data, fmt.pixfmt)?,
        Input::Surface(surface) => surface,
    };
    let poc = s.gop_pos as i32; // POC == frames since the IDR (8.3.1)
    let brc = bitrate_kbps > 0;
    let coded = s.engine.take_coded(display)?;

    let built = (|| {
        let engine = &s.engine;
        if is_idr {
            let seq = seq_param(engine, fmt, gop_len, bitrate_kbps, s.level_idc, s.ctb_log2);
            bufs.param(display, engine, ffi::VAEncSequenceParameterBufferType, &seq)?;
            // iHD encodes at its fastest target usage (7) without one, VPL's balanced 4 is ours
            bufs.hold.push(encshell::quality_buffer(display, engine, rc.quality)?);
        }
        if brc && (is_idr || rc.dirty) {
            encshell::rc_buffers(display, engine, bitrate_kbps, 51, fmt.fps, &mut bufs.hold)?;
            if rc.hrd_ms > 0 {
                bufs.hold.push(encshell::hrd_buffer(display, engine, bitrate_kbps, rc.hrd_ms)?);
            }
        }
        if is_idr && s.packed_mask & ffi::VA_ENC_PACKED_HEADER_SEQUENCE != 0 {
            // VPS+SPS+PPS as one packed sequence header, the ffmpeg/gst layout.
            let (ty, nals) = (ffi::VAEncPackedHeaderSequence, &s.seq_nals);
            bufs.packed(display, engine, ty, nals, nals.len() as u32 * 8, true)?;
        }
        let pic = pic_param(engine, poc, is_idr, qp, brc, s.ctb_log2, coded.id());
        bufs.param(display, engine, ffi::VAEncPictureParameterBufferType, &pic)?;
        if s.packed_mask & ffi::VA_ENC_PACKED_HEADER_SLICE != 0 {
            let w = &mut s.slice_header;
            w.clear();
            let refs = engine.refs() as u32;
            write_slice_header(w, is_idr, poc, s.gpb, refs_now(engine, poc), refs);
            let ty = ffi::VAEncPackedHeaderSlice;
            bufs.packed(display, engine, ty, w.as_bytes(), w.bit_len(), false)?;
        }
        let slice = slice_param(engine, s.ctu_count, poc, is_idr, s.gpb);
        bufs.param(display, engine, ffi::VAEncSliceParameterBufferType, &slice)
    })();
    let queued = built.and_then(|()| bufs.queue(&s.engine, surface));
    bufs.hold.clear();
    if let Err(e) = queued {
        // read out and kept, not destroyed unread, the next picture writes it over
        enc::discard_coded(Some(&mut s.engine), coded);
        return Err(e);
    }
    Ok((coded, is_idr && s.packed_mask & ffi::VA_ENC_PACKED_HEADER_SEQUENCE == 0))
}

// Cold, once per session.
#[allow(clippy::disallowed_methods)]
fn concat_nals(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(vps.len() + sps.len() + pps.len());
    v.extend_from_slice(vps);
    v.extend_from_slice(sps);
    v.extend_from_slice(pps);
    v
}

fn seq_param(
    engine: &EncEngine,
    fmt: InFormat,
    gop: u32,
    bitrate_kbps: u32,
    level_idc: u8,
    ctb_log2: u32,
) -> ffi::VAEncSequenceParameterBufferHEVC {
    let ten = ten_bit(fmt);
    let depth_minus8 = if ten { 2 } else { 0 };
    let seq_fields: u32 = 1 // chroma_format_idc = 1 (4:2:0)
        | depth_minus8 << 3 // bit_depth_luma_minus8
        | depth_minus8 << 6 // bit_depth_chroma_minus8
        | 1 << 11 // amp_enabled_flag, VDEnc codes asymmetric partitions whatever it is told
        | 1 << 16; // low_delay_seq (decode order == display order)
    ffi::VAEncSequenceParameterBufferHEVC {
        general_profile_idc: if ten { 2 } else { 1 }, // Main 10 or Main
        general_level_idc: level_idc,
        general_tier_flag: 0,
        intra_period: gop,
        intra_idr_period: gop,
        ip_period: 1,
        bits_per_second: bitrate_kbps.saturating_mul(1000),
        pic_width_in_luma_samples: engine.coded_w as u16,
        pic_height_in_luma_samples: engine.coded_h as u16,
        seq_fields,
        log2_min_luma_coding_block_size_minus3: 0, // MinCb 8
        log2_diff_max_min_luma_coding_block_size: (ctb_log2 - 3) as u8,
        log2_min_transform_block_size_minus2: 0,   // min TB 4
        log2_diff_max_min_transform_block_size: 3, // max TB 32
        max_transform_hierarchy_depth_inter: 2,
        max_transform_hierarchy_depth_intra: 2,
        pcm_sample_bit_depth_luma_minus1: 0,
        pcm_sample_bit_depth_chroma_minus1: 0,
        log2_min_pcm_luma_coding_block_size_minus3: 0,
        log2_max_pcm_luma_coding_block_size_minus3: 0,
        vui_parameters_present_flag: 0,
        vui_fields: 0,
        aspect_ratio_idc: 0,
        sar_width: 0,
        sar_height: 0,
        vui_num_units_in_tick: if fmt.fps.0 > 0 { fmt.fps.1 } else { 1 },
        vui_time_scale: if fmt.fps.0 > 0 { fmt.fps.0 } else { 30 },
        min_spatial_segmentation_idc: 0,
        max_bytes_per_pic_denom: 0,
        max_bits_per_min_cu_denom: 0,
        scc_fields: 0,
        va_reserved: [0; ffi::VA_PADDING_MEDIUM - 1],
    }
}

fn hevc_pic(surface: ffi::VASurfaceID, poc: i32, flags: u32) -> ffi::VAPictureHEVC {
    ffi::VAPictureHEVC {
        picture_id: surface,
        pic_order_cnt: poc,
        flags,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

/// diff_cu_qp_delta_depth of 8x8 quantization groups, where the rate control's QP deltas fall.
/// VDEnc's HuC codes them per 8x8 whatever the CTB, a coarser group in the PPS dequantizes its
/// pictures with the wrong QP, worse the more bits (iHD 26.1, VPL writes the same).
fn qg_depth(ctb_log2: u32) -> u8 {
    ctb_log2.saturating_sub(3) as u8
}

/// References the P picture `poc` takes, the pictures since its IDR up to what the engine keeps.
/// 0 for an IDR, whose POC is 0.
fn refs_now(engine: &EncEngine, poc: i32) -> u32 {
    (poc.max(0) as u32).min(engine.refs() as u32)
}

/// The `n` pictures before `poc`, the last coded first, RefPicSetStCurrBefore and the list it
/// makes without modifications (8.3.4).
fn ref_list(engine: &EncEngine, poc: i32, n: u32) -> [ffi::VAPictureHEVC; 15] {
    let mut l = [ffi::VAPictureHEVC::invalid(); 15];
    let flags = ffi::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
    for (i, r) in l.iter_mut().enumerate().take(n as usize) {
        *r = hevc_pic(engine.recon_back(i), poc - 1 - i as i32, flags);
    }
    l
}

fn pic_param(
    engine: &EncEngine,
    poc: i32,
    is_idr: bool,
    qp: u8,
    brc: bool,
    ctb_log2: u32,
    coded_buf: ffi::VABufferID,
) -> ffi::VAEncPictureParameterBufferHEVC {
    let refs = ref_list(engine, poc, if is_idr { 0 } else { refs_now(engine, poc) });
    let default_minus1 = engine.refs().saturating_sub(1) as u8;
    let coding_type: u32 = if is_idr { 1 } else { 2 }; // 1 = I, 2 = P
    let pic_fields: u32 = (is_idr as u32)
        | coding_type << 1
        | 1 << 4 // reference_pic_flag
        | (brc as u32) << 9; // cu_qp_delta_enabled (iHD BRC requirement)
    ffi::VAEncPictureParameterBufferHEVC {
        decoded_curr_pic: hevc_pic(engine.recon_cur(), poc, 0),
        reference_frames: refs,
        coded_buf,
        collocated_ref_pic_index: 0xFF, // no TMVP
        last_picture: 0,
        pic_init_qp: qp,
        diff_cu_qp_delta_depth: if brc { qg_depth(ctb_log2) } else { 0 },
        pps_cb_qp_offset: 0,
        pps_cr_qp_offset: 0,
        num_tile_columns_minus1: 0,
        num_tile_rows_minus1: 0,
        column_width_minus1: [0; 19],
        row_height_minus1: [0; 21],
        log2_parallel_merge_level_minus2: 0,
        ctu_max_bitsize_allowed: 0,
        num_ref_idx_l0_default_active_minus1: default_minus1,
        num_ref_idx_l1_default_active_minus1: default_minus1,
        slice_pic_parameter_set_id: 0,
        nal_unit_type: if is_idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R },
        pic_fields,
        hierarchical_level_plus1: 0,
        va_byte_reserved: 0,
        scc_fields: 0,
        va_reserved: [0; ffi::VA_PADDING_HIGH - 1],
    }
}

fn slice_param(
    engine: &EncEngine,
    ctu_count: u32,
    poc: i32,
    is_idr: bool,
    gpb: bool,
) -> ffi::VAEncSliceParameterBufferHEVC {
    let n = if is_idr { 0 } else { refs_now(engine, poc) };
    let l0 = ref_list(engine, poc, n);
    let l1 = if gpb { l0 } else { [ffi::VAPictureHEVC::invalid(); 15] };
    let minus1 = n.saturating_sub(1) as u8;
    let slice_fields: u32 = 1; // last_slice_of_pic (single slice per picture)
    ffi::VAEncSliceParameterBufferHEVC {
        slice_segment_address: 0,
        num_ctu_in_slice: ctu_count,
        slice_type: slice_type(is_idr, gpb),
        slice_pic_parameter_set_id: 0,
        num_ref_idx_l0_active_minus1: minus1,
        num_ref_idx_l1_active_minus1: if gpb { minus1 } else { 0 },
        ref_pic_list0: l0,
        ref_pic_list1: l1,
        luma_log2_weight_denom: 0,
        delta_chroma_log2_weight_denom: 0,
        delta_luma_weight_l0: [0; 15],
        luma_offset_l0: [0; 15],
        delta_chroma_weight_l0: [[0; 2]; 15],
        chroma_offset_l0: [[0; 2]; 15],
        delta_luma_weight_l1: [0; 15],
        luma_offset_l1: [0; 15],
        delta_chroma_weight_l1: [[0; 2]; 15],
        chroma_offset_l1: [[0; 2]; 15],
        max_num_merge_cand: 5,
        slice_qp_delta: 0,
        slice_cb_qp_offset: 0,
        slice_cr_qp_offset: 0,
        slice_beta_offset_div2: 0,
        slice_tc_offset_div2: 0,
        slice_fields,
        pred_weight_table_bit_offset: 0,
        pred_weight_table_bit_length: 0,
        va_reserved: [0; ffi::VA_PADDING_MEDIUM - 2],
    }
}

fn nal_header(nal_type: u8) -> [u8; 2] {
    [nal_type << 1, 1]
}

fn write_ptl(w: &mut BitWriter, level_idc: u8, ten: bool) {
    w.u(0, 2); // general_profile_space
    w.flag(false); // general_tier_flag
    w.u(if ten { 2 } else { 1 }, 5); // general_profile_idc = Main 10 or Main
    // Main 10 decoders decode Main (A.3.2), so a Main stream flags both.
    w.u(if ten { 0x2000_0000 } else { 0x6000_0000 }, 32);
    w.flag(true); // general_progressive_source_flag
    w.flag(false); // general_interlaced_source_flag
    w.flag(false); // general_non_packed_constraint_flag
    w.flag(true); // general_frame_only_constraint_flag
    w.u(0, 22); // general_reserved_zero_43bits (22 + 21)
    w.u(0, 21);
    w.flag(false); // general_inbld_flag (reserved)
    w.u(level_idc as u32, 8); // general_level_idc
}

fn write_vps(level_idc: u8, ten: bool, refs: u32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(0, 4); // vps_video_parameter_set_id
    w.flag(true); // vps_base_layer_internal_flag
    w.flag(true); // vps_base_layer_available_flag
    w.u(0, 6); // vps_max_layers_minus1
    w.u(0, 3); // vps_max_sub_layers_minus1
    w.flag(true); // vps_temporal_id_nesting_flag
    w.u(0xFFFF, 16); // vps_reserved_0xffff_16bits
    write_ptl(&mut w, level_idc, ten);
    w.flag(false); // vps_sub_layer_ordering_info_present_flag
    w.ue(refs); // vps_max_dec_pic_buffering_minus1[0], the references and the current picture
    w.ue(0); // vps_max_num_reorder_pics[0]
    w.ue(0); // vps_max_latency_increase_plus1[0]
    w.u(0, 6); // vps_max_layer_id
    w.ue(0); // vps_num_layer_sets_minus1
    w.flag(false); // vps_timing_info_present_flag
    w.flag(false); // vps_extension_flag
    w.rbsp_trailing_bits();
    annexb_nal(&nal_header(NAL_VPS), &w.into_bytes())
}

/// `colour` is the VUI's full range flag, primaries, transfer and matrix, see
/// [`colour::vui_colour`].
/// `coded` is the coded width and height, `fmt` the picture's.
fn write_sps(
    coded: (u32, u32),
    fmt: InFormat,
    level_idc: u8,
    ctb_log2: u32,
    refs: u32,
    colour: Option<(bool, u8, u8, u8)>,
) -> Vec<u8> {
    let crop_r = (coded.0 - fmt.width) / 2;
    let crop_b = (coded.1 - fmt.height) / 2;
    let mut w = BitWriter::new();
    w.u(0, 4); // sps_video_parameter_set_id
    w.u(0, 3); // sps_max_sub_layers_minus1
    w.flag(true); // sps_temporal_id_nesting_flag
    write_ptl(&mut w, level_idc, ten_bit(fmt));
    w.ue(0); // sps_seq_parameter_set_id
    w.ue(1); // chroma_format_idc (4:2:0)
    w.ue(coded.0); // pic_width_in_luma_samples
    w.ue(coded.1); // pic_height_in_luma_samples
    let cropping = crop_r > 0 || crop_b > 0;
    w.flag(cropping); // conformance_window_flag
    if cropping {
        w.ue(0); // conf_win_left_offset
        w.ue(crop_r); // conf_win_right_offset (already in SubWidthC units)
        w.ue(0); // conf_win_top_offset
        w.ue(crop_b); // conf_win_bottom_offset (already in SubHeightC units)
    }
    let depth_minus8 = if ten_bit(fmt) { 2 } else { 0 };
    w.ue(depth_minus8); // bit_depth_luma_minus8
    w.ue(depth_minus8); // bit_depth_chroma_minus8
    w.ue(4); // log2_max_pic_order_cnt_lsb_minus4 -> MaxPicOrderCntLsb 256
    w.flag(false); // sps_sub_layer_ordering_info_present_flag
    // the references and the current picture, at most 5 of level 4's MaxDpbSize 6 at 1080p (A.4.2)
    w.ue(refs); // sps_max_dec_pic_buffering_minus1
    w.ue(0); // sps_max_num_reorder_pics
    w.ue(0); // sps_max_latency_increase_plus1
    w.ue(0); // log2_min_luma_coding_block_size_minus3 (MinCb 8)
    w.ue(ctb_log2 - 3); // log2_diff_max_min_luma_coding_block_size
    w.ue(0); // log2_min_luma_transform_block_size_minus2 (min TB 4)
    w.ue(3); // log2_diff_max_min_luma_transform_block_size (max TB 32)
    w.ue(2); // max_transform_hierarchy_depth_inter
    w.ue(2); // max_transform_hierarchy_depth_intra
    w.flag(false); // scaling_list_enabled_flag
    w.flag(true); // amp_enabled_flag, as the sequence parameters say
    w.flag(false); // sample_adaptive_offset_enabled_flag
    w.flag(false); // pcm_enabled_flag
    // set i holds the i + 1 pictures before, what a P picture i + 1 or more after its IDR takes
    w.ue(refs); // num_short_term_ref_pic_sets
    for i in 0..refs {
        if i != 0 {
            w.flag(false); // inter_ref_pic_set_prediction_flag
        }
        w.ue(i + 1); // num_negative_pics
        w.ue(0); // num_positive_pics
        for _ in 0..=i {
            w.ue(0); // delta_poc_s0_minus1, each one before the last
            w.flag(true); // used_by_curr_pic_s0_flag
        }
    }
    w.flag(false); // long_term_ref_pics_present_flag
    w.flag(false); // sps_temporal_mvp_enabled_flag
    w.flag(false); // strong_intra_smoothing_enabled_flag
    let have_fps = fmt.fps.0 > 0;
    w.flag(have_fps || colour.is_some()); // vui_parameters_present_flag
    if have_fps || colour.is_some() {
        w.flag(false); // aspect_ratio_info_present_flag
        w.flag(false); // overscan_info_present_flag
        w.flag(colour.is_some()); // video_signal_type_present_flag
        if let Some((full, primaries, transfer, matrix)) = colour {
            w.u(5, 3); // video_format, unspecified
            w.flag(full); // video_full_range_flag
            w.flag(true); // colour_description_present_flag
            w.u(u32::from(primaries), 8); // colour_primaries
            w.u(u32::from(transfer), 8); // transfer_characteristics
            w.u(u32::from(matrix), 8); // matrix_coeffs
        }
        w.flag(false); // chroma_loc_info_present_flag
        w.flag(false); // neutral_chroma_indication_flag
        w.flag(false); // field_seq_flag
        w.flag(false); // frame_field_info_present_flag
        w.flag(false); // default_display_window_flag
        w.flag(have_fps); // vui_timing_info_present_flag
        if have_fps {
            w.u(fmt.fps.1, 32); // vui_num_units_in_tick
            w.u(fmt.fps.0, 32); // vui_time_scale
            w.flag(false); // vui_poc_proportional_to_timing_flag
            w.flag(false); // vui_hrd_parameters_present_flag
        }
        w.flag(false); // bitstream_restriction_flag
    }
    w.flag(false); // sps_extension_present_flag
    w.rbsp_trailing_bits();
    annexb_nal(&nal_header(NAL_SPS), &w.into_bytes())
}

fn write_pps(qp: u8, brc: bool, ctb_log2: u32, refs: u32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pps_pic_parameter_set_id
    w.ue(0); // pps_seq_parameter_set_id
    w.flag(false); // dependent_slice_segments_enabled_flag
    w.flag(false); // output_flag_present_flag
    w.u(0, 3); // num_extra_slice_header_bits
    w.flag(false); // sign_data_hiding_enabled_flag
    w.flag(false); // cabac_init_present_flag
    w.ue(refs - 1); // num_ref_idx_l0_default_active_minus1
    w.ue(refs - 1); // num_ref_idx_l1_default_active_minus1
    w.se(qp as i32 - 26); // init_qp_minus26
    w.flag(false); // constrained_intra_pred_flag
    w.flag(false); // transform_skip_enabled_flag
    w.flag(brc); // cu_qp_delta_enabled_flag (driver BRC patches CU QPs)
    if brc {
        w.ue(u32::from(qg_depth(ctb_log2))); // diff_cu_qp_delta_depth
    }
    w.se(0); // pps_cb_qp_offset
    w.se(0); // pps_cr_qp_offset
    w.flag(false); // pps_slice_chroma_qp_offsets_present_flag
    w.flag(false); // weighted_pred_flag
    w.flag(false); // weighted_bipred_flag
    w.flag(false); // transquant_bypass_enabled_flag
    w.flag(false); // tiles_enabled_flag
    w.flag(false); // entropy_coding_sync_enabled_flag
    w.flag(false); // pps_loop_filter_across_slices_enabled_flag
    w.flag(false); // deblocking_filter_control_present_flag
    w.flag(false); // pps_scaling_list_data_present_flag
    w.flag(false); // lists_modification_present_flag
    w.ue(0); // log2_parallel_merge_level_minus2
    w.flag(false); // slice_segment_header_extension_present_flag
    w.flag(false); // pps_extension_present_flag
    w.rbsp_trailing_bits();
    annexb_nal(&nal_header(NAL_PPS), &w.into_bytes())
}

/// 7.4.7.1, 2 I, 1 P, 0 B for a low delay B picture.
fn slice_type(is_idr: bool, gpb: bool) -> u8 {
    match (is_idr, gpb) {
        (true, _) => 2,
        (false, false) => 1,
        (false, true) => 0,
    }
}

/// Slice segment header (7.3.6.1). It ends with byte_alignment(), so bit_length is byte-aligned.
/// A P slice takes `n` references of the `refs` the SPS has an RPS for and the PPS defaults to.
fn write_slice_header(w: &mut BitWriter, is_idr: bool, poc: i32, gpb: bool, n: u32, refs: u32) {
    let nal_type = if is_idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R };
    for b in [0u8, 0, 0, 1, nal_type << 1, 1] {
        w.u(b as u32, 8);
    }
    w.flag(true); // first_slice_segment_in_pic_flag
    if is_idr {
        w.flag(false); // no_output_of_prior_pics_flag (IRAP NAL types 16..23)
    }
    w.ue(0); // slice_pic_parameter_set_id
    w.ue(u32::from(slice_type(is_idr, gpb))); // slice_type
    if !is_idr {
        w.u((poc & 0xFF) as u32, 8); // slice_pic_order_cnt_lsb (log2 max = 8)
        w.flag(true); // short_term_ref_pic_set_sps_flag
        if refs > 1 {
            // u(v) of Ceil(Log2(num_short_term_ref_pic_sets)) bits (7.4.7.1)
            w.u(n - 1, u32::BITS - (refs - 1).leading_zeros()); // short_term_ref_pic_set_idx
        }
        w.flag(n != refs); // num_ref_idx_active_override_flag
        if n != refs {
            w.ue(n - 1); // num_ref_idx_l0_active_minus1
            if gpb {
                w.ue(n - 1); // num_ref_idx_l1_active_minus1
            }
        }
        if gpb {
            // list 1 is RefPicSetStCurrBefore too (8.3.4), the previous pictures
            w.flag(false); // mvd_l1_zero_flag
        }
        w.ue(0); // five_minus_max_num_merge_cand (MaxNumMergeCand = 5)
    }
    w.se(0); // slice_qp_delta
    w.rbsp_trailing_bits(); // identical bit pattern to byte_alignment()
}

fn h265_nal_type(raw: &[u8]) -> u8 {
    (raw[0] >> 1) & 0x3F
}

// Cold, once at announce.
#[allow(clippy::disallowed_methods)]
fn hvcc_record(
    vps_nal: &[u8],
    sps_nal: &[u8],
    pps_nal: &[u8],
    level_idc: u8,
    ten: bool,
) -> Vec<u8> {
    let vps = &vps_nal[4..];
    let sps = &sps_nal[4..];
    let pps = &pps_nal[4..];
    let mut v = Vec::with_capacity(23 + 15 + vps.len() + sps.len() + pps.len());
    v.push(1); // configurationVersion
    v.push(if ten { 2 } else { 1 }); // profile_space(2)=0 | tier(1)=0 | profile_idc(5), Main 10 or Main
    let compat: u32 = if ten { 0x2000_0000 } else { 0x6000_0000 };
    v.extend_from_slice(&compat.to_be_bytes()); // compatibility flags
    v.extend_from_slice(&[0x90, 0, 0, 0, 0, 0]);
    v.push(level_idc); // general_level_idc
    v.extend_from_slice(&[0xF0, 0x00]); // reserved(4)=1111 + min_spatial_segmentation_idc
    v.push(0xFC); // reserved(6) + parallelismType = 0
    v.push(0xFD); // reserved(6) + chromaFormat = 1 (4:2:0)
    let depth_minus8: u8 = if ten { 2 } else { 0 };
    v.push(0xF8 | depth_minus8); // reserved(5) + bitDepthLumaMinus8
    v.push(0xF8 | depth_minus8); // reserved(5) + bitDepthChromaMinus8
    v.extend_from_slice(&[0, 0]); // avgFrameRate = unspecified
    // constantFrameRate=0, numTemporalLayers=1, temporalIdNested=1, lengthSizeMinusOne=3.
    v.push(0x0F);
    v.push(3); // numOfArrays: VPS, SPS, PPS
    for (nal_type, nal) in [(NAL_VPS, vps), (NAL_SPS, sps), (NAL_PPS, pps)] {
        v.push(0x80 | nal_type); // array_completeness=1 + NAL_unit_type
        v.extend_from_slice(&1u16.to_be_bytes()); // numNalus
        v.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        v.extend_from_slice(nal);
    }
    v
}

/// Appends `au` to `out` with 4-byte lengths for start codes, VPS, SPS and PPS dropped.
fn annexb_to_hvcc(au: &[u8], out: &mut Vec<u8>) {
    h264parse::for_each_nal(au, |nal| {
        let raw = nal.raw(au);
        if matches!(h265_nal_type(raw), NAL_VPS | NAL_SPS | NAL_PPS) {
            return;
        }
        out.extend_from_slice(&(raw.len() as u32).to_be_bytes());
        out.extend_from_slice(raw);
    });
}

impl VaapiH265Enc {
    fn process_gpu(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        loop {
            self.drain_pending(ctx)?;
            // a seek's flush waits behind no picture it would discard
            if self.staged.full() || ctx.command_pending() {
                return Ok(());
            }
            // room on the GPU first, the oldest picture read if it is done, else the input
            // waits for the wake and the caller goes on
            if self.flight.full() && !self.finish(ctx, self.room())? {
                return Ok(());
            }
            let Some(inbuf) = inputs.pop() else { break };
            let Some(h) = GpuFrameHeader::from_bytes(inbuf.memory.data()) else {
                self.warn(ctx, K::GpuHeader);
                continue;
            };
            let Some((kind, fps)) = self.gpu.as_ref().and_then(|g| Some((g.link?.0, g.fps))) else {
                self.warn(ctx, K::NoGpuLink);
                continue;
            };
            let pixfmt =
                if kind == crate::vpp::format::P010 { RawPixFmt::P010 } else { RawPixFmt::Nv12 };
            let fmt = InFormat { width: h.disp_w, height: h.disp_h, pixfmt, fps };
            self.in_fmt = Some(fmt);
            let Some(frame) = self.gpu.as_mut().and_then(|g| g.take(h.token)) else {
                self.warn(ctx, K::NoGpuFrame { token: h.token });
                continue;
            };
            // the rate gate, a key the source or a seek asks for passes
            let must = inbuf.flags.contains(BufferFlags::PREVIEW)
                || inbuf.flags.contains(BufferFlags::INTRA);
            if !self.gpu.as_mut().is_some_and(|g| g.keep(inbuf.pts, must)) {
                drop(frame);
                ctx.count_drop();
                continue;
            }
            // a session rebuilt reads its pictures first
            if !self.session_fits(fmt) {
                self.finish_all(ctx)?;
            }
            if !self.ensure_session(ctx, fmt) {
                continue; // warned inside
            }
            // the driver pads a surface past the session's alignment (720 rows come as 736),
            // a frame at least the session's coded size encodes, the picture is the session's
            let coded =
                self.session.as_ref().map_or((0, 0), |s| (s.engine.coded_w, s.engine.coded_h));
            if frame.coded_w < coded.0 || frame.coded_h < coded.1 {
                self.warn(
                    ctx,
                    K::CodedSize { coded: (frame.coded_w, frame.coded_h), session: coded },
                );
                continue;
            }
            let imported = match (&mut self.gpu, &self.display) {
                (Some(g), Some(d)) => g.imported(d, &frame),
                _ => Err(K::NoGpuLink),
            };
            let surface = imported.map_err(|e| failed(ctx, DESC.name, e))?;
            self.encode_frame(
                ctx,
                Input::Surface(surface),
                Some(frame),
                inbuf.memory.side(),
                inbuf.pts,
                inbuf.duration,
                inbuf.flags.contains(BufferFlags::PREVIEW),
                inbuf.flags.contains(BufferFlags::INTRA),
            )?;
            // the GPU codes it while the next picture is prepared, a full ring returns above
        }
        self.drain_pending(ctx)?;
        Ok(())
    }
}

impl Element for VaapiH265Enc {
    fn desc(&self) -> &'static ElementDesc {
        if self.gpu.is_some() { &DESC_GPU } else { &DESC }
    }

    // Cold, once at preroll.
    #[allow(clippy::disallowed_methods)]
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.gpu.is_none() {
            return Ok(());
        }
        let formats = match self.ensure_display(ctx).then_some(()).and(self.display.as_ref()) {
            Some(d) => gpuin::offers(d, &[SurfaceFormat::NV12, SurfaceFormat::P010]),
            None => Vec::new(),
        };
        gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_sink(Self::IN, offers))
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(v)) = ctx.prop("qp") {
            self.qp = v.clamp(0, 51) as u8;
        }
        if let Some(Value::Int(v)) = ctx.prop("bitrate") {
            self.bitrate_kbps = v.clamp(0, 500_000) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("gop") {
            self.gop_len = v.clamp(1, 256) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("hrd") {
            self.hrd_ms = v.clamp(0, 10_000) as u32;
        }
        let family = ctx
            .negotiated_src(Self::OUT)
            .and_then(|f| ctx.family_name(f.family))
            .unwrap_or(FAM_ANNEXB);
        self.hvcc = family == FAM_HVCC;
        self.bytes_out = family == "bytes";
        if let Some(f) = ctx.negotiated_sink(Self::IN) {
            self.color = StreamColor::read(ctx, f);
        }
        if let Some(g) = &mut self.gpu {
            g.start(ctx, ctx.negotiated_sink(Self::IN)).map_err(|e| failed(ctx, DESC.name, e))?;
        }
        let ahead = self.ahead.zip(self.gpu.as_ref().and_then(|g| Some((g.link?.0, g.fps))));
        if let Some(((width, height), (kind, fps))) = ahead {
            let pixfmt =
                if kind == crate::vpp::format::P010 { RawPixFmt::P010 } else { RawPixFmt::Nv12 };
            if self.ensure_display(ctx) {
                self.ensure_session(ctx, InFormat { width, height, pixfmt, fps });
            }
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.staged.resume(&mut Self::pad(ctx))? {
            return Ok(Flow::Ok);
        }
        while let Some(c) = ctx.io().next_completion() {
            if let Some((seq, signalled)) = self.fences.completed(&c) {
                self.flight.unfence(seq, signalled);
            }
        }
        self.finish_ready(ctx)?;
        if !self.ensure_display(ctx) {
            while inputs.pop().is_some() {}
            return Ok(Flow::Ok);
        }
        if self.gpu.is_some() {
            self.process_gpu(ctx, &mut inputs)?;
            self.finish_idle(ctx)?;
            return Ok(Flow::Ok);
        }
        if self.in_fmt.is_none()
            && let Some(f) = ctx.negotiated_sink(Self::IN).cloned()
        {
            self.in_fmt = enc::read_in_format(ctx, &f);
        }
        loop {
            // A dry output pool must not stop input consumption, see vaapih264enc.
            self.drain_pending(ctx)?;
            // a seek's flush waits behind no picture it would discard
            if self.staged.full() || ctx.command_pending() {
                return Ok(Flow::Ok);
            }
            let Some(inbuf) = inputs.pop() else { break };
            let Some(fmt) = self.in_fmt else {
                self.warn(ctx, K::FrameBeforeFormat);
                continue;
            };
            if inbuf.memory.data().len() < fmt.frame_bytes() {
                self.warn(
                    ctx,
                    K::ShortFrame { len: inbuf.memory.data().len(), need: fmt.frame_bytes() },
                );
                continue;
            }
            // every picture out first, a session rebuilt or the one upload surface waits for them
            self.finish_all(ctx)?;
            if !self.ensure_session(ctx, fmt) {
                continue;
            }
            self.encode_frame(
                ctx,
                Input::Raw(inbuf.memory.data()),
                None,
                inbuf.memory.side(),
                inbuf.pts,
                inbuf.duration,
                inbuf.flags.contains(BufferFlags::PREVIEW),
                inbuf.flags.contains(BufferFlags::INTRA),
            )?;
            // the GPU codes it while the caller goes on, it leaves on the next pass
            break;
        }
        self.drain_pending(ctx)?;
        self.finish_idle(ctx)?;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(f) => {
                self.color = StreamColor::read(ctx, f);
                if let Some(fmt) = enc::read_in_format(ctx, f) {
                    log!(
                        &*ctx,
                        Level::Debug,
                        "format_change",
                        width = fmt.width,
                        height = fmt.height
                    );
                    self.in_fmt = Some(fmt);
                }
                if let Some(g) = &mut self.gpu {
                    g.format_change(ctx, f);
                }
            }
            Event::PropChanged { name: "bitrate", value: Value::Int(v) } => {
                let v = *v;
                if self.session.is_some() && self.bitrate_kbps == 0 {
                    if !self.warned_rc_mode {
                        self.warned_rc_mode = true;
                        self.warn(ctx, K::BitrateOnFixedQuantizer);
                    }
                } else if v > 0 {
                    self.bitrate_kbps = v.clamp(1, 500_000) as u32;
                    log!(&*ctx, Level::Debug, "bitrate", kbps = self.bitrate_kbps);
                    self.rc_dirty = true;
                } else {
                    self.warn(ctx, K::BitrateZero);
                }
            }
            Event::PropChanged { name: "hrd", value: Value::Int(v) } => {
                self.hrd_ms = (*v).clamp(0, 10_000) as u32;
                log!(&*ctx, Level::Debug, "hrd", ms = self.hrd_ms);
                self.rc_dirty = true;
            }
            Event::PropChanged { name: "force-keyframe", value: Value::Int(v) } if *v != 0 => {
                log!(
                    &*ctx,
                    Level::Debug,
                    "forced_key",
                    reason = "prop",
                    session = self.session.is_some()
                );
                if let Some(s) = &mut self.session {
                    s.gop_pos = 0; // next frame is an IDR (POC resets with it)
                }
            }
            Event::FlushStart => {
                self.flight.park(self.session.as_mut().map(|s| &mut s.engine));
                self.cadence.reset();
                log!(&*ctx, Level::Debug, "flush", pending = self.staged.len());
                self.warned.summary(ctx, "flush");
                if let Some(g) = &mut self.gpu {
                    g.flush(ctx);
                }
                self.staged.discard();
                if let Some(s) = &mut self.session {
                    s.gop_pos = 0; // post-seek output opens on an IDR
                }
            }
            Event::Eos => {
                self.finish_all(ctx)?;
                let drained = self.staged.finish(&mut Self::pad(ctx))?;
                let pending = self.staged.len();
                let d = self.sides.dropped;
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    frames = self.frames,
                    pending = pending,
                    drained = drained,
                    side_dropped = d.windows + d.malformed + d.room + d.rpu
                );
                self.warned.summary(ctx, "eos");
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        // read and dropped before the session goes
        self.flight.park(None);
        self.flight.reap(None, true);
        self.staged.discard();
        if let Some(g) = &mut self.gpu {
            g.stop();
        }
        self.reads.tell(ctx, &self.fences, true);
        self.fences.close(ctx);
        self.session = None;
        self.display = None;
        self.in_fmt = None;
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::h265parse;

    #[test]
    fn level_ladder_picks_sane_levels() {
        assert_eq!(pick_level(1920, 1088, (30, 1)), 120, "1080p30 → L4");
        assert_eq!(pick_level(1920, 1088, (60, 1)), 123, "1080p60 → L4.1");
        assert_eq!(pick_level(1280, 720, (30, 1)), 93, "720p30 → L3.1");
        assert_eq!(pick_level(320, 240, (30, 1)), 60, "tiny → L2");
    }

    fn parsed_sets(refs: u32) -> (h265parse::Sps, h265parse::Pps) {
        let fmt = InFormat { width: 1920, height: 1080, pixfmt: RawPixFmt::Nv12, fps: (25, 1) };
        let sps = write_sps((1920, 1088), fmt, 120, 6, refs, None);
        let pps = write_pps(26, true, 6, refs);
        let sps = h265parse::parse_sps(&sps[4..]).expect("own SPS parses");
        let pps = h265parse::parse_pps(&pps[4..]).expect("own PPS parses");
        (sps, pps)
    }

    #[test]
    fn the_sps_has_a_reference_set_for_each_count() {
        for refs in 1..=REFS {
            let (sps, pps) = parsed_sets(refs);
            let dpb = sps.sps_max_dec_pic_buffering_minus1;
            assert_eq!(dpb, refs, "the references and the picture");
            assert_eq!(sps.sps_max_num_reorder_pics, 0);
            assert_eq!(sps.num_short_term_ref_pic_sets, refs);
            for (i, rps) in sps.st_rps.iter().enumerate() {
                let want: Vec<i32> = (1..=i as i32 + 1).map(|d| -d).collect();
                assert_eq!(&rps.delta_poc_s0[..], &want[..], "set {i} of {refs}");
                assert!(rps.used_s0.iter().all(|u| *u) && rps.delta_poc_s1.is_empty());
            }
            assert_eq!(pps.num_ref_idx_l0_default_active_minus1, refs - 1);
            assert_eq!(pps.num_ref_idx_l1_default_active_minus1, refs - 1);
            let vps = write_vps(120, false, refs);
            // the VPS's dec_pic_buffering ue sits after the 12 byte PTL, read it back
            let mut rbsp = Vec::new();
            for &b in &vps[6..] {
                if !(b == 3 && rbsp.ends_with(&[0, 0])) {
                    rbsp.push(b);
                }
            }
            let mut r = h264parse::BitReader::new(&rbsp);
            r.u(32);
            for _ in 0..12 {
                r.u(8);
            }
            r.flag(); // vps_sub_layer_ordering_info_present_flag
            assert_eq!(r.ue(), refs, "vps_max_dec_pic_buffering_minus1");
        }
    }

    #[test]
    fn slice_headers_take_the_set_of_the_pictures_since_the_idr() {
        let mut w = BitWriter::new();
        for refs in 1..=REFS {
            let (sps, pps) = parsed_sets(refs);
            for gpb in [false, true] {
                for poc in 0..6i32 {
                    let idr = poc == 0;
                    let n = (poc as u32).min(refs);
                    w.clear();
                    write_slice_header(&mut w, idr, poc, gpb, n, refs);
                    let ty = if idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R };
                    let nal = &w.as_bytes()[4..];
                    let sh = h265parse::parse_slice_header(nal, ty, &sps, &pps).expect("parses");
                    let at = format!("refs {refs} gpb {gpb} poc {poc}");
                    assert_eq!(sh.slice_data_byte_offset, w.bit_len() / 8 - 4, "{at}");
                    if idr {
                        continue;
                    }
                    let want = if gpb { h265parse::SliceType::B } else { h265parse::SliceType::P };
                    assert!(sh.slice_type == want, "{at}");
                    assert_eq!(sh.pic_order_cnt_lsb, poc as u32, "{at}");
                    let want: Vec<i32> = (1..=n as i32).map(|d| -d).collect();
                    assert_eq!(&sh.st_rps.delta_poc_s0[..], &want[..], "{at}");
                    assert_eq!(sh.num_ref_idx_l0_active_minus1 + 1, n, "{at}");
                    if gpb {
                        assert_eq!(sh.num_ref_idx_l1_active_minus1 + 1, n, "{at}");
                    }
                }
            }
        }
    }

    #[test]
    fn hvcc_record_shape() {
        let vps = [0u8, 0, 0, 1, 0x40, 0x01, 0xAA];
        let sps = [0u8, 0, 0, 1, 0x42, 0x01, 0xBB, 0xCC];
        let pps = [0u8, 0, 0, 1, 0x44, 0x01, 0xDD];
        let rec = hvcc_record(&vps, &sps, &pps, 120, false);
        assert_eq!(rec[0], 1, "configurationVersion");
        assert_eq!(rec[1], 1, "Main profile");
        assert_eq!(rec[12], 120, "level (after 1+1+4+6 header bytes)");
        assert_eq!(rec[22], 3, "three arrays");
        assert_eq!(rec[23], 0x80 | 32, "VPS array header");
        let vps_len = u16::from_be_bytes([rec[26], rec[27]]) as usize;
        assert_eq!(vps_len, 3, "VPS NAL length");
        assert_eq!(&rec[28..28 + vps_len], &vps[4..]);
    }

    #[test]
    fn h265_nal_types_read_from_two_byte_header() {
        assert_eq!(h265_nal_type(&[32 << 1, 1]), 32);
        assert_eq!(h265_nal_type(&[19 << 1, 1]), 19);
        assert_eq!(h265_nal_type(&[1 << 1, 1]), 1);
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_preview_and_the_picture_after_it_are_both_keys() {
        let test = "a_preview_and_the_picture_after_it_are_both_keys";
        let Some(caps) = crate::probe() else {
            return eprintln!("skip {test}: no VA-API device");
        };
        if !caps.supports_encode("h265/annexb") {
            return eprintln!("skip {test}: no HEVC encode entrypoint");
        }
        let (w, h) = (128u32, 64u32);
        let mut hx = pf_harness::Harness::with_pool(VaapiH265Enc::new(), 256 * 1024, 16);
        hx.fix_sink_format(
            VaapiH265Enc::IN,
            "video/raw",
            &[
                ("width", ValueDesc::Int(w.into())),
                ("height", ValueDesc::Int(h.into())),
                ("pixfmt", ValueDesc::Id("nv12")),
                ("fps", ValueDesc::Rat(25, 1)),
            ],
        );
        for i in 0..6u64 {
            let frame: Vec<u8> = (0..(w * h * 3 / 2) as usize)
                .map(|p| ((p as u64 + i * 7) % 220 + 16) as u8)
                .collect();
            let mut b = hx.alloc(&frame);
            b.pts = Timestamp::from_nanos(i * 40_000_000);
            if i == 0 {
                b.flags = BufferFlags::PREVIEW;
            }
            hx.push(b).expect("pushed");
        }
        // the last picture leaves with the end, the GPU had it in flight
        let out = hx.eos().expect("the end");
        assert_eq!(out.len(), 6, "every picture coded");
        let key = |i: usize| out[i].flags.contains(BufferFlags::KEYFRAME);
        let preview = |i: usize| out[i].flags.contains(BufferFlags::PREVIEW);
        assert!(key(0) && preview(0), "the preview is a key and says so, {:?}", out[0].flags);
        assert!(key(1) && !preview(1), "the run opens on a key of its own, {:?}", out[1].flags);
        assert!(!key(2) && !preview(2), "then the gop runs on, {:?}", out[2].flags);
        let idr = |i: usize| {
            let au = out[i].memory.data();
            h264parse::split_nals(au).iter().any(|n| matches!(h265_nal_type(n.raw(au)), 19 | 20))
        };
        assert!(idr(0) && idr(1) && !idr(2), "the flags match the slices");
    }

    /// A flush while a picture is on the GPU, the pictures after it code clean and open on a
    /// key. iHD 26.1 fails every picture after a coded buffer destroyed unread.
    #[test]
    fn the_pictures_after_a_flush_with_one_in_flight_code_clean() {
        let test = "the_pictures_after_a_flush_with_one_in_flight_code_clean";
        let Some(caps) = crate::probe() else {
            return eprintln!("skip {test}: no VA-API device");
        };
        if !caps.supports_encode("h265/annexb") {
            return eprintln!("skip {test}: no encode entrypoint");
        }
        let (w, h) = (128u32, 64u32);
        let mut hx =
            pf_harness::Harness::with_pool(VaapiH265Enc::new().with_bitrate(500), 256 * 1024, 16);
        hx.fix_sink_format(
            VaapiH265Enc::IN,
            "video/raw",
            &[
                ("width", ValueDesc::Int(w.into())),
                ("height", ValueDesc::Int(h.into())),
                ("pixfmt", ValueDesc::Id("nv12")),
                ("fps", ValueDesc::Rat(25, 1)),
            ],
        );
        let frame = |i: u64| -> Vec<u8> {
            (0..(w * h * 3 / 2) as usize).map(|p| ((p as u64 + i * 7) % 220 + 16) as u8).collect()
        };
        for i in 0..4u64 {
            let mut b = hx.alloc(&frame(i));
            b.pts = Timestamp::from_nanos(i * 40_000_000);
            hx.push(b).expect("pushed");
        }
        while hx.pull(VaapiH265Enc::OUT).is_some() {}
        hx.seek(0, Timestamp::from_nanos(10 * 40_000_000)).expect("the flush");
        for i in 10..16u64 {
            let mut b = hx.alloc(&frame(i));
            b.pts = Timestamp::from_nanos(i * 40_000_000);
            hx.push(b).expect("pushed");
        }
        let out = hx.eos().expect("the end");
        let warned: Vec<String> = hx
            .bus_messages()
            .into_iter()
            .filter_map(|m| match m {
                BusMessage::Warning { error, .. } => Some(format!("{error}")),
                _ => None,
            })
            .collect();
        assert!(warned.is_empty(), "{test} warned {warned:?}");
        assert_eq!(out.len(), 6, "{test} every picture after the flush");
        assert!(out[0].flags.contains(BufferFlags::KEYFRAME), "{test} the run opens on a key");
    }

    struct Coded {
        kinds: Vec<u8>,
        maxscl: Option<u32>,
        rpu: Option<Vec<u8>>,
    }

    /// Each picture coded under `vui` with an HDR10+ record and a two layer RPU on every input,
    /// the source twice the picture's size where `dovi` asks for the RPUs.
    fn coded_with_side(test: &str, vui: StreamColor, dovi: bool) -> Option<Vec<Coded>> {
        use pf_hdr::rpu::{Sample, nal_form, sample};
        use pf_hdr::side::{FrameMeta, Kind, Writer};
        let Some(caps) = crate::probe() else {
            eprintln!("skip {test}: no VA-API device");
            return None;
        };
        if !caps.supports_encode("h265/annexb") {
            eprintln!("skip {test}: no HEVC encode entrypoint");
            return None;
        }
        let (w, h) = (128u32, 64u32);
        let enc = VaapiH265Enc::new().with_gop(2).with_colour(vui);
        let enc = if dovi { enc.with_dovi((2 * w, 2 * h), 0) } else { enc };
        let mut hx = pf_harness::Harness::with_pool(enc, 256 * 1024, 16);
        hx.fix_sink_format(
            VaapiH265Enc::IN,
            "video/raw",
            &[
                ("width", ValueDesc::Int(w.into())),
                ("height", ValueDesc::Int(h.into())),
                ("pixfmt", ValueDesc::Id("nv12")),
                ("fps", ValueDesc::Rat(25, 1)),
            ],
        );
        for i in 0..3u64 {
            let frame: Vec<u8> = (0..(w * h * 3 / 2) as usize)
                .map(|p| ((p as u64 + i * 7) % 220 + 16) as u8)
                .collect();
            let mut b = hx.alloc(&frame);
            b.pts = Timestamp::from_nanos(i * 40_000_000);
            let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
            t35.extend(pf_hdr::hdr10plus::sample_payload([1000 * (i as u32 + 1), 1, 1], 1000));
            let area = Some([0, 0, 20 + 2 * i as u32, 40]);
            let rpu = sample(Sample { nlq: Some(false), area, ..Sample::P8 });
            let mut side = [0u8; 1024];
            let mut rec = Writer::new(&mut side);
            assert!(rec.push(Kind::T35, &t35));
            assert!(rec.push(Kind::DoviRpu, &nal_form(&rpu)));
            assert!(rec.push(Kind::DoviEl, &[]));
            let n = rec.len();
            b.memory.set_side(&side[..n]).expect("a fresh slot's side area");
            hx.push(b).expect("pushed");
        }
        let out = hx.eos().expect("the end");
        assert_eq!(out.len(), 3, "{test} every picture coded");
        let read = |au: &[u8]| {
            let kinds = pf_hdr::h26x::annexb_nals(au).map(|n| n[0] >> 1 & 0x3F).collect();
            let (mut side, mut scratch) = ([0u8; 2048], [0u8; 2048]);
            let mut scan = pf_hdr::h26x::Scan::new(Writer::new(&mut side));
            pf_hdr::h26x::scan_annexb(pf_hdr::h26x::Codec::H265, au, &mut scratch, &mut scan);
            let n = scan.side.len();
            let m = FrameMeta::read(&side[..n]).expect("records");
            assert!(!m.dovi_el(), "{test} no enhancement layer");
            Coded {
                kinds,
                maxscl: m.hdr10plus().and_then(|h| Some(h.ok()?.windows[0].maxscl[0])),
                rpu: m.dovi_rpu().map(|(_, b)| b.to_vec()),
            }
        };
        Some(out.iter().map(|b| read(b.memory.data())).collect())
    }

    fn pq() -> StreamColor {
        StreamColor {
            transfer: Some(profluens_video::color::TransferFn::Pq),
            primaries: Some(profluens_video::color::Primaries::Bt2020),
            ..StreamColor::default()
        }
    }

    #[test]
    fn each_picture_carries_its_own_hdr10plus_before_its_slices_under_pq() {
        let test = "each_picture_carries_its_own_hdr10plus_before_its_slices_under_pq";
        let Some(out) = coded_with_side(test, pq(), false) else { return };
        for (i, c) in out.iter().enumerate() {
            let kinds = &c.kinds;
            assert_eq!(c.maxscl, Some(1000 * (i as u32 + 1)), "{test} picture {i} its own");
            let sei = kinds.iter().position(|&k| k == NAL_PREFIX_SEI).expect("an SEI");
            let slice = kinds.iter().position(|&k| k < 32).expect("a slice");
            assert!(sei < slice, "{test} picture {i} {kinds:?}");
            assert!(kinds[..sei].iter().all(|&k| (32..=34).contains(&k)), "{test} {kinds:?}");
            assert_eq!(c.rpu, None, "{test} no RPU where none is asked for");
        }
    }

    #[test]
    fn each_picture_carries_its_own_rpu_behind_its_slices_as_a_single_layers() {
        use pf_hdr::rpu::{Sample, nal_form, sample};
        let test = "each_picture_carries_its_own_rpu_behind_its_slices_as_a_single_layers";
        let Some(out) = coded_with_side(test, pq(), true) else { return };
        for (i, c) in out.iter().enumerate() {
            let kinds = &c.kinds;
            // the source is twice the picture, so its bars are half
            let area = Some([0, 0, 10 + i as u32, 20]);
            let want = nal_form(&sample(Sample { area, ..Sample::P8 }));
            assert_eq!(c.rpu.as_deref(), Some(&want[..]), "{test} picture {i} its own");
            assert_eq!(kinds.last(), Some(&pf_hdr::h26x::H265_DOVI_RPU), "{test} {kinds:?}");
            assert_eq!(kinds.iter().filter(|&&k| k >= 62).count(), 1, "{test} {kinds:?}");
            assert_eq!(c.maxscl, Some(1000 * (i as u32 + 1)), "{test} HDR10+ beside it");
        }
    }

    #[test]
    fn an_sdr_session_writes_no_side_records() {
        let test = "an_sdr_session_writes_no_side_records";
        let sdr = StreamColor {
            transfer: Some(profluens_video::color::TransferFn::Bt709),
            ..StreamColor::default()
        };
        let Some(out) = coded_with_side(test, sdr, true) else { return };
        for c in out {
            assert_eq!((c.maxscl, c.rpu), (None, None), "{test}");
            assert!(!c.kinds.contains(&NAL_PREFIX_SEI), "{test} {:?}", c.kinds);
            assert!(c.kinds.iter().all(|&k| k < 62), "{test} {:?}", c.kinds);
        }
    }
}
