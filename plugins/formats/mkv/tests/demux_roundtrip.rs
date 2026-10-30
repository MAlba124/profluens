//! Mux then demux in a real pipeline reads every track back bit-exact, with its timestamps.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libflac::{FlacEncoder, SampleFormat};
use pf_mkv::ebml::id;
use pf_mkv::{MatroskaWriter, MkvDemux, MkvMux, TrackConfig};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "bytesrc",
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

struct ByteSrc {
    data: Vec<u8>,
    chunk: usize,
    pos: usize,
}

impl ByteSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for ByteSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.pos = 0;
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok),
        };
        let cap = buf.memory.capacity();
        let n = self.chunk.min(cap).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any("flac"), OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "recsink",
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

#[derive(Default, Debug)]
struct Recorded {
    bytes: Vec<u8>,
    ptss: Vec<Option<u64>>,
}

struct RecSink {
    rec: Arc<Mutex<Recorded>>,
}

impl RecSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.bytes.extend_from_slice(buf.memory.data());
            rec.ptss.push(buf.pts.nanos());
        }
        Ok(Flow::Ok)
    }
}

fn make_flac(
    sample_rate: u32,
    channels: u32,
    block: usize,
    n: usize,
    seed: i32,
) -> (Vec<u8>, Vec<Vec<u8>>) {
    let (mut enc, mut header) =
        FlacEncoder::new_streaming(sample_rate, channels, SampleFormat::S16, block as u32).unwrap();
    let mut frames = Vec::new();
    let mut phase = seed;
    for _ in 0..n {
        let mut pcm = Vec::new();
        for _ in 0..block {
            for c in 0..channels {
                let v = ((phase.wrapping_mul(41) + c as i32 * 900) & 0x3FFF) as i16 - 0x2000;
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

fn header_prefix(stream: &[u8]) -> Vec<u8> {
    let cluster =
        stream.windows(4).position(|w| w == id::CLUSTER).expect("stream has at least one Cluster");
    stream[..cluster].to_vec()
}

fn run_demux(stream: Vec<u8>, chunk: usize, track_numbers: &[u64]) -> Vec<(u64, Recorded)> {
    let header = header_prefix(&stream);
    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: stream, chunk, pos: 0 });
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(ByteSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), track_numbers.len(), "one src pad per track");

    let mut recs: Vec<(u64, Arc<Mutex<Recorded>>)> = Vec::new();
    for (i, ap) in added.iter().enumerate() {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let sink = p.add(RecSink { rec: Arc::clone(&rec) });
        p.link(ap.element.src(ap.pad), sink.sink(RecSink::IN)).expect("pad -> sink");
        recs.push((track_numbers[i], rec));
    }

    p.run().expect("run");

    recs.into_iter()
        .map(|(tn, rec)| (tn, Arc::try_unwrap(rec).unwrap().into_inner().unwrap()))
        .collect()
}

#[test]
fn single_track_element_roundtrip() {
    let (codec_private, frames) = make_flac(48_000, 2, 4096, 6, 1);

    let stream = mux_with_element(codec_private.clone(), &frames);

    let recs = run_demux(stream, 4096, &[1]);
    assert_eq!(recs.len(), 1, "one track");
    let (tn, rec) = &recs[0];
    assert_eq!(*tn, 1);

    let mut want = codec_private.clone();
    for f in &frames {
        want.extend_from_slice(f);
    }
    assert_eq!(rec.bytes, want, "reconstructed native FLAC stream is head + frames, bit-exact");
    assert_eq!(&rec.bytes[..4], b"fLaC", "reconstruction begins with the native fLaC marker");
}

#[test]
fn two_track_library_roundtrip() {
    let (cp1, frames1) = make_flac(48_000, 2, 4096, 5, 1);
    let (cp2, frames2) = make_flac(44_100, 1, 4096, 5, 777);

    let tracks = vec![
        TrackConfig::flac(1, cp1.clone(), 48_000.0, 2, 16),
        TrackConfig::flac(2, cp2.clone(), 44_100.0, 1, 16),
    ];
    let mut w = MatroskaWriter::new(tracks);
    let mut stream = Vec::new();
    w.write_header(&mut stream).unwrap();
    let dur = 85_000_000u64; // ~one 48k block, ns
    for i in 0..5 {
        w.write_frame(&mut stream, 1, i as u64 * dur, &frames1[i], true).unwrap();
        w.write_frame(&mut stream, 2, i as u64 * dur, &frames2[i], true).unwrap();
    }
    w.finalize(&mut stream);

    let recs = run_demux(stream, 500, &[1, 2]);
    assert_eq!(recs.len(), 2, "two tracks → two src pads");

    let (_, r1) = recs.iter().find(|(tn, _)| *tn == 1).expect("track 1");
    let mut want1 = cp1.clone();
    for f in &frames1 {
        want1.extend_from_slice(f);
    }
    assert_eq!(r1.bytes, want1, "track 1 reconstructed bit-exact");

    let (_, r2) = recs.iter().find(|(tn, _)| *tn == 2).expect("track 2");
    let mut want2 = cp2.clone();
    for f in &frames2 {
        want2.extend_from_slice(f);
    }
    assert_eq!(r2.bytes, want2, "track 2 reconstructed bit-exact");

    assert_ne!(r1.bytes, r2.bytes, "the two tracks demultiplexed distinctly");
}

#[test]
fn timestamps_survive_roundtrip() {
    let (codec_private, frames) = make_flac(48_000, 2, 4096, 4, 1);
    let stream = mux_with_element(codec_private, &frames);
    let recs = run_demux(stream, 4096, &[1]);
    let (_, rec) = &recs[0];

    let block_ns = 4096u64 * 1_000_000_000 / 48_000; // ~85_333_333 ns
    let scale = 1_000_000u64; // default TimestampScale (1 ms/tick)
    let expected_frame_pts: Vec<u64> = (0..frames.len() as u64)
        .map(|i| {
            let ns = i * block_ns;
            ((ns + scale / 2) / scale) * scale // round to nearest tick, back to ns
        })
        .collect();

    let got: Vec<u64> = rec.ptss.iter().map(|p| p.expect("demuxed buffers carry a pts")).collect();
    assert_eq!(got.len(), frames.len() + 1, "codec head + one buffer per frame");
    assert_eq!(got[0], expected_frame_pts[0], "codec head rides the first frame's pts");
    assert_eq!(&got[1..], &expected_frame_pts[..], "each frame's pts matches the muxed timeline");
}

fn mux_with_element(codec_private: Vec<u8>, frames: &[Vec<u8>]) -> Vec<u8> {
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(FrameSrc { frames: frames.to_vec(), next: 0 });
    let mux = p.add(MkvMux::flac(codec_private, 48_000.0, 2, 16));
    let sink = p.add(ByteCollect { got: Arc::clone(&got) });
    p.link(src.src(FrameSrc::OUT), mux.sink(MkvMux::IN)).expect("src -> mux");
    p.link(mux.src(MkvMux::OUT), sink.sink(ByteCollect::IN)).expect("mux -> sink");
    p.run().expect("run mux");

    got.lock().unwrap().clone()
}

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

struct ByteCollect {
    got: Arc<Mutex<Vec<u8>>>,
}
impl ByteCollect {
    const IN: SinkPad = SinkPad(0);
}

impl Element for ByteCollect {
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
