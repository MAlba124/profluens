//! The simulated reactor driven by hand, with no pipeline.

// Tests build throwaway vectors.
#![allow(clippy::disallowed_methods)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pf_sim::driver::reactor::{
    After, Break, Datagrams, Exchange, Faults, Lookup, SILENT_LOOKUP, Serve, SimReactor, Udp,
    World, pattern,
};
use pf_sim::prng::Prng;
use pf_sim::time::{MS, NONE, SEC};
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::clock::{Clock, MockClock};
use profluens_core::id::{ElementId, FormatId};
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Name, Op, OpId, OpenMode, Reactor, Submission,
};
use profluens_core::memory::Pool;
use profluens_core::time::Timestamp;

const KEY: u64 = 0xFEED;
const PEER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 80);

struct Rig {
    r: SimReactor,
    world: Arc<Mutex<World>>,
    clock: MockClock,
    pool: Pool,
    next: u64,
    seen: Vec<Completion>,
}

fn rig(faults: Faults, seed: u64, slot: usize) -> Rig {
    let mut world = World::new(faults);
    world.peer("media.test", PEER, Serve::Pattern { key: KEY, len: 1000 });
    let world = Arc::new(Mutex::new(world));
    let clock = MockClock::new();
    let r = SimReactor::new(Arc::clone(&world), Arc::new(clock.clone()), Prng::new(seed));
    Rig { r, world, clock, pool: Pool::new(slot), next: 0, seen: Vec::new() }
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
        self.r
            .submit(&mut vec![Submission { op, element: ElementId(3), kind, buf, user: 0 }], names);
        op
    }

    fn submit(&mut self, kind: Op, buf: Option<Buffer>) -> OpId {
        self.submit_named(kind, buf, &[])
    }

    fn turn(&mut self) -> Vec<Completion> {
        let mut out = Vec::new();
        self.r.run_once(&mut out);
        out.into_iter().map(|(_, c)| c).collect()
    }

    fn wait(&mut self, op: OpId) -> Completion {
        for _ in 0..10_000 {
            let done = self.turn();
            self.seen.extend(done);
            if let Some(i) = self.seen.iter().position(|c| c.op == op) {
                return self.seen.remove(i);
            }
            let due = self.r.due_in().unwrap_or_else(|| panic!("{op:?} waits with no deadline"));
            self.clock.advance(Timestamp(due.as_nanos() as u64));
        }
        panic!("{op:?} never completed");
    }

    fn connect(&mut self) -> Handle {
        let op = self.submit(Op::Connect { addr: PEER }, None);
        match self.wait(op).result {
            IoResult::Socket { handle, .. } => handle,
            other => panic!("connect gave {other:?}"),
        }
    }

    fn ask(&mut self, h: Handle, offset: u64) {
        let mut sent = 0;
        let req = offset.to_le_bytes();
        while sent < 8 {
            let op = self.submit(Op::Send { handle: h, skip: sent }, Some(self.buf(&req)));
            let IoResult::Ok(n) = self.wait(op).result else { panic!("send") };
            sent += n as u32;
        }
    }

    fn drain(&mut self, h: Handle) -> (Vec<u8>, Vec<usize>) {
        let (mut got, mut sizes) = (Vec::new(), Vec::new());
        loop {
            let op = self.submit(Op::Recv { handle: h }, Some(self.buf(&[])));
            let c = self.wait(op);
            match c.result {
                IoResult::Ok(0) => return (got, sizes),
                IoResult::Ok(n) => {
                    got.extend_from_slice(c.buf.expect("buffer").memory.data());
                    sizes.push(n);
                }
                other => panic!("recv gave {other:?}"),
            }
        }
    }
}

fn stream(from: u64, to: u64) -> Vec<u8> {
    (from..to).map(|at| pattern(KEY, at)).collect()
}

fn name(s: &str) -> Name {
    Name { off: 0, len: s.len() as u32 }
}

#[test]
fn a_stream_split_at_any_byte_is_the_same_bytes_in_order() {
    let whole = rig(Faults::NONE, 1, 64);
    drop(whole);
    let mut sizes_seen = std::collections::BTreeSet::new();
    for seed in 0..40 {
        let mut r = rig(Faults { split: true, latency: (0, 3 * MS), ..Faults::NONE }, seed, 64);
        let h = r.connect();
        r.ask(h, 100);
        let (got, sizes) = r.drain(h);
        assert_eq!(got, stream(100, 1000), "seed {seed}");
        sizes_seen.extend(sizes);
    }
    assert!(
        sizes_seen.contains(&1) && sizes_seen.len() > 30,
        "splits of many sizes, {sizes_seen:?}"
    );
    let mut r = rig(Faults::NONE, 0, 64);
    let h = r.connect();
    r.ask(h, 0);
    let (got, sizes) = r.drain(h);
    assert_eq!((got, sizes[0]), (stream(0, 1000), 64));
    assert_eq!(r.clock.now(), Timestamp::ZERO, "nothing took time");
}

#[test]
fn an_op_completes_at_its_deadline_and_the_reactor_says_when() {
    let mut r = rig(Faults { latency: (5 * MS, 5 * MS), ..Faults::NONE }, 0, 64);
    let op = r.submit(Op::Connect { addr: PEER }, None);
    assert!(r.turn().is_empty());
    assert_eq!(r.r.due_in(), Some(Duration::from_millis(5)));
    assert!(r.r.is_idle(), "nothing is due now, so a teardown does not wait");
    r.clock.advance(Timestamp(5 * MS - 1));
    assert!(r.turn().is_empty(), "one nanosecond short");
    r.clock.advance(Timestamp(1));
    assert!(
        matches!(r.turn()[..], [Completion { op: done, result: IoResult::Socket { .. }, .. }] if done == op)
    );
    assert_eq!(r.r.due_in(), None);
    let timer = r.submit(Op::Timer { after: Duration::from_millis(30) }, None);
    assert_eq!(r.r.due_in(), Some(Duration::from_millis(30)));
    r.submit(Op::Cancel { target: timer }, None);
    assert_eq!(r.turn()[0].result, IoResult::Cancelled);
}

#[test]
fn two_receives_on_one_stream_keep_their_order_whatever_latency_they_draw() {
    for seed in 0..200 {
        let mut r = rig(Faults { latency: (0, 9 * MS), split: true, ..Faults::NONE }, seed, 16);
        let h = r.connect();
        r.ask(h, 0);
        let a = r.submit(Op::Recv { handle: h }, Some(r.buf(&[])));
        let b = r.submit(Op::Recv { handle: h }, Some(r.buf(&[])));
        let mut order = Vec::new();
        let mut got = Vec::new();
        while order.len() < 2 {
            for c in r.turn() {
                order.push(c.op);
                got.extend_from_slice(c.buf.expect("buffer").memory.data());
            }
            if let Some(due) = r.r.due_in() {
                r.clock.advance(Timestamp(due.as_nanos() as u64));
            }
        }
        assert_eq!(order, [a, b], "seed {seed}");
        assert_eq!(got, stream(0, got.len() as u64), "seed {seed}");
    }
}

#[test]
fn file_reads_complete_out_of_order_and_a_written_file_reads_back() {
    let mut reversed = 0;
    for seed in 0..50 {
        let mut r = rig(Faults { latency: (0, 5 * MS), ..Faults::NONE }, seed, 8);
        r.world.lock().unwrap().file("in", (0u8..32).collect());
        let op = r.submit_named(Op::Open { path: name("in"), mode: OpenMode::Read }, None, b"in");
        let IoResult::File { handle, len: 32 } = r.wait(op).result else { panic!("open") };
        let ops: Vec<OpId> = (0..4)
            .map(|i| {
                r.submit(Op::Read { handle, offset: i * 8, at: 0, len: u32::MAX }, Some(r.buf(&[])))
            })
            .collect();
        let mut done = Vec::new();
        while done.len() < 4 {
            for c in r.turn() {
                let at = ops.iter().position(|op| *op == c.op).expect("a read");
                assert_eq!(
                    c.buf.expect("buffer").memory.data(),
                    &(at as u8 * 8..at as u8 * 8 + 8).collect::<Vec<_>>()[..]
                );
                done.push(at);
            }
            if let Some(due) = r.r.due_in() {
                r.clock.advance(Timestamp(due.as_nanos() as u64));
            }
        }
        reversed += (done != [0, 1, 2, 3]) as u32;
    }
    assert!(reversed > 10, "positioned reads complete in any order, {reversed} of 50 did");

    let mut r = rig(Faults::NONE, 0, 8);
    let op = r.submit_named(Op::Open { path: name("out"), mode: OpenMode::Create }, None, b"out");
    let IoResult::File { handle, len: 0 } = r.wait(op).result else { panic!("create") };
    let late = r.submit(Op::Write { handle, offset: 4 }, Some(r.buf(b"5678")));
    let early = r.submit(Op::Write { handle, offset: 0 }, Some(r.buf(b"1234")));
    assert_eq!((r.wait(late).result, r.wait(early).result), (IoResult::Ok(4), IoResult::Ok(4)));
    assert_eq!(r.world.lock().unwrap().contents("out"), Some(&b"12345678"[..]));
    let op = r.submit_named(
        Op::Rename { from: Name { off: 0, len: 3 }, to: Name { off: 3, len: 2 } },
        None,
        b"outto",
    );
    assert_eq!(r.wait(op).result, IoResult::Ok(0));
    let op = r.submit_named(Op::Stat { path: name("to") }, None, b"to");
    assert_eq!(r.wait(op).result, IoResult::File { handle: Handle::NONE, len: 8 });
    let op = r.submit_named(Op::Open { path: name("out"), mode: OpenMode::Read }, None, b"out");
    assert_eq!(r.wait(op).result, IoResult::Err(IoError::NotFound));
    let op = r.submit_named(Op::Unlink { path: name("to") }, None, b"to");
    assert_eq!(r.wait(op).result, IoResult::Ok(0));
    // The handle outlives the name, as an open file outlives its unlink.
    let read = r.submit(Op::Read { handle, offset: 0, at: 0, len: u32::MAX }, Some(r.buf(&[])));
    assert_eq!(r.wait(read).buf.expect("buffer").memory.data(), b"12345678");
    let range = r.submit(Op::Read { handle, offset: 1, at: 2, len: 3 }, Some(r.buf(&[])));
    let got = r.wait(range);
    assert_eq!(got.result, IoResult::Ok(3));
    assert_eq!(
        &got.buf.expect("buffer").memory.data()[2..],
        b"234",
        "a range lands at its place in the window"
    );
    let close = r.submit(Op::Close { handle }, None);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    let read = r.submit(Op::Read { handle, offset: 0, at: 0, len: u32::MAX }, Some(r.buf(&[])));
    assert_eq!(r.wait(read).result, IoResult::Err(IoError::BadHandle));
}

#[test]
fn connects_are_refused_and_time_out_within_their_budget() {
    let faults = Faults { refuse: 2, time_out: 1, connect_timeout: 3 * SEC, ..Faults::NONE };
    let mut r = rig(faults, 0, 8);
    let mut results = Vec::new();
    for _ in 0..4 {
        let op = r.submit(Op::Connect { addr: PEER }, None);
        results.push(r.wait(op).result);
    }
    let refused = IoResult::Err(IoError::Refused);
    assert_eq!(results[..3], [refused, refused, IoResult::Err(IoError::TimedOut)]);
    assert!(matches!(results[3], IoResult::Socket { .. }), "the budget is spent");
    assert_eq!(r.clock.now(), Timestamp(3 * SEC), "the timeout took its time");
    let nobody = SocketAddr::from(([10, 0, 0, 9], 80));
    let op = r.submit(Op::Connect { addr: nobody }, None);
    assert_eq!(r.wait(op).result, refused, "nobody listens there");
    let w = r.world.lock().unwrap();
    assert_eq!((w.connects, w.failures), (1, 4));
}

fn echo() -> Serve {
    let mut held: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
    Serve::Datagram(Box::new(move |d: &mut Datagrams<'_>| {
        if let Some((from, data)) = d.input {
            d.out.push((from, data.to_vec()));
            held.push((from, data.to_vec()));
            d.wake = d.now + 30 * MS;
        } else {
            d.out.append(&mut held);
            d.wake = NONE;
        }
    }))
}

const UDP_PEER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 443);

#[test]
fn datagrams_reach_a_peer_and_come_back_on_the_wire() {
    let mut r = rig(Faults::NONE, 3, 2048);
    {
        let mut w = r.world.lock().unwrap();
        w.peer("media.test", UDP_PEER, echo());
        w.udp = Udp { delay: (10 * MS, 10 * MS), ..Udp::NONE };
    }
    let any = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
    let op = r.submit(Op::Bind { addr: any }, None);
    let IoResult::Socket { handle, addr } = r.wait(op).result else { panic!("bound") };
    assert_eq!(addr.port(), 40_000, "the world's first ephemeral port");
    let op = r.submit(Op::Peer { handle, addr: UDP_PEER }, None);
    assert_eq!(r.wait(op).result, IoResult::Ok(0));
    let recv = r.submit(Op::Recv { handle }, Some(r.buf(&[])));
    let send = r.submit(Op::Send { handle, skip: 0 }, Some(r.buf(b"ping")));
    assert_eq!(r.wait(send).result, IoResult::Ok(4));
    let c = r.wait(recv);
    assert_eq!((c.result, c.buf.unwrap().memory.data()), (IoResult::Ok(4), &b"ping"[..]));
    assert_eq!(r.clock.now(), Timestamp(20 * MS), "there and back");
    let recv = r.submit(Op::Recv { handle }, Some(r.buf(&[])));
    assert_eq!(r.wait(recv).result, IoResult::Ok(4));
    assert_eq!(r.clock.now(), Timestamp(50 * MS), "10 ms there, 30 of wake, 10 back");
    r.world.lock().unwrap().udp.black_hole = true;
    let send = r.submit(Op::Send { handle, skip: 0 }, Some(r.buf(b"lost")));
    assert_eq!(r.wait(send).result, IoResult::Ok(4), "a send of UDP does not know");
    let recv = r.submit(Op::Recv { handle }, Some(r.buf(&[])));
    assert!(r.r.due_in().is_none(), "nothing will come");
    r.submit(Op::Cancel { target: recv }, None);
    assert_eq!(r.wait(recv).result, IoResult::Cancelled);
    assert_eq!(r.world.lock().unwrap().udp_lost, 1);
}

#[test]
fn udp_loss_and_duplication_are_the_seed() {
    let run = |seed| {
        let mut r = rig(Faults::NONE, seed, 2048);
        {
            let mut w = r.world.lock().unwrap();
            w.peer("media.test", UDP_PEER, echo());
            w.udp = Udp { loss: 20, dup: 20, delay: (MS, 40 * MS), black_hole: false };
        }
        let any = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
        let op = r.submit(Op::Bind { addr: any }, None);
        let IoResult::Socket { handle, .. } = r.wait(op).result else { panic!("bound") };
        let op = r.submit(Op::Peer { handle, addr: UDP_PEER }, None);
        r.wait(op);
        for i in 0..50u8 {
            let send = r.submit(Op::Send { handle, skip: 0 }, Some(r.buf(&[i])));
            r.wait(send);
        }
        let mut got = Vec::new();
        loop {
            let recv = r.submit(Op::Recv { handle }, Some(r.buf(&[])));
            if r.r.due_in().is_none() {
                break;
            }
            got.push(r.wait(recv).buf.unwrap().memory.data()[0]);
        }
        let w = r.world.lock().unwrap();
        (got, w.udp_lost, w.udp_duplicated)
    };
    let (got, lost, dup) = run(7);
    assert_eq!(run(7), (got.clone(), lost, dup), "a seed replays");
    assert!(lost > 0 && dup > 0, "{lost} lost, {dup} duplicated");
    assert!(got.windows(2).any(|w| w[0] > w[1]), "a wide delay reorders");
    assert_ne!(run(8).0, got);
}

#[test]
fn a_record_lookup_is_answered_from_the_world() {
    let mut r = rig(Faults { latency: (5 * MS, 5 * MS), ..Faults::NONE }, 0, 512);
    {
        let mut w = r.world.lock().unwrap();
        w.record("media.test", 65, Lookup::Message(vec![1, 2, 3]));
        w.record("quiet.test", 65, Lookup::Silent);
        w.record("none.test", 65, Lookup::NoRecord);
    }
    let mut ask = |host: &str, kind| {
        let op = r.submit_named(
            Op::Lookup { host: name(host), kind },
            Some(r.buf(&[])),
            host.as_bytes(),
        );
        let c = r.wait(op);
        (c.result, c.buf.map(|b| b.memory.data().to_vec()), r.clock.now())
    };
    assert_eq!(
        ask("media.test", 65),
        (IoResult::Record(3), Some(vec![1, 2, 3]), Timestamp(5 * MS))
    );
    assert_eq!(ask("media.test", 28).0, IoResult::Err(IoError::NoRecord), "another type has none");
    assert_eq!(ask("none.test", 65).0, IoResult::Err(IoError::NoRecord));
    assert_eq!(ask("other.test", 65).0, IoResult::Err(IoError::NoRecord));
    let (result, _, at) = ask("quiet.test", 65);
    assert_eq!(
        (result, at),
        (IoResult::Err(IoError::NoRecord), Timestamp(20 * MS + SILENT_LOOKUP)),
        "a silent server times out"
    );
}

#[test]
fn a_name_resolves_to_its_peer_and_a_lookup_fails_within_its_budget() {
    let mut r = rig(Faults { dns_fail: 1, latency: (20 * MS, 20 * MS), ..Faults::NONE }, 0, 8);
    let ask = |r: &mut Rig, host: &str, skip| {
        let op = r.submit_named(
            Op::Resolve { host: name(host), port: 8080, skip },
            None,
            host.as_bytes(),
        );
        r.wait(op).result
    };
    assert_eq!(ask(&mut r, "media.test", 0), IoResult::Err(IoError::NoAddress), "the fault");
    assert_eq!(ask(&mut r, "media.test", 0), IoResult::Addr(SocketAddr::new(PEER.ip(), 8080)));
    assert_eq!(ask(&mut r, "media.test", 1), IoResult::Err(IoError::NoAddress), "one address");
    assert_eq!(ask(&mut r, "other.test", 0), IoResult::Err(IoError::NoAddress));
    assert_eq!(r.clock.now(), Timestamp(80 * MS), "a slow answer each time");
}

#[test]
fn a_connection_is_reset_mid_stream_once() {
    let faults = Faults { resets: 1, reset_within: 500, split: true, ..Faults::NONE };
    for seed in 0..30 {
        let mut r = rig(faults, seed, 64);
        let h = r.connect();
        r.ask(h, 0);
        let mut got = Vec::new();
        let reset = loop {
            let op = r.submit(Op::Recv { handle: h }, Some(r.buf(&[])));
            let c = r.wait(op);
            match c.result {
                IoResult::Ok(_) => got.extend_from_slice(c.buf.expect("buffer").memory.data()),
                other => break other,
            }
        };
        assert_eq!(reset, IoResult::Err(IoError::Reset), "seed {seed}");
        assert!(got.len() < 500 && got == stream(0, got.len() as u64), "seed {seed}");
        let op = r.submit(Op::Send { handle: h, skip: 0 }, Some(r.buf(b"x")));
        assert_eq!(r.wait(op).result, IoResult::Err(IoError::Reset));
        let h = r.connect();
        r.ask(h, got.len() as u64);
        let (rest, _) = r.drain(h);
        got.extend(rest);
        assert_eq!(got, stream(0, 1000), "seed {seed}");
    }
}

#[test]
fn a_peer_stalls_for_a_while_or_for_good() {
    let faults = Faults { stalls: 1, stall_within: 200, stall_for: 7 * SEC, ..Faults::NONE };
    let mut r = rig(faults, 3, 64);
    let h = r.connect();
    r.ask(h, 0);
    let (got, _) = r.drain(h);
    assert_eq!(got, stream(0, 1000));
    assert_eq!(r.clock.now(), Timestamp(7 * SEC), "one receive was held");

    let mut r = rig(Faults { stall_for: NONE, ..faults }, 3, 64);
    let h = r.connect();
    r.ask(h, 0);
    let mut got = Vec::new();
    let stuck = loop {
        let op = r.submit(Op::Recv { handle: h }, Some(r.buf(&[])));
        if r.turn()
            .into_iter()
            .map(|c| got.extend_from_slice(c.buf.expect("buffer").memory.data()))
            .count()
            == 0
        {
            break op;
        }
    };
    assert_eq!(r.r.due_in(), None, "nothing will complete it");
    r.submit(Op::Cancel { target: stuck }, None);
    let c = r.turn().pop().expect("the cancel completes it");
    assert_eq!((c.op, c.result), (stuck, IoResult::Cancelled));
    assert!(c.buf.is_some_and(|b| b.memory.is_empty()), "a cancelled receive gives no bytes");
    let (rest, _) = r.drain(h);
    got.extend(rest);
    assert_eq!(got, stream(0, 1000));
}

#[test]
fn a_short_write_takes_part_and_the_peer_sees_the_whole_request_once() {
    let mut shorts = 0;
    for seed in 0..40 {
        let mut r = rig(Faults { short_writes: true, ..Faults::NONE }, seed, 64);
        let h = r.connect();
        let req = 250u64.to_le_bytes();
        let mut sent = 0u32;
        while sent < 8 {
            let op = r.submit(Op::Send { handle: h, skip: sent }, Some(r.buf(&req)));
            let IoResult::Ok(n) = r.wait(op).result else { panic!("send") };
            shorts += (n < 8 - sent as usize) as u32;
            sent += n as u32;
        }
        assert_eq!(r.drain(h).0, stream(250, 1000), "seed {seed}");
    }
    assert!(shorts > 20, "sends came up short, {shorts}");
}

#[test]
fn a_custom_peer_answers_what_it_is_sent() {
    let mut world = World::new(Faults { split: true, ..Faults::NONE });
    let mut lines = 0;
    let serve = move |x: &mut Exchange<'_>| {
        while let Some(end) = x.input[x.consumed..].iter().position(|b| *b == b'\n') {
            let line = &x.input[x.consumed..x.consumed + end];
            x.out.extend(line.iter().rev());
            x.out.push(b'\n');
            x.consumed += end + 1;
            lines += 1;
        }
        x.after = if lines >= 3 { After::Close } else { After::Open };
    };
    world.peer("echo.test", PEER, Serve::Custom(Box::new(serve)));
    let world = Arc::new(Mutex::new(world));
    let clock = MockClock::new();
    let r = SimReactor::new(Arc::clone(&world), Arc::new(clock.clone()), Prng::new(7));
    let mut r = Rig { r, world, clock, pool: Pool::new(64), next: 0, seen: Vec::new() };
    let h = r.connect();
    for part in [&b"ab"[..], b"c\nde", b"f\n", b"ghi\n"] {
        let op = r.submit(Op::Send { handle: h, skip: 0 }, Some(r.buf(part)));
        assert_eq!(r.wait(op).result, IoResult::Ok(part.len()));
    }
    assert_eq!(r.drain(h).0, b"cba\nfed\nihg\n");
}

#[test]
fn a_link_with_a_rate_carries_it_and_two_connections_share_it() {
    // 400 octets a second, so the peer's 1000 take two and a half seconds.
    let mut r = rig(Faults::at_rate(400), 1, 4096);
    let h = r.connect();
    r.ask(h, 0);
    let (got, sizes) = r.drain(h);
    assert_eq!(got, stream(0, 1000));
    assert!(sizes.iter().all(|n| *n <= 100), "a quarter of a second at most, {sizes:?}");
    assert_eq!(r.clock.now(), Timestamp(2500 * MS));

    let mut r = rig(Faults::at_rate(400), 1, 4096);
    let (a, b) = (r.connect(), r.connect());
    r.ask(a, 0);
    r.ask(b, 0);
    let (mut left, mut ends) = ([1000usize; 2], [0u64; 2]);
    while left.iter().any(|n| *n > 0) {
        for (i, h) in [a, b].into_iter().enumerate() {
            if left[i] == 0 {
                continue;
            }
            let op = r.submit(Op::Recv { handle: h }, Some(r.buf(&[])));
            let IoResult::Ok(n) = r.wait(op).result else { panic!("recv") };
            left[i] -= n;
            ends[i] = r.clock.now().0;
        }
    }
    assert_eq!(ends[1], 5000 * MS);
    assert!(ends[0] >= 4500 * MS, "the first ended at {} ms", ends[0] / MS);

    let stepped = Faults { rate: [(0, 100), (SEC, 900), (NONE, 0), (NONE, 0)], ..Faults::NONE };
    let mut r = rig(stepped, 1, 4096);
    let h = r.connect();
    r.ask(h, 0);
    assert_eq!(r.drain(h).0, stream(0, 1000));
    assert_eq!(r.clock.now(), Timestamp(2 * SEC));
    assert_eq!(
        (
            stepped.rate_at(0),
            stepped.rate_at(SEC - 1),
            stepped.rate_at(SEC),
            Faults::NONE.rate_at(SEC)
        ),
        (100, 100, 900, 0)
    );
}

#[test]
fn a_seed_replays() {
    let faults =
        Faults { latency: (0, 4 * MS), split: true, resets: 1, reset_within: 800, ..Faults::NONE };
    let trace = |seed| {
        let mut r = rig(faults, seed, 32);
        let h = r.connect();
        r.ask(h, 0);
        let mut trace = Vec::new();
        loop {
            let op = r.submit(Op::Recv { handle: h }, Some(r.buf(&[])));
            let c = r.wait(op);
            trace.push((r.clock.now(), c.result));
            if !matches!(c.result, IoResult::Ok(n) if n > 0) {
                return trace;
            }
        }
    };
    assert_eq!(trace(11), trace(11));
    assert_ne!(trace(11), trace(12));
}

#[test]
fn the_breakages_break_the_stream() {
    let run = |breakage| {
        (0..200)
            .filter(|seed| {
                let mut r = rig(Faults { split: true, ..Faults::NONE }, *seed, 32);
                r.world.lock().unwrap().breakage = breakage;
                let h = r.connect();
                r.ask(h, 0);
                let mut got = Vec::new();
                for _ in 0..2000 {
                    let _ = r.submit(Op::Recv { handle: h }, Some(r.buf(&[])));
                    match r.turn().pop() {
                        Some(Completion { result: IoResult::Ok(0), .. }) => break,
                        Some(c) => got.extend_from_slice(c.buf.expect("buffer").memory.data()),
                        None => return true,
                    }
                }
                got != stream(0, 1000)
            })
            .count()
    };
    assert_eq!(run(Break::None), 0);
    for breakage in [Break::Duplicate, Break::Skip, Break::Lose] {
        assert!(run(breakage) > 150, "{breakage:?} shows in most runs");
    }
}

fn echoing(r: &mut Rig) -> Handle {
    {
        let mut w = r.world.lock().unwrap();
        w.peer("media.test", UDP_PEER, echo());
        w.udp = Udp { delay: (10 * MS, 10 * MS), ..Udp::NONE };
    }
    let any = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
    let op = r.submit(Op::Bind { addr: any }, None);
    let IoResult::Socket { handle, .. } = r.wait(op).result else { panic!("bound") };
    let op = r.submit(Op::Peer { handle, addr: UDP_PEER }, None);
    assert_eq!(r.wait(op).result, IoResult::Ok(0));
    handle
}

#[test]
fn lent_buffers_take_datagrams_and_the_last_one_filled_ends_the_receive_dry() {
    let mut r = rig(Faults::NONE, 3, 2048);
    let handle = echoing(&mut r);
    let lends: Vec<OpId> =
        (0..2).map(|_| r.submit(Op::Lend { handle }, Some(r.buf(&[])))).collect();
    let many = r.submit(Op::RecvMany { handle }, None);
    let send = r.submit(Op::SendMany { handle, addr: None, segment: 1 }, Some(r.buf(b"abc")));
    assert_eq!(r.wait(send).result, IoResult::Ok(3), "one send of three datagrams");
    for (op, want) in lends.into_iter().zip([b"a", b"b"]) {
        let c = r.wait(op);
        assert_eq!(c.result, IoResult::Datagram { len: 1, from: UDP_PEER, segment: 0 });
        assert_eq!(c.buf.unwrap().memory.data(), want);
    }
    assert_eq!(r.wait(many).result, IoResult::Err(IoError::Dry));
    let lend = r.submit(Op::Lend { handle }, Some(r.buf(&[])));
    let many = r.submit(Op::RecvMany { handle }, None);
    assert_eq!(r.wait(lend).buf.unwrap().memory.data(), b"c");
    assert_eq!(r.wait(many).result, IoResult::Err(IoError::Dry));
    let many = r.submit(Op::RecvMany { handle }, None);
    assert_eq!(r.wait(many).result, IoResult::Err(IoError::Dry), "nothing lent");
}

#[test]
fn a_cancel_or_a_close_gives_back_every_lent_buffer_once() {
    let mut r = rig(Faults::NONE, 5, 2048);
    let handle = echoing(&mut r);
    let lends: Vec<OpId> =
        (0..4).map(|_| r.submit(Op::Lend { handle }, Some(r.buf(&[])))).collect();
    let many = r.submit(Op::RecvMany { handle }, None);
    r.submit(Op::Cancel { target: lends[0] }, None);
    r.submit(Op::Cancel { target: many }, None);
    assert_eq!(r.wait(many).result, IoResult::Cancelled);
    for op in lends {
        let c = r.wait(op);
        assert_eq!(c.result, IoResult::Cancelled);
        assert!(c.buf.is_some());
    }
    let lend = r.submit(Op::Lend { handle }, Some(r.buf(&[])));
    let many = r.submit(Op::RecvMany { handle }, None);
    let close = r.submit(Op::Close { handle }, None);
    assert_eq!(r.wait(close).result, IoResult::Ok(0));
    assert_eq!(r.wait(lend).result, IoResult::Cancelled);
    assert_eq!(r.wait(many).result, IoResult::Cancelled);
    assert!(r.turn().is_empty() && r.seen.is_empty(), "nothing completes twice");
}

#[test]
fn under_loss_and_duplication_every_lent_buffer_comes_back_once() {
    for seed in 0..64 {
        let mut r = rig(Faults::NONE, seed, 2048);
        let handle = echoing(&mut r);
        r.world.lock().unwrap().udp =
            Udp { loss: 25, dup: 25, delay: (MS, 30 * MS), black_hole: false };
        let mut lent: Vec<OpId> =
            (0..6).map(|_| r.submit(Op::Lend { handle }, Some(r.buf(&[])))).collect();
        let mut many = r.submit(Op::RecvMany { handle }, None);
        for i in 0..20u8 {
            let send = r.submit(Op::Send { handle, skip: 0 }, Some(r.buf(&[i])));
            r.wait(send);
        }
        let mut done = std::collections::HashSet::new();
        let mut filled = 0;
        for _ in 0..200 {
            for c in r.turn().into_iter().chain(std::mem::take(&mut r.seen)) {
                assert!(done.insert(c.op), "seed {seed}: {:?} completed twice", c.op);
                if let IoResult::Datagram { .. } = c.result {
                    filled += 1;
                    lent.retain(|op| *op != c.op);
                    lent.push(r.submit(Op::Lend { handle }, Some(r.buf(&[]))));
                }
                if c.op == many {
                    many = r.submit(Op::RecvMany { handle }, None);
                }
            }
            let Some(due) = r.r.due_in() else { break };
            r.clock.advance(Timestamp(due.as_nanos() as u64));
        }
        assert!(filled > 0, "seed {seed}");
        let close = r.submit(Op::Close { handle }, None);
        for op in lent.iter().copied().chain([close, many]) {
            if !done.contains(&op) {
                r.wait(op);
            }
        }
    }
}
