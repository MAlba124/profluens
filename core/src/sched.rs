use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use crate::batch::{Batch, Inputs};
use crate::bus::{BusMessage, BusSender};
use crate::clock::{Clock, Nap};
use crate::command::{Command, Control, Cursor, Finish, PLAYING};
use crate::counters::ElementCounters;
use crate::ctx::{AnnouncePayload, Ctx, PadFormats, PadOffers, SeekTarget, StopReason};
use crate::element::{Element, Flow};
use crate::error::{Error, FormatError, GraphError, RejectedFormat, Scope};
use crate::event::Event;
use crate::feedback::{Slot, UNSEEN, Upstream};
use crate::format::{FixedFormat, Vocabulary};
use crate::id::{ElementId, GroupId, LinkId, SinkPad, SrcPad};
use crate::io::{Completion, Reactor, ReactorFactory, Waker};
use crate::level::Gauge;
use crate::log::{Level, Log};
use crate::memory::Pool;
use crate::pipeline::{BYTES, RunEnd, RunReport, SeekMode};
use crate::props::PropTable;
use crate::pull::{Mode, START};
use crate::ring::{Consumer, Leaky, Producer};
use crate::station::{Room, Station, Wake};
use crate::time::{Rate, Timestamp};
use crate::topology::{Join, Joining, Leave, PadEdit, Part};
use crate::weave::{Full, Weave};

const INLINE_INPUT_CAP: usize = 8;

pub(crate) const PARK_BACKSTOP: Duration = Duration::from_millis(10);
/// The group park bound when nothing is scheduled, a safety net. Every wait has its signal or
/// its own timer, `late_wakes` in the scheduler stats counts the ones that had not.
pub(crate) const GROUP_PARK_BACKSTOP: Duration = Duration::from_secs(1);

pub(crate) fn deliver_event(
    elem: &mut dyn Element,
    ctx: &mut Ctx,
    sink_pad: Option<SinkPad>,
    ev: Event,
    vocabulary: &Vocabulary,
) -> Result<(), Error> {
    if let (Event::FormatChange(f), Some(pad)) = (&ev, sink_pad) {
        let own = ctx.sink_offers(pad).map(|own| (own, own.offers.admits(f)));
        // No offer of the family is another codec. Said once, then dropped up to a format it takes.
        let bytes = vocabulary.formats.get("bytes");
        let named = |own: &PadOffers| {
            !own.offers.offers.is_empty()
                && own.offers.offers.iter().all(|o| !o.2 && Some(o.0.0) != bytes)
        };
        let foreign = matches!(own, Some((own, None)) if named(own));
        if let Some((own, _)) = own.filter(|(_, admits)| *admits == Some(false) || foreign) {
            let names = [&vocabulary.formats, &vocabulary.fields, &vocabulary.values];
            let element = ctx.element();
            let rejected = RejectedFormat {
                element,
                element_name: elem.desc().name,
                pad,
                pad_name: own.name.clone(),
                family: f.family,
                announced: f.render(names).into(),
                offers: own.lines.clone(),
            };
            if foreign {
                let err = Error::Format(FormatError::FamilyRefused(rejected.into()));
                if !ctx.set_refused(pad, true) {
                    ctx.post(BusMessage::Error { element, error: err });
                }
                crate::log!(
                    &*ctx,
                    Level::Debug,
                    "format_refused",
                    pad = pad.0 as u64,
                    family = f.family.0 as u64
                );
                return Ok(());
            }
            let err = Error::Format(FormatError::AnnouncementRejected(rejected.into()));
            ctx.post(BusMessage::Error { element, error: err.clone() });
            return Err(err);
        }
        ctx.set_refused(pad, false);
        ctx.set_negotiated_sink(pad, f.clone());
        crate::log!(
            &*ctx,
            Level::Debug,
            "format_change",
            pad = pad.0 as u64,
            family = f.family.0 as u64,
        );
    }
    ctx.set_event_pad(sink_pad);
    let told = elem.event(ctx, &ev);
    ctx.set_event_pad(None);
    told
}

pub fn process_once(
    elem: &mut dyn Element,
    ctx: &mut Ctx,
    with_input: bool,
) -> Result<Flow, Error> {
    process_within(elem, ctx, with_input, Timestamp::NONE)
}

fn process_within(
    elem: &mut dyn Element,
    ctx: &mut Ctx,
    with_input: bool,
    room: Timestamp,
) -> Result<Flow, Error> {
    if !with_input {
        return elem.process(ctx, Inputs::empty());
    }
    let mut input = ctx.take_input();
    let rows = if room.is_some() { input.within(room) } else { usize::MAX };
    let flow = elem.process(ctx, Inputs::limited(&mut input, rows));
    ctx.set_input(input);
    flow
}

/// Reclaim scratch after a `process()` call and resolve what the element announced.
pub fn after_process(
    ctx: &mut Ctx,
    vocabulary: &Vocabulary,
    sink: impl FnMut(&mut Ctx, SrcPad, u32, FixedFormat),
) {
    ctx.reset_scratch();
    resolve_announcements(ctx, vocabulary, sink);
}

/// Hand each announced format that differs from the link's to `sink`, in announce order.
pub fn resolve_announcements(
    ctx: &mut Ctx,
    vocabulary: &Vocabulary,
    mut sink: impl FnMut(&mut Ctx, SrcPad, u32, FixedFormat),
) {
    for ann in ctx.take_announcements() {
        let fixed = match ann.payload {
            AnnouncePayload::Named { family, fields } => {
                match vocabulary.build_fixed(family, &fields) {
                    Some(f) => f,
                    None => {
                        warn_unresolved(ctx, family, &fields);
                        continue;
                    }
                }
            }
            AnnouncePayload::Fixed(f) => f,
        };
        if ctx.negotiated_src(ann.pad) == Some(&fixed) {
            continue;
        }
        ctx.set_negotiated_src(ann.pad, fixed.clone());
        sink(ctx, ann.pad, ann.at, fixed);
    }
}

fn warn_unresolved(
    ctx: &mut Ctx,
    family: &'static str,
    fields: &[(&'static str, crate::format::ValueDesc)],
) {
    let element = ctx.element();
    ctx.post(BusMessage::Warning {
        element,
        error: Error::Format(FormatError::AnnouncementUnresolved {
            element,
            family,
            fields: fields.iter().map(|(n, _)| *n).collect(),
        }),
    });
}

#[derive(Clone)]
pub(crate) struct GroupShared {
    pub(crate) vocabulary: Arc<Vocabulary>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) base: Arc<AtomicI64>,
    pub(crate) tracing: Arc<AtomicBool>,
    pub(crate) wall: Option<Instant>,
    pub(crate) control: Arc<Control>,
}

impl GroupShared {
    fn trace_ns(&self) -> u64 {
        match self.wall {
            Some(epoch) => epoch.elapsed().as_nanos() as u64,
            None => self.clock.now().0,
        }
    }
}

pub(crate) struct UpLink {
    pub(crate) pad: SinkPad,
    pub(crate) data: Consumer<Batch>,
    pub(crate) shells: Producer<Batch>,
    pub(crate) ended: bool,
    pub(crate) peer: Arc<Station>,
    pub(crate) room: Arc<Room>,
    pub(crate) level: Arc<Gauge>,
    pub(crate) marked: bool,
    pub(crate) popped: u64,
    pub(crate) end: Timestamp,
    pub(crate) told: (u64, Timestamp),
    pub(crate) seen: (u64, usize, Timestamp),
    pub(crate) next: Option<Box<(UpLink, Option<FixedFormat>)>>,
    pub(crate) slot: Arc<Slot>,
}

impl UpLink {
    pub(crate) fn new(
        pad: SinkPad,
        data: Consumer<Batch>,
        shells: Producer<Batch>,
        peer: Arc<Station>,
        room: Arc<Room>,
        level: Arc<Gauge>,
        slot: Arc<Slot>,
    ) -> UpLink {
        let (ended, popped, end, told) = (false, 0, Timestamp::NONE, (0, Timestamp::NONE));
        let seen = (0, 0, Timestamp::NONE);
        let marked = level.marked();
        UpLink {
            pad,
            data,
            shells,
            ended,
            peer,
            room,
            level,
            marked,
            popped,
            end,
            told,
            seen,
            next: None,
            slot,
        }
    }
}

pub(crate) struct DownLink {
    pub(crate) pad: SrcPad,
    pub(crate) link: LinkId,
    pub(crate) data: Producer<Batch>,
    pub(crate) shells: Consumer<Batch>,
    pub(crate) leaky: Leaky,
    pub(crate) peer: Arc<Station>,
    pub(crate) room: Arc<Room>,
    pub(crate) staged: Option<Batch>,
    pub(crate) level: Arc<Gauge>,
    pub(crate) limited: bool,
    /// The time limit set on the link, `NONE` for none. The tail grows it to the interleave.
    pub(crate) base: Timestamp,
    /// It carries a sparse stream, which never measures, starves or rests by time.
    pub(crate) sparse: bool,
    /// The consumer and its pad, named when the link's marks are crossed. `None` with no marks.
    pub(crate) mark: Option<(ElementId, SinkPad)>,
    pub(crate) slot: Arc<Slot>,
    pub(crate) seen: u64,
}

// Field order is drop order. Rings drop before logs, elements last.
pub(crate) struct GroupSetup {
    pub(crate) linked_pads: Vec<Vec<bool>>,
    pub(crate) path_latencies: Vec<Timestamp>,
    pub(crate) pad_counts: Vec<(usize, usize)>,
    pub(crate) shared: GroupShared,
    pub(crate) cursor: Arc<Cursor>,
    pub(crate) seek_gen: u64,
    pub(crate) station: Arc<Station>,
    pub(crate) backstop: Duration,
    pub(crate) props: Vec<Arc<PropTable>>,
    pub(crate) counters: Vec<Arc<ElementCounters>>,
    pub(crate) credits: Vec<u32>,
    pub(crate) bus: BusSender,
    pub(crate) pools: Vec<Pool>,
    pub(crate) factory: ReactorFactory,
    pub(crate) downstream: Vec<DownLink>,
    pub(crate) upstream: Vec<UpLink>,
    pub(crate) logs: Vec<Option<Log>>,
    pub(crate) formats: Vec<PadFormats>,
    pub(crate) ids: Vec<ElementId>,
    pub(crate) elements: Vec<Box<dyn Element>>,
    pub(crate) stream_pads: Vec<Vec<(u32, SrcPad)>>,
    pub(crate) join: Option<Join>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Running,
    Draining { next: usize },
    Ended,
}

/// A restart handed to a member, which the members run on full rings for until the tail is
/// back where a ring filled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Restart {
    Idle,
    /// Handed at a splice, the head has yet to read behind it. What the tail puts out until
    /// then it held from before.
    Handed,
    /// The head read behind it, the next media the tail feeds a full ring is where it was.
    Reading,
}

/// What one pass of a group did, for the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Work was done.
    Progressed,
    /// Nothing to do until a signal, running time `until` or the interval `within`.
    Idle { until: Timestamp, within: Duration },
    /// Work exists but waits on what `on` names.
    Blocked { on: BlockedOn },
    /// The run is over and the group is torn down.
    Exit,
}

/// Where the stepped driver may stop a pass and let other groups run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cut {
    /// Past the stop check and the command drain, before any ring is touched.
    Drained,
    End,
}

/// What a blocked group waits for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockedOn {
    /// The ring fed from the tail's `pad` is full and holds a staged batch.
    Ring { pad: SrcPad },
    /// `element`'s pool is at its cap.
    Pool { element: ElementId },
    /// Reactor ops are in flight and only an outside event completes one.
    Io,
}

enum Sent {
    Pushed,
    Staged,
    Dropped { rows: u64, shell: Option<Batch> },
}

fn fed(d: &DownLink, (bytes, from, to): (u64, Timestamp, Timestamp), eos: bool, bus: &BusSender) {
    d.level.feed(bytes, from, to);
    d.peer.signal();
    if eos {
        d.level.end();
    }
    if let Some((element, pad)) = d.mark {
        d.level.judge(true, |fill, held| {
            bus.send(BusMessage::Buffering { element, pad: Some(pad), fill, held })
        });
    }
}

/// Drop a link taken off a tail and wake its consumer, which may rest on that ring.
fn close(link: DownLink) {
    let peer = Arc::clone(&link.peer);
    drop(link);
    peer.signal();
}

fn send(d: &mut DownLink, out: Batch, bus: &BusSender) -> Sent {
    debug_assert!(d.staged.is_none(), "one staged batch per link");
    let (mut bytes, mut from, mut to) = out.span();
    let eos = out.eos;
    let mut out = match d.data.try_push(out) {
        Ok(()) => {
            fed(d, (bytes, from, to), eos, bus);
            return Sent::Pushed;
        }
        Err(out) => out,
    };
    let mut dropped = None;
    if d.leaky == Leaky::DropNewest && !out.eos && !out.is_empty() {
        let rows = out.len() as u64;
        out.retain_state();
        if out.is_inert() {
            return Sent::Dropped { rows, shell: Some(out) };
        }
        dropped = Some(rows);
        (bytes, from, to) = (0, Timestamp::NONE, Timestamp::NONE);
    }
    // Raise the flag, then look once more. A pop after that look signals us.
    d.room.raise();
    match d.data.try_push(out) {
        Ok(()) => fed(d, (bytes, from, to), eos, bus),
        Err(out) => d.staged = Some(out),
    }
    match dropped {
        Some(rows) => Sent::Dropped { rows, shell: None },
        None if d.staged.is_none() => Sent::Pushed,
        None => Sent::Staged,
    }
}

// Field order is drop order. Contexts and reactor first, then rings, elements last.
pub(crate) struct Group {
    ctxs: Vec<Ctx>,
    reactor: Option<Box<dyn Reactor>>,
    factory: ReactorFactory,
    completions: Vec<(ElementId, Completion)>,
    shared: GroupShared,
    counters: Vec<Arc<ElementCounters>>,
    downstream: Vec<DownLink>,
    closing: Vec<DownLink>,
    upstream: Vec<UpLink>,
    ids: Vec<ElementId>,
    elements: Vec<Box<dyn Element>>,
    member_sink_pads: Vec<Option<SinkPad>>,
    head_sink_pad: Option<SinkPad>,
    is_source: bool,
    fan_in: bool,
    phase: Phase,
    head_done: bool,
    end_held: bool,
    seek_gen: u64,
    /// Splices applied, so a feed loop sees its links change under a batch.
    splices: u32,
    /// Joined and yet to push, the driver waits on the first batch to splice.
    prerolling: bool,
    applied: u64,
    cursor: Arc<Cursor>,
    station: Arc<Station>,
    backstop: Duration,
    leaving: Option<Leave>,
    undone: bool,
    cut_head: bool,
    cut_pads: Vec<SinkPad>,
    fan_out: bool,
    io_woke: bool,
    restart: Restart,
    weave: Weave,
}

/// Queue `link` behind the last of those behind `up`.
// Once per hand-over at a splice.
#[allow(clippy::disallowed_methods)]
fn queue_behind(up: &mut UpLink, link: UpLink, format: Option<FixedFormat>) {
    let mut last = &mut up.next;
    while let Some(next) = last {
        last = &mut next.0.next;
    }
    *last = Some(Box::new((link, format)));
}

fn fans_out(links: &[DownLink]) -> bool {
    (1..links.len()).any(|j| links[..j].iter().any(|o| o.pad == links[j].pad))
}

pub(crate) struct Tag {
    pub(crate) cursor: Arc<Cursor>,
    pub(crate) station: Arc<Station>,
    pub(crate) ids: Vec<ElementId>,
    joining: Option<Arc<Joining>>,
    bus: BusSender,
}

impl Tag {
    pub(crate) fn of(setup: &GroupSetup) -> Tag {
        let joining = setup.join.as_ref().map(|j| Arc::clone(&j.joining));
        let (cursor, station) = (Arc::clone(&setup.cursor), Arc::clone(&setup.station));
        Tag { cursor, station, ids: setup.ids.clone(), joining, bus: setup.bus.clone() }
    }
}

pub(crate) fn judge(tag: &Tag, failed: Option<&Error>) -> (bool, bool) {
    let Some(e) = failed else {
        return (false, false);
    };
    let taken = tag.joining.as_ref().is_some_and(|j| j.fail(e));
    (taken, !taken && e.scope() == Scope::Branch)
}

pub(crate) fn run_group(setup: GroupSetup, tag: &Tag) -> (Result<(), Error>, (bool, bool)) {
    let mut group = match Group::open(setup) {
        Ok(group) => group,
        Err(e) => {
            let verdict = judge(tag, Some(&e));
            return (Err(e), verdict);
        }
    };
    // A panic in a pass still closes the group, so elements stop and free what they hold.
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drive(&mut group)))
    {
        Ok(result) => result,
        Err(payload) => Err(group_panicked(tag, &payload)),
    };
    let verdict = judge(tag, result.as_ref().err());
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| group.end(verdict)));
    (result, verdict)
}

/// The error for a panic of `tag`'s thread, with the payload's text.
pub(crate) fn group_panicked(tag: &Tag, payload: &Box<dyn std::any::Any + Send>) -> Error {
    let message = match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
        (Some(s), _) => String::from(*s),
        (None, Some(s)) => s.clone(),
        (None, None) => String::from("non-string panic payload"),
    };
    Error::GroupPanicked { element: tag.ids[0], message }
}

/// Take a closed group out of the run, failing it, rolling back its change or sealing a branch.
pub(crate) fn retire_group(
    control: &Control,
    tag: &Tag,
    failed: Option<Error>,
    (taken, sealed): (bool, bool),
) -> Option<Error> {
    match &failed {
        Some(error) if sealed => tag
            .bus
            .send(BusMessage::BranchSealed { group: GroupId(tag.ids[0].0), error: error.clone() }),
        Some(_) if !taken => control.abort(),
        _ => {}
    }
    control.retire(&tag.cursor, &tag.station, tag.ids[0]);
    control.topology.driver.signal();
    failed.filter(|_| !taken && !sealed)
}

fn build_ctxs(setup: &mut GroupSetup) -> Vec<Ctx> {
    let m = setup.elements.len();
    let shared = &setup.shared;
    let mut ctxs: Vec<Ctx> = setup
        .ids
        .iter()
        .zip(setup.pools.iter().zip(&setup.credits))
        .map(|(id, (pool, &credits))| {
            Ctx::new(pool.clone(), setup.bus.clone(), *id, BYTES, credits)
        })
        .collect();
    for (ctx, &(nsrcs, nsinks)) in ctxs.iter_mut().zip(setup.pad_counts.iter()) {
        ctx.configure_pads(nsrcs, nsinks);
    }
    let tail_shells: usize =
        setup.downstream.iter().map(|d| d.data.capacity() + d.shells.capacity() + 1).sum();
    if tail_shells > 0 {
        ctxs[m - 1].bump_spare_cap(tail_shells);
    }
    for (ctx, formats) in ctxs.iter_mut().zip(std::mem::take(&mut setup.formats)) {
        ctx.set_negotiated(formats);
    }
    for (ctx, log) in ctxs.iter_mut().zip(std::mem::take(&mut setup.logs)) {
        if let Some(log) = log {
            ctx.set_log(log);
        }
    }
    for ctx in ctxs.iter_mut() {
        ctx.set_vocabulary(Arc::clone(&shared.vocabulary));
    }
    let latencies = std::mem::take(&mut setup.path_latencies);
    let linked = std::mem::take(&mut setup.linked_pads);
    for (ctx, pads) in ctxs.iter_mut().zip(std::mem::take(&mut setup.stream_pads)) {
        pads.into_iter().for_each(|(stream, pad)| ctx.set_stream_pad(stream, pad));
    }
    for ((ctx, lat), linked) in ctxs.iter_mut().zip(latencies).zip(linked) {
        ctx.set_clock(Arc::clone(&shared.clock), Arc::clone(&shared.base));
        ctx.set_steady(shared.wall);
        ctx.set_path_latency(lat);
        ctx.set_control(Arc::clone(&shared.control), Arc::clone(&setup.cursor), setup.seek_gen);
        ctx.set_linked_pads(linked);
    }
    for (ctx, c) in ctxs.iter_mut().zip(&setup.counters) {
        ctx.set_trace(Arc::clone(c), Arc::clone(&shared.tracing));
        ctx.set_station(Arc::clone(&setup.station));
        if let Some(tail) = setup.counters.last() {
            ctx.set_tail(Arc::clone(tail));
        }
    }
    for ((ctx, elem), props) in ctxs.iter_mut().zip(&setup.elements).zip(&setup.props) {
        let descs = elem.desc().props;
        if !descs.is_empty() {
            ctx.set_props(descs, Arc::clone(props));
        }
    }
    ctxs
}

impl Group {
    pub(crate) fn open(mut setup: GroupSetup) -> Result<Group, Error> {
        let join = setup.join.take();
        let mut group = Group::build(setup)?;
        (group.undone, group.prerolling) = (join.is_some(), join.is_some());
        if let Err(e) = group.start() {
            if e.scope() == Scope::Branch && join.is_none() {
                group.seal();
            }
            return Err(e);
        }
        let Some(join) = join else { return Ok(group) };
        if let Err(e) = group.land(&join) {
            group.stop();
            return Err(e);
        }
        group.undone = false;
        Ok(group)
    }

    fn land(&mut self, join: &Join) -> Result<(), Error> {
        if join.paused {
            self.set_paused(true)?;
        }
        if join.rate != Rate::ONE {
            self.broadcast(&Event::RateAsked { rate: join.rate })?;
        }
        self.seek(self.seek_gen, join.to_time)?;
        join.joining.started();
        self.shared.control.topology.driver.signal();
        Ok(())
    }

    /// Close the group, sealed when it failed with `Branch` scope.
    fn end(&mut self, (taken, sealed): (bool, bool)) {
        self.undone |= taken;
        if sealed {
            self.seal();
        }
        self.close();
    }

    pub(crate) fn close(&mut self) {
        crate::log!(&self.ctxs[0], Level::Trace, "group_done", is_source = self.is_source);
        if self.leaving.is_some() {
            for (ctx, id) in self.ctxs.iter_mut().zip(&self.ids) {
                ctx.post(BusMessage::ElementRemoved { element: *id });
            }
        }
        self.settle_io();
        self.stop();
        self.settle_io();
    }

    fn build(mut setup: GroupSetup) -> Result<Group, Error> {
        let is_source = setup.upstream.is_empty();
        let fan_in = setup.upstream.len() > 1;
        let head_sink_pad = if is_source || fan_in { None } else { Some(setup.upstream[0].pad) };
        let ctxs = build_ctxs(&mut setup);
        let member_sink_pads: Vec<Option<SinkPad>> =
            setup.elements.iter().map(|e| e.desc().sinks.first().map(|_| SinkPad(0))).collect();
        // One Vec of each per group at setup, reused across passes.
        #[allow(clippy::disallowed_methods)]
        let (completions, cut_pads, closing) = (Vec::new(), Vec::new(), Vec::new());
        let GroupSetup {
            shared,
            cursor,
            seek_gen,
            station,
            backstop,
            counters,
            downstream,
            upstream,
            ids,
            elements,
            factory,
            ..
        } = setup;
        let fan_out = fans_out(&downstream);
        downstream.iter().for_each(|d| d.level.restart(seek_gen));
        let mut weave = Weave::default();
        weave.recount(&downstream);
        for up in &upstream {
            let _ = up.level.restarted(seek_gen);
        }
        debug_assert!(!elements.is_empty(), "a group has at least its head");
        Ok(Group {
            ctxs,
            reactor: None,
            factory,
            completions,
            shared,
            counters,
            downstream,
            closing,
            upstream,
            ids,
            elements,
            member_sink_pads,
            head_sink_pad,
            is_source,
            fan_in,
            phase: Phase::Running,
            head_done: false,
            end_held: false,
            seek_gen,
            splices: 0,
            prerolling: false,
            applied: cursor.get(),
            cursor,
            station,
            backstop,
            leaving: None,
            undone: false,
            cut_head: false,
            cut_pads,
            fan_out,
            io_woke: false,
            restart: Restart::Idle,
            weave,
        })
    }

    fn start(&mut self) -> Result<(), Error> {
        for i in 0..self.elements.len() {
            if let Err(e) = self.elements[i].start(&mut self.ctxs[i]) {
                let reason = self.stop_reason();
                for j in (0..i).rev() {
                    self.ctxs[j].set_stop_reason(reason);
                    self.elements[j].stop(&mut self.ctxs[j]);
                }
                return Err(e);
            }
            // Started with its links as they are, see `Pipeline::relinked`.
            self.ctxs[i].link_seen();
        }
        self.pair_pulls();
        Ok(())
    }

    fn pair_pulls(&mut self) {
        let m = self.elements.len();
        for i in 0..m {
            let offered = i + 1 < m && self.ctxs[i].serving.mode == Mode::Offered;
            let on = offered && self.ctxs[i + 1].pulling.mode == Mode::Wanted;
            let depth = self.ctxs.get(i + 1).map_or(0, |c| c.io_credits() as usize);
            self.ctxs[i].serving.pair(on, depth);
            if i > 0 && self.ctxs[i].pulling.mode != Mode::On {
                self.ctxs[i].pulling.pair(false, 0);
            }
            if on {
                self.ctxs[i + 1].pulling.pair(true, depth);
                self.ctxs[i + 1].pulling.reset = Some(self.ctxs[i].seek_target().unwrap_or(START));
            }
        }
        if m > 0 && self.ctxs[0].pulling.mode != Mode::On {
            self.ctxs[0].pulling.pair(false, 0);
        }
    }

    fn reset_pull(&mut self, i: usize) {
        if i + 1 >= self.elements.len() || !self.ctxs[i + 1].pulling.is_on() {
            return;
        }
        let from = self.ctxs[i].seek_target().unwrap_or(START);
        self.ctxs[i].serving.clear();
        self.ctxs[i + 1].pulling.clear();
        self.ctxs[i + 1].pulling.reset = Some(from);
    }

    fn sync_pull(&mut self, i: usize, progressed: &mut bool) {
        if i > 0 && self.ctxs[i].pulling.is_on() {
            let (left, right) = self.ctxs.split_at_mut(i);
            let (from, to) = (&mut right[0].pulling, &mut left[i - 1].serving);
            *progressed |= !from.asks.is_empty() || from.over != to.over;
            to.asks.append(&mut from.asks);
            to.over = from.over;
        }
        if i + 1 < self.elements.len() && self.ctxs[i].serving.mode == Mode::Withdrawn {
            self.reset_pull(i);
            self.ctxs[i].serving.mode = Mode::Off;
            self.ctxs[i + 1].pulling.mode = Mode::Off;
            *progressed = true;
        }
    }

    pub(crate) fn pass(&mut self) -> Result<Outcome, Error> {
        match self.pass_top()? {
            Some(over) => Ok(over),
            None => self.pass_rest(),
        }
    }

    fn pass_top(&mut self) -> Result<Option<Outcome>, Error> {
        if self.shared.control.is_stopped() {
            return Ok(Some(Outcome::Exit));
        }
        self.apply_commands()?;
        Ok((self.leaving == Some(Leave::Cut)).then_some(Outcome::Exit))
    }

    fn end_dead_inputs(&mut self) {
        let dead = |up: &&mut UpLink| !up.ended && up.data.is_closed() && up.data.is_empty();
        self.upstream.iter_mut().filter(dead).for_each(|up| up.ended = true);
        if !self.is_source && self.upstream.iter().all(|up| up.ended) {
            self.head_done = true;
        }
    }

    fn pass_rest(&mut self) -> Result<Outcome, Error> {
        if self.phase == Phase::Ended {
            return self.rest_ended();
        }
        let mut progressed = false;
        self.read_feedback()?;
        self.publish_up();
        self.push_staged(&mut progressed);
        if self.staged_pad().is_none() || !self.fan_in {
            self.feed(&mut progressed)?;
        }
        if self.leaving.is_some() {
            self.end_dead_inputs();
        }
        // A restart is work for the members, a full ring waits until the tail feeds it again.
        let rest = match self.restart {
            Restart::Idle => self.staged_pad().or_else(|| self.resting()),
            Restart::Handed | Restart::Reading => None,
        };
        if let Some(pad) = rest {
            self.tell_taken();
            let blocked = Outcome::Blocked { on: BlockedOn::Ring { pad } };
            let kept = self.run_keepers(&mut progressed)?;
            if kept.is_none() {
                self.drive_io(&mut progressed);
            }
            return Ok(match kept {
                _ if progressed => Outcome::Progressed,
                Some(Outcome::Idle { until, within })
                    if until.is_some() || within != Duration::MAX =>
                {
                    Outcome::Idle { until, within }
                }
                _ => blocked,
            });
        }
        self.run_members(&mut progressed)?;
        self.tell_taken();
        self.route(&mut progressed);
        if self.prerolling && self.downstream.iter().any(|d| !d.data.is_empty()) {
            self.prerolling = false;
            self.shared.control.topology.driver.signal();
        }
        self.weave.measure(&self.downstream);
        self.drive_io(&mut progressed);
        let rest = self.settle(progressed);
        Ok(if self.terminate()? { Outcome::Progressed } else { rest })
    }

    fn broadcast(&mut self, ev: &Event) -> Result<(), Error> {
        for i in 0..self.elements.len() {
            self.elements[i].event(&mut self.ctxs[i], ev)?;
        }
        Ok(())
    }

    fn apply_commands(&mut self) -> Result<(), Error> {
        let head = self.shared.control.log.head();
        if head == self.applied {
            return Ok(());
        }
        let log = &self.shared.control.log;
        let is_seek = |seq: &u64| matches!(log.read(*seq), Command::Seek { .. });
        let is_skip = |seq: &u64| matches!(log.read(*seq), Command::SkipTo { .. });
        let last_seek = (self.applied..head).rev().find(is_seek);
        let last_jump = (self.applied..head).rev().find(|seq| is_seek(seq) || is_skip(seq));
        while self.applied < head {
            let here = Some(self.applied);
            match self.shared.control.log.read(self.applied) {
                Command::Stop => {}
                Command::Pause => self.set_paused(true)?,
                Command::Resume => self.set_paused(false)?,
                Command::Seek { seek_gen, to_time } if last_seek == here => {
                    self.seek(seek_gen, to_time)?
                }
                Command::SkipTo { to_time } if last_jump == here => self.skip_to(to_time)?,
                Command::SetRate { rate } => self.broadcast(&Event::RateAsked { rate })?,
                Command::SetOffset { element, by } => self.set_offset(element, by)?,
                Command::Splice { step } => self.splice(step)?,
                Command::Seek { .. } | Command::SkipTo { .. } => {}
            }
            self.applied += 1;
        }
        self.shared.control.log.advance(&self.cursor, self.applied);
        // A splice held back by a full log posts once the driver sees the room.
        if self.shared.control.topology.changing() {
            self.shared.control.topology.driver.signal();
        }
        self.shared.control.retry_held();
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) -> Result<(), Error> {
        for c in self.ctxs.iter_mut() {
            c.set_paused(paused);
        }
        self.broadcast(if paused { &Event::Paused } else { &Event::Resumed })
    }

    fn set_offset(&mut self, element: ElementId, by: i64) -> Result<(), Error> {
        let Some(i) = self.ids.iter().position(|id| *id == element) else {
            return Ok(());
        };
        self.ctxs[i].set_offset(by);
        self.elements[i].event(&mut self.ctxs[i], &Event::Offset { by })
    }

    fn seek(&mut self, seek_gen: u64, to_time: Timestamp) -> Result<(), Error> {
        let mode = self.shared.control.mode_of(seek_gen);
        for (c, id) in self.ctxs.iter_mut().zip(&self.ids) {
            c.set_seek_target(seek_gen, to_time, self.shared.control.target(*id, seek_gen));
            if mode == Some(SeekMode::Preview) {
                c.arm_preview();
            }
            c.set_still(mode == Some(SeekMode::Key));
        }
        self.seek_gen = seek_gen;
        self.flush()
    }

    fn splice(&mut self, step: u32) -> Result<(), Error> {
        let Some(part) = self.shared.control.topology.take_part(step, self.ids[0]) else {
            return Ok(());
        };
        self.splices += 1;
        let Part {
            mut down_add,
            up_add,
            down_cut,
            up_cut,
            handover,
            mut edits,
            leave,
            undo,
            seek_gen,
            ..
        } = part;
        self.splice_cuts(down_cut, up_cut, &up_add, leave)?;
        #[allow(clippy::disallowed_methods)]
        let mut formats = Vec::new();
        edits.retain(|(_, edit)| match edit {
            PadEdit::SinkFormat(pad, format) if handover.contains(pad) => {
                formats.push((*pad, format.clone()));
                false
            }
            _ => true,
        });
        let restart = self.splice_edits(edits, &mut down_add, seek_gen)?;
        let joined = !up_add.is_empty();
        self.splice_adds(down_add, up_add, restart, &formats)?;
        if leave.is_some() {
            (self.leaving, self.undone) = (leave, undo);
            self.cut_head = self.is_source || self.upstream.is_empty();
        }
        self.splice_verdict(joined)
    }

    fn splice_cuts(
        &mut self,
        down: Vec<LinkId>,
        up: Vec<SinkPad>,
        up_add: &[UpLink],
        leave: Option<Leave>,
    ) -> Result<(), Error> {
        for link in down {
            if let Some(at) = self.downstream.iter().position(|d| d.link == link) {
                self.cut_down(at)?;
            }
        }
        for pad in up.into_iter().filter(|_| leave != Some(Leave::Drain)) {
            if let Some(at) = self.upstream.iter().position(|u| u.pad == pad) {
                let up = self.upstream.remove(at);
                while up.data.try_pop().is_some() {}
                if up.room.take() {
                    up.peer.signal();
                }
            }
            self.ctxs[0].discard_input(self.fan_in.then_some(pad));
            if up_add.iter().all(|u| u.pad != pad) {
                self.cut_pads.push(pad);
                self.ctxs[0].set_pad_closed(pad);
                self.elements[0].event(&mut self.ctxs[0], &Event::SinkLinked { pad, on: false })?;
            }
        }
        Ok(())
    }

    fn cut_down(&mut self, at: usize) -> Result<(), Error> {
        let tail = self.elements.len() - 1;
        let cut = self.downstream.remove(at);
        let pad = cut.pad;
        self.weave.recount(&self.downstream);
        if cut.staged.is_some() && !cut.data.is_closed() {
            self.closing.push(cut);
        } else {
            close(cut);
        }
        if self.downstream.iter().any(|d| d.pad == pad) {
            return Ok(());
        }
        self.ctxs[tail].told(pad, Upstream::default());
        self.ctxs[tail].set_pad_linked(pad, false);
        let off = Event::SrcLinked { pad, on: false, restart: None };
        self.elements[tail].event(&mut self.ctxs[tail], &off)?;
        self.ctxs[tail].link_seen();
        Ok(())
    }

    /// The tail reads what its links were told. A pad shared by several links reads none.
    fn read_feedback(&mut self) -> Result<(), Error> {
        let (tail, mut j, mut cut) = (self.elements.len() - 1, 0, false);
        while j < self.downstream.len() {
            let d = &mut self.downstream[j];
            if d.slot.version() == d.seen {
                j += 1;
                continue;
            }
            let (seen, up) = d.slot.read(self.seek_gen);
            d.seen = seen;
            if d.slot.sealed() {
                self.cut_down(j)?;
                cut = true;
                continue;
            }
            let pad = d.pad;
            if !self.fan_out || self.downstream.iter().filter(|o| o.pad == pad).count() == 1 {
                self.tell(tail, pad, up)?;
            }
            j += 1;
        }
        if cut {
            self.fan_out = fans_out(&self.downstream);
            self.splice_verdict(false)?;
        }
        Ok(())
    }

    fn tell(&mut self, i: usize, pad: SrcPad, up: Upstream) -> Result<(), Error> {
        let old = self.ctxs[i].told(pad, up);
        for feedback in up.news(&old) {
            self.elements[i].event(&mut self.ctxs[i], &Event::Feedback { pad, feedback })?;
        }
        Ok(())
    }

    fn publish_up(&mut self) {
        for up in &self.upstream {
            if let Some(told) = self.ctxs[0].take_published(up.pad) {
                up.slot.write(&told, self.seek_gen);
                up.peer.signal();
            }
        }
    }

    /// A branch-scoped failure. Every link ends here and the rest of the run goes on.
    fn seal(&mut self) {
        for mut up in self.upstream.iter() {
            loop {
                up.slot.seal();
                up.slot.write(&Upstream::default(), self.seek_gen);
                up.peer.signal();
                let Some(next) = &up.next else { break };
                up = &next.0;
            }
        }
        for d in &self.downstream {
            d.slot.seal();
            d.peer.signal();
        }
    }

    fn splice_edits(
        &mut self,
        edits: Vec<(ElementId, PadEdit)>,
        down_add: &mut [DownLink],
        seek_gen: u64,
    ) -> Result<Option<SeekTarget>, Error> {
        let tail = self.elements.len() - 1;
        let (fresh, over) = (seek_gen == self.seek_gen, self.input_over());
        let mut restart = None;
        for (id, edit) in edits {
            let Some(i) = self.ids.iter().position(|e| *e == id) else {
                continue;
            };
            let ctx = &mut self.ctxs[i];
            match edit {
                PadEdit::Pads(srcs, sinks) => ctx.grow_pads(srcs, sinks),
                PadEdit::StreamPad(stream, pad) => ctx.set_stream_pad(stream, pad),
                PadEdit::SinkFormat(pad, format) => ctx.set_negotiated_sink(pad, format),
                PadEdit::SinkOffers(offers) => ctx.add_offers(offers),
                PadEdit::Latency(latency) => ctx.set_path_latency(latency),
                PadEdit::SrcFormat(pad, format) => match ctx.negotiated_src(pad).cloned() {
                    None => ctx.set_negotiated_src(pad, format),
                    Some(now) if now != format => {
                        let told = |d: &&mut DownLink| d.pad == pad && d.staged.is_none();
                        if let Some(d) = down_add.iter_mut().find(told) {
                            let mut first = Batch::new(BYTES);
                            first.push_event(Event::FormatChange(now));
                            d.staged = Some(first);
                        }
                    }
                    Some(_) => {}
                },
                PadEdit::Restart if fresh => {
                    self.restart = Restart::Handed;
                    crate::log!(&*ctx, Level::Debug, "restart_handed", member = i as u64);
                    ctx.aim(self.shared.control.target(id, self.seek_gen));
                    if self.phase != Phase::Running || self.head_done {
                        (self.head_done, self.phase) = (over, Phase::Running);
                        self.cursor.end(PLAYING);
                    }
                    if i == tail && !down_add.is_empty() {
                        restart = ctx.seek_target();
                        continue;
                    }
                    self.elements[i].event(ctx, &Event::FlushStart)?;
                    ctx.clear_outputs();
                    if i < tail {
                        self.ctxs[i + 1].discard_input(None);
                        self.reset_pull(i);
                    }
                }
                PadEdit::Restart => {}
            }
        }
        Ok(restart)
    }

    fn splice_adds(
        &mut self,
        down: Vec<DownLink>,
        up: Vec<UpLink>,
        restart: Option<SeekTarget>,
        handover: &[(SinkPad, FixedFormat)],
    ) -> Result<(), Error> {
        let tail = self.elements.len() - 1;
        for mut d in down {
            let pad = d.pad;
            d.level.restart(self.seek_gen);
            self.weave.join(&d);
            let shared = self.downstream.iter().any(|o| o.pad == pad);
            self.ctxs[tail].set_pad_linked(pad, true);
            self.ctxs[tail].bump_spare_cap(d.data.capacity() + d.shells.capacity() + 1);
            let mut first = d.staged.take().unwrap_or_else(|| Batch::new(BYTES));
            if shared {
                let mut held = self.ctxs[tail].take_output(pad);
                self.elements[tail].event(&mut self.ctxs[tail], &Event::SrcJoined { pad })?;
                let slot = self.ctxs[tail].output_on(pad);
                first.append(slot);
                slot.append(&mut held);
                self.ctxs[tail].recycle_shell(held);
            }
            if self.phase == Phase::Ended {
                if !shared {
                    first.append(self.ctxs[tail].output_on(pad));
                }
                first.eos = true;
            }
            if first.eos || !first.is_inert() {
                first.seek_gen = self.seek_gen;
                send(&mut d, first, self.ctxs[tail].bus());
            }
            self.downstream.push(d);
            self.weave.recount(&self.downstream);
            if !shared {
                let on = Event::SrcLinked { pad, on: true, restart };
                self.elements[tail].event(&mut self.ctxs[tail], &on)?;
                self.ctxs[tail].link_seen();
            }
        }
        self.fan_out = fans_out(&self.downstream);
        let mut joins = up;
        let mut i = 0;
        while i < joins.len() {
            let pad = joins[i].pad;
            let Some((_, format)) = handover.iter().find(|(p, _)| *p == pad) else {
                i += 1;
                continue;
            };
            match self.upstream.iter_mut().find(|u| u.pad == pad && !u.ended) {
                Some(old) => {
                    let up = joins.remove(i);
                    up.level.restarted(self.seek_gen);
                    queue_behind(old, up, Some(format.clone()));
                }
                None => {
                    self.upstream.retain(|u| u.pad != pad);
                    self.ctxs[0].set_negotiated_sink(pad, format.clone());
                    i += 1;
                }
            }
        }
        if !joins.is_empty() {
            let at = self.shared.control.position_now();
            self.ctxs.iter_mut().for_each(|c| c.set_presentation(at));
        }
        for up in joins {
            let pad = up.pad;
            self.cut_pads.retain(|p| *p != pad);
            self.ctxs[0].reopen_pad(pad);
            up.level.restarted(self.seek_gen);
            self.upstream.push(up);
            self.elements[0].event(&mut self.ctxs[0], &Event::SinkLinked { pad, on: true })?;
        }
        let fan_in = self.upstream.len() > 1;
        let head_pad = if self.upstream.len() == 1 { Some(self.upstream[0].pad) } else { None };
        match (self.fan_in, fan_in, self.head_sink_pad, head_pad) {
            (false, true, Some(pad), _) => self.ctxs[0].move_input(pad, true),
            (true, false, _, Some(pad)) => self.ctxs[0].move_input(pad, false),
            _ => {}
        }
        (self.fan_in, self.head_sink_pad) = (fan_in, head_pad);
        Ok(())
    }

    fn splice_verdict(&mut self, joined: bool) -> Result<(), Error> {
        if self.cut_head || (!self.is_source && self.upstream.iter().all(|up| up.ended)) {
            self.head_done = true;
        } else if joined && self.head_done && self.phase == Phase::Running {
            self.head_done = false;
        } else if joined && self.head_done {
            self.flush_as(false)?;
        }
        let terminal = self.downstream.is_empty();
        self.cursor.set_terminal(terminal);
        if terminal && self.phase == Phase::Ended {
            self.cursor.end(self.seek_gen);
        }
        Ok(())
    }

    fn input_over(&self) -> bool {
        self.cut_head || (!self.is_source && self.upstream.is_empty())
    }

    fn skip_to(&mut self, to_time: Timestamp) -> Result<(), Error> {
        for c in self.ctxs.iter_mut() {
            c.set_presentation(to_time);
        }
        self.broadcast(&Event::SkippedTo { to_time })
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.flush_as(true)
    }

    fn flush_as(&mut self, revive: bool) -> Result<(), Error> {
        self.broadcast(&Event::FlushStart)?;
        for (c, counters) in self.ctxs.iter_mut().zip(&self.counters) {
            c.discard_buffers();
            counters.forget_pts_out();
        }
        (0..self.elements.len()).for_each(|i| self.reset_pull(i));
        let tail = self.elements.len() - 1;
        self.weave.restart();
        self.restart = Restart::Idle;
        for d in self.downstream.iter_mut() {
            d.level.restart(self.seek_gen);
            d.seen = UNSEEN;
            if let Some(mut stale) = d.staged.take() {
                stale.retain_state();
                let slot = self.ctxs[tail].output_on(d.pad);
                stale.append(slot);
                std::mem::swap(slot, &mut stale);
                self.ctxs[tail].recycle_shell(stale);
            }
        }
        self.head_done = self.input_over();
        self.phase = Phase::Running;
        self.cursor.end(PLAYING);
        for c in self.ctxs.iter_mut() {
            c.clear_pads_closed();
        }
        for pad in &self.cut_pads {
            self.ctxs[0].set_pad_closed(*pad);
        }
        for up in self.upstream.iter_mut() {
            up.ended &= !revive;
            if up.ended {
                self.ctxs[0].set_pad_closed(up.pad);
            }
            if up.level.restarted(self.seek_gen) {
                up.end = Timestamp::NONE;
            }
        }
        self.tell_taken();
        Ok(())
    }

    fn rest_ended(&mut self) -> Result<Outcome, Error> {
        let mut progressed = false;
        self.read_feedback()?;
        self.push_staged(&mut progressed);
        self.drive_io(&mut progressed);
        let held = self.staged_pad().or_else(|| self.closing.first().map(|d| d.pad));
        if self.leaving.is_some() && held.is_none() {
            return Ok(Outcome::Exit);
        }
        if self.downstream.is_empty() {
            progressed |= self.shared.control.try_finish() == Finish::Busy;
        }
        Ok(match held {
            _ if progressed => Outcome::Progressed,
            Some(pad) => Outcome::Blocked { on: BlockedOn::Ring { pad } },
            None => Outcome::Idle { until: Timestamp::NONE, within: Duration::MAX },
        })
    }

    fn staged_pad(&self) -> Option<SrcPad> {
        self.downstream.iter().find(|d| d.staged.is_some()).map(|d| d.pad)
    }

    fn resting(&mut self) -> Option<SrcPad> {
        for j in 0..self.downstream.len() {
            let d = &self.downstream[j];
            if !(d.limited && d.level.full()) {
                continue;
            }
            match self.weave.full(j, &self.downstream) {
                Full::Pass => continue,
                Full::Rest => {}
                Full::Capped(interleave) => {
                    let (tail, pad) = (self.elements.len() - 1, self.downstream[j].pad);
                    let error = Error::Graph(GraphError::Interleaved {
                        element: self.ids[tail],
                        pad,
                        interleave,
                    });
                    self.ctxs[tail].post(BusMessage::Warning { element: self.ids[tail], error });
                }
            }
            let d = &self.downstream[j];
            // Raised before the second look, so the next take signals this group.
            d.room.raise();
            if d.level.full() {
                return Some(d.pad);
            }
        }
        None
    }

    fn room(&self) -> Timestamp {
        let limited = self.downstream.iter().filter(|d| d.limited && !d.sparse);
        limited.map(|d| d.level.room()).min().unwrap_or(Timestamp::NONE)
    }

    fn tell_room(&mut self, cap: usize) {
        let at_cap = |d: &DownLink| d.staged.is_some() || d.data.len() >= d.data.capacity();
        let tail = self.elements.len() - 1;
        let (ctxs, downstream) = (&mut self.ctxs, &self.downstream);
        ctxs[tail].clear_full_pads();
        for d in downstream.iter().filter(|d| at_cap(d)) {
            // Raised before the second look, so the next take signals this group.
            d.room.raise();
            if at_cap(d) {
                ctxs[tail].set_full_pad(d.pad);
            }
        }
        for i in 0..tail {
            if self.ctxs[i].asks_room() {
                let behind = self.ctxs[i + 1].input_len() >= cap;
                self.ctxs[i].clear_full_pads();
                if behind {
                    let hop = self.ctxs[i].hop_pad();
                    self.ctxs[i].set_full_pad(hop);
                }
            }
        }
    }

    fn run_keepers(&mut self, progressed: &mut bool) -> Result<Option<Outcome>, Error> {
        if !self.ctxs.iter().any(Ctx::asks_room) {
            return Ok(None);
        }
        self.tell_room(INLINE_INPUT_CAP);
        for i in 0..self.elements.len() {
            if self.ctxs[i].asks_room() {
                self.run_member(i, Timestamp::NONE, progressed)?;
            }
        }
        self.drive_io(progressed);
        Ok(Some(self.settle(*progressed)))
    }

    fn tell_taken(&mut self) {
        for up in self.upstream.iter_mut() {
            let pad = self.fan_in.then_some(up.pad);
            let rows = match pad {
                Some(pad) => self.ctxs[0].input_len_on(pad),
                None => self.ctxs[0].input_len(),
            };
            if (up.popped, rows, up.end) == up.seen {
                continue;
            }
            up.seen = (up.popped, rows, up.end);
            let (left, front) = self.ctxs[0].input_left(pad);
            let now =
                (up.popped - left.min(up.popped), if front.is_some() { front } else { up.end });
            if now == up.told {
                continue;
            }
            up.told = now;
            up.level.take(now.0, now.1);
            if up.room.take() {
                up.peer.signal();
            }
            if up.marked {
                let (element, pad, bus) = (self.ids[0], Some(up.pad), self.ctxs[0].bus());
                up.level.judge(false, |fill, held| {
                    bus.send(BusMessage::Buffering { element, pad, fill, held })
                });
            }
        }
    }

    fn push_staged(&mut self, progressed: &mut bool) {
        let bus = self.ctxs[self.elements.len() - 1].bus();
        for d in self.closing.iter_mut() {
            if let Some(out) = d.staged.take().filter(|_| !d.data.is_closed()) {
                send(d, out, bus);
            }
        }
        let mut k = 0;
        while k < self.closing.len() {
            match self.closing[k].staged.is_some() {
                true => k += 1,
                false => close(self.closing.swap_remove(k)),
            }
        }
        let leaving = self.leaving.is_some();
        for d in self.downstream.iter_mut() {
            if d.staged.is_some() && d.limited && d.level.full() {
                // staged behind a full level, the take that makes room signals the group
                d.room.raise();
                continue;
            }
            let Some(out) = d.staged.take() else { continue };
            if !(leaving && d.data.is_closed()) {
                send(d, out, bus);
            }
            if d.staged.is_none() {
                *progressed = true;
            }
        }
    }

    fn pop(&self, link: usize) -> Option<Batch> {
        let up = &self.upstream[link];
        let batch = match up.data.try_pop() {
            Some(batch) => batch,
            None if !up.ended && up.slot.sealed() => up.data.try_pop().unwrap_or_else(|| {
                let mut end = Batch::new(BYTES);
                (end.eos, end.seek_gen) = (true, self.seek_gen);
                end
            }),
            None => return None,
        };
        if up.room.take() {
            up.peer.signal();
        }
        Some(batch)
    }

    fn ingest(&mut self, link: usize, mut batch: Batch) -> Result<bool, Error> {
        debug_assert!(self.fan_in || link == 0, "a linear head has one upstream link");
        let mut link = link;
        if batch.seek_gen > self.seek_gen {
            let (ring, splices) = (self.upstream[link].data.ring_id(), self.splices);
            self.apply_commands()?;
            debug_assert!(batch.seek_gen <= self.seek_gen, "the producer read it in the log");
            if self.splices != splices {
                // A splice moved the link, or cut it and its data with it.
                match self.upstream.iter().position(|u| u.data.ring_id() == ring) {
                    Some(at) => link = at,
                    None => return Ok(false),
                }
            }
        }
        let (bytes, _, end) = batch.span();
        self.upstream[link].popped += bytes;
        if batch.seek_gen < self.seek_gen {
            batch.retain_state();
            self.head_append(link, &mut batch);
            batch.clear();
            let _ = self.upstream[link].shells.try_push(batch);
            return Ok(false);
        }
        let up = &mut self.upstream[link];
        if end.is_some() {
            up.end = end;
        }
        let handed = batch.eos && up.next.is_some();
        if handed {
            batch.eos = false;
            if let Some(format) = up.next.as_mut().and_then(|next| next.1.take()) {
                batch.push_event(Event::FormatChange(format));
            }
        } else if batch.eos {
            up.ended = true;
            if !self.fan_in {
                self.head_done = true;
            }
        }
        if !batch.is_empty() {
            self.counters[0].record_in(batch.len() as u64, bytes);
        }
        if let Some(t) = batch.pushed_at.take() {
            self.counters[0].queue_ns.record(self.shared.trace_ns().saturating_sub(t));
        }
        self.head_append(link, &mut batch);
        batch.clear();
        let _ = self.upstream[link].shells.try_push(batch);
        if handed {
            self.hand_over(link);
        }
        Ok(true)
    }

    fn hand_over(&mut self, link: usize) {
        let Some(next) = self.upstream[link].next.take() else {
            return;
        };
        let (up, format) = *next;
        if let Some(format) = format {
            self.ctxs[0].set_negotiated_sink(up.pad, format);
        }
        self.upstream[link] = up;
        if self.upstream[link].room.take() {
            self.upstream[link].peer.signal();
        }
    }

    fn hand_over_dead(&mut self) {
        for link in 0..self.upstream.len() {
            let up = &self.upstream[link];
            if up.next.is_some() && up.data.is_closed() && up.data.is_empty() {
                self.hand_over(link);
            }
        }
    }

    fn head_append(&mut self, link: usize, batch: &mut Batch) {
        if self.fan_in {
            self.ctxs[0].append_input_on(self.upstream[link].pad, batch);
        } else {
            self.ctxs[0].input_append(batch);
        }
    }

    fn feed(&mut self, progressed: &mut bool) -> Result<(), Error> {
        self.hand_over_dead();
        if self.fan_in {
            self.feed_fan_in(progressed)
        } else if !self.upstream.is_empty() {
            self.feed_linear(progressed)
        } else {
            Ok(())
        }
    }

    fn feed_fan_in(&mut self, progressed: &mut bool) -> Result<(), Error> {
        let splices = self.splices;
        for i in 0..self.upstream.len() {
            while self.ctxs[0].input_len_on(self.upstream[i].pad) < INLINE_INPUT_CAP {
                let Some(batch) = self.pop(i) else { break };
                if self.ingest(i, batch)? {
                    *progressed = true;
                }
                if self.splices != splices {
                    return Ok(());
                }
            }
        }
        for up in self.upstream.iter().filter(|up| up.ended) {
            self.ctxs[0].set_pad_closed(up.pad);
        }
        if self.upstream.iter().all(|up| up.ended) {
            self.head_done = true;
        }
        Ok(())
    }

    fn feed_linear(&mut self, progressed: &mut bool) -> Result<(), Error> {
        let splices = self.splices;
        while self.ctxs[0].input_len() < INLINE_INPUT_CAP && self.splices == splices {
            let Some(batch) = self.pop(0) else { break };
            if self.ingest(0, batch)? {
                *progressed = true;
            }
        }
        Ok(())
    }

    fn run_members(&mut self, progressed: &mut bool) -> Result<(), Error> {
        let m = self.elements.len();
        let room = self.room();
        if self.weave.timed >= 2 {
            let behind = self.weave.behind(&self.downstream);
            self.ctxs[m - 1].set_behind(behind);
        }
        let cap = if room.is_some() { 1 } else { INLINE_INPUT_CAP };
        if self.ctxs.iter().any(Ctx::asks_room) {
            self.tell_room(cap);
        }
        for i in 0..m {
            if i + 1 < m && self.ctxs[i + 1].input_len() >= cap && !self.ctxs[i].asks_room() {
                continue;
            }
            self.run_member(i, room, progressed)?;
        }
        Ok(())
    }

    fn run_member(
        &mut self,
        i: usize,
        room: Timestamp,
        progressed: &mut bool,
    ) -> Result<(), Error> {
        if let Some(up) = self.ctxs.get_mut(i + 1).and_then(|c| c.take_published(SinkPad(0))) {
            let hop = self.ctxs[i].hop_pad();
            self.tell(i, hop, up)?;
        }
        self.deliver_prop_changes(i)?;
        let trace_t0 = self.shared.tracing.load(Ordering::Relaxed).then(|| self.shared.trace_ns());
        self.deliver_due_events(i, progressed)?;
        self.drop_refused(i);
        let with_input = !(i == 0 && (self.is_source || self.fan_in));
        let pending = self.ctxs[i].pending();
        let flow = process_within(&mut *self.elements[i], &mut self.ctxs[i], with_input, room);
        if let Some(t0) = trace_t0 {
            self.counters[i].process_ns.record(self.shared.trace_ns().saturating_sub(t0));
        }
        let flow = match flow {
            Err(error) if error.scope() == Scope::Buffer => {
                self.counters[i].record_drops(1);
                self.ctxs[i].post(BusMessage::Warning { element: self.ids[i], error });
                Flow::Ok
            }
            flow => flow?,
        };
        if i == 0 {
            self.publish_up();
        }
        if self.ctxs[i].pending() < pending {
            *progressed = true;
        }
        if i == 0 && self.is_source && matches!(flow, Flow::Eos) {
            self.head_done = true;
        }
        after_process(&mut self.ctxs[i], &self.shared.vocabulary, |ctx, pad, at, fixed| {
            ctx.output_on(pad).push_event_at(at, Event::FormatChange(fixed));
        });
        let (nbuf, nbytes) = self.ctxs[i].total_output();
        if nbuf > 0 {
            self.counters[i].record_out(nbuf, nbytes);
            let counters = &self.counters[i];
            self.ctxs[i]
                .newest_output_pts()
                .for_each(|(pad, pts)| counters.record_pts_out(pad, pts));
            *progressed = true;
        }
        self.forward_io(i)?;
        self.sync_pull(i, progressed);
        if i == 0 && nbuf > 0 && self.restart == Restart::Handed {
            self.restart = Restart::Reading;
        }
        if i + 1 < self.elements.len() {
            self.hand_off_inline(i, nbuf, nbytes);
        }
        Ok(())
    }

    fn deliver_prop_changes(&mut self, i: usize) -> Result<(), Error> {
        let mut dirty = self.ctxs[i].poll_prop_changes();
        while dirty != 0 {
            let bit = dirty.trailing_zeros() as usize;
            dirty &= dirty - 1;
            if let Some((name, value)) = self.ctxs[i].prop_entry(bit) {
                self.elements[i].event(&mut self.ctxs[i], &Event::PropChanged { name, value })?;
            }
        }
        Ok(())
    }

    fn deliver_due_events(&mut self, i: usize, progressed: &mut bool) -> Result<(), Error> {
        let vocabulary = &self.shared.vocabulary;
        if i == 0 && self.fan_in {
            for up in &self.upstream {
                while let Some(ev) = self.ctxs[0].any_input_event_on(up.pad) {
                    let pad = Some(up.pad);
                    deliver_event(&mut *self.elements[0], &mut self.ctxs[0], pad, ev, vocabulary)?;
                    *progressed = true;
                }
            }
        } else if !(i == 0 && self.is_source) {
            let pad = if i == 0 { self.head_sink_pad } else { self.member_sink_pads[i] };
            while let Some(ev) = self.ctxs[i].due_input_event() {
                deliver_event(&mut *self.elements[i], &mut self.ctxs[i], pad, ev, vocabulary)?;
                *progressed = true;
            }
        }
        Ok(())
    }

    fn drop_refused(&mut self, i: usize) {
        let dropped = match (i == 0 && self.fan_in, i == 0) {
            (true, _) => {
                self.upstream.iter().map(|up| self.ctxs[0].drop_refused(up.pad, true)).sum()
            }
            (false, true) => {
                self.head_sink_pad.map_or(0, |pad| self.ctxs[0].drop_refused(pad, false))
            }
            (false, false) => {
                self.member_sink_pads[i].map_or(0, |pad| self.ctxs[i].drop_refused(pad, false))
            }
        };
        if dropped > 0 {
            self.counters[i].record_drops(dropped);
        }
    }

    fn hand_off_inline(&mut self, i: usize, nbuf: u64, nbytes: u64) {
        if nbuf > 0 {
            self.counters[i + 1].record_in(nbuf, nbytes);
        }
        let (left, right) = self.ctxs.split_at_mut(i + 1);
        let hop = left[i].hop_pad();
        right[0].input_append(left[i].output_on(hop));
        if right[0].pulling.is_on() {
            right[0].pulling.tags.append(&mut left[i].serving.tags);
        }
        let (unrouted, _) = left[i].total_output();
        if unrouted > 0 {
            self.counters[i].record_drops(unrouted);
            left[i].clear_outputs();
        }
    }

    fn route(&mut self, progressed: &mut bool) {
        let tail = self.elements.len() - 1;
        for d in &self.downstream {
            while let Some(shell) = d.shells.try_pop() {
                self.ctxs[tail].recycle_shell(shell);
            }
        }
        for j in 0..self.downstream.len() {
            let pad = self.downstream[j].pad;
            let out = self.ctxs[tail].output_on(pad);
            if out.is_inert() && !out.eos {
                continue;
            }
            let shared = self.fan_out && self.downstream[j + 1..].iter().any(|o| o.pad == pad);
            let mut out = match shared {
                true => self.ctxs[tail].share_output(pad),
                false => self.ctxs[tail].take_output(pad),
            };
            out.seek_gen = self.seek_gen;
            if self.shared.tracing.load(Ordering::Relaxed) {
                out.pushed_at = Some(self.shared.trace_ns());
            }
            *progressed = true;
            // A codec head or a bare event is no media, only a stamped row says the tail is back
            // where the ring filled.
            let newest = out.newest();
            let d = &mut self.downstream[j];
            let full = d.staged.is_some() || (d.limited && d.level.full());
            // A restart runs the tail on a full level, its output waits staged like one a full
            // ring refused, the level is a bound the ring does not keep.
            let sent = if d.staged.is_some() {
                self.stage_behind(j, out)
            } else if full && self.restart != Restart::Idle {
                d.room.raise();
                d.staged = Some(out);
                Sent::Staged
            } else {
                send(d, out, self.ctxs[tail].bus())
            };
            // The tail fed media read behind the restart to a full ring, so the group rests on
            // it from the next pass.
            let caught_up = newest.is_some() && (full || !matches!(sent, Sent::Pushed));
            if self.restart == Restart::Reading && caught_up {
                crate::log!(
                    &self.ctxs[tail],
                    Level::Debug,
                    "restart_done",
                    pad = pad.0 as u64,
                    pts = newest
                );
                self.restart = Restart::Idle;
            }
            match sent {
                Sent::Pushed | Sent::Staged => {}
                Sent::Dropped { rows, shell } => {
                    self.counters[tail].record_drops(rows);
                    if let Some(shell) = shell {
                        self.ctxs[tail].recycle_shell(shell);
                    }
                }
            }
            self.counters[tail].record_queue_fill(self.downstream[j].data.len() as u32);
        }
        let (unrouted, _) = self.ctxs[tail].total_output();
        if unrouted > 0 {
            self.counters[tail].record_drops(unrouted);
        }
        self.ctxs[tail].clear_outputs();
    }

    /// Queue `out` behind the batch link `j`'s ring refused, which only a restart runs the tail
    /// past. A leaky ring loses the rows as it would have.
    fn stage_behind(&mut self, j: usize, mut out: Batch) -> Sent {
        let tail = self.elements.len() - 1;
        let d = &mut self.downstream[j];
        let (rows, eos) = (out.len() as u64, out.eos);
        let leaks = d.leaky == Leaky::DropNewest && rows > 0;
        if leaks {
            out.retain_state();
        }
        if let Some(staged) = d.staged.as_mut() {
            staged.append(&mut out);
            staged.eos |= eos;
        }
        if leaks {
            return Sent::Dropped { rows, shell: Some(out) };
        }
        self.ctxs[tail].recycle_shell(out);
        Sent::Staged
    }

    fn forward_io(&mut self, i: usize) -> Result<(), Error> {
        let (subs, names) = self.ctxs[i].submissions();
        if subs.is_empty() {
            return Ok(());
        }
        let reactor = match &mut self.reactor {
            Some(reactor) => reactor,
            None => {
                let mut built =
                    (self.factory)().map_err(|kind| Error::Io { what: "reactor init", kind })?;
                built.set_waker(Waker(Arc::clone(&self.station)));
                self.reactor.insert(built)
            }
        };
        reactor.submit(subs, names);
        self.ctxs[i].names_spent();
        Ok(())
    }

    fn drive_io(&mut self, progressed: &mut bool) {
        let Some(reactor) = &mut self.reactor else {
            return;
        };
        reactor.run_once(&mut self.completions);
        *progressed |= !self.completions.is_empty();
        self.io_woke = !self.completions.is_empty() && self.station.bell.get().is_some();
        self.deliver_completions();
    }

    fn deliver_completions(&mut self) {
        for (elem, c) in self.completions.drain(..) {
            if let Some(idx) = self.ids.iter().position(|e| *e == elem) {
                self.ctxs[idx].deliver_completion(c);
            }
        }
    }

    fn terminate(&mut self) -> Result<bool, Error> {
        debug_assert!(
            self.head_done || self.phase == Phase::Running,
            "only a seek clears head_done, and it returns the phase to Running"
        );
        let quiescent = self.staged_pad().is_none()
            && self.ctxs.iter().all(|c| c.io_flying() == 0 && c.inbox_empty() && c.inputs_empty());
        if !(self.head_done && quiescent) || self.end_held {
            return Ok(false);
        }
        let m = self.elements.len();
        let next = match self.phase {
            Phase::Running => 0,
            Phase::Draining { next } => next,
            Phase::Ended => {
                debug_assert!(false, "an ended group rests before its pass");
                return Ok(false);
            }
        };
        debug_assert!(next <= m, "Draining counts delivered Eos, at most one per element");
        if next < m {
            self.elements[next].event(&mut self.ctxs[next], &Event::Eos)?;
            self.phase = Phase::Draining { next: next + 1 };
            return Ok(true);
        }
        self.mark_ended();
        Ok(true)
    }

    fn mark_ended(&mut self) {
        let tail = self.elements.len() - 1;
        for d in self.downstream.iter_mut() {
            let mut out = self.ctxs[tail].take_output(d.pad);
            out.seek_gen = self.seek_gen;
            out.eos = true;
            send(d, out, self.ctxs[tail].bus());
        }
        if self.downstream.is_empty() {
            self.cursor.end(self.seek_gen);
        }
        self.phase = Phase::Ended;
    }

    fn settle(&mut self, progressed: bool) -> Outcome {
        let mut until = Timestamp::NONE;
        let mut within = Duration::MAX;
        let mut starved = None;
        self.end_held = false;
        for (ctx, id) in self.ctxs.iter_mut().zip(&self.ids) {
            if let Some(at) = ctx.take_wake_at() {
                until = until.min(at);
            }
            within = within.min(ctx.take_wake_within());
            if ctx.take_starved() && starved.is_none() {
                starved = Some(*id);
            }
            self.end_held |= ctx.take_end_held();
        }
        if progressed {
            return Outcome::Progressed;
        }
        let flying = self.ctxs.iter().any(|c| c.io_flying() > 0);
        let due = self.reactor.as_ref().and_then(|r| r.due_in());
        let within = within.min(due.filter(|_| flying).unwrap_or(Duration::MAX));
        match starved {
            _ if flying && until.is_none() && within == Duration::MAX => {
                Outcome::Blocked { on: BlockedOn::Io }
            }
            Some(element) if until.is_none() && within == Duration::MAX => {
                Outcome::Blocked { on: BlockedOn::Pool { element } }
            }
            _ => Outcome::Idle { until, within },
        }
    }

    fn held(&self) -> usize {
        let staged = self.downstream.iter().filter_map(|d| d.staged.as_ref()).map(Batch::len);
        self.ctxs.iter().map(Ctx::held).sum::<usize>() + staged.sum::<usize>()
    }

    fn clock_deadline(&self, until: Timestamp) -> Timestamp {
        let base = self.shared.base.load(Ordering::Acquire);
        let base = if base == crate::time::BASE_UNSET { 0 } else { base };
        crate::time::deadline(base, until, Timestamp::ZERO)
    }

    fn settle_io(&mut self) {
        for i in 0..self.elements.len() {
            let _ = self.forward_io(i);
        }
        while let Some(reactor) = &mut self.reactor {
            reactor.run_once(&mut self.completions);
            let idle = reactor.is_idle();
            self.deliver_completions();
            if idle {
                break;
            }
            wait_yield();
        }
    }

    fn stop_reason(&self) -> StopReason {
        match (self.undone, self.leaving) {
            (true, _) => StopReason::RolledBack,
            (false, Some(_)) => StopReason::Removed,
            (false, None) => StopReason::RunEnd,
        }
    }

    fn stop(&mut self) {
        let reason = self.stop_reason();
        for i in (0..self.elements.len()).rev() {
            self.ctxs[i].set_stop_reason(reason);
            self.elements[i].stop(&mut self.ctxs[i]);
        }
    }
}

fn drive(group: &mut Group) -> Result<(), Error> {
    let station = Arc::clone(&group.station);
    let (mut last, mut parked) = (Woke::Signal, 0);
    // The park ended with no signal, so only the reactor had work.
    let mut unsignaled = false;
    loop {
        // Sampled before the pass looks at any work, so a later signal ends the park.
        let seen = station.epoch();
        let outcome = group.pass()?;
        if unsignaled && outcome != Outcome::Progressed {
            station.count_empty_wake();
        }
        let (until, within) = match outcome {
            Outcome::Progressed => {
                if last == Woke::Backstop && station.epoch() == parked && !group.io_woke {
                    station.count_late_wake();
                }
                last = Woke::Signal;
                continue;
            }
            Outcome::Exit => return Ok(()),
            Outcome::Blocked { .. } => (Timestamp::NONE, Duration::MAX),
            Outcome::Idle { until, within } => (until, within),
        };
        parked = seen;
        last = wait_idle(group, &station, seen, until, within, last == Woke::Ripe);
        unsignaled = last == Woke::Signal && station.epoch() == seen;
        if last == Woke::Backstop {
            station.count_tick_expiry();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Woke {
    Signal,
    Ripe,
    Slice,
    Backstop,
}

// The threaded driver's wait, the only place a group sleeps.
#[allow(clippy::disallowed_methods)]
fn wait_idle(
    group: &mut Group,
    station: &Station,
    seen: u64,
    until: Timestamp,
    within: Duration,
    mut spent: bool,
) -> Woke {
    let limit = within.min(group.backstop);
    let started = Instant::now();
    loop {
        let left = limit.saturating_sub(started.elapsed());
        let nap = match until.is_some() {
            true => group.shared.clock.nap(group.clock_deadline(until)),
            false => Nap::Signal,
        };
        spent &= nap == Nap::Due;
        let timeout = match nap {
            Nap::Due if !spent => return Woke::Ripe,
            Nap::Sleep(nap) if nap < left => nap,
            _ if left.is_zero() => {
                let scheduled = within < group.backstop || matches!(nap, Nap::Sleep(_));
                return if scheduled { Woke::Slice } else { Woke::Backstop };
            }
            _ => left,
        };
        let sleep = &mut |t| group.reactor.as_mut().is_some_and(|r| r.park(t));
        if station.park(seen, Some(timeout), Some(sleep)) == Wake::Signal {
            return Woke::Signal;
        }
    }
}

fn wait_yield() {
    std::thread::yield_now();
}

struct Seat {
    group: Option<Group>,
    tag: Tag,
    seen: u64,
    at: Option<Cut>,
    ripe: bool,
    last: Option<Outcome>,
    within_at: Timestamp,
    spent: bool,
}

/// A run under the stepped driver, from [`Pipeline::step`](crate::pipeline::Pipeline::step).
pub struct Stepped {
    /// Seats of the groups not closed, `open[i]` the handle of `seats[i]`, ascending.
    seats: Vec<Seat>,
    open: Vec<usize>,
    next: usize,
    clock: Arc<dyn Clock>,
    control: Arc<Control>,
    bus: BusSender,
    failed: Option<Error>,
    end: Option<RunEnd>,
}

fn ripe(clock: &dyn Clock, group: &Group, until: Timestamp) -> bool {
    until.is_some() && clock.nap(group.clock_deadline(until)) == Nap::Due
}

impl Stepped {
    // One seat per group, once per run.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn open(
        setups: Vec<GroupSetup>,
        clock: Arc<dyn Clock>,
        control: Arc<Control>,
        end: RunEnd,
    ) -> Result<Stepped, Error> {
        let (seats, open, bus) =
            (Vec::with_capacity(setups.len()), Vec::with_capacity(setups.len()), end.bus.clone());
        let mut run =
            Stepped { seats, open, next: 0, clock, control, bus, failed: None, end: Some(end) };
        setups.into_iter().for_each(|setup| run.seat(setup));
        match run.failed.clone() {
            None => Ok(run),
            Some(e) => run.finish().and(Err(e)),
        }
    }

    fn seat(&mut self, setup: GroupSetup) {
        let tag = Tag::of(&setup);
        let group = match self.failed {
            None => Group::open(setup)
                .map_err(|e| {
                    self.failed =
                        retire_group(&self.control, &tag, Some(e.clone()), judge(&tag, Some(&e)))
                })
                .ok(),
            Some(_) => {
                self.control.abort();
                self.control.retire(&tag.cursor, &tag.station, tag.ids[0]);
                None
            }
        };
        let handle = self.next;
        self.next += 1;
        if group.is_some() {
            let (within_at, at, ripe, spent) = (Timestamp::NONE, None, false, false);
            self.open.push(handle);
            self.seats.push(Seat { group, tag, seen: 0, at, ripe, last: None, within_at, spent });
        }
    }

    fn slot(&self, g: usize) -> Option<usize> {
        self.open.binary_search(&g).ok()
    }

    /// Start the groups a commit staged and advance the change in flight.
    pub fn service(&mut self) {
        for setup in self.control.topology.take_setups() {
            self.seat(setup);
        }
        self.control.topology.advance(&self.control, &self.bus);
    }

    /// How many groups the run had, the handles below this, in topological order.
    pub fn groups(&self) -> usize {
        self.next
    }

    pub fn elements(&self, g: usize) -> &[ElementId] {
        self.slot(g).map_or(&[], |s| &self.seats[s].tag.ids)
    }

    /// The seats held and the room for them, which follow the groups that have not closed.
    pub fn seat_rows(&self) -> (usize, usize) {
        (self.seats.len(), self.seats.capacity())
    }

    pub fn running(&self) -> &[usize] {
        &self.open
    }

    /// Whether a pass of group `g` could do anything now.
    pub fn ready(&self, g: usize) -> bool {
        let Some(seat) = self.slot(g).map(|s| &self.seats[s]) else {
            return false;
        };
        let Some(group) = &seat.group else {
            return false;
        };
        if seat.at.is_some() || group.station.epoch() != seat.seen {
            return true;
        }
        match seat.last {
            Some(Outcome::Idle { until, .. }) => {
                let ripe = !seat.spent && ripe(&*self.clock, group, until);
                ripe || self.clock.now() >= seat.within_at
            }
            Some(Outcome::Blocked { .. }) => false,
            _ => true,
        }
    }

    /// The earliest clock reading at which a group becomes ready without a signal.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        let mut next = Timestamp::NONE;
        for (g, seat) in self.open.iter().copied().zip(&self.seats) {
            let (Some(group), Some(Outcome::Idle { until, .. })) = (&seat.group, seat.last) else {
                continue;
            };
            if self.ready(g) {
                continue;
            }
            next = next.min(seat.within_at);
            if !seat.spent {
                next = next.min(group.clock_deadline(until));
            }
        }
        next.is_some().then_some(next)
    }

    /// One pass of group `g`, tearing it down on `Exit` or an error.
    pub fn pass(&mut self, g: usize) -> Result<Outcome, Error> {
        self.pass_to(g, Cut::End).map(|o| o.unwrap_or(Outcome::Progressed))
    }

    /// The pass of group `g` up to `cut`, `None` while it stands there.
    pub fn pass_to(&mut self, g: usize, cut: Cut) -> Result<Option<Outcome>, Error> {
        let Some(slot) = self.slot(g) else {
            return Ok(Some(Outcome::Exit));
        };
        let (seat, clock) = (&mut self.seats[slot], &*self.clock);
        let Some(group) = seat.group.as_mut() else {
            return Ok(Some(Outcome::Exit));
        };
        let mut top = Ok(None);
        let fresh = seat.at.take().is_none();
        if fresh {
            seat.ripe = match seat.last {
                Some(Outcome::Idle { until, .. }) => !seat.spent && ripe(clock, group, until),
                _ => false,
            };
            seat.seen = group.station.epoch();
            top = group.pass_top();
        }
        let result = match top {
            Ok(None) if fresh && cut == Cut::Drained => {
                seat.at = Some(cut);
                self.end.iter_mut().for_each(RunEnd::drain_logs);
                return Ok(None);
            }
            Ok(None) => group.pass_rest(),
            Ok(Some(over)) => Ok(over),
            Err(e) => Err(e),
        };
        if let Ok(Outcome::Idle { until, within }) = result {
            let within = u64::try_from(within.as_nanos()).map_or(Timestamp::NONE, Timestamp);
            seat.within_at = clock.now().saturating_add(within);
            seat.spent = seat.ripe && ripe(clock, group, until);
        }
        let result = match result {
            Ok(Outcome::Exit) => self.close(g, None).map_or(Ok(Outcome::Exit), Err),
            Err(e) => self.close(g, Some(e)).map_or(Ok(Outcome::Exit), Err),
            ok => ok,
        };
        if let (Some(slot), Ok(outcome)) = (self.slot(g), &result) {
            self.seats[slot].last = Some(*outcome);
        }
        self.failed = self.failed.take().or_else(|| result.as_ref().err().cloned());
        self.service();
        self.end.iter_mut().for_each(RunEnd::drain_logs);
        result.map(Some)
    }

    fn close(&mut self, g: usize, failed: Option<Error>) -> Option<Error> {
        let slot = self.slot(g)?;
        self.open.remove(slot);
        let mut seat = self.seats.remove(slot);
        let mut group = seat.group.take()?;
        let verdict = judge(&seat.tag, failed.as_ref());
        group.end(verdict);
        drop(group);
        retire_group(&self.control, &seat.tag, failed, verdict)
    }

    /// What the last pass of group `g` returned, `Exit` once it is gone.
    pub fn outcome(&self, g: usize) -> Option<Outcome> {
        match self.slot(g) {
            Some(slot) => self.seats[slot].last,
            None => (g < self.next).then_some(Outcome::Exit),
        }
    }

    pub fn held(&self, g: usize) -> usize {
        self.slot(g).and_then(|s| self.seats[s].group.as_ref()).map_or(0, Group::held)
    }

    pub fn control(&self) -> &Control {
        &self.control
    }

    /// End the run and return the first element error, as `run()` does.
    pub fn finish(mut self) -> Result<RunReport, Error> {
        self.close_all();
        let Some(end) = self.end.take() else {
            return Ok(RunReport::default());
        };
        let (report, result) = end.finish(self.failed.take());
        result.map(|()| report)
    }

    fn close_all(&mut self) {
        while let Some(g) = self.open.last().copied() {
            self.close(g, None);
        }
    }
}

impl Drop for Stepped {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.close_all();
        }
    }
}

#[cfg(test)]
// Test fixtures and assertions allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    use crate::buffer::{Buffer, BufferFlags};
    use crate::bus::Bus;
    use crate::clock::MockClock;
    use crate::command::{ALL, COMMAND_LOG_CAP, CommandError};
    use crate::element::{ElementDesc, InputPolicy, LatencyDesc, PadDesc, SchedHint};
    use crate::format::{
        ConstraintSpec, FieldSpec, Interned, OfferDesc, OfferSpec, Value, ValueDesc,
    };
    use crate::id::{FieldId, FormatId, Interner};
    use crate::io::NoBackend;
    use crate::pipeline::{Pipeline, SchedulerStats};
    use crate::ring::{spsc, spsc_leaky};

    static ANY: [OfferDesc; 1] = [OfferDesc::any("bytes")];

    const fn pad(name: &'static str) -> PadDesc {
        PadDesc { name, offers: &ANY }
    }

    type Pads = &'static [PadDesc];

    const fn desc(name: &'static str, sinks: Pads, srcs: Pads) -> ElementDesc {
        sched_desc(name, sinks, srcs, SchedHint::Passive)
    }

    const fn active(name: &'static str, sinks: Pads, srcs: Pads) -> ElementDesc {
        sched_desc(name, sinks, srcs, SchedHint::Active)
    }

    const fn sched_desc(
        name: &'static str,
        sinks: Pads,
        srcs: Pads,
        sched: SchedHint,
    ) -> ElementDesc {
        ElementDesc {
            name,
            sinks,
            srcs,
            props: &[],
            sched,
            inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
            latency: LatencyDesc {
                min: Timestamp::ZERO,
                max: Timestamp::ZERO,
                is_live: false,
                jitter: Timestamp::ZERO,
            },
            make_default: None,
            io: false,
            couplings: &[],
        }
    }

    static ONE_SINK: [PadDesc; 1] = [pad("sink")];
    static ONE_SRC: [PadDesc; 1] = [pad("src")];
    static TWO_SINKS: [PadDesc; 2] = [pad("a"), pad("b")];
    static TWO_SRCS: [PadDesc; 2] = [pad("x"), pad("y")];
    static SRC: ElementDesc = desc("src", &[], &ONE_SRC);
    static SRC2: ElementDesc = desc("src2", &[], &TWO_SRCS);
    static FILTER: ElementDesc = desc("filter", &ONE_SINK, &ONE_SRC);
    static SINK: ElementDesc = desc("sink", &ONE_SINK, &[]);
    static MUX: ElementDesc = desc("mux", &TWO_SINKS, &ONE_SRC);
    static A_FILTER: ElementDesc = active("filter", &ONE_SINK, &ONE_SRC);
    static A_SINK: ElementDesc = active("sink", &ONE_SINK, &[]);
    static A_MUX: ElementDesc = active("mux", &TWO_SINKS, &ONE_SRC);
    static A_TEE: ElementDesc = active("tee", &ONE_SINK, &TWO_SRCS);
    const IN: SinkPad = SinkPad(0);
    const OUT: SrcPad = SrcPad(0);
    const MUX_B: SinkPad = SinkPad(1);
    const TEE_Y: SrcPad = SrcPad(1);
    static GAIN: [crate::element::PropDesc; 1] = [crate::element::PropDesc {
        name: "gain",
        allowed: crate::format::Constraint::Any,
        live: true,
    }];
    static A_KNOB: ElementDesc = ElementDesc { props: &GAIN, ..active("knob", &ONE_SINK, &[]) };

    #[derive(Default)]
    struct Seen {
        calls: AtomicU32,
        buffers: AtomicU32,
        flushes: AtomicU32,
        eos: AtomicU32,
        order: AtomicU32,
        paused: AtomicU32,
        resumed: AtomicU32,
        props: AtomicU32,
        stops: AtomicU32,
        log: std::sync::Mutex<Vec<Saw>>,
        renders: std::sync::Mutex<Vec<(Timestamp, Timestamp)>>,
        targets: std::sync::Mutex<Vec<(u64, Timestamp)>>,
        reseek: std::sync::Mutex<Option<(Arc<Control>, Command)>>,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum Saw {
        Flush,
        Paused,
        Resumed,
        Skip(Timestamp),
        Rate(u16, u16),
        Offset(i64),
        Format(i64),
        Tags(String),
        Buffer,
        Positional,
        Eos,
    }

    fn n(a: &AtomicU32) -> u32 {
        a.load(Ordering::Relaxed)
    }

    #[derive(Debug)]
    struct StartRefused;

    impl std::fmt::Display for StartRefused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("start refused")
        }
    }

    impl std::error::Error for StartRefused {}

    struct Probe {
        desc: &'static ElementDesc,
        seen: Arc<Seen>,
        stamp: Arc<AtomicU32>,
        emit: Option<u32>,
        reload: Option<u32>,
        sent: u64,
        pts_step: Timestamp,
        outs: &'static [u32],
        reads: &'static [u32],
        bounded: bool,
        wake_at: Option<Timestamp>,
        slow: Duration,
        paced: bool,
        pump: Option<Duration>,
        announce: Option<&'static str>,
        rates: &'static [(u64, i64)],
        log: bool,
        polite: bool,
        sip: bool,
        fail_start: bool,
        gate: Option<Arc<AtomicBool>>,
        backlog: u32,
        tail: u32,
        lay: Option<fn(u64) -> (u32, Timestamp)>,
    }

    fn probe(desc: &'static ElementDesc) -> Probe {
        Probe {
            desc,
            seen: Arc::default(),
            stamp: Arc::default(),
            emit: None,
            reload: None,
            sent: 0,
            pts_step: Timestamp::ZERO,
            outs: &[],
            reads: &[],
            bounded: false,
            wake_at: None,
            slow: Duration::ZERO,
            paced: false,
            pump: None,
            announce: None,
            rates: &[],
            log: false,
            sip: false,
            polite: false,
            fail_start: false,
            gate: None,
            backlog: 0,
            tail: 0,
            lay: None,
        }
    }

    fn source(buffers: u32) -> Probe {
        Probe { emit: Some(buffers), ..probe(&SRC) }
    }

    impl Probe {
        fn mark_order(&self) {
            let at = self.stamp.fetch_add(1, Ordering::Relaxed) + 1;
            self.seen.order.store(at, Ordering::Relaxed);
        }

        fn saw(&self, what: Saw) {
            if self.log {
                self.seen.log.lock().unwrap().push(what);
            }
        }

        fn take(&mut self, ctx: &mut Ctx, buf: Buffer) {
            self.seen.buffers.fetch_add(1, Ordering::Relaxed);
            self.saw(Saw::Buffer);
            if !self.slow.is_zero() {
                std::thread::sleep(self.slow);
            }
            for &extra in self.outs.iter().skip(1) {
                let fresh = ctx.alloc();
                ctx.out(SrcPad(extra)).push(fresh);
            }
            if let Some(&first) = self.outs.first() {
                ctx.out(SrcPad(first)).push(buf);
            }
        }

        fn emit(&mut self, ctx: &mut Ctx) -> Result<Flow, Error> {
            match self.emit {
                None => return Ok(Flow::Ok),
                Some(0) => return Ok(Flow::Eos),
                Some(_) => {}
            }
            if self.polite && !ctx.room(OUT) {
                return Ok(Flow::Ok);
            }
            if let Some(lay) = self.lay {
                let (pad, pts) = lay(self.sent);
                let mut buf = ctx.alloc();
                buf.memory.set_len(8);
                (buf.pts, buf.duration) = (pts, MS(10));
                ctx.out(SrcPad(pad)).push(buf);
                self.sent += 1;
                self.emit = self.emit.map(|left| left - 1);
                return Ok(Flow::Ok);
            }
            let outs: &[u32] = if self.outs.is_empty() { &[0] } else { self.outs };
            let mut bufs = Vec::with_capacity(outs.len());
            for &pad in outs {
                let buf = match self.bounded {
                    true => ctx.try_alloc(),
                    false => Some(ctx.alloc()),
                };
                let Some(buf) = buf else { return Ok(Flow::Ok) };
                bufs.push((pad, buf));
            }
            for &(_, rate) in self.rates.iter().filter(|(at, _)| *at == self.sent) {
                ctx.announce_format(OUT, "audio/raw", &[("rate", ValueDesc::Int(rate))]);
            }
            for (pad, mut buf) in bufs {
                if self.pts_step != Timestamp::ZERO {
                    buf.pts = Timestamp(self.sent * self.pts_step.0);
                }
                ctx.out(SrcPad(pad)).push(buf);
            }
            self.sent += 1;
            self.emit = self.emit.map(|left| left - 1);
            if let Some(family) = self.announce.take() {
                ctx.announce_format(OUT, family, &[]);
            }
            Ok(Flow::Ok)
        }
    }

    impl Element for Probe {
        fn desc(&self) -> &'static ElementDesc {
            self.desc
        }
        fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
            if self.fail_start {
                return Err(Error::element_failed(ctx.element(), StartRefused));
            }
            Ok(())
        }
        fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
            self.seen.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(at) = self.wake_at {
                ctx.wake_at(at);
            }
            if let Some(interval) = self.pump {
                ctx.wake_within(interval);
            }
            if self.gate.as_ref().is_some_and(|open| !open.load(Ordering::Acquire)) {
                return Ok(Flow::Ok);
            }
            for &pad in self.reads {
                let mut batch = ctx.take_input_on(SinkPad(pad));
                while let Some(buf) = batch.pop_front() {
                    self.take(ctx, buf);
                }
                ctx.recycle_input(batch);
            }
            while let Some(front) = inputs.front() {
                if self.paced {
                    let pts = front.pts;
                    if ctx.before_presentation(pts, Timestamp::ZERO) {
                        ctx.count_drop();
                        inputs.pop();
                        continue;
                    }
                    if !ctx.due(pts) {
                        break;
                    }
                    self.seen.renders.lock().unwrap().push((pts, ctx.now()));
                }
                let Some(buf) = inputs.pop() else { break };
                self.take(ctx, buf);
                if self.sip {
                    break;
                }
            }
            if self.backlog > 0 {
                if let Some(buf) = ctx.try_alloc() {
                    ctx.out(OUT).push(buf);
                    self.backlog -= 1;
                }
                if self.backlog > 0 {
                    ctx.hold_end();
                }
            }
            self.emit(ctx)
        }
        fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
            match event {
                Event::FlushStart => self.saw(Saw::Flush),
                Event::Paused => self.saw(Saw::Paused),
                Event::Resumed => self.saw(Saw::Resumed),
                Event::SkippedTo { to_time } => self.saw(Saw::Skip(*to_time)),
                Event::RateAsked { rate } => self.saw(Saw::Rate(rate.num(), rate.den())),
                Event::Offset { by } => self.saw(Saw::Offset(*by)),
                Event::FormatChange(f) => self.saw(Saw::Format(rate_of(f))),
                Event::Tags(t) => self.saw(Saw::Tags(t.get("TITLE").unwrap_or("").into())),
                Event::Segment { .. } | Event::Gap { .. } | Event::Patch { .. } => {
                    self.saw(Saw::Positional)
                }
                Event::Eos => self.saw(Saw::Eos),
                _ => {}
            }
            let counter = match event {
                Event::FlushStart => {
                    let target = ctx.seek_target().map(|t| (t.to_byte, t.at));
                    self.seen.targets.lock().unwrap().extend(target);
                    if let Some((control, cmd)) = self.seen.reseek.lock().unwrap().take() {
                        match cmd {
                            Command::Seek { to_time, .. } => {
                                control.seek_bytes(to_time, &[(ALL, 20, to_time)]).unwrap()
                            }
                            _ => control.append(cmd).unwrap(),
                        };
                    }
                    if self.reload.is_some() {
                        (self.emit, self.sent) = (self.reload, 0);
                    }
                    &self.seen.flushes
                }
                Event::Paused => &self.seen.paused,
                Event::Resumed => &self.seen.resumed,
                Event::PropChanged { .. } => &self.seen.props,
                Event::Eos => {
                    self.mark_order();
                    (self.backlog, self.tail) = (self.tail, 0);
                    &self.seen.eos
                }
                _ => return Ok(()),
            };
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn stop(&mut self, _ctx: &mut Ctx) {
            self.mark_order();
            self.seen.stops.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Rig {
        group: Group,
        bus: Bus,
        pool: Pool,
        control: Arc<Control>,
        clock: MockClock,
        station: Arc<Station>,
        up_peers: Vec<Arc<Station>>,
        down_peers: Vec<Arc<Station>>,
        seen: Vec<Arc<Seen>>,
        up: Vec<(Producer<Batch>, Consumer<Batch>)>,
        down: Vec<(Consumer<Batch>, Producer<Batch>)>,
    }

    fn vocabulary() -> Arc<Vocabulary> {
        let (mut formats, mut fields, values) = (Interner::new(), Interner::new(), Interner::new());
        formats.intern("bytes");
        formats.intern("audio/raw");
        fields.intern("rate");
        Arc::new(Vocabulary { formats, fields, values })
    }

    fn rig(probes: Vec<Probe>, ups: &[u32], downs: &[u32]) -> Rig {
        rig_with(probes, ups, downs, 64, Leaky::Block)
    }

    fn rig_with(probes: Vec<Probe>, ups: &[u32], downs: &[u32], slots: u32, leaky: Leaky) -> Rig {
        rig_sized(probes, ups, downs, slots, leaky, 4)
    }

    fn rig_sized(
        mut probes: Vec<Probe>,
        ups: &[u32],
        downs: &[u32],
        slots: u32,
        leaky: Leaky,
        cap: usize,
    ) -> Rig {
        let stamp: Arc<AtomicU32> = Arc::default();
        for p in probes.iter_mut() {
            p.stamp = Arc::clone(&stamp);
        }
        let seen: Vec<Arc<Seen>> = probes.iter().map(|p| Arc::clone(&p.seen)).collect();
        let m = probes.len();
        let pad_counts: Vec<(usize, usize)> =
            probes.iter().map(|p| (p.desc.srcs.len(), p.desc.sinks.len())).collect();
        let station = Arc::new(Station::new());
        let (mut upstream, mut up, mut up_peers) = (Vec::new(), Vec::new(), Vec::new());
        for &pad in ups {
            let (data_tx, data) = spsc::<Batch>(4);
            let (shells, shell_rx) = spsc::<Batch>(4);
            let (peer, room) = (Arc::new(Station::new()), Arc::new(Room::new()));
            up_peers.push(Arc::clone(&peer));
            upstream.push(UpLink::new(
                SinkPad(pad),
                data,
                shells,
                peer,
                room,
                Arc::default(),
                Arc::default(),
            ));
            up.push((data_tx, shell_rx));
        }
        let (mut downstream, mut down, mut down_peers) = (Vec::new(), Vec::new(), Vec::new());
        for &pad in downs {
            let (data, data_rx) = spsc_leaky::<Batch>(cap, leaky);
            let (shell_tx, shells) = spsc::<Batch>(cap);
            let (peer, room) = (Arc::new(Station::new()), Arc::new(Room::new()));
            down_peers.push(Arc::clone(&peer));
            let pad = SrcPad(pad);
            let link = LinkId(downstream.len() as u32);
            let (level, slot) = (Arc::default(), Arc::default());
            downstream.push(DownLink {
                pad,
                link,
                data,
                shells,
                leaky,
                peer,
                room,
                staged: None,
                level,
                limited: false,
                base: Timestamp::NONE,
                sparse: false,
                mark: None,
                slot,
                seen: 0,
            });
            down.push((data_rx, shell_tx));
        }
        let base = Arc::new(AtomicI64::new(0));
        let control = Control::new(Arc::clone(&base));
        let clock = MockClock::new();
        let (run_clock, terminal) = (Arc::new(clock.clone()), [downs.is_empty()]);
        let (cursors, seek_gen) =
            control.begin_run(run_clock, vec![Arc::clone(&station)], &terminal, false);
        let mut all = vec![Arc::clone(&station)];
        all.extend(up_peers.iter().chain(&down_peers).cloned());
        control.install_stations(all);
        clock.attach(crate::clock::GroupWake::new(&control));
        let pool = Pool::bounded(64, slots);
        let (bus_tx, bus) = Bus::channel();
        let setup = GroupSetup {
            linked_pads: pad_counts.iter().map(|&(nsrcs, _)| vec![true; nsrcs]).collect(),
            path_latencies: vec![Timestamp::ZERO; m],
            pad_counts,
            shared: GroupShared {
                vocabulary: vocabulary(),
                clock: Arc::new(clock.clone()),
                base,
                tracing: Arc::new(AtomicBool::new(false)),
                wall: Some(Instant::now()),
                control: Arc::clone(&control),
            },
            cursor: Arc::clone(&cursors[0]),
            seek_gen,
            station: Arc::clone(&station),
            backstop: PARK_BACKSTOP,
            props: (0..m).map(|_| Arc::new(PropTable::new(0))).collect(),
            counters: (0..m).map(|_| Arc::default()).collect(),
            credits: vec![1; m],
            bus: bus_tx,
            pools: vec![pool.clone(); m],
            factory: Arc::new(|| Ok(Box::<NoBackend>::default() as Box<dyn Reactor>)),
            downstream,
            upstream,
            logs: (0..m).map(|_| None).collect(),
            formats: (0..m).map(|_| PadFormats::default()).collect(),
            ids: (0..m as u32).map(ElementId).collect(),
            elements: probes.into_iter().map(|p| Box::new(p) as Box<dyn Element>).collect(),
            stream_pads: (0..m).map(|_| Vec::new()).collect(),
            join: None,
        };
        let group = match Group::build(setup) {
            Ok(g) => g,
            Err(e) => panic!("build: {e:?}"),
        };
        Rig { group, bus, pool, control, clock, station, up_peers, down_peers, seen, up, down }
    }

    fn buf(pool: &Pool) -> Buffer {
        let mut memory = pool.acquire();
        memory.set_len(8);
        Buffer {
            memory,
            pts: Timestamp::NONE,
            dts: Timestamp::NONE,
            duration: Timestamp::NONE,
            flags: BufferFlags::empty(),
            format: BYTES,
            sync: None,
        }
    }

    fn batch(pool: &Pool, buffers: usize) -> Batch {
        let mut b = Batch::new(BYTES);
        for _ in 0..buffers {
            b.push(buf(pool));
        }
        b
    }

    fn mark() -> Batch {
        let mut b = Batch::new(BYTES);
        b.eos = true;
        b
    }

    fn send(r: &Rig, cmd: Command) -> u64 {
        r.control.append(cmd).expect("the command enters the log")
    }

    fn seek_to(r: &Rig, to_byte: u64, to_time: Timestamp) -> u64 {
        r.control
            .seek_bytes(to_time, &[(ALL, to_byte, to_time)])
            .expect("the command enters the log")
    }

    fn seek(r: &Rig) {
        seek_to(r, 0, Timestamp::ZERO);
    }

    fn targets(r: &Rig, element: usize) -> Vec<(u64, Timestamp)> {
        r.seen[element].targets.lock().unwrap().clone()
    }

    fn audio(rate: i64) -> FixedFormat {
        let v = vocabulary();
        let mut f = FixedFormat::new(v.family_id("audio/raw").unwrap());
        f.set(v.field_id("rate").unwrap(), Value::Int(rate));
        f
    }

    fn rate_of(f: &FixedFormat) -> i64 {
        match f.get(vocabulary().field_id("rate").unwrap()) {
            Some(Value::Int(rate)) => rate,
            _ => -1,
        }
    }

    fn format(rate: i64) -> Event {
        Event::FormatChange(audio(rate))
    }

    fn tags(title: &str) -> Event {
        let mut t = crate::event::TagList::new();
        t.add("TITLE", title);
        Event::Tags(t)
    }

    fn loaded(pool: &Pool, rate: i64) -> Batch {
        let mut b = Batch::new(BYTES);
        b.push_event(tags("a"));
        b.push_event(Event::Segment { base: Timestamp::ZERO, rate: 1.0 });
        b.push(buf(pool));
        b.push_event(format(rate));
        b.push_event(Event::Gap { until: Timestamp::from_millis(5) });
        b.push(buf(pool));
        b.push_event(Event::Patch { offset: 0, data: pool.acquire() });
        b.push_event(tags("b"));
        b.push_event(Event::Eos);
        b.eos = true;
        b
    }

    fn kept(rate: i64) -> [Saw; 3] {
        [Saw::Tags("a".into()), Saw::Format(rate), Saw::Tags("b".into())]
    }

    fn log(r: &Rig, element: usize) -> Vec<Saw> {
        r.seen[element].log.lock().unwrap().clone()
    }

    fn negotiated_rate(r: &Rig, element: usize, pad: u32) -> Option<i64> {
        r.group.ctxs[element].negotiated_sink(SinkPad(pad)).map(rate_of)
    }

    fn announced_rate(r: &Rig) -> Option<i64> {
        r.group.ctxs[0].negotiated_src(OUT).map(rate_of)
    }

    fn ferry(from: &Rig, to: &Rig, count: usize) -> usize {
        let mut moved = 0;
        while moved < count {
            let Some(b) = from.down[0].0.try_pop() else {
                break;
            };
            assert!(to.up[0].0.try_push(b).is_ok(), "the far ring has room");
            moved += 1;
        }
        moved
    }

    #[test]
    fn ingest_drops_a_stale_batch_and_recycles_its_buffers() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        r.group.seek_gen = 2;
        let mut stale = batch(&r.pool, 2);
        stale.seek_gen = 1;
        stale.eos = true;
        assert_eq!(r.pool.stats().outstanding, 2);
        assert!(!r.group.ingest(0, stale).unwrap());
        assert_eq!(r.pool.stats().outstanding, 0, "its buffers went back to the pool");
        assert!(r.group.ctxs[0].inputs_empty());
        assert_eq!(r.group.counters[0].snapshot().batches_in, 0);
        assert!(!r.group.upstream[0].ended && !r.group.head_done, "a stale mark ends nothing");
        assert!(r.up[0].1.try_pop().is_some(), "a stale shell goes back to the producer too");
    }

    #[test]
    fn ingest_counts_data_and_returns_the_drained_shell() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        r.group.seek_gen = 2;
        let mut fresh = batch(&r.pool, 3);
        fresh.seek_gen = 2;
        assert!(r.group.ingest(0, fresh).unwrap());
        let c = r.group.counters[0].snapshot();
        assert_eq!((c.batches_in, c.buffers_in, c.bytes_in), (1, 3, 24));
        assert_eq!(r.group.ctxs[0].input_len(), 3);
        assert!(!r.group.upstream[0].ended && !r.group.head_done);
        let shell = r.up[0].1.try_pop().expect("the shell comes back on the shell ring");
        assert!(shell.is_inert() && !shell.eos);
        let input = r.group.ctxs[0].take_input();
        assert!(
            input.cap_rows() >= 3 && shell.cap_rows() == 0,
            "the rows' storage moved onto the input, the shell has the input's old"
        );
        r.group.ctxs[0].set_input(input);
    }

    #[test]
    fn ingest_latches_the_mark_without_counting_it_as_a_batch() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        assert!(r.group.ingest(0, mark()).unwrap());
        assert!(r.group.upstream[0].ended);
        assert!(r.group.head_done, "a linear head's one pad ended");
        assert_eq!(r.group.counters[0].snapshot().batches_in, 0);
        let shell = r.up[0].1.try_pop().expect("the mark's shell comes back too");
        assert!(!shell.eos, "a recycled shell must not end the next stream");
    }

    #[test]
    fn ingest_on_a_fan_in_head_lands_on_its_pad_and_ends_only_that_pad() {
        let mut r = rig(vec![probe(&MUX)], &[0, 1], &[0]);
        let mut last = batch(&r.pool, 1);
        last.eos = true;
        assert!(r.group.ingest(1, last).unwrap());
        assert!(r.group.upstream[1].ended && !r.group.upstream[0].ended);
        assert!(!r.group.head_done, "the feed step decides over all pads");
        assert_eq!(r.group.ctxs[0].input_len(), 0, "nothing on the primary input");
        assert_eq!(r.group.ctxs[0].take_input_on(SinkPad(1)).len(), 1);
    }

    #[test]
    fn fan_in_feed_closes_ended_pads_and_finishes_when_all_have() {
        let mut r = rig(vec![probe(&MUX)], &[0, 1], &[0]);
        assert!(r.up[0].0.try_push(batch(&r.pool, 1)).is_ok());
        assert!(r.up[1].0.try_push(mark()).is_ok());
        let mut progressed = false;
        r.group.feed(&mut progressed).unwrap();
        assert!(progressed);
        assert!(r.group.ctxs[0].is_pad_closed(SinkPad(1)));
        assert!(!r.group.ctxs[0].is_pad_closed(SinkPad(0)));
        assert!(!r.group.head_done);

        assert!(r.up[0].0.try_push(mark()).is_ok());
        r.group.feed(&mut progressed).unwrap();
        assert!(r.group.ctxs[0].is_pad_closed(SinkPad(0)));
        assert!(r.group.head_done);
    }

    #[test]
    fn linear_feed_stops_at_the_backlog_cap_and_latches_the_mark() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        for _ in 0..3 {
            assert!(r.up[0].0.try_push(batch(&r.pool, 4)).is_ok());
        }
        assert!(r.up[0].0.try_push(mark()).is_ok());
        let mut progressed = false;
        r.group.feed(&mut progressed).unwrap();
        assert!(progressed);
        assert_eq!(r.group.ctxs[0].input_len(), INLINE_INPUT_CAP);
        r.group.feed(&mut progressed).unwrap();
        assert_eq!(r.group.ctxs[0].input_len(), INLINE_INPUT_CAP, "held at the cap");
        assert!(!r.group.head_done);

        r.group.ctxs[0].discard_buffers();
        r.group.feed(&mut progressed).unwrap();
        assert_eq!(r.group.ctxs[0].input_len(), 4);
        assert!(r.group.head_done, "the mark behind the last batch was seen");
    }

    #[test]
    fn a_pop_signals_the_producer_only_when_it_waits_for_room() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        assert!(r.up[0].0.try_push(batch(&r.pool, 1)).is_ok());
        assert!(r.up[0].0.try_push(batch(&r.pool, 1)).is_ok());
        assert!(r.group.pop(0).is_some());
        assert_eq!(r.up_peers[0].epoch(), 0, "nobody waits, nobody is signaled");
        r.group.upstream[0].room.raise();
        assert!(r.group.pop(0).is_some());
        assert_eq!(r.up_peers[0].epoch(), 1);
        assert!(r.group.pop(0).is_none());
        assert_eq!(r.up_peers[0].epoch(), 1, "an empty pop made no room");
        let mut progressed = false;
        r.group.feed(&mut progressed).unwrap();
        assert!(!progressed && !r.group.head_done, "open and empty is just idle");
    }

    #[test]
    fn flush_resets_elements_staged_data_and_end_of_stream_state() {
        let mut r = rig(vec![probe(&FILTER), probe(&SINK)], &[0], &[]);
        assert!(r.group.ingest(0, batch(&r.pool, 2)).unwrap());
        let staged = buf(&r.pool);
        r.group.ctxs[0].out(OUT).push(staged);
        let mut inline = batch(&r.pool, 1);
        r.group.ctxs[1].input_append(&mut inline);
        r.group.phase = Phase::Draining { next: 1 };
        r.group.head_done = true;
        r.group.upstream[0].ended = true;
        r.group.ctxs[0].set_pad_closed(SinkPad(0));
        assert_eq!(r.pool.stats().outstanding, 4);

        r.group.flush().unwrap();

        assert_eq!((n(&r.seen[0].flushes), n(&r.seen[1].flushes)), (1, 1));
        assert_eq!(r.pool.stats().outstanding, 0, "staged buffers are discarded");
        assert!(r.group.ctxs.iter().all(|c| c.inputs_empty()));
        assert_eq!(r.group.phase, Phase::Running);
        assert!(!r.group.head_done);
        assert!(!r.group.upstream[0].ended);
        assert!(!r.group.ctxs[0].is_pad_closed(SinkPad(0)));
    }

    #[test]
    fn a_seek_flushes_once() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        r.group.apply_commands().unwrap();
        assert_eq!(n(&r.seen[0].flushes), 0, "no seek, no flush");
        seek(&r);
        r.group.apply_commands().unwrap();
        r.group.apply_commands().unwrap();
        assert_eq!(n(&r.seen[0].flushes), 1);
        assert_eq!(r.group.seek_gen, 1);
    }

    fn consumer(fan_in: bool) -> (Rig, usize) {
        match fan_in {
            false => (rig(vec![Probe { log: true, ..probe(&SINK) }], &[0], &[]), 0),
            true => {
                let mux = Probe { log: true, reads: &[0, 1], ..probe(&MUX) };
                (rig(vec![mux], &[0, 1], &[]), 1)
            }
        }
    }

    fn post_seek(pool: &Pool) -> Batch {
        let mut b = batch(pool, 1);
        b.seek_gen = 1;
        b
    }

    fn assert_kept(r: &Rig, link: usize, before: &[Saw]) {
        let mut want = before.to_vec();
        want.push(Saw::Flush);
        want.extend(kept(48_000));
        want.push(Saw::Buffer);
        assert_eq!(log(r, 0), want);
        let pad = r.group.upstream[link].pad;
        assert_eq!(negotiated_rate(r, 0, pad.0), Some(48_000), "installed on the sink pad");
        assert!(!r.group.head_done && !r.group.upstream[link].ended, "the mark ended nothing");
        assert_eq!(r.pool.stats().outstanding, 0, "pre-seek buffers and the patch recycled");
    }

    #[test]
    fn a_stale_ring_batch_still_delivers_its_state_events_after_the_flush() {
        for fan_in in [false, true] {
            let (mut r, link) = consumer(fan_in);
            seek(&r);
            assert_eq!(settle(&mut r), IDLE, "flushed, with nothing in the ring yet");
            assert!(r.up[link].0.try_push(loaded(&r.pool, 48_000)).is_ok());
            assert!(r.up[link].0.try_push(post_seek(&r.pool)).is_ok());
            assert_eq!(settle(&mut r), IDLE);
            assert_kept(&r, link, &[]);
            assert_eq!(r.group.counters[0].snapshot().buffers_in, 1, "stale rows are not input");
        }
    }

    #[test]
    fn a_flush_keeps_the_state_events_staged_on_the_heads_input() {
        for fan_in in [false, true] {
            let (mut r, link) = consumer(fan_in);
            assert!(r.group.ingest(link, loaded(&r.pool, 48_000)).unwrap());
            assert!(r.group.upstream[link].ended, "the mark was honoured before the seek");
            seek(&r);
            assert!(r.up[link].0.try_push(post_seek(&r.pool)).is_ok());
            assert_eq!(settle(&mut r), IDLE);
            assert_kept(&r, link, &[]);
        }
    }

    #[test]
    fn a_flush_does_not_redeliver_a_state_event_the_element_already_had() {
        let mut r = rig(vec![Probe { log: true, sip: true, ..probe(&SINK) }], &[0], &[]);
        assert!(r.group.ingest(0, loaded(&r.pool, 48_000)).unwrap());
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        let before = [Saw::Tags("a".into()), Saw::Positional, Saw::Buffer];
        assert_eq!(log(&r, 0), before);
        seek(&r);
        assert!(r.up[0].0.try_push(post_seek(&r.pool)).is_ok());
        assert_eq!(settle(&mut r), IDLE);
        let mut want = before.to_vec();
        want.extend([Saw::Flush, Saw::Format(48_000), Saw::Tags("b".into()), Saw::Buffer]);
        assert_eq!(log(&r, 0), want);
    }

    #[test]
    fn a_flush_keeps_state_events_on_an_inline_hop() {
        for crossed in [true, false] {
            let idle = Probe { emit: None, ..probe(&SRC) };
            let mut r = rig(vec![idle, Probe { log: true, ..probe(&SINK) }], &[], &[]);
            let mut pre = loaded(&r.pool, 48_000);
            match crossed {
                true => r.group.ctxs[1].input_append(&mut pre),
                false => r.group.ctxs[0].output_on(OUT).append(&mut pre),
            }
            seek(&r);
            assert_eq!(settle(&mut r), IDLE);
            let mut want = vec![Saw::Flush];
            want.extend(kept(48_000));
            assert_eq!(log(&r, 1), want, "crossed {crossed}");
            assert_eq!(negotiated_rate(&r, 1, 0), Some(48_000));
            assert_eq!(r.pool.stats().outstanding, 0);
            assert_eq!(r.group.phase, Phase::Running, "no in-band Eos survived");
        }
    }

    fn state_of(b: &mut Batch) -> Vec<Saw> {
        let mut out = Vec::new();
        while let Some(ev) = b.next_event_any() {
            out.push(match ev {
                Event::FormatChange(f) => Saw::Format(rate_of(&f)),
                Event::Tags(t) => Saw::Tags(t.get("TITLE").unwrap().into()),
                _ => Saw::Positional,
            });
        }
        out
    }

    #[test]
    fn a_flush_sends_the_state_events_of_an_output_slot_under_the_new_generation() {
        let mut r = rig(vec![Probe { emit: None, ..probe(&SRC) }], &[], &[0]);
        let mut pre = loaded(&r.pool, 48_000);
        r.group.ctxs[0].output_on(OUT).append(&mut pre);
        seek(&r);
        assert_eq!(settle(&mut r), IDLE);
        let mut out = r.down[0].0.try_pop().expect("the state events still leave");
        assert_eq!((out.seek_gen, out.len(), out.eos), (1, 0, false));
        assert_eq!(state_of(&mut out), kept(48_000));
        assert!(r.down[0].0.try_pop().is_none(), "and only once");
        assert_eq!(r.pool.stats().outstanding, 0);
    }

    #[test]
    fn a_flush_sends_the_state_events_of_a_staged_batch_ahead_of_the_slots() {
        let mut r = rig(vec![Probe { reload: Some(1), ..source(0) }], &[], &[0]);
        r.group.downstream[0].staged = Some(loaded(&r.pool, 48_000));
        let slot = r.group.ctxs[0].output_on(OUT);
        slot.push(buf(&r.pool));
        slot.push_event(format(44_100));
        slot.push_event(Event::Gap { until: Timestamp::ZERO });
        seek(&r);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert!(r.group.downstream[0].staged.is_none(), "the staged batch is gone");

        let (mut far, link) = consumer(false);
        seek(&far);
        assert_eq!(ferry(&r, &far, 4), 1, "state and post-seek data share one batch");
        assert_eq!(settle(&mut far), IDLE);
        let mut want = vec![Saw::Flush];
        want.extend(kept(48_000));
        want.extend([Saw::Format(44_100), Saw::Buffer]);
        assert_eq!(log(&far, 0), want, "older first, then the reloaded source's buffer");
        assert_eq!(negotiated_rate(&far, 0, 0), Some(44_100));
        assert!(!far.group.upstream[link].ended, "the staged mark was pre-seek");
        assert_eq!(r.pool.stats().outstanding, 0, "no pre-seek buffer was kept anywhere");
    }

    #[test]
    fn a_flush_moves_a_pending_announcement_ahead_of_post_seek_output() {
        let (mut ctx, _bus, vocabulary) = step_ctx();
        for _ in 0..2 {
            let row = ctx.alloc();
            ctx.out(OUT).push(row);
        }
        ctx.announce_format(OUT, "audio/raw", &[("rate", ValueDesc::Int(48_000))]);
        ctx.discard_buffers();
        let row = ctx.alloc();
        ctx.out(OUT).push(row);
        resolve_announcements(&mut ctx, &vocabulary, |ctx, pad, at, f| {
            ctx.output_on(pad).push_event_at(at, Event::FormatChange(f));
        });
        let out = ctx.output_on(OUT);
        assert!(out.pop_front().is_none(), "the format precedes the first post-seek row");
        assert!(matches!(out.due_event(), Some(Event::FormatChange(_))));
        assert!(out.pop_front().is_some());
    }

    #[test]
    fn announcing_emits_only_when_the_src_pads_format_changes() {
        let (mut ctx, _bus, vocabulary) = step_ctx();
        let mut emitted = Vec::new();
        let mut announce = |ctx: &mut Ctx, pad: u32, rate: i64| {
            ctx.announce_format(SrcPad(pad), "audio/raw", &[("rate", ValueDesc::Int(rate))]);
            resolve_announcements(ctx, &vocabulary, |_, pad, _, f| {
                emitted.push((pad.0, rate_of(&f)));
            });
        };
        announce(&mut ctx, 0, 48_000);
        announce(&mut ctx, 0, 48_000);
        announce(&mut ctx, 0, 44_100);
        announce(&mut ctx, 0, 44_100);
        announce(&mut ctx, 0, 48_000);
        announce(&mut ctx, 1, 48_000);
        assert_eq!(emitted, [(0, 48_000), (0, 44_100), (0, 48_000), (1, 48_000)]);
        assert_eq!(ctx.negotiated_src(OUT).map(rate_of), Some(48_000), "the producer's record");
        assert!(ctx.negotiated_sink(IN).is_none(), "and nothing on the sink pad of that index");
    }

    #[test]
    fn forwarding_the_format_a_link_already_has_emits_nothing() {
        let (mut ctx, _bus, vocabulary) = step_ctx();
        let (rate, channels) = (FieldId(0), FieldId(5));
        let build = |order: [(FieldId, i64); 2]| {
            let mut f = FixedFormat::new(FormatId(1));
            for (field, value) in order {
                f.set(field, Value::Int(value));
            }
            f
        };
        let fixed = build([(rate, 48_000), (channels, 2)]);
        let srcs = vec![None, Some(fixed.clone())];
        ctx.set_negotiated(PadFormats { sinks: vec![None], srcs, ..Default::default() });
        let mut emitted = 0;
        let mut forward = |ctx: &mut Ctx, pad: u32, f: FixedFormat| {
            ctx.forward_format(SrcPad(pad), f);
            resolve_announcements(ctx, &vocabulary, |_, _, _, _| emitted += 1);
            emitted
        };
        assert_eq!(forward(&mut ctx, 1, fixed.clone()), 0, "the link-time format is current");
        assert_eq!(forward(&mut ctx, 0, fixed.clone()), 1);
        assert_eq!(forward(&mut ctx, 0, fixed), 1);
        let reordered = build([(channels, 2), (rate, 48_000)]);
        assert_eq!(forward(&mut ctx, 0, reordered.clone()), 1, "equal up to field order");
        assert_eq!(forward(&mut ctx, 1, reordered), 1);
        assert_eq!(forward(&mut ctx, 0, build([(channels, 1), (rate, 48_000)])), 2);
    }

    #[test]
    fn a_source_announcing_every_buffer_sends_one_format_change_per_change() {
        let src =
            Probe { rates: &[(0, 48_000), (1, 48_000), (2, 44_100), (3, 44_100)], ..source(4) };
        let mut r = rig(vec![src], &[], &[0]);
        let mut sent = Vec::new();
        for _ in 0..4 {
            assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
            let mut out = r.down[0].0.try_pop().expect("one buffer per pass");
            sent.push(state_of(&mut out));
        }
        let none = Vec::new();
        let want = [vec![Saw::Format(48_000)], none.clone(), vec![Saw::Format(44_100)], none];
        assert_eq!(sent, want);
    }

    #[test]
    fn a_seek_before_the_first_output_is_consumed_does_not_lose_a_latched_format() {
        let src = Probe { announce: Some("audio/raw"), reload: Some(1), ..source(1) };
        let mut p = rig(vec![src], &[], &[0]);
        let (mut c, _) = consumer(false);
        assert_eq!(settle(&mut p), IDLE, "one buffer with its format, then the mark");
        seek(&p);
        seek(&c);
        assert_eq!(settle(&mut p), IDLE, "the same again, with no second announcement");
        assert_eq!(ferry(&p, &c, 4), 4);
        assert_eq!(settle(&mut c), IDLE, "at rest at its end");
        assert_eq!(log(&c, 0), [Saw::Flush, Saw::Format(-1), Saw::Buffer, Saw::Eos]);
        let audio = c.group.shared.vocabulary.family_id("audio/raw");
        assert_eq!(c.group.ctxs[0].negotiated_sink(IN).map(|f| f.family), audio);
    }

    #[test]
    fn a_seek_back_across_a_format_change_ends_on_the_earlier_format() {
        const A: i64 = 48_000;
        const B: i64 = 44_100;
        static DELIVERED: [(u64, i64); 2] = [(0, A), (1, B)];
        static IN_RING: [(u64, i64); 2] = [(0, A), (2, B)];
        static STAGED: [(u64, i64); 2] = [(0, A), (4, B)];
        for (rates, consumed) in [(&DELIVERED, 2), (&IN_RING, 1), (&STAGED, 1)] {
            let src = Probe { rates: &rates[..], reload: Some(1), ..source(100) };
            let mut p = rig(vec![src], &[], &[0]);
            let (mut c, _) = consumer(false);
            assert_eq!(settle(&mut p), ring(0), "four batches in the ring and one staged");
            assert_eq!(ferry(&p, &c, consumed), consumed);
            assert_eq!(settle(&mut c), IDLE);
            let before = log(&c, 0);
            assert_eq!(before.contains(&Saw::Format(B)), consumed == 2);

            seek(&p);
            seek(&c);
            assert_eq!(p.group.pass().unwrap(), Outcome::Progressed);
            assert_eq!(announced_rate(&p), Some(A), "the producer's record follows");
            ferry(&p, &c, 4);
            settle(&mut c);

            let after = &log(&c, 0)[before.len()..];
            let want: &[Saw] = match consumed {
                2 => &[Saw::Flush, Saw::Format(A), Saw::Buffer],
                _ => &[Saw::Flush, Saw::Format(B), Saw::Format(A), Saw::Buffer],
            };
            assert_eq!(&after[..want.len()], want, "consumed {consumed}");
            assert_eq!(negotiated_rate(&c, 0, 0), Some(A));
        }
    }

    fn eos_counts(r: &Rig) -> Vec<u32> {
        r.seen.iter().map(|s| n(&s.eos)).collect()
    }

    fn finish_head(r: &mut Rig) {
        let mut progressed = false;
        r.group.run_members(&mut progressed).unwrap();
        assert!(r.group.head_done, "the source returned Flow::Eos");
    }

    #[test]
    fn eos_reaches_each_element_once_in_order_then_the_mark_goes_out() {
        let mut r = rig(vec![source(0), probe(&FILTER), probe(&FILTER)], &[], &[0]);
        assert!(!r.group.terminate().unwrap(), "head not done yet");
        finish_head(&mut r);
        for k in 1..=3usize {
            assert!(r.group.terminate().unwrap());
            assert_eq!(r.group.phase, Phase::Draining { next: k }, "one Eos per pass");
            let want: Vec<u32> = (0..3).map(|j| (j < k) as u32).collect();
            assert_eq!(eos_counts(&r), want);
        }
        assert!(r.down[0].0.try_pop().is_none(), "no mark before the last Eos");
        assert!(r.group.terminate().unwrap());
        assert_eq!(r.group.phase, Phase::Ended);
        assert_eq!(eos_counts(&r), [1, 1, 1]);
        let order: Vec<u32> = r.seen.iter().map(|s| n(&s.order)).collect();
        assert_eq!(order, [1, 2, 3], "chain order");
        let mark = r.down[0].0.try_pop().expect("the consumer holds the mark");
        assert!(mark.eos && mark.is_empty());
        assert!(r.down[0].0.try_pop().is_none(), "exactly one mark");
    }

    #[test]
    fn an_element_that_holds_output_holds_the_end_until_it_is_out() {
        let holder = Probe { backlog: 3, tail: 2, ..probe(&FILTER) };
        let forward = Probe { outs: &[0], ..probe(&FILTER) };
        let mut r = rig_with(vec![source(0), holder, forward], &[], &[0], 2, Leaky::Block);
        let mut got = Vec::new();
        let mut eos_at = Vec::new();
        for _ in 0..40 {
            let outcome = r.group.pass().unwrap();
            if let Some(mut batch) = r.down[0].0.try_pop() {
                got.push((batch.len(), batch.eos));
                assert!(batch.len() <= 2, "no more than the pool holds, {}", batch.len());
                while batch.pop_front().is_some() {}
            }
            eos_at.push((got.iter().map(|(n, _)| n).sum::<usize>(), eos_counts(&r)));
            if outcome == Outcome::Exit || r.group.phase == Phase::Ended {
                break;
            }
        }
        while let Some(batch) = r.down[0].0.try_pop() {
            got.push((batch.len(), batch.eos));
        }
        let buffers: usize = got.iter().map(|(n, _)| n).sum();
        assert_eq!(buffers, 5, "every buffer it held, {got:?}");
        let marks: Vec<&(usize, bool)> = got.iter().filter(|(_, eos)| *eos).collect();
        assert_eq!(marks.len(), 1, "one mark, {got:?}");
        assert_eq!(got.last(), Some(&(0, true)), "and it is last, behind the buffers, {got:?}");
        let first = |element: usize| {
            eos_at.iter().find(|(_, eos)| eos[element] == 1).expect("it had Eos").0
        };
        assert!(first(1) >= 3, "the holder had Eos with {} buffers out, {eos_at:?}", first(1));
        assert!(first(2) >= 5, "the next element had Eos with {} out, {eos_at:?}", first(2));
        assert_eq!(eos_counts(&r), [1, 1, 1]);
    }

    #[test]
    fn pending_input_holds_the_terminus() {
        let mut r = rig(vec![source(0), probe(&FILTER)], &[], &[0]);
        finish_head(&mut r);
        let mut pending = batch(&r.pool, 1);
        r.group.ctxs[1].input_append(&mut pending);
        assert!(!r.group.terminate().unwrap());
        assert_eq!(r.group.phase, Phase::Running);
        assert_eq!(eos_counts(&r), [0, 0]);
    }

    #[test]
    fn a_seek_revives_a_draining_and_an_ended_group() {
        let mut r = rig(vec![source(0), probe(&FILTER)], &[], &[0]);
        finish_head(&mut r);
        assert!(r.group.terminate().unwrap());
        assert_eq!(r.group.phase, Phase::Draining { next: 1 });
        seek(&r);
        r.group.apply_commands().unwrap();
        assert_eq!(r.group.phase, Phase::Running);
        assert!(!r.group.head_done);

        finish_head(&mut r);
        for _ in 0..3 {
            assert!(r.group.terminate().unwrap());
        }
        assert_eq!(r.group.phase, Phase::Ended);
        assert_eq!(eos_counts(&r), [2, 1], "Eos is delivered again after the revival");
        let mark = r.down[0].0.try_pop().expect("mark");
        assert_eq!(mark.seek_gen, 1, "stamped with the generation it ended under");
        seek(&r);
        r.group.apply_commands().unwrap();
        assert_eq!(r.group.phase, Phase::Running);
        assert!(!r.group.head_done);
        assert_eq!(r.group.seek_gen, 2);
    }

    #[test]
    fn a_terminal_group_rests_at_its_terminus_and_the_decision_ends_it() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        assert!(r.group.ingest(0, mark()).unwrap());
        assert!(r.group.terminate().unwrap());
        assert_eq!((r.group.phase, eos_counts(&r)), (Phase::Draining { next: 1 }, vec![1]));
        assert!(r.group.terminate().unwrap());
        assert_eq!(r.group.phase, Phase::Ended, "it rests like any other group");
        assert!(!r.control.is_finished(), "ending is not the decision");
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert!(r.control.is_finished() && r.control.is_stopped());
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
        assert_eq!(n(&r.seen[0].stops), 0, "stopping is the driver's teardown");
    }

    #[cfg(not(loom))]
    #[test]
    fn a_resting_sink_that_finds_the_writer_lock_held_runs_again() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        assert!(r.group.ingest(0, mark()).unwrap());
        assert!(r.group.terminate().unwrap() && r.group.terminate().unwrap());
        let control = Arc::clone(&r.control);
        assert_eq!(control.while_appending(|| r.group.pass().unwrap()), Outcome::Progressed);
        assert!(!control.is_finished());
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert!(control.is_finished());
    }

    #[test]
    fn a_group_that_is_not_terminal_never_asks_for_the_end() {
        let mut r = rig(vec![source(0)], &[], &[0]);
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!(r.group.phase, Phase::Ended);
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert!(!r.control.is_finished() && !r.control.is_stopped());
    }

    #[test]
    fn a_full_ring_with_no_consumer_blocks_until_the_run_is_over() {
        let mut r = rig(vec![source(0)], &[], &[0]);
        while r.group.downstream[0].data.try_push(Batch::new(BYTES)).is_ok() {}
        r.down.clear();
        assert_eq!(settle(&mut r), ring(0));
        assert_eq!(r.group.phase, Phase::Ended);
        assert!(r.group.downstream[0].staged.as_ref().is_some_and(|b| b.eos));
        assert_eq!(r.group.pass().unwrap(), ring(0));
        r.control.abort();
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
    }

    #[test]
    fn an_ended_group_rests_until_the_run_is_over() {
        let mut r = rig(vec![source(0)], &[], &[0]);
        r.group.phase = Phase::Ended;
        let idle = IDLE;
        assert_eq!(r.group.pass().unwrap(), idle);
        assert_eq!(r.group.pass().unwrap(), idle);
        assert_eq!(r.group.phase, Phase::Ended);
        assert_eq!(n(&r.seen[0].buffers) + n(&r.seen[0].eos), 0, "no element ran");
        r.down.clear();
        assert_eq!(r.group.pass().unwrap(), idle);
        r.control.abort();
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
    }

    #[test]
    fn whole_passes_carry_a_source_to_its_end_on_one_thread() {
        let mut r = rig(vec![source(3)], &[], &[0]);
        r.group.start().unwrap();
        let mut passes = 0;
        while r.group.phase != Phase::Ended {
            assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
            passes += 1;
            assert!(passes < 16, "the group never ended");
        }
        assert_eq!(passes, 5, "three buffers, one Eos, one mark");
        for _ in 0..3 {
            let b = r.down[0].0.try_pop().expect("data batch");
            assert!(b.len() == 1 && !b.eos);
        }
        assert!(r.down[0].0.try_pop().expect("mark").eos);
        let c = r.group.counters[0].snapshot();
        assert_eq!((c.batches_out, c.buffers_out), (3, 3));
        assert_eq!(eos_counts(&r), [1]);
        assert_eq!(r.down_peers[0].epoch(), 4, "one signal per push, the mark included");
    }

    #[test]
    fn paused_and_resumed_are_delivered_once_per_transition() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        let handle = Arc::clone(&r.control);
        let idle = IDLE;
        let seen = r.station.epoch();
        handle.pause();
        assert_ne!(r.station.epoch(), seen, "a pause signals the station");
        handle.pause();
        assert_eq!(r.control.log.head(), 1, "a pause while paused is not a command");
        assert_eq!(r.group.pass().unwrap(), idle, "paused with nothing to do parks the driver");
        assert_eq!(r.group.pass().unwrap(), idle);
        assert_eq!((n(&r.seen[0].paused), n(&r.seen[0].resumed)), (1, 0));
        let seen = r.station.epoch();
        handle.resume();
        assert_ne!(r.station.epoch(), seen, "and so does a resume");
        assert_eq!(r.group.pass().unwrap(), idle);
        assert_eq!(r.group.pass().unwrap(), idle);
        assert_eq!((n(&r.seen[0].paused), n(&r.seen[0].resumed)), (1, 1));
        handle.resume();
        assert_eq!(r.control.log.head(), 2, "and neither is a resume while playing");
    }

    #[test]
    fn a_paused_group_still_moves_the_data_it_has() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        Arc::clone(&r.control).pause();
        assert!(r.up[0].0.try_push(batch(&r.pool, 2)).is_ok());
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!((n(&r.seen[0].paused), n(&r.seen[0].buffers)), (1, 2));
    }

    #[test]
    fn a_seek_while_paused_flushes_and_reprimes_in_the_next_pass() {
        let mut r = rig(vec![Probe { reload: Some(1), ..source(0) }], &[], &[0]);
        Arc::clone(&r.control).pause();
        r.group.phase = Phase::Ended;
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(n(&r.seen[0].paused), 1, "an ended group is told too");
        let seen = r.station.epoch();
        seek(&r);
        assert_ne!(r.station.epoch(), seen, "a seek signals the station");
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed, "still paused, and it re-primes");
        assert_eq!((n(&r.seen[0].flushes), n(&r.seen[0].paused)), (1, 1));
        assert_eq!((r.group.phase, r.group.seek_gen), (Phase::Running, 1));
        assert_eq!(r.down[0].0.try_pop().expect("the re-primed buffer").seek_gen, 1);
    }

    fn ended_source() -> Rig {
        let mut r = rig(vec![Probe { log: true, reload: Some(3), ..source(0) }], &[], &[0]);
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!((r.group.phase, log(&r, 0)), (Phase::Ended, vec![Saw::Eos]));
        r
    }

    #[test]
    fn pause_and_resume_reach_an_ended_group_in_order() {
        let mut r = ended_source();
        let calls = n(&r.seen[0].calls);
        send(&r, Command::Pause);
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(log(&r, 0), [Saw::Eos, Saw::Paused]);
        send(&r, Command::Resume);
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(log(&r, 0), [Saw::Eos, Saw::Paused, Saw::Resumed]);
        assert_eq!(r.group.phase, Phase::Ended, "neither revives it");
        assert_eq!(n(&r.seen[0].calls), calls, "and no element ran again");
    }

    #[test]
    fn pause_and_resume_reach_a_draining_group() {
        let mut r = rig(vec![Probe { log: true, ..source(0) }, probe(&FILTER)], &[], &[0]);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(r.group.phase, Phase::Draining { next: 1 });
        send(&r, Command::Pause);
        send(&r, Command::Resume);
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!(log(&r, 0), [Saw::Eos, Saw::Paused, Saw::Resumed]);
        assert_eq!((r.group.phase, eos_counts(&r)), (Phase::Ended, vec![1, 1]));
    }

    #[test]
    fn a_group_that_slept_through_a_pause_and_a_resume_gets_both() {
        let mut r = rig(vec![Probe { paced: true, log: true, ..probe(&SINK) }], &[0], &[]);
        let mut early = buf(&r.pool);
        early.pts = MS(10);
        let mut b = Batch::new(BYTES);
        b.push(early);
        assert!(r.up[0].0.try_push(b).is_ok());
        r.clock.advance(MS(4));
        send(&r, Command::Pause);
        r.clock.advance(MS(50));
        let last = send(&r, Command::Resume);
        assert!(!r.control.applied_by_all(last));
        assert_eq!(settle(&mut r), idle_until(MS(10)));
        assert!(r.control.applied_by_all(last));
        assert_eq!(log(&r, 0), [Saw::Paused, Saw::Resumed]);
        r.clock.advance(MS(6));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(log(&r, 0), [Saw::Paused, Saw::Resumed, Saw::Buffer]);
    }

    #[test]
    fn two_pending_seeks_flush_once_for_the_last_target() {
        let mut r = rig(vec![probe(&FILTER), probe(&SINK)], &[0], &[]);
        seek_to(&r, 10, MS(1));
        let last = seek_to(&r, 20, MS(2));
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!((n(&r.seen[0].flushes), n(&r.seen[1].flushes)), (1, 1));
        assert_eq!((targets(&r, 0), targets(&r, 1)), (vec![(20, MS(2))], vec![(20, MS(2))]));
        assert_eq!(r.group.seek_gen, 2);
        assert!(r.control.applied_by_all(last), "both are applied, one of them without a flush");
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(n(&r.seen[0].flushes), 1, "and not again");
    }

    #[test]
    fn the_seek_target_is_the_applied_commands_and_not_the_newest_issued() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        assert!(r.group.ctxs[0].seek_target().is_none(), "no seek yet");
        assert_eq!(r.group.ctxs[0].applied_seek_gen(), 0, "the generation the run began in");
        seek_to(&r, 10, MS(1));
        assert_eq!(r.group.ctxs[0].applied_seek_gen(), 0, "issued is not applied");
        assert_eq!(r.group.pass().unwrap(), IDLE);
        seek_to(&r, 20, MS(2));
        let held = r.group.ctxs[0].seek_target().map(|t| (t.to_byte, t.at));
        assert_eq!(held, Some((10, MS(1))), "still the one it flushed for");
        assert_eq!(r.group.ctxs[0].seek_gen(), 2, "while the newest generation is visible");
        assert_eq!(r.group.ctxs[0].applied_seek_gen(), 1, "and the applied one is its own");
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(r.group.ctxs[0].applied_seek_gen(), 2);
        assert_eq!(targets(&r, 0), [(10, MS(1)), (20, MS(2))]);
    }

    #[test]
    fn a_seek_landing_during_a_flush_gets_a_flush_and_a_target_of_its_own() {
        for through_ingest in [false, true] {
            let (mut r, link) = consumer(false);
            seek_to(&r, 10, MS(1));
            let second = Command::Seek { seek_gen: 0, to_time: MS(2) };
            *r.seen[0].reseek.lock().unwrap() = Some((Arc::clone(&r.control), second));
            match through_ingest {
                true => assert!(r.group.ingest(link, post_seek(&r.pool)).unwrap()),
                false => r.group.apply_commands().unwrap(),
            }
            assert_eq!((r.group.seek_gen, targets(&r, 0)), (1, vec![(10, MS(1))]));
            assert_eq!(settle(&mut r), IDLE);
            assert_eq!(r.group.seek_gen, 2);
            assert_eq!(targets(&r, 0), [(10, MS(1)), (20, MS(2))], "ingest {through_ingest}");
            assert_eq!(r.pool.stats().outstanding, 0, "the first seek's batch went too");
        }
    }

    #[test]
    fn pause_seek_and_resume_are_applied_in_the_order_appended() {
        const SEEK: Command = Command::Seek { seek_gen: 0, to_time: Timestamp::ZERO };
        const PAUSE: Command = Command::Pause;
        const RESUME: Command = Command::Resume;
        let orders = [
            (vec![PAUSE, SEEK, RESUME], [Saw::Paused, Saw::Flush, Saw::Resumed]),
            (vec![SEEK, PAUSE, RESUME], [Saw::Flush, Saw::Paused, Saw::Resumed]),
            (vec![SEEK, PAUSE, SEEK, RESUME], [Saw::Paused, Saw::Flush, Saw::Resumed]),
        ];
        for (commands, want) in orders {
            let (mut r, _) = consumer(false);
            for cmd in commands {
                send(&r, cmd);
            }
            assert_eq!(r.group.pass().unwrap(), IDLE);
            assert_eq!(log(&r, 0), want);
        }
    }

    #[test]
    fn a_paused_group_renders_nothing_in_the_pass_that_applies_the_pause() {
        let mut r = paced(1, MS(10));
        r.clock.advance(MS(10));
        send(&r, Command::Pause);
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!((n(&r.seen[0].paused), n(&r.seen[0].buffers)), (1, 0));
    }

    #[test]
    fn a_stopped_group_applies_nothing_and_a_running_one_is_revived_by_the_seek() {
        let mut r = ended_source();
        let calls = n(&r.seen[0].calls);
        while r.down[0].0.try_pop().is_some() {}
        send(&r, Command::Stop);
        seek(&r);
        send(&r, Command::Pause);
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
        assert_eq!(log(&r, 0), [Saw::Eos], "no flush and no pause, nothing would read the result");
        assert_eq!(n(&r.seen[0].calls), calls, "and the source did not start over");

        let mut r = ended_source();
        while r.down[0].0.try_pop().is_some() {}
        seek(&r);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!((r.group.phase, n(&r.seen[0].flushes)), (Phase::Running, 1));
        assert_eq!(r.down[0].0.try_pop().expect("the reloaded stream").seek_gen, 1);
    }

    #[test]
    fn a_seek_and_a_stop_reach_a_group_whose_mark_is_staged() {
        let staged_mark = || {
            let mut r = rig(vec![Probe { reload: Some(1), ..source(4) }], &[], &[0]);
            assert_eq!(settle(&mut r), ring(0));
            assert!(r.group.phase == Phase::Ended && r.group.downstream[0].staged.is_some());
            r
        };
        let mut r = staged_mark();
        seek(&r);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!((r.group.phase, n(&r.seen[0].flushes)), (Phase::Running, 1));
        let staged = r.group.downstream[0].staged.as_ref().expect("the ring is still full");
        assert!(!staged.eos && staged.seek_gen == 1, "the stale mark went, a fresh batch waits");

        let mut r = staged_mark();
        send(&r, Command::Stop);
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
    }

    #[test]
    fn a_stop_ends_the_group_at_its_next_pass_whatever_is_pending() {
        let (mut r, _) = consumer(false);
        assert!(r.up[0].0.try_push(batch(&r.pool, 1)).is_ok());
        let pause = send(&r, Command::Pause);
        let stop = send(&r, Command::Stop);
        assert_eq!(r.control.append(Command::Stop), Err(CommandError::Redundant));
        seek(&r);
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
        assert!(log(&r, 0).is_empty(), "nothing is applied in a run that is over");
        assert_eq!(n(&r.seen[0].calls), 0, "no element ran");
        assert!(!r.control.applied_by_all(pause) && !r.control.applied_by_all(stop));
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit, "and it stays over");
        r.control.retire(&r.group.cursor, &r.group.station, r.group.ids[0]);
        assert!(r.control.applied_by_all(stop));
    }

    #[test]
    fn a_batch_from_an_unapplied_seek_applies_the_backlog_in_order_first() {
        let (mut r, link) = consumer(false);
        assert!(r.group.ingest(link, batch(&r.pool, 2)).unwrap());
        send(&r, Command::Pause);
        seek_to(&r, 10, MS(1));
        send(&r, Command::Stop);
        assert!(r.up[link].0.try_push(post_seek(&r.pool)).is_ok());
        let mut progressed = false;
        r.group.feed(&mut progressed).unwrap();
        assert_eq!(log(&r, 0), [Saw::Paused, Saw::Flush], "in log order, up to the stop");
        assert_eq!((r.group.seek_gen, targets(&r, 0)), (1, vec![(10, MS(1))]));
        assert_eq!(r.group.ctxs[0].input_len(), 1, "the post-seek buffer, not the old two");
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit, "the stop ends it at the next pass");
    }

    #[test]
    fn a_group_follows_the_log_past_its_capacity() {
        let (mut r, _) = consumer(false);
        for round in 0..COMMAND_LOG_CAP as u64 {
            send(&r, Command::Pause);
            let last = send(&r, Command::Resume);
            seek_to(&r, round, MS(round));
            assert_eq!(r.group.pass().unwrap(), IDLE);
            assert!(r.control.applied_by_all(last + 1));
        }
        let round = [Saw::Paused, Saw::Resumed, Saw::Flush];
        assert!(log(&r, 0).chunks(3).all(|c| c == round), "every command, in order");
        assert_eq!(log(&r, 0).len(), 3 * COMMAND_LOG_CAP);
        assert_eq!(r.group.seek_gen, COMMAND_LOG_CAP as u64);
        for _ in 0..COMMAND_LOG_CAP / 2 {
            send(&r, Command::Pause);
            send(&r, Command::Resume);
        }
        assert_eq!(r.control.append(Command::Pause), Err(CommandError::Full));
        assert_eq!(r.group.pass().unwrap(), IDLE);
        send(&r, Command::Pause);
    }

    fn skip_to(r: &Rig, to_time: Timestamp) -> u64 {
        send(r, Command::SkipTo { to_time })
    }

    fn point(r: &Rig, element: usize) -> Option<Timestamp> {
        r.group.ctxs[element].presentation_point()
    }

    #[test]
    fn a_skip_tells_every_element_the_point_and_discards_nothing() {
        let filter = Probe { log: true, sip: true, outs: &[0], ..probe(&FILTER) };
        let mut r = rig(vec![filter, probe(&SINK)], &[0], &[]);
        assert!(r.group.ingest(0, batch(&r.pool, 3)).unwrap());
        assert_eq!((point(&r, 0), point(&r, 1)), (None, None), "no seek and no skip yet");
        let seen = r.station.epoch();
        let seq = skip_to(&r, MS(700));
        assert_ne!(r.station.epoch(), seen, "a skip signals the station");
        assert!(r.group.ctxs[0].command_pending() && !r.control.applied_by_all(seq));
        r.group.apply_commands().unwrap();
        assert!(!r.group.ctxs[0].command_pending() && r.control.applied_by_all(seq));
        assert_eq!((point(&r, 0), point(&r, 1)), (Some(MS(700)), Some(MS(700))));
        assert_eq!(log(&r, 0), [Saw::Skip(MS(700))], "one event, and no flush");
        assert_eq!((r.group.seek_gen, r.control.seek_gen()), (0, 0), "no generation moved");
        assert!(r.group.ctxs[0].seek_target().is_none(), "and it is no seek");
        assert_eq!(r.group.ctxs[0].input_len(), 3, "the staged input is still there");
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!((n(&r.seen[0].buffers), n(&r.seen[1].buffers)), (3, 3), "and it all plays");
        assert_eq!(n(&r.seen[0].flushes) + n(&r.seen[1].flushes), 0);
    }

    #[test]
    fn a_seek_sets_the_point_to_its_time_and_a_skip_moves_it() {
        let (mut r, _) = consumer(false);
        seek_to(&r, 10, MS(250));
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(point(&r, 0), Some(MS(250)), "a seek's time is where presentation starts");
        skip_to(&r, MS(900));
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(point(&r, 0), Some(MS(900)));
        skip_to(&r, MS(100));
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(point(&r, 0), Some(MS(100)));
        assert_eq!(log(&r, 0), [Saw::Flush, Saw::Skip(MS(900)), Saw::Skip(MS(100))]);
        assert_eq!(targets(&r, 0), [(10, MS(250))], "the seek target is still the seek's");
    }

    #[test]
    fn of_the_skips_and_seeks_in_one_drain_only_the_last_jump_sets_the_point() {
        const fn seek_at(ms: u64) -> Command {
            Command::Seek { seek_gen: 0, to_time: Timestamp::from_millis(ms) }
        }
        const fn skip_at(ms: u64) -> Command {
            Command::SkipTo { to_time: Timestamp::from_millis(ms) }
        }
        let skip = |ms| Saw::Skip(MS(ms));
        let orders: [(&[Command], &[Saw], u64, u32); 8] = [
            (&[skip_at(1), skip_at(2), skip_at(3)], &[skip(3)], 3, 0),
            (&[skip_at(1), seek_at(5)], &[Saw::Flush], 5, 1),
            (&[seek_at(5), skip_at(7)], &[Saw::Flush, skip(7)], 7, 1),
            (&[seek_at(5), skip_at(7), seek_at(9)], &[Saw::Flush], 9, 1),
            (&[seek_at(5), skip_at(7), seek_at(9), skip_at(11)], &[Saw::Flush, skip(11)], 11, 1),
            (&[skip_at(1), seek_at(5), skip_at(7), skip_at(8)], &[Saw::Flush, skip(8)], 8, 1),
            (
                &[skip_at(1), Command::Pause, skip_at(2), Command::Resume],
                &[Saw::Paused, skip(2), Saw::Resumed],
                2,
                0,
            ),
            (
                &[Command::Pause, seek_at(5), Command::Resume, skip_at(6)],
                &[Saw::Paused, Saw::Flush, Saw::Resumed, skip(6)],
                6,
                1,
            ),
        ];
        for (commands, want, at, flushes) in orders {
            let (mut r, _) = consumer(false);
            for cmd in commands {
                send(&r, *cmd);
            }
            assert_eq!(r.group.pass().unwrap(), IDLE);
            assert_eq!(log(&r, 0), want, "{commands:?}");
            assert_eq!(point(&r, 0), Some(MS(at)), "{commands:?}");
            assert_eq!(n(&r.seen[0].flushes), flushes, "{commands:?}");
            let seeks = commands.iter().filter(|c| matches!(c, Command::Seek { .. })).count();
            assert_eq!(r.group.seek_gen, seeks as u64, "only a seek takes a generation");
        }
    }

    #[test]
    fn skips_in_separate_drains_are_each_delivered() {
        let (mut r, _) = consumer(false);
        for ms in [1, 2, 3] {
            skip_to(&r, MS(ms));
            assert_eq!(r.group.pass().unwrap(), IDLE);
        }
        assert_eq!(log(&r, 0), [Saw::Skip(MS(1)), Saw::Skip(MS(2)), Saw::Skip(MS(3))]);
    }

    #[test]
    fn a_batch_from_an_unapplied_seek_applies_a_skip_behind_it_after_the_flush() {
        let (mut r, link) = consumer(false);
        assert!(r.group.ingest(link, batch(&r.pool, 2)).unwrap());
        skip_to(&r, MS(3));
        seek_to(&r, 10, MS(5));
        skip_to(&r, MS(7));
        assert!(r.up[link].0.try_push(post_seek(&r.pool)).is_ok());
        let mut progressed = false;
        r.group.feed(&mut progressed).unwrap();
        assert_eq!(log(&r, 0), [Saw::Flush, Saw::Skip(MS(7))]);
        assert_eq!((r.group.seek_gen, point(&r, 0)), (1, Some(MS(7))));
        assert_eq!(r.group.ctxs[0].input_len(), 1, "the post-seek buffer, not the old two");
    }

    #[test]
    fn a_skip_leaves_a_staged_batch_and_its_generation_alone() {
        let mut r = rig(vec![Probe { log: true, ..source(6) }], &[], &[0]);
        assert_eq!(settle(&mut r), ring(0));
        let staged = |r: &Rig| r.group.downstream[0].staged.as_ref().map(|b| (b.len(), b.seek_gen));
        assert_eq!(staged(&r), Some((1, 0)));
        skip_to(&r, MS(500));
        assert_eq!(r.group.pass().unwrap(), ring(0));
        assert_eq!((log(&r, 0), staged(&r)), (vec![Saw::Skip(MS(500))], Some((1, 0))));
        let mut got = 0;
        for _ in 0..16 {
            while let Some(b) = r.down[0].0.try_pop() {
                assert_eq!(b.seek_gen, 0);
                got += b.len();
            }
            let _ = r.group.pass().unwrap();
        }
        assert_eq!(got, 6);
    }

    #[test]
    fn a_skip_reaches_an_ended_group_and_does_not_revive_it() {
        let mut r = ended_source();
        let calls = n(&r.seen[0].calls);
        while r.down[0].0.try_pop().is_some() {}
        skip_to(&r, Timestamp::from_secs(60));
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(log(&r, 0), [Saw::Eos, Saw::Skip(Timestamp::from_secs(60))]);
        assert_eq!((r.group.phase, r.group.seek_gen), (Phase::Ended, 0), "it still rests");
        assert_eq!(n(&r.seen[0].calls), calls, "no element ran again");
        assert!(r.down[0].0.try_pop().is_none(), "and no second mark went out");
        assert_eq!(point(&r, 0), Some(Timestamp::from_secs(60)), "a later seek starts from it");
    }

    #[test]
    fn a_skip_reaches_a_draining_group_which_ends_once_in_its_generation() {
        let mut r = rig(vec![Probe { log: true, ..source(0) }, probe(&FILTER)], &[], &[0]);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(r.group.phase, Phase::Draining { next: 1 });
        skip_to(&r, MS(40));
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!(log(&r, 0), [Saw::Eos, Saw::Skip(MS(40))]);
        assert_eq!((r.group.phase, eos_counts(&r)), (Phase::Ended, vec![1, 1]));
        let mark = r.down[0].0.try_pop().expect("the mark");
        assert!(mark.eos && mark.seek_gen == 0 && r.down[0].0.try_pop().is_none(), "one mark");
    }

    #[test]
    fn a_skip_does_not_stand_between_an_ended_sink_and_the_finish() {
        let mut r = rig(vec![Probe { log: true, ..probe(&SINK) }], &[0], &[]);
        assert!(r.group.ingest(0, mark()).unwrap());
        assert!(r.group.terminate().unwrap() && r.group.terminate().unwrap());
        assert_eq!(r.group.phase, Phase::Ended);
        skip_to(&r, MS(40));
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(log(&r, 0), [Saw::Eos, Saw::Skip(MS(40))], "applied, and no second Eos");
        assert!(r.control.is_finished(), "the stream ended in the generation that is newest");
        let late = r.control.append(Command::SkipTo { to_time: MS(50) });
        assert_eq!(late, Err(CommandError::Finished));
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
    }

    #[test]
    fn a_skip_past_the_end_drops_what_is_left_and_the_stream_ends_once() {
        let mut r = paced(3, MS(10));
        assert!(r.up[0].0.try_push(mark()).is_ok());
        assert_eq!(settle(&mut r), idle_until(MS(10)));
        skip_to(&r, Timestamp::from_secs(5));
        assert_eq!(settle(&mut r), IDLE);
        let c = r.group.counters[0].snapshot();
        assert_eq!((n(&r.seen[0].buffers), c.drops, c.buffers_in), (0, 3, 3), "counted");
        assert_eq!((n(&r.seen[0].eos), r.group.phase), (1, Phase::Ended));
        assert!(r.control.is_finished(), "in generation zero, which is still the newest");
        assert_eq!(r.pool.stats().outstanding, 0);
    }

    #[test]
    fn a_paced_sink_drops_what_is_before_the_point_and_takes_the_rest_on_time() {
        let mut r = paced(5, MS(10));
        r.clock.advance(MS(4));
        assert_eq!(settle(&mut r), idle_until(MS(10)));
        skip_to(&r, MS(25));
        assert_eq!(r.group.clock_deadline(MS(30)), MS(9), "five milliseconds away now");
        assert_eq!(settle(&mut r), idle_until(MS(30)), "10 and 20 went, 30 waits for its time");
        assert_eq!((n(&r.seen[0].buffers), r.group.counters[0].snapshot().drops), (0, 2));
        r.clock.advance(MS(5));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        for _ in 0..2 {
            r.clock.advance(MS(10));
            assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        }
        assert_eq!(settle(&mut r), IDLE);
        let want = [(MS(30), MS(30)), (MS(40), MS(40)), (MS(50), MS(50))];
        assert_eq!(*r.seen[0].renders.lock().unwrap(), want, "each at its own deadline");
        assert_eq!(r.group.counters[0].snapshot().drops, 2, "and none of them was a drop");
    }

    #[test]
    fn a_skip_while_paused_rebases_from_the_pause_and_the_resume_plays_from_the_point() {
        let mut r = paced(5, MS(10));
        r.clock.advance(MS(4));
        send(&r, Command::Pause);
        assert_eq!(settle(&mut r), IDLE);
        r.clock.advance(MS(100));
        skip_to(&r, MS(30));
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!((n(&r.seen[0].buffers), r.group.counters[0].snapshot().drops), (0, 2));
        assert_eq!(r.group.ctxs[0].input_len(), 3);
        r.clock.advance(MS(100));
        send(&r, Command::Resume);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(40)));
        assert_eq!(*r.seen[0].renders.lock().unwrap(), [(MS(30), MS(30))]);
    }

    #[test]
    fn a_stopped_group_applies_no_skip() {
        let (mut r, _) = consumer(false);
        send(&r, Command::Stop);
        skip_to(&r, MS(5));
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
        assert!(log(&r, 0).is_empty() && point(&r, 0).is_none());
    }

    #[test]
    fn the_newest_pts_an_element_put_out_is_in_its_counters_and_a_flush_forgets_it() {
        let src = Probe { reload: Some(2), pts_step: MS(40), ..source(3) };
        let mut r = rig(vec![src, Probe { outs: &[0], ..probe(&FILTER) }], &[], &[0]);
        let pts_out = |r: &Rig, i: usize| r.group.counters[i].snapshot().pts_out;
        assert_eq!((pts_out(&r, 0), pts_out(&r, 1)), (None, None));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(pts_out(&r, 0), Some(Timestamp::ZERO), "a pts of zero is a pts");
        for _ in 0..2 {
            assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        }
        assert_eq!((pts_out(&r, 0), pts_out(&r, 1)), (Some(MS(80)), Some(MS(80))));
        skip_to(&r, MS(500));
        r.group.apply_commands().unwrap();
        assert_eq!(pts_out(&r, 0), Some(MS(80)));
        seek(&r);
        r.group.apply_commands().unwrap();
        assert_eq!((pts_out(&r, 0), pts_out(&r, 1)), (None, None), "nothing since the flush");
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(pts_out(&r, 0), Some(Timestamp::ZERO), "behind the old position, and true");
        let mut r = rig(vec![source(2)], &[], &[0]);
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!((pts_out(&r, 0), r.group.counters[0].snapshot().buffers_out), (None, 2));
    }

    #[test]
    fn the_position_is_kept_for_each_src_pad() {
        let one = Probe { outs: &[1], pts_step: MS(40), reload: Some(1), ..source(3) };
        let mut r = rig(vec![Probe { desc: &SRC2, ..one }], &[], &[0, 1]);
        for _ in 0..3 {
            assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        }
        let s = r.group.counters[0].snapshot();
        assert_eq!((s.pad_pts(SrcPad(0)), s.pad_pts(SrcPad(1))), (None, Some(MS(80))));
        assert_eq!(s.pts_out, Some(MS(80)), "the element's is the furthest pad's");
        seek(&r);
        r.group.apply_commands().unwrap();
        assert_eq!(r.group.counters[0].snapshot().pad_pts(SrcPad(1)), None, "a flush forgets it");
    }

    #[test]
    fn a_failed_start_stops_the_started_elements_in_reverse() {
        let bad = Probe { fail_start: true, ..probe(&SINK) };
        let mut r = rig(vec![probe(&FILTER), probe(&FILTER), bad], &[0], &[]);
        let err = r.group.start().unwrap_err();
        let cause = std::error::Error::source(&err).expect("the element's own error");
        assert!(cause.is::<StartRefused>(), "start() hands the payload back untouched");
        let stops: Vec<u32> = r.seen.iter().map(|s| n(&s.stops)).collect();
        assert_eq!(stops, [1, 1, 0], "the element that failed never started");
        assert_eq!((n(&r.seen[1].order), n(&r.seen[0].order)), (1, 2), "reverse order");
    }

    #[test]
    fn teardown_stops_in_reverse_and_the_rings_close_with_the_group() {
        let mut r = rig(vec![probe(&FILTER), probe(&FILTER)], &[0], &[0]);
        r.group.start().unwrap();
        let seen = (r.station.epoch(), r.up_peers[0].epoch(), r.down_peers[0].epoch());
        r.group.close();
        assert_eq!((n(&r.seen[1].order), n(&r.seen[0].order)), (1, 2));
        assert!(!r.down[0].0.is_closed() && !r.up[0].0.is_closed());
        let Rig { group, down, up, station, up_peers, down_peers, .. } = r;
        drop(group);
        assert!(down[0].0.is_closed() && up[0].0.is_closed());
        let now = (station.epoch(), up_peers[0].epoch(), down_peers[0].epoch());
        assert_eq!(now, seen);
    }

    fn step_ctx() -> (Ctx, Bus, Arc<Vocabulary>) {
        let (tx, bus) = Bus::channel();
        let vocabulary = vocabulary();
        let mut ctx = Ctx::new(Pool::bounded(64, 8), tx, ElementId(7), BYTES, 1);
        ctx.configure_pads(2, 1);
        ctx.set_vocabulary(Arc::clone(&vocabulary));
        (ctx, bus, vocabulary)
    }

    #[test]
    fn resolved_announcements_reach_the_sink_with_their_pad_and_row() {
        let (mut ctx, bus, vocabulary) = step_ctx();
        let row = ctx.alloc();
        ctx.out(SrcPad(1)).push(row);
        ctx.announce_format(SrcPad(1), "audio/raw", &[("rate", ValueDesc::Int(48_000))]);
        ctx.forward_format(SrcPad(0), FixedFormat::new(FormatId(0)));

        let mut got = Vec::new();
        resolve_announcements(&mut ctx, &vocabulary, |_, pad, at, f| got.push((pad, at, f)));

        assert_eq!(got.len(), 2, "in announce order");
        let rate = vocabulary.field_id("rate").unwrap();
        assert_eq!((got[0].0, got[0].1), (SrcPad(1), 1), "captured after one pushed row");
        assert_eq!(got[0].2.family, vocabulary.family_id("audio/raw").unwrap());
        assert_eq!(got[0].2.get(rate), Some(Value::Int(48_000)));
        assert_eq!((got[1].0, got[1].1, got[1].2.family), (SrcPad(0), 0, FormatId(0)));
        assert!(bus.try_recv().is_none(), "nothing to warn about");
        assert!(ctx.take_announcements().is_empty(), "drained");
    }

    #[test]
    fn an_unresolvable_announcement_warns_once_and_reaches_no_sink() {
        let (mut ctx, bus, vocabulary) = step_ctx();
        ctx.announce_format(SrcPad(0), "video/never-offered", &[]);
        ctx.announce_format(SrcPad(0), "audio/raw", &[("never-a-field", ValueDesc::Int(1))]);
        ctx.announce_format(SrcPad(1), "audio/raw", &[]);

        let mut pads = Vec::new();
        resolve_announcements(&mut ctx, &vocabulary, |_, pad, _, _| pads.push(pad));

        assert_eq!(pads, [SrcPad(1)], "only the resolvable one, and the drain went on");
        let expect: [(&str, &[&str]); 2] =
            [("video/never-offered", &[]), ("audio/raw", &["never-a-field"])];
        for (family, fields) in expect {
            match bus.try_recv() {
                Some(BusMessage::Warning { element, error: Error::Format(e) }) => {
                    let want = FormatError::AnnouncementUnresolved {
                        element: ElementId(7),
                        family,
                        fields: fields.into(),
                    };
                    assert_eq!((element, e), (ElementId(7), want), "names what was announced");
                }
                _ => panic!("one typed Warning per unresolvable announcement"),
            }
        }
        assert!(bus.try_recv().is_none());
    }

    #[test]
    fn a_format_change_the_pad_cannot_admit_is_a_typed_rejection_on_the_bus() {
        static STRICT: ElementDesc = desc("strict", &ONE_SINK, &[]);
        let (mut ctx, bus, vocabulary) = step_ctx();
        let v = &vocabulary;
        let (mut formats, mut fields, mut values) =
            (v.formats.clone(), v.fields.clone(), v.values.clone());
        let allowed = ConstraintSpec::Eq(ValueDesc::Int(44_100));
        let rate = [FieldSpec { field: "rate", allowed, preferred: None }];
        let menu = [OfferSpec { family: "audio/raw", fields: &rate }];
        let offers = Interned::intern(&menu, [&mut formats, &mut fields, &mut values]);
        let lines: Box<[Box<str>]> = ["audio/raw, rate=44100".into()].into();
        let key = (false, IN.0);
        let own = crate::ctx::PadOffers {
            key,
            name: "in".into(),
            offers,
            lines: lines.clone(),
            menu: crate::solve::NONE,
        };
        ctx.set_negotiated(PadFormats {
            sinks: vec![None],
            offers: vec![own],
            ..Default::default()
        });

        let audio = vocabulary.family_id("audio/raw").unwrap();
        let rate = vocabulary.field_id("rate").unwrap();
        let mut elem = probe(&STRICT);
        let change = |hz| {
            let mut f = FixedFormat::new(audio);
            f.set(rate, Value::Int(hz));
            Event::FormatChange(f)
        };

        deliver_event(&mut elem, &mut ctx, Some(IN), change(44_100), &vocabulary).unwrap();
        assert!(bus.try_recv().is_none());
        deliver_event(&mut elem, &mut ctx, Some(SinkPad(3)), change(48_000), &vocabulary).unwrap();

        let err =
            deliver_event(&mut elem, &mut ctx, Some(IN), change(48_000), &vocabulary).unwrap_err();
        let want = FormatError::AnnouncementRejected(
            RejectedFormat {
                element: ElementId(7),
                element_name: "strict",
                pad: IN,
                pad_name: "in".into(),
                family: audio,
                announced: "audio/raw, rate=48000".into(),
                offers: lines,
            }
            .into(),
        );
        assert!(matches!(&err, Error::Format(e) if *e == want), "{err:?}");
        let installed = ctx.negotiated_sink(IN).and_then(|f| f.get(rate));
        assert_eq!(installed, Some(Value::Int(44_100)), "the refused format was not installed");
        match bus.try_recv() {
            Some(BusMessage::Error { element, error: Error::Format(e) }) => {
                assert_eq!((element, e), (ElementId(7), want), "the bus copy is the same error");
            }
            _ => panic!("the rejection is posted"),
        }
    }

    #[test]
    fn the_scheduler_rides_an_announcement_as_a_format_change_at_its_row() {
        let announcer = Probe { announce: Some("audio/raw"), ..source(1) };
        let mut r = rig(vec![announcer], &[], &[0]);
        let mut progressed = false;
        r.group.run_members(&mut progressed).unwrap();
        assert!(progressed);
        let audio = r.group.shared.vocabulary.family_id("audio/raw").unwrap();
        let out = r.group.ctxs[0].output_on(OUT);
        assert!(out.due_event().is_none(), "the event follows the buffer it was announced after");
        assert!(out.pop_front().is_some());
        match out.due_event() {
            Some(Event::FormatChange(f)) => assert_eq!(f.family, audio),
            _ => panic!("FormatChange due after the first row"),
        }
        assert!(r.bus.try_recv().is_none());
    }

    #[test]
    fn process_once_hands_over_the_input_and_keeps_what_was_left() {
        let (mut ctx, _bus, _vocabulary) = step_ctx();
        let mut sipper = Probe { sip: true, ..probe(&FILTER) };
        let pool = Pool::bounded(64, 8);
        let mut input = batch(&pool, 2);
        ctx.input_append(&mut input);

        assert_eq!(process_once(&mut sipper, &mut ctx, false).unwrap(), Flow::Ok);
        assert_eq!((n(&sipper.seen.buffers), ctx.input_len()), (0, 2), "no input offered");
        assert_eq!(process_once(&mut sipper, &mut ctx, true).unwrap(), Flow::Ok);
        assert_eq!((n(&sipper.seen.buffers), ctx.input_len()), (1, 1), "the rest goes back");
    }

    const IDLE: Outcome = Outcome::Idle { until: Timestamp::NONE, within: Duration::MAX };

    fn idle_until(until: Timestamp) -> Outcome {
        Outcome::Idle { until, within: Duration::MAX }
    }

    fn ring(pad: u32) -> Outcome {
        Outcome::Blocked { on: BlockedOn::Ring { pad: SrcPad(pad) } }
    }

    fn settle(r: &mut Rig) -> Outcome {
        for _ in 0..64 {
            match r.group.pass().unwrap() {
                Outcome::Progressed => {}
                other => return other,
            }
        }
        panic!("the group never came to rest");
    }

    #[test]
    fn an_empty_input_is_idle_without_a_deadline() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert!(r.up[0].0.try_push(batch(&r.pool, 1)).is_ok());
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(r.group.pass().unwrap(), IDLE);
    }

    #[test]
    fn consuming_input_is_progress_even_when_nothing_comes_out() {
        let mut r = rig(vec![Probe { sip: true, ..probe(&SINK) }], &[0], &[]);
        assert!(r.up[0].0.try_push(batch(&r.pool, 3)).is_ok());
        for left in [2, 1, 0] {
            assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
            assert_eq!(r.group.ctxs[0].input_len(), left);
        }
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(n(&r.seen[0].buffers), 3);
    }

    #[test]
    fn an_armed_wake_is_idle_until_the_earliest_deadline() {
        let head = Probe { wake_at: Some(Timestamp::from_millis(70)), ..probe(&FILTER) };
        let tail = Probe { wake_at: Some(Timestamp::from_millis(40)), ..probe(&SINK) };
        let mut r = rig(vec![head, tail], &[0], &[]);
        let idle = idle_until(Timestamp::from_millis(40));
        assert_eq!(r.group.pass().unwrap(), idle);
        assert_eq!(r.group.pass().unwrap(), idle, "re-armed every pass");
        assert!(r.up[0].0.try_push(batch(&r.pool, 1)).is_ok());
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert!(r.group.ctxs.iter_mut().all(|c| c.take_wake_at().is_none()));
    }

    #[test]
    fn a_full_ring_stages_one_batch_and_blocks_the_group_on_it() {
        let mut r = rig(vec![source(100)], &[], &[0]);
        assert_eq!(settle(&mut r), ring(0));
        let made = r.group.counters[0].snapshot().buffers_out;
        assert_eq!(made, 5, "four in the ring and one staged");
        assert!(r.group.downstream[0].staged.is_some());
        assert_eq!(r.group.pass().unwrap(), ring(0));
        assert_eq!(r.group.counters[0].snapshot().buffers_out, made, "no element ran");
        assert!(r.group.downstream[0].room.take());
        assert!(r.down[0].0.try_pop().is_some());
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed, "the staged batch went in");
        assert_eq!(r.group.counters[0].snapshot().buffers_out, made + 1, "and the source ran");
        assert_eq!(settle(&mut r), ring(0));
        assert!(r.group.downstream[0].room.take(), "raised again by the new failed push");
    }

    #[test]
    fn a_restart_runs_the_members_behind_a_staged_batch_until_the_tail_feeds_the_ring() {
        let mut r = rig(vec![Probe { pts_step: MS(10), ..source(100) }], &[], &[0]);
        assert_eq!(settle(&mut r), ring(0));
        let made = r.group.counters[0].snapshot().buffers_out;
        r.group.restart = Restart::Handed;
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed, "the member ran on the full ring");
        assert_eq!(r.group.counters[0].snapshot().buffers_out, made + 1);
        let staged = r.group.downstream[0].staged.as_ref().expect("the ring is still full");
        assert_eq!(staged.len(), 2, "the new batch queued behind the refused one");
        assert_eq!(r.group.restart, Restart::Idle, "feeding the full ring ends the restart");
        assert_eq!(r.group.pass().unwrap(), ring(0), "and the group rests on it again");
        assert_eq!(r.group.counters[0].snapshot().buffers_out, made + 1, "no element ran");
        for _ in 0..4 {
            assert!(r.down[0].0.try_pop().is_some());
        }
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        let first = r.down[0].0.try_pop().expect("the staged batch went in");
        assert_eq!(first.len(), 2, "ahead of what the source made after it");
        assert_eq!(r.down[0].0.try_pop().map(|b| b.len()), Some(1));
    }

    #[test]
    fn what_the_tail_held_from_before_a_restart_does_not_end_it() {
        let gate = Arc::new(AtomicBool::new(false));
        let head = Probe { pts_step: MS(10), gate: Some(Arc::clone(&gate)), ..source(100) };
        let tail = Probe { outs: &[0], ..probe(&FILTER) };
        let mut r = rig(vec![head, tail], &[], &[0]);
        for _ in 0..4 {
            assert!(r.group.downstream[0].data.try_push(timed(&r.pool, 0, 1)).is_ok());
        }
        r.group.downstream[0].staged = Some(timed(&r.pool, 1, 1));
        r.group.restart = Restart::Handed;
        let mut left = timed(&r.pool, 2, 1);
        r.group.ctxs[1].output_on(OUT).append(&mut left);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed, "the held row went out");
        let staged = |r: &Rig| r.group.downstream[0].staged.as_ref().map(Batch::len);
        assert_eq!(staged(&r), Some(2), "behind the refused batch");
        assert_eq!(r.group.restart, Restart::Handed, "the head read nothing yet");
        gate.store(true, Ordering::Release);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed, "the head read, the tail fed");
        assert_eq!(staged(&r), Some(3));
        assert_eq!(r.group.restart, Restart::Idle, "media read behind the restart ends it");
        assert_eq!(r.group.pass().unwrap(), ring(0));
    }

    #[test]
    fn a_restart_runs_the_members_on_a_full_level_until_the_tail_feeds_the_link() {
        let mut r = rig(vec![Probe { pts_step: MS(10), ..source(100) }], &[], &[0]);
        limit(&mut r, Limit::time(MS(20)));
        assert_eq!(settle(&mut r), ring(0));
        assert!(r.group.downstream[0].staged.is_none(), "full by the level, not the ring");
        let made = r.group.counters[0].snapshot().buffers_out;
        assert_eq!(made, 3);
        r.group.restart = Restart::Handed;
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed, "the member ran on the full link");
        assert_eq!(r.group.counters[0].snapshot().buffers_out, made + 1);
        assert_eq!(r.down[0].0.len(), 3, "its batch waits, the level is a bound");
        assert_eq!(r.group.downstream[0].staged.as_ref().map(Batch::len), Some(1));
        assert_eq!(r.group.restart, Restart::Idle, "feeding the full link ends the restart");
        assert_eq!(r.group.pass().unwrap(), ring(0), "and the group rests on it again");
        assert_eq!(r.group.counters[0].snapshot().buffers_out, made + 1, "no element ran");
        let mut taken = 0;
        assert_eq!(drain(&r, 0, &mut taken), 3);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert!(r.group.downstream[0].staged.is_none(), "the staged batch went in with room");
    }

    #[test]
    fn a_restart_behind_a_staged_batch_leaks_the_rows_a_leaky_ring_would_have() {
        let src = Probe { pts_step: MS(10), ..source(100) };
        let mut r = rig_with(vec![src], &[], &[0], 64, Leaky::DropNewest);
        for _ in 0..4 {
            assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        }
        assert_eq!(r.down[0].0.len(), 4, "the ring is full");
        let mut state = Batch::new(BYTES);
        state.push_event(tags("a"));
        r.group.downstream[0].staged = Some(state);
        let drops = r.group.counters[0].snapshot().drops;
        r.group.restart = Restart::Handed;
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(r.group.counters[0].snapshot().drops, drops + 1, "the row leaked, and counted");
        let staged = r.group.downstream[0].staged.as_ref().expect("the state still waits");
        assert!(staged.is_empty() && staged.has_events(), "only the state queued behind");
        assert_eq!(r.group.restart, Restart::Idle);
        assert_eq!(r.group.pass().unwrap(), ring(0));
    }

    #[test]
    fn a_stalled_link_does_not_hold_up_the_other_pad() {
        let both = Probe { outs: &[0, 1], emit: Some(100), ..probe(&SRC2) };
        let mut r = rig(vec![both], &[], &[0, 1]);
        let mut got = 0;
        let outcome = loop {
            let outcome = r.group.pass().unwrap();
            while r.down[0].0.try_pop().is_some() {
                got += 1;
            }
            if outcome != Outcome::Progressed {
                break outcome;
            }
            assert!(got < 64, "the group never blocked");
        };
        assert_eq!(outcome, ring(1), "the outcome names the stalled pad");
        assert_eq!(got, 5, "pad 0 got every batch produced, the fifth in the pass that staged");
        assert!(r.group.downstream[0].staged.is_none());
        assert!(r.group.downstream[1].staged.is_some(), "exactly one staged, on the stalled link");
        let made = r.group.counters[0].snapshot().buffers_out;
        assert_eq!(made, 10, "five per pad");
        assert_eq!(r.group.pass().unwrap(), ring(1));
        assert_eq!(r.group.counters[0].snapshot().buffers_out, made, "no element runs");
        assert_eq!(r.down_peers[1].epoch(), 4, "the staged batch signaled nobody yet");

        assert!(r.down[1].0.try_pop().is_some());
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(r.down_peers[1].epoch(), 5);
        assert_eq!(r.group.counters[0].snapshot().buffers_out, made + 2, "it resumed");
    }

    #[test]
    fn a_dry_pool_blocks_the_group_and_a_returned_slot_signals_it() {
        let src = Probe { bounded: true, ..source(100) };
        let mut r = rig_with(vec![src], &[], &[0], 2, Leaky::Block);
        let pool = Outcome::Blocked { on: BlockedOn::Pool { element: ElementId(0) } };
        assert_eq!(settle(&mut r), pool);
        assert_eq!(r.pool.stats().outstanding, 2);
        assert_eq!(r.group.pass().unwrap(), pool);
        let seen = r.station.epoch();
        drop(r.down[0].0.try_pop().expect("a batch holding one slot"));
        assert_eq!(r.station.epoch(), seen + 1, "the slot coming back signaled the group");
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(settle(&mut r), pool);
    }

    #[test]
    fn a_deadline_outranks_a_dry_pool() {
        let src = Probe { bounded: true, wake_at: Some(Timestamp::from_millis(5)), ..source(100) };
        let mut r = rig_with(vec![src], &[], &[0], 1, Leaky::Block);
        assert_eq!(settle(&mut r), idle_until(Timestamp::from_millis(5)));
    }

    #[test]
    fn a_full_leaky_ring_drops_data_and_stages_only_the_mark() {
        let mut r = rig_with(vec![source(10)], &[], &[0], 64, Leaky::DropNewest);
        assert_eq!(settle(&mut r), ring(0), "it never blocked on data");
        let c = r.group.counters[0].snapshot();
        assert_eq!((c.buffers_out, c.drops), (10, 6), "four fit, six were dropped and counted");
        assert_eq!(r.group.phase, Phase::Ended);
        let staged = r.group.downstream[0].staged.as_ref().expect("the mark is never dropped");
        assert!(staged.eos && staged.is_empty());
        assert_eq!(r.pool.stats().outstanding, 4, "dropped buffers went back to the pool");
        for _ in 0..4 {
            assert!(!r.down[0].0.try_pop().expect("data").eos);
        }
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert!(r.down[0].0.try_pop().expect("mark").eos);
        assert_eq!(r.group.pass().unwrap(), IDLE);
    }

    #[test]
    fn a_seek_discards_what_was_staged() {
        let mut r = rig(vec![source(100)], &[], &[0]);
        assert_eq!(settle(&mut r), ring(0));
        seek(&r);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        let staged = r.group.downstream[0].staged.as_ref().expect("full ring, staged again");
        assert_eq!(staged.seek_gen, 1, "the pre-seek batch is gone, this one is fresh");
        assert_eq!(r.pool.stats().outstanding, 5, "four stale in the ring and one staged");
    }

    #[test]
    fn a_staged_batch_whose_consumer_left_stays_staged_until_the_run_is_over() {
        let mut r = rig(vec![source(100)], &[], &[0]);
        assert_eq!(settle(&mut r), ring(0));
        r.down.clear();
        assert_eq!(r.group.pass().unwrap(), ring(0));
        assert!(r.group.downstream[0].staged.is_some());
        r.control.abort();
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
    }

    #[test]
    fn a_terminal_group_ends_the_run_through_whole_passes() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        assert!(r.up[0].0.try_push(mark()).is_ok());
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!((eos_counts(&r), r.control.is_finished()), (vec![1], true));
        assert_eq!(r.group.pass().unwrap(), Outcome::Exit);
    }

    #[test]
    fn a_closed_and_drained_ring_does_not_end_the_stream() {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        assert!(r.up[0].0.try_push(batch(&r.pool, 1)).is_ok());
        r.up.clear();
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!((n(&r.seen[0].buffers), eos_counts(&r)), (1, vec![0]));
        assert!(!r.group.head_done && r.group.phase == Phase::Running);
    }

    #[test]
    fn a_batch_from_an_unobserved_seek_flushes_before_it_is_ingested() {
        for fan_in in [false, true] {
            let (mut r, link) = consumer(fan_in);
            assert!(r.group.ingest(link, batch(&r.pool, 2)).unwrap());
            seek(&r);
            assert!(r.up[link].0.try_push(post_seek(&r.pool)).is_ok());
            let mut progressed = false;
            r.group.feed(&mut progressed).unwrap();
            assert!(progressed);
            assert_eq!((r.group.seek_gen, n(&r.seen[0].flushes)), (1, 1), "flushed in the feed");
            assert_eq!(r.pool.stats().outstanding, 1, "the pre-seek rows went, the new one stayed");
            assert_eq!(settle(&mut r), IDLE);
            assert_eq!(n(&r.seen[0].flushes), 1, "and the next pass does not flush again");
            assert_eq!(log(&r, 0), [Saw::Flush, Saw::Buffer], "the post-seek buffer was not lost");
        }
    }

    #[test]
    fn a_newer_batch_overtaken_by_a_second_seek_is_stale_after_the_flush() {
        let (mut r, link) = consumer(false);
        seek(&r);
        seek(&r);
        assert!(!r.group.ingest(link, post_seek(&r.pool)).unwrap());
        assert_eq!((r.group.seek_gen, n(&r.seen[0].flushes)), (2, 1), "one flush, to the latest");
        assert_eq!(r.pool.stats().outstanding, 0);
        assert!(r.group.ctxs[0].inputs_empty());
    }

    #[test]
    fn a_flush_inside_the_feed_revives_a_pad_that_had_ended() {
        let mux = Probe { log: true, reads: &[0, 1], ..probe(&MUX) };
        let mut r = rig(vec![mux], &[0, 1], &[]);
        assert!(r.up[0].0.try_push(mark()).is_ok());
        seek(&r);
        let mut last = post_seek(&r.pool);
        last.eos = true;
        assert!(r.up[1].0.try_push(last).is_ok());
        let mut progressed = false;
        r.group.feed(&mut progressed).unwrap();
        assert!(!r.group.upstream[0].ended && r.group.upstream[1].ended);
        assert!(!r.group.ctxs[0].is_pad_closed(SinkPad(0)), "the flush took the old mark back");
        assert!(!r.group.head_done, "pad 0 is live again, so the head is not done");
    }

    #[test]
    fn fan_in_feed_holds_each_pad_at_the_cap_and_closes_a_pad_at_its_mark() {
        let mut r = rig(vec![probe(&MUX)], &[0, 1], &[]);
        for _ in 0..2 {
            assert!(r.up[1].0.try_push(batch(&r.pool, 4)).is_ok());
        }
        let mut last = batch(&r.pool, 4);
        last.eos = true;
        assert!(r.up[1].0.try_push(last).is_ok());
        let mut progressed = false;
        r.group.feed(&mut progressed).unwrap();
        assert_eq!(r.group.ctxs[0].input_len_on(SinkPad(1)), INLINE_INPUT_CAP);
        assert!(!r.group.ctxs[0].is_pad_closed(SinkPad(1)), "the mark is still in the ring");
        assert!(r.group.ctxs[0].front_input_on(SinkPad(1)).is_some());
        for left in (0..INLINE_INPUT_CAP).rev() {
            assert!(r.group.ctxs[0].pop_input_on(SinkPad(1)).is_some());
            assert_eq!(r.group.ctxs[0].input_len_on(SinkPad(1)), left);
        }
        r.group.feed(&mut progressed).unwrap();
        assert_eq!(r.group.ctxs[0].input_len_on(SinkPad(1)), 4);
        assert!(r.group.ctxs[0].is_pad_closed(SinkPad(1)));
        assert!(!r.group.head_done, "pad 0 is still open");
    }

    #[test]
    fn a_full_leaky_ring_drops_the_rows_and_keeps_their_state_events() {
        let src = Probe { rates: &[(6, 44_100)], ..source(10) };
        let mut r = rig_with(vec![src], &[], &[0], 64, Leaky::DropNewest);
        assert_eq!(settle(&mut r), ring(0), "blocked on the format, never on data");
        let c = r.group.counters[0].snapshot();
        assert_eq!((c.buffers_out, c.drops), (7, 3), "buffers 4 to 6 were dropped and counted");
        assert_eq!(r.pool.stats().outstanding, 4, "the dropped rows recycled");
        let staged = r.group.downstream[0].staged.as_ref().expect("the format waits for room");
        assert!(staged.is_empty() && staged.has_events() && !staged.eos);
        assert_eq!(announced_rate(&r), Some(44_100), "so the producer will not resend it");

        assert!(r.group.downstream[0].room.take());
        let (mut delivered, mut formats) = (0, Vec::new());
        let mut ended = false;
        for _ in 0..64 {
            while let Some(mut b) = r.down[0].0.try_pop() {
                formats.extend(state_of(&mut b));
                delivered += b.len() as u64;
                ended |= b.eos;
            }
            if ended {
                break;
            }
            let _ = r.group.pass().unwrap();
        }
        assert!(ended, "the mark came through");
        assert_eq!(formats, [Saw::Format(44_100)], "exactly once");
        let c = r.group.counters[0].snapshot();
        assert_eq!(c.buffers_out, 10);
        assert_eq!(delivered + c.drops, 10, "every row was delivered or counted");
    }

    #[test]
    fn a_format_on_a_full_leaky_ring_reaches_the_consumer_before_later_rows() {
        let src = Probe { rates: &[(5, 44_100)], ..source(8) };
        let mut p = rig_with(vec![src], &[], &[0], 64, Leaky::DropNewest);
        let (mut c, _) = consumer(false);
        assert_eq!(settle(&mut p), ring(0));
        for _ in 0..64 {
            let moved = ferry(&p, &c, 4);
            let rest = settle(&mut c);
            assert!(rest == IDLE || rest == Outcome::Exit, "at rest, or over once it ended");
            if moved == 0 && p.group.phase == Phase::Ended && p.group.staged_pad().is_none() {
                break;
            }
            let _ = settle(&mut p);
        }
        let got = log(&c, 0);
        let at = got.iter().position(|s| *s == Saw::Format(44_100)).expect("the format arrived");
        assert_eq!(got.iter().filter(|s| matches!(s, Saw::Format(_))).count(), 1);
        assert_eq!(&got[..at], [Saw::Buffer, Saw::Buffer, Saw::Buffer, Saw::Buffer]);
        assert_eq!(negotiated_rate(&c, 0, 0), Some(44_100));
        assert_eq!(got.last(), Some(&Saw::Eos));
    }

    fn paced(buffers: u64, step: Timestamp) -> Rig {
        let r = rig(vec![Probe { paced: true, ..probe(&SINK) }], &[0], &[]);
        let mut b = Batch::new(BYTES);
        for k in 1..=buffers {
            let mut row = buf(&r.pool);
            row.pts = Timestamp(k * step.0);
            b.push(row);
        }
        assert!(r.up[0].0.try_push(b).is_ok());
        r
    }

    const MS: fn(u64) -> Timestamp = Timestamp::from_millis;

    #[test]
    fn an_early_buffer_stays_in_the_input_and_the_pass_is_idle_until_its_deadline() {
        let mut r = paced(2, MS(10));
        assert_eq!(settle(&mut r), idle_until(MS(10)));
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(10)), "armed again by every pass");
        assert_eq!((n(&r.seen[0].buffers), r.group.ctxs[0].input_len()), (0, 2), "still input");
        assert_eq!(r.group.clock_deadline(MS(10)), MS(10));

        r.clock.advance(MS(9));
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(10)), "one millisecond short");
        r.clock.advance(MS(1));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed, "rendered at its deadline");
        assert_eq!((n(&r.seen[0].buffers), r.group.ctxs[0].input_len()), (1, 1));
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(20)), "and the next one waits");
        r.clock.advance(MS(60));
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!(*r.seen[0].renders.lock().unwrap(), [(MS(10), MS(10)), (MS(20), MS(70))]);
    }

    #[test]
    fn a_paced_sink_follows_a_rate_and_its_own_offset_through_due_alone() {
        use crate::time::Rate;
        let r = rig(vec![Probe { paced: true, log: true, ..probe(&SINK) }], &[0], &[]);
        let mut b = Batch::new(BYTES);
        for k in 1..=6 {
            let mut row = buf(&r.pool);
            row.pts = MS(k * 10);
            b.push(row);
        }
        assert!(r.up[0].0.try_push(b).is_ok());
        let mut r = r;
        r.control.set_sinks(vec![0], 1);
        assert_eq!(settle(&mut r), idle_until(MS(10)));
        r.clock.advance(MS(10));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        let seen = r.station.epoch();
        send(&r, Command::SetRate { rate: Rate::new(2, 1).unwrap() });
        assert_ne!(r.station.epoch(), seen, "a rate signals, a sink holds a deadline");
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(15)));
        assert_eq!(r.group.ctxs[0].running_time(MS(20)), MS(15));
        assert_eq!(r.group.ctxs[0].stream_time(MS(15)), MS(20));
        r.clock.advance(MS(5));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        let by = MS(7).0 as i64;
        send(&r, Command::SetOffset { element: ElementId(0), by });
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(27)));
        assert_eq!(r.group.ctxs[0].offset(), by);
        assert_eq!(r.group.ctxs[0].running_time(MS(30)), MS(20), "no timestamp moved");
        r.clock.advance(MS(11));
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(27)), "one millisecond short");
        r.clock.advance(MS(1));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        send(&r, Command::SetOffset { element: ElementId(0), by: -(MS(3).0 as i64) });
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(32)));
        send(&r, Command::SetOffset { element: ElementId(0), by: i64::MIN });
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        let renders = [(10, 10), (20, 15), (30, 27), (40, 27), (50, 27), (60, 27)];
        let renders = renders.map(|(pts, at)| (MS(pts), MS(at)));
        assert_eq!(*r.seen[0].renders.lock().unwrap(), renders);
        let (b, early) = (Saw::Buffer, Saw::Offset(-(MS(3).0 as i64)));
        let events = [b.clone(), Saw::Rate(2, 1), b.clone(), Saw::Offset(by), b.clone(), early];
        let rest = [b.clone(), b.clone(), Saw::Offset(i64::MIN), b];
        assert_eq!(log(&r, 0), [&events[..], &rest[..]].concat());
        let stray = Command::SetOffset { element: ElementId(1), by };
        assert_eq!(r.control.append(stray), Err(crate::command::CommandError::NotASink));
    }

    #[test]
    fn a_rate_is_told_to_all_an_offset_to_its_sink_and_one_element_retimes() {
        use crate::error::GraphError;
        use crate::time::Rate;
        let filter = Probe { log: true, outs: &[0], ..probe(&FILTER) };
        let mut r = rig(vec![filter, Probe { log: true, ..probe(&SINK) }], &[0], &[]);
        r.control.set_sinks(vec![1], 2);
        assert!(r.group.ctxs[0].claim_timeline().is_ok());
        let second = r.group.ctxs[1].claim_timeline();
        let first = ElementId(0);
        let twice = GraphError::TimelineClaimed { element: ElementId(1), first };
        assert!(matches!(&second, Err(Error::Graph(e)) if *e == twice), "{second:?}");
        let half = Rate::new(1, 2).unwrap();
        send(&r, Command::SetRate { rate: half });
        send(&r, Command::SetOffset { element: ElementId(1), by: 9 });
        let not_a_sink = Command::SetOffset { element: first, by: 9 };
        assert_eq!(r.control.append(not_a_sink), Err(crate::command::CommandError::NotASink));
        r.group.apply_commands().unwrap();
        assert_eq!(log(&r, 0), [Saw::Rate(1, 2)]);
        assert_eq!(log(&r, 1), [Saw::Rate(1, 2), Saw::Offset(9)]);
        assert_eq!((r.group.ctxs[0].offset(), r.group.ctxs[1].offset()), (0, 9));
        assert_eq!(r.group.ctxs[1].running_time(MS(40)), MS(40));
        let seen = r.station.epoch();
        r.group.ctxs[0].retime(MS(30), half);
        assert_ne!(r.station.epoch(), seen, "a piece signals, a sink holds a deadline");
        assert_eq!(r.group.ctxs[1].running_time(MS(40)), MS(50));
        assert_eq!(r.group.ctxs[1].running_time(MS(20)), MS(20), "before the switch as it was");
    }

    #[test]
    fn the_path_latency_is_part_of_the_deadline_a_sink_arms() {
        let mut r = paced(1, MS(10));
        r.group.ctxs[0].set_path_latency(MS(5));
        assert_eq!(settle(&mut r), idle_until(MS(15)));
        r.clock.advance(MS(14));
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(15)));
        r.clock.advance(MS(1));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
    }

    #[test]
    fn nothing_is_due_while_paused_and_the_resume_shifts_the_deadline() {
        let mut r = paced(1, MS(10));
        let handle = Arc::clone(&r.control);
        r.clock.advance(MS(4));
        handle.pause();
        assert_eq!(settle(&mut r), IDLE, "paused, so no deadline is armed");
        r.clock.advance(MS(50));
        assert_eq!(r.group.pass().unwrap(), IDLE, "past the deadline, and still not rendered");
        assert_eq!((n(&r.seen[0].paused), n(&r.seen[0].buffers)), (1, 0));

        let seen = r.station.epoch();
        handle.resume();
        assert_ne!(r.station.epoch(), seen, "the resume is what runs the group again");
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(10)));
        assert_eq!(r.group.clock_deadline(MS(10)), MS(60), "on the shifted base");
        r.clock.advance(MS(6));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(*r.seen[0].renders.lock().unwrap(), [(MS(10), MS(10))]);
    }

    #[test]
    fn a_rebase_moves_the_deadline_by_the_next_pass() {
        let mut r = paced(1, MS(100));
        assert_eq!(settle(&mut r), idle_until(MS(100)));
        assert_eq!(r.group.clock_deadline(MS(100)), MS(100));
        let seen = r.station.epoch();
        r.group.shared.base.store(-(MS(90).0 as i64), Ordering::Release);
        r.control.signal_all();
        assert_ne!(r.station.epoch(), seen);
        assert_eq!(r.group.pass().unwrap(), idle_until(MS(100)));
        assert_eq!(r.group.clock_deadline(MS(100)), MS(10), "ten milliseconds away now");
        r.clock.advance(MS(10));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);

        let mut r = paced(1, MS(100));
        assert_eq!(settle(&mut r), idle_until(MS(100)));
        seek(&r);
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!((n(&r.seen[0].flushes), n(&r.seen[0].buffers)), (1, 0));
        assert_eq!(r.pool.stats().outstanding, 0);
    }

    #[test]
    fn an_interval_request_rides_the_idle_outcome_and_outranks_a_dry_pool() {
        let pump = Some(Duration::from_millis(8));
        let mut r = rig(vec![Probe { pump, paced: true, ..probe(&SINK) }], &[0], &[]);
        let within = Duration::from_millis(8);
        assert_eq!(r.group.pass().unwrap(), Outcome::Idle { until: Timestamp::NONE, within });
        Arc::clone(&r.control).pause();
        assert_eq!(r.group.pass().unwrap(), Outcome::Idle { until: Timestamp::NONE, within });

        let src = Probe { bounded: true, pump, ..source(100) };
        let mut r = rig_with(vec![src], &[], &[0], 1, Leaky::Block);
        assert_eq!(settle(&mut r), Outcome::Idle { until: Timestamp::NONE, within });
    }

    fn resting() -> Rig {
        let mut r = rig(vec![probe(&SINK)], &[0], &[]);
        r.group.backstop = Duration::from_secs(30);
        r
    }

    fn wait(r: &mut Rig, seen: u64, until: Timestamp, within: Duration) -> Woke {
        wait_idle(&mut r.group, &r.station, seen, until, within, false)
    }

    #[test]
    fn the_wait_ends_at_once_on_a_reached_deadline_or_a_pending_signal() {
        let mut r = resting();
        let at = Instant::now();
        let none = Duration::MAX;
        r.clock.advance(MS(10));
        let seen = r.station.epoch();
        assert_eq!(wait(&mut r, seen, MS(10), none), Woke::Ripe);
        let seen = r.station.epoch();
        r.clock.advance(MS(1));
        assert_eq!(wait(&mut r, seen, MS(50), none), Woke::Signal);
        let seen = r.station.epoch();
        r.station.signal();
        assert_eq!(wait(&mut r, seen, Timestamp::NONE, none), Woke::Signal);
        assert!(at.elapsed() < Duration::from_secs(10), "nothing slept");
        assert_eq!(r.station.stats().parks, 0);
    }

    #[test]
    fn the_interval_bounds_the_wait_while_the_clock_stands_still() {
        let mut r = resting();
        Arc::clone(&r.control).pause();
        let (seen, at) = (r.station.epoch(), Instant::now());
        let within = Duration::from_millis(8);
        let until = Timestamp::from_secs(3600);
        assert_eq!(wait(&mut r, seen, until, within), Woke::Slice);
        let took = at.elapsed();
        assert!(took >= within && took < Duration::from_secs(10), "{took:?}");
        assert_eq!(r.station.stats().parks, 1);
    }

    #[test]
    fn a_virtual_deadline_out_of_reach_ends_on_the_backstop_and_not_before() {
        let mut r = resting();
        r.group.backstop = Duration::from_millis(20);
        let (seen, at) = (r.station.epoch(), Instant::now());
        assert_eq!(wait(&mut r, seen, MS(10), Duration::MAX), Woke::Backstop);
        assert!(at.elapsed() >= Duration::from_millis(20));
        let within = Duration::from_millis(20);
        assert_eq!(wait(&mut r, seen, Timestamp::NONE, within), Woke::Backstop);
        let ripe = r.clock.now();
        let at = Instant::now();
        assert_eq!(
            wait_idle(&mut r.group, &r.station, seen, ripe, Duration::MAX, true),
            Woke::Backstop
        );
        assert!(at.elapsed() >= Duration::from_millis(20), "it did not spin");
    }

    #[test]
    fn a_wall_clock_deadline_is_slept_out_exactly() {
        let mut r = resting();
        let clock = Arc::new(crate::clock::InstantClock::new());
        r.group.shared.clock = clock.clone();
        let until = clock.now().saturating_add(MS(30));
        let seen = r.station.epoch();
        assert_eq!(
            wait_idle(&mut r.group, &r.station, seen, until, Duration::MAX, true),
            Woke::Ripe
        );
        let now = clock.now();
        assert!(now >= until, "never early");
        assert!(now < until.saturating_add(Timestamp::from_secs(10)), "and not on the backstop");

        r.group.backstop = Duration::from_millis(20);
        let until = clock.now().saturating_add(Timestamp::from_secs(3600));
        let at = Instant::now();
        assert_eq!(wait(&mut r, seen, until, Duration::MAX), Woke::Slice);
        assert!(at.elapsed() >= Duration::from_millis(20));
    }

    /// A backend whose landed work only `run_once` takes, its park returning at once while any is
    /// left.
    struct Landed(Arc<AtomicU32>);

    impl Reactor for Landed {
        fn submit(&mut self, subs: &mut Vec<crate::io::Submission>, _names: &[u8]) {
            subs.clear();
        }
        fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
            out.clear();
            self.0.store(0, Ordering::Relaxed);
        }
        fn due_in(&self) -> Option<Duration> {
            None
        }
        fn is_idle(&self) -> bool {
            true
        }
        fn park(&mut self, timeout: Duration) -> bool {
            if self.0.load(Ordering::Relaxed) > 0 {
                return true;
            }
            std::thread::sleep(timeout);
            false
        }
    }

    fn with_landed(r: &mut Rig) -> Arc<AtomicU32> {
        let landed = Arc::new(AtomicU32::new(1));
        let bell = Arc::clone(&landed);
        let _ = r.station.bell.set(Arc::new(move || {
            bell.fetch_add(1, Ordering::Relaxed);
        }));
        r.group.reactor = Some(Box::new(Landed(Arc::clone(&landed))));
        r.group.backstop = Duration::from_millis(20);
        landed
    }

    fn assert_the_park_sleeps(r: &mut Rig) {
        let (seen, at) = (r.station.epoch(), Instant::now());
        assert_eq!(wait(r, seen, Timestamp::NONE, Duration::MAX), Woke::Backstop);
        assert!(at.elapsed() >= Duration::from_millis(20), "the park returned at once");
    }

    #[test]
    fn a_group_resting_on_a_full_ring_reaps_its_reactor_so_its_park_sleeps() {
        let mut r = rig(vec![source(100)], &[], &[0]);
        assert_eq!(settle(&mut r), ring(0));
        let landed = with_landed(&mut r);
        assert_eq!(r.group.pass().unwrap(), ring(0), "a bell alone is no progress");
        assert_eq!(landed.load(Ordering::Relaxed), 0, "the blocked pass took what landed");
        assert_the_park_sleeps(&mut r);
    }

    #[test]
    fn an_ended_group_reaps_its_reactor_so_its_park_sleeps() {
        let mut r = ended_source();
        let landed = with_landed(&mut r);
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(landed.load(Ordering::Relaxed), 0, "the ended pass took what landed");
        assert_the_park_sleeps(&mut r);
    }

    fn run_without_a_missed_wake(mut p: Pipeline) -> SchedulerStats {
        p.backstop = Duration::from_secs(8);
        let tap = p.tap_handle();
        let at = Instant::now();
        p.run().unwrap();
        let took = at.elapsed();
        let stats = tap.scheduler();
        assert_eq!(stats.tick_expiries, 0, "a park ran out the backstop, {stats:?} in {took:?}");
        assert_eq!(stats.late_wakes, 0);
        assert!(took < p.backstop, "{took:?}");
        stats
    }

    fn a_source(buffers: u32) -> Probe {
        Probe { bounded: true, ..source(buffers) }
    }

    fn a_sink() -> Probe {
        probe(&A_SINK)
    }

    fn link(p: &mut Pipeline, src: (ElementId, SrcPad), sink: (ElementId, SinkPad)) {
        if let Err(e) = p.link(src, sink) {
            panic!("link: {e:?}");
        }
    }

    #[test]
    fn source_to_sink_runs_on_signals_alone() {
        let mut p = Pipeline::new();
        let sink = a_sink();
        let seen = Arc::clone(&sink.seen);
        let (a, b) = (p.add(a_source(20_000)), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        let stats = run_without_a_missed_wake(p);
        assert_eq!(n(&seen.buffers), 20_000);
        assert!(stats.parks > 0, "a four-slot ring between two threads parks somebody");
    }

    #[test]
    fn a_transform_between_two_rings_runs_on_signals_alone() {
        let mut p = Pipeline::new();
        let sink = a_sink();
        let seen = Arc::clone(&sink.seen);
        let filter = Probe { outs: &[0], ..probe(&A_FILTER) };
        let (a, b, c) = (p.add(a_source(20_000)), p.add(filter), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        link(&mut p, b.src(OUT), c.sink(IN));
        run_without_a_missed_wake(p);
        assert_eq!(n(&seen.buffers), 20_000);
    }

    #[test]
    fn a_fan_out_with_one_slow_branch_runs_on_signals_alone() {
        let mut p = Pipeline::new();
        let (fast, slow) = (a_sink(), Probe { slow: Duration::from_micros(300), ..a_sink() });
        let (seen_fast, seen_slow) = (Arc::clone(&fast.seen), Arc::clone(&slow.seen));
        let tee = Probe { outs: &[0, 1], ..probe(&A_TEE) };
        let (a, t) = (p.add(a_source(400)), p.add(tee));
        let (x, y) = (p.add(fast), p.add(slow));
        link(&mut p, a.src(OUT), t.sink(IN));
        link(&mut p, t.src(OUT), x.sink(IN));
        link(&mut p, t.src(TEE_Y), y.sink(IN));
        let stats = run_without_a_missed_wake(p);
        assert_eq!((n(&seen_fast.buffers), n(&seen_slow.buffers)), (400, 400));
        assert!(stats.parks > 20, "the slow branch held the tee back, {stats:?}");
    }

    #[test]
    fn a_fan_in_runs_on_signals_alone() {
        let mut p = Pipeline::new();
        let sink = a_sink();
        let seen = Arc::clone(&sink.seen);
        let mux = Probe { reads: &[0, 1], outs: &[0], ..probe(&A_MUX) };
        let (a, b) = (p.add(a_source(10_000)), p.add(a_source(3_000)));
        let (m, s) = (p.add(mux), p.add(sink));
        link(&mut p, a.src(OUT), m.sink(IN));
        link(&mut p, b.src(OUT), m.sink(MUX_B));
        link(&mut p, m.src(OUT), s.sink(IN));
        run_without_a_missed_wake(p);
        assert_eq!(n(&seen.buffers), 13_000);
    }

    #[test]
    fn a_pool_smaller_than_the_rings_runs_on_signals_alone() {
        let mut p = Pipeline::new();
        let sink = a_sink();
        let seen = Arc::clone(&sink.seen);
        let filter = Probe { outs: &[0], ..probe(&A_FILTER) };
        let (a, b, c) = (p.add(a_source(20_000)), p.add(filter), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        link(&mut p, b.src(OUT), c.sink(IN));
        p.set_pool(64, 2);
        let tap = p.tap_handle();
        let stats = run_without_a_missed_wake(p);
        assert_eq!(n(&seen.buffers), 20_000);
        assert!(stats.parks > 0, "the source parked on the pool");
        assert!(tap.pool().high_water <= 2);
    }

    #[test]
    fn a_leaky_ring_runs_on_signals_alone_and_loses_no_mark() {
        let mut p = Pipeline::new();
        let sink = Probe { slow: Duration::from_micros(200), ..a_sink() };
        let seen = Arc::clone(&sink.seen);
        let (a, b) = (p.add(source(2_000)), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        p.set_queue_leaky(b, Leaky::DropNewest);
        let tap = p.tap_handle();
        run_without_a_missed_wake(p);
        let drops = tap.snapshot(a).unwrap().drops;
        assert!(drops > 0, "a source this far ahead of its sink overflows the ring");
        assert_eq!(n(&seen.buffers) as u64 + drops, 2_000, "delivered or counted");
    }

    #[test]
    fn pause_resume_and_seek_while_running_need_no_backstop() {
        const BUFFERS: u32 = 120;
        let mut p = Pipeline::new();
        let src = Probe {
            reload: Some(BUFFERS),
            pts_step: Timestamp::from_millis(1),
            ..a_source(BUFFERS)
        };
        let sink = Probe { paced: true, ..a_sink() };
        let (seen_src, seen_sink) = (Arc::clone(&src.seen), Arc::clone(&sink.seen));
        let (a, b) = (p.add(src), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        let (pause, seeker) = (p.control(), p.control());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));

        let wait_for = |what: &dyn Fn() -> bool| {
            let at = Instant::now();
            while !what() {
                assert!(at.elapsed() < Duration::from_secs(5), "never happened");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        wait_for(&|| n(&seen_sink.buffers) >= 10);
        pause.pause();
        wait_for(&|| n(&seen_src.paused) == 1 && n(&seen_sink.paused) == 1);
        let frozen = n(&seen_sink.buffers);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(n(&seen_sink.buffers), frozen, "nothing renders once the sink applied it");
        pause.resume();
        wait_for(&|| n(&seen_src.resumed) == 1);
        wait_for(&|| n(&seen_sink.buffers) >= frozen + 10);
        let _ = seeker.seek_bytes(Timestamp::ZERO, &[(crate::command::ALL, 0, Timestamp::ZERO)]);
        wait_for(&|| n(&seen_src.flushes) == 1 && n(&seen_sink.flushes) == 1);

        run.join().unwrap();
        assert!(n(&seen_sink.buffers) >= BUFFERS, "the whole stream again after the seek");
        assert_eq!((n(&seen_src.eos), n(&seen_sink.eos)), (1, 1));
    }

    #[test]
    fn pause_resume_and_seek_each_wake_a_pipeline_at_rest() {
        let mut p = Pipeline::new();
        let src = Probe { emit: None, reload: Some(3), ..probe(&SRC) };
        let sink = a_sink();
        let (seen_src, seen_sink) = (Arc::clone(&src.seen), Arc::clone(&sink.seen));
        let (a, b) = (p.add(src), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        let (pause, seeker) = (p.control(), p.control());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));

        let wait_for = |what: &dyn Fn() -> bool| {
            let at = Instant::now();
            while !what() {
                assert!(at.elapsed() < Duration::from_secs(5), "never happened");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let both = |count: &dyn Fn(&Seen) -> u32, want: u32| {
            count(&seen_src) == want && count(&seen_sink) == want
        };
        wait_for(&|| n(&seen_src.calls) >= 1 && n(&seen_sink.calls) >= 1);
        let rest = || std::thread::sleep(Duration::from_millis(20));
        rest();
        pause.pause();
        wait_for(&|| both(&|s| n(&s.paused), 1));
        rest();
        pause.resume();
        wait_for(&|| both(&|s| n(&s.resumed), 1));
        rest();
        let _ = seeker.seek_bytes(Timestamp::ZERO, &[(crate::command::ALL, 0, Timestamp::ZERO)]);
        run.join().unwrap();
        assert!(both(&|s| n(&s.flushes), 1));
        assert_eq!(n(&seen_sink.buffers), 3, "the reloaded stream played out");
    }

    #[test]
    fn a_property_set_and_a_stop_each_wake_a_pipeline_at_rest() {
        let mut p = Pipeline::new();
        let knob = probe(&A_KNOB);
        let (seen_src, seen_knob) = (Arc::default(), Arc::clone(&knob.seen));
        let src = Probe { emit: None, seen: Arc::clone(&seen_src), ..probe(&SRC) };
        let (a, b) = (p.add(src), p.add(knob));
        link(&mut p, a.src(OUT), b.sink(IN));
        let (props, stop) = (p.prop_handle(), p.control());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));

        let wait_for = |what: &dyn Fn() -> bool| {
            let at = Instant::now();
            while !what() {
                assert!(at.elapsed() < Duration::from_secs(5), "never happened");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let seen_src: Arc<Seen> = seen_src;
        wait_for(&|| n(&seen_src.calls) >= 1 && n(&seen_knob.calls) >= 1);
        std::thread::sleep(Duration::from_millis(20));
        props.set(b, "gain", Value::Int(3)).unwrap();
        wait_for(&|| n(&seen_knob.props) == 1);
        std::thread::sleep(Duration::from_millis(20));
        stop.stop();
        run.join().unwrap();
        assert_eq!((n(&seen_src.stops), n(&seen_knob.stops)), (1, 1));
    }

    fn wait_for(what: impl Fn() -> bool) {
        let at = Instant::now();
        while !what() {
            assert!(at.elapsed() < Duration::from_secs(5), "never happened");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn paced_pipeline(buffers: u32, step: Timestamp) -> (Pipeline, Arc<Seen>, Arc<Seen>) {
        let mut p = Pipeline::new();
        let src = Probe { reload: Some(buffers), pts_step: step, ..a_source(buffers) };
        let sink = Probe { paced: true, ..a_sink() };
        let (seen_src, seen_sink) = (Arc::clone(&src.seen), Arc::clone(&sink.seen));
        let (a, b) = (p.add(src), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        (p, seen_src, seen_sink)
    }

    #[test]
    fn a_sink_on_a_mock_clock_renders_each_buffer_at_the_advance_that_reaches_it() {
        let step = MS(10);
        let (mut p, _, sink) = paced_pipeline(10, step);
        let clock = MockClock::new();
        p.set_clock(Arc::new(clock.clone()));
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));

        wait_for(|| n(&sink.buffers) == 1);
        for k in 1..10 {
            clock.advance(MS(9));
            std::thread::sleep(Duration::from_millis(3));
            assert_eq!(n(&sink.buffers), k, "rendered ahead of its deadline");
            clock.advance(MS(1));
            wait_for(|| n(&sink.buffers) == k + 1);
        }
        let stats = run.join().unwrap();
        assert!(stats.parks >= 10, "it slept between buffers, {stats:?}");
        let renders = sink.renders.lock().unwrap();
        let want: Vec<_> =
            (0..10).map(|k| (Timestamp(k * step.0), Timestamp(k * step.0))).collect();
        assert_eq!(*renders, want, "each at exactly its own deadline");
    }

    #[test]
    fn pause_resume_and_seek_reach_a_sink_waiting_on_a_far_mock_deadline() {
        let hour = Timestamp::from_secs(3600);
        let (mut p, _, sink) = paced_pipeline(3, hour);
        let clock = MockClock::new();
        p.set_clock(Arc::new(clock.clone()));
        let (pause, seeker, tap) = (p.control(), p.control(), p.tap_handle());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));
        let settle = || std::thread::sleep(Duration::from_millis(5));

        wait_for(|| n(&sink.buffers) == 1);
        settle();
        pause.pause();
        wait_for(|| n(&sink.paused) == 1);
        clock.advance(hour);
        settle();
        assert_eq!(n(&sink.buffers), 1, "rendered while paused");
        pause.resume();
        wait_for(|| n(&sink.resumed) == 1);
        settle();
        assert_eq!(n(&sink.buffers), 1, "the paused hour is not running time");

        let _ = seeker.seek_bytes(
            Timestamp::from_secs(1800),
            &[(crate::command::ALL, 0, Timestamp::from_secs(1800))],
        );
        wait_for(|| n(&sink.flushes) == 1 && tap.snapshot(ElementId(1)).unwrap().drops == 1);
        settle();
        assert_eq!(n(&sink.buffers), 1);
        clock.advance(Timestamp::from_secs(1800));
        wait_for(|| n(&sink.buffers) == 2);
        clock.advance(hour);
        run.join().unwrap();
        assert_eq!((n(&sink.buffers), n(&sink.eos)), (3, 1));
        let renders = sink.renders.lock().unwrap();
        let at = |s: u64| Timestamp::from_secs(s);
        assert_eq!(*renders, [(at(0), at(0)), (at(3600), at(3600)), (at(7200), at(7200))]);
    }

    #[test]
    fn a_skip_on_a_mock_clock_renders_the_buffer_at_the_point_at_once() {
        let at = |s: u64| Timestamp::from_secs(s);
        let mut p = Pipeline::new();
        let src = Probe { sent: 3600, pts_step: at(1), ..a_source(60) };
        let sink = Probe { paced: true, log: true, ..a_sink() };
        let seen = Arc::clone(&sink.seen);
        let (a, b) = (p.add(src), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        let clock = MockClock::new();
        p.set_clock(Arc::new(clock.clone()));
        let (seeker, stop, tap) = (p.control(), p.control(), p.tap_handle());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));
        let settle = || std::thread::sleep(Duration::from_millis(5));

        wait_for(|| n(&seen.calls) >= 1);
        clock.advance(at(3600));
        wait_for(|| n(&seen.buffers) == 1);
        settle();
        seeker.skip_to(at(3610));
        wait_for(|| n(&seen.buffers) == 2);
        settle();
        assert_eq!(n(&seen.buffers), 2, "the buffer after the point waits for its second");
        assert_eq!(*seen.renders.lock().unwrap(), [(at(3600), at(3600)), (at(3610), at(3610))]);
        assert_eq!(*seen.log.lock().unwrap(), [Saw::Buffer, Saw::Skip(at(3610)), Saw::Buffer]);
        let sink = tap.snapshot(b).unwrap();
        assert_eq!((sink.drops, n(&seen.flushes)), (9, 0), "3601 to 3609, counted, and no flush");
        assert_eq!(tap.now(), at(3610), "running time reads the point");
        clock.advance(at(1));
        wait_for(|| n(&seen.buffers) == 3);
        stop.stop();
        let stats = run.join().unwrap();
        assert_eq!(stats.tick_expiries, 0, "{stats:?}");
        assert_eq!(seen.renders.lock().unwrap().last(), Some(&(at(3611), at(3611))));
    }

    #[test]
    fn a_sink_on_the_real_clock_paces_at_the_media_rate() {
        let (p, _, sink) = paced_pipeline(30, MS(5));
        let at = Instant::now();
        run_without_a_missed_wake(p);
        let took = at.elapsed();
        assert!(took >= Duration::from_millis(145), "faster than real time, {took:?}");
        let renders = sink.renders.lock().unwrap();
        assert_eq!(renders.len(), 30);
        assert!(renders.iter().all(|(pts, now)| now >= pts), "never ahead of a deadline");
        let mut late: Vec<u64> = renders.iter().map(|(pts, now)| now.0 - pts.0).collect();
        late.sort_unstable();
        let median = Duration::from_nanos(late[late.len() / 2]);
        assert!(median < Duration::from_millis(500), "median lateness {median:?}");
    }

    #[test]
    fn a_stop_during_a_long_wait_on_the_real_clock_returns_promptly() {
        let (mut p, _, sink) = paced_pipeline(2, Timestamp::from_secs(3600));
        let stop = p.control();
        p.backstop = Duration::from_secs(60);
        let run = std::thread::spawn(move || p.run());
        wait_for(|| n(&sink.buffers) == 1);
        std::thread::sleep(Duration::from_millis(20));
        let at = Instant::now();
        stop.stop();
        run.join().unwrap().unwrap();
        assert!(at.elapsed() < Duration::from_secs(10), "{:?}", at.elapsed());
        assert_eq!((n(&sink.buffers), n(&sink.stops)), (1, 1));
    }

    #[test]
    fn a_resume_while_playing_runs_a_parked_group_without_the_backstop() {
        let gate = Arc::new(AtomicBool::new(false));
        let mut p = Pipeline::new();
        let sink = Probe { gate: Some(Arc::clone(&gate)), ..a_sink() };
        let seen = Arc::clone(&sink.seen);
        let (a, b) = (p.add(a_source(50)), p.add(sink));
        link(&mut p, a.src(OUT), b.sink(IN));
        let (pause, control) = (p.control(), p.control());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));
        wait_for(|| n(&seen.calls) >= 1);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(n(&seen.buffers), 0);
        gate.store(true, Ordering::Release);
        pause.resume();
        run.join().unwrap();
        assert_eq!(n(&seen.buffers), 50);
        assert_eq!(control.log.head(), 0, "a resume while playing is not a command");
    }

    fn at_rest() -> (Pipeline, Arc<Seen>, Arc<Seen>) {
        let (mut p, src, sink) = paced_pipeline(3, Timestamp::from_secs(3600));
        p.set_clock(Arc::new(MockClock::new()));
        (p, src, sink)
    }

    #[test]
    fn every_group_applies_every_command_of_a_burst_longer_than_the_log() {
        const PAIRS: u32 = 2 * COMMAND_LOG_CAP as u32;
        let (p, src, sink) = at_rest();
        let (pause, stop, control) = (p.control(), p.control(), p.control());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));
        wait_for(|| n(&sink.buffers) == 1);
        for _ in 0..PAIRS {
            pause.pause();
            pause.resume();
        }
        wait_for(|| control.applied_by_all(2 * PAIRS as u64 - 1));
        for seen in [&src, &sink] {
            assert_eq!((n(&seen.paused), n(&seen.resumed)), (PAIRS, PAIRS), "none coalesced");
        }
        assert_eq!(n(&sink.buffers), 1, "and nothing rendered for it");
        stop.stop();
        run.join().unwrap();
        assert!(control.applied_by_all(2 * PAIRS as u64), "a group that left holds nothing back");
    }

    #[test]
    fn a_burst_of_seeks_leaves_every_group_on_the_last_target() {
        const SEEKS: u64 = 3 * COMMAND_LOG_CAP as u64;
        let (p, src, sink) = at_rest();
        let (seeker, stop, control) = (p.control(), p.control(), p.control());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));
        wait_for(|| n(&sink.buffers) == 1);
        for k in 1..=SEEKS {
            let seek = Command::Seek { seek_gen: 0, to_time: Timestamp::from_secs(k) };
            seeker.send_with(seek, &[(crate::command::ALL, k, Timestamp::from_secs(k))]);
        }
        wait_for(|| control.applied_by_all(SEEKS - 1));
        for seen in [&src, &sink] {
            let targets = seen.targets.lock().unwrap();
            assert_eq!(targets.last(), Some(&(SEEKS, Timestamp::from_secs(SEEKS))));
            let whole = targets.iter().all(|(byte, time)| *time == Timestamp::from_secs(*byte));
            assert!(whole, "a target is one seek's, never half of two");
            assert!(targets.windows(2).all(|w| w[0].0 <= w[1].0), "in order");
            assert_eq!(targets.len() as u32, n(&seen.flushes));
        }
        let _ = seeker
            .seek_bytes(Timestamp::ZERO, &[(crate::command::ALL, SEEKS + 1, Timestamp::ZERO)]);
        wait_for(|| sink.renders.lock().unwrap().len() == 2);
        let played = (Timestamp::ZERO, Timestamp::ZERO);
        assert_eq!(*sink.renders.lock().unwrap(), [played, played]);
        stop.stop();
        run.join().unwrap();
    }

    fn uneven(long: u32, reload: u32, step: Timestamp) -> (Pipeline, [Arc<Seen>; 2]) {
        let stamp: Arc<AtomicU32> = Arc::default();
        let stamped = |probe: Probe| Probe { stamp: Arc::clone(&stamp), log: true, ..probe };
        let mut p = Pipeline::new();
        let short = stamped(a_sink());
        let paced = stamped(Probe { paced: true, ..a_sink() });
        let seen = [Arc::clone(&short.seen), Arc::clone(&paced.seen)];
        let (a, b) = (p.add(stamped(Probe { reload: Some(4), ..a_source(4) })), p.add(short));
        link(&mut p, a.src(OUT), b.sink(IN));
        let src = Probe { reload: Some(reload), pts_step: step, ..a_source(long) };
        let (c, d) = (p.add(stamped(src)), p.add(paced));
        link(&mut p, c.src(OUT), d.sink(IN));
        (p, seen)
    }

    #[test]
    fn a_sink_whose_stream_is_short_rests_and_the_run_ends_with_the_long_one() {
        let (p, [short, long]) = uneven(30, 0, MS(5));
        let at = Instant::now();
        run_without_a_missed_wake(p);
        let took = at.elapsed();
        assert!(took >= Duration::from_millis(145), "the short stream ended the run, {took:?}");
        assert_eq!((n(&short.buffers), n(&short.eos), n(&short.stops)), (4, 1, 1));
        assert_eq!((n(&long.buffers), n(&long.eos), n(&long.stops)), (30, 1, 1));
        assert!(n(&short.order) > 4 && n(&long.order) > 4, "a stop is the end of the run");
    }

    #[test]
    fn a_seek_after_the_short_sink_ended_plays_both_branches_again_from_the_target() {
        let (p, [short, long]) = uneven(400, 8, MS(25));
        let (seeker, control) = (p.control(), p.control());
        let run = std::thread::spawn(move || run_without_a_missed_wake(p));
        wait_for(|| n(&short.eos) == 1 && n(&long.buffers) >= 1);
        assert!(!control.is_finished() && n(&long.eos) == 0, "one sink of two has ended");
        let _ = seeker.seek_bytes(Timestamp::ZERO, &[(crate::command::ALL, 0, Timestamp::ZERO)]);
        run.join().unwrap();
        assert!(control.is_finished());
        let replay =
            |buffers| [vec![Saw::Flush], vec![Saw::Buffer; buffers], vec![Saw::Eos]].concat();
        let whole = [vec![Saw::Buffer; 4], vec![Saw::Eos], replay(4)].concat();
        assert_eq!(*short.log.lock().unwrap(), whole);
        let seen = long.log.lock().unwrap();
        assert!(seen.ends_with(&replay(8)) && n(&long.eos) == 1, "{seen:?}");
        let renders = long.renders.lock().unwrap();
        let after: Vec<Timestamp> = renders[renders.len() - 8..].iter().map(|r| r.0).collect();
        assert_eq!(after, (0..8).map(|k| MS(25 * k)).collect::<Vec<_>>(), "from pts zero");
        assert_eq!((n(&short.stops), n(&long.stops)), (1, 1));
    }

    use crate::level::{Held, Limit};

    fn timed(pool: &Pool, first: u64, rows: u64) -> Batch {
        let mut b = Batch::new(BYTES);
        for k in first..first + rows {
            let mut row = buf(pool);
            (row.pts, row.duration) = (MS(10 * k), MS(10));
            b.push(row);
        }
        b
    }

    fn limit(r: &mut Rig, limit: Limit) {
        r.group.downstream[0].level.set_limit(limit);
        r.group.downstream[0].limited = true;
    }

    fn feed(r: &Rig, link: usize, b: Batch) {
        let (bytes, from, to) = b.span();
        assert!(r.up[link].0.try_push(b).is_ok());
        r.group.upstream[link].level.feed(bytes, from, to);
    }

    fn drain(r: &Rig, link: usize, taken: &mut u64) -> usize {
        let mut rows = 0;
        while let Some(b) = r.down[link].0.try_pop() {
            let (bytes, _, to) = b.span();
            (*taken, rows) = (*taken + bytes, rows + b.len());
            r.group.downstream[link].level.take(*taken, to);
        }
        rows
    }

    #[test]
    fn what_waits_in_the_heads_input_is_still_held_by_its_link() {
        let mut r = rig(vec![Probe { paced: true, ..probe(&A_SINK) }], &[0], &[]);
        let level = Arc::clone(&r.group.upstream[0].level);
        feed(&r, 0, timed(&r.pool, 0, 3));
        assert_eq!(level.held(), Held { bytes: 24, time: MS(30) }, "in the ring");
        for left in [2u64, 1, 0] {
            r.group.pass().unwrap();
            assert_eq!(level.held(), Held { bytes: 8 * left, time: MS(10 * left) }, "{left} left");
            r.clock.advance(MS(10));
        }
        assert_eq!(n(&r.seen[0].buffers), 3);
    }

    #[test]
    fn a_link_at_its_limit_rests_the_group_and_a_take_signals_it() {
        let mut r = rig(vec![Probe { pts_step: MS(10), ..source(100) }], &[], &[0]);
        limit(&mut r, Limit::time(MS(25)));
        assert_eq!(settle(&mut r), ring(0));
        assert_eq!(r.group.counters[0].snapshot().buffers_out, 4);
        assert!(
            r.group.downstream[0].staged.is_none(),
            "it rests on the level, with nothing staged"
        );
        assert_eq!(r.group.downstream[0].level.held().time, MS(30));
        assert_eq!(r.group.pass().unwrap(), ring(0));
        assert_eq!(n(&r.seen[0].calls), 4, "no element ran");
        assert!(r.group.downstream[0].room.take(), "the flag is up, so a take signals");
        let first = r.down[0].0.try_pop().expect("a batch");
        r.group.downstream[0].level.take(first.total_bytes(), MS(10));
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(settle(&mut r), ring(0));
        assert_eq!(r.group.counters[0].snapshot().buffers_out, 5);
    }

    /// A demuxer whose second pad alone is linked, its audio, hands that pad's buffers to the
    /// member fused after it, not its first pad's. AVI's AC-3 went to its decoder as the video.
    #[test]
    fn an_inline_hop_takes_the_linked_pad_not_the_first() {
        static DEMUX: ElementDesc = desc("demux", &ONE_SINK, &TWO_SRCS);
        let demux = Probe { outs: &[1], ..probe(&DEMUX) };
        let inline = Probe { outs: &[0], ..probe(&FILTER) };
        let mut r = rig(vec![demux, inline], &[0], &[0]);
        r.group.ctxs[0].set_linked_pads(vec![false, true]);
        assert_eq!(r.group.ctxs[0].hop_pad(), SrcPad(1));
        feed(&r, 0, timed(&r.pool, 0, 10));
        for _ in 0..16 {
            let _ = r.group.pass().unwrap();
        }
        assert_eq!(
            n(&r.seen[1].buffers),
            10,
            "every buffer of the linked pad reached the next member"
        );
    }

    #[test]
    fn a_pass_moves_no_more_than_the_link_has_room_for() {
        let filter = Probe { outs: &[0], ..probe(&A_FILTER) };
        let inline = Probe { outs: &[0], ..probe(&FILTER) };
        let mut r = rig(vec![filter, inline], &[0], &[0]);
        limit(&mut r, Limit::time(MS(25)));
        feed(&r, 0, timed(&r.pool, 0, 10));
        let mut taken = 0;
        let mut got = Vec::new();
        for _ in 0..16 {
            if r.group.pass().unwrap() == ring(0) {
                assert!(
                    r.group.downstream[0].level.held().time <= MS(25 + 10),
                    "over by a buffer at most"
                );
                got.push(drain(&r, 0, &mut taken));
            }
        }
        got.push(drain(&r, 0, &mut taken));
        assert_eq!(got, [3, 3, 3, 1]);
        assert_eq!((n(&r.seen[0].buffers), n(&r.seen[1].buffers)), (10, 10));
        assert_eq!(r.group.upstream[0].level.held(), Held::default(), "all of it taken");
    }

    #[test]
    fn rows_with_no_stamp_are_not_held_back_by_a_limit_in_time() {
        let mut r = rig(vec![Probe { outs: &[0], ..probe(&A_FILTER) }], &[0], &[0]);
        limit(&mut r, Limit::time(MS(25)));
        feed(&r, 0, batch(&r.pool, 10));
        r.group.pass().unwrap();
        assert_eq!(r.down[0].0.try_pop().map(|b| b.len()), Some(10), "bytes pass whole");
        assert_eq!(r.group.pass().unwrap(), IDLE);
    }

    #[test]
    fn a_limit_in_bytes_rests_the_group_too() {
        let mut r = rig(vec![Probe { outs: &[0], sip: true, ..probe(&A_FILTER) }], &[0], &[0]);
        limit(&mut r, Limit::bytes(20));
        feed(&r, 0, batch(&r.pool, 10));
        assert_eq!(settle(&mut r), ring(0));
        assert_eq!(r.group.downstream[0].level.held().bytes, 24);
        assert_eq!(n(&r.seen[0].buffers), 3);
        assert_eq!(r.group.upstream[0].level.held().bytes, 56, "the rest is still its upstream's");
    }

    #[test]
    fn a_source_that_asks_for_room_is_never_staged_and_a_pop_signals_it() {
        let mut r = rig(vec![Probe { polite: true, ..source(100) }], &[], &[0]);
        assert_eq!(settle(&mut r), IDLE);
        assert_eq!(r.group.counters[0].snapshot().buffers_out, 4);
        assert!(r.group.downstream[0].staged.is_none());
        let calls = n(&r.seen[0].calls);
        assert_eq!(r.group.pass().unwrap(), IDLE);
        assert_eq!(
            n(&r.seen[0].calls),
            calls + 1,
            "the element still runs, to receive into its store"
        );
        assert!(
            r.group.downstream[0].room.take(),
            "the flag is up, so the pop that makes room signals"
        );
        assert!(r.down[0].0.try_pop().is_some());
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(r.group.counters[0].snapshot().buffers_out, 5);
    }

    #[test]
    fn a_seek_starts_the_level_over_from_the_first_row_behind_it() {
        let src = Probe { pts_step: MS(10), reload: Some(100), ..source(100) };
        let mut r = rig(vec![src], &[], &[0]);
        limit(&mut r, Limit::time(MS(25)));
        assert_eq!(settle(&mut r), ring(0));
        let level = Arc::clone(&r.group.downstream[0].level);
        assert_eq!(level.held().time, MS(30));
        seek(&r);
        assert_eq!(r.group.pass().unwrap(), Outcome::Progressed);
        assert_eq!(level.held().time, Timestamp::ZERO);
        let mut taken = 0;
        assert_eq!(drain(&r, 0, &mut taken), 4, "the stream before");
        assert_eq!(settle(&mut r), ring(0));
        assert!(r.group.downstream[0].staged.is_none());
        assert_eq!(level.held().time, MS(30), "at the limit again");
        level.restarted(1);
        assert_eq!(level.held().time, MS(30), "at no place in it yet");
        level.take(taken, MS(20));
        assert_eq!(level.held().time, MS(10));
    }

    const WEAVE_LIMIT: Timestamp = Timestamp::from_millis(100);

    fn skewed(k: u64) -> (u32, Timestamp) {
        match k.checked_sub(40) {
            None => (0, MS(10 * k)),
            Some(j) if j.is_multiple_of(2) => (1, MS(10 * (j / 2))),
            Some(j) => (0, MS(400 + 10 * (j / 2))),
        }
    }

    fn only_first(k: u64) -> (u32, Timestamp) {
        (0, MS(10 * k))
    }

    fn weave_link(d: &mut DownLink) {
        d.level.set_limit(Limit::time(WEAVE_LIMIT));
        (d.limited, d.base) = (true, WEAVE_LIMIT);
    }

    fn weave_rig(lay: fn(u64) -> (u32, Timestamp), rows: u32) -> Rig {
        let src = Probe { lay: Some(lay), emit: Some(rows), ..probe(&SRC2) };
        let mut r = rig_sized(vec![src], &[], &[0, 1], 1024, Leaky::Block, 1024);
        r.group.downstream.iter_mut().for_each(weave_link);
        r.group.weave.recount(&r.group.downstream);
        r
    }

    fn limit_of(r: &Rig, link: usize) -> Timestamp {
        r.group.downstream[link].level.limit().time
    }

    fn weave_until_rest(r: &mut Rig, take: impl Fn(&Rig) -> bool) -> Outcome {
        let mut taken = 0;
        for _ in 0..2_000 {
            match r.group.pass().unwrap() {
                Outcome::Progressed => {}
                other => return other,
            }
            if take(r) {
                drain(r, 1, &mut taken);
            }
        }
        panic!("the group never came to rest");
    }

    #[test]
    fn a_full_link_grows_while_a_sibling_starves_and_rests_once_it_is_fed() {
        let mut r = weave_rig(skewed, 400);
        let starving = |r: &Rig| r.group.downstream[1].level.fed_to().is_none();
        assert_eq!(weave_until_rest(&mut r, starving), ring(0));
        let (ahead, behind) =
            (r.group.downstream[0].level.held().time, r.group.downstream[1].level.held().time);
        assert!(
            ahead > MS(400) && behind > Timestamp::ZERO,
            "it ran past the skew, {ahead:?} {behind:?}"
        );
        assert_eq!(r.group.weave.value, MS(600), "three halves of the 400 ms spread");
        assert_eq!(
            (limit_of(&r, 0), limit_of(&r, 1)),
            (MS(700), MS(700)),
            "every timed link holds it past its own"
        );
        assert_eq!(ahead, MS(700), "rests at the grown limit");
    }

    #[test]
    fn a_full_link_rests_when_its_sibling_is_sparse_or_ended() {
        for sparse in [true, false] {
            let mut r = weave_rig(skewed, 400);
            match sparse {
                true => r.group.downstream[1].sparse = true,
                false => r.group.downstream[1].level.end(),
            }
            r.group.weave.recount(&r.group.downstream);
            assert_eq!(weave_until_rest(&mut r, |_| true), ring(0));
            assert_eq!(r.group.downstream[0].level.held().time, MS(100), "sparse {sparse}");
            assert_eq!(limit_of(&r, 0), WEAVE_LIMIT, "sparse {sparse}");
        }
    }

    #[test]
    fn a_link_full_in_bytes_rests_the_tail_while_a_sibling_starves() {
        let mut r = weave_rig(skewed, 400);
        r.group.downstream[0].level.set_limit(Limit { bytes: 64, time: WEAVE_LIMIT });
        assert_eq!(weave_until_rest(&mut r, |_| true), ring(0));
        assert_eq!(
            r.group.downstream[0].level.held(),
            Held { bytes: 64, time: MS(80) },
            "the byte cap holds"
        );
        assert_eq!(limit_of(&r, 0), WEAVE_LIMIT);
    }

    fn alternate_then_first(k: u64) -> (u32, Timestamp) {
        match k {
            0..10 => ((k % 2) as u32, MS(10 * (k / 2))),
            _ => (0, MS(50 + 10 * (k - 10))),
        }
    }

    #[test]
    fn an_ended_link_leaves_the_measure() {
        let mut r = weave_rig(alternate_then_first, 200);
        let mut taken = [0u64; 2];
        for k in 0..200 {
            if k == 10 {
                r.group.downstream[1].level.end();
            }
            r.group.pass().unwrap();
            drain(&r, 0, &mut taken[0]);
            drain(&r, 1, &mut taken[1]);
        }
        assert!(r.group.weave.value <= MS(15), "{:?}", r.group.weave.value);
    }

    #[test]
    fn a_cut_link_leaves_the_rules() {
        let mut r = weave_rig(skewed, 400);
        assert_eq!(r.group.weave.timed, 2);
        r.group.cut_down(1).unwrap();
        assert_eq!(r.group.weave.timed, 1);
        assert_eq!(weave_until_rest(&mut r, |_| false), ring(0));
        assert_eq!(limit_of(&r, 0), WEAVE_LIMIT, "no sibling, no growth");
    }

    #[test]
    fn a_sparse_link_holds_back_no_input_of_the_pass() {
        let mut r = rig(vec![Probe { outs: &[0], ..probe(&A_FILTER) }], &[0], &[0]);
        limit(&mut r, Limit::time(MS(25)));
        r.group.downstream[0].sparse = true;
        feed(&r, 0, timed(&r.pool, 0, 10));
        r.group.pass().unwrap();
        assert_eq!(r.down[0].0.try_pop().map(|b| b.len()), Some(10), "the pass took all of it");
    }

    fn lockstep(k: u64) -> (u32, Timestamp) {
        ((k % 2) as u32, MS(10 * (k / 2)))
    }

    #[test]
    fn a_drained_sibling_not_behind_grows_nothing() {
        let mut r = weave_rig(lockstep, 400);
        assert_eq!(weave_until_rest(&mut r, |_| true), ring(0));
        assert_eq!(
            r.group.weave.value,
            MS(15),
            "three halves of the 10 ms the two lie apart, no growth"
        );
        assert!(
            r.group.downstream[0].level.held().time <= MS(125),
            "rests at its limit and the measure"
        );
        assert!(!r.group.ctxs[0].behind(), "nothing is behind");
    }

    #[test]
    fn a_tail_says_a_track_is_behind_while_one_starves_behind_another() {
        let mut r = weave_rig(skewed, 400);
        for _ in 0..5 {
            r.group.pass().unwrap();
        }
        assert!(r.group.ctxs[0].behind(), "pad 1 fed nothing while pad 0 holds data");
    }

    #[test]
    fn a_sparse_link_full_in_time_never_rests_the_tail() {
        let mut r = weave_rig(only_first, 30);
        r.group.downstream[0].sparse = true;
        r.group.weave.recount(&r.group.downstream);
        assert_eq!(weave_until_rest(&mut r, |_| true), IDLE);
        assert_eq!(r.group.downstream[0].level.held().time, MS(300), "all of it went out");
    }

    fn bare_link(pad: u32) -> DownLink {
        let (data, _rx) = spsc_leaky::<Batch>(4, Leaky::Block);
        let (_tx, shells) = spsc::<Batch>(4);
        let (peer, room) = (Arc::new(Station::new()), Arc::new(Room::new()));
        let (level, slot) = (Arc::default(), Arc::default());
        let mut d = DownLink {
            pad: SrcPad(pad),
            link: LinkId(pad),
            data,
            shells,
            leaky: Leaky::Block,
            peer,
            room,
            staged: None,
            level,
            limited: false,
            base: Timestamp::NONE,
            sparse: false,
            mark: None,
            slot,
            seen: 0,
        };
        weave_link(&mut d);
        d
    }

    #[test]
    fn the_interleave_rises_at_once_where_the_link_ahead_is_full_and_lowers_only_past_the_hysteresis()
     {
        let links = [bare_link(0), bare_link(1)];
        let mut w = Weave::default();
        w.recount(&links);
        let fed = |link: usize, from: u64, to: u64| links[link].level.feed(8, MS(from), MS(to));
        fed(0, 0, 300);
        links[0].level.take(8, MS(250));
        fed(1, 0, 10);
        w.measure(&links);
        assert_eq!(w.value, Timestamp::ZERO, "the link ahead has room, nothing is raised");
        fed(0, 300, 360);
        assert!(links[0].level.full());
        w.measure(&links);
        assert_eq!(w.value, MS(525), "raised at once to three halves of the 350 ms spread");
        assert_eq!(links[1].level.limit().time, MS(625), "every timed link holds it past its own");
        // Pad 1 catches up, and the value lowers once the least link moved on twice the settle.
        fed(1, 10, 400);
        fed(0, 360, 410);
        w.measure(&links);
        assert_eq!(w.value, MS(525), "the least link moved on 390 ms");
        fed(1, 400, 1_060);
        fed(0, 410, 1_065);
        w.measure(&links);
        assert_eq!(
            w.value,
            Timestamp::from_micros(7_500),
            "1050 ms on, lowered to three halves of the 5 ms spread"
        );
        assert_eq!(links[0].level.limit().time, Timestamp::from_micros(107_500));
    }

    #[test]
    fn a_flush_starts_the_measure_over_and_keeps_the_grown_limit() {
        let mut r = weave_rig(skewed, 400);
        let starving = |r: &Rig| r.group.downstream[1].level.fed_to().is_none();
        weave_until_rest(&mut r, starving);
        let grown = r.group.weave.value;
        seek(&r);
        r.group.pass().unwrap();
        assert_eq!(r.group.weave.value, grown);
        let limit = grown.saturating_add(WEAVE_LIMIT);
        assert_eq!((limit_of(&r, 0), limit_of(&r, 1)), (limit, limit));
        assert_eq!(r.group.weave.at, Timestamp::NONE, "measured again behind the seek");
    }

    #[test]
    fn a_link_spliced_in_stands_at_the_grown_limit() {
        let mut r = weave_rig(skewed, 400);
        let starving = |r: &Rig| r.group.downstream[1].level.fed_to().is_none();
        weave_until_rest(&mut r, starving);
        r.group.splice_adds(vec![bare_link(1)], vec![], None, &[]).unwrap();
        assert_eq!(r.group.weave.timed, 3);
        assert_eq!(limit_of(&r, 2), MS(700));
    }

    #[test]
    fn a_tail_past_the_cap_rests_and_warns_once() {
        let mut r = weave_rig(only_first, 1_000);
        assert_eq!(weave_until_rest(&mut r, |_| false), ring(0));
        assert_eq!(r.group.downstream[0].level.held().time, crate::weave::MAX_INTERLEAVE);
        assert_eq!(r.group.pass().unwrap(), ring(0));
        let warned = std::iter::from_fn(|| r.bus.try_recv())
            .filter(|m| {
                matches!(
                    m,
                    BusMessage::Warning { error: Error::Graph(GraphError::Interleaved { .. }), .. }
                )
            })
            .count();
        assert_eq!(warned, 1);
    }
}
