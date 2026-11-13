//! `vaapiav1dec`, the VA-API AV1 decode element. The host parses OBUs and headers and
//! keeps the reference slots, the driver does entropy decoding and everything after.
//! With `apply_grain` the grained picture goes to `current_display_picture`, a second
//! surface, while `current_frame` stays the reference.

pub mod params;

pub use pf_bitstream::av1::parse::{header, obu, refs, seq, stream};
#[cfg(test)]
mod tests;

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
use profluens_video::color::{
    self as colour, ChromaSite, HdrStatic, Matrix, Primaries, Range, TransferFn,
};
use profluens_video::decoder::{COLOR_FIELDS, Color, color_fields};
use profluens_video::format::COLOR_OFFER_FIELDS;

use crate::error::{Transient, VaapiErrorKind as K, Verdict, Warned, failed, log_fatal};
use crate::ffi;
use crate::gpuframe::{
    self, F_LAYOUT, F_MODIFIER, FAMILY_VIDEO_GPU, FrameToken, FrameTx, GPU_FRAME_HEADER_LEN,
    GpuFrame, GpuFrameChannel, GpuFrameHeader,
};
use crate::probe;
use crate::va::{
    Buffer as VaBuffer, Config, Context, Display, ExportedSurface, ReadbackImage, SurfaceFormat,
    Surfaces, VaError,
};
use crate::vp9dec::{CopyFail, PendingOut, copy_out, frame_bytes};
use pf_bitstream::av1::{Dpb, Ref};
use pf_bitstream::{DpbError, Event as DpbEvent, Events, Fate, Slot};

pub use header::{FrameHeader, FrameType};
pub use obu::Av1Error;
pub use seq::{ColorConfig, SequenceHeader};
pub use stream::{Frame, Group, Stream};

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
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "av1", fields: &COLOR_FIELDS }];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

// Cold: make_default boxes one element when the registry builds the default.
#[allow(clippy::disallowed_methods)]
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "vaapiav1dec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VaapiAv1Dec::new())),
    io: false,
    couplings: &[],
};

static SRCS_ZEROCOPY: [PadDesc; 1] = [PadDesc { name: "src", offers: OfferDesc::PROBED }];
static DESC_ZEROCOPY: ElementDesc = ElementDesc {
    name: "vaapiav1dec",
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

use header::NUM_REF_FRAMES;

/// A unit of more frames, as SVT-AV1's hierarchical alt-refs after a key frame, waits
/// for as many free surfaces as it decodes frames.
const UNIT_RESERVE: usize = 2;

/// A decoded frame, row `Slot` of the DPB's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pic {
    /// the reference, `current_frame`
    pub surface: ffi::VASurfaceID,
    /// the picture put out, grained when `apply_grain`, else `surface`
    pub display: ffi::VASurfaceID,
    pub width: u32,
    pub height: u32,
}

/// The reference surfaces of the DPB's slots, the ungrained ones.
pub(crate) fn ref_surfaces(
    slots: &[Option<Ref>; NUM_REF_FRAMES],
    pics: &[Pic],
) -> [ffi::VASurfaceID; NUM_REF_FRAMES] {
    let mut refs = [ffi::VA_INVALID_SURFACE; NUM_REF_FRAMES];
    for (r, s) in refs.iter_mut().zip(slots) {
        if let Some(p) = s.and_then(|s| pics.get(usize::from(s.slot.0))) {
            *r = p.surface;
        }
    }
    refs
}

/// A DPB refusal as the warning the element names it by, the slot index from the header.
pub(crate) fn refusal(slots: &[Option<Ref>; NUM_REF_FRAMES], h: &FrameHeader, e: DpbError) -> K {
    let empty = |i: u8| slots[usize::from(i & 7)].is_none();
    match e {
        DpbError::MissingReference if h.show_existing_frame => {
            K::Av1EmptyRefSlot { slot: h.frame_to_show_map_idx & 7 }
        }
        DpbError::MissingReference => match h.ref_frame_idx.iter().find(|&&i| empty(i)) {
            Some(&slot) => K::Av1EmptyRefSlot { slot },
            None => K::Dpb(e),
        },
        DpbError::NotShowable => K::Av1Stream(Av1Error::Invalid("show_existing_frame")),
        e => K::Dpb(e),
    }
}

pub(crate) fn stated_color(c: &ColorConfig) -> Color {
    Color {
        matrix: Matrix::from_h273(c.matrix_coefficients),
        range: Some(if c.color_range { Range::Full } else { Range::Limited }),
        transfer: TransferFn::from_h273(c.transfer_characteristics),
        primaries: Primaries::from_h273(c.color_primaries),
        chroma: ChromaSite::from_av1(u32::from(c.chroma_sample_position)),
        ..Color::default()
    }
}

pub(crate) fn supported(s: &SequenceHeader) -> bool {
    let c = &s.color;
    s.seq_profile == 0
        && !c.mono_chrome
        && c.subsampling_x
        && c.subsampling_y
        && matches!(c.bit_depth, 8 | 10)
}

struct VaState {
    // Field order is drop order, the context goes before its surfaces and its config.
    context: Context,
    surfaces: Surfaces,
    _config: Config,
    format: SurfaceFormat,
    grain: bool,
    free: VecDeque<ffi::VASurfaceID>,
    coded_w: u32,
    coded_h: u32,
    readback: Option<ReadbackImage>,
}

pub struct VaapiAv1Dec {
    // Field order is drop order, the buffers and the session go before the display they
    // live on.
    group_bufs: Vec<VaBuffer>,
    va: Option<VaState>,
    display: Option<Display>,
    device: Option<PathBuf>,

    dpb: Dpb,
    /// the DPB's slots, a row per target surface seen
    slots: Vec<Pic>,
    /// per slot, held by the DPB
    held: Vec<bool>,
    /// per slot, the picture its next `Output` puts out
    outs: Vec<Option<PendingOut>>,
    /// The first picture goes at once and holds the next decode until its group put out.
    first: crate::firstout::FirstOut,
    /// Spatial layers shown in the temporal unit in hand, one bit each.
    shown_layers: u8,
    tiles: Vec<ffi::VASliceParameterBufferAV1>,
    output_queue: VecDeque<PendingOut>,
    side: crate::colour::HdrCarry,
    hdr: HdrStatic,
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
    tx: Option<FrameTx>,
    released: Vec<FrameToken>,
    next_token: FrameToken,
    in_flight: Vec<(ffi::VASurfaceID, FrameToken)>,
    link: Option<(u32, u64)>,
    only: bool,
}

// SAFETY: `SchedHint::Active`, the element runs on one scheduler thread and never
// shares its `Display`. The display preroll opens moves to that thread with the element.
unsafe impl Send for VaapiAv1Dec {}

impl Default for VaapiAv1Dec {
    fn default() -> Self {
        Self::new()
    }
}

impl VaapiAv1Dec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::with_zerocopy(None)
    }

    pub fn new_zerocopy(channel: Arc<GpuFrameChannel>) -> Self {
        Self::with_zerocopy(Some(channel))
    }

    // Cold: once per element. The lists are sized for their bound here.
    #[allow(clippy::disallowed_methods)]
    fn with_zerocopy(zerocopy: Option<Arc<GpuFrameChannel>>) -> Self {
        VaapiAv1Dec {
            va: None,
            display: None,
            device: None,
            dpb: Dpb::new(),
            slots: Vec::with_capacity(32),
            held: Vec::with_capacity(32),
            outs: Vec::with_capacity(32),
            first: crate::firstout::FirstOut::new(),
            shown_layers: 0,
            tiles: Vec::with_capacity(64),
            group_bufs: Vec::with_capacity(32),
            output_queue: VecDeque::with_capacity(16),
            side: crate::colour::HdrCarry::new(),
            hdr: HdrStatic::default(),
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
            tx: None,
            released: Vec::with_capacity(16),
            next_token: 1,
            in_flight: Vec::with_capacity(16),
            link: None,
            only: false,
        }
    }

    fn warn(&mut self, ctx: &mut Ctx, kind: K) {
        if !self.warned.note(ctx, kind) {
            return;
        }
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: failed(ctx, DESC.name, kind) });
    }

    fn picture_failed(&mut self, ctx: &mut Ctx, e: &VaError, kind: K) {
        match self.transient.judge(e) {
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

    // Cold: VA setup, once per stream and per sequence that outgrows it.
    #[allow(clippy::disallowed_methods)]
    fn ensure_va(&mut self, ctx: &mut Ctx, seq: &SequenceHeader, key: bool) -> bool {
        let ten_bit = seq.color.bit_depth == 10;
        let format = if ten_bit { SurfaceFormat::P010 } else { SurfaceFormat::NV12 };
        let grain = seq.film_grain_params_present;
        let (w, h) = (seq.max_frame_width, seq.max_frame_height);
        let (mut coded_w, mut coded_h) = (w.next_multiple_of(16), h.next_multiple_of(16));
        if let Some(va) = &self.va {
            if va.format == format && va.coded_w >= w && va.coded_h >= h && (va.grain || !grain) {
                return true;
            }
            if !key {
                self.warn(ctx, K::Av1Resize { width: w, height: h });
                return false;
            }
            if va.format == format {
                (coded_w, coded_h) = (coded_w.max(va.coded_w), coded_h.max(va.coded_h));
            }
            log!(
                &*ctx,
                Level::Debug,
                "reinit",
                width = w,
                height = h,
                ten_bit = ten_bit,
                grain = grain
            );
        }
        self.teardown_session(ctx);
        let Some(display) = &self.display else {
            return false;
        };
        let per_frame = 1 + u32::from(grain);
        let refs = NUM_REF_FRAMES as u32 * per_frame;
        let reserve = UNIT_RESERVE as u32 * per_frame;
        let count = crate::va::decode_surface_count(refs, reserve, self.zerocopy.is_some());
        log!(&*ctx, Level::Info, "surfaces", count = count, width = coded_w, height = coded_h);
        log!(
            &*ctx,
            Level::Info,
            "session",
            ten_bit = ten_bit,
            grain = grain,
            zero_copy = self.zerocopy.is_some()
        );
        let readback_mode = self.zerocopy.is_none();
        let built =
            Config::new_decode_for(display, ffi::VAProfileAV1Profile0, format).and_then(|config| {
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
        let free = surfaces.ids().iter().copied().collect();
        self.row = if readback_mode { vec![0u8; coded_w as usize * 2] } else { Vec::new() };
        self.va = Some(VaState {
            _config: config,
            context,
            format,
            grain,
            surfaces,
            free,
            coded_w,
            coded_h,
            readback,
        });
        true
    }

    fn teardown_session(&mut self, ctx: &Ctx) {
        // the frames die with the session, the parser keeps its slots for the key frame in hand
        let mut ev = Events::new();
        self.dpb.drop_frames(&mut ev);
        self.apply(&ev);
        self.slots.clear();
        self.held.clear();
        self.outs.clear();
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
        self.slots
            .iter()
            .zip(&self.held)
            .any(|(p, &held)| held && (p.surface == id || p.display == id))
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

    /// A frame that failed, the frames up to the next key frame parse and are skipped. `quiet`
    /// when the DPB waited for a key frame already.
    fn lose(&mut self, ctx: &mut Ctx, quiet: bool, kind: Option<K>) {
        if let Some(kind) = kind.filter(|_| !quiet) {
            self.warn(ctx, kind);
        }
        self.dpb.await_key();
    }

    fn decode_unit(&mut self, ctx: &mut Ctx, tu: &[u8], pts: Timestamp, duration: Timestamp) {
        let mut at = 0;
        self.shown_layers = 0;
        loop {
            let quiet = self.dpb.awaiting_key();
            // a parse error resets the parser and waits for a key frame
            let frame = match self.dpb.next_frame(tu, &mut at) {
                Ok(Some(f)) => f,
                Ok(None) => return,
                Err(e) => return self.lose(ctx, quiet, Some(K::Av1Stream(e))),
            };
            if !quiet && self.dpb.awaiting_key() {
                // A new sequence starts at a key frame, section 7.5.
                log!(&*ctx, Level::Debug, "seq_change", key = false, pts = pts);
            }
            if !self.frame(ctx, tu, &frame, pts, duration) {
                return;
            }
        }
    }

    /// False once the element failed.
    fn frame(
        &mut self,
        ctx: &mut Ctx,
        tu: &[u8],
        frame: &Frame,
        pts: Timestamp,
        duration: Timestamp,
    ) -> bool {
        let h = &frame.header;
        let quiet = self.dpb.awaiting_key();
        let mut ev = Events::new();
        let fate = match self.dpb.begin(h, &mut ev) {
            Ok(fate) => fate,
            Err(e) => {
                let kind = refusal(self.dpb.slots(), h, e);
                self.lose(ctx, quiet, Some(kind));
                return true;
            }
        };
        if fate == Fate::Skip {
            if ev.as_slice().is_empty() {
                log!(&*ctx, Level::Trace, "await_key", pts = pts);
            } else {
                self.show_again(ctx, frame.spatial_id, &ev, quiet, pts, duration);
            }
            return true;
        }
        let Some(seq) = self.dpb.stream().seq else {
            self.abandon(ctx, h, Some(K::Av1Stream(Av1Error::NoSequenceHeader)));
            return true;
        };
        if !supported(&seq) {
            let c = &seq.color;
            self.fail(
                ctx,
                K::UnsupportedAv1 {
                    profile: seq.seq_profile,
                    bit_depth: c.bit_depth,
                    mono_chrome: c.mono_chrome,
                    subsampling_x: c.subsampling_x,
                    subsampling_y: c.subsampling_y,
                },
            );
            self.abandon(ctx, h, None);
            return false;
        }
        if h.show_frame && !self.take_shown(frame.spatial_id) {
            self.abandon(ctx, h, Some(K::Av1Stream(Av1Error::Invalid("show_frame"))));
            return true;
        }
        if !self.ensure_va(ctx, &seq, h.frame_type == FrameType::Key) {
            self.abandon(ctx, h, None);
            return !self.failed;
        }
        // A size past the surfaces would overrun the driver's write and the readback row.
        if let Some(va) = &self.va
            && (h.upscaled_width > va.coded_w || h.frame_height > va.coded_h)
        {
            self.abandon(ctx, h, Some(K::Av1Stream(Av1Error::Invalid("frame_size"))));
            return true;
        }
        let Some(target) = self.acquire_surface() else {
            self.abandon(ctx, h, Some(K::NoFreeSurface));
            return true;
        };
        let display = match h.film_grain.apply_grain {
            false => Some(target),
            true => self.acquire_surface(),
        };
        let Some(display) = display else {
            self.release_if_unreferenced(target);
            self.abandon(ctx, h, Some(K::NoFreeSurface));
            return true;
        };
        if let Err(e) = self.submit(&seq, frame, tu, target, display) {
            self.picture_failed(ctx, &e, K::Submit(e));
            self.release_if_unreferenced(target);
            self.release_if_unreferenced(display);
            self.abandon(ctx, h, None);
            return !self.failed;
        }
        let (width, height) = (h.upscaled_width, h.frame_height);
        let out = match h.show_frame {
            true => self
                .goes_out(ctx, pts)
                .and_then(|preview| self.pending(display, width, height, pts, duration, preview)),
            false => None,
        };
        self.decoded(h, fate, Pic { surface: target, display, width, height }, out);
        true
    }

    /// A frame the DPB began that was never decoded. The slots it would have taken are let go,
    /// so a frame naming one of them fails instead of decoding against the frame before.
    fn abandon(&mut self, ctx: &mut Ctx, h: &FrameHeader, kind: Option<K>) {
        if let Some(kind) = kind {
            self.warn(ctx, kind);
        }
        let mut ev = Events::new();
        self.dpb.abandon(h, &mut ev);
        self.apply(&ev);
    }

    /// Ends a frame decoded into `pic` with `fate`, `out` what it puts out. One shown but
    /// without `out` is decoded for the references alone.
    fn decoded(&mut self, h: &FrameHeader, fate: Fate, pic: Pic, out: Option<PendingOut>) {
        let slot = self.slot_of(pic.surface);
        let i = usize::from(slot.0);
        self.slots[i] = pic;
        self.held[i] = true;
        let mut h = *h;
        h.show_frame &= out.is_some();
        self.outs[i] = out;
        let mut ev = Events::new();
        self.dpb.end(&h, fate, slot, &mut ev);
        self.apply(&ev);
    }

    /// A frame shown again, its `Output` first in `ev`. A key frame took every slot after it.
    fn show_again(
        &mut self,
        ctx: &mut Ctx,
        spatial_id: u8,
        ev: &Events,
        quiet: bool,
        pts: Timestamp,
        duration: Timestamp,
    ) {
        let shown = self.take_shown(spatial_id);
        if shown
            && let Some(&DpbEvent::Output(slot)) = ev.as_slice().first()
            && let Some(preview) = self.goes_out(ctx, pts)
        {
            let i = usize::from(slot.0);
            let p = self.slots[i];
            self.outs[i] = self.pending(p.display, p.width, p.height, pts, duration, preview);
        }
        self.apply(ev);
        if !shown {
            self.lose(ctx, quiet, Some(K::Av1Stream(Av1Error::Invalid("show_existing_frame"))));
        }
    }

    /// One shown frame per spatial layer and temporal unit (7.5). False for a second one,
    /// which is how a hostile unit asks for a readback per 3-byte header.
    fn take_shown(&mut self, spatial_id: u8) -> bool {
        let bit = 1u8 << (spatial_id & 3);
        let first = self.shown_layers & bit == 0;
        self.shown_layers |= bit;
        first
    }

    /// Carries out what the DPB asked, in order.
    fn apply(&mut self, ev: &Events) {
        for e in ev.as_slice() {
            match *e {
                DpbEvent::Output(slot) => {
                    if let Some(out) = self.outs[usize::from(slot.0)].take() {
                        self.output_queue.push_back(out);
                    }
                }
                DpbEvent::Release(slot) => {
                    let i = usize::from(slot.0);
                    (self.held[i], self.outs[i]) = (false, None);
                    let p = self.slots[i];
                    self.release_if_unreferenced(p.surface);
                    self.release_if_unreferenced(p.display);
                }
            }
        }
    }

    /// The DPB's name of target surface `id`, a slot per surface seen.
    fn slot_of(&mut self, id: ffi::VASurfaceID) -> Slot {
        let i = match self.slots.iter().position(|p| p.surface == id) {
            Some(i) => i,
            None => {
                self.slots.push(Pic { surface: id, display: id, width: 0, height: 0 });
                self.held.push(false);
                self.outs.push(None);
                self.slots.len() - 1
            }
        };
        Slot(i as u16)
    }

    /// Lets go of every frame the DPB holds, the parser starts over at the next key frame.
    fn discard_dpb(&mut self) {
        let mut ev = Events::new();
        self.dpb.discard(&mut ev);
        self.apply(&ev);
    }

    fn submit(
        &mut self,
        seq: &SequenceHeader,
        frame: &Frame,
        tu: &[u8],
        target: ffi::VASurfaceID,
        display: ffi::VASurfaceID,
    ) -> Result<(), VaError> {
        // The buffers are taken out so every exit frees them while the display is open.
        let mut bufs = std::mem::take(&mut self.group_bufs);
        let r = self.render(seq, frame, tu, target, display, &mut bufs);
        bufs.clear();
        self.group_bufs = bufs;
        r
    }

    fn render(
        &mut self,
        seq: &SequenceHeader,
        frame: &Frame,
        tu: &[u8],
        target: ffi::VASurfaceID,
        display: ffi::VASurfaceID,
        bufs: &mut Vec<VaBuffer>,
    ) -> Result<(), VaError> {
        let (Some(dpy), Some(va)) = (&self.display, &self.va) else {
            return Ok(());
        };
        let refs = ref_surfaces(self.dpb.slots(), &self.slots);
        let pic = params::pic_param(seq, &frame.header, refs, target, display);
        let pic_buf =
            VaBuffer::new_struct(dpy, &va.context, ffi::VAPictureParameterBufferType, &pic)?;
        let st = self.dpb.stream();
        for g in &st.groups {
            self.tiles.clear();
            let tiles = st.tiles.get(g.first..g.first + g.count).unwrap_or(&[]);
            self.tiles.extend(tiles.iter().map(|t| params::tile_param(t, g)));
            let data = tu.get(g.data_start..g.data_end).unwrap_or(&[]);
            bufs.push(VaBuffer::new_array(
                dpy,
                &va.context,
                ffi::VASliceParameterBufferType,
                &self.tiles,
            )?);
            bufs.push(VaBuffer::new_data(dpy, &va.context, data)?);
        }
        va.context.begin(target)?;
        let rendered = va.context.render(&[pic_buf.id()]).and_then(|()| {
            for [tiles, data] in bufs.as_chunks::<2>().0 {
                va.context.render(&[tiles.id(), data.id()])?;
            }
            Ok(())
        });
        // vaEndPicture runs after a failed render too, so the context is not left mid-picture.
        let ended = va.context.end();
        rendered.and(ended)
    }

    /// Whether a picture at `pts` goes out, and as a preview. A decode-only one counts a drop.
    fn goes_out(&mut self, ctx: &mut Ctx, pts: Timestamp) -> Option<bool> {
        let before = self.only || ctx.before_presentation(pts, Timestamp::ZERO);
        let preview = before && !self.only && ctx.take_preview();
        if before && !preview {
            log!(&*ctx, Level::Trace, "decode_only", pts = pts);
            ctx.count_drop();
            return None;
        }
        Some(preview)
    }

    fn pending(
        &self,
        surface: ffi::VASurfaceID,
        width: u32,
        height: u32,
        pts: Timestamp,
        duration: Timestamp,
        preview: bool,
    ) -> Option<PendingOut> {
        let (Some(va), Some(seq)) = (&self.va, &self.dpb.stream().seq) else {
            return None;
        };
        let color = Color { hdr: self.hdr, dovi: self.side.dovi, ..stated_color(&seq.color) };
        Some(PendingOut {
            surface,
            export: surface,
            pts,
            duration,
            width,
            height,
            coded_w: va.coded_w,
            coded_h: va.coded_h,
            format: va.format,
            color,
            preview,
        })
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
        // a pending command, a seek's flush, cuts the wait short, the decoder runs again after it
        if !crate::va::wait_surface(dpy, out.surface, || ctx.command_pending()) {
            ctx.wake_within(crate::enc::POLL_WAKE);
            return Ok(false);
        }
        let mut exported = match ExportedSurface::export(dpy, out.surface) {
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
        let mut buf = match ctx.try_alloc_sized(GPU_FRAME_HEADER_LEN) {
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
        self.discard_dpb();
        self.first.reset();
    }

    fn drain_all(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        self.drain_presentation_releases();
        if !self.emit_queued(ctx, 0)? {
            ctx.hold_end();
            return Ok(false);
        }
        Ok(true)
    }

    fn surfaces_needed(&self, tu: &[u8]) -> usize {
        let seq = self.dpb.stream().seq.as_ref();
        let reduced = seq.is_some_and(|s| s.reduced_still_picture_header);
        let per_frame = 1 + usize::from(seq.is_some_and(|s| s.film_grain_params_present));
        let want = stream::decoded_frames(tu, reduced).max(1) * per_frame;
        let cap = self
            .va
            .as_ref()
            .map_or(want, |va| va.surfaces.ids().len().saturating_sub(NUM_REF_FRAMES * per_frame));
        want.min(cap.max(per_frame))
    }
}

impl Element for VaapiAv1Dec {
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
            // a picture waits queued while the next decodes, the GPU works on both
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
            self.side.gather_av1(&inbuf.memory, inbuf.pts.0, &mut self.hdr);
            self.decode_unit(ctx, inbuf.memory.data(), inbuf.pts, inbuf.duration);
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
        self.dpb = Dpb::new();
        self.slots.clear();
        self.held.clear();
        self.outs.clear();
        self.first.reset();
        self.va = None;
        self.display = None;
    }
}
