// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mp4::Mp4Demux;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
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

static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any("h264/annexb"), OfferDesc::any("bytes")];
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
    lens: Vec<usize>,
    flags: Vec<BufferFlags>,
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
            rec.lens.push(buf.memory.data().len());
            rec.flags.push(buf.flags);
        }
        Ok(Flow::Ok)
    }
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

fn head_through_moov(file: &[u8]) -> Vec<u8> {
    let mut at = 0usize;
    while at + 8 <= file.len() {
        let size =
            u32::from_be_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]) as usize;
        if &file[at + 4..at + 8] == b"mdat" {
            return file[..at].to_vec();
        }
        let advance = if size == 0 {
            file.len() - at
        } else if size == 1 {
            u64::from_be_bytes(file[at + 8..at + 16].try_into().unwrap()) as usize
        } else {
            size
        };
        if advance == 0 {
            break;
        }
        at += advance;
    }
    file.to_vec()
}

fn run_demux(file: Vec<u8>, chunk: usize) -> Vec<Recorded> {
    let head = head_through_moov(&file);
    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: file, chunk, pos: 0 });
    let demux = p.add(Mp4Demux::new(head));
    p.link(src.src(ByteSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    let mut recs: Vec<Arc<Mutex<Recorded>>> = Vec::new();
    for ap in &added {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let sink = p.add(RecSink { rec: Arc::clone(&rec) });
        p.link(ap.element.src(ap.pad), sink.sink(RecSink::IN)).expect("pad -> sink");
        recs.push(rec);
    }

    p.run().expect("run");
    recs.into_iter().map(|rec| Arc::try_unwrap(rec).unwrap().into_inner().unwrap()).collect()
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[test]
fn single_h264_track_reconstructs_annex_b() {
    let file = fixture_bytes("tiny_h264.mp4");
    let recs = run_demux(file, 4096);
    assert_eq!(recs.len(), 1, "one track → one src pad");
    let rec = &recs[0];

    assert_eq!(&rec.bytes[..4], &START_CODE, "reconstruction begins with an Annex B start code");
    assert_eq!(rec.lens.len(), 16, "codec head + one buffer per sample");
    assert!(rec.flags[0].contains(BufferFlags::HEADER), "the head is flagged");
    assert!(
        rec.flags[1..].iter().all(|f| !f.contains(BufferFlags::HEADER)),
        "media is not: {:?}",
        rec.flags
    );
    let mut off = 0;
    for &len in &rec.lens {
        assert!(len >= 4, "each buffer is at least a start code + payload");
        assert_eq!(
            &rec.bytes[off..off + 4],
            &START_CODE,
            "each output buffer starts with a start code"
        );
        off += len;
    }
    assert_eq!(off, rec.bytes.len(), "buffer lengths tile the reconstructed stream");
}

#[test]
fn demuxed_timestamps_are_present_and_monotone() {
    let file = fixture_bytes("tiny_h264.mp4");
    let recs = run_demux(file, 4096);
    let rec = &recs[0];
    let ptss: Vec<u64> =
        rec.ptss.iter().map(|p| p.expect("every demuxed buffer carries a pts")).collect();
    assert_eq!(ptss.len(), 16);
    assert_eq!(ptss[0], ptss[1], "codec head rides the first sample's pts");
    assert_eq!(ptss[0], 0, "first sample presents at 0");
    for w in ptss[1..].windows(2) {
        assert!(w[1] > w[0], "sample pts strictly increase: {} !> {}", w[1], w[0]);
    }
    let last = *ptss.last().unwrap();
    let expected = 14u64 * 1_000_000_000 / 15; // ~933_333_333 ns
    assert!(
        last.abs_diff(expected) < 2_000_000,
        "last pts ~ 14/15 s (got {last}, want ~{expected})"
    );
}

#[test]
fn ragged_chunking_is_chunk_size_invariant() {
    let file = fixture_bytes("tiny_h264.mp4");
    let reference = run_demux(file.clone(), 1 << 20); // whole-file chunk
    let ref_rec = &reference[0];

    for &chunk in &[1usize, 2, 3, 7, 13, 64, 137, 512, 1000, 4096] {
        let got = run_demux(file.clone(), chunk);
        assert_eq!(got.len(), 1, "chunk {chunk}: still one track");
        assert_eq!(got[0].bytes, ref_rec.bytes, "chunk {chunk}: reconstructed stream identical");
        assert_eq!(got[0].ptss, ref_rec.ptss, "chunk {chunk}: pts sequence identical");
        assert_eq!(got[0].lens, ref_rec.lens, "chunk {chunk}: per-buffer framing identical");
    }
}

#[test]
fn bframes_fixture_demuxes() {
    let file = fixture_bytes("bframes_h264.mp4");
    let recs = run_demux(file, 4096);
    assert_eq!(recs.len(), 1);
    let rec = &recs[0];
    assert_eq!(rec.lens.len(), 16, "codec head + 15 samples");
    assert!(rec.ptss.iter().all(|p| p.is_some()), "every buffer carries a pts");
    assert_eq!(&rec.bytes[..4], &START_CODE);
}
