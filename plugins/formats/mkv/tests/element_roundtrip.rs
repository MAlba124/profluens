//! `MkvMux` in a real pipeline produces a well-formed single-track MKV.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

mod common;

use std::sync::{Arc, Mutex};

use common::{parse_simple_block, walk};
use pf_libflac::{FlacEncoder, SampleFormat};
use pf_mkv::MkvMux;
use pf_mkv::ebml::id;
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const MASTERS: &[&[u8]] =
    &[id::EBML, id::SEGMENT, id::INFO, id::TRACKS, id::TRACK_ENTRY, id::AUDIO, id::CLUSTER];

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "framesrc",
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

struct FrameSrc {
    frames: Vec<Vec<u8>>,
    next: usize,
}

impl FrameSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for FrameSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.next = 0;
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.next >= self.frames.len() {
            return Ok(Flow::Eos);
        }
        let f = &self.frames[self.next];
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok),
        };
        assert!(f.len() <= buf.memory.capacity(), "frame exceeds pool slot");
        buf.memory.as_mut_full()[..f.len()].copy_from_slice(f);
        buf.memory.set_len(f.len());
        ctx.out(Self::OUT).push(buf);
        self.next += 1;
        Ok(Flow::Ok)
    }
}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "bytesink",
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

struct ByteSink {
    got: Arc<Mutex<Vec<u8>>>,
}

impl ByteSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for ByteSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut got = self.got.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            got.extend_from_slice(buf.memory.data());
        }
        Ok(Flow::Ok)
    }
}

fn make_flac(sample_rate: u32, channels: u32, block: usize, n: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let (mut enc, mut header) =
        FlacEncoder::new_streaming(sample_rate, channels, SampleFormat::S16, block as u32).unwrap();
    let mut frames = Vec::new();
    let mut phase = 0i32;
    for _ in 0..n {
        let mut pcm = Vec::new();
        for _ in 0..block {
            for c in 0..channels {
                let v = ((phase.wrapping_mul(53) + c as i32 * 700) & 0x3FFF) as i16 - 0x2000;
                pcm.extend_from_slice(&v.to_le_bytes());
                phase = phase.wrapping_add(1);
            }
        }
        enc.encode_frames(&pcm, &mut frames).unwrap();
    }
    let body = enc.finish_frames(&mut frames);
    let off = pf_libflac::streaminfo_offset();
    header[off..off + body.len()].copy_from_slice(&body);
    (header, frames)
}

fn run_mux(codec_private: Vec<u8>, frames: Vec<Vec<u8>>) -> Vec<u8> {
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(FrameSrc { frames, next: 0 });
    let mux = p.add(MkvMux::flac(codec_private, 48_000.0, 2, 16));
    let sink = p.add(ByteSink { got: Arc::clone(&got) });
    p.link(src.src(FrameSrc::OUT), mux.sink(MkvMux::IN)).expect("src->mux");
    p.link(mux.src(MkvMux::OUT), sink.sink(ByteSink::IN)).expect("mux->sink");
    p.run().expect("run");

    got.lock().unwrap().clone()
}

#[test]
fn mkvmux_pipeline_produces_valid_mkv() {
    let (codec_private, frames) = make_flac(48_000, 2, 4096, 5);
    let stream = run_mux(codec_private.clone(), frames.clone());

    assert!(!stream.is_empty(), "mux produced bytes");
    assert_eq!(&stream[..4], id::EBML, "stream starts with the EBML Header ID");

    let els = walk(&stream, MASTERS);
    assert!(els.iter().any(|(_, e)| e.id == id::SEGMENT), "Segment present");

    let entries = els.iter().filter(|(_, e)| e.id == id::TRACK_ENTRY).count();
    assert_eq!(entries, 1, "one TrackEntry (single-track element)");
    assert!(els.iter().any(|(_, e)| e.id == id::CUES), "Cues, so players need not bisect");
    let mut cid = None;
    let mut cpriv = None;
    for (_d, el) in &els {
        if el.id == id::CODEC_ID {
            let (s, e) = el.data.unwrap();
            cid = Some(String::from_utf8(stream[s..e].to_vec()).unwrap());
        } else if el.id == id::CODEC_PRIVATE {
            let (s, e) = el.data.unwrap();
            cpriv = Some(stream[s..e].to_vec());
        }
    }
    assert_eq!(cid.as_deref(), Some("A_FLAC"), "CodecID A_FLAC");
    assert_eq!(cpriv.as_ref(), Some(&codec_private), "CodecPrivate as constructed");

    let mut base = 0i64;
    let mut blocks = Vec::new();
    for (_d, el) in &els {
        if el.id == id::TIMESTAMP {
            let (s, e) = el.data.unwrap();
            let mut v = 0i64;
            for &b in &stream[s..e] {
                v = (v << 8) | b as i64;
            }
            base = v;
        } else if el.id == id::SIMPLE_BLOCK {
            let (s, e) = el.data.unwrap();
            let blk = parse_simple_block(&stream[s..e]);
            blocks.push((base + blk.rel_ts as i64, blk));
        }
    }
    assert_eq!(blocks.len(), frames.len(), "one SimpleBlock per frame");
    let mut last = i64::MIN;
    for (i, ((ts, blk), frame)) in blocks.iter().zip(frames.iter()).enumerate() {
        assert_eq!(blk.track, 1, "block {i} on track 1");
        assert_eq!(&blk.frame, frame, "block {i} exact FLAC frame bytes");
        assert!(*ts >= last, "block {i} timestamp non-decreasing");
        last = *ts;
    }
}

#[test]
fn mkvmux_empty_stream_writes_nothing() {
    let (codec_private, _f) = make_flac(48_000, 2, 4096, 0);
    let stream = run_mux(codec_private, Vec::new());
    assert!(stream.is_empty(), "an empty input produces no MKV bytes");
}
