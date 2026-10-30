//! No allocation per op in steady state, on both backends.
//! One test in its own binary, since the counter is global.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pf_reactor::{OsReactor, Want};
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::id::{ElementId, FormatId};
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Name, Op, OpId, OpenMode, Reactor, Submission,
};
use profluens_core::memory::Pool;
use profluens_core::time::Timestamp;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: forwards to the system allocator with the same layout.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

struct Rig {
    r: OsReactor,
    pool: Pool,
    next: u64,
    subs: Vec<Submission>,
    out: Vec<(ElementId, Completion)>,
    kept: Vec<(ElementId, Completion)>,
}

impl Rig {
    fn buf(&self, len: usize) -> Buffer {
        let mut memory = self.pool.acquire();
        memory.set_len(len);
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

    fn submit(&mut self, kind: Op, buf: Option<Buffer>, names: &[u8]) -> OpId {
        let op = OpId(self.next);
        self.next += 1;
        self.subs.push(Submission { op, element: ElementId(1), kind, buf, user: 0 });
        self.r.submit(&mut self.subs, names);
        op
    }

    fn wait(&mut self, op: OpId) -> Completion {
        let start = Instant::now();
        loop {
            if let Some(i) = self.kept.iter().position(|(_, c)| c.op == op) {
                return self.kept.swap_remove(i).1;
            }
            self.r.run_once(&mut self.out);
            self.kept.append(&mut self.out);
            if let Some(i) = self.kept.iter().position(|(_, c)| c.op == op) {
                return self.kept.swap_remove(i).1;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{op:?} never completed");
            std::thread::yield_now();
        }
    }
}

struct Udp {
    sock: Handle,
    addr: SocketAddr,
    tx: UdpSocket,
}

fn round(
    r: &mut Rig,
    files: (Handle, Handle),
    client: Handle,
    peer: &mut std::net::TcpStream,
    path: &[u8],
    udp: &Udp,
) {
    let name = Name { off: 0, len: path.len() as u32 };
    let st = r.submit(Op::Stat { path: name }, None, path);
    assert!(matches!(r.wait(st).result, IoResult::File { .. }));
    let open = r.submit(Op::Open { path: name, mode: OpenMode::Read }, None, path);
    let IoResult::File { handle, .. } = r.wait(open).result else { panic!("open") };
    let close = r.submit(Op::Close { handle }, None, &[]);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    let lend = r.submit(Op::Lend { handle: udp.sock }, Some(r.buf(0)), &[]);
    let many = r.submit(Op::RecvMany { handle: udp.sock }, None, &[]);
    r.r.run_once(&mut r.out);
    r.kept.append(&mut r.out);
    udp.tx.send_to(b"lent", udp.addr).expect("send");
    assert!(matches!(r.wait(lend).result, IoResult::Datagram { len: 4, .. }));
    assert_eq!(r.wait(many).result, IoResult::Err(IoError::Dry));
    let w = r.submit(Op::Write { handle: files.0, offset: 0 }, Some(r.buf(4096)), &[]);
    assert_eq!(r.wait(w).result, IoResult::Ok(4096));
    let rd = r.submit(
        Op::Read { handle: files.1, offset: 0, at: 0, len: u32::MAX },
        Some(r.buf(0)),
        &[],
    );
    assert_eq!(r.wait(rd).result, IoResult::Ok(4096));
    let recv = r.submit(Op::Recv { handle: client }, Some(r.buf(0)), &[]);
    peer.write_all(b"ping").expect("write");
    assert_eq!(r.wait(recv).result, IoResult::Ok(4));
    let send = r.submit(Op::Send { handle: client, skip: 0 }, Some(r.buf(4)), &[]);
    assert_eq!(r.wait(send).result, IoResult::Ok(4));
    let mut back = [0u8; 4];
    peer.read_exact(&mut back).expect("read");
    let t = r.submit(Op::Timer { after: Duration::from_micros(10) }, None, &[]);
    assert_eq!(r.wait(t).result, IoResult::Ok(0));
}

#[test]
fn steady_state_ops_allocate_nothing() {
    for want in [Want::Uring, Want::Epoll] {
        steady(want);
    }
}

fn steady(want: Want) {
    let reactor = OsReactor::on(want).expect("the backend");
    let mut r = Rig {
        r: reactor,
        pool: Pool::new(4096),
        next: 0,
        subs: Vec::with_capacity(4),
        out: Vec::with_capacity(64),
        kept: Vec::with_capacity(64),
    };
    let path =
        std::env::temp_dir().join(format!("pf-reactor-alloc-{want:?}-{}", std::process::id()));
    let bytes = path.as_os_str().as_encoded_bytes();
    let open = r.submit(
        Op::Open { path: Name { off: 0, len: bytes.len() as u32 }, mode: OpenMode::Create },
        None,
        bytes,
    );
    let IoResult::File { handle: file, .. } = r.wait(open).result else { panic!("create") };
    let first = r.submit(Op::Write { handle: file, offset: 0 }, Some(r.buf(4096)), &[]);
    assert_eq!(r.wait(first).result, IoResult::Ok(4096));
    let open = r.submit(
        Op::Open { path: Name { off: 0, len: bytes.len() as u32 }, mode: OpenMode::Read },
        None,
        bytes,
    );
    let IoResult::File { handle: reader, .. } = r.wait(open).result else { panic!("open") };
    let files = (file, reader);
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind");
    let connect = r.submit(Op::Connect { addr: listener.local_addr().expect("addr") }, None, &[]);
    let IoResult::Socket { handle: client, .. } = r.wait(connect).result else { panic!("connect") };
    let (mut peer, _) = listener.accept().expect("accept");
    let bind = r.submit(Op::Bind { addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)) }, None, &[]);
    let IoResult::Socket { handle: sock, addr } = r.wait(bind).result else { panic!("bind") };
    let udp = Udp {
        sock,
        addr,
        tx: UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind"),
    };

    for _ in 0..8 {
        round(&mut r, files, client, &mut peer, bytes, &udp);
    }
    let before = ALLOCS.load(Ordering::Relaxed);
    const ROUNDS: usize = 200;
    for _ in 0..ROUNDS {
        round(&mut r, files, client, &mut peer, bytes, &udp);
    }
    let grew = ALLOCS.load(Ordering::Relaxed) - before;
    assert_eq!(grew, 0, "{want:?}: {grew} allocations over {ROUNDS} rounds of eleven ops");
    for h in [file, reader] {
        let close = r.submit(Op::Close { handle: h }, None, &[]);
        assert_eq!(r.wait(close).result, IoResult::Ok(0));
    }
    let _ = std::fs::remove_file(&path);
}
