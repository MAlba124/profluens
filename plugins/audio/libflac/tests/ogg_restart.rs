// A fresh `oggflacdec` linked behind `oggdemux` with a restart, as a server drops a playing
// Ogg FLAC item to another audio chain, against a stream ffmpeg writes. Skipped without it.

// Test fixtures and clock, not an element hot path.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::process::Command;
use std::sync::{Arc, Mutex};

use pf_libflac::OggFlacDec;
use pf_ogg::OggDemux;
use profluens_core::batch::Inputs;
use profluens_core::clock::MockClock;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::Timestamp;
use profluens_core::topology::Leave;

const RATE: u64 = 16_000;
/// Mono s16.
const FRAME: usize = 2;

fn stream() -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("pf_libflac_restart_{}.oga", std::process::id()));
    let made = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("sine=frequency=440:sample_rate=16000:duration=60")
        .args(["-ac", "1", "-c:a", "flac", "-sample_fmt", "s16", "-f", "ogg"])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    let data = made.then(|| std::fs::read(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    if data.is_none() {
        eprintln!("skipped, no ffmpeg with flac and ogg");
    }
    data
}

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

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static BYTE_PADS: [PadDesc; 1] = [PadDesc { name: "pad", offers: &BYTES }];
static PCM: [OfferDesc; 1] = [OfferDesc::any("audio/raw")];
static PCM_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &PCM }];
static SERVE: ElementDesc = desc("serve", &[], &BYTE_PADS);
static HOLD: ElementDesc = desc("hold", &BYTE_PADS, &[]);
static TAKE: ElementDesc = desc("take", &PCM_PADS, &[]);

/// Serves every range the demuxer asks at once.
struct Serve(Arc<Vec<u8>>);

impl Element for Serve {
    fn desc(&self) -> &'static ElementDesc {
        &SERVE
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.offer_ranges(SrcPad(0));
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(mut ask) = ctx.next_ask(SrcPad(0)) {
            let from = (ask.offset as usize).min(self.0.len());
            let n = (ask.len as usize).min(self.0.len() - from);
            ask.buf.memory.as_mut_full()[..n].copy_from_slice(&self.0[from..from + n]);
            ask.buf.memory.set_len(n);
            ctx.serve(SrcPad(0), ask.tag, ask.buf);
        }
        match ctx.asks_over(SrcPad(0)) {
            true => Ok(Flow::Eos),
            false => Ok(Flow::Ok),
        }
    }
}

/// Pushes the file in small buffers, from a restart's byte after one.
struct Push {
    data: Arc<Vec<u8>>,
    at: usize,
}

impl Element for Push {
    fn desc(&self) -> &'static ElementDesc {
        &SERVE
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.at >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let n = 4096.min(buf.memory.capacity()).min(self.data.len() - self.at);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.at += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.at = ctx.seek_target().map_or(0, |t| t.to_byte as usize);
        }
        Ok(())
    }
}

/// Keeps every packet, a chain that stopped taking, so the demuxer parks the next one.
struct Hold {
    kept: Vec<profluens_core::buffer::Buffer>,
    last: Arc<Mutex<Option<u64>>>,
}

impl Element for Hold {
    fn desc(&self) -> &'static ElementDesc {
        &HOLD
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            if let Some(pts) = buf.pts.nanos() {
                *self.last.lock().unwrap() = Some(pts);
            }
            self.kept.push(buf);
        }
        Ok(Flow::Ok)
    }
}

/// The PCM and the stamp of its first buffer.
#[derive(Default)]
struct Took {
    first: Option<u64>,
    pcm: Vec<u8>,
}

struct Take(Arc<Mutex<Took>>);

impl Element for Take {
    fn desc(&self) -> &'static ElementDesc {
        &TAKE
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut took = self.0.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            took.first = took.first.or(buf.pts.nanos());
            took.pcm.extend_from_slice(buf.memory.data());
        }
        Ok(Flow::Ok)
    }
}

/// Passes every ready group until none is, false when the run did not end.
fn step_through(run: &mut Stepped) -> bool {
    loop {
        run.service();
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        if ready.is_empty() {
            break;
        }
        for g in ready {
            run.pass(g).expect("pass");
        }
    }
    (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
}

/// The pipeline a server builds, the file pulled by `oggdemux` by stream, or pushed.
fn open(data: &Arc<Vec<u8>>, pull: bool) -> (Pipeline, MockClock, ElementId, ElementId, SrcPad) {
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
    let src = match pull {
        true => p.add(Serve(Arc::clone(data))),
        false => p.add(Push { data: Arc::clone(data), at: 0 }),
    };
    let head = data[..data.len().min(64 << 10)].to_vec();
    let demux = p.add(OggDemux::with_len(Some(data.len() as u64)).with_head(head));
    p.set_element_pool_bytes(demux, 4096, 256, 2 << 20);
    p.link(src.src(SrcPad(0)), demux.sink(OggDemux::IN)).expect("serve ! oggdemux");
    let srcs = p.preroll().expect("preroll").srcs;
    assert_eq!(srcs.len(), 1, "one stream");
    (p, clock, src, demux, srcs[0].pad)
}

fn decoder(p: &mut Pipeline, demux: ElementId, pad: SrcPad, took: &Arc<Mutex<Took>>) {
    let dec = p.add(OggFlacDec::new());
    let take = p.add(Take(Arc::clone(took)));
    p.link(demux.src(pad), dec.sink(OggFlacDec::IN)).expect("oggdemux ! oggflacdec");
    p.link(dec.src(OggFlacDec::OUT), take.sink(SinkPad(0))).expect("oggflacdec ! take");
}

/// The chain played to where the demuxer parks a packet, then a fresh decoder linked in its place
/// restarts at an earlier target. It starts at the target, not at the packet held from before
/// the restart, and decodes on from there with nothing the restart read again dropped.
#[test]
fn a_decoder_linked_with_a_restart_starts_at_its_target() {
    let Some(data) = stream() else { return };
    let data = Arc::new(data);

    let (mut p, clock, _, demux, pad) = open(&data, true);
    let whole = Arc::default();
    decoder(&mut p, demux, pad, &whole);
    let mut run = p.step(Arc::new(clock)).expect("step");
    assert!(step_through(&mut run), "the reference run ends");
    run.finish().expect("finish");
    let whole = std::mem::take(&mut whole.lock().unwrap().pcm);
    let samples = whole.len() / FRAME;
    assert!(samples.abs_diff(60 * RATE as usize) < RATE as usize, "{samples} samples");

    for pull in [true, false] {
        let (mut p, clock, src, demux, pad) = open(&data, pull);
        let last = Arc::default();
        let hold = p.add(Hold { kept: Vec::new(), last: Arc::clone(&last) });
        p.link(demux.src(pad), hold.sink(SinkPad(0))).expect("oggdemux ! hold");
        let mut run = p.step(Arc::new(clock)).expect("step");
        assert!(!step_through(&mut run), "the demuxer parks behind the held chain, pull {pull}");
        let sent = last.lock().unwrap().expect("packets sent");
        assert!(sent > 10_000_000_000, "sent to {sent} ns, pull {pull}");

        // A target on the sample grid, well behind what was sent and read ahead.
        let target = Timestamp(sent / 2 / 1_000_000 * 1_000_000);
        p.remove(hold, Leave::Cut).expect("remove the held chain");
        let took = Arc::default();
        decoder(&mut p, demux, pad, &took);
        p.commit_at(target, &[(src, 0, target), (demux, 0, target)]).expect("commit");
        assert!(step_through(&mut run), "the run ends, pull {pull}");
        run.finish().expect("finish");

        let took = took.lock().unwrap();
        let ms = |ns: u64| ns / 1_000_000;
        let first = took.first.expect("the fresh decoder played");
        let what = format!("sent to {} ms, pull {pull}", ms(sent));
        assert_eq!(ms(first), ms(target.0), "placed at the target, {what}");
        let from = (target.0 * RATE / 1_000_000_000) as usize * FRAME;
        assert_eq!(took.pcm.len(), whole.len() - from, "on from the target to the end, {what}");
        assert!(took.pcm == whole[from..], "the stream from the target, nothing dropped, {what}");
    }
}
