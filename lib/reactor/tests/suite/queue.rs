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

fn rig_in_order(slot: usize) -> Rig {
    Rig { r: super::make_in_order(), pool: Pool::new(slot), next: 0, seen: Vec::new() }
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
        self.open_told(path, mode).0
    }

    /// The handle and whether the open said the file's reads go out in order.
    fn open_told(&mut self, path: &std::path::Path, mode: OpenMode) -> (Handle, bool) {
        let bytes = path.as_os_str().as_encoded_bytes();
        let name = Name { off: 0, len: bytes.len() as u32 };
        let op = self.submit_named(Op::Open { path: name, mode }, None, bytes);
        match self.wait(op).result {
            IoResult::File { handle, in_order, .. } => (handle, in_order),
            other => panic!("open gave {other:?}"),
        }
    }

    fn count(&self, op: OpId) -> usize {
        self.seen.iter().filter(|c| c.op == op).count()
    }

    /// The completions of `ops` in the order they came, once all of them did.
    fn all(&mut self, ops: &[OpId]) -> Vec<Completion> {
        let start = Instant::now();
        while ops.iter().any(|op| self.count(*op) == 0) {
            self.turn();
            assert!(start.elapsed() < PATIENCE, "not all of {ops:?} completed");
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut got = Vec::new();
        let mut i = 0;
        while i < self.seen.len() {
            if ops.contains(&self.seen[i].op) {
                got.push(self.seen.remove(i));
            } else {
                i += 1;
            }
        }
        got
    }

    fn read(&mut self, handle: Handle, offset: u64, len: u32) -> OpId {
        self.submit(Op::Read { handle, offset, at: 0, len }, Some(self.buf(b"")))
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

/// A reactor whose queue room is taken by receives that never land, so the next push waits.
fn crowded(pool: &Pool) -> pf_reactor::OsReactor {
    let mut r = pf_reactor::OsReactor::sized(super::WANT, pf_reactor::MIN_OPS).expect("backend");
    for i in 0..pf_reactor::MIN_OPS as u64 + 2 {
        let (h, _) = bound(&mut r, 1000 + 2 * i);
        one(&mut r, 1001 + 2 * i, Op::Recv { handle: h }, Some(bare(pool)));
    }
    r
}

fn settled(r: &mut pf_reactor::OsReactor, for_how_long: Duration) -> Vec<Completion> {
    let (mut seen, mut out, start) = (Vec::new(), Vec::new(), Instant::now());
    while start.elapsed() < for_how_long {
        r.run_once(&mut out);
        seen.extend(out.drain(..).map(|(_, c)| c));
        std::thread::sleep(Duration::from_millis(2));
    }
    seen
}

fn result_of(seen: &[Completion], op: u64) -> Option<IoResult> {
    seen.iter().find(|c| c.op == OpId(op)).map(|c| c.result)
}

#[test]
fn a_lent_buffer_comes_back_when_its_socket_closes_under_a_waiting_receive() {
    let pool = Pool::new(2048);
    let mut r = crowded(&pool);
    let (sock, _) = bound(&mut r, 1);
    one(&mut r, 2, Op::Lend { handle: sock }, Some(bare(&pool)));
    one(&mut r, 3, Op::RecvMany { handle: sock }, None);
    one(&mut r, 4, Op::Close { handle: sock }, None);
    let seen = settled(&mut r, Duration::from_millis(100));
    assert_eq!(result_of(&seen, 3), Some(IoResult::Cancelled));
    assert_eq!(result_of(&seen, 4), Some(IoResult::Ok(0)));
    assert_eq!(result_of(&seen, 2), Some(IoResult::Cancelled), "the lent buffer never came back");
    // the slot's next socket lends and receives with nothing left over
    let (next, _) = bound(&mut r, 5);
    assert_eq!(next.slot(), sock.slot());
    one(&mut r, 6, Op::Lend { handle: next }, Some(bare(&pool)));
    one(&mut r, 7, Op::Close { handle: next }, None);
    let seen = settled(&mut r, Duration::from_millis(50));
    assert_eq!(result_of(&seen, 6), Some(IoResult::Cancelled));
}

#[test]
fn a_lent_buffer_comes_back_on_cancel_once_its_waiting_receive_is_cancelled() {
    let pool = Pool::new(2048);
    let mut r = crowded(&pool);
    let (sock, _) = bound(&mut r, 1);
    one(&mut r, 2, Op::Lend { handle: sock }, Some(bare(&pool)));
    one(&mut r, 3, Op::RecvMany { handle: sock }, None);
    one(&mut r, 4, Op::Cancel { target: OpId(3) }, None);
    let seen = settled(&mut r, Duration::from_millis(50));
    assert_eq!(result_of(&seen, 3), Some(IoResult::Cancelled));
    assert_eq!(
        result_of(&seen, 2),
        Some(IoResult::Cancelled),
        "the receive's end returns the lends"
    );
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

/// The open says so where the reads are held in order, for an element to ask fewer of them. Only
/// the ring holds them, epoll reads a file as it is pushed, and a file written is not held.
#[test]
fn an_open_says_whether_the_reads_of_its_file_go_out_in_order() {
    let path = tmp("says-order");
    std::fs::write(&path, b"in order").expect("fixture");
    let mut r = rig_in_order(4096);
    assert_eq!(r.open_told(&path, OpenMode::Read).1, super::NAME == "uring");
    assert!(!r.open_told(&tmp("says-order-out"), OpenMode::Create).1);
    let mut r = rig(4096);
    r.r = super::make_free();
    assert!(!r.open_told(&path, OpenMode::Read).1, "reads left free");
}

#[test]
fn reads_of_a_file_in_order_complete_in_the_order_submitted() {
    const PART: usize = 64 << 10;
    let mut r = rig_in_order(PART);
    let path = tmp("in-order");
    let payload: Vec<u8> = (0..16 * PART).map(|i| (i * 13 % 251) as u8).collect();
    std::fs::write(&path, &payload).expect("fixture");
    let h = r.open(&path, OpenMode::Read);
    // scrambled, so file order and submission order differ
    let at: Vec<u64> = (0..16u64).map(|i| (i * 7 % 16) * PART as u64).collect();
    let reads: Vec<OpId> = at.iter().map(|o| r.read(h, *o, u32::MAX)).collect();
    let got = r.all(&reads);
    assert_eq!(got.iter().map(|c| c.op).collect::<Vec<_>>(), reads, "in submission order");
    for (c, o) in got.iter().zip(&at) {
        let o = *o as usize;
        assert_eq!(c.result, IoResult::Ok(PART));
        assert_eq!(c.buf.as_ref().expect("buffer").memory.data(), &payload[o..o + PART]);
    }
}

/// The kernel workers a thread's free reads left outlive a cap on their count, so order kept by
/// one worker alone broke on a thread that read before.
#[test]
fn reads_of_a_file_in_order_keep_it_on_a_thread_that_read_freely_before() {
    const PART: usize = 64 << 10;
    let path = tmp("in-order-after-free");
    let payload: Vec<u8> = (0..16 * PART).map(|i| (i * 11 % 251) as u8).collect();
    std::fs::write(&path, &payload).expect("fixture");
    let mut free = rig(PART);
    let h = free.open(&path, OpenMode::Read);
    for _ in 0..8 {
        let reads: Vec<OpId> =
            (0..16u64).map(|i| free.read(h, i * PART as u64, u32::MAX)).collect();
        free.all(&reads);
    }
    let mut r = rig_in_order(PART);
    let h = r.open(&path, OpenMode::Read);
    for round in 0..8 {
        let at: Vec<u64> = (0..16u64).map(|i| (i * 7 % 16) * PART as u64).collect();
        let reads: Vec<OpId> = at.iter().map(|o| r.read(h, *o, u32::MAX)).collect();
        let got = r.all(&reads);
        assert_eq!(got.iter().map(|c| c.op).collect::<Vec<_>>(), reads, "round {round}");
    }
}

#[test]
fn a_read_in_order_over_the_end_lets_the_reads_behind_it_go() {
    let mut r = rig_in_order(64 << 10);
    let path = tmp("in-order-end");
    let payload: Vec<u8> = (0..100_000).map(|i| (i % 249) as u8).collect();
    std::fs::write(&path, &payload).expect("fixture");
    let h = r.open(&path, OpenMode::Read);
    let reads = [r.read(h, 0, 65_536), r.read(h, 65_536, 65_536), r.read(h, 131_072, 65_536)];
    let again = r.read(h, 10, 10);
    let got = r.all(&[reads[0], reads[1], reads[2], again]);
    let results: Vec<IoResult> = got.iter().map(|c| c.result).collect();
    let short = 100_000 - 65_536;
    let want = [IoResult::Ok(65_536), IoResult::Ok(short), IoResult::Ok(0), IoResult::Ok(10)];
    assert_eq!(results, want);
    assert_eq!(got[1].buf.as_ref().expect("buffer").memory.data(), &payload[65_536..]);
    assert_eq!(got[3].buf.as_ref().expect("buffer").memory.data(), &payload[10..20]);
}

#[test]
fn reads_waiting_behind_one_in_order_cancel_at_once_and_the_file_reads_on() {
    const PART: usize = 256 << 10;
    let mut r = rig_in_order(PART);
    let path = tmp("in-order-cancel");
    let payload: Vec<u8> = (0..8 * PART).map(|i| (i * 5 % 241) as u8).collect();
    std::fs::write(&path, &payload).expect("fixture");
    let h = r.open(&path, OpenMode::Read);
    let reads: Vec<OpId> = (0..8).map(|i| r.read(h, (i * PART) as u64, u32::MAX)).collect();
    for op in &reads[1..] {
        r.submit(Op::Cancel { target: *op }, None);
    }
    let got = r.all(&reads);
    let first = got.iter().find(|c| c.op == reads[0]).expect("the first read");
    assert_eq!(first.result, IoResult::Ok(PART));
    assert_eq!(first.buf.as_ref().expect("buffer").memory.data(), &payload[..PART]);
    for c in got.iter().filter(|c| c.op != reads[0]) {
        assert!(c.buf.is_some(), "the buffer rides back");
        if super::NAME == "uring" {
            assert_eq!(c.result, IoResult::Cancelled, "{:?} never started, it waited", c.op);
        }
    }
    r.settle(Duration::from_millis(50));
    for op in &reads {
        assert_eq!(r.count(*op), 0, "{op:?} completed again");
    }
    let next = r.read(h, (5 * PART) as u64, u32::MAX);
    let c = r.wait(next);
    assert_eq!(c.result, IoResult::Ok(PART));
    assert_eq!(c.buf.expect("buffer").memory.data(), &payload[5 * PART..6 * PART]);
}

#[test]
fn a_close_cancels_the_reads_in_order_it_finds_queued() {
    let mut r = rig_in_order(256 << 10);
    let path = tmp("in-order-close");
    std::fs::write(&path, vec![3u8; 2 << 20]).expect("fixture");
    let h = r.open(&path, OpenMode::Read);
    let reads: Vec<OpId> = (0..6).map(|i| r.read(h, i * (256 << 10), u32::MAX)).collect();
    let close = r.submit(Op::Close { handle: h }, None);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    let got = r.all(&reads);
    assert!(got.iter().all(|c| c.result == IoResult::Cancelled && c.buf.is_some()));
    r.settle(Duration::from_millis(50));
    assert!(reads.iter().all(|op| r.count(*op) == 0), "a read completed twice");
}

#[test]
fn a_closing_handle_refuses_new_work_and_the_close_completes() {
    let mut r = rig(2048);
    let bind = r.submit(Op::Bind { addr: loopback() }, None);
    let IoResult::Socket { handle: sock, .. } = r.wait(bind).result else { panic!("bind") };
    let recv = r.submit(Op::Recv { handle: sock }, Some(r.buf(b"")));
    r.r.park(Duration::ZERO);
    let close = r.submit(Op::Close { handle: sock }, None);
    // before the receive's cancel lands, a poll that would keep the slot busy for ever
    let ready = r.submit(Op::Ready { handle: sock }, None);
    let send = r.submit(Op::Send { handle: sock, skip: 0 }, Some(r.buf(b"late")));
    assert_eq!(r.wait(ready).result, IoResult::Err(IoError::BadHandle));
    assert_eq!(r.wait(send).result, IoResult::Err(IoError::BadHandle));
    assert_eq!(r.wait(recv).result, IoResult::Cancelled);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
}

#[test]
fn an_accept_that_lands_before_its_cancel_closes_the_connection() {
    use std::io::Read;
    let mut r = rig(64);
    let listen = r.submit(Op::Listen { addr: loopback() }, None);
    let IoResult::Socket { handle: listener, addr } = r.wait(listen).result else {
        panic!("listen")
    };
    let accept = r.submit(Op::Accept { handle: listener }, None);
    r.r.park(Duration::ZERO);
    let mut client = std::net::TcpStream::connect(addr).expect("connect");
    // the accept's completion waits on the queue, not yet harvested
    let start = Instant::now();
    while !r.r.park(Duration::from_millis(10)) {
        assert!(start.elapsed() < PATIENCE, "the accept never landed");
    }
    r.submit(Op::Cancel { target: accept }, None);
    assert_eq!(r.wait(accept).result, IoResult::Cancelled);
    client.set_read_timeout(Some(Duration::from_secs(2))).expect("timeout");
    let mut b = [0u8; 1];
    assert_eq!(client.read(&mut b).expect("the accepted end was closed"), 0);
}

#[test]
fn a_long_path_finds_a_block_after_a_burst_of_short_ones() {
    let mut r = rig(64);
    let path = tmp("burst");
    std::fs::write(&path, b"x").expect("fixture");
    let bytes = path.as_os_str().as_encoded_bytes().to_vec();
    let name = Name { off: 0, len: bytes.len() as u32 };
    let stats: Vec<OpId> =
        (0..256).map(|_| r.submit_named(Op::Stat { path: name }, None, &bytes)).collect();
    for c in r.all(&stats) {
        assert!(matches!(c.result, IoResult::File { len: 1, .. }), "{:?}", c.result);
    }
    // the same file by a name past the small block
    let long = format!("{}{}", "/.".repeat(400), path.display());
    let name = Name { off: 0, len: long.len() as u32 };
    let op = r.submit_named(Op::Stat { path: name }, None, long.as_bytes());
    assert!(matches!(r.wait(op).result, IoResult::File { len: 1, .. }));
    let op =
        r.submit_named(Op::Stat { path: Name { off: 0, len: bytes.len() as u32 } }, None, &bytes);
    assert!(matches!(r.wait(op).result, IoResult::File { len: 1, .. }), "short names still go");
}
