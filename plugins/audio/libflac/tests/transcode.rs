//! `flacdec ! flacenc` links on `audio/raw` and the encoder adopts the announced format.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libflac::{FlacDec, FlacDecoder, FlacEnc, FlacEncoder, SampleFormat};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const RATE: u32 = 22_050;
const CHANNELS: u32 = 2;

fn gen_s16(n: usize) -> (Vec<u8>, Vec<i64>) {
    let mut bytes = Vec::with_capacity(n * CHANNELS as usize * 2);
    let mut expected = Vec::with_capacity(n * CHANNELS as usize);
    for i in 0..n {
        for c in 0..CHANNELS {
            let phase = 2.0 * std::f64::consts::PI * (3.0 + c as f64) * (i as f64) / 384.0;
            let s = (11000.0 * phase.sin()).round() as i64;
            expected.push(s);
            bytes.extend_from_slice(&(s as i16).to_le_bytes());
        }
    }
    (bytes, expected)
}

fn encode_stream(pcm: &[u8]) -> Vec<u8> {
    let (mut enc, header) =
        FlacEncoder::new_streaming(RATE, CHANNELS, SampleFormat::S16, 4096).expect("enc");
    let mut out = header;
    enc.encode_interleaved(pcm, &mut out).expect("encode");
    enc.finish(&mut out);
    out
}

static BYTES_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "packetsrc",
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

struct PacketSrc {
    packets: Vec<Vec<u8>>,
    next: usize,
    burst: usize,
}

impl PacketSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for PacketSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.next = 0;
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        for _ in 0..self.burst {
            if self.next >= self.packets.len() {
                return Ok(Flow::Eos);
            }
            let pkt = &self.packets[self.next];
            let Some(mut buf) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            buf.memory.as_mut_full()[..pkt.len()].copy_from_slice(pkt);
            buf.memory.set_len(pkt.len());
            ctx.out(Self::OUT).push(buf);
            self.next += 1;
        }
        Ok(Flow::Ok)
    }
}

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES_OFFERS }];
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

#[test]
fn flacdec_links_to_flacenc_over_audio_raw() {
    let mut p = Pipeline::new();
    let dec = p.add(FlacDec::new());
    let enc = p.add(FlacEnc::new(44_100, 2, SampleFormat::S16));
    let link =
        p.link(dec.src(FlacDec::OUT), enc.sink(FlacEnc::IN)).expect("flacdec ! flacenc links");
    let fam = p.negotiated(link).expect("format fixed").family;
    assert_eq!(p.family_name(fam), Some("audio/raw"), "typed family preferred over bytes");
}

#[test]
fn transcode_is_lossless_and_learns_the_announced_format() {
    let (pcm, expected) = gen_s16(10_000);
    let stream = encode_stream(&pcm);
    let packets: Vec<Vec<u8>> = stream.chunks(1500).map(<[u8]>::to_vec).collect();

    let mut p = Pipeline::new();
    let src = p.add(PacketSrc { packets, next: 0, burst: 1 });
    let dec = p.add(FlacDec::new());
    // Wrong on purpose, only flacdec's FormatChange can make the output correct.
    let enc = p.add(FlacEnc::new(8_000, 1, SampleFormat::S32));
    let got = Arc::new(Mutex::new(Vec::new()));
    let sink = p.add(ByteSink { got: Arc::clone(&got) });
    p.link(src.src(PacketSrc::OUT), dec.sink(FlacDec::IN)).expect("src!dec");
    p.link(dec.src(FlacDec::OUT), enc.sink(FlacEnc::IN)).expect("dec!enc");
    p.link(enc.src(FlacEnc::OUT), sink.sink(ByteSink::IN)).expect("enc!sink");
    p.run().expect("run");

    let flac = got.lock().unwrap().clone();
    assert!(flac.starts_with(b"fLaC"), "re-encoded output is a FLAC stream");
    let out = FlacDecoder::decode(&flac).expect("re-encoded stream decodes");
    assert_eq!(out.info.sample_rate, RATE, "rate learned from the announcement");
    assert_eq!(out.info.channels, CHANNELS, "channels learned from the announcement");
    assert_eq!(out.info.bits_per_sample, 16, "sample format learned from the announcement");
    assert_eq!(out.samples, expected, "decode → re-encode → decode is lossless");
}

#[test]
fn parallel_chains_transcode_in_one_pipeline() {
    const CHAINS: usize = 4;
    let mut p = Pipeline::new();
    p.set_pool(4096, (CHAINS as u32) * 32 + 32);

    let mut expects = Vec::new();
    let mut gots = Vec::new();
    for i in 0..CHAINS {
        let (pcm, expected) = gen_s16(6_000 + i * 3_500);
        let stream = encode_stream(&pcm);
        let packets: Vec<Vec<u8>> = stream.chunks(1500).map(<[u8]>::to_vec).collect();

        let src = p.add(PacketSrc { packets, next: 0, burst: 2 });
        let dec = p.add(FlacDec::new());
        let enc = p.add(FlacEnc::new(8_000, 1, SampleFormat::S32)); // learned per chain
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = p.add(ByteSink { got: Arc::clone(&got) });
        p.link(src.src(PacketSrc::OUT), dec.sink(FlacDec::IN)).expect("src!dec");
        p.link(dec.src(FlacDec::OUT), enc.sink(FlacEnc::IN)).expect("dec!enc");
        p.link(enc.src(FlacEnc::OUT), sink.sink(ByteSink::IN)).expect("enc!sink");
        expects.push(expected);
        gots.push(got);
    }

    let run = std::thread::spawn(move || p.run());
    for _ in 0..600 {
        if run.is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(run.is_finished(), "parallel forest did not reach EOS");
    run.join().expect("joined").expect("run ok");

    for (i, (got, expected)) in gots.iter().zip(&expects).enumerate() {
        let flac = got.lock().unwrap().clone();
        let out = FlacDecoder::decode(&flac).unwrap_or_else(|e| panic!("chain {i}: {e:?}"));
        assert_eq!(out.info.sample_rate, RATE, "chain {i} rate learned");
        assert_eq!(&out.samples, expected, "chain {i} lossless and not cross-wired");
    }
}

#[test]
fn bursty_source_with_starved_pool_does_not_deadlock() {
    // Regression for a livelock where an inlined source filled the pool with decoder input.
    // A small pool, a bursting source and a timeout as the deadlock detector.
    let (pcm, expected) = gen_s16(60_000);
    let stream = encode_stream(&pcm);
    let packets: Vec<Vec<u8>> = stream.chunks(512).map(<[u8]>::to_vec).collect();
    assert!(packets.len() > 64, "enough packets to overwhelm a 16-slot pool");

    let mut p = pf_reactor::pipeline();
    p.set_pool(4096, 16);
    let src = p.add(PacketSrc { packets, next: 0, burst: 4 });
    let dec = p.add(FlacDec::new());
    let enc = p.add(FlacEnc::new(8_000, 1, SampleFormat::S32));
    let got = Arc::new(Mutex::new(Vec::new()));
    let sink = p.add(ByteSink { got: Arc::clone(&got) });
    p.link(src.src(PacketSrc::OUT), dec.sink(FlacDec::IN)).expect("src!dec");
    p.link(dec.src(FlacDec::OUT), enc.sink(FlacEnc::IN)).expect("dec!enc");
    p.link(enc.src(FlacEnc::OUT), sink.sink(ByteSink::IN)).expect("enc!sink");

    let run = std::thread::spawn(move || p.run());
    for _ in 0..600 {
        if run.is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(run.is_finished(), "transcode deadlocked under a starved pool");
    run.join().expect("joined").expect("run ok");

    let flac = got.lock().unwrap().clone();
    let out = FlacDecoder::decode(&flac).expect("output decodes");
    assert_eq!(out.samples, expected, "still lossless under pool pressure");
}
