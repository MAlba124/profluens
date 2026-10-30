//! `OggMux` and `OggDemux` end to end in a real [`Pipeline`].

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_ogg::header_flags;
use pf_ogg::{OggDemux, OggMux, OggWriter, PageHeader, demux_all};
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

    fn new(packets: Vec<Vec<u8>>) -> Self {
        Self { packets, next: 0 }
    }
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
            "test packet {} ({} bytes) exceeds pool slot ({}), would split the boundary",
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

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "packetsink",
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

struct PacketSink {
    got: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl PacketSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for PacketSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.got.lock().unwrap().push(buf.memory.data().to_vec());
        }
        Ok(Flow::Ok)
    }
}

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

fn run_roundtrip(packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(PacketSrc::new(packets));
    let mux = p.add(OggMux::new());
    let demux = p.add(OggDemux::new());
    let sink = p.add(PacketSink { got: Arc::clone(&got) });
    p.link(src.src(PacketSrc::OUT), mux.sink(OggMux::IN)).expect("src->mux");
    p.link(mux.src(OggMux::OUT), demux.sink(OggDemux::IN)).expect("mux->demux");
    p.link(demux.src(OggDemux::OUT), sink.sink(PacketSink::IN)).expect("demux->sink");
    p.run().expect("run");

    got.lock().unwrap().clone()
}

#[test]
fn roundtrip_preserves_packets_and_boundaries() {
    let packets: Vec<Vec<u8>> = vec![
        vec![1, 2, 3, 4],
        vec![],                                 // nil packet
        (0..500u32).map(|i| i as u8).collect(), // multi-segment, one page
        vec![0xAB; 10],
        (0..70_000u32).map(|i| (i * 7) as u8).collect(), // spans >1 page via 255-lacing
        vec![9],
    ];
    let got = run_roundtrip(packets.clone());
    assert_eq!(got.len(), packets.len(), "same number of packets out as in");
    for (i, (a, b)) in packets.iter().zip(got.iter()).enumerate() {
        assert_eq!(a, b, "packet {i} bytes and boundary preserved");
    }
}

#[test]
fn roundtrip_many_small_packets() {
    let packets: Vec<Vec<u8>> =
        (0..1000u32).map(|i| vec![(i & 0xff) as u8; (i % 7) as usize]).collect();
    let got = run_roundtrip(packets.clone());
    assert_eq!(got.len(), packets.len());
    for (i, (a, b)) in packets.iter().zip(got.iter()).enumerate() {
        assert_eq!(a, b, "packet {i}");
    }
}

#[test]
fn roundtrip_all_zero_length_packets() {
    let packets: Vec<Vec<u8>> = vec![vec![], vec![], vec![]];
    let got = run_roundtrip(packets.clone());
    assert_eq!(got, packets);
}

#[test]
fn roundtrip_single_packet() {
    let packets = vec![vec![42, 43, 44]];
    let got = run_roundtrip(packets.clone());
    assert_eq!(got, packets);
}

fn mux_pipeline(packets: Vec<Vec<u8>>, serial: u32) -> Vec<u8> {
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let src = p.add(PacketSrc::new(packets));
    let mux = p.add(OggMux::with_serial(serial));
    let sink = p.add(ByteSink { got: Arc::clone(&got) });
    p.link(src.src(PacketSrc::OUT), mux.sink(OggMux::IN)).expect("src->mux");
    p.link(mux.src(OggMux::OUT), sink.sink(ByteSink::IN)).expect("mux->sink");
    p.run().expect("run");

    got.lock().unwrap().clone()
}

fn page_flags(stream: &[u8]) -> Vec<u8> {
    let mut off = 0;
    let mut flags = Vec::new();
    while off < stream.len() {
        let page = PageHeader::parse(&stream[off..]).expect("mux emitted a valid page");
        off += page.len();
        flags.push(page.header_type());
    }
    assert_eq!(off, stream.len(), "mux output is exactly a sequence of whole pages");
    flags
}

#[test]
fn mux_pipeline_output_parses_as_valid_ogg() {
    let packets: Vec<Vec<u8>> =
        vec![vec![1, 2, 3, 4], vec![], (0..500u32).map(|i| i as u8).collect(), vec![7]];
    let stream = mux_pipeline(packets.clone(), 0xABCD_1234);
    let flags = page_flags(&stream);
    assert!(!flags.is_empty(), "mux produced pages");
    assert_eq!(flags[0] & header_flags::BOS, header_flags::BOS, "first page bos");

    let got = demux_all(&stream);
    assert_eq!(got.len(), packets.len(), "every packet recovered");
    for (i, (p, g)) in packets.iter().zip(got.iter()).enumerate() {
        assert_eq!(&g.data, p, "packet {i} bytes");
        assert_eq!(g.serial, 0xABCD_1234, "packet {i} serial stamped by mux");
    }
    assert!(got.first().unwrap().bos, "first packet tagged bos");
}

#[test]
fn mux_finish_contract_terminates_with_eos() {
    let packets: Vec<&[u8]> = vec![&[1, 2, 3, 4], &[], &[9; 500], &[7]];
    let serial = 1234;

    let mut w = OggWriter::new(serial);
    let mut stream = Vec::new();
    for &pkt in &packets {
        w.write_packet(&mut stream, pkt, 0).expect("write");
        w.flush(&mut stream); // per-packet page flush, exactly like the element
    }
    w.finish(&mut stream); // the eos page the element flushes at end-of-stream

    let got = demux_all(&stream);
    assert_eq!(got.len(), packets.len() + 1, "N packets + trailing nil eos packet");
    for (i, (p, g)) in packets.iter().zip(got.iter()).enumerate() {
        assert_eq!(&g.data, *p, "packet {i}");
        assert_eq!(g.serial, serial);
    }
    let trailing = got.last().unwrap();
    assert!(trailing.data.is_empty(), "trailing eos packet is nil");
    let flags = page_flags(&stream);
    assert_eq!(
        flags.last().copied().unwrap() & header_flags::EOS,
        header_flags::EOS,
        "last page carries eos"
    );
    assert!(trailing.eos, "final packet tagged eos");
    assert!(got.first().unwrap().bos, "first packet tagged bos");
}

#[test]
// A test's own fixtures.
#[allow(clippy::disallowed_methods)]
fn the_demuxer_publishes_the_duration_from_the_last_page() {
    const SERIAL: u32 = 0x77;
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2]);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    let mut file = Vec::new();
    pf_ogg::write_page(&mut file, header_flags::BOS, 0, SERIAL, 0, &[head.len() as u8], &head);
    pf_ogg::write_page(&mut file, 0, 0, SERIAL, 1, &[4], b"tags");
    for seq in 2..10u32 {
        let granule = 48_000 * u64::from(seq - 1) + 312;
        let flags = if seq == 9 { header_flags::EOS } else { 0 };
        pf_ogg::write_page(&mut file, flags, granule, SERIAL, seq, &[200], &[seq as u8; 200]);
    }
    let len = file.len() as u64;
    for (tail, want) in [(file.clone(), Some(8_000_000_000u64)), (Vec::new(), None)] {
        let got = Arc::new(Mutex::new(Vec::new()));
        let mut p = Pipeline::new();
        let src = p.add(PacketSrc::new(file.chunks(100).map(<[u8]>::to_vec).collect()));
        let demux = p.add(OggDemux::with_extent(Some(len), tail));
        let sink = p.add(PacketSink { got: Arc::clone(&got) });
        p.link(src.src(PacketSrc::OUT), demux.sink(OggDemux::IN)).expect("src->demux");
        p.link(demux.src(OggDemux::OUT), sink.sink(PacketSink::IN)).expect("demux->sink");
        p.run().expect("run");
        let index = p.index(demux).expect("published");
        assert_eq!(index.duration.nanos(), want);
        assert_eq!(index.file_len, Some(len));
        assert!(index.entries.is_empty() && !index.complete, "{index:?}");
        assert_eq!(got.lock().unwrap().len(), 10, "every packet still comes out");
    }
}
