//! A backend against loopback sockets and temp files. The including file gives `make()` and `NAME`.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
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

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
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
        assert!(subs.is_empty(), "submit drains");
        op
    }

    fn submit(&mut self, kind: Op, buf: Option<Buffer>) -> OpId {
        self.submit_named(kind, buf, &[])
    }

    fn turn(&mut self) {
        let mut out = Vec::new();
        self.r.run_once(&mut out);
        for (element, c) in out {
            assert_eq!(element, ElementId(1));
            assert_eq!(c.user, c.op.0 + 100, "the user tag rides back");
            self.seen.push(c);
        }
    }

    fn done(&mut self, op: OpId) -> bool {
        self.turn();
        self.seen.iter().any(|c| c.op == op)
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

    fn path_op(&mut self, path: &Path, make: impl Fn(Name) -> Op) -> IoResult {
        let bytes = path.as_os_str().as_encoded_bytes();
        let op = self.submit_named(make(Name { off: 0, len: bytes.len() as u32 }), None, bytes);
        self.wait(op).result
    }

    fn open(&mut self, path: &Path, mode: OpenMode) -> IoResult {
        self.path_op(path, |path| Op::Open { path, mode })
    }

    fn socket(&mut self, kind: Op) -> (Handle, SocketAddr) {
        let op = self.submit(kind, None);
        match self.wait(op).result {
            IoResult::Socket { handle, addr } => (handle, addr),
            other => panic!("{kind:?} gave {other:?}"),
        }
    }
}

fn tmp(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pf-reactor-{}-{}", super::NAME, std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name)
}

#[test]
fn a_file_is_created_written_read_stat_renamed_and_unlinked() {
    let mut r = rig(4096);
    let (a, b) = (tmp("file-a"), tmp("file-b"));
    let IoResult::File { handle: w, len: 0 } = r.open(&a, OpenMode::Create) else {
        panic!("create")
    };
    let first = r.submit(Op::Write { handle: w, offset: 0 }, Some(r.buf(b"hello ")));
    let second = r.submit(Op::Write { handle: w, offset: 6 }, Some(r.buf(b"reactor")));
    assert_eq!((r.wait(first).result, r.wait(second).result), (IoResult::Ok(6), IoResult::Ok(7)));
    let close = r.submit(Op::Close { handle: w }, None);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));

    assert_eq!(
        r.path_op(&a, |path| Op::Stat { path }),
        IoResult::File { handle: Handle::NONE, len: 13 }
    );
    let both = [a.as_os_str().as_encoded_bytes(), b.as_os_str().as_encoded_bytes()].concat();
    let (la, lb) = (a.as_os_str().len() as u32, b.as_os_str().len() as u32);
    let rename = Op::Rename { from: Name { off: 0, len: la }, to: Name { off: la, len: lb } };
    let op = r.submit_named(rename, None, &both);
    assert_eq!(r.wait(op).result, IoResult::Ok(0));
    assert_eq!(r.open(&a, OpenMode::Read), IoResult::Err(IoError::NotFound));

    let IoResult::File { handle, len: 13 } = r.open(&b, OpenMode::Read) else { panic!("open") };
    let read = r.submit(Op::Read { handle, offset: 6, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    let got = r.wait(read);
    assert_eq!(got.result, IoResult::Ok(7));
    assert_eq!(got.buf.expect("the buffer rides back").memory.data(), b"reactor");
    let end = r.submit(Op::Read { handle, offset: 13, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    assert_eq!(r.wait(end).result, IoResult::Ok(0), "a read at the end is the end of the file");
    let range = r.submit(Op::Read { handle, offset: 2, at: 3, len: 4 }, Some(r.buf(b"")));
    let got = r.wait(range);
    assert_eq!(got.result, IoResult::Ok(4));
    assert_eq!(&got.buf.expect("the buffer rides back").memory.data()[3..], b"llo ");

    assert_eq!(r.path_op(&b, |path| Op::Unlink { path }), IoResult::Ok(0));
    assert_eq!(r.path_op(&b, |path| Op::Stat { path }), IoResult::Err(IoError::NotFound));
}

#[test]
fn a_directory_is_not_a_file_and_a_closed_handle_names_nothing() {
    let mut r = rig(64);
    assert_eq!(r.open(&std::env::temp_dir(), OpenMode::Read), IoResult::Err(IoError::NotAFile));
    let path = tmp("stale");
    std::fs::write(&path, b"x").expect("fixture");
    let IoResult::File { handle, .. } = r.open(&path, OpenMode::Read) else { panic!("open") };
    let close = r.submit(Op::Close { handle }, None);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    let read = r.submit(Op::Read { handle, offset: 0, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    assert_eq!(r.wait(read).result, IoResult::Err(IoError::BadHandle));
    let IoResult::File { handle: again, .. } = r.open(&path, OpenMode::Read) else {
        panic!("open")
    };
    assert_eq!(again.slot(), handle.slot());
    assert_ne!(again, handle);
    let read = r.submit(Op::Read { handle, offset: 0, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    assert_eq!(r.wait(read).result, IoResult::Err(IoError::BadHandle));
    let close = r.submit(Op::Close { handle }, None);
    assert_eq!(r.wait(close).result, IoResult::Err(IoError::BadHandle));
}

#[test]
fn connect_accept_send_and_recv_over_loopback() {
    let mut r = rig(4096);
    let (listener, at) = r.socket(Op::Listen { addr: loopback() });
    let accept = r.submit(Op::Accept { handle: listener }, None);
    assert!(!r.done(accept), "nobody has connected");
    let (client, peer) = r.socket(Op::Connect { addr: at });
    assert_eq!(peer, at);
    let IoResult::Socket { handle: server, .. } = r.wait(accept).result else { panic!("accept") };

    let recv = r.submit(Op::Recv { handle: server }, Some(r.buf(b"")));
    assert!(!r.done(recv), "nothing was sent");
    let send = r.submit(Op::Send { handle: client, skip: 2 }, Some(r.buf(b"..ping")));
    assert_eq!(r.wait(send).result, IoResult::Ok(4), "the bytes past the skip");
    let got = r.wait(recv);
    assert_eq!(got.result, IoResult::Ok(4));
    assert_eq!(got.buf.expect("buffer").memory.data(), b"ping");

    let close = r.submit(Op::Close { handle: client }, None);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    let end = r.submit(Op::Recv { handle: server }, Some(r.buf(b"")));
    assert_eq!(r.wait(end).result, IoResult::Ok(0));
}

#[test]
fn a_refused_connect_says_so() {
    let addr = TcpListener::bind(loopback()).expect("bind").local_addr().expect("addr");
    let mut r = rig(64);
    let op = r.submit(Op::Connect { addr }, None);
    assert_eq!(r.wait(op).result, IoResult::Err(IoError::Refused));
}

#[test]
fn two_recvs_on_one_stream_complete_in_the_order_submitted() {
    let listener = TcpListener::bind(loopback()).expect("bind");
    let mut r = rig(4);
    let (client, _) = r.socket(Op::Connect { addr: listener.local_addr().expect("addr") });
    let (mut server, _) = listener.accept().expect("accept");
    let first = r.submit(Op::Recv { handle: client }, Some(r.buf(b"")));
    let second = r.submit(Op::Recv { handle: client }, Some(r.buf(b"")));
    server.write_all(b"abcdefgh").expect("write");
    let (a, b) = (r.wait(first), r.wait(second));
    assert_eq!(a.buf.expect("buffer").memory.data(), b"abcd");
    assert_eq!(b.buf.expect("buffer").memory.data(), b"efgh");
}

#[test]
fn a_short_write_is_reported_and_resumed() {
    const LEN: usize = 8 << 20;
    let listener = TcpListener::bind(loopback()).expect("bind");
    let mut r = rig(LEN);
    let (client, _) = r.socket(Op::Connect { addr: listener.local_addr().expect("addr") });
    let (mut server, _) = listener.accept().expect("accept");
    let payload: Vec<u8> = (0..LEN).map(|i| (i * 31 % 251) as u8).collect();

    let op = r.submit(Op::Send { handle: client, skip: 0 }, Some(r.buf(&payload)));
    let first = r.wait(op);
    let IoResult::Ok(mut sent) = first.result else { panic!("send gave {:?}", first.result) };
    assert!(sent > 0 && sent < LEN, "nobody reads, so the socket took {sent} of {LEN}");
    let mut buf = first.buf.expect("the buffer rides back");
    let stuck = r.submit(Op::Send { handle: client, skip: sent as u32 }, Some(buf));
    assert!(!r.done(stuck), "a full socket leaves the send waiting");

    let reader = std::thread::spawn(move || {
        let mut got = Vec::with_capacity(LEN);
        server.read_to_end(&mut got).expect("read");
        got
    });
    let mut op = stuck;
    loop {
        let c = r.wait(op);
        let IoResult::Ok(n) = c.result else { panic!("send gave {:?}", c.result) };
        sent += n;
        buf = c.buf.expect("buffer");
        if sent == LEN {
            break;
        }
        op = r.submit(Op::Send { handle: client, skip: sent as u32 }, Some(buf));
    }
    let close = r.submit(Op::Close { handle: client }, None);
    r.wait(close);
    assert!(reader.join().expect("reader") == payload, "the bytes differ");
}

#[test]
fn a_peer_reset_fails_the_recv() {
    let listener = TcpListener::bind(loopback()).expect("bind");
    let mut r = rig(64);
    let (client, _) = r.socket(Op::Connect { addr: listener.local_addr().expect("addr") });
    let (server, _) = listener.accept().expect("accept");
    let send = r.submit(Op::Send { handle: client, skip: 0 }, Some(r.buf(b"unread")));
    assert_eq!(r.wait(send).result, IoResult::Ok(6));
    // Closing with unread input makes Linux answer with a reset.
    let recv = r.submit(Op::Recv { handle: client }, Some(r.buf(b"")));
    std::thread::sleep(Duration::from_millis(20));
    drop(server);
    assert_eq!(r.wait(recv).result, IoResult::Err(IoError::Reset));
}

/// A listener with no backlog drops further SYNs, so those connects stay under way.
fn stuck_listener() -> (i32, SocketAddr) {
    let l = TcpListener::bind(loopback()).expect("bind");
    let addr = l.local_addr().expect("addr");
    let fd = std::os::fd::IntoRawFd::into_raw_fd(l);
    // SAFETY: an fd this test owns and an integer.
    assert_eq!(unsafe { libc::listen(fd, 0) }, 0);
    (fd, addr)
}

#[test]
fn a_connect_under_way_is_cancelled_and_its_slot_freed() {
    let (fd, addr) = stuck_listener();
    let mut r = rig(64);
    let ops: Vec<OpId> = (0..4).map(|_| r.submit(Op::Connect { addr }, None)).collect();
    r.r.park(Duration::ZERO);
    std::thread::sleep(Duration::from_millis(50));
    let stuck: Vec<OpId> = ops.into_iter().filter(|op| !r.done(*op)).collect();
    assert!(!stuck.is_empty(), "a full backlog leaves a connect under way");
    for op in &stuck {
        r.submit(Op::Cancel { target: *op }, None);
    }
    for op in stuck {
        let c = r.wait(op);
        assert_eq!(c.result, IoResult::Cancelled);
    }
    r.submit(Op::Cancel { target: OpId(9999) }, None);
    r.turn();
    // SAFETY: the fd from `stuck_listener`, closed once.
    unsafe { libc::close(fd) };
}

#[test]
fn close_cancels_what_waits_on_the_handle_first() {
    let listener = TcpListener::bind(loopback()).expect("bind");
    let mut r = rig(64);
    let (client, _) = r.socket(Op::Connect { addr: listener.local_addr().expect("addr") });
    let _server = listener.accept().expect("accept");
    let recv = r.submit(Op::Recv { handle: client }, Some(r.buf(b"")));
    assert!(!r.done(recv));
    let close = r.submit(Op::Close { handle: client }, None);
    r.turn();
    let order: Vec<(OpId, IoResult)> = r.seen.iter().map(|c| (c.op, c.result)).collect();
    assert_eq!(order, [(recv, IoResult::Cancelled), (close, IoResult::Ok(0))]);
    assert!(r.seen[0].buf.is_some(), "the cancelled recv gives its buffer back");
    let late = r.submit(Op::Recv { handle: client }, Some(r.buf(b"")));
    assert_eq!(r.wait(late).result, IoResult::Err(IoError::BadHandle));
}

#[test]
fn timers_complete_in_deadline_order_and_not_early() {
    let mut r = rig(64);
    assert!(r.r.due_in().is_none(), "nothing is timed");
    let start = Instant::now();
    let ms = |n| Op::Timer { after: Duration::from_millis(n) };
    let (c, a, b) = (r.submit(ms(60), None), r.submit(ms(20), None), r.submit(ms(40), None));
    let gone = r.submit(ms(30), None);
    r.submit(Op::Cancel { target: gone }, None);
    assert!(r.r.due_in().expect("a timer is pending") <= Duration::from_millis(20));
    r.turn();
    assert!(r.r.is_idle(), "a timer is not waited for at teardown");
    let mut order = Vec::new();
    while order.len() < 4 {
        r.turn();
        for c in r.seen.drain(..) {
            let due = match c.op {
                op if op == a => 20,
                op if op == b => 40,
                op if op == gone => 0,
                _ => 60,
            };
            assert!(start.elapsed() >= Duration::from_millis(due), "{:?} fired early", c.op);
            order.push((c.op, c.result));
        }
        assert!(start.elapsed() < PATIENCE);
        std::thread::sleep(Duration::from_millis(1));
    }
    let ok = IoResult::Ok(0);
    assert_eq!(order, [(gone, IoResult::Cancelled), (a, ok), (b, ok), (c, ok)]);
    assert!(r.r.due_in().is_none());
}

#[test]
fn a_name_resolves_and_an_unknown_one_fails() {
    let mut r = rig(64);
    let name =
        |s: &str| Op::Resolve { host: Name { off: 0, len: s.len() as u32 }, port: 80, skip: 0 };
    let op = r.submit_named(name("localhost"), None, b"localhost");
    let IoResult::Addr(addr) = r.wait(op).result else { panic!("localhost") };
    assert!(addr.ip().is_loopback() && addr.port() == 80);
    let op = r.submit_named(name("127.0.0.1"), None, b"127.0.0.1");
    assert_eq!(r.wait(op).result, IoResult::Addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 80))));
    let past = Op::Resolve { host: Name { off: 0, len: 9 }, port: 80, skip: 1 };
    let op = r.submit_named(past, None, b"127.0.0.1");
    assert_eq!(r.wait(op).result, IoResult::Err(IoError::NoAddress));
    // RFC 6761 6.4 reserves `.invalid`, so no resolver answers for it.
    let op = r.submit_named(name("nowhere.invalid"), None, b"nowhere.invalid");
    assert_eq!(r.wait(op).result, IoResult::Err(IoError::NoAddress));
    let op = r.submit_named(name("localhost"), None, b"localhost");
    r.submit(Op::Cancel { target: op }, None);
    assert_eq!(r.wait(op).result, IoResult::Cancelled);
    std::thread::sleep(Duration::from_millis(50));
    r.turn();
    assert!(r.seen.is_empty(), "the late answer is dropped");
}

/// HTTPS record type (RFC 9460 14.3.1).
const HTTPS: u16 = 65;

#[test]
fn a_lookup_of_a_name_with_no_record_completes_as_none() {
    let mut r = rig(4096);
    let name = b"nowhere.invalid";
    let op = r.submit_named(
        Op::Lookup { host: Name { off: 0, len: name.len() as u32 }, kind: HTTPS },
        Some(r.buf(b"")),
        name,
    );
    let c = r.wait(op);
    assert_eq!(c.result, IoResult::Err(IoError::NoRecord));
    assert!(c.buf.is_some_and(|b| b.memory.is_empty()), "the buffer rides back, empty");
    let op = r.submit_named(
        Op::Lookup { host: Name { off: 0, len: 3 }, kind: HTTPS },
        Some(r.buf(b"")),
        b"a\0b",
    );
    assert_eq!(r.wait(op).result, IoResult::Err(IoError::NoRecord));
}

#[test]
fn a_cancelled_lookup_completes_as_cancelled_once() {
    let mut r = rig(4096);
    let name = b"localhost";
    let op = r.submit_named(
        Op::Lookup { host: Name { off: 0, len: name.len() as u32 }, kind: HTTPS },
        Some(r.buf(b"")),
        name,
    );
    r.submit(Op::Cancel { target: op }, None);
    assert_eq!(r.wait(op).result, IoResult::Cancelled);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(200) {
        r.turn();
        assert!(r.seen.is_empty(), "the late answer is dropped");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn udp_binds_sends_and_receives_whole_datagrams() {
    let mut r = rig(2048);
    let (a, at_a) = r.socket(Op::Bind { addr: loopback() });
    let (b, at_b) = r.socket(Op::Bind { addr: loopback() });
    let recv = r.submit(Op::Recv { handle: b }, Some(r.buf(b"")));
    assert!(!r.done(recv));
    let to = r.submit(Op::SendTo { handle: a, addr: at_b }, Some(r.buf(b"one")));
    assert_eq!(r.wait(to).result, IoResult::Ok(3));
    assert_eq!(r.wait(recv).buf.expect("buffer").memory.data(), b"one");
    let peer = r.submit(Op::Peer { handle: b, addr: at_a }, None);
    assert_eq!(r.wait(peer).result, IoResult::Ok(0));
    let stranger = UdpSocket::bind(loopback()).expect("bind");
    stranger.send_to(b"noise", at_b).expect("send");
    let send = r.submit(Op::Send { handle: b, skip: 0 }, Some(r.buf(b"two")));
    assert_eq!(r.wait(send).result, IoResult::Ok(3));
    let back = r.submit(Op::Recv { handle: a }, Some(r.buf(b"")));
    assert_eq!(r.wait(back).buf.expect("buffer").memory.data(), b"two");
    let quiet = r.submit(Op::Recv { handle: b }, Some(r.buf(b"")));
    std::thread::sleep(Duration::from_millis(20));
    assert!(!r.done(quiet), "the stranger's datagram was dropped by the kernel");
    let accept = r.submit(Op::Accept { handle: a }, None);
    assert_eq!(r.wait(accept).result, IoResult::Err(IoError::WrongKind));
}

#[test]
fn a_parked_socket_is_adopted_by_the_bind_of_its_address() {
    let sock = UdpSocket::bind(loopback()).expect("bind");
    let addr = pf_reactor::park_udp(sock).expect("park");
    let mut r = rig(64);
    let (handle, local) = r.socket(Op::Bind { addr });
    assert_eq!(local, addr);
    let sender = UdpSocket::bind(loopback()).expect("bind");
    sender.send_to(b"kept", addr).expect("send");
    let recv = r.submit(Op::Recv { handle }, Some(r.buf(b"")));
    assert_eq!(r.wait(recv).buf.expect("buffer").memory.data(), b"kept");
    let again = r.submit(Op::Bind { addr }, None);
    assert_eq!(r.wait(again).result, IoResult::Err(IoError::AddrInUse));
}

#[test]
fn a_connect_to_a_std_listener_carries_bytes_both_ways() {
    let listener = TcpListener::bind(loopback()).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let echo = std::thread::spawn(move || {
        let (mut s, _): (TcpStream, _) = listener.accept().expect("accept");
        let mut b = [0u8; 5];
        s.read_exact(&mut b).expect("read");
        s.write_all(&b).expect("write");
    });
    let mut r = rig(64);
    let (h, _) = r.socket(Op::Connect { addr });
    let send = r.submit(Op::Send { handle: h, skip: 0 }, Some(r.buf(b"hello")));
    assert_eq!(r.wait(send).result, IoResult::Ok(5));
    let recv = r.submit(Op::Recv { handle: h }, Some(r.buf(b"")));
    assert_eq!(r.wait(recv).buf.expect("buffer").memory.data(), b"hello");
    echo.join().expect("echo");
}

fn fifo(name: &str) -> PathBuf {
    let path = tmp(name);
    let _ = std::fs::remove_file(&path);
    let made = std::process::Command::new("mkfifo").arg(&path).status().expect("mkfifo runs");
    assert!(made.success(), "mkfifo {}", path.display());
    path
}

#[test]
fn a_reactor_of_files_reads_them_and_on_epoll_waits_on_nothing() {
    let mut r = Rig { r: super::make_files(), pool: Pool::new(64), next: 0, seen: Vec::new() };
    let path = tmp("files-only");
    std::fs::write(&path, b"only files").expect("fixture");
    let IoResult::File { handle, len: 10 } = r.open(&path, OpenMode::Read) else { panic!("open") };
    let read = r.submit(Op::Read { handle, offset: 5, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    assert_eq!(r.wait(read).buf.expect("the buffer rides back").memory.data(), b"files");
    let pipe = fifo("files-only-pipe");
    let IoResult::File { handle: p, .. } = r.open(&pipe, OpenMode::Read) else {
        panic!("open the pipe")
    };
    let recv = r.submit(Op::Recv { handle: p }, Some(r.buf(b"")));
    if super::NAME == "epoll" {
        assert_eq!(r.wait(recv).result, IoResult::Err(IoError::Os(libc::EMFILE)));
    } else {
        std::thread::sleep(Duration::from_millis(20));
        assert!(!r.done(recv), "the ring waits for a writer");
        r.submit(Op::Cancel { target: recv }, None);
        assert_eq!(r.wait(recv).result, IoResult::Cancelled);
    }
    for op in [Op::Close { handle }, Op::Close { handle: p }] {
        let close = r.submit(op, None);
        assert_eq!(r.wait(close).result, IoResult::Ok(0));
    }
    let _ = (std::fs::remove_file(&path), std::fs::remove_file(&pipe));
}

#[test]
fn a_pipe_opens_with_no_writer_and_its_read_waits_is_cancelled_and_ends() {
    use profluens_core::io::NO_LEN;
    let mut r = rig(64);
    let path = fifo("pipe");
    let began = Instant::now();
    let IoResult::File { handle, len: NO_LEN } = r.open(&path, OpenMode::Read) else {
        panic!("open")
    };
    assert!(began.elapsed() < Duration::from_secs(2), "the open waited for a writer");
    let at = r.submit(Op::Read { handle, offset: 0, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    assert_eq!(r.wait(at).result, IoResult::Err(IoError::WrongKind));

    let waits = r.submit(Op::Recv { handle }, Some(r.buf(b"")));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!r.done(waits), "a receive with no writer completed");
    r.submit(Op::Cancel { target: waits }, None);
    assert_eq!(r.wait(waits).result, IoResult::Cancelled);

    let recv = r.submit(Op::Recv { handle }, Some(r.buf(b"")));
    let writer = {
        let path = path.clone();
        std::thread::spawn(move || {
            let mut pipe =
                std::fs::OpenOptions::new().write(true).open(path).expect("the writer opens");
            pipe.write_all(b"through a pipe").expect("write");
        })
    };
    assert_eq!(r.wait(recv).buf.expect("buffer").memory.data(), b"through a pipe");
    writer.join().expect("the writer");
    let end = r.submit(Op::Recv { handle }, Some(r.buf(b"")));
    assert_eq!(r.wait(end).result, IoResult::Ok(0), "the writer left, which is the end");
    let close = r.submit(Op::Close { handle }, None);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_adopted_file_reads_on_a_descriptor_of_its_own() {
    use std::os::fd::AsRawFd;
    let mut r = rig(4096);
    let a = tmp("adopted");
    std::fs::write(&a, b"held open").expect("write");
    let file = std::fs::File::open(&a).expect("open");
    let fd = file.as_raw_fd();
    let op = r.submit(Op::Adopt { fd, mode: OpenMode::Read }, None);
    let IoResult::File { handle, len: 9 } = r.wait(op).result else { panic!("adopt") };
    drop(file);
    std::fs::remove_file(&a).expect("unlink");
    let read = r.submit(Op::Read { handle, offset: 5, at: 0, len: u32::MAX }, Some(r.buf(b"")));
    let c = r.wait(read);
    assert_eq!(c.result, IoResult::Ok(4));
    assert_eq!(c.buf.as_ref().map(|b| b.memory.data()), Some(&b"open"[..]));
    let file = std::fs::File::open(tmp("")).expect("the temp dir");
    let op = r.submit(Op::Adopt { fd: file.as_raw_fd(), mode: OpenMode::Read }, None);
    assert_eq!(r.wait(op).result, IoResult::Err(IoError::NotAFile));
    let op = r.submit(Op::Adopt { fd: file.as_raw_fd(), mode: OpenMode::Create }, None);
    assert_eq!(r.wait(op).result, IoResult::Err(IoError::Unsupported));
}
