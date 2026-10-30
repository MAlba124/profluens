#![allow(unsafe_code)]

use std::ffi::CStr;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, OnceLock, Weak};

#[cfg(loom)]
use loom::cell::UnsafeCell;
#[cfg(loom)]
use loom::sync::atomic::{
    AtomicBool as Busy, AtomicU32 as Next, AtomicU64 as Word, fence as handshake,
};
#[cfg(not(loom))]
use std::cell::UnsafeCell;
#[cfg(not(loom))]
use std::sync::atomic::{
    AtomicBool as Busy, AtomicU32 as Next, AtomicU64 as Word, fence as handshake,
};

use crate::station::{Starved, Station};

/// Classes of 64, 128 and 256 bytes, then four per doubling to 16 MiB.
const SMALL_CLASSES: usize = 67;
pub const SIZED_TOP: usize = 16 << 20;
/// The largest exact buffer served. Elements size them from file fields, so a hostile length
/// is refused instead of mapped.
pub const EXACT_MAX: usize = 256 << 20;
const CLASS_REGION_BYTES: usize = 256 * 1024;
const CLASS_FIRST_SLOTS: u32 = 32;
/// The least class a reserved one serves in place of its own.
pub const RESERVE_FLOOR: usize = CLASS_REGION_BYTES / 4;
/// Slots a main region maps at a time, at most, within [`REGION_BYTES`].
pub const REGION_SLOTS: u32 = 8;
/// Bytes a main region maps at a time, at most.
pub const REGION_BYTES: usize = 16 << 20;
/// Bytes a main region of small slots maps at least.
pub const MIN_REGION_BYTES: usize = 64 << 10;
/// Slots a main region of small slots maps at most.
pub const SMALL_REGION_SLOTS: u32 = 64;
/// Acquires between the takes that keep the high water within its bound.
const TAKE_SPAN: u32 = 32;
/// Charge bit of a sized slot counted under the count cap.
const COUNTED: u32 = 1 << 31;
/// Read bytes a pool's reactor credits keep in flight, see [`Pool::io_depth`].
pub const IO_DEPTH_BYTES: u64 = 8 << 20;
/// The least alignment of a slot, one cache line.
pub const MIN_ALIGN: usize = 64;
const END: u32 = u32::MAX;
/// Return-stack top of a region the pool let go of.
const CLOSED: u32 = u32::MAX - 1;
/// Return-stack word, an acquirer waits for the next release.
const WANT: u64 = 1 << 32;
/// Return-stack word, one slot pushed since the last take, 15 bits.
const PUSHED: u64 = 1 << 33;
/// Return-stack word, one of the pushed counted under the cap, 15 bits.
const PUSHED_COUNTED: u64 = 1 << 48;
const TOP: u64 = u32::MAX as u64;
/// State-word bit set once the pool has let go of the region.
const DETACHED: u64 = 1 << 63;

/// Slots a main region of `stride` maps.
pub fn region_slots(stride: usize) -> u32 {
    let stride = stride.max(1);
    let big = (REGION_BYTES / stride).clamp(1, REGION_SLOTS as usize);
    let small = (MIN_REGION_BYTES / stride).min(SMALL_REGION_SLOTS as usize);
    big.max(small) as u32
}

fn small_class(n: usize) -> Option<(usize, usize)> {
    if n <= 256 {
        let size = n.max(64).next_power_of_two();
        return Some((size.trailing_zeros() as usize - 6, size));
    }
    let e = (n - 1).ilog2();
    let step = 1 << (e - 2);
    let k = (n - (1 << e)).div_ceil(step);
    let class = 2 + (e as usize - 8) * 4 + k;
    (class < SMALL_CLASSES).then_some((class, (1 << e) + k * step))
}

fn class_size(class: usize) -> usize {
    match class {
        0..=2 => 64 << class,
        _ => (4 + (class - 3) % 4 + 1) << (8 + (class - 3) / 4 - 2),
    }
}

/// A sized request past the top class or the byte budget, with the largest the pool serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLarge {
    pub n: usize,
    pub max: usize,
}

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a sized buffer of {} bytes is past what the pool serves, {}", self.n, self.max)
    }
}

impl std::error::Error for TooLarge {}

/// Why [`Pool::reserve_sized`] reserved nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveError {
    /// No class of the pool serves a buffer that large.
    TooLarge(TooLarge),
    /// The allocator could not map `bytes`.
    Unmapped { bytes: usize },
}

impl std::fmt::Display for ReserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReserveError::TooLarge(e) => e.fmt(f),
            ReserveError::Unmapped { bytes } => {
                write!(f, "a reserve of {bytes} bytes could not be mapped")
            }
        }
    }
}

impl std::error::Error for ReserveError {}

/// What a container knows at open of the sizes of the samples its demuxer takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Known {
    /// Nothing, a stream of segments, or a file fragmented, growing or with no index.
    #[default]
    Nothing,
    /// Where the cued Clusters start, from Matroska Cues, so the largest span only bounds a block.
    Clusters,
    /// Every sample's size, an MP4's sample tables.
    Samples,
}

/// What a container says at open of the samples its demuxer takes, zero where it says nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Demand {
    pub known: Known,
    /// The largest sample, with [`Known::Clusters`] a bound on a block.
    pub largest: u64,
    /// The most bytes in `window` samples in a row of one track, with [`Known::Samples`] only.
    pub peak: u64,
    pub window: u32,
}

impl Demand {
    pub const fn unknown() -> Demand {
        Demand { known: Known::Nothing, largest: 0, peak: 0, window: 0 }
    }

    /// Two tracks read through one pool, the larger sample and the peaks added.
    pub fn and(self, other: Demand) -> Demand {
        let known = match (self.known, other.known) {
            (Known::Nothing, k) | (k, Known::Nothing) => k,
            (a, b) => a.min(b),
        };
        Demand {
            known,
            largest: self.largest.max(other.largest),
            peak: self.peak.saturating_add(other.peak),
            window: self.window.max(other.window),
        }
    }
}

/// A view whose bytes do not start on a `u16` boundary, so it cannot be read as words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Misaligned {
    pub address: usize,
}

impl std::fmt::Display for Misaligned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "memory at {:#x} is not aligned for 16 bit words", self.address)
    }
}

impl std::error::Error for Misaligned {}

/// An allocator's `register`, see [`Allocator::register`].
pub type RegisterFn =
    unsafe fn(ctx: *mut (), base: *mut u8, len: usize, stride: usize) -> Option<u64>;

pub type MapFn = unsafe fn(ctx: *mut (), len: usize, align: usize, name: &CStr) -> *mut u8;
pub type UnmapFn = unsafe fn(ctx: *mut (), base: *mut u8, len: usize, align: usize, token: u64);

/// The region allocator, function pointers with a context. Built with [`Allocator::new`].
#[derive(Clone, Copy)]
pub struct Allocator {
    ctx: *mut (),
    map: MapFn,
    unmap: UnmapFn,
    register: Option<RegisterFn>,
}

impl Allocator {
    /// # Safety
    ///
    /// `map` returns null or `len` bytes at `align`, zeroed, that nothing else reads or writes
    /// until `unmap` takes them back, and `len` bytes are readable before any write. `unmap` is
    /// called once per mapping, possibly after every `Pool` is gone, so `ctx` outlives every
    /// view of every region. `register` returns a nonzero token for a region it accepts.
    pub const unsafe fn new(
        ctx: *mut (),
        map: MapFn,
        unmap: UnmapFn,
        register: Option<RegisterFn>,
    ) -> Allocator {
        Allocator { ctx, map, unmap, register }
    }
}

// SAFETY: a backend keeps its `ctx` state behind its own synchronisation, the heap one has none.
unsafe impl Send for Allocator {}
unsafe impl Sync for Allocator {}

/// Names a heap region's pages in the process's maps, or clears them on `None`.
pub type Namer = unsafe fn(base: *mut u8, len: usize, name: Option<&CStr>) -> bool;

static NAMER: OnceLock<Namer> = OnceLock::new();

/// Sets the process's heap region namer, the first call wins.
pub fn set_namer(namer: Namer) {
    let _ = NAMER.set(namer);
}

/// The longest region name, the kernel's limit with its NUL (prctl(2)).
const NAME_MAX: usize = 80;
const OWNER_MAX: usize = 48;

#[derive(Clone, Copy)]
struct Owner {
    bytes: [u8; OWNER_MAX],
    len: u8,
}

impl Owner {
    fn new(name: &str) -> Owner {
        let mut o = Owner { bytes: [0; OWNER_MAX], len: 0 };
        for (dst, b) in o.bytes.iter_mut().zip(name.bytes()) {
            *dst = if b.is_ascii_graphic() && !b"[]\\$`".contains(&b) { b } else { b'_' };
            o.len += 1;
        }
        o
    }
}

fn region_name<'a>(buf: &'a mut [u8; NAME_MAX], owner: &Owner, class: usize) -> &'a CStr {
    let (mut n, unit): (usize, &[u8]) = match class {
        c if c >= 1 << 20 && c % (1 << 20) == 0 => (c >> 20, b"M"),
        c if c >= 1 << 10 && c % (1 << 10) == 0 => (c >> 10, b"K"),
        c => (c, b""),
    };
    let (mut digits, mut i) = ([0u8; 20], 20);
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let mut at = 0;
    for part in [b"pf:pool:", &owner.bytes[..owner.len as usize], b":", &digits[i..], unit] {
        let take = part.len().min(NAME_MAX - 1 - at);
        buf[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    buf[at] = 0;
    CStr::from_bytes_until_nul(&buf[..=at]).unwrap_or(c"")
}

/// The most alignment `calloc` serves, past it `alloc_zeroed` writes every byte.
const CALLOC_ALIGN: usize = 16;

// The word before the base keeps the start to free, its low bit set once named.
unsafe fn heap_map(_: *mut (), len: usize, align: usize, name: &CStr) -> *mut u8 {
    let Some(total) = len.checked_add(align).and_then(|n| n.checked_add(8)) else {
        return std::ptr::null_mut();
    };
    let Ok(layout) = std::alloc::Layout::from_size_align(total, CALLOC_ALIGN) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `total` is nonzero.
    let raw = unsafe { std::alloc::alloc_zeroed(layout) };
    if raw.is_null() {
        return raw;
    }
    let skip = (raw as usize + 8).next_multiple_of(align) - raw as usize;
    // SAFETY: `skip` is at most `align + 7`, so the base, `len` past it and the word before lie in
    // the allocation.
    unsafe {
        let base = raw.add(skip);
        let named = !name.is_empty() && NAMER.get().is_some_and(|f| f(base, len, Some(name)));
        base.sub(8).cast::<usize>().write_unaligned(raw as usize | named as usize);
        base
    }
}

unsafe fn heap_unmap(_: *mut (), base: *mut u8, len: usize, align: usize, _: u64) {
    // SAFETY: `base` came from `heap_map` with this `len` and `align`.
    unsafe {
        let word = base.sub(8).cast::<usize>().read_unaligned();
        if let (1, Some(f)) = (word & 1, NAMER.get()) {
            f(base, len, None);
        }
        let raw = (word & !1) as *mut u8;
        let layout = std::alloc::Layout::from_size_align_unchecked(len + align + 8, CALLOC_ALIGN);
        std::alloc::dealloc(raw, layout)
    }
}

impl Allocator {
    /// Zeroed heap memory, the default.
    pub const HEAP: Allocator =
        Allocator { ctx: std::ptr::null_mut(), map: heap_map, unmap: heap_unmap, register: None };
}

#[derive(Clone, Copy)]
pub struct PoolConfig {
    pub slot_size: usize,
    /// Buffers out at once, slots and sized buffers together.
    pub slots: u32,
    /// Class bytes sized buffers may hold at once.
    pub bytes: u64,
    /// Alignment of every slot, [`MIN_ALIGN`] at least.
    pub align: usize,
    pub allocator: Allocator,
}

impl PoolConfig {
    /// Cache-line aligned heap slots with a byte budget of what the slots hold together.
    pub const fn heap(slot_size: usize, slots: u32) -> Self {
        let bytes = slot_size as u64 * slots as u64;
        Self { slot_size, slots, bytes, align: MIN_ALIGN, allocator: Allocator::HEAP }
    }
}

/// A fixed-slot-size buffer pool, cloned as a shared handle.
pub struct Pool {
    inner: Arc<PoolInner>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Main,
    Small(u8),
    Exact,
}

struct Sized {
    /// Class bytes the buffer holds of the budget, with [`COUNTED`] under the count cap.
    charge: AtomicU32,
    touched: AtomicU32,
}

#[repr(align(64))]
struct Line<T>(T);

struct Region {
    base: NonNull<u8>,
    len: usize,
    align: usize,
    token: u64,
    stride: usize,
    slot_size: usize,
    allocator: Allocator,
    kind: Kind,
    locked: bool,
    counts: Box<[AtomicU32]>,
    next: Box<[Next]>,
    sized: Box<[Sized]>,
    pool: Weak<PoolInner>,
    /// Return stack, its top slot or `END`, `CLOSED` once detached, `WANT` and the pushed counts.
    head: Line<Word>,
    /// Releases since the close, plus `DETACHED` and what was out at the close.
    state: Word,
    /// Per-slot side data, mapped at the first `set_side` on the region.
    side: OnceLock<SideArena>,
}

// SAFETY: bytes are reached only through views, a free stack only under the pool lock and the
// return stack by atomics.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    #[allow(clippy::disallowed_methods)]
    fn map(
        allocator: Allocator,
        slot_size: usize,
        slots: u32,
        align: usize,
        kind: Kind,
        locked: bool,
        pool: Weak<PoolInner>,
    ) -> Option<NonNull<Region>> {
        let align = align.max(MIN_ALIGN).next_power_of_two();
        let stride = slot_size.max(1).checked_next_multiple_of(align)?;
        let len = stride.checked_mul(slots.max(1) as usize)?;
        let mut buf = [0u8; NAME_MAX];
        let name = pool.upgrade().map_or(c"", |p| region_name(&mut buf, &p.owner, slot_size));
        // SAFETY: the allocator's contract, `len` is nonzero and `align` a power of two.
        let base = NonNull::new(unsafe { (allocator.map)(allocator.ctx, len, align, name) })?;
        let token = match allocator.register {
            // SAFETY: the mapping is this region's alone until the token is back.
            Some(register) => {
                match unsafe { register(allocator.ctx, base.as_ptr(), len, stride) } {
                    Some(token) => token,
                    None => {
                        // SAFETY: mapped above and not registered, so unmapped with token 0.
                        unsafe { (allocator.unmap)(allocator.ctx, base.as_ptr(), len, align, 0) };
                        return None;
                    }
                }
            }
            None => 0,
        };
        let slots = slots.max(1);
        let link = |i: u32| if i + 1 < slots { i + 1 } else { END };
        let sized = match kind {
            Kind::Small(_) => slots,
            _ => 0,
        };
        let region = Region {
            base,
            len,
            align,
            token,
            stride,
            slot_size: slot_size.max(1),
            allocator,
            kind,
            locked,
            counts: (0..slots).map(|_| AtomicU32::new(1)).collect(),
            next: (0..slots).map(|i| Next::new(link(i))).collect(),
            sized: (0..sized)
                .map(|_| Sized { charge: AtomicU32::new(0), touched: AtomicU32::new(0) })
                .collect(),
            pool,
            head: Line(Word::new(END as u64)),
            state: Word::new(0),
            side: OnceLock::new(),
        };
        Some(NonNull::from(Box::leak(Box::new(region))))
    }

    #[inline]
    fn slot_ptr(&self, slot: u32) -> *mut u8 {
        // SAFETY: `slot` indexes the table, so the slot lies inside the mapping.
        unsafe { self.base.as_ptr().add(slot as usize * self.stride) }
    }
}

enum Give {
    Pushed,
    /// Pushed, and the push took a raised `WANT`.
    Wanted,
    Closed,
}

fn give_back(head: &Word, next: &Next, slot: u32, counted: bool, mut pin: impl FnMut()) -> Give {
    let add = PUSHED + if counted { PUSHED_COUNTED } else { 0 };
    // Acquire, so a drain after a seen CLOSED follows the close.
    let mut seen = head.load(Ordering::Acquire);
    loop {
        if seen as u32 == CLOSED {
            return Give::Closed;
        }
        if seen & WANT != 0 {
            pin();
        }
        next.store(seen as u32, Ordering::Relaxed);
        let new = ((seen & !(WANT | TOP)) + add) | slot as u64;
        // Release publishes the link, acquire pairs with `raise` so the enlisted are visible.
        match head.compare_exchange_weak(seen, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) if seen & WANT != 0 => return Give::Wanted,
            Ok(_) => return Give::Pushed,
            Err(now) => seen = now,
        }
    }
}

fn take(head: &Word, keep: u64) -> u64 {
    if head.load(Ordering::Relaxed) as u32 == END {
        return END as u64;
    }
    head.swap(END as u64 | keep, Ordering::AcqRel)
}

fn raise(head: &Word) -> u64 {
    head.swap(END as u64 | WANT, Ordering::AcqRel)
}

fn note_drained(cell: &Word, n: u64, starved: &Starved) {
    cell.fetch_add(n, Ordering::Relaxed);
    // Dekker with `want`, count then look for the enlisted.
    handshake(Ordering::SeqCst);
    starved.wake();
}

fn want(starved: &Starved, station: &Arc<Station>, raise: impl FnOnce()) {
    starved.enlist(station);
    raise();
    // Dekker with `note_drained`, raise the flag then read the counts.
    handshake(Ordering::SeqCst);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// The first of its class stays, a later one goes while class bytes are past the budget.
    Budget,
    /// Never, mapped by [`Pool::reserve_sized`].
    Reserved,
    /// At once, a burst past a reserve.
    Burst,
}

#[derive(Clone, Copy)]
struct Held {
    region: NonNull<Region>,
    free: u32,
    /// A second free stack of folded returns, so a fold walks no chain unless both have some.
    spare: u32,
    out: u32,
    locked: bool,
    keep: Keep,
}

impl Held {
    fn new(region: NonNull<Region>, locked: bool, keep: Keep) -> Held {
        Held { region, free: 0, spare: END, out: 0, locked, keep }
    }
}

fn pop(h: &mut Held) -> Option<u32> {
    if h.free == END {
        if h.spare == END {
            return None;
        }
        h.free = std::mem::replace(&mut h.spare, END);
    }
    // SAFETY: a held region is mapped.
    let r = unsafe { h.region.as_ref() };
    let slot = h.free;
    h.free = r.next[slot as usize].load(Ordering::Relaxed);
    h.out += 1;
    Some(slot)
}

#[derive(Default)]
struct Counts {
    out: u64,
    sized_out: u64,
    bytes_out: u64,
    acquires: u64,
    recycles: u64,
    high_water: u64,
    bytes_high: u64,
}

impl Counts {
    fn buffers_out(&self) -> u64 {
        self.out + self.sized_out
    }
}

fn fold(h: &mut Held, word: u64, c: &mut Counts) {
    let k = (word >> 33) as u32 & 0x7FFF;
    if k == 0 {
        return;
    }
    // SAFETY: a held region is mapped.
    let r = unsafe { h.region.as_ref() };
    let chain = word as u32;
    if h.free == END {
        h.free = chain;
    } else if h.spare == END {
        h.spare = chain;
    } else {
        let tail = (1..k).fold(chain, |at, _| r.next[at as usize].load(Ordering::Relaxed));
        r.next[tail as usize].store(h.spare, Ordering::Relaxed);
        h.spare = chain;
    }
    h.out -= k;
    match r.kind {
        Kind::Main => {
            c.out -= k as u64;
            c.recycles += k as u64;
        }
        Kind::Small(_) => {
            c.bytes_out -= k as u64 * r.slot_size as u64;
            c.sized_out -= word >> 48;
        }
        Kind::Exact => {}
    }
}

/// A slot of the cursor region of `list`, taking its returns once it has nothing free.
fn next_slot(
    list: &mut [Held],
    cursor: &mut u32,
    counts: &mut Counts,
    keep: u64,
    ok: impl Fn(&Counts) -> bool,
) -> Option<(NonNull<Region>, u32)> {
    for _ in 0..list.len() {
        let at = (*cursor as usize) % list.len();
        let h = &mut list[at];
        if h.keep != Keep::Reserved {
            for taken in [false, true] {
                if taken {
                    if h.locked {
                        break;
                    }
                    // SAFETY: a held region is mapped.
                    fold(h, take(&unsafe { h.region.as_ref() }.head.0, keep), counts);
                }
                if ok(counts)
                    && let Some(slot) = pop(h)
                {
                    return Some((h.region, slot));
                }
            }
        }
        *cursor = ((at + 1) % list.len()) as u32;
    }
    None
}

fn from_reserve(
    small: &mut [Vec<Held>; SMALL_CLASSES],
    mut classes: u128,
    want: u32,
) -> Option<(NonNull<Region>, u32)> {
    while classes != 0 {
        let class = classes.trailing_zeros() as usize;
        let reserved = small[class].iter_mut().filter(|h| h.keep == Keep::Reserved);
        if let Some(taken) =
            reserved.into_iter().find_map(|h| pop_fit(h, want).map(|slot| (h.region, slot)))
        {
            return Some(taken);
        }
        classes &= classes - 1;
    }
    None
}

/// Pops the free slot whose touched bytes fit `want` most tightly, else the most touched.
fn pop_fit(h: &mut Held, want: u32) -> Option<u32> {
    // SAFETY: a held region is mapped.
    let r = unsafe { h.region.as_ref() };
    debug_assert!(h.spare == END, "a reserve releases under the lock, so nothing is folded");
    let (mut at, mut prev) = (h.free, END);
    // Touched bytes, the block and the one before it.
    type Spot = (u32, u32, u32);
    let (mut fit, mut big): (Option<Spot>, Option<Spot>) = (None, None);
    while at != END {
        let touched = r.sized[at as usize].touched.load(Ordering::Relaxed);
        if touched >= want && fit.is_none_or(|(t, ..)| touched < t) {
            fit = Some((touched, at, prev));
        }
        if big.is_none_or(|(t, ..)| touched > t) {
            big = Some((touched, at, prev));
        }
        prev = at;
        at = r.next[at as usize].load(Ordering::Relaxed);
    }
    let (_, slot, before) = fit.or(big)?;
    let next = r.next[slot as usize].load(Ordering::Relaxed);
    match before {
        END => h.free = next,
        before => r.next[before as usize].store(next, Ordering::Relaxed),
    }
    h.out += 1;
    Some(slot)
}

/// Closes the stack under the lock. Returns the region once nothing is out, for [`unmap_closed`]
/// after the lock drops, so no waiter spins through the allocator.
fn close(mut h: Held, c: &mut Counts) -> Option<NonNull<Region>> {
    // SAFETY: a held region is mapped.
    let r = unsafe { h.region.as_ref() };
    let word = r.head.0.swap(CLOSED as u64, Ordering::AcqRel);
    fold(&mut h, word, c);
    // Releases after the swap count down from zero, so the op that lands on DETACHED is the last.
    let add = DETACHED + h.out as u64;
    (r.state.fetch_add(add, Ordering::AcqRel).wrapping_add(add) == DETACHED).then_some(h.region)
}

/// Unmaps what [`close`] handed back, the detached among `closed`.
fn unmap_closed(closed: impl IntoIterator<Item = NonNull<Region>>, mapped: &AtomicU64) {
    for region in closed {
        // SAFETY: `close` saw the state land on DETACHED, so nothing names the region.
        unsafe { unmap(region, Some(mapped)) }
    }
}

/// # Safety
/// The caller saw the state word land on `DETACHED`, so nothing names the region.
unsafe fn unmap(region: NonNull<Region>, mapped: Option<&AtomicU64>) {
    let r = unsafe { Box::from_raw(region.as_ptr()) };
    if let Some(mapped) = mapped {
        let side = r.side.get().map_or(0, SideArena::len);
        mapped.fetch_sub(r.len as u64 + side, Ordering::Relaxed);
    }
    // SAFETY: the mapping `map` made, with its layout and token.
    unsafe { (r.allocator.unmap)(r.allocator.ctx, r.base.as_ptr(), r.len, r.align, r.token) };
}

fn release(region: NonNull<Region>, slot: u32) {
    // SAFETY: the view's count on the slot kept the region mapped until this call.
    let r = unsafe { region.as_ref() };
    if let Some(side) = r.side.get() {
        side.claims[slot as usize].store(0, Ordering::Release);
    }
    // The next owner's count, set here so the acquirer never writes the slot's line.
    r.counts[slot as usize].store(1, Ordering::Relaxed);
    if r.locked {
        return release_locked(region, slot);
    }
    let mut pin = None;
    let counted =
        r.sized.get(slot as usize).is_some_and(|s| s.charge.load(Ordering::Relaxed) & COUNTED != 0);
    let give = give_back(&r.head.0, &r.next[slot as usize], slot, counted, || {
        if pin.is_none() {
            pin = r.pool.upgrade();
        }
    });
    // After a push the acquirer may close and unmap the region, so `r` is not read again.
    match give {
        Give::Pushed => {}
        Give::Wanted => {
            if let Some(pool) = pin {
                pool.starved.wake();
            }
        }
        Give::Closed => drain(region, slot),
    }
}

fn release_locked(region: NonNull<Region>, slot: u32) {
    // SAFETY: the view's count on the slot kept the region mapped until this call.
    let r = unsafe { region.as_ref() };
    let (Some(pool), Kind::Small(class)) = (r.pool.upgrade(), r.kind) else {
        return drain(region, slot);
    };
    let mut regions = pool.lock();
    if r.head.0.load(Ordering::Relaxed) as u32 == CLOSED {
        drop(regions);
        return drain(region, slot);
    }
    let Regions { small, counts, class_bytes, .. } = &mut *regions;
    let list = &mut small[class as usize];
    let at = list.iter().position(|h| h.region == region).expect("an attached region is listed");
    let h = &mut list[at];
    r.next[slot as usize].store(h.free, Ordering::Relaxed);
    h.free = slot;
    h.out -= 1;
    let charge = r.sized[slot as usize].charge.load(Ordering::Relaxed);
    counts.bytes_out -= (charge & !COUNTED) as u64;
    counts.sized_out -= (charge & COUNTED != 0) as u64;
    let shed = match h.keep {
        Keep::Budget => at > 0 && *class_bytes as u64 > pool.bytes_budget,
        Keep::Reserved => false,
        Keep::Burst => true,
    };
    let mut closed = None;
    if h.out == 0 && shed {
        *class_bytes -= r.len;
        closed = close(list.swap_remove(at), counts);
    }
    drop(regions);
    unmap_closed(closed, &pool.bytes_mapped);
    pool.starved.wake();
}

fn drain(region: NonNull<Region>, slot: u32) {
    // SAFETY: the view's count on the slot kept the region mapped until this call.
    let r = unsafe { region.as_ref() };
    let pool = r.pool.upgrade();
    if let Some(pool) = &pool {
        match r.kind {
            Kind::Main => note_drained(&pool.drained[0], 1, &pool.starved),
            Kind::Small(_) => {
                let charge = r.sized[slot as usize].charge.load(Ordering::Relaxed);
                let counted = charge & COUNTED != 0;
                pool.drained[1].fetch_add(counted as u64, Ordering::Relaxed);
                note_drained(&pool.drained[2], (charge & !COUNTED) as u64, &pool.starved);
            }
            Kind::Exact => {}
        }
    }
    if r.state.fetch_sub(1, Ordering::AcqRel).wrapping_sub(1) == DETACHED {
        // SAFETY: the word landed on DETACHED by this call, the last release.
        unsafe { unmap(region, pool.as_deref().map(|p| &p.bytes_mapped)) }
    }
}

fn exact(n: usize) -> Option<Memory> {
    let region = Region::map(Allocator::HEAP, n, 1, MIN_ALIGN, Kind::Exact, false, Weak::new())?;
    // SAFETY: just mapped, with its one slot free.
    let r = unsafe { region.as_ref() };
    r.head.0.store(CLOSED as u64, Ordering::Relaxed);
    r.state.store(DETACHED + 1, Ordering::Relaxed);
    Some(Memory { region: Some(region), slot: 0, offset: 0, len: 0, side: 0 })
}

/// A lock of one exchange and one store. A holder never parks and never maps or unmaps, so a
/// waiter spins then yields.
struct Seat<T> {
    busy: Busy,
    cell: UnsafeCell<T>,
}

// SAFETY: `take` hands out one `&mut T` at a time, the busy flag's acquire pairs with the last
// holder's release.
unsafe impl<T: Send> Sync for Seat<T> {}

struct Seated<'a, T> {
    seat: &'a Seat<T>,
}

#[cfg(not(loom))]
fn seat_ptr<T>(cell: &UnsafeCell<T>) -> *mut T {
    cell.get()
}

#[cfg(loom)]
fn seat_ptr<T>(cell: &UnsafeCell<T>) -> *mut T {
    cell.with_mut(|p| p)
}

#[cfg(not(loom))]
fn pause(spins: u32) {
    if spins < 64 {
        std::hint::spin_loop();
    } else {
        std::thread::yield_now();
    }
}

#[cfg(loom)]
fn pause(_: u32) {
    loom::thread::yield_now();
}

impl<T> Seat<T> {
    fn new(value: T) -> Seat<T> {
        Seat { busy: Busy::new(false), cell: UnsafeCell::new(value) }
    }

    fn take(&self) -> Seated<'_, T> {
        let mut spins = 0;
        // Acquire pairs with the release in `Seated::drop`. A failed exchange stores nothing on the
        // line.
        while self
            .busy
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            pause(spins);
            spins += 1;
        }
        Seated { seat: self }
    }

    fn get_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self`, so no holder.
        unsafe { &mut *seat_ptr(&self.cell) }
    }
}

impl<T> Deref for Seated<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the seat is held until the drop below, by the exchange in `take`.
        unsafe { &*seat_ptr(&self.seat.cell) }
    }
}

impl<T> DerefMut for Seated<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` is the one guard.
        unsafe { &mut *seat_ptr(&self.seat.cell) }
    }
}

impl<T> Drop for Seated<'_, T> {
    fn drop(&mut self) {
        self.seat.busy.store(false, Ordering::Release);
    }
}

struct Regions {
    slot_size: usize,
    retired: bool,
    main: Vec<Held>,
    cursor: u32,
    small_cursor: [u32; SMALL_CLASSES],
    /// Acquires since a take last found returns.
    since_take: u32,
    mapped: u32,
    small: [Vec<Held>; SMALL_CLASSES],
    classes: u128,
    reserves: u128,
    class_bytes: usize,
    counts: Counts,
}

impl Regions {
    fn attached(&self) -> u64 {
        (self.main.len() + self.small.iter().map(Vec::len).sum::<usize>()) as u64
    }
}

fn each_lockfree(
    main: &mut [Held],
    small: &mut [Vec<Held>; SMALL_CLASSES],
    classes: u128,
    mut f: impl FnMut(&mut Held),
) {
    main.iter_mut().for_each(&mut f);
    let mut bits = classes;
    while bits != 0 {
        small[bits.trailing_zeros() as usize].iter_mut().filter(|h| !h.locked).for_each(&mut f);
        bits &= bits - 1;
    }
}

struct PoolInner {
    regions: Seat<Regions>,
    slot_size: AtomicUsize,
    align: usize,
    allocator: Allocator,
    owner: Owner,
    max_slots: AtomicU32,
    bytes_budget: u64,
    slot_allocations: AtomicU64,
    bytes_mapped: AtomicU64,
    /// Regions the allocator refused to map, see [`Pool::map_failures`].
    map_failures: AtomicU64,
    drained: [Word; 3],
    starved: Starved,
}

// SAFETY: the region pointers are used under the lock, and a region is `Sync`.
unsafe impl Send for PoolInner {}
unsafe impl Sync for PoolInner {}

impl PoolInner {
    fn lock(&self) -> Seated<'_, Regions> {
        self.regions.take()
    }

    fn keep(&self) -> u64 {
        if self.starved.wanted() { WANT } else { 0 }
    }

    fn fold_drained(&self, c: &mut Counts) {
        let grab = |cell: &Word| match cell.load(Ordering::Relaxed) {
            0 => 0,
            _ => cell.swap(0, Ordering::Relaxed),
        };
        c.out -= grab(&self.drained[0]);
        c.sized_out -= grab(&self.drained[1]);
        c.bytes_out -= grab(&self.drained[2]);
    }

    /// Takes every return stack, so the counts are exact as of now.
    fn reclaim(&self, regions: &mut Regions) {
        let keep = self.keep();
        let Regions { main, small, classes, counts, .. } = regions;
        let before = counts.buffers_out();
        // SAFETY: a held region is mapped.
        each_lockfree(main, small, *classes, |h| {
            fold(h, take(&unsafe { h.region.as_ref() }.head.0, keep), counts)
        });
        self.fold_drained(counts);
        if counts.buffers_out() < before {
            regions.since_take = 0;
        }
    }

    /// A count that `passes` a high water takes the returns near it, at most once per [`TAKE_SPAN`]
    /// acquires.
    fn water(&self, reg: &mut Regions, class: Option<usize>, passes: impl Fn(&Counts) -> bool) {
        let bound = (reg.counts.high_water / 2).clamp(1, TAKE_SPAN as u64);
        if (reg.since_take as u64) < bound {
            return;
        }
        let keep = self.keep();
        let Regions { main, small, small_cursor, cursor, counts, .. } = reg;
        let before = counts.buffers_out();
        let near = match class {
            None => std::slice::from_mut(&mut main[*cursor as usize]),
            Some(class) => {
                let len = small[class].len();
                let at = small_cursor[class] as usize % len.max(1);
                &mut small[class][at..(at + 1).min(len)]
            }
        };
        for h in near.iter_mut().filter(|h| !h.locked) {
            // SAFETY: a held region is mapped.
            fold(h, take(&unsafe { h.region.as_ref() }.head.0, keep), counts);
        }
        if counts.buffers_out() < before {
            reg.since_take = 0;
        }
        if passes(&reg.counts) {
            self.reclaim(reg);
        }
    }

    fn enlist(&self, station: &Arc<Station>) {
        let mut regions = self.lock();
        let Regions { main, small, classes, counts, .. } = &mut *regions;
        want(&self.starved, station, || {
            // SAFETY: a held region is mapped.
            each_lockfree(main, small, *classes, |h| {
                fold(h, raise(&unsafe { h.region.as_ref() }.head.0), counts)
            });
        });
        self.fold_drained(counts);
        regions.since_take = 0;
    }

    /// Closes every region, returning the ones to unmap once the lock is gone.
    fn detach_all(regions: &mut Regions) -> Vec<NonNull<Region>> {
        let Regions { main, small, counts, .. } = regions;
        let held = std::mem::take(main);
        let small = small.iter_mut().flat_map(std::mem::take);
        let closed = held.into_iter().chain(small).filter_map(|h| close(h, counts)).collect();
        regions.mapped = 0;
        regions.cursor = 0;
        regions.small_cursor = [0; SMALL_CLASSES];
        regions.classes = 0;
        regions.reserves = 0;
        regions.class_bytes = 0;
        closed
    }
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        let closed = PoolInner::detach_all(self.regions.get_mut());
        unmap_closed(closed, &self.bytes_mapped);
    }
}

impl Pool {
    /// An unbounded pool of heap slots.
    pub fn new(slot_size: usize) -> Self {
        Self::bounded(slot_size, u32::MAX)
    }

    /// A pool of heap slots capped at `max_slots` buffers out at once.
    pub fn bounded(slot_size: usize, max_slots: u32) -> Self {
        Self::with_config(PoolConfig::heap(slot_size, max_slots))
    }

    /// A pool as configured, mapping nothing until the first acquire.
    pub fn with_config(config: PoolConfig) -> Self {
        Self::named(config, "unnamed")
    }

    /// A pool whose regions are named `pf:pool:<owner>:<class>` in the process's maps.
    // Once per pool at pipeline setup.
    #[allow(clippy::disallowed_methods)]
    pub fn named(config: PoolConfig, owner: &str) -> Self {
        let slot_size = config.slot_size.max(1);
        let regions = Regions {
            slot_size,
            retired: false,
            main: Vec::new(),
            cursor: 0,
            small_cursor: [0; SMALL_CLASSES],
            since_take: 0,
            mapped: 0,
            small: std::array::from_fn(|_| Vec::new()),
            classes: 0,
            reserves: 0,
            class_bytes: 0,
            counts: Counts::default(),
        };
        Self {
            inner: Arc::new(PoolInner {
                regions: Seat::new(regions),
                slot_size: AtomicUsize::new(slot_size),
                align: config.align.max(MIN_ALIGN).next_power_of_two(),
                allocator: config.allocator,
                owner: Owner::new(owner),
                max_slots: AtomicU32::new(config.slots.max(1)),
                bytes_budget: config.bytes,
                slot_allocations: AtomicU64::new(0),
                bytes_mapped: AtomicU64::new(0),
                map_failures: AtomicU64::new(0),
                drained: std::array::from_fn(|_| Word::new(0)),
                starved: Starved::new(),
            }),
        }
    }

    pub fn slot_size(&self) -> usize {
        self.inner.slot_size.load(Ordering::Relaxed)
    }

    pub fn id(&self) -> PoolId {
        PoolId(Arc::as_ptr(&self.inner) as usize)
    }

    /// Reactor read credits that keep [`IO_DEPTH_BYTES`] in flight, 4 to 64 and at most the slots.
    pub fn io_depth(&self) -> u32 {
        (IO_DEPTH_BYTES / self.slot_size().max(1) as u64).clamp(4, 64).min(self.max_slots() as u64)
            as u32
    }

    /// Buffers available before hitting the cap (`u32::MAX` for unbounded pools).
    pub fn free_slots(&self) -> u32 {
        let (out, _) = self.in_use();
        (self.max_slots() as u64).saturating_sub(out) as u32
    }

    pub fn max_slots(&self) -> u32 {
        self.inner.max_slots.load(Ordering::Relaxed)
    }

    /// Raises the cap to `slots` buffers out at once, `true` when it rose. Never lowers it.
    pub fn grow_cap(&self, slots: u32) -> bool {
        let regions = self.inner.lock();
        let grow = self.inner.max_slots.load(Ordering::Relaxed) < slots;
        if grow {
            self.inner.max_slots.store(slots, Ordering::Relaxed);
        }
        drop(regions);
        grow
    }

    /// [`acquire`](Self::acquire) bounded by the cap, `None` when full or when no region can be
    /// mapped.
    pub fn try_acquire(&self) -> Option<Memory> {
        let inner = &*self.inner;
        let mut regions = inner.lock();
        let reg = &mut *regions;
        if reg.retired {
            return None;
        }
        let max = inner.max_slots.load(Ordering::Relaxed) as u64;
        let keep = inner.keep();
        let under = |c: &Counts| c.buffers_out() < max;
        let next = |reg: &mut Regions| {
            let Regions { main, cursor, counts, .. } = reg;
            next_slot(main, cursor, counts, keep, under)
        };
        let taken = next(reg).or_else(|| {
            // The rest of the stacks are taken now, so the counts are exact.
            inner.reclaim(reg);
            next(reg)
        });
        let (region, slot) = match taken {
            Some(taken) => taken,
            None => {
                if reg.counts.buffers_out() >= max {
                    return None;
                }
                let attached: u64 = reg.main.iter().map(|h| h.out as u64).sum();
                let draining = reg.counts.out.saturating_sub(attached);
                let room = max.saturating_sub(reg.mapped as u64 + draining);
                let slots = room.min(region_slots(reg.slot_size) as u64) as u32;
                if slots == 0 {
                    return None;
                }
                // The slots are reserved, then mapped and registered with no waiter spinning.
                reg.mapped += slots;
                let slot_size = reg.slot_size;
                drop(regions);
                let pool = Arc::downgrade(&self.inner);
                let region = Region::map(
                    inner.allocator,
                    slot_size,
                    slots,
                    inner.align,
                    Kind::Main,
                    false,
                    pool,
                );
                regions = inner.lock();
                let reg = &mut *regions;
                // A resize or retire in between dropped the reservation with the regions.
                let kept = !reg.retired && reg.slot_size == slot_size;
                if kept {
                    reg.mapped = reg.mapped.saturating_sub(slots);
                }
                let Some(region) = region else {
                    inner.map_failures.fetch_add(1, Ordering::Relaxed);
                    return None;
                };
                if !kept {
                    // SAFETY: just mapped, nothing names it and no count went out.
                    unsafe { unmap(region, None) };
                    return None;
                }
                inner.slot_allocations.fetch_add(1, Ordering::Relaxed);
                // SAFETY: just mapped.
                inner
                    .bytes_mapped
                    .fetch_add(unsafe { region.as_ref() }.len as u64, Ordering::Relaxed);
                reg.mapped += slots;
                let mut h = Held::new(region, false, Keep::Budget);
                let slot = pop(&mut h).expect("a fresh region's slot");
                reg.main.push(h);
                reg.cursor = reg.main.len() as u32 - 1;
                (region, slot)
            }
        };
        let reg = &mut *regions;
        reg.counts.acquires += 1;
        reg.counts.out += 1;
        reg.since_take += 1;
        if reg.counts.buffers_out() > reg.counts.high_water {
            inner.water(reg, None, |c| c.buffers_out() > c.high_water);
            reg.counts.high_water = reg.counts.high_water.max(reg.counts.buffers_out());
        }
        drop(regions);
        Some(Memory { region: Some(region), slot, offset: 0, len: 0, side: 0 })
    }

    /// Enlists the station before a second attempt, so a slot returned after it signals the group.
    pub(crate) fn try_acquire_or_enlist(&self, station: &Arc<Station>) -> Option<Memory> {
        if let Some(m) = self.try_acquire() {
            return Some(m);
        }
        self.inner.enlist(station);
        self.try_acquire()
    }

    pub(crate) fn enlist(&self, station: &Arc<Station>) {
        self.inner.enlist(station);
    }

    pub(crate) fn reserve_starved(&self, groups: usize) {
        self.inner.starved.reserve(groups);
    }

    pub(crate) fn retire(&self) {
        let mut regions = self.inner.lock();
        regions.retired = true;
        let closed = PoolInner::detach_all(&mut regions);
        drop(regions);
        unmap_closed(closed, &self.inner.bytes_mapped);
    }

    /// Changes the slot size for later acquires, existing views stay valid.
    /// Regions the allocator refused to map. A `try_acquire` that returned `None` while this
    /// moved was a map failure, not backpressure.
    pub fn map_failures(&self) -> u64 {
        self.inner.map_failures.load(Ordering::Relaxed)
    }

    pub fn resize(&self, slot_size: usize) {
        let mut regions = self.inner.lock();
        let slot_size = slot_size.max(1);
        if regions.slot_size == slot_size {
            return;
        }
        let Regions { main, counts, .. } = &mut *regions;
        let closed: Vec<_> =
            std::mem::take(main).into_iter().filter_map(|h| close(h, counts)).collect();
        regions.mapped = 0;
        regions.cursor = 0;
        regions.slot_size = slot_size;
        self.inner.slot_size.store(slot_size, Ordering::Relaxed);
        drop(regions);
        unmap_closed(closed, &self.inner.bytes_mapped);
    }

    /// A buffer of at least `n` stale bytes that never waits, for cold paths. `None` past
    /// [`EXACT_MAX`] or when the heap refuses it.
    pub fn acquire_exact(&self, n: usize) -> Option<Memory> {
        if n > EXACT_MAX {
            return None;
        }
        let slot_size = self.slot_size();
        let fills_a_slot = n <= slot_size && n >= slot_size / 4;
        if fills_a_slot && let Some(m) = self.try_acquire() {
            return Some(m);
        }
        // Fallbacks stay outside the slot budget, counting them there deadlocks a paced consumer.
        if let Some(small) = self.small_free(n) {
            return Some(small);
        }
        self.inner.slot_allocations.fetch_add(1, Ordering::Relaxed);
        exact(n)
    }

    fn small_free(&self, n: usize) -> Option<Memory> {
        let (class, size) = small_class(n)?;
        if size >= self.slot_size() {
            return None;
        }
        self.take_class(class, size, None)
    }

    /// A class-sized buffer of at least `n` stale bytes, `Ok(None)` when the byte budget or the
    /// count is spent.
    pub fn try_acquire_sized(&self, n: usize) -> Result<Option<Memory>, TooLarge> {
        let (class, size) = self.sized_class(n)?;
        Ok(self.take_class(class, size, Some(self.inner.bytes_budget)))
    }

    fn sized_class(&self, n: usize) -> Result<(usize, usize), TooLarge> {
        let budget = self.inner.bytes_budget;
        let within = |&(_, size): &(usize, usize)| size as u64 <= budget;
        small_class(n).filter(within).ok_or_else(|| {
            let top = (SIZED_TOP as u64).min(budget) as usize;
            let max = match small_class(top) {
                _ if top < 64 => 0,
                Some((_, size)) if size <= top => size,
                Some((class, _)) => class_size(class - 1),
                None => SIZED_TOP,
            };
            TooLarge { n, max }
        })
    }

    /// Maps `count` buffers of the class serving `size` for the life of the pool. Each call adds a
    /// region.
    // Once per pool at run setup.
    #[allow(clippy::disallowed_methods)]
    pub fn reserve_sized(&self, size: usize, count: u32) -> Result<(), ReserveError> {
        let (class, size) = self.sized_class(size).map_err(ReserveError::TooLarge)?;
        let inner = &*self.inner;
        let mut regions = inner.lock();
        if regions.retired || count == 0 {
            return Ok(());
        }
        let (pool, kind) = (Arc::downgrade(&self.inner), Kind::Small(class as u8));
        let unmapped = ReserveError::Unmapped { bytes: size.saturating_mul(count as usize) };
        let region = Region::map(Allocator::HEAP, size, count, MIN_ALIGN, kind, true, pool)
            .ok_or(unmapped)?;
        inner.slot_allocations.fetch_add(1, Ordering::Relaxed);
        // SAFETY: just mapped.
        inner.bytes_mapped.fetch_add(unsafe { region.as_ref() }.len as u64, Ordering::Relaxed);
        regions.small[class].push(Held::new(region, true, Keep::Reserved));
        regions.classes |= 1u128 << class;
        regions.reserves |= 1u128 << class;
        Ok(())
    }

    pub(crate) fn try_acquire_sized_or_enlist(
        &self,
        n: usize,
        station: &Arc<Station>,
    ) -> Result<Option<Memory>, TooLarge> {
        if let Some(m) = self.try_acquire_sized(n)? {
            return Ok(Some(m));
        }
        self.inner.enlist(station);
        self.try_acquire_sized(n)
    }

    fn take_class(&self, class: usize, size: usize, budget: Option<u64>) -> Option<Memory> {
        let inner = &*self.inner;
        let mut regions = inner.lock();
        let reg = &mut *regions;
        if reg.retired {
            return None;
        }
        let (max, size64) = (inner.max_slots.load(Ordering::Relaxed) as u64, size as u64);
        let over =
            |c: &Counts| budget.is_some_and(|b| c.bytes_out + size64 > b || c.buffers_out() >= max);
        let high = |c: &Counts| {
            c.bytes_out + size64 > c.bytes_high
                || (budget.is_some() && c.buffers_out() >= c.high_water)
        };
        let keep = inner.keep();
        let own = |reg: &mut Regions| {
            let Regions { small, small_cursor, counts, .. } = reg;
            next_slot(&mut small[class], &mut small_cursor[class], counts, keep, |c| !over(c))
        };
        // The class's own regions, then every return, so a refusal is exact.
        let own = own(reg).or_else(|| {
            inner.reclaim(reg);
            own(reg)
        });
        if own.is_none() && over(&reg.counts) {
            return None;
        }
        let above = match size >= RESERVE_FLOOR {
            true => reg.reserves & !((1u128 << class) - 1),
            false => reg.reserves & (1u128 << class),
        };
        let (region, slot) = match own.or_else(|| from_reserve(&mut reg.small, above, size as u32))
        {
            Some(taken) => taken,
            None => {
                let list = &mut reg.small[class];
                let (pool, kind) = (Arc::downgrade(&self.inner), Kind::Small(class as u8));
                let doubled = CLASS_FIRST_SLOTS << list.len().min(16);
                let slots = doubled.min((CLASS_REGION_BYTES / size) as u32).max(1);
                // A burst past the budget, or one large slot, releases under the lock.
                let past = (reg.class_bytes + size * slots as usize) as u64 > inner.bytes_budget;
                let keep = if above != 0 { Keep::Burst } else { Keep::Budget };
                let locked = above != 0 || (!list.is_empty() && (past || slots == 1));
                drop(regions);
                let region =
                    Region::map(Allocator::HEAP, size, slots, MIN_ALIGN, kind, locked, pool);
                regions = inner.lock();
                let reg = &mut *regions;
                let region = region?;
                if reg.retired {
                    // SAFETY: just mapped, nothing names it and no count went out.
                    unsafe { unmap(region, None) };
                    return None;
                }
                inner.slot_allocations.fetch_add(1, Ordering::Relaxed);
                // SAFETY: just mapped.
                let len = unsafe { region.as_ref() }.len;
                inner.bytes_mapped.fetch_add(len as u64, Ordering::Relaxed);
                reg.class_bytes += len;
                let mut h = Held::new(region, locked, keep);
                let slot = pop(&mut h).expect("a fresh region's slot");
                reg.small[class].push(h);
                reg.classes |= 1u128 << class;
                (region, slot)
            }
        };
        let reg = &mut *regions;
        reg.since_take += 1;
        if high(&reg.counts) {
            inner.water(reg, Some(class), high);
        }
        let c = &mut reg.counts;
        c.bytes_out += size64;
        c.bytes_high = c.bytes_high.max(c.bytes_out);
        // SAFETY: just taken, under the seat.
        let entry = &unsafe { region.as_ref() }.sized[slot as usize];
        let charge = size as u32 | if budget.is_some() { COUNTED } else { 0 };
        if entry.charge.load(Ordering::Relaxed) != charge {
            entry.charge.store(charge, Ordering::Relaxed);
        }
        if entry.touched.load(Ordering::Relaxed) < size as u32 {
            entry.touched.store(size as u32, Ordering::Relaxed);
        }
        if budget.is_some() {
            c.sized_out += 1;
            c.high_water = c.high_water.max(c.buffers_out());
        }
        drop(regions);
        Some(Memory { region: Some(region), slot, offset: 0, len: 0, side: 0 })
    }

    /// Takes a slot, or past the cap an unpooled heap buffer of the slot size.
    pub fn acquire(&self) -> Memory {
        if let Some(m) = self.try_acquire() {
            return m;
        }
        self.inner.slot_allocations.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().counts.acquires += 1;
        exact(self.slot_size()).expect("the heap maps a slot sized buffer")
    }

    /// Buffers out now and the cap, the two [`stats`](Self::stats) numbers a per-pass check reads.
    pub fn in_use(&self) -> (u64, u64) {
        let mut regions = self.inner.lock();
        self.inner.reclaim(&mut regions);
        let cap = if regions.retired { 0 } else { self.max_slots() as u64 };
        (regions.counts.buffers_out(), cap)
    }

    pub fn stats(&self) -> PoolStats {
        let inner = &*self.inner;
        let mut regions = inner.lock();
        inner.reclaim(&mut regions);
        let c = &regions.counts;
        PoolStats {
            slot_allocations: inner.slot_allocations.load(Ordering::Relaxed),
            acquires: c.acquires,
            recycles: c.recycles,
            high_water: c.high_water,
            outstanding: c.buffers_out(),
            max_slots: if regions.retired {
                0
            } else {
                inner.max_slots.load(Ordering::Relaxed) as u64
            },
            slot_size: regions.slot_size as u64,
            bytes_out: c.bytes_out,
            bytes_high: c.bytes_high,
            bytes_budget: inner.bytes_budget,
            regions: regions.attached(),
            bytes_mapped: inner.bytes_mapped.load(Ordering::Relaxed),
        }
    }
}

impl Clone for Pool {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Regions ever mapped, exact buffers included, flat in steady state.
    pub slot_allocations: u64,
    pub acquires: u64,
    pub recycles: u64,
    pub high_water: u64,
    /// Buffers out now, slots and sized together, what the cap gates on.
    pub outstanding: u64,
    pub max_slots: u64,
    pub slot_size: u64,
    pub bytes_out: u64,
    pub bytes_high: u64,
    pub bytes_budget: u64,
    pub regions: u64,
    /// Bytes of every region of the pool still mapped, draining ones included.
    pub bytes_mapped: u64,
}

/// The identity of a slot while it is out of its pool, equal for every view of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId {
    region: usize,
    slot: u32,
}

/// The identity of a pool, what [`Memory::pool_id`] and [`Pool::id`] compare by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PoolId(usize);

/// The side area of one slot, what per-frame metadata fits in.
pub const SIDE_CAP: usize = 2048;

/// Why [`Memory::set_side`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideError {
    /// Nothing to put.
    Empty,
    /// Past [`SIDE_CAP`].
    TooLarge { len: usize },
    /// Another view of the slot holds the area, or the view is an empty husk.
    Taken,
}

impl std::fmt::Display for SideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SideError::Empty => write!(f, "no side data"),
            SideError::TooLarge { len } => write!(f, "{len} bytes of side data, past {SIDE_CAP}"),
            SideError::Taken => write!(f, "the slot's side area is another view's"),
        }
    }
}

impl std::error::Error for SideError {}

struct SideArena {
    claims: Box<[AtomicU32]>,
    bytes: Box<[std::cell::UnsafeCell<u8>]>,
}

// SAFETY: a slot's bytes are written by the one claimant and read after, see `set_side`.
unsafe impl Sync for SideArena {}

impl SideArena {
    // Once per region that carries side data.
    #[allow(clippy::disallowed_methods)]
    fn new(slots: usize) -> SideArena {
        let n = slots.max(1) * SIDE_CAP;
        let layout = std::alloc::Layout::array::<std::cell::UnsafeCell<u8>>(n)
            .expect("a region's side area fits the address space");
        // SAFETY: `n` is nonzero, so the layout is.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        if raw.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        let cells = std::ptr::slice_from_raw_parts_mut(raw.cast::<std::cell::UnsafeCell<u8>>(), n);
        // SAFETY: allocated just above by the global allocator with the layout of `[UnsafeCell<u8>;
        // n]`.
        let bytes = unsafe { Box::from_raw(cells) };
        SideArena { claims: (0..slots).map(|_| AtomicU32::new(0)).collect(), bytes }
    }

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn at(&self, slot: u32) -> *mut u8 {
        // UnsafeCell<u8> is a u8, and `slot` indexes the region's table.
        std::cell::UnsafeCell::raw_get(self.bytes[slot as usize * SIDE_CAP..].as_ptr())
    }
}

/// A refcounted `(region, slot, offset, len)` view into pooled bytes, copy-on-write when shared.
pub struct Memory {
    region: Option<NonNull<Region>>,
    slot: u32,
    offset: u32,
    len: u32,
    /// Bytes of the slot's side area this view states, 0 for none.
    side: u32,
}

// SAFETY: a view is a count on its slot, exclusive while it is the one view and read-only
// otherwise.
unsafe impl Send for Memory {}
unsafe impl Sync for Memory {}

impl Drop for Memory {
    fn drop(&mut self) {
        let Some(region) = self.region.take() else {
            return;
        };
        // SAFETY: this view's count kept the region mapped.
        let r = unsafe { region.as_ref() };
        // As in `Arc`'s drop, release orders our use of the bytes before the next owner's.
        let count = &r.counts[self.slot as usize];
        // The one view goes back with no atomic write, the push's release ordering our use.
        if count.load(Ordering::Acquire) == 1 || count.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            release(region, self.slot);
        }
    }
}

impl Memory {
    #[inline]
    fn region(&self) -> Option<&Region> {
        // SAFETY: this view's count on its slot keeps the region mapped.
        self.region.map(|r| unsafe { &*r.as_ptr() })
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.region().map_or(0, |r| r.slot_size - self.offset as usize)
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn data(&self) -> &[u8] {
        let r = self.region().expect("memory is live");
        // SAFETY: the window lies inside the mapping, and a write needs the one view, which `&self`
        // excludes.
        unsafe {
            std::slice::from_raw_parts(
                r.slot_ptr(self.slot).add(self.offset as usize),
                self.len as usize,
            )
        }
    }

    #[inline]
    pub fn slot_id(&self) -> Option<SlotId> {
        let region = self.region?.as_ptr() as usize;
        Some(SlotId { region, slot: self.slot })
    }

    /// The pool the slot belongs to, `None` for an empty husk or a buffer of no pool.
    pub fn pool_id(&self) -> Option<PoolId> {
        let r = self.region()?;
        (r.pool.strong_count() > 0).then_some(PoolId(r.pool.as_ptr() as usize))
    }

    /// Whether this is the one view of its slot, so writing to it copies nothing.
    #[inline]
    pub fn is_unique(&self) -> bool {
        let Some(r) = self.region() else { return false };
        r.counts[self.slot as usize].load(Ordering::Acquire) == 1
    }

    /// Allocator context, registered token and window offset in the region, for device export.
    pub fn device(&self) -> Option<(*mut (), u64, usize)> {
        let r = self.region()?;
        let offset = self.slot as usize * r.stride + self.offset as usize;
        (r.token != 0).then_some((r.allocator.ctx, r.token, offset))
    }

    /// The whole window for filling, copied into a fresh slot first when shared. That copy is
    /// a heap buffer when the pool has no slot to spare, so a shared device slot written
    /// through here may come back with `device()` of `None`.
    #[inline]
    pub fn as_mut_full(&mut self) -> &mut [u8] {
        let region = self.region.expect("memory is live");
        // SAFETY: this view's count on its slot keeps the region mapped.
        let r = unsafe { region.as_ref() };
        // We hold one count, and a count only grows through a view, so one is ours alone.
        if r.counts[self.slot as usize].load(Ordering::Acquire) != 1 {
            self.unshare();
        }
        let r = self.region().expect("memory is live");
        let (ptr, len) = (r.slot_ptr(self.slot), r.slot_size - self.offset as usize);
        // SAFETY: the one view, checked or just made, and `&mut self` for its lifetime.
        unsafe { std::slice::from_raw_parts_mut(ptr.add(self.offset as usize), len) }
    }

    #[cold]
    #[inline(never)]
    fn unshare(&mut self) {
        let region = self.region.expect("memory is live");
        // SAFETY: this view's count on its slot keeps the region mapped.
        let r = unsafe { region.as_ref() };
        let want = r.slot_size;
        // A copy of a slot the pool already mapped, so only a heap out of memory refuses it.
        let fresh = match (r.kind, r.pool.upgrade()) {
            (Kind::Small(_), Some(inner)) => {
                Pool { inner }.small_free(want).or_else(|| exact(want))
            }
            (Kind::Main, Some(inner)) => Pool { inner }.acquire_exact(want).or_else(|| exact(want)),
            _ => exact(want),
        };
        let mut fresh = fresh.expect("the heap maps a copy of a slot");
        let n = want.min(fresh.capacity());
        // SAFETY: a read of the whole slot, which other views may read too.
        let whole = unsafe { std::slice::from_raw_parts(r.slot_ptr(self.slot), n) };
        fresh.as_mut_full()[..n].copy_from_slice(whole);
        fresh.offset = self.offset.min(n as u32);
        fresh.len = self.len.min(n as u32 - fresh.offset);
        if self.side != 0 {
            let _ = fresh.set_side(self.side());
        }
        *self = fresh;
    }

    /// The used bytes as native-endian 16 bit words, a trailing odd byte left out.
    #[inline]
    pub fn words(&self) -> Result<&[u16], Misaligned> {
        let bytes = self.data();
        let address = bytes.as_ptr() as usize;
        if !address.is_multiple_of(align_of::<u16>()) {
            return Err(Misaligned { address });
        }
        // SAFETY: aligned just above, in bounds of `bytes`, and every bit pattern is a u16.
        Ok(unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u16>(), bytes.len() / 2) })
    }

    /// [`as_mut_full`](Self::as_mut_full) as native-endian 16 bit words.
    #[inline]
    pub fn as_mut_full_words(&mut self) -> Result<&mut [u16], Misaligned> {
        let bytes = self.as_mut_full();
        let address = bytes.as_ptr() as usize;
        if !address.is_multiple_of(align_of::<u16>()) {
            return Err(Misaligned { address });
        }
        let n = bytes.len() / 2;
        // SAFETY: aligned just above, in bounds of the one exclusive view, any bit pattern is a
        // u16.
        Ok(unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<u16>(), n) })
    }

    pub fn set_len(&mut self, n: usize) {
        self.len = n.min(self.capacity()).min(u32::MAX as usize) as u32;
    }

    /// The side data this view states, empty for none.
    #[inline]
    pub fn side(&self) -> &[u8] {
        let Some(side) = self.region().filter(|_| self.side != 0).and_then(|r| r.side.get()) else {
            return &[];
        };
        // SAFETY: written once under the claim before this view stated it, and not again until the
        // slot's release.
        unsafe { std::slice::from_raw_parts(side.at(self.slot), self.side as usize) }
    }

    /// Put `bytes` in the slot's side area for this view and its later clones, never overwriting
    /// another view's.
    pub fn set_side(&mut self, bytes: &[u8]) -> Result<(), SideError> {
        if bytes.is_empty() {
            return Err(SideError::Empty);
        }
        if bytes.len() > SIDE_CAP {
            return Err(SideError::TooLarge { len: bytes.len() });
        }
        let r = self.region().ok_or(SideError::Taken)?;
        let side = r.side.get_or_init(|| {
            let side = SideArena::new(r.counts.len());
            if let Some(pool) = r.pool.upgrade() {
                pool.bytes_mapped.fetch_add(side.len(), Ordering::Relaxed);
            }
            side
        });
        let claim = &side.claims[self.slot as usize];
        if claim.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return Err(SideError::Taken);
        }
        // SAFETY: the claim makes this the one writer, and no view reads it before `self.side` is
        // set.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), side.at(self.slot), bytes.len()) };
        self.side = bytes.len() as u32;
        Ok(())
    }

    /// The view over its whole slot again, with no bytes used.
    pub fn widen(&mut self) {
        (self.offset, self.len, self.side) = (0, 0, 0);
    }

    /// A zero-copy sub-view of the used bytes, `None` when out of bounds.
    pub fn slice(&self, offset: usize, len: usize) -> Option<Memory> {
        let end = offset.checked_add(len)?;
        if end > self.len as usize {
            return None;
        }
        let mut view = self.clone();
        view.offset += offset as u32;
        view.len = len as u32;
        view.side = 0;
        Some(view)
    }

    pub(crate) fn take(&mut self) -> Memory {
        Memory {
            region: self.region.take(),
            slot: self.slot,
            offset: std::mem::replace(&mut self.offset, 0),
            len: std::mem::replace(&mut self.len, 0),
            side: std::mem::replace(&mut self.side, 0),
        }
    }
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("len", &self.len)
            .field("capacity", &self.capacity())
            .finish()
    }
}

impl Clone for Memory {
    fn clone(&self) -> Self {
        if let Some(r) = self.region() {
            r.counts[self.slot as usize].fetch_add(1, Ordering::Relaxed);
        }
        Memory {
            region: self.region,
            slot: self.slot,
            offset: self.offset,
            len: self.len,
            side: self.side,
        }
    }
}

/// Per-buffer fence for device memory, not yet wired.
#[derive(Clone)]
pub struct SyncPoint {
    _priv: (),
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    #[test]
    fn words_view_the_bytes_and_write_through_the_one_view() {
        let pool = Pool::bounded(256, 2);
        let mut m = pool.try_acquire().unwrap();
        let w = m.as_mut_full_words().unwrap();
        assert_eq!(w.len(), 128);
        w[0] = 0x0201;
        w[127] = 0xfffe;
        m.set_len(256);
        assert_eq!(&m.data()[..2], &0x0201u16.to_ne_bytes());
        assert_eq!(m.words().unwrap()[127], 0xfffe);
        let odd = m.slice(1, 4).unwrap();
        assert!(odd.words().is_err(), "an odd offset is no word boundary");
        let even = m.slice(2, 5).unwrap();
        assert_eq!(even.words().unwrap().len(), 2, "the odd tail byte is left out");
    }

    #[test]
    fn views_of_one_slot_share_its_id_and_name_their_pool() {
        let pool = Pool::bounded(64, 4);
        let a = pool.try_acquire().unwrap();
        let b = pool.try_acquire().unwrap();
        assert_eq!(a.slot_id(), a.clone().slot_id());
        assert_eq!(a.slot_id(), a.slice(0, 0).and_then(|s| s.slot_id()));
        assert_ne!(a.slot_id(), b.slot_id());
        assert_eq!(a.pool_id(), Some(pool.id()));
        let other = Pool::bounded(64, 1);
        assert_ne!(other.try_acquire().unwrap().pool_id(), Some(pool.id()));
    }

    #[test]
    fn a_view_is_unique_until_cloned_and_again_once_the_clone_goes() {
        let pool = Pool::bounded(64, 2);
        let a = pool.try_acquire().unwrap();
        assert!(a.is_unique());
        let b = a.clone();
        assert!(!a.is_unique() && !b.is_unique());
        drop(b);
        assert!(a.is_unique());
    }

    #[test]
    fn words_of_a_shared_view_copy_before_writing() {
        let pool = Pool::bounded(64, 4);
        let mut a = pool.try_acquire().unwrap();
        a.as_mut_full_words().unwrap()[0] = 7;
        a.set_len(64);
        let b = a.clone();
        a.as_mut_full_words().unwrap()[0] = 9;
        assert_eq!(b.words().unwrap()[0], 7, "the reader keeps its words");
        assert_eq!(a.words().unwrap()[0], 9);
    }

    use super::*;

    #[test]
    fn region_names_carry_the_owner_sanitised_and_the_class() {
        let mut buf = [0u8; NAME_MAX];
        assert_eq!(region_name(&mut buf, &Owner::new("mkvdemux"), 4 << 20), c"pf:pool:mkvdemux:4M");
        assert_eq!(region_name(&mut buf, &Owner::new("dec"), 3 << 10), c"pf:pool:dec:3K");
        assert_eq!(region_name(&mut buf, &Owner::new("dec"), 1500), c"pf:pool:dec:1500");
        assert_eq!(region_name(&mut buf, &Owner::new("dec"), 0), c"pf:pool:dec:0");
        let odd = Owner::new("a b[c]$d`e\\fé");
        assert_eq!(region_name(&mut buf, &odd, 64), c"pf:pool:a_b_c__d_e_f__:64");
        let long = Owner::new(&"x".repeat(200));
        let name = region_name(&mut buf, &long, usize::MAX).to_bytes();
        assert!(name.len() < NAME_MAX, "{}", name.len());
        assert!(name.ends_with(b":18446744073709551615"));
    }

    #[test]
    fn views_share_the_backing_until_the_last_drops() {
        let pool = Pool::bounded(64, 1);
        let mut a = pool.acquire();
        a.as_mut_full()[..4].copy_from_slice(b"abcd");
        a.set_len(4);
        let b = a.clone();
        assert_eq!(b.data(), b"abcd");
        assert!(pool.try_acquire().is_none(), "one slot, still outstanding");
        drop(a);
        assert!(pool.try_acquire().is_none(), "a clone still holds the backing");
        drop(b);
        assert_eq!(pool.stats().recycles, 1, "recycled once, not per view");
        assert!(pool.try_acquire().is_some(), "last view recycled the slot");
    }

    #[test]
    fn slice_is_a_zero_copy_window_and_never_panics_on_bad_ranges() {
        let pool = Pool::new(64);
        let mut m = pool.acquire();
        m.as_mut_full()[..8].copy_from_slice(b"01234567");
        m.set_len(8);
        let s = m.slice(2, 4).expect("in bounds");
        assert_eq!(s.data(), b"2345");
        let ss = s.slice(1, 2).expect("slice of slice");
        assert_eq!(ss.data(), b"34");
        assert!(m.slice(7, 2).is_none());
        assert!(m.slice(9, 0).is_none());
        assert!(m.slice(usize::MAX, 2).is_none());
        drop(m);
        assert_eq!(ss.data(), b"34");
    }

    #[test]
    fn a_widened_slice_has_its_whole_slot_and_a_shared_one_copies_to_write() {
        let pool = Pool::new(64);
        let mut m = pool.acquire();
        m.as_mut_full()[..8].copy_from_slice(b"01234567");
        m.set_len(8);
        let mut s = m.slice(4, 2).expect("in bounds");
        assert_eq!(s.capacity(), 60);
        s.widen();
        assert_eq!((s.len(), s.capacity()), (0, 64));
        s.as_mut_full()[0] = b'x';
        assert_eq!(m.data(), b"01234567", "the other view keeps its bytes");
        drop(m);
        let mut alone = s;
        alone.widen();
        alone.as_mut_full()[0] = b'y';
        alone.set_len(1);
        assert_eq!(alone.data(), b"y");
    }

    #[test]
    fn mutating_a_shared_view_copies_on_write() {
        let pool = Pool::new(64);
        let mut a = pool.acquire();
        a.as_mut_full()[..4].copy_from_slice(b"aaaa");
        a.set_len(4);
        let b = a.clone();
        a.as_mut_full()[..4].copy_from_slice(b"zzzz");
        assert_eq!(a.data(), b"zzzz");
        assert_eq!(b.data(), b"aaaa", "sharer unaffected by the CoW mutation");
        let before = pool.stats().slot_allocations;
        a.as_mut_full()[0] = b'q';
        assert_eq!(pool.stats().slot_allocations, before, "unique path is in-place");
    }

    #[test]
    fn taken_husk_reads_empty_and_recycles_nothing() {
        let pool = Pool::bounded(64, 1);
        let mut m = pool.acquire();
        m.set_len(3);
        let moved = m.take();
        assert_eq!(m.len(), 0);
        assert_eq!(m.capacity(), 0);
        drop(m);
        assert!(pool.try_acquire().is_none(), "moved view still owns the slot");
        drop(moved);
        assert!(pool.try_acquire().is_some());
    }

    #[test]
    fn recycles_slots_flat_allocation() {
        let pool = Pool::new(1024);
        for _ in 0..100 {
            let mut m = pool.acquire();
            assert_eq!(m.capacity(), 1024);
            m.set_len(10);
            drop(m);
        }
        let s = pool.stats();
        assert_eq!(s.slot_allocations, 1, "one region, recycled 99 times");
        assert_eq!(s.high_water, 1);
        assert_eq!(s.acquires, 100);
        assert_eq!(s.recycles, 100);
        assert_eq!((s.regions, s.bytes_mapped), (1, 1024 * region_slots(1024) as u64));
    }

    #[test]
    fn regions_grow_by_slots_up_to_the_cap() {
        let per = region_slots(64);
        let pool = Pool::bounded(64, per + 3);
        let held: Vec<Memory> =
            (0..per + 3).map(|_| pool.try_acquire().expect("under the cap")).collect();
        let s = pool.stats();
        assert_eq!(
            (s.regions, s.slot_allocations),
            (2, 2),
            "two regions, the second of three slots"
        );
        assert_eq!(s.bytes_mapped, 64 * (per as u64 + 3), "the cap bounds what is mapped");
        assert!(pool.try_acquire().is_none(), "cap reached");
        drop(held);
        assert_eq!(pool.stats().bytes_mapped, 64 * (per as u64 + 3), "regions stay for reuse");
        let again: Vec<Memory> =
            (0..per + 3).map(|_| pool.try_acquire().expect("reused")).collect();
        assert_eq!(pool.stats().slot_allocations, 2, "no new region");
        drop(again);
    }

    #[test]
    fn big_slot_regions_map_a_slot_at_a_time() {
        assert_eq!(region_slots(64), SMALL_REGION_SLOTS, "a few regions of packets, not hundreds");
        assert_eq!(region_slots(4096), 16, "64 KiB of pages");
        assert_eq!(region_slots(8192), REGION_SLOTS);
        assert_eq!(region_slots(REGION_BYTES / REGION_SLOTS as usize), REGION_SLOTS);
        assert_eq!(region_slots(4 << 20), 4);
        assert_eq!(region_slots(6 << 20), 2);
        assert_eq!(region_slots(12 << 20), 1);
        assert_eq!(region_slots(64 << 20), 1);
        let slot = REGION_BYTES / 2;
        let pool = Pool::bounded(slot, 8);
        let a = pool.try_acquire().expect("one");
        assert_eq!(
            pool.stats().bytes_mapped,
            2 * slot as u64,
            "two slots of half the region bytes"
        );
        let b = pool.try_acquire().expect("two");
        assert_eq!(pool.stats().regions, 1);
        let c = pool.try_acquire().expect("three");
        assert_eq!((pool.stats().regions, pool.stats().bytes_mapped), (2, 4 * slot as u64));
        drop((a, b, c));
    }

    #[test]
    fn the_cap_grows_on_request() {
        let pool = Pool::bounded(64, 2);
        let held: Vec<Memory> =
            (0..2).map(|_| pool.try_acquire().expect("under the cap")).collect();
        assert!(pool.try_acquire().is_none(), "cap reached");
        assert!(pool.grow_cap(4), "raised");
        assert!(!pool.grow_cap(3), "never lowered");
        assert_eq!((pool.max_slots(), pool.stats().max_slots, pool.free_slots()), (4, 4, 2));
        let more: Vec<Memory> =
            (0..2).map(|_| pool.try_acquire().expect("under the raised cap")).collect();
        assert!(pool.try_acquire().is_none(), "the raised cap holds");
        drop((held, more));
    }

    #[test]
    fn slots_are_aligned() {
        let pool = Pool::new(100);
        let a = pool.acquire();
        let b = pool.acquire();
        assert_eq!(a.data().as_ptr() as usize % MIN_ALIGN, 0);
        assert_eq!(b.data().as_ptr() as usize % MIN_ALIGN, 0);
        assert_eq!(a.capacity(), 100, "the window is the slot size, not the stride");
        let pool = Pool::with_config(PoolConfig { align: 4096, ..PoolConfig::heap(100, 4) });
        let page = pool.acquire();
        assert_eq!(page.data().as_ptr() as usize % 4096, 0);
    }

    #[test]
    fn resize_drains_the_old_regions_under_live_views() {
        let pool = Pool::bounded(64, 4);
        let mut old = pool.acquire();
        old.as_mut_full()[..3].copy_from_slice(b"old");
        old.set_len(3);
        let twin = old.clone();
        assert_eq!(pool.stats().bytes_mapped, 64 * 4);
        pool.resize(256);
        assert_eq!(pool.slot_size(), 256);
        let s = pool.stats();
        assert_eq!(
            (s.regions, s.bytes_mapped, s.outstanding),
            (0, 64 * 4, 1),
            "draining, still mapped, still out"
        );
        let fresh = pool.try_acquire().expect("a slot at the new size");
        assert_eq!(fresh.capacity(), 256);
        assert_eq!(
            pool.stats().bytes_mapped,
            64 * 4 + 256 * 3,
            "the slot out of the old region counts against the cap"
        );
        assert_eq!(old.data(), b"old", "a view into a draining region reads");
        old.as_mut_full()[0] = b'O';
        assert_eq!((old.data(), twin.data()), (&b"Old"[..], &b"old"[..]));
        assert_eq!(old.capacity(), 256, "copy-on-write landed in the new size");
        drop(old);
        assert_eq!(
            pool.stats().bytes_mapped,
            64 * 4 + 256 * 3,
            "the twin still holds the old region"
        );
        drop(twin);
        assert_eq!(pool.stats().bytes_mapped, 256 * 3, "unmapped at the last release");
        assert_eq!(pool.stats().outstanding, 1);
        drop(fresh);
        assert_eq!(pool.stats().outstanding, 0);
        assert_eq!(pool.free_slots(), 4);
    }

    #[test]
    fn retire_releases_everything_as_the_last_views_drop() {
        let pool = Pool::bounded(4096, 8);
        let kept = pool.acquire();
        let small = pool.acquire_exact(10).expect("exact");
        drop(pool.acquire_exact(10).expect("exact"));
        drop(pool.acquire());
        let before = pool.stats();
        assert_eq!(before.regions, 2, "one main, one small");
        pool.retire();
        let s = pool.stats();
        assert_eq!((s.regions, s.max_slots), (0, 0));
        assert_eq!(s.bytes_mapped, before.bytes_mapped, "held, so still mapped");
        assert!(pool.try_acquire().is_none());
        assert_eq!(pool.acquire().capacity(), 4096, "the unbounded path goes to the heap");
        drop(small);
        assert_eq!(pool.stats().bytes_mapped, 4096 * 8, "the small region went with its last slot");
        drop(kept);
        assert_eq!(pool.stats().bytes_mapped, 0, "and the main one");
        assert_eq!(pool.stats().outstanding, 0);
    }

    #[test]
    fn views_outlive_their_pool() {
        let pool = Pool::bounded(64, 2);
        let mut m = pool.acquire();
        m.as_mut_full()[..2].copy_from_slice(b"hi");
        m.set_len(2);
        let n = m.clone();
        drop(pool);
        assert_eq!(n.data(), b"hi");
        m.as_mut_full()[0] = b'H';
        assert_eq!((m.data(), n.data()), (&b"Hi"[..], &b"hi"[..]));
        drop((m, n));
    }

    #[test]
    fn a_custom_allocator_is_called_once_per_region() {
        use std::sync::atomic::AtomicUsize;
        static MAPS: AtomicUsize = AtomicUsize::new(0);
        static UNMAPS: AtomicUsize = AtomicUsize::new(0);
        static STRIDE: AtomicUsize = AtomicUsize::new(0);
        unsafe fn map(ctx: *mut (), len: usize, align: usize, name: &CStr) -> *mut u8 {
            MAPS.fetch_add(1, Ordering::Relaxed);
            unsafe { heap_map(ctx, len, align, name) }
        }
        unsafe fn unmap(ctx: *mut (), base: *mut u8, len: usize, align: usize, token: u64) {
            assert_eq!(token, 7);
            UNMAPS.fetch_add(1, Ordering::Relaxed);
            unsafe { heap_unmap(ctx, base, len, align, token) }
        }
        unsafe fn register(_: *mut (), _: *mut u8, _: usize, stride: usize) -> Option<u64> {
            STRIDE.store(stride, Ordering::Relaxed);
            Some(7)
        }
        let allocator =
            Allocator { ctx: std::ptr::null_mut(), map, unmap, register: Some(register) };
        let pool = Pool::with_config(PoolConfig { allocator, ..PoolConfig::heap(100, 2) });
        let a = pool.acquire();
        let b = pool.acquire();
        assert_eq!(MAPS.load(Ordering::Relaxed), 1, "one region for two slots");
        assert_eq!(STRIDE.load(Ordering::Relaxed), 128, "the slot rounded to the alignment");
        let null = std::ptr::null_mut();
        assert_eq!(a.device(), Some((null, 7, 0)));
        assert_eq!(b.device(), Some((null, 7, 128)));
        assert_eq!(b.slice(0, 0).unwrap().device(), Some((null, 7, 128)));
        assert!(
            pool.acquire_exact(10).expect("exact").device().is_none(),
            "small buffers are heap regions"
        );
        drop((a, b));
        assert_eq!(UNMAPS.load(Ordering::Relaxed), 0, "kept for reuse");
        drop(pool);
        assert_eq!(UNMAPS.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_allocator_runs_with_the_pool_unlocked() {
        static POOL: std::sync::OnceLock<Pool> = std::sync::OnceLock::new();
        // Both take the pool lock, so a map or unmap under it would spin forever here.
        unsafe fn register(_: *mut (), _: *mut u8, _: usize, _: usize) -> Option<u64> {
            Some(1 + POOL.get().map_or(0, |p| p.stats().regions))
        }
        unsafe fn unmap(ctx: *mut (), base: *mut u8, len: usize, align: usize, token: u64) {
            let _ = POOL.get().map(Pool::in_use);
            unsafe { heap_unmap(ctx, base, len, align, token) }
        }
        let allocator = Allocator { register: Some(register), unmap, ..Allocator::HEAP };
        let pool = POOL
            .get_or_init(|| Pool::with_config(PoolConfig { allocator, ..PoolConfig::heap(64, 2) }));
        let a = pool.try_acquire().expect("mapped with the lock free");
        assert_eq!(a.device().map(|d| d.1), Some(1), "registered with the lock free");
        drop(a);
        pool.resize(128);
        assert_eq!(pool.stats().regions, 0, "the old region was unmapped after the resize");
        let b = pool.try_acquire().expect("a region of the new size");
        drop(b);
        pool.retire();
        assert_eq!(pool.stats().bytes_mapped, 0);
    }

    #[test]
    fn a_failed_registration_maps_nothing() {
        unsafe fn refuse(_: *mut (), _: *mut u8, _: usize, _: usize) -> Option<u64> {
            None
        }
        let allocator = Allocator { register: Some(refuse), ..Allocator::HEAP };
        let pool = Pool::with_config(PoolConfig { allocator, ..PoolConfig::heap(64, 2) });
        assert!(pool.try_acquire().is_none());
        assert_eq!(pool.stats().bytes_mapped, 0);
    }

    #[test]
    fn concurrent_outstanding_allocates_more() {
        let pool = Pool::new(8);
        let per = region_slots(8);
        let held: Vec<Memory> = (0..per + 1).map(|_| pool.acquire()).collect();
        assert_eq!(pool.stats().slot_allocations, 2, "a second region past the first's slots");
        assert_eq!(pool.stats().high_water, per as u64 + 1);
        drop(held);
        let _c = pool.acquire();
        assert_eq!(pool.stats().slot_allocations, 2);
    }

    #[test]
    fn data_roundtrip() {
        let pool = Pool::new(16);
        let mut m = pool.acquire();
        m.as_mut_full()[..3].copy_from_slice(&[1, 2, 3]);
        m.set_len(3);
        assert_eq!(m.data(), &[1, 2, 3]);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn set_len_clamps_to_capacity() {
        let pool = Pool::new(4);
        let mut m = pool.acquire();
        m.set_len(999);
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn a_group_enlisted_on_a_pool_with_slots_free_is_woken_by_the_next_return() {
        let pool = Pool::bounded(64, 4);
        let station = Arc::new(Station::new());
        let held = pool.try_acquire().unwrap();
        let lease = held.clone();
        let seen = station.epoch();
        pool.enlist(&station);
        assert!(pool.free_slots() > 0);
        assert_eq!(station.epoch(), seen, "enlisting is no wake");
        drop(held);
        assert_eq!(station.epoch(), seen, "the slot is still out, as its lease");
        drop(lease);
        assert_ne!(station.epoch(), seen, "the slot's return signals the station");
        let seen = station.epoch();
        drop(pool.try_acquire().unwrap());
        assert_eq!(station.epoch(), seen);
    }

    #[test]
    fn bounded_pool_backpressure() {
        let pool = Pool::bounded(8, 2);
        assert_eq!(pool.free_slots(), 2);
        let a = pool.try_acquire().expect("slot 1");
        let b = pool.try_acquire().expect("slot 2");
        assert_eq!(pool.free_slots(), 0);
        assert!(pool.try_acquire().is_none(), "cap reached, backpressure");
        drop(a);
        assert_eq!(pool.free_slots(), 1);
        let _c = pool.try_acquire().expect("slot freed after drop");
        assert!(pool.try_acquire().is_none());
        drop(b);
    }

    #[test]
    fn an_exact_buffer_past_the_cap_is_refused_not_mapped() {
        let pool = Pool::new(4096);
        assert!(pool.acquire_exact(EXACT_MAX + 1).is_none());
        assert!(pool.acquire_exact(usize::MAX).is_none(), "no overflow on the way to the heap");
        assert_eq!(pool.acquire_exact(EXACT_MAX).map(|m| m.capacity() >= EXACT_MAX), Some(true));
    }

    #[test]
    fn acquire_exact_recycles_small_buffers() {
        let pool = Pool::bounded(64 * 1024, 4);
        let warm = pool.acquire_exact(212).expect("exact");
        drop(warm);
        let after_warm = pool.stats().slot_allocations;
        for _ in 0..1000 {
            let mut m = pool.acquire_exact(212).expect("exact");
            assert!(m.capacity() >= 212, "at least the requested bytes");
            m.set_len(212);
            drop(m);
        }
        let stats = pool.stats();
        assert_eq!(stats.slot_allocations, after_warm, "steady state allocates nothing");
        assert_eq!(stats.outstanding, 0, "small buffers never enter the slot budget");
        assert_eq!(pool.free_slots(), 4, "every slot still available");
    }

    #[test]
    fn class_regions_are_sized_by_bytes_and_a_burst_past_the_budget_is_not_kept() {
        let budget = 256 * 1024;
        let pool =
            Pool::with_config(PoolConfig { bytes: budget, ..PoolConfig::heap(64 * 1024, 4) });
        let one: Vec<Memory> = (0..32).map(|_| pool.acquire_exact(212).expect("exact")).collect();
        assert_eq!(
            (pool.stats().regions, pool.stats().bytes_mapped),
            (1, 32 * 256),
            "a first region of 32 slots"
        );
        let burst: Vec<Memory> =
            (32..992).map(|_| pool.acquire_exact(212).expect("exact")).collect();
        let doubling = 256 * (32 + 64 + 128 + 256 + 512);
        assert_eq!(
            (pool.stats().regions, pool.stats().bytes_mapped),
            (5, doubling),
            "each doubling, within the budget"
        );
        let more: Vec<Memory> = (0..8).map(|_| pool.acquire_exact(212).expect("exact")).collect();
        assert_eq!(
            (pool.stats().regions, pool.stats().bytes_mapped),
            (6, doubling + 256 * 1024),
            "a capped one past the budget"
        );
        drop(more);
        assert_eq!(pool.stats().regions, 5, "the burst region went when it emptied");
        drop((one, burst));
        assert_eq!(
            (pool.stats().regions, pool.stats().bytes_mapped),
            (5, doubling),
            "those within the budget are kept"
        );
        let pool = Pool::with_config(PoolConfig { bytes: 1 << 20, ..PoolConfig::heap(4096, 4) });
        let big = pool.try_acquire_sized(300 * 1024).unwrap().expect("within the budget");
        assert_eq!(big.capacity(), 320 * 1024, "a quarter octave above 256 KiB");
        assert_eq!(pool.stats().bytes_mapped, 320 * 1024, "one slot, one region");
        drop(big);
    }

    #[test]
    fn class_regions_within_the_budget_are_kept() {
        let pool = Pool::with_config(PoolConfig { bytes: 4 << 20, ..PoolConfig::heap(4096, 8) });
        let held: Vec<Memory> =
            (0..4).map(|_| pool.try_acquire_sized(500 * 1024).unwrap().unwrap()).collect();
        assert_eq!(pool.stats().regions, 4, "four one-slot regions of the 512 KiB class");
        drop(held);
        assert_eq!(pool.stats().regions, 4, "all within the budget, all kept");
        let before = pool.stats().slot_allocations;
        for _ in 0..100 {
            let again: Vec<Memory> =
                (0..4).map(|_| pool.try_acquire_sized(500 * 1024).unwrap().unwrap()).collect();
            drop(again);
        }
        assert_eq!(pool.stats().slot_allocations, before, "steady state maps nothing");
    }

    #[test]
    fn a_reserve_survives_release_past_the_budget_and_maps_nothing_per_buffer() {
        // Class bytes are past the budget from the first sample.
        let pool = Pool::with_config(PoolConfig { bytes: 6 << 20, ..PoolConfig::heap(64, 24) });
        pool.reserve_sized(3 << 20, 4).unwrap();
        let (allocations, mapped) = (pool.stats().slot_allocations, pool.stats().bytes_mapped);
        assert_eq!((allocations, mapped), (1, 4 * (3 << 20)), "one region of four 3 MiB slots");
        let sizes = [3 << 20, 900 << 10, 320 << 10, 1500 << 10, 2 << 20, 700 << 10];
        for round in 0..200 {
            let a = pool.try_acquire_sized(sizes[round % 6]).unwrap().expect("a reserved slot");
            let b = pool.try_acquire_sized(sizes[(round + 1) % 6]).unwrap().expect("another");
            assert!(a.capacity() >= sizes[round % 6]);
            drop((a, b));
        }
        let s = pool.stats();
        assert_eq!(s.slot_allocations, allocations, "nothing mapped past the reserve");
        assert_eq!(s.bytes_mapped, mapped, "and nothing unmapped");
        assert_eq!((s.bytes_out, s.outstanding), (0, 0), "every charge came back");
    }

    #[cfg(target_os = "linux")]
    fn resident() -> u64 {
        // VmRSS is in kB, statm counts pages of a size that is not 4096 on every kernel.
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
        line.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap() * 1024
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_heap_region_is_not_resident_until_written() {
        let pool = Pool::with_config(PoolConfig { bytes: 16 << 20, ..PoolConfig::heap(64, 64) });
        let before = resident();
        pool.reserve_sized(16 << 20, 64).unwrap();
        let grown = resident().saturating_sub(before);
        assert_eq!(pool.stats().bytes_mapped, 1 << 30);
        assert!(grown < 64 << 20, "a 1 GiB reserve made {grown} bytes resident");
        let mut m = pool.try_acquire_sized(16 << 20).unwrap().unwrap();
        assert!(m.as_mut_full().iter().all(|&b| b == 0), "and reads zero");
        assert_eq!(m.as_mut_full().as_ptr() as usize % MIN_ALIGN, 0);
    }

    #[test]
    fn a_reserve_reuses_the_slot_already_touched_for_a_size() {
        let pool = Pool::with_config(PoolConfig { bytes: 64 << 20, ..PoolConfig::heap(64, 24) });
        pool.reserve_sized(4 << 20, 8).unwrap();
        let key = pool.try_acquire_sized(3 << 20).unwrap().unwrap();
        let small: Vec<Memory> =
            (0..2).map(|_| pool.try_acquire_sized(300 << 10).unwrap().unwrap()).collect();
        let (key_slot, small_slots) =
            (key.slot_id(), small.iter().map(Memory::slot_id).collect::<Vec<_>>());
        drop((key, small));
        let small: Vec<Memory> =
            (0..2).map(|_| pool.try_acquire_sized(300 << 10).unwrap().unwrap()).collect();
        let key = pool.try_acquire_sized(3 << 20).unwrap().unwrap();
        assert_eq!(key.slot_id(), key_slot, "a keyframe where a keyframe was");
        let mut again: Vec<_> = small.iter().map(Memory::slot_id).collect();
        again.sort();
        let mut before = small_slots;
        before.sort();
        assert_eq!(again, before, "the small frames where small ones were, no third slot touched");
    }

    #[test]
    fn a_reserved_buffer_is_charged_at_its_own_class() {
        let pool = Pool::with_config(PoolConfig { bytes: 4 << 20, ..PoolConfig::heap(64, 24) });
        pool.reserve_sized(3 << 20, 8).unwrap();
        let held: Vec<Memory> =
            (0..4).map(|_| pool.try_acquire_sized(1 << 20).unwrap().unwrap()).collect();
        assert_eq!(pool.stats().bytes_out, 4 << 20, "four 1 MiB classes, not four 3 MiB slots");
        assert!(pool.try_acquire_sized(1 << 20).unwrap().is_none(), "the budget refuses a fifth");
        assert_eq!(pool.stats().slot_allocations, 1, "all four from the reserve");
        drop(held);
        assert_eq!(pool.stats().bytes_out, 0);
    }

    #[test]
    fn a_burst_past_a_reserve_goes_at_its_release() {
        let pool = Pool::with_config(PoolConfig { bytes: 64 << 20, ..PoolConfig::heap(64, 24) });
        pool.reserve_sized(2 << 20, 2).unwrap();
        let held: Vec<Memory> =
            (0..3).map(|_| pool.try_acquire_sized(2 << 20).unwrap().unwrap()).collect();
        let s = pool.stats();
        assert_eq!((s.regions, s.slot_allocations), (2, 2), "the third maps a burst region");
        drop(held);
        let s = pool.stats();
        assert_eq!(
            (s.regions, s.bytes_mapped),
            (1, 2 * (2 << 20)),
            "the burst went, the reserve stays"
        );
        let big = pool.try_acquire_sized(4 << 20).unwrap().unwrap();
        let small = pool.try_acquire_sized(1000).unwrap().unwrap();
        assert_eq!((big.capacity(), small.capacity()), (4 << 20, 1024));
        assert_eq!(pool.stats().slot_allocations, 4);
    }

    #[test]
    fn an_unreserved_big_class_still_sheds() {
        let pool = Pool::with_config(PoolConfig { bytes: 2 << 20, ..PoolConfig::heap(64, 24) });
        drop(pool.try_acquire_sized(512 << 10).unwrap().unwrap());
        for _ in 0..10 {
            let held: Vec<Memory> =
                (0..2).map(|_| pool.try_acquire_sized(1 << 20).unwrap().unwrap()).collect();
            drop(held);
        }
        let s = pool.stats();
        assert_eq!(s.regions, 2, "the second 1 MiB region goes, class bytes past the budget");
        assert_eq!(s.slot_allocations, 12, "and is mapped again each time, what a reserve avoids");
    }

    #[test]
    fn a_reserve_past_the_budget_or_the_top_is_refused() {
        let pool = Pool::with_config(PoolConfig { bytes: 1 << 20, ..PoolConfig::heap(64, 24) });
        let refused = pool.reserve_sized(2 << 20, 4);
        assert_eq!(refused, Err(ReserveError::TooLarge(TooLarge { n: 2 << 20, max: 1 << 20 })));
        let wide = Pool::with_config(PoolConfig { bytes: u64::MAX, ..PoolConfig::heap(64, 24) });
        assert!(matches!(wide.reserve_sized(SIZED_TOP + 1, 1), Err(ReserveError::TooLarge(_))));
        assert_eq!(
            pool.stats().slot_allocations + wide.stats().slot_allocations,
            0,
            "nothing mapped"
        );
    }

    #[test]
    fn the_byte_budget_bounds_sized_buffers() {
        let pool = Pool::with_config(PoolConfig { bytes: 3000, ..PoolConfig::heap(64, 64) });
        let a = pool.try_acquire_sized(1000).unwrap().expect("1024 of 3000");
        assert_eq!(a.capacity(), 1024, "the class size, up to 2x the request");
        let b = pool.try_acquire_sized(1000).unwrap().expect("2048 of 3000");
        assert_eq!(pool.stats().bytes_out, 2048);
        assert!(pool.try_acquire_sized(1000).unwrap().is_none(), "a third would pass the budget");
        assert!(
            pool.try_acquire_sized(500).unwrap().is_some(),
            "952 bytes remain, so a 512 class fits"
        );
        drop(a);
        assert_eq!(pool.stats().bytes_out, 1024, "the release returned its class bytes");
        assert!(pool.try_acquire_sized(1000).unwrap().is_some(), "and the budget serves again");
        assert_eq!(
            pool.try_acquire_sized(2561).err(),
            Some(TooLarge { n: 2561, max: 2560 }),
            "past the budget"
        );
        assert_eq!(
            pool.try_acquire_sized(SIZED_TOP + 1).err().map(|e| e.n),
            Some(SIZED_TOP + 1),
            "past the top class"
        );
        let wide = Pool::with_config(PoolConfig { bytes: u64::MAX, ..PoolConfig::heap(64, 64) });
        assert_eq!(
            wide.try_acquire_sized(SIZED_TOP + 1).err(),
            Some(TooLarge { n: SIZED_TOP + 1, max: SIZED_TOP })
        );
        drop(b);
        assert_eq!(pool.free_slots(), 64, "the count is whole again");
        assert_eq!(pool.stats().bytes_high, 2560, "the most out at once");
    }

    #[test]
    fn the_byte_budget_holds_under_concurrent_acquirers() {
        const BUDGET: u64 = 8 * 1024;
        let pool = Pool::with_config(PoolConfig { bytes: BUDGET, ..PoolConfig::heap(64, 64) });
        let live = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                let (pool, live) = (pool.clone(), &live);
                s.spawn(move || {
                    for i in 0..5000 {
                        let n = [100usize, 700, 1500, 3000][i % 4];
                        let Some(mut m) = pool.try_acquire_sized(n).unwrap() else {
                            continue;
                        };
                        let out = live.fetch_add(m.capacity() as u64, Ordering::AcqRel)
                            + m.capacity() as u64;
                        assert!(out <= BUDGET, "{out} bytes out under a budget of {BUDGET}");
                        m.as_mut_full()[0] = i as u8;
                        m.set_len(n);
                        live.fetch_sub(m.capacity() as u64, Ordering::AcqRel);
                        drop(m);
                    }
                });
            }
        });
        assert_eq!(pool.stats().bytes_out, 0, "every byte came back");
        assert!(pool.stats().bytes_high <= BUDGET);
        let s = pool.stats();
        assert!(
            s.bytes_mapped <= BUDGET + 4 * CLASS_REGION_BYTES as u64,
            "{} mapped: a region a class over the budget at most",
            s.bytes_mapped
        );
    }

    #[test]
    fn sized_buffers_count_against_the_slot_cap() {
        let pool = Pool::with_config(PoolConfig { bytes: 1 << 20, ..PoolConfig::heap(64, 3) });
        let a = pool.try_acquire_sized(100).unwrap().expect("one of three");
        let b = pool.try_acquire().expect("two of three");
        assert_eq!(pool.free_slots(), 1);
        let c = pool.try_acquire_sized(5000).unwrap().expect("three of three");
        assert!(pool.try_acquire_sized(100).unwrap().is_none(), "at the count, sized");
        assert!(pool.try_acquire().is_none(), "at the count, a slot");
        assert_eq!((pool.stats().outstanding, pool.stats().high_water), (3, 3));
        let exact = pool.acquire_exact(100).expect("exact");
        assert_eq!(pool.stats().outstanding, 3, "the exact path is outside the count");
        drop(a);
        assert!(pool.try_acquire().is_some(), "a sized release frees the count for a slot");
        drop((b, c, exact));
        assert_eq!((pool.stats().outstanding, pool.free_slots()), (0, 3));
    }

    #[test]
    fn a_group_starved_of_bytes_is_woken_by_a_release() {
        let pool = Pool::with_config(PoolConfig { bytes: 1024, ..PoolConfig::heap(64, 4) });
        let station = Arc::new(Station::new());
        let held =
            pool.try_acquire_sized_or_enlist(1000, &station).unwrap().expect("within the budget");
        let seen = station.epoch();
        assert!(
            pool.try_acquire_sized_or_enlist(1000, &station).unwrap().is_none(),
            "starved and enlisted"
        );
        assert_eq!(station.epoch(), seen);
        drop(held);
        assert_ne!(station.epoch(), seen, "the release signals the station");
    }

    #[test]
    fn concurrent_small_buffers_leave_the_slot_budget_alone() {
        let pool = Pool::bounded(64 * 1024, 4);
        std::thread::scope(|s| {
            for _ in 0..8 {
                let pool = pool.clone();
                s.spawn(move || {
                    for i in 0..2000 {
                        let n = [40usize, 212, 1000, 5000][i % 4];
                        let mut m = pool.acquire_exact(n).expect("exact");
                        assert!(m.capacity() >= n);
                        m.set_len(n);
                    }
                });
            }
        });
        assert_eq!(pool.stats().outstanding, 0, "slot budget untouched by small traffic");
        let slots: Vec<Memory> = (0..4).map(|_| pool.try_acquire().expect("slot free")).collect();
        assert!(pool.try_acquire().is_none(), "cap still enforced");
        drop(slots);
    }

    #[test]
    fn pool_cap_holds_under_concurrent_acquirers() {
        const SLOTS: u32 = 4;
        let pool = Pool::bounded(64, SLOTS);
        let live = std::sync::atomic::AtomicU32::new(0);
        let (tx, rx) = std::sync::mpsc::sync_channel::<Memory>(8);
        let rx = std::sync::Mutex::new(rx);
        std::thread::scope(|s| {
            for _ in 0..8 {
                let pool = pool.clone();
                let (live, tx) = (&live, tx.clone());
                s.spawn(move || {
                    for i in 0..5000 {
                        let Some(mut m) = pool.try_acquire() else {
                            continue;
                        };
                        let n = live.fetch_add(1, Ordering::AcqRel) + 1;
                        assert!(n <= SLOTS, "{n} slots live at once with a cap of {SLOTS}");
                        m.as_mut_full()[0] = i as u8;
                        m.set_len(1);
                        let twin = m.clone();
                        let _ = tx.try_send(twin);
                        live.fetch_sub(1, Ordering::AcqRel);
                        drop(m);
                    }
                });
            }
            drop(tx);
            s.spawn(|| {
                let rx = rx.lock().unwrap();
                while let Ok(m) = rx.recv() {
                    assert_eq!(m.len(), 1);
                    drop(m);
                }
            });
        });
        assert_eq!(pool.stats().outstanding, 0, "every slot came back");
        assert_eq!(pool.free_slots(), SLOTS, "full headroom restored");
        let held: Vec<Memory> =
            (0..SLOTS).map(|_| pool.try_acquire().expect("slot free")).collect();
        assert!(pool.try_acquire().is_none(), "cap still enforced after the storm");
        drop(held);
    }

    #[test]
    fn copy_on_write_of_a_small_buffer_does_not_take_a_slot() {
        let pool = Pool::bounded(4096, 4);
        let mut shared = Vec::new();
        for _ in 0..8 {
            let mut m = pool.acquire_exact(600).expect("exact");
            m.set_len(600);
            let tee = m.clone();
            m.as_mut_full()[0] = 0xAB;
            shared.push((m, tee));
        }
        assert_eq!(pool.stats().outstanding, 0, "small CoW must not charge the slot budget");
        assert_eq!(pool.free_slots(), 4, "every slot still available to a real client");
        let slots: Vec<Memory> = (0..4).map(|_| pool.try_acquire().expect("slot free")).collect();
        assert_eq!(slots.len(), 4);
    }

    #[test]
    fn copy_on_write_keeps_the_window() {
        let pool = Pool::new(64);
        let mut m = pool.acquire();
        m.as_mut_full()[..8].copy_from_slice(b"01234567");
        m.set_len(8);
        let mut s = m.slice(2, 4).unwrap();
        s.as_mut_full()[0] = b'x';
        assert_eq!((s.data(), m.data()), (&b"x345"[..], &b"01234567"[..]));
        assert_eq!(s.capacity(), 62);
    }

    #[test]
    fn classes_are_a_quarter_octave_apart_above_256_bytes() {
        let sizes: Vec<usize> = (0..SMALL_CLASSES).map(class_size).collect();
        assert_eq!(&sizes[..8], &[64, 128, 256, 320, 384, 448, 512, 640]);
        assert_eq!(sizes[SMALL_CLASSES - 1], SIZED_TOP);
        for (class, &size) in sizes.iter().enumerate() {
            assert_eq!(size % 64, 0, "every class a multiple of the slot alignment");
            assert_eq!(small_class(size), Some((class, size)), "a class size is its own class");
            assert_eq!(
                small_class(size + 1).map(|c| c.0),
                (class + 1 < SMALL_CLASSES).then_some(class + 1)
            );
            if class > 2 {
                assert!(
                    (size - sizes[class - 1]) * 4 <= sizes[class - 1],
                    "at most a quarter wasted"
                );
            }
        }
        assert_eq!(small_class(SIZED_TOP + 1), None);
    }

    #[test]
    fn small_classes_are_kept_apart() {
        let pool = Pool::new(64 * 1024);
        for n in [1usize, 64, 65, 200, 1000, 4096, 8191] {
            let a = pool.acquire_exact(n).expect("exact");
            assert!(a.capacity() >= n, "{n}: at least n bytes");
            drop(a);
            let b = pool.acquire_exact(n).expect("exact");
            assert!(b.capacity() >= n, "{n}: recycled buffer still fits");
        }
    }

    #[test]
    fn bounded_pool_free_list_does_not_retain_beyond_cap() {
        let pool = Pool::bounded(8, 2);
        let held: Vec<_> = (0..5).map(|_| pool.acquire()).collect();
        assert_eq!(pool.stats().slot_allocations, 4, "one region of two and three exact buffers");
        assert_eq!(pool.stats().outstanding, 2, "the heap ones are outside the budget");
        drop(held);
        let a = pool.acquire();
        let b = pool.acquire();
        assert_eq!(pool.stats().slot_allocations, 4, "reused the 2 retained slots");
        let c = pool.acquire();
        assert_eq!(pool.stats().slot_allocations, 5, "the spike was not retained");
        drop((a, b, c));
    }

    #[test]
    fn a_memory_view_is_three_words() {
        assert_eq!(std::mem::size_of::<Memory>(), 24);
    }

    // Real threads and a sleep, to land the return while the acquirer is parked.
    #[allow(clippy::disallowed_methods)]
    fn park_for_a_return(pool: &Pool, out: Memory, enlist: impl FnOnce(&Arc<Station>) -> bool) {
        use std::time::Duration;
        let station = Arc::new(Station::new());
        let seen = station.epoch();
        assert!(!enlist(&station), "nothing free, so enlisted");
        let freer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(out);
        });
        let wake = station.park(seen, Some(Duration::from_secs(10)), None);
        assert_eq!(wake, crate::station::Wake::Signal, "the return signaled the parked station");
        freer.join().unwrap();
        assert!(pool.free_slots() > 0);
    }

    #[test]
    fn a_return_from_another_thread_wakes_a_starved_acquirer() {
        let pool = Pool::bounded(64, 2);
        let (a, b) = (pool.try_acquire().unwrap(), pool.try_acquire().unwrap());
        park_for_a_return(&pool, a, |st| pool.try_acquire_or_enlist(st).is_some());
        assert!(pool.try_acquire().is_some());
        drop(b);
    }

    #[test]
    fn a_return_into_a_locked_class_region_wakes_a_starved_acquirer() {
        let pool = Pool::with_config(PoolConfig { bytes: 1 << 20, ..PoolConfig::heap(64, 8) });
        let a = pool.try_acquire_sized(400 << 10).unwrap().unwrap();
        let b = pool.try_acquire_sized(400 << 10).unwrap().unwrap();
        assert_eq!(
            pool.stats().regions,
            2,
            "the second region of the class releases under the lock"
        );
        park_for_a_return(&pool, b, |st| {
            pool.try_acquire_sized_or_enlist(400 << 10, st).unwrap().is_some()
        });
        assert!(pool.try_acquire_sized(400 << 10).unwrap().is_some());
        drop(a);
    }

    #[test]
    fn a_return_into_a_draining_region_wakes_a_starved_acquirer() {
        let pool = Pool::bounded(64, 1);
        let old = pool.try_acquire().unwrap();
        pool.resize(128);
        assert!(pool.try_acquire().is_none(), "the draining slot still holds the cap");
        park_for_a_return(&pool, old, |st| pool.try_acquire_or_enlist(st).is_some());
        assert_eq!(pool.try_acquire().unwrap().capacity(), 128);
        assert_eq!(pool.stats().bytes_mapped, 128, "the old region went with its slot");
    }

    #[test]
    fn concurrent_returns_keep_the_counts_and_the_cap() {
        use crate::ring::spsc;
        const SLOTS: u32 = 16;
        const N: usize = 50_000;
        let pool =
            Pool::with_config(PoolConfig { bytes: 64 << 10, ..PoolConfig::heap(256, SLOTS) });
        let (tx, rx) = spsc::<Memory>(8);
        let (tee_tx, tee_rx) = spsc::<Memory>(8);
        std::thread::scope(|s| {
            s.spawn(move || {
                while let Some(m) = rx.pop() {
                    assert_eq!(m.data()[0], m.len() as u8, "the bytes the acquirer wrote");
                    let _ = tee_tx.try_push(m);
                }
            });
            s.spawn(move || while tee_rx.pop().is_some() {});
            let mut sent = 0;
            while sent < N {
                let n = 1 + sent % 200;
                let taken = if sent % 3 == 0 {
                    pool.try_acquire_sized(n).unwrap()
                } else {
                    pool.try_acquire()
                };
                let Some(mut m) = taken else { continue };
                let (out, cap) = pool.in_use();
                assert!(out <= cap, "{out} out under a cap of {cap}");
                m.as_mut_full()[0] = n as u8;
                m.set_len(n);
                assert!(tx.push(m).is_ok(), "the consumer runs until the end");
                sent += 1;
            }
            drop(tx);
        });
        let s = pool.stats();
        assert_eq!((s.outstanding, s.bytes_out), (0, 0), "every buffer came back");
        assert!(s.high_water <= SLOTS as u64 && s.bytes_high <= 64 << 10);
        assert_eq!(pool.free_slots(), SLOTS);
    }

    #[test]
    fn the_high_water_is_exact_for_a_thread_that_releases_its_own() {
        for depth in [1usize, 5, 40, 100] {
            let pool = Pool::new(64);
            for _ in 0..50 {
                let held: Vec<Memory> = (0..depth).map(|_| pool.acquire()).collect();
                drop(held);
            }
            let s = pool.stats();
            assert_eq!((s.high_water, s.outstanding), (depth as u64, 0), "depth {depth}");
            assert_eq!(s.acquires, 50 * depth as u64);
        }
        let pool = Pool::with_config(PoolConfig { bytes: 1 << 20, ..PoolConfig::heap(64, 1024) });
        for _ in 0..100 {
            let held: Vec<Memory> =
                (0..7).map(|_| pool.try_acquire_sized(200).unwrap().unwrap()).collect();
            drop(held);
        }
        assert_eq!((pool.stats().high_water, pool.stats().bytes_high), (7, 7 * 256));
    }

    #[test]
    fn the_high_water_stays_within_its_bound_against_another_thread() {
        use crate::ring::spsc;
        const DEPTH: usize = 8;
        let pool = Pool::bounded(64, 1024);
        let (tx, rx) = spsc::<Memory>(DEPTH);
        std::thread::scope(|s| {
            s.spawn(move || while rx.pop().is_some() {});
            for _ in 0..200_000 {
                let m = pool.try_acquire().expect("1024 slots for a ring of 8");
                assert!(tx.push(m).is_ok());
            }
            drop(tx);
        });
        let s = pool.stats();
        assert!(
            s.high_water <= DEPTH as u64 + 2 + TAKE_SPAN as u64,
            "{} out at the water",
            s.high_water
        );
        assert_eq!((s.outstanding, pool.free_slots()), (0, 1024));
        assert_eq!(s.slot_allocations, 1, "one region of 64 serves a ring of 8");
    }

    #[test]
    fn a_resize_and_a_retire_close_regions_with_folded_spare_slots() {
        static MAPS: AtomicU64 = AtomicU64::new(0);
        static UNMAPS: AtomicU64 = AtomicU64::new(0);
        unsafe fn map(ctx: *mut (), len: usize, align: usize, name: &CStr) -> *mut u8 {
            MAPS.fetch_add(1, Ordering::Relaxed);
            unsafe { heap_map(ctx, len, align, name) }
        }
        unsafe fn unmap(ctx: *mut (), base: *mut u8, len: usize, align: usize, token: u64) {
            UNMAPS.fetch_add(1, Ordering::Relaxed);
            unsafe { heap_unmap(ctx, base, len, align, token) }
        }
        let allocator = Allocator { map, unmap, ..Allocator::HEAP };
        for retire in [false, true] {
            let pool = Pool::with_config(PoolConfig { allocator, ..PoolConfig::heap(64, 256) });
            // The water take folds four returns onto the spare stack while the free stack has
            // slots.
            let first: Vec<Memory> = (0..4).map(|_| pool.try_acquire().unwrap()).collect();
            drop(first);
            let held: Vec<Memory> = (0..4).map(|_| pool.try_acquire().unwrap()).collect();
            assert_eq!(pool.stats().high_water, 4, "the returns were taken, not counted twice");
            let kept = held[0].clone();
            if retire {
                pool.retire();
                assert!(pool.try_acquire().is_none(), "retired");
            } else {
                pool.resize(128);
                assert_eq!(pool.try_acquire().unwrap().capacity(), 128);
            }
            drop(held);
            assert!(pool.stats().bytes_mapped >= 64 * 64, "the old region waits for its last view");
            drop(kept);
            let s = pool.stats();
            assert_eq!(
                s.bytes_mapped,
                if retire { 0 } else { 128 * region_slots(128) as u64 },
                "retire {retire}"
            );
            assert_eq!(s.outstanding, 0);
            drop(pool);
        }
        assert_eq!(
            MAPS.load(Ordering::Relaxed),
            UNMAPS.load(Ordering::Relaxed),
            "each region unmapped once"
        );
    }

    #[test]
    fn returns_from_another_thread_reach_the_cursor_across_regions() {
        use crate::ring::spsc;
        const SLOTS: u32 = 256;
        let pool = Pool::bounded(1024, SLOTS);
        let (tx, rx) = spsc::<Memory>(64);
        std::thread::scope(|s| {
            s.spawn(move || while rx.pop().is_some() {});
            for i in 0..100_000u32 {
                let m = loop {
                    match pool.try_acquire() {
                        Some(m) => break m,
                        None => std::hint::spin_loop(),
                    }
                };
                if i % 1000 == 0 {
                    let (out, cap) = pool.in_use();
                    assert!(out <= cap && out <= 66, "{out} out, the ring and two hands");
                }
                assert!(tx.push(m).is_ok());
            }
            drop(tx);
        });
        let s = pool.stats();
        assert!(
            (2..=4).contains(&s.regions),
            "{} regions of 64, what the ring held plus the cursor's",
            s.regions
        );
        assert_eq!((s.outstanding, pool.free_slots(), s.recycles), (0, SLOTS, 100_000));
    }

    #[test]
    fn a_pool_dropped_while_other_threads_release_unmaps_every_region_once() {
        static MAPS: AtomicU64 = AtomicU64::new(0);
        static UNMAPS: AtomicU64 = AtomicU64::new(0);
        unsafe fn map(ctx: *mut (), len: usize, align: usize, name: &CStr) -> *mut u8 {
            MAPS.fetch_add(1, Ordering::Relaxed);
            unsafe { heap_map(ctx, len, align, name) }
        }
        unsafe fn unmap(ctx: *mut (), base: *mut u8, len: usize, align: usize, token: u64) {
            UNMAPS.fetch_add(1, Ordering::Relaxed);
            unsafe { heap_unmap(ctx, base, len, align, token) }
        }
        let allocator = Allocator { map, unmap, ..Allocator::HEAP };
        for round in 0..300 {
            let pool = Pool::with_config(PoolConfig { allocator, ..PoolConfig::heap(64, 32) });
            let mut left: Vec<Memory> = (0..24).map(|_| pool.try_acquire().unwrap()).collect();
            let right = left.split_off(12);
            std::thread::scope(|s| {
                s.spawn(move || drop(left));
                s.spawn(move || drop(right));
                match round % 3 {
                    0 => drop(pool),
                    1 => pool.retire(),
                    _ => pool.resize(128),
                }
            });
        }
        assert_eq!(
            MAPS.load(Ordering::Relaxed),
            UNMAPS.load(Ordering::Relaxed),
            "each region unmapped once"
        );
    }
}

// Parks below pass no timeout, so a lost wake is a deadlock loom reports.
#[cfg(loom)]
mod loom_tests {
    use super::*;

    type Shared = (Starved, Word, [Next; 2]);

    fn park_until(st: &Station, mut work: impl FnMut() -> bool) {
        loop {
            let seen = st.epoch();
            if work() {
                return;
            }
            st.park(seen, None, None);
        }
    }

    fn freer(shared: &Arc<Shared>, slot: u32) -> loom::thread::JoinHandle<()> {
        let shared = Arc::clone(shared);
        loom::thread::spawn(move || {
            let (starved, head, next) = &*shared;
            let mut pinned = false;
            match give_back(head, &next[slot as usize], slot, false, || pinned = true) {
                Give::Wanted => {
                    assert!(pinned, "the pool is pinned before the push that saw WANT");
                    starved.wake();
                }
                Give::Pushed => {}
                Give::Closed => unreachable!("never closed here"),
            }
        })
    }

    fn acquire_until(st: &Arc<Station>, shared: &Shared, slots: u64) {
        let (starved, head, _) = shared;
        let mut got = 0;
        park_until(st, || {
            let keep = if starved.wanted() { WANT } else { 0 };
            got += (take(head, keep) >> 33) & 0x7FFF;
            if got >= slots {
                return true;
            }
            want(starved, st, || got += (raise(head) >> 33) & 0x7FFF);
            got >= slots
        });
    }

    fn shared() -> Arc<Shared> {
        Arc::new((Starved::new(), Word::new(END as u64), [Next::new(END), Next::new(END)]))
    }

    #[test]
    fn loom_pool_return_wakes_a_starved_acquirer() {
        loom::model(|| {
            let st = Arc::new(Station::new());
            let shared = shared();
            let f = freer(&shared, 0);
            acquire_until(&st, &shared, 1);
            f.join().unwrap();
        });
    }

    #[test]
    fn loom_pool_two_returns_wake_a_starved_acquirer() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let st = Arc::new(Station::new());
            let shared = shared();
            let (a, b) = (freer(&shared, 0), freer(&shared, 1));
            acquire_until(&st, &shared, 2);
            a.join().unwrap();
            b.join().unwrap();
        });
    }

    #[test]
    fn loom_pool_drained_release_wakes_a_starved_acquirer() {
        loom::model(|| {
            let st = Arc::new(Station::new());
            let shared = Arc::new((Starved::new(), Word::new(0)));
            let f = {
                let shared = Arc::clone(&shared);
                loom::thread::spawn(move || note_drained(&shared.1, 1, &shared.0))
            };
            park_until(&st, || {
                if shared.1.load(Ordering::Relaxed) > 0 {
                    return true;
                }
                want(&shared.0, &st, || {});
                shared.1.load(Ordering::Relaxed) > 0
            });
            f.join().unwrap();
        });
    }

    #[test]
    fn loom_pool_seat_hands_out_one_holder_at_a_time() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(3);
        model.check(|| {
            let seat = Arc::new(Seat::new(0u32));
            let holders: Vec<_> = (0..2)
                .map(|_| {
                    let seat = Arc::clone(&seat);
                    loom::thread::spawn(move || *seat.take() += 1)
                })
                .collect();
            for h in holders {
                h.join().unwrap();
            }
            assert_eq!(*seat.take(), 2, "each held the seat alone");
        });
    }

    #[test]
    fn loom_pool_close_and_a_release_unmap_once() {
        static UNMAPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        unsafe fn unmap_once(ctx: *mut (), base: *mut u8, len: usize, align: usize, token: u64) {
            UNMAPS.fetch_add(1, Ordering::Relaxed);
            unsafe { heap_unmap(ctx, base, len, align, token) }
        }
        loom::model(|| {
            UNMAPS.store(0, Ordering::Relaxed);
            let allocator = Allocator { unmap: unmap_once, ..Allocator::HEAP };
            let region =
                Region::map(allocator, 64, 2, MIN_ALIGN, Kind::Main, false, Weak::new()).unwrap();
            let mut h = Held::new(region, false, Keep::Budget);
            let slot = pop(&mut h).unwrap();
            let view = Memory { region: Some(region), slot, offset: 0, len: 0, side: 0 };
            let releaser = loom::thread::spawn(move || drop(view));
            let mut counts = Counts { out: 1, ..Counts::default() };
            unmap_closed(close(h, &mut counts), &AtomicU64::new(0));
            releaser.join().unwrap();
            assert_eq!(UNMAPS.load(Ordering::Relaxed), 1, "unmapped once, by the last of the two");
        });
    }
}

#[cfg(all(test, not(loom)))]
mod side_tests {
    use super::*;

    fn filled(pool: &Pool, fill: u8) -> Memory {
        let mut m = pool.try_acquire().unwrap();
        m.as_mut_full()[..64].fill(fill);
        m.set_len(64);
        m
    }

    #[test]
    fn side_data_rides_clones_and_not_slices() {
        let pool = Pool::bounded(256, 2);
        let mut m = filled(&pool, 7);
        assert!(m.side().is_empty());
        m.set_side(&[1, 2, 3]).unwrap();
        let c = m.clone();
        assert_eq!(c.side(), &[1, 2, 3]);
        assert_eq!(c.data(), m.data(), "the payload is untouched");
        assert!(m.slice(0, 8).unwrap().side().is_empty(), "a sub-view is another unit");
        let mut moved = m;
        assert_eq!(moved.take().side(), &[1, 2, 3]);
    }

    #[test]
    fn one_claim_a_slot_until_its_release() {
        let pool = Pool::bounded(256, 1);
        let mut m = filled(&pool, 1);
        let mut other = m.clone();
        m.set_side(&[9]).unwrap();
        assert_eq!(other.set_side(&[8]), Err(SideError::Taken), "a shared slot is written once");
        assert!(other.side().is_empty(), "a view from before the write states none");
        drop((m, other));
        let mut again = filled(&pool, 2);
        assert!(again.side().is_empty(), "the slot came back clean");
        again.set_side(&[5, 5]).unwrap();
        assert_eq!(again.side(), &[5, 5]);
    }

    #[test]
    fn a_write_to_a_shared_view_takes_its_side_data_along() {
        let pool = Pool::bounded(256, 4);
        let mut m = filled(&pool, 3);
        m.set_side(&[4, 4, 4]).unwrap();
        let keep = m.clone();
        m.as_mut_full()[0] = 0xEE;
        assert_eq!(m.side(), &[4, 4, 4], "copied with the payload");
        assert_eq!(keep.side(), &[4, 4, 4]);
        assert_eq!(keep.data()[0], 3);
    }

    #[test]
    fn empty_and_oversized_side_data_is_refused() {
        let pool = Pool::bounded(256, 1);
        let mut m = filled(&pool, 0);
        assert_eq!(m.set_side(&[]), Err(SideError::Empty));
        assert_eq!(m.set_side(&[0; SIDE_CAP + 1]), Err(SideError::TooLarge { len: SIDE_CAP + 1 }));
        m.set_side(&[0xAB; SIDE_CAP]).unwrap();
        assert_eq!(m.side().len(), SIDE_CAP);
        m.widen();
        assert!(m.side().is_empty());
    }

    #[test]
    fn an_exact_buffer_carries_side_data_too() {
        let mut m = exact(32).expect("exact");
        m.set_side(&[1]).unwrap();
        assert_eq!(m.clone().side(), &[1]);
    }
}
