//! Queue backend behavior beyond the contract basics, on both backends.
//! The including file gives `make()`, `factory()` and `NAME`.

use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::id::{ElementId, FormatId};
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Name, Op, OpId, OpenMode, Reactor, Submission,
};
use profluens_core::memory::Pool;
use profluens_core::time::Timestamp;

const PATIENCE: Duration = Duration::from_secs(10);

struct Rig {
    r: Box<dyn Reactor>,
    pool: Pool,
    next: u64,
    seen: Vec<Completion>,
}

fn rig(slot: usize) -> Rig {
    Rig { r: super::make(), pool: Pool::new(slot), next: 0, seen: Vec::new() }
}

impl Rig {
    fn buf(&self, bytes: &[u8]) -> Buffer {
        let mut memory = self.pool.acquire();
        memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
        memory.set_len(bytes.len());
        Buffer {
            memory,
            pts: Timestamp::NONE,
            dts: Timestamp::NONE,
            duration: Timestamp::NONE,
            flags: BufferFlags::empty(),
            format: FormatId(0),
            sync: None,
        }
    }

    fn submit_named(&mut self, kind: Op, buf: Option<Buffer>, names: &[u8]) -> OpId {
        let op = OpId(self.next);
        self.next += 1;
        let mut subs = vec![Submission { op, element: ElementId(1), kind, buf, user: op.0 }];
        self.r.submit(&mut subs, names);
        op
    }

    fn submit(&mut self, kind: Op, buf: Option<Buffer>) -> OpId {
        self.submit_named(kind, buf, &[])
    }

    fn turn(&mut self) {
        let mut out = Vec::new();
        self.r.run_once(&mut out);
        self.seen.extend(out.into_iter().map(|(_, c)| c));
    }

    fn wait(&mut self, op: OpId) -> Completion {
        let start = Instant::now();
        loop {
            self.turn();
            if let Some(i) = self.seen.iter().position(|c| c.op == op) {
                return self.seen.remove(i);
            }
            assert!(start.elapsed() < PATIENCE, "{op:?} never completed");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn settle(&mut self, for_how_long: Duration) {
        let start = Instant::now();
        while start.elapsed() < for_how_long {
            self.turn();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn open(&mut self, path: &std::path::Path, mode: OpenMode) -> Handle {
        let bytes = path.as_os_str().as_encoded_bytes();
        let name = Name { off: 0, len: bytes.len() as u32 };
        let op = self.submit_named(Op::Open { path: name, mode }, None, bytes);
        match self.wait(op).result {
            IoResult::File { handle, .. } => handle,
            other => panic!("open gave {other:?}"),
        }
    }

    fn count(&self, op: OpId) -> usize {
        self.seen.iter().filter(|c| c.op == op).count()
    }
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pf-reactor-queue-{}-{}",
        super::NAME,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name)
}

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

#[test]
fn the_factory_is_a_typed_result() {
    let made = (super::factory())();
    assert!(made.is_ok(), "this kernel has the backend, got {:?}", made.err());
    let either = (pf_reactor::factory())();
    assert!(either.is_ok());
}

#[test]
fn a_cancelled_file_read_completes_once_when_it_lands() {
    let mut r = rig(1 << 20);
    let path = tmp("cancel-read");
    let payload: Vec<u8> = (0..(1 << 20)).map(|i| (i % 253) as u8).collect();
    std::fs::write(&path, &payload).expect("fixture");
    let h = r.open(&path, OpenMode::Read);
    let reads: Vec<OpId> = (0..4)
        .map(|i| {
            r.submit(
                Op::Read { handle: h, offset: i * (1 << 20), at: 0, len: u32::MAX },
                Some(r.buf(b"")),
            )
        })
        .collect();
    for op in &reads {
        r.submit(Op::Cancel { target: *op }, None);
    }
    for op in &reads {
        let c = r.wait(*op);
        assert_eq!(c.result, IoResult::Cancelled);
        assert!(c.buf.is_some(), "the buffer rides back with the cancel");
    }
    r.settle(Duration::from_millis(100));
    for op in &reads {
        assert_eq!(r.count(*op), 0, "{op:?} completed again");
    }
    let read = r.submit(Op::Read { handle: h, offset: 0, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    let got = r.wait(read);
    assert_eq!(got.result, IoResult::Ok(1 << 20));
    assert_eq!(got.buf.expect("buffer").memory.data(), &payload[..]);
}

#[test]
fn a_file_close_follows_its_cancels_and_waits_for_the_reads() {
    let mut r = rig(256 << 10);
    let path = tmp("close-read");
    std::fs::write(&path, vec![7u8; 2 << 20]).expect("fixture");
    let h = r.open(&path, OpenMode::Read);
    let reads: Vec<OpId> = (0..6)
        .map(|i| {
            r.submit(
                Op::Read { handle: h, offset: i * (256 << 10), at: 0, len: u32::MAX },
                Some(r.buf(b"")),
            )
        })
        .collect();
    let close = r.submit(Op::Close { handle: h }, None);
    r.wait(close);
    let order: Vec<OpId> = r.seen.iter().map(|c| c.op).chain([close]).collect();
    let close_at = order.iter().position(|op| *op == close).expect("the close");
    for op in &reads {
        let at = order.iter().position(|o| o == op).expect("cancelled");
        assert!(at < close_at, "{op:?} completed after the close");
        assert_eq!(
            r.seen.iter().find(|c| c.op == *op).map(|c| c.result),
            Some(IoResult::Cancelled)
        );
    }
    r.settle(Duration::from_millis(50));
    for op in &reads {
        assert_eq!(r.count(*op), 1);
    }
    let late = r.submit(Op::Read { handle: h, offset: 0, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    assert_eq!(r.wait(late).result, IoResult::Err(IoError::BadHandle));
    let again = r.submit(Op::Close { handle: h }, None);
    assert_eq!(r.wait(again).result, IoResult::Err(IoError::BadHandle));
}

#[test]
fn a_long_buffer_goes_whole() {
    const LEN: usize = 9 << 20;
    let mut r = rig(LEN);
    let path = tmp("parts");
    let payload: Vec<u8> = (0..LEN).map(|i| (i * 7 % 251) as u8).collect();
    let w = r.open(&path, OpenMode::Create);
    let write = r.submit(Op::Write { handle: w, offset: 0 }, Some(r.buf(&payload)));
    assert_eq!(r.wait(write).result, IoResult::Ok(LEN));
    let close = r.submit(Op::Close { handle: w }, None);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    assert_eq!(std::fs::read(&path).expect("read back"), payload);

    let h = r.open(&path, OpenMode::Read);
    let read = r.submit(Op::Read { handle: h, offset: 0, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    let got = r.wait(read);
    assert_eq!(got.result, IoResult::Ok(LEN));
    assert_eq!(got.buf.expect("buffer").memory.data(), &payload[..]);
    let tail = r.submit(
        Op::Read { handle: h, offset: LEN as u64 - 100, at: 0, len: u32::MAX },
        Some(r.buf(b"")),
    );
    let got = r.wait(tail);
    assert_eq!(got.result, IoResult::Ok(100));
    assert_eq!(got.buf.expect("buffer").memory.data(), &payload[LEN - 100..]);
}

#[test]
fn a_cancelled_recv_completes_once_and_a_cancel_of_nothing_is_nothing() {
    let listener = TcpListener::bind(loopback()).expect("bind");
    let mut r = rig(64);
    let connect = r.submit(Op::Connect { addr: listener.local_addr().expect("addr") }, None);
    let IoResult::Socket { handle: client, .. } = r.wait(connect).result else { panic!("connect") };
    let _server = listener.accept().expect("accept");
    let recv = r.submit(Op::Recv { handle: client }, Some(r.buf(b"")));
    r.settle(Duration::from_millis(20));
    assert_eq!(r.count(recv), 0, "nothing was sent");
    r.submit(Op::Cancel { target: recv }, None);
    assert_eq!(r.wait(recv).result, IoResult::Cancelled);
    r.submit(Op::Cancel { target: recv }, None);
    r.submit(Op::Cancel { target: OpId(77_777) }, None);
    r.settle(Duration::from_millis(50));
    assert_eq!(r.count(recv), 0, "a second cancel made a second completion");
    assert!(
        r.seen.is_empty(),
        "something completed that was never submitted, {:?}",
        r.seen.iter().map(|c| c.op).collect::<Vec<_>>()
    );
    let send = r.submit(Op::Send { handle: client, skip: 0 }, Some(r.buf(b"still here")));
    assert_eq!(r.wait(send).result, IoResult::Ok(10));
}

#[test]
fn timers_fire_in_order_and_cancelled_ones_leave_no_deadline() {
    let mut r = rig(64);
    let ms = |n| Op::Timer { after: Duration::from_millis(n) };
    let late = r.submit(ms(40), None);
    let soon = r.submit(ms(10), None);
    let mid = r.submit(ms(25), None);
    let gone = r.submit(ms(15), None);
    r.submit(Op::Cancel { target: gone }, None);
    assert!(r.r.due_in().expect("a deadline") <= Duration::from_millis(10));
    let order: Vec<OpId> = [gone, soon, mid, late].iter().map(|op| r.wait(*op).op).collect();
    assert_eq!(order, [gone, soon, mid, late]);
    assert!(r.r.due_in().is_none());
    let one = r.submit(ms(30_000), None);
    r.submit(Op::Cancel { target: one }, None);
    assert_eq!(r.wait(one).result, IoResult::Cancelled);
    assert!(r.r.due_in().is_none(), "a cancelled timer left its deadline");
}

#[test]
fn reads_past_the_queue_room_wait_and_complete() {
    const LEN: usize = 4096;
    let made = pf_reactor::OsReactor::sized(super::WANT, 1).expect("the backend");
    let mut r = Rig { r: Box::new(made), pool: Pool::new(LEN), next: 0, seen: Vec::new() };
    let n = 4 * pf_reactor::MIN_OPS;
    let path = tmp("room");
    let payload: Vec<u8> = (0..n * LEN).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &payload).expect("fixture");
    let h = r.open(&path, OpenMode::Read);
    let reads: Vec<OpId> = (0..n)
        .map(|i| {
            r.submit(
                Op::Read { handle: h, offset: (i * LEN) as u64, at: 0, len: u32::MAX },
                Some(r.buf(b"")),
            )
        })
        .collect();
    for (i, op) in reads.iter().enumerate() {
        let got = r.wait(*op);
        assert_eq!(got.result, IoResult::Ok(LEN));
        assert_eq!(got.buf.expect("buffer").memory.data(), &payload[i * LEN..(i + 1) * LEN]);
    }
}

fn os_rig(slot: usize) -> (pf_reactor::OsReactor, Pool) {
    (pf_reactor::OsReactor::on(super::WANT).expect("the backend"), Pool::new(slot))
}

fn bare(pool: &Pool) -> Buffer {
    let mut memory = pool.acquire();
    memory.set_len(0);
    Buffer {
        memory,
        pts: Timestamp::NONE,
        dts: Timestamp::NONE,
        duration: Timestamp::NONE,
        flags: BufferFlags::empty(),
        format: FormatId(0),
        sync: None,
    }
}

fn one(r: &mut pf_reactor::OsReactor, op: u64, kind: Op, buf: Option<Buffer>) {
    let mut subs = vec![Submission { op: OpId(op), element: ElementId(1), kind, buf, user: op }];
    r.submit(&mut subs, &[]);
}

fn bound(r: &mut pf_reactor::OsReactor, op: u64) -> (Handle, SocketAddr) {
    one(r, op, Op::Bind { addr: loopback() }, None);
    let mut out = Vec::new();
    r.run_once(&mut out);
    match out.first().map(|(_, c)| c.result) {
        Some(IoResult::Socket { handle, addr }) => (handle, addr),
        other => panic!("bind gave {other:?}"),
    }
}

#[test]
fn what_a_pass_pushed_rides_the_park_and_ends_it() {
    let (mut r, pool) = os_rig(2048);
    let (sock, addr) = bound(&mut r, 1);
    let before = r.enters();
    one(&mut r, 2, Op::Recv { handle: sock }, Some(bare(&pool)));
    let mut out = Vec::new();
    r.run_once(&mut out);
    assert!(out.is_empty());
    assert_eq!(r.enters(), before, "a pass that delivered nothing does not enter");
    let sender = UdpSocket::bind(loopback()).expect("bind");
    let late = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        sender.send_to(b"hi", addr).expect("send");
    });
    let start = Instant::now();
    while out.is_empty() {
        assert!(start.elapsed() < PATIENCE, "the receive never reached the kernel");
        r.park(Duration::from_secs(1));
        r.run_once(&mut out);
    }
    late.join().expect("sender");
    assert_eq!(out[0].1.result, IoResult::Ok(2));
    let spent = r.enters();
    assert_eq!(spent.submits, before.submits, "the park carried the push");
    assert!(spent.waits > before.waits);
}

#[test]
fn a_group_that_never_parks_has_its_pushes_entered_within_hold_passes() {
    let (mut r, pool) = os_rig(4096);
    let path = tmp("hold");
    std::fs::write(&path, vec![5u8; 4096]).expect("fixture");
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut subs = vec![Submission {
        op: OpId(1),
        element: ElementId(1),
        kind: Op::Open { path: Name { off: 0, len: bytes.len() as u32 }, mode: OpenMode::Read },
        buf: None,
        user: 1,
    }];
    r.submit(&mut subs, bytes);
    let mut out = Vec::new();
    let start = Instant::now();
    while out.is_empty() {
        assert!(start.elapsed() < PATIENCE, "the open never completed");
        r.run_once(&mut out);
    }
    let Some(IoResult::File { handle, .. }) = out.first().map(|(_, c)| c.result) else {
        panic!("open gave {out:?}", out = out.len())
    };
    let before = r.enters().submits;
    one(&mut r, 2, Op::Read { handle, offset: 0, at: 0, len: u32::MAX }, Some(bare(&pool)));
    let mut passes = 0;
    out.clear();
    while r.enters().submits == before && out.is_empty() {
        passes += 1;
        assert!(passes <= pf_reactor::HOLD as usize + 1, "a read waited past the hold");
        r.run_once(&mut out);
    }
    assert!(r.enters().submits - before <= 1, "one enter for the pass that handed it over");
    // entered, a read lands once the kernel's worker is done with it
    let start = Instant::now();
    while out.is_empty() {
        assert!(start.elapsed() < PATIENCE, "the read never completed");
        r.run_once(&mut out);
    }
    assert_eq!(out[0].1.result, IoResult::Ok(4096));
}

#[test]
fn a_paced_receive_enters_the_kernel_once_a_datagram() {
    const N: u64 = 50;
    let (mut r, pool) = os_rig(2048);
    let (sock, addr) = bound(&mut r, 1);
    let start = r.enters();
    let sender = UdpSocket::bind(loopback()).expect("bind");
    let feeder = std::thread::spawn(move || {
        for _ in 0..N {
            std::thread::sleep(Duration::from_millis(2));
            sender.send_to(&[1; 64], addr).expect("send");
        }
    });
    let (mut got, mut next, mut out) = (0, 10, Vec::new());
    one(&mut r, next, Op::Recv { handle: sock }, Some(bare(&pool)));
    let t = Instant::now();
    while got < N {
        assert!(t.elapsed() < PATIENCE, "{got} of {N} datagrams");
        r.run_once(&mut out);
        if out.is_empty() {
            r.park(Duration::from_secs(1));
            continue;
        }
        for (_, c) in out.drain(..) {
            assert_eq!(c.result, IoResult::Ok(64));
            got += 1;
            next += 1;
            one(&mut r, next, Op::Recv { handle: sock }, c.buf);
        }
    }
    feeder.join().expect("feeder");
    let spent = r.enters();
    let enters = spent.submits + spent.waits - start.submits - start.waits;
    assert!(enters <= N + 2, "{enters} enters for {N} datagrams, {spent:?}");
}

#[test]
fn many_timers_fire_in_deadline_order_and_each_completes_once() {
    const N: u64 = 600;
    let mut r = rig(64);
    let after = |i: u64| Duration::from_millis((i * 7919) % 41);
    let start = Instant::now();
    let ops: Vec<OpId> = (0..N).map(|i| r.submit(Op::Timer { after: after(i) }, None)).collect();
    let cancelled = |i: u64| i % 3 == 1;
    for i in (0..N).filter(|i| cancelled(*i)) {
        r.submit(Op::Cancel { target: ops[i as usize] }, None);
        if i % 2 == 0 {
            r.submit(Op::Cancel { target: ops[i as usize] }, None);
        }
    }
    let mut fired = Vec::new();
    while r.seen.len() < N as usize {
        assert!(start.elapsed() < PATIENCE, "{} of {N} timers", r.seen.len());
        let before = r.seen.len();
        r.turn();
        for c in &r.seen[before..] {
            let i = c.op.0;
            if cancelled(i) {
                assert_eq!(c.result, IoResult::Cancelled, "timer {i}");
            } else {
                assert_eq!(c.result, IoResult::Ok(0), "timer {i}");
                assert!(start.elapsed() >= after(i), "timer {i} fired early");
                fired.push(i);
            }
        }
        for &i in fired.iter().rev().take(2) {
            r.submit(Op::Cancel { target: ops[i as usize] }, None);
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    r.settle(Duration::from_millis(20));
    for op in &ops {
        assert_eq!(r.count(*op), 1, "{op:?} completed {} times", r.count(*op));
    }
    let key = |i: &u64| (after(*i), *i);
    assert!(fired.windows(2).all(|w| key(&w[0]) <= key(&w[1])), "out of order: {fired:?}");
    assert!(r.r.due_in().is_none());
}

#[test]
fn a_paced_lent_receive_enters_the_kernel_once_a_datagram() {
    const N: u64 = 50;
    let (mut r, pool) = os_rig(2048);
    let (sock, addr) = bound(&mut r, 1);
    for op in 10..18 {
        one(&mut r, op, Op::Lend { handle: sock }, Some(bare(&pool)));
    }
    one(&mut r, 2, Op::RecvMany { handle: sock }, None);
    let start = r.enters();
    let sender = UdpSocket::bind(loopback()).expect("bind");
    let feeder = std::thread::spawn(move || {
        for _ in 0..N {
            std::thread::sleep(Duration::from_millis(2));
            sender.send_to(&[1; 64], addr).expect("send");
        }
    });
    let (mut got, mut next, mut out) = (0, 100, Vec::new());
    let t = Instant::now();
    while got < N {
        assert!(t.elapsed() < PATIENCE, "{got} of {N} datagrams");
        r.run_once(&mut out);
        if out.is_empty() {
            r.park(Duration::from_secs(1));
            continue;
        }
        for (_, c) in out.drain(..) {
            assert!(matches!(c.result, IoResult::Datagram { len: 64, .. }), "{:?}", c.result);
            got += 1;
            next += 1;
            one(&mut r, next, Op::Lend { handle: sock }, c.buf);
        }
        for _ in 0..4 {
            r.run_once(&mut out);
            assert!(out.is_empty() || got + out.len() as u64 <= N);
        }
    }
    feeder.join().expect("feeder");
    let spent = r.enters();
    let enters = spent.submits + spent.waits - start.submits - start.waits;
    assert!(spent.submits - start.submits <= 2, "{spent:?}");
    assert!(enters <= N + 2, "{enters} enters for {N} datagrams, {spent:?}");
}
