#![allow(unsafe_code)]

use Ordering::{Acquire, Relaxed, Release, SeqCst};
#[cfg(loom)]
use loom::{cell::UnsafeCell, sync::Arc, sync::atomic::*};
#[cfg(not(loom))]
use std::{cell::UnsafeCell, sync::Arc, sync::atomic::*};

// Read-copy update with hazard pointer style reuse. A version is reused only at zero readers and
// never current.
struct Version<T> {
    readers: AtomicUsize,
    value: UnsafeCell<T>,
    older: *mut Version<T>,
}

struct Shared<T> {
    current: AtomicPtr<Version<T>>,
    made: AtomicPtr<Version<T>>,
}

// SAFETY: hands `&T` to many threads and drops `T` on whichever thread is last.
unsafe impl<T: Send + Sync> Send for Shared<T> {}
unsafe impl<T: Send + Sync> Sync for Shared<T> {}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        let mut at = self.made.load(Acquire);
        while !at.is_null() {
            // SAFETY: made by `Box::into_raw` in `publish`, and no handle is left.
            at = unsafe { Box::from_raw(at) }.older;
        }
    }
}

/// The writing end, one thread publishes at a time.
pub struct Publisher<T>(Arc<Shared<T>>);

/// A cloneable reading end for any thread.
pub struct Reader<T>(Arc<Shared<T>>);

impl<T> Clone for Reader<T> {
    fn clone(&self) -> Self {
        Reader(Arc::clone(&self.0))
    }
}

/// A published value, `value` at first.
pub fn published<T: Send + Sync>(value: T) -> (Publisher<T>, Reader<T>) {
    let null = || AtomicPtr::new(std::ptr::null_mut());
    let mut publisher = Publisher(Arc::new(Shared { current: null(), made: null() }));
    publisher.publish(value);
    let reader = publisher.reader();
    (publisher, reader)
}

impl<T: Send + Sync> Publisher<T> {
    /// Replace the value. A reader that began before reads the one before, whole.
    // A version is made only while every other is being read.
    #[allow(clippy::disallowed_methods)]
    pub fn publish(&mut self, value: T) {
        let (current, made) = (self.0.current.load(Acquire), self.0.made.load(Acquire));
        let mut at = made;
        // SAFETY: a version lives as long as the handles do.
        while let Some(version) = unsafe { at.as_ref() } {
            if at != current && version.readers.load(Acquire) == 0 {
                // SAFETY: not current and no reader counted in, and readers recheck `current`.
                unsafe { put(&version.value, value) };
                self.0.current.store(at, Release);
                // Dekker with `read`, the reader sees this address or the next publish sees its
                // count.
                return fence(SeqCst);
            }
            at = version.older;
        }
        let version =
            Version { readers: AtomicUsize::new(0), value: UnsafeCell::new(value), older: made };
        at = Box::into_raw(Box::new(version));
        self.0.made.store(at, Release);
        self.0.current.store(at, Release);
        fence(SeqCst);
    }

    pub fn reader(&self) -> Reader<T> {
        Reader(Arc::clone(&self.0))
    }
}

impl<T> Reader<T> {
    /// Call `read` on the current value without holding up a publish.
    pub fn read<R>(&self, read: impl FnOnce(&T) -> R) -> R {
        let version = loop {
            // SAFETY: a version lives as long as the handles do, and one is published.
            let version = unsafe { &*self.0.current.load(Acquire) };
            version.readers.fetch_add(1, Release);
            // Dekker with `publish`, count in, fence, then recheck `current`.
            fence(SeqCst);
            if std::ptr::eq(self.0.current.load(Acquire), version) {
                break version;
            }
            version.readers.fetch_sub(1, Release);
        };
        // SAFETY: counted in and still current, so the writer leaves it alone.
        let out = unsafe { peek(&version.value, read) };
        version.readers.fetch_sub(1, Release);
        out
    }
}

/// `N` words one thread writes and any reads without a lock, a seqlock odd while written.
pub struct Snapshot<const N: usize> {
    version: AtomicU64,
    words: [AtomicU64; N],
}

impl<const N: usize> Default for Snapshot<N> {
    fn default() -> Self {
        Snapshot { version: AtomicU64::new(0), words: std::array::from_fn(|_| AtomicU64::new(0)) }
    }
}

impl<const N: usize> Snapshot<N> {
    /// Replace the words. One writer at a time, the caller serializes any more.
    pub fn write(&self, words: [u64; N]) {
        let v = self.version.load(Relaxed);
        self.version.store(v.wrapping_add(1), Relaxed);
        fence(Release);
        self.words.iter().zip(words).for_each(|(at, w)| at.store(w, Relaxed));
        self.version.store(v.wrapping_add(2), Release);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Acquire)
    }

    pub fn read(&self) -> (u64, [u64; N]) {
        loop {
            let v = self.version.load(Acquire);
            let words = std::array::from_fn(|i| self.words[i].load(Relaxed));
            fence(Acquire);
            if v & 1 == 0 && self.version.load(Relaxed) == v {
                return (v, words);
            }
            #[cfg(loom)]
            loom::thread::yield_now();
            std::hint::spin_loop();
        }
    }
}

#[cfg(not(loom))]
unsafe fn put<T>(cell: &UnsafeCell<T>, value: T) {
    unsafe {
        *cell.get() = value;
    }
}
#[cfg(loom)]
unsafe fn put<T>(cell: &UnsafeCell<T>, value: T) {
    cell.with_mut(|p| *p = value);
}
#[cfg(not(loom))]
unsafe fn peek<T, R>(cell: &UnsafeCell<T>, read: impl FnOnce(&T) -> R) -> R {
    unsafe { read(&*cell.get()) }
}
#[cfg(loom)]
unsafe fn peek<T, R>(cell: &UnsafeCell<T>, read: impl FnOnce(&T) -> R) -> R {
    cell.with(|p| read(&*p))
}

#[cfg(all(test, not(loom)))]
// Test staging and threads.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn versions<T>(shared: &Shared<T>) -> usize {
        let (mut at, mut count) = (shared.made.load(Ordering::Acquire), 0);
        while let Some(version) = unsafe { at.as_ref() } {
            (at, count) = (version.older, count + 1);
        }
        count
    }

    #[test]
    fn a_reader_reads_what_was_published_last() {
        let (mut publisher, reader) = published(1u32);
        let get = |reader: &Reader<u32>| reader.read(|v| *v);
        assert_eq!(get(&reader), 1);
        publisher.publish(2);
        assert_eq!((get(&reader), get(&publisher.reader()), get(&reader.clone())), (2, 2, 2));
    }

    #[test]
    fn with_no_reader_reading_two_versions_serve_every_publish() {
        let (mut publisher, reader) = published(vec![0u32]);
        for k in 1..1000 {
            publisher.publish(vec![k; 3]);
            assert_eq!(reader.read(|v| v.len()), 3);
        }
        assert_eq!(versions(&publisher.0), 2, "the current one and the one before");
    }

    #[test]
    fn a_reader_in_a_read_keeps_its_version_and_the_writer_makes_another() {
        let (mut publisher, reader) = published(vec![1u32]);
        let seen = reader.read(|held| {
            publisher.publish(vec![2]);
            publisher.publish(vec![3]);
            held[0]
        });
        assert_eq!((seen, reader.read(|v| v[0])), (1, 3));
        assert_eq!(versions(&publisher.0), 3, "one reader at once and two");
        for k in 4..100 {
            publisher.publish(vec![k]);
        }
        assert_eq!(versions(&publisher.0), 3, "and the read version is used again");
    }

    #[test]
    fn a_snapshot_reads_the_last_write_and_moves_its_version_by_two() {
        let snap = Snapshot::<3>::default();
        assert_eq!(snap.read(), (0, [0; 3]));
        snap.write([1, 2, 3]);
        snap.write([4, 5, 6]);
        assert_eq!((snap.version(), snap.read()), (4, (4, [4, 5, 6])));
    }

    #[test]
    fn a_snapshot_read_on_another_thread_is_never_torn() {
        let snap = Arc::new(Snapshot::<4>::default());
        let writer = {
            let snap = Arc::clone(&snap);
            std::thread::spawn(move || (1..=100_000u64).for_each(|k| snap.write([k; 4])))
        };
        let mut last = 0;
        while !writer.is_finished() {
            let (_, w) = snap.read();
            assert!(w.iter().all(|x| *x == w[0]) && w[0] >= last, "{w:?} after {last}");
            last = w[0];
        }
        writer.join().unwrap();
        assert_eq!(snap.read().1, [100_000; 4]);
    }

    struct Counted(Arc<AtomicUsize>);
    impl Counted {
        fn new(alive: &Arc<AtomicUsize>) -> Counted {
            alive.fetch_add(1, Ordering::SeqCst);
            Counted(Arc::clone(alive))
        }
    }
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_value_is_dropped_when_its_version_is_taken_again_and_all_with_the_last_handle() {
        let alive = Arc::new(AtomicUsize::new(0));
        let (mut publisher, reader) = published(Counted::new(&alive));
        for _ in 0..10 {
            publisher.publish(Counted::new(&alive));
        }
        assert_eq!(alive.load(Ordering::SeqCst), 2, "the two versions hold one each");
        drop(publisher);
        assert_eq!(alive.load(Ordering::SeqCst), 2, "a reader still reads what the writer left");
        reader.read(|_| ());
        drop(reader);
        assert_eq!(alive.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn readers_on_threads_see_whole_values_in_order_and_versions_stay_few() {
        const READERS: usize = 4;
        let (mut publisher, reader) = published(vec![0u64; 16]);
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..READERS)
            .map(|_| {
                let (reader, stop) = (reader.clone(), Arc::clone(&stop));
                std::thread::spawn(move || {
                    let mut last = 0;
                    while !stop.load(Ordering::Acquire) {
                        let now = reader.read(|v| {
                            assert!(v.iter().all(|x| *x == v[0]), "torn: {v:?}");
                            v[0]
                        });
                        assert!(now >= last, "went back from {last} to {now}");
                        last = now;
                    }
                })
            })
            .collect();
        for k in 1..=200_000u64 {
            publisher.publish(vec![k; 16]);
        }
        stop.store(true, Ordering::Release);
        threads.into_iter().for_each(|t| t.join().expect("a reader"));
        assert!(versions(&publisher.0) <= READERS + 2, "{} versions", versions(&publisher.0));
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;

    fn whole(pair: &(u32, u32)) -> u32 {
        assert_eq!(pair.1, pair.0 * 7, "torn");
        pair.0
    }

    #[test]
    fn loom_a_reader_sees_whole_values_in_order_against_a_writer_that_reuses() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let (mut publisher, reader) = published((0u32, 0u32));
            let writer = loom::thread::spawn(move || {
                for k in 1..=3 {
                    publisher.publish((k, k * 7));
                }
            });
            let first = reader.read(whole);
            let second = reader.read(whole);
            assert!(second >= first, "went back from {first} to {second}");
            writer.join().unwrap();
            assert_eq!(reader.read(whole), 3);
        });
    }

    #[test]
    fn loom_two_readers_and_a_writer() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let (mut publisher, reader) = published((0u32, 0u32));
            let other = {
                let reader = reader.clone();
                loom::thread::spawn(move || reader.read(whole))
            };
            let writer = loom::thread::spawn(move || {
                publisher.publish((1, 7));
                publisher.publish((2, 14));
            });
            let mine = reader.read(whole);
            assert!(mine <= 2 && other.join().unwrap() <= 2);
            writer.join().unwrap();
            assert_eq!(reader.read(whole), 2);
        });
    }

    fn snapshot_writer<const N: usize>(
        snap: Arc<Snapshot<N>>,
        writes: u64,
    ) -> loom::thread::JoinHandle<()> {
        loom::thread::spawn(move || (1..=writes).for_each(|k| snap.write([k; N])))
    }

    #[test]
    fn loom_a_snapshot_read_is_one_whole_write() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(2);
        model.check(|| {
            let snap = Arc::new(Snapshot::<2>::default());
            let writer = snapshot_writer(Arc::clone(&snap), 2);
            let (_, [a, b]) = snap.read();
            assert_eq!(a, b, "torn");
            writer.join().unwrap();
            assert_eq!(snap.read(), (4, [2, 2]));
        });
    }

    #[test]
    fn loom_a_snapshot_never_goes_back() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(2);
        model.check(|| {
            let snap = Arc::new(Snapshot::<1>::default());
            let writer = snapshot_writer(Arc::clone(&snap), 2);
            let (v1, [a]) = snap.read();
            let (v2, [b]) = snap.read();
            assert!(b >= a && v2 >= v1 && v1 == 2 * a, "went back from {a} to {b}");
            writer.join().unwrap();
        });
    }

    #[test]
    fn loom_a_reader_that_saw_the_version_change_reads_what_came_with_it() {
        loom::model(|| {
            let snap = Arc::new(Snapshot::<2>::default());
            let writer = snapshot_writer(Arc::clone(&snap), 1);
            if snap.version() == 2 {
                assert_eq!(snap.read().1, [1, 1], "the version came before its words");
            }
            writer.join().unwrap();
        });
    }

    #[test]
    fn loom_the_last_handle_frees_what_was_published() {
        loom::model(|| {
            let (mut publisher, reader) = published(Arc::new(0u32));
            let writer = loom::thread::spawn(move || publisher.publish(Arc::new(1)));
            let seen = reader.read(|v| **v);
            assert!(seen <= 1);
            drop(reader);
            writer.join().unwrap();
        });
    }
}
