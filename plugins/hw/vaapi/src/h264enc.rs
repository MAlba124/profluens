//! `vaapih264enc`, VA-API H.264 encode. NV12/I420 in, Annex-B or AVCC out.
//! IDR + P only, no reordering, up to [`REFS`] references a P picture, headers authored here and
//! sent as packed headers.

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
use profluens_video::color::StreamColor;
use profluens_video::keys::keyed;

use crate::bitwriter::{BitWriter, annexb_nal};
use crate::colour;
use crate::enc::{self, EncEngine, FrameBufs, InFormat, PadOut, RawPixFmt, Staged, Stamp};
use crate::encshell;
use crate::error::{VaapiErrorKind as K, Warned, failed};
use crate::ffi;
use crate::gpuframe::{self, GpuFrameChannel, GpuFrameHeader};
use crate::gpuin::{self, GpuIn, Input};
use crate::h264parse;
use crate::probe;
use crate::va::{self, Buffer as VaBuffer, Display, SurfaceFormat};

const FAM_ANNEXB: &str = "h264/annexb";
const FAM_AVCC: &str = "h264/avcc";

static PIXFMT_VALUES: [ValueDesc; 2] = [ValueDesc::Id("nv12"), ValueDesc::Id("i420")];

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
    OfferDesc { family: FAM_AVCC, fields: &OUT_FIELDS },
    OfferDesc::any("bytes"),
];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINKS_GPU: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

static PROPS: [PropDesc; 6] = [
    PropDesc { name: "qp", allowed: Constraint::Any, live: false },
    // Live. The RC mode is fixed at session build, a live change cannot flip CQP and VBR.
    PropDesc { name: "bitrate", allowed: Constraint::Any, live: true },
    PropDesc { name: "gop", allowed: Constraint::Any, live: false },
    PropDesc { name: "low-power", allowed: Constraint::Any, live: false },
    // Live. A nonzero write forces the next frame to an IDR.
    PropDesc { name: "force-keyframe", allowed: Constraint::Any, live: true },
    // HRD buffer size in ms at the current bitrate (0 = driver default). Live.
    PropDesc { name: "hrd", allowed: Constraint::Any, live: true },
];

// Cold, once per default build.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "vaapih264enc",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VaapiH264Enc::new())),
    io: false,
    couplings: &[Coupling::same(0, 0, enc::F_WIDTH), Coupling::same(0, 0, enc::F_HEIGHT)],
};

static DESC_GPU: ElementDesc = ElementDesc { sinks: &SINKS_GPU, make_default: None, ..DESC };

/// level_idc, MaxMBPS, MaxFS and MaxDpbMbs (Table A-1).
const LEVELS: [(u8, u32, u32, u32); 12] = [
    (10, 1_485, 99, 396),
    (11, 3_000, 396, 900),
    (12, 6_000, 396, 2_376),
    (13, 11_880, 396, 2_376),
    (21, 19_800, 792, 4_752),
    (22, 20_250, 1_620, 8_100),
    (30, 40_500, 1_620, 8_100),
    (31, 108_000, 3_600, 18_000),
    (32, 216_000, 5_120, 20_480),
    (41, 245_760, 8_192, 32_768),
    (42, 522_240, 8_704, 34_816),
    (51, 983_040, 36_864, 184_320),
];

fn pick_level(mbs_w: u32, mbs_h: u32, fps: (u32, u32)) -> u8 {
    let fs = mbs_w * mbs_h;
    let fps_int = if fps.0 > 0 { fps.0.div_ceil(fps.1) } else { 30 };
    let mbps = fs.saturating_mul(fps_int);
    for (idc, max_mbps, max_fs, _) in LEVELS {
        if fs <= max_fs && mbps <= max_mbps {
            return idc;
        }
    }
    52
}

/// References a P picture takes at most. VDEnc searches two at the balanced target usage and
/// three at the best (iHD MaxRefIdx0), the shader encoder more.
pub const REFS: u32 = 3;

/// max_num_ref_frames of the SPS, [`REFS`] or what the level's DPB holds of the picture
/// (A.3.1 item h), the same before a session and in it.
fn sps_refs(mbs_w: u32, mbs_h: u32, level: u8) -> u32 {
    let dpb = LEVELS.iter().find(|l| l.0 == level).map_or(184_320, |l| l.3);
    (dpb / (mbs_w * mbs_h).max(1)).clamp(1, REFS)
}

const PROFILE_HIGH: u8 = 100;

/// The quantizer of a session no `qp` was given, also its PPS `pic_init_qp`.
pub const DEFAULT_QP: u8 = 26;

/// The SPS and PPS (Annex B) a session writes, known before a device opens, `color` the one
/// [`VaapiH264Enc::with_colour`] pins.
pub fn parameter_sets(
    width: u32,
    height: u32,
    fps: (u32, u32),
    qp: u8,
    color: &StreamColor,
) -> (Vec<u8>, Vec<u8>) {
    let (coded_w, coded_h) = (enc::align_up(width.max(2), 16), enc::align_up(height.max(2), 16));
    let level = pick_level(coded_w / 16, coded_h / 16, fps);
    let refs = sps_refs(coded_w / 16, coded_h / 16, level);
    let vui = colour::vui_colour(color);
    (write_sps(coded_w, coded_h, width.max(2), height.max(2), fps, level, refs, vui), write_pps(qp))
}

/// profile_idc, constraint flags and level_idc of the SPS, the RFC 6381 `avc1` bytes.
pub fn profile_level(width: u32, height: u32, fps: (u32, u32)) -> [u8; 3] {
    [PROFILE_HIGH, 0, pick_level(width.div_ceil(16), height.div_ceil(16), fps)]
}

struct Session {
    engine: EncEngine,
    sps_nal: Vec<u8>,
    pps_nal: Vec<u8>,
    /// The packed slice header, rewritten each frame.
    slice_header: BitWriter,
    packed_mask: u32,
    gop_pos: u32,
    frame_num: u32,
    idr_pic_id: u16,
    mbs_w: u32,
    mbs_h: u32,
    /// max_num_ref_frames of the SPS.
    sps_refs: u32,
    /// The colour the SPS states.
    color: StreamColor,
}

/// VA-API H.264 encoder element.
pub struct VaapiH264Enc {
    device: Option<PathBuf>,
    // Field order is drop order, what holds the display's handle goes before it.
    inflight: Option<enc::InFlight>,
    gpu: Option<GpuIn>,
    session: Option<Session>,
    display: Option<Display>,

    qp: u8,
    bitrate_kbps: u32,
    gop_len: u32,
    low_power: bool,
    hrd_ms: u32,
    rc_dirty: bool,
    warned_rc_mode: bool,
    keys: Option<(Arc<[i64]>, i64)>,
    /// The stamp of the frame before, for [`keyed`], none after a flush.
    key_prev: Option<i64>,

    in_fmt: Option<InFormat>,
    avcc: bool,
    bytes_out: bool,
    bufs: FrameBufs,
    staged: Staged,
    cadence: enc::IntraCadence,
    disabled: bool,
    warned: Warned,
    frames: u64,
    /// The picture size a zero-copy session opens at in `start`.
    ahead: Option<(u32, u32)>,
    /// The colour the VUI states, the input's unless `pinned`.
    color: StreamColor,
    pinned: bool,
}

// SAFETY: an Active element runs on one dedicated thread, the raw VADisplay never crosses threads.
unsafe impl Send for VaapiH264Enc {}

#[cfg(test)]
impl crate::enc_eos::Stage for VaapiH264Enc {
    fn staged(&mut self) -> &mut Staged {
        &mut self.staged
    }
}

impl Default for VaapiH264Enc {
    fn default() -> Self {
        Self::new()
    }
}

impl VaapiH264Enc {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        VaapiH264Enc {
            device: None,
            inflight: None,
            gpu: None,
            display: None,
            session: None,
            qp: DEFAULT_QP,
            bitrate_kbps: 0,
            gop_len: 60,
            low_power: false,
            hrd_ms: 0,
            rc_dirty: false,
            warned_rc_mode: false,
            keys: None,
            key_prev: None,
            in_fmt: None,
            avcc: false,
            bytes_out: false,
            bufs: FrameBufs::new(),
            staged: Staged::new(),
            cadence: enc::IntraCadence::default(),
            disabled: false,
            warned: Warned::default(),
            frames: 0,
            ahead: None,
            color: StreamColor::default(),
            pinned: false,
        }
    }

    /// `video/gpu` in, the frames of the producer on `channel`.
    pub fn new_zerocopy(channel: Arc<GpuFrameChannel>) -> Self {
        VaapiH264Enc { gpu: Some(GpuIn::new(channel)), ..Self::new() }
    }

    /// The frame rate of a `video/gpu` input, which its format does not state.
    pub fn with_fps(mut self, num: u32, den: u32) -> Self {
        if let Some(g) = &mut self.gpu {
            g.fps = (num, den.max(1));
        }
        self
    }

    /// Constant-QP quantizer (0..51, 7.4.2.2). The `qp` prop overrides.
    pub fn with_qp(mut self, qp: u8) -> Self {
        self.qp = qp.min(51);
        self
    }

    /// Keyframe (IDR) period in frames (1 = all-intra). The `gop` prop overrides.
    pub fn with_gop(mut self, gop: u32) -> Self {
        self.gop_len = gop.clamp(1, 256);
        self
    }

    /// Also force an IDR on every frame within `tol` ns of one of the sorted `keys`.
    pub fn with_keys(mut self, keys: Arc<[i64]>, tol: i64) -> Self {
        self.keys = Some((keys, tol.max(0)));
        self
    }

    /// Target bitrate in kbit/s (0 = CQP). The `bitrate` prop overrides.
    pub fn with_bitrate(mut self, kbps: u32) -> Self {
        self.bitrate_kbps = kbps;
        self
    }

    /// HRD buffer depth in ms at the target bitrate (0 = off). The `hrd` prop overrides.
    pub fn with_hrd_ms(mut self, ms: u32) -> Self {
        self.hrd_ms = ms.min(10_000);
        self
    }

    /// Encodes on VDEnc (EncSliceLP) where the driver has it, the `low-power` prop.
    /// Opens the session for `width` x `height` NV12 pictures in `start`, beside the demuxer and
    /// the decoder, instead of on the first picture. A first picture of another size reopens it.
    /// The colour the VUI states whatever the input announces, so a head written before the
    /// session (see [`parameter_sets`]) is the SPS it writes.
    pub fn with_colour(mut self, color: StreamColor) -> Self {
        (self.color, self.pinned) = (color, true);
        self
    }

    pub fn with_ahead(mut self, width: u32, height: u32) -> Self {
        self.ahead = Some((width, height));
        self
    }

    pub fn with_low_power(mut self, on: bool) -> Self {
        self.low_power = on;
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
            if s.engine.width == fmt.width && s.engine.height == fmt.height && s.color == self.color
            {
                return true;
            }
            log!(&*ctx, Level::Debug, "reinit", width = fmt.width, height = fmt.height);
            self.session = None;
        }
        let Some(display) = &self.display else {
            return false;
        };

        if !fmt.width.is_multiple_of(2) || !fmt.height.is_multiple_of(2) {
            // Frame cropping counts 2-sample units for 4:2:0 (7.4.2.1.1), odd sizes are not
            // representable.
            self.warn(ctx, K::OddDimensions { width: fmt.width, height: fmt.height });
            return false;
        }

        let Some(caps) = probe::probe() else {
            return false;
        };
        let profile = ffi::VAProfileH264High;
        let profile_caps = caps.profiles.iter().find(|p| p.name == "H264High");
        // VDEnc-only parts offer EncSliceLP alone, so it is the fallback as well as the choice.
        let lp = profile_caps.is_some_and(|p| p.enc_lp && (self.low_power || !p.enc));
        if self.low_power && !lp {
            log!(&*ctx, Level::Warn, "low_power_unavailable", fallback = "EncSlice");
        }
        let rc_mode = if self.bitrate_kbps > 0 { ffi::VA_RC_VBR } else { ffi::VA_RC_CQP };
        // VDEnc first when asked, the shader encoder where it refuses the session (Gen9's
        // takes no VBR)
        let has_enc = profile_caps.is_none_or(|p| p.enc);
        let (mbs_w, mbs_h) =
            (enc::align_up(fmt.width, 16) / 16, enc::align_up(fmt.height, 16) / 16);
        let level = pick_level(mbs_w, mbs_h, fmt.fps);
        let sps_refs = sps_refs(mbs_w, mbs_h, level);
        let order = match (lp, has_enc) {
            (true, true) => [Some(ffi::VAEntrypointEncSliceLP), Some(ffi::VAEntrypointEncSlice)],
            (true, false) => [Some(ffi::VAEntrypointEncSliceLP), None],
            (false, _) => [Some(ffi::VAEntrypointEncSlice), None],
        };
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
            let l0 = enc::max_refs(display, profile, entrypoint).0;
            let refs = sps_refs.min(l0).max(1);
            let engine = EncEngine::new_packed(
                display,
                profile,
                entrypoint,
                rc_mode,
                SurfaceFormat::NV12,
                fmt.width,
                fmt.height,
                16, // H.264 codes whole macroblocks (7.4.2.1.1)
                packed_mask,
                refs as usize,
            );
            match engine {
                Ok(e) => {
                    opened = Some((e, entrypoint, packed_mask));
                    break;
                }
                Err(e) => refused = Some(e),
            }
        }
        let Some((engine, entrypoint, packed_mask)) = opened else {
            if let Some(e) = refused {
                self.warn(ctx, K::SessionSetup(e));
            }
            return false;
        };
        if let Some(e) = refused.filter(|_| entrypoint == ffi::VAEntrypointEncSlice) {
            log!(&*ctx, Level::Info, "low_power_refused", status = e.status);
        }
        log!(&*ctx, Level::Debug, "packed_headers", mask = packed_mask);
        let lp = entrypoint == ffi::VAEntrypointEncSliceLP;

        let (w, h, refs) = (engine.width, engine.height, engine.refs() as u32);
        let vui = colour::vui_colour(&self.color);
        let sps_nal =
            write_sps(engine.coded_w, engine.coded_h, w, h, fmt.fps, level, sps_refs, vui);
        let pps_nal = write_pps(self.qp);
        log!(&*ctx, Level::Info, "session", width = w, height = h, level = level, low_power = lp);
        log!(&*ctx, Level::Info, "references", refs = refs, sps_refs = sps_refs);
        let cqp = self.bitrate_kbps == 0;
        let (kbps, qp, gop) = (self.bitrate_kbps, self.qp, self.gop_len);
        log!(
            &*ctx,
            Level::Info,
            "rate_control",
            cqp = cqp,
            bitrate_kbps = kbps,
            qp = qp,
            gop = gop
        );

        self.session = Some(Session {
            engine,
            sps_nal,
            pps_nal,
            slice_header: BitWriter::new(),
            packed_mask,
            gop_pos: 0,
            frame_num: 0,
            // Consecutive IDRs differ in idr_pic_id (7.4.3), across a session rebuild too.
            idr_pic_id: self.session.as_ref().map_or(0, |s| s.idr_pic_id.wrapping_add(1) & 0xF),
            mbs_w,
            mbs_h,
            sps_refs,
            color: self.color,
        });
        if self.bytes_out {
            ctx.announce_format(Self::OUT, "bytes", &[]);
        } else {
            let family = if self.avcc { FAM_AVCC } else { FAM_ANNEXB };
            ctx.announce_format(
                Self::OUT,
                family,
                &[
                    (enc::F_WIDTH, ValueDesc::Int(fmt.width as i64)),
                    (enc::F_HEIGHT, ValueDesc::Int(fmt.height as i64)),
                ],
            );
        }
        if let (true, Some(s)) = (self.avcc, self.session.as_ref()) {
            let stamp = Stamp {
                pts: Timestamp::NONE,
                duration: Timestamp::NONE,
                key: true,
                preview: false,
            };
            self.staged.push(avcc_record(&s.sps_nal, &s.pps_nal), stamp);
        }
        true
    }

    fn pad(ctx: &mut Ctx) -> PadOut<'_> {
        PadOut { ctx, pad: Self::OUT, name: DESC.name }
    }

    fn drain_pending(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        let sent = self.staged.drain(&mut Self::pad(ctx))?;
        if !sent {
            log!(&*ctx, Level::Trace, "out_pool_dry", staged = self.staged.len() as u64);
        }
        Ok(sent)
    }

    /// Reads the picture in flight once the GPU is done with it and sends it, waiting for it
    /// when `wait`. False while it is still being coded, a wake asked for.
    fn finish(&mut self, ctx: &mut Ctx, wait: bool) -> Result<bool, Error> {
        let Some(f) = &self.inflight else {
            return Ok(true);
        };
        if !wait {
            match f.coded.idle() {
                Ok(true) => {}
                Ok(false) => {
                    ctx.wake_within(enc::POLL_WAKE);
                    return Ok(false);
                }
                // read by the map below, iHD fails every picture after a coded buffer destroyed
                // unread
                Err(e) => self.warn(ctx, K::Encode(e)),
            }
        }
        let Some(f) = self.inflight.take() else {
            return Ok(true);
        };
        let b = &mut self.bufs;
        b.coded.clear();
        if let (true, Some(s)) = (f.head, &self.session) {
            b.coded.extend_from_slice(&s.sps_nal);
            b.coded.extend_from_slice(&s.pps_nal);
        }
        // the map waits for the GPU
        if let Err(e) = f.coded.read_coded(&mut b.coded) {
            self.warn(ctx, K::Encode(e));
            return Ok(true);
        }
        if self.avcc {
            b.conv.clear();
            annexb_to_avcc(&b.coded, &mut b.conv);
            std::mem::swap(&mut b.coded, &mut b.conv);
        }
        let len = self.bufs.coded.len() as u64;
        log!(&*ctx, Level::Trace, "encoded", bytes = len, key = f.stamp.key as u64);
        self.frames += 1;
        self.staged.send_or_stage(&mut Self::pad(ctx), &mut self.bufs.coded, f.stamp)?;
        Ok(true)
    }

    /// Hands the picture to the GPU, it goes out on [`Self::finish`]. `frame` is the producer's
    /// picture the GPU reads, held until then.
    #[allow(clippy::too_many_arguments)]
    fn encode_frame(
        &mut self,
        ctx: &mut Ctx,
        input: Input<'_>,
        frame: Option<crate::gpuframe::GpuFrame>,
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
            if preview && s.gop_pos != 0 {
                log!(&*ctx, Level::Debug, "forced_key", pts = pts, reason = "preview");
                s.gop_pos = 0;
                s.idr_pic_id = s.idr_pic_id.wrapping_add(1) & 0xF;
            }
            // a source intra picture off its keyframe grid is a scene change, a P there smears
            // for frames, a quarter GOP from the last IDR at least, never off a key list's grid
            let cut = self.cadence.note(intra);
            if cut && self.keys.is_none() && s.gop_pos >= (self.gop_len / 4).max(1) {
                log!(&*ctx, Level::Debug, "forced_key", pts = pts, reason = "intra");
                s.gop_pos = 0;
                s.idr_pic_id = s.idr_pic_id.wrapping_add(1) & 0xF;
            }
            let prev = self.key_prev;
            if let Some(p) = pts.nanos() {
                self.key_prev = Some(p as i64);
            }
            if let (Some((keys, tol)), Some(p)) = (&self.keys, pts.nanos())
                && s.gop_pos != 0
                && keyed(keys, *tol, prev, p as i64)
            {
                log!(&*ctx, Level::Debug, "forced_key", pts = pts, reason = "key_list");
                s.gop_pos = 0;
                // Consecutive IDRs differ in idr_pic_id (7.4.3).
                s.idr_pic_id = s.idr_pic_id.wrapping_add(1) & 0xF;
            }
            let is_idr = s.gop_pos == 0;
            if is_idr {
                s.frame_num = 0;
            }
            let cfg = RcUpdate { dirty: self.rc_dirty, hrd_ms: self.hrd_ms };
            encode_one(
                display,
                s,
                fmt,
                self.qp,
                self.bitrate_kbps,
                self.gop_len,
                is_idr,
                cfg,
                input,
                &mut self.bufs,
            )
            .map(|q| (is_idr, q))
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
        s.engine.advance_recon();
        s.frame_num += 1;
        s.gop_pos += 1;
        if preview || s.gop_pos >= gop_len {
            s.gop_pos = 0;
            s.idr_pic_id = s.idr_pic_id.wrapping_add(1) & 0xF;
        }
        let stamp = Stamp { pts, duration, key: is_idr, preview };
        self.inflight = Some(enc::InFlight { coded, head, stamp, _frame: frame });
        Ok(())
    }
}

impl VaapiH264Enc {
    fn process_gpu(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        loop {
            self.drain_pending(ctx)?;
            // a seek's flush waits behind no picture it would discard
            if self.staged.full() || ctx.command_pending() {
                return Ok(());
            }
            let Some(inbuf) = inputs.pop() else { break };
            log!(&*ctx, Level::Trace, "input", pts = inbuf.pts);
            let Some(h) = GpuFrameHeader::from_bytes(inbuf.memory.data()) else {
                self.warn(ctx, K::GpuHeader);
                continue;
            };
            let fps = self.gpu.as_ref().map_or((0, 1), |g| g.fps);
            if self
                .gpu
                .as_ref()
                .and_then(|g| g.link)
                .is_some_and(|(k, _)| k != crate::vpp::format::NV12)
            {
                self.warn(ctx, K::GpuLayout { layout: "10-bit" });
                continue;
            }
            let fmt = InFormat { width: h.disp_w, height: h.disp_h, pixfmt: RawPixFmt::Nv12, fps };
            self.in_fmt = Some(fmt);
            let Some(frame) = self.gpu.as_mut().and_then(|g| g.take(h.token)) else {
                self.warn(ctx, K::NoGpuFrame { token: h.token });
                continue;
            };
            // the picture before out first, a session rebuilt or an upload waits for it
            self.finish(ctx, true)?;
            if !self.ensure_session(ctx, fmt) {
                continue; // warned inside
            }
            let coded =
                self.session.as_ref().map_or((0, 0), |s| (s.engine.coded_w, s.engine.coded_h));
            if (frame.coded_w, frame.coded_h) != coded {
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
                inbuf.pts,
                inbuf.duration,
                inbuf.flags.contains(BufferFlags::PREVIEW),
                inbuf.flags.contains(BufferFlags::INTRA),
            )?;
            // the GPU codes it while the caller goes on, the next decode queued behind it
            break;
        }
        self.drain_pending(ctx)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct RcUpdate {
    dirty: bool,
    hrd_ms: u32,
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
        Input::Raw(data) => {
            s.engine.upload(display, data, fmt.pixfmt)?;
            None
        }
        Input::Surface(surface) => Some(surface),
    };
    let ctxva = s.engine.context();
    let coded =
        VaBuffer::new_empty(display, ctxva, ffi::VAEncCodedBufferType, s.engine.coded_buf_size())?;

    let built = (|| {
        let engine = &s.engine;
        if is_idr {
            let seq = seq_param(engine, fmt, gop_len, bitrate_kbps, s.sps_refs);
            bufs.param(display, engine, ffi::VAEncSequenceParameterBufferType, &seq)?;
            // iHD encodes at its fastest target usage (7) without one, VPL's balanced 4 is ours
            bufs.hold.push(encshell::quality_buffer(display, engine, encshell::QUALITY_BALANCED)?);
        }
        // VA accepts misc parameters on any picture, so a live update retargets from the next one.
        if bitrate_kbps > 0 && (is_idr || rc.dirty) {
            encshell::rc_buffers(display, engine, bitrate_kbps, 51, fmt.fps, &mut bufs.hold)?;
            if rc.hrd_ms > 0 {
                bufs.hold.push(encshell::hrd_buffer(display, engine, bitrate_kbps, rc.hrd_ms)?);
            }
        }
        if is_idr {
            for (mask, ty, nal) in [
                (ffi::VA_ENC_PACKED_HEADER_SEQUENCE, ffi::VAEncPackedHeaderSequence, &s.sps_nal),
                (ffi::VA_ENC_PACKED_HEADER_PICTURE, ffi::VAEncPackedHeaderPicture, &s.pps_nal),
            ] {
                if s.packed_mask & mask != 0 {
                    bufs.packed(display, engine, ty, nal, nal.len() as u32 * 8, true)?;
                }
            }
        }
        let pic = pic_param(engine, s.frame_num, is_idr, qp, coded.id());
        bufs.param(display, engine, ffi::VAEncPictureParameterBufferType, &pic)?;
        if s.packed_mask & ffi::VA_ENC_PACKED_HEADER_SLICE != 0 {
            let w = &mut s.slice_header;
            w.clear();
            write_slice_header(w, is_idr, s.frame_num, s.idr_pic_id, refs_now(engine, s.frame_num));
            let ty = ffi::VAEncPackedHeaderSlice;
            bufs.packed(display, engine, ty, w.as_bytes(), w.bit_len(), false)?;
        }
        let slice = slice_param(engine, s, is_idr, qp);
        bufs.param(display, engine, ffi::VAEncSliceParameterBufferType, &slice)
    })();
    let queued = built.and_then(|()| bufs.queue(&s.engine, surface));
    bufs.hold.clear();
    queued?;

    // Without packed headers the coded buffer holds only the slice, SPS/PPS go in front on IDR.
    let packed_seq = s.packed_mask
        & (ffi::VA_ENC_PACKED_HEADER_SEQUENCE | ffi::VA_ENC_PACKED_HEADER_PICTURE)
        != 0;
    Ok((coded, is_idr && !packed_seq))
}

fn seq_param(
    engine: &EncEngine,
    fmt: InFormat,
    gop: u32,
    bitrate_kbps: u32,
    sps_refs: u32,
) -> ffi::VAEncSequenceParameterBufferH264 {
    let seq_fields: u32 = 1 // chroma_format_idc = 1 (4:2:0)
        | 1 << 2  // frame_mbs_only_flag
        | 1 << 5  // direct_8x8_inference_flag
        | 4 << 6  // log2_max_frame_num_minus4 = 4 (MaxFrameNum 256)
        | 4 << 12;
    let crop_r = (engine.coded_w - engine.width) / 2;
    let crop_b = (engine.coded_h - engine.height) / 2;
    ffi::VAEncSequenceParameterBufferH264 {
        seq_parameter_set_id: 0,
        level_idc: pick_level(engine.coded_w / 16, engine.coded_h / 16, fmt.fps),
        intra_period: gop,
        intra_idr_period: gop,
        ip_period: 1,
        bits_per_second: bitrate_kbps.saturating_mul(1000),
        max_num_ref_frames: sps_refs,
        picture_width_in_mbs: (engine.coded_w / 16) as u16,
        picture_height_in_mbs: (engine.coded_h / 16) as u16,
        seq_fields,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        num_ref_frames_in_pic_order_cnt_cycle: 0,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: [0; 256],
        frame_cropping_flag: (crop_r > 0 || crop_b > 0) as u8,
        frame_crop_left_offset: 0,
        frame_crop_right_offset: crop_r,
        frame_crop_top_offset: 0,
        frame_crop_bottom_offset: crop_b,
        vui_parameters_present_flag: 0,
        vui_fields: 0,
        aspect_ratio_idc: 0,
        sar_width: 0,
        sar_height: 0,
        num_units_in_tick: if fmt.fps.0 > 0 { fmt.fps.1 } else { 1 },
        time_scale: if fmt.fps.0 > 0 { fmt.fps.0.saturating_mul(2) } else { 60 },
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

fn ref_pic(surface: ffi::VASurfaceID, frame_num: u32) -> ffi::VAPictureH264 {
    ffi::VAPictureH264 {
        picture_id: surface,
        frame_idx: frame_num,
        flags: ffi::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        TopFieldOrderCnt: (frame_num * 2) as i32,
        BottomFieldOrderCnt: (frame_num * 2) as i32,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

/// References the P picture `frame_num` takes, the pictures since its IDR up to what the engine
/// keeps. 0 for an IDR, whose frame_num is 0.
fn refs_now(engine: &EncEngine, frame_num: u32) -> u32 {
    frame_num.min(engine.refs() as u32)
}

/// The `n` pictures before `frame_num`, the last coded first, the initial RefPicList0 of a P
/// slice (8.2.4.2.1, descending PicNum).
fn ref_list<const N: usize>(engine: &EncEngine, frame_num: u32, n: u32) -> [ffi::VAPictureH264; N] {
    let mut l = [ffi::VAPictureH264::invalid(); N];
    for (i, r) in l.iter_mut().enumerate().take(n as usize) {
        *r = ref_pic(engine.recon_back(i), frame_num - 1 - i as u32);
    }
    l
}

fn pic_param(
    engine: &EncEngine,
    frame_num: u32,
    is_idr: bool,
    qp: u8,
    coded_buf: ffi::VABufferID,
) -> ffi::VAEncPictureParameterBufferH264 {
    let refs = ref_list(engine, frame_num, if is_idr { 0 } else { refs_now(engine, frame_num) });
    let pic_fields: u32 = (is_idr as u32)
        | 1 << 1  // reference_pic_flag = 1 (every frame is a reference here)
        | 1 << 3 // entropy_coding_mode_flag = 1 (CABAC)
        | 1 << 8; // transform_8x8_mode_flag, High's 8x8 transform, as the PPS says
    ffi::VAEncPictureParameterBufferH264 {
        CurrPic: ref_pic(engine.recon_cur(), frame_num),
        ReferenceFrames: refs,
        coded_buf,
        pic_parameter_set_id: 0,
        seq_parameter_set_id: 0,
        last_picture: 0,
        frame_num: frame_num as u16,
        pic_init_qp: qp,
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        chroma_qp_index_offset: 0,
        second_chroma_qp_index_offset: 0,
        pic_fields,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

fn slice_param(
    engine: &EncEngine,
    s: &Session,
    is_idr: bool,
    _qp: u8,
) -> ffi::VAEncSliceParameterBufferH264 {
    let n = if is_idr { 0 } else { refs_now(engine, s.frame_num) };
    ffi::VAEncSliceParameterBufferH264 {
        macroblock_address: 0,
        num_macroblocks: s.mbs_w * s.mbs_h,
        macroblock_info: ffi::VA_INVALID_ID,
        slice_type: if is_idr { 2 } else { 0 },
        pic_parameter_set_id: 0,
        idr_pic_id: s.idr_pic_id,
        pic_order_cnt_lsb: (s.frame_num * 2) as u16,
        delta_pic_order_cnt_bottom: 0,
        delta_pic_order_cnt: [0; 2],
        direct_spatial_mv_pred_flag: 0,
        // the PPS default is one, as the slice header says
        num_ref_idx_active_override_flag: (n > 1) as u8,
        num_ref_idx_l0_active_minus1: n.saturating_sub(1) as u8,
        num_ref_idx_l1_active_minus1: 0,
        RefPicList0: ref_list(engine, s.frame_num, n),
        RefPicList1: [ffi::VAPictureH264::invalid(); 32],
        luma_log2_weight_denom: 0,
        chroma_log2_weight_denom: 0,
        luma_weight_l0_flag: 0,
        luma_weight_l0: [0; 32],
        luma_offset_l0: [0; 32],
        chroma_weight_l0_flag: 0,
        chroma_weight_l0: [[0; 2]; 32],
        chroma_offset_l0: [[0; 2]; 32],
        luma_weight_l1_flag: 0,
        luma_weight_l1: [0; 32],
        luma_offset_l1: [0; 32],
        chroma_weight_l1_flag: 0,
        chroma_weight_l1: [[0; 2]; 32],
        chroma_offset_l1: [[0; 2]; 32],
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        disable_deblocking_filter_idc: 0,
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

#[allow(clippy::too_many_arguments)]
fn write_sps(
    coded_w: u32,
    coded_h: u32,
    width: u32,
    height: u32,
    fps: (u32, u32),
    level: u8,
    refs: u32,
    colour: Option<(bool, u8, u8, u8)>,
) -> Vec<u8> {
    let mbs_w = coded_w / 16;
    let mbs_h = coded_h / 16;
    let crop_r = (coded_w - width) / 2;
    let crop_b = (coded_h - height) / 2;

    let mut w = BitWriter::new();
    w.u(u32::from(PROFILE_HIGH), 8);
    w.u(0, 8); // constraint_set flags + reserved_zero_2bits
    w.u(level as u32, 8);
    w.ue(0); // seq_parameter_set_id
    w.ue(1); // chroma_format_idc = 4:2:0
    w.ue(0); // bit_depth_luma_minus8
    w.ue(0); // bit_depth_chroma_minus8
    w.flag(false); // qpprime_y_zero_transform_bypass_flag
    w.flag(false); // seq_scaling_matrix_present_flag
    w.ue(4); // log2_max_frame_num_minus4 -> MaxFrameNum = 256
    // POC type 0, not 2. Real decoders mishandle type 2, x264 always emits 0.
    w.ue(0); // pic_order_cnt_type
    w.ue(4); // log2_max_pic_order_cnt_lsb_minus4 -> MaxPicOrderCntLsb = 256
    w.ue(refs); // max_num_ref_frames
    w.flag(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(mbs_w - 1); // pic_width_in_mbs_minus1
    w.ue(mbs_h - 1); // pic_height_in_map_units_minus1
    w.flag(true); // frame_mbs_only_flag
    w.flag(true); // direct_8x8_inference_flag
    let cropping = crop_r > 0 || crop_b > 0;
    w.flag(cropping);
    if cropping {
        w.ue(0); // frame_crop_left_offset
        w.ue(crop_r); // frame_crop_right_offset (already in CropUnitX)
        w.ue(0); // frame_crop_top_offset
        w.ue(crop_b); // frame_crop_bottom_offset (already in CropUnitY)
    }
    // bitstream_restriction declares zero reorder depth (E.2.1). Without it a decoder buffers
    // for worst-case reordering, oxideav-h264 among them.
    w.flag(true); // vui_parameters_present_flag
    w.flag(false); // aspect_ratio_info_present_flag
    w.flag(false); // overscan_info_present_flag
    w.flag(colour.is_some()); // video_signal_type_present_flag
    if let Some((full, primaries, transfer, matrix)) = colour {
        w.u(5, 3); // video_format, unspecified
        w.flag(full); // video_full_range_flag
        w.flag(true); // colour_description_present_flag
        w.u(u32::from(primaries), 8); // colour_primaries
        w.u(u32::from(transfer), 8); // transfer_characteristics
        w.u(u32::from(matrix), 8); // matrix_coefficients
    }
    w.flag(false); // chroma_loc_info_present_flag
    let have_fps = fps.0 > 0;
    w.flag(have_fps); // timing_info_present_flag
    if have_fps {
        w.u(fps.1, 32); // num_units_in_tick
        w.u(fps.0.saturating_mul(2), 32); // time_scale, two ticks per frame (E.2.1)
        w.flag(true); // fixed_frame_rate_flag
    }
    w.flag(false); // nal_hrd_parameters_present_flag
    w.flag(false); // vcl_hrd_parameters_present_flag
    w.flag(false); // pic_struct_present_flag
    w.flag(true); // bitstream_restriction_flag
    w.flag(true); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom (unlimited)
    w.ue(0); // max_bits_per_mb_denom (unlimited)
    w.ue(15); // log2_max_mv_length_horizontal
    w.ue(15); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames, decode order == output order
    w.ue(refs); // max_dec_frame_buffering (>= max_num_ref_frames)
    w.rbsp_trailing_bits();
    annexb_nal(&[0x67], &w.into_bytes())
}

fn write_pps(qp: u8) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.flag(true); // entropy_coding_mode_flag = CABAC
    w.flag(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.flag(false); // weighted_pred_flag
    w.u(0, 2); // weighted_bipred_idc
    w.se(qp as i32 - 26); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.flag(false); // deblocking_filter_control_present_flag
    w.flag(false); // constrained_intra_pred_flag
    w.flag(false); // redundant_pic_cnt_present_flag
    // High's tail (7.3.2.2), VPL codes the 8x8 transform too
    w.flag(true); // transform_8x8_mode_flag
    w.flag(false); // pic_scaling_matrix_present_flag
    w.se(0); // second_chroma_qp_index_offset
    w.rbsp_trailing_bits();
    annexb_nal(&[0x68], &w.into_bytes())
}

/// The packed slice header into `w`, its bit length is `w.bit_len()`. A P slice takes `refs`
/// references.
fn write_slice_header(w: &mut BitWriter, is_idr: bool, frame_num: u32, idr_pic_id: u16, refs: u32) {
    // Start code and NAL header count toward bit_length, the driver treats the buffer as literal
    // stream.
    for b in [0u8, 0, 0, 1, if is_idr { 0x65 } else { 0x41 }] {
        w.u(b as u32, 8);
    }
    w.ue(0); // first_mb_in_slice
    w.ue(if is_idr { 7 } else { 5 }); // slice_type (I-all / P-all)
    w.ue(0); // pic_parameter_set_id
    w.u(frame_num & 0xFF, 8); // frame_num, log2_max_frame_num = 8
    if is_idr {
        w.ue(idr_pic_id as u32); // idr_pic_id
    }
    // pic_order_cnt_lsb, 8 bits, 2 per frame. Wrapping is the decoder's job (8.2.1.1).
    w.u((frame_num * 2) & 0xFF, 8);
    if !is_idr {
        w.flag(refs > 1); // num_ref_idx_active_override_flag, the PPS default is one
        if refs > 1 {
            w.ue(refs - 1); // num_ref_idx_l0_active_minus1
        }
        w.flag(false); // ref_pic_list_modification_flag_l0 (7.3.3.1)
    }
    if is_idr {
        w.flag(false); // no_output_of_prior_pics_flag
        w.flag(false); // long_term_reference_flag
    } else {
        w.flag(false); // adaptive_ref_pic_marking_mode_flag (sliding window)
    }
    if !is_idr {
        w.ue(0); // cabac_init_idc (entropy_coding_mode_flag is 1)
    }
    w.se(0); // slice_qp_delta
}

// Cold, once at announce.
#[allow(clippy::disallowed_methods)]
fn avcc_record(sps_nal: &[u8], pps_nal: &[u8]) -> Vec<u8> {
    let sps = &sps_nal[4..];
    let pps = &pps_nal[4..];
    let mut v = Vec::with_capacity(11 + sps.len() + pps.len());
    v.push(1); // configurationVersion
    v.push(sps[1]); // AVCProfileIndication
    v.push(sps[2]); // profile_compatibility
    v.push(sps[3]); // AVCLevelIndication
    v.push(0xFF); // '111111' + lengthSizeMinusOne = 3 (4-byte lengths)
    v.push(0xE1); // '111' + numOfSequenceParameterSets = 1
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1); // numOfPictureParameterSets
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    v
}

/// Appends `au` to `out` with 4-byte lengths for start codes, SPS and PPS dropped.
fn annexb_to_avcc(au: &[u8], out: &mut Vec<u8>) {
    h264parse::for_each_nal(au, |nal| {
        if matches!(nal.unit_type, h264parse::NAL_SPS | h264parse::NAL_PPS) {
            return;
        }
        let raw = nal.raw(au);
        out.extend_from_slice(&(raw.len() as u32).to_be_bytes());
        out.extend_from_slice(raw);
    });
}

impl Element for VaapiH264Enc {
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
            Some(d) => gpuin::offers(d, &[SurfaceFormat::NV12]),
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
        if let Some(Value::Int(v)) = ctx.prop("low-power") {
            self.low_power = v != 0;
        }
        if let Some(Value::Int(v)) = ctx.prop("hrd") {
            self.hrd_ms = v.clamp(0, 10_000) as u32;
        }
        let family = ctx
            .negotiated_src(Self::OUT)
            .and_then(|f| ctx.family_name(f.family))
            .unwrap_or(FAM_ANNEXB);
        self.avcc = family == FAM_AVCC;
        self.bytes_out = family == "bytes";
        if let (false, Some(f)) = (self.pinned, ctx.negotiated_sink(Self::IN)) {
            self.color = StreamColor::read(ctx, f);
        }
        if let Some(g) = &mut self.gpu {
            g.start(ctx, ctx.negotiated_sink(Self::IN)).map_err(|e| failed(ctx, DESC.name, e))?;
        }
        if let (Some((width, height)), Some(g)) = (self.ahead, &self.gpu) {
            let fmt = InFormat { width, height, pixfmt: RawPixFmt::Nv12, fps: g.fps };
            if self.ensure_display(ctx) {
                self.ensure_session(ctx, fmt);
            }
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.staged.resume(&mut Self::pad(ctx))? {
            return Ok(Flow::Ok);
        }
        self.finish(ctx, false)?;
        if !self.ensure_display(ctx) {
            while inputs.pop().is_some() {}
            return Ok(Flow::Ok);
        }
        if self.gpu.is_some() {
            self.process_gpu(ctx, &mut inputs)?;
            self.finish(ctx, false)?;
            return Ok(Flow::Ok);
        }
        if self.in_fmt.is_none()
            && let Some(f) = ctx.negotiated_sink(Self::IN).cloned()
        {
            self.in_fmt = enc::read_in_format(ctx, &f);
        }
        loop {
            // A dry output pool must not stop input consumption. Upstream raw frames pin a pool a
            // downstream muxer may be waiting on (shared-pool deadlock). Bounded by STAGED_MAX.
            self.drain_pending(ctx)?;
            // a seek's flush waits behind no picture it would discard
            if self.staged.full() || ctx.command_pending() {
                return Ok(Flow::Ok); // staged cap, leave input queued upstream
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
            // the picture before out first, a session rebuilt or an upload waits for it
            self.finish(ctx, true)?;
            if !self.ensure_session(ctx, fmt) {
                continue; // warned inside
            }
            self.encode_frame(
                ctx,
                Input::Raw(inbuf.memory.data()),
                None,
                inbuf.pts,
                inbuf.duration,
                inbuf.flags.contains(BufferFlags::PREVIEW),
                inbuf.flags.contains(BufferFlags::INTRA),
            )?;
            // the GPU codes it while the caller goes on, it leaves on the next pass
            break;
        }
        self.drain_pending(ctx)?;
        self.finish(ctx, false)?;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(f) => {
                if !self.pinned {
                    self.color = StreamColor::read(ctx, f);
                }
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
                    s.gop_pos = 0; // next frame opens a new GOP (IDR)
                    // Consecutive IDRs differ in idr_pic_id (7.4.3). The GOP-wrap bump does not run
                    // for a forced IDR.
                    s.idr_pic_id = s.idr_pic_id.wrapping_add(1) & 0xF;
                }
            }
            Event::FlushStart => {
                // read and dropped, iHD fails every picture after a coded buffer destroyed unread
                if let Some(f) = self.inflight.take() {
                    let _ = f.coded.read_coded(&mut self.bufs.coded);
                    self.bufs.coded.clear();
                }
                self.cadence.reset();
                self.key_prev = None;
                log!(&*ctx, Level::Debug, "flush", pending = self.staged.len());
                self.warned.summary(ctx, "flush");
                if let Some(g) = &mut self.gpu {
                    g.flush(ctx);
                }
                self.staged.discard();
                if let Some(s) = &mut self.session {
                    s.gop_pos = 0;
                    s.idr_pic_id = s.idr_pic_id.wrapping_add(1) & 0xF;
                }
            }
            Event::Eos => {
                self.finish(ctx, true)?;
                let drained = self.staged.finish(&mut Self::pad(ctx))?;
                let pending = self.staged.len();
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    frames = self.frames,
                    pending = pending,
                    drained = drained
                );
                self.warned.summary(ctx, "eos");
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.inflight = None;
        self.staged.discard();
        if let Some(g) = &mut self.gpu {
            g.stop();
        }
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

    #[test]
    fn level_ladder_picks_sane_levels() {
        // 1080p60, 8160 MBs, 489600 MB/s -> 4.2.
        assert_eq!(pick_level(120, 68, (60, 1)), 42);
        // 720p30, 3600 MBs, 108000 MB/s -> 3.1.
        assert_eq!(pick_level(80, 45, (30, 1)), 31);
        // 99 MBs fits L1 but 2970 MB/s exceeds its 1485 MaxMBPS -> 1.1.
        assert_eq!(pick_level(11, 9, (30, 1)), 11);
    }

    #[test]
    fn the_avc1_bytes_are_known_from_size_and_rate() {
        assert_eq!(profile_level(1920, 1038, (23_976, 1000)), [100, 0, 41]);
        assert_eq!(profile_level(1920, 1080, (60, 1)), [100, 0, 42]);
        assert_eq!(profile_level(320, 180, (25, 1)), [100, 0, 12]);
    }

    #[test]
    fn sps_round_trips_through_own_parser() {
        let fmt =
            InFormat { width: 1920, height: 1080, pixfmt: enc::RawPixFmt::Nv12, fps: (30, 1) };
        let sps_nal = {
            struct Dims {
                coded_w: u32,
                coded_h: u32,
                width: u32,
                height: u32,
            }
            let d = Dims { coded_w: 1920, coded_h: 1088, width: 1920, height: 1080 };
            let mbs_w = d.coded_w / 16;
            let mbs_h = d.coded_h / 16;
            let crop_r = (d.coded_w - d.width) / 2;
            let crop_b = (d.coded_h - d.height) / 2;
            let mut w = BitWriter::new();
            w.u(100, 8);
            w.u(0, 8);
            w.u(pick_level(mbs_w, mbs_h, fmt.fps) as u32, 8);
            w.ue(0);
            w.ue(1);
            w.ue(0);
            w.ue(0);
            w.flag(false);
            w.flag(false);
            w.ue(4);
            w.ue(2);
            w.ue(1);
            w.flag(false);
            w.ue(mbs_w - 1);
            w.ue(mbs_h - 1);
            w.flag(true);
            w.flag(true);
            let cropping = crop_r > 0 || crop_b > 0;
            w.flag(cropping);
            if cropping {
                w.ue(0);
                w.ue(crop_r);
                w.ue(0);
                w.ue(crop_b);
            }
            w.flag(false); // no VUI in this variant
            w.rbsp_trailing_bits();
            annexb_nal(&[0x67], &w.into_bytes())
        };
        let sps = h264parse::parse_sps(&sps_nal[4..]).expect("own SPS parses");
        assert_eq!(sps.width(), 1920, "cropped width");
        assert_eq!(sps.height(), 1080, "cropped height");
        assert_eq!(sps.width_in_mbs(), 120);
        assert_eq!(sps.height_in_mbs(), 68);
        assert_eq!(sps.max_num_ref_frames, 1);
    }

    #[test]
    fn the_parameter_sets_are_known_before_a_session() {
        let (sps, pps) =
            parameter_sets(1920, 1038, (23_976, 1000), DEFAULT_QP, &StreamColor::default());
        let p = h264parse::parse_sps(&sps[4..]).expect("own SPS parses");
        assert_eq!((p.width(), p.height()), (1920, 1038), "cropped to the picture");
        assert_eq!((p.profile_idc, p.level_idc), (100, 41));
        assert_eq!([sps[5], sps[6], sps[7]], profile_level(1920, 1038, (23_976, 1000)));
        assert_eq!(pps, write_pps(DEFAULT_QP));
    }

    /// The colour a session states in the VUI parses back, the reorder depth after it too, and
    /// an input that states none writes none.
    #[test]
    fn the_sps_states_the_colour_it_was_given() {
        use profluens_video::color::{Matrix, Primaries, Range, TransferFn};
        let sd = StreamColor {
            matrix: Matrix::from_h273(6),
            primaries: Primaries::from_h273(6),
            transfer: TransferFn::from_h273(6),
            range: Some(Range::Limited),
            ..StreamColor::default()
        };
        let (sps, _) = parameter_sets(720, 576, (25, 1), DEFAULT_QP, &sd);
        let p = h264parse::parse_sps(&sps[4..]).expect("own SPS parses");
        assert_eq!(
            (p.colour.matrix, p.colour.primaries, p.colour.transfer),
            (sd.matrix, sd.primaries, sd.transfer)
        );
        assert_eq!(p.colour.range, Some(Range::Limited));
        assert_eq!(p.max_num_reorder_frames, Some(0), "the restriction after the colour");
        let full = StreamColor { range: Some(Range::Full), ..sd };
        let (sps, _) = parameter_sets(720, 576, (25, 1), DEFAULT_QP, &full);
        assert_eq!(
            h264parse::parse_sps(&sps[4..]).map(|p| p.colour.range),
            Some(Some(Range::Full))
        );
        let (sps, _) = parameter_sets(720, 576, (25, 1), DEFAULT_QP, &StreamColor::default());
        let p = h264parse::parse_sps(&sps[4..]).expect("own SPS parses");
        assert_eq!((p.colour.matrix, p.colour.range), (None, None), "no colour stated");
        assert_eq!(p.max_num_reorder_frames, Some(0));
    }

    #[test]
    fn the_sps_declares_the_references_the_level_holds() {
        let (sps, pps) = parameter_sets(1920, 1080, (30, 1), DEFAULT_QP, &StreamColor::default());
        let p = h264parse::parse_sps(&sps[4..]).expect("own SPS parses");
        assert_eq!((p.max_num_ref_frames, p.max_dec_frame_buffering), (REFS, Some(REFS)));
        assert_eq!(p.max_num_reorder_frames, Some(0), "no reordering");
        assert!(p.max_num_ref_frames <= p.max_dpb_frames(), "within level {}", p.level_idc);
        let pps = h264parse::parse_pps(&pps[4..]).expect("own PPS parses");
        assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 0, "slices override past one");
        // 4.1 holds 4 of 1080p, 3.0 not one of it
        assert_eq!(sps_refs(120, 68, 41), REFS);
        assert_eq!(sps_refs(120, 68, 30), 1);
        assert_eq!(sps_refs(80, 45, 31), REFS);
    }

    #[test]
    fn p_slice_headers_state_their_references() {
        let (sps_nal, pps_nal) =
            parameter_sets(320, 240, (25, 1), DEFAULT_QP, &StreamColor::default());
        let sps = h264parse::parse_sps(&sps_nal[4..]).expect("SPS");
        let pps = h264parse::parse_pps(&pps_nal[4..]).expect("PPS");
        let mut w = BitWriter::new();
        for (frame_num, refs) in [(0u32, 0u32), (1, 1), (2, 2), (3, 3), (200, 3)] {
            let idr = frame_num == 0;
            w.clear();
            write_slice_header(&mut w, idr, frame_num, 5, refs);
            let nal = &w.as_bytes()[4..];
            let ty = if idr { 5 } else { 1 };
            let sh = h264parse::parse_slice_header(nal, ty, 1, &sps, &pps).expect("parses");
            assert_eq!(sh.frame_num, frame_num & 0xFF);
            assert_eq!(sh.pic_order_cnt_lsb, (frame_num * 2) & 0xFF);
            assert_eq!(sh.is_idr, idr);
            if !idr {
                assert_eq!(sh.slice_type, h264parse::SliceType::P);
                assert_eq!(sh.num_ref_idx_l0_active_minus1 + 1, refs, "frame {frame_num}");
                assert!(sh.ref_list_mods_l0.is_empty() && !sh.adaptive_ref_pic_marking_mode_flag);
            }
            // what the driver appends from comes right after the parsed header
            assert_eq!(sh.header_bits_in_rbsp as u32, w.bit_len() - 40, "frame {frame_num}");
        }
    }

    #[test]
    fn avcc_record_shape() {
        let sps = [0u8, 0, 0, 1, 0x67, 100, 0, 42, 0xAA, 0xBB];
        let pps = [0u8, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80];
        let rec = avcc_record(&sps, &pps);
        assert_eq!(rec[0], 1, "configurationVersion");
        assert_eq!(rec[1], 100, "profile from SPS[1]");
        assert_eq!(rec[3], 42, "level from SPS[3]");
        assert_eq!(rec[4], 0xFF, "4-byte lengths");
        assert_eq!(rec[5], 0xE1, "one SPS");
        let sps_len = u16::from_be_bytes([rec[6], rec[7]]) as usize;
        assert_eq!(sps_len, 6, "SPS NAL length (start code stripped)");
        assert_eq!(&rec[8..8 + sps_len], &sps[4..]);
    }

    #[test]
    fn annexb_to_avcc_drops_parameter_sets_and_length_prefixes() {
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1, 0x67, 0xAA]); // SPS (dropped)
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xBB]); // PPS (dropped)
        au.extend_from_slice(&[0, 0, 1, 0x65, 1, 2, 3]); // IDR slice (3-byte code)
        let mut avcc = Vec::new();
        annexb_to_avcc(&au, &mut avcc);
        assert_eq!(&avcc[..4], &4u32.to_be_bytes(), "length prefix");
        assert_eq!(&avcc[4..], &[0x65, 1, 2, 3]);
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_preview_and_the_picture_after_it_are_both_keys() {
        let test = "a_preview_and_the_picture_after_it_are_both_keys";
        let Some(caps) = crate::probe() else {
            return eprintln!("skip {test}: no VA-API device");
        };
        if !caps.supports_encode("h264/annexb") {
            return eprintln!("skip {test}: no H.264 encode entrypoint");
        }
        let (w, h) = (128u32, 64u32);
        let mut hx = pf_harness::Harness::with_pool(VaapiH264Enc::new(), 256 * 1024, 16);
        hx.fix_sink_format(
            VaapiH264Enc::IN,
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
            h264parse::split_nals(au).iter().any(|n| n.unit_type == h264parse::NAL_SLICE_IDR)
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
        if !caps.supports_encode("h264/annexb") {
            return eprintln!("skip {test}: no encode entrypoint");
        }
        let (w, h) = (128u32, 64u32);
        let mut hx =
            pf_harness::Harness::with_pool(VaapiH264Enc::new().with_bitrate(500), 256 * 1024, 16);
        hx.fix_sink_format(
            VaapiH264Enc::IN,
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
        while hx.pull(VaapiH264Enc::OUT).is_some() {}
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
}
