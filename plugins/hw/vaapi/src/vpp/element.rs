//! `vaapipostproc`, frames scaled and converted through the VA processor to nv12 or p010.
//! `video/raw` or `video/gpu` in and out. Frames wholly before a seek's presentation point are
//! dropped. A PQ or HLG input to nv12 is tone mapped to BT.709 SDR by the driver's HDR10 filter.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{
    ConstraintDesc, FieldDesc, FixedFormat, OfferDesc, OfferSpec, Value, ValueDesc,
};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::{Memory, SideError};
use profluens_video::color::{
    ContentLight, HdrStatic, Mastering, Matrix, Primaries, Range, StreamColor, TransferFn,
};
use profluens_video::format::{
    COLOR_OFFER_FIELDS, FAMILY, FIELD_FPS, FIELD_HEIGHT, FIELD_PIXFMT, FIELD_WIDTH, PixelFormat,
};
use profluens_video::geometry::{frame_size, plane_geometry, plane_offset};

use crate::error::{VaapiErrorKind as K, Warned, failed};
use crate::ffi;
use crate::gpuframe::{
    self, DmabufFormat, F_LAYOUT, F_MODIFIER, FAMILY_VIDEO_GPU, FRAMES_OUT, FrameRx, FrameToken,
    FrameTx, GPU_FRAME_HEADER_LEN, GpuFrame, GpuFrameChannel, GpuFrameHeader, Handoff,
};
use crate::probe;
use crate::va::{Display, ExportedSurface, ReadbackImage, SurfaceFormat, Surfaces, UploadImage};
use crate::vpp::format::{self, Kind, NV12, P010};
use crate::vpp::linear::linear_imports;
use crate::vpp::plan::{Plan, Target, shown};
use crate::vpp::proc::{
    Color, Hdr10, Imported, Out, Proc, Src, ToneMap, dmabuf_key, import, layout_key, surfaces,
};

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

pub(crate) static DESCS: [[ElementDesc; 2]; 2] = [
    [desc(&RAW_SINKS, &RAW_SRCS, Some(make_default)), desc(&RAW_SINKS, &GPU_SRCS, None)],
    [desc(&GPU_SINKS, &RAW_SRCS, None), desc(&GPU_SINKS, &GPU_SRCS, None)],
];

const IMPORTS_MAX: usize = FRAMES_OUT;
const OUT_SURFACES: u32 = 8;
const CODED_ALIGN: u32 = 16;

pub use crate::stats::Stats;

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

/// Whether `c` is PQ or HLG.
fn is_hdr(c: &StreamColor) -> bool {
    c.transfer.is_some_and(TransferFn::is_hdr)
}

/// The colour a tone mapped picture states, BT.709 limited, no HDR metadata.
fn sdr() -> StreamColor {
    StreamColor {
        matrix: Some(Matrix::Bt709),
        range: Some(Range::Limited),
        transfer: Some(TransferFn::Bt709),
        primaries: Some(Primaries::Bt709),
        ..StreamColor::default()
    }
}

/// The filter's input, the stream's own, else a 1000 cd/m2 BT.2020 display. Zeros map every
/// sample to black.
fn hdr10_of(h: &HdrStatic) -> Hdr10 {
    let m = match h.mastering == Mastering::default() {
        true => Mastering {
            primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
            white: [15635, 16450],
            max_luminance: 10_000_000,
            min_luminance: 1,
        },
        false => h.mastering,
    };
    let l = match h.light == ContentLight::default() {
        true => ContentLight { max_cll: 1000, max_fall: 400 },
        false => h.light,
    };
    Hdr10 {
        primaries: m.primaries,
        white: m.white,
        max_luminance: m.max_luminance,
        min_luminance: m.min_luminance,
        max_cll: l.max_cll,
        max_fall: l.max_fall,
    }
}

/// The source colour the filter reads, every code point stated, unknowns as BT.2020.
fn explicit_of(c: &StreamColor) -> Color {
    let range = match c.range {
        Some(Range::Full) => ffi::VA_SOURCE_RANGE_FULL,
        _ => ffi::VA_SOURCE_RANGE_REDUCED,
    };
    Color::explicit(
        c.primaries.map_or(9, Primaries::to_h273),
        c.transfer.map_or(16, TransferFn::to_h273),
        c.matrix.map_or(9, Matrix::to_h273),
        range,
    )
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
    /// The driver has the HDR10 to SDR filter.
    can_tone: bool,
    /// The filter and the metadata it was made from, kept once the driver ran it.
    tone: Option<(ToneMap, HdrStatic)>,
}

struct GpuIn {
    channel: Arc<GpuFrameChannel>,
    frames: Option<FrameRx>,
    link: Option<(Kind, u64)>,
    imports: Vec<Imported>,
    /// Raw pictures or linear dmabufs, whichever the link fixed, see `with_dmabuf_input`.
    either: bool,
    /// The driver refused a linear import, the CPU copies each picture in from then on.
    cpu: bool,
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
    /// Degrees clockwise each picture is turned.
    rotation: u16,
    /// The pixel aspect ratio of the pictures in, they come out square.
    par: (u32, u32),
    // field order is drop order, what holds the display's handle goes first
    session: Option<Session>,
    /// A processor made and warmed at start, the first session takes it.
    warm: Option<Proc>,
    warm_asked: bool,
    gpu_in: Option<GpuIn>,
    gpu_out: Option<GpuOut>,
    display: Option<Display>,
    disabled: bool,
    in_fmt: Option<InFormat>,
    color: StreamColor,
    /// The source transfer the driver refused to tone map, not tried again on a new session.
    tone_refused: Option<TransferFn>,
    announced: Option<(u32, u32, PixelFormat, StreamColor)>,
    stats: Arc<Stats>,
    last_pts: Option<u64>,
    period_ns: u64,
    warned: Warned,
    /// Every tone mapped run counts as refused.
    #[cfg(test)]
    refuse_tone: bool,
}

impl VaapiPostProc {
    pub const IN: SinkPad = IN_PAD;
    pub const OUT: SrcPad = OUT_PAD;

    pub fn new(target: Target) -> Self {
        VaapiPostProc {
            target,
            fixed: false,
            rotation: 0,
            par: (1, 1),
            session: None,
            warm: None,
            warm_asked: false,
            gpu_in: None,
            gpu_out: None,
            display: None,
            disabled: false,
            in_fmt: None,
            color: StreamColor::default(),
            tone_refused: None,
            announced: None,
            stats: Arc::default(),
            last_pts: None,
            period_ns: 0,
            warned: Warned::default(),
            #[cfg(test)]
            refuse_tone: false,
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
                either: false,
                cpu: false,
            }),
            ..Self::new(target)
        }
    }

    /// Takes `video/raw` or linear `video/gpu` frames from `channel`, a software decoder's udmabuf
    /// slots in dmabuf mode. The dmabuf link is offered only where [`linear_imports`] passed, and
    /// a picture the driver refuses is copied in by the CPU, so the stream never stops for it.
    ///
    /// [`linear_imports`]: crate::vpp::linear_imports
    // Cold: construction.
    #[allow(clippy::disallowed_methods)]
    pub fn with_dmabuf_input(mut self, channel: Arc<GpuFrameChannel>) -> Self {
        self.gpu_in = Some(GpuIn {
            channel,
            frames: None,
            link: None,
            imports: Vec::with_capacity(IMPORTS_MAX),
            either: true,
            cpu: false,
        });
        self
    }

    /// Whether frames come as dmabufs, the link fixed `video/gpu`.
    fn gpu_linked(&self) -> bool {
        self.gpu_in.as_ref().is_some_and(|g| !g.either || g.link.is_some())
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

    /// Turns every picture `degrees` clockwise, 90, 180 or 270, the box fit to the turned one.
    pub fn with_rotation(mut self, degrees: u16) -> Self {
        self.rotation = degrees;
        self
    }

    /// Pictures in of pixel aspect `num`:`den` come out square, stretched to the shape they show.
    pub fn with_par(mut self, num: u32, den: u32) -> Self {
        self.par = (num, den);
        self
    }

    /// Makes the processor at start and runs the tone map once, off the first picture's path.
    /// iHD sets a context's tone map up on its first picture, 12 ms on Alder Lake-N, 33 ms the
    /// first time in a process.
    pub fn with_warm_tone_map(mut self) -> Self {
        self.warm_asked = true;
        self
    }

    /// The processor, its tone map run once on a target sized picture.
    fn warm_up(&mut self) -> Result<(), K> {
        let (w, h) = (self.target.max_width.max(64), self.target.max_height.max(64));
        let d = self.display()?;
        let proc = Proc::new(d).map_err(K::SessionSetup)?;
        if proc.tone_maps() {
            let src = surfaces(d, P010, w, h, 1).map_err(K::SessionSetup)?;
            let dst = surfaces(d, NV12, w, h, 1).map_err(K::SessionSetup)?;
            let tm = proc.tone_map(&hdr10_of(&HdrStatic::default())).map_err(K::SessionSetup)?;
            let pq = Color::explicit(9, 16, 9, ffi::VA_SOURCE_RANGE_REDUCED);
            let out = Out {
                surface: dst.ids()[0],
                place: (0, 0, w, h),
                fill: false,
                color: Color::SDR,
                rotation: 0,
            };
            proc.run(
                Src { surface: src.ids()[0], at: (0, 0), size: (w, h), color: pq },
                out,
                Some(&tm),
            )
            .map_err(K::Process)?;
            crate::va::sync_surface(d, dst.ids()[0]).map_err(K::Process)?;
        }
        self.warm = Some(proc);
        Ok(())
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

    /// Whether pictures to `out` are tone mapped, a PQ or HLG input to nv12 on a driver with
    /// the filter that has not refused the input's transfer.
    fn maps_tones(&self, out: Kind) -> bool {
        out == NV12
            && is_hdr(&self.color)
            && self.tone_refused != self.color.transfer
            && self.session.as_ref().is_some_and(|s| s.can_tone)
    }

    /// The colour the output states, BT.709 when tone mapped, else the input's.
    fn out_color(&self, out: Kind) -> StreamColor {
        if self.maps_tones(out) { sdr() } else { self.color }
    }

    fn plan_of(&self, f: InFormat) -> Option<Plan> {
        let (w, h) = shown(f.width, f.height, self.par);
        Plan::turned(w, h, f.kind(), self.target, self.rotation, self.fixed)
    }

    fn session(&mut self, ctx: &Ctx, f: InFormat, p: Plan) -> Result<(), K> {
        let input = f.kind();
        let (sw, sh) = ((f.width + 1) & !1, (f.height + 1) & !1);
        let out = (p.width, p.height, p.out);
        // iHD keeps the tone map's state on the context, a plain pass after it comes out
        // shifted, so a source gone SDR on a session that mapped gets a new one
        let maps = self.maps_tones(p.out);
        if self.session.as_ref().is_some_and(|s| s.tone.is_some() && !maps) {
            log!(ctx, Level::Debug, "tone_off", width = f.width, height = f.height);
            self.session = None;
        }
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
        // a dmabuf link keeps the upload surface for pictures the driver refuses to import
        let upload = self.gpu_in.as_ref().is_none_or(|g| g.either);
        let gpu_out = self.gpu_out.as_ref().map(|g| g.align);
        let warm = self.warm.take();
        let d = self.display()?;
        let proc = match warm {
            Some(p) => p,
            None => Proc::new(d).map_err(K::SessionSetup)?,
        };
        if !proc.caps.turns(p.rotation) {
            return Err(K::DriverLacks(match p.rotation {
                90 => "a turn of 90 degrees",
                180 => "a turn of 180 degrees",
                _ => "a turn of 270 degrees",
            }));
        }
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
        // every pass paints the bars and alignment rows with the background colour, the CPU
        // fills them once where the driver paints none
        if p.bars() && !proc.paints_background(d) {
            for &s in dst.ids() {
                black(d, s, p.out, coded)?;
            }
        }
        let read = match gpu_out {
            Some(_) => None,
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
        let can_tone = proc.tone_maps();
        self.stats.made(count + u32::from(src.is_some()), 0);
        self.session = Some(Session {
            key: f,
            proc,
            out,
            src,
            src_size: (sw, sh),
            dst,
            coded,
            read,
            can_tone,
            tone: None,
        });
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
        let kind = if pixfmt == PixelFormat::P010 { P010 } else { NV12 };
        // the colour too, a fixed size output still tells the encoder of new HDR metadata
        let color = self.out_color(kind);
        if self.announced == Some((width, height, pixfmt, color)) {
            return;
        }
        self.announced = Some((width, height, pixfmt, color));
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
        let n = fixed + color.write_fields(&mut out[fixed..]);
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
        let refused = self.tone_refused == self.color.transfer;
        if p.out == NV12 && is_hdr(&self.color) && !refused && !self.maps_tones(p.out) {
            // the samples pass as they are, the picture looks washed out. A refusal warned once.
            self.warn(ctx, K::DriverLacks("HDR10 to SDR tone mapping"));
        }
        let len = match self.gpu_out {
            Some(_) => GPU_FRAME_HEADER_LEN,
            None => frame_size(out_pixfmt(p.out), p.width, p.height),
        };
        let surface = match &mut self.gpu_out {
            Some(g) => {
                g.reclaim();
                let Some(&s) = g.free.last() else {
                    log!(&*ctx, Level::Trace, "wait_surface", out = g.out.len() as u64);
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
            log!(&*ctx, Level::Trace, "wait_slot");
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

    /// `side` is the input picture's side records, the output carries them unless tone mapped.
    #[allow(clippy::too_many_arguments)]
    fn process_into(
        &mut self,
        ctx: &mut Ctx,
        src: ffi::VASurfaceID,
        src_at: (u32, u32),
        f: InFormat,
        p: Plan,
        sink: Sink,
        side: &[u8],
    ) -> Result<Buffer, K> {
        let buf = match sink {
            Sink::Raw(mut buf, len) => {
                let dst = self.session.as_ref().ok_or(K::NoDevice)?.dst.ids()[0];
                self.convert(ctx, src, src_at, f, p, dst)?;
                let s = self.session.as_mut().ok_or(K::NoDevice)?;
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
                self.carry_side(ctx, side, &mut buf.memory, p.out);
                buf
            }
            Sink::Gpu(mut buf, dst) => {
                let coded = self.session.as_ref().ok_or(K::NoDevice)?.coded;
                let ran = self.convert(ctx, src, src_at, f, p, dst);
                if ran.is_ok() {
                    // before the send, so the view the channel holds states it too
                    self.carry_side(ctx, side, &mut buf.memory, p.out);
                }
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

    /// After [`Self::convert`], so a tone map the driver refused counts as none.
    fn carry_side(&self, ctx: &Ctx, side: &[u8], to: &mut Memory, out: Kind) {
        if carry_side(side, to, self.maps_tones(out)).is_err() {
            log!(ctx, Level::Debug, "side_refused", bytes = side.len() as u64);
        }
    }

    /// `src` into `dst` by plan `p`, tone mapped when [`Self::maps_tones`]. A tone map the
    /// driver refuses is warned once and the element converts that transfer without it from
    /// then on, the session kept unless it mapped before.
    fn convert(
        &mut self,
        ctx: &mut Ctx,
        src: ffi::VASurfaceID,
        src_at: (u32, u32),
        f: InFormat,
        p: Plan,
        dst: ffi::VASurfaceID,
    ) -> Result<(), K> {
        let maps = self.maps_tones(p.out);
        let s = self.session.as_mut().ok_or(K::NoDevice)?;
        let place = |color| Out {
            surface: dst,
            place: p.place,
            fill: p.bars(),
            color,
            rotation: p.va_rotation(),
        };
        let size = (f.width, f.height);
        if maps {
            let hdr = self.color.hdr;
            let fresh = match &s.tone {
                Some((_, h)) if *h == hdr => None,
                _ => Some(s.proc.tone_map(&hdr10_of(&hdr)).map_err(K::SessionSetup)?),
            };
            let color = explicit_of(&self.color);
            let tone = fresh.as_ref().or(s.tone.as_ref().map(|(t, _)| t));
            let ran = s
                .proc
                .run(Src { surface: src, at: src_at, size, color }, place(Color::SDR), tone)
                .is_ok();
            #[cfg(test)]
            let ran = ran && !self.refuse_tone;
            if ran {
                if let Some(tm) = fresh {
                    s.tone = Some((tm, hdr));
                }
                return Ok(());
            }
            // iHD 26.1 on Gen9 refuses an HLG source this way. A session that never mapped
            // holds no tone state and is kept, one that did is made anew by the next picture.
            self.tone_refused = self.color.transfer;
            self.warn(ctx, K::DriverLacks("HDR to SDR tone mapping of this source"));
        }
        let s = self.session.as_mut().ok_or(K::NoDevice)?;
        let color = color_of(&self.color, f.height);
        s.proc
            .run(Src { surface: src, at: src_at, size, color }, place(color), None)
            .map_err(K::Process)
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
        // a queued handoff exports the picture as it is, the dma-buf's fence orders the consumer,
        // else the export waits for it
        let export = match g.channel.handoff() {
            Handoff::Queued => ExportedSurface::export_queued,
            Handoff::Done => ExportedSurface::export,
        };
        let t = crate::stats::mark();
        let mut e = export(dpy, dst).map_err(K::SurfaceExport)?;
        if g.channel.handoff() == Handoff::Done {
            self.stats.waited(t);
        }
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
        // a seek's flush waits behind no picture it would discard
        while ctx.room(OUT_PAD) && !ctx.command_pending() {
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
            let mut out = self
                .process_into(ctx, src, (0, 0), f, p, sink, inbuf.memory.side())
                .map_err(|e| Self::fail(ctx, e))?;
            (out.pts, out.dts, out.duration, out.flags) =
                (inbuf.pts, inbuf.dts, inbuf.duration, inbuf.flags);
            self.announce(ctx, f, p.width, p.height, out_pixfmt(p.out));
            ctx.out(OUT_PAD).push(out);
            // the picture goes downstream now, the next would hold it back
            break;
        }
        Ok(())
    }

    fn process_gpu(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        if let Some(b) = inputs.front() {
            log!(&*ctx, Level::Trace, "run", pts = b.pts, queued = inputs.len() as u64);
        }
        // a seek's flush waits behind no picture it would discard
        while ctx.room(OUT_PAD) && !ctx.command_pending() {
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
            let src =
                self.source(ctx, &frame, f, kind, modifier).map_err(|e| Self::fail(ctx, e))?;
            let mut out = self
                .process_into(ctx, src, (h.crop_x, h.crop_y), f, p, sink, inbuf.memory.side())
                .map_err(|e| Self::fail(ctx, e))?;
            drop(frame);
            (out.pts, out.dts, out.duration, out.flags) =
                (inbuf.pts, inbuf.dts, inbuf.duration, inbuf.flags);
            log!(&*ctx, Level::Trace, "processed", pts = out.pts);
            self.announce(ctx, f, p.width, p.height, out_pixfmt(p.out));
            ctx.out(OUT_PAD).push(out);
            // the picture goes downstream now, the next would hold it back
            break;
        }
        Ok(())
    }

    /// The surface `frame` is read from, imported, or on a linear link the upload surface the
    /// CPU copied it into once the driver refused an import.
    fn source(
        &mut self,
        ctx: &Ctx,
        frame: &GpuFrame,
        f: InFormat,
        kind: Kind,
        modifier: u64,
    ) -> Result<ffi::VASurfaceID, K> {
        let (either, cpu) = self.gpu_in.as_ref().map_or((false, false), |g| (g.either, g.cpu));
        if !cpu {
            match self.imported(frame, kind, modifier) {
                Ok(id) => return Ok(id),
                Err(e) if either => {
                    log!(ctx, Level::Warn, "import_refused", layout = kind.name, err = e.name());
                    if let Some(g) = &mut self.gpu_in {
                        g.cpu = true;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let display = self.display.as_ref().ok_or(K::NoDevice)?;
        let src = self.session.as_ref().and_then(|s| s.src.as_ref()).ok_or(K::NoDevice)?.ids()[0];
        copy_in(display, src, frame, f)?;
        Ok(src)
    }

    fn imported(
        &mut self,
        frame: &GpuFrame,
        kind: Kind,
        modifier: u64,
    ) -> Result<ffi::VASurfaceID, K> {
        let key = dmabuf_key(frame)?;
        let shape = layout_key(frame);
        let display = self.display.as_ref().ok_or(K::NoDevice)?;
        let gpu = self.gpu_in.as_mut().ok_or(K::NoGpuLink)?;
        if let Some(i) = gpu.imports.iter().find(|i| i.key == key && i.shape == shape) {
            return Ok(i.id);
        }
        // a pool slot laid out anew, its old surface goes once the GPU is done with it
        if let Some(at) = gpu.imports.iter().position(|i| i.key == key) {
            let _ = crate::va::sync_surface(display, gpu.imports[at].id);
            gpu.imports.swap_remove(at);
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

/// `side` onto `to`, none for a tone mapped picture, whose HDR metadata went with its PQ.
fn carry_side(side: &[u8], to: &mut Memory, tone_mapped: bool) -> Result<(), SideError> {
    if side.is_empty() || tone_mapped {
        return Ok(());
    }
    to.set_side(side)
}

pub(crate) fn black(
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
    let mut planes = [(0, 0); 3];
    for (p, plane) in planes.iter_mut().enumerate().take(f.pixfmt.plane_count()) {
        let g = plane_geometry(f.pixfmt, f.width, f.height, p).ok_or(short)?;
        *plane = (plane_offset(f.pixfmt, f.width, f.height, p).ok_or(short)?, g.stride);
    }
    upload_planes(display, surface, f, data, &planes)
}

/// `f`'s planes at `(offset, pitch)` in `data` into `surface`, I420 interleaved to NV12.
fn upload_planes(
    display: &Display,
    surface: ffi::VASurfaceID,
    f: InFormat,
    data: &[u8],
    planes: &[(usize, usize); 3],
) -> Result<(), K> {
    let short = K::ShortFrame { len: data.len(), need: frame_size(f.pixfmt, f.width, f.height) };
    // rows as tight as the format lays them, `pitch` apart
    let geom = |p: usize| {
        let g = plane_geometry(f.pixfmt, f.width, f.height, p).ok_or(short)?;
        let (at, pitch) = planes[p];
        if pitch < g.stride {
            return Err(short);
        }
        Ok((g.height, g.stride, at, pitch))
    };
    let row_of = |at: usize, pitch: usize, len: usize, r: usize| {
        data.get(at + r * pitch..at + r * pitch + len).ok_or(short)
    };
    let (w, h) = ((f.width + 1) & !1, (f.height + 1) & !1);
    let mut img =
        UploadImage::acquire_for(display, surface, w, h, f.kind().surface()).map_err(K::Upload)?;
    let (rows, len, at, pitch) = geom(0)?;
    for r in 0..rows {
        let src = row_of(at, pitch, len, r)?;
        let row = r as u32;
        img.row_mut(0, row, len).ok_or(K::PlaneOverrun { plane: "Y", row })?.copy_from_slice(src);
    }
    let (rows, len, co, cp) = geom(1)?;
    match f.pixfmt {
        PixelFormat::I420 => {
            let (_, _, vo, vp) = geom(2)?;
            for r in 0..rows {
                let u = row_of(co, cp, len, r)?;
                let v = row_of(vo, vp, len, r)?;
                let row = r as u32;
                let dst =
                    img.row_mut(1, row, len * 2).ok_or(K::PlaneOverrun { plane: "CbCr", row })?;
                for ((d, &u), &v) in dst.as_chunks_mut::<2>().0.iter_mut().zip(u).zip(v) {
                    *d = [u, v];
                }
            }
        }
        _ => {
            for r in 0..rows {
                let src = row_of(co, cp, len, r)?;
                let row = r as u32;
                img.row_mut(1, row, len)
                    .ok_or(K::PlaneOverrun { plane: "CbCr", row })?
                    .copy_from_slice(src);
            }
        }
    }
    img.finish().map_err(K::Upload)
}

/// A linear frame's dmabuf mapped and copied into `surface`, for a picture the driver would not
/// import.
fn copy_in(
    display: &Display,
    surface: ffi::VASurfaceID,
    frame: &GpuFrame,
    f: InFormat,
) -> Result<(), K> {
    let fd = frame.fd(0).ok_or(K::Dmabuf { errno: libc::EBADF })?;
    let errno = || K::Dmabuf { errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0) };
    // SAFETY: lseek on the frame's own fd, a dma-buf reports its size at SEEK_END.
    let len = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    if len <= 0 {
        return Err(errno());
    }
    let len = len as usize;
    // SAFETY: a read-only shared map of the whole dma-buf, unmapped below before returning.
    let base = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0)
    };
    if base == libc::MAP_FAILED {
        return Err(errno());
    }
    // SAFETY: the mapping above, `len` bytes, alive until the munmap.
    let data = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
    let mut planes = [(0, 0); 3];
    for (p, plane) in planes.iter_mut().zip(frame.planes.iter()) {
        *p = (plane.offset as usize, plane.pitch as usize);
    }
    let copied = upload_planes(display, surface, f, data, &planes);
    // SAFETY: the mapping made above, `data` is not used past here.
    unsafe { libc::munmap(base, len) };
    copied
}

// SAFETY: the element runs on one scheduler thread at a time, its own when Active or its
// upstream's when Passive, and never shares its display. The display opened at preroll may be
// used from a later scheduler thread, which libva permits, as nothing in it is thread affine.
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
        let either = self.gpu_in.as_ref().is_some_and(|g| g.either);
        let tiled = self.gpu_out.is_some() || self.gpu_in.as_ref().is_some_and(|g| !g.either);
        let formats = match tiled.then(|| self.display()) {
            Some(Ok(d)) => exported(d, &[SurfaceFormat::NV12, SurfaceFormat::P010]),
            Some(Err(e)) => {
                self.warn(ctx, e);
                Vec::new()
            }
            None => Vec::new(),
        };
        if either {
            // dmabufs first so a decoder offering both hands its slots over
            let linear = match linear_imports() {
                Ok(l) => l,
                Err(e) => {
                    log!(&*ctx, Level::Debug, "linear_refused", err = e.name());
                    &[]
                }
            };
            gpuframe::with_gpu_offers(linear, |gpu| {
                let mut all: Vec<OfferSpec<'_>> = gpu.to_vec();
                all.extend_from_slice(&IN_OFFERS);
                ctx.offer_sink(IN_PAD, &all)
            })?;
        } else if self.gpu_in.is_some() {
            gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_sink(IN_PAD, offers))?;
        }
        if self.gpu_out.is_some() {
            gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_src(OUT_PAD, offers))?;
        }
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.warm_asked && self.warm.is_none() && self.session.is_none() {
            match self.warm_up() {
                Ok(()) => log!(&*ctx, Level::Debug, "warm"),
                Err(e) => self.warn(ctx, e),
            }
        }
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
        if self.gpu_linked() {
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
                // a fixed size output announces again only on a new colour, see `announce`
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

#[cfg(test)]
// Test fixtures.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use pf_harness::Harness;
    use profluens_core::memory::Pool;

    /// Whether the device has the processor, and the HDR10 to SDR filter.
    fn proc_device(test: &str) -> Option<bool> {
        let Some(caps) = probe::probe() else {
            eprintln!("skip {test}: no VA-API device");
            return None;
        };
        if !caps.video_proc {
            eprintln!("skip {test}: no VAEntrypointVideoProc");
            return None;
        }
        let d = Display::open(&caps.device).expect("the probed device opens");
        Some(Proc::new(&d).expect("a processing context").tone_maps())
    }

    fn hdr10(max_cll: u16) -> StreamColor {
        StreamColor {
            matrix: Some(Matrix::Bt2020),
            range: Some(Range::Limited),
            transfer: Some(TransferFn::Pq),
            primaries: Some(Primaries::Bt2020),
            hdr: HdrStatic {
                light: ContentLight { max_cll, max_fall: 400 },
                ..HdrStatic::default()
            },
            ..StreamColor::default()
        }
    }

    const W: u32 = 128;
    const H: u32 = 64;

    fn fix(hn: &mut Harness, c: &StreamColor, change: bool) {
        let mut f = [(FIELD_WIDTH, ValueDesc::Int(i64::from(W))); 16];
        f[1] = (FIELD_HEIGHT, ValueDesc::Int(i64::from(H)));
        f[2] = (FIELD_PIXFMT, ValueDesc::Id("p010"));
        let n = 3 + c.write_fields(&mut f[3..]);
        match change {
            true => hn.change_sink_format(IN_PAD, FAMILY, &f[..n]).expect("the change"),
            false => hn.fix_sink_format(IN_PAD, FAMILY, &f[..n]),
        }
    }

    /// A mid grey P010 picture through, its output pulled.
    fn one(hn: &mut Harness) {
        let grey = (512u16 << 6).to_le_bytes();
        let frame: Vec<u8> = grey.iter().copied().cycle().take((W * H * 3) as usize).collect();
        let b = hn.alloc(&frame);
        hn.push(b).expect("pushed");
        assert!(hn.pull(OUT_PAD).is_some(), "a picture out");
    }

    fn announced(hn: &Harness) -> StreamColor {
        StreamColor::read_vocabulary(hn.vocabulary(), &hn.announced().expect("a format"))
    }

    #[test]
    fn a_refused_tone_map_is_not_tried_on_a_new_session_each_picture() {
        let test = "a_refused_tone_map_is_not_tried_on_a_new_session_each_picture";
        match proc_device(test) {
            None => return,
            Some(false) => return eprintln!("skip {test}: no HDR10 to SDR tone mapping"),
            Some(true) => {}
        }
        let target = Target { max_width: W / 2, max_height: H / 2, depth: 8 };
        let vpp = VaapiPostProc { refuse_tone: true, ..VaapiPostProc::new(target) };
        let stats = vpp.stats();
        let mut hn = Harness::with_pool(vpp, 1 << 20, 8);
        fix(&mut hn, &hdr10(1000), false);
        one(&mut hn);
        let made = stats.snapshot().surfaces;
        assert!(made > 0, "{test} a session");
        for _ in 0..3 {
            one(&mut hn);
        }
        assert_eq!(stats.snapshot().surfaces, made, "{test} the session kept");
        let c = announced(&hn);
        assert_eq!(c, hdr10(1000), "{test} the colour it still has");
    }

    #[test]
    fn a_fixed_size_output_announces_new_hdr_metadata() {
        let test = "a_fixed_size_output_announces_new_hdr_metadata";
        if proc_device(test).is_none() {
            return;
        }
        let target = Target { max_width: W / 2, max_height: H / 2, depth: 10 };
        let mut hn = Harness::with_pool(VaapiPostProc::new(target).with_fixed_size(), 1 << 20, 8);
        fix(&mut hn, &hdr10(1000), false);
        one(&mut hn);
        assert_eq!((hn.format_changes(), announced(&hn)), (1, hdr10(1000)), "{test}");
        fix(&mut hn, &hdr10(600), true);
        one(&mut hn);
        assert_eq!((hn.format_changes(), announced(&hn)), (2, hdr10(600)), "{test} the new light");
        fix(&mut hn, &hdr10(600), true);
        one(&mut hn);
        assert_eq!(hn.format_changes(), 2, "{test} the same colour, the same format");
    }

    #[test]
    fn a_scaled_picture_carries_the_side_records_of_its_input() {
        let pool = Pool::bounded(256, 1);
        let mut to = pool.acquire();
        carry_side(&[1, 4, 0, 9, 9, 9, 9], &mut to, false).unwrap();
        assert_eq!(to.side(), &[1, 4, 0, 9, 9, 9, 9]);
        assert_eq!(to.clone().side(), to.side(), "a view handed on states them too");
    }

    #[test]
    fn a_tone_mapped_picture_and_one_without_records_carry_none() {
        let pool = Pool::bounded(256, 2);
        let mut to = pool.acquire();
        carry_side(&[1, 4, 0, 9, 9, 9, 9], &mut to, true).unwrap();
        assert!(to.side().is_empty());
        carry_side(&[], &mut to, false).unwrap();
        assert!(to.side().is_empty());
    }
}
