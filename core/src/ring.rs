// Slot access is the audited unsafe, model-checked with loom.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;

#[cfg(loom)]
use loom::cell::UnsafeCell;
#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
#[cfg(loom)]
use loom::sync::{Arc, Condvar, Mutex};

#[cfg(not(loom))]
use std::cell::UnsafeCell;
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
#[cfg(not(loom))]
use std::sync::{Arc, Condvar, Mutex};

#[repr(align(64))]
struct CachePadded<T>(T);

/// # Safety
/// Caller is the sole producer and the slot holds no live value.
#[cfg(not(loom))]
unsafe fn slot_write<T>(slot: &UnsafeCell<MaybeUninit<T>>, val: T) {
    unsafe {
        (*slot.get()).write(val);
    }
}
#[cfg(loom)]
unsafe fn slot_write<T>(slot: &UnsafeCell<MaybeUninit<T>>, val: T) {
    slot.with_mut(|p| (*p).write(val));
}

/// # Safety
/// Caller is the sole consumer and the slot holds a published value, read once.
#[cfg(not(loom))]
unsafe fn slot_read<T>(slot: &UnsafeCell<MaybeUninit<T>>) -> T {
    unsafe { (*slot.get()).assume_init_read() }
}
#[cfg(loom)]
unsafe fn slot_read<T>(slot: &UnsafeCell<MaybeUninit<T>>) -> T {
    slot.with(|p| (*p).assume_init_read())
}

/// # Safety
/// Caller has exclusive access and the slot holds an unconsumed value.
#[cfg(not(loom))]
unsafe fn slot_drop<T>(slot: &UnsafeCell<MaybeUninit<T>>) {
    unsafe {
        (*slot.get()).assume_init_drop();
    }
}
#[cfg(loom)]
unsafe fn slot_drop<T>(slot: &UnsafeCell<MaybeUninit<T>>) {
    slot.with_mut(|p| (*p).assume_init_drop());
}

/// Overflow policy of a ring's blocking push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaky {
    /// A full ring blocks the producer.
    Block,
    /// A full ring drops the incoming value and counts it.
    DropNewest,
}

struct Inner<T> {
    buf: Box<[UnsafeCell<MaybeUninit<T>>]>,
    cap: usize,
    mask: usize,
    leaky: Leaky,
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    dropped: AtomicUsize,
    closed: AtomicBool,
    consumer_waiting: AtomicBool,
    producer_waiting: AtomicBool,
    gate: Mutex<()>,
    cvar: Condvar,
}

// SAFETY: SPSC, each side touches only its own index and the slots it owns.
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> Inner<T> {
    fn is_empty(&self) -> bool {
        self.head.0.load(Ordering::Acquire) == self.tail.0.load(Ordering::Acquire)
    }

    fn is_full(&self) -> bool {
        let tail = self.tail.0.load(Ordering::Acquire);
        let head = self.head.0.load(Ordering::Acquire);
        tail.wrapping_sub(head) == self.cap
    }

    fn wake(&self) {
        // Under the gate, so a notify cannot land between a waiter's re-check and its wait.
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.cvar.notify_all();
    }

    fn close(&self, waiting: &AtomicBool) {
        // A parker flags, fences, then reads closed, so it sees the close or we see its flag.
        self.closed.store(true, Ordering::Release);
        fence(Ordering::SeqCst);
        if waiting.load(Ordering::Relaxed) {
            self.wake();
        }
    }
}

impl<T> Drop for Inner<T> {
    #[cfg(not(loom))]
    fn drop(&mut self) {
        let head = *self.head.0.get_mut();
        let tail = *self.tail.0.get_mut();
        let mut h = head;
        while h != tail {
            let idx = h & self.mask;
            // SAFETY: slots in [head, tail) are initialized and not yet consumed.
            unsafe {
                slot_drop(&self.buf[idx]);
            }
            h = h.wrapping_add(1);
        }
    }

    // Loom atomics have no get_mut and the models hold only usize, so nothing to drain.
    #[cfg(loom)]
    fn drop(&mut self) {}
}

/// The producing end, Send but not Sync.
pub struct Producer<T> {
    inner: Arc<Inner<T>>,
    cached_head: Cell<usize>,
    _not_sync: PhantomData<Cell<()>>,
}

/// The consuming end, Send but not Sync.
pub struct Consumer<T> {
    inner: Arc<Inner<T>>,
    cached_tail: Cell<usize>,
    _not_sync: PhantomData<Cell<()>>,
}

/// Creates a blocking ring of `capacity` slots, rounded up to a power of two.
pub fn spsc<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    spsc_leaky(capacity, Leaky::Block)
}

/// Creates a ring with an explicit overflow policy.
pub fn spsc_leaky<T>(capacity: usize, leaky: Leaky) -> (Producer<T>, Consumer<T>) {
    let cap = capacity.max(1).next_power_of_two();
    let buf = (0..cap)
        .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let inner = Arc::new(Inner {
        buf,
        cap,
        mask: cap - 1,
        leaky,
        head: CachePadded(AtomicUsize::new(0)),
        tail: CachePadded(AtomicUsize::new(0)),
        dropped: AtomicUsize::new(0),
        closed: AtomicBool::new(false),
        consumer_waiting: AtomicBool::new(false),
        producer_waiting: AtomicBool::new(false),
        gate: Mutex::new(()),
        cvar: Condvar::new(),
    });
    (
        Producer { inner: Arc::clone(&inner), cached_head: Cell::new(0), _not_sync: PhantomData },
        Consumer { inner, cached_tail: Cell::new(0), _not_sync: PhantomData },
    )
}

impl<T> Producer<T> {
    /// Slot count, the requested capacity rounded up to a power of two.
    pub fn capacity(&self) -> usize {
        self.inner.cap
    }

    /// Non-blocking push, handing the value back when full.
    pub fn try_push(&self, val: T) -> Result<(), T> {
        // Own index, so Relaxed.
        let tail = self.inner.tail.0.load(Ordering::Relaxed);

        // cached_head <= head, so a stale cache underestimates room. Acquire pairs with the
        // consumer's head Release.
        if tail.wrapping_sub(self.cached_head.get()) == self.inner.cap {
            let head = self.inner.head.0.load(Ordering::Acquire);
            self.cached_head.set(head);
            if tail.wrapping_sub(head) == self.inner.cap {
                return Err(val);
            }
        }

        let idx = tail & self.inner.mask;
        // SAFETY: the slot is past head and we are the only producer.
        unsafe {
            slot_write(&self.inner.buf[idx], val);
        }
        // Release publishes the slot write before the index that exposes it.
        self.inner.tail.0.store(tail.wrapping_add(1), Ordering::Release);

        // Dekker handshake with a parked consumer on every publish. Gating it on an empty edge
        // loses wakeups.
        fence(Ordering::SeqCst);
        if self.inner.consumer_waiting.load(Ordering::Relaxed) {
            self.inner.wake();
        }
        Ok(())
    }

    /// Blocking push, applying the ring's Leaky policy when full.
    pub fn push(&self, mut val: T) -> Result<(), T> {
        loop {
            match self.try_push(val) {
                Ok(()) => return Ok(()),
                Err(v) => {
                    val = v;
                    if self.inner.closed.load(Ordering::Acquire) {
                        return Err(val);
                    }
                    if self.inner.leaky == Leaky::DropNewest {
                        // Relaxed, a producer-owned tally that orders nothing.
                        self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                        drop(val);
                        return Ok(());
                    }
                    let mut guard = self.inner.gate.lock().unwrap();
                    self.inner.producer_waiting.store(true, Ordering::Relaxed);
                    fence(Ordering::SeqCst); // pairs with the consumer's pop fence
                    if self.inner.is_full() && !self.inner.closed.load(Ordering::Acquire) {
                        guard = self.inner.cvar.wait(guard).unwrap();
                    }
                    self.inner.producer_waiting.store(false, Ordering::Relaxed);
                    drop(guard);
                }
            }
        }
    }

    /// Values a DropNewest push has dropped so far.
    pub fn dropped(&self) -> usize {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Whether the consumer has been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Momentary queue length, may overestimate while the consumer drains.
    pub fn len(&self) -> usize {
        let tail = self.inner.tail.0.load(Ordering::Relaxed);
        let head = self.inner.head.0.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Consumer<T> {
    /// Slot count, the requested capacity rounded up to a power of two.
    pub fn capacity(&self) -> usize {
        self.inner.cap
    }

    /// Non-blocking pop, `None` when empty.
    pub fn try_pop(&self) -> Option<T> {
        // Own index, so Relaxed.
        let head = self.inner.head.0.load(Ordering::Relaxed);

        // cached_tail <= tail, so a stale cache underestimates items. Acquire pairs with the
        // producer's tail Release.
        if head == self.cached_tail.get() {
            let tail = self.inner.tail.0.load(Ordering::Acquire);
            self.cached_tail.set(tail);
            if head == tail {
                return None;
            }
        }

        let idx = head & self.inner.mask;
        // SAFETY: the slot is before tail and we are the only consumer.
        let val = unsafe { slot_read(&self.inner.buf[idx]) };
        // Release frees the slot only after it is fully read.
        self.inner.head.0.store(head.wrapping_add(1), Ordering::Release);

        // Dekker handshake with a parked producer, mirror of try_push.
        fence(Ordering::SeqCst);
        if self.inner.producer_waiting.load(Ordering::Relaxed) {
            self.inner.wake();
        }
        Some(val)
    }

    /// Whether the producer has been dropped, items may still be queued.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Names the ring, the same for its producer's lifetime.
    pub(crate) fn ring_id(&self) -> usize {
        Arc::as_ptr(&self.inner) as *const () as usize
    }

    /// Momentary queue length from the consumer side.
    pub fn len(&self) -> usize {
        let head = self.inner.head.0.load(Ordering::Relaxed);
        let tail = self.inner.tail.0.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Blocking pop, `None` once the producer is gone and the ring drained.
    pub fn pop(&self) -> Option<T> {
        loop {
            if let Some(v) = self.try_pop() {
                return Some(v);
            }
            if self.inner.closed.load(Ordering::Acquire) {
                // A push may have landed just before the close.
                return self.try_pop();
            }
            let mut guard = self.inner.gate.lock().unwrap();
            self.inner.consumer_waiting.store(true, Ordering::Relaxed);
            fence(Ordering::SeqCst); // pairs with the producer's push fence
            if self.inner.is_empty() && !self.inner.closed.load(Ordering::Acquire) {
                guard = self.inner.cvar.wait(guard).unwrap();
            }
            self.inner.consumer_waiting.store(false, Ordering::Relaxed);
            drop(guard);
        }
    }

    /// Blocking pop that also gives up once `timeout` elapses.
    // A blocking pop for app threads and tests. No pass calls it.
    #[allow(clippy::disallowed_methods)]
    pub fn pop_timeout(&self, timeout: std::time::Duration) -> Option<T> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(v) = self.try_pop() {
                return Some(v);
            }
            if self.inner.closed.load(Ordering::Acquire) {
                return self.try_pop();
            }
            let left = deadline.checked_duration_since(std::time::Instant::now())?;
            let mut guard = self.inner.gate.lock().unwrap();
            self.inner.consumer_waiting.store(true, Ordering::Relaxed);
            fence(Ordering::SeqCst); // pairs with the producer's push fence
            if self.inner.is_empty() && !self.inner.closed.load(Ordering::Acquire) {
                guard = self.inner.cvar.wait_timeout(guard, left).unwrap().0;
            }
            self.inner.consumer_waiting.store(false, Ordering::Relaxed);
            drop(guard);
        }
    }
}

impl<T> Drop for Producer<T> {
    fn drop(&mut self) {
        self.inner.close(&self.inner.consumer_waiting);
    }
}

impl<T> Drop for Consumer<T> {
    fn drop(&mut self) {
        self.inner.close(&self.inner.producer_waiting);
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn single_thread_fifo_and_full() {
        let (p, c) = spsc::<u32>(4);
        assert!(c.try_pop().is_none(), "starts empty");
        for i in 0..4 {
            assert!(p.try_push(i).is_ok());
        }
        assert_eq!(p.try_push(99), Err(99), "full at capacity");
        for i in 0..4 {
            assert_eq!(c.try_pop(), Some(i), "FIFO");
        }
        assert!(c.try_pop().is_none());
    }

    #[test]
    fn wraps_around() {
        let (p, c) = spsc::<usize>(2);
        for round in 0..1000 {
            assert!(p.try_push(round).is_ok());
            assert_eq!(c.try_pop(), Some(round));
        }
    }

    #[test]
    fn blocking_transfer_across_threads() {
        const N: usize = 1_000_000;
        let (p, c) = spsc::<usize>(64);
        let producer = std::thread::spawn(move || {
            for i in 0..N {
                p.push(i).expect("consumer alive");
            }
        });
        let consumer = std::thread::spawn(move || {
            for expected in 0..N {
                let got = c.pop().expect("producer alive until N");
                assert_eq!(got, expected, "FIFO preserved, nothing lost");
            }
            assert!(c.pop().is_none(), "closed and drained");
        });
        producer.join().unwrap();
        consumer.join().unwrap();
    }

    #[test]
    // A test's own fixtures and timing, not an element.
    #[allow(clippy::disallowed_methods)]
    fn timed_pop_gives_up_and_still_sees_a_late_push_and_a_close() {
        let (p, c) = spsc::<usize>(4);
        let at = std::time::Instant::now();
        assert_eq!(c.pop_timeout(std::time::Duration::from_millis(30)), None);
        assert!(at.elapsed() >= std::time::Duration::from_millis(25), "it actually waited");
        assert!(!c.is_closed(), "a timeout is not a close");

        let pusher = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            p.push(7).expect("consumer alive");
        });
        let at = std::time::Instant::now();
        assert_eq!(c.pop_timeout(std::time::Duration::from_secs(10)), Some(7));
        assert!(
            at.elapsed() < std::time::Duration::from_secs(5),
            "woke on the push, not the timeout"
        );
        pusher.join().unwrap();

        assert_eq!(c.pop_timeout(std::time::Duration::from_secs(10)), None);
        assert!(c.is_closed());
    }

    #[test]
    fn tiny_capacity_stress() {
        const N: usize = 100_000;
        let (p, c) = spsc::<usize>(1);
        let prod = std::thread::spawn(move || {
            for i in 0..N {
                p.push(i).expect("consumer alive");
            }
        });
        let cons = std::thread::spawn(move || {
            for expected in 0..N {
                assert_eq!(c.pop(), Some(expected));
            }
            assert!(c.pop().is_none());
        });
        prod.join().unwrap();
        cons.join().unwrap();
    }

    #[test]
    fn producer_drop_closes_after_drain() {
        let (p, c) = spsc::<i32>(8);
        p.push(1).unwrap();
        p.push(2).unwrap();
        drop(p);
        assert_eq!(c.pop(), Some(1));
        assert_eq!(c.pop(), Some(2));
        assert_eq!(c.pop(), None, "drained + closed");
    }

    #[test]
    fn consumer_drop_unblocks_producer() {
        let (p, c) = spsc::<i32>(1);
        p.push(1).unwrap();
        drop(c);
        assert_eq!(p.push(2), Err(2), "consumer gone, push fails instead of hanging");
    }

    #[test]
    fn drop_releases_undelivered_elements() {
        let (p, c) = spsc::<Box<u64>>(8);
        p.try_push(Box::new(1)).unwrap();
        p.try_push(Box::new(2)).unwrap();
        drop(p);
        drop(c);
    }

    #[test]
    fn leaky_drop_newest_counts_and_never_blocks() {
        let (p, _c) = spsc_leaky::<usize>(2, Leaky::DropNewest);
        assert_eq!(p.dropped(), 0);
        p.push(0).unwrap();
        p.push(1).unwrap();
        p.push(2).unwrap();
        p.push(3).unwrap();
        assert_eq!(p.dropped(), 2, "two overflow values dropped and counted");
    }

    #[test]
    fn leaky_drop_newest_keeps_the_oldest_in_order() {
        let (p, c) = spsc_leaky::<usize>(2, Leaky::DropNewest);
        for v in 0..5 {
            p.push(v).unwrap();
        }
        assert_eq!(p.dropped(), 3);
        assert_eq!(c.try_pop(), Some(0));
        assert_eq!(c.try_pop(), Some(1));
        assert_eq!(c.try_pop(), None, "only the first two survived");
    }

    #[test]
    fn block_default_has_zero_drops() {
        let (p, c) = spsc::<usize>(2);
        p.push(0).unwrap();
        assert_eq!(c.try_pop(), Some(0));
        assert_eq!(p.dropped(), 0);
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;

    #[test]
    fn spsc_fifo_no_loss_no_dup() {
        const N: usize = 3;

        loom::model(|| {
            let (p, c) = spsc::<usize>(2);

            let producer = loom::thread::spawn(move || {
                for i in 0..N {
                    while p.try_push(i).is_err() {
                        loom::thread::yield_now();
                    }
                }
            });

            let mut received = Vec::with_capacity(N);
            while received.len() < N {
                if let Some(v) = c.try_pop() {
                    received.push(v);
                } else {
                    loom::thread::yield_now();
                }
            }

            producer.join().unwrap();

            let expected: Vec<usize> = (0..N).collect();
            assert_eq!(received, expected, "FIFO order preserved, nothing lost or duplicated");

            assert!(c.try_pop().is_none(), "nothing left after all items consumed");
        });
    }

    #[test]
    fn no_lost_wakeup_on_empty_edge() {
        loom::model(|| {
            let (p, c) = spsc::<usize>(1);
            let inner_p = Arc::clone(&p.inner);
            let inner_c = Arc::clone(&c.inner);

            let producer = loom::thread::spawn(move || {
                // try_push's publish, fence and flag load.
                inner_p.tail.0.store(1, Ordering::Release);
                fence(Ordering::SeqCst);
                inner_p.consumer_waiting.load(Ordering::Relaxed)
            });

            let consumer = loom::thread::spawn(move || {
                if inner_c.head.0.load(Ordering::Relaxed) == inner_c.tail.0.load(Ordering::Acquire)
                {
                    // pop's park decision. The flag stays set while parked.
                    inner_c.consumer_waiting.store(true, Ordering::Relaxed);
                    fence(Ordering::SeqCst);
                    let still_empty = inner_c.is_empty();
                    if !still_empty {
                        inner_c.consumer_waiting.store(false, Ordering::Relaxed);
                    }
                    still_empty
                } else {
                    false
                }
            });

            let producer_saw_waiter = producer.join().unwrap();
            let consumer_would_sleep = consumer.join().unwrap();

            assert!(
                !(consumer_would_sleep && !producer_saw_waiter),
                "lost wakeup: consumer parked on empty while producer skipped the wake",
            );
            assert_eq!(c.inner.tail.0.load(Ordering::Acquire), 1, "item was published");
        });
    }

    #[test]
    fn no_lost_wakeup_on_full_edge() {
        loom::model(|| {
            let (p, c) = spsc::<usize>(1);
            p.try_push(7).expect("empty ring has room");
            let inner_p = Arc::clone(&p.inner);
            let inner_c = Arc::clone(&c.inner);

            let consumer = loom::thread::spawn(move || {
                // try_pop's free, fence and flag load.
                inner_c.head.0.store(1, Ordering::Release);
                fence(Ordering::SeqCst);
                inner_c.producer_waiting.load(Ordering::Relaxed)
            });

            let producer = loom::thread::spawn(move || {
                if inner_p.is_full() {
                    // push's park decision. The flag stays set while parked.
                    inner_p.producer_waiting.store(true, Ordering::Relaxed);
                    fence(Ordering::SeqCst);
                    let still_full = inner_p.is_full();
                    if !still_full {
                        inner_p.producer_waiting.store(false, Ordering::Relaxed);
                    }
                    still_full
                } else {
                    false
                }
            });

            let consumer_saw_waiter = consumer.join().unwrap();
            let producer_would_sleep = producer.join().unwrap();

            assert!(
                !(producer_would_sleep && !consumer_saw_waiter),
                "lost wakeup: producer parked on full while consumer skipped the wake",
            );
        });
    }

    #[test]
    fn a_close_wakes_a_consumer_parked_on_an_empty_ring() {
        loom::model(|| {
            let (p, c) = spsc::<usize>(1);
            let consumer = loom::thread::spawn(move || c.pop());
            drop(p);
            assert_eq!(consumer.join().unwrap(), None, "closed and drained");
        });
    }

    #[test]
    fn a_close_wakes_a_producer_parked_on_a_full_ring() {
        loom::model(|| {
            let (p, c) = spsc::<usize>(1);
            p.try_push(1).expect("empty ring has room");
            let producer = loom::thread::spawn(move || p.push(2));
            drop(c);
            assert_eq!(producer.join().unwrap(), Err(2), "nobody is left to drain it");
        });
    }

    #[test]
    fn leaky_drop_newest_conserves_items() {
        const N: usize = 3;
        loom::model(|| {
            let (p, c) = spsc_leaky::<usize>(1, Leaky::DropNewest);

            let producer = loom::thread::spawn(move || {
                for i in 0..N {
                    p.push(i).expect("consumer alive");
                }
                p.dropped()
            });

            let mut received = Vec::new();
            loop {
                match c.try_pop() {
                    Some(v) => received.push(v),
                    None => {
                        if c.is_closed() {
                            while let Some(v) = c.try_pop() {
                                received.push(v);
                            }
                            break;
                        }
                        loom::thread::yield_now();
                    }
                }
            }
            let dropped = producer.join().unwrap();

            assert_eq!(received.len() + dropped, N, "every item delivered or counted");
            for w in received.windows(2) {
                assert!(w[0] < w[1], "delivered items keep FIFO order, no duplicates");
            }
        });
    }
}
