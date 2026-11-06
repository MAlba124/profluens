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
use profluens_core::pull::Ask;

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

/// Serves the demuxer's asks, a pulled run.
struct RangeSrc(Vec<u8>);

impl Element for RangeSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.offer_ranges(SrcPad(0));
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(Ask { offset, len, tag, mut buf }) = ctx.next_ask(SrcPad(0)) {
            let from = (offset as usize).min(self.0.len());
            let to = (from + len as usize).min(self.0.len());
            buf.memory.as_mut_full()[..to - from].copy_from_slice(&self.0[from..to]);
            buf.memory.set_len(to - from);
            ctx.serve(SrcPad(0), tag, buf);
        }
        Ok(if ctx.asks_over(SrcPad(0)) { Flow::Eos } else { Flow::Ok })
    }
}

/// Every pad's cues, the file pushed or pulled.
fn every_pad(file: Vec<u8>, pull: bool) -> Vec<Vec<Cue>> {
    let mut p = Pipeline::new();
    let head = file.clone();
    let src = if pull { p.add(RangeSrc(file)) } else { p.add(ByteSrc(file, 0)) };
    let demux = p.add(Mp4Demux::new(head));
    p.link(src.src(SrcPad(0)), demux.sink(Mp4Demux::IN)).expect("link");
    let added = p.preroll().expect("preroll").srcs;
    let recs: Vec<_> = added
        .iter()
        .map(|pad| {
            let rec = Arc::new(Mutex::new((Vec::new(), None)));
            let sink = p.add(Record(Arc::clone(&rec)));
            p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).expect("link");
            rec
        })
        .collect();
    p.run().expect("run");
    recs.iter().map(|rec| std::mem::take(&mut rec.lock().unwrap().0)).collect()
}

/// A cue beside video, pushed or read by asks, lasts its sample's stts duration, an empty sample
/// a gap, the last one to the track's end.
#[test]
fn a_cue_beside_video_lasts_its_samples_duration_pushed_or_pulled() {
    let text = [(500, tx3g("")), (1_500, tx3g("one")), (700, tx3g("")), (2_300, tx3g("two"))];
    let file = movie_beside_video(&text, 6);
    for pull in [false, true] {
        let pads = every_pad(file.clone(), pull);
        let cue = |at: u64, dur: u64, text: &str| (at * MS, Some(dur * MS), text.to_string());
        assert_eq!(pads[1], [cue(500, 1_500, "one"), cue(2_700, 2_300, "two")], "pulled {pull}");
    }
}

/// `movie` beside a `vp09` track of `pictures` one second keyframes, the text chunk stored ahead
/// of the video chunk.
fn movie_beside_video(text: &[(u32, Vec<u8>)], pictures: u32) -> Vec<u8> {
    let mut vp09 = vec![0u8; 78];
    vp09[7] = 1;
    vp09[24..28].copy_from_slice(&[0, 64, 0, 48]);
    let pics: Vec<(u32, Vec<u8>)> = (0..pictures)
        .map(|n| {
            let mut b = vec![b'V'];
            b.extend(n.to_be_bytes());
            b.resize(8, 0);
            (1000, b)
        })
        .collect();
    let stbl = |entry: &[u8], samples: &[(u32, Vec<u8>)], offset: u32, sync: bool| {
        let mut stts = be32(&[samples.len() as u32]);
        samples.iter().for_each(|(d, _)| stts.extend(be32(&[1, *d])));
        let mut stsz = be32(&[0, samples.len() as u32]);
        samples.iter().for_each(|(_, b)| stsz.extend(be32(&[b.len() as u32])));
        let mut stbl = [
            full(b"stsd", &[&be32(&[1])[..], entry].concat()),
            full(b"stts", &stts),
            full(b"stsz", &stsz),
            full(b"stsc", &be32(&[1, 1, samples.len() as u32, 1])),
            full(b"stco", &be32(&[1, offset])),
        ]
        .concat();
        if sync {
            let all: Vec<u32> = (1..=samples.len() as u32).collect();
            let mut stss = be32(&[all.len() as u32]);
            stss.extend(be32(&all));
            stbl.extend(full(b"stss", &stss));
        }
        stbl
    };
    let trak = |id: u32, handler: Option<&[u8; 4]>, total: u32, dims: [u32; 2], stbl: Vec<u8>| {
        let mut mdhd = vec![0u8; 8];
        mdhd.extend(be32(&[1000, total, 0x55c4_0000]));
        let mut mdia = full(b"mdhd", &mdhd);
        if let Some(h) = handler {
            let mut hdlr = vec![0u8; 4];
            hdlr.extend_from_slice(h);
            hdlr.extend([0u8; 13]);
            mdia.extend(full(b"hdlr", &hdlr));
        }
        mdia.extend(boxed(b"minf", &boxed(b"stbl", &stbl)));
        let mut tkhd = vec![0u8; 8];
        tkhd.extend(be32(&[id, 0, total]));
        tkhd.extend([0u8; 52]);
        tkhd.extend(be32(&[dims[0] << 16, dims[1] << 16]));
        boxed(b"trak", &[full(b"tkhd", &tkhd), boxed(b"mdia", &mdia)].concat())
    };
    let text_total: u32 = text.iter().map(|s| s.0).sum();
    let build = |text_at: u32, video_at: u32| {
        let mut mvhd = vec![0u8; 8];
        mvhd.extend(be32(&[1000, text_total.max(pictures * 1000)]));
        mvhd.extend([0u8; 80]);
        let video = stbl(&boxed(b"vp09", &vp09), &pics, video_at, true);
        let text = stbl(&tx3g_entry(), text, text_at, false);
        let moov = [
            full(b"mvhd", &mvhd),
            trak(1, None, pictures * 1000, [64, 48], video),
            trak(2, Some(b"text"), text_total, [0, 0], text),
        ]
        .concat();
        [boxed(b"ftyp", b"isom\0\0\0\0isom"), boxed(b"moov", &moov)].concat()
    };
    let text_at = build(0, 0).len() as u32 + 8;
    let text_bytes: Vec<u8> = text.iter().flat_map(|(_, b)| b.clone()).collect();
    let head = build(text_at, text_at + text_bytes.len() as u32);
    let data = [text_bytes, pics.into_iter().flat_map(|(_, b)| b).collect()].concat();
    [head, boxed(b"mdat", &data)].concat()
}

/// A cue sample stored ahead of the keyframe a seek lands on is gone back for while its track is
/// on, not past the reach, and a track that is off moves nothing.
#[test]
fn a_seek_goes_back_for_the_cue_up_on_a_track_that_is_on() {
    const S: u64 = 1_000_000_000;
    let text = [(2_000, tx3g("")), (3_000, tx3g("Up")), (35_000, tx3g("")), (1_000, tx3g("Last"))];
    let file = movie_beside_video(&text, 41);
    let mut reader = Mp4Reader::new(&file).expect("resolves");
    let tables = reader.tables();
    let offset = |track: usize, n: usize| {
        let mut of = tables.samples().iter().filter(|s| usize::from(s.track_index) == track);
        of.nth(n).expect("the sample").offset
    };
    let on = tables.seek_index(&|_| true);
    let entry = |index: &profluens_core::pipeline::Index, sec: u64| {
        index.entries.iter().find(|e| e.time_ns == sec * S).copied().expect("an entry")
    };
    // 3 s is inside "Up", 1 s inside the gap before it, 30 s past the reach of the gap from 5 s
    assert_eq!((entry(&on, 3).restart, entry(&on, 3).restart_ns), (offset(1, 1), 2 * S));
    assert_eq!(entry(&on, 1).restart, offset(1, 0));
    assert_eq!(entry(&on, 30).restart, offset(0, 30), "{:?}", entry(&on, 30));
    assert_eq!(entry(&on, 40).restart, offset(1, 3));
    let off = tables.seek_index(&|_| false);
    assert!(off.entries.iter().all(|e| e.restart == e.byte), "{:?}", off.entries);
    // the stream restarted at the entry's byte gates the text track at the cue
    let r = reader.seek_stream(entry(&on, 3).restart, Some(3 * S), 3 * S);
    assert_eq!((r.to_byte, r.at_ns), (offset(1, 1), 2 * S));
    reader.select(1, false);
    let r = reader.seek_stream(offset(0, 3), Some(3 * S), 3 * S);
    assert_eq!(r.to_byte, offset(0, 3));
}

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
