//! A muxed A_FLAC track demuxes and decodes bit-exact through `FlacDec`.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libflac::{FlacDec, FlacDecoder, FlacEncoder, SampleFormat};
use pf_mkv::ebml::id;
use pf_mkv::{MatroskaWriter, MkvDemux, TrackConfig};
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

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("audio/raw")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "pcmsink",
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

struct PcmSink {
    got: Arc<Mutex<Vec<u8>>>,
}
impl PcmSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for PcmSink {
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

fn signal(frames: usize, channels: u32) -> Vec<i16> {
    let mut v = Vec::with_capacity(frames * channels as usize);
    let mut phase = 0i32;
    for _ in 0..frames {
        for c in 0..channels {
            let s = ((phase.wrapping_mul(31) + c as i32 * 1234) & 0x3FFF) as i16 - 0x2000;
            v.push(s);
            phase = phase.wrapping_add(1);
        }
    }
    v
}

fn encode(samples: &[i16], channels: u32, rate: u32) -> (Vec<u8>, Vec<u8>, Vec<Vec<u8>>) {
    let (mut enc, mut header) =
        FlacEncoder::new_streaming(rate, channels, SampleFormat::S16, 4096).unwrap();
    let block = 4096usize;
    let per = channels as usize;
    let mut frames = Vec::new();
    let mut i = 0usize;
    while i < samples.len() {
        let end = (i + block * per).min(samples.len());
        let mut pcm = Vec::with_capacity((end - i) * 2);
        for &s in &samples[i..end] {
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        enc.encode_frames(&pcm, &mut frames).unwrap();
        i = end;
    }
    let body = enc.finish_frames(&mut frames);
    let off = pf_libflac::streaminfo_offset();
    header[off..off + body.len()].copy_from_slice(&body);
    let codec_private = header.clone();
    let mut native = header;
    for f in &frames {
        native.extend_from_slice(f);
    }
    (native, codec_private, frames)
}

fn mux(codec_private: &[u8], frames: &[Vec<u8>], rate: u32, channels: u32) -> Vec<u8> {
    mux_delayed(codec_private, frames, rate, channels, 0)
}

fn mux_delayed(
    codec_private: &[u8],
    frames: &[Vec<u8>],
    rate: u32,
    channels: u32,
    delay_ns: u64,
) -> Vec<u8> {
    let mut track = TrackConfig::flac(1, codec_private.to_vec(), rate as f64, channels, 16);
    track.codec_delay_ns = delay_ns;
    let mut w = MatroskaWriter::new(vec![track]);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let block_ns = 4096u64 * 1_000_000_000 / rate as u64;
    for (i, f) in frames.iter().enumerate() {
        w.write_frame(&mut out, 1, i as u64 * block_ns, f, true).unwrap();
    }
    w.finalize(&mut out);
    out
}

fn header_prefix(stream: &[u8]) -> Vec<u8> {
    let cluster = stream.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster");
    stream[..cluster].to_vec()
}

fn run(stream: Vec<u8>) -> Vec<u8> {
    let header = header_prefix(&stream);
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: stream, chunk: 1024, pos: 0 });
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(ByteSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 1, "one A_FLAC track → one src pad");
    let dec = p.add(FlacDec::new());
    let sink = p.add(PcmSink { got: Arc::clone(&got) });
    p.link(added[0].element.src(added[0].pad), dec.sink(FlacDec::IN))
        .expect("demux pad -> flacdec");
    p.link(dec.src(FlacDec::OUT), sink.sink(PcmSink::IN)).expect("flacdec -> pcmsink");

    p.run().expect("run");

    got.lock().unwrap().clone()
}

fn pcm_to_i64(bytes: &[u8]) -> Vec<i64> {
    bytes.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]]) as i64).collect()
}

#[test]
fn muxed_a_flac_decodes_bit_exact_through_flacdec() {
    for &(frames, ch, rate) in &[
        (1usize, 2u32, 44_100u32), // one interchannel sample, one short frame
        (4096, 1, 48_000),         // exactly one full block, mono
        (10_000, 2, 44_100),       // several frames (> block), stereo
        (13_337, 1, 44_100),       // several frames, mono, non-block-multiple
    ] {
        let samples = signal(frames, ch);
        let expected: Vec<i64> = samples.iter().map(|&s| s as i64).collect();

        let (native, codec_private, flac_frames) = encode(&samples, ch, rate);

        let ref_dec = FlacDecoder::decode(&native).expect("native decode");
        assert_eq!(ref_dec.samples, expected, "native FLAC lossless (frames={frames} ch={ch})");

        let stream = mux(&codec_private, &flac_frames, rate, ch);
        let pcm = run(stream);
        let decoded = pcm_to_i64(&pcm);
        assert_eq!(
            decoded, expected,
            "A_FLAC mux → demux → flacdec bit-exact (frames={frames} ch={ch} rate={rate})"
        );
    }
}

#[test]
fn a_declared_codec_delay_reaches_the_decoder_and_is_cut_from_the_start() {
    let (frames, channels, rate) = (10_000usize, 2u32, 48_000u32);
    let samples = signal(frames, channels);
    let (_, codec_private, packets) = encode(&samples, channels, rate);
    let whole = run(mux(&codec_private, &packets, rate, channels));
    assert_eq!(whole.len(), frames * 4, "every sample without a delay");
    let block_and_one = 4097 * 1_000_000_000 / u64::from(rate);
    let cases = [
        (23_020_833, 1105),
        (block_and_one, 4097),
        (7_200_000_000_000, frames),
        (u64::MAX, frames),
    ];
    for (delay_ns, cut) in cases {
        let stream = mux_delayed(&codec_private, &packets, rate, channels, delay_ns);
        let mut reader = pf_mkv::MatroskaReader::new();
        reader.push(&header_prefix(&stream)).expect("the head parses");
        assert_eq!(reader.tracks()[0].codec_delay_ns, delay_ns, "read as written");
        let out = run(stream);
        assert_eq!(out.len(), (frames - cut) * 4, "a delay of {delay_ns} ns cuts {cut} samples");
        assert!(out == whole[cut * 4..], "a delay of {delay_ns} ns, what is left is the rest");
    }
}
