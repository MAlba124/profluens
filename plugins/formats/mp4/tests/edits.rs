//! Edit lists (ISO/IEC 14496-12 8.6.6) on the presentation timeline, end to end.

// Tests own their fixtures and their recordings.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libav::LibavH264Dec as H264Dec;
use pf_mp4::{Mp4Demux, Mp4Reader};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const SCALE: i64 = 15_360;
const PICTURE: i64 = 1024;

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(path).expect("the fixture")
}

fn boxes(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let (mut out, mut at) = (Vec::new(), 0usize);
    while at + 8 <= data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = data[at + 4..at + 8].try_into().unwrap();
        let end = if size == 0 { data.len() } else { at + size };
        out.push((kind, at, end));
        at = end;
    }
    out
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn elst(entries: &[(u32, i32)]) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend((entries.len() as u32).to_be_bytes());
    for &(segment, media) in entries {
        body.extend(segment.to_be_bytes());
        body.extend(media.to_be_bytes());
        body.extend(0x0001_0000u32.to_be_bytes());
    }
    boxed(b"edts", &boxed(b"elst", &body))
}

fn rebuild(kind: &[u8; 4], body: &[u8], edts: &[u8], shift: i64) -> Vec<u8> {
    match kind {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
            let mut out = Vec::new();
            for (child, start, end) in boxes(body) {
                let inner = &body[start + 8..end];
                match &child {
                    b"edts" if kind == b"trak" => {}
                    b"tkhd" => {
                        out.extend_from_slice(&body[start..end]);
                        out.extend_from_slice(edts);
                    }
                    _ => out.extend(rebuild(&child, inner, edts, shift)),
                }
            }
            boxed(kind, &out)
        }
        b"stco" => {
            let mut out = body[..8].to_vec();
            for row in body[8..].chunks(4) {
                let at = u32::from_be_bytes(row.try_into().unwrap()) as i64 + shift;
                out.extend((at as u32).to_be_bytes());
            }
            boxed(kind, &out)
        }
        _ => boxed(kind, body),
    }
}

fn with_edits(entries: &[(u32, i32)]) -> Vec<u8> {
    edited("tiny_h264.mp4", entries)
}

fn edited(name: &str, entries: &[(u32, i32)]) -> Vec<u8> {
    let file = fixture(name);
    let tops = boxes(&file);
    let &(_, start, end) = tops.iter().find(|b| &b.0 == b"moov").expect("a moov");
    let body = &file[start + 8..end];
    let edts = elst(entries);
    let grown = rebuild(b"moov", body, &edts, 0).len() as i64 - (end - start) as i64;
    let moov = rebuild(b"moov", body, &edts, grown);
    [&file[..start], &moov, &file[end..]].concat()
}

fn samples(file: &[u8]) -> Vec<(f64, bool)> {
    let reader = Mp4Reader::new(file).expect("the movie resolves");
    reader.samples().iter().map(|s| (s.pts as f64 / PICTURE as f64, s.hidden)).collect()
}

fn shown(from: f64, n: usize) -> Vec<(f64, bool)> {
    (0..n).map(|k| (from + k as f64, false)).collect()
}

fn stamp(n: i64) -> u64 {
    (n * PICTURE * 1_000_000_000 / SCALE) as u64
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
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = buf.memory.capacity().min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

static PIXFMTS: [ValueDesc; 1] = [ValueDesc::Id("i420")];
static RAW_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Set(&PIXFMTS), preferred: None },
];
static RAW_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &RAW_FIELDS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_OFFERS }];
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

struct Record(Arc<Mutex<Vec<u64>>>);

impl Element for Record {
    fn desc(&self) -> &'static ElementDesc {
        &RECORD_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.0.lock().unwrap().push(buf.pts.nanos().expect("a stamped picture"));
        }
        Ok(Flow::Ok)
    }
}

fn decoded(file: Vec<u8>) -> Vec<u64> {
    let mut p = Pipeline::new();
    let head = file.clone();
    let src = p.add(ByteSrc { data: file, pos: 0 });
    let demux = p.add(Mp4Demux::new(head));
    p.link(src.src(SrcPad(0)), demux.sink(Mp4Demux::IN)).expect("link");
    let added = p.preroll().expect("preroll").srcs;
    let dec = p.add(H264Dec::new());
    let shown = Arc::new(Mutex::new(Vec::new()));
    let sink = p.add(Record(Arc::clone(&shown)));
    p.link(added[0].element.src(added[0].pad), dec.sink(H264Dec::IN)).expect("link");
    p.link(dec.src(H264Dec::OUT), sink.sink(SinkPad(0))).expect("link");
    p.run().expect("run");

    shown.lock().unwrap().clone()
}

#[test]
fn a_cut_out_of_the_middle_shows_both_edits_back_to_back() {
    let file = with_edits(&[(200, 0), (400, 9 * PICTURE as i32)]);
    let want: Vec<(f64, bool)> = [shown(0.0, 3), vec![(3.0, true); 6], shown(3.0, 6)].concat();
    assert_eq!(samples(&file), want);
    assert_eq!(decoded(file), (0..9).map(stamp).collect::<Vec<_>>());
}

#[test]
fn a_leading_empty_edit_starts_the_track_later() {
    let file = with_edits(&[(500, -1), (1000, 0)]);
    assert_eq!(samples(&file), shown(7.5, 15));
    let late = (0..15)
        .map(|n| ((SCALE / 2 + n * PICTURE) * 1_000_000_000 / SCALE) as u64)
        .collect::<Vec<_>>();
    assert_eq!(decoded(file), late);
}

#[test]
fn a_leading_media_time_hides_what_comes_before_it() {
    let file = with_edits(&[(600, 6 * PICTURE as i32)]);
    assert_eq!(samples(&file), [vec![(0.0, true); 6], shown(0.0, 9)].concat());
    assert_eq!(decoded(file), (0..9).map(stamp).collect::<Vec<_>>());
}

#[test]
fn a_seek_into_a_later_edit_restarts_at_its_sync_sample() {
    let reader = Mp4Reader::new(&with_edits(&[(200, 0), (400, 9 * PICTURE as i32)])).unwrap();
    let tables = reader.tables();
    let first = reader.samples()[0].offset;
    let r = tables.restart(stamp(5), stamp(5), &|_| true);
    assert_eq!((r.to_byte, r.at_ns), (first, 0));
    let index = tables.seek_index(&|_| true);
    assert_eq!(index.entries.iter().map(|e| (e.time_ns, e.byte)).collect::<Vec<_>>(), [(0, first)]);
    assert_eq!(index.duration.nanos(), Some(600_000_000));
}

#[test]
fn with_b_frames_the_decoder_shows_what_the_edits_present() {
    let lead = 2 * PICTURE as i32;
    for entries in [
        vec![(200, lead), (400, lead + 9 * PICTURE as i32)],
        vec![(300, lead + 4 * PICTURE as i32)],
        vec![(250, -1), (500, lead)],
    ] {
        let file = edited("bframes_h264.mp4", &entries);
        let reader = Mp4Reader::new(&file).expect("the movie resolves");
        assert!(reader.samples().iter().any(|s| s.hidden) || entries[0].1 == -1, "{entries:?}");
        let mut want: Vec<u64> = reader
            .samples()
            .iter()
            .filter(|s| !s.hidden)
            .map(|s| reader.ticks_to_ns(0, s.pts))
            .collect();
        want.sort_unstable();
        assert_eq!(decoded(file), want, "{entries:?}");
    }
}

#[test]
fn the_edits_set_the_duration_and_what_is_not_followed_is_said() {
    let reader = Mp4Reader::new(&with_edits(&[(200, 0), (400, 9 * PICTURE as i32)])).unwrap();
    assert_eq!(reader.tracks()[0].duration_ns, 600_000_000);
    assert_eq!(reader.tracks()[0].edit_refused, None);
    let back = Mp4Reader::new(&with_edits(&[(200, 9 * PICTURE as i32), (200, 0)])).unwrap();
    assert_eq!(back.tracks()[0].edit_refused, Some(pf_mp4::edit::EditRefusal::Backwards));
    assert_eq!(back.samples().iter().filter(|s| !s.hidden).count(), 3, "the first edit plays");
    let past = Mp4Reader::new(&with_edits(&[(200, 100 * PICTURE as i32)])).unwrap();
    assert_eq!(past.tracks()[0].edit_refused, Some(pf_mp4::edit::EditRefusal::NothingPresented));
    assert_eq!(past.samples().len(), 15, "nothing is dropped");
}
