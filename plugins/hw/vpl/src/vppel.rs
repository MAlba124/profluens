//! `vplvpp`, scaling and conversion to nv12 or p010 through the runtime's VPP.
//! A PQ or HLG input bound to 8 bits is tone mapped to BT.709 where offered.

use std::collections::VecDeque;
use std::sync::Arc;

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, OfferDesc, Value};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_video::decoder::Color;
use profluens_video::gpuframe::GpuFrameHeader;

use crate::emit::{Mode, Output, Picture, RAW_OFFERS, VPL_OFFERS};
use crate::error::{MfxError, VplErrorKind as K, failed};
use crate::ffi;
use crate::format::{FAMILY_RAW, InFormat, Layout, RAW_IN_PIXFMTS, fields, read_format};
use crate::frames::{Geometry, SurfaceChannel};
use crate::group::{Joining, VplGroup};
use crate::input::Input;
use crate::park::{Park, Stall, park, stalled};
use crate::probe;
use crate::pts::{PtsMap, Stamp, to_ts90};
use crate::session::{Owner, Surface};
use crate::stats::Meter;
use crate::vppcore::{Plan, Processed, Setup, Target, VppCore, out_pool, plan, plan_fixed};
use profluens_core::buffer::BufferFlags;
use profluens_core::log::Loggable;
use profluens_core::time::Timestamp;

pub const NAME: &str = "vplvpp";
const EXTRA: usize = 64;
/// Outputs in flight at most, under the output pool's floor.
pub const MAX_ASYNC_DEPTH: u16 = 8;
/// How long stop waits for one output still in flight.
const STOP_WAIT_MS: u32 = 1000;
const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

static RAW_IN_FIELDS: [profluens_core::format::FieldDesc; 14] = fields(&RAW_IN_PIXFMTS);
static RAW_IN_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY_RAW, fields: &RAW_IN_FIELDS }];
static RAW_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_IN_OFFERS }];
static VPL_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &VPL_OFFERS }];
static RAW_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &RAW_OFFERS }];
static VPL_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &VPL_OFFERS }];

static PROPS: [PropDesc; 3] = [
    PropDesc { name: "max-width", allowed: Constraint::Any, live: false },
    PropDesc { name: "max-height", allowed: Constraint::Any, live: false },
    PropDesc { name: "depth", allowed: Constraint::Any, live: false },
];

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
        props: &PROPS,
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default,
        io: false,
        couplings: &[],
    }
}

// Cold, registry default.
#[allow(clippy::disallowed_methods)]
fn make_default() -> Box<dyn Element> {
    Box::new(VplVpp::new(Target::default()))
}

static DESCS: [[ElementDesc; 2]; 2] = [
    [desc(&RAW_SINKS, &RAW_SRCS, Some(make_default)), desc(&RAW_SINKS, &VPL_SRCS, None)],
    [desc(&VPL_SINKS, &RAW_SRCS, None), desc(&VPL_SINKS, &VPL_SRCS, None)],
];

struct Pending {
    surface: Surface,
    geom: Geometry,
    stamp: Stamp,
    dts: profluens_core::time::Timestamp,
    flags: profluens_core::buffer::BufferFlags,
    submitted: u64,
}

/// What opening the VPP for a shape did.
enum Opened {
    Ready,
    /// A shape the runtime refuses after an earlier one worked, the input is dropped.
    Refused,
    /// The join is busy, parked, the input waits in `retry`.
    Wait,
}

/// A session surface, its shape and the foreign surface an import shares.
type In = (Surface, Geometry, Option<Surface>);

/// What became of an input handed to the VPP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Push {
    Taken,
    /// Waits in `retry`, a wake is asked for.
    Parked,
    /// The runtime would not read a foreign surface directly.
    Refused(MfxError),
}

pub struct VplVpp {
    target: Target,
    fixed: bool,
    input: Input,
    out: Output,
    group: Option<Arc<VplGroup>>,
    joining: Joining,
    async_depth: Option<u16>,
    meter: Meter,
    core: Option<VppCore>,
    key: Option<(InFormat, Geometry)>,
    refused: Option<(InFormat, Geometry)>,
    in_fmt: Option<InFormat>,
    color: Color,
    /// Outputs in flight, oldest first, as many as the async depth.
    pending: VecDeque<Pending>,
    /// Inputs the runtime still reads, each with the surface an import shares.
    held: VecDeque<(Surface, Option<Surface>)>,
    /// Outputs a flush dropped while the runtime may still make them.
    retired: Vec<Surface>,
    /// The component the last input came from, kept by the VPP's own.
    producer: Option<Arc<Owner>>,
    /// An earlier producer is kept, let go once nothing is in flight.
    settle_due: bool,
    retry: Option<In>,
    /// The runtime may hold a picture and emit it with a later call, so outputs are matched by
    /// stamp.
    stamps: PtsMap,
    /// Keyed as `stamps`, `None` empty, so a frame without a pts keeps its dts and flags.
    extra: [(Option<u64>, Timestamp, BufferFlags); EXTRA],
    extra_next: usize,
    last_out: Option<Timestamp>,
    /// A raw frame taken off the pad while its VPP's join was busy.
    raw_wait: Option<Buffer>,
    ended: bool,
    /// The runtime gave up its last kept picture after the end.
    drained: bool,
    busy: Stall,
    disabled: bool,
    tone_said: bool,
    pub frames: u64,
    /// Test hook, the next calls report the device busy.
    #[cfg(test)]
    inject_busy: Option<Arc<std::sync::atomic::AtomicU32>>,
}

// SAFETY: SchedHint::Active, one thread, the session its own.
unsafe impl Send for VplVpp {}

impl VplVpp {
    pub const IN: SinkPad = IN;
    pub const OUT: SrcPad = OUT;

    // Cold, construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(target: Target) -> Self {
        VplVpp {
            target,
            fixed: false,
            input: Input::raw(),
            out: Output::new(Mode::Raw, OUT, IN),
            group: None,
            joining: Joining::default(),
            async_depth: None,
            meter: Meter::default(),
            core: None,
            key: None,
            refused: None,
            in_fmt: None,
            color: Color::default(),
            pending: VecDeque::with_capacity(usize::from(MAX_ASYNC_DEPTH)),
            held: VecDeque::with_capacity(EXTRA),
            retired: Vec::with_capacity(usize::from(MAX_ASYNC_DEPTH)),
            producer: None,
            settle_due: false,
            retry: None,
            stamps: PtsMap::new(),
            extra: [(None, Timestamp::NONE, BufferFlags::empty()); EXTRA],
            extra_next: 0,
            last_out: None,
            raw_wait: None,
            ended: false,
            drained: false,
            busy: Stall::default(),
            disabled: false,
            tone_said: false,
            frames: 0,
            #[cfg(test)]
            inject_busy: None,
        }
    }

    pub fn new_surfaces(target: Target, channel: Arc<SurfaceChannel>) -> Self {
        VplVpp { input: Input::surfaces(channel), ..Self::new(target) }
    }

    pub fn with_surface_output(mut self, channel: Arc<SurfaceChannel>) -> Self {
        self.out = Output::new(Mode::Vpl(channel), OUT, IN);
        self
    }

    /// The output is always the target's box, the picture centred with bars, one shape for an
    /// encoder.
    pub fn with_fixed_size(mut self) -> Self {
        self.fixed = true;
        self
    }

    /// The chain's join group. Without one the session joins what `PF_VPL_JOIN` says.
    pub fn with_group(mut self, group: Arc<VplGroup>) -> Self {
        self.group = Some(group);
        self
    }

    /// Outputs in flight, 1 to 8. `PF_VPL_ASYNC` sets it for elements that name none.
    pub fn with_async_depth(mut self, depth: u16) -> Self {
        self.async_depth = Some(depth.clamp(1, MAX_ASYNC_DEPTH));
        self
    }

    pub fn meter(&self) -> Meter {
        self.meter
    }

    fn depth(&self) -> u16 {
        probe::runtime().map_or(1, |rt| rt.async_depth(self.async_depth)).min(MAX_ASYNC_DEPTH)
    }

    fn plan_of(&self, w: u32, h: u32, input: Layout) -> Option<Plan> {
        if self.fixed {
            plan_fixed(w, h, input, self.target)
        } else {
            plan(w, h, input, self.target)
        }
    }

    fn warn(ctx: &mut Ctx, kind: K) {
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: failed(ctx, NAME, kind) });
    }

    /// The VPP for `f` at `geom`, opened in the chain's group when the shape is new.
    fn core(&mut self, ctx: &mut Ctx, f: InFormat, geom: Geometry) -> Result<Opened, Error> {
        let p = self.plan_of(geom.disp_w, geom.disp_h, f.layout()).ok_or_else(|| {
            failed(ctx, NAME, K::OddDimensions { width: geom.disp_w, height: geom.disp_h })
        })?;
        if self.key == Some((f, geom)) && self.core.is_some() {
            return Ok(Opened::Ready);
        }
        if self.refused == Some((f, geom)) {
            return Ok(Opened::Refused);
        }
        let rt = probe::runtime().ok_or_else(|| failed(ctx, NAME, K::NoDevice))?;
        let session = match self.joining.open(rt.parent_for(self.group.as_ref()), || rt.session()) {
            Ok(Some(s)) => s,
            Ok(None) => {
                if self.busy.tick(ctx.steady()) {
                    let e = stalled(ffi::MFX_WRN_IN_EXECUTION, "MFXJoinSession");
                    return Err(failed(ctx, NAME, K::Stalled(e)));
                }
                self.meter.park(Park::Busy);
                park(ctx, Park::Busy);
                return Ok(Opened::Wait);
            }
            Err(e) => {
                self.disabled = true;
                return Err(failed(ctx, NAME, K::Session(e)));
            }
        };
        let mut info = probe::frame_info(geom.fourcc, geom.disp_w as u16, geom.disp_h as u16);
        info.Width = geom.coded_w as u16;
        info.Height = geom.coded_h as u16;
        info.CropX = geom.crop_x as u16;
        info.CropY = geom.crop_y as u16;
        let rate = f.rate();
        info.FrameRateExtN = rate.0;
        info.FrameRateExtD = rate.1;
        let depth = self.depth();
        let pool = out_pool(self.out.hold(), depth);
        let core = match VppCore::new(
            rt,
            session,
            info,
            p,
            &self.color,
            Setup { async_depth: depth, pool },
            &*ctx,
        ) {
            Ok(c) => c,
            Err(e) if self.core.is_some() => {
                self.refused = Some((f, geom));
                Self::warn(ctx, e);
                return Ok(Opened::Refused);
            }
            Err(e) => {
                self.disabled = true;
                return Err(failed(ctx, NAME, e));
            }
        };
        if let (Some(e), false) = (core.tone_map_refused, self.tone_said) {
            self.tone_said = true;
            Self::warn(ctx, K::NoToneMap(e));
        }
        log!(
            &*ctx,
            Level::Info,
            "vpp",
            width = p.width,
            height = p.height,
            tone_mapped = core.tone_mapped
        );
        self.core = Some(core);
        self.producer = None;
        self.settle_due = false;
        self.key = Some((f, geom));
        self.busy.clear();
        self.input.reset_session();
        Ok(Opened::Ready)
    }

    /// Sends the done outputs out in order. `false` while the oldest is still owed, or has no room.
    fn flush_pending(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        while let Some(p) = self.pending.front() {
            let color =
                if self.core.as_ref().is_some_and(|c| c.tone_mapped) { sdr() } else { self.color };
            let pic = Picture {
                surface: &p.surface,
                geom: p.geom,
                pts: p.stamp.pts,
                dts: p.dts,
                duration: p.stamp.duration,
                flags: p.flags,
                color,
                fps: self.in_fmt.and_then(|f| f.fps).map(|(n, d)| (n as u32, d as u32)),
                submitted: p.submitted,
            };
            if !self.out.emit(ctx, NAME, &pic, &mut self.meter, &mut |_| {})? {
                return Ok(false);
            }
            self.pending.pop_front();
            self.frames = self.out.frames;
        }
        Ok(true)
    }

    /// Room for another output in flight.
    fn in_flight_room(&self) -> bool {
        self.pending.len() < usize::from(self.depth())
    }

    /// Inputs the runtime let go of are dropped, the rest stay held. Retired outputs go once
    /// made, then old producers once nothing is in flight.
    fn let_go(&mut self) {
        self.held.retain(|(s, _)| s.locked() > 0);
        self.retired.retain(|s| matches!(s.ready(), Ok(false)));
        if self.settle_due
            && self.pending.is_empty()
            && self.retired.is_empty()
            && let Some(c) = &self.core
        {
            c.owner().settle(self.producer.as_ref());
            self.settle_due = false;
        }
    }

    /// The VPP's tasks read `input`, or the surface an import shares, past its handles. Its
    /// component stays open until the VPP's closes or settles.
    fn keep_producer(&mut self, input: &Surface, src: Option<&Surface>) {
        let (Some(from), Some(core)) = (src.unwrap_or(input).owner(), &self.core) else {
            return;
        };
        if self.producer.as_ref().is_some_and(|p| Arc::ptr_eq(p, from)) {
            return;
        }
        core.owner().keep(from);
        self.settle_due |= self.producer.is_some();
        self.producer = Some(Arc::clone(from));
    }

    /// Waits out the outputs still in flight, `true` once the runtime made them all.
    fn quiesce(&mut self) -> bool {
        let pending = self.pending.iter().map(|p| &p.surface);
        pending.chain(&self.retired).all(|s| !matches!(s.sync(STOP_WAIT_MS), Ok(false)))
    }

    fn hold(&mut self, input: Surface, src: Option<Surface>) {
        if input.locked() > 0 {
            self.held.push_back((input, src));
        }
    }

    fn stamp_in(&mut self, inbuf: &Buffer, log: &dyn Loggable) -> u64 {
        let ts = to_ts90(inbuf.pts);
        self.stamps.put(ts, Stamp { pts: inbuf.pts, duration: inbuf.duration, only: false });
        self.extra[self.extra_next] = (Some(ts), inbuf.dts, inbuf.flags);
        self.extra_next = (self.extra_next + 1) % EXTRA;
        log!(log, Level::Trace, "in"; "ts={ts} pts={:?}", inbuf.pts);
        ts
    }

    fn stamp_out(&mut self, out: &Surface, log: &dyn Loggable) -> (Stamp, Timestamp, BufferFlags) {
        let ts = out.timestamp();
        let stamp = self.stamps.take_or_next(ts, true, self.last_out);
        if stamp.pts.is_some() && self.last_out.is_none_or(|l| stamp.pts > l) {
            self.last_out = Some(stamp.pts);
        }
        let key = to_ts90(stamp.pts);
        let (dts, flags) = match self.extra.iter_mut().find(|e| e.0 == Some(key)) {
            Some(e) => {
                let got = (e.1, e.2);
                e.0 = None;
                got
            }
            None => (Timestamp::NONE, BufferFlags::empty()),
        };
        log!(log, Level::Trace, "out"; "ts={ts} pts={:?}", stamp.pts);
        (stamp, dts, flags)
    }

    fn process_one(
        &mut self,
        input: Option<&Surface>,
        log: &dyn Loggable,
    ) -> Option<Result<Processed, K>> {
        if self.injected_busy() {
            return Some(Ok(Processed::Busy));
        }
        self.core.as_mut().map(|c| c.process(input, log))
    }

    #[cfg(test)]
    fn injected_busy(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.inject_busy.as_ref().is_some_and(|n| {
            n.try_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1)).is_ok()
        })
    }

    #[cfg(not(test))]
    fn injected_busy(&self) -> bool {
        false
    }

    /// Parks the input in `retry`, an error once the device stayed busy past the stall limit.
    fn park_retry(&mut self, ctx: &mut Ctx, input: In, p: Park) -> Result<Push, Error> {
        if p == Park::Busy && self.busy.tick(ctx.steady()) {
            let e = stalled(ffi::MFX_WRN_DEVICE_BUSY, "MFXVideoVPP_ProcessFrameAsync");
            return Err(failed(ctx, NAME, K::Stalled(e)));
        }
        self.retry = Some(input);
        self.meter.park(p);
        park(ctx, p);
        Ok(Push::Parked)
    }

    fn push(&mut self, ctx: &mut Ctx, (input, geom, src): In) -> Result<Push, Error> {
        let now = ctx.steady();
        let got =
            self.process_one(Some(&input), &*ctx).ok_or_else(|| failed(ctx, NAME, K::NoSession))?;
        match got {
            Ok(Processed::Out(out)) => {
                let g = self.core.as_ref().map_or(geom, VppCore::geometry);
                self.keep_producer(&input, src.as_ref());
                self.hold(input, src);
                let (stamp, dts, flags) = self.stamp_out(&out, &*ctx);
                let p = Pending { surface: out, geom: g, stamp, dts, flags, submitted: now };
                self.pending.push_back(p);
                self.busy.clear();
                self.input.mark_direct();
                Ok(Push::Taken)
            }
            Ok(Processed::Kept) => {
                self.keep_producer(&input, src.as_ref());
                self.hold(input, src);
                self.busy.clear();
                self.input.mark_direct();
                Ok(Push::Taken)
            }
            Ok(Processed::Busy) => self.park_retry(ctx, (input, geom, src), Park::Busy),
            Ok(Processed::Wait) => self.park_retry(ctx, (input, geom, src), Park::Pool),
            Err(K::Process(e)) if self.input.wants_import(&e) => Ok(Push::Refused(e)),
            Err(e) => {
                Self::warn(ctx, e);
                Ok(Push::Taken)
            }
        }
    }

    /// `false` when the input waits in `retry`. A foreign surface the runtime refuses goes again
    /// imported, where the runtime offers imports.
    fn submit(
        &mut self,
        ctx: &mut Ctx,
        surface: &Surface,
        geom: Geometry,
        inbuf: &Buffer,
    ) -> Result<bool, Error> {
        let Some(f) = self.in_fmt else {
            Self::warn(ctx, K::FrameBeforeFormat);
            return Ok(true);
        };
        match self.core(ctx, f, geom)? {
            Opened::Ready => {}
            Opened::Refused => {
                ctx.count_drop();
                return Ok(true);
            }
            // The foreign surface waits as it is, it is made the session's on the next try.
            Opened::Wait => {
                self.retry = Some((surface.clone(), geom, None));
                self.stamp_in(inbuf, &*ctx);
                return Ok(false);
            }
        }
        let ts = self.stamp_in(inbuf, &*ctx);
        let mut imported = false;
        loop {
            let core = self.core.as_ref().ok_or_else(|| failed(ctx, NAME, K::NoSession))?;
            let input =
                self.input.for_session(core.owner(), surface).map_err(|e| failed(ctx, NAME, e))?;
            input.set_timestamp(ts);
            // An import shares the foreign surface's memory, which stays held with it.
            let src = (!self.input.is_direct()).then(|| surface.clone());
            match self.push(ctx, (input, geom, src))? {
                Push::Taken => return Ok(true),
                Push::Parked => return Ok(false),
                Push::Refused(e) if !imported && probe::probe().is_some_and(|c| c.import) => {
                    log!(&*ctx, Level::Info, "vpp_import", status = e.status);
                    self.input.mark_indirect();
                    imported = true;
                }
                Push::Refused(e) => {
                    Self::warn(ctx, K::Process(e));
                    return Ok(true);
                }
            }
        }
    }

    /// A parked input again. One parked on the join opens the VPP first, then becomes the
    /// session's surface.
    fn resend(&mut self, ctx: &mut Ctx, (surface, geom, src): In) -> Result<Push, Error> {
        if self.core.is_some() && self.key.is_some() {
            return self.push(ctx, (surface, geom, src));
        }
        let Some(f) = self.in_fmt else {
            Self::warn(ctx, K::FrameBeforeFormat);
            return Ok(Push::Taken);
        };
        match self.core(ctx, f, geom)? {
            Opened::Ready => {}
            Opened::Refused => {
                ctx.count_drop();
                return Ok(Push::Taken);
            }
            Opened::Wait => {
                self.retry = Some((surface, geom, src));
                return Ok(Push::Parked);
            }
        }
        let core = self.core.as_ref().ok_or_else(|| failed(ctx, NAME, K::NoSession))?;
        let ts = surface.timestamp();
        let input =
            self.input.for_session(core.owner(), &surface).map_err(|e| failed(ctx, NAME, e))?;
        input.set_timestamp(ts);
        let src = (!self.input.is_direct()).then(|| surface.clone());
        self.push(ctx, (input, geom, src))
    }

    /// One drain call, `false` when it waits.
    fn drain_step(&mut self, ctx: &mut Ctx) -> Result<bool, Error> {
        let now = ctx.steady();
        let Some(got) = self.process_one(None, &*ctx) else {
            self.drained = true;
            return Ok(true);
        };
        match got {
            Ok(Processed::Out(out)) => {
                let g = self.core.as_ref().map(VppCore::geometry);
                let (stamp, dts, flags) = self.stamp_out(&out, &*ctx);
                let geom = g.ok_or_else(|| failed(ctx, NAME, K::NoSession))?;
                let p = Pending { surface: out, geom, stamp, dts, flags, submitted: now };
                self.pending.push_back(p);
                self.busy.clear();
                Ok(true)
            }
            Ok(Processed::Kept) => {
                self.drained = true;
                Ok(true)
            }
            Ok(Processed::Busy) => {
                if self.busy.tick(ctx.steady()) {
                    let e = stalled(ffi::MFX_WRN_DEVICE_BUSY, "MFXVideoVPP_ProcessFrameAsync");
                    return Err(failed(ctx, NAME, K::Stalled(e)));
                }
                self.meter.park(Park::Busy);
                park(ctx, Park::Busy);
                Ok(false)
            }
            Ok(Processed::Wait) => {
                self.meter.park(Park::Pool);
                park(ctx, Park::Pool);
                Ok(false)
            }
            Err(e) => {
                Self::warn(ctx, e);
                self.drained = true;
                Ok(true)
            }
        }
    }

    fn finished(&self) -> bool {
        self.disabled
            || (self.drained
                && self.pending.is_empty()
                && self.retry.is_none()
                && self.raw_wait.is_none())
    }

    /// Feeds and drains until something waits, the end held while anything is owed.
    fn run(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<(), Error> {
        self.let_go();
        loop {
            if self.flush_pending(ctx)? && self.settle_due {
                self.let_go();
            }
            if let Some(input) = self.retry.take() {
                match self.resend(ctx, input)? {
                    Push::Taken => continue,
                    Push::Parked => break,
                    Push::Refused(e) => {
                        Self::warn(ctx, K::Process(e));
                        continue;
                    }
                }
            }
            if !ctx.room(OUT) || !self.in_flight_room() {
                break;
            }
            let Some(front) = self.raw_wait.as_ref().or_else(|| inputs.front()) else {
                // A harness may end with buffers still queued, they go first.
                if !self.ended || self.drained || ctx.input_len() > 0 {
                    break;
                }
                if !self.drain_step(ctx)? {
                    break;
                }
                continue;
            };
            // A pending join is retried by `core()` below, from `raw_wait` or the next input.
            if ctx.before_presentation(front.pts, front.duration)
                && !front.flags.contains(BufferFlags::PREVIEW)
            {
                if self.raw_wait.take().is_none() {
                    inputs.pop();
                }
                ctx.count_drop();
                continue;
            }
            let Some(inbuf) = self.raw_wait.take().or_else(|| inputs.pop()) else { break };
            let taken = if self.input.is_surfaces() {
                let Some(h) = GpuFrameHeader::from_bytes(inbuf.memory.data()) else {
                    Self::warn(ctx, K::FrameHeader);
                    continue;
                };
                let Some(frame) = self.input.take(h.token) else {
                    Self::warn(ctx, K::NoFrame { token: h.token });
                    continue;
                };
                self.submit(ctx, &frame.surface, frame.geom, &inbuf)?
            } else {
                let Some(f) = self.in_fmt else {
                    Self::warn(ctx, K::FrameBeforeFormat);
                    continue;
                };
                let layout: Layout = f.layout();
                let geom = Geometry {
                    coded_w: f.width.next_multiple_of(16),
                    coded_h: f.height.next_multiple_of(16),
                    disp_w: f.width,
                    disp_h: f.height,
                    crop_x: 0,
                    crop_y: 0,
                    fourcc: layout.fourcc(),
                };
                match self.core(ctx, f, geom)? {
                    Opened::Ready => {}
                    Opened::Refused => {
                        ctx.count_drop();
                        continue;
                    }
                    // A raw frame has no surface yet, it is uploaded once the VPP is open.
                    Opened::Wait => {
                        self.raw_wait = Some(inbuf);
                        break;
                    }
                }
                let core = self.core.as_ref().ok_or_else(|| failed(ctx, NAME, K::NoSession))?;
                let surface = core.input_surface().map_err(|e| failed(ctx, NAME, e))?;
                if let Err(e) = crate::input::upload(&surface, f, inbuf.memory.data()) {
                    Self::warn(ctx, e);
                    continue;
                }
                self.submit(ctx, &surface, geom, &inbuf)?
            };
            if !taken {
                break;
            }
        }
        if self.ended && !self.finished() {
            ctx.hold_end();
        }
        Ok(())
    }

    fn discard(&mut self, inputs: &mut Inputs<'_>) {
        while let Some(b) = inputs.pop() {
            if let Some(h) = GpuFrameHeader::from_bytes(b.memory.data()) {
                drop(self.input.take(h.token));
            }
        }
    }
}

fn sdr() -> Color {
    use profluens_video::color::{Matrix, Primaries, Range, TransferFn};
    Color {
        primaries: Some(Primaries::Bt709),
        transfer: Some(TransferFn::Bt709),
        matrix: Some(Matrix::Bt709),
        range: Some(Range::Limited),
        ..Color::default()
    }
}

impl Element for VplVpp {
    fn desc(&self) -> &'static ElementDesc {
        &DESCS[usize::from(self.input.is_surfaces())][usize::from(!self.out.is_raw())]
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(v)) = ctx.prop("max-width") {
            self.target.max_width = v.clamp(0, 16384) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("max-height") {
            self.target.max_height = v.clamp(0, 16384) as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("depth") {
            self.target.depth = if v >= 10 { 10 } else { 8 };
        }
        // Held inputs plus the outputs in flight, declared before the producer inits.
        self.input.declare_hold(u32::from(self.depth()) + 2);
        self.input.start(ctx, NAME, ctx.negotiated_sink(IN))?;
        self.out.start(ctx, NAME)
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.disabled {
            self.discard(&mut inputs);
            return Ok(Flow::Ok);
        }
        self.meter.report(ctx.steady(), &*ctx);
        if self.in_fmt.is_none()
            && let Some(f) = ctx.negotiated_sink(IN).cloned()
        {
            self.in_fmt = read_format(ctx, &f);
            self.color = Color::read(ctx, &f);
        }
        self.out.drain_released();
        self.run(ctx, &mut inputs)?;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(f) => {
                self.in_fmt = read_format(ctx, f);
                self.color = Color::read(ctx, f);
                self.input.format_change(ctx, f);
            }
            Event::FlushStart => {
                self.retired.extend(self.pending.drain(..).map(|p| p.surface));
                self.retry = None;
                self.raw_wait = None;
                self.stamps.clear();
                self.extra = [(None, Timestamp::NONE, BufferFlags::empty()); EXTRA];
                self.last_out = None;
                self.ended = false;
                self.drained = false;
                self.busy.clear();
                self.input.flush(ctx);
                self.out.flush();
            }
            // A parked input goes first, then the drain and `pending`, the end held until the
            // ring took it all.
            Event::Eos => {
                self.ended = true;
                if !self.disabled {
                    self.out.drain_released();
                    self.run(ctx, &mut Inputs::empty())?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.meter.report_final(&*ctx);
        // Producers close only once the runtime is done with their surfaces. A wait that runs
        // out leaves them to the VPP's own close.
        if self.quiesce()
            && let Some(c) = &self.core
        {
            c.owner().settle(None);
        }
        self.pending.clear();
        self.retired.clear();
        self.producer = None;
        self.settle_due = false;
        self.retry = None;
        self.raw_wait = None;
        self.stamps.clear();
        self.extra = [(None, Timestamp::NONE, BufferFlags::empty()); EXTRA];
        self.last_out = None;
        self.input.stop();
        self.out.stop();
        self.core = None;
        self.joining.clear();
        self.held.clear();
        self.key = None;
        self.refused = None;
        self.in_fmt = None;
        self.ended = false;
        self.drained = false;
        self.busy.clear();
    }
}

#[cfg(test)]
// Hardware test, skips without the runtime.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use pf_harness::Harness;
    use profluens_core::format::ValueDesc;

    use super::*;

    /// Frames out of `n` grey 320x240 inputs, the device busy for `before_last` calls ahead of
    /// the last input and `before_end` calls ahead of the end. `busy_joins` joins of the
    /// VPP's session into a fake group come back in execution first.
    fn scale(n: u64, before_last: u32, before_end: u32, busy_joins: u32) -> (usize, Vec<String>) {
        let busy = Arc::new(AtomicU32::new(0));
        let target = Target { max_width: 160, max_height: 120, depth: 8 };
        let mut vpp = VplVpp::new(target);
        vpp.inject_busy = Some(Arc::clone(&busy));
        if busy_joins > 0 {
            vpp = vpp.with_group(VplGroup::of(crate::session::Session::fake(900)));
            crate::session::fake::busy_joins(busy_joins);
        }
        let mut h = Harness::with_pool(vpp, 1 << 20, 8).real_wakes();
        h.fix_sink_format(
            IN,
            "video/raw",
            &[
                ("width", ValueDesc::Int(320)),
                ("height", ValueDesc::Int(240)),
                ("pixfmt", ValueDesc::Id("i420")),
            ],
        );
        let frame = vec![128u8; 320 * 240 * 3 / 2];
        let mut out = 0;
        for i in 0..n {
            if i + 1 == n {
                busy.store(before_last, Ordering::Release);
            }
            let mut b = h.alloc(&frame);
            b.pts = Timestamp::from_nanos(i * 40_000_000);
            h.push(b).expect("push");
            while h.pull(OUT).is_some() {
                out += 1;
            }
        }
        busy.fetch_add(before_end, Ordering::AcqRel);
        out += h.eos().expect("eos").len();
        let said = h
            .bus_messages()
            .iter()
            .filter_map(|m| match m {
                BusMessage::Warning { error, .. } | BusMessage::Error { error, .. } => {
                    Some(error.to_string())
                }
                _ => None,
            })
            .collect();
        (out, said)
    }

    #[test]
    fn the_end_waits_for_an_input_parked_on_a_busy_device() {
        let test = "the_end_waits_for_an_input_parked_on_a_busy_device";
        if probe::probe().is_none() {
            return probe::skip(test, "no VPL runtime");
        }
        for (last, end) in [(0, 0), (3, 0), (0, 3), (2, 2)] {
            let (out, said) = scale(6, last, end, 0);
            assert!(said.is_empty(), "{test}: busy {last} {end} said {said:?}");
            assert_eq!(out, 6, "{test}: busy {last} before the last, {end} before the end");
        }
    }

    #[test]
    fn a_raw_frame_parked_on_a_busy_join_goes_through_once_the_join_takes() {
        let test = "a_raw_frame_parked_on_a_busy_join_goes_through_once_the_join_takes";
        if probe::probe().is_none() {
            return probe::skip(test, "no VPL runtime");
        }
        for joins in [1, 2, 4] {
            let (out, said) = scale(3, 0, 0, joins);
            assert!(said.is_empty(), "{test}: {joins} busy joins said {said:?}");
            assert_eq!(out, 3, "{test}: {joins} busy joins");
        }
    }
}
