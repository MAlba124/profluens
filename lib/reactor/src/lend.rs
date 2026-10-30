//! Lent buffers and the multishot receive over a per-socket provided buffer ring.
//! A lent buffer is an op that completes once when its buffer comes back, filled or cancelled.

use profluens_core::io::{COALESCE_ROOM, Handle, IoError, IoResult, LEND_HEAD, Op, Submission};
use rustix::io::Errno;
use stein_io::queue::{self, ADDR_LEN, Cqe, Fd, Group, RingPage};

use crate::ops::Started;
use crate::{Kind, NONE, OsReactor, State, errno, sys};

pub(crate) const LENT: usize = 256;
const CONTROL: u32 = 64;
pub(crate) const HEAD: usize = 16 + ADDR_LEN + CONTROL as usize;
const _: () = assert!(HEAD <= LEND_HEAD);
/// `SOL_UDP` options, absent from libc.
const UDP_SEGMENT: i32 = 103;
const UDP_GRO: i32 = 104;

pub(crate) struct Lent {
    page: Box<RingPage>,
    bids: Box<[u32; LENT]>,
    free: Vec<u16>,
    held: u16,
    group: Option<Group>,
    recv: u32,
    drying: bool,
    gro: bool,
    small: bool,
}

impl Lent {
    // Once per socket that lends.
    #[allow(clippy::disallowed_methods)]
    fn new() -> Box<Lent> {
        Box::new(Lent {
            page: Box::new(RingPage::ZERO),
            bids: Box::new([NONE; LENT]),
            free: (0..LENT as u16).rev().collect(),
            held: 0,
            group: None,
            recv: NONE,
            drying: false,
            gro: false,
            small: false,
        })
    }

    pub(crate) fn reset(&mut self) {
        (self.gro, self.small, self.recv, self.drying) = (false, false, NONE, false);
    }
}

impl OsReactor {
    pub(crate) fn lend(&mut self, s: Submission) {
        let Op::Lend { handle } = s.kind else { return };
        match self.slot(handle) {
            // A closing slot is released once its flying ops land, a new lend would outlive it.
            Ok(slot) if slot.closing != NONE => {
                return self.complete(s, IoResult::Err(IoError::BadHandle));
            }
            Ok(slot) if matches!(slot.kind, Kind::Udp | Kind::Tcp) => {}
            Ok(_) => return self.complete(s, IoResult::Err(IoError::WrongKind)),
            Err(e) => return self.complete(s, IoResult::Err(e)),
        }
        let mut s = s;
        // A lent buffer is filled from the start of its slot, whatever view it was.
        if let Some(buf) = s.buf.as_mut() {
            buf.memory.widen();
        }
        let room = s.buf.as_ref().map_or(0, |b| b.memory.capacity());
        if room <= HEAD {
            return self.complete(s, IoResult::Err(IoError::Os(Errno::NOBUFS.raw_os_error())));
        }
        let Some(idx) = self.pend(s, handle) else {
            return;
        };
        let Ok(slot) = self.slot_mut(handle) else {
            return;
        };
        let fd = slot.raw();
        let lent = slot.lent.get_or_insert_with(Lent::new);
        let Some(bid) = lent.free.pop() else {
            return self.finish(idx, IoResult::Err(IoError::Full));
        };
        lent.bids[bid as usize] = idx;
        lent.held += 1;
        let group = lent.group;
        if room < COALESCE_ROOM {
            lent.small = true;
            if lent.gro {
                lent.gro = false;
                sys::set_sockopt(fd, libc::SOL_UDP, UDP_GRO, 0);
            }
        }
        let Some(p) = self.ops[idx as usize].as_mut() else {
            return;
        };
        (p.state, p.bid) = (State::Lent, bid);
        if let (Some(group), Some(buf)) = (group, p.buf.as_mut()) {
            // The ring has room for every id, so an offer is never refused.
            let _ = self.q.provide(group, bid, sys::into(buf));
        }
    }

    pub(crate) fn start_many(&mut self, idx: u32, on: Handle) -> Started {
        let Ok(slot) = self.slot(on) else {
            return Started::Done(IoResult::Err(IoError::BadHandle));
        };
        let (udp, fd) = (matches!(slot.kind, Kind::Udp), slot.raw());
        let Some(lent) = self.slots[on.slot() as usize].lent.as_mut().filter(|l| l.held > 0) else {
            return Started::Done(IoResult::Err(IoError::Dry));
        };
        let group = match lent.group {
            Some(group) => group,
            None => {
                lent.page.0.fill(0);
                let group = match sys::add_group(&mut self.q, &mut lent.page, LENT as u16) {
                    Ok(group) => group,
                    Err(_) => return Started::Done(IoResult::Err(IoError::Full)),
                };
                lent.group = Some(group);
                let gro = udp && !lent.small;
                if gro != lent.gro {
                    lent.gro = gro;
                    sys::set_sockopt(fd, libc::SOL_UDP, UDP_GRO, i32::from(gro));
                }
                for bid in 0..LENT {
                    let at = lent.bids[bid];
                    if let Some(buf) = self
                        .ops
                        .get_mut(at as usize)
                        .and_then(Option::as_mut)
                        .and_then(|p| p.buf.as_mut())
                    {
                        let _ = self.q.provide(group, bid as u16, sys::into(buf));
                    }
                }
                group
            }
        };
        (lent.recv, lent.drying) = (idx, false);
        let op = match udp {
            true => {
                queue::Op::RecvMsgMulti { fd: Fd::Raw(fd), group, name: true, control: CONTROL }
            }
            false => queue::Op::RecvMulti { fd: Fd::Raw(fd), group },
        };
        self.push(idx, op)
    }

    pub(crate) fn landed_many(&mut self, idx: u32, c: &Cqe, on: Handle) {
        let filled = c.buffer();
        if let Some(bid) = filled {
            self.filled(on, bid, c);
        }
        if c.more() {
            return;
        }
        let (_, _, state) = self.grounded(idx);
        let lent = self.slot(on).ok().and_then(|s| s.lent.as_ref());
        let (held, drying) = lent.map_or((0, false), |l| (l.held, l.drying));
        let result = match (state, c.result()) {
            (State::Cancelling, _) if drying && held > 0 => return self.restart(idx),
            (State::Cancelling, _) if drying => IoResult::Err(IoError::Dry),
            (State::Cancelling, _) | (_, Err(Errno::CANCELED)) => IoResult::Cancelled,
            // The ring ran dry, or the kernel ended the receive after a buffer. What
            // was lent since goes on being received into.
            (_, Err(Errno::NOBUFS)) | (_, Ok(_)) if held > 0 && (filled.is_some() || c.res < 0) => {
                return self.restart(idx);
            }
            (_, Err(Errno::NOBUFS)) => IoResult::Err(IoError::Dry),
            (_, Ok(_)) if filled.is_some() => IoResult::Err(IoError::Dry),
            (_, Ok(_)) => IoResult::Ok(0),
            (_, Err(e)) => IoResult::Err(errno(e)),
        };
        self.disarm(on);
        self.finish(idx, result);
        self.closable(on);
    }

    fn filled(&mut self, on: Handle, bid: u16, c: &Cqe) {
        let Ok(slot) = self.slot_mut(on) else { return };
        let udp = matches!(slot.kind, Kind::Udp);
        let Some(lent) = slot.lent.as_mut() else {
            return;
        };
        let at = std::mem::replace(&mut lent.bids[bid as usize], NONE);
        if at == NONE {
            return;
        }
        lent.free.push(bid);
        lent.held -= 1;
        if lent.held == 0 {
            self.dry(on);
        }
        let Some(buf) =
            self.ops.get_mut(at as usize).and_then(Option::as_mut).and_then(|p| p.buf.as_mut())
        else {
            return;
        };
        let n = c.res.max(0) as usize;
        buf.memory.set_len(n);
        let result = match udp {
            true => {
                let (result, payload) = datagram(buf.memory.data());
                match buf.memory.slice(HEAD, payload) {
                    Some(view) => buf.memory = view,
                    None => buf.memory.set_len(0),
                }
                result
            }
            false => IoResult::Ok(n),
        };
        self.finish(at, result);
    }

    /// Nothing is lent to `on`. Cancel its receive to end dry, as the ring would and epoll would
    /// not.
    fn dry(&mut self, on: Handle) {
        let Some(lent) = self.slots[on.slot() as usize].lent.as_mut() else {
            return;
        };
        let Some(p) = self.ops.get_mut(lent.recv as usize).and_then(Option::as_mut) else {
            return;
        };
        if let (State::Flying, Some(key)) = (p.state, p.key) {
            p.state = State::Cancelling;
            lent.drying = true;
            self.q.cancel(key);
            (self.queued, self.urgent) = (true, true);
        }
    }

    pub(crate) fn undry(&mut self, on: Handle) {
        if let Some(lent) = self.slots.get_mut(on.slot() as usize).and_then(|s| s.lent.as_mut()) {
            lent.drying = false;
        }
    }

    pub(crate) fn disarm(&mut self, on: Handle) {
        let Ok(slot) = self.slot_mut(on) else { return };
        let Some(lent) = slot.lent.as_mut() else {
            return;
        };
        let group = lent.group.take();
        (lent.recv, lent.drying) = (NONE, false);
        let mut back = [NONE; LENT];
        for (bid, at) in lent.bids.iter_mut().enumerate() {
            if *at != NONE {
                back[bid] = std::mem::replace(at, NONE);
                lent.free.push(bid as u16);
            }
        }
        lent.held = 0;
        if let Some(group) = group {
            let _ = self.q.remove_group(group);
        }
        for at in back.into_iter().filter(|at| *at != NONE) {
            self.finish(at, IoResult::Cancelled);
        }
    }

    /// A cancelled lend comes back now unless the ring holds it, which returns it at receive end.
    pub(crate) fn unlend(&mut self, idx: u32) {
        let Some(p) = self.ops[idx as usize].as_ref() else {
            return;
        };
        let (on, bid) = (p.on, p.bid);
        let Ok(slot) = self.slot_mut(on) else {
            return self.finish(idx, IoResult::Cancelled);
        };
        let Some(lent) = slot.lent.as_mut() else {
            return;
        };
        if lent.group.is_some() || lent.bids[bid as usize] != idx {
            return;
        }
        lent.bids[bid as usize] = NONE;
        lent.free.push(bid);
        lent.held -= 1;
        self.finish(idx, IoResult::Cancelled);
    }

    pub(crate) fn start_send_many(
        &mut self,
        idx: u32,
        fd: Fd,
        addr: Option<std::net::SocketAddr>,
        segment: u16,
    ) -> Started {
        let Some(buf) = self.ops[idx as usize].as_mut().and_then(|p| p.buf.as_mut()) else {
            return Started::Done(IoResult::Err(IoError::Unsupported));
        };
        let mem = sys::from(buf, 0);
        if segment == 0 || mem.len() <= segment as usize {
            let op = match addr {
                Some(to) => queue::Op::SendTo { fd, mem, to },
                None => queue::Op::Send { fd, mem },
            };
            return self.push(idx, op);
        }
        let mut room = [0u8; stein_io::msg::CONTROL_ROOM];
        let size = segment.to_ne_bytes();
        let control =
            stein_io::msg::Control { level: libc::SOL_UDP, kind: UDP_SEGMENT, data: &size };
        let Some(n) = stein_io::msg::encode(&mut room, control) else {
            return Started::Done(IoResult::Err(IoError::Unsupported));
        };
        let op = queue::Op::SendMsg { fd, mem, control: queue::Mem::EMPTY, to: addr };
        match self.q.push_control(op, &room[..n], idx as u64) {
            Ok(key) => {
                self.flying(idx, key);
                Started::Flying
            }
            Err(e) => crate::ops::refused(e),
        }
    }
}

fn datagram(bytes: &[u8]) -> (IoResult, usize) {
    let word = |at: usize| {
        bytes.get(at..at + 4).map_or(0, |w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]]) as usize)
    };
    let payload = word(8).min(bytes.len().saturating_sub(HEAD));
    let name = &bytes[16.min(bytes.len())..(16 + word(0).min(ADDR_LEN)).min(bytes.len())];
    let unknown = std::net::SocketAddr::from(([0, 0, 0, 0], 0));
    let from = queue::addr_of(name).unwrap_or(unknown);
    let control =
        bytes.get(16 + ADDR_LEN..16 + ADDR_LEN + word(4).min(CONTROL as usize)).unwrap_or(&[]);
    let segment = match stein_io::msg::first(control) {
        Some(c) if c.level == libc::SOL_UDP && c.kind == UDP_GRO && c.data.len() >= 4 => {
            u16::try_from(i32::from_ne_bytes([c.data[0], c.data[1], c.data[2], c.data[3]]))
                .unwrap_or(0)
        }
        _ => 0,
    };
    (IoResult::Datagram { len: payload, from, segment }, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn laid_out(name: &[u8], control: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; HEAD + payload.len()];
        b[0..4].copy_from_slice(&(name.len() as u32).to_ne_bytes());
        b[4..8].copy_from_slice(&(control.len() as u32).to_ne_bytes());
        b[8..12].copy_from_slice(&(payload.len() as u32).to_ne_bytes());
        b[16..16 + name.len()].copy_from_slice(name);
        b[16 + ADDR_LEN..16 + ADDR_LEN + control.len()].copy_from_slice(control);
        b[HEAD..].copy_from_slice(payload);
        b
    }

    fn sockaddr_in(port: u16, ip: [u8; 4]) -> Vec<u8> {
        let mut v = vec![0u8; 16];
        v[0..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
        v[2..4].copy_from_slice(&port.to_be_bytes());
        v[4..8].copy_from_slice(&ip);
        v
    }

    #[test]
    fn a_receive_says_its_source_and_payload() {
        let b = laid_out(&sockaddr_in(5004, [10, 0, 0, 7]), &[], b"rtp!");
        let (r, n) = datagram(&b);
        assert_eq!(n, 4);
        assert_eq!(
            r,
            IoResult::Datagram { len: 4, from: "10.0.0.7:5004".parse().unwrap(), segment: 0 }
        );
    }

    #[test]
    fn a_coalesced_receive_says_its_segment() {
        let mut control = [0u8; 64];
        let size = 1200i32.to_ne_bytes();
        let c = stein_io::msg::Control { level: libc::SOL_UDP, kind: UDP_GRO, data: &size };
        let n = stein_io::msg::encode(&mut control, c).unwrap();
        let b = laid_out(&sockaddr_in(1, [127, 0, 0, 1]), &control[..n], &[7u8; 3000]);
        let (r, _) = datagram(&b);
        assert!(matches!(r, IoResult::Datagram { len: 3000, segment: 1200, .. }), "{r:?}");
    }

    #[test]
    fn a_header_that_claims_too_much_is_cut_to_the_buffer() {
        let mut b = laid_out(&[], &[], b"ab");
        b[8..12].copy_from_slice(&9999u32.to_ne_bytes());
        b[0..4].copy_from_slice(&9999u32.to_ne_bytes());
        let (r, n) = datagram(&b);
        assert_eq!(n, 2);
        assert!(matches!(r, IoResult::Datagram { len: 2, segment: 0, .. }));
    }
}
