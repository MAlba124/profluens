use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::clock::Clock;
use crate::element::ElementDesc;
use crate::id::{ElementId, SinkPad, SrcPad};
use crate::level::{Gauge, Held, Limit, RangeSet, Ranges};
use crate::pipeline::{Addressed, Index, Serves};
use crate::props::PropTable;
use crate::publish::{Publisher, Reader, published};
use crate::time::Timestamp;

#[derive(Clone)]
pub(crate) struct Row {
    pub(crate) desc: &'static ElementDesc,
    pub(crate) props: Arc<PropTable>,
    pub(crate) counters: Arc<ElementCounters>,
    pub(crate) gone: bool,
    pub(crate) levels: Vec<(crate::id::SinkPad, Arc<Gauge>)>,
}

pub(crate) const CHUNK: usize = 64;

/// A per-element table published in chunks, so a change copies the chunk it touched and shares the
/// rest.
#[derive(Clone)]
pub(crate) struct Chunks<T> {
    len: usize,
    chunks: Arc<Vec<(u32, Arc<[T]>)>>,
}

impl<T> Chunks<T> {
    // Once per pipeline and view.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn empty() -> Self {
        Chunks { len: 0, chunks: Arc::new(Vec::new()) }
    }

    pub(crate) fn get(&self, i: usize) -> Option<&T> {
        let at = self.chunks.binary_search_by_key(&((i / CHUNK) as u32), |c| c.0).ok()?;
        self.chunks[at].1.get(i % CHUNK)
    }

    #[cfg(test)]
    pub(crate) fn kept(&self) -> usize {
        self.chunks.len()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.chunks.iter().flat_map(|c| c.1.iter())
    }
}

impl<T: Clone> Chunks<T> {
    /// `table` again, `dirty` chunks copied, those `keep` refuses dropped, the rest shared.
    // A chunk per change, as the whole table was before.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn update(
        &mut self,
        table: &[T],
        dirty: &mut Vec<usize>,
        keep: impl Fn(usize) -> bool,
    ) {
        if self.len != table.len() {
            let (lo, hi) = (self.len.min(table.len()), self.len.max(table.len()));
            dirty.extend(lo / CHUNK..hi.div_ceil(CHUNK));
        }
        if dirty.is_empty() {
            return;
        }
        dirty.sort_unstable();
        dirty.dedup();
        let chunks = Arc::make_mut(&mut self.chunks);
        for &c in dirty.iter() {
            let rows = table
                .get(c * CHUNK..table.len().min((c + 1) * CHUNK))
                .filter(|r| !r.is_empty() && keep(c));
            match (chunks.binary_search_by_key(&(c as u32), |c| c.0), rows) {
                (Ok(at), Some(rows)) => chunks[at].1 = Arc::from(rows),
                (Err(at), Some(rows)) => chunks.insert(at, (c as u32, Arc::from(rows))),
                (Ok(at), None) => drop(chunks.remove(at)),
                (Err(_), None) => {}
            }
        }
        self.len = table.len();
        dirty.clear();
    }
}

impl<T: Clone> From<Vec<T>> for Chunks<T> {
    // A table published whole once, the empty dirty list allocates nothing.
    #[allow(clippy::disallowed_methods)]
    fn from(table: Vec<T>) -> Self {
        let mut out = Chunks::empty();
        out.update(&table, &mut Vec::new(), |_| true);
        out
    }
}

pub(crate) fn add_stats(
    mut acc: crate::memory::PoolStats,
    s: crate::memory::PoolStats,
) -> crate::memory::PoolStats {
    acc.slot_allocations += s.slot_allocations;
    acc.acquires += s.acquires;
    acc.recycles += s.recycles;
    acc.high_water += s.high_water;
    acc.outstanding += s.outstanding;
    acc.max_slots = acc.max_slots.saturating_add(s.max_slots);
    acc.slot_size = acc.slot_size.max(s.slot_size);
    acc.bytes_out += s.bytes_out;
    acc.bytes_high += s.bytes_high;
    acc.bytes_budget = acc.bytes_budget.saturating_add(s.bytes_budget);
    acc.regions += s.regions;
    acc.bytes_mapped += s.bytes_mapped;
    acc
}

pub(crate) struct View {
    pub(crate) rows: Chunks<Row>,
    pub(crate) slots: Arc<Vec<(u32, u32)>>,
    pub(crate) count: usize,
    /// The run's pools by element id, sorted. One fully returned after its element left is in
    /// `retired`.
    pub(crate) pools: Arc<Vec<(u32, crate::memory::Pool)>>,
    pub(crate) retired: crate::memory::PoolStats,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) edges: Vec<(ElementId, SrcPad, ElementId, SinkPad)>,
    pub(crate) stream_pads: Chunks<Vec<(u32, SrcPad)>>,
}

impl View {
    // An empty view, empty chunk lists and no table.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn empty(clock: Arc<dyn Clock>) -> View {
        let (pools, retired) = (Arc::new(Vec::new()), crate::memory::PoolStats::default());
        let (slots, edges, stream_pads) = (Arc::new(Vec::new()), Vec::new(), Chunks::empty());
        View { rows: Chunks::empty(), slots, count: 0, pools, retired, clock, edges, stream_pads }
    }

    pub(crate) fn row(&self, el: ElementId) -> Option<&Row> {
        self.rows.get(self.slot(el)?).filter(|row| !row.gone)
    }

    pub(crate) fn slot(&self, el: ElementId) -> Option<usize> {
        let k = self.slots.binary_search_by_key(&el.0, |x| x.0).ok()?;
        Some(self.slots[k].1 as usize)
    }

    pub(crate) fn pool_stats(&self) -> crate::memory::PoolStats {
        self.pools.iter().map(|(_, p)| p.stats()).fold(self.retired, add_stats)
    }
}

pub(crate) type Roster = crate::publish::Reader<View>;

pub(crate) fn row<T>(roster: &Roster, el: ElementId, read: impl FnOnce(&Row) -> T) -> Option<T> {
    roster.read(|view| view.row(el).map(read))
}

/// Src pads with their own position in a [`CounterSnapshot`], later pads share the last.
pub const POSITION_PADS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CounterSnapshot {
    pub buffers_in: u64,
    pub buffers_out: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub batches_in: u64,
    pub batches_out: u64,
    pub queue_high_water: u32,
    pub drops: u64,
    pub pts_out: Option<Timestamp>,
    /// The newest pts per src pad, read with [`pad_pts`](Self::pad_pts).
    pub pad_pts_out: [Option<Timestamp>; POSITION_PADS],
}

impl CounterSnapshot {
    pub fn pad_pts(&self, pad: SrcPad) -> Option<Timestamp> {
        self.pad_pts_out[(pad.0 as usize).min(POSITION_PADS - 1)]
    }
}

/// A lock-free power-of-two latency histogram, bucket `i` counts `[2^i, 2^(i+1))` ns.
#[derive(Default)]
pub struct LatencyHistogram {
    buckets: [AtomicU64; Self::BUCKETS],
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl LatencyHistogram {
    pub const BUCKETS: usize = 32;

    pub fn record(&self, ns: u64) {
        let bucket = (63 - (ns | 1).leading_zeros() as usize).min(Self::BUCKETS - 1);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> LatencySnapshot {
        let mut buckets = [0u64; Self::BUCKETS];
        for (b, a) in buckets.iter_mut().zip(&self.buckets) {
            *b = a.load(Ordering::Relaxed);
        }
        LatencySnapshot {
            buckets,
            count: self.count.load(Ordering::Relaxed),
            sum_ns: self.sum_ns.load(Ordering::Relaxed),
            max_ns: self.max_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LatencySnapshot {
    pub buckets: [u64; LatencyHistogram::BUCKETS],
    pub count: u64,
    pub sum_ns: u64,
    pub max_ns: u64,
}

impl LatencySnapshot {
    pub fn mean_ns(&self) -> u64 {
        self.sum_ns.checked_div(self.count).unwrap_or(0)
    }

    pub fn quantile_ns(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((self.count as f64) * q.clamp(0.0, 1.0)).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &c) in self.buckets.iter().enumerate() {
            seen += c;
            if seen >= target {
                let geometric_midpoint = (1u128 << i) as f64 * std::f64::consts::SQRT_2;
                return (geometric_midpoint as u64).min(self.max_ns);
            }
        }
        self.max_ns
    }
}

/// Live per-element counters, updated by the scheduler once per batch.
#[derive(Default)]
pub struct ElementCounters {
    buffers_in: AtomicU64,
    buffers_out: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    batches_in: AtomicU64,
    batches_out: AtomicU64,
    queue_high_water: AtomicU32,
    drops: AtomicU64,
    pad_pts_plus_one: [AtomicU64; POSITION_PADS],
    pub process_ns: LatencyHistogram,
    pub queue_ns: LatencyHistogram,
    pub wait_lateness_ns: LatencyHistogram,
    pub store: Gauge,
    pub ranges: Ranges,
    /// Octets a second its input comes in at, plus one, zero while unknown.
    in_rate: AtomicU64,
    pub(crate) index: IndexSlot,
    /// How it is addressed and whether an index is coming, packed by [`SERVED`] and the rest.
    serves: AtomicU8,
    duration_plus_one: AtomicU64,
}

const SERVED: u8 = 1;
const BY_TIME: u8 = 1 << 1;
const BY_NONE: u8 = 2 << 1;
const BY_MASK: u8 = 3 << 1;
const REMOTE: u8 = 1 << 3;
const INDEXING: u8 = 1 << 4;
/// A link of the element changed since its index was published, the restarts may move.
const RELINKED: u8 = 1 << 5;

/// An element's published [`Index`]. Writers take turns, a reader never waits.
pub(crate) struct IndexSlot {
    writer: Mutex<Publisher<Option<Arc<Index>>>>,
    reader: Reader<Option<Arc<Index>>>,
}

impl Default for IndexSlot {
    fn default() -> Self {
        let (writer, reader) = published(None);
        IndexSlot { writer: Mutex::new(writer), reader }
    }
}

impl IndexSlot {
    pub(crate) fn publish(&self, index: Index) {
        let index = Some(Arc::new(index));
        self.writer.lock().unwrap_or_else(PoisonError::into_inner).publish(index);
    }

    pub(crate) fn get(&self) -> Option<Arc<Index>> {
        self.reader.read(Clone::clone)
    }
}

impl ElementCounters {
    pub fn record_in(&self, buffers: u64, bytes: u64) {
        self.batches_in.fetch_add(1, Ordering::Relaxed);
        self.buffers_in.fetch_add(buffers, Ordering::Relaxed);
        self.bytes_in.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_out(&self, buffers: u64, bytes: u64) {
        self.batches_out.fetch_add(1, Ordering::Relaxed);
        self.buffers_out.fetch_add(buffers, Ordering::Relaxed);
        self.bytes_out.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_queue_fill(&self, fill: u32) {
        self.queue_high_water.fetch_max(fill, Ordering::Relaxed);
    }

    pub fn record_drops(&self, buffers: u64) {
        self.drops.fetch_add(buffers, Ordering::Relaxed);
    }

    /// Record the newest pts of a batch produced on src pad `pad`, `NONE` records nothing.
    pub fn record_pts_out(&self, pad: usize, pts: Timestamp) {
        let slot = &self.pad_pts_plus_one[pad.min(POSITION_PADS - 1)];
        slot.store(slot.load(Ordering::Relaxed).max(pts.0.wrapping_add(1)), Ordering::Relaxed);
    }

    pub(crate) fn set_in_rate(&self, bytes_per_second: u64) {
        self.in_rate.store(bytes_per_second.saturating_add(1), Ordering::Relaxed);
    }

    pub fn in_rate(&self) -> Option<u64> {
        self.in_rate.load(Ordering::Relaxed).checked_sub(1)
    }

    pub(crate) fn set_serves(&self, serves: Serves) {
        let by = match serves.by {
            Addressed::Byte => 0,
            Addressed::Time => BY_TIME,
            Addressed::No => BY_NONE,
        };
        let packed = SERVED | by | if serves.remote { REMOTE } else { 0 };
        self.serves.fetch_and(INDEXING | RELINKED, Ordering::AcqRel);
        self.serves.fetch_or(packed, Ordering::AcqRel);
    }

    pub fn serves(&self) -> Option<Serves> {
        let bits = self.serves.load(Ordering::Acquire);
        let by = match bits & BY_MASK {
            0 => Addressed::Byte,
            BY_TIME => Addressed::Time,
            _ => Addressed::No,
        };
        (bits & SERVED != 0).then_some(Serves { by, remote: bits & REMOTE != 0 })
    }

    pub(crate) fn set_indexing(&self, on: bool) {
        match on {
            true => self.serves.fetch_or(INDEXING, Ordering::AcqRel),
            false => self.serves.fetch_and(!INDEXING, Ordering::AcqRel),
        };
    }

    /// An index is coming or the published one may move, planning on it waits.
    pub(crate) fn indexing(&self) -> bool {
        self.serves.load(Ordering::Acquire) & (INDEXING | RELINKED) != 0
    }

    pub(crate) fn set_relinked(&self, on: bool) {
        match on {
            true => self.serves.fetch_or(RELINKED, Ordering::AcqRel),
            false => self.serves.fetch_and(!RELINKED, Ordering::AcqRel),
        };
    }

    pub(crate) fn set_duration(&self, ns: u64) {
        self.duration_plus_one.store(ns.saturating_add(1), Ordering::Release);
    }

    pub fn duration(&self) -> Option<Timestamp> {
        self.duration_plus_one.load(Ordering::Acquire).checked_sub(1).map(Timestamp)
    }

    pub fn forget_pts_out(&self) {
        self.pad_pts_plus_one.iter().for_each(|slot| slot.store(0, Ordering::Relaxed));
    }

    pub fn snapshot(&self) -> CounterSnapshot {
        let pad_pts_out = std::array::from_fn(|pad| {
            self.pad_pts_plus_one[pad].load(Ordering::Relaxed).checked_sub(1).map(Timestamp)
        });
        CounterSnapshot {
            buffers_in: self.buffers_in.load(Ordering::Relaxed),
            buffers_out: self.buffers_out.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
            batches_in: self.batches_in.load(Ordering::Relaxed),
            batches_out: self.batches_out.load(Ordering::Relaxed),
            queue_high_water: self.queue_high_water.load(Ordering::Relaxed),
            drops: self.drops.load(Ordering::Relaxed),
            pts_out: pad_pts_out.iter().flatten().max().copied(),
            pad_pts_out,
        }
    }
}

/// A cloneable handle for reading live stats off a running pipeline from any thread.
#[derive(Clone)]
pub struct TapHandle {
    entries: Roster,
    idle: Arc<crate::command::Control>,
}

impl TapHandle {
    pub(crate) fn new(entries: Roster, idle: Arc<crate::command::Control>) -> Self {
        Self { entries, idle }
    }

    pub fn pool(&self) -> crate::memory::PoolStats {
        self.entries.read(View::pool_stats)
    }

    /// One element's pool, `None` for an unknown id, outside a run and once it left.
    pub fn pool_of(&self, el: ElementId) -> Option<crate::memory::PoolStats> {
        let at = |view: &View| view.pools.binary_search_by_key(&el.0, |p| p.0).ok();
        self.entries.read(|view| at(view).map(|at| view.pools[at].1.stats()))
    }

    /// Slots out and caps across the run's pools, and whether any pool is over its cap.
    pub fn pool_use(&self) -> (u64, u64, bool) {
        let sum = |(out, cap, over): (u64, u64, bool), (o, c): (u64, u64)| {
            (out + o, cap.saturating_add(c), over || o > c)
        };
        self.entries
            .read(|view| view.pools.iter().map(|(_, p)| p.in_use()).fold((0, 0, false), sum))
    }

    pub fn scheduler(&self) -> crate::pipeline::SchedulerStats {
        self.idle.scheduler_stats()
    }

    /// How many elements the pipeline ever had, the next one's id.
    pub fn len(&self) -> usize {
        self.entries.read(|view| view.count)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn element_name(&self, el: ElementId) -> Option<&'static str> {
        row(&self.entries, el, |row| row.desc.name)
    }

    /// A snapshot of one element's cumulative counters, `None` for an unknown or gone id.
    pub fn snapshot(&self, el: ElementId) -> Option<CounterSnapshot> {
        row(&self.entries, el, |row| row.counters.snapshot())
    }

    /// The `(process, queue_wait, sink_wait_lateness)` histograms, zero unless tracing.
    pub fn latency(
        &self,
        el: ElementId,
    ) -> Option<(LatencySnapshot, LatencySnapshot, LatencySnapshot)> {
        row(&self.entries, el, |row| {
            let c = &row.counters;
            (c.process_ns.snapshot(), c.queue_ns.snapshot(), c.wait_lateness_ns.snapshot())
        })
    }

    /// What the link into sink pad `pad` of `el` holds now.
    pub fn queued(&self, el: ElementId, pad: crate::id::SinkPad) -> Option<Held> {
        row(&self.entries, el, |row| {
            row.levels.iter().find(|(p, _)| *p == pad).map(|(_, l)| l.held())
        })?
    }

    /// The limit the link into sink pad `pad` of `el` stands at now, grown or set.
    pub fn link_limit(&self, el: ElementId, pad: crate::id::SinkPad) -> Option<Limit> {
        row(&self.entries, el, |row| {
            row.levels.iter().find(|(p, _)| *p == pad).map(|(_, l)| l.limit())
        })?
    }

    pub fn index(&self, el: ElementId) -> Option<Arc<Index>> {
        row(&self.entries, el, |row| row.counters.index.get()).flatten()
    }

    pub fn serves(&self, el: ElementId) -> Option<Serves> {
        row(&self.entries, el, |row| row.counters.serves()).flatten()
    }

    pub fn in_rate(&self, el: ElementId) -> Option<u64> {
        row(&self.entries, el, |row| row.counters.in_rate()).flatten()
    }

    /// What `el` holds in its own store and the ranges of its input that is.
    pub fn stored(&self, el: ElementId) -> Option<(Held, RangeSet)> {
        row(&self.entries, el, |row| (row.counters.store.held(), row.counters.ranges.read()))
    }

    /// The stream position being presented. Unlike `Control::position` not held once the run is
    /// over.
    pub fn now(&self) -> Timestamp {
        self.idle.presented_on(|| self.entries.read(|view| Arc::clone(&view.clock)).now())
    }

    pub fn stream_time(&self, running: Timestamp) -> Timestamp {
        self.idle.timeline.stream(running)
    }
}

#[derive(Clone, Debug, Default)]
pub struct LatencyReport {
    pub paths: Vec<PathLatency>,
}

impl LatencyReport {
    pub fn total(&self) -> Timestamp {
        self.paths.first().map_or(Timestamp::ZERO, |p| p.total)
    }
}

/// The worst source to sink path for one sink.
#[derive(Clone, Debug)]
pub struct PathLatency {
    pub sink: ElementId,
    /// Sum of the declared minimum latencies upstream of the sink on its worst path.
    pub total: Timestamp,
    /// `(element, declared min latency)` along the path, source first, sink included.
    pub per_element: Vec<(ElementId, Timestamp)>,
    pub is_live: bool,
    /// The sum of the path's link time limits. A link with no limit, or a sparse one, adds
    /// nothing.
    pub residency: Timestamp,
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    // Fixtures allocate freely.
    #[allow(clippy::disallowed_methods)]
    fn a_chunked_table_copies_the_chunks_that_changed_and_shares_the_rest() {
        let mut table: Vec<u32> = (0..130).collect();
        let mut view = Chunks::from(table.clone());
        let before = view.clone();
        table[3] = 1_000;
        table.push(130);
        view.update(&table, &mut vec![0], |_| true);
        assert_eq!(view.len(), 131);
        assert!(view.iter().copied().eq(table.iter().copied()), "the view reads the table");
        assert!(
            Arc::ptr_eq(&view.chunks[1].1, &before.chunks[1].1),
            "an untouched chunk is shared"
        );
        assert!(
            !Arc::ptr_eq(&view.chunks[0].1, &before.chunks[0].1)
                && !Arc::ptr_eq(&view.chunks[2].1, &before.chunks[2].1)
        );
        assert_eq!(
            (before.get(3), before.get(130)),
            (Some(&3), None),
            "a reader's version stays whole"
        );
        table.push(131);
        view.update(&table, &mut Vec::new(), |_| true);
        assert_eq!(view.get(131), Some(&131));
        let same = view.clone();
        view.update(&table, &mut Vec::new(), |_| true);
        assert!(
            Arc::ptr_eq(&view.chunks, &same.chunks),
            "a publish that changed nothing shares the whole"
        );
        view.update(&table, &mut vec![1], |c| c != 1);
        assert_eq!(
            (view.chunks.len(), view.len(), view.get(CHUNK + 1), view.get(2 * CHUNK)),
            (2, 132, None, Some(&128))
        );
        assert!(Arc::ptr_eq(&view.chunks[0].1, &same.chunks[0].1));
    }

    #[test]
    fn the_view_names_what_is_there_past_the_first_chunk_and_forgets_what_left() {
        use crate::element::{Element, ElementDesc, Flow, InputPolicy, LatencyDesc, SchedHint};
        static DESC: ElementDesc = ElementDesc {
            name: "solo",
            sinks: &[],
            srcs: &[],
            props: &[],
            sched: SchedHint::Active,
            inputs: InputPolicy::None,
            latency: LatencyDesc::NONE,
            make_default: None,
            io: false,
            couplings: &[],
        };
        struct Solo;
        impl Element for Solo {
            fn desc(&self) -> &'static ElementDesc {
                &DESC
            }
            fn process(
                &mut self,
                _: &mut crate::ctx::Ctx,
                _: crate::batch::Inputs<'_>,
            ) -> Result<Flow, crate::error::Error> {
                Ok(Flow::Eos)
            }
        }
        let mut p = crate::pipeline::Pipeline::new();
        let tap = p.tap_handle();
        let els: Vec<ElementId> = (0..3 * CHUNK as u32 + 5).map(|_| p.add(Solo)).collect();
        assert_eq!(tap.len(), els.len());
        p.remove(els[2], crate::topology::Leave::Cut).unwrap();
        p.remove(els[CHUNK + 1], crate::topology::Leave::Cut).unwrap();
        for (i, el) in els.iter().enumerate() {
            let gone = i == 2 || i == CHUNK + 1;
            assert_eq!(tap.element_name(*el), (!gone).then_some("solo"), "element {i}");
        }
        let kept = |p: &crate::pipeline::Pipeline| p.view_chunks();
        assert_eq!(kept(&p), 4);
        els[CHUNK..2 * CHUNK]
            .iter()
            .filter(|el| **el != els[CHUNK + 1])
            .for_each(|el| p.remove(*el, crate::topology::Leave::Cut).unwrap());
        assert_eq!(
            (kept(&p), tap.len()),
            (3, els.len()),
            "a chunk whose elements all left is dropped"
        );
        assert_eq!(tap.element_name(els[2 * CHUNK]), Some("solo"));
    }

    #[test]
    fn counters_accumulate_and_snapshot() {
        let c = ElementCounters::default();
        c.record_in(4, 1000);
        c.record_in(2, 500);
        c.record_out(6, 1500);
        c.record_queue_fill(3);
        c.record_queue_fill(1);
        let s = c.snapshot();
        assert_eq!(s.buffers_in, 6);
        assert_eq!(s.bytes_in, 1500);
        assert_eq!(s.batches_in, 2);
        assert_eq!(s.buffers_out, 6);
        assert_eq!(s.bytes_out, 1500);
        assert_eq!(s.batches_out, 1);
        assert_eq!(s.queue_high_water, 3);
    }

    #[test]
    fn the_output_position_keeps_the_newest_pts_until_a_flush() {
        let c = ElementCounters::default();
        let ms = Timestamp::from_millis;
        assert_eq!(c.snapshot().pts_out, None);
        c.record_pts_out(0, Timestamp::NONE);
        assert_eq!(c.snapshot().pts_out, None, "a batch with no pts records nothing");
        c.record_pts_out(0, Timestamp::ZERO);
        assert_eq!(c.snapshot().pts_out, Some(Timestamp::ZERO));
        for pts in [ms(120), ms(40), ms(80), Timestamp::NONE] {
            c.record_pts_out(0, pts);
        }
        assert_eq!(c.snapshot().pts_out, Some(ms(120)));
        c.forget_pts_out();
        assert_eq!(c.snapshot().pts_out, None, "a flush forgets it");
        c.record_pts_out(0, ms(10));
        assert_eq!(c.snapshot().pts_out, Some(ms(10)), "behind the old position, and true");
        c.record_pts_out(0, Timestamp::MAX);
        assert_eq!(c.snapshot().pts_out, Some(Timestamp::MAX));
    }

    #[test]
    fn each_src_pad_has_its_own_output_position() {
        let c = ElementCounters::default();
        let ms = Timestamp::from_millis;
        c.record_pts_out(0, ms(40));
        for pts in [ms(20), ms(900), ms(4_000)] {
            c.record_pts_out(1, pts);
        }
        let s = c.snapshot();
        assert_eq!((s.pad_pts(SrcPad(0)), s.pad_pts(SrcPad(1))), (Some(ms(40)), Some(ms(4_000))));
        assert_eq!(s.pad_pts(SrcPad(2)), None, "a pad that put nothing out");
        assert_eq!(s.pts_out, Some(ms(4_000)), "the element's is the furthest pad's");
        c.record_pts_out(POSITION_PADS + 3, ms(70));
        c.record_pts_out(POSITION_PADS - 1, ms(50));
        let s = c.snapshot();
        assert_eq!(s.pad_pts(SrcPad(POSITION_PADS as u32 - 1)), Some(ms(70)));
        assert_eq!(s.pad_pts(SrcPad(400)), Some(ms(70)));
        c.forget_pts_out();
        assert_eq!(c.snapshot(), CounterSnapshot::default(), "a flush forgets every pad");
    }

    #[test]
    fn tap_handle_reads_counters_and_clock() {
        use crate::clock::MockClock;

        let counters = Arc::new(ElementCounters::default());
        let clock = MockClock::new();
        let base = Arc::new(std::sync::atomic::AtomicI64::new(crate::time::BASE_UNSET));
        use crate::element::{InputPolicy, LatencyDesc, SchedHint};
        static DESC: ElementDesc = ElementDesc {
            name: "testsrc",
            sinks: &[],
            srcs: &[],
            props: &[],
            sched: SchedHint::Active,
            inputs: InputPolicy::None,
            latency: LatencyDesc::NONE,
            make_default: None,
            io: false,
            couplings: &[],
        };
        let props = Arc::new(PropTable::new(0));
        let row = Row {
            desc: &DESC,
            props,
            counters: Arc::clone(&counters),
            gone: false,
            levels: Vec::new(),
        };
        let view = |rows: Vec<Row>| {
            let (slots, count) =
                (Arc::new((0..rows.len() as u32).map(|i| (i, i)).collect()), rows.len());
            View { rows: rows.into(), slots, count, ..View::empty(Arc::new(clock.clone())) }
        };
        let (mut publisher, roster) = crate::publish::published(view(vec![row.clone()]));
        let tap = TapHandle::new(roster, crate::command::Control::new(Arc::clone(&base)));
        assert_eq!(tap.pool(), crate::memory::PoolStats::default(), "no run: no pools");
        assert_eq!(tap.scheduler().parks, 0, "no run: nothing has parked");

        assert!(tap.now().is_none(), "no run yet, no running time");
        assert_eq!(tap.element_name(ElementId(0)), Some("testsrc"));
        assert!(tap.snapshot(ElementId(1)).is_none(), "unknown element");

        base.store(crate::time::base_from_clock(clock.now()), Ordering::Release);
        clock.advance(Timestamp::from_millis(5));
        counters.record_out(1, 1_000_000);

        let s = tap.snapshot(ElementId(0)).unwrap();
        assert_eq!(s.bytes_out, 1_000_000);
        assert_eq!(tap.now(), Timestamp::from_millis(5));
        let bits_per_sec = s.bytes_out * 8 * 1_000_000_000 / tap.now().0;
        assert_eq!(bits_per_sec, 1_600_000_000);

        let joined = Row {
            desc: &DESC,
            props: Arc::new(PropTable::new(0)),
            counters: Arc::default(),
            gone: false,
            levels: Vec::new(),
        };
        publisher.publish(view(vec![Row { gone: true, ..row }, joined]));
        assert!(tap.snapshot(ElementId(0)).is_none(), "it left");
        assert_eq!(tap.element_name(ElementId(0)), None);
        assert_eq!(tap.snapshot(ElementId(1)), Some(CounterSnapshot::default()), "it joined");
        assert_eq!(tap.len(), 2);
    }
}
