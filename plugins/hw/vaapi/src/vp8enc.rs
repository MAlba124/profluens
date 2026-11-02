//! `vaapivp8enc`, VA-API VP8 encode. The driver writes the whole frame, so there are
//! no packed headers. `qp` is the quantizer index (0..127, RFC 6386 section 9.6).

use std::path::PathBuf;

use profluens_core::batch::Inputs;
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

use crate::enc::{self, EncEngine, FrameBufs, InFormat, PadOut, Staged, Stamp};
use crate::encshell;
use crate::error::{VaapiErrorKind as K, Warned, failed};
use crate::ffi;
use crate::probe;
use crate::va::{self, Buffer as VaBuffer, Display, SurfaceFormat};

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
static SRC_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: "vp8", fields: &OUT_FIELDS }, OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

static PROPS: [PropDesc; 5] = [
    PropDesc { name: "qp", allowed: Constraint::Any, live: false },
    PropDesc { name: "bitrate", allowed: Constraint::Any, live: true },
    PropDesc { name: "gop", allowed: Constraint::Any, live: false },
    PropDesc { name: "force-keyframe", allowed: Constraint::Any, live: true },
    PropDesc { name: "hrd", allowed: Constraint::Any, live: true },
];

// Cold: make_default boxes one element when the registry builds the default, not per-frame.
#[allow(clippy::disallowed_methods)]
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "vaapivp8enc",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VaapiVp8Enc::new())),
    io: false,
    couplings: &[Coupling::same(0, 0, enc::F_WIDTH), Coupling::same(0, 0, enc::F_HEIGHT)],
};

struct Session {
    engine: EncEngine,
    gop_pos: u32,
}

pub struct VaapiVp8Enc {
    device: Option<PathBuf>,
    // Field order is drop order, the session goes before the display it lives on.
    session: Option<Session>,
    display: Option<Display>,

    qindex: u8,
    bitrate_kbps: u32,
    gop_len: u32,
    hrd_ms: u32,
    rc_dirty: bool,
    warned_rc_mode: bool,

    in_fmt: Option<InFormat>,
    bytes_out: bool,
    bufs: FrameBufs,
    staged: Staged,
    disabled: bool,
    warned: Warned,
    frames: u64,
}

// SAFETY: `SchedHint::Active` single-thread confinement, as for the other VA
// elements, the raw `VADisplay` never leaves this element's scheduler thread.
unsafe impl Send for VaapiVp8Enc {}

/// `VAEncPictureParameterBufferVP8.pic_flags` bit positions, va_enc_vp8.h in declaration
/// order from bit 0.
const PF_FRAME_TYPE_INTER: u32 = 1;
const PF_SHOW_FRAME: u32 = 1 << 4;
const PF_REFRESH_GOLDEN: u32 = 1 << 19;
const PF_REFRESH_ALTERNATE: u32 = 1 << 20;
const PF_REFRESH_LAST: u32 = 1 << 21;
/// Two bits, 1 is LAST (RFC 6386 section 9.7.2).
const PF_COPY_LAST_TO_GOLDEN: u32 = 1 << 22;
const PF_COPY_LAST_TO_ALTERNATE: u32 = 1 << 24;
const PF_MB_NO_COEFF_SKIP: u32 = 1 << 28;

/// The picture flag word: LAST tracks the newest frame, GOLDEN and ALTREF copy it, and a
/// key frame refreshes all three (section 9.7).
fn pic_flags(is_kf: bool) -> u32 {
    let mut v = PF_SHOW_FRAME | PF_MB_NO_COEFF_SKIP;
    if is_kf {
        v |= PF_REFRESH_GOLDEN | PF_REFRESH_ALTERNATE | PF_REFRESH_LAST;
    } else {
        v |= PF_FRAME_TYPE_INTER
            | PF_REFRESH_LAST
            | PF_COPY_LAST_TO_GOLDEN
            | PF_COPY_LAST_TO_ALTERNATE;
    }
    v
}

#[cfg(test)]
impl crate::enc_eos::Stage for VaapiVp8Enc {
    fn staged(&mut self) -> &mut Staged {
        &mut self.staged
    }
}

impl Default for VaapiVp8Enc {
    fn default() -> Self {
        Self::new()
    }
}

impl VaapiVp8Enc {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        VaapiVp8Enc {
            device: None,
            display: None,
            session: None,
            qindex: 40,
            bitrate_kbps: 0,
            gop_len: 60,
            hrd_ms: 0,
            rc_dirty: false,
            warned_rc_mode: false,
            in_fmt: None,
            bytes_out: false,
            bufs: FrameBufs::new(),
            staged: Staged::new(),
            disabled: false,
            warned: Warned::default(),
            frames: 0,
        }
    }

    pub fn with_qindex(mut self, q: u8) -> Self {
        self.qindex = q.min(127);
        self
    }

    pub fn with_gop(mut self, gop: u32) -> Self {
        self.gop_len = gop.clamp(1, 1024);
        self
    }

    pub fn with_bitrate(mut self, kbps: u32) -> Self {
        self.bitrate_kbps = kbps;
        self
    }

    pub fn with_hrd_ms(mut self, ms: u32) -> Self {
        self.hrd_ms = ms.min(10_000);
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
            if s.engine.width == fmt.width && s.engine.height == fmt.height {
                return true;
            }
            log!(&*ctx, Level::Debug, "reinit", width = fmt.width, height = fmt.height);
            self.session = None;
        }
        let Some(display) = &self.display else {
            return false;
        };
        let rc_mode = if self.bitrate_kbps > 0 { ffi::VA_RC_VBR } else { ffi::VA_RC_CQP };
        let engine = match EncEngine::new(
            display,
            ffi::VAProfileVP8Version0_3,
            ffi::VAEntrypointEncSlice,
            rc_mode,
            SurfaceFormat::NV12,
            fmt.width,
            fmt.height,
            16, // VP8 codes whole macroblocks (RFC 6386 section 2)
        ) {
            Ok(e) => e,
            Err(e) => {
                self.warn(ctx, K::SessionSetup(e));
                return false;
            }
        };
        self.session = Some(Session { engine, gop_pos: 0 });
        log!(&*ctx, Level::Info, "session", width = fmt.width, height = fmt.height);
        let (kbps, q, gop) = (self.bitrate_kbps, self.qindex, self.gop_len);
        log!(
            &*ctx,
            Level::Info,
            "rate_control",
            cqp = kbps == 0,
            bitrate_kbps = kbps,
            qindex = q,
            gop = gop
        );
        if self.bytes_out {
            ctx.announce_format(Self::OUT, "bytes", &[]);
        } else {
            ctx.announce_format(
                Self::OUT,
                "vp8",
                &[
                    (enc::F_WIDTH, ValueDesc::Int(fmt.width as i64)),
                    (enc::F_HEIGHT, ValueDesc::Int(fmt.height as i64)),
                ],
            );
        }
        true
    }

    fn pad(ctx: &mut Ctx) -> PadOut<'_> {
        PadOut { ctx, pad: Self::OUT, name: DESC.name }
    }

    fn drain_pending(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        self.staged.drain(&mut Self::pad(ctx))
    }

    fn encode_frame(
        &mut self,
        ctx: &mut Ctx,
        data: &[u8],
        pts: Timestamp,
        duration: Timestamp,
    ) -> Result<(), Error> {
        let Some(fmt) = self.in_fmt else {
            return Ok(());
        };
        let result = {
            let (Some(display), Some(s)) = (&self.display, &mut self.session) else {
                return Ok(());
            };
            let is_kf = s.gop_pos == 0;
            let rc = RcUpdate { dirty: self.rc_dirty, hrd_ms: self.hrd_ms };
            self.bufs.coded.clear();
            let (q, kbps, gop) = (self.qindex, self.bitrate_kbps, self.gop_len);
            encode_one(display, s, fmt, q, kbps, gop, is_kf, rc, data, &mut self.bufs)
                .map(|()| is_kf)
        };
        let is_kf = match result {
            Ok(v) => v,
            Err(e) => {
                self.warn(ctx, K::Encode(e));
                return Ok(());
            }
        };
        self.rc_dirty = false; // the RC/HRD update rode this frame's submission
        log!(&*ctx, Level::Trace, "encoded", bytes = self.bufs.coded.len(), key = is_kf);
        self.frames += 1;

        let gop_len = self.gop_len.clamp(1, 1024);
        let Some(s) = self.session.as_mut() else {
            return Ok(());
        };
        s.engine.advance_recon();
        s.gop_pos += 1;
        if s.gop_pos >= gop_len {
            s.gop_pos = 0;
        }
        let stamp = Stamp { pts, duration, key: is_kf, preview: false };
        self.staged.send_or_stage(&mut Self::pad(ctx), &mut self.bufs.coded, stamp)
    }
}

#[derive(Clone, Copy)]
struct RcUpdate {
    dirty: bool,
    hrd_ms: u32,
}

/// The driver writes all VP8 headers, the coded buffer is the whole frame.
#[allow(clippy::too_many_arguments)]
fn encode_one(
    display: &Display,
    s: &mut Session,
    fmt: InFormat,
    qindex: u8,
    bitrate_kbps: u32,
    gop_len: u32,
    is_kf: bool,
    rc: RcUpdate,
    data: &[u8],
    bufs: &mut FrameBufs,
) -> va::VaResult<()> {
    s.engine.upload(display, data, fmt.pixfmt)?;

    let engine = &s.engine;
    let ctxva = engine.context();
    let coded =
        VaBuffer::new_empty(display, ctxva, ffi::VAEncCodedBufferType, engine.coded_buf_size())?;
    let built =
        params(display, engine, fmt, qindex, bitrate_kbps, gop_len, is_kf, rc, &coded, bufs);
    let submitted = built.and_then(|()| bufs.submit(display, engine, None));
    bufs.hold.clear();
    submitted?;
    coded.read_coded(&mut bufs.coded)
}

#[allow(clippy::too_many_arguments)]
fn params(
    display: &Display,
    engine: &EncEngine,
    fmt: InFormat,
    qindex: u8,
    bitrate_kbps: u32,
    gop_len: u32,
    is_kf: bool,
    rc: RcUpdate,
    coded: &VaBuffer,
    bufs: &mut FrameBufs,
) -> va::VaResult<()> {
    if is_kf {
        let seq = ffi::VAEncSequenceParameterBufferVP8 {
            frame_width: engine.width,
            frame_height: engine.height,
            frame_width_scale: 0,
            frame_height_scale: 0,
            error_resilient: 0,
            kf_auto: 0, // we force keyframes ourselves
            kf_min_dist: 0,
            kf_max_dist: gop_len,
            bits_per_second: bitrate_kbps.saturating_mul(1000),
            intra_period: gop_len,
            reference_frames: [ffi::VA_INVALID_SURFACE; 4],
            va_reserved: [0; ffi::VA_PADDING_LOW],
        };
        bufs.param(display, engine, ffi::VAEncSequenceParameterBufferType, &seq)?;
    }

    if bitrate_kbps > 0 && (is_kf || rc.dirty) {
        let q = (4, 127);
        encshell::rc_buffers_q(display, engine, bitrate_kbps, q, fmt.fps, &mut bufs.hold)?;
        if rc.hrd_ms > 0 {
            bufs.hold.push(encshell::hrd_buffer(display, engine, bitrate_kbps, rc.hrd_ms)?);
        }
    }

    // ref_flags bit order per va_enc_vp8.h: force_kf, no_ref_last, no_ref_gf,
    // no_ref_arf, temporal_id:8, first_ref:2, second_ref:2.
    let ref_flags: u32 = if is_kf {
        1 | (1 << 1) | (1 << 2) | (1 << 3) // force_kf + no references
    } else {
        0
    };
    let pic_flags = pic_flags(is_kf);

    let lf = ((qindex as i32) / 2).clamp(1, 63) as i8;

    let pic = ffi::VAEncPictureParameterBufferVP8 {
        reconstructed_frame: engine.recon_cur(),
        ref_last_frame: if is_kf { ffi::VA_INVALID_SURFACE } else { engine.recon_ref() },
        ref_gf_frame: if is_kf { ffi::VA_INVALID_SURFACE } else { engine.recon_ref() },
        ref_arf_frame: if is_kf { ffi::VA_INVALID_SURFACE } else { engine.recon_ref() },
        coded_buf: coded.id(),
        ref_flags,
        pic_flags,
        loop_filter_level: [lf; 4],
        ref_lf_delta: [0; 4],
        mode_lf_delta: [0; 4],
        sharpness_level: 0,
        clamp_qindex_high: 127,
        clamp_qindex_low: 0,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    };
    bufs.param(display, engine, ffi::VAEncPictureParameterBufferType, &pic)?;

    let q = ffi::VAQMatrixBufferVP8 {
        quantization_index: [qindex as u16; 4],
        quantization_index_delta: [0; 5],
        va_reserved: [0; ffi::VA_PADDING_LOW],
    };
    bufs.param(display, engine, ffi::VAQMatrixBufferType, &q)
}

impl Element for VaapiVp8Enc {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(v)) = ctx.prop("qp") {
            self.qindex = v.clamp(0, 127) as u8;
        }
        if let Some(Value::Int(v)) = ctx.prop("bitrate") {
            self.bitrate_kbps = v.clamp(0, 500_000) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("gop") {
            self.gop_len = v.clamp(1, 1024) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("hrd") {
            self.hrd_ms = v.clamp(0, 10_000) as u32;
        }
        self.bytes_out = ctx
            .negotiated_src(Self::OUT)
            .and_then(|f| ctx.family_name(f.family))
            .is_some_and(|name| name == "bytes");
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.staged.resume(&mut Self::pad(ctx))? {
            return Ok(Flow::Ok);
        }
        if !self.ensure_display(ctx) {
            while inputs.pop().is_some() {}
            return Ok(Flow::Ok);
        }
        if self.in_fmt.is_none()
            && let Some(f) = ctx.negotiated_sink(Self::IN).cloned()
        {
            self.in_fmt = enc::read_in_format(ctx, &f);
        }
        loop {
            // A dry output pool must not stop input, a downstream muxer shares the pool.
            self.drain_pending(ctx)?;
            if self.staged.full() {
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
            if !self.ensure_session(ctx, fmt) {
                continue;
            }
            self.encode_frame(ctx, inbuf.memory.data(), inbuf.pts, inbuf.duration)?;
        }
        self.drain_pending(ctx)?;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(f) => {
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
                    s.gop_pos = 0; // next frame is a keyframe (refreshes all refs)
                }
            }
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", pending = self.staged.len());
                self.warned.summary(ctx, "flush");
                self.staged.discard();
                if let Some(s) = &mut self.session {
                    s.gop_pos = 0; // post-seek output opens on a keyframe
                }
            }
            Event::Eos => {
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
        self.staged.discard();
        self.session = None;
        self.display = None;
        self.in_fmt = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The word read back through the header's bitfield layout.
    fn field(v: u32, bit: u32, width: u32) -> u32 {
        (v >> bit) & ((1 << width) - 1)
    }

    #[test]
    fn pic_flags_land_in_the_header_fields() {
        let key = pic_flags(true);
        let inter = pic_flags(false);
        for v in [key, inter] {
            assert_eq!(field(v, 4, 1), 1, "show_frame");
            assert_eq!(field(v, 28, 1), 1, "mb_no_coeff_skip");
            assert_eq!(field(v, 26, 2), 0, "sign_bias golden and alternate");
            assert_eq!(field(v, 18, 1), 0, "refresh_entropy_probs");
            assert_eq!(field(v, 21, 1), 1, "refresh_last");
        }
        assert_eq!(field(key, 0, 1), 0, "key frame_type");
        assert_eq!((field(key, 19, 1), field(key, 20, 1)), (1, 1), "key refreshes golden, alt");
        assert_eq!((field(key, 22, 2), field(key, 24, 2)), (0, 0), "key copies nothing");
        assert_eq!(field(inter, 0, 1), 1, "inter frame_type");
        assert_eq!((field(inter, 19, 1), field(inter, 20, 1)), (0, 0));
        assert_eq!(field(inter, 22, 2), 1, "copy_buffer_to_golden = LAST");
        assert_eq!(field(inter, 24, 2), 1, "copy_buffer_to_alternate = LAST");
    }
}
