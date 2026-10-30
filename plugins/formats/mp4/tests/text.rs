// Tests own their fixtures and their recordings.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mp4::{Mp4Demux, Mp4Reader};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    boxed(kind, &[&[0u8; 4][..], body].concat())
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn movie(entry: &[u8], samples: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let build = |offset: u32| {
        let mut stts = be32(&[samples.len() as u32]);
        samples.iter().for_each(|(d, _)| stts.extend(be32(&[1, *d])));
        let mut stsz = be32(&[0, samples.len() as u32]);
        samples.iter().for_each(|(_, b)| stsz.extend(be32(&[b.len() as u32])));
        let stbl = [
            full(b"stsd", &[&be32(&[1])[..], entry].concat()),
            full(b"stts", &stts),
            full(b"stsz", &stsz),
            full(b"stsc", &be32(&[1, 1, samples.len() as u32, 1])),
            full(b"stco", &be32(&[1, offset])),
        ]
        .concat();
        let total: u32 = samples.iter().map(|s| s.0).sum();
        let mut mdhd = vec![0u8; 8];
        mdhd.extend(be32(&[1000, total, 0x55c4_0000]));
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"text");
        hdlr.extend([0u8; 13]);
        let mut tkhd = vec![0u8; 8];
        tkhd.extend(be32(&[1, 0, total]));
        tkhd.extend([0u8; 60]);
        let mdia =
            [full(b"mdhd", &mdhd), full(b"hdlr", &hdlr), boxed(b"minf", &boxed(b"stbl", &stbl))]
                .concat();
        let trak = boxed(b"trak", &[full(b"tkhd", &tkhd), boxed(b"mdia", &mdia)].concat());
        let mut mvhd = vec![0u8; 8];
        mvhd.extend(be32(&[1000, total]));
        mvhd.extend([0u8; 80]);
        [
            boxed(b"ftyp", b"isom\0\0\0\0isom"),
            boxed(b"moov", &[full(b"mvhd", &mvhd), trak].concat()),
        ]
        .concat()
    };
    let head = build(0);
    let head = build(head.len() as u32 + 8);
    let data: Vec<u8> = samples.iter().flat_map(|(_, b)| b.clone()).collect();
    [head, boxed(b"mdat", &data)].concat()
}

fn tx3g(text: &str) -> Vec<u8> {
    let mut b = (text.len() as u16).to_be_bytes().to_vec();
    b.extend_from_slice(text.as_bytes());
    b
}

fn vttc(text: &str) -> Vec<u8> {
    boxed(b"vttc", &[boxed(b"sttg", b"line:90%"), boxed(b"payl", text.as_bytes())].concat())
}

fn tx3g_entry() -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0, 0, 0, 0, 1];
    body.extend([0u8; 30]);
    body.extend(boxed(b"ftab", &[0, 1, 0, 1, 5, b'A', b'r', b'i', b'a', b'l']));
    boxed(b"tx3g", &body)
}

fn wvtt_entry() -> Vec<u8> {
    boxed(b"wvtt", &[&[0u8, 0, 0, 0, 0, 0, 0, 1][..], &boxed(b"vttC", b"WEBVTT")].concat())
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

struct ByteSrc(Vec<u8>, usize);

impl Element for ByteSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.1 >= self.0.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = buf.memory.capacity().min(self.0.len() - self.1).min(7);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.1 += n;
        Ok(Flow::Ok)
    }
}

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static RECORD_DESC: ElementDesc = ElementDesc {
    name: "record",
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

type Cue = (u64, Option<u64>, String);

struct Record(Arc<Mutex<(Vec<Cue>, Option<String>)>>);

impl Element for Record {
    fn desc(&self) -> &'static ElementDesc {
        &RECORD_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let family = ctx
            .negotiated_sink(SinkPad(0))
            .and_then(|f| ctx.family_name(f.family))
            .map(str::to_string);
        let mut rec = self.0.lock().unwrap();
        rec.1 = family.or(rec.1.take());
        while let Some(buf) = inputs.pop() {
            let text = String::from_utf8_lossy(buf.memory.data()).into_owned();
            rec.0.push((buf.pts.nanos().expect("a stamped cue"), buf.duration.nanos(), text));
        }
        Ok(Flow::Ok)
    }
}

fn cues(file: Vec<u8>) -> (Vec<Cue>, Option<String>) {
    let mut p = Pipeline::new();
    let head = file.clone();
    let src = p.add(ByteSrc(file, 0));
    let demux = p.add(Mp4Demux::new(head));
    p.link(src.src(SrcPad(0)), demux.sink(Mp4Demux::IN)).expect("link");
    let added = p.preroll().expect("preroll").srcs;
    let rec = Arc::new(Mutex::new((Vec::new(), None)));
    let sink = p.add(Record(Arc::clone(&rec)));
    p.link(added[0].element.src(added[0].pad), sink.sink(SinkPad(0))).expect("link");
    p.run().expect("run");

    std::mem::take(&mut *rec.lock().unwrap())
}

const MS: u64 = 1_000_000;

#[test]
fn tx3g_cues_come_out_as_subrip_with_their_durations() {
    let samples = [
        (200, tx3g("")),
        (700, tx3g("first cue")),
        (100, tx3g("")),
        (500, [tx3g("second\nline two"), boxed(b"styl", &[0, 0])].concat()),
        (0, tx3g("")),
    ];
    let file = movie(&tx3g_entry(), &samples);
    let reader = Mp4Reader::new(&file).expect("resolves");
    assert_eq!(reader.tracks()[0].family(), "subtitle/srt");
    let (got, family) = cues(file);
    assert_eq!(family.as_deref(), Some("subtitle/srt"));
    let want = [
        (200 * MS, Some(700 * MS), "first cue".to_string()),
        (MS * 1000, Some(500 * MS), "second\nline two".into()),
    ];
    assert_eq!(got, want);
}

#[test]
fn wvtt_cues_come_out_as_webvtt_text() {
    let samples = [
        (400, vttc("hello")),
        (300, boxed(b"vtte", &[])),
        (600, [vttc("<i>one</i>"), boxed(b"vtta", b"NOTE"), vttc("two")].concat()),
        (500, vttc("last")),
    ];
    let (got, family) = cues(movie(&wvtt_entry(), &samples));
    assert_eq!(family.as_deref(), Some("subtitle/vtt"));
    let cue = |at: u64, dur: u64, text: &str| (at * MS, Some(dur * MS), text.to_string());
    assert_eq!(
        got,
        [
            cue(0, 400, "hello"),
            cue(700, 600, "<i>one</i>"),
            cue(700, 600, "two"),
            cue(1300, 500, "last")
        ]
    );
}
