//! Later reactor contract ops on both backends. The including file gives `WANT` and `NAME`.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pf_reactor::OsReactor;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::id::{ElementId, FormatId};
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Name, Op, OpId, OpenMode, Reactor, Submission,
};
use profluens_core::memory::Pool;
use profluens_core::time::Timestamp;

const PATIENCE: Duration = Duration::from_secs(10);

struct Rig {
    r: OsReactor,
    pool: Pool,
    next: u64,
    seen: Vec<Completion>,
}

fn rig(slot: usize) -> Rig {
    let r = OsReactor::on(super::WANT).expect("the backend");
    Rig { r, pool: Pool::new(slot), next: 0, seen: Vec::new() }
}

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn tmp(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pf-contract-{}-{}", super::NAME, std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name)
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
        let mut subs = vec![Submission { op, element: ElementId(1), kind, buf, user: op.0 + 100 }];
        self.r.submit(&mut subs, names);
        op
    }

    fn submit(&mut self, kind: Op, buf: Option<Buffer>) -> OpId {
        self.submit_named(kind, buf, &[])
    }

    fn turn(&mut self) {
        let mut out = Vec::new();
        self.r.run_once(&mut out);
        for (_, c) in out {
            assert_eq!(c.user, c.op.0 + 100, "the user tag rides back");
            assert!(!self.seen.iter().any(|s| s.op == c.op), "{:?} completed twice", c.op);
            self.seen.push(c);
        }
    }

    fn wait(&mut self, op: OpId) -> Completion {
        let start = Instant::now();
        loop {
            if let Some(i) = self.seen.iter().position(|c| c.op == op) {
                return self.seen.remove(i);
            }
            assert!(start.elapsed() < PATIENCE, "{op:?} never completed");
            self.turn();
            self.r.park(Duration::from_millis(5));
        }
    }

    fn path_op(&mut self, path: &Path, make: impl Fn(Name) -> Op) -> IoResult {
        let bytes = path.as_os_str().as_encoded_bytes();
        let op = self.submit_named(make(Name { off: 0, len: bytes.len() as u32 }), None, bytes);
        self.wait(op).result
    }

    fn open(&mut self, path: &Path, mode: OpenMode) -> IoResult {
        self.path_op(path, |path| Op::Open { path, mode })
    }

    fn bound(&mut self) -> (Handle, SocketAddr) {
        let op = self.submit(Op::Bind { addr: loopback() }, None);
        match self.wait(op).result {
            IoResult::Socket { handle, addr } => (handle, addr),
            other => panic!("bind gave {other:?}"),
        }
    }

    fn write(&mut self, handle: Handle, offset: u64, bytes: &[u8]) {
        let op = self.submit(Op::Write { handle, offset }, Some(self.buf(bytes)));
        assert_eq!(self.wait(op).result, IoResult::Ok(bytes.len()));
    }

    fn close(&mut self, handle: Handle) {
        let op = self.submit(Op::Close { handle }, None);
        assert_eq!(self.wait(op).result, IoResult::Ok(0));
    }
}

#[test]
fn a_create_replaces_a_file_writes_through_a_symlink_and_makes_its_parents() {
    let mut r = rig(4096);
    let old = tmp("create-old");
    std::fs::write(&old, b"old contents").expect("fixture");
    let held = std::fs::File::open(&old).expect("held");
    let IoResult::File { handle, len: 0 } = r.open(&old, OpenMode::Create) else {
        panic!("create")
    };
    r.write(handle, 0, b"new");
    r.close(handle);
    assert_eq!(std::fs::read(&old).expect("read"), b"new");
    let mut kept = Vec::new();
    std::io::Read::read_to_end(&mut &held, &mut kept).expect("read held");
    assert_eq!(kept, b"old contents", "the old file was unlinked, not truncated");

    let (target, link) = (tmp("create-target"), tmp("create-link"));
    std::fs::write(&target, b"x").expect("fixture");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let IoResult::File { handle, .. } = r.open(&link, OpenMode::Create) else { panic!("create") };
    r.write(handle, 0, b"through");
    r.close(handle);
    assert!(
        std::fs::symlink_metadata(&link).expect("link").file_type().is_symlink(),
        "the link stays"
    );
    assert_eq!(std::fs::read(&target).expect("read"), b"through");

    let deep = tmp("create-a").join("b").join("c.bin");
    let _ = std::fs::remove_dir_all(tmp("create-a"));
    let IoResult::File { handle, .. } = r.open(&deep, OpenMode::Create) else { panic!("create") };
    r.write(handle, 0, b"deep");
    r.close(handle);
    assert_eq!(std::fs::read(&deep).expect("read"), b"deep");
}

#[test]
fn a_name_past_the_small_block_goes_and_one_the_kernel_refuses_does_not() {
    let mut r = rig(64);
    let dir = tmp(&"d".repeat(200));
    let long = dir.join("e".repeat(250));
    let _ = std::fs::create_dir_all(&dir);
    let IoResult::File { handle, .. } = r.open(&long, OpenMode::Create) else {
        panic!("a long name")
    };
    r.close(handle);
    assert!(matches!(r.path_op(&long, |path| Op::Stat { path }), IoResult::File { len: 0, .. }));
    let both =
        [long.as_os_str().as_encoded_bytes(), tmp("short").as_os_str().as_encoded_bytes()].concat();
    let (la, lb) = (long.as_os_str().len() as u32, tmp("short").as_os_str().len() as u32);
    let rename = Op::Rename { from: Name { off: 0, len: la }, to: Name { off: la, len: lb } };
    let op = r.submit_named(rename, None, &both);
    assert_eq!(r.wait(op).result, IoResult::Ok(0));
    let huge = PathBuf::from(format!("/{}", "x".repeat(5000)));
    assert_eq!(r.open(&huge, OpenMode::Read), IoResult::Err(IoError::Os(libc::ENAMETOOLONG)));
    let nul = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/a\0b"));
    assert_eq!(r.open(&nul, OpenMode::Read), IoResult::Err(IoError::Os(libc::EINVAL)));
    let names: Vec<PathBuf> = (0..40).map(|i| tmp(&format!("many-{i}"))).collect();
    let ops: Vec<OpId> = names
        .iter()
        .map(|p| {
            let b = p.as_os_str().as_encoded_bytes();
            r.submit_named(
                Op::Open { path: Name { off: 0, len: b.len() as u32 }, mode: OpenMode::Create },
                None,
                b,
            )
        })
        .collect();
    for op in ops {
        let IoResult::File { handle, .. } = r.wait(op).result else { panic!("create") };
        r.close(handle);
    }
}

#[test]
fn a_cancelled_open_completes_once_and_leaves_nothing_open() {
    let mut r = rig(64);
    let path = tmp("cancel-open");
    std::fs::write(&path, b"x").expect("fixture");
    let bytes = path.as_os_str().as_encoded_bytes();
    let open = r.submit_named(
        Op::Open { path: Name { off: 0, len: bytes.len() as u32 }, mode: OpenMode::Read },
        None,
        bytes,
    );
    r.submit(Op::Cancel { target: open }, None);
    match r.wait(open).result {
        IoResult::Cancelled => {}
        IoResult::File { handle, .. } => r.close(handle),
        other => panic!("{other:?}"),
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(50) {
        r.turn();
    }
    assert!(r.seen.is_empty(), "one completion");
}

#[test]
fn hints_ride_the_queue_and_a_close_waits_for_them() {
    let mut r = rig(64 << 10);
    r.r.set_hygiene_window(64 << 10);
    let path = tmp("hints");
    let IoResult::File { handle, .. } = r.open(&path, OpenMode::Create) else { panic!("create") };
    let chunk = vec![7u8; 64 << 10];
    for i in 0..8u64 {
        r.write(handle, i * chunk.len() as u64, &chunk);
    }
    r.close(handle);
    assert!(r.r.is_idle(), "nothing of the file is left in flight");
    assert_eq!(std::fs::metadata(&path).expect("meta").len(), 8 * chunk.len() as u64);
    let IoResult::File { handle, len } = r.open(&path, OpenMode::ReadOnce) else { panic!("open") };
    assert_eq!(len, 8 << 16);
    for i in 0..8u64 {
        let op =
            r.submit(Op::Read { handle, offset: i << 16, at: 0, len: u32::MAX }, Some(r.buf(b"")));
        assert_eq!(r.wait(op).result, IoResult::Ok(64 << 10));
    }
    r.close(handle);
}

fn lend(r: &mut Rig, sock: Handle, n: usize) -> (Vec<OpId>, OpId) {
    let lends = (0..n).map(|_| r.submit(Op::Lend { handle: sock }, Some(r.buf(b"")))).collect();
    let many = r.submit(Op::RecvMany { handle: sock }, None);
    (lends, many)
}

#[test]
fn lent_buffers_take_datagrams_with_their_source_and_a_dry_set_ends_the_receive() {
    let mut r = rig(2048);
    let (sock, addr) = r.bound();
    let (lends, many) = lend(&mut r, sock, 2);
    let tx = UdpSocket::bind(loopback()).expect("bind");
    for word in [&b"one"[..], b"two", b"three"] {
        tx.send_to(word, addr).expect("send");
    }
    let mut got = Vec::new();
    for op in lends {
        let c = r.wait(op);
        let IoResult::Datagram { len, from, segment: 0 } = c.result else {
            panic!("{:?}", c.result)
        };
        assert_eq!(from, tx.local_addr().expect("addr"));
        let data = c.buf.expect("the buffer comes back").memory.data().to_vec();
        assert_eq!(data.len(), len);
        got.push(data);
    }
    assert_eq!(got, [b"one".to_vec(), b"two".to_vec()]);
    assert_eq!(
        r.wait(many).result,
        IoResult::Err(IoError::Dry),
        "the last lent buffer filled ends it"
    );
    let (lends, many) = lend(&mut r, sock, 1);
    let c = r.wait(lends[0]);
    assert!(matches!(c.result, IoResult::Datagram { len: 5, .. }), "{:?}", c.result);
    assert_eq!(c.buf.expect("buffer").memory.data(), b"three");
    assert_eq!(r.wait(many).result, IoResult::Err(IoError::Dry));
}

#[test]
fn a_cancelled_receive_gives_back_every_lent_buffer_first() {
    let mut r = rig(2048);
    let (sock, _) = r.bound();
    let (lends, many) = lend(&mut r, sock, 5);
    r.turn();
    r.submit(Op::Cancel { target: many }, None);
    let end = r.wait(many);
    assert_eq!(end.result, IoResult::Cancelled);
    for op in lends {
        let c = r.wait(op);
        assert_eq!(c.result, IoResult::Cancelled);
        assert!(c.buf.is_some(), "the buffer comes back");
    }
    let (lends, many) = lend(&mut r, sock, 1);
    r.close(sock);
    assert_eq!(r.wait(lends[0]).result, IoResult::Cancelled);
    assert_eq!(r.wait(many).result, IoResult::Cancelled);
}

#[test]
fn a_lend_with_no_receive_comes_back_on_its_cancel_and_on_a_close() {
    let mut r = rig(2048);
    let (sock, _) = r.bound();
    let a = r.submit(Op::Lend { handle: sock }, Some(r.buf(b"")));
    let b = r.submit(Op::Lend { handle: sock }, Some(r.buf(b"")));
    r.submit(Op::Cancel { target: a }, None);
    assert_eq!(r.wait(a).result, IoResult::Cancelled);
    let close = r.submit(Op::Close { handle: sock }, None);
    assert_eq!(r.wait(b).result, IoResult::Cancelled);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    let stale = r.submit(Op::Lend { handle: sock }, Some(r.buf(b"")));
    assert_eq!(r.wait(stale).result, IoResult::Err(IoError::BadHandle));
}

#[test]
fn a_burst_lands_in_the_lent_buffers_at_one_wake() {
    const N: usize = 64;
    let mut r = rig(2048);
    let (sock, addr) = r.bound();
    let (mut lends, many) = lend(&mut r, sock, N);
    r.turn();
    let tx = UdpSocket::bind(loopback()).expect("bind");
    for i in 0..N as u32 {
        tx.send_to(&i.to_be_bytes(), addr).expect("send");
    }
    let mut next = 0u32;
    while let Some(op) = lends.first().copied() {
        let c = r.wait(op);
        lends.remove(0);
        assert_eq!(c.buf.expect("buffer").memory.data(), next.to_be_bytes(), "in arrival order");
        next += 1;
    }
    assert_eq!(next as usize, N);
    assert_eq!(r.wait(many).result, IoResult::Err(IoError::Dry));
}

#[test]
fn a_segmented_send_is_received_coalesced_into_a_large_lent_buffer() {
    let mut r = rig(70_000);
    let (rx, addr) = r.bound();
    let (tx, _) = r.bound();
    let (lends, many) = lend(&mut r, rx, 2);
    r.turn();
    let payload: Vec<u8> = (0..250u32).map(|i| i as u8).collect();
    let send = r
        .submit(Op::SendMany { handle: tx, addr: Some(addr), segment: 100 }, Some(r.buf(&payload)));
    assert_eq!(r.wait(send).result, IoResult::Ok(250));
    let c = r.wait(lends[0]);
    let IoResult::Datagram { len: 250, segment: 100, .. } = c.result else {
        panic!("{:?}", c.result)
    };
    assert_eq!(c.buf.expect("buffer").memory.data(), &payload[..]);
    r.close(rx);
    assert_eq!(r.wait(lends[1]).result, IoResult::Cancelled);
    assert_eq!(r.wait(many).result, IoResult::Cancelled);
}

#[test]
fn a_segmented_send_reaches_a_small_receiver_as_its_datagrams() {
    let mut r = rig(4096);
    let (tx, _) = r.bound();
    let rx = UdpSocket::bind(loopback()).expect("bind");
    rx.set_read_timeout(Some(PATIENCE)).expect("timeout");
    let payload = vec![9u8; 2500];
    let to = Some(rx.local_addr().expect("addr"));
    let send =
        r.submit(Op::SendMany { handle: tx, addr: to, segment: 1000 }, Some(r.buf(&payload)));
    assert_eq!(r.wait(send).result, IoResult::Ok(2500));
    let mut sizes = Vec::new();
    let mut got = [0u8; 4096];
    for _ in 0..3 {
        sizes.push(rx.recv(&mut got).expect("a datagram"));
    }
    assert_eq!(sizes, [1000, 1000, 500]);
    let one = r.submit(Op::SendMany { handle: tx, addr: to, segment: 0 }, Some(r.buf(b"whole")));
    assert_eq!(r.wait(one).result, IoResult::Ok(5));
    assert_eq!(rx.recv(&mut got).expect("a datagram"), 5);
}

#[test]
fn a_stream_fills_lent_buffers_and_its_close_ends_the_receive() {
    let mut r = rig(4096);
    let listener = TcpListener::bind(loopback()).expect("listen");
    let addr = listener.local_addr().expect("addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let connect = r.submit(Op::Connect { addr }, None);
    let start = Instant::now();
    let mut peer = loop {
        r.turn();
        match listener.accept() {
            Ok((peer, _)) => break peer,
            Err(_) => assert!(start.elapsed() < PATIENCE, "no connection"),
        }
        r.r.park(Duration::from_millis(1));
    };
    peer.set_nonblocking(false).expect("blocking");
    let IoResult::Socket { handle, .. } = r.wait(connect).result else { panic!("connect") };
    let (lends, many) = lend(&mut r, handle, 4);
    r.turn();
    peer.write_all(b"stream bytes").expect("write");
    let c = r.wait(lends[0]);
    let IoResult::Ok(n) = c.result else { panic!("{:?}", c.result) };
    let mut got = c.buf.expect("buffer").memory.data().to_vec();
    assert_eq!(got.len(), n);
    let mut rest = lends[1..].to_vec();
    while got.len() < 12 {
        let c = r.wait(rest.remove(0));
        got.extend_from_slice(c.buf.expect("buffer").memory.data());
    }
    assert_eq!(got, b"stream bytes");
    drop(peer);
    assert_eq!(r.wait(many).result, IoResult::Ok(0), "the peer's close");
    for op in rest {
        assert_eq!(r.wait(op).result, IoResult::Cancelled);
    }
    r.close(handle);
}
