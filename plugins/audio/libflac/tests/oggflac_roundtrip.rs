//! PCM to FLAC to Ogg-FLAC through `oggdemux ! oggflacdeframe ! flacdec` is bit-exact, and the
//! `oggflacdec` the `flac/ogg` lookup returns decodes to the same PCM.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

mod support;

use pf_harness::Harness;
use pf_libflac::SampleFormat;
use pf_libflac::{FlacDec, FlacDecoder, FlacEncoder, FlacError, OggFlacDec, OggFlacDeframe};
use pf_ogg::{OggDemux, OggMux};
use pf_registry::decoder_for;
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_elements::io::FileSrc;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/out/ogg_flac.oga");

/// What decodes the demuxer's `flac/ogg` packets.
#[derive(Clone, Copy, Debug)]
enum Chain {
    Split,
    Lookup,
}

/// The decode `chain` linked from `from`, its last element.
fn add_chain(p: &mut Pipeline, chain: Chain, from: (ElementId, SrcPad)) -> ElementId {
    let into = match chain {
        Chain::Split => {
            let deframe = p.add(OggFlacDeframe::new());
            let dec = p.add(FlacDec::new());
            p.link(deframe.src(OggFlacDeframe::OUT), dec.sink(FlacDec::IN)).expect("deframe->dec");
            (deframe, dec)
        }
        Chain::Lookup => {
            let desc = decoder_for(&[pf_libflac::ELEMENTS], "flac/ogg").expect("flac/ogg decoder");
            assert_eq!(desc.name, "oggflacdec");
            let dec = p.add_boxed((desc.make_default.expect("constructible"))());
            (dec, dec)
        }
    };
    p.link(from.0.src(from.1), into.0.sink(SinkPad(0))).expect("demux->chain");
    into.1
}

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

fn run_pipeline(packets: Vec<Vec<u8>>, chain: Chain) -> Vec<u8> {
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(PacketSrc { packets, next: 0 });
    let mux = p.add(OggMux::new());
    let demux = p.add(OggDemux::new());
    let sink = p.add(PcmSink { got: Arc::clone(&got) });
    p.link(src.src(PacketSrc::OUT), mux.sink(OggMux::IN)).expect("src->mux");
    p.link(mux.src(OggMux::OUT), demux.sink(OggDemux::IN)).expect("mux->demux");
    let dec = add_chain(&mut p, chain, (demux, OggDemux::OUT));
    p.link(dec.src(FlacDec::OUT), sink.sink(PcmSink::IN)).expect("flacdec->sink");
    p.run().expect("run");

    got.lock().unwrap().clone()
}

fn decode_file(path: &str, chain: Chain) -> Vec<u8> {
    let got = Arc::new(Mutex::new(Vec::new()));
    let len = std::fs::metadata(path).expect("fixture").len();
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(path));
    let demux = p.add(OggDemux::with_len(Some(len)));
    let sink = p.add(PcmSink { got: Arc::clone(&got) });
    p.link(src.src(FileSrc::OUT), demux.sink(OggDemux::IN)).expect("src->demux");
    let dec = add_chain(&mut p, chain, (demux, OggDemux::OUT));
    p.link(dec.src(FlacDec::OUT), sink.sink(PcmSink::IN)).expect("dec->sink");
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

        for chain in [Chain::Split, Chain::Lookup] {
            let pcm = run_pipeline(packets.clone(), chain);
            let decoded = pcm_to_i64(&pcm);
            assert_eq!(
                decoded, expected,
                "Ogg-FLAC {chain:?} round trip must be bit-exact (frames={frames} ch={ch} rate={rate})"
            );
        }
    }
}

#[test]
fn the_fixture_decodes_through_the_lookup_as_through_deframe_then_flacdec() {
    if !std::path::Path::new(FIXTURE).exists() {
        eprintln!("skipped, no {FIXTURE}, cargo xtask fixtures makes it");
        return;
    }
    let split = decode_file(FIXTURE, Chain::Split);
    let lookup = decode_file(FIXTURE, Chain::Lookup);
    assert!(split.len() > 64 * 1024, "the fixture decodes to PCM, {} bytes", split.len());
    assert_eq!(lookup.len(), split.len(), "the same number of PCM bytes");
    assert!(lookup == split, "the same PCM through oggflacdec as through oggflacdeframe ! flacdec");
}

/// The PCM `packets` decode to, one packet a buffer.
fn harness_decode(h: &mut Harness, packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in packets {
        let buf = h.alloc(p);
        h.push(buf).expect("push");
        h.drain_outputs().iter().for_each(|b| out.extend_from_slice(b.memory.data()));
    }
    h.eos().expect("eos").iter().for_each(|b| out.extend_from_slice(b.memory.data()));
    out
}

/// The mapping packet and the further header packets it counts.
fn heads(packets: &[Vec<u8>]) -> usize {
    1 + usize::from(u16::from_be_bytes([packets[0][7], packets[0][8]]))
}

/// The demuxer sends the mapping head once a run, a seek past the first page none.
#[test]
fn oggflacdec_strips_the_head_each_run_and_keeps_it_across_a_flush() {
    let samples = signal(12_000, 2);
    let packets = encode_ogg_flac_packets(&samples, 2, 44_100);
    let pcm = interleave_le(&samples);
    let mut h = Harness::new(OggFlacDec::new());
    assert!(harness_decode(&mut h, &packets) == pcm, "the first run");
    h.stop();
    assert!(harness_decode(&mut h, &packets) == pcm, "a run after a stop");
    h.stop();

    let mut h = Harness::new(OggFlacDec::new());
    let (head, frames) = packets.split_at(heads(&packets));
    for p in head {
        let buf = h.alloc(p);
        h.push(buf).expect("push");
    }
    drop(h.drain_outputs());
    h.seek(0, profluens_core::time::Timestamp(0)).expect("seek");
    assert!(harness_decode(&mut h, frames) == pcm, "frames alone after a flush");
}

/// The headers with a last APPLICATION block holding a copy of the first frame. A decoder
/// searching for a frame sync in headers fed as bytes would play it.
fn with_a_frame_in_the_head(mut packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let n = heads(&packets);
    let last = if n == 1 { 13 } else { 0 };
    packets[n - 1][last] &= 0x7F;
    let frame = packets[n].clone();
    let len = u32::try_from(4 + frame.len()).expect("a block length");
    let mut block = vec![0x82];
    block.extend_from_slice(&len.to_be_bytes()[1..]);
    block.extend_from_slice(b"test");
    block.extend_from_slice(&frame);
    packets.insert(n, block);
    let count = u16::from_be_bytes([packets[0][7], packets[0][8]]) + 1;
    packets[0][7..9].copy_from_slice(&count.to_be_bytes());
    packets
}

/// A seek to the start reads the file from byte 0, the demuxer sends the headers again ahead of
/// the frames. Fed to the decoder as bytes they would be junk it resyncs past, a frame sync
/// inside them played.
#[test]
fn oggflacdec_drops_the_headers_sent_again_behind_a_flush() {
    let samples = signal(12_000, 2);
    let packets = with_a_frame_in_the_head(encode_ogg_flac_packets(&samples, 2, 44_100));
    let pcm = interleave_le(&samples);
    let mut h = Harness::new(OggFlacDec::new());
    let half = heads(&packets) + (packets.len() - heads(&packets)) / 2;
    for p in &packets[..half] {
        let buf = h.alloc(p);
        h.push(buf).expect("push");
    }
    drop(h.drain_outputs());
    h.seek(0, profluens_core::time::Timestamp(0)).expect("seek");
    assert!(harness_decode(&mut h, &packets) == pcm, "the whole stream again");
    assert!(h.bus_messages().is_empty(), "nothing posted");
}

#[test]
fn oggflacdec_fails_on_a_first_packet_without_the_mapping_head() {
    let packets = encode_ogg_flac_packets(&signal(4_000, 1), 1, 44_100);
    let mut h = Harness::new(OggFlacDec::new());
    let buf = h.alloc(&packets[heads(&packets)]);
    let err = h.push(buf).expect_err("no mapping head");
    let source = std::error::Error::source(&err).expect("an element's failure");
    assert_eq!(source.downcast_ref::<FlacError>(), Some(&FlacError::MappingSignature), "{err}");
}

/// The PCM, then what the playing decoder and a fresh one linked mid-run decoded. The fresh one
/// replaces the playing one when `cut`, else joins beside it.
fn decode_relinked(cut: bool) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use profluens_core::clock::MockClock;
    use profluens_core::sched::{Outcome, Stepped};
    use profluens_core::time::Timestamp;
    use profluens_core::topology::Leave;
    use profluens_elements::flow::Queue;

    fn pass(run: &mut Stepped) -> bool {
        run.service();
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        for g in &ready {
            run.pass(*g).expect("pass");
        }
        !ready.is_empty()
    }

    let samples = signal(400_000, 2);
    let pcm = interleave_le(&samples);
    let packets = encode_ogg_flac_packets(&samples, 2, 44_100);
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p = Pipeline::with(Default::default(), false, Arc::new(clock.clone()));
    let src = p.add(PacketSrc { packets, next: 0 });
    let mux = p.add(OggMux::new());
    let demux = p.add(OggDemux::new());
    p.link(src.src(PacketSrc::OUT), mux.sink(OggMux::IN)).expect("src->mux");
    p.link(mux.src(OggMux::OUT), demux.sink(OggDemux::IN)).expect("mux->demux");
    let (first, second) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
    // the queue heads a branch that leaves whole, as the streamer's audio chain
    let branch = |p: &mut Pipeline, got: &Arc<Mutex<Vec<u8>>>| {
        let q = p.add(Queue::new());
        let dec = p.add(OggFlacDec::new());
        let sink = p.add(PcmSink { got: Arc::clone(got) });
        p.link(demux.src(OggDemux::OUT), q.sink(Queue::IN)).expect("demux->queue");
        p.link(q.src(Queue::OUT), dec.sink(OggFlacDec::IN)).expect("queue->dec");
        p.link(dec.src(OggFlacDec::OUT), sink.sink(PcmSink::IN)).expect("dec->sink");
        [sink, dec, q]
    };
    let playing = branch(&mut p, &first);
    let mut run = p.step(Arc::new(clock)).expect("step");
    while first.lock().unwrap().is_empty() {
        assert!(pass(&mut run), "the first decoder plays");
    }
    if cut {
        for el in playing {
            p.remove(el, Leave::Cut).expect("remove the branch");
        }
    }
    branch(&mut p, &second);
    p.commit().expect("commit");
    while pass(&mut run) {}
    assert!((0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit)), "the run ends");
    run.finish().expect("finish");
    let (first, second) = (first.lock().unwrap().clone(), second.lock().unwrap().clone());
    (pcm, first, second)
}

/// The ABR's relink, a fresh `oggflacdec` put in place of the playing one mid-run, decodes on
/// from the head `oggdemux` sends it again.
#[test]
fn a_fresh_oggflacdec_linked_mid_run_decodes_on() {
    let (pcm, first, second) = decode_relinked(true);
    assert!(pcm.starts_with(&first), "the first decoder played the start");
    let lens = (first.len(), second.len());
    assert!(lens.1 > 0 && lens.0 + lens.1 <= pcm.len(), "{lens:?} of {}", pcm.len());
    assert!(pcm.ends_with(&second), "the fresh decoder decodes on to the end");
}

/// A second decoder joining the playing pad decodes from the head `oggdemux` hands it alone.
#[test]
fn an_oggflacdec_joining_a_playing_pad_decodes_on_beside_the_first() {
    let (pcm, first, second) = decode_relinked(false);
    assert!(first == pcm, "the first decoder plays the whole stream, {} bytes", first.len());
    assert!(!second.is_empty() && second.len() < pcm.len(), "{} of {}", second.len(), pcm.len());
    assert!(pcm.ends_with(&second), "the joining decoder decodes on to the end");
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
