//! `tsdemux` on the ts fixtures against `ffprobe`, decoding, rollover, splices, segments, restarts.

#![allow(clippy::disallowed_methods)] // test fixtures

use std::sync::{Arc, Mutex};

use pf_libav::LibavAudioDec;
use pf_libav::LibavH264Dec as H264Dec;
use pf_tsdemux::TsDemux;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::{Ctx, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::{Pipeline, Rejoin};
use profluens_core::time::Timestamp;
use profluens_elements::flow::Queue;

const SECOND: u64 = 1_000_000_000;

/// The crate's own fixtures first, then pf-ts's.
fn fixture_path(name: &str, ext: &str) -> String {
    let own = format!("{}/tests/fixtures/{name}.{ext}", env!("CARGO_MANIFEST_DIR"));
    match std::path::Path::new(&own).exists() {
        true => own,
        false => format!("{}/../ts/tests/fixtures/{name}.{ext}", env!("CARGO_MANIFEST_DIR")),
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = fixture_path(name, "ts");
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[derive(Clone, Copy, Debug)]
struct Packet {
    stream: usize,
    pts: i64,
    dts: i64,
    size: usize,
    key: bool,
}

fn packets(name: &str) -> Vec<Packet> {
    let path = fixture_path(name, "packets.csv");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    text.lines()
        .map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            let num = |s: &str| s.parse::<i64>().unwrap_or_else(|_| panic!("{line}"));
            Packet {
                stream: num(f[0]) as usize,
                pts: num(f[1]),
                dts: num(f[2]),
                size: num(f[3]) as usize,
                key: f[5].contains('K'),
            }
        })
        .collect()
}

fn ns(ticks: i64, origin: i64) -> u64 {
    (i128::from(ticks) * 100_000 / 9 - i128::from(origin) * 100_000 / 9).max(0) as u64
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "piecesrc",
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

#[derive(Clone, Copy)]
struct Mark {
    reset: bool,
    stream_ns: Option<u64>,
}

struct PieceSrc {
    segments: Vec<(Option<Mark>, Vec<u8>)>,
    chunk: usize,
    at: (usize, usize),
}

impl PieceSrc {
    const OUT: SrcPad = SrcPad(0);

    fn file(data: Vec<u8>, chunk: usize) -> Self {
        Self { segments: vec![(None, data)], chunk, at: (0, 0) }
    }
}

impl Element for PieceSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let (segment, pos) = self.at;
        let Some((mark, bytes)) = self.segments.get(segment) else { return Ok(Flow::Eos) };
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let n = self.chunk.min(buf.memory.capacity()).min(bytes.len() - pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&bytes[pos..pos + n]);
        buf.memory.set_len(n);
        if let (0, Some(mark)) = (pos, mark) {
            buf.flags = BufferFlags::SEGMENT;
            if mark.reset {
                buf.flags |= BufferFlags::DISCONT;
            }
            buf.pts = mark.stream_ns.map_or(Timestamp::NONE, Timestamp);
        }
        ctx.out(Self::OUT).push(buf);
        self.at = if pos + n == bytes.len() { (segment + 1, 0) } else { (segment, pos + n) };
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let (Event::FlushStart, Some(target)) = (event, ctx.seek_target()) {
            self.at = (0, target.to_byte as usize);
        }
        Ok(())
    }
}

static ANY: ConstraintDesc = ConstraintDesc::Any;
static AAC_FIELDS: [FieldDesc; 2] = [
    FieldDesc { field: "rate", allowed: ANY, preferred: None },
    FieldDesc { field: "channels", allowed: ANY, preferred: None },
];
static REC_OFFERS: [OfferDesc; 11] = [
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
    OfferDesc::any("mpeg1"),
    OfferDesc::any("mpeg2"),
    OfferDesc::any("mpeg4/asp"),
    OfferDesc::any("subtitle/dvb"),
    OfferDesc { family: "aac", fields: &AAC_FIELDS },
    OfferDesc::any("ac3"),
    OfferDesc::any("eac3"),
    OfferDesc::any("mp3"),
    OfferDesc::any("bytes"),
];
static REC_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &REC_OFFERS }];
static REC_DESC: ElementDesc = ElementDesc {
    name: "recsink",
    sinks: &REC_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct Recorded {
    units: Vec<(Option<u64>, usize, u32)>,
    bytes: Vec<u8>,
    formats: Vec<(String, Option<i64>, Option<i64>)>,
}

struct RecSink {
    rec: Arc<Mutex<Recorded>>,
}

impl RecSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &REC_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.bytes.extend_from_slice(buf.memory.data());
            rec.units.push((buf.pts.nanos(), buf.memory.len(), buf.flags.0));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(format) = event {
            let int = |name: &str| match ctx.field_id(name).and_then(|id| format.get(id)) {
                Some(Value::Int(v)) => Some(v),
                _ => None,
            };
            let family = ctx.family_name(format.family).unwrap_or("?").to_string();
            self.rec.lock().unwrap().formats.push((family, int("rate"), int("channels")));
        }
        Ok(())
    }
}

fn run(
    src: PieceSrc,
    head: &[u8],
    select: Option<StreamKind>,
) -> Vec<(StreamKind, &'static str, Recorded)> {
    let mut p = Pipeline::new();
    let src = p.add(src);
    let demux = p.add(TsDemux::new(head.to_vec()));
    p.link(src.src(PieceSrc::OUT), demux.sink(TsDemux::IN)).expect("src to demux");
    p.set_select_all(select.is_none());
    let added = p.preroll().expect("preroll").srcs;
    let streams: Vec<(StreamKind, &'static str)> =
        p.streams(demux).iter().map(|s| (s.kind, s.codec)).collect();
    let mut pads: Vec<(usize, ElementId, SrcPad)> =
        added.iter().enumerate().map(|(n, a)| (n, a.element, a.pad)).collect();
    if let Some(kind) = select {
        for (n, (k, _)) in streams.iter().enumerate().filter(|(_, (k, _))| *k == kind) {
            let _ = k;
            pads.push((n, demux, p.select(demux, n as u32).expect("select")));
        }
    }
    let mut out = Vec::new();
    for (n, element, pad) in pads {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let sink = p.add(RecSink { rec: Arc::clone(&rec) });
        p.link(element.src(pad), sink.sink(RecSink::IN)).expect("pad to sink");
        out.push((n, rec));
    }
    p.run().expect("the run ends");
    out.into_iter()
        .map(|(n, rec)| (streams[n].0, streams[n].1, rec.lock().unwrap().clone()))
        .collect()
}

fn whole(name: &str, chunk: usize) -> Vec<(StreamKind, &'static str, Recorded)> {
    let data = fixture(name);
    run(PieceSrc::file(data.clone(), chunk), &data, None)
}

fn origin(of: &[Packet]) -> i64 {
    let first = |stream: usize| of.iter().find(|p| p.stream == stream).map(|p| p.dts);
    (0..4).filter_map(first).min().expect("a packet")
}

#[test]
fn h264_and_aac_leave_as_ffprobe_lists_them() {
    let theirs = packets("h264_aac");
    let base = origin(&theirs);
    let out = whole("h264_aac", 4096);
    assert_eq!(out.len(), 2);
    let (kind, codec, video) = &out[0];
    assert_eq!((*kind, *codec), (StreamKind::Video, "h264/annexb"));
    let rows: Vec<&Packet> = theirs.iter().filter(|p| p.stream == 0).collect();
    assert_eq!(video.units.len(), rows.len());
    for (ours, row) in video.units.iter().zip(&rows) {
        assert_eq!((ours.0, ours.1), (Some(ns(row.pts, base)), row.size), "{row:?}");
    }
    assert_eq!(video.formats, [("h264/annexb".to_string(), None, None)]);
    assert!(
        video.units[0].2 & BufferFlags::KEYFRAME.0 != 0,
        "the first picture is one to start at"
    );
    assert_eq!(&video.bytes[..4], [0, 0, 0, 1], "Annex B");

    let (kind, codec, audio) = &out[1];
    assert_eq!((*kind, *codec), (StreamKind::Audio, "aac"));
    let rows: Vec<&Packet> = theirs.iter().filter(|p| p.stream == 1).collect();
    assert_eq!(audio.formats, [("aac".to_string(), Some(48_000), Some(2))]);
    assert_eq!(audio.units[0].1, 2, "the config leads");
    assert!(audio.units[0].2 & BufferFlags::HEADER.0 != 0, "the config is the head");
    assert!(audio.units[1..].iter().all(|u| u.2 & BufferFlags::HEADER.0 == 0), "the units are not");
    assert_eq!(&audio.bytes[..2], [0x11, 0x90], "AAC-LC, 48 kHz, stereo");
    assert_eq!(audio.units.len(), rows.len() + 1);
    for (ours, row) in audio.units[1..].iter().zip(&rows) {
        assert_eq!(ours.1 + 7, row.size, "{row:?}");
        // Frames behind the first of a PES are stamped by samples since, ffprobe counts ticks.
        let at = ours.0.expect("stamped");
        assert!(at.abs_diff(ns(row.pts, base)) <= 1, "{at} against {row:?}");
    }
    assert_eq!(audio.units[1].0, Some(0), "the audio starts first, at zero");
}

#[test]
fn hevc_and_ac3_leave_as_ffprobe_lists_them() {
    for (name, family) in [("hevc_ac3", "ac3"), ("hevc_eac3_dvb", "eac3")] {
        let theirs = packets(name);
        let base = origin(&theirs);
        let out = whole(name, 4096);
        let (kind, codec, video) = &out[0];
        assert_eq!((*kind, *codec), (StreamKind::Video, "h265/annexb"));
        let rows: Vec<&Packet> = theirs.iter().filter(|p| p.stream == 0).collect();
        assert_eq!(video.units.len(), rows.len(), "{name}");
        for (ours, row) in video.units.iter().zip(&rows) {
            assert_eq!((ours.0, ours.1), (Some(ns(row.pts, base)), row.size), "{name} {row:?}");
        }
        let (kind, codec, audio) = &out[1];
        assert_eq!((*kind, *codec), (StreamKind::Audio, family));
        let rows: Vec<&Packet> = theirs.iter().filter(|p| p.stream == 1).collect();
        assert_eq!(audio.bytes.len(), rows.iter().map(|p| p.size).sum::<usize>(), "{name}");
        assert_eq!(audio.units[0].0, Some(ns(rows[0].pts, base)));
        assert_eq!(audio.formats.len(), 1);
        assert_eq!(audio.formats[0].0, family);
        assert_eq!(&audio.bytes[..2], [0x0B, 0x77], "an AC-3 sync word");
    }
}

#[test]
fn reordered_pictures_keep_their_presentation_times() {
    let theirs = packets("bframes");
    let base = origin(&theirs);
    let out = whole("bframes", 1000);
    let video = &out[0].2;
    assert_eq!(video.units.len(), theirs.len());
    for (ours, row) in video.units.iter().zip(&theirs) {
        assert_eq!((ours.0, ours.1), (Some(ns(row.pts, base)), row.size), "{row:?}");
    }
    assert!(theirs.iter().any(|p| p.pts != p.dts), "the fixture reorders");
    let stamps: Vec<u64> = video.units.iter().filter_map(|u| u.0).collect();
    assert!(stamps.windows(2).any(|w| w[1] < w[0]), "decode order is not presentation order");
}

#[test]
fn layer_two_audio_is_mp3() {
    let out = whole("audio_mp2", 4096);
    assert_eq!(
        (out[0].0, out[0].1),
        (StreamKind::Audio, "mp3"),
        "layer II is mp3 as in Matroska and MP4"
    );
    assert_eq!(out[0].2.bytes.len(), packets("audio_mp2").iter().map(|p| p.size).sum::<usize>());
}

#[test]
fn mpeg_video_leaves_as_ffprobe_lists_it() {
    for (name, family) in
        [("mpeg2", "mpeg2"), ("mpeg2_bframes", "mpeg2"), ("mpeg1", "mpeg1"), ("mpeg4", "mpeg4/asp")]
    {
        let theirs = packets(name);
        let base = origin(&theirs);
        let out = whole(name, 4096);
        assert_eq!(out.len(), 1, "{name}");
        let (kind, codec, video) = &out[0];
        assert_eq!((*kind, *codec), (StreamKind::Video, family), "{name}");
        assert_eq!(video.formats, [(family.to_string(), None, None)], "{name}");
        assert_eq!(video.units.len(), theirs.len(), "{name}");
        for (ours, row) in video.units.iter().zip(&theirs) {
            assert_eq!((ours.0, ours.1), (Some(ns(row.pts, base)), row.size), "{name} {row:?}");
            let key = ours.2 & BufferFlags::KEYFRAME.0 != 0;
            assert_eq!(key, row.key, "{name} {row:?}");
        }
        let keys = theirs.iter().filter(|p| p.key).count();
        assert!(keys >= 2, "{name} has {keys} keys");
        assert_eq!(&video.bytes[..4], [0, 0, 1, if family == "mpeg4/asp" { 0xB0 } else { 0xB3 }]);
    }
    assert!(packets("mpeg2_bframes").iter().any(|p| p.pts != p.dts), "the fixture reorders");
}

#[test]
fn mpeg_video_cut_anywhere_is_the_same() {
    for name in ["mpeg2_bframes", "mpeg4"] {
        let reference = whole(name, 1 << 20);
        for chunk in [1usize, 188, 1000] {
            assert_eq!(whole(name, chunk), reference, "{name} in pieces of {chunk}");
        }
    }
}

#[test]
fn dvb_subtitles_leave_whole_behind_their_page_ids() {
    let theirs = packets("dvbsub");
    let base = origin(&theirs);
    let out = whole("dvbsub", 4096);
    assert_eq!(out.len(), 2);
    assert_eq!((out[0].0, out[0].1), (StreamKind::Video, "mpeg2"));
    let (kind, codec, subs) = &out[1];
    assert_eq!((*kind, *codec), (StreamKind::Text, "subtitle/dvb"));
    assert_eq!(subs.formats, [("subtitle/dvb".to_string(), None, None)]);
    let rows: Vec<&Packet> = theirs.iter().filter(|p| p.stream == 1).collect();
    assert_eq!(subs.units.len(), rows.len() + 1, "the page ids lead");
    let head = subs.units[0];
    assert!(head.2 & BufferFlags::HEADER.0 != 0 && head.1 == 5, "{head:?}");
    // ffmpeg's subtitling_descriptor, page 1 for both, type 0x10, normal.
    assert_eq!(&subs.bytes[..5], [0, 1, 0, 1, 0x10]);
    let mut at = 5;
    for (ours, row) in subs.units[1..].iter().zip(&rows) {
        // ffprobe counts the segments alone.
        assert_eq!((ours.0, ours.1), (Some(ns(row.pts, base)), row.size + 3), "{row:?}");
        assert!(ours.2 & BufferFlags::HEADER.0 == 0);
        let unit = &subs.bytes[at..at + ours.1];
        assert_eq!(&unit[..3], [0x20, 0x00, 0x0F], "the whole PES data field, EN 300 743 table 3");
        assert_eq!(unit[unit.len() - 1], 0xFF, "with its end marker");
        at += ours.1;
    }
}

#[test]
fn timestamps_go_on_across_the_rollover() {
    let theirs = packets("wrap");
    assert!(origin(&theirs) < 0 && theirs.iter().any(|p| p.dts > 0));
    let theirs: Vec<Packet> = theirs
        .iter()
        .map(|p| Packet { pts: p.pts + (1 << 33), dts: p.dts + (1 << 33), ..*p })
        .collect();
    let base = origin(&theirs);
    let out = whole("wrap", 4096);
    let video = &out[0].2;
    let rows: Vec<&Packet> = theirs.iter().filter(|p| p.stream == 0).collect();
    assert_eq!(video.units.len(), rows.len());
    for (ours, row) in video.units.iter().zip(&rows) {
        assert_eq!((ours.0, ours.1), (Some(ns(row.pts, base)), row.size), "{row:?}");
    }
    let last = video.units.iter().filter_map(|u| u.0).max().unwrap();
    assert!(last > SECOND && last < 3 * SECOND, "two seconds of video, {last}");
}

#[test]
fn how_the_input_is_cut_changes_nothing() {
    for name in ["h264_aac", "hevc_ac3", "wrap"] {
        let reference = whole(name, 1 << 20);
        for chunk in [1usize, 7, 188, 1000, 4096] {
            assert_eq!(whole(name, chunk), reference, "{name} in pieces of {chunk}");
        }
    }
}

#[test]
fn only_what_is_selected_is_put_out() {
    let data = fixture("h264_aac");
    let all = run(PieceSrc::file(data.clone(), 4096), &data, None);
    let audio = run(PieceSrc::file(data.clone(), 4096), &data, Some(StreamKind::Audio));
    assert_eq!(audio.len(), 1);
    assert_eq!(audio[0], all[1], "the audio is what it is with the video beside it");
}

#[test]
fn a_stream_spliced_onto_itself_goes_on_from_where_it_was() {
    let data = fixture("h264_aac");
    let twice = [data.clone(), data.clone()].concat();
    let once = run(PieceSrc::file(data.clone(), 4096), &data, None);
    let out = run(PieceSrc::file(twice, 4096), &data, None);
    let (first, both) = (&once[0].2.units, &out[0].2.units);
    assert!(both.len() >= 2 * first.len() - 1 && both.len() <= 2 * first.len());
    let stamps: Vec<u64> = both.iter().filter_map(|u| u.0).collect();
    assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "no B-frames, so the stamps ascend");
    let end = first.last().unwrap().0.unwrap();
    let second = stamps[first.len()];
    assert!(second >= end && second <= end + SECOND / 5, "{second} behind {end}");
    assert!(*stamps.last().unwrap() > 2 * end - SECOND / 2);
}

#[test]
fn a_segment_lands_where_its_source_says() {
    let data = fixture("h264_aac");
    let theirs = packets("h264_aac");
    let first_seen = origin(&theirs);
    let at = |stream_ns| Some(Mark { reset: true, stream_ns: Some(stream_ns) });
    let segments = vec![(at(4 * SECOND), data.clone()), (at(10 * SECOND), data.clone())];
    let out = run(PieceSrc { segments, chunk: 3000, at: (0, 0) }, &data, None);
    let video = &out[0].2.units;
    let rows: Vec<&Packet> = theirs.iter().filter(|p| p.stream == 0).collect();
    assert_eq!(video.len(), 2 * rows.len());
    for (n, (ours, row)) in video.iter().zip(rows.iter().chain(&rows)).enumerate() {
        let start = if n < rows.len() { 4 * SECOND } else { 10 * SECOND };
        assert_eq!(ours.0, Some(start + ns(row.pts, first_seen)), "unit {n}");
    }
    assert_eq!(out[1].2.formats.len(), 1, "an unchanged format is sent on once");
    let configs = out[1].2.units.iter().filter(|u| u.1 == 2).count();
    assert_eq!(configs, 2, "the config leads each stretch");
}

#[test]
fn a_segment_with_no_time_goes_on_in_the_stretch_it_is_in() {
    let data = fixture("h264_aac");
    let half = (data.len() / 2 / 188) * 188;
    let mark = |reset| Some(Mark { reset, stream_ns: None });
    let start = Some(Mark { reset: true, stream_ns: Some(0) });
    let reference = run(
        PieceSrc { segments: vec![(start, data.clone())], chunk: 4096, at: (0, 0) },
        &data,
        None,
    );
    let segments = vec![(start, data[..half].to_vec()), (mark(false), data[half..].to_vec())];
    let out = run(PieceSrc { segments, chunk: 4096, at: (0, 0) }, &data, None);
    assert_eq!(out, reference);
}

#[test]
fn the_page_ids_lead_once_across_spliced_segments() {
    let data = fixture("dvbsub");
    let at = |stream_ns| Some(Mark { reset: true, stream_ns: Some(stream_ns) });
    let segments = vec![(at(0), data.clone()), (at(10 * SECOND), data.clone())];
    let out = run(PieceSrc { segments, chunk: 3000, at: (0, 0) }, &data, None);
    let subs = &out[1].2;
    assert_eq!(out[1].1, "subtitle/dvb");
    let heads: Vec<usize> =
        (0..subs.units.len()).filter(|n| subs.units[*n].2 & BufferFlags::HEADER.0 != 0).collect();
    // A decoder past a splice is not waiting for them and would read them as a broken set.
    assert_eq!(heads, [0], "{:?}", subs.units);
    assert_eq!(subs.units.len(), 1 + 2 * 4);
}

static RAW_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_OFFERS }];
static RAW_OFFERS: [OfferDesc; 3] =
    [OfferDesc::any("video/raw"), OfferDesc::any("audio/raw"), OfferDesc::any("subtitle/bitmap")];
static RAW_DESC: ElementDesc = ElementDesc {
    name: "rawsink",
    sinks: &RAW_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct RawSink {
    rec: Arc<Mutex<Recorded>>,
}

impl Element for RawSink {
    fn desc(&self) -> &'static ElementDesc {
        &RAW_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.units.push((buf.pts.nanos(), buf.memory.len(), buf.flags.0));
            rec.bytes.extend_from_slice(buf.memory.data());
        }
        Ok(Flow::Ok)
    }
}

/// Each stream through a queue and its decoder into a [`RawSink`], none for a stream left out.
/// The queue makes a passive decoder a group of its own beside the others.
fn decode(
    name: &str,
    chunk: usize,
    decoders: impl Fn(&mut Pipeline, StreamKind) -> Option<(ElementId, SinkPad, SrcPad)>,
) -> Vec<(StreamKind, Recorded)> {
    let data = fixture(name);
    let mut p = Pipeline::new();
    let src = p.add(PieceSrc::file(data.clone(), chunk));
    let demux = p.add(TsDemux::new(data));
    p.link(src.src(PieceSrc::OUT), demux.sink(TsDemux::IN)).expect("src to demux");
    p.set_select_all(false);
    p.preroll().expect("preroll");
    let kinds: Vec<StreamKind> = p.streams(demux).iter().map(|s| s.kind).collect();
    let mut out = Vec::new();
    for (n, kind) in kinds.into_iter().enumerate() {
        let Some((dec, dec_in, dec_out)) = decoders(&mut p, kind) else { continue };
        let pad = p.select(demux, n as u32).expect("select");
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let sink = p.add(RawSink { rec: Arc::clone(&rec) });
        let queue = p.add(Queue::new());
        p.link(demux.src(pad), queue.sink(Queue::IN)).expect("demux to queue");
        p.link(queue.src(Queue::OUT), dec.sink(dec_in)).expect("queue to decoder");
        p.link(dec.src(dec_out), sink.sink(SinkPad(0))).expect("decoder to sink");
        out.push((kind, rec));
    }
    p.run().expect("the run ends");
    let mut warnings = Vec::new();
    while let Some(message) = p.bus().try_recv() {
        if let profluens_core::bus::BusMessage::Warning { error, .. } = message {
            warnings.push(error.to_string());
        }
    }
    assert!(warnings.is_empty(), "{name} {warnings:?}");
    out.into_iter().map(|(kind, rec)| (kind, rec.lock().unwrap().clone())).collect()
}

#[test]
fn mpeg_video_decodes() {
    use pf_libav::{LibavMpeg2Dec, LibavMpeg4Dec};
    for name in ["mpeg1", "mpeg2_bframes", "mpeg4"] {
        let out = decode(name, 777, |p, kind| {
            (kind == StreamKind::Video).then(|| match name {
                "mpeg4" => (p.add(LibavMpeg4Dec::new()), LibavMpeg4Dec::IN, LibavMpeg4Dec::OUT),
                _ => (p.add(LibavMpeg2Dec::new()), LibavMpeg2Dec::IN, LibavMpeg2Dec::OUT),
            })
        });
        let pictures = &out[0].1;
        assert_eq!(pictures.units.len(), 50, "{name}, two seconds at 25 pictures a second");
        assert!(pictures.units.iter().all(|u| u.1 == 128 * 96 * 3 / 2), "{name}");
        let stamps: Vec<u64> = pictures.units.iter().filter_map(|u| u.0).collect();
        assert_eq!(stamps.len(), 50, "{name}");
        assert!(stamps.windows(2).all(|w| w[1] == w[0] + SECOND / 25), "{name} {stamps:?}");
    }
}

#[test]
fn dvb_subtitles_decode_to_bitmaps_over_mpeg2() {
    use pf_libav::LibavMpeg2Dec;
    use pf_text::DvbSubDec;
    let theirs = packets("dvbsub");
    let base = origin(&theirs);
    let first_cue = theirs.iter().find(|p| p.stream == 1).expect("a cue").pts;
    let out = decode("dvbsub", 1000, |p, kind| match kind {
        StreamKind::Video => {
            Some((p.add(LibavMpeg2Dec::new()), LibavMpeg2Dec::IN, LibavMpeg2Dec::OUT))
        }
        StreamKind::Text => Some((p.add(DvbSubDec::new()), DvbSubDec::IN, DvbSubDec::OUT)),
        _ => None,
    });
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].1.units.len(), 150, "six seconds of pictures");
    let (kind, subs) = &out[1];
    assert_eq!(*kind, StreamKind::Text);
    let mut at = 0;
    let mut shown = Vec::new();
    for unit in &subs.units {
        let ds = pf_text::pgs::decode_bitmap(&subs.bytes[at..at + unit.1]).expect("a bitmap");
        at += unit.1;
        shown.push((unit.0.expect("timed"), ds));
    }
    let shows: Vec<&(u64, _)> = shown.iter().filter(|(_, ds)| !ds.is_clear()).collect();
    assert_eq!((shown.len(), shows.len()), (4, 2), "two cues, a clear behind each");
    let (pts, ds) = shows[0];
    assert_eq!(*pts, ns(first_cue, base));
    assert_eq!((ds.x, ds.y, ds.width, ds.height), (260, 480, 200, 40));
    assert_eq!((ds.video_width, ds.video_height), (720, 576), "the authoring grid");
    assert_eq!(shown[1].0 - shown[0].0, 2 * SECOND, "the clear two seconds on");
    assert_eq!(shows[1].0 - shows[0].0, 3 * SECOND);
    // Index 1 is white in the cue's palette.
    let ink = ds.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).count();
    assert_eq!(ink, 200 * 40, "the whole box is inked");
}

#[test]
fn the_streams_decode() {
    let data = fixture("h264_aac");
    for chunk in [777usize, 1 << 16] {
        let mut p = Pipeline::new();
        let src = p.add(PieceSrc::file(data.clone(), chunk));
        let demux = p.add(TsDemux::new(data.clone()));
        p.link(src.src(PieceSrc::OUT), demux.sink(TsDemux::IN)).expect("src to demux");
        let added = p.preroll().expect("preroll").srcs;
        let (pictures, sound) =
            (Arc::new(Mutex::new(Recorded::default())), Arc::new(Mutex::new(Recorded::default())));
        let video = p.add(H264Dec::new());
        let video_sink = p.add(RawSink { rec: Arc::clone(&pictures) });
        p.link(added[0].element.src(added[0].pad), video.sink(H264Dec::IN))
            .expect("demux to h264dec");
        p.link(video.src(H264Dec::OUT), video_sink.sink(SinkPad(0))).expect("h264dec to sink");
        let audio = p.add(LibavAudioDec::aac());
        let audio_sink = p.add(RawSink { rec: Arc::clone(&sound) });
        p.link(added[1].element.src(added[1].pad), audio.sink(LibavAudioDec::IN))
            .expect("demux to libavaacdec");
        p.link(audio.src(LibavAudioDec::OUT), audio_sink.sink(SinkPad(0)))
            .expect("libavaacdec to sink");
        p.run().expect("the run ends");

        let pictures = pictures.lock().unwrap();
        assert_eq!(pictures.units.len(), 30, "two seconds at 15 pictures a second");
        assert!(pictures.units.iter().all(|u| u.1 == 128 * 96 * 3 / 2));
        let stamps: Vec<u64> = pictures.units.iter().filter_map(|u| u.0).collect();
        assert!(stamps.windows(2).all(|w| w[0] < w[1]));
        let sound = sound.lock().unwrap();
        let octets: usize = sound.units.iter().map(|u| u.1).sum();
        assert!(octets > 0 && sound.units.len() > 20, "{} buffers of sound", sound.units.len());
    }
}

struct SoundSink {
    formats: Arc<Mutex<Vec<(i64, usize)>>>,
}

static SOUND_FIELDS: [FieldDesc; 2] = [
    FieldDesc { field: "rate", allowed: ANY, preferred: None },
    FieldDesc { field: "channels", allowed: ANY, preferred: None },
];
static SOUND_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SOUND_FIELDS }];
static SOUND_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SOUND_OFFERS }];
static SOUND_DESC: ElementDesc = ElementDesc {
    name: "soundsink",
    sinks: &SOUND_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

impl Element for SoundSink {
    fn desc(&self) -> &'static ElementDesc {
        &SOUND_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut formats = self.formats.lock().unwrap();
        while inputs.pop().is_some() {
            if let Some(last) = formats.last_mut() {
                last.1 += 1;
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(format) = event
            && let Some(Value::Int(rate)) = ctx.field_id("rate").and_then(|id| format.get(id))
        {
            self.formats.lock().unwrap().push((rate, 0));
        }
        Ok(())
    }
}

#[test]
fn sound_that_changes_its_rate_at_a_splice_is_decoded_on_at_the_new_one() {
    let first = fixture("h264_aac");
    let path = format!("{}/tests/fixtures/aac_24k.ts", env!("CARGO_MANIFEST_DIR"));
    let second = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mark = |at| Some(Mark { reset: true, stream_ns: Some(at) });
    let segments = vec![
        (mark(0), first.clone()),
        (mark(2 * SECOND), second),
        (mark(3 * SECOND), first.clone()),
    ];
    let mut p = Pipeline::new();
    let src = p.add(PieceSrc { segments, chunk: 4096, at: (0, 0) });
    let demux = p.add(TsDemux::new(first));
    p.link(src.src(PieceSrc::OUT), demux.sink(TsDemux::IN)).expect("src to demux");
    p.set_select_all(false);
    p.preroll().expect("preroll");
    let sound = p
        .streams(demux)
        .iter()
        .position(|s| s.kind == StreamKind::Audio)
        .expect("a stream of sound");
    let pad = p.select(demux, sound as u32).expect("select");
    let dec = p.add(LibavAudioDec::aac());
    let formats = Arc::new(Mutex::new(Vec::new()));
    let sink = p.add(SoundSink { formats: Arc::clone(&formats) });
    p.link(demux.src(pad), dec.sink(LibavAudioDec::IN)).expect("demux to libavaacdec");
    p.link(dec.src(LibavAudioDec::OUT), sink.sink(SinkPad(0))).expect("libavaacdec to sink");
    p.run().expect("the run ends");
    let mut warnings = Vec::new();
    while let Some(message) = p.bus().try_recv() {
        if let profluens_core::bus::BusMessage::Warning { error, .. } = message {
            warnings.push(error.to_string());
        }
    }
    assert!(warnings.is_empty(), "{warnings:?}");
    let formats = formats.lock().unwrap().clone();
    let rates: Vec<i64> = formats.iter().map(|f| f.0).collect();
    assert_eq!(rates, [48_000, 24_000, 48_000]);
    assert!((90..=96).contains(&formats[0].1), "{formats:?}");
    assert!((20..=26).contains(&formats[1].1), "{formats:?}");
    assert!((90..=96).contains(&formats[2].1), "{formats:?}");
}

/// Keeps every buffer it takes until it is taken out, a sink whose link waits.
struct Hold {
    rec: Arc<Mutex<Recorded>>,
    kept: Vec<profluens_core::buffer::Buffer>,
}

impl Element for Hold {
    fn desc(&self) -> &'static ElementDesc {
        &REC_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.rec.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.units.push((buf.pts.nanos(), buf.memory.len(), buf.flags.0));
            self.kept.push(buf);
        }
        Ok(Flow::Ok)
    }
}

/// Passes every ready group until none is, false when the run did not end.
fn step_through(run: &mut profluens_core::sched::Stepped) -> bool {
    loop {
        run.service();
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        if ready.is_empty() {
            break;
        }
        for g in ready {
            run.pass(g).expect("pass");
        }
    }
    let exit = Some(profluens_core::sched::Outcome::Exit);
    (0..run.groups()).all(|g| run.outcome(g) == exit)
}

/// An audio sink that keeps its frames dries the pool, and the demuxer parks the next unit it
/// read, of the audio or of the video. The audio moved to another sink with a restart starts at
/// the first unit behind the restart, behind its config, not at a frame it parked ahead, and the
/// video loses no picture it parked and gets none twice, also with the audio moved on again
/// right behind. The pool sizes park a unit of each.
#[test]
fn a_restart_drops_what_the_track_parked_and_the_other_tracks_keep_theirs() {
    use profluens_core::clock::MockClock;
    use profluens_core::topology::Leave;
    let data = fixture("h264_aac");
    let mut whole = Pipeline::new();
    let src = whole.add(PieceSrc::file(data.clone(), 1880));
    let demux = whole.add(TsDemux::new(data.clone()));
    whole.link(src.src(PieceSrc::OUT), demux.sink(TsDemux::IN)).expect("src to demux");
    whole.set_select_all(false);
    whole.preroll().expect("preroll");
    let (video_all, audio_all) = (Arc::default(), Arc::default());
    for (stream, rec) in [(0, &video_all), (1, &audio_all)] {
        let pad = whole.select(demux, stream).expect("select");
        let sink = whole.add(RecSink { rec: Arc::clone(rec) });
        whole.link(demux.src(pad), sink.sink(RecSink::IN)).expect("pad to sink");
    }
    whole.run().expect("the run ends");
    let index = whole.index(demux).expect("an index grown to the end");
    let (video_all, audio_all) = (video_all.lock().unwrap().clone(), audio_all.lock().unwrap());
    // The first frame of each audio PES as ffprobe lists it, and where the PES starts.
    let theirs = std::fs::read_to_string(fixture_path("h264_aac", "packets.csv")).expect("csv");
    let base = origin(&packets("h264_aac"));
    let starts: Vec<(u64, u64)> = theirs
        .lines()
        .map(|line| line.split(',').collect::<Vec<_>>())
        .filter(|f| f[0] == "1" && f[4] != "N/A")
        .map(|f| (f[4].parse().expect("pos"), ns(f[1].parse().expect("pts"), base)))
        .collect();

    for (slots, twice) in (40..48).chain(64..72).flat_map(|n| [(n, false), (n, true)]) {
        let case = format!("{slots} slots, moved twice {twice}");
        let clock = MockClock::starting_at(Timestamp::from_secs(1));
        let debug = profluens_core::log::DebugSpec::default();
        let mut p = Pipeline::with(debug, false, Arc::new(clock.clone()));
        p.set_pool(4096, slots);
        p.set_select_all(false);
        let src = p.add(PieceSrc::file(data.clone(), 1880));
        let demux = p.add(TsDemux::new(data.clone()));
        p.link(src.src(PieceSrc::OUT), demux.sink(TsDemux::IN)).expect("src to demux");
        p.preroll().expect("preroll");
        let (video, held, mut moved) = (Arc::default(), Arc::default(), Arc::default());
        let pad = p.select(demux, 0).expect("select the video");
        let sink = p.add(RecSink { rec: Arc::clone(&video) });
        p.link(demux.src(pad), sink.sink(RecSink::IN)).expect("pad to sink");
        let pad = p.select(demux, 1).expect("select the audio");
        let hold = p.add(Hold { rec: Arc::clone(&held), kept: Vec::new() });
        p.link(demux.src(pad), hold.sink(RecSink::IN)).expect("pad to hold");
        let mut run = p.step(Arc::new(clock)).expect("step");
        assert!(!step_through(&mut run), "the pool runs dry, {case}");

        let position = held.lock().unwrap().units.last().and_then(|u| u.0).expect("held");
        let Rejoin::At(byte, at) = index.restart_for(1, Timestamp(position)) else {
            panic!("a keyframe, {case}")
        };
        p.remove(hold, Leave::Cut).expect("remove");
        let sink = p.add(RecSink { rec: Arc::clone(&moved) });
        p.link(demux.src(pad), sink.sink(RecSink::IN)).expect("pad to sink");
        p.commit_at(Timestamp(position), &[(src, byte, at), (demux, byte, at)]).expect("commit");
        if twice {
            // The audio moves on again before the video read past what it put out.
            while p.changing() {
                run.service();
                if let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) {
                    run.pass(g).expect("pass");
                }
            }
            p.remove(sink, Leave::Cut).expect("remove");
            moved = Arc::default();
            let sink = p.add(RecSink { rec: Arc::clone(&moved) });
            p.link(demux.src(pad), sink.sink(RecSink::IN)).expect("pad to sink");
            let targets = [(src, byte, at), (demux, byte, at)];
            p.commit_at(Timestamp(position), &targets).expect("commit");
        }
        assert!(step_through(&mut run), "the run ends, {case}");
        run.finish().expect("finish");

        let video = video.lock().unwrap();
        assert_eq!(video.units, video_all.units, "every picture once, {case}");
        let moved = moved.lock().unwrap();
        let head = moved.units.first().copied().expect("the config");
        assert!(head.2 & BufferFlags::HEADER.0 != 0, "the config leads, {case}");
        let &(_, first) = starts.iter().find(|(pos, _)| *pos >= byte).expect("a PES behind");
        let frame = |u: &(Option<u64>, usize, u32)| u.2 & BufferFlags::HEADER.0 == 0;
        let at_first = |u: &(Option<u64>, usize, u32)| u.0.is_some_and(|t| t.abs_diff(first) <= 1);
        let from = audio_all.units.iter().position(|u| frame(u) && at_first(u));
        let from = from.expect("the frame in the whole run");
        assert_eq!(moved.units[1..], audio_all.units[from..], "on from the restart, {case}");
        eprintln!("{case}: position {position}, restart {byte} at {at:?}");
    }
}
