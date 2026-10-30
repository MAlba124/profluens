//! `mp4v` sample entries promoted by the esds objectTypeIndication (ISO/IEC 14496-1 7.2.6.6)
//! to `mpeg4/asp`, `mpeg2` and `mpeg1`, through `Mp4Reader`, `Mp4Demux` and pf-libav.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libav::{LibavMpeg2Dec, LibavMpeg4Dec};
use pf_mp4::{Mp4Demux, Mp4Reader, Reframer, TrackKind};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const W: i64 = 128;
const H: i64 = 96;
const FRAMES: usize = 25;
const SYNC: [usize; 3] = [0, 10, 20];
const FILES: [(&str, &str); 3] =
    [("mp4v_mpeg4.mp4", "mpeg4/asp"), ("mp4v_mpeg2.mp4", "mpeg2"), ("mp4v_mpeg1.mp4", "mpeg1")];

fn fixture(name: &str) -> Vec<u8> {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

fn head_through_moov(file: &[u8]) -> Vec<u8> {
    let mut at = 0usize;
    while at + 8 <= file.len() {
        let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        if &file[at + 4..at + 8] == b"mdat" {
            return file[..at].to_vec();
        }
        if size < 8 {
            break;
        }
        at += size;
    }
    file.to_vec()
}

fn has(data: &[u8], code: u8) -> bool {
    data.windows(4).any(|w| w == [0, 0, 1, code])
}

#[test]
fn mp4v_tracks_take_their_family_and_decoder_specific_info() {
    for (name, family) in FILES {
        let file = fixture(name);
        let r = Mp4Reader::new(&head_through_moov(&file)).expect("resolves");
        let t = &r.tracks()[0];
        assert_eq!((t.family(), t.kind()), (family, TrackKind::Video), "{name}");
        assert_eq!((t.entry.width, t.entry.height), (W as u32, H as u32), "{name}");
        assert!(matches!(t.reframer(), Reframer::Passthrough), "{name}: plain frames");
        assert_eq!(t.codec_head(), t.entry.config_record, "{name}: the DSI leads in band");
        let samples = r.samples();
        assert_eq!(samples.len(), FRAMES, "{name}");
        let sync: Vec<usize> = (0..FRAMES).filter(|&i| samples[i].sync).collect();
        assert_eq!(sync, SYNC, "{name}: sync samples from stss");
        let first = &samples[0];
        let first = &file[first.offset as usize..][..first.size as usize];
        if family == "mpeg4/asp" {
            // visual_object_sequence_start_code and a video_object_layer_start_code.
            let dsi = &t.entry.config_record;
            assert!(dsi.starts_with(&[0, 0, 1, 0xB0]) && has(dsi, 0x20), "{dsi:02X?}");
            assert!(!has(first, 0x20) && !has(first, 0xB0), "no VOL in band, the DSI is needed");
        } else {
            // FFmpeg writes no DSI for MPEG-1 and MPEG-2, the sequence header is in band.
            assert!(t.entry.config_record.is_empty(), "{name}");
            assert!(first.starts_with(&[0, 0, 1, 0xB3]), "{name}");
        }
    }
}

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
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let n = 4096.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

static DIMS: [FieldDesc; 2] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
];
static CODED_OFFERS: [OfferDesc; 3] = [
    OfferDesc { family: "mpeg4/asp", fields: &DIMS },
    OfferDesc { family: "mpeg2", fields: &DIMS },
    OfferDesc { family: "mpeg1", fields: &DIMS },
];
static PIXFMTS: [ValueDesc; 1] = [ValueDesc::Id("i420")];
static RAW_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Set(&PIXFMTS), preferred: None },
];
static RAW_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &RAW_FIELDS }];
static CODED_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &CODED_OFFERS }];
static RAW_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_OFFERS }];
const fn rec_desc(pads: &'static [PadDesc]) -> ElementDesc {
    ElementDesc {
        name: "recsink",
        sinks: pads,
        srcs: &[],
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}
static CODED_DESC: ElementDesc = rec_desc(&CODED_PADS);
static RAW_DESC: ElementDesc = rec_desc(&RAW_PADS);

#[derive(Default)]
struct Recorded {
    buffers: Vec<(BufferFlags, Vec<u8>)>,
    family: String,
    width: i64,
    height: i64,
}

struct RecSink {
    desc: &'static ElementDesc,
    rec: Arc<Mutex<Recorded>>,
}

impl RecSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        if let Some(f) = ctx.negotiated_sink(Self::IN) {
            let int = |name: &str| match ctx.field_id(name).and_then(|id| f.get(id)) {
                Some(Value::Int(v)) => v,
                _ => 0,
            };
            (rec.width, rec.height) = (int("width"), int("height"));
            rec.family = ctx.family_name(f.family).unwrap_or_default().to_string();
        }
        while let Some(buf) = inputs.pop() {
            rec.buffers.push((buf.flags, buf.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }
}

/// `mp4demux` into `dec` when given, else straight into a sink of the coded family.
fn run(name: &str, dec: Option<Box<dyn Element>>) -> Recorded {
    let file = fixture(name);
    let head = head_through_moov(&file);
    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: file, pos: 0 });
    let demux = p.add(Mp4Demux::new(head));
    p.link(src.src(ByteSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 1, "{name}: one video track");
    let rec = Arc::new(Mutex::new(Recorded::default()));
    let desc = if dec.is_some() { &RAW_DESC } else { &CODED_DESC };
    let sink = p.add(RecSink { desc, rec: Arc::clone(&rec) });
    let from = added[0].element.src(added[0].pad);
    match dec {
        Some(dec) => {
            let dec = p.add_boxed(dec);
            p.link(from, dec.sink(SinkPad(0))).expect("demux -> dec");
            p.link(dec.src(SrcPad(0)), sink.sink(RecSink::IN)).expect("dec -> sink");
        }
        None => {
            p.link(from, sink.sink(RecSink::IN)).expect("demux -> sink");
        }
    }
    p.run().expect("run");
    Arc::try_unwrap(rec).ok().expect("sink released").into_inner().unwrap()
}

#[test]
fn the_demuxer_leads_with_the_decoder_specific_info_and_flags_the_sync_samples() {
    for (name, family) in FILES {
        let rec = run(name, None);
        assert_eq!((rec.family.as_str(), rec.width, rec.height), (family, W, H), "{name}");
        let file = fixture(name);
        let r = Mp4Reader::new(&head_through_moov(&file)).expect("resolves");
        let dsi = &r.tracks()[0].entry.config_record;
        let media = match rec.buffers.first() {
            Some((flags, head)) if flags.contains(BufferFlags::HEADER) => {
                assert_eq!(head, dsi, "{name}: the head is the DSI");
                &rec.buffers[1..]
            }
            _ => {
                assert!(dsi.is_empty(), "{name}: a DSI must lead");
                &rec.buffers[..]
            }
        };
        assert_eq!(family == "mpeg4/asp", !dsi.is_empty(), "{name}");
        assert_eq!(media.len(), FRAMES, "{name}: a buffer per sample");
        let keys: Vec<usize> =
            (0..FRAMES).filter(|&i| media[i].0.contains(BufferFlags::KEYFRAME)).collect();
        assert_eq!(keys, SYNC, "{name}");
        for (i, (flags, data)) in media.iter().enumerate() {
            let s = &r.samples()[i];
            assert_eq!(data[..], file[s.offset as usize..][..s.size as usize], "{name} {i}");
            assert!(!flags.contains(BufferFlags::HEADER), "{name} {i}");
        }
    }
}

fn decodes(name: &str, dec: impl Element + 'static) {
    let rec = run(name, Some(Box::new(dec)));
    assert_eq!((rec.family.as_str(), rec.width, rec.height), ("video/raw", W, H), "{name}");
    assert_eq!(rec.buffers.len(), FRAMES, "{name}: every frame decodes");
    let i420 = (W * H + 2 * (W / 2) * (H / 2)) as usize;
    assert!(rec.buffers.iter().all(|(_, d)| d.len() == i420), "{name}: full I420 pictures");
}

#[test]
fn mpeg1_and_mpeg2_in_mp4_decode_through_libav() {
    decodes("mp4v_mpeg2.mp4", LibavMpeg2Dec::new());
    decodes("mp4v_mpeg1.mp4", LibavMpeg2Dec::new());
}

/// FFmpeg's mpeg4 decoder reverts the size a VOL states when its packet holds no VOP
/// (`libavcodec/h263dec.c` `ff_h263_decode_frame`), so pf-libav opens with the DSI as extradata.
#[test]
fn mpeg4_in_mp4_decodes_through_libav() {
    decodes("mp4v_mpeg4.mp4", LibavMpeg4Dec::new());
}
