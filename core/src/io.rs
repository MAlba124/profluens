use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::buffer::Buffer;
use crate::id::ElementId;
pub use crate::station::Waker;

/// An op id packing `(element, seq)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct OpId(pub u64);

/// A reactor's generational name for an open file or socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Handle(u64);

impl Handle {
    pub const NONE: Handle = Handle(u64::MAX);

    pub const fn new(slot: u32, generation: u32) -> Handle {
        Handle(((generation as u64) << 32) | slot as u64)
    }

    pub const fn slot(self) -> u32 {
        self.0 as u32
    }

    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub const fn is_none(self) -> bool {
        self.0 == u64::MAX
    }
}

/// A path or host name as a span of the bytes handed to [`Reactor::submit`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Name {
    pub off: u32,
    pub len: u32,
}

impl Name {
    pub fn of(self, names: &[u8]) -> &[u8] {
        names.get(self.off as usize..(self.off + self.len) as usize).unwrap_or(&[])
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenMode {
    Read,
    /// Read once front to back, dropping read pages from the page cache.
    ReadOnce,
    /// Create or replace a file for writing, with its parent directories.
    Create,
}

/// One operation, plain data of a fixed size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Completes as [`IoResult::File`].
    Open {
        path: Name,
        mode: OpenMode,
    },
    /// A file the element already holds open, served without resolving any path. The reactor
    /// duplicates `fd` when the op is submitted. Completes as [`IoResult::File`].
    Adopt {
        fd: RawFd,
        mode: OpenMode,
    },
    /// Completes as [`IoResult::File`] with no handle.
    Stat {
        path: Name,
    },
    Rename {
        from: Name,
        to: Name,
    },
    Unlink {
        path: Name,
    },
    /// Close a file or socket, cancelling its ops in flight first.
    Close {
        handle: Handle,
    },
    /// Positioned read of up to `len` bytes into the window from byte `at` on. `Ok(0)` is EOF.
    Read {
        handle: Handle,
        offset: u64,
        at: u32,
        len: u32,
    },
    Write {
        handle: Handle,
        offset: u64,
    },
    /// The `skip`th address of `host`, as [`IoResult::Addr`].
    Resolve {
        host: Name,
        port: u16,
        skip: u8,
    },
    /// The system resolver's DNS answer for `host` records of type `kind` in class IN.
    Lookup {
        host: Name,
        kind: u16,
    },
    /// A TCP connection. Completes as [`IoResult::Socket`] with the peer.
    Connect {
        addr: SocketAddr,
    },
    /// A UDP socket bound to `addr`. Completes with the local address.
    Bind {
        addr: SocketAddr,
    },
    /// The default peer of a UDP socket, which then takes datagrams from it alone.
    Peer {
        handle: Handle,
        addr: SocketAddr,
    },
    /// A TCP listener. Completes with the local address.
    Listen {
        addr: SocketAddr,
    },
    /// One connection off a listener. Completes with the peer.
    Accept {
        handle: Handle,
    },
    /// Whatever the socket has, up to the buffer. `Ok(0)` is a stream peer's close.
    Recv {
        handle: Handle,
    },
    /// Send the buffer's bytes from `skip` on. `Ok(n)` says how many a stream took.
    Send {
        handle: Handle,
        skip: u32,
    },
    SendTo {
        handle: Handle,
        addr: SocketAddr,
    },
    /// Hand the buffer to the socket's [`Op::RecvMany`], completing when it comes back filled or
    /// `Cancelled`.
    Lend {
        handle: Handle,
    },
    /// A receive that stays in flight, filling lent buffers. Completes once, when it ends.
    RecvMany {
        handle: Handle,
    },
    /// The buffer as datagrams of `segment` bytes in one send (`UDP_SEGMENT`). Zero is one
    /// datagram.
    SendMany {
        handle: Handle,
        addr: Option<SocketAddr>,
        segment: u16,
    },
    /// The fd of `handle` readable, polled for it, nothing read. Completes as `Ok` with the
    /// events. A dma-buf turns readable once every job writing it is done.
    Ready {
        handle: Handle,
    },
    /// Completes `after` this long on the reactor's own time.
    Timer {
        after: Duration,
    },
    /// Complete `target` as cancelled unless it already completed. Has no completion itself.
    Cancel {
        target: OpId,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IoError {
    NotFound,
    Denied,
    NotAFile,
    BadHandle,
    WrongKind,
    Refused,
    Reset,
    TimedOut,
    Unreachable,
    AddrInUse,
    /// The name has no address, or none at this index.
    NoAddress,
    /// The name has no record of the type asked for.
    NoRecord,
    /// A many-shot receive has no lent buffer left to fill.
    Dry,
    /// The reactor is at one of its limits.
    Full,
    NoBackend,
    Unsupported,
    /// Any other error of the OS, by number.
    Os(i32),
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            IoError::NotFound => "not found",
            IoError::Denied => "permission denied",
            IoError::NotAFile => "not a regular file",
            IoError::BadHandle => "stale handle",
            IoError::WrongKind => "wrong kind of handle",
            IoError::Refused => "connection refused",
            IoError::Reset => "connection reset",
            IoError::TimedOut => "timed out",
            IoError::Unreachable => "unreachable",
            IoError::AddrInUse => "address in use",
            IoError::NoAddress => "no address",
            IoError::NoRecord => "no record",
            IoError::Dry => "no lent buffer left",
            IoError::Full => "reactor full",
            IoError::NoBackend => "no reactor backend",
            IoError::Unsupported => "unsupported by the backend",
            IoError::Os(n) => return write!(f, "os error {n}"),
        };
        f.write_str(s)
    }
}

impl std::error::Error for IoError {}

/// The length of a pipe, which has none.
pub const NO_LEN: u64 = u64::MAX;

/// Bytes at the front of a buffer lent to a UDP socket a backend may take for the receive's header.
pub const LEND_HEAD: usize = 256;

/// A lent buffer at least this large may take coalesced datagrams (`UDP_GRO`).
pub const COALESCE_ROOM: usize = LEND_HEAD + 65_535;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IoResult {
    /// Bytes moved, and zero for an op that moves none.
    Ok(usize),
    /// An opened file and its length, [`NO_LEN`] for a pipe read with `Recv`.
    File {
        handle: Handle,
        len: u64,
    },
    /// A socket and the address its op names.
    Socket {
        handle: Handle,
        addr: SocketAddr,
    },
    Addr(SocketAddr),
    /// A DNS message of this many bytes in the op's buffer.
    Record(usize),
    /// A lent buffer filled with `len` bytes from `from`, datagrams of `segment` bytes when
    /// coalesced.
    Datagram {
        len: usize,
        from: SocketAddr,
        segment: u16,
    },
    Cancelled,
    Err(IoError),
}

pub struct Completion {
    pub op: OpId,
    pub user: u64,
    pub result: IoResult,
    pub buf: Option<Buffer>,
}

pub struct Submission {
    pub op: OpId,
    pub element: ElementId,
    pub kind: Op,
    pub buf: Option<Buffer>,
    pub user: u64,
}

#[derive(Default)]
pub(crate) struct Mailbox {
    pub(crate) inbox: VecDeque<Completion>,
    pub(crate) outbox: Vec<Submission>,
    pub(crate) names: Vec<u8>,
    next_op: u64,
    pub(crate) flying: u32,
}

/// The element's IO handle, borrowed from `Ctx` for one call.
pub struct Io<'c> {
    element: ElementId,
    mail: &'c mut Mailbox,
    credits: u32,
}

impl<'c> Io<'c> {
    pub(crate) fn new(element: ElementId, mail: &'c mut Mailbox, credits: u32) -> Self {
        Self { element, mail, credits }
    }

    /// How many ops this element may keep in flight.
    pub fn credits(&self) -> u32 {
        self.credits
    }

    fn push(&mut self, kind: Op, buf: Option<Buffer>, user: u64) -> OpId {
        let op = OpId(((self.element.0 as u64) << 32) | self.mail.next_op);
        self.mail.next_op = (self.mail.next_op + 1) & u32::MAX as u64;
        if self.mail.outbox.capacity() == 0 {
            self.mail.outbox.reserve((self.credits as usize).clamp(4, 64));
        }
        self.mail.flying += !matches!(kind, Op::Cancel { .. }) as u32;
        self.mail.outbox.push(Submission { op, element: self.element, kind, buf, user });
        op
    }

    fn name(&mut self, bytes: &[u8]) -> Name {
        let off = self.mail.names.len() as u32;
        self.mail.names.extend_from_slice(bytes);
        Name { off, len: bytes.len() as u32 }
    }

    fn path(&mut self, path: &Path) -> Name {
        self.name(path.as_os_str().as_encoded_bytes())
    }

    pub fn open(&mut self, path: &Path, mode: OpenMode, user: u64) -> OpId {
        let path = self.path(path);
        self.push(Op::Open { path, mode }, None, user)
    }

    /// Serve `file` as if opened with `mode`. It must stay open until this op completes.
    pub fn adopt(&mut self, file: BorrowedFd<'_>, mode: OpenMode, user: u64) -> OpId {
        self.push(Op::Adopt { fd: file.as_raw_fd(), mode }, None, user)
    }

    pub fn stat(&mut self, path: &Path, user: u64) -> OpId {
        let path = self.path(path);
        self.push(Op::Stat { path }, None, user)
    }

    pub fn rename(&mut self, from: &Path, to: &Path, user: u64) -> OpId {
        let (from, to) = (self.path(from), self.path(to));
        self.push(Op::Rename { from, to }, None, user)
    }

    pub fn unlink(&mut self, path: &Path, user: u64) -> OpId {
        let path = self.path(path);
        self.push(Op::Unlink { path }, None, user)
    }

    pub fn close(&mut self, handle: Handle, user: u64) -> OpId {
        self.push(Op::Close { handle }, None, user)
    }

    pub fn read(&mut self, handle: Handle, offset: u64, buf: Buffer, user: u64) -> OpId {
        self.push(Op::Read { handle, offset, at: 0, len: u32::MAX }, Some(buf), user)
    }

    /// Read up to `len` bytes at `offset` into the window from byte `at` on.
    pub fn read_range(
        &mut self,
        handle: Handle,
        offset: u64,
        at: u32,
        len: u32,
        buf: Buffer,
        user: u64,
    ) -> OpId {
        self.push(Op::Read { handle, offset, at, len }, Some(buf), user)
    }

    pub fn write(&mut self, handle: Handle, offset: u64, buf: Buffer, user: u64) -> OpId {
        self.push(Op::Write { handle, offset }, Some(buf), user)
    }

    pub fn resolve(&mut self, host: &str, port: u16, skip: u8, user: u64) -> OpId {
        let host = self.name(host.as_bytes());
        self.push(Op::Resolve { host, port, skip }, None, user)
    }

    /// Ask the system resolver for the records of type `kind` of `host` into `buf`.
    pub fn lookup(&mut self, host: &str, kind: u16, buf: Buffer, user: u64) -> OpId {
        let host = self.name(host.as_bytes());
        self.push(Op::Lookup { host, kind }, Some(buf), user)
    }

    pub fn connect(&mut self, addr: SocketAddr, user: u64) -> OpId {
        self.push(Op::Connect { addr }, None, user)
    }

    pub fn bind(&mut self, addr: SocketAddr, user: u64) -> OpId {
        self.push(Op::Bind { addr }, None, user)
    }

    pub fn peer(&mut self, handle: Handle, addr: SocketAddr, user: u64) -> OpId {
        self.push(Op::Peer { handle, addr }, None, user)
    }

    pub fn listen(&mut self, addr: SocketAddr, user: u64) -> OpId {
        self.push(Op::Listen { addr }, None, user)
    }

    pub fn accept(&mut self, handle: Handle, user: u64) -> OpId {
        self.push(Op::Accept { handle }, None, user)
    }

    /// Receive into `buf`. One recv per stream at a time keeps its bytes in order.
    pub fn recv(&mut self, handle: Handle, buf: Buffer, user: u64) -> OpId {
        self.push(Op::Recv { handle }, Some(buf), user)
    }

    /// Send `buf` from byte `skip` on. One send per stream at a time.
    pub fn send(&mut self, handle: Handle, buf: Buffer, skip: u32, user: u64) -> OpId {
        self.push(Op::Send { handle, skip }, Some(buf), user)
    }

    pub fn send_to(&mut self, handle: Handle, addr: SocketAddr, buf: Buffer, user: u64) -> OpId {
        self.push(Op::SendTo { handle, addr }, Some(buf), user)
    }

    /// Lend `buf` to the socket's many-shot receive. Completes when it comes back.
    pub fn lend(&mut self, handle: Handle, buf: Buffer, user: u64) -> OpId {
        self.push(Op::Lend { handle }, Some(buf), user)
    }

    /// A receive that fills lent buffers until it ends.
    pub fn recv_many(&mut self, handle: Handle, user: u64) -> OpId {
        self.push(Op::RecvMany { handle }, None, user)
    }

    /// `buf` as datagrams of `segment` bytes in one send, to `addr` or the peer.
    pub fn send_many(
        &mut self,
        handle: Handle,
        addr: Option<SocketAddr>,
        buf: Buffer,
        segment: u16,
        user: u64,
    ) -> OpId {
        self.push(Op::SendMany { handle, addr, segment }, Some(buf), user)
    }

    pub fn timer(&mut self, after: Duration, user: u64) -> OpId {
        self.push(Op::Timer { after }, None, user)
    }

    /// Completes once the fd of `handle` is readable, a dma-buf once its writers are done.
    pub fn ready(&mut self, handle: Handle, user: u64) -> OpId {
        self.push(Op::Ready { handle }, None, user)
    }

    /// Cancel `op`, also turning its completion already in the inbox into `Cancelled`.
    pub fn cancel(&mut self, op: OpId) {
        match self.mail.inbox.iter_mut().find(|c| c.op == op) {
            Some(done) => done.result = IoResult::Cancelled,
            None => drop(self.push(Op::Cancel { target: op }, None, 0)),
        }
    }

    /// The next completed op of this element, FIFO.
    pub fn next_completion(&mut self) -> Option<Completion> {
        self.mail.inbox.pop_front()
    }
}

/// Builds a reactor on each thread group's own thread.
pub type ReactorFactory = Arc<dyn Fn() -> Result<Box<dyn Reactor>, IoError> + Send + Sync>;

/// A reactor backend, one per thread group. Only `park` waits.
pub trait Reactor {
    /// Take the submitted ops in order, draining `subs`. `names` is valid for this call only.
    fn submit(&mut self, subs: &mut Vec<Submission>, names: &[u8]);
    /// Make what progress there is and put what completed in `out`, cleared first.
    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>);
    /// Time until an op completes on the reactor's own time, `None` if only an outside event can.
    fn due_in(&self) -> Option<Duration>;
    /// Nothing is left that completes without an outside event.
    fn is_idle(&self) -> bool;
    /// The wake of the group this reactor serves, given once before any op.
    fn set_waker(&mut self, _waker: Waker) {}
    /// Sleep until an op completes, the waker's bell rings or `timeout` passes, true when woken.
    fn park(&mut self, _timeout: Duration) -> bool {
        false
    }
}

/// The reactor of a pipeline with no backend, failing every op.
#[derive(Default)]
pub(crate) struct NoBackend {
    done: Vec<(ElementId, Completion)>,
}

impl Reactor for NoBackend {
    fn submit(&mut self, subs: &mut Vec<Submission>, _names: &[u8]) {
        for s in subs.drain(..).filter(|s| !matches!(s.kind, Op::Cancel { .. })) {
            let result = IoResult::Err(IoError::NoBackend);
            self.done.push((s.element, Completion { op: s.op, user: s.user, result, buf: s.buf }));
        }
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        out.clear();
        out.append(&mut self.done);
    }

    fn due_in(&self) -> Option<Duration> {
        None
    }

    fn is_idle(&self) -> bool {
        self.done.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn handle_packs_slot_and_generation() {
        let h = Handle::new(7, 3);
        assert_eq!((h.slot(), h.generation()), (7, 3));
        assert!(!h.is_none() && Handle::NONE.is_none());
        assert_ne!(h, Handle::new(7, 4), "a reused slot is another handle");
    }

    #[test]
    fn a_name_outside_its_bytes_is_empty() {
        let names = b"abcdef";
        assert_eq!(Name { off: 2, len: 3 }.of(names), b"cde");
        assert_eq!(Name { off: 4, len: 9 }.of(names), b"");
    }

    #[test]
    fn io_names_ops_and_counts_what_flies() {
        let mut mail = Mailbox::default();
        let mut io = Io::new(ElementId(5), &mut mail, 2);
        let a = io.open(Path::new("/a/b"), OpenMode::Read, 1);
        let b = io.rename(Path::new("x"), Path::new("yz"), 2);
        assert_eq!((a, b), (OpId(5 << 32), OpId((5 << 32) | 1)));
        assert_eq!(mail.flying, 2);
        let Op::Rename { from, to } = mail.outbox[1].kind else { panic!("a rename") };
        assert_eq!((from.of(&mail.names), to.of(&mail.names)), (&b"x"[..], &b"yz"[..]));
    }

    #[test]
    fn the_op_sequence_wraps_before_the_element_bits() {
        let mut mail = Mailbox { next_op: u32::MAX as u64, ..Mailbox::default() };
        let mut io = Io::new(ElementId(4), &mut mail, 2);
        let last = io.open(Path::new("/a"), OpenMode::Read, 1);
        let wrapped = io.open(Path::new("/b"), OpenMode::Read, 2);
        assert_eq!((last, wrapped), (OpId((4 << 32) | u32::MAX as u64), OpId(4 << 32)));
    }

    #[test]
    fn cancel_turns_a_waiting_completion_and_forwards_the_rest() {
        let mut mail = Mailbox::default();
        let done = Completion { op: OpId(9), user: 0, result: IoResult::Ok(4), buf: None };
        mail.inbox.push_back(done);
        let mut io = Io::new(ElementId(0), &mut mail, 1);
        io.cancel(OpId(9));
        io.cancel(OpId(10));
        assert_eq!(io.next_completion().map(|c| c.result), Some(IoResult::Cancelled));
        assert_eq!(mail.outbox.len(), 1);
        assert_eq!(mail.outbox[0].kind, Op::Cancel { target: OpId(10) });
        assert_eq!(mail.flying, 0, "a cancel has no completion to wait for");
    }

    #[test]
    fn no_backend_fails_every_op_once() {
        let mut r = NoBackend::default();
        let mut subs = vec![
            Submission {
                op: OpId(1),
                element: ElementId(2),
                kind: Op::Timer { after: Duration::ZERO },
                buf: None,
                user: 3,
            },
            Submission {
                op: OpId(2),
                element: ElementId(2),
                kind: Op::Cancel { target: OpId(1) },
                buf: None,
                user: 0,
            },
        ];
        r.submit(&mut subs, &[]);
        let mut out = Vec::new();
        r.run_once(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.result, IoResult::Err(IoError::NoBackend));
        assert!(r.is_idle() && r.due_in().is_none());
    }
}
