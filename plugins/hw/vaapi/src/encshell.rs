//! The encoder element VP9 and AV1 share, a `Codec` gives the profile and the parameter buffers.
//! Written after ffmpeg's `vp9_vaapi` and `av1_vaapi`, neither has run on hardware that encodes
//! them.

// the elements are public over a codec trait and an engine that stay this crate's
#![allow(private_bounds, private_interfaces)]

use std::marker::PhantomData;
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

use crate::enc::{self, EncEngine, InFormat, PadOut, RawPixFmt, Staged, Stamp};
use crate::error::{VaapiErrorKind as K, Warned, failed};
use crate::ffi;
use crate::gpuframe::{self, GpuFrameChannel, GpuFrameHeader};
use crate::gpuin::{self, GpuIn, Input};
use crate::probe;
use crate::va::{Buffer as VaBuffer, Display, SurfaceFormat, VaResult};

pub(crate) const IN: SinkPad = SinkPad(0);
pub(crate) const OUT: SrcPad = SrcPad(0);

const BUFFERS_MAX: usize = 16;
const MISC_MAX: usize = 64;

static PIXFMTS: [ValueDesc; 3] =
    [ValueDesc::Id("nv12"), ValueDesc::Id("i420"), ValueDesc::Id("p010")];
static RAW_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: enc::F_WIDTH, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: enc::F_HEIGHT, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: enc::F_PIXFMT, allowed: ConstraintDesc::Set(&PIXFMTS), preferred: None },
    FieldDesc { field: enc::F_FPS, allowed: ConstraintDesc::Any, preferred: None },
];
static RAW_OFFERS: [OfferDesc; 1] = [OfferDesc { family: enc::RAW_FAMILY, fields: &RAW_FIELDS }];
pub(crate) static RAW_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_OFFERS }];
pub(crate) static GPU_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
pub(crate) static OUT_FIELDS: [FieldDesc; 2] = [
    FieldDesc { field: enc::F_WIDTH, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: enc::F_HEIGHT, allowed: ConstraintDesc::Any, preferred: None },
];

pub(crate) type MakeFn = fn() -> Box<dyn Element>;

pub(crate) const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    make_default: Option<MakeFn>,
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub qindex: u8,
    pub gop: u32,
    pub bitrate_kbps: u32,
    pub low_power: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { qindex: 100, gop: 60, bitrate_kbps: 0, low_power: true }
    }
}

pub(crate) struct Picture {
    pub key: bool,
    pub frame_num: u32,
    pub coded: ffi::VABufferID,
}

pub(crate) trait Codec: Send + 'static {
    const FAMILY: &'static str;
    const ALIGN: u32;
    const PACKED: u32 = 0;
    type State: Send;
    fn desc(gpu: bool) -> &'static ElementDesc;
    fn profile(ten: bool) -> (ffi::VAProfile, &'static str);
    fn begin(
        display: &Display,
        profile: ffi::VAProfile,
        entrypoint: ffi::VAEntrypoint,
        engine: &EncEngine,
        cfg: &Settings,
        fmt: InFormat,
    ) -> Result<Self::State, K>;
    fn buffers(
        st: &mut Self::State,
        display: &Display,
        engine: &EncEngine,
        cfg: &Settings,
        fmt: InFormat,
        p: &Picture,
        out: &mut Vec<VaBuffer>,
    ) -> VaResult<()>;
    /// The picture is coded and read, reference state moves on. A dropped picture never
    /// reaches this, so the references stay what the bitstream says.
    fn commit(_st: &mut Self::State) {}
}

struct Session<S> {
    engine: EncEngine,
    ten: bool,
    low_power: bool,
    gop_pos: u32,
    codec: S,
}

/// VA-API VP9 or AV1 encoder, `video/raw` or `video/gpu` in, the codec's family out.
pub struct VaEnc<C: Codec> {
    cfg: Settings,
    // field order is drop order, what holds the display's handle goes first
    gpu: Option<GpuIn>,
    session: Option<Session<C::State>>,
    display: Option<Display>,
    in_fmt: Option<InFormat>,
    hold: Vec<VaBuffer>,
    ids: Vec<ffi::VABufferID>,
    coded: Vec<u8>,
    staged: Staged,
    disabled: bool,
    warned: Warned,
    frames: u64,
    codec: PhantomData<C>,
}

// SAFETY: an Active element runs on one scheduler thread and never shares its display.
unsafe impl<C: Codec> Send for VaEnc<C> {}

#[cfg(test)]
impl<C: Codec> crate::enc_eos::Stage for VaEnc<C> {
    fn staged(&mut self) -> &mut Staged {
        &mut self.staged
    }
}

impl<C: Codec> VaEnc<C> {
    pub const IN: SinkPad = IN;
    pub const OUT: SrcPad = OUT;

    // Cold: construction.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn with_settings(cfg: Settings) -> Self {
        VaEnc {
            cfg,
            gpu: None,
            session: None,
            display: None,
            in_fmt: None,
            hold: Vec::with_capacity(BUFFERS_MAX),
            ids: Vec::with_capacity(BUFFERS_MAX),
            coded: Vec::new(),
            staged: Staged::new(),
            disabled: false,
            warned: Warned::default(),
            frames: 0,
            codec: PhantomData,
        }
    }

    pub fn zerocopy(mut self, channel: Arc<GpuFrameChannel>) -> Self {
        self.gpu = Some(GpuIn::new(channel));
        self
    }

    pub fn with_fps(mut self, num: u32, den: u32) -> Self {
        if let Some(g) = &mut self.gpu {
            g.fps = (num, den.max(1));
        }
        self
    }

    pub fn coded_align(&self) -> u32 {
        C::ALIGN
    }

    fn name() -> &'static str {
        C::desc(false).name
    }

    fn warn(&mut self, ctx: &mut Ctx, kind: K) {
        if !self.warned.note(ctx, kind) {
            return;
        }
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: failed(ctx, Self::name(), kind) });
    }

    /// The encoder stops for good, every later frame is dropped.
    fn disable(&mut self, ctx: &mut Ctx, kind: K) {
        if !self.disabled {
            self.disabled = true;
            log!(&*ctx, Level::Error, "disabled", kind = kind.name(), status = kind.va_status());
            let element = ctx.element();
            ctx.post(BusMessage::Warning { element, error: failed(ctx, Self::name(), kind) });
        }
    }

    fn ensure_display(&mut self, ctx: &mut Ctx) -> bool {
        if self.display.is_some() {
            return true;
        }
        let Some(caps) = probe::probe() else {
            self.disable(ctx, K::NoDevice);
            return false;
        };
        match Display::open(&caps.device) {
            Ok(d) => self.display = Some(d),
            Err(e) => self.disable(ctx, K::DisplayOpen(e)),
        }
        self.display.is_some()
    }

    fn ensure_session(&mut self, ctx: &mut Ctx, fmt: InFormat) -> bool {
        let ten = fmt.pixfmt == RawPixFmt::P010;
        if let Some(s) = &self.session {
            if s.engine.width == fmt.width && s.engine.height == fmt.height && s.ten == ten {
                return true;
            }
            log!(
                &*ctx,
                Level::Debug,
                "reinit",
                width = fmt.width,
                height = fmt.height,
                ten_bit = ten
            );
            self.session = None;
        }
        match self.open_session(fmt, ten) {
            Ok(s) => {
                let c = &self.cfg;
                log!(
                    &*ctx,
                    Level::Info,
                    "session",
                    width = fmt.width,
                    height = fmt.height,
                    ten_bit = ten,
                    low_power = s.low_power,
                );
                let cqp = c.bitrate_kbps == 0;
                log!(
                    &*ctx,
                    Level::Info,
                    "rate_control",
                    cqp = cqp,
                    bitrate_kbps = c.bitrate_kbps,
                    qindex = c.qindex,
                    gop = c.gop
                );
                self.session = Some(s);
                ctx.announce_format(
                    OUT,
                    C::FAMILY,
                    &[
                        (enc::F_WIDTH, ValueDesc::Int(fmt.width as i64)),
                        (enc::F_HEIGHT, ValueDesc::Int(fmt.height as i64)),
                    ],
                );
                true
            }
            Err(e) => {
                self.disable(ctx, e);
                false
            }
        }
    }

    fn open_session(&mut self, fmt: InFormat, ten: bool) -> Result<Session<C::State>, K> {
        if !fmt.width.is_multiple_of(2) || !fmt.height.is_multiple_of(2) {
            return Err(K::OddDimensions { width: fmt.width, height: fmt.height });
        }
        let display = self.display.as_ref().ok_or(K::NoDevice)?;
        let caps = probe::probe().ok_or(K::NoDevice)?;
        let (profile, name) = C::profile(ten);
        let row = caps.profiles.iter().find(|p| p.name == name);
        let (enc, lp) = row.map_or((false, false), |p| (p.enc, p.enc_lp));
        let entrypoint = match (enc, lp) {
            (_, true) if self.cfg.low_power || !enc => ffi::VAEntrypointEncSliceLP,
            (true, _) => ffi::VAEntrypointEncSlice,
            _ => return Err(K::NoEncodeEntrypoint { profile: name }),
        };
        let rc = if self.cfg.bitrate_kbps > 0 { ffi::VA_RC_VBR } else { ffi::VA_RC_CQP };
        let format = if ten { SurfaceFormat::P010 } else { SurfaceFormat::NV12 };
        let engine = EncEngine::new_packed(
            display,
            profile,
            entrypoint,
            rc,
            format,
            fmt.width,
            fmt.height,
            C::ALIGN,
            C::PACKED,
        )
        .map_err(K::SessionSetup)?;
        let codec = C::begin(display, profile, entrypoint, &engine, &self.cfg, fmt)?;
        let low_power = entrypoint == ffi::VAEntrypointEncSliceLP;
        Ok(Session { engine, ten, low_power, gop_pos: 0, codec })
    }

    fn encode(
        &mut self,
        ctx: &mut Ctx,
        input: Input<'_>,
        pts: Timestamp,
        duration: Timestamp,
        preview: bool,
    ) -> Result<(), Error> {
        let Some(fmt) = self.in_fmt else {
            return Ok(());
        };
        let (Some(display), Some(s)) = (&self.display, &mut self.session) else {
            return Ok(());
        };
        // a preview is a key frame of its own, the run proper opens on the next
        if preview {
            log!(&*ctx, Level::Debug, "forced_key", pts = pts);
            s.gop_pos = 0;
        }
        let key = s.gop_pos == 0;
        self.coded.clear();
        let r = picture::<C>(
            display,
            s,
            &self.cfg,
            fmt,
            input,
            key,
            &mut self.hold,
            &mut self.ids,
            &mut self.coded,
        );
        if let Err(e) = r {
            self.warn(ctx, K::Encode(e));
            return Ok(());
        }
        log!(&*ctx, Level::Trace, "encoded", bytes = self.coded.len(), key = key);
        self.frames += 1;
        s.engine.advance_recon();
        s.gop_pos += 1;
        if preview || s.gop_pos >= self.cfg.gop.max(1) {
            s.gop_pos = 0;
        }
        let stamp = Stamp { pts, duration, key, preview };
        self.staged.send_or_stage(&mut Self::pad(ctx), &mut self.coded, stamp)
    }

    fn pad(ctx: &mut Ctx) -> PadOut<'_> {
        PadOut { ctx, pad: OUT, name: Self::name() }
    }

    fn drain_pending(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        self.staged.drain(&mut Self::pad(ctx))
    }

    fn process_raw(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        loop {
            self.drain_pending(ctx)?;
            if self.staged.full() {
                return Ok(());
            }
            let Some(inbuf) = inputs.pop() else { break };
            let Some(fmt) = self.in_fmt else {
                self.warn(ctx, K::FrameBeforeFormat);
                continue;
            };
            let (len, need) = (inbuf.memory.data().len(), fmt.frame_bytes());
            if len < need {
                self.warn(ctx, K::ShortFrame { len, need });
                continue;
            }
            if !self.ensure_session(ctx, fmt) {
                continue;
            }
            self.encode(
                ctx,
                Input::Raw(inbuf.memory.data()),
                inbuf.pts,
                inbuf.duration,
                inbuf.flags.contains(BufferFlags::PREVIEW),
            )?;
        }
        self.drain_pending(ctx)?;
        Ok(())
    }

    fn process_gpu(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        loop {
            self.drain_pending(ctx)?;
            if self.staged.full() {
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
            if !enc::dims_in_range(h.disp_w, h.disp_h) {
                self.warn(ctx, K::GpuHeader);
                continue;
            }
            let fmt = InFormat { width: h.disp_w, height: h.disp_h, pixfmt, fps };
            self.in_fmt = Some(fmt);
            let Some(frame) = self.gpu.as_mut().and_then(|g| g.take(h.token)) else {
                self.warn(ctx, K::NoGpuFrame { token: h.token });
                continue;
            };
            if !self.ensure_session(ctx, fmt) {
                continue;
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
            let surface = imported.map_err(|e| failed(ctx, Self::name(), e))?;
            self.encode(
                ctx,
                Input::Surface(surface),
                inbuf.pts,
                inbuf.duration,
                inbuf.flags.contains(BufferFlags::PREVIEW),
            )?;
            drop(frame);
        }
        self.drain_pending(ctx)?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn picture<C: Codec>(
    display: &Display,
    s: &mut Session<C::State>,
    cfg: &Settings,
    fmt: InFormat,
    input: Input<'_>,
    key: bool,
    hold: &mut Vec<VaBuffer>,
    ids: &mut Vec<ffi::VABufferID>,
    coded: &mut Vec<u8>,
) -> VaResult<()> {
    let surface = match input {
        Input::Raw(data) => {
            s.engine.upload(display, data, fmt.pixfmt)?;
            s.engine.input()
        }
        Input::Surface(surface) => surface,
    };
    let out = VaBuffer::new_empty(
        display,
        s.engine.context(),
        ffi::VAEncCodedBufferType,
        s.engine.coded_buf_size(),
    )?;
    let p = Picture { key, frame_num: s.gop_pos, coded: out.id() };
    hold.clear();
    let built = C::buffers(&mut s.codec, display, &s.engine, cfg, fmt, &p, hold);
    ids.clear();
    ids.extend(hold.iter().map(VaBuffer::id));
    let submitted = built.and_then(|_| s.engine.submit_from(display, ids, surface));
    hold.clear();
    submitted?;
    out.read_coded(coded)?;
    C::commit(&mut s.codec);
    Ok(())
}

impl<C: Codec> Element for VaEnc<C> {
    fn desc(&self) -> &'static ElementDesc {
        C::desc(self.gpu.is_some())
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
        gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_sink(IN, offers))
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(g) = &mut self.gpu {
            g.start(ctx, ctx.negotiated_sink(IN)).map_err(|e| failed(ctx, Self::name(), e))?;
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.staged.resume(&mut Self::pad(ctx))? {
            return Ok(Flow::Ok);
        }
        if self.disabled || !self.ensure_display(ctx) {
            while inputs.pop().is_some() {}
            return Ok(Flow::Ok);
        }
        if self.in_fmt.is_none()
            && let Some(f) = ctx.negotiated_sink(IN).cloned()
        {
            self.in_fmt = enc::read_in_format(ctx, &f);
        }
        if self.gpu.is_some() {
            self.process_gpu(ctx, &mut inputs)?;
        } else {
            self.process_raw(ctx, &mut inputs)?;
        }
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
                if let Some(g) = &mut self.gpu {
                    g.format_change(ctx, f);
                }
            }
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", pending = self.staged.len());
                self.warned.summary(ctx, "flush");
                if let Some(g) = &mut self.gpu {
                    g.flush(ctx);
                }
                self.staged.discard();
                if let Some(s) = &mut self.session {
                    s.gop_pos = 0;
                }
            }
            Event::Eos => {
                let drained = self.staged.finish(&mut Self::pad(ctx))?;
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    frames = self.frames,
                    pending = self.staged.len(),
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
        self.hold.clear();
        if let Some(g) = &mut self.gpu {
            g.stop();
        }
        self.session = None;
        self.display = None;
        self.in_fmt = None;
        // The next run opens its own display and session, what disabled this one may be gone.
        self.disabled = false;
    }
}

/// A `#[repr(C)]` VA misc parameter struct of plain integers with no padding, so its bytes
/// are all initialised and [`misc_buffer`] may read them.
pub(crate) trait MiscParam: Copy {}
impl MiscParam for ffi::VAEncMiscParameterRateControl {}
impl MiscParam for ffi::VAEncMiscParameterFrameRate {}
impl MiscParam for ffi::VAEncMiscParameterHRD {}

pub(crate) fn misc_buffer<T: MiscParam>(
    display: &Display,
    engine: &EncEngine,
    misc_type: u32,
    value: &T,
) -> VaResult<VaBuffer> {
    const { assert!(4 + std::mem::size_of::<T>() <= MISC_MAX) };
    let size = std::mem::size_of::<T>();
    let mut bytes = [0u8; MISC_MAX];
    bytes[..4].copy_from_slice(&misc_type.to_ne_bytes());
    // SAFETY: T is a MiscParam, a padding free #[repr(C)] struct of integers, every byte of it
    // is initialised.
    bytes[4..4 + size].copy_from_slice(unsafe {
        std::slice::from_raw_parts(value as *const T as *const u8, size)
    });
    VaBuffer::new_typed_bytes(
        display,
        engine.context(),
        ffi::VAEncMiscParameterBufferType,
        &bytes[..4 + size],
    )
}

pub(crate) fn rc_buffers(
    display: &Display,
    engine: &EncEngine,
    bitrate_kbps: u32,
    max_q: u32,
    fps: (u32, u32),
    out: &mut Vec<VaBuffer>,
) -> VaResult<()> {
    rc_buffers_q(display, engine, bitrate_kbps, (1, max_q), fps, out)
}

/// The rate control and frame rate misc buffers, `q` the quantizer range.
pub(crate) fn rc_buffers_q(
    display: &Display,
    engine: &EncEngine,
    bitrate_kbps: u32,
    q: (u32, u32),
    fps: (u32, u32),
    out: &mut Vec<VaBuffer>,
) -> VaResult<()> {
    let rc = ffi::VAEncMiscParameterRateControl {
        bits_per_second: bitrate_kbps.saturating_mul(1000),
        target_percentage: 95,
        window_size: 1000,
        initial_qp: 0,
        min_qp: q.0,
        basic_unit_size: 0,
        rc_flags: 0,
        ICQ_quality_factor: 0,
        max_qp: q.1,
        quality_factor: 0,
        target_frame_size: 0,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    };
    let (mut num, mut den) = if fps.0 > 0 { fps } else { (30, 1) };
    // Both halves are 16 bits, scaled down together so the rate stays.
    while num > 0xFFFF || den > 0xFFFF {
        num >>= 1;
        den = (den >> 1).max(1);
    }
    let fr = ffi::VAEncMiscParameterFrameRate {
        framerate: (den << 16) | num,
        framerate_flags: 0,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    };
    out.push(misc_buffer(display, engine, ffi::VAEncMiscParameterTypeRateControl, &rc)?);
    out.push(misc_buffer(display, engine, ffi::VAEncMiscParameterTypeFrameRate, &fr)?);
    Ok(())
}

/// The HRD misc buffer, `hrd_ms` of the target bitrate deep and half full at the start.
pub(crate) fn hrd_buffer(
    display: &Display,
    engine: &EncEngine,
    bitrate_kbps: u32,
    hrd_ms: u32,
) -> VaResult<VaBuffer> {
    let bits = (bitrate_kbps as u64 * 1000).saturating_mul(hrd_ms as u64) / 1000;
    let buffer_size = bits.min(u32::MAX as u64) as u32;
    let hrd = ffi::VAEncMiscParameterHRD {
        initial_buffer_fullness: buffer_size / 2,
        buffer_size,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    };
    misc_buffer(display, engine, ffi::VAEncMiscParameterTypeHRD, &hrd)
}
