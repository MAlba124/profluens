//! `vaapipostproc`, frames scaled and converted through the VA processor to nv12 or p010.
//! `video/raw` or `video/gpu` in and out. Frames wholly before a seek's presentation point are
//! dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, FixedFormat, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_video::color::{Matrix, Range, StreamColor};
use profluens_video::format::{
    COLOR_OFFER_FIELDS, FAMILY, FIELD_FPS, FIELD_HEIGHT, FIELD_PIXFMT, FIELD_WIDTH, PixelFormat,
};
use profluens_video::geometry::{frame_size, plane_geometry, plane_offset};

use crate::error::{VaapiErrorKind as K, Warned, failed};
use crate::ffi;
use crate::gpuframe::{
    self, DmabufFormat, F_LAYOUT, F_MODIFIER, FAMILY_VIDEO_GPU, FRAMES_OUT, FrameRx, FrameToken,
    FrameTx, GPU_FRAME_HEADER_LEN, GpuFrame, GpuFrameChannel, GpuFrameHeader,
};
use crate::probe;
use crate::va::{Display, ExportedSurface, ReadbackImage, SurfaceFormat, Surfaces, UploadImage};
use crate::vpp::format::{self, Kind, NV12, P010};
use crate::vpp::plan::{Plan, Target, plan, plan_fixed};
use crate::vpp::proc::{Color, Imported, Out, Proc, Src, dmabuf_key, import, surfaces};

const NAME: &str = "vaapipostproc";
const IN_PAD: SinkPad = SinkPad(0);
const OUT_PAD: SrcPad = SrcPad(0);

static IN_PIXFMTS: [ValueDesc; 3] =
    [ValueDesc::Id("i420"), ValueDesc::Id("nv12"), ValueDesc::Id("p010")];
static OUT_PIXFMTS: [ValueDesc; 2] = [ValueDesc::Id("nv12"), ValueDesc::Id("p010")];

const fn fields(pixfmts: &'static [ValueDesc]) -> [FieldDesc; 14] {
    const fn any(field: &'static str) -> FieldDesc {
        FieldDesc { field, allowed: ConstraintDesc::Any, preferred: None }
    }
    let c = COLOR_OFFER_FIELDS;
    [
        any(FIELD_WIDTH),
        any(FIELD_HEIGHT),
        FieldDesc { field: FIELD_PIXFMT, allowed: ConstraintDesc::Set(pixfmts), preferred: None },
        any(FIELD_FPS),
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
}

static IN_FIELDS: [FieldDesc; 14] = fields(&IN_PIXFMTS);
static OUT_FIELDS: [FieldDesc; 14] = fields(&OUT_PIXFMTS);
static IN_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &IN_FIELDS }];
static OUT_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &OUT_FIELDS }];
static RAW_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &IN_OFFERS }];
static RAW_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OUT_OFFERS }];
static GPU_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
static GPU_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: OfferDesc::PROBED }];

type MakeFn = fn() -> Box<dyn Element>;

const fn desc(
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    make_default: Option<MakeFn>,
) -> ElementDesc {
    ElementDesc {
        name: NAME,
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

// Cold: make_default boxes one element when the registry builds the default, not per-frame.
#[allow(clippy::disallowed_methods)]
fn make_default() -> Box<dyn Element> {
    Box::new(VaapiPostProc::new(Target::default()))
}

static DESCS: [[ElementDesc; 2]; 2] = [
    [desc(&RAW_SINKS, &RAW_SRCS, Some(make_default)), desc(&RAW_SINKS, &GPU_SRCS, None)],
    [desc(&GPU_SINKS, &RAW_SRCS, None), desc(&GPU_SINKS, &GPU_SRCS, None)],
];

const IMPORTS_MAX: usize = FRAMES_OUT;
const OUT_SURFACES: u32 = 8;
const CODED_ALIGN: u32 = 16;

#[derive(Debug, Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub imports: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InFormat {
    width: u32,
    height: u32,
    pixfmt: PixelFormat,
    fps: Option<(i32, i32)>,
}

impl InFormat {
    fn kind(&self) -> Kind {
        if self.pixfmt == PixelFormat::P010 { P010 } else { NV12 }
    }
}

fn out_pixfmt(k: Kind) -> PixelFormat {
    if k == P010 { PixelFormat::P010 } else { PixelFormat::Nv12 }
}

fn read_format(ctx: &Ctx, f: &FixedFormat) -> Option<InFormat> {
    let get = |field: &str| f.get(ctx.field_id(field)?);
    let int = |field| match get(field)? {
        Value::Int(v) if v > 0 => u32::try_from(v).ok(),
        _ => None,
    };
    let pixfmt = match get(FIELD_PIXFMT)? {
        Value::Id(id) => PixelFormat::from_caps_name(ctx.value_name(id)?)?,
        _ => return None,
    };
    if !matches!(pixfmt, PixelFormat::I420 | PixelFormat::Nv12 | PixelFormat::P010) {
        return None;
    }
    let fps = match get(FIELD_FPS) {
        Some(Value::Rat(n, d)) if n > 0 && d > 0 => Some((n, d)),
        _ => None,
    };
    Some(InFormat { width: int(FIELD_WIDTH)?, height: int(FIELD_HEIGHT)?, pixfmt, fps })
}

pub(crate) fn read_link(ctx: &Ctx, f: Option<&FixedFormat>) -> Option<(Kind, u64)> {
    let (fourcc, modifier) = gpuframe::link_layout(ctx, f)?;
    let kind = format::OUTPUTS.into_iter().find(|k| k.fourcc == fourcc)?;
    Some((kind, modifier))
}

fn color_of(c: &StreamColor, height: u32) -> Color {
    let r = c.resolve(height);
    let standard = match r.matrix {
        Matrix::Bt601 => ffi::VAProcColorStandardBT601,
        Matrix::Bt709 => ffi::VAProcColorStandardBT709,
        Matrix::Bt2020 => ffi::VAProcColorStandardBT2020,
        _ => ffi::VAProcColorStandardNone,
    };
    let range = match r.range {
        Range::Limited => ffi::VA_SOURCE_RANGE_REDUCED,
        Range::Full => ffi::VA_SOURCE_RANGE_FULL,
    };
    Color { standard, range, ..Color::default() }
}

// Cold, once at preroll.
#[allow(clippy::disallowed_methods)]
pub(crate) fn exported(d: &Display, formats: &[SurfaceFormat]) -> Vec<DmabufFormat> {
    formats
        .iter()
        .filter_map(|&f| ExportedSurface::probe_for(d, f).ok())
        .map(|(fourcc, modifier)| DmabufFormat { fourcc, modifiers: vec![modifier] })
        .collect()
}

struct Session {
    key: InFormat,
    proc: Proc,
    out: (u32, u32, Kind),
    src: Option<Surfaces>,
    src_size: (u32, u32),
    dst: Surfaces,
    coded: (u32, u32),
    read: Option<ReadbackImage>,
}

struct GpuIn {
    channel: Arc<GpuFrameChannel>,
    frames: Option<FrameRx>,
    link: Option<(Kind, u64)>,
    imports: Vec<Imported>,
}

struct GpuOut {
    channel: Arc<GpuFrameChannel>,
    tx: Option<FrameTx>,
    modifier: Option<u64>,
    align: u32,
    free: Vec<ffi::VASurfaceID>,
    out: Vec<(FrameToken, ffi::VASurfaceID)>,
    released: Vec<FrameToken>,
    next_token: FrameToken,
}

impl GpuOut {
    fn reclaim(&mut self) {
        let Some(tx) = self.tx.as_mut() else { return };
        tx.drain_released(&mut self.released);
        for t in self.released.drain(..) {
            if let Some(i) = self.out.iter().position(|(token, _)| *token == t) {
                self.free.push(self.out.swap_remove(i).1);
            }
        }
    }
}

enum Prepared {
    Ready(Plan, Sink),
    Wait,
    Skip,
}

enum Sink {
    Raw(Buffer, usize),
    Gpu(Buffer, ffi::VASurfaceID),
}

/// VA-API scale and convert, `video/raw` or `video/gpu` in and out.
pub struct VaapiPostProc {
    target: Target,
    fixed: bool,
    // field order is drop order, what holds the display's handle goes first
    session: Option<Session>,
    gpu_in: Option<GpuIn>,
    gpu_out: Option<GpuOut>,
    display: Option<Display>,
    disabled: bool,
    in_fmt: Option<InFormat>,
    color: StreamColor,
    announced: Option<(u32, u32, PixelFormat)>,
    stats: Arc<Stats>,
    last_pts: Option<u64>,
    period_ns: u64,
    warned: Warned,
}

impl VaapiPostProc {
    pub const IN: SinkPad = IN_PAD;
    pub const OUT: SrcPad = OUT_PAD;

    pub fn new(target: Target) -> Self {
        VaapiPostProc {
            target,
            fixed: false,
            session: None,
            gpu_in: None,
            gpu_out: None,
            display: None,
            disabled: false,
            in_fmt: None,
            color: StreamColor::default(),
            announced: None,
            stats: Arc::default(),
            last_pts: None,
            period_ns: 0,
            warned: Warned::default(),
        }
    }

    // Cold: construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new_zerocopy(target: Target, channel: Arc<GpuFrameChannel>) -> Self {
        VaapiPostProc {
            gpu_in: Some(GpuIn {
                channel,
                frames: None,
                link: None,
                imports: Vec::with_capacity(IMPORTS_MAX),
            }),
            ..Self::new(target)
        }
    }

    // Cold: construction.
    #[allow(clippy::disallowed_methods)]
    pub fn with_gpu_output(mut self, channel: Arc<GpuFrameChannel>) -> Self {
        let n = OUT_SURFACES as usize;
        let (free, out, released) =
            (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        self.gpu_out = Some(GpuOut {
            channel,
            tx: None,
            modifier: None,
            align: CODED_ALIGN,
            free,
            out,
            released,
            next_token: 1,
        });
        self
    }

    pub fn with_fixed_size(mut self) -> Self {
        self.fixed = true;
        self
    }

    pub fn with_coded_align(mut self, align: u32) -> Self {
        if let Some(g) = &mut self.gpu_out {
            g.align = align.max(2);
        }
        self
    }

    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    fn fail(ctx: &Ctx, kind: K) -> Error {
        failed(ctx, NAME, kind)
    }

    fn warn(&mut self, ctx: &mut Ctx, kind: K) {
        if !self.warned.note(ctx, kind) {
            return;
        }
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: failed(ctx, NAME, kind) });
    }

    fn display(&mut self) -> Result<&Display, K> {
        if self.display.is_none() {
            let caps = probe::probe().ok_or(K::NoDevice)?;
            self.display = Some(Display::open(&caps.device).map_err(K::DisplayOpen)?);
        }
        self.display.as_ref().ok_or(K::NoDevice)
    }

    fn plan_of(&self, f: InFormat) -> Option<Plan> {
        if self.fixed {
            plan_fixed(f.width, f.height, f.kind(), self.target)
        } else {
            plan(f.width, f.height, f.kind(), self.target)
        }
    }

    fn session(&mut self, ctx: &Ctx, f: InFormat, p: Plan) -> Result<(), K> {
        let input = f.kind();
        let (sw, sh) = ((f.width + 1) & !1, (f.height + 1) & !1);
        let out = (p.width, p.height, p.out);
        if let Some(s) = &mut self.session {
            if s.key == f {
                return Ok(());
            }
            if !s.proc.caps.fits((sw, sh), (p.width, p.height)) {
                return Err(K::ProcSize { width: f.width, height: f.height });
            }
            if s.out == out && s.proc.caps.takes(input, p.out) {
                if s.src.is_some() && s.src_size != (sw, sh) {
                    log!(ctx, Level::Debug, "src_resize", width = sw, height = sh);
                    let d = self.display.as_ref().ok_or(K::NoDevice)?;
                    s.src = Some(surfaces(d, input, sw, sh, 1).map_err(K::SessionSetup)?);
                    s.src_size = (sw, sh);
                }
                s.key = f;
                return Ok(());
            }
        }
        self.session = None;
        let (upload, gpu_out) = (self.gpu_in.is_none(), self.gpu_out.as_ref().map(|g| g.align));
        let d = self.display()?;
        let proc = Proc::new(d).map_err(K::SessionSetup)?;
        if !proc.caps.takes(input, p.out) {
            return Err(K::NoConversion { input: input.name, output: p.out.name });
        }
        if !proc.caps.fits((sw, sh), (p.width, p.height)) {
            return Err(K::ProcSize { width: f.width, height: f.height });
        }
        let src = if upload {
            Some(surfaces(d, input, sw, sh, 1).map_err(K::SessionSetup)?)
        } else {
            None
        };
        let (coded, count) = match gpu_out {
            Some(a) => ((p.width.next_multiple_of(a), p.height.next_multiple_of(a)), OUT_SURFACES),
            None => ((p.width, p.height), 1),
        };
        let dst = surfaces(d, p.out, coded.0, coded.1, count).map_err(K::SessionSetup)?;
        let read = match gpu_out {
            Some(_) => {
                // the alignment padding is coded too, black costs the least
                for &s in dst.ids() {
                    black(d, s, p.out, coded)?;
                }
                None
            }
            None => Some(
                ReadbackImage::new(d, p.out.fourcc, p.out.bpp, p.width, p.height)
                    .map_err(K::SessionSetup)?,
            ),
        };
        if let Some(g) = &mut self.gpu_out {
            g.free.clear();
            g.free.extend_from_slice(dst.ids());
            g.out.clear();
        }
        self.session =
            Some(Session { key: f, proc, out, src, src_size: (sw, sh), dst, coded, read });
        log!(
            ctx,
            Level::Info,
            "session",
            in_w = f.width,
            in_h = f.height,
            out_w = p.width,
            out_h = p.height
        );
        let (gpu_in, gpu_out) = (self.gpu_in.is_some(), self.gpu_out.is_some());
        log!(
            ctx,
            Level::Info,
            "conversion",
            input = input.name,
            output = p.out.name,
            gpu_in = gpu_in,
            gpu_out = gpu_out
        );
        Ok(())
    }

    fn announce(
        &mut self,
        ctx: &mut Ctx,
        f: InFormat,
        width: u32,
        height: u32,
        pixfmt: PixelFormat,
    ) {
        if self.announced == Some((width, height, pixfmt)) {
            return;
        }
        self.announced = Some((width, height, pixfmt));
        let mut out = [(FIELD_WIDTH, ValueDesc::Int(0)); 14];
        out[0] = (FIELD_WIDTH, ValueDesc::Int(i64::from(width)));
        out[1] = (FIELD_HEIGHT, ValueDesc::Int(i64::from(height)));
        let family = match &self.gpu_out {
            Some(g) => {
                let fourcc = if pixfmt == PixelFormat::P010 { P010.fourcc } else { NV12.fourcc };
                let layout = gpuframe::layout_of(fourcc).unwrap_or(gpuframe::LAYOUT_NV12_DMABUF);
                out[2] = (F_LAYOUT, ValueDesc::Id(layout));
                out[3] = (
                    F_MODIFIER,
                    ValueDesc::Int(g.modifier.unwrap_or(gpuframe::DRM_FORMAT_MOD_INVALID) as i64),
                );
                FAMILY_VIDEO_GPU
            }
            None => {
                out[2] = (FIELD_PIXFMT, ValueDesc::Id(pixfmt.caps_name()));
                out[3] =
                    (FIELD_FPS, ValueDesc::Rat(f.fps.map_or(0, |r| r.0), f.fps.map_or(1, |r| r.1)));
                FAMILY
            }
        };
        let fixed = if self.gpu_out.is_none() && f.fps.is_none() { 3 } else { 4 };
        let n = fixed + self.color.write_fields(&mut out[fixed..]);
        ctx.announce_format(OUT_PAD, family, &out[..n]);
    }

    fn prepare(&mut self, ctx: &mut Ctx, f: InFormat) -> Result<Prepared, Error> {
        let Some(p) = self.plan_of(f) else {
            self.warn(ctx, K::ProcSize { width: f.width, height: f.height });
            return Ok(Prepared::Skip);
        };
        if let Err(e) = self.session(ctx, f, p) {
            if matches!(e, K::ProcSize { .. }) {
                self.warn(ctx, e);
                return Ok(Prepared::Skip);
            }
            self.disabled = true;
            return Err(Self::fail(ctx, e));
        }
        let len = match self.gpu_out {
            Some(_) => GPU_FRAME_HEADER_LEN,
            None => frame_size(out_pixfmt(p.out), p.width, p.height),
        };
        let surface = match &mut self.gpu_out {
            Some(g) => {
                g.reclaim();
                let Some(&s) = g.free.last() else {
                    ctx.await_recycle();
                    return Ok(Prepared::Wait);
                };
                Some(s)
            }
            None => None,
        };
        let too_large = |ctx: &Ctx| match surface {
            Some(_) => K::HeaderSlot { need: len, capacity: ctx.slot_size() },
            None => K::FrameSlot {
                layout: p.out.name,
                width: p.width,
                height: p.height,
                need: len,
                capacity: ctx.slot_size(),
            },
        };
        let Some(buf) = ctx.try_alloc_sized(len).map_err(|_| Self::fail(ctx, too_large(ctx)))?
        else {
            ctx.await_recycle();
            return Ok(Prepared::Wait);
        };
        let sink = match surface {
            Some(s) => {
                if let Some(g) = &mut self.gpu_out {
                    g.free.pop();
                }
                Sink::Gpu(buf, s)
            }
            None => Sink::Raw(buf, len),
        };
        Ok(Prepared::Ready(p, sink))
    }

    fn process_into(
        &mut self,
        ctx: &mut Ctx,
        src: ffi::VASurfaceID,
        src_at: (u32, u32),
        f: InFormat,
        p: Plan,
        sink: Sink,
    ) -> Result<Buffer, K> {
        let color = color_of(&self.color, f.height);
        let s = self.session.as_mut().ok_or(K::NoDevice)?;
        let buf = match sink {
            Sink::Raw(mut buf, len) => {
                let dst = s.dst.ids()[0];
                s.proc
                    .run(
                        Src { surface: src, at: src_at, size: (f.width, f.height), color },
                        Out { surface: dst, place: p.place, fill: p.bars(), color },
                        None,
                    )
                    .map_err(K::Process)?;
                let read = s.read.as_mut().ok_or(K::NoDevice)?;
                let view = read.read(dst, p.width, p.height).map_err(K::Readback)?;
                let row = p.width as usize * out_pixfmt(p.out).bytes_per_sample();
                let out = &mut buf.memory.as_mut_full()[..len];
                let (y, uv) = out.split_at_mut(row * p.height as usize);
                for (r, line) in y.chunks_exact_mut(row).enumerate() {
                    let r = r as u32;
                    line.copy_from_slice(
                        view.row(0, r, row).ok_or(K::PlaneOverrun { plane: "Y", row: r })?,
                    );
                }
                for (r, line) in uv.chunks_exact_mut(row).enumerate() {
                    let r = r as u32;
                    line.copy_from_slice(
                        view.row(1, r, row).ok_or(K::PlaneOverrun { plane: "CbCr", row: r })?,
                    );
                }
                drop(view);
                buf.memory.set_len(len);
                buf
            }
            Sink::Gpu(mut buf, dst) => {
                let coded = s.coded;
                let ran = s
                    .proc
                    .run(
                        Src { surface: src, at: src_at, size: (f.width, f.height), color },
                        Out { surface: dst, place: p.place, fill: p.bars(), color },
                        None,
                    )
                    .map_err(K::Process);
                let r = ran.and_then(|_| self.send(ctx, &mut buf, dst, coded, p));
                if let Err(e) = r {
                    if let Some(g) = &mut self.gpu_out {
                        g.free.push(dst);
                    }
                    return Err(e);
                }
                buf
            }
        };
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        Ok(buf)
    }

    fn send(
        &mut self,
        ctx: &Ctx,
        header: &mut Buffer,
        dst: ffi::VASurfaceID,
        coded: (u32, u32),
        p: Plan,
    ) -> Result<(), K> {
        let dpy = self.display.as_ref().ok_or(K::NoDevice)?.raw();
        let g = self.gpu_out.as_mut().ok_or(K::NoDevice)?;
        let mut e = ExportedSurface::export(dpy, dst).map_err(K::SurfaceExport)?;
        if let Some(negotiated) = g.modifier.filter(|&m| m != e.drm_modifier) {
            return Err(K::ModifierChanged { negotiated, exported: e.drm_modifier });
        }
        let token = g.next_token;
        g.next_token = g.next_token.wrapping_add(1);
        let h = GpuFrameHeader {
            token,
            coded_w: coded.0,
            coded_h: coded.1,
            disp_w: p.width,
            disp_h: p.height,
            crop_x: 0,
            crop_y: 0,
        };
        h.write_to(&mut header.memory.as_mut_full()[..GPU_FRAME_HEADER_LEN]);
        header.memory.set_len(GPU_FRAME_HEADER_LEN);
        // the frame owns the exported fds, the export closes none of them
        let frame = GpuFrame {
            token,
            fds: std::mem::take(&mut e.fds),
            drm_format: e.drm_format,
            coded_w: e.coded_w,
            coded_h: e.coded_h,
            disp_w: p.width,
            disp_h: p.height,
            crop_x: 0,
            crop_y: 0,
            planes: std::mem::take(&mut e.planes),
            epoch: ctx.applied_seek_gen(),
            lease: None,
        };
        let tx = g.tx.as_mut().ok_or(K::ChannelTaken)?;
        if !tx.push(frame, header.memory.clone()) {
            return Err(K::ChannelFull);
        }
        g.out.push((token, dst));
        Ok(())
    }

    fn early(&mut self, ctx: &Ctx, b: &Buffer) -> bool {
        let Some(pts) = b.pts.nanos() else {
            return false;
        };
        if let Some(last) = self.last_pts.filter(|&l| pts > l) {
            self.period_ns = pts - last;
        }
        self.last_pts = Some(pts);
        if b.flags.contains(BufferFlags::PREVIEW) {
            return false;
        }
        let Some(at) = ctx.presentation_point().and_then(|t| t.nanos()) else {
            return false;
        };
        let fps = self
            .in_fmt
            .and_then(|f| f.fps)
            .filter(|&(n, d)| n > 0 && d > 0)
            .map(|(n, d)| d as u64 * 1_000_000_000 / n as u64);
        let len = b.duration.nanos().filter(|&d| d > 0).or(fps).unwrap_or(self.period_ns);
        len > 0 && pts.saturating_add(len) <= at
    }

    fn process_raw(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        while ctx.room(OUT_PAD) {
            if inputs.front().is_some_and(|b| self.early(ctx, b)) {
                if let Some(b) = inputs.pop() {
                    log!(&*ctx, Level::Trace, "early", pts = b.pts);
                }
                ctx.count_drop();
                continue;
            }
            let Some(f) = self.in_fmt else {
                if inputs.pop().is_none() {
                    break;
                }
                self.warn(ctx, K::FrameBeforeFormat);
                continue;
            };
            let identity =
                self.plan_of(f).is_some_and(|p| p.is_identity(f.width, f.height, f.kind()));
            if identity && f.pixfmt != PixelFormat::I420 && self.gpu_out.is_none() {
                let Some(buf) = inputs.pop() else { break };
                self.announce(ctx, f, f.width, f.height, f.pixfmt);
                ctx.out(OUT_PAD).push(buf);
                continue;
            }
            if inputs.front().is_none() {
                break;
            }
            let (p, sink) = match self.prepare(ctx, f)? {
                Prepared::Ready(p, sink) => (p, sink),
                Prepared::Wait => return Ok(()),
                Prepared::Skip => {
                    inputs.pop();
                    ctx.count_drop();
                    continue;
                }
            };
            let Some(inbuf) = inputs.pop() else { break };
            let need = frame_size(f.pixfmt, f.width, f.height);
            let Some(data) = inbuf.memory.data().get(..need) else {
                self.warn(ctx, K::ShortFrame { len: inbuf.memory.data().len(), need });
                continue;
            };
            let src = self
                .session
                .as_ref()
                .and_then(|s| s.src.as_ref())
                .map(|s| s.ids()[0])
                .ok_or_else(|| Self::fail(ctx, K::NoDevice))?;
            upload(
                self.display.as_ref().ok_or_else(|| Self::fail(ctx, K::NoDevice))?,
                src,
                f,
                data,
            )
            .map_err(|e| Self::fail(ctx, e))?;
            let mut out =
                self.process_into(ctx, src, (0, 0), f, p, sink).map_err(|e| Self::fail(ctx, e))?;
            (out.pts, out.dts, out.duration, out.flags) =
                (inbuf.pts, inbuf.dts, inbuf.duration, inbuf.flags);
            self.announce(ctx, f, p.width, p.height, out_pixfmt(p.out));
            ctx.out(OUT_PAD).push(out);
        }
        Ok(())
    }

    fn process_gpu(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        while ctx.room(OUT_PAD) {
            let Some(front) = inputs.front() else { break };
            let Some(h) = GpuFrameHeader::from_bytes(front.memory.data()) else {
                inputs.pop();
                self.warn(ctx, K::GpuHeader);
                continue;
            };
            let Some((kind, modifier)) = self.gpu_in.as_ref().and_then(|g| g.link) else {
                inputs.pop();
                self.warn(ctx, K::NoGpuLink);
                continue;
            };
            let f = InFormat {
                width: h.disp_w,
                height: h.disp_h,
                pixfmt: out_pixfmt(kind),
                fps: self.in_fmt.and_then(|f| f.fps),
            };
            if self.early(ctx, front) {
                log!(&*ctx, Level::Trace, "early", pts = front.pts);
                inputs.pop();
                drop(
                    self.gpu_in
                        .as_mut()
                        .and_then(|g| g.frames.as_mut())
                        .and_then(|r| r.take(h.token)),
                );
                ctx.count_drop();
                continue;
            }
            let (p, sink) = match self.prepare(ctx, f)? {
                Prepared::Ready(p, sink) => (p, sink),
                Prepared::Wait => return Ok(()),
                Prepared::Skip => {
                    inputs.pop();
                    drop(
                        self.gpu_in
                            .as_mut()
                            .and_then(|g| g.frames.as_mut())
                            .and_then(|r| r.take(h.token)),
                    );
                    ctx.count_drop();
                    continue;
                }
            };
            let Some(inbuf) = inputs.pop() else { break };
            let Some(frame) =
                self.gpu_in.as_mut().and_then(|g| g.frames.as_mut()).and_then(|r| r.take(h.token))
            else {
                self.warn(ctx, K::NoGpuFrame { token: h.token });
                continue;
            };
            let src = self.imported(&frame, kind, modifier).map_err(|e| Self::fail(ctx, e))?;
            let mut out = self
                .process_into(ctx, src, (h.crop_x, h.crop_y), f, p, sink)
                .map_err(|e| Self::fail(ctx, e))?;
            drop(frame);
            (out.pts, out.dts, out.duration, out.flags) =
                (inbuf.pts, inbuf.dts, inbuf.duration, inbuf.flags);
            self.announce(ctx, f, p.width, p.height, out_pixfmt(p.out));
            ctx.out(OUT_PAD).push(out);
        }
        Ok(())
    }

    fn imported(
        &mut self,
        frame: &GpuFrame,
        kind: Kind,
        modifier: u64,
    ) -> Result<ffi::VASurfaceID, K> {
        let key = dmabuf_key(frame)?;
        let display = self.display.as_ref().ok_or(K::NoDevice)?;
        let gpu = self.gpu_in.as_mut().ok_or(K::NoGpuLink)?;
        if let Some(i) = gpu.imports.iter().find(|i| i.key == key) {
            return Ok(i.id);
        }
        if gpu.imports.len() >= IMPORTS_MAX {
            // The GPU may still read the last one, the destroy waits for it.
            for i in &gpu.imports {
                let _ = crate::va::sync_surface(display, i.id);
            }
            gpu.imports.clear();
        }
        let i = import(display, frame, kind, modifier)?;
        let id = i.id;
        gpu.imports.push(i);
        self.stats.imports.fetch_add(1, Ordering::Relaxed);
        Ok(id)
    }
}

fn black(
    display: &Display,
    surface: ffi::VASurfaceID,
    kind: Kind,
    (w, h): (u32, u32),
) -> Result<(), K> {
    let wide = kind.depth > 8;
    let mut img =
        UploadImage::acquire_for(display, surface, w, h, kind.surface()).map_err(K::Upload)?;
    let row = w as usize * if wide { 2 } else { 1 };
    let fill = |line: &mut [u8], v: u8| match wide {
        true => line
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|s| *s = (u16::from(v) << 8).to_le_bytes()),
        false => line.fill(v),
    };
    for r in 0..h {
        fill(img.row_mut(0, r, row).ok_or(K::PlaneOverrun { plane: "Y", row: r })?, 16);
    }
    for r in 0..h / 2 {
        fill(img.row_mut(1, r, row).ok_or(K::PlaneOverrun { plane: "CbCr", row: r })?, 128);
    }
    img.finish().map_err(K::Upload)
}

fn upload(display: &Display, surface: ffi::VASurfaceID, f: InFormat, data: &[u8]) -> Result<(), K> {
    let short = K::ShortFrame { len: data.len(), need: frame_size(f.pixfmt, f.width, f.height) };
    let geom = |p| {
        plane_geometry(f.pixfmt, f.width, f.height, p)
            .zip(plane_offset(f.pixfmt, f.width, f.height, p))
            .ok_or(short)
    };
    let (w, h) = ((f.width + 1) & !1, (f.height + 1) & !1);
    let mut img =
        UploadImage::acquire_for(display, surface, w, h, f.kind().surface()).map_err(K::Upload)?;
    let (yg, yo) = geom(0)?;
    for r in 0..yg.height {
        let src = data.get(yo + r * yg.stride..yo + (r + 1) * yg.stride).ok_or(short)?;
        let row = r as u32;
        img.row_mut(0, row, yg.stride)
            .ok_or(K::PlaneOverrun { plane: "Y", row })?
            .copy_from_slice(src);
    }
    let (cg, co) = geom(1)?;
    match f.pixfmt {
        PixelFormat::I420 => {
            let (vg, vo) = geom(2)?;
            for r in 0..cg.height {
                let u = data.get(co + r * cg.stride..co + (r + 1) * cg.stride).ok_or(short)?;
                let v = data.get(vo + r * vg.stride..vo + (r + 1) * vg.stride).ok_or(short)?;
                let row = r as u32;
                let dst = img
                    .row_mut(1, row, cg.stride * 2)
                    .ok_or(K::PlaneOverrun { plane: "CbCr", row })?;
                for ((d, &u), &v) in dst.as_chunks_mut::<2>().0.iter_mut().zip(u).zip(v) {
                    *d = [u, v];
                }
            }
        }
        _ => {
            for r in 0..cg.height {
                let src = data.get(co + r * cg.stride..co + (r + 1) * cg.stride).ok_or(short)?;
                let row = r as u32;
                img.row_mut(1, row, cg.stride)
                    .ok_or(K::PlaneOverrun { plane: "CbCr", row })?
                    .copy_from_slice(src);
            }
        }
    }
    img.finish().map_err(K::Upload)
}

// SAFETY: an Active element runs on one scheduler thread at a time and never shares its
// display. The display opened at preroll may be used from a later scheduler thread, which libva
// permits, as nothing in it is thread affine.
unsafe impl Send for VaapiPostProc {}

impl Element for VaapiPostProc {
    fn desc(&self) -> &'static ElementDesc {
        &DESCS[usize::from(self.gpu_in.is_some())][usize::from(self.gpu_out.is_some())]
    }

    // Cold, once at preroll.
    #[allow(clippy::disallowed_methods)]
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.gpu_in.is_none() && self.gpu_out.is_none() {
            return Ok(());
        }
        let formats = match self.display() {
            Ok(d) => exported(d, &[SurfaceFormat::NV12, SurfaceFormat::P010]),
            Err(e) => {
                self.warn(ctx, e);
                Vec::new()
            }
        };
        if self.gpu_in.is_some() {
            gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_sink(IN_PAD, offers))?;
        }
        if self.gpu_out.is_some() {
            gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_src(OUT_PAD, offers))?;
        }
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let link = read_link(ctx, ctx.negotiated_sink(IN_PAD));
        if let Some(g) = &mut self.gpu_in {
            g.link = link.or(g.link);
            if g.frames.is_none() {
                g.frames =
                    Some(g.channel.take_rx().ok_or_else(|| Self::fail(ctx, K::ReceiverTaken))?);
            }
        }
        let out_link = read_link(ctx, ctx.negotiated_src(OUT_PAD));
        if let Some(g) = &mut self.gpu_out {
            g.modifier = out_link.map(|l| l.1).or(g.modifier);
            if g.tx.is_none() {
                g.tx = Some(g.channel.take_tx().ok_or_else(|| Self::fail(ctx, K::ChannelTaken))?);
            }
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.disabled {
            while inputs.pop().is_some() {}
            return Ok(Flow::Ok);
        }
        if self.in_fmt.is_none()
            && let Some(f) = ctx.negotiated_sink(IN_PAD).cloned()
        {
            self.in_fmt = read_format(ctx, &f);
            self.color = StreamColor::read(ctx, &f);
        }
        if self.gpu_in.is_some() {
            self.process_gpu(ctx, &mut inputs)?;
        } else {
            self.process_raw(ctx, &mut inputs)?;
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(f) => {
                self.in_fmt = read_format(ctx, f);
                let (w, h) = self.in_fmt.map_or((0, 0), |f| (f.width, f.height));
                log!(
                    &*ctx,
                    Level::Debug,
                    "format_change",
                    width = w,
                    height = h,
                    known = self.in_fmt.is_some()
                );
                self.color = StreamColor::read(ctx, f);
                if !self.fixed {
                    self.announced = None;
                }
                if let Some(g) = &mut self.gpu_in {
                    g.link = read_link(ctx, Some(f)).or(g.link);
                }
            }
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush");
                self.warned.summary(ctx, "flush");
                self.last_pts = None;
                if let Some(r) = self.gpu_in.as_mut().and_then(|g| g.frames.as_mut()) {
                    r.flush_before(ctx.applied_seek_gen());
                }
            }
            Event::Eos => {
                let frames = self.stats.frames.load(Ordering::Relaxed);
                let imports = self.stats.imports.load(Ordering::Relaxed);
                log!(&*ctx, Level::Info, "eos", frames = frames, imports = imports);
                self.warned.summary(ctx, "eos");
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.session = None;
        self.announced = None;
        if let Some(g) = &mut self.gpu_in {
            g.imports.clear();
            if let Some(r) = g.frames.take() {
                g.channel.put_rx(r);
            }
        }
        if let Some(g) = &mut self.gpu_out {
            g.free.clear();
            g.out.clear();
            if let Some(tx) = g.tx.take() {
                g.channel.put_tx(tx);
            }
        }
    }
}
