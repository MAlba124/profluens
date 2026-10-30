use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::batch::Batch;
use crate::bus::{Bus, BusMessage, BusSender};
use crate::clock::{Clock, InstantClock};
use crate::command::{Control, Cursor, Target};
use crate::counters::{
    CHUNK, Chunks, CounterSnapshot, ElementCounters, LatencyReport, Roster, Row, TapHandle, View,
};
use crate::couple::{Coupling, Rule};
use crate::ctx::{AddedPad as DynPad, Ctx, PadFormats, PadOffers, Stream};
use crate::element::{Element, ElementDesc, InputPolicy, PadDesc, SchedHint};
use crate::error::{
    EmptyIntersection, Error, FilterRejected, GraphError, LinkEnd, LinkError, Narrowing,
    NoCommonFormat, OfferFault, PropError, SolveError, TopologyError,
};
use crate::format::{
    ConstraintSpec, FieldSpec, FixedFormat, Interned, MAX_OFFER_FIELDS, MAX_SET_VALUES, OfferDesc,
    OfferSpec, PROBED, Value, ValueDesc, Vocabulary, WILDCARD, constraint_line,
};
use crate::id::{ElementId, FieldId, FormatId, Interner, LinkId, SinkPad, SrcPad, ValueId};
use crate::io::{NoBackend, ReactorFactory};
use crate::level::{Gauge, Limit};
use crate::log::{
    DebugSpec, Level, LevelFilter, Log, LogDrain, LogRecord, LogWriter, Logs, log_channel,
};
use crate::memory::{Allocator, Pool, PoolConfig, PoolStats};
use crate::props::{PropHandle, PropTable, validate_and_set};
use crate::publish::{Publisher, published};
use crate::ring::{Consumer, Producer, spsc};
use crate::sched::{
    DownLink, GROUP_PARK_BACKSTOP, GroupSetup, GroupShared, Stepped, Tag, UpLink, judge,
    retire_group,
    run_group,
};
use crate::slots::Slots;
use crate::solve::{Fail, FailKind, List, NONE, Reason, Scope, Solver};
use crate::station::{Room, Station};
use crate::time::Timestamp;
use crate::topology::{Flight, Join, Joining, Leave, PadEdit, Part};

pub(crate) const BYTES: FormatId = FormatId(0);

/// What a change does when what joins raises the latency of an element that stays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LatencyPolicy {
    /// The element's deadlines move later by the difference, once.
    Grow,
    /// The element keeps its latency and the rings into it leak what is late.
    Hold,
}

#[derive(Clone)]
struct Edge {
    src: ElementId,
    src_pad: SrcPad,
    sink: ElementId,
    sink_pad: SinkPad,
    format: FixedFormat,
    live: bool,
    filter: Option<Arc<Filter>>,
    solved: bool,
    why: [Reason; MAX_OFFER_FIELDS],
    /// Bit per field of `format` no upstream element states and the sink pad does not take at
    /// every value.
    open: u16,
    risky: u16,
    /// Per open field the values downstream takes, absent where it takes any.
    takes: Vec<(FieldId, Vec<Value>)>,
    menus: [u32; 2],
    slots: [u32; 2],
}

struct Filter {
    offers: Interned,
    line: Box<str>,
}

#[derive(Clone, Copy)]
struct RuleIds {
    to: Option<FieldId>,
    from: Option<FieldId>,
    value: Option<Value>,
}

#[derive(Default)]
struct Resolved {
    checked: bool,
    names: usize,
    ids: Vec<RuleIds>,
    probed: bool,
}

// An empty Vec allocates nothing.
#[allow(clippy::disallowed_methods)]
static NO_OFFERS: Interned = Interned { offers: Vec::new(), fields: Vec::new(), sets: Vec::new() };

type LiveGroup = (Vec<ElementId>, Arc<Station>);

static VACANT: ElementDesc = ElementDesc {
    name: "?",
    sinks: &[],
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: crate::element::LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

type Published =
    (Chunks<Row>, Chunks<Vec<(u32, SrcPad)>>, Arc<Vec<(u32, u32)>>, Arc<Vec<(u32, Pool)>>);

#[derive(Clone, Copy, Default)]
struct Tune {
    pool: Option<PoolSize>,
    credits: u32,
    alloc: Option<(usize, Allocator)>,
    reserve: Option<(usize, u32)>,
    queue: Option<usize>,
    leaky: Option<crate::ring::Leaky>,
    limit: Option<Limit>,
    marks: Option<(Limit, Limit)>,
    auto: bool,
}

// A few per change, boxing the edge would allocate per edit.
#[allow(clippy::large_enum_variant)]
enum Undo {
    Unlink(LinkId),
    Restore(LinkId, Edge),
    Forget(ElementId),
    Auto(ElementId, LinkId),
}

struct Pending {
    change: u32,
    journal: Vec<Undo>,
    groups: Vec<LiveGroup>,
    leaving: Vec<(ElementId, Leave)>,
    latency: Vec<(u32, Timestamp)>,
    pads: Vec<(u32, (usize, usize))>,
    leaky: Vec<(u32, Option<crate::ring::Leaky>)>,
}

struct LiveRun {
    clock: Arc<dyn Clock>,
    wall: Option<Instant>,
    groups: Vec<LiveGroup>,
    factory: ReactorFactory,
    own: Vec<(ElementId, Pool)>,
    parting: Vec<(ElementId, ElementId, Pool)>,
    latency: Vec<(u32, Timestamp)>,
    pads: Vec<(u32, (usize, usize))>,
    journal: Vec<Undo>,
    leaving: Vec<(ElementId, Leave)>,
    flight: Option<Pending>,
    spawned: usize,
    gate: Option<LogGate>,
    drains: Producer<LogDrain>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Run(usize),
    New(usize),
}

fn part(step: &mut Vec<Part>, head: ElementId) -> &mut Part {
    let at = step.iter().position(|p| p.head == head).unwrap_or_else(|| {
        step.push(Part::new(head, 0));
        step.len() - 1
    });
    &mut step[at]
}

#[derive(Default)]
struct DynPads {
    srcs: Vec<DynPad>,
    sinks: Vec<DynPad>,
}

pub(crate) fn pad_at<'a>(
    statics: &'static [PadDesc],
    added: &'a [DynPad],
    index: u32,
) -> Option<(&'a str, &'static [OfferDesc])> {
    let i = index as usize;
    match statics.get(i) {
        Some(p) => Some((p.name, p.offers)),
        None => added.get(i - statics.len()).map(|d| (d.name.as_str(), d.offers)),
    }
}

fn pad_named(statics: &'static [PadDesc], added: &[DynPad], name: &str) -> Option<u32> {
    let names = statics.iter().map(|p| p.name).chain(added.iter().map(|d| d.name.as_str()));
    names.into_iter().position(|n| n == name).map(|i| i as u32)
}

/// A pad that appeared during [`Pipeline::preroll`], ready for [`Pipeline::link`].
#[derive(Clone, Debug)]
pub struct AddedPad<P> {
    pub element: ElementId,
    pub pad: P,
    pub name: String,
}

/// The pads [`Pipeline::preroll`] found, per side.
#[derive(Clone, Debug, Default)]
pub struct AddedPads {
    pub srcs: Vec<AddedPad<SrcPad>>,
    pub sinks: Vec<AddedPad<SinkPad>>,
}

/// A summary of a finished [`Pipeline::run`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Payload buffers ever heap-allocated, flat when the run allocated nothing.
    pub pool_slot_allocations: u64,
    pub pool_high_water: u64,
    pub buffers: u64,
}

#[derive(Clone, Copy)]
struct PoolSize {
    slot: usize,
    slots: u32,
    bytes: u64,
}

/// Park statistics summed over the groups' wake stations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    pub parks: u64,
    /// Parks that ended on the backstop instead of on a signal.
    pub tick_expiries: u64,
    /// Backstop expiries after which the group found work nobody had signaled.
    pub late_wakes: u64,
    /// Wakes with no signal after which the pass found nothing to do, a group that spins.
    pub empty_wakes: u64,
    pub parked_ns: u64,
}

impl SchedulerStats {
    pub(crate) fn add(&mut self, other: SchedulerStats) {
        self.parks += other.parks;
        self.tick_expiries += other.tick_expiries;
        self.late_wakes += other.late_wakes;
        self.empty_wakes += other.empty_wakes;
        self.parked_ns += other.parked_ns;
    }
}

/// A keyframe of an [`Index`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub time_ns: u64,
    /// The byte the container restarts at for this keyframe.
    pub byte: u64,
    /// The earliest byte a selected track needs at `time_ns`, and the stamp there.
    pub restart: u64,
    pub restart_ns: u64,
}

impl IndexEntry {
    /// A keyframe no track restarts ahead of.
    pub const fn key(time_ns: u64, byte: u64) -> Self {
        Self { time_ns, byte, restart: byte, restart_ns: time_ns }
    }
}

/// The time to byte map of the element that parses the format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    pub entries: Vec<IndexEntry>,
    /// Total source length in bytes, for the proportional fallback.
    pub file_len: Option<u64>,
    pub duration: Timestamp,
    /// Every keyframe is in `entries`. False while reading, a target past the last is then
    /// estimated.
    pub complete: bool,
    /// The stream the entries are keyframes of, `None` for an element with one src pad.
    pub stream: Option<u32>,
    /// Keyframes of GOPs, so a skip reaches the GOP being presented and no further.
    pub grouped: bool,
}

impl Default for Index {
    // An empty Vec allocates nothing.
    #[allow(clippy::disallowed_methods)]
    fn default() -> Self {
        let (entries, duration) = (Vec::new(), Timestamp::NONE);
        Index { entries, file_len: None, duration, complete: false, stream: None, grouped: false }
    }
}

impl Index {
    fn floor(&self, ns: u64) -> Option<&IndexEntry> {
        let n = self.entries.partition_point(|e| e.time_ns <= ns);
        n.checked_sub(1).map(|n| &self.entries[n])
    }

    fn estimate(&self, ns: u64) -> Option<u64> {
        match (self.file_len, self.duration.nanos()) {
            (Some(len), Some(dur)) if dur > 0 => {
                // A hostile short duration aims past the file, the end is the farthest byte.
                let at = u128::from(ns) * u128::from(len) / u128::from(dur);
                Some(at.min(u128::from(len)) as u64)
            }
            _ => None,
        }
    }

    fn known(&self, ns: u64) -> Option<&IndexEntry> {
        let (floor, last) = (self.floor(ns)?, self.entries.last()?);
        let past =
            !self.complete && ns > last.time_ns && self.estimate(ns).is_some_and(|b| b > last.byte);
        (!past).then_some(floor)
    }

    /// The keyframe a seek to `target` restarts at, `None` where the byte is estimated.
    pub fn keyframe(&self, target: Timestamp) -> Option<&IndexEntry> {
        self.known(target.nanos()?)
    }

    /// The byte to resume reading from and the stream time it lands on, `None` if unseekable.
    pub fn resolve(&self, target: Timestamp) -> Option<(u64, Timestamp)> {
        // Before the first keyframe an estimate lands past it, so the first is where to start.
        if let Some(first) =
            self.entries.first().filter(|e| target.nanos().is_some_and(|t| t < e.time_ns))
        {
            return Some((first.byte, Timestamp(first.time_ns)));
        }
        match self.keyframe(target) {
            Some(e) => Some((e.byte, Timestamp(e.time_ns))),
            None => self.estimate(target.nanos()?).map(|byte| (byte, target)),
        }
    }

    /// Where the source restarts for `plan`, `None` for a plan that restarts nothing.
    pub fn restart(&self, plan: SeekPlan) -> Option<(u64, Timestamp)> {
        let SeekPlan::Seek { to_byte, key, .. } = plan else {
            return None;
        };
        let entry = key
            .nanos()
            .and_then(|ns| self.floor(ns))
            .filter(|e| (e.time_ns, e.byte) == (key.0, to_byte));
        Some(entry.map_or((to_byte, key), |e| (e.restart, Timestamp(e.restart_ns))))
    }

    /// The command for a seek to `target`, a skip when the keyframe is demuxed and `reach` allows.
    pub fn plan(
        &self,
        target: Timestamp,
        mode: SeekMode,
        position: Timestamp,
        demuxed: Option<Timestamp>,
        reach: Reach,
    ) -> SeekPlan {
        let Some((to_byte, key)) = self.resolve(target) else {
            return SeekPlan::Unseekable;
        };
        let read = demuxed.is_some_and(|out| out.is_some() && key <= out);
        // An estimated byte names no keyframe, and only what was read is skipped to.
        let known = self.keyframe(target).is_some();
        let held = match reach {
            Reach::Read => true,
            Reach::Group => !known || key <= position,
        };
        match mode {
            SeekMode::Keyframe => SeekPlan::Seek { to_byte, key, to_time: key },
            SeekMode::Accurate | SeekMode::Preview if read && held && position < target => {
                SeekPlan::Skip { to_time: target }
            }
            SeekMode::Accurate | SeekMode::Preview => {
                SeekPlan::Seek { to_byte, key, to_time: target }
            }
        }
    }
}

/// How far ahead a skip may reach, by what the decoder costs on the way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reach {
    /// The group being presented. A target in a later group is a seek. Video.
    Group,
    /// All the demuxer has put out, every packet decodes alone. Audio.
    Read,
}

/// Where presentation starts after a seek, at its target or at the keyframe before it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeekMode {
    Accurate,
    Keyframe,
    /// Accurate, the first picture decoded on the way goes out flagged `BufferFlags::PREVIEW`.
    Preview,
}

/// What [`Index::plan`] or [`Control::seek`] chose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeekPlan {
    Skip {
        to_time: Timestamp,
    },
    Seek {
        to_byte: u64,
        key: Timestamp,
        to_time: Timestamp,
    },
    /// Held until an index that is being read lands, then sent. A newer seek replaces it.
    Deferred,
    Unseekable,
}

/// What a seek can do now, from what the sources and the indexes say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seekable {
    /// A source has not said how it is addressed yet.
    Unknown,
    /// No index and no source addressed by time.
    No,
    /// Skips only, a source over an origin that serves no ranges.
    Forward,
    /// Sources addressed by byte, with an index.
    Byte,
    /// Sources addressed by time, a stream of segments.
    Time,
}

/// How a source is addressed, published with
/// [`Ctx::publish_serves`](crate::ctx::Ctx::publish_serves).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Serves {
    pub by: Addressed,
    /// Fetched over a network, so a skip reaches all that was read.
    pub remote: bool,
}

/// Where a source restarts behind a seek.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Addressed {
    Byte,
    /// At the time of the presentation point, a stream of segments.
    Time,
    /// Nowhere. Only forward skips reach into what it has read.
    No,
}

impl Serves {
    pub const FILE: Serves = Serves { by: Addressed::Byte, remote: false };
}

/// Owns the topology, the clock and the lifecycle of a run.
pub struct Pipeline {
    slots: Slots,
    elements: Vec<Option<Box<dyn Element>>>,
    edges: Vec<Edge>,
    edge_ids: Vec<u32>,
    next_link: u32,
    formats: Interner,
    fields: Interner,
    values: Interner,
    bus_sender: BusSender,
    bus: Bus,
    slot_size: usize,
    pool_slots: usize,
    queue_cap: usize,
    reactor_factory: Option<ReactorFactory>,
    last_report: Option<RunReport>,
    failed_change: Option<u32>,
    log_level: Option<Level>,
    log_queue_cap: usize,
    clock: Arc<dyn Clock>,
    clock_explicit: bool,
    base_shared: Arc<AtomicI64>,
    pools: Vec<(u32, Pool)>,
    retired: PoolStats,
    fading: Vec<u32>,
    build_log_rank: u8,
    debug: DebugSpec,
    debug_pinned: bool,
    log_writer: Option<Arc<dyn LogWriter>>,
    log_session: u64,
    control: Arc<Control>,
    initial_paused: bool,
    tracing: Arc<AtomicBool>,
    tune: Vec<Tune>,
    latency_policy: LatencyPolicy,
    pool_cap: u64,
    pub(crate) backstop: Duration,
    dyn_pads: Vec<DynPads>,
    offers: Vec<Vec<PadOffers>>,
    menus: HashMap<(usize, usize), u32>,
    menu_ids: u32,
    prerolled: usize,
    streams: Vec<Vec<Stream>>,
    stream_pads: Vec<Vec<(u32, SrcPad)>>,
    published: Published,
    dirty: (Vec<usize>, Vec<usize>, bool, bool),
    select_all: bool,
    rows: Vec<Row>,
    vacant: (Arc<PropTable>, Arc<ElementCounters>),
    view: Publisher<View>,
    live_run: Option<LiveRun>,
    solver: Option<Box<Solver>>,
    resolved: Vec<Resolved>,
}

impl Pipeline {
    /// A pipeline configured from `PROFLUENS_DEBUG` and the wall clock. A `trace` level on the
    /// pipeline (`trace` or `pipeline:trace`) also records the per-element latency histograms,
    /// as [`set_tracing`](Self::set_tracing) does.
    pub fn new() -> Self {
        let env = std::env::var("PROFLUENS_DEBUG").ok().map(|s| DebugSpec::parse(&s));
        let Logs { writer, debug } = crate::log::current_logs();
        let debug_pinned = env.is_some();
        let spec = env.or(debug).unwrap_or_default();
        let tracing = build_rank(&spec) >= Level::Trace.rank();
        let mut p = Self::with(spec, tracing, Arc::new(InstantClock::new()));
        (p.log_writer, p.debug_pinned) = (writer, debug_pinned);
        p
    }

    /// A pipeline that reads nothing from the process environment.
    // Once at build, empty tables allocate nothing until the graph is assembled.
    #[allow(clippy::disallowed_methods)]
    pub fn with(debug: DebugSpec, tracing: bool, default_clock: Arc<dyn Clock>) -> Self {
        let (tx, rx) = Bus::channel();
        let base_shared = Arc::new(AtomicI64::new(crate::time::BASE_UNSET));
        let build_log_rank = build_rank(&debug);
        let p = Self {
            slots: Slots::default(),
            elements: Vec::new(),
            edges: Vec::new(),
            edge_ids: Vec::new(),
            next_link: 0,
            formats: Interner::new(),
            fields: Interner::new(),
            values: Interner::new(),
            bus_sender: tx,
            bus: rx,
            slot_size: 128 * 1024,
            pool_slots: 64,
            queue_cap: 4,
            reactor_factory: None,
            last_report: None,
            failed_change: None,
            log_level: None,
            log_queue_cap: 256,
            view: published(View::empty(Arc::clone(&default_clock))).0,
            clock: default_clock,
            pools: Vec::new(),
            retired: PoolStats::default(),
            fading: Vec::new(),
            build_log_rank,
            debug,
            debug_pinned: false,
            log_writer: None,
            log_session: 0,
            clock_explicit: false,
            base_shared: Arc::clone(&base_shared),
            control: Control::new(base_shared),
            initial_paused: false,
            tracing: Arc::new(AtomicBool::new(tracing)),
            dyn_pads: Vec::new(),
            offers: Vec::new(),
            menus: HashMap::new(),
            menu_ids: 0,
            prerolled: 0,
            streams: Vec::new(),
            stream_pads: Vec::new(),
            published: (
                Vec::new().into(),
                Vec::new().into(),
                Arc::new(Vec::new()),
                Arc::new(Vec::new()),
            ),
            dirty: (Vec::new(), Vec::new(), false, false),
            select_all: true,
            rows: Vec::new(),
            vacant: (Arc::new(PropTable::new(0)), Arc::default()),
            live_run: None,
            solver: Some(Box::default()),
            resolved: Vec::new(),
            tune: Vec::new(),
            latency_policy: LatencyPolicy::Grow,
            pool_cap: u64::MAX,
            backstop: GROUP_PARK_BACKSTOP,
        };
        p.control.set_view(p.view.reader());
        p
    }

    /// Enable or disable per-element latency histograms, live.
    /// Bounds how long an idle group parks without a signal, one second by default. Lower it
    /// when an app wants a group to re-check its state on its own.
    pub fn set_park_backstop(&mut self, backstop: Duration) {
        self.backstop = backstop.max(Duration::from_millis(1));
    }

    pub fn set_tracing(&mut self, on: bool) {
        self.tracing.store(on, Ordering::Release);
    }

    pub fn set_reactor_factory(&mut self, factory: ReactorFactory) {
        self.reactor_factory = Some(factory);
    }

    fn need_reactor(&self, ids: &[ElementId]) -> Result<(), Error> {
        if self.reactor_factory.is_some() {
            return Ok(());
        }
        match ids.iter().find(|id| self.rows[self.ix(**id)].desc.io) {
            Some(&element) => {
                Err(Error::NoReactor { element, element_name: self.name_of(element) })
            }
            None => Ok(()),
        }
    }

    /// Force the pipeline clock for the next run, over any element-provided one.
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
        self.clock_explicit = true;
        self.publish();
    }

    // Once per graph change, off the media path.
    #[allow(clippy::disallowed_methods)]
    fn publish(&mut self) {
        let table = &self.rows;
        let keep = |c: usize| table.iter().skip(c * CHUNK).take(CHUNK).any(|r| !r.gone);
        self.published.0.update(&self.rows, &mut self.dirty.0, keep);
        self.published.1.update(&self.stream_pads, &mut self.dirty.1, keep);
        if std::mem::take(&mut self.dirty.2) {
            self.published.2 = Arc::new(self.slots.map().to_vec());
        }
        if std::mem::take(&mut self.dirty.3) {
            self.published.3 = Arc::new(self.pools.clone());
        }
        let (rows, stream_pads) = (self.published.0.clone(), self.published.1.clone());
        let (slots, pools) = (Arc::clone(&self.published.2), Arc::clone(&self.published.3));
        let edges = self.live().iter().map(|e| (e.src, e.src_pad, e.sink, e.sink_pad)).collect();
        let (clock, retired, count) =
            (Arc::clone(&self.clock), self.retired, self.slots.next() as usize);
        self.view.publish(View { rows, slots, count, pools, retired, clock, edges, stream_pads });
    }

    #[cfg(test)]
    pub(crate) fn view_chunks(&self) -> usize {
        self.published.0.kept()
    }

    fn fold_pools(&mut self) -> bool {
        let (pools, retired) = (&mut self.pools, &mut self.retired);
        let mut folded = false;
        self.fading.sort_unstable();
        self.fading.dedup();
        self.fading.retain(|el| {
            let Ok(at) = pools.binary_search_by_key(el, |p| p.0) else {
                return false;
            };
            let done = pools[at].1.in_use() == (0, 0);
            if done {
                *retired = crate::counters::add_stats(*retired, pools.remove(at).1.stats());
                folded = true;
            }
            !done
        });
        self.dirty.3 |= folded;
        folded
    }

    fn touched(&mut self, s: usize) {
        self.dirty.0.push(s / CHUNK);
        self.dirty.1.push(s / CHUNK);
    }

    fn slot(&self, el: ElementId) -> Option<usize> {
        self.slots.get(el.0)
    }

    fn ix(&self, el: ElementId) -> usize {
        self.slots.get(el.0).unwrap_or(usize::MAX)
    }

    fn row_of(&self, el: ElementId) -> Option<&Row> {
        self.slot(el).map(|s| &self.rows[s])
    }

    fn id_of(&self, s: u32) -> ElementId {
        ElementId(self.slots.id(s as usize))
    }

    /// Start the next run paused, prerolled and holding until [`Control::resume`].
    pub fn start_paused(&mut self, on: bool) {
        self.initial_paused = on;
    }

    /// The control block that stops, pauses, seeks and changes the pipeline from any thread.
    pub fn control(&self) -> Arc<Control> {
        Arc::clone(&self.control)
    }

    pub fn set_pool(&mut self, slot_size: usize, slots: u32) {
        self.slot_size = slot_size.max(1);
        self.pool_slots = slots.max(1) as usize;
    }

    /// Log up to `level` to stderr from a drain thread during a run.
    pub fn log_to_stderr(&mut self, level: Level) {
        self.log_level = Some(level);
    }

    /// Log at the levels of `spec` from here on, unless `PROFLUENS_DEBUG` set them in
    /// [`new`](Self::new).
    pub fn set_debug(&mut self, spec: DebugSpec) {
        if !self.debug_pinned {
            (self.build_log_rank, self.debug) = (build_rank(&spec), spec);
        }
    }

    /// Hand records and build lines to `writer` in place of stderr, from the next run and line on.
    pub fn set_log_writer(&mut self, writer: Arc<dyn LogWriter>) {
        self.log_writer = Some(writer);
    }

    /// Stamps every record of the next run and change with `session`, the application's id for
    /// this pipeline. Zero is none.
    pub fn set_log_session(&mut self, session: u64) {
        self.log_session = session;
    }

    fn plog(&self, level: Level, msg: std::fmt::Arguments<'_>) {
        plog(self.log_writer.as_deref(), self.plog_rank(), level, msg);
    }

    fn plog_rank(&self) -> u8 {
        self.build_log_rank.max(self.log_level.map_or(0, Level::rank))
    }

    fn display_of(&self, el: ElementId) -> String {
        format!("{}#{}", self.name_of(el), el.0)
    }

    fn format_display(&self, f: &FixedFormat) -> String {
        f.render([&self.formats, &self.fields, &self.values])
    }

    // Build-time registration, the box is the dyn-dispatch handle.
    #[allow(clippy::disallowed_methods)]
    pub fn add(&mut self, element: impl Element + 'static) -> ElementId {
        self.add_boxed(Box::new(element))
    }

    // Build-time registration.
    #[allow(clippy::disallowed_methods)]
    pub fn add_boxed(&mut self, element: Box<dyn Element>) -> ElementId {
        let desc = element.desc();
        let (props, counters) = (Arc::new(PropTable::new(desc.props.len())), Arc::default());
        for &(name, v) in element.presets() {
            let v = match v {
                ValueDesc::Int(n) => Value::Int(n),
                ValueDesc::Rat(n, d) => Value::Rat(n, d),
                ValueDesc::Id(s) => Value::Id(ValueId(self.values.intern(s))),
            };
            match desc.props.iter().position(|p| p.name == name) {
                Some(i) if desc.props[i].allowed.accepts(v) => props.preset(i, v),
                _ => self.plog(Level::Warn, format_args!("{}: preset {name} refused", desc.name)),
            }
        }
        let probed = desc.srcs.iter().chain(desc.sinks).any(|p| OfferDesc::is_probed(p.offers));
        let row = Row { desc, props, counters, gone: false, levels: Vec::new() };
        let (id, s, new) = self.slots.add();
        let id = ElementId(id);
        let resolved = Resolved { probed, ..Resolved::default() };
        if new {
            self.rows.push(row);
            self.elements.push(Some(element));
            self.resolved.push(resolved);
            self.stream_pads.push(Vec::new());
            self.dyn_pads.push(DynPads::default());
            self.offers.push(Vec::new());
            self.streams.push(Vec::new());
            self.tune.push(Tune::default());
        } else {
            (self.rows[s], self.elements[s], self.resolved[s]) = (row, Some(element), resolved);
        }
        self.dirty.2 = true;
        self.touched(s);
        self.publish();
        if let Some(run) = &mut self.live_run {
            run.journal.push(Undo::Forget(id));
        }
        self.plog(
            Level::Info,
            format_args!(
                "add {}#{} ({} static pad{})",
                desc.name,
                id.0,
                desc.sinks.len() + desc.srcs.len(),
                if desc.sinks.len() + desc.srcs.len() == 1 { "" } else { "s" },
            ),
        );
        id
    }

    /// Cap the bytes the pools of a run hold together, cutting each pool proportionally.
    pub fn set_pool_cap(&mut self, bytes: u64) {
        self.pool_cap = bytes;
    }

    pub fn set_element_pool(&mut self, el: ElementId, slot_size: usize, slots: u32) {
        let bytes = slot_size.max(1) as u64 * slots.max(1) as u64;
        self.set_element_pool_bytes(el, slot_size, slots, bytes);
    }

    /// How many reactor ops `el` may keep in flight, in place of its pool's `io_depth`.
    pub fn set_element_credits(&mut self, el: ElementId, credits: u32) {
        if let Some(t) = self.tune_of(el) {
            t.credits = credits.max(1);
        }
    }

    /// Size one element's pool with an explicit byte budget for its sized buffers.
    pub fn set_element_pool_bytes(
        &mut self,
        el: ElementId,
        slot_size: usize,
        slots: u32,
        bytes: u64,
    ) {
        if let Some(t) = self.tune_of(el) {
            t.pool = Some(PoolSize { slot: slot_size.max(1), slots: slots.max(1), bytes });
        }
    }

    /// Keep `count` sized buffers of the class serving `size` mapped in `el`'s pool for the run.
    pub fn set_element_reserve(&mut self, el: ElementId, size: usize, count: u32) {
        if let Some(t) = self.tune_of(el) {
            t.reserve = (count > 0).then_some((size, count));
        }
    }

    /// Map one element's pool from `allocator` with regions aligned to `align`.
    pub fn set_element_allocator(&mut self, el: ElementId, align: usize, allocator: Allocator) {
        if let Some(t) = self.tune_of(el) {
            t.alloc = Some((align, allocator));
        }
    }

    pub fn set_queue_capacity(&mut self, el: ElementId, batches: usize) {
        if let Some(t) = self.tune_of(el) {
            t.queue = Some(batches.max(1));
        }
    }

    pub fn set_queue_leaky(&mut self, el: ElementId, leaky: crate::ring::Leaky) {
        if let Some(t) = self.tune_of(el) {
            t.leaky = Some(leaky);
        }
    }

    pub fn set_latency_policy(&mut self, policy: LatencyPolicy) {
        self.latency_policy = policy;
    }

    /// Limit what the links into `el` hold, in bytes and in stream time.
    pub fn set_link_limit(&mut self, el: ElementId, limit: Limit) {
        if let Some(t) = self.tune_of(el) {
            t.limit = Some(limit);
        }
    }

    /// Set low and high marks on the links into `el`, crossing one says `BusMessage::Buffering`.
    pub fn set_link_marks(&mut self, el: ElementId, low: Limit, high: Limit) {
        if let Some(t) = self.tune_of(el) {
            t.marks = Some((low, high));
        }
    }

    /// Set the rings into `to` as those into `from` are set.
    pub fn copy_link_settings(&mut self, from: ElementId, to: ElementId) {
        if let (Some(f), Some(t)) = (self.slot(from), self.slot(to)) {
            let Tune { queue, leaky, limit, marks, .. } = self.tune[f];
            self.tune[t] = Tune { queue, leaky, limit, marks, ..self.tune[t] };
        }
    }

    fn tune_of(&mut self, el: ElementId) -> Option<&mut Tune> {
        let s = self.slot(el)?;
        self.tune.get_mut(s)
    }

    /// Link a src pad to a sink pad, negotiating their formats.
    ///
    /// ```
    /// # use profluens_core::id::{ElementId, SinkPad, SrcPad};
    /// # use profluens_core::pipeline::Pipeline;
    /// fn join(p: &mut Pipeline, a: ElementId, b: ElementId) {
    ///     let _ = p.link(a.src(SrcPad(0)), b.sink(SinkPad(0)));
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// # use profluens_core::id::{ElementId, SinkPad, SrcPad};
    /// # use profluens_core::pipeline::Pipeline;
    /// fn join(p: &mut Pipeline, a: ElementId, b: ElementId) {
    ///     let _ = p.link(b.sink(SinkPad(0)), a.src(SrcPad(0)));
    /// }
    /// ```
    pub fn link(
        &mut self,
        src: (ElementId, SrcPad),
        sink: (ElementId, SinkPad),
    ) -> Result<LinkId, Error> {
        self.link_journaled(src, sink, None)
    }

    /// [`link`](Self::link) with per-field constraints that narrow this edge alone.
    pub fn link_filtered(
        &mut self,
        src: (ElementId, SrcPad),
        sink: (ElementId, SinkPad),
        filter: &[FieldSpec<'_>],
    ) -> Result<LinkId, Error> {
        let over = |what, found: usize, limit: usize| {
            let fault = OfferFault::OverLimit { what, found: found as u32, limit: limit as u32 };
            Err(Error::Solve(SolveError::BadFilter(fault)))
        };
        if filter.len() > MAX_OFFER_FIELDS {
            return over("fields in a filter", filter.len(), MAX_OFFER_FIELDS);
        }
        for f in filter {
            if let ConstraintSpec::Set(vs) = f.allowed
                && vs.len() > MAX_SET_VALUES
            {
                return over("values in a set", vs.len(), MAX_SET_VALUES);
            }
        }
        let names = [&mut self.formats, &mut self.fields, &mut self.values];
        let offers = Interned::intern(&[OfferSpec { family: WILDCARD, fields: filter }], names);
        let line = filter.iter().map(|f| format!("{}={}", f.field, constraint_line(&f.allowed)));
        let line = line.collect::<Vec<_>>().join(", ").into();
        self.link_journaled(src, sink, Some(Arc::new(Filter { offers, line })))
    }

    fn link_journaled(
        &mut self,
        src: (ElementId, SrcPad),
        sink: (ElementId, SinkPad),
        filter: Option<Arc<Filter>>,
    ) -> Result<LinkId, Error> {
        let id = self.link_at(None, src, sink, filter)?;
        if let Some(run) = &mut self.live_run {
            run.journal.push(Undo::Unlink(id));
        }
        self.publish();
        Ok(id)
    }

    // Once per link, the edge's empty taken values filled by the solve.
    #[allow(clippy::disallowed_methods)]
    fn link_at(
        &mut self,
        at: Option<LinkId>,
        src: (ElementId, SrcPad),
        sink: (ElementId, SinkPad),
        filter: Option<Arc<Filter>>,
    ) -> Result<LinkId, Error> {
        let (_, src_menu) = self.src_at(src.0, src.1)?;
        let (_, sink_menu) = self.sink_at(sink.0, sink.1)?;
        let s = self.offers_of(src.0, (true, src.1.0), src_menu)?;
        let k = self.offers_of(sink.0, (false, sink.1.0), sink_menu)?;
        let slots = [self.ix(src.0), self.ix(sink.0)];
        self.vet(src.0, slots[0])?;
        self.vet(sink.0, slots[1])?;
        // The names are built only for a line that is written.
        let (debug, info) =
            (Level::Debug.rank() <= self.plog_rank(), Level::Info.rank() <= self.plog_rank());
        if debug {
            self.plog(
                Level::Debug,
                format_args!(
                    "negotiate {}.{} (offers {:?}) x {}.{} (offers {:?})",
                    self.display_of(src.0),
                    self.src_name(src),
                    self.offers[slots[0]][s].lines,
                    self.display_of(sink.0),
                    self.sink_name(sink),
                    self.offers[slots[1]][k].lines,
                ),
            );
        }
        let id = at.unwrap_or(LinkId(self.next_link));
        self.next_link += at.is_none() as u32;
        let ((src, src_pad), (sink, sink_pad)) = (src, sink);
        let (format, why) = (FixedFormat::new(BYTES), [Reason::default(); MAX_OFFER_FIELDS]);
        let menus = [self.offers[slots[0]][s].menu, self.offers[slots[1]][k].menu];
        let slots = slots.map(|x| x as u32);
        let takes = Vec::new();
        let edge = Some(Edge {
            src,
            src_pad,
            sink,
            sink_pad,
            format,
            live: false,
            filter,
            solved: false,
            why,
            open: 0,
            risky: 0,
            takes,
            menus,
            slots,
        });
        self.put_edge(id, edge);
        let mut solver = self.solver.take().unwrap_or_default();
        self.closure(&mut solver, Scope::Link(id));
        self.gather(&mut solver, None, None);
        let mut solved = solver.solve();
        if solved.is_err_and(|f| f.kind == FailKind::Cycle) {
            solver.scope.clear();
            solver.scope.push(id);
            self.gather(&mut solver, None, None);
            solved = solver.solve();
        }
        let made = match solved {
            Ok(()) => {
                self.write_solved(&solver, false);
                Ok(())
            }
            Err(fail) => Err(self.refused(&mut solver, id, fail)),
        };
        self.solver = Some(solver);
        #[allow(clippy::disallowed_methods)]
        let names = |p: &Self| {
            let (sp, dp) =
                (p.src_name((src, src_pad)).to_string(), p.sink_name((sink, sink_pad)).to_string());
            (p.display_of(src), sp, p.display_of(sink), dp)
        };
        if let Err(e) = made {
            if debug {
                let (sn, sp, dn, dp) = names(self);
                self.plog(Level::Debug, format_args!("link FAILED {sn}.{sp} -> {dn}.{dp}: {e}"));
            }
            self.put_edge(id, None);
            self.next_link -= at.is_none() as u32;
            return Err(e);
        }
        if info {
            let (sn, sp, dn, dp) = names(self);
            let format = self.edge(id).map(|e| self.format_display(&e.format));
            self.plog(
                Level::Info,
                format_args!("link {sn}.{sp} -> {dn}.{dp} : {}", format.unwrap_or_default()),
            );
        }
        Ok(id)
    }

    // Once per element and per vocabulary growth, the solve reads the ids after.
    #[allow(clippy::disallowed_methods)]
    fn vet(&mut self, el: ElementId, i: usize) -> Result<(), Error> {
        let desc = self.rows[i].desc;
        if !self.resolved[i].checked {
            crate::couple::check(desc).map_err(|(index, why)| {
                let element_name = desc.name;
                Error::Solve(SolveError::BadCoupling {
                    element: el,
                    element_name,
                    index: index as u32,
                    why,
                })
            })?;
            for c in desc.couplings {
                if let Coupling::Field { rule: Rule::Fixed(ValueDesc::Id(name)), .. } = c {
                    self.values.intern(name);
                }
            }
            self.resolved[i].checked = true;
        }
        if self.resolved[i].names != self.fields.len() {
            let ids = desc.couplings.iter().map(|c| self.rule_ids(c)).collect();
            (self.resolved[i].names, self.resolved[i].ids) = (self.fields.len(), ids);
        }
        Ok(())
    }

    fn rule_ids(&self, c: &Coupling) -> RuleIds {
        let field = |name: &str| self.fields.get(name).map(FieldId);
        let Coupling::Field { field: name, rule, .. } = *c else {
            return RuleIds { to: None, from: None, value: None };
        };
        let from = match rule {
            Rule::Copy { from } => field(from),
            _ => field(name),
        };
        let value = match rule {
            Rule::Fixed(ValueDesc::Int(n)) => Some(Value::Int(n)),
            Rule::Fixed(ValueDesc::Rat(n, d)) => Some(Value::Rat(n, d)),
            Rule::Fixed(ValueDesc::Id(name)) => {
                self.values.get(name).map(|id| Value::Id(ValueId(id)))
            }
            _ => None,
        };
        RuleIds { to: field(name), from, value }
    }

    fn pad_is_wild(&self, el: u32, src: bool, pad: u32) -> bool {
        let (desc, added) = (self.rows[el as usize].desc, &self.dyn_pads[el as usize]);
        let (statics, added) =
            if src { (desc.srcs, &added.srcs) } else { (desc.sinks, &added.sinks) };
        pad_at(statics, added, pad).is_some_and(|(_, menu)| menu.iter().any(OfferSpec::is_wildcard))
    }

    /// The edges coupled to the scope's seeds. A running edge is a boundary, its format pinned.
    fn closure(&self, s: &mut Solver, scope: Scope) {
        let pinning = self.live_run.is_some();
        let key =
            |el: u32, src: bool, pad: u32| (el as u64) << 33 | (src as u64) << 32 | pad as u64;
        s.scope.clear();
        s.work.clear();
        s.seen.clear();
        let visit = |s: &mut Solver, p: u64| {
            if !s.seen.contains(&p) {
                s.seen.push(p);
                s.work.push(p);
            }
        };
        s.ends.clear();
        s.pads.clear();
        for (e, &i) in self.edges.iter().zip(&self.edge_ids) {
            let ends = [key(e.slots[0], true, e.src_pad.0), key(e.slots[1], false, e.sink_pad.0)];
            s.pads.extend([(ends[0], s.ends.len() as u32), (ends[1], s.ends.len() as u32)]);
            s.ends.push((ends, i, e.live, false));
        }
        s.pads.sort_unstable();
        for k in 0..s.ends.len() {
            let (ends, i, live, _) = s.ends[k];
            let seed = match scope {
                Scope::Link(link) => i == link.0,
                Scope::Fresh => !live,
            };
            if seed {
                visit(s, ends[0]);
                visit(s, ends[1]);
            }
        }
        while let Some(p) = s.work.pop() {
            let (el, src, pad) = ((p >> 33) as u32, p >> 32 & 1 == 1, p as u32);
            let far = if src { 1 } else { 0 };
            let mut at = s.pads.partition_point(|x| x.0 < p);
            while let Some(&(_, k)) = s.pads.get(at).filter(|x| x.0 == p) {
                at += 1;
                let k = k as usize;
                let (ends, i, live, taken) = s.ends[k];
                if !taken {
                    s.ends[k].3 = true;
                    s.scope.push(LinkId(i));
                }
                if !(pinning && live) {
                    visit(s, ends[far]);
                }
            }
            let desc = self.rows[el as usize].desc;
            for c in desc.couplings {
                let (k, o) = match *c {
                    Coupling::Through { sink, src } => (sink, src),
                    Coupling::Field { sink, src, rule: Rule::Same | Rule::Copy { .. }, .. } => {
                        (sink, src)
                    }
                    Coupling::Field { .. } => continue,
                };
                match src {
                    true if o == pad => visit(s, key(el, false, k)),
                    false if k == pad => visit(s, key(el, true, o)),
                    _ => {}
                }
            }
            if desc.couplings.is_empty() && self.pad_is_wild(el, src, pad) {
                let added = &self.dyn_pads[el as usize];
                let n = if src {
                    desc.sinks.len() + added.sinks.len()
                } else {
                    desc.srcs.len() + added.srcs.len()
                };
                for q in (0..n as u32).filter(|q| self.pad_is_wild(el, !src, *q)) {
                    visit(s, key(el, !src, q));
                }
            }
        }
        s.scope.sort_unstable_by_key(|l| l.0);
    }

    fn run_scope(&self, s: &mut Solver) {
        let (mut mask, mut fed) = (std::mem::take(&mut s.mask), std::mem::take(&mut s.fed));
        self.fed_into(&mut mask, &mut fed);
        s.scope.clear();
        for (e, &i) in self.edges.iter().zip(&self.edge_ids) {
            if has(&mask, e.src) {
                s.scope.push(LinkId(i));
            }
        }
        (s.mask, s.fed) = (mask, fed);
    }

    fn gather(&self, s: &mut Solver, skip_filter: Option<LinkId>, probe: Option<LinkId>) {
        if s.families.0 != self.formats.len() {
            s.families = (
                self.formats.len(),
                [WILDCARD, "bytes"].map(|f| self.formats.get(f).map(FormatId)),
            );
        }
        s.begin(self.slots.len(), s.families.1);
        let pinning = self.live_run.is_some();
        let missed = std::mem::take(&mut s.missed);
        for &(el, key) in &missed {
            if let Some(o) =
                self.offers.get(el as usize).and_then(|os| os.iter().find(|o| o.key == key))
            {
                s.keep(o.menu, &o.offers);
            }
        }
        s.missed = missed;
        s.missed.clear();
        let mut at = 0;
        for i in 0..s.scope.len() {
            let link = s.scope[i];
            while self.edge_ids.get(at).is_some_and(|x| *x < link.0) {
                at += 1;
            }
            let Some(e) = self.edges.get(at).filter(|_| self.edge_ids[at] == link.0) else {
                continue;
            };
            let src = self.slot_for(s, e.slots[0], (true, e.src_pad.0), e.menus[0]);
            let sink = self.slot_for(s, e.slots[1], (false, e.sink_pad.0), e.menus[1]);
            let filter =
                e.filter.as_deref().filter(|_| skip_filter != Some(link)).map(|f| &f.offers);
            let pin = (pinning && e.live).then_some((&e.format, e.open));
            s.edge(link, (src, sink), filter, pin, probe == Some(link));
        }
        // A pad whose running edge the change cut still runs at that edge's format.
        for undo in self.live_run.as_ref().map_or(&[][..], |r| &r.journal[..]) {
            let Undo::Restore(_, e) = undo else { continue };
            if let Some(slot) = s.find(e.slots[0], true, e.src_pad.0).filter(|_| e.live) {
                s.pin_pad(slot, &e.format, e.open, NONE, None);
            }
        }
        for elem in 0..s.elem_count() {
            let el = s.elem_id(elem);
            let desc = self.rows[el as usize].desc;
            s.begin_rules(
                elem,
                !desc.sinks.is_empty() || !self.dyn_pads[el as usize].sinks.is_empty(),
            );
            for (i, c) in desc.couplings.iter().enumerate() {
                self.rule(s, elem, el, (i, c));
            }
            s.end_rules(elem, desc.couplings.is_empty());
        }
    }

    fn slot_for(&self, s: &mut Solver, el: u32, key: (bool, u32), menu: u32) -> u32 {
        if let Some(slot) = s.slot_kept(el, key, menu) {
            return slot;
        }
        let own = self.offers[el as usize].iter().find(|o| o.key == key);
        let (menu, offers) = own.map_or((NONE, &NO_OFFERS), |o| (o.menu, &o.offers));
        s.slot(el, key, menu, offers)
    }

    fn rule(&self, s: &mut Solver, elem: usize, el: u32, (at, c): (usize, &Coupling)) {
        let cached = &self.resolved[el as usize];
        let fresh = cached.names == self.fields.len();
        let ids = cached.ids.get(at).copied().filter(|_| fresh).unwrap_or_else(|| self.rule_ids(c));
        let (sink, src, rule) = match *c {
            Coupling::Through { sink, src } => {
                if let (Some(k), Some(o)) = (s.find(el, false, sink), s.find(el, true, src)) {
                    s.through(elem, k, o);
                }
                return;
            }
            Coupling::Field { sink, src, rule, .. } => (sink, src, rule),
        };
        let (Some(o), Some(to)) = (s.find(el, true, src), ids.to) else {
            return;
        };
        match rule {
            Rule::Same | Rule::Copy { .. } => {
                if let (Some(k), Some(from)) = (s.find(el, false, sink), ids.from) {
                    s.same(elem, (k, o), to, from, matches!(rule, Rule::Copy { .. }));
                }
            }
            Rule::Prop { prop } => match self.prop_at(el as usize, prop) {
                Some(v) => s.constant(o, to, v, true),
                None => s.chosen(o, to),
            },
            Rule::Fixed(_) => {
                if let Some(v) = ids.value {
                    s.constant(o, to, v, false);
                }
            }
            Rule::Takes => s.picks(o, to),
        }
    }

    // At run start and per commit, the taken values kept per edge.
    #[allow(clippy::disallowed_methods)]
    fn write_solved(&mut self, s: &Solver, solved: bool) {
        let mut at = 0;
        let mut vals = Vec::new();
        for (i, es) in s.edges.iter().enumerate().filter(|(_, e)| e.solved()) {
            at = match self.edge_ids[at..].iter().position(|x| *x >= es.link.0) {
                Some(k) if self.edge_ids[at + k] == es.link.0 => at + k,
                _ => match self.edge_at(es.link) {
                    Some(k) => k,
                    None => continue,
                },
            };
            if let Some(e) = self.edges.get_mut(at) {
                e.format.copy_from(s.out(i));
                (e.why, e.solved, e.open, e.risky) = (es.why, solved, es.open, es.risky);
                e.takes.clear();
                for (k, &(field, _)) in e.format.fields().iter().enumerate() {
                    if e.open >> k & 1 == 1 && s.takes(i, field, &mut vals) {
                        e.takes.push((field, vals.clone()));
                    }
                }
            }
        }
    }

    fn solve_run(&mut self, commit: bool) -> Result<(), Error> {
        let mut solver = self.solver.take().unwrap_or_default();
        self.run_scope(&mut solver);
        let mut vetted = Ok(());
        let mut at = 0;
        for el in (0..solver.mask.len()).map(|k| ElementId(solver.mask[k])) {
            while self.slots.map().get(at).is_some_and(|x| x.0 < el.0) {
                at += 1;
            }
            let i = self
                .slots
                .map()
                .get(at)
                .filter(|x| x.0 == el.0)
                .map_or(usize::MAX, |x| x.1 as usize);
            vetted = vetted.and_then(|()| self.need_preroll(el, i)).and_then(|()| self.vet(el, i));
        }
        let solved = vetted.and_then(|()| {
            self.gather(&mut solver, None, None);
            match solver.solve() {
                Ok(()) if commit => {
                    self.write_solved(&solver, true);
                    Ok(())
                }
                Ok(()) => Ok(()),
                Err(fail) => {
                    let fail = self.traced(&mut solver, fail);
                    Err(self.solve_error(&solver, fail))
                }
            }
        });
        let meets = solver.meets;
        self.solver = Some(solver);
        if commit && solved.is_ok() {
            self.plog(Level::Debug, format_args!("solved the run, {meets} meets"));
        }
        solved
    }

    /// Solve what a commit adds against the running edges. A closure with a cycle keeps what link
    /// time fixed.
    fn solve_fresh(&mut self, commit: bool) -> Result<(), Error> {
        let mut solver = self.solver.take().unwrap_or_default();
        self.closure(&mut solver, Scope::Fresh);
        self.gather(&mut solver, None, None);
        let solved = match solver.solve() {
            Ok(()) => {
                if commit {
                    self.write_solved(&solver, true);
                }
                Ok(())
            }
            Err(fail) if fail.kind == FailKind::Cycle => Ok(()),
            Err(fail) => {
                let fail = self.traced(&mut solver, fail);
                Err(self.solve_error(&solver, fail))
            }
        };
        let (meets, edges) = (solver.meets, solver.edges.len());
        self.solver = Some(solver);
        if commit && solved.is_ok() {
            self.plog(
                Level::Debug,
                format_args!("solved the change, {edges} edges in scope, {meets} meets"),
            );
        }
        solved
    }

    fn traced(&self, s: &mut Solver, fail: Fail) -> Fail {
        if fail.kind != FailKind::Empty {
            return fail;
        }
        s.trace = true;
        self.gather(s, None, None);
        let again = s.solve().err().unwrap_or(fail);
        s.trace = false;
        again
    }

    /// Solve and write nothing, a dry run of the run start or of the next commit.
    pub fn solve_check(&mut self) -> Result<(), Error> {
        match self.live_run.is_some() {
            true => self.solve_fresh(false),
            false => self.solve_run(false),
        }
    }

    /// The `meet` calls of the last solve, a measure of its cost.
    pub fn solve_meets(&self) -> u64 {
        self.solver.as_ref().map_or(0, |s| s.meets)
    }

    fn need_preroll(&self, element: ElementId, i: usize) -> Result<(), Error> {
        let (desc, probed) = (self.rows[i].desc, self.resolved[i].probed);
        match element.0 as usize >= self.prerolled && probed {
            true => {
                Err(Error::Graph(GraphError::NotPrerolled { element, element_name: desc.name }))
            }
            false => Ok(()),
        }
    }

    fn link_end(&self, s: &Solver, slot: u32, list: Option<List>) -> LinkEnd {
        let x = s.slots[slot as usize];
        let element = self.id_of(x.el);
        let pad = match x.src {
            true => self.src_name((element, SrcPad(x.pad))),
            false => self.sink_name((element, SinkPad(x.pad))),
        };
        let own = self.offers[x.el as usize].iter().find(|o| o.key == (x.src, x.pad));
        let names = [&self.formats, &self.fields, &self.values];
        LinkEnd {
            element,
            element_name: self.name_of(element),
            pad: pad.into(),
            offers: own.map(|o| o.lines.clone()).unwrap_or_default(),
            bound: list.filter(|_| x.coupled).map(|l| s.render(l, names).into()),
        }
    }

    // Error construction, cold.
    #[allow(clippy::disallowed_methods)]
    fn refused(&mut self, s: &mut Solver, link: LinkId, fail: Fail) -> Error {
        if fail.kind == FailKind::Inconsistent {
            return Error::Solve(SolveError::Inconsistent { link });
        }
        if fail.kind == FailKind::Wide {
            return Error::Solve(SolveError::TooManyFields {
                link: s.edges[fail.edge as usize].link,
            });
        }
        let Some(edge) = self.edge(link).cloned() else {
            return Error::Solve(SolveError::Inconsistent { link });
        };
        let at = |s: &Solver, el: u32, src: bool, pad: u32| s.find(el, src, pad).unwrap_or(NONE);
        if let Some(filter) = &edge.filter {
            self.gather(s, Some(link), None);
            if s.solve().is_ok() {
                let (a, b) = (
                    at(s, edge.slots[0], true, edge.src_pad.0),
                    at(s, edge.slots[1], false, edge.sink_pad.0),
                );
                let (src, sink, filter) =
                    (self.link_end(s, a, None), self.link_end(s, b, None), filter.line.clone());
                return Error::Solve(SolveError::FilterRejected(Box::new(FilterRejected {
                    src,
                    sink,
                    filter,
                })));
            }
        }
        // The rest of the graph without this edge says what each of its pads may carry.
        self.gather(s, None, Some(link));
        let told = s.solve().is_ok();
        let (a, b) = (
            at(s, edge.slots[0], true, edge.src_pad.0),
            at(s, edge.slots[1], false, edge.sink_pad.0),
        );
        let mut src = self.link_end(s, a, told.then(|| s.narrowed(a)));
        if let Some(carried) = s.carried(a).filter(|_| told) {
            src.bound = Some(s.render(carried, [&self.formats, &self.fields, &self.values]).into());
        }
        let sink = self.link_end(s, b, told.then(|| s.narrowed(b)));
        Error::Link(LinkError::NoCommonFormat(Box::new(NoCommonFormat { src, sink })))
    }

    // Error rendering, cold.
    #[allow(clippy::disallowed_methods)]
    fn solve_error(&self, s: &Solver, fail: Fail) -> Error {
        let edge = &s.edges[fail.edge as usize];
        let link = edge.link;
        let e = match fail.kind {
            FailKind::Wide => SolveError::TooManyFields { link },
            FailKind::Inconsistent | FailKind::Cycle => SolveError::Inconsistent { link },
            FailKind::Empty => {
                let src = self.link_end(s, edge.src, Some(fail.lists[0]));
                let sink = self.link_end(s, edge.sink, Some(fail.lists[1]));
                let field = fail.field.and_then(|f| self.fields.resolve(f.0)).map(Into::into);
                let walk = |mut at: u32| {
                    let mut out = Vec::new();
                    while let Some(step) = s.steps.get(at as usize) {
                        let x = s.slots[step.slot as usize];
                        let element = self.id_of(x.el);
                        let pad = match x.src {
                            true => self.src_name((element, SrcPad(x.pad))),
                            false => self.sink_name((element, SinkPad(x.pad))),
                        };
                        out.push(Narrowing {
                            link: (step.link != NONE).then_some(LinkId(step.link)),
                            element,
                            element_name: self.name_of(element),
                            pad: pad.into(),
                            rule: step.by.name(),
                            constraint: s.render_c(step.c, &self.values).into(),
                        });
                        at = step.prev;
                    }
                    out
                };
                let mut path = walk(fail.from[0]);
                path.reverse();
                path.extend(walk(fail.from[1]));
                SolveError::Empty(Box::new(EmptyIntersection {
                    link,
                    src,
                    sink,
                    field,
                    path: path.into(),
                }))
            }
        };
        Error::Solve(e)
    }

    fn put_edge(&mut self, link: LinkId, edge: Option<Edge>) {
        match (self.edge_ids.binary_search(&link.0), edge) {
            (Ok(k), Some(e)) => self.edges[k] = e,
            (Err(k), Some(e)) => {
                self.edge_ids.insert(k, link.0);
                self.edges.insert(k, e);
            }
            (Ok(k), None) => {
                self.edge_ids.remove(k);
                drop(self.edges.remove(k));
            }
            (Err(_), None) => {}
        }
    }

    fn take_edge(&mut self, link: LinkId) -> Option<Edge> {
        let k = self.edge_at(link)?;
        self.edge_ids.remove(k);
        Some(self.edges.remove(k))
    }

    fn edge_at(&self, link: LinkId) -> Option<usize> {
        self.edge_ids.binary_search(&link.0).ok()
    }

    fn edge(&self, link: LinkId) -> Option<&Edge> {
        self.edge_at(link).map(|k| &self.edges[k])
    }

    fn links(&self) -> impl Iterator<Item = &Edge> {
        self.edges.iter()
    }

    /// Take a link away, in a run as part of the next commit.
    pub fn unlink(&mut self, link: LinkId) -> Result<(), Error> {
        let none = Error::Topology(TopologyError::NoSuchLink { link });
        let edge = self.take_edge(link).ok_or(none)?;
        if let Some(run) = &mut self.live_run {
            run.journal.push(Undo::Restore(link, edge));
        }
        self.publish();
        Ok(())
    }

    /// Move the sink end of a link to another sink pad, keeping its id.
    pub fn relink(&mut self, link: LinkId, sink: (ElementId, SinkPad)) -> Result<(), Error> {
        let none = Error::Topology(TopologyError::NoSuchLink { link });
        let old = self.take_edge(link).ok_or(none)?;
        let (from, filter) = ((old.src, old.src_pad), old.filter.clone());
        // Journaled first, so the solve sees the src pad's running format.
        if let Some(run) = &mut self.live_run {
            run.journal.push(Undo::Restore(link, old.clone()));
        }
        if let Err(e) = self.link_at(Some(link), from, sink, filter) {
            if let Some(run) = &mut self.live_run {
                run.journal.pop();
            }
            self.put_edge(link, Some(old));
            return Err(e);
        }
        self.publish();
        Ok(())
    }

    /// Remove an element and its links, in a run as part of the next commit.
    pub fn remove(&mut self, el: ElementId, how: Leave) -> Result<(), Error> {
        self.live_desc(el)?;
        let i = self.ix(el);
        // From the end, as an unlink shifts the edges after the one it takes.
        for k in (0..self.edge_ids.len()).rev() {
            if self.edges.get(k).is_some_and(|e| e.src == el || e.sink == el) {
                self.unlink(LinkId(self.edge_ids[k]))?;
            }
        }
        match &mut self.live_run {
            Some(run) if self.elements[i].is_none() => run.leaving.push((el, how)),
            _ => self.forget(el),
        }
        Ok(())
    }

    // Empty tables allocate nothing.
    #[allow(clippy::disallowed_methods)]
    fn forget(&mut self, el: ElementId) {
        let Some(i) = self.slots.free(el.0) else {
            return;
        };
        let (props, counters) = (Arc::clone(&self.vacant.0), Arc::clone(&self.vacant.1));
        self.rows[i] = Row { desc: &VACANT, props, counters, gone: true, levels: Vec::new() };
        (self.elements[i], self.tune[i], self.resolved[i]) =
            (None, Tune::default(), Resolved::default());
        (self.dyn_pads[i], self.offers[i], self.streams[i]) =
            (DynPads::default(), Vec::new(), Vec::new());
        self.stream_pads[i] = Vec::new();
        if self.pools.binary_search_by_key(&el.0, |p| p.0).is_ok() {
            self.fading.push(el.0);
        }
        self.dirty.2 = true;
        self.touched(i);
        self.publish();
    }

    /// How many elements were ever added, which is the next element's id.
    pub fn element_count(&self) -> usize {
        self.slots.next() as usize
    }

    /// Rows the per-element tables hold, the free ones among them, and the edges held.
    pub fn table_rows(&self) -> (usize, usize, usize) {
        (self.slots.len(), self.slots.len() - self.slots.map().len(), self.edges.len())
    }

    /// The streams `el` described at preroll, indexed by stream number.
    pub fn streams(&self, el: ElementId) -> &[Stream] {
        self.slot(el).map_or(&[], |s| self.streams[s].as_slice())
    }

    /// Whether preroll selects every stream an element describes, on by default.
    pub fn set_select_all(&mut self, on: bool) {
        self.select_all = on;
    }

    /// Select a stream of `el`, giving it a src pad to link.
    // One pad name per stream ever selected.
    #[allow(clippy::disallowed_methods)]
    pub fn select(&mut self, el: ElementId, stream: u32) -> Result<SrcPad, Error> {
        let i = self.ix(el);
        let none = Error::Topology(TopologyError::NoSuchStream { element: el, stream });
        let of = self.streams.get(i).and_then(|s| s.get(stream as usize)).ok_or(none)?;
        if let Some((_, pad)) = self.stream_pads[i].iter().find(|(s, _)| *s == stream) {
            return Ok(*pad);
        }
        let pad = SrcPad((self.rows[i].desc.srcs.len() + self.dyn_pads[i].srcs.len()) as u32);
        self.dyn_pads[i].srcs.push(DynPad { name: of.name.to_string(), offers: of.offers });
        self.stream_pads[i].push((stream, pad));
        self.touched(i);
        self.publish();
        Ok(pad)
    }

    pub fn stream_pad(&self, el: ElementId, stream: u32) -> Option<SrcPad> {
        let pads = &self.stream_pads[self.slot(el)?];
        pads.iter().find(|(s, _)| *s == stream).map(|(_, pad)| *pad)
    }

    fn offers_of(
        &mut self,
        el: ElementId,
        key: (bool, u32),
        menu: &'static [OfferDesc],
    ) -> Result<usize, Error> {
        let i = self.ix(el);
        let at = self.offers[i].iter().position(|o| o.key == key);
        if let Some(at) = at {
            return Ok(at);
        }
        let (src, sink) = ((el, SrcPad(key.1)), (el, SinkPad(key.1)));
        let name: Box<str> = if key.0 { self.src_name(src) } else { self.sink_name(sink) }.into();
        if OfferDesc::is_probed(menu) {
            let element_name = self.name_of(el);
            let pending = LinkError::OffersPending { element: el, element_name, pad: name };
            return Err(Error::Link(pending));
        }
        let names = [&mut self.formats, &mut self.fields, &mut self.values];
        let (offers, lines) = (Interned::intern(menu, names), menu.iter().map(|o| o.family.into()));
        let next = self.menu_ids;
        let menu = *self.menus.entry((menu.as_ptr() as usize, menu.len())).or_insert(next);
        self.menu_ids += (menu == next) as u32;
        let own = PadOffers { key, name, offers, lines: lines.collect(), menu };
        self.offers[i].push(own);
        Ok(self.offers[i].len() - 1)
    }

    pub fn element_desc(&self, el: ElementId) -> Option<&'static ElementDesc> {
        self.live_desc(el).ok()
    }

    fn live_desc(&self, el: ElementId) -> Result<&'static ElementDesc, Error> {
        let i = self.ix(el);
        let here = self.elements.get(i).is_some_and(Option::is_some);
        match self.rows.get(i) {
            Some(row) if here || self.group_of(el).is_some() => Ok(row.desc),
            _ => Err(Error::Link(LinkError::UnknownElement { element: el })),
        }
    }

    fn group_of(&self, el: ElementId) -> Option<usize> {
        self.live_run.as_ref()?.groups.iter().position(|(ids, _)| ids.contains(&el))
    }

    fn src_at(&self, el: ElementId, pad: SrcPad) -> Result<(&str, &'static [OfferDesc]), Error> {
        let desc = self.live_desc(el)?;
        pad_at(desc.srcs, &self.dyn_pads[self.ix(el)].srcs, pad.0)
            .ok_or(Error::Link(LinkError::NoSrcPad { element: el, element_name: desc.name, pad }))
    }

    fn sink_at(&self, el: ElementId, pad: SinkPad) -> Result<(&str, &'static [OfferDesc]), Error> {
        let desc = self.live_desc(el)?;
        pad_at(desc.sinks, &self.dyn_pads[self.ix(el)].sinks, pad.0)
            .ok_or(Error::Link(LinkError::NoSinkPad { element: el, element_name: desc.name, pad }))
    }

    fn src_name(&self, (el, pad): (ElementId, SrcPad)) -> &str {
        self.src_at(el, pad).map_or("?", |(name, _)| name)
    }

    fn sink_name(&self, (el, pad): (ElementId, SinkPad)) -> &str {
        self.sink_at(el, pad).map_or("?", |(name, _)| name)
    }

    /// Find a src pad by name, static or added at preroll.
    pub fn src_pad(&self, el: ElementId, name: &str) -> Result<SrcPad, Error> {
        let desc = self.live_desc(el)?;
        pad_named(desc.srcs, &self.dyn_pads[self.ix(el)].srcs, name).map(SrcPad).ok_or_else(|| {
            Error::Link(LinkError::UnknownSrcPad {
                element: el,
                element_name: desc.name,
                pad: name.into(),
            })
        })
    }

    /// Find a sink pad by name, static or added at preroll.
    pub fn sink_pad(&self, el: ElementId, name: &str) -> Result<SinkPad, Error> {
        let desc = self.live_desc(el)?;
        pad_named(desc.sinks, &self.dyn_pads[self.ix(el)].sinks, name).map(SinkPad).ok_or_else(
            || {
                Error::Link(LinkError::UnknownSinkPad {
                    element: el,
                    element_name: desc.name,
                    pad: name.into(),
                })
            },
        )
    }

    /// The offer families a src pad declares, in declaration order.
    pub fn src_families(&self, el: ElementId, pad: SrcPad) -> Option<Vec<&'static str>> {
        let (_, offers) = self.src_at(el, pad).ok().filter(|(_, o)| !OfferDesc::is_probed(o))?;
        Some(offers.iter().map(|o| o.family).collect())
    }

    fn name_of(&self, el: ElementId) -> &'static str {
        self.rows.get(self.ix(el)).map_or("?", |row| row.desc.name)
    }

    fn pad_count(&self, el: ElementId) -> (usize, usize) {
        let Some(i) = self.slot(el) else {
            return (0, 0);
        };
        let (row, d) = (&self.rows[i], &self.dyn_pads[i]);
        (row.desc.srcs.len() + d.srcs.len(), row.desc.sinks.len() + d.sinks.len())
    }

    /// The link into sink pad `pad` of `el` and the src pad it leaves, `None` unlinked.
    pub fn link_into(&self, el: ElementId, pad: SinkPad) -> Option<(LinkId, ElementId, SrcPad)> {
        let into = |(e, i): (&Edge, &u32)| {
            ((e.sink, e.sink_pad) == (el, pad)).then_some((LinkId(*i), e.src, e.src_pad))
        };
        self.edges.iter().zip(&self.edge_ids).find_map(into)
    }

    pub fn negotiated(&self, link: LinkId) -> Option<&FixedFormat> {
        self.edge(link).map(|e| &e.format)
    }

    /// Fields of a link's format decided by the announcement while playing that the sink pad does
    /// not take at every value.
    pub fn unsettled(&self, link: LinkId) -> impl Iterator<Item = FieldId> + '_ {
        let fields = self.edge(link).map(|e| (e.format.fields(), e.risky));
        fields.into_iter().flat_map(|(f, risky)| {
            f.iter().enumerate().filter(move |(i, _)| risky >> i & 1 == 1).map(|(_, x)| x.0)
        })
    }

    /// The values of `field` downstream of a link takes, `None` where it takes any or the field is
    /// fixed.
    pub fn takes(&self, link: LinkId, field: FieldId) -> Option<&[Value]> {
        let e = self.edge(link)?;
        e.takes.iter().find(|(f, _)| *f == field).map(|(_, v)| v.as_slice())
    }

    pub fn family_name(&self, id: FormatId) -> Option<&str> {
        self.formats.resolve(id.0)
    }

    pub fn field_name(&self, id: FieldId) -> Option<&str> {
        self.fields.resolve(id.0)
    }

    pub fn field_id(&self, name: &str) -> Option<FieldId> {
        self.fields.get(name).map(FieldId)
    }

    pub fn value_id(&self, name: &str) -> Option<ValueId> {
        self.values.get(name).map(ValueId)
    }

    /// Preroll the elements added since the last call, collecting the pads they add.
    // Once per element, before run.
    #[allow(clippy::disallowed_methods)]
    pub fn preroll(&mut self) -> Result<AddedPads, Error> {
        let pool = Pool::bounded(self.slot_size, 1);
        let mut appeared = AddedPads::default();
        let mut next = self.slots.map().partition_point(|x| (x.0 as usize) < self.prerolled);
        while let Some(&(id, i)) = self.slots.map().get(next) {
            next += 1;
            let (id, i) = (ElementId(id), i as usize);
            let Some(desc) = self.elements[i].as_ref().map(|e| e.desc()) else {
                continue;
            };
            let mut ctx =
                Ctx::new(pool.clone(), self.bus_sender.clone(), id, BYTES, self.tune[i].credits);
            let (nsrcs, nsinks) = (desc.srcs.len(), desc.sinks.len());
            ctx.configure_pads(nsrcs, nsinks);
            ctx.set_trace(Arc::clone(&self.rows[i].counters), Arc::clone(&self.tracing));
            let take = std::mem::take::<Interner>;
            let (formats, fields) = (take(&mut self.formats), take(&mut self.fields));
            let names = Vocabulary { formats, fields, values: take(&mut self.values) };
            ctx.probe = Some((desc, names, Vec::new()));
            let result = self.elements[i].as_mut().map_or(Ok(()), |el| el.preroll(&mut ctx));
            let Some((_, names, found)) = ctx.probe.take() else {
                continue;
            };
            (self.formats, self.fields, self.values) = (names.formats, names.fields, names.values);
            result?;
            for mut f in found {
                (f.menu, self.menu_ids) = (self.menu_ids, self.menu_ids + 1);
                self.offers[i].push(f);
            }
            let (srcs, sinks) = ctx.take_added_pads();
            for (k, ap) in srcs.into_iter().enumerate() {
                let pad = SrcPad((nsrcs + k) as u32);
                let format = self.announce_added(id, (true, pad.0), &ap);
                self.bus_sender.send(BusMessage::SrcPadAdded { element: id, pad, format });
                appeared.srcs.push(AddedPad { element: id, pad, name: ap.name.clone() });
                self.dyn_pads[i].srcs.push(ap);
            }
            for (k, ap) in sinks.into_iter().enumerate() {
                let pad = SinkPad((nsinks + k) as u32);
                let format = self.announce_added(id, (false, pad.0), &ap);
                self.bus_sender.send(BusMessage::SinkPadAdded { element: id, pad, format });
                appeared.sinks.push(AddedPad { element: id, pad, name: ap.name.clone() });
                self.dyn_pads[i].sinks.push(ap);
            }
            self.streams[i] = ctx.take_streams();
            for stream in 0..if self.select_all { self.streams[i].len() as u32 } else { 0 } {
                let pad = self.select(id, stream)?;
                let name = self.streams[i][stream as usize].name.to_string();
                appeared.srcs.push(AddedPad { element: id, pad, name });
            }
            self.prerolled = id.0 as usize + 1;
        }
        Ok(appeared)
    }

    fn announce_added(&mut self, id: ElementId, key: (bool, u32), ap: &DynPad) -> FixedFormat {
        let own = self.offers[self.ix(id)].iter().find(|o| o.key == key);
        let family = match ap.offers.first() {
            Some(o) if o.family != PROBED => Some(FormatId(self.formats.intern(o.family))),
            _ => own.and_then(|o| o.offers.offers.first()).map(|o| o.0),
        };
        self.plog(
            Level::Info,
            format_args!(
                "pad added {}.{} ({}, offers {:?})",
                self.display_of(id),
                ap.name,
                if key.0 { "src" } else { "sink" },
                own.map_or_else(
                    || ap.offers.iter().map(|o| o.family.into()).collect(),
                    |o| o.lines.clone()
                ),
            ),
        );
        FixedFormat::new(family.unwrap_or(BYTES))
    }

    /// Run the pipeline to EOS on one thread per group and return the first error.
    // The list of group threads, once per run.
    #[allow(clippy::disallowed_methods)]
    pub fn run(&mut self) -> Result<(), Error> {
        let (mut setups, end) = self.build_run(true)?;
        let control = Arc::clone(&self.control);
        control.topology.serve(true);
        // Unwinding out of here aborts the run and joins the groups, so no thread outlives it.
        let mut guard = RunGuard { control: Arc::clone(&control), threads: Vec::new(), armed: true };
        let mut first_err = None;
        loop {
            // Sampled first, so whatever is signaled from here on ends the park.
            let seen = control.topology.driver.epoch();
            for setup in setups.drain(..).chain(control.topology.take_setups()) {
                let spawned = self.live_run.as_mut().map(|run| &mut run.spawned);
                let gi = spawned.map_or(0, |spawned| std::mem::replace(spawned, *spawned + 1));
                let head = setup.ids[0];
                match spawn_group(gi, setup) {
                    Ok(t) => guard.threads.push((head, t)),
                    Err(e) => {
                        let kind = crate::io::IoError::Os(e.raw_os_error().unwrap_or(0));
                        first_err.get_or_insert(Error::Io { what: "spawn group thread", kind });
                        control.abort();
                    }
                }
            }
            for id in control.topology.take_exited() {
                let Some(at) = guard.threads.iter().position(|(_, t)| t.thread().id() == id) else {
                    continue;
                };
                let (head, thread) = guard.threads.swap_remove(at);
                let err = match thread.join() {
                    Ok(Ok(())) => continue,
                    Ok(Err(e)) => e,
                    Err(_) => Error::GroupPanicked {
                        element: head,
                        message: "outside the group's run".into(),
                    },
                };
                first_err.get_or_insert(err);
            }
            if guard.threads.is_empty() {
                break;
            }
            while let Some(edit) = control.topology.take_edit() {
                edit(self);
            }
            control.topology.advance(&control, &self.bus_sender);
            // Pools of elements a committed change removed go back as soon as their group is out.
            self.settle();
            // A backstop, the groups signal the steps a change waits on.
            let within = control.topology.in_flight().then_some(Duration::from_millis(20));
            control.topology.driver.park(seen, within, None);
        }
        guard.armed = false;
        let (report, result) = end.finish(first_err);
        control.topology.serve(false);
        self.settle();
        self.reap(true);
        self.live_run = None;
        self.last_report = Some(report);
        result
    }

    /// Build a run driven one pass at a time on the caller's thread.
    pub fn step(&mut self, clock: Arc<dyn Clock>) -> Result<Stepped, Error> {
        self.set_clock(Arc::clone(&clock));
        let (setups, end) = self.build_run(false)?;
        Stepped::open(setups, clock, Arc::clone(&self.control), end)
    }

    // Run setup, once per run.
    #[allow(clippy::disallowed_methods)]
    fn build_run(&mut self, threaded: bool) -> Result<(Vec<GroupSetup>, RunEnd), Error> {
        self.control.clear_stop();
        let part = self.part();
        for &(id, i) in self.slots.map().iter().filter(|(id, _)| !has(&part, ElementId(*id))) {
            let (element, element_name) = (ElementId(id), self.rows[i as usize].desc.name);
            let graph = match self.links().any(|e| e.src == element || e.sink == element) {
                true => GraphError::Starved { element, element_name },
                false => GraphError::Unlinked { element, element_name },
            };
            self.bus_sender.send(BusMessage::Warning { element, error: Error::Graph(graph) });
        }
        let order = self.topo_order_of(&part)?;
        self.need_reactor(&order)?;
        self.solve_run(true)?;
        let clock = self.select_clock(&order);
        // Clock before base, so a tap never pairs the new base with the old clock.
        self.publish();
        self.base_shared.store(crate::time::base_from_clock(clock.now()), Ordering::Release);
        if threaded {
            clock.attach(crate::clock::GroupWake::new(&self.control));
        }
        let groups = self.compute_groups(&order)?;
        for (gi, ids) in groups.iter().enumerate() {
            self.plog(
                Level::Info,
                format_args!(
                    "group {gi} (thread): {}",
                    ids.iter().map(|i| self.display_of(*i)).collect::<Vec<_>>().join(" ! "),
                ),
            );
        }
        self.build_pools(groups.len(), &part);
        let stations: Vec<Arc<Station>> = groups.iter().map(|_| Arc::new(Station::new())).collect();
        let (downstream, upstream) = self.build_links(&groups, &stations)?;
        let plan = Plan { threaded, clock, groups, stations, downstream, upstream };
        let (setups, drains) = self.group_setups(plan);
        let (log_thread, drains) = match threaded {
            true => {
                let spawned = drains.map(LogDrainThread::spawn).transpose().map_err(|e| {
                    let kind = crate::io::IoError::Os(e.raw_os_error().unwrap_or(0));
                    Error::Io { what: "spawn log drain", kind }
                })?;
                (spawned, None)
            }
            false => (None, drains),
        };
        let (bus, control) = (self.bus_sender.clone(), Arc::clone(&self.control));
        let (log_rank, writer) = (self.plog_rank(), self.log_writer.clone());
        self.publish();
        let view = self.view.reader();
        Ok((setups, RunEnd { view, log_thread, drains, bus, control, log_rank, writer }))
    }

    fn select_clock(&mut self, order: &[ElementId]) -> Arc<dyn Clock> {
        if self.clock_explicit {
            self.plog(Level::Info, format_args!("clock <- explicit (set_clock)"));
            return Arc::clone(&self.clock);
        }
        let mut provider = None;
        for id in order.iter().rev() {
            let i = self.ix(*id);
            let offered = self.elements[i].as_mut().and_then(|el| el.provide_clock());
            if let Some(c) = offered {
                self.clock = c;
                provider = Some(*id);
                break;
            }
        }
        match provider {
            Some(p) => self.plog(
                Level::Info,
                format_args!(
                    "clock <- {} (sink-provided, masters the pipeline)",
                    self.display_of(p)
                ),
            ),
            None => self.plog(Level::Info, format_args!("clock <- default (InstantClock)")),
        }
        Arc::clone(&self.clock)
    }

    fn pools_worst(&self, ids: &[u32]) -> u128 {
        let worst = |s: PoolSize| s.slot as u128 * s.slots as u128 + s.bytes as u128;
        ids.iter().map(|i| worst(self.pool_size(self.ix(ElementId(*i))))).sum()
    }

    fn pool_of(&self, id: ElementId, all: u128) -> Pool {
        let el = self.ix(id);
        let PoolSize { slot, slots, bytes } = self.pool_size(el);
        let cap = self.pool_cap as u128;
        let cut = |n: u128| (n * cap / all).max(1);
        let (slots, bytes) = match all > cap {
            true => (cut(slots as u128) as u32, cut(bytes as u128).max(slot as u128) as u64),
            false => (slots, bytes),
        };
        let heap = (crate::memory::MIN_ALIGN, Allocator::HEAP);
        let tune = self.tune.get(el).copied().unwrap_or_default();
        let (align, allocator) = tune.alloc.unwrap_or(heap);
        let config = PoolConfig { slot_size: slot, slots, bytes, align, allocator };
        let pool = Pool::named(config, self.name_of(id));
        if let Some((size, count)) = tune.reserve {
            let count = if all > cap { cut(count as u128) as u32 } else { count };
            if let Err(e) = pool.reserve_sized(size, count) {
                self.plog(
                    Level::Warn,
                    format_args!("{}: pool reserve refused, {e}", self.display_of(id)),
                );
            }
        }
        pool
    }

    fn pool_size(&self, el: usize) -> PoolSize {
        let own = self.tune.get(el).and_then(|t| t.pool);
        let bytes = self.slot_size as u64 * self.pool_slots as u64;
        own.unwrap_or(PoolSize { slot: self.slot_size, slots: self.pool_slots as u32, bytes })
    }

    fn build_pools(&mut self, groups: usize, part: &[u32]) {
        let all = self.pools_worst(part);
        self.pools = self
            .slots
            .map()
            .iter()
            .map(|(id, _)| (*id, self.pool_of(ElementId(*id), all)))
            .collect();
        (self.retired, self.dirty.3) = (PoolStats::default(), true);
        self.fading.clear();
        self.pools.iter().for_each(|(_, p)| p.reserve_starved(groups));
    }

    // The link tables, once per run.
    #[allow(clippy::disallowed_methods)]
    fn build_links(
        &mut self,
        groups: &[Vec<ElementId>],
        stations: &[Arc<Station>],
    ) -> Result<Links, Error> {
        let mut group_of = vec![0usize; self.slots.len()];
        for (gi, ids) in groups.iter().enumerate() {
            for id in ids {
                group_of[self.ix(*id)] = gi;
            }
        }
        let mut downstream: Vec<Vec<DownLink>> = groups.iter().map(|_| Vec::new()).collect();
        let mut upstream: Vec<Vec<UpLink>> = groups.iter().map(|_| Vec::new()).collect();
        let part = self.part();
        let links = self.edge_ids.iter().map(|i| LinkId(*i)).zip(&self.edges);
        for (link, edge) in links.filter(|(_, e)| has(&part, e.src)) {
            let (gs, gd) = (group_of[edge.slots[0] as usize], group_of[edge.slots[1] as usize]);
            if gs == gd {
                continue;
            }
            if groups[gs].last() != Some(&edge.src) {
                let (src, sink) = (edge.src, edge.sink);
                return Err(Error::Graph(GraphError::NonTailBranch { src, sink }));
            }
            let (down, up) = self.ring(edge, link, &stations[gs], &stations[gd]);
            note_level((&mut self.rows, &mut self.dirty.0), edge, &down.level);
            downstream[gs].push(down);
            upstream[gd].push(up);
        }
        Ok((downstream, upstream))
    }

    // Run setup, once per run.
    #[allow(clippy::disallowed_methods)]
    fn group_setups(&mut self, plan: Plan) -> (Vec<GroupSetup>, Option<Drains>) {
        let Plan { threaded, clock, groups, stations, mut downstream, mut upstream } = plan;
        let none: ReactorFactory = Arc::new(|| Ok(Box::<NoBackend>::default() as _));
        let factory = self.reactor_factory.clone().unwrap_or(none);
        let gate = self.log_gate();
        let stamps = (!threaded).then_some(&clock);
        let mut ids: Vec<u32> = groups.iter().flatten().map(|el| el.0).collect();
        ids.sort_unstable();
        let (logs, held) = self.logs_of(gate.as_ref(), stamps, &ids);
        let (drains, joining) = spsc(DRAINS_WAITING);
        let pools =
            ids.iter().filter_map(|i| find_ref(&self.pools, ElementId(*i)).cloned()).collect();
        let mut tables = self.tables(ids, pools, logs);
        // Cursors are made past every fallible step, so a failed setup leaves none unretired.
        let terminal: Vec<bool> = downstream.iter().map(Vec::is_empty).collect();
        self.control.set_sinks(self.sinks(), self.element_count());
        let (cursors, seek_gen) = self.control.begin_run(
            Arc::clone(&clock),
            stations.clone(),
            &terminal,
            self.initial_paused,
        );
        let wall = threaded.then(Instant::now);
        let shared = self.shared(&clock, wall);
        let mut setups = Vec::new();
        for (gi, ids) in groups.iter().enumerate() {
            let (cursor, station) = (Arc::clone(&cursors[gi]), Arc::clone(&stations[gi]));
            let (down, up) =
                (std::mem::take(&mut downstream[gi]), std::mem::take(&mut upstream[gi]));
            let wiring =
                Wiring { cursor, seek_gen, station, downstream: down, upstream: up, join: None };
            setups.push(self.group_setup(ids.clone(), &mut tables, &shared, &factory, wiring));
        }
        self.edges.iter_mut().for_each(|e| e.live = true);
        let own = self.pools.iter().map(|(i, pool)| (ElementId(*i), pool.clone())).collect();
        let latency = tables.ids.iter().copied().zip(tables.path_latency).collect();
        let pads = tables.ids.iter().copied().zip(tables.pad_counts).collect();
        let groups = groups.into_iter().zip(stations).collect();
        let (journal, leaving, flight) = (Vec::new(), Vec::new(), None);
        let writer = self.log_writer.clone();
        let styled = writer.is_none() && crate::log::stderr_colors_enabled();
        let logging = gate.is_some().then_some(Drains { held, joining, writer, styled });
        self.live_run = Some(LiveRun {
            clock,
            wall,
            groups,
            factory,
            own,
            parting: Vec::new(),
            latency,
            pads,
            journal,
            leaving,
            flight,
            spawned: 0,
            gate,
            drains,
        });
        (setups, logging)
    }

    // Once per run and per change, tables of the elements that join.
    #[allow(clippy::disallowed_methods)]
    fn tables(&self, ids: Vec<u32>, pools: Vec<Pool>, logs: Vec<Option<Log>>) -> Tables {
        let pad_counts: Vec<(usize, usize)> =
            ids.iter().map(|i| self.pad_count(ElementId(*i))).collect();
        let table = |&(srcs, sinks): &(usize, usize)| PadFormats {
            sinks: vec![None; sinks],
            srcs: vec![None; srcs],
            ..Default::default()
        };
        let mut negotiated: Vec<PadFormats> = pad_counts.iter().map(table).collect();
        let mut linked_pads: Vec<Vec<bool>> =
            pad_counts.iter().map(|&(srcs, _)| vec![false; srcs]).collect();
        let (part, lat, _) = self.compute_in_latency();
        for e in self.links() {
            let (src, live) = (at(&ids, e.src), has(&part, e.src));
            if let Some(slot) = src.and_then(|k| linked_pads[k].get_mut(e.src_pad.0 as usize)) {
                *slot = true;
            }
            if !live {
                continue;
            }
            let slot = src.and_then(|k| negotiated[k].srcs.get_mut(e.src_pad.0 as usize));
            if let Some(slot) = slot.filter(|f| f.as_ref().is_none_or(|f| f.family == BYTES)) {
                *slot = Some(e.format.clone());
            }
            if let Some(k) = src.filter(|_| e.format.family != BYTES) {
                negotiated[k].hold_to(e.src_pad, &e.takes);
            }
            let sink =
                at(&ids, e.sink).and_then(|k| negotiated[k].sinks.get_mut(e.sink_pad.0 as usize));
            if let Some(slot) = sink {
                *slot = Some(e.format.clone());
            }
        }
        let latency = |i: &u32| at(&part, ElementId(*i)).map_or(Timestamp::ZERO, |k| lat[k]);
        let path_latency = ids.iter().map(latency).collect();
        Tables { ids, pad_counts, negotiated, path_latency, linked_pads, pools, logs }
    }

    // Once per run and per change.
    #[allow(clippy::disallowed_methods)]
    fn sinks(&self) -> Vec<u32> {
        let mut feeds: Vec<u32> = self.links().map(|e| e.src.0).collect();
        feeds.sort_unstable();
        let mut part = self.part();
        part.retain(|el| feeds.binary_search(el).is_err());
        part
    }

    fn shared(&self, clock: &Arc<dyn Clock>, wall: Option<Instant>) -> GroupShared {
        GroupShared {
            vocabulary: self.vocabulary(),
            clock: Arc::clone(clock),
            base: Arc::clone(&self.base_shared),
            tracing: Arc::clone(&self.tracing),
            wall,
            control: Arc::clone(&self.control),
        }
    }

    fn group_setup(
        &mut self,
        ids: Vec<ElementId>,
        t: &mut Tables,
        shared: &GroupShared,
        factory: &ReactorFactory,
        wiring: Wiring,
    ) -> GroupSetup {
        let ks: Vec<usize> = ids.iter().map(|id| t.ids.partition_point(|x| *x < id.0)).collect();
        debug_assert!(ids.iter().zip(&ks).all(|(id, k)| t.ids.get(*k) == Some(&id.0)));
        let at: Vec<usize> = ids.iter().map(|id| self.ix(*id)).collect();
        for (i, k) in at.iter().zip(&ks) {
            t.negotiated[*k].offers = self.offers[*i].clone();
        }
        let elements = at.iter().filter_map(|i| self.elements[*i].take());
        GroupSetup {
            linked_pads: ks.iter().map(|k| t.linked_pads[*k].clone()).collect(),
            path_latencies: ks.iter().map(|k| t.path_latency[*k]).collect(),
            pad_counts: ks.iter().map(|k| t.pad_counts[*k]).collect(),
            shared: shared.clone(),
            cursor: wiring.cursor,
            seek_gen: wiring.seek_gen,
            station: wiring.station,
            backstop: self.backstop,
            props: at.iter().map(|i| Arc::clone(&self.rows[*i].props)).collect(),
            counters: at.iter().map(|i| Arc::clone(&self.rows[*i].counters)).collect(),
            credits: at.iter().map(|i| self.tune[*i].credits).collect(),
            bus: self.bus_sender.clone(),
            pools: ks.iter().map(|k| t.pools[*k].clone()).collect(),
            factory: Arc::clone(factory),
            downstream: wiring.downstream,
            upstream: wiring.upstream,
            logs: ks.iter().map(|k| t.logs.get_mut(*k).and_then(Option::take)).collect(),
            formats: ks.iter().map(|k| std::mem::take(&mut t.negotiated[*k])).collect(),
            stream_pads: at.iter().map(|i| self.stream_pads[*i].clone()).collect(),
            join: wiring.join,
            elements: elements.collect(),
            ids,
        }
    }

    fn vocabulary(&self) -> Arc<Vocabulary> {
        let (formats, fields) = (self.formats.clone(), self.fields.clone());
        Arc::new(Vocabulary { formats, fields, values: self.values.clone() })
    }

    fn log_gate(&self) -> Option<LogGate> {
        let spec = &self.debug;
        let rank = self.log_level.map_or(0, Level::rank).max(spec.global.unwrap_or(0));
        if rank == 0 && spec.targets.iter().all(|(_, r)| *r == 0) {
            return None;
        }
        let filter = LevelFilter::new();
        filter.set(Level::from_rank(rank));
        Some(LogGate { rank, shared: Arc::new(filter) })
    }

    // Logging wiring, once per run and per change, not the per-buffer path.
    #[allow(clippy::disallowed_methods)]
    fn logs_of(
        &self,
        gate: Option<&LogGate>,
        stamps: Option<&Arc<dyn Clock>>,
        ids: &[u32],
    ) -> (Vec<Option<Log>>, Vec<LogDrain>) {
        let mut logs: Vec<Option<Log>> = ids.iter().map(|_| None).collect();
        let mut drains = Vec::new();
        let Some(gate) = gate else {
            return (logs, drains);
        };
        for (k, i) in ids.iter().map(|i| *i as usize).enumerate() {
            let name = self.name_of(ElementId(i as u32));
            let named = self.debug.targets.iter().filter(|(n, _)| n == name);
            let target = named.map(|(_, r)| *r).max().unwrap_or(0);
            let rank = gate.rank.max(target);
            if rank == 0 {
                continue;
            }
            let filter = match target > gate.rank {
                true => Arc::new(LevelFilter::with_level(
                    Level::from_rank(rank).unwrap_or(Level::Error),
                )),
                false => Arc::clone(&gate.shared),
            };
            let (sink, drain) = log_channel(self.log_queue_cap);
            let mut log = Log::new(sink, filter);
            log.set_element(ElementId(i as u32), name);
            log.set_session(self.log_session);
            if let Some(clock) = stamps {
                log.set_clock(Arc::clone(clock));
            }
            logs[k] = Some(log);
            drains.push(drain);
        }
        (logs, drains)
    }

    pub fn last_report(&self) -> Option<RunReport> {
        self.last_report
    }

    // Once per run and per change.
    #[allow(clippy::disallowed_methods)]
    fn part(&self) -> Vec<u32> {
        let (mut part, mut fed) = (Vec::new(), Vec::new());
        self.fed_into(&mut part, &mut fed);
        part
    }

    fn fed_into(&self, part: &mut Vec<u32>, fed: &mut Vec<u32>) {
        part.clear();
        part.extend(self.links().flat_map(|e| [e.src.0, e.sink.0]));
        part.sort_unstable();
        part.dedup();
        loop {
            fed.clear();
            for e in self.links().filter(|e| has(part, e.src)) {
                fed.push(e.sink.0);
                if self.rows[e.slots[0] as usize].desc.inputs == InputPolicy::None {
                    fed.push(e.src.0);
                }
            }
            fed.sort_unstable();
            fed.dedup();
            let had = part.len();
            part.retain(|el| fed.binary_search(el).is_ok());
            if part.len() == had {
                break;
            }
        }
    }

    fn live(&self) -> Vec<&Edge> {
        let part = self.part();
        self.links().filter(|e| has(&part, e.src)).collect()
    }

    // Topological sort, once per run at setup.
    #[allow(clippy::disallowed_methods)]
    fn topo_order(&self) -> Result<Vec<ElementId>, Error> {
        self.topo_order_of(&self.part())
    }

    // Topological sort, once per run at setup and per change.
    #[allow(clippy::disallowed_methods)]
    fn topo_order_of(&self, part: &[u32]) -> Result<Vec<ElementId>, Error> {
        let n = part.len();
        if n == 0 {
            return Err(Error::Graph(GraphError::Empty));
        }
        // Kahn's algorithm, by position in `part`.
        let mut indeg = vec![0usize; n];
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for e in self.links() {
            let (Some(u), Some(v)) = (at(part, e.src), at(part, e.sink)) else {
                continue;
            };
            indeg[v] += 1;
            adj[u].push(v);
        }
        let mut ready: Vec<usize> = (0..n).filter(|k| indeg[*k] == 0).collect();
        if ready.is_empty() {
            return Err(Error::Graph(GraphError::NoSource));
        }
        let mut order = Vec::with_capacity(n);
        while let Some(u) = ready.pop() {
            order.push(ElementId(part[u]));
            for &v in &adj[u] {
                indeg[v] -= 1;
                if indeg[v] == 0 {
                    ready.push(v);
                }
            }
        }
        if order.len() != n {
            let elements = (0..n).filter(|k| indeg[*k] != 0).map(|k| ElementId(part[k])).collect();
            return Err(Error::Graph(GraphError::Cycle { elements }));
        }
        Ok(order)
    }

    // Group partitioning, once per run at setup.
    #[allow(clippy::disallowed_methods)]
    fn compute_groups(&self, order: &[ElementId]) -> Result<Vec<Vec<ElementId>>, Error> {
        let n = self.slots.len();
        let mut incoming: Vec<Vec<ElementId>> = vec![Vec::new(); n];
        for e in self.live() {
            incoming[e.slots[1] as usize].push(e.src);
        }
        let mut group_of: Vec<Option<usize>> = vec![None; n];
        let mut groups: Vec<Vec<ElementId>> = Vec::new();
        for &id in order {
            let i = self.ix(id);
            let sched = self.elements[i]
                .as_ref()
                .ok_or(Error::Graph(GraphError::ElementConsumed { element: id }))?
                .desc()
                .sched;
            let is_source = incoming[i].is_empty();
            if is_source || matches!(sched, SchedHint::Active) {
                group_of[i] = Some(groups.len());
                groups.push(vec![id]);
            } else {
                if incoming[i].len() != 1 {
                    return Err(Error::Graph(GraphError::PassiveMultipleInputs {
                        element: id,
                        inputs: incoming[i].len(),
                    }));
                }
                let up = incoming[i][0];
                let g = group_of[self.ix(up)].ok_or(Error::Graph(
                    GraphError::PassiveBeforeUpstream { element: id, upstream: up },
                ))?;
                if *groups[g].last().unwrap() != up {
                    return Err(Error::Graph(GraphError::PassiveFanOut {
                        element: id,
                        upstream: up,
                    }));
                }
                groups[g].push(id);
                group_of[i] = Some(g);
            }
        }
        Ok(groups)
    }

    /// Set an element property, validated against its declared constraint.
    pub fn set(&mut self, el: ElementId, prop: &str, v: Value) -> Result<(), Error> {
        let row = self.row_of(el).ok_or(Error::Prop(PropError::UnknownElement { element: el }))?;
        validate_and_set(el, row.desc, &row.props, prop, v, false)
    }

    pub fn set_str(&mut self, el: ElementId, prop: &str, s: &str) -> Result<(), Error> {
        let id = ValueId(self.values.intern(s));
        self.set(el, prop, Value::Id(id))
    }

    pub fn prop_value(&self, el: ElementId, prop: &str) -> Option<Value> {
        self.prop_at(self.slot(el)?, prop)
    }

    fn prop_at(&self, s: usize, prop: &str) -> Option<Value> {
        let row = &self.rows[s];
        row.props.get(row.desc.props.iter().position(|p| p.name == prop)?)
    }

    pub fn value_name(&self, id: ValueId) -> Option<&str> {
        self.values.resolve(id.0)
    }

    /// A cloneable handle that sets live properties from another thread.
    pub fn prop_handle(&self) -> PropHandle {
        PropHandle::new(self.view.reader(), Arc::clone(&self.control))
    }

    /// A cloneable handle that reads element counters and running time from any thread.
    pub fn tap_handle(&self) -> TapHandle {
        TapHandle::new(self.view.reader(), Arc::clone(&self.control))
    }

    pub fn index(&self, el: ElementId) -> Option<Arc<Index>> {
        self.row_of(el)?.counters.index.get()
    }

    /// Publish `index` as the map of `el`, for a controller that read it where `el` cannot.
    pub fn publish_index(&self, el: ElementId, index: Index) {
        if let Some(row) = self.row_of(el) {
            row.counters.index.publish(index);
            row.counters.set_indexing(false);
            self.control.resolve_pending();
        }
    }

    /// Say that `el` will publish an index, so an early seek is held and not refused.
    pub fn will_index(&self, el: ElementId) {
        if let Some(row) = self.row_of(el) {
            row.counters.set_indexing(true);
        }
    }

    /// State the presentation duration for `el` where `el` does not know it.
    pub fn state_duration(&self, el: ElementId, ns: u64) {
        if let Some(row) = self.row_of(el) {
            row.counters.set_duration(ns);
        }
    }

    pub fn serves(&self, el: ElementId) -> Option<Serves> {
        self.row_of(el)?.counters.serves()
    }

    #[cfg(test)]
    pub(crate) fn counters_of(&self, el: ElementId) -> Arc<crate::counters::ElementCounters> {
        Arc::clone(&self.rows[self.ix(el)].counters)
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// Say on the bus why `element` plays no part.
    pub fn warn(&self, element: ElementId, error: Error) {
        self.bus_sender.send(BusMessage::Warning { element, error });
    }

    /// Mark `el` as added by an inserter fed by `link`, said on the bus at the commit in a run.
    pub fn mark_auto(&mut self, el: ElementId, link: LinkId) {
        let Some(t) = self.tune_of(el) else { return };
        t.auto = true;
        match &mut self.live_run {
            Some(run) => run.journal.push(Undo::Auto(el, link)),
            None => self.bus_sender.send(BusMessage::Inserted { element: el, link }),
        }
    }

    pub fn is_auto(&self, el: ElementId) -> bool {
        self.slot(el).is_some_and(|s| self.tune[s].auto)
    }

    /// Render the topology as Graphviz `dot`, clustered by thread group.
    // Diagnostic dump, called before run.
    #[allow(clippy::disallowed_methods)]
    pub fn dump_dot(&self) -> String {
        let mut s = String::from("digraph profluens {\n  rankdir=LR;\n  node [shape=box];\n");
        for &(i, k) in self.slots.map() {
            let name = self.elements[k as usize].as_ref().map_or("?", |el| el.desc().name);
            let probed = self.probed_label(ElementId(i));
            let auto = if self.is_auto(ElementId(i)) { " (auto)" } else { "" };
            s.push_str(&format!("  e{i} [label=\"{name}{auto}{probed}\"];\n"));
        }
        if let Ok(order) = self.topo_order()
            && let Ok(groups) = self.compute_groups(&order)
        {
            for (gi, ids) in groups.iter().enumerate() {
                s.push_str(&format!(
                        "  subgraph cluster_{gi} {{\n    label=\"group {gi} (thread)\";\n    style=dashed;\n"
                    ));
                for id in ids {
                    s.push_str(&format!("    e{};\n", id.0));
                }
                s.push_str("  }\n");
            }
        }
        for e in self.links() {
            let fam = self.format_display(&e.format);
            s.push_str(&format!("  e{} -> e{} [label=\"{fam}\"]", e.src.0, e.sink.0));
            let mut note: Vec<String> = e.filter.iter().map(|f| format!("[{}]", f.line)).collect();
            match e.solved {
                false => note.push("provisional".into()),
                true => note.extend(e.format.fields().iter().zip(e.why).map(|((f, _), why)| {
                    format!("{}: {}", self.fields.resolve(f.0).unwrap_or("?"), why.name())
                })),
            }
            let unsettled: Vec<&str> = e
                .format
                .fields()
                .iter()
                .enumerate()
                .filter(|(i, _)| e.risky >> i & 1 == 1)
                .map(|(_, (f, _))| self.fields.resolve(f.0).unwrap_or("?"))
                .collect();
            if !unsettled.is_empty() {
                note.push(format!("unsettled: {}", unsettled.join(", ")));
            }
            s.push_str(&format!(" [xlabel=\"{}\"];\n", note.join("\\n")));
        }
        s.push_str("}\n");
        s
    }

    // Diagnostic dump, called before run.
    #[allow(clippy::disallowed_methods)]
    fn probed_label(&self, el: ElementId) -> String {
        let i = self.ix(el);
        let (desc, dynp) = (self.rows[i].desc, &self.dyn_pads[i]);
        let mut out = String::new();
        for (src, statics, added) in
            [(true, desc.srcs, &dynp.srcs), (false, desc.sinks, &dynp.sinks)]
        {
            for pad in 0..(statics.len() + added.len()) as u32 {
                let Some((name, menu)) = pad_at(statics, added, pad) else {
                    continue;
                };
                if OfferDesc::is_probed(menu) {
                    let own = self.offers[i].iter().find(|o| o.key == (src, pad));
                    let own = own.map(|o| o.lines.join(" | "));
                    out.push_str(&format!("\\n{name}: {}", own.as_deref().unwrap_or("pending")));
                }
            }
        }
        out
    }

    // Diagnostic, computed on demand.
    #[allow(clippy::disallowed_methods)]
    pub fn latency_report(&self) -> LatencyReport {
        let (part, lat, from) = self.compute_in_latency();
        let n = self.rows.len();
        let (mut in_lat, mut pred) = (vec![Timestamp::ZERO; n], vec![None; n]);
        for (k, el) in part.iter().enumerate() {
            let i = self.ix(ElementId(*el));
            (in_lat[i], pred[i]) = (lat[k], from[k].map(|j| self.ix(ElementId(part[j]))));
        }
        let mut has_out = vec![false; n];
        for e in self.links() {
            has_out[e.slots[0] as usize] = true;
        }
        let mut paths = Vec::new();
        for &(_, i) in self.slots.map() {
            let i = i as usize;
            if has_out[i] {
                continue;
            }
            let mut chain = Vec::new();
            let mut cur = Some(i);
            let (mut is_live, mut residency) = (false, Timestamp::ZERO);
            while let Some(c) = cur {
                let lat = &self.rows[c].desc.latency;
                is_live |= lat.is_live;
                chain.push((self.id_of(c as u32), lat.min));
                cur = pred[c];
                if let Some(p) = cur {
                    residency = residency.saturating_add(self.residency(p, c));
                }
            }
            chain.reverse();
            paths.push(crate::counters::PathLatency {
                sink: self.id_of(i as u32),
                total: in_lat[i],
                per_element: chain,
                is_live,
                residency,
            });
        }
        paths.sort_by_key(|p| std::cmp::Reverse(p.total));
        LatencyReport { paths }
    }

    fn residency(&self, from: usize, to: usize) -> Timestamp {
        let limit = |s: u32| {
            self.tune.get(s as usize).and_then(|t| t.limit).map_or(Timestamp::NONE, |l| l.time)
        };
        let timed = |e: &Edge| {
            e.slots[0] as usize == from
                && limit(e.slots[1]).is_some()
                && !self.sparse(from, e.src_pad)
        };
        let Some(edge) = self.links().find(|e| e.slots == [from as u32, to as u32]) else {
            return Timestamp::ZERO;
        };
        match timed(edge) {
            false => Timestamp::ZERO,
            true if self.links().filter(|e| timed(e)).count() >= 2 => {
                limit(edge.slots[1]).max(crate::weave::MAX_INTERLEAVE)
            }
            true => limit(edge.slots[1]),
        }
    }

    // Once per run and per change.
    #[allow(clippy::disallowed_methods)]
    fn compute_in_latency(&self) -> (Vec<u32>, Vec<Timestamp>, Vec<Option<usize>>) {
        let part = self.part();
        let n = part.len();
        let mut in_lat = vec![Timestamp::ZERO; n];
        let mut pred: Vec<Option<usize>> = vec![None; n];
        let mut outs: Vec<(usize, usize)> =
            self.links().filter_map(|e| Some((at(&part, e.src)?, at(&part, e.sink)?))).collect();
        outs.sort_unstable();
        if let Ok(order) = self.topo_order_of(&part) {
            for id in order {
                let Some(u) = at(&part, id) else { continue };
                let via = in_lat[u].saturating_add(self.rows[self.ix(id)].desc.latency.min);
                let from = outs.partition_point(|o| o.0 < u);
                for &(_, v) in outs[from..].iter().take_while(|o| o.0 == u) {
                    if pred[v].is_none() || via > in_lat[v] {
                        in_lat[v] = via;
                        pred[v] = Some(u);
                    }
                }
            }
        }
        (part, in_lat, pred)
    }

    pub fn counters(&self, el: ElementId) -> CounterSnapshot {
        self.row_of(el).map(|row| row.counters.snapshot()).unwrap_or_default()
    }
}

impl Pipeline {
    fn sparse(&self, s: usize, pad: SrcPad) -> bool {
        let stream = self.stream_pads.get(s).and_then(|pads| pads.iter().find(|(_, p)| *p == pad));
        stream.and_then(|(n, _)| self.streams[s].get(*n as usize)).is_some_and(|st| st.sparse)
    }

    fn ring(
        &self,
        edge: &Edge,
        link: LinkId,
        from: &Arc<Station>,
        to: &Arc<Station>,
    ) -> (DownLink, UpLink) {
        let tune = self.tune.get(edge.slots[1] as usize).copied().unwrap_or_default();
        let cap = tune.queue.unwrap_or(self.queue_cap);
        let leaky = tune.leaky.unwrap_or(crate::ring::Leaky::Block);
        let (data_tx, data_rx) = crate::ring::spsc_leaky::<Batch>(cap, leaky);
        let shells_for_ring_and_last_burst = 2 * cap + 2;
        let (shell_tx, shell_rx) = spsc::<Batch>(shells_for_ring_and_last_burst);
        let room = Arc::new(Room::new());
        let (peer, staged) = (Arc::clone(to), None);
        let level: Arc<Gauge> = Arc::default();
        level.set_limit(tune.limit.unwrap_or(Limit::NONE));
        let pad = edge.src_pad;
        let room_up = Arc::clone(&room);
        let limited = level.limit() != Limit::NONE;
        if let Some((low, high)) = tune.marks {
            level.set_marks(low, high);
        }
        let mark = level.marked().then_some((edge.sink, edge.sink_pad));
        let slot: Arc<crate::feedback::Slot> = Arc::default();
        let (level_up, slot_up) = (Arc::clone(&level), Arc::clone(&slot));
        let (base, sparse) = (level.limit().time, self.sparse(edge.slots[0] as usize, pad));
        let down = DownLink {
            pad,
            link,
            data: data_tx,
            shells: shell_rx,
            leaky,
            peer,
            room,
            staged,
            level,
            limited,
            base,
            sparse,
            mark,
            slot,
            seen: 0,
        };
        (
            down,
            UpLink::new(
                edge.sink_pad,
                data_rx,
                shell_tx,
                Arc::clone(from),
                room_up,
                level_up,
                slot_up,
            ),
        )
    }

    /// Put the edits made since the last commit into the run.
    pub fn commit(&mut self) -> Result<u32, Error> {
        self.commit_at(Timestamp::NONE, &[])
    }

    /// [`commit`](Self::commit), presenting from `to_time`, with where elements read.
    pub fn commit_at(&mut self, to_time: Timestamp, targets: &[Target]) -> Result<u32, Error> {
        self.settle();
        let planned = match &self.live_run {
            None => return Err(Error::Topology(TopologyError::NotRunning)),
            Some(_) if self.control.is_stopped() => Err(Error::Topology(TopologyError::NotRunning)),
            Some(run) if run.flight.is_some() => Err(Error::Topology(TopologyError::Busy)),
            Some(_) => self.plan(to_time, targets),
        };
        if planned.is_err() {
            self.abandon();
        }
        planned
    }

    /// Whether a committed change has not spliced yet, so a commit would be refused.
    pub fn changing(&mut self) -> bool {
        self.settle();
        self.live_run.as_ref().is_some_and(|run| run.flight.is_some())
    }

    /// Take back the edits made since the last commit.
    pub fn abandon(&mut self) {
        if let Some(run) = self.live_run.as_mut() {
            run.leaving.clear();
            let journal = std::mem::take(&mut run.journal);
            self.revert(journal);
        }
    }

    /// Take back the committed change that has not spliced, returning its number.
    pub fn cancel(&mut self) -> Option<u32> {
        let change = self.control.topology.cancel()?;
        self.control.topology.advance(&self.control, &self.bus_sender);
        self.settle();
        Some(change)
    }

    /// The last change that failed before its splice, as of the last settle.
    pub fn failed_change(&self) -> Option<u32> {
        self.failed_change
    }

    /// Bring the books up to date with how the last change ended and the groups that left.
    pub fn settle(&mut self) {
        self.settle_change();
        self.reap(false);
    }

    fn settle_change(&mut self) {
        let Some(run) = self.live_run.as_mut() else {
            return;
        };
        let Some(change) = run.flight.as_ref().map(|f| f.change) else {
            return;
        };
        let Some(how) = self.control.topology.take_ended(change) else {
            return;
        };
        let Some(done) = run.flight.take() else {
            return;
        };
        let Pending { journal, groups, leaving, latency, pads, leaky, .. } = done;
        let mut gone: Vec<ElementId> = leaving.iter().map(|(el, _)| *el).collect();
        let mut ran = groups;
        if how.is_err() {
            self.failed_change = Some(change);
            std::mem::swap(&mut run.groups, &mut ran);
            (run.latency, run.pads) = (latency, pads);
            for (el, was) in leaky {
                if let Some(t) = self.tune_of(ElementId(el)) {
                    t.leaky = was;
                }
            }
            let made = |u: &Undo| match u {
                Undo::Forget(el) => Some(*el),
                _ => None,
            };
            gone = journal.iter().filter_map(made).collect();
            self.revert(journal);
        }
        for el in gone {
            self.forget(el);
            let Some(run) = self.live_run.as_mut() else {
                continue;
            };
            let Some(at) = run.own.iter().position(|(e, _)| *e == el) else {
                continue;
            };
            let pool = run.own.swap_remove(at).1;
            // A group that drains still allocates, so its pools go once it is out.
            match ran.iter().find(|(ids, _)| ids.contains(&el)) {
                Some((ids, _)) => run.parting.push((ids[0], el, pool)),
                None => pool.retire(),
            }
        }
    }

    fn reap(&mut self, all: bool) {
        if let Some(run) = self.live_run.as_mut().filter(|run| !run.parting.is_empty()) {
            let parting = &mut run.parting;
            self.control.topology.gone_with(|gone| {
                parting.retain(|(head, _, pool)| {
                    let out = all || gone.contains(head);
                    if out {
                        pool.retire();
                    }
                    !out
                });
            });
        }
        if !self.fading.is_empty() && self.fold_pools() {
            self.publish();
        }
    }

    fn revert(&mut self, journal: Vec<Undo>) {
        for undo in journal.into_iter().rev() {
            match undo {
                Undo::Unlink(link) => self.put_edge(link, None),
                Undo::Restore(link, edge) => self.put_edge(link, Some(edge)),
                Undo::Forget(el) => self.forget(el),
                Undo::Auto(el, _) => {
                    if let Some(t) = self.tune_of(el) {
                        t.auto = false;
                    }
                }
            }
        }
        self.publish();
    }

    // Once per change, off the media path.
    #[allow(clippy::disallowed_methods)]
    fn plan(&mut self, to_time: Timestamp, targets: &[Target]) -> Result<u32, Error> {
        let refuse = |e: TopologyError| Err(Error::Topology(e));
        let Some(run) = self.live_run.as_ref() else {
            return refuse(TopologyError::NotRunning);
        };
        let (groups, leaving) = (run.groups.clone(), run.leaving.clone());
        let cut = |u: &Undo| match u {
            Undo::Restore(link, edge) if edge.live => Some((*link, edge.clone())),
            _ => None,
        };
        let cuts: Vec<(LinkId, Edge)> = run.journal.iter().filter_map(cut).collect();
        let (clock, wall) = (Arc::clone(&run.clock), run.wall);
        let (factory, told_latency) = (Arc::clone(&run.factory), run.latency.clone());
        let mut told_pads = run.pads.clone();
        let n = self.element_count();
        let mut members: Vec<(u32, usize)> = Vec::new();
        for (g, (ids, _)) in groups.iter().enumerate() {
            members.extend(ids.iter().map(|id| (id.0, g)));
        }
        members.sort_unstable();
        let group_of = |el: ElementId| find(&members, el);
        let how = |el: &ElementId| leaving.iter().find(|(e, _)| e == el).map(|(_, how)| *how);
        let mut leaves: Vec<Option<Leave>> = vec![None; groups.len()];
        for (g, (ids, _)) in groups.iter().enumerate() {
            leaves[g] = ids.iter().find_map(how);
            let stays = ids.iter().find(|id| how(id).is_none());
            let removed = ids.iter().copied().find(|id| how(id).is_some());
            if let (Some(removed), Some(element)) = (removed, stays) {
                return refuse(TopologyError::PartialGroup { element: *element, removed });
            }
        }
        let unspliced = |(i, e): (&u32, &Edge)| (!e.live).then(|| (*i as usize, e.clone()));
        let mut fresh: Vec<(usize, Edge)> =
            self.edge_ids.iter().zip(&self.edges).filter_map(unspliced).collect();
        let stays = |el: ElementId| group_of(el).filter(|g| leaves[*g].is_none());
        let mut added: Vec<ElementId> = Vec::new();
        for el in fresh.iter().flat_map(|(_, e)| [e.src, e.sink]) {
            let idle = self.elements[self.ix(el)].is_some();
            match stays(el) {
                Some(_) => {}
                None if idle && !added.contains(&el) => added.push(el),
                None if idle => {}
                None => return refuse(TopologyError::Gone { element: el }),
            }
        }
        self.need_reactor(&added)?;
        for el in &added {
            let desc = self.rows[self.ix(*el)].desc;
            if desc.inputs != InputPolicy::None && fresh.iter().all(|(_, e)| e.sink != *el) {
                let starved = GraphError::Starved { element: *el, element_name: desc.name };
                return Err(Error::Graph(starved));
            }
        }
        let mut new_groups: Vec<Vec<ElementId>> = Vec::new();
        let mut new_of: Vec<(u32, usize)> = Vec::new();
        let mut left = added.clone();
        while !left.is_empty() {
            let placed =
                |el: &ElementId| fresh.iter().all(|(_, e)| e.sink != *el || !left.contains(&e.src));
            let Some(at) = left.iter().position(placed) else {
                return Err(Error::Graph(GraphError::Cycle { elements: left.into() }));
            };
            let el = left.remove(at);
            let into = |(_, e): &(usize, Edge)| (e.sink == el).then_some(e.src);
            let ups: Vec<ElementId> = fresh.iter().filter_map(into).collect();
            let passive = self.rows[self.ix(el)].desc.sched == SchedHint::Passive;
            if passive && ups.len() > 1 {
                let inputs = ups.len();
                return Err(Error::Graph(GraphError::PassiveMultipleInputs {
                    element: el,
                    inputs,
                }));
            }
            let behind = ups.first().filter(|_| passive).and_then(|up| find(&new_of, *up));
            let g = match behind.filter(|g| new_groups[*g].last() == ups.first()) {
                Some(g) => g,
                None => {
                    new_groups.push(Vec::new());
                    new_groups.len() - 1
                }
            };
            new_groups[g].push(el);
            put(&mut new_of, el, g);
        }
        let place = |el: ElementId| match (stays(el), find(&new_of, el)) {
            (Some(g), _) => Some(Place::Run(g)),
            (None, Some(g)) => Some(Place::New(g)),
            _ => None,
        };
        let ids_of = |at: Place| match at {
            Place::Run(g) => &groups[g].0,
            Place::New(g) => &new_groups[g],
        };
        // A group sealed by a failure is in the books and out of the run, and takes no part.
        let topology = &self.control.topology;
        let sealed = |at: Option<Place>| match at {
            Some(Place::Run(g)) => topology.gone_with(|gone| gone.contains(&groups[g].0[0])),
            _ => false,
        };
        for (_, e) in &fresh {
            if let Some(element) = [e.src, e.sink].into_iter().find(|el| sealed(place(*el))) {
                return refuse(TopologyError::Gone { element });
            }
            let (Some(from), Some(to)) = (place(e.src), place(e.sink)) else {
                continue;
            };
            match (from == to, from) {
                (true, Place::Run(_)) => {
                    return refuse(TopologyError::Inline { src: e.src, sink: e.sink });
                }
                (true, Place::New(_)) => continue,
                _ => {}
            }
            if ids_of(from).last() != Some(&e.src) {
                return Err(Error::Graph(GraphError::NonTailBranch { src: e.src, sink: e.sink }));
            }
            if ids_of(to).first() != Some(&e.sink) {
                return refuse(TopologyError::NotAHead { sink: e.sink });
            }
            let (element, pad) = (e.sink, e.sink_pad.0);
            if self.links().filter(|o| (o.sink, o.sink_pad.0) == (element, pad)).count() > 1 {
                return refuse(TopologyError::PadLinked { element, src: false, pad });
            }
        }
        for (_, e) in &cuts {
            let (a, b) = (group_of(e.src), group_of(e.sink));
            if a == b && a.is_some_and(|g| leaves[g].is_none()) {
                return refuse(TopologyError::Inline { src: e.src, sink: e.sink });
            }
        }
        for (el, ..) in targets {
            match (el.0 as usize) < n {
                true => match place(*el) {
                    at if sealed(at) => return refuse(TopologyError::Gone { element: *el }),
                    Some(Place::Run(g)) => {
                        let gains = fresh.iter().any(|(_, e)| e.src == *el);
                        if groups[g].0.last() == Some(el) && !gains {
                            return refuse(TopologyError::RestartAtRing { element: *el });
                        }
                    }
                    Some(Place::New(_)) => {}
                    None => return refuse(TopologyError::Gone { element: *el }),
                },
                false => return refuse(TopologyError::Gone { element: *el }),
            }
        }
        self.solve_fresh(true)?;
        for (i, e) in fresh.iter_mut() {
            if let Some(solved) = self.edge(LinkId(*i as u32)) {
                (e.format, e.why, e.solved) = (solved.format.clone(), solved.why, solved.solved);
            }
        }

        let stations: Vec<Arc<Station>> =
            new_groups.iter().map(|_| Arc::new(Station::new())).collect();
        let station = |at: Place| match at {
            Place::Run(g) => &groups[g].1,
            Place::New(g) => &stations[g],
        };
        let head = |g: usize| groups[g].0[0];
        let (mut attach, mut splice): (Vec<Part>, Vec<Part>) = (Vec::new(), Vec::new());
        let mut down: Vec<Vec<DownLink>> = new_groups.iter().map(|_| Vec::new()).collect();
        let mut up: Vec<Vec<UpLink>> = new_groups.iter().map(|_| Vec::new()).collect();
        let (mut wait, mut prerolls) = (Vec::new(), true);
        let mut pad_counts: Vec<(u32, (usize, usize))> = Vec::new();
        for el in fresh.iter().flat_map(|(_, e)| [e.src, e.sink]).filter(|el| stays(*el).is_some())
        {
            put(&mut pad_counts, el, self.pad_count(el));
        }
        let (runs, mut latency, _) = self.compute_in_latency();
        let worst = |of: &mut dyn Iterator<Item = Timestamp>| of.max().unwrap_or(Timestamp::ZERO);
        let told = |el: ElementId| find(&told_latency, el).unwrap_or(Timestamp::ZERO);
        let stayed = |el: ElementId| group_of(el).is_some_and(|g| leaves[g].is_none());
        let raised = runs
            .iter()
            .zip(&latency)
            .any(|(el, l)| stayed(ElementId(*el)) && *l > told(ElementId(*el)));
        let mut held = Vec::new();
        if raised && self.latency_policy == LatencyPolicy::Hold {
            let asked = worst(&mut latency.iter().copied());
            for (el, l) in
                runs.iter().zip(latency.iter_mut()).filter(|(el, _)| stayed(ElementId(**el)))
            {
                *l = (*l).min(told(ElementId(*el)));
            }
            for (_, e) in fresh.iter().filter(|(_, e)| stayed(e.sink)) {
                let i = e.slots[1] as usize;
                if !held.iter().any(|(el, _)| *el == e.sink.0) {
                    held.push((e.sink.0, self.tune[i].leaky));
                }
                self.tune[i].leaky = Some(crate::ring::Leaky::DropNewest);
            }
            let kept = worst(&mut told_latency.iter().map(|t| t.1));
            self.bus_sender.send(BusMessage::LatencyHeld { kept, asked });
        }
        let lat = |el: ElementId| at(&runs, el).map_or(Timestamp::ZERO, |k| latency[k]);
        let mut grown = |el: ElementId| {
            let now = find(&pad_counts, el).unwrap_or((0, 0));
            (find(&told_pads, el).unwrap_or((0, 0)) != now).then(|| {
                put(&mut told_pads, el, now);
                PadEdit::Pads(now.0, now.1)
            })
        };
        for (i, e) in &fresh {
            let (Some(from), Some(to)) = (place(e.src), place(e.sink)) else {
                continue;
            };
            if from == to {
                continue;
            }
            let (d, u) = self.ring(e, LinkId(*i as u32), station(from), station(to));
            note_level((&mut self.rows, &mut self.dirty.0), e, &d.level);
            match from {
                Place::New(g) => down[g].push(d),
                Place::Run(g) => {
                    let recut = cuts.iter().any(|(_, c)| (c.src, c.src_pad) == (e.src, e.src_pad));
                    let early = matches!(to, Place::New(_)) && !recut;
                    prerolls &= early || matches!(to, Place::Run(_));
                    let p = part(if early { &mut attach } else { &mut splice }, head(g));
                    let i = e.slots[0] as usize;
                    p.edits.extend(grown(e.src).map(|edit| (e.src, edit)));
                    let stream = self.stream_pads[i].iter().find(|(_, pad)| *pad == e.src_pad);
                    p.edits.extend(stream.map(|(s, pad)| (e.src, PadEdit::StreamPad(*s, *pad))));
                    p.edits.push((e.src, PadEdit::SrcFormat(e.src_pad, e.format.clone())));
                    p.down_add.push(d);
                }
            }
            match to {
                Place::New(g) => up[g].push(u),
                Place::Run(g) => {
                    let p = part(&mut splice, head(g));
                    let i = e.slots[1] as usize;
                    p.edits.extend(grown(e.sink).map(|edit| (e.sink, edit)));
                    p.edits.push((e.sink, PadEdit::SinkFormat(e.sink_pad, e.format.clone())));
                    let own = self.offers[i].iter().find(|o| o.key == (false, e.sink_pad.0));
                    p.edits.extend(own.map(|own| (e.sink, PadEdit::SinkOffers(own.clone()))));
                    if matches!(from, Place::New(_)) {
                        wait.push((head(g), e.sink_pad));
                    }
                    p.up_add.push(u);
                }
            }
        }
        if !prerolls {
            wait.clear();
        }
        for (g, (ids, _)) in groups.iter().enumerate() {
            for el in ids.iter().filter(|el| targets.iter().any(|(t, ..)| t == *el)) {
                let early = attach.iter().any(|p| p.head == head(g));
                let step = if early { &mut attach } else { &mut splice };
                part(step, head(g)).edits.push((*el, PadEdit::Restart));
            }
        }
        for (link, e) in &cuts {
            let (Some(a), Some(b)) = (group_of(e.src), group_of(e.sink)) else {
                continue;
            };
            let into = |(_, f): &(usize, Edge)| (f.sink, f.sink_pad) == (e.sink, e.sink_pad);
            if leaves[a] == Some(Leave::Drain) && leaves[b].is_none() && fresh.iter().any(into) {
                wait.retain(|w| *w != (head(b), e.sink_pad));
                part(&mut splice, head(b)).handover.push(e.sink_pad);
            } else if leaves[a].is_none() || leaves[b].is_none() {
                part(&mut splice, head(a)).down_cut.push(*link);
                part(&mut splice, head(b)).up_cut.push(e.sink_pad);
            }
        }
        for (g, how) in leaves.iter().enumerate() {
            if how.is_some() {
                part(&mut splice, head(g)).leave = *how;
            }
        }
        for (g, (ids, _)) in groups.iter().enumerate().filter(|(g, _)| leaves[*g].is_none()) {
            for el in ids.iter().filter(|el| told(**el) != lat(**el)) {
                part(&mut splice, head(g)).edits.push((*el, PadEdit::Latency(lat(*el))));
            }
        }
        let (old, new) =
            (worst(&mut told_latency.iter().map(|t| t.1)), worst(&mut latency.iter().copied()));
        if old != new {
            self.bus_sender.send(BusMessage::LatencyChanged { old, new });
        }

        let terminal: Vec<bool> = down.iter().map(Vec::is_empty).collect();
        let makes = |el: &&ElementId| self.rows[self.ix(**el)].desc.inputs == InputPolicy::None;
        let sources: Vec<ElementId> = added.iter().filter(makes).copied().collect();
        let gone = |el: u32| self.slots.get(el).is_none();
        let sinks = (self.sinks(), n);
        let (cursors, seek_gen, paused, rate, position) =
            self.control.join(&stations, &terminal, sinks, targets, &sources, gone);
        attach.iter_mut().chain(splice.iter_mut()).for_each(|p| p.seek_gen = seek_gen);
        let to_time = if to_time.is_some() { to_time } else { position };
        let joining = Joining::new();
        let mut ids: Vec<u32> = added.iter().map(|el| el.0).collect();
        ids.sort_unstable();
        let all = self.pools_worst(&runs);
        let mut own = Vec::new();
        for el in ids.iter().map(|el| ElementId(*el)) {
            let pool = self.pool_of(el, all);
            pool.reserve_starved(groups.len() + new_groups.len());
            put(&mut self.pools, el, pool.clone());
            own.push((el, pool));
        }
        self.dirty.3 = true;
        self.fold_pools();
        self.publish();
        let Some(run) = self.live_run.as_ref() else {
            return refuse(TopologyError::NotRunning);
        };
        let stamps = run.wall.is_none().then_some(&clock);
        let (mut logs, drains) = self.logs_of(run.gate.as_ref(), stamps, &ids);
        let (mut lost, mut drains) = ((0, ElementId(0)), drains.into_iter());
        for (k, el) in ids.iter().enumerate() {
            let Some(drain) = logs[k].as_ref().and_then(|_| drains.next()) else {
                continue;
            };
            if run.drains.try_push(drain).is_err() {
                (logs[k], lost) = (None, (lost.0 + 1, ElementId(*el)));
            }
        }
        if lost.0 > 0 {
            let error = Error::Topology(TopologyError::LogsLost { count: lost.0 });
            self.bus_sender.send(BusMessage::Warning { element: lost.1, error });
        }
        let pools = own.iter().map(|(_, pool)| pool.clone()).collect();
        let mut tables = self.tables(ids, pools, logs);
        let shared = self.shared(&clock, wall);
        let mut setups = Vec::new();
        for (g, ids) in new_groups.iter().enumerate() {
            let join = Some(Join { to_time, paused, rate, joining: Arc::clone(&joining) });
            let (cursor, station) = (Arc::clone(&cursors[g]), Arc::clone(&stations[g]));
            let (downstream, upstream) = (std::mem::take(&mut down[g]), std::mem::take(&mut up[g]));
            let wiring = Wiring { cursor, seek_gen, station, downstream, upstream, join };
            setups.push(self.group_setup(ids.clone(), &mut tables, &shared, &factory, wiring));
        }
        let cut_out = |ids: &Vec<ElementId>| Part {
            leave: Some(Leave::Cut),
            undo: true,
            ..Part::new(ids[0], seek_gen)
        };
        let mut rollback: Vec<Part> = new_groups.iter().map(cut_out).collect();
        for p in &attach {
            part(&mut rollback, p.head).down_cut.extend(p.down_add.iter().map(|d| d.link));
        }
        let topology = &self.control.topology;
        let change = topology.number();
        let attach_id = (!attach.is_empty()).then(|| topology.number());
        let splice_id = topology.number();
        let mut steps = vec![(splice_id, splice)];
        steps.extend(attach_id.map(|id| (id, attach)));
        let new = new_groups.len() as u32;
        let cursors = cursors.clone();
        let flight = Flight {
            change,
            attach: attach_id,
            splice: splice_id,
            groups: new,
            cursors,
            joining,
            wait,
            rollback,
        };

        for (i, _) in &fresh {
            if let Some(k) = self.edge_at(LinkId(*i as u32)) {
                self.edges[k].live = true;
            }
        }
        let Some(run) = self.live_run.as_mut() else {
            return refuse(TopologyError::NotRunning);
        };
        let before = std::mem::take(&mut run.groups);
        let staying = before.iter().enumerate().filter(|(g, _)| leaves[*g].is_none());
        run.groups = staying.map(|(_, group)| group.clone()).collect();
        run.groups.extend(new_groups.into_iter().zip(stations));
        run.own.extend(own);
        let latency = std::mem::replace(&mut run.latency, runs.into_iter().zip(latency).collect());
        let joined = tables.ids.iter().copied().zip(tables.pad_counts);
        let mut now: Vec<(u32, (usize, usize))> =
            told_pads.into_iter().filter(|(el, _)| stayed(ElementId(*el))).chain(joined).collect();
        now.sort_unstable_by_key(|p| p.0);
        let pads = std::mem::replace(&mut run.pads, now);
        let parting = run.parting.iter().map(|(head, ..)| head);
        let mut heads: Vec<u32> = before
            .iter()
            .chain(&run.groups)
            .map(|(ids, _)| &ids[0])
            .chain(parting)
            .map(|h| h.0)
            .collect();
        heads.sort_unstable();
        let (journal, leaving) =
            (std::mem::take(&mut run.journal), std::mem::take(&mut run.leaving));
        for undo in &journal {
            if let Undo::Auto(element, link) = undo {
                self.bus_sender.send(BusMessage::Inserted { element: *element, link: *link });
            }
        }
        run.flight =
            Some(Pending { change, journal, groups: before, leaving, latency, pads, leaky: held });
        self.control.topology.stage(flight, steps, setups, &heads);
        self.control.topology.advance(&self.control, &self.bus_sender);
        Ok(change)
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

fn note_level((rows, dirty): (&mut [Row], &mut Vec<usize>), edge: &Edge, level: &Arc<Gauge>) {
    dirty.push(edge.slots[1] as usize / CHUNK);
    if let Some(row) = rows.get_mut(edge.slots[1] as usize) {
        row.levels.retain(|(pad, _)| *pad != edge.sink_pad);
        row.levels.push((edge.sink_pad, Arc::clone(level)));
    }
}

fn build_rank(debug: &DebugSpec) -> u8 {
    let on_pipeline = debug.targets.iter().find(|(n, _)| n == "pipeline").map(|(_, r)| *r);
    on_pipeline.or(debug.global).unwrap_or(0)
}

fn plog(writer: Option<&dyn LogWriter>, rank: u8, level: Level, msg: std::fmt::Arguments<'_>) {
    if level.rank() > rank {
        return;
    }
    if let Some(writer) = writer {
        writer.line(level, msg);
    } else if crate::log::stderr_colors_enabled() {
        eprintln!(
            "\x1b[2m[       build]\x1b[0m {}{:5}\x1b[0m \x1b[94mpipeline\x1b[0m {msg}",
            crate::log::level_sgr(level),
            level.name(),
        );
    } else {
        eprintln!("[       build] {:5} pipeline {msg}", level.name());
    }
}

fn has(ids: &[u32], el: ElementId) -> bool {
    ids.binary_search(&el.0).is_ok()
}

fn at(ids: &[u32], el: ElementId) -> Option<usize> {
    ids.binary_search(&el.0).ok()
}

fn find<T: Copy>(table: &[(u32, T)], el: ElementId) -> Option<T> {
    table.binary_search_by_key(&el.0, |x| x.0).ok().map(|k| table[k].1)
}

fn find_ref<T>(table: &[(u32, T)], el: ElementId) -> Option<&T> {
    table.binary_search_by_key(&el.0, |x| x.0).ok().map(|k| &table[k].1)
}

fn put<T>(table: &mut Vec<(u32, T)>, el: ElementId, value: T) {
    match table.binary_search_by_key(&el.0, |x| x.0) {
        Ok(k) => table[k].1 = value,
        Err(k) => table.insert(k, (el.0, value)),
    }
}

type Links = (Vec<Vec<DownLink>>, Vec<Vec<UpLink>>);

struct Plan {
    threaded: bool,
    clock: Arc<dyn Clock>,
    groups: Vec<Vec<ElementId>>,
    stations: Vec<Arc<Station>>,
    downstream: Vec<Vec<DownLink>>,
    upstream: Vec<Vec<UpLink>>,
}

struct Tables {
    ids: Vec<u32>,
    pad_counts: Vec<(usize, usize)>,
    negotiated: Vec<PadFormats>,
    path_latency: Vec<Timestamp>,
    linked_pads: Vec<Vec<bool>>,
    pools: Vec<Pool>,
    logs: Vec<Option<Log>>,
}

struct Wiring {
    cursor: Arc<Cursor>,
    seek_gen: u64,
    station: Arc<Station>,
    downstream: Vec<DownLink>,
    upstream: Vec<UpLink>,
    join: Option<Join>,
}

pub(crate) struct RunEnd {
    view: Roster,
    log_thread: Option<LogDrainThread>,
    drains: Option<Drains>,
    pub(crate) bus: BusSender,
    control: Arc<Control>,
    log_rank: u8,
    writer: Option<Arc<dyn LogWriter>>,
}

impl RunEnd {
    pub(crate) fn drain_logs(&mut self) {
        if let Some(drains) = &mut self.drains {
            drains.sweep();
        }
    }

    pub(crate) fn finish(mut self, first_err: Option<Error>) -> (RunReport, Result<(), Error>) {
        self.control.topology.over(&self.bus);
        self.drain_logs();
        if let Some(mut drain) = self.log_thread.take() {
            drain.join();
        }
        let mut report = RunReport::default();
        let s = self.view.read(View::pool_stats);
        (report.pool_slot_allocations, report.pool_high_water, report.buffers) =
            (s.slot_allocations, s.high_water, s.acquires);
        let sched = self.control.scheduler_stats();
        plog(
            self.writer.as_deref(),
            self.log_rank,
            Level::Info,
            format_args!(
                "scheduler parks={} tick_expiries={} late_wakes={} empty_wakes={} parked_ms={}",
                sched.parks,
                sched.tick_expiries,
                sched.late_wakes,
                sched.empty_wakes,
                sched.parked_ns / 1_000_000,
            ),
        );
        let result = match first_err {
            None => {
                self.bus.send(BusMessage::Eos);
                Ok(())
            }
            Some(e) => {
                let error = e.clone();
                self.bus.send(BusMessage::Error { element: ElementId(u32::MAX), error });
                Err(e)
            }
        };
        (report, result)
    }
}

/// Holds the group threads of a run. A drop while armed, the unwind of `run`, aborts the run
/// and joins them, so no group thread outlives its pipeline.
struct RunGuard {
    control: Arc<Control>,
    threads: Vec<(ElementId, JoinHandle<Result<(), Error>>)>,
    armed: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.control.abort();
        for (_, t) in self.threads.drain(..) {
            let _ = t.join();
        }
        self.control.topology.serve(false);
    }
}

/// Reports the thread's exit to the topology however the thread ends.
struct Exited<'a>(&'a Control);

impl Drop for Exited<'_> {
    fn drop(&mut self) {
        self.0.topology.exited();
    }
}

// One name per group thread, once per group.
#[allow(clippy::disallowed_methods)]
fn spawn_group(
    gi: usize,
    setup: GroupSetup,
) -> std::io::Result<JoinHandle<Result<(), Error>>> {
    let control = Arc::clone(&setup.shared.control);
    let tag = Tag::of(&setup);
    std::thread::Builder::new().name(format!("pf-group-{gi}")).spawn(move || {
        let _exited = Exited(&control);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_group(setup, &tag)));
        let (failed, verdict) = match r {
            Ok((result, verdict)) => (result.err(), verdict),
            Err(payload) => {
                let e = crate::sched::group_panicked(&tag, &payload);
                let verdict = judge(&tag, Some(&e));
                (Some(e), verdict)
            }
        };
        retire_group(&control, &tag, failed, verdict).map_or(Ok(()), Err)
    })
}

struct LogGate {
    rank: u8,
    shared: Arc<LevelFilter>,
}

const DRAINS_WAITING: usize = 64;

struct Drains {
    held: Vec<LogDrain>,
    joining: Consumer<LogDrain>,
    writer: Option<Arc<dyn LogWriter>>,
    styled: bool,
}

impl Drains {
    fn sweep(&mut self) -> bool {
        use std::io::Write;
        while let Some(drain) = self.joining.try_pop() {
            self.held.push(drain);
        }
        if self.held.is_empty() {
            return false;
        }
        let Drains { held, writer, styled, .. } = self;
        if let Some(writer) = writer {
            return drain_into(held, |rec| writer.record(rec));
        }
        // One write per sweep, not one per format piece.
        let mut lock = std::io::BufWriter::with_capacity(1 << 14, std::io::stderr().lock());
        let progressed = drain_into(held, |rec| {
            let _ = crate::log::format_record_styled(&mut lock, rec, *styled);
        });
        let _ = lock.flush();
        progressed
    }
}

fn drain_into(held: &mut Vec<LogDrain>, mut emit: impl FnMut(&LogRecord)) -> bool {
    let mut progressed = false;
    held.retain_mut(|d| {
        let closed = d.is_closed();
        progressed |= d.sweep(&mut emit);
        !closed
    });
    progressed
}

struct LogDrainThread {
    handle: Option<JoinHandle<()>>,
    over: Arc<AtomicBool>,
}

impl LogDrainThread {
    fn spawn(drains: Drains) -> std::io::Result<Self> {
        let over = Arc::new(AtomicBool::new(false));
        let told = Arc::clone(&over);
        let handle = std::thread::Builder::new()
            .name("pf-log-drain".into())
            .spawn(move || Self::run(drains, &told))?;
        Ok(Self { handle: Some(handle), over })
    }

    // The log drain thread of a threaded run, not an element.
    #[allow(clippy::disallowed_methods)]
    fn run(mut drains: Drains, over: &AtomicBool) {
        let (first, most) = (Duration::from_millis(2), Duration::from_millis(64));
        let mut idle = first;
        loop {
            // Read first, so what was logged before the run ended is swept after.
            let last = over.load(Ordering::Acquire);
            let progressed = drains.sweep();
            if last && !progressed {
                break;
            }
            match progressed {
                true => idle = first,
                false => {
                    std::thread::park_timeout(idle);
                    idle = (idle * 2).min(most);
                }
            }
        }
    }

    fn join(&mut self) {
        self.over.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

impl Drop for LogDrainThread {
    fn drop(&mut self) {
        self.over.store(true, Ordering::Release);
    }
}
