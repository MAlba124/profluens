use std::io::{self, ErrorKind};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};

use profluens_core::io::{Handle, IoError, IoResult, Op, OpId, Submission};
use rustix::io::Errno;
use stein_io::queue::{self, Cqe, Fd, Key};

use crate::{BELL, HINT, Kind, NONE, OsReactor, Slot, State, errno, io_error, sys};

pub(crate) enum Started {
    Flying,
    Wait,
    Done(IoResult),
}

fn side(kind: &Op) -> (bool, bool) {
    match kind {
        Op::Recv { .. } | Op::Accept { .. } | Op::RecvMany { .. } => (true, false),
        Op::Send { .. } | Op::SendTo { .. } | Op::SendMany { .. } => (false, true),
        _ => (false, false),
    }
}

fn is_file(kind: &Op) -> bool {
    matches!(kind, Op::Read { .. } | Op::Write { .. }) || is_path(kind)
}

fn is_path(kind: &Op) -> bool {
    matches!(kind, Op::Open { .. } | Op::Stat { .. } | Op::Rename { .. } | Op::Unlink { .. })
}

pub(crate) fn refused(e: stein_io::IoError) -> Started {
    match e {
        stein_io::IoError::OpsFull => Started::Wait,
        stein_io::IoError::Kernel(e) => Started::Done(IoResult::Err(errno(e))),
        _ => Started::Done(IoResult::Err(IoError::Unsupported)),
    }
}

pub(crate) fn moved(c: &Cqe) -> IoResult {
    match c.result() {
        Ok(n) => IoResult::Ok(n as usize),
        Err(e) => IoResult::Err(errno(e)),
    }
}

impl OsReactor {
    pub(crate) fn start(&mut self, idx: u32) -> Started {
        let Some(p) = self.ops[idx as usize].as_ref() else {
            return Started::Wait;
        };
        let (kind, on) = (p.kind, p.on);
        if is_path(&kind) {
            return self.start_path(idx);
        }
        let (fd, tag, busy_read, busy_write) = match self.slot(on) {
            Ok(s) => (s.raw(), tag(&s.kind), s.busy_read, s.busy_write),
            Err(e) => return Started::Done(IoResult::Err(e)),
        };
        let (read, write) = side(&kind);
        if (read && busy_read) || (write && busy_write) {
            return Started::Wait;
        }
        let Some(p) = self.ops[idx as usize].as_mut() else {
            return Started::Wait;
        };
        let progress = p.progress;
        let Some(buf) = p.buf.as_mut() else {
            let op = match (kind, tag) {
                (Op::RecvMany { .. }, Tag::Stream | Tag::Udp) => return self.start_many(idx, on),
                (Op::Accept { .. }, Tag::Listener) => {
                    queue::Op::Accept { fd: Fd::Raw(fd), multi: false }
                }
                (Op::Accept { .. }, _) => return Started::Done(IoResult::Err(IoError::WrongKind)),
                (Op::Ready { .. }, _) => {
                    queue::Op::Poll { fd: Fd::Raw(fd), events: libc::POLLIN as u32, multi: false }
                }
                _ => return Started::Done(IoResult::Err(IoError::Unsupported)),
            };
            return self.push(idx, op);
        };
        let fd = Fd::Raw(fd);
        let op = match (kind, tag) {
            (Op::Read { offset, at, len, .. }, Tag::File) => {
                queue::Op::Read { fd, mem: sys::into_range(buf, at, len), at: offset }
            }
            (Op::Write { offset, .. }, Tag::File) => {
                queue::Op::Write { fd, mem: sys::from(buf, progress), at: offset + progress as u64 }
            }
            (Op::Recv { .. }, Tag::Stream | Tag::Udp) => {
                queue::Op::Recv { fd, mem: sys::into(buf) }
            }
            (Op::Recv { .. }, Tag::Pipe) => {
                queue::Op::Poll { fd, events: libc::POLLIN as u32, multi: false }
            }
            (Op::Send { skip, .. }, Tag::Stream | Tag::Udp)
                if buf.memory.len() <= skip as usize =>
            {
                return Started::Done(IoResult::Ok(0));
            }
            (Op::Send { skip, .. }, Tag::Stream | Tag::Udp) => {
                queue::Op::Send { fd, mem: sys::from(buf, skip as usize) }
            }
            (Op::SendTo { addr, .. }, Tag::Udp) => {
                queue::Op::SendTo { fd, mem: sys::from(buf, 0), to: addr }
            }
            (Op::SendMany { addr, segment, .. }, Tag::Udp) => {
                return self.start_send_many(idx, fd, addr, segment);
            }
            _ => return Started::Done(IoResult::Err(IoError::WrongKind)),
        };
        self.push(idx, op)
    }

    pub(crate) fn push(&mut self, idx: u32, op: queue::Op) -> Started {
        match self.q.push(op, idx as u64) {
            Ok(key) => {
                self.flying(idx, key);
                Started::Flying
            }
            Err(e) => refused(e),
        }
    }

    pub(crate) fn flying(&mut self, idx: u32, key: Key) {
        let Some(p) = self.ops[idx as usize].as_mut() else {
            return;
        };
        p.key = Some(key);
        p.state = State::Flying;
        self.queued = true;
        let (kind, on) = (p.kind, p.on);
        let (read, write) = side(&kind);
        if is_file(&kind) {
            self.file_flying += 1;
        }
        if let Ok(s) = self.slot_mut(on) {
            s.busy_read |= read;
            s.busy_write |= write;
            s.flying += 1;
        }
    }

    pub(crate) fn grounded(&mut self, idx: u32) -> (Op, Handle, State) {
        let Some(p) = self.ops[idx as usize].as_mut() else {
            return (Op::Close { handle: Handle::NONE }, Handle::NONE, State::Waiting);
        };
        let (kind, on, state) = (p.kind, p.on, p.state);
        p.key = None;
        p.state = State::Waiting;
        let (read, write) = side(&kind);
        if is_file(&kind) {
            self.file_flying = self.file_flying.saturating_sub(1);
        }
        if let Ok(s) = self.slot_mut(on) {
            s.busy_read &= !read;
            s.busy_write &= !write;
            s.flying = s.flying.saturating_sub(1);
        }
        (kind, on, state)
    }

    pub(crate) fn closable(&mut self, on: Handle) {
        let close = match self.slot(on) {
            Ok(s) if s.flying == 0 && s.closing != NONE => s.closing,
            _ => return,
        };
        self.close_now(on, close);
    }

    pub(crate) fn landed(&mut self, c: Cqe) {
        if c.user == BELL {
            self.bell_flying = false;
            return;
        }
        if c.user & HINT != 0 {
            return self.landed_hint(c.user as u32);
        }
        let idx = c.user as u32;
        let Some(p) =
            self.ops.get(idx as usize).and_then(Option::as_ref).filter(|p| p.key.is_some())
        else {
            return;
        };
        if let Op::RecvMany { .. } = p.kind {
            let on = p.on;
            return self.landed_many(idx, &c, on);
        }
        let (kind, on, state) = self.grounded(idx);
        if state == State::Cancelling {
            if matches!(kind, Op::Connect { .. }) {
                self.release(on);
            }
            if is_path(&kind) {
                self.path_cancelled(idx, &c);
            }
            self.finish(idx, IoResult::Cancelled);
            return self.closable(on);
        }
        if is_path(&kind) {
            self.landed_path(idx, &c);
            return self.closable(on);
        }
        match kind {
            Op::Read { handle, offset, at, .. } => self.landed_read(idx, &c, handle, offset, at),
            Op::Write { .. } => self.landed_write(idx, &c),
            Op::Connect { addr } => self.landed_connect(idx, &c, on, addr),
            Op::Accept { .. } => self.landed_accept(idx, &c),
            Op::Recv { .. } if self.slot(on).is_ok_and(|s| matches!(s.kind, Kind::Pipe)) => {
                self.landed_pipe(idx, &c, on)
            }
            Op::Recv { .. } => {
                let result = moved(&c);
                if let (IoResult::Ok(n), Some(Some(p))) = (result, self.ops.get_mut(idx as usize))
                    && let Some(b) = p.buf.as_mut()
                {
                    b.memory.set_len(n);
                }
                self.finish(idx, result);
            }
            _ => self.finish(idx, moved(&c)),
        }
        self.closable(on);
    }

    fn landed_read(&mut self, idx: u32, c: &Cqe, handle: Handle, offset: u64, at: u32) {
        let n = match c.result() {
            Ok(n) => n as usize,
            Err(e) => return self.finish(idx, IoResult::Err(errno(e))),
        };
        if let Some(b) = self.ops[idx as usize].as_mut().and_then(|p| p.buf.as_mut()) {
            b.memory.set_len(at as usize + n);
        }
        let hint = match self.slot_mut(handle) {
            Ok(Slot { kind: Kind::File { hygiene }, .. }) => hygiene.on_read(offset, n),
            _ => None,
        };
        if let Some(hint) = hint {
            self.hint(handle, hint);
        }
        self.finish(idx, IoResult::Ok(n));
    }

    fn landed_write(&mut self, idx: u32, c: &Cqe) {
        let n = match c.result() {
            Ok(0) => return self.finish(idx, IoResult::Err(errno(Errno::NOSPC))),
            Ok(n) => n as usize,
            Err(e) => return self.finish(idx, IoResult::Err(errno(e))),
        };
        let Some(p) = self.ops[idx as usize].as_mut() else {
            return;
        };
        let (from, len) = (p.progress, p.buf.as_ref().map_or(0, |b| b.memory.len()));
        p.progress = (from + n).min(len);
        let (total, kind) = (p.progress, p.kind);
        let Op::Write { handle, offset } = kind else {
            return;
        };
        let hints = match self.slot_mut(handle) {
            Ok(Slot { kind: Kind::File { hygiene }, .. }) => {
                hygiene.on_write(offset + from as u64, n)
            }
            _ => [None, None],
        };
        for hint in hints.into_iter().flatten() {
            self.hint(handle, hint);
        }
        if total == len {
            return self.finish(idx, IoResult::Ok(total));
        }
        self.restart(idx);
    }

    fn landed_connect(&mut self, idx: u32, c: &Cqe, on: Handle, addr: SocketAddr) {
        match c.result() {
            Ok(_) => {
                if let Ok(s) = self.slot_mut(on) {
                    s.kind = Kind::Tcp;
                }
                self.finish(idx, IoResult::Socket { handle: on, addr });
            }
            Err(e) => {
                self.release(on);
                self.finish(idx, IoResult::Err(errno(e)));
            }
        }
    }

    fn landed_accept(&mut self, idx: u32, c: &Cqe) {
        let result = match c.result() {
            Ok(fd) => {
                let fd = sys::adopt(fd as RawFd);
                let peer = c.addr.ok_or_else(|| io::Error::from(ErrorKind::NotConnected));
                self.socket(Kind::Tcp, fd, peer)
            }
            Err(e) => IoResult::Err(errno(e)),
        };
        self.finish(idx, result);
    }

    /// A pipe turned readable or its writer left. Polled first, since a read with no writer yet
    /// says end.
    fn landed_pipe(&mut self, idx: u32, c: &Cqe, on: Handle) {
        if let Err(e) = c.result() {
            return self.finish(idx, IoResult::Err(errno(e)));
        }
        // A close waits for its ops, so the slot is the one the op started on.
        let Some(pipe) = self.slots.get(on.slot() as usize).and_then(|s| s.fd.as_ref()) else {
            return;
        };
        let Some(buf) = self.ops[idx as usize].as_mut().and_then(|p| p.buf.as_mut()) else {
            return;
        };
        match rustix::io::read(pipe, buf.memory.as_mut_full()) {
            Ok(n) => {
                buf.memory.set_len(n);
                self.finish(idx, IoResult::Ok(n));
            }
            Err(Errno::AGAIN | Errno::INTR) => self.restart(idx),
            Err(e) => self.finish(idx, IoResult::Err(errno(e))),
        }
    }

    pub(crate) fn connect(&mut self, s: Submission, addr: SocketAddr) {
        let family = match addr {
            SocketAddr::V4(_) => rustix::net::AddressFamily::INET,
            SocketAddr::V6(_) => rustix::net::AddressFamily::INET6,
        };
        let flags = rustix::net::SocketFlags::NONBLOCK | rustix::net::SocketFlags::CLOEXEC;
        let fd =
            match rustix::net::socket_with(family, rustix::net::SocketType::STREAM, flags, None) {
                Ok(fd) => fd,
                Err(e) => return self.complete(s, IoResult::Err(errno(e))),
            };
        let raw = fd.as_raw_fd();
        let on = match self.insert(Kind::Connecting, fd) {
            Ok(on) => on,
            Err(e) => return self.complete(s, IoResult::Err(e)),
        };
        let Some(idx) = self.pend(s, on) else {
            return self.release(on);
        };
        if let Started::Done(result) =
            self.push(idx, queue::Op::Connect { fd: Fd::Raw(raw), to: addr })
        {
            self.release(on);
            self.finish(idx, result);
        } else if self.ops[idx as usize].as_ref().is_some_and(|p| p.state == State::Waiting) {
            self.release(on);
            self.finish(idx, IoResult::Err(IoError::Full));
        }
    }

    // Cold path, socket setup.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn setup_op(&mut self, s: Submission) {
        let result = match s.kind {
            Op::Bind { addr } => {
                let sock = crate::take_parked(addr).map_or_else(|| UdpSocket::bind(addr), Ok);
                match sock.and_then(|u| u.set_nonblocking(true).map(|()| u)) {
                    Ok(u) => {
                        let local = u.local_addr();
                        self.socket(Kind::Udp, OwnedFd::from(u), local)
                    }
                    Err(e) => IoResult::Err(io_error(&e)),
                }
            }
            Op::Listen { addr } => {
                match TcpListener::bind(addr).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
                    Ok(l) => {
                        let local = l.local_addr();
                        self.socket(Kind::Listener, OwnedFd::from(l), local)
                    }
                    Err(e) => IoResult::Err(io_error(&e)),
                }
            }
            Op::Peer { handle, addr } => match self.slot(handle) {
                Ok(Slot { kind: Kind::Udp, fd: Some(fd), .. }) => {
                    match rustix::net::connect(fd.as_fd(), &addr) {
                        Ok(()) => IoResult::Ok(0),
                        Err(e) => IoResult::Err(errno(e)),
                    }
                }
                Ok(_) => IoResult::Err(IoError::WrongKind),
                Err(e) => IoResult::Err(e),
            },
            _ => IoResult::Err(IoError::Unsupported),
        };
        self.complete(s, result);
    }

    /// Close `handle`. Its ops complete as cancelled first, those on the queue once the kernel lets
    /// go.
    pub(crate) fn close(&mut self, s: Submission, handle: Handle) {
        match self.slot(handle) {
            Err(e) => return self.complete(s, IoResult::Err(e)),
            Ok(slot) if slot.closing != NONE => {
                return self.complete(s, IoResult::Err(IoError::BadHandle));
            }
            Ok(_) => {}
        }
        for i in 0..self.ops.len() as u32 {
            if self.ops[i as usize]
                .as_ref()
                .is_some_and(|p| p.on == handle && !matches!(p.kind, Op::Close { .. }))
            {
                self.cancel_at(i);
            }
        }
        let Some(idx) = self.pend(s, Handle::NONE) else {
            return;
        };
        match self.slot_mut(handle) {
            Ok(slot) if slot.flying > 0 => slot.closing = idx,
            _ => self.close_now(handle, idx),
        }
    }

    pub(crate) fn close_now(&mut self, handle: Handle, close: u32) {
        self.release(handle);
        self.finish(close, IoResult::Ok(0));
    }

    pub(crate) fn cancel(&mut self, target: OpId) {
        let found = self.ops.iter().position(|p| {
            p.as_ref().is_some_and(|p| p.op == target && !matches!(p.kind, Op::Close { .. }))
        });
        if let Some(i) = found {
            return self.cancel_at(i as u32);
        }
        if let Some(t) = self.take_timer(target) {
            let c = profluens_core::io::Completion {
                op: t.op,
                user: t.user,
                result: IoResult::Cancelled,
                buf: None,
            };
            return self.done.push((t.element, c));
        }
        if let Some(done) = self.resolver.cancel(target) {
            self.done.push(done);
        }
    }

    fn cancel_at(&mut self, idx: u32) {
        let Some(p) = self.ops[idx as usize].as_mut() else {
            return;
        };
        match (p.state, p.key) {
            (State::Flying, Some(key)) => {
                p.state = State::Cancelling;
                self.q.cancel(key);
                (self.queued, self.urgent) = (true, true);
            }
            (State::Waiting, _) => {
                let (on, many) = (p.on, matches!(p.kind, Op::RecvMany { .. }));
                self.waiting.retain(|w| *w != idx);
                if many {
                    self.unarm(idx, on);
                }
                self.finish(idx, IoResult::Cancelled);
            }
            (State::Lent, _) => self.unlend(idx),
            (State::Cancelling, _) if matches!(p.kind, Op::RecvMany { .. }) => {
                let on = p.on;
                self.undry(on);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    File,
    Pipe,
    Stream,
    Udp,
    Listener,
    Other,
}

fn tag(kind: &Kind) -> Tag {
    match kind {
        Kind::File { .. } => Tag::File,
        Kind::Pipe => Tag::Pipe,
        Kind::Tcp => Tag::Stream,
        Kind::Udp => Tag::Udp,
        Kind::Listener => Tag::Listener,
        Kind::Connecting | Kind::Free => Tag::Other,
    }
}
