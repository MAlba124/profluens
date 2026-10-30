//! PCM to FLAC to Ogg-FLAC through `oggdemux ! oggflacdeframe ! flacdec` is bit-exact.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

mod support;

use pf_libflac::{FlacDec, FlacDecoder, FlacEncoder, OggFlacDeframe, SampleFormat};
use pf_ogg::{OggDemux, OggMux};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

fn signal(frames: usize, channels: u32) -> Vec<i16> {
    let mut v = Vec::with_capacity(frames * channels as usize);
    for i in 0..frames {
        for c in 0..channels {
            let phase = 2.0 * std::f64::consts::PI * (3.0 + c as f64) * i as f64 / 200.0;
            v.push((9000.0 * phase.sin()) as i16);
        }
    }
    v
}

fn interleave_le(samples: &[i16]) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    pcm
}

fn encode_ogg_flac_packets(samples: &[i16], channels: u32, rate: u32) -> Vec<Vec<u8>> {
    let pcm = interleave_le(samples);
    let (mut enc, mut header) = FlacEncoder::new(rate, channels, SampleFormat::S16).expect("enc");
    let mut frames = Vec::new();
    enc.encode_interleaved(&pcm, &mut frames).expect("encode");
    let body = enc.finish(&mut frames);
    header[pf_libflac::streaminfo_offset()..pf_libflac::streaminfo_offset() + body.len()]
        .copy_from_slice(&body);
    let whole = [header.as_slice(), frames.as_slice()].concat();
    let (head, cuts) = support::split_frames(&whole);
    assert_eq!(head.end, header.len());
    let frame_packets: Vec<Vec<u8>> = cuts.into_iter().map(|r| whole[r].to_vec()).collect();

    assert_eq!(&header[..4], b"fLaC", "native FLAC signature");
    let mut blocks: Vec<&[u8]> = Vec::new();
    let mut p = 4usize;
    loop {
        let bh = &header[p..p + 4];
        let last = bh[0] & 0x80 != 0;
        let len = ((bh[1] as usize) << 16) | ((bh[2] as usize) << 8) | bh[3] as usize;
        blocks.push(&header[p..p + 4 + len]);
        p += 4 + len;
        if last {
            break;
        }
    }
    assert_eq!(p, header.len(), "metadata chain consumes the whole native header");

    let further_headers = (blocks.len() - 1) as u16;
    let mut mapping = vec![0x7F];
    mapping.extend_from_slice(b"FLAC");
    mapping.extend_from_slice(&[0x01, 0x00]); // mapping version 1.0
    mapping.extend_from_slice(&further_headers.to_be_bytes()); // header packet count, big-endian
    mapping.extend_from_slice(b"fLaC");
    mapping.extend_from_slice(blocks[0]); // STREAMINFO (block header + body)

    let mut packets: Vec<Vec<u8>> = Vec::new();
    packets.push(mapping);
    for b in &blocks[1..] {
        packets.push(b.to_vec());
    }
    packets.extend(frame_packets);
    packets
}

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "packetsrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active, // a real source: its own group, drives the pipeline
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct PacketSrc {
    packets: Vec<Vec<u8>>,
    next: usize,
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
        if self.next >= self.packets.len() {
            return Ok(Flow::Eos);
        }
        let pkt = &self.packets[self.next];
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok), // pool full, try again next turn
        };
        assert!(
            pkt.len() <= buf.memory.capacity(),
            "test packet {} ({} bytes) exceeds pool slot ({})",
            self.next,
            pkt.len(),
            buf.memory.capacity()
        );
        buf.memory.as_mut_full()[..pkt.len()].copy_from_slice(pkt);
        buf.memory.set_len(pkt.len());
        ctx.out(Self::OUT).push(buf);
        self.next += 1;
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
    sched: SchedHint::Active, // its own group, so buffers cross a real ring
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

fn pcm_to_i64(bytes: &[u8]) -> Vec<i64> {
    bytes.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]]) as i64).collect()
}

fn run_pipeline(packets: Vec<Vec<u8>>) -> Vec<u8> {
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(PacketSrc { packets, next: 0 });
    let mux = p.add(OggMux::new());
    let demux = p.add(OggDemux::new());
    let deframe = p.add(OggFlacDeframe::new());
    let dec = p.add(FlacDec::new());
    let sink = p.add(PcmSink { got: Arc::clone(&got) });
    p.link(src.src(PacketSrc::OUT), mux.sink(OggMux::IN)).expect("src->mux");
    p.link(mux.src(OggMux::OUT), demux.sink(OggDemux::IN)).expect("mux->demux");
    p.link(demux.src(OggDemux::OUT), deframe.sink(OggFlacDeframe::IN)).expect("demux->deframe");
    p.link(deframe.src(OggFlacDeframe::OUT), dec.sink(FlacDec::IN)).expect("deframe->flacdec");
    p.link(dec.src(FlacDec::OUT), sink.sink(PcmSink::IN)).expect("flacdec->sink");
    p.run().expect("run");

    got.lock().unwrap().clone()
}

#[test]
fn ogg_flac_deframe_roundtrip_is_bit_exact() {
    for &(frames, ch, rate) in &[
        (1usize, 2u32, 44100u32), // one interchannel sample, single short frame
        (600, 1, 48000),          // sub-block mono
        (10_000, 2, 44100),       // several frames (> 4608 block), stereo
        (13_337, 1, 44100),       // several frames, mono, non-block-multiple
    ] {
        let samples = signal(frames, ch);
        let expected: Vec<i64> = samples.iter().map(|&s| s as i64).collect();

        let packets = encode_ogg_flac_packets(&samples, ch, rate);
        assert!(packets.len() >= 2, "at least the mapping header + one frame packet");

        let native = reconstruct_native(&packets);
        let ref_dec = FlacDecoder::decode(&native).expect("native decode");
        assert_eq!(
            ref_dec.samples, expected,
            "native FLAC decode lossless (frames={frames} ch={ch})"
        );
        assert_eq!(ref_dec.info.channels, ch);
        assert_eq!(ref_dec.info.sample_rate, rate);

        let pcm = run_pipeline(packets);
        let decoded = pcm_to_i64(&pcm);
        assert_eq!(
            decoded, expected,
            "Ogg-FLAC de-frame round trip must be bit-exact (frames={frames} ch={ch} rate={rate})"
        );
    }
}

fn reconstruct_native(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut native = Vec::new();
    native.extend_from_slice(&packets[0][9..]); // drop 0x7F "FLAC" v.v count
    for pkt in &packets[1..] {
        native.extend_from_slice(pkt);
    }
    native
}

#[test]
fn mapping_header_packet_has_the_expected_shape() {
    let samples = signal(100, 2);
    let packets = encode_ogg_flac_packets(&samples, 2, 44100);
    let h = &packets[0];
    assert_eq!(h[0], 0x7F, "mapping packet type");
    assert_eq!(&h[1..5], b"FLAC", "mapping signature");
    assert_eq!(&h[5..7], &[0x01, 0x00], "mapping version 1.0");
    assert_eq!(&h[7..9], &0u16.to_be_bytes(), "header packet count 0");
    assert_eq!(&h[9..13], b"fLaC", "native FLAC signature after the mapping prefix");
    assert_eq!(h[13] & 0x80, 0x80, "STREAMINFO is the last metadata block");
    assert_eq!(h[13] & 0x7F, 0, "block type 0 == STREAMINFO");
    assert_eq!(h.len(), 9 + 4 + 4 + 34, "mapping packet is 9 + fLaC + STREAMINFO");
}
