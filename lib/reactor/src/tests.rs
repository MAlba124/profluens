//! Teardown and cancel refusal under injected faults, on both backends.
#![allow(clippy::disallowed_methods)]

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::id::{ElementId, FormatId};
use profluens_core::io::{Completion, Handle, IoError, IoResult, Op, OpId, Reactor, Submission};
use profluens_core::memory::Pool;
use profluens_core::time::Timestamp;

use crate::{OsReactor, Want};

const PATIENCE: Duration = Duration::from_secs(10);
const BOTH: [Want; 2] = [Want::Uring, Want::Epoll];

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
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

fn one(r: &mut OsReactor, op: u64, kind: Op, buf: Option<Buffer>) {
    let mut subs = vec![Submission { op: OpId(op), element: ElementId(1), kind, buf, user: op }];
    r.submit(&mut subs, &[]);
}

fn wait(r: &mut OsReactor, seen: &mut Vec<Completion>, op: u64) -> Completion {
    let (start, mut out) = (Instant::now(), Vec::new());
    loop {
        if let Some(i) = seen.iter().position(|c| c.op == OpId(op)) {
            return seen.remove(i);
        }
        assert!(start.elapsed() < PATIENCE, "op {op} never completed");
        r.run_once(&mut out);
        seen.extend(out.drain(..).map(|(_, c)| c));
        r.park(Duration::from_millis(5));
    }
}

fn bound(r: &mut OsReactor, seen: &mut Vec<Completion>, op: u64) -> (Handle, SocketAddr) {
    one(r, op, Op::Bind { addr: loopback() }, None);
    match wait(r, seen, op).result {
        IoResult::Socket { handle, addr } => (handle, addr),
        other => panic!("bind gave {other:?}"),
    }
}

/// Drop a reactor with a receive on the queue. The buffers still out after and how long it took.
fn torn_down(want: Want, drain_fails: bool) -> (u64, Duration) {
    let pool = Pool::new(2048);
    let mut r = OsReactor::on(want).expect("the backend");
    let mut seen = Vec::new();
    let (sock, _) = bound(&mut r, &mut seen, 1);
    one(&mut r, 2, Op::Recv { handle: sock }, Some(bare(&pool)));
    r.park(Duration::ZERO);
    assert_eq!(pool.in_use().0, 1);
    r.faults.drain_fails = drain_fails;
    let start = Instant::now();
    drop(r);
    (pool.in_use().0, start.elapsed())
}

#[test]
fn a_drained_teardown_frees_and_a_failed_one_keeps_what_the_kernel_may_use() {
    for want in BOTH {
        let (out, _) = torn_down(want, false);
        assert_eq!(out, 0, "{want:?}: a drained queue gives the receive's buffer back");
        let (out, took) = torn_down(want, true);
        assert_eq!(out, 1, "{want:?}: the kernel may still write the buffer, so it stays out");
        assert!(took < Duration::from_secs(1), "{want:?}: the queue's drop waited again, {took:?}");
    }
}

#[test]
fn a_refused_cancel_is_asked_again_and_the_receive_ends() {
    for want in BOTH {
        let pool = Pool::new(2048);
        let mut r = OsReactor::on(want).expect("the backend");
        let mut seen = Vec::new();
        let (sock, _) = bound(&mut r, &mut seen, 1);
        one(&mut r, 2, Op::Recv { handle: sock }, Some(bare(&pool)));
        r.park(Duration::ZERO);
        r.faults.refuse_cancels = 1;
        one(&mut r, 3, Op::Cancel { target: OpId(2) }, None);
        assert_eq!(r.faults.refuse_cancels, 0, "{want:?}: the cancel was asked");
        let c = wait(&mut r, &mut seen, 2);
        assert_eq!(c.result, IoResult::Cancelled, "{want:?}");
        assert!(c.buf.is_some(), "{want:?}: the buffer rides back");
    }
}

#[test]
fn a_second_cancel_after_a_refused_one_is_asked() {
    for want in BOTH {
        let pool = Pool::new(2048);
        let mut r = OsReactor::on(want).expect("the backend");
        let mut seen = Vec::new();
        let (sock, _) = bound(&mut r, &mut seen, 1);
        one(&mut r, 2, Op::Recv { handle: sock }, Some(bare(&pool)));
        r.park(Duration::ZERO);
        r.faults.refuse_cancels = 1;
        let twice = vec![
            Submission {
                op: OpId(3),
                element: ElementId(1),
                kind: Op::Cancel { target: OpId(2) },
                buf: None,
                user: 3,
            },
            Submission {
                op: OpId(4),
                element: ElementId(1),
                kind: Op::Cancel { target: OpId(2) },
                buf: None,
                user: 4,
            },
        ];
        let mut subs = twice;
        r.submit(&mut subs, &[]);
        assert_eq!(wait(&mut r, &mut seen, 2).result, IoResult::Cancelled, "{want:?}");
    }
}

#[test]
fn a_dry_receive_whose_cancel_was_refused_ends_dry() {
    for want in BOTH {
        let pool = Pool::new(2048);
        let mut r = OsReactor::on(want).expect("the backend");
        let mut seen = Vec::new();
        let (sock, addr) = bound(&mut r, &mut seen, 1);
        one(&mut r, 2, Op::Lend { handle: sock }, Some(bare(&pool)));
        one(&mut r, 3, Op::RecvMany { handle: sock }, None);
        r.park(Duration::ZERO);
        r.faults.refuse_cancels = 1;
        let tx = UdpSocket::bind(loopback()).expect("bind");
        tx.send_to(b"last", addr).expect("send");
        let c = wait(&mut r, &mut seen, 2);
        assert!(matches!(c.result, IoResult::Datagram { len: 4, .. }), "{want:?} {:?}", c.result);
        assert_eq!(r.faults.refuse_cancels, 0, "{want:?}: the dry cancel was asked");
        assert_eq!(wait(&mut r, &mut seen, 3).result, IoResult::Err(IoError::Dry), "{want:?}");
    }
}
