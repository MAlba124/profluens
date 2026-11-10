//! The software video decoder element shared by the codec crates.
//! A codec supplies a [`Codec`], and [`VideoDec`] handles pool slots, announcement, flush, seek and
//! EOS.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use pf_memory::MemoryError;
use pf_memory::udmabuf::{Dmabuf, sync_write};
use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::feedback::Feedback;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, OfferSpec, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::{Memory, PoolStats, SlotId};
use profluens_core::time::Timestamp;

use crate::color;
use crate::format::{FAMILY, FIELD_HEIGHT, FIELD_PIXFMT, FIELD_WIDTH, PixelFormat, PixelFormats};
use crate::geometry::plane_geometry;
use crate::gpuframe::{
    self, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_NV12, DRM_FORMAT_P010, DRM_FORMAT_YUV420, DmabufFds,
    DmabufFormat, ExportedPlane, F_HEIGHT, F_LAYOUT, F_MODIFIER, F_WIDTH, FAMILY_VIDEO_GPU,
    FrameToken, FrameTx, GPU_FRAME_HEADER_LEN, GpuFrame, GpuFrameChannel, GpuFrameHeader, Planes,
};
use crate::simd::{self, Lanes};
use pf_hdr::carry::Carry;
pub use pf_hdr::h26x::Scan;
use pf_hdr::side::Writer;

const SINK_PAD: SinkPad = SinkPad(0);
const SRC_PAD: SrcPad = SrcPad(0);

static PROBED_SRC: [PadDesc; 1] = [PadDesc { name: "src", offers: OfferDesc::PROBED }];

fn dmabuf_layout(pixfmt: &str) -> Option<(u32, &'static str)> {
    match pixfmt {
        "i420" => Some((DRM_FORMAT_YUV420, gpuframe::LAYOUT_I420_DMABUF)),
        "nv12" => Some((DRM_FORMAT_NV12, gpuframe::LAYOUT_NV12_DMABUF)),
        "p010" => Some((DRM_FORMAT_P010, gpuframe::LAYOUT_P010_DMABUF)),
        _ => None,
    }
}

fn codec_writes(desc: &ElementDesc, fourcc: u32) -> bool {
    let writes = |v: &ValueDesc| match v {
        ValueDesc::Id(name) => dmabuf_layout(name).is_some_and(|(fc, _)| fc == fourcc),
        _ => false,
    };
    desc.srcs
        .first()
        .into_iter()
        .flat_map(|pad| pad.offers.iter())
        .flat_map(|offer| offer.fields.iter().filter(|f| f.field == FIELD_PIXFMT))
        .any(|f| match &f.allowed {
            ConstraintDesc::Set(values) => values.iter().any(writes),
            ConstraintDesc::Eq(v) => writes(v),
            _ => false,
        })
}

const fn any(field: &'static str) -> FieldDesc {
    FieldDesc { field, allowed: ConstraintDesc::Any, preferred: None }
}

/// Colour fields a sink offer declares. Undeclared field names are lost at interning.
pub static COLOR_FIELDS: [FieldDesc; 10] = [
    any(color::FIELD_MATRIX),
    any(color::FIELD_RANGE),
    any(color::FIELD_TRANSFER),
    any(color::FIELD_PRIMARIES),
    any(color::FIELD_MASTERING_RG),
    any(color::FIELD_MASTERING_BW),
    any(color::FIELD_MASTERING_LUMINANCE),
    any(color::FIELD_CONTENT_LIGHT),
    any(color::FIELD_CHROMA_SITE),
    any(color::FIELD_DOVI),
];

pub const fn src_fields(pixfmts: &'static [ValueDesc]) -> [FieldDesc; 7] {
    [
        any(FIELD_WIDTH),
        any(FIELD_HEIGHT),
        FieldDesc { field: FIELD_PIXFMT, allowed: ConstraintDesc::Set(pixfmts), preferred: None },
        any(color::FIELD_MATRIX),
        any(color::FIELD_RANGE),
        any(color::FIELD_TRANSFER),
        any(color::FIELD_PRIMARIES),
    ]
}

/// Src offer fields of a decoder that states its own colour ([`Held::color`]).
/// Every colour name is listed, since an announced value survives interning only if some offer
/// names it.
pub const fn src_fields_stating_color(pixfmts: &'static [ValueDesc]) -> [FieldDesc; 13] {
    let c = crate::format::COLOR_OFFER_FIELDS;
    [
        any(FIELD_WIDTH),
        any(FIELD_HEIGHT),
        FieldDesc { field: FIELD_PIXFMT, allowed: ConstraintDesc::Set(pixfmts), preferred: None },
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

fn link_takes(ctx: &Ctx) -> PixelFormats {
    let Some(field) = ctx.field_id(FIELD_PIXFMT) else {
        return PixelFormats::NONE;
    };
    let Some(vals) = ctx.src_takes(SRC_PAD, field) else {
        return PixelFormats::NONE;
    };
    let name = |v: &Value| match v {
        Value::Id(id) => ctx.value_name(*id),
        _ => None,
    };
    PixelFormats::of_names(vals.iter().filter_map(name))
}

/// Three planes as a software decoder lays them out, chroma shifted by `chroma_shift` (x, y), 0 or
/// 1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Planar {
    pub bits: u8,
    pub chroma_shift: (u8, u8),
}

/// The pixel format and pack layout of a [`Planar`] picture on a link taking `takes`.
/// `None` for a depth outside 8 to 16 or a shift past 1.
pub fn place(p: Planar, takes: PixelFormats) -> Option<(PixelFormat, HeldLayout)> {
    let Planar { bits, chroma_shift } = p;
    if !(8..=16).contains(&bits) || chroma_shift.0 > 1 || chroma_shift.1 > 1 {
        return None;
    }
    if let Some(f) = PixelFormat::planar(bits, chroma_shift).filter(|f| takes.has(*f)) {
        return Some((f, HeldLayout::Native));
    }
    let reduce = HeldLayout::Reduce { chroma_shift, bits };
    let p010 = match (bits, chroma_shift) {
        (10, (1, 1)) => (PixelFormat::P010, HeldLayout::Planar10),
        _ => (PixelFormat::P010, reduce),
    };
    let i420 = match (bits, chroma_shift) {
        (8, (1, 1)) => (PixelFormat::I420, HeldLayout::Native),
        _ => (PixelFormat::I420, reduce),
    };
    let order: &[(PixelFormat, HeldLayout)] = if bits > 8 { &[p010, i420] } else { &[i420] };
    Some(order.iter().copied().find(|(f, _)| takes.has(*f)).unwrap_or(order[0]))
}

const PICKS: &[Coupling] = &[Coupling::takes(0, FIELD_PIXFMT)];

pub const fn picking(mut desc: ElementDesc) -> ElementDesc {
    desc.couplings = PICKS;
    desc
}

pub const fn sink_pad(offers: &'static [OfferDesc]) -> [PadDesc; 1] {
    [PadDesc { name: "sink", offers }]
}

pub const fn src_pad(offers: &'static [OfferDesc]) -> [PadDesc; 1] {
    [PadDesc { name: "src", offers }]
}

/// A decoder's element descriptor. Active, since a frame decode takes milliseconds.
pub const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    make_default: fn() -> Box<dyn Element>,
) -> ElementDesc {
    desc_with_props(name, sinks, srcs, &[], make_default)
}

pub const fn desc_with_props(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    props: &'static [PropDesc],
    make_default: fn() -> Box<dyn Element>,
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props,
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: Some(make_default),
        io: false,
        couplings: &[],
    }
}

pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub pixfmt: &'static str,
    pub planes: [Vec<u8>; 3],
    pub pts: Timestamp,
    pub duration: Timestamp,
}

#[derive(Clone, Copy)]
pub struct PlaneRef<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

impl PlaneRef<'_> {
    pub const EMPTY: PlaneRef<'static> = PlaneRef { data: &[], stride: 0 };
}

/// The colour a bitstream states (ITU-T H.273). `None` or zero fields defer to the container.
pub type Color = color::StreamColor;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum HeldLayout {
    #[default]
    Native,
    /// 4:2:0 10-bit samples in LE 16-bit words, low aligned, packed to P010. dav1d's high bit depth
    /// layout.
    Planar10,
    Reduce {
        chroma_shift: (u8, u8),
        bits: u8,
    },
    Gray,
}

/// A picture the codec keeps until a pool slot is free.
pub struct Held {
    pub width: u32,
    pub height: u32,
    pub pixfmt: PixelFormat,
    pub pts: Timestamp,
    pub duration: Timestamp,
    pub handle: u32,
    pub color: Color,
    pub layout: HeldLayout,
}

/// A picture in a pool slot or codec memory. `offsets` and `strides` are bytes into `memory`.
pub struct Sent {
    pub memory: Memory,
    pub width: u32,
    pub height: u32,
    pub pixfmt: PixelFormat,
    pub layout: HeldLayout,
    pub offsets: [usize; 3],
    pub strides: [usize; 3],
    pub pts: Timestamp,
    pub duration: Timestamp,
    pub color: Color,
}

impl Sent {
    fn planes(&self) -> [PlaneRef<'_>; 3] {
        let data = self.memory.data();
        [0, 1, 2].map(|p| match data.get(self.offsets[p]..) {
            Some(d) if p < self.pixfmt.plane_count() || self.layout != HeldLayout::Native => {
                PlaneRef { data: d, stride: self.strides[p] }
            }
            _ => PlaneRef::EMPTY,
        })
    }
}

enum Ready {
    Owned(Picture),
    Held(Held),
    Sent(Sent),
}

impl Ready {
    fn shape(&self) -> (u32, u32, &'static str, Color) {
        match self {
            Ready::Owned(p) => (p.width, p.height, p.pixfmt, Color::default()),
            Ready::Held(h) => (h.width, h.height, h.pixfmt.caps_name(), h.color),
            Ready::Sent(s) => (s.width, s.height, s.pixfmt.caps_name(), s.color),
        }
    }

    fn pts(&self) -> Timestamp {
        match self {
            Ready::Owned(p) => p.pts,
            Ready::Held(h) => h.pts,
            Ready::Sent(s) => s.pts,
        }
    }
}

pub struct Out<'a> {
    ready: &'a mut VecDeque<Ready>,
    ctx: &'a mut Ctx,
    watch: &'a mut Watch,
    takes: PixelFormats,
}

impl Out<'_> {
    pub fn place(&self, planes: Planar) -> Option<(PixelFormat, HeldLayout)> {
        place(planes, self.takes)
    }

    pub fn takes(&self) -> PixelFormats {
        self.takes
    }

    /// The element's context, for a codec's own log records.
    pub fn ctx(&self) -> &Ctx {
        self.ctx
    }

    pub fn push(&mut self, picture: Picture) {
        self.watch.pictures = self.watch.pictures.saturating_add(1);
        if !self.decode_only(picture.pts) {
            self.ready.push_back(Ready::Owned(picture));
        }
    }

    /// Queue a held picture. `false` when it is before the presentation point and the codec must
    /// release it.
    pub fn push_held(&mut self, held: Held) -> bool {
        self.watch.pictures = self.watch.pictures.saturating_add(1);
        if self.decode_only(held.pts) {
            return false;
        }
        self.ready.push_back(Ready::Held(held));
        true
    }

    /// Whether `pts` is before the presentation point. `true` counts it dropped, so do not push it.
    pub fn decode_only(&mut self, pts: Timestamp) -> bool {
        if self.watch.hidden.take(pts) {
            self.ctx.count_drop();
            return true;
        }
        if !self.ctx.before_presentation(pts, Timestamp::ZERO) {
            return false;
        }
        // A codec may ask before packing and again on push for the same picture.
        if pts.is_some() && pts == self.watch.preview {
            return false;
        }
        if self.ctx.take_preview() {
            self.watch.preview = pts;
            return false;
        }
        self.ctx.count_drop();
        true
    }

    pub fn push_sent(&mut self, sent: Sent) -> bool {
        self.watch.pictures = self.watch.pictures.saturating_add(1);
        if self.decode_only(sent.pts) {
            return false;
        }
        self.ready.push_back(Ready::Sent(sent));
        true
    }

    /// A pool slot of at least `bytes`. `None` is backpressure, call [`starve`](Self::starve).
    pub fn frame_memory(&mut self, bytes: usize) -> Option<Memory> {
        if self.ctx.grow_slots(bytes) {
            log!(&*self.ctx, Level::Debug, "grow_slots", bytes = bytes);
        }
        let memory = self.ctx.try_alloc()?.memory;
        if memory.device().is_some()
            && let Err(e) = Dmabuf::of(&memory).and_then(|d| sync_write(d.fd, true))
        {
            self.fail(VideoDecError::Dmabuf(e));
        }
        Some(memory)
    }

    pub fn starve(&mut self) {
        self.watch.starved = true;
    }

    pub fn pool_stats(&self) -> PoolStats {
        self.ctx.pool_stats()
    }

    pub fn warn(&mut self, error: impl std::error::Error + Send + Sync + 'static) {
        self.watch.warned = true;
        let element = self.ctx.element();
        self.ctx
            .post(BusMessage::Warning { element, error: Error::element_failed(element, error) });
    }

    pub fn fail(&mut self, error: impl std::error::Error + Send + Sync + 'static) {
        if self.watch.failed.is_none() {
            self.watch.failed = Some(Error::element_failed(self.ctx.element(), error));
        }
    }
}

/// Slots kept beyond what the codec and waiting pictures hold, for frames a sink keeps shown and
/// the next picture.
pub const SLOT_SLACK: u32 = 6;

/// The most slots the pool grows to. H.264 holds at most 16 reference frames (Table A-1), so more
/// is a leak.
pub const MAX_SLOTS: u32 = 40;

pub const SILENT_UNITS: u32 = 48;

struct Watch {
    pictures: u64,
    warned: bool,
    silent: u32,
    settled: bool,
    failed: Option<Error>,
    starved: bool,
    preview: Timestamp,
    hidden: Hidden,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            pictures: 0,
            warned: false,
            silent: 0,
            settled: false,
            failed: None,
            starved: false,
            preview: Timestamp::NONE,
            hidden: Hidden::default(),
        }
    }
}

impl Watch {
    fn preview_flag(&mut self, pts: Timestamp) -> BufferFlags {
        if pts.is_some() && pts == self.preview {
            self.preview = Timestamp::NONE;
            return BufferFlags::PREVIEW;
        }
        BufferFlags::empty()
    }
}

/// Fixed ring of decode-only stamps. The demuxer stamps a decode-only unit at or before its first
/// shown picture.
struct Hidden {
    stamps: [Timestamp; HIDDEN_MAX],
    len: usize,
}

impl Default for Hidden {
    fn default() -> Self {
        Hidden { stamps: [Timestamp::NONE; HIDDEN_MAX], len: 0 }
    }
}

const HIDDEN_MAX: usize = 32;

impl Hidden {
    fn note(&mut self, pts: Timestamp) {
        if self.len == HIDDEN_MAX {
            self.stamps.copy_within(1.., 0);
            self.len -= 1;
        }
        self.stamps[self.len] = pts;
        self.len += 1;
    }

    fn take(&mut self, pts: Timestamp) -> bool {
        let Some(at) = self.stamps[..self.len].iter().position(|s| *s == pts) else {
            return false;
        };
        self.stamps.copy_within(at + 1..self.len, at);
        self.len -= 1;
        true
    }
}

pub trait Codec: Send + 'static {
    const DESC: &'static ElementDesc;

    fn new() -> Self;

    fn start(&mut self, _ctx: &Ctx) {}

    fn reset(&mut self);

    fn decode(&mut self, unit: &[u8], pts: Timestamp, duration: Timestamp, out: &mut Out<'_>);

    /// [`decode`](Self::decode) given the unit's memory, for a codec that reads it past the call.
    /// The shell calls this one.
    fn decode_memory(
        &mut self,
        unit: &Memory,
        pts: Timestamp,
        duration: Timestamp,
        out: &mut Out<'_>,
    ) {
        self.decode(unit.data(), pts, duration, out);
    }

    fn finish(&mut self, out: &mut Out<'_>);

    /// Whether no other picture refers to `unit`. Asked only before the presentation point, `true`
    /// skips it.
    fn disposable(&self, _unit: &[u8]) -> bool {
        false
    }

    /// The sink drops frames for lateness, or is on time again. A codec that skips non-reference
    /// pictures turns that on here.
    fn late(&mut self, _late: bool) {}

    /// The planes of the held picture `handle` as decoded. Asked once, when a pool slot is free.
    fn held_planes(&self, _handle: u32) -> [PlaneRef<'_>; 3] {
        [PlaneRef::EMPTY; 3]
    }

    fn release_held(&mut self, _handle: u32) {}

    /// Visit every view of memory the codec holds, so the shell can tell a pool too small from
    /// backpressure.
    fn for_each_held(&self, _f: &mut dyn FnMut(&Memory)) {}

    /// HDR side records and static messages of `unit`, before it is decoded.
    fn scan(&mut self, _unit: &[u8], _scan: &mut Scan<'_>) {}
}

#[derive(Debug, PartialEq, Eq)]
pub enum VideoDecError {
    /// A picture does not fit a pool slot. Fatal, truncating would emit a corrupt frame.
    SlotTooSmall {
        width: u32,
        height: u32,
        pixfmt: &'static str,
        need: usize,
        capacity: usize,
    },
    PlaneShort {
        plane: usize,
        need: usize,
        have: usize,
    },
    StrideShort {
        plane: usize,
        stride: usize,
        row: usize,
    },
    ChannelTaken,
    Dmabuf(MemoryError),
    Layout {
        pixfmt: &'static str,
        fixed: u32,
    },
    HeldLayout {
        layout: HeldLayout,
        pixfmt: &'static str,
    },
    PoolTooSmall {
        slots: u64,
        held: usize,
    },
    NothingShown {
        units: u32,
    },
}

impl fmt::Display for VideoDecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoDecError::SlotTooSmall { width, height, pixfmt, need, capacity } => write!(
                f,
                "{width}x{height} {pixfmt} picture needs {need} bytes but pool slots hold \
                 {capacity}, raise the pipeline pool slot size"
            ),
            VideoDecError::PlaneShort { plane, need, have } => {
                write!(f, "held plane {plane} has {have} bytes, its rows need {need}")
            }
            VideoDecError::StrideShort { plane, stride, row } => {
                write!(f, "held plane {plane} has stride {stride}, under its row of {row} bytes")
            }
            VideoDecError::ChannelTaken => {
                write!(f, "the frame channel's decoder end is another element's")
            }
            VideoDecError::Dmabuf(e) => write!(f, "the picture's slot is no usable dmabuf, {e}"),
            VideoDecError::Layout { pixfmt, fixed } => {
                write!(f, "a {pixfmt} picture on a video/gpu link that fixed FourCC {fixed:#010x}")
            }
            VideoDecError::HeldLayout { layout, pixfmt } => {
                write!(f, "a held picture laid out {layout:?} cannot be put out as {pixfmt}")
            }
            VideoDecError::PoolTooSmall { slots, held } => write!(
                f,
                "the decoder holds {held} of the pool's {slots} slots, its references and \
                 the pictures waiting, under the slack the sink and the next picture need, \
                 and the pool grows no further"
            ),
            VideoDecError::NothingShown { units } => write!(
                f,
                "no picture decoded, the last {units} units were all dropped, see the warnings before"
            ),
        }
    }
}

impl std::error::Error for VideoDecError {}

/// Copy strided `planes` into `dst` packed tight. Bytes written.
pub fn pack_planes(
    dst: &mut [u8],
    pixfmt: PixelFormat,
    width: u32,
    height: u32,
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    let mut at = 0;
    for (plane, src) in planes.iter().enumerate().take(pixfmt.plane_count()) {
        let Some(g) = plane_geometry(pixfmt, width, height, plane) else {
            break;
        };
        if g.height == 0 || g.stride == 0 {
            continue;
        }
        if src.stride < g.stride {
            return Err(VideoDecError::StrideShort { plane, stride: src.stride, row: g.stride });
        }
        let need = (g.height - 1) * src.stride + g.stride;
        if src.data.len() < need {
            return Err(VideoDecError::PlaneShort { plane, need, have: src.data.len() });
        }
        for row in 0..g.height {
            let from = row * src.stride;
            dst[at..at + g.stride].copy_from_slice(&src.data[from..from + g.stride]);
            at += g.stride;
        }
    }
    Ok(at)
}

/// Pack 4:2:0 10-bit LE planes into tight P010 (drm_fourcc.h `DRM_FORMAT_P010`). Bytes written.
pub fn pack_p010(
    dst: &mut [u8],
    width: u32,
    height: u32,
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    pack_p010_on(Lanes::detect(), dst, width, height, planes)
}

pub(crate) fn pack_p010_on(
    lanes: Lanes,
    dst: &mut [u8],
    width: u32,
    height: u32,
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let shapes = [(2 * w, h), (2 * cw, ch), (2 * cw, ch)];
    for (plane, (src, &(row, rows))) in planes.iter().zip(&shapes).enumerate() {
        if rows == 0 || row == 0 {
            continue;
        }
        if src.stride < row {
            return Err(VideoDecError::StrideShort { plane, stride: src.stride, row });
        }
        let need = (rows - 1) * src.stride + row;
        if src.data.len() < need {
            return Err(VideoDecError::PlaneShort { plane, need, have: src.data.len() });
        }
    }
    let (luma, chroma) = (2 * w * h, 4 * cw * ch);
    let (y_dst, rest) = dst[..luma + chroma].split_at_mut(luma);
    if w > 0 {
        for (row, d) in y_dst.chunks_exact_mut(2 * w).enumerate() {
            let from = row * planes[0].stride;
            let src = &planes[0].data[from..from + 2 * w];
            let done = 2 * simd::shift(lanes, d, src, P010_SHIFT);
            p010_luma_row(&mut d[done..], &src[done..]);
        }
    }
    if cw > 0 {
        for (row, d) in rest.chunks_exact_mut(4 * cw).enumerate() {
            let (fu, fv) = (row * planes[1].stride, row * planes[2].stride);
            let u = &planes[1].data[fu..fu + 2 * cw];
            let v = &planes[2].data[fv..fv + 2 * cw];
            let done = simd::interleave_shift(lanes, d, u, v, P010_SHIFT);
            p010_chroma_row(&mut d[4 * done..], &u[2 * done..], &v[2 * done..]);
        }
    }
    Ok(luma + chroma)
}

const P010_SHIFT: u32 = 6;

// Fixed-width chunks so it vectorizes.
#[inline]
fn p010_luma_row(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(src.as_chunks::<2>().0) {
        *d = (u16::from_le_bytes(*s) << P010_SHIFT).to_le_bytes();
    }
}

#[inline]
fn p010_chroma_row(dst: &mut [u8], u: &[u8], v: &[u8]) {
    let pairs = u.as_chunks::<2>().0.iter().zip(v.as_chunks::<2>().0);
    for (d, (u, v)) in dst.as_chunks_mut::<4>().0.iter_mut().zip(pairs) {
        let u = (u16::from_le_bytes(*u) << P010_SHIFT).to_le_bytes();
        let v = (u16::from_le_bytes(*v) << P010_SHIFT).to_le_bytes();
        *d = [u[0], u[1], v[0], v[1]];
    }
}

fn reduces_to(bits: u8, pixfmt: PixelFormat) -> bool {
    match pixfmt {
        PixelFormat::I420 => (8..=16).contains(&bits),
        PixelFormat::P010 => (9..=16).contains(&bits),
        _ => false,
    }
}

/// Pack a [`HeldLayout::Reduce`] picture into tight I420 or P010. Bytes written.
/// Denser chroma is filtered 1 2 1 to the 4:2:0 sites (ITU-T H.264 Figure E-1,
/// chroma_sample_loc_type 0).
pub fn pack_reduced(
    dst: &mut [u8],
    width: u32,
    height: u32,
    chroma_shift: (u8, u8),
    bits: u8,
    to: PixelFormat,
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    let lanes = Lanes::detect();
    pack_reduced_on(lanes, dst, (width, height), chroma_shift, bits, to, planes)
}

pub(crate) fn pack_reduced_on(
    lanes: Lanes,
    dst: &mut [u8],
    (width, height): (u32, u32),
    chroma_shift: (u8, u8),
    bits: u8,
    to: PixelFormat,
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    let (w, h) = (width as usize, height as usize);
    let shift = (chroma_shift.0.min(1), chroma_shift.1.min(1));
    let wide = bits > 8;
    let size = 1 + usize::from(wide);
    if !reduces_to(bits, to) {
        let layout = HeldLayout::Reduce { chroma_shift, bits };
        return Err(VideoDecError::HeldLayout { layout, pixfmt: to.caps_name() });
    }
    if wide && to == PixelFormat::I420 {
        return pack_narrowed(lanes, dst, (w, h), shift, bits, planes);
    }
    let (sc, sr) = (w.div_ceil(1 << shift.0), h.div_ceil(1 << shift.1));
    let shapes = [(size * w, h), (size * sc, sr), (size * sc, sr)];
    for (plane, (src, &(row, rows))) in planes.iter().zip(&shapes).enumerate() {
        if rows == 0 || row == 0 {
            continue;
        }
        if src.stride < row {
            return Err(VideoDecError::StrideShort { plane, stride: src.stride, row });
        }
        let need = (rows - 1) * src.stride + row;
        if src.data.len() < need {
            return Err(VideoDecError::PlaneShort { plane, need, have: src.data.len() });
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (luma, chroma) = (size * w * h, 2 * size * cw * ch);
    let (y_dst, c_dst) = dst[..luma + chroma].split_at_mut(luma);
    let up = 16 - u32::from(bits.min(16));
    if w > 0 {
        for (row, d) in y_dst.chunks_exact_mut(size * w).enumerate() {
            let from = row * planes[0].stride;
            let src = &planes[0].data[from..from + size * w];
            match wide {
                true => {
                    let done = 2 * simd::shift(lanes, d, src, up);
                    deep_luma_row(&mut d[done..], &src[done..], up);
                }
                false => d.copy_from_slice(src),
            }
        }
    }
    if cw == 0 || ch == 0 {
        return Ok(luma + chroma);
    }
    let fit = Fit { shift, cols: sc, rows: sr };
    let taps = shift.0 == 0;
    let mut t = Temps::new();
    if wide {
        for (y, d) in c_dst.chunks_exact_mut(4 * cw).enumerate() {
            let (u, v) = (fit.rows_of::<true>(&planes[1], y), fit.rows_of::<true>(&planes[2], y));
            let done = simd::fit_p010(lanes, taps, bits, u, v, d, up);
            for (k, d) in d[4 * done..].chunks_mut(4 * RUN).enumerate() {
                let (n, x0) = (d.len() / 4, done + k * RUN);
                let (su, sv) = (&mut t.u[..n], &mut t.v[..n]);
                fit.sites::<true>(u, x0, su, &mut t.taps);
                fit.sites::<true>(v, x0, sv, &mut t.taps);
                for ((d, u), v) in d.as_chunks_mut::<4>().0.iter_mut().zip(&*su).zip(&*sv) {
                    let (u, v) = ((u << up).to_le_bytes(), (v << up).to_le_bytes());
                    *d = [u[0], u[1], v[0], v[1]];
                }
            }
        }
    } else {
        let (u_dst, v_dst) = c_dst.split_at_mut(cw * ch);
        for (src, dst) in [(&planes[1], u_dst), (&planes[2], v_dst)] {
            for (y, d) in dst.chunks_exact_mut(cw).enumerate() {
                let rows = fit.rows_of::<false>(src, y);
                let done = simd::fit8(lanes, taps, rows, d);
                for (k, d) in d[done..].chunks_mut(RUN).enumerate() {
                    let sites = &mut t.u[..d.len()];
                    fit.sites::<false>(rows, done + k * RUN, sites, &mut t.taps);
                    for (d, s) in d.iter_mut().zip(&*sites) {
                        *d = *s as u8;
                    }
                }
            }
        }
    }
    Ok(luma + chroma)
}

fn check_planes(planes: &[PlaneRef<'_>], shapes: &[(usize, usize)]) -> Result<(), VideoDecError> {
    for (plane, (src, &(row, rows))) in planes.iter().zip(shapes).enumerate() {
        if rows == 0 || row == 0 {
            continue;
        }
        if src.stride < row {
            return Err(VideoDecError::StrideShort { plane, stride: src.stride, row });
        }
        let need = (rows - 1) * src.stride + row;
        if src.data.len() < need {
            return Err(VideoDecError::PlaneShort { plane, need, have: src.data.len() });
        }
    }
    Ok(())
}

#[inline(always)]
fn narrow(s: u16, down: u32) -> u8 {
    ((u32::from(s) + (1 << down >> 1)) >> down).min(255) as u8
}

fn pack_narrowed(
    lanes: Lanes,
    dst: &mut [u8],
    (w, h): (usize, usize),
    shift: (u8, u8),
    bits: u8,
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    let (sc, sr) = (w.div_ceil(1 << shift.0), h.div_ceil(1 << shift.1));
    check_planes(planes, &[(2 * w, h), (2 * sc, sr), (2 * sc, sr)])?;
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (luma, chroma) = (w * h, 2 * cw * ch);
    let (y_dst, c_dst) = dst[..luma + chroma].split_at_mut(luma);
    let down = u32::from(bits) - 8;
    if w > 0 {
        for (row, d) in y_dst.chunks_exact_mut(w).enumerate() {
            let from = row * planes[0].stride;
            let src = &planes[0].data[from..from + 2 * w];
            let done = simd::narrow(lanes, d, src, down);
            for (d, s) in d[done..].iter_mut().zip(src[2 * done..].as_chunks::<2>().0) {
                *d = narrow(u16::from_le_bytes(*s), down);
            }
        }
    }
    if cw == 0 || ch == 0 {
        return Ok(luma + chroma);
    }
    let fit = Fit { shift, cols: sc, rows: sr };
    let taps = shift.0 == 0;
    let mut t = Temps::new();
    let (u_dst, v_dst) = c_dst.split_at_mut(cw * ch);
    for (src, dst) in [(&planes[1], u_dst), (&planes[2], v_dst)] {
        for (y, d) in dst.chunks_exact_mut(cw).enumerate() {
            let rows = fit.rows_of::<true>(src, y);
            let done = simd::fit_narrow(lanes, taps, bits, rows, d, down);
            for (k, d) in d[done..].chunks_mut(RUN).enumerate() {
                let sites = &mut t.u[..d.len()];
                fit.sites::<true>(rows, done + k * RUN, sites, &mut t.taps);
                for (d, s) in d.iter_mut().zip(&*sites) {
                    *d = narrow(*s, down);
                }
            }
        }
    }
    Ok(luma + chroma)
}

pub fn pack_gray(
    dst: &mut [u8],
    width: u32,
    height: u32,
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    let (w, h) = (width as usize, height as usize);
    check_planes(&planes[..1], &[(w, h)])?;
    let (luma, chroma) = (w * h, 2 * w.div_ceil(2) * h.div_ceil(2));
    let (y_dst, c_dst) = dst[..luma + chroma].split_at_mut(luma);
    if w > 0 {
        for (row, d) in y_dst.chunks_exact_mut(w).enumerate() {
            let from = row * planes[0].stride;
            d.copy_from_slice(&planes[0].data[from..from + w]);
        }
    }
    c_dst.fill(128);
    Ok(luma + chroma)
}

#[inline]
fn deep_luma_row(dst: &mut [u8], src: &[u8], up: u32) {
    for (d, s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(src.as_chunks::<2>().0) {
        *d = (u16::from_le_bytes(*s) << up).to_le_bytes();
    }
}

struct Fit {
    shift: (u8, u8),
    cols: usize,
    rows: usize,
}

impl Fit {
    #[inline]
    fn rows_of<'a, const WIDE: bool>(&self, p: &PlaneRef<'a>, y: usize) -> (&'a [u8], &'a [u8]) {
        let (a, b) = match self.shift.1 {
            0 => (2 * y, (2 * y + 1).min(self.rows - 1)),
            _ => (y, y),
        };
        let row = (1 + usize::from(WIDE)) * self.cols;
        let at = |r: usize| &p.data[r * p.stride..r * p.stride + row];
        (at(a), at(b))
    }

    // 1 2 1 across columns clamped at the edges, averaged over both rows.
    #[inline]
    fn sites<const WIDE: bool>(
        &self,
        (a, b): (&[u8], &[u8]),
        x0: usize,
        out: &mut [u16],
        [ra, rb]: &mut [Taps; 2],
    ) {
        let n = out.len();
        if self.shift.0 == 1 {
            samples::<WIDE>(a, x0, &mut ra[..n]);
            samples::<WIDE>(b, x0, &mut rb[..n]);
            for ((o, a), b) in out.iter_mut().zip(&*ra).zip(&*rb) {
                *o = ((u32::from(*a) + u32::from(*b) + 1) >> 1) as u16;
            }
            return;
        }
        let lead = usize::from(x0 == 0);
        let from = 2 * x0 + lead - 1;
        let m = (2 * n + 1 - lead).min(self.cols - from);
        samples::<WIDE>(a, from, &mut ra[lead..lead + m]);
        samples::<WIDE>(b, from, &mut rb[lead..lead + m]);
        for r in [&mut *ra, &mut *rb] {
            r[0] = r[lead];
            let last = r[lead + m - 1];
            r[lead + m..=2 * n].fill(last);
        }
        let taps = |r: &Taps, i: usize| {
            u32::from(r[2 * i]) + 2 * u32::from(r[2 * i + 1]) + u32::from(r[2 * i + 2])
        };
        for (i, o) in out.iter_mut().enumerate() {
            *o = ((taps(ra, i) + taps(rb, i) + 4) >> 3) as u16;
        }
    }
}

const RUN: usize = 128;

/// One row's samples around a run of sites, the clamped edge taps included.
type Taps = [u16; 2 * RUN + 1];

// Scalar scratch, zeroed once a picture rather than once a run.
struct Temps {
    taps: [Taps; 2],
    u: [u16; RUN],
    v: [u16; RUN],
}

impl Temps {
    fn new() -> Temps {
        Temps { taps: [[0; 2 * RUN + 1]; 2], u: [0; RUN], v: [0; RUN] }
    }
}

#[inline]
fn samples<const WIDE: bool>(row: &[u8], from: usize, out: &mut [u16]) {
    let n = out.len();
    match WIDE {
        true => {
            for (o, s) in out.iter_mut().zip(row[2 * from..2 * (from + n)].as_chunks::<2>().0) {
                *o = u16::from_le_bytes(*s);
            }
        }
        false => {
            for (o, s) in out.iter_mut().zip(&row[from..from + n]) {
                *o = u16::from(*s);
            }
        }
    }
}

fn packed_layout(pixfmt: PixelFormat, width: u32, height: u32) -> [(usize, usize); 3] {
    let mut out = [(0, 0); 3];
    let mut at = 0;
    for (plane, o) in out.iter_mut().enumerate().take(pixfmt.plane_count()) {
        let Some(g) = plane_geometry(pixfmt, width, height, plane) else {
            break;
        };
        *o = (at, g.stride);
        at += g.size;
    }
    out
}

fn packed_size(pixfmt: PixelFormat, width: u32, height: u32) -> usize {
    (0..pixfmt.plane_count())
        .filter_map(|p| plane_geometry(pixfmt, width, height, p))
        .map(|g| g.size)
        .sum()
}

/// An NV12 slot's size, even and one pitch for both planes, as VA-API surfaces are.
fn nv12_even(width: u32, height: u32) -> (usize, usize) {
    (width.next_multiple_of(2) as usize, height.next_multiple_of(2) as usize)
}

fn nv12_size(width: u32, height: u32) -> usize {
    let (w, h) = nv12_even(width, height);
    w * h * 3 / 2
}

/// The planes, their count and the coded size of an NV12 slot.
fn nv12_layout(width: u32, height: u32) -> ([(usize, usize); 3], usize, (u32, u32)) {
    let (w, h) = nv12_even(width, height);
    ([(0, w), (w * h, w), (0, 0)], 2, (w as u32, h as u32))
}

fn tight_i420(s: &[u8], w: usize, h: usize) -> [PlaneRef<'_>; 3] {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (y, c) = s.split_at(w * h);
    let (u, v) = c.split_at(cw * ch);
    [
        PlaneRef { data: y, stride: w },
        PlaneRef { data: u, stride: cw },
        PlaneRef { data: v, stride: cw },
    ]
}

/// An 8-bit 4:2:0 picture interleaved into tight NV12 at the even size, its last row and column
/// repeated into the padding. A picture laid out otherwise goes to `scratch` as I420 first.
/// Bytes written.
pub fn pack_nv12(
    dst: &mut [u8],
    scratch: Option<&mut Vec<u8>>,
    layout: HeldLayout,
    pixfmt: PixelFormat,
    width: u32,
    height: u32,
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    let lanes = Lanes::detect();
    pack_nv12_on(lanes, dst, scratch, (layout, pixfmt), (width, height), planes)
}

pub(crate) fn pack_nv12_on(
    lanes: Lanes,
    dst: &mut [u8],
    scratch: Option<&mut Vec<u8>>,
    (layout, pixfmt): (HeldLayout, PixelFormat),
    (width, height): (u32, u32),
    planes: &[PlaneRef<'_>; 3],
) -> Result<usize, VideoDecError> {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let i420 = match (layout, pixfmt, scratch) {
        (HeldLayout::Native, PixelFormat::I420, _) => *planes,
        (layout, _, None) => return Err(VideoDecError::HeldLayout { layout, pixfmt: "nv12" }),
        (_, _, Some(scratch)) => {
            let need = packed_size(PixelFormat::I420, width, height);
            if scratch.len() < need {
                scratch.resize(need, 0);
            }
            let s = &mut scratch[..need];
            match (layout, pixfmt) {
                (HeldLayout::Native, PixelFormat::Gray8) | (HeldLayout::Gray, _) => {
                    pack_gray(s, width, height, planes)?
                }
                (HeldLayout::Reduce { chroma_shift, bits }, _) => {
                    let to = PixelFormat::I420;
                    pack_reduced_on(lanes, s, (width, height), chroma_shift, bits, to, planes)?
                }
                (HeldLayout::Planar10, _) => {
                    let to = PixelFormat::I420;
                    pack_reduced_on(lanes, s, (width, height), (1, 1), 10, to, planes)?
                }
                (layout, _) => return Err(VideoDecError::HeldLayout { layout, pixfmt: "nv12" }),
            };
            tight_i420(&scratch[..need], w, h)
        }
    };
    check_planes(&i420, &[(w, h), (cw, ch), (cw, ch)])?;
    let (ew, eh) = nv12_even(width, height);
    let (y_dst, uv_dst) = dst[..ew * eh * 3 / 2].split_at_mut(ew * eh);
    if w == 0 || h == 0 {
        return Ok(ew * eh * 3 / 2);
    }
    for (r, d) in y_dst.chunks_exact_mut(ew).enumerate() {
        let from = r.min(h - 1) * i420[0].stride;
        d[..w].copy_from_slice(&i420[0].data[from..from + w]);
        if ew > w {
            d[w] = d[w - 1];
        }
    }
    for (r, d) in uv_dst.chunks_exact_mut(ew).enumerate() {
        let (fu, fv) = (r * i420[1].stride, r * i420[2].stride);
        let (u, v) = (&i420[1].data[fu..fu + cw], &i420[2].data[fv..fv + cw]);
        let done = simd::interleave(lanes, d, u, v);
        let pairs = d[2 * done..].as_chunks_mut::<2>().0.iter_mut();
        for ((d, &u), &v) in pairs.zip(&u[done..]).zip(&v[done..]) {
            *d = [u, v];
        }
    }
    Ok(ew * eh * 3 / 2)
}

struct DmabufOut {
    channel: Arc<GpuFrameChannel>,
    formats: Vec<DmabufFormat>,
    tx: Option<FrameTx>,
    link: Option<(u32, u64)>,
    next_token: FrameToken,
    released: Vec<FrameToken>,
    desc: &'static ElementDesc,
    /// Frames the full channel refused since the last flush or end.
    refused: u64,
    /// A picture as tight I420 on its way to an NV12 slot, when the codec lays it out otherwise.
    scratch: Vec<u8>,
}

impl DmabufOut {
    /// The layout and modifier a `pixfmt` picture goes out in, `None` on a raw link. The first the
    /// importer takes of the picture's own and NV12, P010 before NV12 for a deep one, relinking
    /// when it is not the fixed one.
    fn target(&mut self, pixfmt: &'static str) -> Result<Option<(u32, u64)>, VideoDecError> {
        let Some((fixed, modifier)) = self.link else {
            return Ok(None);
        };
        let order: &[u32] = match pixfmt {
            "i420" => &[DRM_FORMAT_YUV420, DRM_FORMAT_NV12],
            "p010" => &[DRM_FORMAT_P010, DRM_FORMAT_NV12],
            "nv12" | "gray8" => &[DRM_FORMAT_NV12],
            _ => &[],
        };
        let takes = |fourcc: u32| {
            fourcc == fixed
                || self
                    .formats
                    .iter()
                    .any(|f| f.fourcc == fourcc && f.modifiers.contains(&modifier))
        };
        match order.iter().copied().find(|&f| takes(f)) {
            Some(fourcc) => {
                self.link = Some((fourcc, modifier));
                Ok(self.link)
            }
            None => Err(VideoDecError::Layout { pixfmt, fixed }),
        }
    }
}

pub struct VideoDec<C: Codec> {
    codec: C,
    dmabuf: Option<DmabufOut>,
    ready: VecDeque<Ready>,
    shape: Option<(u32, u32, &'static str, Color)>,
    stalled: bool,
    watch: Watch,
    pending: Option<Buffer>,
    held: Vec<SlotId>,
    late: bool,
    carry: Carry,
    side: Box<[u8]>,
    takes: PixelFormats,
}

impl<C: Codec> VideoDec<C> {
    pub const IN: SinkPad = SINK_PAD;
    pub const OUT: SrcPad = SRC_PAD;

    // Once per element, at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self {
            codec: C::new(),
            dmabuf: None,
            ready: VecDeque::new(),
            shape: None,
            stalled: false,
            watch: Watch::default(),
            carry: Carry::new(),
            side: side_buffer(),
            pending: None,
            held: Vec::new(),
            late: false,
            takes: PixelFormats::NONE,
        }
    }

    /// Dmabuf mode, `video/gpu` offered beside `video/raw` in the `formats` the codec writes.
    // Once per element.
    #[allow(clippy::disallowed_methods)]
    pub fn with_dmabuf(mut self, channel: Arc<GpuFrameChannel>, formats: &[DmabufFormat]) -> Self {
        let desc = Box::leak(Box::new(ElementDesc { srcs: &PROBED_SRC, ..*C::DESC }));
        let formats = written_formats(C::DESC, formats);
        self.dmabuf = Some(DmabufOut {
            channel,
            formats,
            tx: None,
            link: None,
            next_token: 0,
            released: Vec::new(),
            desc,
            refused: 0,
            scratch: Vec::new(),
        });
        self
    }

    /// Raise the pool cap when held slots leave fewer than [`SLOT_SLACK`] free. `Ok(false)` is
    /// backpressure.
    // On a starve or stall only.
    #[allow(clippy::disallowed_methods)]
    fn pool_too_small(&mut self, ctx: &mut Ctx) -> Result<bool, VideoDecError> {
        let stats = ctx.pool_stats();
        let pool = ctx.pool_id();
        let held = &mut self.held;
        held.clear();
        let mut note = |m: &Memory| {
            if m.pool_id() == Some(pool) {
                held.extend(m.slot_id());
            }
        };
        self.codec.for_each_held(&mut note);
        for r in &self.ready {
            if let Ready::Sent(s) = r {
                note(&s.memory);
            }
        }
        held.sort_unstable();
        held.dedup();
        let n = held.len();
        let want = (n as u32).saturating_add(SLOT_SLACK);
        // Holding none of the pool, every slot is downstream and comes back.
        if n == 0 || u64::from(want) <= stats.max_slots {
            return Ok(false);
        }
        if !ctx.grow_slot_cap(want.min(MAX_SLOTS)) {
            return Err(VideoDecError::PoolTooSmall { slots: stats.max_slots, held: n });
        }
        log!(&*ctx, Level::Debug, "grow_slot_cap", held = n, slots = want);
        Ok(true)
    }

    fn fatal(&mut self, ctx: &Ctx, error: VideoDecError) -> Error {
        self.drop_front();
        Error::element_failed(ctx.element(), error)
    }

    fn emit(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        while let Some(p) = self.ready.front() {
            let shape = p.shape();
            let (width, height, pixfmt, color) = shape;
            // A layout the importer takes with the link's modifier, the picture's own or NV12.
            let link = match self.dmabuf.as_mut().map(|d| d.target(pixfmt)) {
                Some(Ok(link)) => link,
                Some(Err(e)) => return Err(self.fatal(ctx, e)),
                None => None,
            };
            let own = dmabuf_layout(pixfmt).map(|(fourcc, _)| fourcc);
            // An NV12 link of a picture laid out otherwise is interleaved into the slot.
            let nv12 =
                link.is_some_and(|(f, _)| f == DRM_FORMAT_NV12) && own != Some(DRM_FORMAT_NV12);
            let layout = link.and_then(|(fourcc, _)| gpuframe::layout_of(fourcc));
            if self.shape != Some(shape) {
                let level = if self.shape.is_none() { Level::Info } else { Level::Debug };
                log!(
                    &*ctx,
                    level,
                    "announce",
                    width = width,
                    height = height,
                    pixfmt = pixfmt,
                    gpu = link.is_some(),
                );
                match (link, layout) {
                    (Some((_, modifier)), Some(layout)) => {
                        announce_gpu(ctx, width, height, layout, modifier, color)
                    }
                    _ => announce_with(ctx, width, height, pixfmt, color),
                }
                self.shape = Some(shape);
            }
            if let (Ready::Sent(sent), Some((fourcc, _))) = (p, link)
                && sent.layout == HeldLayout::Native
                && own == Some(fourcc)
                && Dmabuf::of(&sent.memory).is_ok()
            {
                self.export_sent(ctx, fourcc)?;
                continue;
            }
            let Some(mut buf) = ctx.try_alloc() else {
                match self.pool_too_small(ctx) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => return Err(Error::element_failed(ctx.element(), error)),
                }
                // A dry pool freezes the graph, so say why once.
                if !self.stalled {
                    self.stalled = true;
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
                return Ok(false);
            };
            self.stalled = false;
            let need = match p {
                _ if nv12 => nv12_size(width, height),
                Ready::Owned(p) => p.planes.iter().map(Vec::len).sum(),
                Ready::Held(h) => packed_size(h.pixfmt, h.width, h.height),
                Ready::Sent(s) => packed_size(s.pixfmt, s.width, s.height),
            };
            let capacity = buf.memory.capacity();
            // Dmabuf mode sizes its slots to the stream, a larger picture grows them.
            if capacity < need && self.dmabuf.is_some() && ctx.grow_slots(need) {
                log!(&*ctx, Level::Debug, "grow_slots", bytes = need);
                drop(buf);
                continue;
            }
            if capacity < need {
                let error = VideoDecError::SlotTooSmall { width, height, pixfmt, need, capacity };
                return Err(self.fatal(ctx, error));
            }
            // A dmabuf slot's CPU write is bracketed by the sync ioctls (dma-buf.h).
            let dmabuf = match link {
                Some(_) => {
                    match Dmabuf::of(&buf.memory).and_then(|d| sync_write(d.fd, true).map(|()| d)) {
                        Ok(d) => Some(d),
                        Err(e) => return Err(self.fatal(ctx, VideoDecError::Dmabuf(e))),
                    }
                }
                None => None,
            };
            let dst = buf.memory.as_mut_full();
            let scratch = self.dmabuf.as_mut().map(|d| &mut d.scratch);
            let (pts, duration) = match p {
                Ready::Owned(p) if nv12 => {
                    let f = PixelFormat::from_caps_name(p.pixfmt).unwrap_or(PixelFormat::I420);
                    let mut planes = [PlaneRef::EMPTY; 3];
                    for (i, (plane, data)) in planes.iter_mut().zip(&p.planes).enumerate() {
                        let stride = plane_geometry(f, width, height, i).map_or(0, |g| g.stride);
                        *plane = PlaneRef { data, stride };
                    }
                    let packed =
                        pack_nv12(dst, scratch, HeldLayout::Native, f, width, height, &planes);
                    if let Err(error) = packed {
                        return Err(self.fatal(ctx, error));
                    }
                    (p.pts, p.duration)
                }
                Ready::Owned(p) => {
                    let mut at = 0;
                    for plane in &p.planes {
                        dst[at..at + plane.len()].copy_from_slice(plane);
                        at += plane.len();
                    }
                    (p.pts, p.duration)
                }
                Ready::Held(h) => {
                    let planes = self.codec.held_planes(h.handle);
                    let packed = match (h.layout, h.pixfmt) {
                        _ if nv12 => {
                            pack_nv12(dst, scratch, h.layout, h.pixfmt, h.width, h.height, &planes)
                        }
                        (HeldLayout::Native, _) => {
                            pack_planes(dst, h.pixfmt, h.width, h.height, &planes)
                        }
                        (HeldLayout::Planar10, PixelFormat::P010) => {
                            pack_p010(dst, h.width, h.height, &planes)
                        }
                        (HeldLayout::Reduce { chroma_shift, bits }, f) => {
                            pack_reduced(dst, h.width, h.height, chroma_shift, bits, f, &planes)
                        }
                        (HeldLayout::Gray, PixelFormat::I420) => {
                            pack_gray(dst, h.width, h.height, &planes)
                        }
                        (layout, _) => Err(VideoDecError::HeldLayout { layout, pixfmt }),
                    };
                    let (pts, duration, handle) = (h.pts, h.duration, h.handle);
                    match packed {
                        Ok(n) => debug_assert_eq!(n, need),
                        Err(error) => {
                            self.drop_front();
                            return Err(Error::element_failed(ctx.element(), error));
                        }
                    }
                    self.codec.release_held(handle);
                    (pts, duration)
                }
                Ready::Sent(s) => {
                    let planes = s.planes();
                    let packed = match (s.layout, s.pixfmt) {
                        _ if nv12 => {
                            pack_nv12(dst, scratch, s.layout, s.pixfmt, s.width, s.height, &planes)
                        }
                        (HeldLayout::Native, _) => {
                            pack_planes(dst, s.pixfmt, s.width, s.height, &planes)
                        }
                        (HeldLayout::Planar10, PixelFormat::P010) => {
                            pack_p010(dst, s.width, s.height, &planes)
                        }
                        (HeldLayout::Reduce { chroma_shift, bits }, f) => {
                            pack_reduced(dst, s.width, s.height, chroma_shift, bits, f, &planes)
                        }
                        (HeldLayout::Gray, PixelFormat::I420) => {
                            pack_gray(dst, s.width, s.height, &planes)
                        }
                        (layout, _) => Err(VideoDecError::HeldLayout { layout, pixfmt }),
                    };
                    match packed {
                        Ok(n) => debug_assert_eq!(n, need),
                        Err(error) => {
                            self.drop_front();
                            return Err(Error::element_failed(ctx.element(), error));
                        }
                    }
                    (s.pts, s.duration)
                }
            };
            buf.memory.set_len(need);
            buf.pts = pts;
            buf.duration = duration;
            buf.flags = self.watch.preview_flag(pts);
            log!(&*ctx, Level::Trace, "frame", pts = buf.pts);
            let side = self.carry.take(pts.0).unwrap_or(&[]);
            match (dmabuf, link, self.dmabuf.as_mut()) {
                (Some(dmabuf), Some((fourcc, _)), Some(d)) => {
                    let pixfmt = PixelFormat::from_caps_name(pixfmt).unwrap_or(PixelFormat::I420);
                    let ended = sync_write(dmabuf.fd, false).map_err(VideoDecError::Dmabuf);
                    let (packed, n, coded) = match nv12 {
                        true => nv12_layout(width, height),
                        false => {
                            let n = pixfmt.plane_count();
                            (packed_layout(pixfmt, width, height), n, (width, height))
                        }
                    };
                    let size = Size { coded, shown: (width, height) };
                    let sent = ended.and_then(|()| {
                        export(d, ctx, buf, dmabuf, fourcc, &packed[..n], size, side)
                    });
                    if let Err(error) = sent {
                        // Held was already released, a later drop_all must not see it again.
                        self.ready.pop_front();
                        return Err(Error::element_failed(ctx.element(), error));
                    }
                }
                _ => {
                    attach_side(ctx, &mut buf.memory, side);
                    ctx.out(SRC_PAD).push(buf);
                }
            }
            self.ready.pop_front();
        }
        Ok(true)
    }

    fn export_sent(&mut self, ctx: &mut Ctx, fourcc: u32) -> Result<(), Error> {
        let Some(Ready::Sent(sent)) = self.ready.pop_front() else {
            return Ok(());
        };
        let ended = Dmabuf::of(&sent.memory).and_then(|d| sync_write(d.fd, false).map(|()| d));
        let dmabuf =
            ended.map_err(|e| Error::element_failed(ctx.element(), VideoDecError::Dmabuf(e)))?;
        let mut planes = [(0, 0); 3];
        let n = sent.pixfmt.plane_count();
        for (p, plane) in planes.iter_mut().enumerate().take(n) {
            *plane = (sent.offsets[p], sent.strides[p]);
        }
        let mut buf = ctx.buffer_of(sent.memory);
        buf.pts = sent.pts;
        buf.duration = sent.duration;
        buf.flags = self.watch.preview_flag(sent.pts);
        log!(&*ctx, Level::Trace, "frame", pts = buf.pts);
        let side = self.carry.take(sent.pts.0).unwrap_or(&[]);
        let Some(d) = self.dmabuf.as_mut() else {
            return Ok(());
        };
        let shown = (sent.width, sent.height);
        let size = Size { coded: shown, shown };
        let sent = export(d, ctx, buf, dmabuf, fourcc, &planes[..n], size, side);
        sent.map_err(|error| Error::element_failed(ctx.element(), error))
    }

    fn after_call(&mut self, ctx: &mut Ctx, pictures: u64) -> Result<(), Error> {
        let w = &mut self.watch;
        if let Some(e) = w.failed.take() {
            return Err(e);
        }
        if w.pictures != pictures {
            w.silent = 0;
            w.settled = true;
            return Ok(());
        }
        if !w.warned || w.settled {
            return Ok(());
        }
        w.silent += 1;
        if w.silent >= SILENT_UNITS {
            w.settled = true;
            let element = ctx.element();
            let error =
                Error::element_failed(element, VideoDecError::NothingShown { units: w.silent });
            ctx.post(BusMessage::Warning { element, error });
        }
        Ok(())
    }

    fn report_refused(&mut self, ctx: &Ctx) {
        if let Some(d) = self.dmabuf.as_mut()
            && d.refused > 0
        {
            log!(ctx, Level::Warn, "channel_refused", frames = std::mem::take(&mut d.refused));
        }
    }

    fn drop_front(&mut self) {
        if let Some(Ready::Held(h)) = self.ready.pop_front() {
            self.codec.release_held(h.handle);
        }
    }

    fn drop_all(&mut self) {
        while !self.ready.is_empty() {
            self.drop_front();
        }
    }
}

impl<C: Codec> Default for VideoDec<C> {
    fn default() -> Self {
        Self::new()
    }
}

fn too_late(ctx: &Ctx) -> bool {
    ctx.feedback(SRC_PAD).qos.is_some_and(|q| q.lateness > 0)
}

impl<C: Codec> VideoDec<C> {
    fn gather_side(&mut self, inbuf: &Buffer) {
        let mut scan = Scan::new(Writer::new(&mut self.side));
        self.codec.scan(inbuf.memory.data(), &mut scan);
        scan.side.merge_under(inbuf.memory.side());
        self.carry.put(inbuf.pts.0, scan.side.bytes());
    }

    fn tell_late(&mut self, ctx: &Ctx, late: bool) {
        if std::mem::replace(&mut self.late, late) != late {
            log!(ctx, Level::Debug, "late", late = late);
            self.codec.late(late);
        }
    }
}

impl<C: Codec> Element for VideoDec<C> {
    fn desc(&self) -> &'static ElementDesc {
        self.dmabuf.as_ref().map_or(C::DESC, |d| d.desc)
    }

    // Once per element at preroll.
    #[allow(clippy::disallowed_methods)]
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let Some(d) = &self.dmabuf else { return Ok(()) };
        let raw = C::DESC.srcs.first().map_or(&[][..], |pad| pad.offers);
        // dmabufs first, so a sink taking both is handed the slots
        gpuframe::with_gpu_offers(&d.formats, |gpu| {
            let mut all: Vec<OfferSpec<'_>> = gpu.to_vec();
            all.extend_from_slice(raw);
            ctx.offer_src(SRC_PAD, &all)
        })
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.codec.start(ctx);
        self.watch = Watch::default();
        self.takes = link_takes(ctx);
        if let Some(d) = &mut self.dmabuf {
            d.link = gpuframe::link_layout(ctx, ctx.negotiated_src(SRC_PAD));
            log!(&*ctx, Level::Debug, "start", gpu = d.link.is_some(), formats = d.formats.len());
            if d.link.is_some() && d.tx.is_none() {
                let taken = || Error::element_failed(ctx.element(), VideoDecError::ChannelTaken);
                d.tx = Some(d.channel.take_tx().ok_or_else(taken)?);
            }
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        if let Some(d) = &mut self.dmabuf
            && let Some(tx) = d.tx.take()
        {
            d.channel.put_tx(tx);
        }
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        loop {
            // No slot, stop taking input. Held pictures keep EOS waiting.
            if !self.emit(ctx)? {
                ctx.hold_end();
                return Ok(Flow::Ok);
            }
            // Decode-only takes no slot, so check for a pending seek or skip here.
            if ctx.command_pending() {
                return Ok(Flow::Ok);
            }
            let (inbuf, again) = match self.pending.take() {
                Some(b) => (b, true),
                None => match inputs.pop() {
                    Some(b) => (b, false),
                    None => break,
                },
            };
            let unit = inbuf.memory.data();
            let only = inbuf.flags.contains(BufferFlags::DECODE_ONLY);
            let before = !again && (only || ctx.before_presentation(inbuf.pts, Timestamp::ZERO));
            if (before || !again && too_late(ctx)) && self.codec.disposable(unit) {
                log!(&*ctx, Level::Trace, "skip_disposable", pts = inbuf.pts, before = before);
                ctx.count_drop();
                continue;
            }
            if only && !again {
                self.watch.hidden.note(inbuf.pts);
            }
            if !again {
                self.gather_side(&inbuf);
            }
            let pictures = self.watch.pictures;
            self.watch.warned = false;
            let mut out =
                Out { ready: &mut self.ready, ctx, watch: &mut self.watch, takes: self.takes };
            self.codec.decode_memory(&inbuf.memory, inbuf.pts, inbuf.duration, &mut out);
            self.after_call(ctx, pictures)?;
            // Wait for a slot unless the pool grew, then retry at once.
            if std::mem::take(&mut self.watch.starved) {
                self.pending = Some(inbuf);
                match self.pool_too_small(ctx) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => return Err(Error::element_failed(ctx.element(), error)),
                }
                ctx.hold_end();
                return Ok(Flow::Ok);
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Feedback { feedback: Feedback::Qos(q), .. } => {
                self.tell_late(ctx, q.lateness > 0)
            }
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", ready = self.ready.len());
                self.report_refused(ctx);
                self.tell_late(ctx, false);
                self.carry.clear();
                self.drop_all();
                self.pending = None;
                self.codec.reset();
                self.stalled = false;
                self.shape = None;
                self.watch.silent = 0;
                self.watch.settled = false;
                if let Some(tx) = self.dmabuf.as_mut().and_then(|d| d.tx.as_mut()) {
                    tx.reset();
                }
            }
            // A skip keeps codec state since the pictures it needs are in flight.
            Event::SkippedTo { to_time } => {
                let mut i = 0;
                let mut dropped = 0u32;
                while i < self.ready.len() {
                    if !ctx.before_presentation(self.ready[i].pts(), Timestamp::ZERO) {
                        i += 1;
                        continue;
                    }
                    ctx.count_drop();
                    dropped += 1;
                    if let Some(Ready::Held(h)) = self.ready.remove(i) {
                        self.codec.release_held(h.handle);
                    }
                }
                log!(&*ctx, Level::Debug, "skip", to = *to_time, dropped = dropped);
            }
            // Emit what is held. Without slots the end waits and `process` drains the rest.
            Event::Eos => {
                self.codec.finish(&mut Out {
                    ready: &mut self.ready,
                    ctx,
                    watch: &mut self.watch,
                    takes: self.takes,
                });
                if let Some(e) = self.watch.failed.take() {
                    return Err(e);
                }
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    pictures = self.watch.pictures,
                    ready = self.ready.len(),
                );
                self.report_refused(ctx);
                if !self.emit(ctx)? {
                    ctx.hold_end();
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// A picture's size in its slot and on screen, the slot's rounded up to even for NV12.
#[derive(Clone, Copy)]
struct Size {
    coded: (u32, u32),
    shown: (u32, u32),
}

#[allow(clippy::too_many_arguments)]
fn export(
    d: &mut DmabufOut,
    ctx: &mut Ctx,
    slot: Buffer,
    dmabuf: Dmabuf,
    fourcc: u32,
    layout: &[(usize, usize)],
    Size { coded, shown: (width, height) }: Size,
    side: &[u8],
) -> Result<(), VideoDecError> {
    let Some(tx) = d.tx.as_mut() else {
        return Err(VideoDecError::ChannelTaken);
    };
    d.released.clear();
    tx.drain_released(&mut d.released);
    // Header first, so a refusal drops the slot here and never parks a frame in the ring.
    let Some(mut out) = ctx.alloc_exact(GPU_FRAME_HEADER_LEN) else {
        log!(&*ctx, Level::Warn, "header_refused", pts = slot.pts);
        ctx.count_drop();
        return Ok(());
    };
    let fd = dmabuf.dup().map_err(VideoDecError::Dmabuf)?;
    let mut fds = DmabufFds::default();
    // SAFETY: the dup is this frame's alone. An empty list takes it.
    unsafe { fds.push(fd) };
    let mut planes = Planes::default();
    for &(at, pitch) in layout {
        // At most three planes, so each fits.
        planes.push(ExportedPlane {
            object_index: 0,
            offset: (dmabuf.offset + at) as u32,
            pitch: pitch as u32,
        });
    }
    let token = d.next_token;
    d.next_token = d.next_token.wrapping_add(1);
    let frame = GpuFrame {
        token,
        fds,
        drm_format: fourcc,
        coded_w: coded.0,
        coded_h: coded.1,
        disp_w: width,
        disp_h: height,
        crop_x: 0,
        crop_y: 0,
        planes,
        epoch: ctx.applied_seek_gen(),
        lease: None,
    };
    let (pts, duration) = (slot.pts, slot.duration);
    // The slot is the lease. The sink's drop returns it to the pool.
    if !tx.push(frame, slot.memory) {
        if d.refused == 0 {
            log!(&*ctx, Level::Warn, "channel_full", pts = pts);
        }
        d.refused += 1;
        ctx.count_drop();
        return Ok(());
    }
    let header = GpuFrameHeader {
        token,
        coded_w: coded.0,
        coded_h: coded.1,
        disp_w: width,
        disp_h: height,
        crop_x: 0,
        crop_y: 0,
    };
    header.write_to(out.memory.as_mut_full());
    out.memory.set_len(GPU_FRAME_HEADER_LEN);
    out.pts = pts;
    out.duration = duration;
    attach_side(ctx, &mut out.memory, side);
    ctx.out(SRC_PAD).push(out);
    Ok(())
}

fn attach_side(ctx: &Ctx, memory: &mut Memory, side: &[u8]) {
    if side.is_empty() {
        return;
    }
    if memory.set_side(side).is_err() {
        log!(ctx, Level::Debug, "side_refused", bytes = side.len() as u64);
    }
}

// At construction, never per unit.
#[allow(clippy::disallowed_methods)]
fn side_buffer() -> Box<[u8]> {
    vec![0u8; pf_hdr::side::SIDE_CAP].into_boxed_slice()
}

fn announce_gpu(
    ctx: &mut Ctx,
    width: u32,
    height: u32,
    layout: &'static str,
    modifier: u64,
    color: Color,
) {
    let mut fields = [(F_WIDTH, ValueDesc::Int(width as i64)); 4 + color::FIELDS.len()];
    fields[1] = (F_HEIGHT, ValueDesc::Int(height as i64));
    fields[2] = (F_LAYOUT, ValueDesc::Id(layout));
    fields[3] = (F_MODIFIER, ValueDesc::Int(modifier as i64));
    let n = 4 + color_fields(ctx, SINK_PAD, color, &mut fields[4..]);
    ctx.announce_format(SRC_PAD, FAMILY_VIDEO_GPU, &fields[..n]);
}

/// The importer's layouts the codec writes, NV12 too where it writes I420 since the shell
/// interleaves it, linear only since a CPU writes the slot.
// Once per element, at construction.
#[allow(clippy::disallowed_methods)]
fn written_formats(desc: &ElementDesc, formats: &[DmabufFormat]) -> Vec<DmabufFormat> {
    let writes = |fourcc| {
        codec_writes(desc, fourcc)
            || fourcc == DRM_FORMAT_NV12 && codec_writes(desc, DRM_FORMAT_YUV420)
    };
    formats
        .iter()
        .filter(|f| writes(f.fourcc) && f.modifiers.contains(&DRM_FORMAT_MOD_LINEAR))
        .map(|f| DmabufFormat { fourcc: f.fourcc, modifiers: vec![DRM_FORMAT_MOD_LINEAR] })
        .collect()
}

pub fn announce(ctx: &mut Ctx, width: u32, height: u32, pixfmt: &'static str) {
    announce_with(ctx, width, height, pixfmt, Color::default());
}

pub fn announce_with(ctx: &mut Ctx, width: u32, height: u32, pixfmt: &'static str, color: Color) {
    let mut fields = [(FIELD_WIDTH, ValueDesc::Int(width as i64)); 3 + color::FIELDS.len()];
    fields[1] = (FIELD_HEIGHT, ValueDesc::Int(height as i64));
    fields[2] = (FIELD_PIXFMT, ValueDesc::Id(pixfmt));
    let n = 3 + color_fields(ctx, SINK_PAD, color, &mut fields[3..]);
    ctx.announce_format(SRC_PAD, FAMILY, &fields[..n]);
}

/// The colour fields of an announcement into `out`, the bitstream's over the container's.
pub fn color_fields(
    ctx: &Ctx,
    sink: SinkPad,
    stated: Color,
    out: &mut [(&'static str, ValueDesc)],
) -> usize {
    let container = ctx.negotiated_sink(sink).map_or(Color::default(), |f| Color::read(ctx, f));
    stated.over(container).write_fields(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    static I420: [ValueDesc; 1] = [ValueDesc::Id("i420")];
    static FIELDS: [FieldDesc; 7] = src_fields(&I420);
    static OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &FIELDS }];
    static SRCS: [PadDesc; 1] = src_pad(&OFFERS);
    static DESC: ElementDesc = ElementDesc {
        name: "i420dec",
        sinks: &[],
        srcs: &SRCS,
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    };

    #[test]
    fn a_codec_writes_the_layouts_its_offers_name() {
        assert_eq!(dmabuf_layout("i420").map(|(f, _)| f), Some(DRM_FORMAT_YUV420));
        assert_eq!(dmabuf_layout("nv12").map(|(f, _)| f), Some(DRM_FORMAT_NV12));
        assert_eq!(dmabuf_layout("p010").map(|(f, _)| f), Some(DRM_FORMAT_P010));
        assert_eq!(dmabuf_layout("gray8"), None);
        assert!(codec_writes(&DESC, DRM_FORMAT_YUV420));
        assert!(!codec_writes(&DESC, DRM_FORMAT_NV12));
        assert!(!codec_writes(&DESC, 0x3432_5258), "XR24 is no picture of any codec");
    }

    #[test]
    fn a_software_decoder_offers_only_the_linear_modifier() {
        let y_tiled = 0x0100_0000_0000_0002;
        let importer = [
            DmabufFormat {
                fourcc: DRM_FORMAT_YUV420,
                modifiers: vec![y_tiled, DRM_FORMAT_MOD_LINEAR],
            },
            DmabufFormat { fourcc: DRM_FORMAT_NV12, modifiers: vec![DRM_FORMAT_MOD_LINEAR] },
        ];
        let written = written_formats(&DESC, &importer);
        let linear = |fourcc| DmabufFormat { fourcc, modifiers: vec![DRM_FORMAT_MOD_LINEAR] };
        assert_eq!(
            written,
            [linear(DRM_FORMAT_YUV420), linear(DRM_FORMAT_NV12)],
            "NV12 too, the shell interleaves the codec's I420"
        );
        let tiled_only = [DmabufFormat { fourcc: DRM_FORMAT_YUV420, modifiers: vec![y_tiled] }];
        assert!(written_formats(&DESC, &tiled_only).is_empty());
    }
}
