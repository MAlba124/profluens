use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::error::Error;
use crate::event::TagList;
use crate::format::FixedFormat;
use crate::id::{ElementId, GroupId, LinkId, SinkPad, SrcPad};
use crate::level::{Fill, Held};
use crate::time::Timestamp;

/// A message from the pipeline to the application.
pub enum BusMessage {
    Error {
        element: ElementId,
        error: Error,
    },
    Warning {
        element: ElementId,
        error: Error,
    },
    Eos,
    StateChanged {
        old: State,
        new: State,
    },
    SrcPadAdded {
        element: ElementId,
        pad: SrcPad,
        format: FixedFormat,
    },
    SinkPadAdded {
        element: ElementId,
        pad: SinkPad,
        format: FixedFormat,
    },
    ElementAdded {
        element: ElementId,
        group: GroupId,
    },
    ElementRemoved {
        element: ElementId,
    },
    LinkChanged {
        link: LinkId,
    },
    Tags {
        element: ElementId,
        tags: TagList,
    },
    SubgraphJoined {
        group: GroupId,
        added_latency: Timestamp,
    },
    LatencyChanged {
        old: Timestamp,
        new: Timestamp,
    },
    /// The pipeline kept its latency because a change asked for more than it allows.
    LatencyHeld {
        kept: Timestamp,
        asked: Timestamp,
    },
    Qos {
        sink: ElementId,
        lateness_ns: i64,
    },
    /// A failed branch was isolated.
    BranchSealed {
        group: GroupId,
        error: Error,
    },
    /// A demuxer learned the presentation duration.
    DurationChanged {
        element: ElementId,
        ns: u64,
    },
    /// `element` published its index, a seek planned on it is no longer deferred.
    IndexPublished {
        element: ElementId,
    },
    /// Topology change `change` is in the log.
    Changed {
        change: u32,
    },
    /// Topology change `change` failed before its splice and was taken back.
    ChangeFailed {
        change: u32,
        error: Error,
    },
    /// The store ahead of `element`'s reader crossed its low or high mark, or the link into its
    /// `pad`.
    Buffering {
        element: ElementId,
        pad: Option<SinkPad>,
        fill: Fill,
        held: Held,
    },
    /// An inserter added `element` to convert, fed by `link`.
    Inserted {
        element: ElementId,
        link: LinkId,
    },
}

/// Drop class of a bus message. `Critical` never drops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageClass {
    Droppable,
    Critical,
}

impl BusMessage {
    pub fn class(&self) -> MessageClass {
        match self {
            BusMessage::Warning { .. } | BusMessage::Qos { .. } | BusMessage::Tags { .. } => {
                MessageClass::Droppable
            }
            BusMessage::Error { .. }
            | BusMessage::Eos
            | BusMessage::StateChanged { .. }
            | BusMessage::SrcPadAdded { .. }
            | BusMessage::SinkPadAdded { .. }
            | BusMessage::ElementAdded { .. }
            | BusMessage::ElementRemoved { .. }
            | BusMessage::LinkChanged { .. }
            | BusMessage::SubgraphJoined { .. }
            | BusMessage::LatencyChanged { .. }
            | BusMessage::LatencyHeld { .. }
            | BusMessage::BranchSealed { .. }
            | BusMessage::Changed { .. }
            | BusMessage::ChangeFailed { .. }
            | BusMessage::Buffering { .. }
            | BusMessage::Inserted { .. }
            | BusMessage::DurationChanged { .. }
            | BusMessage::IndexPublished { .. } => MessageClass::Critical,
        }
    }
}

/// Pipeline state. Pause is a clock op, not a state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Stopped,
    Ready,
    Playing,
}

pub const DEFAULT_CAPACITY: usize = 1024;

struct Shared {
    inner: Mutex<Inner>,
    ready: Condvar,
    /// Rung after a post, for an owner that sleeps elsewhere than in `recv`.
    bell: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

struct Inner {
    queue: VecDeque<BusMessage>,
    capacity: usize,
    senders: usize,
    // Parked receivers, under the lock so a skipped notify loses no wake.
    waiting: usize,
    droppable: usize,
}

/// Whether `new` supersedes `old`, the latest state of one pad or element.
fn supersedes(old: &BusMessage, new: &BusMessage) -> bool {
    match (old, new) {
        (
            BusMessage::Buffering { element: a, pad: p, .. },
            BusMessage::Buffering { element: b, pad: q, .. },
        ) => a == b && p == q,
        (
            BusMessage::DurationChanged { element: a, .. },
            BusMessage::DurationChanged { element: b, .. },
        )
        | (BusMessage::IndexPublished { element: a }, BusMessage::IndexPublished { element: b }) => {
            a == b
        }
        _ => false,
    }
}

impl Inner {
    /// Post without blocking. Returns how many messages were dropped (0 or 1).
    fn post(&mut self, msg: BusMessage) -> u64 {
        if self.queue.len() < self.capacity {
            self.push(msg);
            return 0;
        }
        if self.evict_oldest_droppable() {
            self.push(msg);
            return 1;
        }
        if msg.class() == MessageClass::Droppable {
            return 1;
        }
        // Past capacity with nothing to drop, a newer state replaces the queued one.
        match self.queue.iter().position(|old| supersedes(old, &msg)) {
            Some(at) => {
                self.queue[at] = msg;
                1
            }
            None => {
                self.push(msg);
                0
            }
        }
    }

    fn push(&mut self, msg: BusMessage) {
        self.droppable += (msg.class() == MessageClass::Droppable) as usize;
        self.queue.push_back(msg);
    }

    fn pop(&mut self) -> Option<BusMessage> {
        let msg = self.queue.pop_front()?;
        self.droppable -= (msg.class() == MessageClass::Droppable) as usize;
        Some(msg)
    }

    fn evict_oldest_droppable(&mut self) -> bool {
        if self.droppable == 0 {
            return false;
        }
        if let Some(i) = self.queue.iter().position(|m| m.class() == MessageClass::Droppable) {
            self.queue.remove(i);
            self.droppable -= 1;
            true
        } else {
            false
        }
    }
}

pub struct BusSender {
    shared: Arc<Shared>,
    dropped: Arc<AtomicU64>,
}

impl Clone for BusSender {
    fn clone(&self) -> Self {
        self.shared.inner.lock().unwrap().senders += 1;
        BusSender { shared: self.shared.clone(), dropped: self.dropped.clone() }
    }
}

impl BusSender {
    /// Post a message without blocking, dropping by class when full.
    pub fn send(&self, msg: BusMessage) {
        let mut inner = self.shared.inner.lock().unwrap();
        let dropped = inner.post(msg);
        let waiting = inner.waiting > 0;
        drop(inner);
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        if waiting {
            self.shared.ready.notify_one();
        }
        if let Some(ring) = self.shared.bell.get() {
            ring();
        }
    }
}

impl Drop for BusSender {
    fn drop(&mut self) {
        let mut inner = self.shared.inner.lock().unwrap();
        inner.senders -= 1;
        let last = inner.senders == 0;
        let waiting = last && inner.waiting > 0;
        drop(inner);
        if waiting {
            self.shared.ready.notify_all();
        }
        if last && let Some(ring) = self.shared.bell.get() {
            ring();
        }
    }
}

/// The bus already has a bell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BellSet;

impl core::fmt::Display for BellSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the bus already has a bell")
    }
}

impl core::error::Error for BellSet {}

/// The receiving half of the bus. A clone drains the same queue.
#[derive(Clone)]
pub struct Bus {
    shared: Arc<Shared>,
    dropped: Arc<AtomicU64>,
}

impl Bus {
    pub fn channel() -> (BusSender, Bus) {
        Self::channel_with_capacity(DEFAULT_CAPACITY)
    }

    pub fn channel_with_capacity(capacity: usize) -> (BusSender, Bus) {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                capacity: capacity.max(1),
                senders: 1,
                waiting: 0,
                droppable: 0,
            }),
            ready: Condvar::new(),
            bell: OnceLock::new(),
        });
        let dropped = Arc::new(AtomicU64::new(0));
        (BusSender { shared: shared.clone(), dropped: dropped.clone() }, Bus { shared, dropped })
    }

    pub fn try_recv(&self) -> Option<BusMessage> {
        self.shared.inner.lock().unwrap().pop()
    }

    /// Rings `bell` after every post and when the last sender leaves, once per bus. An owner
    /// that sleeps on a socket or an eventfd wakes by it and reads with `try_recv`.
    pub fn set_bell(&self, bell: Arc<dyn Fn() + Send + Sync>) -> Result<(), BellSet> {
        self.shared.bell.set(bell).map_err(|_| BellSet)
    }

    /// Block for the oldest message, `None` once every sender is gone and the queue drained.
    pub fn recv(&self) -> Option<BusMessage> {
        let mut inner = self.shared.inner.lock().unwrap();
        loop {
            if let Some(msg) = inner.pop() {
                return Some(msg);
            }
            if inner.senders == 0 {
                return None;
            }
            inner.waiting += 1;
            inner = self.shared.ready.wait(inner).unwrap();
            inner.waiting -= 1;
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn err() -> Error {
        Error::GroupPanicked { element: ElementId(0), message: String::new() }
    }

    fn warn(n: i64) -> BusMessage {
        BusMessage::Qos { sink: ElementId(0), lateness_ns: n }
    }

    fn critical(n: i64) -> BusMessage {
        BusMessage::LatencyChanged { old: Timestamp::ZERO, new: Timestamp::from_nanos(n as u64) }
    }

    fn qos_marker(m: &BusMessage) -> Option<i64> {
        match m {
            BusMessage::Qos { lateness_ns, .. } => Some(*lateness_ns),
            _ => None,
        }
    }

    fn crit_marker(m: &BusMessage) -> Option<u64> {
        match m {
            BusMessage::LatencyChanged { new, .. } => new.nanos(),
            _ => None,
        }
    }

    fn buffering(pad: u32, fill: Fill) -> BusMessage {
        let (element, pad, held) = (ElementId(1), Some(SinkPad(pad)), Held::default());
        BusMessage::Buffering { element, pad, fill, held }
    }

    #[test]
    fn a_full_queue_keeps_the_latest_state_of_a_pad_and_counts_what_it_drops() {
        let (tx, rx) = Bus::channel_with_capacity(2);
        tx.send(buffering(0, Fill::Low));
        tx.send(buffering(1, Fill::Low));
        tx.send(buffering(0, Fill::High));
        tx.send(buffering(0, Fill::Low));
        tx.send(critical(7));
        assert_eq!(rx.dropped(), 2, "two states of pad 0 were superseded");
        let fills: Vec<Option<Fill>> = std::iter::from_fn(|| rx.try_recv())
            .map(|m| match m {
                BusMessage::Buffering { fill, .. } => Some(fill),
                _ => None,
            })
            .collect();
        assert_eq!(fills, [Some(Fill::Low), Some(Fill::Low), None]);
        tx.send(warn(1));
        tx.send(warn(2));
        tx.send(critical(1));
        tx.send(critical(2));
        tx.send(critical(3));
        assert_eq!(rx.dropped(), 4, "the warnings went first, then nothing");
        let rest: Vec<u64> =
            std::iter::from_fn(|| rx.try_recv()).filter_map(|m| crit_marker(&m)).collect();
        assert_eq!(rest, [1, 2, 3], "a distinct critical message is never lost");
    }

    #[test]
    fn classification_table() {
        use MessageClass::{Critical, Droppable};
        let cases: &[(BusMessage, MessageClass)] = &[
            (BusMessage::Error { element: ElementId(0), error: err() }, Critical),
            (BusMessage::Warning { element: ElementId(0), error: err() }, Droppable),
            (BusMessage::Eos, Critical),
            (BusMessage::StateChanged { old: State::Ready, new: State::Playing }, Critical),
            (
                BusMessage::SrcPadAdded {
                    element: ElementId(0),
                    pad: SrcPad(0),
                    format: FixedFormat::new(crate::id::FormatId(0)),
                },
                Critical,
            ),
            (
                BusMessage::SinkPadAdded {
                    element: ElementId(0),
                    pad: SinkPad(0),
                    format: FixedFormat::new(crate::id::FormatId(0)),
                },
                Critical,
            ),
            (BusMessage::ElementAdded { element: ElementId(0), group: GroupId(0) }, Critical),
            (BusMessage::ElementRemoved { element: ElementId(0) }, Critical),
            (BusMessage::LinkChanged { link: LinkId(0) }, Critical),
            (
                BusMessage::SubgraphJoined { group: GroupId(0), added_latency: Timestamp::ZERO },
                Critical,
            ),
            (BusMessage::LatencyChanged { old: Timestamp::ZERO, new: Timestamp::ZERO }, Critical),
            (BusMessage::Qos { sink: ElementId(0), lateness_ns: 0 }, Droppable),
            (BusMessage::BranchSealed { group: GroupId(0), error: err() }, Critical),
        ];
        for (msg, want) in cases {
            assert_eq!(msg.class(), *want);
        }
    }

    #[test]
    fn droppable_drop_oldest_with_exact_counter() {
        let (tx, bus) = Bus::channel_with_capacity(4);
        for i in 0..10 {
            tx.send(warn(i));
        }
        assert_eq!(bus.dropped(), 6, "exact drop count");

        let mut got = Vec::new();
        while let Some(m) = bus.try_recv() {
            got.push(qos_marker(&m).unwrap());
        }
        assert_eq!(got, vec![6, 7, 8, 9]);
    }

    #[test]
    fn critical_never_lost_evicts_oldest_droppable() {
        let (tx, bus) = Bus::channel_with_capacity(3);
        tx.send(warn(1));
        tx.send(warn(2));
        tx.send(warn(3));
        tx.send(critical(100));
        assert_eq!(bus.dropped(), 1, "one droppable evicted for the critical");

        let a = bus.recv().unwrap();
        let b = bus.recv().unwrap();
        let c = bus.recv().unwrap();
        assert_eq!(qos_marker(&a), Some(2));
        assert_eq!(qos_marker(&b), Some(3));
        assert_eq!(crit_marker(&c), Some(100));
        assert!(bus.try_recv().is_none());
    }

    #[test]
    fn critical_grows_past_cap_when_no_droppable_to_evict() {
        let (tx, bus) = Bus::channel_with_capacity(2);
        tx.send(critical(1));
        tx.send(critical(2));
        tx.send(critical(3));
        assert_eq!(bus.dropped(), 0, "no critical is ever dropped");
        let mut got = Vec::new();
        while let Some(m) = bus.try_recv() {
            got.push(crit_marker(&m).unwrap());
        }
        assert_eq!(got, vec![1, 2, 3], "all critical survive, in order");
    }

    #[test]
    fn droppable_into_all_critical_full_bus_drops_itself() {
        let (tx, bus) = Bus::channel_with_capacity(2);
        tx.send(critical(1));
        tx.send(critical(2));
        tx.send(warn(9));
        assert_eq!(bus.dropped(), 1);
        let a = bus.recv().unwrap();
        let b = bus.recv().unwrap();
        assert_eq!(crit_marker(&a), Some(1));
        assert_eq!(crit_marker(&b), Some(2));
        assert!(bus.try_recv().is_none(), "the droppable did not sneak in");
    }

    #[test]
    fn ordering_preserved_for_survivors_mixed() {
        let (tx, bus) = Bus::channel_with_capacity(4);
        tx.send(critical(10));
        tx.send(warn(1));
        tx.send(critical(20));
        tx.send(warn(2));
        tx.send(warn(3));
        tx.send(warn(4));
        assert_eq!(bus.dropped(), 2);

        let mut crits = Vec::new();
        let mut drops = Vec::new();
        while let Some(m) = bus.try_recv() {
            if let Some(c) = crit_marker(&m) {
                crits.push(c);
            } else if let Some(d) = qos_marker(&m) {
                drops.push(d);
            }
        }
        assert_eq!(crits, vec![10, 20], "criticals kept, in order");
        assert_eq!(drops, vec![3, 4], "newest droppables kept, in order");
    }

    #[test]
    fn recv_blocks_and_wakes_across_threads() {
        let (tx, bus) = Bus::channel_with_capacity(8);
        let woke = Arc::new(AtomicU64::new(0));
        let woke2 = woke.clone();
        let bus = Arc::new(bus);
        let bus2 = bus.clone();

        let h = thread::spawn(move || {
            let m = bus2.recv().expect("a message arrives");
            assert_eq!(crit_marker(&m), Some(42));
            woke2.fetch_add(1, Ordering::SeqCst);
        });

        thread::yield_now();
        tx.send(critical(42));
        h.join().unwrap();
        assert_eq!(woke.load(Ordering::SeqCst), 1);
    }

    fn parked_receiver(bus: Bus) -> std::sync::mpsc::Receiver<Option<u64>> {
        let (bus, (done, result)) = (Arc::new(bus), std::sync::mpsc::channel());
        let rx = bus.clone();
        thread::spawn(move || done.send(rx.recv().and_then(|m| crit_marker(&m))));
        while bus.shared.inner.lock().unwrap().waiting == 0 {
            thread::yield_now();
        }
        result
    }

    #[test]
    fn the_bell_rings_on_a_post_and_when_the_last_sender_leaves_once_set() {
        let (tx, bus) = Bus::channel_with_capacity(8);
        let rings = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let r = Arc::clone(&rings);
        let bell = Arc::new(move || {
            r.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(bus.set_bell(bell), Ok(()));
        assert_eq!(bus.set_bell(Arc::new(|| {})), Err(BellSet), "one bell per bus");
        let tx2 = tx.clone();
        tx.send(critical(1));
        tx2.send(warn(2));
        assert_eq!(rings.load(Ordering::Relaxed), 2, "once per post, whatever the class");
        drop(tx);
        assert_eq!(rings.load(Ordering::Relaxed), 2, "a sender leaving with another left is quiet");
        drop(tx2);
        assert_eq!(rings.load(Ordering::Relaxed), 3, "the last one leaving rings");
        assert_eq!(bus.try_recv().map(|m| m.class()), Some(MessageClass::Critical));
    }

    #[test]
    fn a_post_wakes_a_receiver_that_is_parked() {
        let (tx, bus) = Bus::channel_with_capacity(8);
        let result = parked_receiver(bus);
        tx.send(critical(7));
        let woke = result.recv_timeout(std::time::Duration::from_secs(20));
        assert_eq!(woke, Ok(Some(7)), "the parked receiver was not woken by the post");
        assert_eq!(tx.shared.inner.lock().unwrap().waiting, 0, "and it is counted out again");
    }

    #[test]
    fn the_last_sender_leaving_wakes_a_receiver_that_is_parked() {
        let (tx, bus) = Bus::channel_with_capacity(8);
        let tx2 = tx.clone();
        let result = parked_receiver(bus);
        drop(tx);
        let early = result.recv_timeout(std::time::Duration::from_millis(50));
        assert!(early.is_err(), "a sender is left, so the receiver stays parked");
        drop(tx2);
        let woke = result.recv_timeout(std::time::Duration::from_secs(20));
        assert_eq!(woke, Ok(None), "the parked receiver did not see the disconnect");
    }

    #[test]
    fn recv_returns_none_when_all_senders_drop() {
        let (tx, bus) = Bus::channel_with_capacity(4);
        let tx2 = tx.clone();
        drop(tx);
        drop(tx2);
        assert!(bus.recv().is_none());
    }

    #[test]
    fn recv_drains_queued_before_reporting_disconnect() {
        let (tx, bus) = Bus::channel_with_capacity(4);
        tx.send(critical(1));
        tx.send(critical(2));
        drop(tx);
        assert_eq!(crit_marker(&bus.recv().unwrap()), Some(1));
        assert_eq!(crit_marker(&bus.recv().unwrap()), Some(2));
        assert!(bus.recv().is_none());
    }

    #[test]
    fn send_after_bus_dropped_is_a_noop() {
        let (tx, bus) = Bus::channel_with_capacity(4);
        drop(bus);
        tx.send(critical(1));
        tx.send(warn(1));
    }
}
