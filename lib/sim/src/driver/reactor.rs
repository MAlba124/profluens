//! The simulated reactor. Core's reactor contract over declared files and peers on the virtual
//! clock, with seeded latencies, split receives and a fault budget so a faulted run still ends.
//! An op's effect is applied at submit and its completion waits for its deadline.

// The simulator's world, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use profluens_core::clock::Clock;
use profluens_core::id::ElementId;
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Op, OpId, OpenMode, Reactor, ReactorFactory, Submission,
};

use super::net::RATE_QUANTUM;
pub use super::net::{Break, Faults, pattern};
use crate::prng::Prng;
use crate::time::{NONE, Ns, SEC};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    Open,
    Close,
    Reset,
}

/// One call of a custom peer. It says how much of `input` it took, writes to `out` and says
/// what becomes of the connection.
pub struct Exchange<'a> {
    pub conn: u32,
    pub input: &'a [u8],
    pub consumed: usize,
    pub out: &'a mut Vec<u8>,
    pub after: After,
    /// Unread octets of earlier answers. A request over them was sent on a response given up half
    /// way.
    pub unread: u64,
    pub base: u64,
    sent: &'a [(u64, Ns)],
}

impl Exchange<'_> {
    /// When octet `at` was first handed to the socket. Zero for one before what is kept.
    pub fn sent_at(&self, at: u64) -> Ns {
        self.sent.iter().rev().find(|s| s.0 <= at).map_or(0, |s| s.1)
    }
}

pub struct Datagrams<'a> {
    pub now: Ns,
    pub input: Option<(SocketAddr, &'a [u8])>,
    pub out: &'a mut Vec<(SocketAddr, Vec<u8>)>,
    pub wake: Ns,
}

/// `Pattern` takes an 8-byte little-endian offset and streams [`pattern`] bytes from there to
/// `len`.
pub enum Serve {
    Pattern { key: u64, len: u64 },
    Custom(Box<dyn FnMut(&mut Exchange<'_>) + Send>),
    Datagram(Box<dyn FnMut(&mut Datagrams<'_>) + Send>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Udp {
    pub loss: u8,
    pub dup: u8,
    /// Drawn per datagram in the range, so a wide range reorders.
    pub delay: (Ns, Ns),
    pub black_hole: bool,
}

impl Udp {
    pub const NONE: Udp = Udp { loss: 0, dup: 0, delay: (0, 0), black_hole: false };
}

const EPHEMERAL: u16 = 40_000;

struct Wire {
    at: Ns,
    seq: u64,
    from: SocketAddr,
    to: SocketAddr,
    data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Message(Vec<u8>),
    /// NXDOMAIN, no data, SERVFAIL and REFUSED all come to this, as the system resolver reports
    /// them.
    NoRecord,
    Silent,
}

/// glibc's default of two attempts of five seconds (`resolv.conf(5)`).
pub const SILENT_LOOKUP: Ns = 10 * SEC;

struct SimRecord {
    host: Vec<u8>,
    kind: u16,
    answer: Lookup,
}

struct SimFile {
    name: Vec<u8>,
    data: Vec<u8>,
}

struct SimPeer {
    host: String,
    addr: SocketAddr,
    serve: Serve,
    conns: u32,
    wake: Ns,
}

impl SimPeer {
    fn datagrams(&self) -> bool {
        matches!(self.serve, Serve::Datagram(_))
    }
}

pub struct World {
    files: Vec<SimFile>,
    peers: Vec<SimPeer>,
    records: Vec<SimRecord>,
    pub faults: Faults,
    pub udp: Udp,
    wire: Vec<Wire>,
    wire_seq: u64,
    bound: Vec<SocketAddr>,
    next_port: u16,
    pub udp_lost: u32,
    pub udp_duplicated: u32,
    pub breakage: Break,
    pub completed: u64,
    pub connects: u32,
    pub failures: u32,
    /// Sends after the peer said it would close. A close for idleness is not counted, nothing told
    /// the client.
    pub sent_after_close: u32,
}

impl World {
    pub fn new(faults: Faults) -> World {
        let (files, peers) = (Vec::new(), Vec::new());
        let (records, wire, bound) = (Vec::new(), Vec::new(), Vec::new());
        World {
            files,
            peers,
            records,
            faults,
            udp: Udp::NONE,
            wire,
            wire_seq: 0,
            bound,
            next_port: EPHEMERAL,
            udp_lost: 0,
            udp_duplicated: 0,
            breakage: Break::None,
            completed: 0,
            connects: 0,
            failures: 0,
            sent_after_close: 0,
        }
    }

    pub fn file(&mut self, name: &str, data: Vec<u8>) {
        let name = name.as_bytes();
        match self.files.iter_mut().find(|f| f.name == name) {
            Some(f) => f.data = data,
            None => self.files.push(SimFile { name: name.to_vec(), data }),
        }
    }

    pub fn contents(&self, name: &str) -> Option<&[u8]> {
        self.files.iter().find(|f| f.name == name.as_bytes()).map(|f| &f.data[..])
    }

    pub fn peer(&mut self, host: &str, addr: SocketAddr, serve: Serve) {
        self.peers.push(SimPeer { host: host.to_string(), addr, serve, conns: 0, wake: NONE });
    }

    pub fn record(&mut self, host: &str, kind: u16, answer: Lookup) {
        let host = host.as_bytes();
        match self.records.iter_mut().find(|r| r.host == host && r.kind == kind) {
            Some(r) => r.answer = answer,
            None => self.records.push(SimRecord { host: host.to_vec(), kind, answer }),
        }
    }

    pub fn factory(world: Arc<Mutex<World>>, clock: Arc<dyn Clock>, seed: u64) -> ReactorFactory {
        let built = Arc::new(std::sync::atomic::AtomicU64::new(0));
        Arc::new(move || {
            let n = built.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let prng = Prng::new(seed).substream_named("reactor").substream(n);
            Ok(Box::new(SimReactor::new(Arc::clone(&world), Arc::clone(&clock), prng))
                as Box<dyn Reactor>)
        })
    }
}

enum Rx {
    Idle,
    Pattern { key: u64, next: u64, end: u64 },
    Bytes { data: Vec<u8>, at: usize },
}

impl Rx {
    fn left(&self) -> u64 {
        match self {
            Rx::Idle => 0,
            Rx::Pattern { next, end, .. } => end.saturating_sub(*next),
            Rx::Bytes { data, at } => (data.len() - at) as u64,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    Open,
    Closed,
    Reset,
}

struct Conn {
    peer: usize,
    id: u32,
    inbox: Vec<u8>,
    base: u64,
    sent: Vec<(u64, Ns)>,
    held: (u64, Ns),
    rx: Rx,
    link: Link,
    given: u64,
    reset_at: u64,
    stall_at: u64,
    last: [Ns; 2],
    quiet_since: Ns,
    declared: bool,
}

struct Socket {
    local: SocketAddr,
    peer: Option<SocketAddr>,
    inbox: VecDeque<(SocketAddr, Vec<u8>)>,
}

impl Socket {
    fn boxed(local: SocketAddr) -> Box<Socket> {
        Box::new(Socket { local, peer: None, inbox: VecDeque::new() })
    }
}

fn owned(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}

/// More are dropped, as a full receive buffer drops them.
const INBOX: usize = 256;

enum What {
    Free,
    File(usize),
    Conn(Box<Conn>),
    Udp(Box<Socket>),
    Listener,
}

struct Slot {
    generation: u32,
    what: What,
}

struct Flying {
    sub: Submission,
    at: Ns,
    seq: u64,
    result: IoResult,
    took: u64,
    /// Completes as it stands whatever happens to its handle. Only the deliberate defect sets it.
    sealed: bool,
}

pub struct SimReactor {
    world: Arc<Mutex<World>>,
    clock: Arc<dyn Clock>,
    prng: Prng,
    slots: Vec<Slot>,
    free: Vec<u32>,
    flying: Vec<Flying>,
    seq: u64,
    broke: bool,
    carried: Ns,
}

const RECV: usize = 0;
const SEND: usize = 1;

fn lock(world: &Mutex<World>) -> MutexGuard<'_, World> {
    world.lock().unwrap_or_else(|e| e.into_inner())
}

impl SimReactor {
    pub fn new(world: Arc<Mutex<World>>, clock: Arc<dyn Clock>, prng: Prng) -> SimReactor {
        let (slots, free, flying) = (Vec::new(), Vec::new(), Vec::new());
        SimReactor { world, clock, prng, slots, free, flying, seq: 0, broke: false, carried: 0 }
    }

    fn now(&self) -> Ns {
        self.clock.now().0
    }

    fn insert(&mut self, what: What) -> Handle {
        if let Some(i) = self.free.pop() {
            self.slots[i as usize].what = what;
            return Handle::new(i, self.slots[i as usize].generation);
        }
        self.slots.push(Slot { generation: 0, what });
        Handle::new(self.slots.len() as u32 - 1, 0)
    }

    fn slot(&mut self, h: Handle) -> Result<&mut What, IoError> {
        match self.slots.get_mut(h.slot() as usize) {
            Some(s) if s.generation == h.generation() && !matches!(s.what, What::Free) => {
                Ok(&mut s.what)
            }
            _ => Err(IoError::BadHandle),
        }
    }

    fn release(&mut self, h: Handle) {
        if let Some(s) =
            self.slots.get_mut(h.slot() as usize).filter(|s| s.generation == h.generation())
        {
            s.what = What::Free;
            s.generation = s.generation.wrapping_add(1);
            self.free.push(h.slot());
        }
    }

    fn conn(&mut self, h: Handle) -> Result<&mut Conn, IoError> {
        match self.slot(h)? {
            What::Conn(c) => Ok(c),
            _ => Err(IoError::WrongKind),
        }
    }

    fn latency(&mut self, f: &Faults) -> Ns {
        self.prng.range(f.latency.0, f.latency.1.saturating_add(1))
    }

    fn fly(&mut self, sub: Submission, at: Ns, result: IoResult, took: u64) {
        self.seq += 1;
        self.flying.push(Flying { sub, at, seq: self.seq, result, took, sealed: false });
    }

    /// Not before the last deadline in that direction, so a stream's completions keep their order.
    fn stream_deadline(&mut self, h: Handle, dir: usize, at: Ns) -> Ns {
        match self.conn(h) {
            Ok(c) if at != NONE => {
                c.last[dir] = c.last[dir].max(at);
                c.last[dir]
            }
            _ => at,
        }
    }

    fn breaks(&mut self, w: &World, b: Break) -> bool {
        let fire = w.breakage == b && !self.broke && self.prng.chance(25);
        self.broke |= fire;
        fire
    }

    fn file_op(&mut self, w: &mut World, s: &mut Submission, names: &[u8]) -> IoResult {
        let find = |w: &World, n: &[u8]| w.files.iter().position(|f| f.name == n);
        match s.kind {
            Op::Open { path, mode } => {
                let name = path.of(names);
                let at = match (find(w, name), mode) {
                    (Some(i), OpenMode::Read | OpenMode::ReadOnce) => i,
                    (None, OpenMode::Read | OpenMode::ReadOnce) => {
                        return IoResult::Err(IoError::NotFound);
                    }
                    (Some(i), OpenMode::Create) => {
                        w.files[i].data.clear();
                        i
                    }
                    (None, OpenMode::Create) => {
                        w.files.push(SimFile { name: name.to_vec(), data: Vec::new() });
                        w.files.len() - 1
                    }
                };
                IoResult::File {
                    handle: self.insert(What::File(at)),
                    len: w.files[at].data.len() as u64,
                }
            }
            Op::Stat { path } => match find(w, path.of(names)) {
                Some(i) => {
                    IoResult::File { handle: Handle::NONE, len: w.files[i].data.len() as u64 }
                }
                None => IoResult::Err(IoError::NotFound),
            },
            // An open handle keeps its index, so an entry that loses its name is emptied in place.
            Op::Rename { from, to } => {
                let (from, to) = (from.of(names), to.of(names));
                let Some(i) = find(w, from) else {
                    return IoResult::Err(IoError::NotFound);
                };
                if let Some(old) = find(w, to).filter(|old| *old != i) {
                    w.files[old].name.clear();
                }
                w.files[i].name = to.to_vec();
                IoResult::Ok(0)
            }
            Op::Unlink { path } => match find(w, path.of(names)) {
                Some(i) => {
                    w.files[i].name.clear();
                    IoResult::Ok(0)
                }
                None => IoResult::Err(IoError::NotFound),
            },
            Op::Read { handle, offset, at, len } => {
                let file = match self.slot(handle) {
                    Ok(What::File(i)) => *i,
                    Ok(_) => return IoResult::Err(IoError::WrongKind),
                    Err(e) => return IoResult::Err(e),
                };
                let Some(buf) = s.buf.as_mut() else {
                    return IoResult::Err(IoError::Unsupported);
                };
                let data = &w.files[file].data;
                let from = (offset as usize).min(data.len());
                let full = buf.memory.as_mut_full();
                let at = (at as usize).min(full.len());
                let dst = &mut full[at..];
                let mut n = dst.len().min(len as usize).min(data.len() - from);
                if w.faults.split && n > 1 {
                    n = self.prng.range(1, n as u64 + 1) as usize;
                }
                dst[..n].copy_from_slice(&data[from..from + n]);
                buf.memory.set_len(at + n);
                IoResult::Ok(n)
            }
            Op::Write { handle, offset } => {
                let file = match self.slot(handle) {
                    Ok(What::File(i)) => *i,
                    Ok(_) => return IoResult::Err(IoError::WrongKind),
                    Err(e) => return IoResult::Err(e),
                };
                let Some(buf) = s.buf.as_ref() else {
                    return IoResult::Err(IoError::Unsupported);
                };
                let (src, data) = (buf.memory.data(), &mut w.files[file].data);
                let end = offset as usize + src.len();
                if data.len() < end {
                    data.resize(end, 0);
                }
                data[offset as usize..end].copy_from_slice(src);
                IoResult::Ok(src.len())
            }
            _ => IoResult::Err(IoError::Unsupported),
        }
    }

    fn connect(&mut self, w: &mut World, s: Submission, addr: SocketAddr) {
        let now = self.now();
        let f = w.faults;
        let peer = w.peers.iter().position(|p| p.addr == addr && !p.datagrams());
        let late = now + self.latency(&f);
        let (Some(peer), 0) = (peer, w.faults.refuse) else {
            w.faults.refuse = w.faults.refuse.saturating_sub(1);
            w.failures += 1;
            return self.fly(s, late, IoResult::Err(IoError::Refused), 0);
        };
        if w.faults.time_out > 0 {
            w.faults.time_out -= 1;
            w.failures += 1;
            return self.fly(s, now + f.connect_timeout, IoResult::Err(IoError::TimedOut), 0);
        }
        let mut conn = Conn {
            peer,
            id: w.peers[peer].conns,
            inbox: Vec::new(),
            base: 0,
            sent: Vec::new(),
            held: (0, 0),
            rx: Rx::Idle,
            link: Link::Open,
            given: 0,
            reset_at: NONE,
            stall_at: NONE,
            last: [0; 2],
            quiet_since: now,
            declared: false,
        };
        w.peers[peer].conns += 1;
        w.connects += 1;
        let spared = w.faults.spare > 0;
        w.faults.spare = w.faults.spare.saturating_sub(1);
        if !spared && w.faults.resets > 0 {
            w.faults.resets -= 1;
            conn.reset_at = self.prng.range(0, f.reset_within.max(1));
        }
        if !spared && w.faults.stalls > 0 {
            w.faults.stalls -= 1;
            conn.stall_at = self.prng.range(0, f.stall_within.max(1));
        }
        let handle = self.insert(What::Conn(Box::new(conn)));
        self.fly(s, late, IoResult::Socket { handle, addr }, 0);
    }

    fn idle_closed(c: &mut Conn, f: &Faults, now: Ns) -> bool {
        let quiet = c.rx.left() == 0
            && c.inbox.is_empty()
            && now.saturating_sub(c.quiet_since) >= f.idle_close;
        if c.link == Link::Open && f.idle_close != NONE && quiet {
            c.link = Link::Closed;
        }
        c.link == Link::Closed
    }

    fn serve(&mut self, w: &mut World, h: Handle) {
        let now = self.now();
        let Ok(c) = self.conn(h) else { return };
        if c.link != Link::Open {
            return;
        }
        c.quiet_since = now;
        match &mut w.peers[c.peer].serve {
            Serve::Pattern { key, len } => {
                if let (Rx::Idle, Some(head)) = (&c.rx, c.inbox.first_chunk::<8>()) {
                    let next = u64::from_le_bytes(*head).min(*len);
                    c.rx = Rx::Pattern { key: *key, next, end: *len };
                    c.link = Link::Closed;
                    c.inbox.drain(..8);
                }
            }
            Serve::Custom(serve) => {
                // Behind what was written before, so a receive in flight keeps its place in the
                // stream.
                let (mut out, at) = match std::mem::replace(&mut c.rx, Rx::Idle) {
                    Rx::Bytes { data, at } => (data, at),
                    _ => (Vec::new(), 0),
                };
                let unread = (out.len() - at) as u64;
                let mut x = Exchange {
                    conn: c.id,
                    input: &c.inbox,
                    consumed: 0,
                    out: &mut out,
                    after: After::Open,
                    unread,
                    base: c.base,
                    sent: &c.sent,
                };
                serve(&mut x);
                let (consumed, after) = (x.consumed.min(c.inbox.len()), x.after);
                c.inbox.drain(..consumed);
                c.base += consumed as u64;
                let spent = c.sent.iter().skip(1).take_while(|s| s.0 <= c.base).count();
                c.sent.drain(..spent);
                c.rx = Rx::Bytes { data: out, at };
                c.declared |= after != After::Open;
                c.link = match after {
                    After::Open => Link::Open,
                    After::Close => Link::Closed,
                    After::Reset => Link::Reset,
                };
            }
            Serve::Datagram(_) => {}
        }
    }

    fn feed(&mut self, w: &World, s: &mut Submission, h: Handle) -> Option<(IoResult, Ns, u64)> {
        let now = self.now();
        let f = w.faults;
        let late = now + self.latency(&f);
        let split = self.prng.next_u64();
        let (duplicate, skip, lose) = (
            self.breaks(w, Break::Duplicate),
            self.breaks(w, Break::Skip),
            self.breaks(w, Break::Lose),
        );
        let free = self.carried.max(now);
        let pace = f.rate_at(free);
        let c = match self.conn(h) {
            Ok(c) => c,
            Err(e) => return Some((IoResult::Err(e), late, 0)),
        };
        if c.link == Link::Reset || c.given >= c.reset_at {
            c.link = Link::Reset;
            return Some((IoResult::Err(IoError::Reset), late, 0));
        }
        let left = c.rx.left().min(c.reset_at - c.given);
        if left == 0 {
            return Self::idle_closed(c, &f, now).then_some((IoResult::Ok(0), late, 0));
        }
        c.quiet_since = now;
        let Some(buf) = s.buf.as_mut() else {
            return Some((IoResult::Err(IoError::Unsupported), late, 0));
        };
        let dst = buf.memory.as_mut_full();
        let mut n = (dst.len() as u64).min(left);
        if f.piece > 0 {
            n = n.min(f.piece);
        }
        if pace > 0 {
            n = n.min((pace.saturating_mul(RATE_QUANTUM) / SEC).max(1));
        }
        if f.split && n > 1 {
            n = 1 + split % n;
        }
        let late = match pace {
            0 => late,
            pace => late.max(free.saturating_add(n.saturating_mul(SEC).div_ceil(pace))),
        };
        match &mut c.rx {
            Rx::Pattern { key, next, .. } => {
                for (i, b) in dst[..n as usize].iter_mut().enumerate() {
                    *b = pattern(*key, *next + i as u64);
                }
                *next += if duplicate { 0 } else { n + skip as u64 };
            }
            Rx::Bytes { data, at } => {
                dst[..n as usize].copy_from_slice(&data[*at..*at + n as usize]);
                *at +=
                    if duplicate { 0 } else { (n as usize + skip as usize).min(data.len() - *at) };
            }
            Rx::Idle => {}
        }
        buf.memory.set_len(n as usize);
        let stalls = c.given <= c.stall_at && c.stall_at < c.given + n;
        c.given += n;
        let at = match (lose, stalls, f.stall_for) {
            (true, ..) | (_, true, NONE) => NONE,
            (_, true, hold) => late + hold,
            _ => late,
        };
        if stalls {
            c.stall_at = NONE;
        }
        if pace > 0 {
            self.carried = late;
        }
        Some((IoResult::Ok(n as usize), at, if duplicate { 0 } else { n }))
    }

    fn recv(&mut self, w: &World, mut s: Submission, h: Handle) {
        match self.slot(h) {
            Ok(What::Udp(_)) => {
                let behind = self.flying.iter().any(|o| {
                    o.at == NONE && matches!(o.sub.kind, Op::Recv { handle } if handle == h)
                });
                let got = match self.slot(h) {
                    Ok(What::Udp(sock)) if !behind => sock.inbox.pop_front(),
                    _ => None,
                };
                let now = self.now();
                return match got {
                    Some((_, d)) => {
                        let n = Self::fill(&mut s, &d);
                        self.fly(s, now, IoResult::Ok(n), 0)
                    }
                    None => self.fly(s, NONE, IoResult::Cancelled, 0),
                };
            }
            Ok(What::Listener) => return self.fly(s, NONE, IoResult::Cancelled, 0),
            Ok(What::Conn(_)) => {}
            Ok(_) => return self.fly(s, self.now(), IoResult::Err(IoError::WrongKind), 0),
            Err(e) => return self.fly(s, self.now(), IoResult::Err(e), 0),
        }
        let behind = self
            .flying
            .iter()
            .any(|o| o.at == NONE && matches!(o.sub.kind, Op::Recv { handle } if handle == h));
        match (behind, self.feed(w, &mut s, h)) {
            (false, Some((result, at, took))) => {
                let at = self.stream_deadline(h, RECV, at);
                self.fly(s, at, result, took)
            }
            _ => self.fly(s, NONE, IoResult::Cancelled, 0),
        }
    }

    fn wake_recvs(&mut self, w: &World, h: Handle) {
        loop {
            let waits = |o: &Flying| {
                o.at == NONE
                    && o.took == 0
                    && matches!(o.sub.kind, Op::Recv { handle } if handle == h)
            };
            let Some(i) = (0..self.flying.len())
                .filter(|i| waits(&self.flying[*i]))
                .min_by_key(|i| self.flying[*i].seq)
            else {
                return;
            };
            let mut o = self.flying.swap_remove(i);
            match self.feed(w, &mut o.sub, h) {
                Some((result, at, took)) => {
                    (o.result, o.took) = (result, took);
                    o.at = self.stream_deadline(h, RECV, at);
                    let lost = o.at == NONE;
                    self.flying.push(o);
                    if lost {
                        return;
                    }
                }
                None => return self.flying.push(o),
            }
        }
    }

    fn send(&mut self, w: &mut World, s: Submission, h: Handle, skip: u32) {
        let now = self.now();
        let f = w.faults;
        let late = now + self.latency(&f);
        let part = self.prng.next_u64();
        let data = s.buf.as_ref().map_or(&[][..], |b| b.memory.data());
        let data = data.get(skip as usize..).unwrap_or(&[]);
        let mut lost = false;
        let mut datagram = None;
        let segment = match s.kind {
            Op::SendMany { segment, .. } if segment > 0 => segment as usize,
            _ => data.len().max(1),
        };
        let result = match self.slot(h) {
            Ok(What::Udp(sock)) => {
                let to = match s.kind {
                    Op::SendTo { addr, .. } | Op::SendMany { addr: Some(addr), .. } => Some(addr),
                    _ => sock.peer,
                };
                datagram = to.map(|to| (sock.local, to, data));
                IoResult::Ok(data.len())
            }
            Ok(What::Conn(c)) if c.link == Link::Reset => IoResult::Err(IoError::Reset),
            Ok(What::Conn(c)) => {
                let mut n = data.len();
                if f.short_writes && n > 1 {
                    n = 1 + (part % n as u64) as usize;
                }
                // Taken, as a socket takes the first write behind a close, and reaches nobody.
                if Self::idle_closed(c, &f, now) {
                    lost = c.declared;
                } else {
                    let end = c.base + c.inbox.len() as u64;
                    let (held, first) = (c.held.0.min(n as u64), c.held.1);
                    for (at, when, len) in [(end, first, held), (end + held, now, n as u64 - held)]
                    {
                        if len > 0 && c.sent.last().is_none_or(|s| s.1 != when) {
                            c.sent.push((at, when));
                        }
                    }
                    let left = (data.len() - n) as u64;
                    c.held = (left, if (n as u64) < c.held.0 { first } else { now });
                    c.inbox.extend_from_slice(&data[..n]);
                    c.quiet_since = now;
                }
                IoResult::Ok(n)
            }
            Ok(_) => IoResult::Err(IoError::WrongKind),
            Err(e) => IoResult::Err(e),
        };
        let at = self.stream_deadline(h, SEND, late);
        let udp = matches!(self.slot(h), Ok(What::Udp(_)));
        if let Some((from, to, data)) = datagram {
            for part in data.chunks(segment) {
                self.wire(w, from, to, owned(part));
            }
            if data.is_empty() {
                self.wire(w, from, to, owned(data));
            }
        }
        self.fly(s, at, result, 0);
        w.sent_after_close += u32::from(lost);
        if !udp && matches!(result, IoResult::Ok(n) if n > 0) {
            self.serve(w, h);
            self.wake_recvs(w, h);
        }
    }

    fn fill(s: &mut Submission, d: &[u8]) -> usize {
        let Some(buf) = s.buf.as_mut() else { return 0 };
        let full = buf.memory.as_mut_full();
        let n = d.len().min(full.len());
        full[..n].copy_from_slice(&d[..n]);
        buf.memory.set_len(n);
        n
    }

    fn wire(&mut self, w: &mut World, from: SocketAddr, to: SocketAddr, mut data: Vec<u8>) {
        let u = w.udp;
        let to_peer = w.peers.iter().any(|p| p.addr == to && p.datagrams());
        if (u.black_hole && to_peer) || (u.loss > 0 && self.prng.chance(u.loss as u32)) {
            w.udp_lost += 1;
            return;
        }
        let copies = 1 + u32::from(u.dup > 0 && self.prng.chance(u.dup as u32));
        w.udp_duplicated += copies - 1;
        let now = self.now();
        for i in 0..copies {
            let at = now + self.prng.range(u.delay.0, u.delay.1.saturating_add(1));
            w.wire_seq += 1;
            let data = if i + 1 < copies { data.clone() } else { std::mem::take(&mut data) };
            w.wire.push(Wire { at, seq: w.wire_seq, from, to, data });
        }
    }

    fn socket_at(&self, addr: SocketAddr) -> Option<Handle> {
        self.slots.iter().enumerate().find_map(|(i, s)| match &s.what {
            What::Udp(sock) if sock.local == addr => Some(Handle::new(i as u32, s.generation)),
            _ => None,
        })
    }

    fn carry_wire(&mut self) {
        let world = Arc::clone(&self.world);
        loop {
            let now = self.now();
            let mut w = lock(&world);
            let next = (0..w.wire.len())
                .filter(|i| {
                    w.wire[*i].at <= now
                        && (Self::to_peer(&w, w.wire[*i].to)
                            || self.socket_at(w.wire[*i].to).is_some()
                            || !w.bound.contains(&w.wire[*i].to))
                })
                .min_by_key(|i| (w.wire[*i].at, w.wire[*i].seq));
            let wake =
                (0..w.peers.len()).find(|i| w.peers[*i].datagrams() && w.peers[*i].wake <= now);
            let Some(i) = next else {
                let Some(p) = wake else { return };
                self.call_peer(&mut w, p, None);
                continue;
            };
            let d = w.wire.swap_remove(i);
            if let Some(p) = w.peers.iter().position(|p| p.addr == d.to && p.datagrams()) {
                self.call_peer(&mut w, p, Some((d.from, &d.data)));
                continue;
            }
            drop(w);
            let Some(h) = self.socket_at(d.to) else {
                continue;
            };
            let waits = |o: &Flying| {
                o.at == NONE && matches!(o.sub.kind, Op::Recv { handle } if handle == h)
            };
            match (0..self.flying.len())
                .filter(|i| waits(&self.flying[*i]))
                .min_by_key(|i| self.flying[*i].seq)
            {
                Some(i) => {
                    let n = Self::fill(&mut self.flying[i].sub, &d.data);
                    (self.flying[i].at, self.flying[i].result) = (now, IoResult::Ok(n));
                }
                None => {
                    if let Ok(What::Udp(sock)) = self.slot(h) {
                        if sock.inbox.len() < INBOX {
                            sock.inbox.push_back((d.from, d.data));
                        }
                        self.feed_many(h);
                    }
                }
            }
        }
    }

    fn to_peer(w: &World, to: SocketAddr) -> bool {
        w.peers.iter().any(|p| p.addr == to && p.datagrams())
    }

    fn call_peer(&mut self, w: &mut World, p: usize, input: Option<(SocketAddr, &[u8])>) {
        let now = self.now();
        let mut out = Vec::new();
        let mut call = Datagrams { now, input, out: &mut out, wake: w.peers[p].wake };
        if let Serve::Datagram(serve) = &mut w.peers[p].serve {
            serve(&mut call);
        }
        let wake = call.wake;
        // A wake already past is one at once, never one again and again.
        w.peers[p].wake = if wake <= now { now + 1 } else { wake };
        let from = w.peers[p].addr;
        for (to, data) in out {
            self.wire(w, from, to, data);
        }
    }

    fn wire_due(&self) -> Ns {
        let w = lock(&self.world);
        w.wire
            .iter()
            .filter(|d| {
                Self::to_peer(&w, d.to)
                    || self.socket_at(d.to).is_some()
                    || !w.bound.contains(&d.to)
            })
            .map(|d| d.at)
            .min()
            .unwrap_or(NONE)
    }

    fn peer_wake(&self) -> Ns {
        let w = lock(&self.world);
        w.peers.iter().filter(|p| p.datagrams()).map(|p| p.wake).min().unwrap_or(NONE)
    }

    fn cancel(&mut self, w: &World, target: OpId) {
        let Some(i) = self.flying.iter().position(|o| o.sub.op == target) else {
            return;
        };
        match self.flying[i].sub.kind {
            // A lend under a running receive comes back when the receive ends.
            Op::Lend { handle } if self.flying[i].at == NONE && self.many(handle).is_some() => {
                return;
            }
            Op::RecvMany { handle } if self.flying[i].at == NONE => {
                return self.end_many(handle, IoResult::Cancelled);
            }
            _ => {}
        }
        let mut o = self.flying.swap_remove(i);
        match (o.sub.kind, o.result) {
            (Op::Connect { .. }, IoResult::Socket { handle, .. }) => self.release(handle),
            (Op::Recv { handle }, IoResult::Ok(_))
                if w.breakage == Break::CancelCompletes && o.took > 0 =>
            {
                // The defect. The bytes stay taken and reach the element.
                let _ = handle;
                (o.at, o.sealed) = (self.now(), true);
                return self.flying.push(o);
            }
            (Op::Recv { handle }, _) if o.took > 0 => {
                if let Ok(c) = self.conn(handle) {
                    c.given -= o.took;
                    match &mut c.rx {
                        Rx::Pattern { next, .. } => *next -= o.took,
                        Rx::Bytes { at, .. } => *at -= o.took as usize,
                        Rx::Idle => {}
                    }
                }
                if let Some(buf) = o.sub.buf.as_mut() {
                    buf.memory.set_len(0);
                }
            }
            _ => {}
        }
        (o.at, o.result, o.took) = (self.now(), IoResult::Cancelled, 0);
        self.flying.push(o);
    }

    fn many(&self, h: Handle) -> Option<usize> {
        self.flying.iter().position(|o| {
            o.at == NONE && matches!(o.sub.kind, Op::RecvMany { handle } if handle == h)
        })
    }

    fn lent(&self, h: Handle) -> Option<usize> {
        let waits =
            |o: &Flying| o.at == NONE && matches!(o.sub.kind, Op::Lend { handle } if handle == h);
        (0..self.flying.len())
            .filter(|i| waits(&self.flying[*i]))
            .min_by_key(|i| self.flying[*i].seq)
    }

    fn lend(&mut self, mut s: Submission, h: Handle) {
        let now = self.now();
        if let Some(buf) = s.buf.as_mut() {
            buf.memory.widen();
        }
        match self.slot(h) {
            Ok(What::Udp(_)) => {}
            Ok(_) => return self.fly(s, now, IoResult::Err(IoError::WrongKind), 0),
            Err(e) => return self.fly(s, now, IoResult::Err(e), 0),
        }
        self.fly(s, NONE, IoResult::Cancelled, 0);
        self.feed_many(h);
    }

    fn recv_many(&mut self, s: Submission, h: Handle) {
        let now = self.now();
        let (running, lent) = (self.many(h).is_some(), self.lent(h).is_some());
        match self.slot(h) {
            Ok(What::Udp(_)) if running => {
                return self.fly(s, now, IoResult::Err(IoError::Unsupported), 0);
            }
            Ok(What::Udp(_)) if !lent => return self.fly(s, now, IoResult::Err(IoError::Dry), 0),
            Ok(What::Udp(_)) => {}
            Ok(_) => return self.fly(s, now, IoResult::Err(IoError::WrongKind), 0),
            Err(e) => return self.fly(s, now, IoResult::Err(e), 0),
        }
        self.fly(s, NONE, IoResult::Cancelled, 0);
        self.feed_many(h);
    }

    /// The last lent buffer filled ends the receive dry, as the OS backend's does.
    fn feed_many(&mut self, h: Handle) {
        if self.many(h).is_none() {
            return;
        }
        let now = self.now();
        loop {
            let unread = matches!(self.slot(h), Ok(What::Udp(sock)) if !sock.inbox.is_empty());
            if !unread {
                return;
            }
            let Some(i) = self.lent(h) else {
                return self.end_many(h, IoResult::Err(IoError::Dry));
            };
            let Ok(What::Udp(sock)) = self.slot(h) else {
                return;
            };
            let Some((from, d)) = sock.inbox.pop_front() else {
                return;
            };
            let len = Self::fill(&mut self.flying[i].sub, &d);
            (self.flying[i].at, self.flying[i].result) =
                (now, IoResult::Datagram { len, from, segment: 0 });
            if self.lent(h).is_none() {
                return self.end_many(h, IoResult::Err(IoError::Dry));
            }
        }
    }

    fn end_many(&mut self, h: Handle, result: IoResult) {
        let now = self.now();
        let Some(i) = self.many(h) else { return };
        (self.flying[i].at, self.flying[i].result) = (now, result);
        while let Some(j) = self.lent(h) {
            (self.flying[j].at, self.flying[j].result) = (now, IoResult::Cancelled);
        }
    }

    fn close(&mut self, w: &mut World, s: Submission, h: Handle) {
        let now = self.now();
        if let Err(e) = self.slot(h) {
            return self.fly(s, now, IoResult::Err(e), 0);
        }
        let on = |k: &Op| match k {
            Op::Read { handle, .. }
            | Op::Write { handle, .. }
            | Op::Recv { handle }
            | Op::Send { handle, .. }
            | Op::SendTo { handle, .. }
            | Op::Accept { handle }
            | Op::Lend { handle }
            | Op::RecvMany { handle }
            | Op::SendMany { handle, .. } => *handle == h,
            _ => false,
        };
        for o in self.flying.iter_mut().filter(|o| on(&o.sub.kind) && !o.sealed) {
            (o.at, o.result, o.took) = (now, IoResult::Cancelled, 0);
        }
        if let Ok(What::Udp(sock)) = self.slot(h) {
            let local = sock.local;
            w.bound.retain(|b| *b != local);
        }
        self.release(h);
        self.fly(s, now, IoResult::Ok(0), 0);
    }
}

impl Reactor for SimReactor {
    fn submit(&mut self, subs: &mut Vec<Submission>, names: &[u8]) {
        let world = Arc::clone(&self.world);
        let mut w = lock(&world);
        for mut s in subs.drain(..) {
            let now = self.now();
            let f = w.faults;
            match s.kind {
                Op::Open { .. }
                | Op::Stat { .. }
                | Op::Rename { .. }
                | Op::Unlink { .. }
                | Op::Read { .. }
                | Op::Write { .. } => {
                    let result = self.file_op(&mut w, &mut s, names);
                    let at = now + self.latency(&f);
                    self.fly(s, at, result, 0);
                }
                // The simulated world has no descriptors.
                Op::Adopt { .. } => {
                    self.fly(s, now, IoResult::Err(IoError::Unsupported), 0);
                }
                Op::Close { handle } => self.close(&mut w, s, handle),
                Op::Resolve { host, port, skip } => {
                    let name = host.of(names);
                    let mut found =
                        w.peers.iter().filter(|p| p.host.as_bytes() == name && !p.datagrams());
                    let mut result = match found.nth(skip as usize) {
                        Some(p) => IoResult::Addr(SocketAddr::new(p.addr.ip(), port)),
                        None => IoResult::Err(IoError::NoAddress),
                    };
                    if w.faults.dns_fail > 0 {
                        w.faults.dns_fail -= 1;
                        result = IoResult::Err(IoError::NoAddress);
                    }
                    let at = now + self.latency(&f);
                    self.fly(s, at, result, 0);
                }
                Op::Lookup { host, kind } => {
                    let name = host.of(names);
                    let found = w
                        .records
                        .iter()
                        .find(|r| r.host == name && r.kind == kind)
                        .map(|r| &r.answer);
                    let late = now + self.latency(&f);
                    let (result, at) = match (found, s.buf.as_mut()) {
                        (Some(Lookup::Message(msg)), Some(buf)) => {
                            let full = buf.memory.as_mut_full();
                            let n = msg.len().min(full.len());
                            full[..n].copy_from_slice(&msg[..n]);
                            buf.memory.set_len(n);
                            (IoResult::Record(n), late)
                        }
                        (Some(Lookup::Silent), _) => {
                            (IoResult::Err(IoError::NoRecord), now + SILENT_LOOKUP)
                        }
                        _ => (IoResult::Err(IoError::NoRecord), late),
                    };
                    self.fly(s, at, result, 0);
                }
                Op::Connect { addr } => self.connect(&mut w, s, addr),
                Op::Bind { addr } => {
                    let local = match addr.port() {
                        0 => {
                            let port = w.next_port;
                            w.next_port = w.next_port.wrapping_add(1).max(EPHEMERAL);
                            let ip = match addr.ip() {
                                IpAddr::V4(ip) if ip.is_unspecified() => {
                                    IpAddr::V4(Ipv4Addr::new(10, 255, 0, 1))
                                }
                                IpAddr::V6(ip) if ip.is_unspecified() => {
                                    IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1))
                                }
                                ip => ip,
                            };
                            SocketAddr::new(ip, port)
                        }
                        _ => addr,
                    };
                    w.bound.push(local);
                    let handle = self.insert(What::Udp(Socket::boxed(local)));
                    self.fly(s, now, IoResult::Socket { handle, addr: local }, 0);
                }
                Op::Listen { addr } => {
                    let handle = self.insert(What::Listener);
                    self.fly(s, now, IoResult::Socket { handle, addr }, 0);
                }
                Op::Peer { handle, addr } => {
                    let result = match self.slot(handle) {
                        Ok(What::Udp(sock)) => {
                            sock.peer = Some(addr);
                            IoResult::Ok(0)
                        }
                        Ok(_) => IoResult::Err(IoError::WrongKind),
                        Err(e) => IoResult::Err(e),
                    };
                    self.fly(s, now, result, 0);
                }
                Op::Accept { handle } | Op::Recv { handle } => self.recv(&w, s, handle),
                Op::Send { handle, skip } => self.send(&mut w, s, handle, skip),
                Op::SendTo { handle, .. } | Op::SendMany { handle, .. } => {
                    self.send(&mut w, s, handle, 0)
                }
                Op::Lend { handle } => self.lend(s, handle),
                Op::RecvMany { handle } => self.recv_many(s, handle),
                Op::Timer { after } => {
                    let after = u64::try_from(after.as_nanos()).unwrap_or(NONE);
                    self.fly(s, now.saturating_add(after), IoResult::Ok(0), 0);
                }
                Op::Cancel { target } => self.cancel(&w, target),
            }
        }
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        out.clear();
        self.carry_wire();
        let now = self.now();
        // In deadline order, and in submission order among equals.
        while let Some(i) = (0..self.flying.len())
            .filter(|i| self.flying[*i].at <= now)
            .min_by_key(|i| (self.flying[*i].at, self.flying[*i].seq))
        {
            let o = self.flying.swap_remove(i);
            let (s, result) = (o.sub, o.result);
            out.push((s.element, Completion { op: s.op, user: s.user, result, buf: s.buf }));
        }
        if !out.is_empty() {
            lock(&self.world).completed += out.len() as u64;
        }
    }

    fn due_in(&self) -> Option<Duration> {
        let flying = self.flying.iter().map(|o| o.at).min().unwrap_or(NONE);
        let next = flying.min(self.wire_due()).min(self.peer_wake());
        (next != NONE).then(|| Duration::from_nanos(next.saturating_sub(self.now())))
    }

    fn is_idle(&self) -> bool {
        let now = self.now();
        !self.flying.iter().any(|o| o.at <= now) && self.wire_due() > now && self.peer_wake() > now
    }
}
