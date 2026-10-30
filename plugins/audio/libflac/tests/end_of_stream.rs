//! The end of a stream drains under the backpressure it ran under (`Ctx::hold_end`).

// Tests own their fixtures and staging `Vec`s.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pf_libflac::{FlacDec, FlacEncoder, SampleFormat};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const RATE: u32 = 48_000;
const SECONDS: usize = 40;
const FRAMES: usize = SECONDS * RATE as usize;
const SLOT: usize = 16 * 1024;
const SLOTS: u32 = 32;

fn stream() -> Vec<u8> {
    let mut pcm = Vec::with_capacity(FRAMES * 4);
    for k in 0..FRAMES {
        let code = (k % 60_000) as i32 - 30_000;
        pcm.extend_from_slice(&(code as i16).to_le_bytes());
        pcm.extend_from_slice(&(code as i16).to_le_bytes());
    }
    let (mut enc, mut header) = FlacEncoder::new(RATE, 2, SampleFormat::S16).expect("encoder");
    let mut frames = Vec::new();
    enc.encode_interleaved(&pcm, &mut frames).expect("encode");
    let body = enc.finish(&mut frames);
    let at = pf_libflac::streaminfo_offset();
    header[at..at + body.len()].copy_from_slice(&body);
    header.extend_from_slice(&frames);
    header
}

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "wholesrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct WholeSrc(Option<Vec<u8>>);

impl Element for WholeSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let Some(data) = self.0.take() else {
            return Ok(Flow::Eos);
        };
        let mut buf = ctx.alloc_exact(data.len()).expect("exact");
        buf.memory.as_mut_full()[..data.len()].copy_from_slice(&data);
        buf.memory.set_len(data.len());
        ctx.out(SrcPad(0)).push(buf);
        Ok(Flow::Ok)
    }
}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("audio/raw")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "countsink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct Counted {
    samples: AtomicU64,
    wrong: AtomicU64,
    at_eos: AtomicU64,
}

struct CountSink(Arc<Counted>);

impl Element for CountSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let mut at = self.0.samples.load(Ordering::Relaxed);
            for frame in buf.memory.data().as_chunks::<4>().0 {
                let code = i16::from_le_bytes([frame[0], frame[1]]);
                if i32::from(code) != (at % 60_000) as i32 - 30_000
                    && self.0.wrong.load(Ordering::Relaxed) == 0
                {
                    self.0.wrong.store(at + 1, Ordering::Relaxed);
                }
                at += 1;
            }
            self.0.samples.store(at, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::Eos) {
            self.0.at_eos.store(self.0.samples.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        Ok(())
    }
}

#[test]
fn a_whole_file_in_one_buffer_is_decoded_within_the_pool() {
    let file = stream();
    let pcm_bytes = FRAMES * 4;
    assert!(pcm_bytes > 8 * SLOT * SLOTS as usize, "the stream is many times the pool");
    let counted = Arc::new(Counted::default());
    let mut p = Pipeline::new();
    p.set_pool(SLOT, SLOTS);
    let src = p.add(WholeSrc(Some(file)));
    let dec = p.add(FlacDec::new());
    let sink = p.add(CountSink(Arc::clone(&counted)));
    p.link(src.src(SrcPad(0)), dec.sink(FlacDec::IN)).expect("src -> flacdec");
    p.link(dec.src(FlacDec::OUT), sink.sink(SinkPad(0))).expect("flacdec -> sink");
    p.run().expect("the run ends");
    let report = p.last_report().expect("a report");
    assert_eq!(counted.wrong.load(Ordering::Relaxed), 0, "a sample out of place, counted from one");
    assert_eq!(
        counted.samples.load(Ordering::Relaxed),
        FRAMES as u64,
        "every sample of the stream"
    );
    assert_eq!(
        counted.at_eos.load(Ordering::Relaxed),
        FRAMES as u64,
        "and all of them ahead of the end"
    );
    eprintln!(
        "{} MiB of PCM through a pool of {} KiB, {} slots out at most, {} buffers allocated",
        pcm_bytes >> 20,
        (SLOT * SLOTS as usize) >> 10,
        report.pool_high_water,
        report.pool_slot_allocations
    );
    assert!(
        report.pool_high_water <= u64::from(SLOTS),
        "{} slots out at once",
        report.pool_high_water
    );
    assert!(
        report.pool_slot_allocations <= u64::from(SLOTS) + 1,
        "{} buffers allocated",
        report.pool_slot_allocations
    );
}

#[test]
fn a_pool_of_a_slot_or_a_few_decodes_the_whole_file() {
    let file = stream();
    let path =
        std::env::temp_dir().join(format!("pf_libflac_small_pool_{}.flac", std::process::id()));
    std::fs::write(&path, &file).expect("write the file");
    for slots in [1u32, 2, 3, 4] {
        let counted = Arc::new(Counted::default());
        let mut p = pf_reactor::pipeline();
        p.set_pool(SLOT, slots);
        let src = p.add(profluens_elements::io::FileSrc::new(path.to_str().expect("a utf-8 path")));
        let dec = p.add(FlacDec::new());
        let sink = p.add(CountSink(Arc::clone(&counted)));
        p.link(src.src(SrcPad(0)), dec.sink(FlacDec::IN)).expect("filesrc -> flacdec");
        p.link(dec.src(FlacDec::OUT), sink.sink(SinkPad(0))).expect("flacdec -> sink");
        let (done, ended) = std::sync::mpsc::channel();
        let run = std::thread::spawn(move || {
            let ran = p.run().map_err(|e| format!("{e:?}"));
            let _ = done.send(ran);
        });
        let ran = ended.recv_timeout(std::time::Duration::from_secs(60));
        assert_eq!(ran, Ok(Ok(())), "a pool of {slots} slots");
        run.join().expect("the run thread");
        assert_eq!(counted.wrong.load(Ordering::Relaxed), 0, "a pool of {slots} slots");
        assert_eq!(
            counted.samples.load(Ordering::Relaxed),
            FRAMES as u64,
            "a pool of {slots} slots"
        );
    }
    let _ = std::fs::remove_file(&path);
}
