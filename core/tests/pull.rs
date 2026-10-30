//! The request port of a source's pad under the stepped driver.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use profluens_core::batch::Inputs;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::pull::{Ask, Fed, Feed, RangeError};
use profluens_core::sched::Outcome;
use profluens_core::time::Timestamp;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ONE_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static ONE_OUT: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    sched: SchedHint,
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static SRC: ElementDesc = desc("rangesrc", &[], &ONE_OUT, SchedHint::Active);
static PULLER: ElementDesc = desc("puller", &ONE_IN, &[], SchedHint::Passive);
static A_PULLER: ElementDesc = desc("apuller", &ONE_IN, &[], SchedHint::Active);

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

// SplitMix64.
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

/// A source that serves ranges out of an array in a seeded order. Unpulled, it pushes the array.
struct RangeSrc {
    data: Arc<Vec<u8>>,
    offer: bool,
    withdraw: bool,
    held: Vec<(Ask, u64)>,
    rng: Rng,
    pos: usize,
    served: Arc<Mutex<u64>>,
}

impl Element for RangeSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.offer {
            ctx.offer_ranges(OUT);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.withdraw && ctx.pulled(OUT) {
            ctx.withdraw_ranges(OUT);
            self.withdraw = false;
            return Ok(Flow::Ok);
        }
        if !ctx.pulled(OUT) {
            while self.pos < self.data.len() {
                let Some(mut buf) = ctx.try_alloc() else {
                    return Ok(Flow::Ok);
                };
                let n = 1000.min(self.data.len() - self.pos);
                buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                buf.memory.set_len(n);
                ctx.out(OUT).push(buf);
                self.pos += n;
            }
            return Ok(Flow::Eos);
        }
        while let Some(ask) = ctx.next_ask(OUT) {
            let wait = self.rng.below(3);
            self.held.push((ask, wait));
        }
        let mut i = 0;
        while i < self.held.len() {
            if self.held[i].1 > 0 {
                self.held[i].1 -= 1;
                i += 1;
                continue;
            }
            let pick = i + self.rng.below((self.held.len() - i) as u64) as usize;
            let (mut ask, _) = self.held.swap_remove(pick);
            let from = (ask.offset as usize).min(self.data.len());
            let n = (ask.len as usize).min(self.data.len() - from);
            ask.buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[from..from + n]);
            ask.buf.memory.set_len(n);
            ctx.serve(OUT, ask.tag, ask.buf);
            *self.served.lock().unwrap() += 1;
        }
        if !self.held.is_empty() {
            ctx.wake_within(Duration::ZERO);
            return Ok(Flow::Ok);
        }
        Ok(if ctx.asks_over(OUT) { Flow::Eos } else { Flow::Ok })
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.held.clear();
            self.pos = ctx.seek_target().map_or(0, |t| t.to_byte as usize);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Got {
    Reset(u64),
    Range { offset: u64, want: u32, bytes: Vec<u8> },
    Pushed(Vec<u8>),
    Refused(RangeError),
}

/// A consumer that asks `chunk` bytes at a time to `end`, taking at most `take` ranges a pass.
struct Puller {
    desc: &'static ElementDesc,
    want: bool,
    chunk: u32,
    end: u64,
    take: usize,
    next: u64,
    epoch: u64,
    seq: u64,
    asked: HashMap<u64, (u64, u32)>,
    over: bool,
    got: Arc<Mutex<Vec<Got>>>,
    extra: bool,
}

impl Element for Puller {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.want {
            ctx.want_ranges(IN);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(from) = ctx.pull_reset(IN) {
            (self.next, self.over) = (from.to_byte, false);
            self.epoch += 1;
            self.asked.clear();
            self.got.lock().unwrap().push(Got::Reset(from.to_byte));
        }
        if !ctx.pulls(IN) {
            while let Some(buf) = inputs.pop() {
                self.got.lock().unwrap().push(Got::Pushed(buf.memory.data().to_vec()));
            }
            return Ok(Flow::Ok);
        }
        let mut taken = 0;
        while taken < self.take {
            let Some((tag, buf)) = ctx.next_served(IN, &mut inputs) else {
                break;
            };
            taken += 1;
            assert_eq!(
                tag >> 32,
                self.epoch,
                "a range asked before the reset is never served after it"
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
        if self.extra && ctx.asks_left(IN) == 0 {
            self.extra = false;
            let buf = ctx.alloc_exact(8).expect("exact");
            let refused = ctx.ask(IN, Ask { offset: 0, len: 8, tag: u64::MAX, buf }).unwrap_err();
            self.got.lock().unwrap().push(Got::Refused(refused));
        }
        if self.next >= self.end && !self.over {
            self.over = true;
            ctx.end_asks(IN);
        }
        Ok(Flow::Ok)
    }
}

struct Run {
    got: Vec<Got>,
    served: u64,
}

struct Setup {
    len: usize,
    offer: bool,
    want: bool,
    withdraw: bool,
    active_consumer: bool,
    chunk: u32,
    end: u64,
    take: usize,
    seek_after: Option<(usize, u64)>,
    extra: bool,
}

const BASE: Setup = Setup {
    len: 10_000,
    offer: true,
    want: true,
    withdraw: false,
    active_consumer: false,
    chunk: 700,
    end: 10_300,
    take: usize::MAX,
    seek_after: None,
    extra: false,
};

fn run(setup: Setup, seed: u64) -> Run {
    let data = Arc::new(ramp(setup.len));
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    p.set_pool(4096, 16);
    let served = Arc::new(Mutex::new(0));
    let src = p.add(RangeSrc {
        data: Arc::clone(&data),
        offer: setup.offer,
        withdraw: setup.withdraw,
        held: Vec::new(),
        rng: Rng(seed),
        pos: 0,
        served: Arc::clone(&served),
    });
    let got = Arc::new(Mutex::new(Vec::new()));
    let consumer = p.add(Puller {
        desc: if setup.active_consumer { &A_PULLER } else { &PULLER },
        want: setup.want,
        chunk: setup.chunk,
        end: setup.end,
        take: setup.take,
        next: 0,
        epoch: 0,
        seq: 0,
        asked: HashMap::new(),
        over: false,
        got: Arc::clone(&got),
        extra: setup.extra,
    });
    p.set_element_credits(consumer, 4);
    p.link(src.src(OUT), consumer.sink(IN)).expect("link");
    let control = p.control();
    let mut rng = Rng(seed ^ 0x5eed);
    let mut sought = false;
    let mut run = p.step(Arc::clone(&shared)).expect("step");
    for _ in 0..100_000 {
        if let (Some((after, to)), false) = (setup.seek_after, sought) {
            let ranges =
                got.lock().unwrap().iter().filter(|g| matches!(g, Got::Range { .. })).count();
            if ranges >= after {
                let _ = control.seek_bytes(
                    Timestamp::ZERO,
                    &[(profluens_core::command::ALL, to, Timestamp::ZERO)],
                );
                sought = true;
            }
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
        if let Outcome::Exit = run.pass(g).expect("pass") {
            continue;
        }
    }
    let stalled = (0..run.groups()).any(|g| !matches!(run.outcome(g), Some(Outcome::Exit)));
    assert!(!stalled, "the run ended, seed {seed}");
    run.finish().expect("finish");
    let got = got.lock().unwrap().clone();
    let served = *served.lock().unwrap();
    Run { got, served }
}

fn ranges_after_reset(got: &[Got]) -> (u64, Vec<(u64, u32, Vec<u8>)>) {
    let at = got.iter().rposition(|g| matches!(g, Got::Reset(_))).expect("a reset");
    let Got::Reset(from) = got[at] else { unreachable!() };
    let mut ranges: Vec<_> = got[at + 1..]
        .iter()
        .filter_map(|g| match g {
            Got::Range { offset, want, bytes } => Some((*offset, *want, bytes.clone())),
            _ => None,
        })
        .collect();
    ranges.sort_by_key(|r| r.0);
    (from, ranges)
}

fn assert_whole(data: &[u8], from: u64, ranges: &[(u64, u32, Vec<u8>)], chunk: u32, end: u64) {
    let mut at = from;
    for (offset, want, bytes) in ranges {
        assert_eq!(*offset, at, "every range is served once, in its place");
        assert_eq!(*want, chunk);
        let lo = (*offset as usize).min(data.len());
        let hi = (*offset as usize + *want as usize).min(data.len());
        assert_eq!(&bytes[..], &data[lo..hi], "the bytes of the range at {offset}");
        at += *want as u64;
    }
    assert!(at >= end, "the ranges reach the end asked for");
}

#[test]
fn ranges_served_out_of_order_pair_with_their_tags() {
    for seed in 0..64 {
        let r = run(BASE, seed);
        let (from, ranges) = ranges_after_reset(&r.got);
        assert_eq!(from, 0);
        assert_whole(&ramp(BASE.len), from, &ranges, BASE.chunk, BASE.end);
        assert!(
            !r.got.iter().any(|g| matches!(g, Got::Pushed(_))),
            "nothing pushed on a pulled pad"
        );
    }
}

#[test]
fn a_range_past_the_end_of_the_file_comes_back_short() {
    let r = run(BASE, 7);
    let (_, ranges) = ranges_after_reset(&r.got);
    let short: Vec<_> =
        ranges.iter().filter(|(_, want, bytes)| bytes.len() < *want as usize).collect();
    // 10 000 bytes in ranges of 700 reach 10 300, so the fifteenth range holds 200 bytes.
    assert_eq!(short.len(), 1);
    assert_eq!((short[0].0, short[0].2.len()), (9_800, 200));
}

#[test]
fn a_seek_drops_what_was_asked_and_served_before_it() {
    for seed in 0..64 {
        let setup = Setup { take: 1, seek_after: Some((3, 4_200)), ..BASE };
        let r = run(setup, seed);
        let resets: Vec<u64> = r
            .got
            .iter()
            .filter_map(|g| if let Got::Reset(at) = g { Some(*at) } else { None })
            .collect();
        assert_eq!(resets, [0, 4_200], "seed {seed}");
        let (from, ranges) = ranges_after_reset(&r.got);
        assert_whole(&ramp(BASE.len), from, &ranges, BASE.chunk, BASE.end);
    }
}

#[test]
fn the_consumer_has_its_credits_of_ranges_out_and_no_more() {
    let r = run(Setup { extra: true, ..BASE }, 3);
    assert!(r.got.contains(&Got::Refused(RangeError::Full)));
}

#[test]
fn a_consumer_behind_a_source_that_only_pushes_takes_the_push() {
    for setup in [Setup { offer: false, ..BASE }, Setup { active_consumer: true, ..BASE }] {
        let r = run(setup, 1);
        let pushed: Vec<u8> = r
            .got
            .iter()
            .flat_map(|g| if let Got::Pushed(b) = g { b.clone() } else { Vec::new() })
            .collect();
        assert_eq!(pushed, ramp(BASE.len));
        assert!(!r.got.iter().any(|g| matches!(g, Got::Reset(_) | Got::Range { .. })));
        assert_eq!(r.served, 0);
    }
}

#[test]
fn a_source_whose_consumer_wants_no_ranges_pushes() {
    let r = run(Setup { want: false, ..BASE }, 2);
    let pushed: usize =
        r.got.iter().map(|g| if let Got::Pushed(b) = g { b.len() } else { 0 }).sum();
    assert_eq!(pushed, BASE.len);
}

#[test]
fn a_withdrawn_offer_turns_the_consumer_to_the_push() {
    let r = run(Setup { withdraw: true, ..BASE }, 4);
    assert_eq!(r.got.first(), Some(&Got::Reset(0)));
    assert!(!r.got.iter().any(|g| matches!(g, Got::Range { .. })));
    let pushed: Vec<u8> = r
        .got
        .iter()
        .flat_map(|g| if let Got::Pushed(b) = g { b.clone() } else { Vec::new() })
        .collect();
    assert_eq!(pushed, ramp(BASE.len));
    assert_eq!(r.served, 0);
}

#[test]
fn a_consumer_that_ends_its_asks_with_none_out_ends_the_run() {
    for seed in 0..16 {
        let r = run(Setup { end: 0, ..BASE }, seed);
        assert_eq!((r.got.as_slice(), r.served), ([Got::Reset(0)].as_slice(), 0), "seed {seed}");
        let r = run(Setup { take: 1, seek_after: Some((2, 20_000)), ..BASE }, seed);
        assert_eq!(r.got.last(), Some(&Got::Reset(20_000)), "seed {seed}");
    }
}

#[test]
fn a_seek_target_names_where_the_pull_restarts() {
    let target = SeekTarget { to_byte: 4_200, at: Timestamp::ZERO };
    let r = run(Setup { take: 1, seek_after: Some((1, target.to_byte)), ..BASE }, 11);
    assert_eq!(
        r.got.iter().rev().find_map(|g| if let Got::Reset(at) = g { Some(*at) } else { None }),
        Some(4_200)
    );
}

type Parsed = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

/// A consumer that parses what a [`Feed`] hands back and asks the last `aside` bytes beside it
/// once.
struct FeedPuller {
    feed: Feed,
    aside: Option<(u64, u32)>,
    windows: Parsed,
    asides: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Element for FeedPuller {
    fn desc(&self) -> &'static ElementDesc {
        &PULLER
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.want_ranges(IN);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(from) = ctx.pull_reset(IN) {
            self.feed.restart(from.to_byte);
            self.windows.lock().unwrap().push((u64::MAX, Vec::new()));
        }
        while let Some(fed) = self.feed.take(ctx, IN, &mut inputs) {
            match fed {
                Fed::Window { at, buf } => {
                    self.windows.lock().unwrap().push((at, buf.memory.data().to_vec()))
                }
                Fed::Aside { tag, buf } => {
                    assert_eq!(tag, 7);
                    self.asides.lock().unwrap().push(buf.memory.data().to_vec());
                }
            }
        }
        if let Some((offset, len)) = self.aside
            && self.feed.aside(ctx, IN, offset, len, 7)
        {
            self.aside = None;
        }
        self.feed.ask(ctx, IN);
        Ok(Flow::Ok)
    }
}

fn run_feed(
    seed: u64,
    window: u32,
    seek_after: Option<(usize, u64)>,
) -> (u64, Vec<u8>, Vec<Vec<u8>>) {
    let data = Arc::new(ramp(BASE.len));
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    p.set_pool(4096, 16);
    let served = Arc::new(Mutex::new(0));
    let src = p.add(RangeSrc {
        data: Arc::clone(&data),
        offer: true,
        withdraw: false,
        held: Vec::new(),
        rng: Rng(seed),
        pos: 0,
        served,
    });
    let windows = Arc::new(Mutex::new(Vec::new()));
    let asides = Arc::new(Mutex::new(Vec::new()));
    let consumer = p.add(FeedPuller {
        feed: Feed::new(window, 3),
        aside: Some((BASE.len as u64 - 100, 100)),
        windows: Arc::clone(&windows),
        asides: Arc::clone(&asides),
    });
    p.set_element_credits(consumer, 4);
    p.link(src.src(OUT), consumer.sink(IN)).expect("link");
    let control = p.control();
    let mut rng = Rng(seed ^ 0xfeed);
    let mut sought = false;
    let mut run = p.step(Arc::clone(&shared)).expect("step");
    for _ in 0..100_000 {
        if let (Some((after, to)), false) = (seek_after, sought)
            && windows.lock().unwrap().len() > after
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
        let _ = run.pass(g).expect("pass");
    }
    let stalled = (0..run.groups()).any(|g| !matches!(run.outcome(g), Some(Outcome::Exit)));
    assert!(!stalled, "the run ended, seed {seed}");
    run.finish().expect("finish");
    let windows = windows.lock().unwrap().clone();
    let last = windows.iter().rposition(|(at, _)| *at == u64::MAX).expect("a reset");
    let after = &windows[last + 1..];
    let from = after.first().map_or(0, |(at, _)| *at);
    let mut at = from;
    let mut bytes = Vec::new();
    for (offset, b) in after {
        assert_eq!(*offset, at, "windows come back in file order, seed {seed}");
        at += b.len() as u64;
        bytes.extend_from_slice(b);
    }
    let asides = asides.lock().unwrap().clone();
    (from, bytes, asides)
}

#[test]
fn a_feed_hands_back_the_file_in_order_however_it_is_served() {
    let data = ramp(BASE.len);
    for seed in 0..64 {
        for window in [700, 1000] {
            let (from, bytes, asides) = run_feed(seed, window, None);
            assert_eq!(from, 0);
            assert_eq!(bytes, data, "seed {seed}, window {window}");
            assert_eq!(asides, [data[BASE.len - 100..].to_vec()], "the range asked aside");
        }
    }
}

#[test]
fn a_feed_starts_over_where_a_seek_restarts_the_pull() {
    let data = ramp(BASE.len);
    for seed in 0..64 {
        let (from, bytes, _) = run_feed(seed, 700, Some((2, 4_200)));
        assert_eq!(from, 4_200, "seed {seed}");
        assert_eq!(bytes, data[4_200..], "seed {seed}");
    }
}
