//! OS backend of core's reactor contract over `stein-io`, io_uring where allowed and epoll
//! otherwise. An op holds its `Buffer` from push to last completion, and a cancel completes only
//! once the kernel lets go, so a slot is never handed back while the kernel may still write it.
#![deny(unsafe_code)]
#![allow(clippy::disallowed_types)]

mod bell;
mod hygiene;
mod lend;
mod ops;
mod path;
mod resolve;
mod sys;

use std::collections::{BinaryHeap, VecDeque};
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use profluens_core::buffer::Buffer;
use profluens_core::id::ElementId;
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Op, OpId, Reactor, ReactorFactory, Submission, Waker,
};
use profluens_core::pipeline::Pipeline;
use rustix::io::Errno;
use stein_io::queue::{self, Cqe, Key, Owned, Queue, Setup};

pub use hygiene::{
    HYGIENE_WINDOW, Hint, POSIX_FADV_DONTNEED, POSIX_FADV_SEQUENTIAL, SYNC_FILE_RANGE_WRITE,
    StreamHygiene,
};
pub use stein_io::Backend;
pub use stein_io::queue::Want;
pub use sys::{fadvise, sync_file_range};

use bell::EventBell;
use resolve::Resolver;

const MAX_OPS: usize = 256;
/// The least room [`OsReactor::sized`] gives, the bell's read and a cancel beside a few ops.
pub const MIN_OPS: usize = 16;
const MAX_PENDING: usize = 1024;
const TABLE: usize = 64;
const MAX_SLOTS: usize = 4096;
const MAX_TIMERS: usize = 4096;
const MAX_PARKED: usize = 64;
const MAX_FDS: usize = 1 << 16;
const NONE: u32 = u32::MAX;
const BELL: u64 = u64::MAX;
const HINT: u64 = 1 << 62;
const REAP: usize = 64;
/// Passes `run_once` leaves owed entries for the park's wait to carry before it enters itself.
pub const HOLD: u8 = 2;
/// Passes epoll leaves a receive armed before unpolled, in a group that never parks.
const ARMED_HOLD: u8 = 16;

/// The OS reactor factory, the ring where it can be set up and epoll otherwise.
// Cold path, once per pipeline.
#[allow(clippy::disallowed_methods)]
pub fn factory() -> ReactorFactory {
    Arc::new(|| Ok(Box::new(OsReactor::new()?) as Box<dyn Reactor>))
}

pub fn pipeline() -> Pipeline {
    pf_memory::name::install();
    let mut p = Pipeline::new();
    p.set_reactor_factory(factory());
    p
}

// Cold path, once per factory.
#[allow(clippy::disallowed_methods)]
pub fn factory_on(want: Want) -> ReactorFactory {
    Arc::new(move || Ok(Box::new(OsReactor::on(want)?) as Box<dyn Reactor>))
}

// Empty static vector, no allocation.
#[allow(clippy::disallowed_methods)]
static PARKED: Mutex<Vec<UdpSocket>> = Mutex::new(Vec::new());

/// Leave a bound UDP socket for the reactor that binds its address to adopt.
pub fn park_udp(sock: UdpSocket) -> Result<SocketAddr, IoError> {
    let addr = sock.local_addr().map_err(|e| io_error(&e))?;
    let mut parked = PARKED.lock().unwrap_or_else(|e| e.into_inner());
    if parked.len() >= MAX_PARKED {
        return Err(IoError::Full);
    }
    parked.push(sock);
    Ok(addr)
}

/// Kernel enters per reactor, added to the process totals on drop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Enters {
    pub submits: u64,
    pub waits: u64,
}

static SUBMITS: AtomicU64 = AtomicU64::new(0);
static WAITS: AtomicU64 = AtomicU64::new(0);

pub fn enters() -> Enters {
    Enters { submits: SUBMITS.load(Ordering::Relaxed), waits: WAITS.load(Ordering::Relaxed) }
}

pub fn take_parked(addr: SocketAddr) -> Option<UdpSocket> {
    let mut parked = PARKED.lock().unwrap_or_else(|e| e.into_inner());
    let at = parked.iter().position(|p| p.local_addr().is_ok_and(|a| a == addr))?;
    Some(parked.swap_remove(at))
}

pub fn io_error(e: &io::Error) -> IoError {
    match e.kind() {
        ErrorKind::NotFound => IoError::NotFound,
        ErrorKind::PermissionDenied => IoError::Denied,
        ErrorKind::IsADirectory => IoError::NotAFile,
        ErrorKind::ConnectionRefused => IoError::Refused,
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            IoError::Reset
        }
        ErrorKind::TimedOut => IoError::TimedOut,
        ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => IoError::Unreachable,
        ErrorKind::AddrInUse => IoError::AddrInUse,
        _ => IoError::Os(e.raw_os_error().unwrap_or(0)),
    }
}

fn errno(e: Errno) -> IoError {
    io_error(&io::Error::from_raw_os_error(e.raw_os_error()))
}

/// `Unsupported` is the kernel or sandbox refusing the ring, which the factory falls back on.
fn setup_error(e: &stein_io::IoError) -> IoError {
    match e {
        stein_io::IoError::Kernel(no) => match *no {
            Errno::NOSYS | Errno::PERM | Errno::ACCESS | Errno::OPNOTSUPP => IoError::Unsupported,
            other => errno(other),
        },
        _ => IoError::Unsupported,
    }
}

enum Kind {
    Free,
    File { hygiene: StreamHygiene },
    Pipe,
    Tcp,
    Udp,
    Listener,
    Connecting,
}

struct Slot {
    generation: u32,
    kind: Kind,
    fd: Option<OwnedFd>,
    /// A receive or accept is on the queue. The next waits, since the ring keeps no order on one
    /// fd.
    busy_read: bool,
    busy_write: bool,
    flying: u32,
    closing: u32,
    lent: Option<Box<lend::Lent>>,
}

impl Slot {
    fn raw(&self) -> RawFd {
        self.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Waiting,
    Flying,
    Cancelling,
    Lent,
}

struct Pending {
    op: OpId,
    element: ElementId,
    user: u64,
    kind: Op,
    buf: Option<Buffer>,
    on: Handle,
    key: Option<Key>,
    state: State,
    progress: usize,
    block: u32,
    step: path::Step,
    bid: u16,
}

#[derive(PartialEq, Eq)]
struct Timer {
    at: Instant,
    seq: u64,
    op: OpId,
    element: ElementId,
    user: u64,
}

impl Ord for Timer {
    // Reversed, so the max-heap's top is the earliest.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.at, other.seq).cmp(&(self.at, self.seq))
    }
}

impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub struct OsReactor {
    /// First, so its drop cancels and waits for in-flight ops before the buffers below are freed.
    q: Queue,
    word: Box<[u8; 8]>,
    bell: Option<Arc<EventBell>>,
    bell_flying: bool,
    ops: Vec<Option<Pending>>,
    free_ops: Vec<u32>,
    waiting: VecDeque<u32>,
    slots: Vec<Slot>,
    free_slots: Vec<u32>,
    timers: BinaryHeap<Timer>,
    timer_seq: u64,
    resolver: Resolver,
    blocks: Vec<Box<[u8]>>,
    free_blocks: Vec<u32>,
    done: Vec<(ElementId, Completion)>,
    waker: Option<Waker>,
    queued: bool,
    held: u8,
    urgent: bool,
    tells: bool,
    /// File ops on the queue. They complete on their own, so teardown waits for them.
    file_flying: u32,
    hygiene_window: u64,
    enters: Enters,
}

impl Drop for OsReactor {
    fn drop(&mut self) {
        SUBMITS.fetch_add(self.enters.submits, Ordering::Relaxed);
        WAITS.fetch_add(self.enters.waits, Ordering::Relaxed);
    }
}

impl OsReactor {
    pub fn new() -> Result<Self, IoError> {
        Self::on(Want::Any)
    }

    pub fn on(want: Want) -> Result<Self, IoError> {
        Self::sized(want, MAX_OPS)
    }

    pub fn sized(want: Want, ops: usize) -> Result<Self, IoError> {
        Self::made(want, ops, Waits::Any)
    }

    /// [`sized`](Self::sized) for files only. On epoll there is no descriptor table, so a pipe or
    /// socket on it fails.
    pub fn files(want: Want, ops: usize) -> Result<Self, IoError> {
        Self::made(want, ops, Waits::Nothing)
    }

    // Once per group.
    #[allow(clippy::disallowed_methods)]
    fn made(want: Want, ops: usize, waits: Waits) -> Result<Self, IoError> {
        let q = queue(want, ops.clamp(MIN_OPS, MAX_OPS), waits)?;
        let tells =
            matches!(q.ring_flags(), stein_io::RingFlags::Deferred | stein_io::RingFlags::Coop);
        Ok(Self {
            q,
            word: Box::new([0; 8]),
            bell: None,
            bell_flying: false,
            ops: Vec::with_capacity(TABLE),
            free_ops: Vec::with_capacity(TABLE),
            waiting: VecDeque::with_capacity(TABLE),
            slots: Vec::with_capacity(64),
            free_slots: Vec::with_capacity(64),
            timers: BinaryHeap::with_capacity(TABLE),
            timer_seq: 0,
            resolver: Resolver::new(),
            blocks: Vec::with_capacity(16),
            free_blocks: Vec::with_capacity(16),
            done: Vec::with_capacity(TABLE),
            waker: None,
            queued: false,
            held: 0,
            urgent: false,
            file_flying: 0,
            hygiene_window: HYGIENE_WINDOW,
            enters: Enters::default(),
            tells,
        })
    }

    pub fn backend(&self) -> Backend {
        self.q.backend()
    }

    pub fn enters(&self) -> Enters {
        self.enters
    }

    pub fn set_hygiene_window(&mut self, bytes: u64) {
        self.hygiene_window = bytes;
        for s in &mut self.slots {
            if let Kind::File { hygiene } = &mut s.kind {
                hygiene.set_window(bytes);
            }
        }
    }

    fn slot(&self, h: Handle) -> Result<&Slot, IoError> {
        match self.slots.get(h.slot() as usize) {
            Some(s) if s.generation == h.generation() && !matches!(s.kind, Kind::Free) => Ok(s),
            _ => Err(IoError::BadHandle),
        }
    }

    fn slot_mut(&mut self, h: Handle) -> Result<&mut Slot, IoError> {
        match self.slots.get_mut(h.slot() as usize) {
            Some(s) if s.generation == h.generation() && !matches!(s.kind, Kind::Free) => Ok(s),
            _ => Err(IoError::BadHandle),
        }
    }

    fn insert(&mut self, kind: Kind, fd: OwnedFd) -> Result<Handle, IoError> {
        if let Some(i) = self.free_slots.pop() {
            let s = &mut self.slots[i as usize];
            (s.kind, s.fd, s.busy_read, s.busy_write, s.flying, s.closing) =
                (kind, Some(fd), false, false, 0, NONE);
            if let Some(lent) = s.lent.as_mut() {
                lent.reset();
            }
            return Ok(Handle::new(i, s.generation));
        }
        if self.slots.len() >= MAX_SLOTS {
            return Err(IoError::Full);
        }
        let s = Slot {
            generation: 0,
            kind,
            fd: Some(fd),
            busy_read: false,
            busy_write: false,
            flying: 0,
            closing: NONE,
            lent: None,
        };
        self.slots.push(s);
        Ok(Handle::new(self.slots.len() as u32 - 1, 0))
    }

    fn release(&mut self, h: Handle) {
        let Ok(s) = self.slot_mut(h) else { return };
        s.kind = Kind::Free;
        s.generation = s.generation.wrapping_add(1);
        let fd = s.fd.take();
        if let Some(fd) = &fd {
            // Epoll watched it by number, which the kernel reuses.
            self.q.forget(fd.as_raw_fd());
        }
        drop(fd);
        self.free_slots.push(h.slot());
    }

    fn socket(&mut self, kind: Kind, fd: OwnedFd, addr: io::Result<SocketAddr>) -> IoResult {
        match addr.map_err(|e| io_error(&e)).and_then(|a| Ok((self.insert(kind, fd)?, a))) {
            Ok((handle, addr)) => IoResult::Socket { handle, addr },
            Err(e) => IoResult::Err(e),
        }
    }

    fn complete(&mut self, s: Submission, result: IoResult) {
        self.done.push((s.element, Completion { op: s.op, user: s.user, result, buf: s.buf }));
    }

    fn pend(&mut self, s: Submission, on: Handle) -> Option<u32> {
        let idx = match self.free_ops.pop() {
            Some(i) => i,
            None if self.ops.len() < MAX_PENDING => {
                self.ops.push(None);
                self.ops.len() as u32 - 1
            }
            None => {
                self.complete(s, IoResult::Err(IoError::Full));
                return None;
            }
        };
        let p = Pending {
            op: s.op,
            element: s.element,
            user: s.user,
            kind: s.kind,
            buf: s.buf,
            on,
            key: None,
            state: State::Waiting,
            progress: 0,
            block: NONE,
            step: path::Step::First,
            bid: 0,
        };
        self.ops[idx as usize] = Some(p);
        Some(idx)
    }

    fn finish(&mut self, idx: u32, result: IoResult) {
        let Some(p) = self.ops[idx as usize].take() else {
            return;
        };
        self.free_ops.push(idx);
        self.path_done(p.block);
        let c = Completion { op: p.op, user: p.user, result, buf: p.buf };
        self.done.push((p.element, c));
    }

    fn pump(&mut self) {
        if !self.bell_flying {
            self.arm_bell();
        }
        for _ in 0..self.waiting.len() {
            let Some(idx) = self.waiting.pop_front() else {
                break;
            };
            self.restart(idx);
        }
    }

    fn restart(&mut self, idx: u32) {
        match self.start(idx) {
            ops::Started::Flying => {}
            ops::Started::Wait => self.waiting.push_back(idx),
            ops::Started::Done(result) => self.finish(idx, result),
        }
    }

    /// Advise the kernel on the file at `on`. Dropped when the queue has no room.
    fn hint(&mut self, on: Handle, hint: Hint) {
        let Ok(s) = self.slot(on) else { return };
        let fd = queue::Fd::Raw(s.raw());
        let window = |len: u64| len.min(u64::from(u32::MAX)) as u32;
        let op = match hint {
            Hint::Sequential => {
                queue::Op::Fadvise { fd, at: 0, len: 0, advice: POSIX_FADV_SEQUENTIAL }
            }
            Hint::Drop { at, len } => {
                queue::Op::Fadvise { fd, at, len: window(len), advice: POSIX_FADV_DONTNEED }
            }
            Hint::Sync { at, len } => {
                queue::Op::SyncFileRange { fd, at, len: window(len), flags: SYNC_FILE_RANGE_WRITE }
            }
        };
        if self.q.push(op, HINT | u64::from(on.slot())).is_err() {
            return;
        }
        self.queued = true;
        self.file_flying += 1;
        if let Ok(s) = self.slot_mut(on) {
            s.flying += 1;
        }
    }

    fn landed_hint(&mut self, at: u32) {
        self.file_flying -= 1;
        let Some(s) = self.slots.get_mut(at as usize) else {
            return;
        };
        s.flying = s.flying.saturating_sub(1);
        let on = Handle::new(at, s.generation);
        self.closable(on);
    }

    fn harvest(&mut self) {
        let mut got = [Cqe::EMPTY; REAP];
        loop {
            let n = self.q.reap(&mut got);
            for c in &got[..n] {
                self.landed(*c);
            }
            if n < REAP {
                return;
            }
        }
    }

    /// What only an enter moves, and whether a push since the last enter is in it.
    fn owed(&mut self) -> (bool, bool) {
        match self.q.backend() {
            Backend::Uring => (self.queued, self.queued),
            Backend::Epoll => {
                let owed = self.q.in_flight() > self.bell_flying as usize;
                self.queued &= owed;
                (owed, self.queued)
            }
        }
    }

    /// Enter once if the pass needs it, else leave the enter to the park.
    fn flush(&mut self) {
        let (owed, fresh) = self.owed();
        let task = self.tells && !self.queued && self.q.pending();
        let delivers = fresh && !self.done.is_empty();
        let hold = if fresh { HOLD } else { ARMED_HOLD };
        if !task && !(owed && (self.urgent || delivers || self.held >= hold)) {
            self.held = if owed { self.held.saturating_add(1) } else { 0 };
            return;
        }
        (self.held, self.urgent) = (0, false);
        self.enters.submits += 1;
        // A refused enter (EAGAIN, EBUSY are Ok) leaves entries in the SQ, so they stay owed.
        let _ = self.q.submit();
        self.queued = self.q.unsubmitted();
        self.harvest();
        self.pump();
    }

    fn arm_bell(&mut self) {
        let Some(fd) = self.bell.as_ref().map(|b| b.raw()) else {
            return;
        };
        let read = queue::Op::Read {
            fd: queue::Fd::Raw(fd),
            mem: sys::word(&mut self.word),
            at: queue::POSITION,
        };
        if self.q.push(read, BELL).is_ok() {
            self.bell_flying = true;
            self.queued = true;
        }
    }

    // The backend reads the clock.
    #[allow(clippy::disallowed_methods)]
    fn fire_timers(&mut self) {
        let now = Instant::now();
        while self.timers.peek().is_some_and(|t| t.at <= now) {
            let Some(t) = self.timers.pop() else { break };
            let c = Completion { op: t.op, user: t.user, result: IoResult::Ok(0), buf: None };
            self.done.push((t.element, c));
        }
    }

    pub(crate) fn take_timer(&mut self, op: OpId) -> Option<Timer> {
        let at = self.timers.iter().position(|t| t.op == op)?;
        // Rare against pushes and fires, a cancel rebuilds the heap without it.
        let mut all = std::mem::take(&mut self.timers).into_vec();
        let t = all.swap_remove(at);
        self.timers = BinaryHeap::from(all);
        Some(t)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Waits {
    Any,
    Nothing,
}

/// The ring is single issuer and needs no descriptor table, and reads files in the kernel's
/// workers, a page cache miss would otherwise hold the group's wait for the disk. Epoll sizes
/// a table up to [`MAX_FDS`].
fn queue(want: Want, ops: usize, waits: Waits) -> Result<Queue, IoError> {
    let ring = || {
        let setup =
            Setup { want: Want::Uring, single_issuer: true, fixed_buffers: 0, punt_reads: true };
        Queue::new(Owned::new(ops, 0), setup).map_err(|r| setup_error(&r.error))
    };
    let epoll = || {
        let setup =
            Setup { want: Want::Epoll, single_issuer: false, fixed_buffers: 0, punt_reads: false };
        let fds = if waits == Waits::Any { fd_room() } else { 0 };
        Queue::new(Owned::new(ops, fds), setup).map_err(|r| setup_error(&r.error))
    };
    match want {
        Want::Uring => ring(),
        Want::Epoll => epoll(),
        Want::Any => ring().or_else(|_| epoll()),
    }
}

fn fd_room() -> usize {
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    limit.current.map_or(MAX_FDS, |n| n as usize).clamp(1024, MAX_FDS)
}

impl Reactor for OsReactor {
    // The backend reads the clock.
    #[allow(clippy::disallowed_methods)]
    fn submit(&mut self, subs: &mut Vec<Submission>, names: &[u8]) {
        for s in subs.drain(..) {
            match s.kind {
                Op::Open { .. } | Op::Stat { .. } | Op::Rename { .. } | Op::Unlink { .. } => {
                    self.path_submit(s, names)
                }
                Op::Adopt { fd, mode } => self.adopt(s, fd, mode),
                Op::Lend { .. } => self.lend(s),
                Op::Close { handle } => self.close(s, handle),
                Op::Resolve { .. } | Op::Lookup { .. } => {
                    if let Some(done) = self.resolver.start(s, names, self.waker.as_ref()) {
                        self.done.push(done);
                    }
                }
                Op::Connect { addr } => self.connect(s, addr),
                Op::Bind { .. } | Op::Listen { .. } | Op::Peer { .. } => self.setup_op(s),
                Op::Read { handle, .. }
                | Op::Write { handle, .. }
                | Op::Accept { handle }
                | Op::Recv { handle }
                | Op::Send { handle, .. }
                | Op::SendTo { handle, .. }
                | Op::RecvMany { handle }
                | Op::SendMany { handle, .. } => {
                    if let Some(idx) = self.pend(s, handle) {
                        self.waiting.push_back(idx);
                    }
                }
                Op::Timer { after } if self.timers.len() < MAX_TIMERS => {
                    let (at, seq) = (Instant::now() + after, self.timer_seq);
                    self.timer_seq += 1;
                    self.timers.push(Timer { at, seq, op: s.op, element: s.element, user: s.user });
                }
                Op::Timer { .. } => self.complete(s, IoResult::Err(IoError::Full)),
                Op::Cancel { target } => self.cancel(target),
            }
        }
        self.pump();
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        out.clear();
        self.resolver.take(&mut self.done);
        self.harvest();
        self.pump();
        self.flush();
        if !self.timers.is_empty() {
            self.fire_timers();
        }
        out.append(&mut self.done);
    }

    // The backend reads the clock.
    #[allow(clippy::disallowed_methods)]
    fn due_in(&self) -> Option<Duration> {
        let next = self.timers.peek()?.at;
        Some(next.saturating_duration_since(Instant::now()))
    }

    fn is_idle(&self) -> bool {
        self.done.is_empty() && self.file_flying == 0 && !self.queued
    }

    // Once per group.
    #[allow(clippy::disallowed_methods)]
    fn set_waker(&mut self, waker: Waker) {
        if let Ok(bell) = EventBell::new() {
            let bell = Arc::new(bell);
            self.bell = Some(Arc::clone(&bell));
            self.arm_bell();
            if self.bell_flying {
                waker.set_bell(Arc::new(move || bell.ring()));
            } else {
                self.bell = None;
            }
        }
        self.waker = Some(waker);
    }

    fn park(&mut self, timeout: Duration) -> bool {
        if !self.done.is_empty() {
            return true;
        }
        (self.held, self.urgent) = (0, false);
        self.enters.waits += 1;
        // A signal cut wait is a spurious wake. A refused enter keeps its entries owed.
        let _ = self.q.tick(Some(timeout));
        self.queued = self.q.unsubmitted();
        self.q.ready()
    }
}
