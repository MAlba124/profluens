//! `filesrc` serving pulled ranges, over an out-of-order splitting reactor and the OS reactor.

// A test's own fixtures, reactor and threads, not an element.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use profluens_core::batch::Inputs;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad};
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Op, Reactor, ReactorFactory, Submission,
};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::pull::Ask;
use profluens_core::sched::Outcome;
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ONE_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static PULLER: ElementDesc = ElementDesc {
    name: "puller",
    sinks: &ONE_IN,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
const IN: SinkPad = SinkPad(0);

// SplitMix64 (Steele, Lea, Flood, OOPSLA 2014).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn ramp(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Got {
    Reset(u64),
    Range { offset: u64, want: u32, bytes: Vec<u8> },
    Pushed(usize),
}

struct Puller {
    chunk: u32,
    end: u64,
    take: usize,
    next: u64,
    epoch: u64,
    seq: u64,
    asked: HashMap<u64, (u64, u32)>,
    over: bool,
    got: Arc<Mutex<Vec<Got>>>,
}

impl Puller {
    fn new(chunk: u32, end: u64, take: usize, got: Arc<Mutex<Vec<Got>>>) -> Puller {
        let asked = HashMap::new();
        Puller { chunk, end, take, next: 0, epoch: 0, seq: 0, asked, over: false, got }
    }
}

impl Element for Puller {
    fn desc(&self) -> &'static ElementDesc {
        &PULLER
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.want_ranges(IN);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(from) = ctx.pull_reset(IN) {
            (self.next, self.over, self.epoch) = (from.to_byte, false, self.epoch + 1);
            self.asked.clear();
            self.got.lock().unwrap().push(Got::Reset(from.to_byte));
        }
        if !ctx.pulls(IN) {
            while let Some(buf) = inputs.pop() {
                self.got.lock().unwrap().push(Got::Pushed(buf.memory.len()));
            }
            return Ok(Flow::Ok);
        }
        for _ in 0..self.take {
            let Some((tag, buf)) = ctx.next_served(IN, &mut inputs) else {
                break;
            };
            assert_eq!(
                tag >> 32,
                self.epoch,
                "a range asked before a seek is not served behind it"
            );
            let (offset, want) = self.asked.remove(&tag).expect("a served tag was asked");
            self.got.lock().unwrap().push(Got::Range {
                offset,
                want,
                bytes: buf.memory.data().to_vec(),
            });
        }
        while self.next < self.end && ctx.asks_left(IN) > 0 {
            let Ok(Some(buf)) = ctx.try_alloc_sized(self.chunk as usize) else {
                break;
            };
            let tag = (self.epoch << 32) | self.seq;
            self.seq += 1;
            self.asked.insert(tag, (self.next, self.chunk));
            ctx.ask(IN, Ask { offset: self.next, len: self.chunk, tag, buf })
                .expect("credits left");
            self.next += self.chunk as u64;
        }
        if self.next >= self.end && !self.over {
            self.over = true;
            ctx.end_asks(IN);
        }
        Ok(Flow::Ok)
    }
}

/// An in-memory reactor that completes, splits and fails reads by seed.
struct Mem {
    data: Arc<Vec<u8>>,
    held: Vec<Submission>,
    done: Vec<(ElementId, Completion)>,
    rng: Rng,
    split: bool,
    fail_at: Option<u64>,
}

impl Reactor for Mem {
    fn submit(&mut self, subs: &mut Vec<Submission>, _names: &[u8]) {
        for s in subs.drain(..) {
            let result = match s.kind {
                Op::Open { .. } => {
                    IoResult::File { handle: Handle::new(1, 1), len: self.data.len() as u64 }
                }
                Op::Read { .. } => {
                    self.held.push(s);
                    continue;
                }
                Op::Cancel { .. } => continue,
                _ => IoResult::Ok(0),
            };
            self.done.push((s.element, Completion { op: s.op, user: s.user, result, buf: s.buf }));
        }
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        out.clear();
        out.append(&mut self.done);
        let mut n = self.rng.below(self.held.len() as u64 + 1);
        while n > 0 && !self.held.is_empty() {
            n -= 1;
            let pick = self.rng.below(self.held.len() as u64) as usize;
            let mut s = self.held.swap_remove(pick);
            let Op::Read { offset, at, len, .. } = s.kind else { unreachable!() };
            let mut buf = s.buf.take().expect("a read has a buffer");
            let full = buf.memory.as_mut_full();
            let from = (offset as usize).min(self.data.len());
            let mut got = (len as usize).min(full.len() - at as usize).min(self.data.len() - from);
            if self.split && got > 1 {
                got = 1 + self.rng.below(got as u64) as usize;
            }
            let result = match self.fail_at {
                Some(bad) if (offset..offset + got as u64 + 1).contains(&bad) => {
                    IoResult::Err(IoError::Os(5))
                }
                _ => {
                    full[at as usize..at as usize + got]
                        .copy_from_slice(&self.data[from..from + got]);
                    buf.memory.set_len(at as usize + got);
                    IoResult::Ok(got)
                }
            };
            out.push((s.element, Completion { op: s.op, user: s.user, result, buf: Some(buf) }));
        }
    }

    fn due_in(&self) -> Option<Duration> {
        (!self.held.is_empty()).then_some(Duration::ZERO)
    }

    fn is_idle(&self) -> bool {
        self.held.is_empty() && self.done.is_empty()
    }
}

struct Plan {
    len: usize,
    chunk: u32,
    end: u64,
    take: usize,
    split: bool,
    fail_at: Option<u64>,
    seek_after: Option<(usize, u64)>,
}

const PLAN: Plan = Plan {
    len: 50_000,
    chunk: 3_000,
    end: 51_000,
    take: usize::MAX,
    split: true,
    fail_at: None,
    seek_after: None,
};

fn stepped(plan: &Plan, seed: u64) -> (Result<(), Error>, Vec<Got>) {
    let data = Arc::new(ramp(plan.len));
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    let (split, fail_at) = (plan.split, plan.fail_at);
    let world = Arc::clone(&data);
    let factory: ReactorFactory = Arc::new(move || {
        let held = Vec::new();
        let rng = Rng(seed);
        Ok(Box::new(Mem { data: Arc::clone(&world), held, done: Vec::new(), rng, split, fail_at })
            as Box<dyn Reactor>)
    });
    p.set_reactor_factory(factory);
    let src = p.add(FileSrc::new("in-memory"));
    let got = Arc::new(Mutex::new(Vec::new()));
    let puller = p.add(Puller::new(plan.chunk, plan.end, plan.take, Arc::clone(&got)));
    p.set_element_pool_bytes(puller, 4096, 16, 1 << 20);
    p.set_element_credits(puller, 6);
    p.set_element_credits(src, 4);
    p.link(src.src(FileSrc::OUT), puller.sink(IN)).expect("link");
    let control = p.control();
    let mut rng = Rng(seed ^ 0xfeed);
    let mut sought = false;
    let result = p.step(Arc::clone(&shared)).and_then(|mut run| {
        for _ in 0..200_000 {
            if let (Some((after, to)), false) = (plan.seek_after, sought)
                && got.lock().unwrap().iter().filter(|g| matches!(g, Got::Range { .. })).count()
                    >= after
            {
                let _ = control.seek_bytes(
                    Timestamp::ZERO,
                    &[(profluens_core::command::ALL, to, Timestamp::ZERO)],
                );
                sought = true;
            }
            let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
            if ready.is_empty() {
                match run.next_deadline() {
                    Some(at) if at > clock.now() => clock.advance(Timestamp(at.0 - clock.now().0)),
                    Some(_) => {}
                    None => break,
                }
                continue;
            }
            let g = ready[rng.below(ready.len() as u64) as usize];
            if let Outcome::Exit = run.pass(g)? {
                continue;
            }
        }
        run.finish().map(drop)
    });
    let got = got.lock().unwrap().clone();
    (result, got)
}

fn after_reset(got: &[Got]) -> (u64, Vec<(u64, u32, Vec<u8>)>) {
    let at = got.iter().rposition(|g| matches!(g, Got::Reset(_))).expect("a reset");
    let Got::Reset(from) = got[at] else { unreachable!() };
    let mut ranges: Vec<_> = got[at + 1..]
        .iter()
        .filter_map(|g| {
            if let Got::Range { offset, want, bytes } = g {
                Some((*offset, *want, bytes.clone()))
            } else {
                None
            }
        })
        .collect();
    ranges.sort_by_key(|r| r.0);
    (from, ranges)
}

fn assert_whole(data: &[u8], plan: &Plan, from: u64, ranges: &[(u64, u32, Vec<u8>)]) {
    let mut at = from;
    for (offset, want, bytes) in ranges {
        assert_eq!(*offset, at, "each range once, in its place");
        let lo = (*offset as usize).min(data.len());
        let hi = (*offset as usize + *want as usize).min(data.len());
        assert_eq!(&bytes[..], &data[lo..hi], "the bytes at {offset}");
        at += *want as u64;
    }
    assert!(at >= plan.end);
}

#[test]
fn split_reads_completing_out_of_order_serve_whole_ranges() {
    for seed in 0..40 {
        let (result, got) = stepped(&PLAN, seed);
        result.expect("the run");
        let (from, ranges) = after_reset(&got);
        assert_eq!(from, 0);
        assert_whole(&ramp(PLAN.len), &PLAN, from, &ranges);
        let short: Vec<_> =
            ranges.iter().filter(|r| r.2.len() < r.1 as usize).map(|r| (r.0, r.2.len())).collect();
        assert_eq!(
            short,
            [(48_000, 2_000)],
            "the range over the end comes back short, seed {seed}"
        );
    }
}

#[test]
fn a_seek_drops_the_reads_in_flight_and_the_pull_restarts_at_its_byte() {
    for seed in 0..40 {
        let plan = Plan { take: 1, seek_after: Some((4, 21_000)), ..PLAN };
        let (result, got) = stepped(&plan, seed);
        result.expect("the run");
        let resets: Vec<u64> = got
            .iter()
            .filter_map(|g| if let Got::Reset(at) = g { Some(*at) } else { None })
            .collect();
        assert_eq!(resets, [0, 21_000], "seed {seed}");
        let (from, ranges) = after_reset(&got);
        assert_whole(&ramp(plan.len), &plan, from, &ranges);
    }
}

#[test]
fn a_failed_read_fails_the_source_with_where_it_read() {
    let plan = Plan { fail_at: Some(20_000), split: false, ..PLAN };
    let (result, _) = stepped(&plan, 3);
    let Err(Error::ElementFailed { source, .. }) = result else { panic!("the run fails") };
    let said = source.to_string();
    assert!(said.contains("at offset 18000") && said.contains("os error 5"), "{said}");
}

#[test]
fn a_file_on_the_os_reactor_serves_its_ranges() {
    let path = std::env::temp_dir().join(format!("pf_ranges_{}", std::process::id()));
    let data = ramp(PLAN.len);
    std::fs::write(&path, &data).expect("fixture");
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&path));
    let got = Arc::new(Mutex::new(Vec::new()));
    let puller = p.add(Puller::new(PLAN.chunk, PLAN.end, usize::MAX, Arc::clone(&got)));
    p.set_element_pool_bytes(puller, 4096, 16, 1 << 20);
    p.link(src.src(FileSrc::OUT), puller.sink(IN)).expect("link");
    p.run().expect("run");
    let got = got.lock().unwrap().clone();
    let (from, ranges) = after_reset(&got);
    assert_whole(&data, &PLAN, from, &ranges);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn push_only_and_a_pipe_push_to_a_consumer_that_wants_ranges() {
    let path = std::env::temp_dir().join(format!("pf_ranges_push_{}", std::process::id()));
    std::fs::write(&path, ramp(PLAN.len)).expect("fixture");
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&path).push_only());
    let got = Arc::new(Mutex::new(Vec::new()));
    let puller = p.add(Puller::new(PLAN.chunk, PLAN.end, usize::MAX, Arc::clone(&got)));
    p.link(src.src(FileSrc::OUT), puller.sink(IN)).expect("link");
    p.run().expect("run");
    let pushed: usize =
        got.lock().unwrap().iter().map(|g| if let Got::Pushed(n) = g { *n } else { 0 }).sum();
    assert_eq!(pushed, PLAN.len);
    let _ = std::fs::remove_file(&path);

    let pipe = std::env::temp_dir().join(format!("pf_ranges_pipe_{}", std::process::id()));
    let _ = std::fs::remove_file(&pipe);
    assert!(std::process::Command::new("mkfifo").arg(&pipe).status().expect("mkfifo").success());
    let writer = {
        let pipe = pipe.clone();
        std::thread::spawn(move || {
            let mut to =
                std::fs::OpenOptions::new().write(true).open(pipe).expect("the writer opens");
            to.write_all(&ramp(PLAN.len)).expect("write");
        })
    };
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&pipe));
    let got = Arc::new(Mutex::new(Vec::new()));
    let puller = p.add(Puller::new(PLAN.chunk, PLAN.end, usize::MAX, Arc::clone(&got)));
    p.link(src.src(FileSrc::OUT), puller.sink(IN)).expect("link");
    p.run().expect("run");
    writer.join().expect("writer");
    let got = got.lock().unwrap().clone();
    let pushed: usize = got.iter().map(|g| if let Got::Pushed(n) = g { *n } else { 0 }).sum();
    assert_eq!(pushed, PLAN.len, "the pipe took its offer back and pushed");
    assert!(!got.iter().any(|g| matches!(g, Got::Range { .. })));
    let _ = std::fs::remove_file(&pipe);
}
