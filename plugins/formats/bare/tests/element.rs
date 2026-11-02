// A test's own fixtures, reactor and stepping, not an element.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::fs::File;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pf_bare::fixture::flac_file;
use pf_bare::{BareHead, BareKind, BareSrc, OpenAt, Reader};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::ALL;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::fetch::drive_slice;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad};
use profluens_core::io::{Completion, Handle, IoResult, Op, Reactor, Submission};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::Stepped;
use profluens_core::time::Timestamp;

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static DESC: ElementDesc = ElementDesc {
    name: "collect",
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

/// A buffer as it went out, its stamp, flags and bytes.
type Out = (u64, BufferFlags, Vec<u8>);

#[derive(Default)]
struct Got {
    bufs: Vec<Out>,
    /// Buffers before the last flush.
    flushed: usize,
}

struct Collect(Arc<Mutex<Got>>);

impl Element for Collect {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(b) = inputs.pop() {
            self.0.lock().unwrap().bufs.push((b.pts.0, b.flags, b.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            let mut g = self.0.lock().unwrap();
            g.flushed = g.bufs.len();
        }
        Ok(())
    }
}

/// What the gate saw and lets through.
#[derive(Default)]
struct Gate {
    /// Reads complete only while open.
    open: bool,
    /// Stale reads are held until one lands on the ring slot of a read of the seek in flight.
    late: bool,
    /// Bumped by the test at a seek, a read is stale when submitted under an older one.
    epoch: u32,
    held: Vec<(u32, Submission)>,
    /// Every read asked, offset and length.
    asked: Vec<(u64, u32)>,
    /// Stale reads completed after their seek.
    stale: u32,
    /// The most reads held at once.
    most: usize,
}

/// Serves an adopted file from memory, never touching the descriptor, its reads held at a gate.
struct GateReactor {
    data: Arc<Vec<u8>>,
    gate: Arc<Mutex<Gate>>,
    done: Vec<(ElementId, Completion)>,
}

impl Reactor for GateReactor {
    fn submit(&mut self, subs: &mut Vec<Submission>, _names: &[u8]) {
        let mut g = self.gate.lock().unwrap();
        for s in subs.drain(..) {
            let result = match s.kind {
                Op::Adopt { .. } => {
                    IoResult::File { handle: Handle::new(1, 1), len: self.data.len() as u64 }
                }
                Op::Read { offset, len, .. } => {
                    g.asked.push((offset, len));
                    let epoch = g.epoch;
                    g.held.push((epoch, s));
                    g.most = g.most.max(g.held.len());
                    continue;
                }
                Op::Cancel { .. } => continue,
                other => panic!("baresrc asked for {other:?}"),
            };
            self.done.push((s.element, Completion { op: s.op, user: s.user, result, buf: s.buf }));
        }
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        out.clear();
        out.append(&mut self.done);
        let mut g = self.gate.lock().unwrap();
        if !g.open {
            return;
        }
        let epoch = g.epoch;
        let ring = u64::from(CREDITS.next_power_of_two());
        let slot = |s: &Submission| s.user % ring;
        let fresh: Vec<u64> = g.held.iter().filter(|h| h.0 == epoch).map(|h| slot(&h.1)).collect();
        let lands = |h: &(u32, Submission)| h.0 < epoch && fresh.contains(&slot(&h.1));
        let mut go = std::mem::take(&mut g.held);
        if g.late && !fresh.is_empty() {
            // a stale read on a live slot alone, the others stay until theirs come round
            match go.iter().position(lands) {
                Some(i) => {
                    let one = go.swap_remove(i);
                    (g.held, go) = (go, vec![one]);
                }
                None => g.held = go.extract_if(.., |h| h.0 < epoch).collect(),
            }
        }
        for (at, mut s) in go {
            g.stale += u32::from(at < epoch);
            let Op::Read { offset, at, len, .. } = s.kind else { panic!("only reads are held") };
            let mut buf = s.buf.take().expect("a read has a buffer");
            let full = buf.memory.as_mut_full();
            let from = (offset as usize).min(self.data.len());
            let n = (len as usize).min(full.len() - at as usize).min(self.data.len() - from);
            full[at as usize..at as usize + n].copy_from_slice(&self.data[from..from + n]);
            buf.memory.set_len(at as usize + n);
            let c = Completion { op: s.op, user: s.user, result: IoResult::Ok(n), buf: Some(buf) };
            out.push((s.element, c));
        }
    }

    fn due_in(&self) -> Option<Duration> {
        let g = self.gate.lock().unwrap();
        (g.open && !g.held.is_empty()).then_some(Duration::ZERO)
    }

    fn is_idle(&self) -> bool {
        self.done.is_empty() && self.gate.lock().unwrap().held.is_empty()
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn head(bytes: &[u8], kind: BareKind) -> BareHead {
    drive_slice(&mut OpenAt::new(bytes.len() as u64, kind), bytes).unwrap().0
}

/// What baresrc puts out from a reader at `ns`, or from the start, read with plain slices.
fn want(h: &BareHead, bytes: &[u8], ns: Option<i64>) -> Vec<Out> {
    let mut rd = Reader::new(h);
    if let Some(ns) = ns {
        drive_slice(&mut rd.seek(h, ns), bytes).unwrap();
    }
    let mut out = Vec::new();
    while let Some(u) = drive_slice(&mut rd.next(h), bytes).unwrap() {
        if out.is_empty() && !h.head.is_empty() {
            out.push((u.pts_ns as u64, BufferFlags::HEADER, h.head.clone()));
        }
        out.push((u.pts_ns as u64, BufferFlags::KEYFRAME, rd.bytes(&u).to_vec()));
    }
    out
}

/// `got` is `want`, told by the first buffer that differs.
fn same(got: &[Out], want: &[Out], what: &str) {
    if let Some(i) = (0..got.len().max(want.len())).find(|&i| got.get(i) != want.get(i)) {
        let at = |o: Option<&Out>| o.map(|o| (o.0, o.1, o.2.len()));
        panic!(
            "{what}, buffer {i} of {} is {:?}, not {:?}",
            want.len(),
            at(got.get(i)),
            at(want.get(i))
        );
    }
}

/// An empty file for baresrc to hold, a read of its own would find nothing.
fn empty() -> File {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("baresrc-empty");
    std::fs::write(&path, []).unwrap();
    File::open(path).unwrap()
}

struct Rig {
    run: Stepped,
    gate: Arc<Mutex<Gate>>,
    got: Arc<Mutex<Got>>,
}

const CREDITS: u32 = 4;

fn rig(bytes: &[u8], h: &BareHead) -> Rig {
    let clock: Arc<dyn Clock> = Arc::new(MockClock::new());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&clock));
    let (data, gate) = (Arc::new(bytes.to_vec()), Arc::new(Mutex::new(Gate::default())));
    let (d, g) = (Arc::clone(&data), Arc::clone(&gate));
    p.set_reactor_factory(Arc::new(move || {
        let r = GateReactor { data: Arc::clone(&d), gate: Arc::clone(&g), done: Vec::new() };
        Ok(Box::new(r) as Box<dyn Reactor>)
    }));
    let src = p.add(BareSrc::new(empty(), h.clone(), bytes.len() as u64));
    p.set_element_credits(src, CREDITS);
    let got = Arc::new(Mutex::new(Got::default()));
    let sink = p.add(Collect(Arc::clone(&got)));
    p.link(src.src(BareSrc::OUT), sink.sink(SinkPad(0))).unwrap();
    let run = p.step(clock).unwrap();
    Rig { run, gate, got }
}

impl Rig {
    /// One pass of every group still running.
    fn pass(&mut self) {
        for g in self.run.running().to_vec() {
            self.run.pass(g).unwrap();
        }
    }

    fn held(&self) -> usize {
        self.gate.lock().unwrap().held.len()
    }

    fn out(&self) -> usize {
        self.got.lock().unwrap().bufs.len()
    }

    /// Opens the gate and runs to the end, what went out after the last flush.
    fn finish(mut self) -> (Vec<Out>, Gate) {
        self.gate.lock().unwrap().open = true;
        for _ in 0..100_000 {
            if self.run.running().is_empty() {
                break;
            }
            self.pass();
        }
        assert!(self.run.running().is_empty(), "the run ends");
        self.run.finish().unwrap();
        let g = std::mem::take(&mut *self.got.lock().unwrap());
        let gate = std::mem::take(&mut *self.gate.lock().unwrap());
        (g.bufs[g.flushed..].to_vec(), gate)
    }
}

fn files() -> Vec<(String, Vec<u8>, BareKind)> {
    let big = flac_file(400, 1000, &[], false).bytes;
    vec![
        ("tone.flac".into(), fixture("tone.flac"), BareKind::Flac),
        ("tone.mp3".into(), fixture("tone.mp3"), BareKind::Mp3),
        ("tone.aac".into(), fixture("tone.aac"), BareKind::Adts),
        ("400 frames.flac".into(), big, BareKind::Flac),
    ]
}

/// Baresrc holds an empty file, so every byte it puts out came through the reactor, and while
/// the reactor holds the reads a pass returns with nothing out.
#[test]
fn every_byte_comes_through_the_reactor_and_a_held_read_blocks_nothing() {
    for (name, bytes, kind) in files() {
        let h = head(&bytes, kind);
        let mut rig = rig(&bytes, &h);
        for _ in 0..50 {
            rig.pass();
        }
        assert_eq!(rig.out(), 0, "{name}, nothing out before a read lands");
        assert!(rig.held() > 0, "{name}, the first read waits at the gate");
        let (out, gate) = rig.finish();
        same(&out, &want(&h, &bytes, None), &name);
        assert!(gate.most <= CREDITS as usize, "{name}, {} reads at once", gate.most);
        assert!(gate.asked.iter().all(|&(at, len)| at + u64::from(len) <= h.end), "{name}");
        let read: u64 = gate.asked.iter().map(|&(_, len)| u64::from(len)).sum();
        assert_eq!(read, h.end - h.first, "{name}, each byte of the audio read once");
    }
}

/// A seek while the read ahead is in flight puts out the frames of the seek and none of the
/// stale bytes, also when a stale read lands on the slot of a read of the seek.
#[test]
fn a_seek_drops_the_reads_in_flight() {
    let f = flac_file(400, 1000, &[], false);
    let h = head(&f.bytes, BareKind::Flac);
    for (to, late) in [(37, false), (37, true), (350, false), (350, true), (2, true)] {
        let to = h.ns(to * 4096 + 100);
        let mut rig = rig(&f.bytes, &h);
        rig.gate.lock().unwrap().open = true;
        while rig.out() < 120 {
            rig.pass();
        }
        rig.gate.lock().unwrap().open = false;
        for _ in 0..200 {
            if rig.held() > 0 {
                break;
            }
            rig.pass();
        }
        assert!(rig.held() > 0, "the read ahead is in flight at {to}");
        {
            let mut g = rig.gate.lock().unwrap();
            (g.epoch, g.late) = (1, late);
        }
        let ts = Timestamp(to as u64);
        rig.run.control().seek_bytes(ts, &[(ALL, 0, ts)]).unwrap();
        for _ in 0..20 {
            rig.pass();
        }
        let (out, gate) = rig.finish();
        assert!(gate.stale > 0, "stale reads landed after the seek to {to}");
        same(&out, &want(&h, &f.bytes, Some(to)), &format!("the seek to {to}"));
    }
}
