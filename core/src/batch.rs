use std::collections::VecDeque;

use crate::buffer::Buffer;
use crate::event::{Event, EventClass};
use crate::id::FormatId;
use crate::time::Timestamp;

/// Owned storage for a span of buffers with a drain cursor and positioned events.
pub struct Batch {
    pub format: FormatId,
    rows: Vec<Buffer>,
    head: usize,
    bytes: u64,
    newest: Timestamp,
    events: VecDeque<(u32, Event)>,
    /// Seek generation the span was produced under, stale batches are dropped.
    pub seek_gen: u64,
    pub(crate) pushed_at: Option<u64>,
    /// End of stream in band, so the producer stays alive to serve a later seek.
    pub eos: bool,
}

impl Batch {
    // Batch shell constructor, empty collections allocate nothing until filled.
    #[allow(clippy::disallowed_methods)]
    pub fn new(format: FormatId) -> Self {
        Self {
            format,
            rows: Vec::new(),
            head: 0,
            bytes: 0,
            newest: Timestamp::NONE,
            events: VecDeque::new(),
            seek_gen: 0,
            pushed_at: None,
            eos: false,
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len() - self.head
    }

    pub fn is_empty(&self) -> bool {
        self.head == self.rows.len()
    }

    pub fn total_bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn newest(&self) -> Timestamp {
        self.newest
    }

    pub(crate) fn span(&self) -> (u64, Timestamp, Timestamp) {
        let rows = self.rows();
        let from = rows.first().map_or(Timestamp::NONE, |b| b.pts);
        let mut to = rows.last().map_or(Timestamp::NONE, |b| match b.duration.is_some() {
            true => b.pts.saturating_add(b.duration),
            false => b.pts,
        });
        for (_, ev) in &self.events {
            if let Event::Gap { until } = ev {
                to = if to.is_some() { to.max(*until) } else { *until };
            }
        }
        (self.total_bytes(), from, to)
    }

    pub(crate) fn within(&self, room: Timestamp) -> usize {
        let rows = self.rows();
        let Some(first) = rows.first().and_then(|b| b.pts.nanos()) else {
            return rows.len().max(1);
        };
        let inside =
            |b: &Buffer| b.pts.nanos().is_none_or(|pts| pts.saturating_sub(first) < room.0);
        rows.iter().take_while(|b| inside(b)).count().max(1)
    }

    pub fn clear(&mut self) {
        self.reset_rows();
        self.events.clear();
        self.pushed_at = None;
        // A recycled shell must not end the next stream.
        self.eos = false;
    }

    pub(crate) fn retain_state(&mut self) {
        self.rows.clear();
        self.head = 0;
        (self.bytes, self.newest) = (0, Timestamp::NONE);
        self.events.retain_mut(|(pos, ev)| {
            *pos = 0;
            ev.class() == EventClass::State
        });
        self.pushed_at = None;
        self.eos = false;
    }

    fn reset_rows(&mut self) {
        let drained = self.head as u32;
        self.rows.clear();
        self.head = 0;
        (self.bytes, self.newest) = (0, Timestamp::NONE);
        for (pos, _) in &mut self.events {
            *pos = pos.saturating_sub(drained);
        }
    }

    /// Drop the drained prefix once it outweighs the live rows. Amortized O(1).
    fn compact(&mut self) {
        if self.head == 0 || self.head < self.rows.len() - self.head {
            return;
        }
        let drained = self.head as u32;
        self.rows.drain(..self.head);
        self.head = 0;
        for (pos, _) in &mut self.events {
            *pos = pos.saturating_sub(drained);
        }
    }

    pub fn push(&mut self, buf: Buffer) {
        self.compact();
        self.bytes += buf.memory.len() as u64;
        self.newest = later(self.newest, buf.pts);
        self.rows.push(buf);
    }

    /// Take the oldest buffer in O(1), `None` behind an undelivered event.
    pub fn pop_front(&mut self) -> Option<Buffer> {
        if self.head == self.rows.len() {
            return None;
        }
        if let Some(&(pos, _)) = self.events.front()
            && pos as usize <= self.head
        {
            return None;
        }
        let row = &mut self.rows[self.head];
        self.bytes -= row.memory.len() as u64;
        let buf = Buffer {
            memory: row.memory.take(),
            pts: row.pts,
            dts: row.dts,
            duration: row.duration,
            flags: row.flags,
            format: row.format,
            sync: row.sync.take(),
        };
        self.head += 1;
        if self.head == self.rows.len() {
            self.reset_rows();
        }
        Some(buf)
    }

    pub fn front(&self) -> Option<&Buffer> {
        match self.events.front() {
            Some(&(pos, _)) if pos as usize <= self.head => None,
            _ => self.rows.get(self.head),
        }
    }

    /// Move all undrained buffers and events of `other` onto `self`. Onto an empty batch the
    /// storage moves.
    pub fn append(&mut self, other: &mut Batch) {
        let h = other.head;
        self.bytes += other.bytes;
        self.newest = later(self.newest, other.newest);
        if self.rows.is_empty() && h == 0 {
            // Positions hold, the rows land at zero and ours, with no rows, are at zero too.
            std::mem::swap(&mut self.rows, &mut other.rows);
            if self.events.is_empty() {
                std::mem::swap(&mut self.events, &mut other.events);
            } else {
                self.events.append(&mut other.events);
            }
        } else {
            self.compact();
            let base = self.rows.len() as u32;
            self.rows.extend(other.rows.drain(h..));
            for (pos, ev) in other.events.drain(..) {
                self.events.push_back((base + (pos.saturating_sub(h as u32)), ev));
            }
        }
        other.reset_rows();
    }

    /// Moves the first `limit` undrained rows, up to an undelivered event, onto `into`. Returns how
    /// many.
    pub(crate) fn take_front(&mut self, limit: usize, into: &mut Batch) -> usize {
        let len = self.len();
        let open = match self.events.front() {
            Some(&(pos, _)) => (pos as usize).saturating_sub(self.head).min(len),
            None => len,
        };
        let n = limit.min(open);
        if n == 0 {
            return 0;
        }
        if n == len {
            let newest = std::mem::replace(&mut self.newest, Timestamp::NONE);
            into.newest = later(into.newest, newest);
            into.bytes += std::mem::take(&mut self.bytes);
            if into.rows.is_empty() && self.head == 0 {
                std::mem::swap(&mut self.rows, &mut into.rows);
            } else {
                into.compact();
                into.rows.extend(self.rows.drain(self.head..));
            }
            // Drained to the end, so a trailing event is rebased to the front as a pop would.
            self.head += n;
            self.reset_rows();
            return n;
        }
        into.compact();
        for row in &mut self.rows[self.head..self.head + n] {
            self.bytes -= row.memory.len() as u64;
            into.push(Buffer {
                memory: row.memory.take(),
                pts: row.pts,
                dts: row.dts,
                duration: row.duration,
                flags: row.flags,
                format: row.format,
                sync: row.sync.take(),
            });
        }
        self.head += n;
        n
    }

    pub(crate) fn share_into(&self, other: &mut Batch) {
        let (base, h) = (other.rows.len() as u32, self.head as u32);
        other.bytes += self.bytes;
        other.newest = later(other.newest, self.newest);
        other.rows.extend(self.rows[self.head..].iter().map(|b| Buffer {
            memory: b.memory.clone(),
            pts: b.pts,
            dts: b.dts,
            duration: b.duration,
            flags: b.flags,
            format: b.format,
            sync: b.sync.clone(),
        }));
        let on = |(pos, ev): &(u32, Event)| (base + pos.saturating_sub(h), ev.clone());
        other.events.extend(self.events.iter().map(on));
    }

    pub fn push_event(&mut self, event: Event) {
        self.events.push_back((self.rows.len() as u32, event));
    }

    pub(crate) fn push_event_at(&mut self, pos: u32, event: Event) {
        let at = self.events.partition_point(|&(p, _)| p <= pos);
        self.events.insert(at, (pos, event));
    }

    pub fn due_event(&mut self) -> Option<Event> {
        match self.events.front() {
            Some(&(pos, _)) if pos as usize <= self.head => self.next_event_any(),
            _ => None,
        }
    }

    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    pub(crate) fn next_event_any(&mut self) -> Option<Event> {
        self.events.pop_front().map(|(_, ev)| ev)
    }

    pub fn cap_rows(&self) -> usize {
        self.rows.capacity()
    }

    pub fn is_inert(&self) -> bool {
        self.is_empty() && self.events.is_empty()
    }

    /// The undrained rows, oldest first, blind to event barriers.
    pub fn rows(&self) -> &[Buffer] {
        &self.rows[self.head..]
    }
}

fn later(a: Timestamp, b: Timestamp) -> Timestamp {
    if a.is_none() || b.is_some() && b > a { b } else { a }
}

pub struct OutBatch<'a> {
    batch: &'a mut Batch,
}

impl<'a> OutBatch<'a> {
    pub(crate) fn new(batch: &'a mut Batch) -> Self {
        Self { batch }
    }

    pub fn push(&mut self, buf: Buffer) {
        self.batch.push(buf);
    }

    /// Moves the input span onto this output, stopping where [`Inputs::pop`] would. Returns how
    /// many rows.
    pub fn forward(&mut self, inputs: &mut Inputs<'_>) -> usize {
        let Some(batch) = inputs.batch.as_mut() else {
            return 0;
        };
        let n = batch.take_front(inputs.rows_left, self.batch);
        inputs.rows_left -= n;
        n
    }

    pub fn len(&self) -> usize {
        self.batch.len()
    }

    pub fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }
}

/// The inputs delivered to `Element::process`.
pub struct Inputs<'a> {
    batch: Option<&'a mut Batch>,
    rows_left: usize,
}

impl<'a> Inputs<'a> {
    pub fn empty() -> Inputs<'static> {
        Inputs { batch: None, rows_left: 0 }
    }

    pub fn owned(batch: &'a mut Batch) -> Self {
        Inputs { batch: Some(batch), rows_left: usize::MAX }
    }

    pub(crate) fn limited(batch: &'a mut Batch, rows: usize) -> Self {
        Inputs { batch: Some(batch), rows_left: rows }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.batch.as_ref().map_or(0, |b| b.len().min(self.rows_left))
    }

    pub fn pop(&mut self) -> Option<Buffer> {
        if self.rows_left == 0 {
            return None;
        }
        let buf = self.batch.as_mut().and_then(|b| b.pop_front())?;
        self.rows_left -= 1;
        Some(buf)
    }

    pub fn front(&self) -> Option<&Buffer> {
        self.batch.as_ref().filter(|_| self.rows_left > 0).and_then(|b| b.front())
    }

    pub fn rows(&self) -> &[Buffer] {
        self.batch.as_ref().map_or(&[], |b| {
            let rows = b.rows();
            &rows[..rows.len().min(self.rows_left)]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufferFlags;
    use crate::memory::Pool;
    use crate::time::Timestamp;

    fn buf(pool: &Pool, fill: u8, len: usize) -> Buffer {
        let mut memory = pool.acquire();
        memory.as_mut_full()[..len].fill(fill);
        memory.set_len(len);
        Buffer {
            memory,
            pts: Timestamp::from_nanos(fill as u64),
            dts: Timestamp::NONE,
            duration: Timestamp::NONE,
            flags: BufferFlags::empty(),
            format: FormatId(0),
            sync: None,
        }
    }

    fn marked(pool: &Pool, seed: u8) -> Buffer {
        let mut b = buf(pool, seed, 1);
        b.dts = Timestamp::from_nanos(1000 + seed as u64);
        b.duration = Timestamp::from_nanos(2000 + seed as u64);
        b.flags = BufferFlags::KEYFRAME | BufferFlags(u32::from(seed) << 8);
        b.format = FormatId(100 + u32::from(seed));
        b
    }

    fn assert_marked(b: &Buffer, seed: u8) {
        assert_eq!(b.memory.data(), &[seed], "payload");
        assert_eq!(b.pts, Timestamp::from_nanos(seed as u64), "pts");
        assert_eq!(b.dts, Timestamp::from_nanos(1000 + seed as u64), "dts");
        assert_eq!(b.duration, Timestamp::from_nanos(2000 + seed as u64), "duration");
        assert_eq!(b.flags, BufferFlags::KEYFRAME | BufferFlags(u32::from(seed) << 8), "flags");
        assert_eq!(b.format, FormatId(100 + u32::from(seed)), "format is the buffer's own");
    }

    #[test]
    fn every_field_survives_push_then_pop() {
        let pool = Pool::bounded(64, 8);
        let mut b = Batch::new(FormatId(0));
        for seed in 1..=3 {
            b.push(marked(&pool, seed));
        }
        for seed in 1..=3 {
            assert_marked(&b.pop_front().unwrap(), seed);
        }
        assert!(b.pop_front().is_none());
    }

    #[test]
    fn every_field_survives_append_after_partial_drain() {
        let pool = Pool::bounded(64, 8);
        let mut a = Batch::new(FormatId(0));
        let mut b = Batch::new(FormatId(0));
        a.push(marked(&pool, 1));
        a.push(marked(&pool, 2));
        assert_marked(&a.pop_front().unwrap(), 1);
        for seed in 3..=5 {
            b.push(marked(&pool, seed));
        }
        assert_marked(&b.pop_front().unwrap(), 3);
        a.append(&mut b);
        assert!(b.is_empty(), "source emptied");
        assert_eq!(a.len(), 3);
        for seed in [2, 4, 5] {
            assert_marked(&a.pop_front().unwrap(), seed);
        }
        assert!(a.pop_front().is_none());
    }

    #[test]
    fn every_field_survives_a_ring_hop() {
        let pool = Pool::bounded(64, 8);
        let (tx, rx) = crate::ring::spsc::<Batch>(2);
        let mut b = Batch::new(FormatId(0));
        for seed in 1..=4 {
            b.push(marked(&pool, seed));
        }
        assert_marked(&b.pop_front().unwrap(), 1);
        b.push_event(Event::Eos);
        b.seek_gen = 7;
        b.eos = true;
        let producer = std::thread::spawn(move || {
            assert!(tx.push(b).is_ok(), "consumer alive");
        });
        let mut got = rx.pop().expect("one batch crossed");
        producer.join().unwrap();
        assert_eq!((got.len(), got.seek_gen, got.eos), (3, 7, true));
        for seed in 2..=4 {
            assert!(got.due_event().is_none(), "the event trails the rows");
            assert_marked(&got.pop_front().unwrap(), seed);
        }
        assert!(matches!(got.due_event(), Some(Event::Eos)));
    }

    #[test]
    fn inputs_rows_reads_in_place_and_tracks_the_cursor() {
        let pool = Pool::bounded(64, 8);
        assert!(Inputs::empty().rows().is_empty());
        let mut b = Batch::new(FormatId(0));
        for seed in 1..=3 {
            b.push(marked(&pool, seed));
        }
        let mut inputs = Inputs::owned(&mut b);
        assert_eq!(inputs.rows().len(), 3);
        assert_marked(&inputs.pop().unwrap(), 1);
        let rows = inputs.rows();
        assert_eq!(rows.len(), 2, "rows start at the cursor");
        assert_marked(&rows[0], 2);
        assert_marked(&rows[1], 3);
        assert_eq!(inputs.len(), 2, "reading rows consumed nothing");
    }

    #[test]
    fn push_event_at_keeps_position_order_and_fronts_pop_in_order() {
        let pool = Pool::bounded(64, 8);
        let mut b = Batch::new(FormatId(0));
        b.push(buf(&pool, 1, 1));
        b.push(buf(&pool, 2, 1));
        b.push_event_at(2, Event::Eos);
        b.push_event_at(1, Event::FlushStart);
        b.push_event_at(0, Event::Paused);
        b.push_event_at(1, Event::FlushStop);
        assert!(b.pop_front().is_none(), "event at row 0 holds the first row");
        assert!(matches!(b.due_event(), Some(Event::Paused)));
        assert!(b.due_event().is_none(), "the next event is not due yet");
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 1);
        assert!(b.pop_front().is_none(), "barrier before row 1");
        assert!(matches!(b.due_event(), Some(Event::FlushStart)));
        assert!(matches!(b.due_event(), Some(Event::FlushStop)));
        assert!(b.due_event().is_none());
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 2);
        assert!(matches!(b.next_event_any(), Some(Event::Eos)));
        assert!(!b.has_events());
    }

    #[test]
    fn events_survive_a_drained_reset_at_the_right_row() {
        let pool = Pool::bounded(64, 8);
        let mut b = Batch::new(FormatId(0));
        b.push(buf(&pool, 1, 1));
        b.push_event(Event::Eos);
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 1);
        assert!(b.is_empty() && b.has_events());
        b.push(buf(&pool, 2, 1));
        assert!(b.pop_front().is_none(), "rebased to row 0, still a barrier");
        assert!(matches!(b.due_event(), Some(Event::Eos)));
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 2);
    }

    #[test]
    fn pop_front_is_fifo_and_rows_track_the_cursor() {
        let pool = Pool::bounded(64, 8);
        let mut b = Batch::new(FormatId(0));
        for (fill, len) in [(1u8, 10usize), (2, 20), (3, 30)] {
            b.push(buf(&pool, fill, len));
        }
        assert_eq!(b.len(), 3);
        assert_eq!(b.total_bytes(), 60);

        let first = b.pop_front().unwrap();
        assert_eq!(first.memory.data()[0], 1, "FIFO: oldest first");
        assert_eq!(b.len(), 2);
        assert_eq!(b.total_bytes(), 50, "drained rows don't count");
        let v = b.rows();
        assert_eq!(v.len(), 2, "rows start at the cursor");
        assert_eq!(v[0].memory.data()[0], 2);
        assert_eq!(v[0].pts, Timestamp::from_nanos(2));

        assert_eq!(b.pop_front().unwrap().memory.data()[0], 2);
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 3);
        assert!(b.pop_front().is_none());
        assert!(b.is_empty());
    }

    #[test]
    fn drained_memory_recycles_immediately() {
        let pool = Pool::bounded(64, 2);
        let mut b = Batch::new(FormatId(0));
        b.push(buf(&pool, 1, 1));
        b.push(buf(&pool, 2, 1));
        assert!(pool.try_acquire().is_none(), "pool exhausted");
        let first = b.pop_front().unwrap();
        drop(first);
        assert!(pool.try_acquire().is_some(), "slot came back while batch part-drained");
    }

    #[test]
    fn push_after_partial_drain_appends_undrained() {
        let pool = Pool::bounded(64, 8);
        let mut b = Batch::new(FormatId(0));
        b.push(buf(&pool, 1, 1));
        b.push(buf(&pool, 2, 1));
        let _ = b.pop_front();
        b.push(buf(&pool, 3, 1));
        assert_eq!(b.len(), 2);
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 2);
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 3);
        assert!(b.pop_front().is_none());
    }

    #[test]
    fn append_moves_only_undrained_rows_and_events() {
        let pool = Pool::bounded(64, 8);
        let mut a = Batch::new(FormatId(0));
        let mut b = Batch::new(FormatId(0));
        b.push(buf(&pool, 1, 1));
        b.push(buf(&pool, 2, 1));
        b.push(buf(&pool, 3, 1));
        let _ = b.pop_front();
        b.push_event(Event::Eos);
        a.append(&mut b);
        assert!(b.is_empty() && !b.has_events(), "source emptied");
        assert_eq!(a.len(), 2, "only undrained rows moved");
        assert!(a.has_events(), "events ride along");
        assert_eq!(a.pop_front().unwrap().memory.data()[0], 2);
        assert!(a.due_event().is_none(), "not due until its position is reached");
        assert_eq!(a.pop_front().unwrap().memory.data()[0], 3);
        assert!(matches!(a.due_event(), Some(Event::Eos)), "due after its preceding rows");
    }

    #[test]
    fn mid_batch_event_is_a_barrier_until_delivered() {
        let pool = Pool::bounded(64, 8);
        let mut b = Batch::new(FormatId(0));
        b.push(buf(&pool, 1, 1));
        b.push_event(Event::Eos);
        b.push(buf(&pool, 2, 1));
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 1, "row before the event flows");
        assert!(b.pop_front().is_none(), "barrier: row 2 held until the event is delivered");
        assert!(b.front().is_none() && b.rows().len() == 1, "front shows only what pops");
        assert!(matches!(b.due_event(), Some(Event::Eos)), "event due exactly here");
        assert_eq!(b.front().unwrap().memory.data()[0], 2);
        assert_eq!(Inputs::owned(&mut b).front().unwrap().memory.data()[0], 2);
        assert!(Inputs::empty().front().is_none());
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 2, "row after the event flows");
    }

    fn tags(title: &str) -> Event {
        let mut t = crate::event::TagList::new();
        t.add("TITLE", title);
        Event::Tags(t)
    }

    fn format(family: u32) -> Event {
        Event::FormatChange(crate::format::FixedFormat::new(FormatId(family)))
    }

    // A test assertion helper, not a hot path.
    #[allow(clippy::disallowed_methods)]
    fn drain_due(b: &mut Batch) -> String {
        let mut out = String::new();
        while let Some(ev) = b.due_event() {
            match ev {
                Event::FormatChange(f) => out.push_str(&format!("f{} ", f.family.0)),
                Event::Tags(t) => out.push_str(&format!("t{} ", t.get("TITLE").unwrap())),
                _ => out.push_str("positional "),
            }
        }
        out
    }

    #[test]
    fn retain_state_drops_rows_and_positional_events_and_keeps_state_in_order() {
        let pool = Pool::bounded(64, 4);
        let mut b = Batch::new(FormatId(0));
        b.push_event(tags("a"));
        b.push_event(Event::Segment { base: Timestamp::ZERO, rate: 1.0 });
        b.push(buf(&pool, 1, 1));
        b.push_event(format(7));
        b.push_event(Event::Gap { until: Timestamp::from_nanos(9) });
        b.push(buf(&pool, 2, 1));
        b.push_event(Event::Patch { offset: 0, data: pool.acquire() });
        b.push_event(tags("b"));
        b.push_event(Event::Eos);
        b.push_event(Event::FlushStop);
        b.eos = true;
        b.pushed_at = Some(1);
        assert_eq!(pool.stats().outstanding, 3);

        b.retain_state();

        assert_eq!(pool.stats().outstanding, 0, "rows and the patch payload recycled");
        assert!(b.is_empty() && b.has_events() && !b.is_inert());
        assert!(!b.eos && b.pushed_at.is_none(), "the mark is positional");
        b.push(buf(&pool, 3, 1));
        assert!(b.pop_front().is_none(), "state events are a barrier before the new row");
        assert_eq!(drain_due(&mut b), "ta f7 tb ");
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 3);
        assert!(b.is_inert());
    }

    #[test]
    fn retain_state_after_a_partial_drain_rebases_to_the_front() {
        let pool = Pool::bounded(64, 4);
        let mut b = Batch::new(FormatId(0));
        for fill in 1..=3 {
            b.push(buf(&pool, fill, 1));
        }
        b.push_event(format(4));
        let _ = b.pop_front();
        b.retain_state();
        assert_eq!(b.len(), 0);
        assert_eq!(drain_due(&mut b), "f4 ", "due at once, not three rows later");
        b.retain_state();
        assert!(b.is_inert(), "nothing to keep is an empty batch");
    }

    #[test]
    fn clear_resets_cursor_and_recycles() {
        let pool = Pool::bounded(64, 2);
        let mut b = Batch::new(FormatId(0));
        b.push(buf(&pool, 1, 1));
        b.push(buf(&pool, 2, 1));
        let _ = b.pop_front();
        b.clear();
        assert!(b.is_empty());
        assert!(pool.try_acquire().is_some(), "clear recycled the undrained row");
        b.push(buf(&pool, 4, 1));
        assert_eq!(b.pop_front().unwrap().memory.data()[0], 4);
    }

    #[test]
    fn a_paced_drain_topped_up_by_push_or_append_stays_bounded() {
        // The sink's pattern, the span is topped up and never drains to empty.
        let pool = Pool::new(64);
        for by_append in [false, true] {
            let mut b = Batch::new(FormatId(0));
            let mut seq = 0u8;
            for _ in 0..10_000 {
                while b.len() < 8 {
                    seq = seq.wrapping_add(1);
                    if by_append {
                        let mut src = Batch::new(FormatId(0));
                        src.push(buf(&pool, seq, 1));
                        b.append(&mut src);
                    } else {
                        b.push(buf(&pool, seq, 1));
                    }
                }
                assert!(b.pop_front().is_some());
            }
            assert_eq!(b.len(), 7);
            assert!(b.cap_rows() <= 32, "rows grew to {} (append {by_append})", b.cap_rows());
            assert_eq!(b.rows().len(), 7, "the compacted span reads the same");
            assert_eq!(b.total_bytes(), 7);
        }
    }

    #[test]
    fn compaction_rebases_event_positions_and_keeps_the_barrier() {
        let pool = Pool::new(64);
        let mut b = Batch::new(FormatId(0));
        for fill in 1..=3 {
            b.push(buf(&pool, fill, 1));
        }
        b.push_event(Event::Eos);
        b.push(buf(&pool, 4, 1));
        b.push(buf(&pool, 5, 1));
        for fill in 1..=3 {
            assert_eq!(b.pop_front().unwrap().memory.data()[0], fill);
        }
        b.push(buf(&pool, 6, 1));
        assert_eq!(b.rows().len(), 3);
        assert!(b.pop_front().is_none(), "the event still fronts row 4");
        assert!(matches!(b.due_event(), Some(Event::Eos)));
        for fill in 4..=6 {
            assert_eq!(b.pop_front().unwrap().memory.data()[0], fill);
        }
        assert!(b.is_inert());
    }

    #[test]
    fn append_onto_an_empty_batch_moves_the_storage_and_keeps_the_barriers() {
        let pool = Pool::bounded(64, 8);
        let mut a = Batch::new(FormatId(0));
        let mut b = Batch::new(FormatId(0));
        a.push(buf(&pool, 9, 1));
        let _ = a.pop_front();
        a.push_event(Event::Eos);
        assert!(a.is_empty() && a.cap_rows() > 0);
        let (a_cap, b_cap) = (a.cap_rows(), 0);
        for fill in 1..=3 {
            b.push(buf(&pool, fill, 1));
        }
        b.push_event(Event::FlushStop);
        let moved = b.cap_rows();
        assert!(moved >= 3 && b_cap == 0);
        a.append(&mut b);
        assert_eq!((a.cap_rows(), b.cap_rows()), (moved, a_cap), "the two vectors changed hands");
        assert!(b.is_inert());
        assert_eq!((a.len(), a.total_bytes()), (3, 3));
        assert!(a.pop_front().is_none(), "the event already waiting fronts the moved rows");
        assert!(matches!(a.due_event(), Some(Event::Eos)));
        for fill in 1..=3 {
            assert_eq!(a.pop_front().unwrap().memory.data()[0], fill);
        }
        assert!(matches!(a.due_event(), Some(Event::FlushStop)), "the moved event trails the rows");
        assert!(a.is_inert());
    }

    #[test]
    fn append_of_a_part_drained_span_copies_the_rest() {
        let pool = Pool::bounded(64, 8);
        let mut a = Batch::new(FormatId(0));
        let mut b = Batch::new(FormatId(0));
        for seed in 1..=3 {
            b.push(marked(&pool, seed));
        }
        let _ = b.pop_front();
        let b_cap = b.cap_rows();
        a.append(&mut b);
        assert_eq!(b.cap_rows(), b_cap, "a drained prefix keeps the storage where it is");
        assert_eq!(a.len(), 2);
        assert_marked(&a.pop_front().unwrap(), 2);
        assert_marked(&a.pop_front().unwrap(), 3);
    }

    #[test]
    fn forward_moves_the_whole_span_in_one_go() {
        let pool = Pool::bounded(64, 8);
        let mut input = Batch::new(FormatId(0));
        let mut out = Batch::new(FormatId(0));
        for seed in 1..=4 {
            input.push(marked(&pool, seed));
        }
        input.push_event(Event::Eos);
        let cap = input.cap_rows();
        let mut inputs = Inputs::owned(&mut input);
        assert_eq!(OutBatch::new(&mut out).forward(&mut inputs), 4);
        assert_eq!(OutBatch::new(&mut out).forward(&mut inputs), 0);
        assert!(inputs.is_empty());
        assert_eq!(
            (out.cap_rows(), out.len(), out.total_bytes()),
            (cap, 4, 4),
            "the storage moved"
        );
        assert_eq!(out.newest(), Timestamp::from_nanos(4));
        assert!(
            input.is_empty() && input.has_events(),
            "the trailing event stays, rebased to the front"
        );
        assert!(matches!(input.due_event(), Some(Event::Eos)));
        for seed in 1..=4 {
            assert_marked(&out.pop_front().unwrap(), seed);
        }
    }

    #[test]
    fn forward_stops_at_an_event_and_at_the_rows_shown() {
        let pool = Pool::bounded(64, 8);
        let mut input = Batch::new(FormatId(0));
        let mut out = Batch::new(FormatId(0));
        out.push(marked(&pool, 9));
        for seed in 1..=2 {
            input.push(marked(&pool, seed));
        }
        input.push_event(Event::Eos);
        for seed in 3..=5 {
            input.push(marked(&pool, seed));
        }
        let mut inputs = Inputs::owned(&mut input);
        assert_eq!(OutBatch::new(&mut out).forward(&mut inputs), 2, "up to the event");
        assert_eq!(OutBatch::new(&mut out).forward(&mut inputs), 0, "the event is a barrier");
        assert_eq!((inputs.len(), out.len(), out.total_bytes()), (3, 3, 3));
        assert!(matches!(input.due_event(), Some(Event::Eos)));
        let mut inputs = Inputs::limited(&mut input, 2);
        assert_eq!(OutBatch::new(&mut out).forward(&mut inputs), 2, "the rows shown");
        assert_eq!(OutBatch::new(&mut out).forward(&mut inputs), 0);
        assert_eq!(inputs.len(), 0, "nothing more is shown");
        assert_eq!(input.len(), 1);
        let mut inputs = Inputs::owned(&mut input);
        assert_eq!(OutBatch::new(&mut out).forward(&mut inputs), 1);
        assert!(input.is_inert());
        for seed in [9, 1, 2, 3, 4, 5] {
            assert_marked(&out.pop_front().unwrap(), seed);
        }
        assert_eq!(out.total_bytes(), 0);
    }

    #[test]
    fn forward_matches_pop_and_push_under_every_split() {
        let pool = Pool::new(64);
        for (events_at, limit) in [(0usize, usize::MAX), (2, usize::MAX), (5, 3), (3, 1), (1, 0)] {
            let fill = |b: &mut Batch| {
                for seed in 1..=5u8 {
                    if seed as usize == events_at {
                        b.push_event(Event::Eos);
                    }
                    b.push(marked(&pool, seed));
                }
            };
            let (mut by_pop, mut by_forward) = (Batch::new(FormatId(0)), Batch::new(FormatId(0)));
            let (mut out_pop, mut out_fwd) = (Batch::new(FormatId(0)), Batch::new(FormatId(0)));
            fill(&mut by_pop);
            fill(&mut by_forward);
            let mut n = 0;
            let mut inputs = Inputs::limited(&mut by_pop, limit);
            while let Some(b) = inputs.pop() {
                out_pop.push(b);
                n += 1;
            }
            let mut inputs = Inputs::limited(&mut by_forward, limit);
            assert_eq!(
                OutBatch::new(&mut out_fwd).forward(&mut inputs),
                n,
                "events at {events_at}, limit {limit}"
            );
            assert_eq!(
                (by_pop.len(), by_pop.total_bytes()),
                (by_forward.len(), by_forward.total_bytes())
            );
            assert_eq!(
                (out_pop.len(), out_pop.total_bytes(), out_pop.newest()),
                (out_fwd.len(), out_fwd.total_bytes(), out_fwd.newest())
            );
            assert_eq!(drain_due(&mut by_pop), drain_due(&mut by_forward));
            loop {
                let (a, b) = (by_pop.pop_front(), by_forward.pop_front());
                assert_eq!(a.as_ref().map(|r| r.pts), b.as_ref().map(|r| r.pts));
                if a.is_none() {
                    let due = drain_due(&mut by_pop);
                    assert_eq!(due, drain_due(&mut by_forward));
                    if due.is_empty() {
                        break;
                    }
                }
            }
            assert!(by_pop.is_inert() && by_forward.is_inert());
        }
    }

    #[test]
    fn the_kept_sums_match_a_walk_of_the_rows() {
        let pool = Pool::new(64);
        let walk = |b: &Batch| b.rows().iter().map(|r| r.memory.len() as u64).sum::<u64>();
        let mut a = Batch::new(FormatId(0));
        for i in [3u8, 7, 5, 1] {
            a.push(buf(&pool, i, i as usize));
        }
        assert_eq!((a.total_bytes(), a.newest()), (16, Timestamp::from_nanos(7)));
        a.pop_front();
        assert_eq!(a.total_bytes(), walk(&a));
        let mut b = Batch::new(FormatId(0));
        b.push(buf(&pool, 9, 2));
        a.share_into(&mut b);
        assert_eq!((b.total_bytes(), b.newest()), (walk(&b), Timestamp::from_nanos(9)));
        let mut c = Batch::new(FormatId(0));
        c.append(&mut b);
        assert_eq!(
            (c.total_bytes(), b.total_bytes(), c.newest()),
            (walk(&c), 0, Timestamp::from_nanos(9))
        );
        while c.pop_front().is_some() {}
        assert_eq!(
            (c.total_bytes(), c.newest()),
            (0, Timestamp::NONE),
            "drained to empty clears both"
        );
        a.push_event(Event::Eos);
        a.retain_state();
        assert_eq!((a.total_bytes(), a.newest()), (0, Timestamp::NONE));
    }
}
