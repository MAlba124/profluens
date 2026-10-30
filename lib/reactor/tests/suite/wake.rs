//! The threaded driver's park against a backend. The including file gives `factory()`.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::io::{Handle, IoResult};
use profluens_core::pipeline::Pipeline;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static PADS: [PadDesc; 1] = [PadDesc { name: "pad", offers: &OFFERS }];

static SRC: ElementDesc = ElementDesc {
    name: "probe",
    sinks: &[],
    srcs: &PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: true,
    couplings: &[],
};

static SINK: ElementDesc = ElementDesc {
    name: "drain",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

const TIMER: u64 = 1;

struct Probe {
    want: usize,
    got: usize,
    sock: Handle,
    waiting: bool,
    said: Sender<Said>,
    armed: Instant,
}

enum Said {
    Bound(SocketAddr),
    Timer(Duration),
}

impl Element for Probe {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.io().bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), 0);
        ctx.io().timer(Duration::from_millis(35), TIMER);
        self.armed = Instant::now();
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(c) = ctx.io().next_completion() {
            match c.result {
                IoResult::Socket { handle, addr } => {
                    self.sock = handle;
                    let _ = self.said.send(Said::Bound(addr));
                }
                IoResult::Ok(_) if c.user == TIMER => {
                    let _ = self.said.send(Said::Timer(self.armed.elapsed()));
                }
                IoResult::Ok(_) => {
                    self.waiting = false;
                    self.got += 1;
                    if let Some(buf) = c.buf {
                        ctx.out(SrcPad(0)).push(buf);
                    }
                }
                other => panic!("the probe got {other:?}"),
            }
        }
        if self.got == self.want {
            return Ok(Flow::Eos);
        }
        if !self.sock.is_none()
            && !self.waiting
            && let Some(buf) = ctx.try_alloc()
        {
            ctx.io().recv(self.sock, buf, 0);
            self.waiting = true;
        }
        Ok(Flow::Ok)
    }
}

struct Drain(Sender<Vec<u8>>);

impl Element for Drain {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let _ = self.0.send(buf.memory.data().to_vec());
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn a_group_waiting_on_a_socket_and_a_timer_is_woken_and_never_late() {
    const N: usize = 8;
    let (said_tx, said) = channel();
    let (data_tx, data) = channel();
    let mut p = Pipeline::new();
    p.set_reactor_factory(super::factory());
    let probe = Probe {
        want: N,
        got: 0,
        sock: Handle::NONE,
        waiting: false,
        said: said_tx,
        armed: Instant::now(),
    };
    let src = p.add(probe);
    let sink = p.add(Drain(data_tx));
    p.link(src.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("link");
    let taps = p.tap_handle();
    let run = std::thread::spawn(move || p.run());

    let patience = Duration::from_secs(10);
    let Said::Bound(addr) = said.recv_timeout(patience).expect("the bind completes") else {
        panic!("the bind comes first");
    };
    let Said::Timer(after) = said.recv_timeout(patience).expect("the timer completes") else {
        panic!("then the timer");
    };
    assert!(after >= Duration::from_millis(35), "the timer fired early, after {after:?}");
    assert!(after < Duration::from_millis(250), "the timer fired late, after {after:?}");

    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    for i in 0..N {
        std::thread::sleep(Duration::from_millis(30));
        sender.send_to(&[i as u8; 3], addr).expect("send");
    }
    for i in 0..N {
        assert_eq!(data.recv_timeout(patience).expect("a datagram"), [i as u8; 3]);
    }
    run.join().expect("the run").expect("ends on its own");
    let stats = taps.scheduler();
    assert_eq!(stats.late_wakes, 0, "the completion ends the park, {stats:?}");
    assert!(stats.parks >= N as u64, "the group slept between datagrams, {stats:?}");
}

static GATE_SINK: ElementDesc = ElementDesc {
    name: "gate",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Filler {
    fill: usize,
    made: usize,
    want: usize,
    got: usize,
    sock: Handle,
    waiting: bool,
    said: Sender<SocketAddr>,
}

impl Element for Filler {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.io().bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), 0);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(c) = ctx.io().next_completion() {
            match c.result {
                IoResult::Socket { handle, addr } => {
                    self.sock = handle;
                    let _ = self.said.send(addr);
                }
                IoResult::Ok(_) => {
                    self.waiting = false;
                    self.got += 1;
                }
                other => panic!("the filler got {other:?}"),
            }
        }
        if self.made < self.fill
            && let Some(buf) = ctx.try_alloc()
        {
            ctx.out(SrcPad(0)).push(buf);
            self.made += 1;
        }
        if self.got == self.want && self.made == self.fill {
            return Ok(Flow::Eos);
        }
        if self.sock.is_none() || self.waiting || self.got == self.want {
            return Ok(Flow::Ok);
        }
        if let Some(buf) = ctx.try_alloc() {
            ctx.io().recv(self.sock, buf, 0);
            self.waiting = true;
        }
        Ok(Flow::Ok)
    }
}

struct Gate(Arc<AtomicBool>);

impl Element for Gate {
    fn desc(&self) -> &'static ElementDesc {
        &GATE_SINK
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.0.load(Ordering::Acquire) {
            ctx.wake_within(Duration::from_millis(1));
            return Ok(Flow::Ok);
        }
        while inputs.pop().is_some() {}
        Ok(Flow::Ok)
    }
}

#[test]
fn a_group_resting_on_a_full_ring_sleeps_through_a_completion() {
    const WANT: usize = 3;
    let (said_tx, said) = channel();
    let open = Arc::new(AtomicBool::new(false));
    let mut p = Pipeline::new();
    p.set_reactor_factory(super::factory());
    p.set_pool(64, 256);
    let filler = Filler {
        fill: 64,
        made: 0,
        want: WANT,
        got: 0,
        sock: Handle::NONE,
        waiting: false,
        said: said_tx,
    };
    let src = p.add(filler);
    let sink = p.add(Gate(Arc::clone(&open)));
    p.link(src.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("link");
    p.set_queue_capacity(sink, 2);
    let taps = p.tap_handle();
    let run = std::thread::spawn(move || p.run());

    let addr = said.recv_timeout(Duration::from_secs(10)).expect("the bind completes");
    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    std::thread::sleep(Duration::from_millis(30));
    sender.send_to(&[1; 3], addr).expect("send");
    std::thread::sleep(Duration::from_millis(60));
    open.store(true, Ordering::Release);
    for _ in 1..WANT {
        sender.send_to(&[2; 3], addr).expect("send");
    }
    run.join().expect("the run").expect("ends on its own");
    let stats = taps.scheduler();
    assert!(stats.empty_wakes <= 2, "the group spun on what it did not reap, {stats:?}");
}
