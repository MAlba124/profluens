//! `oggdemux` tells its source the stream's bitrate up its sink pad.

// A test's own fixtures and threads, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pf_ogg::OggDemux;
use profluens_core::batch::Inputs;
use profluens_core::command::ALL;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::feedback::{Bitrate, Feedback, RateKind};
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static PADS: [PadDesc; 1] = [PadDesc { name: "pad", offers: &BYTES }];

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static TOLD: ElementDesc = desc("told", &[], &PADS);
static REC: ElementDesc = desc("rec", &PADS, &[]);

struct Told {
    data: Arc<Vec<u8>>,
    pos: usize,
    ranges: bool,
    rates: Arc<Mutex<Vec<Option<Bitrate>>>>,
}

impl Element for Told {
    fn desc(&self) -> &'static ElementDesc {
        &TOLD
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.ranges {
            ctx.offer_ranges(SrcPad(0));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if ctx.pulled(SrcPad(0)) {
            while let Some(mut ask) = ctx.next_ask(SrcPad(0)) {
                let from = (ask.offset as usize).min(self.data.len());
                let n = (ask.len as usize).min(self.data.len() - from);
                ask.buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[from..from + n]);
                ask.buf.memory.set_len(n);
                ctx.serve(SrcPad(0), ask.tag, ask.buf);
            }
            if ctx.asks_over(SrcPad(0)) {
                return Ok(Flow::Eos);
            }
            ctx.wake_within(Duration::from_millis(1));
            return Ok(Flow::Ok);
        }
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = 4096.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Feedback { feedback: Feedback::Bitrate(b), .. } => {
                self.rates.lock().unwrap().push(Some(*b))
            }
            Event::FlushStart => self.rates.lock().unwrap().push(None),
            _ => {}
        }
        Ok(())
    }
}

struct Rec {
    pace: Duration,
    seen: Arc<Mutex<usize>>,
}

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {
            *self.seen.lock().unwrap() += 1;
            std::thread::sleep(self.pace);
        }
        Ok(Flow::Ok)
    }
}

fn file() -> Option<Arc<Vec<u8>>> {
    let path = std::env::temp_dir().join(format!(
        "pf_ogg_bitrate_{}_{:?}.opus",
        std::process::id(),
        std::thread::current().id()
    ));
    let made = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args([
            "-v",
            "error",
            "-y",
            "-threads",
            "1",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=d=12:a=0.4:r=48000:seed=1",
        ])
        .args(["-ac", "2", "-c:a", "libopus", "-b:a", "160k", "-f", "ogg"])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("skipped: no ffmpeg");
        return None;
    }
    let data = std::fs::read(&path).expect("the fixture");
    let _ = std::fs::remove_file(&path);
    Some(Arc::new(data))
}

fn told(
    data: &Arc<Vec<u8>>,
    ranges: bool,
    len: bool,
    pace: Duration,
    seek_after: Option<usize>,
) -> Vec<Option<Bitrate>> {
    let rates = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(0usize));
    let mut p = pf_reactor::pipeline();
    let src = p.add(Told { data: Arc::clone(data), pos: 0, ranges, rates: Arc::clone(&rates) });
    let demux = p.add(OggDemux::with_len(len.then_some(data.len() as u64)));
    let sink = p.add(Rec { pace, seen: Arc::clone(&seen) });
    p.link(src.src(SrcPad(0)), demux.sink(OggDemux::IN)).expect("told ! oggdemux");
    p.link(demux.src(OggDemux::OUT), sink.sink(SinkPad(0))).expect("oggdemux ! rec");
    let control = p.control();
    let run = std::thread::spawn(move || p.run());
    if let Some(after) = seek_after {
        let began = Instant::now();
        while *seen.lock().unwrap() < after {
            assert!(began.elapsed() < Duration::from_secs(10), "the packets went out");
            std::thread::sleep(Duration::from_millis(1));
        }
        control.seek_bytes(Timestamp::ZERO, &[(ALL, 0, Timestamp::ZERO)]).expect("a seek");
    }
    run.join().expect("the run thread").expect("run");

    rates.lock().unwrap().clone()
}

fn near(data: &[u8], b: &Bitrate) -> bool {
    let secs_ms = pf_ogg::stream_duration_ns(&first_packet(data), data, serial(data))
        .expect("a duration")
        / 1_000_000;
    let want = data.len() as u64 * 8 * 1_000 / secs_ms;
    b.bits_per_second.abs_diff(want) <= want / 10
}

fn first_packet(data: &[u8]) -> Vec<u8> {
    pf_ogg::demux_all(data).into_iter().next().expect("a packet").data
}

fn serial(data: &[u8]) -> u32 {
    pf_ogg::demux_all(data)[0].serial
}

#[test]
fn pulled_and_told_its_length_the_rate_is_the_length_over_the_duration_then_measured() {
    let Some(data) = file() else { return };
    let rates: Vec<Bitrate> =
        told(&data, true, true, Duration::ZERO, None).into_iter().flatten().collect();
    assert_eq!(
        rates.first().map(|b| b.kind),
        Some(RateKind::Average),
        "the average at the first output, {rates:?}"
    );
    assert!(rates.iter().all(|b| near(&data, b)), "{rates:?}");
    assert!(rates[1..].iter().any(|b| b.kind == RateKind::Local), "then measured, {rates:?}");
}

#[test]
fn pushed_with_no_length_the_rate_is_measured() {
    let Some(data) = file() else { return };
    let rates: Vec<Bitrate> =
        told(&data, false, false, Duration::ZERO, None).into_iter().flatten().collect();
    assert!(!rates.is_empty(), "measured once a second is in");
    assert!(rates.iter().all(|b| b.kind == RateKind::Local && near(&data, b)), "{rates:?}");
}

#[test]
fn behind_a_seek_back_the_rate_is_told_again() {
    let Some(data) = file() else { return };
    let rates = told(&data, true, true, Duration::from_millis(2), Some(300));
    let behind =
        rates.iter().rposition(Option::is_none).map(|at| rates[at + 1..].iter().flatten().count());
    assert!(behind.is_some_and(|n| n >= 3), "told behind the seek, {rates:?}");
}
