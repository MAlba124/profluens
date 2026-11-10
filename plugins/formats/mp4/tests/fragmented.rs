//! Fragmented MP4 (ISO/IEC 14496-12 8.8) through `Mp4Reader` and `Mp4Demux`.

// Tests own their fixtures.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libav::LibavH264Dec as H264Dec;
use pf_mp4::frag::{self, FragmentRef};
use pf_mp4::{Mp4Demux, Mp4Error, Mp4Reader, TrackKind};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::{IndexEntry, Pipeline};
use profluens_core::time::Timestamp;

const SECOND: u64 = 1_000_000_000;
const FRAME: u64 = SECOND / 15;

fn fixture(name: &str) -> Vec<u8> {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/frag");
    path.push(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Out {
    track: usize,
    pts_ns: u64,
    sync: bool,
    size: usize,
    sum: u64,
}

fn drain(reader: &mut Mp4Reader, out: &mut Vec<Out>) {
    while let Some(s) = reader.next_sample() {
        let track = &reader.tracks()[s.track_index];
        let ns = track.stamp_ns(s.pts) as i128 - i128::from(reader.shift_ns());
        let data = s.payload.data();
        out.push(Out {
            track: s.track_index,
            pts_ns: ns.max(0) as u64,
            sync: s.sync,
            size: data.len(),
            sum: data
                .iter()
                .enumerate()
                .fold(0u64, |sum, (i, b)| sum.wrapping_add((i as u64 + 1) * u64::from(*b))),
        });
    }
}

fn read_all(file: &[u8], chunk: usize) -> Vec<Out> {
    let mut reader = Mp4Reader::new(file).expect("the fixture resolves");
    assert!(reader.is_fragmented());
    let mut out = Vec::new();
    for piece in file.chunks(chunk) {
        reader.push_bytes(piece);
        drain(&mut reader, &mut out);
    }
    assert_eq!(reader.take_error(), None);
    out
}

fn mdat_octets(file: &[u8]) -> usize {
    let (mut at, mut total) = (0usize, 0usize);
    while at + 8 <= file.len() {
        let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        if &file[at + 4..at + 8] == b"mdat" {
            total += size - 8;
        }
        at += size.max(8);
    }
    total
}

fn ffprobe_packets(name: &str) -> Option<Vec<(usize, u64, usize, bool)>> {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/frag");
    path.push(name);
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "packet=stream_index,pts_time,size,flags",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(',').collect();
        let [stream, pts, size, flags] = fields[..] else {
            continue;
        };
        let micros = (pts.parse::<f64>().ok()? * 1e6).round() as u64;
        rows.push((stream.parse().ok()?, micros, size.parse().ok()?, flags.starts_with('K')));
    }
    Some(rows)
}

#[test]
fn a_fragmented_file_yields_every_sample_of_its_fragments() {
    let file = fixture("frag_av.mp4");
    let out = read_all(&file, 4096);
    let video: Vec<&Out> = out.iter().filter(|s| s.track == 0).collect();
    let audio: Vec<&Out> = out.iter().filter(|s| s.track == 1).collect();
    assert_eq!((video.len(), audio.len()), (45, 142));
    assert_eq!(out.iter().map(|s| s.size).sum::<usize>(), mdat_octets(&file));
    let sync: Vec<u64> = video.iter().filter(|s| s.sync).map(|s| s.pts_ns).collect();
    assert_eq!(sync.len(), 3);
    assert!(audio.iter().all(|s| s.sync), "every audio sample is a sync sample");
    let mut stamps: Vec<u64> = video.iter().map(|s| s.pts_ns).collect();
    stamps.sort_unstable();
    for (n, stamp) in stamps.iter().enumerate() {
        let expect = (n as u64 + 2) * SECOND / 15;
        assert!(stamp.abs_diff(expect) <= 1, "picture {n} at {stamp}, expected {expect}");
    }
}

#[test]
fn a_fragmented_file_matches_what_ffprobe_lists() {
    for name in ["frag_av.mp4", "frag_sidx.mp4", "frag_hybrid.mp4"] {
        let Some(mut theirs) = ffprobe_packets(name) else {
            eprintln!("skipped, no ffprobe");
            return;
        };
        let out = read_all(&fixture(name), 1 << 20);
        let mut ours: Vec<(usize, u64, usize, bool)> =
            out.iter().map(|s| (s.track, (s.pts_ns + 500) / 1000, s.size, s.sync)).collect();
        ours.sort_unstable();
        theirs.sort_unstable();
        assert_eq!(ours.len(), theirs.len(), "{name}: sample count");
        for (a, b) in ours.iter().zip(&theirs) {
            // ffprobe prints six decimals of a time the reader keeps in ticks.
            assert_eq!((a.0, a.2, a.3), (b.0, b.2, b.3), "{name}: {a:?} against {b:?}");
            assert!(a.1.abs_diff(b.1) <= 1, "{name}: {a:?} against {b:?}");
        }
    }
}

#[test]
fn how_the_stream_is_cut_up_changes_nothing() {
    for name in ["frag_av.mp4", "frag_sidx.mp4", "frag_hybrid.mp4"] {
        let file = fixture(name);
        let whole = read_all(&file, file.len());
        assert!(!whole.is_empty());
        for chunk in [1usize, 7, 188, 4096, 5000] {
            assert_eq!(read_all(&file, chunk), whole, "{name} in pieces of {chunk}");
        }
    }
}

#[test]
fn a_movie_with_its_first_samples_in_moov_plays_them_and_then_its_fragments() {
    let file = fixture("frag_hybrid.mp4");
    let reader = Mp4Reader::new(&file).expect("resolves");
    assert!(reader.is_fragmented());
    assert_eq!(reader.samples().len(), 15, "the first group of pictures is in moov");
    let out = read_all(&file, 4096);
    assert_eq!(out.len(), 45);
    let stamps: Vec<u64> = out.iter().map(|s| s.pts_ns).collect();
    assert!(stamps.windows(2).all(|w| w[0] < w[1]), "no B-frames, so the stamps ascend");
    assert_eq!((stamps[0], stamps[44].abs_diff(44 * SECOND / 15) <= 1), (0, true));
}

#[derive(Clone, Copy)]
struct Mark {
    reset: bool,
    stream_ns: Option<u64>,
    media_ns: Option<u64>,
}

const FIRST: Mark = Mark { reset: true, stream_ns: Some(0), media_ns: None };
const NEXT: Mark = Mark { reset: false, stream_ns: None, media_ns: None };
const SWITCH: Mark = Mark { reset: true, stream_ns: None, media_ns: None };

fn read_segments(init: &[u8], segments: &[(Mark, Vec<u8>)], chunk: usize) -> (Mp4Reader, Vec<Out>) {
    let mut reader = Mp4Reader::new(init).expect("the init segment resolves");
    let mut out = Vec::new();
    for (mark, bytes) in segments {
        reader.mark(mark.reset, mark.stream_ns, mark.media_ns);
        for piece in bytes.chunks(chunk) {
            reader.push_bytes(piece);
            drain(&mut reader, &mut out);
        }
    }
    (reader, out)
}

#[test]
fn media_segments_behind_an_init_segment_keep_one_timeline() {
    let init = fixture("dash_init-stream0.m4s");
    let segments: Vec<(Mark, Vec<u8>)> = [(FIRST, "00001"), (NEXT, "00002"), (NEXT, "00003")]
        .map(|(mark, n)| (mark, fixture(&format!("dash_stream0-{n}.m4s"))))
        .into();
    for chunk in [1usize, 1000, 1 << 20] {
        let (mut reader, out) = read_segments(&init, &segments, chunk);
        assert_eq!(reader.take_error(), None);
        assert_eq!(out.len(), 45, "pieces of {chunk}");
        for (n, s) in out.iter().enumerate() {
            let expect = n as u64 * SECOND / 15;
            assert!(s.pts_ns.abs_diff(expect) <= 1, "picture {n} at {}", s.pts_ns);
            assert_eq!(s.sync, n % 15 == 0);
        }
    }
}

#[test]
fn a_switch_of_rendition_keeps_the_timeline_and_changes_the_track() {
    let init = fixture("dash_init-stream0.m4s");
    let small = [fixture("dash_init-stream1.m4s"), fixture("dash_stream1-00002.m4s")].concat();
    let segments = vec![
        (FIRST, fixture("dash_stream0-00001.m4s")),
        (SWITCH, small),
        (NEXT, fixture("dash_stream1-00003.m4s")),
    ];
    let mut reader = Mp4Reader::new(&init).expect("resolves");
    assert_eq!((reader.tracks()[0].width, reader.tracks()[0].height), (128, 96));
    let mut out = Vec::new();
    let mut epochs = Vec::new();
    for (mark, bytes) in &segments {
        reader.mark(mark.reset, mark.stream_ns, mark.media_ns);
        reader.push_bytes(bytes);
        drain(&mut reader, &mut out);
        epochs.push(reader.init_epoch());
    }
    assert_eq!(reader.take_error(), None);
    assert_eq!(epochs, [0, 1, 1], "one init segment changed the track");
    assert_eq!((reader.tracks()[0].width, reader.tracks()[0].height), (64, 48));
    assert_eq!(out.len(), 45);
    for (n, s) in out.iter().enumerate() {
        assert!(s.pts_ns.abs_diff(n as u64 * SECOND / 15) <= 1, "picture {n} at {}", s.pts_ns);
    }
}

#[test]
fn an_init_segment_that_repeats_the_last_one_changes_nothing() {
    let init = fixture("dash_init-stream0.m4s");
    let again = [init.clone(), fixture("dash_stream0-00002.m4s")].concat();
    let segments = vec![(FIRST, fixture("dash_stream0-00001.m4s")), (SWITCH, again)];
    let (mut reader, out) = read_segments(&init, &segments, 4096);
    assert_eq!((reader.init_epoch(), reader.take_error()), (0, None));
    assert_eq!(out.len(), 30);
}

#[test]
fn a_mark_lands_a_segment_where_its_source_says() {
    let init = fixture("dash_init-stream0.m4s");
    let second = fixture("dash_stream0-00002.m4s");
    let at = Mark { reset: true, stream_ns: Some(10 * SECOND), media_ns: None };
    let (_, out) = read_segments(&init, &[(at, second.clone())], 4096);
    assert_eq!((out.len(), out[0].pts_ns), (15, 10 * SECOND));
    let exact = Mark { reset: true, stream_ns: Some(10 * SECOND), media_ns: Some(SECOND / 2) };
    let (_, out) = read_segments(&init, &[(exact, second)], 4096);
    assert_eq!(out[0].pts_ns, 10 * SECOND + SECOND / 2);
}

#[test]
fn a_seek_index_is_read_off_a_sidx_and_off_an_mfra() {
    let file = fixture("frag_sidx.mp4");
    let reader = Mp4Reader::new(&file).expect("resolves");
    let tables = reader.tables();
    let fragments = tables.fragments();
    assert_eq!(fragments.len(), 3, "a fragment per group of pictures");
    for (n, f) in fragments.iter().enumerate() {
        assert_eq!(f.time_ns, n as u64 * SECOND);
        assert_eq!(&file[f.offset as usize + 4..f.offset as usize + 8], b"moof");
    }
    assert_eq!(tables.fragmented_duration_ns(), 3 * SECOND);

    let file = fixture("frag_av.mp4");
    let bare = Mp4Reader::new(&file).expect("resolves");
    assert!(bare.tables().fragments().is_empty(), "nothing in the head indexes this one");
    let size =
        frag::mfra_size(&file).expect("ffmpeg closes a fragmented file with an mfra") as usize;
    let indexed = Mp4Reader::indexed(&file, &file[file.len() - size..]).expect("resolves");
    let tables = indexed.tables();
    let fragments: Vec<FragmentRef> = tables.fragments().to_vec();
    assert_eq!(fragments.len(), 3);
    for (n, f) in fragments.iter().enumerate() {
        assert!(f.time_ns.abs_diff(n as u64 * SECOND + 2 * FRAME) <= 2, "{f:?}");
        assert_eq!(&file[f.offset as usize + 4..f.offset as usize + 8], b"moof");
    }
    let restart = tables.restart(SECOND + SECOND / 2, SECOND + SECOND / 2, &|_| true);
    assert_eq!(restart.to_byte, fragments[1].offset);
}

#[test]
fn a_fragmented_movie_indexes_its_fragments_or_its_first() {
    let file = fixture("frag_sidx.mp4");
    let tables = Mp4Reader::new(&file).expect("resolves").tables();
    let index = tables.seek_index(&|_| true);
    let named: Vec<(u64, u64)> = tables.fragments().iter().map(|f| (f.time_ns, f.offset)).collect();
    let entries: Vec<(u64, u64)> = index.entries.iter().map(|e| (e.time_ns, e.byte)).collect();
    assert_eq!((entries.len(), entries), (3, named));
    for e in &index.entries {
        let r = tables.restart(e.time_ns, e.time_ns, &|_| true);
        assert_eq!((e.restart, e.restart_ns), (r.to_byte, r.at_ns));
    }
    assert_eq!((index.complete, index.duration), (true, Timestamp(3 * SECOND)));

    let file = fixture("frag_av.mp4");
    let bare = Mp4Reader::new(&file).expect("resolves").tables();
    let first = bare.restart(0, 0, &|_| true).to_byte;
    assert_eq!(&file[first as usize + 4..first as usize + 8], b"moof");
    assert_eq!(bare.seek_index(&|_| true).entries, [IndexEntry::key(0, first)]);
}

fn seek_and_read(file: &[u8], reader: &mut Mp4Reader, to_byte: u64, point: u64) -> Vec<Out> {
    reader.seek_stream(to_byte, Some(point), point);
    let mut out = Vec::new();
    for piece in file[to_byte as usize..].chunks(3000) {
        reader.push_bytes(piece);
        drain(reader, &mut out);
    }
    out
}

#[test]
fn a_seek_restarts_each_track_at_its_own_sample() {
    let file = fixture("frag_av.mp4");
    let size = frag::mfra_size(&file).unwrap() as usize;
    let mut reader = Mp4Reader::indexed(&file, &file[file.len() - size..]).expect("resolves");
    let point = 2 * SECOND + SECOND / 2;
    let restart = reader.tables().restart(point, point, &|_| true);
    let out = seek_and_read(&file, &mut reader, restart.to_byte, point);
    let video: Vec<&Out> = out.iter().filter(|s| s.track == 0).collect();
    let audio: Vec<&Out> = out.iter().filter(|s| s.track == 1).collect();
    assert!(video[0].sync);
    assert!(video[0].pts_ns.abs_diff(2 * SECOND + 2 * FRAME) <= 2, "{:?}", video[0]);
    assert_eq!(video.len(), 15);
    let lead = reader.tracks()[1].pre_roll_ns();
    assert!(reader.tracks()[1].kind() == TrackKind::Audio && lead > 0);
    let frame = 1024 * SECOND / 48_000;
    assert!(
        audio[0].pts_ns <= point - lead && audio[0].pts_ns + frame > point - lead,
        "{:?}",
        audio[0]
    );
}

#[test]
fn a_seek_with_no_index_reads_from_the_first_fragment_to_the_same_samples() {
    let file = fixture("frag_av.mp4");
    let size = frag::mfra_size(&file).unwrap() as usize;
    let mut indexed = Mp4Reader::indexed(&file, &file[file.len() - size..]).expect("resolves");
    let mut bare = Mp4Reader::new(&file).expect("resolves");
    for point in [0, SECOND / 3, SECOND + 1, 2 * SECOND + SECOND / 2, 2 * SECOND + 14 * FRAME] {
        let by_index = indexed.tables().restart(point, point, &|_| true);
        let by_scan = bare.tables().restart(point, point, &|_| true);
        assert_eq!(&file[by_scan.to_byte as usize + 4..by_scan.to_byte as usize + 8], b"moof");
        let a = seek_and_read(&file, &mut indexed, by_index.to_byte, point);
        let b = seek_and_read(&file, &mut bare, by_scan.to_byte, point);
        assert!(!a.is_empty());
        assert_eq!(a, b, "a seek to {point}");
    }
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn fragment(run: &[u8], payload: usize) -> Vec<u8> {
    let tfhd = boxed(b"tfhd", &be32(&[0x0002_0000, 1]));
    let traf = boxed(b"traf", &[tfhd, boxed(b"trun", run)].concat());
    [boxed(b"moof", &traf), boxed(b"mdat", &vec![0xAB; payload])].concat()
}

#[test]
fn a_run_that_claims_more_than_its_mdat_holds_is_cut_and_said() {
    let init = fixture("dash_init-stream0.m4s");
    // Four billion samples of the default size, which the init segment leaves at zero.
    let empty = fragment(&be32(&[0x0000_0001, u32::MAX, 0]), 8);
    let (mut reader, out) = read_segments(&init, &[(FIRST, empty)], 4096);
    assert!(out.is_empty());
    assert!(matches!(reader.take_error(), Some(Mp4Error::Inconsistent(_))));

    let tfhd = boxed(b"tfhd", &be32(&[0x0002_0010, 1, 1]));
    let run = boxed(b"trun", &be32(&[0x0000_0001, u32::MAX, 16 + 8 + 20 + 8 + 20 + 8]));
    let moof = boxed(b"moof", &boxed(b"traf", &[tfhd, run].concat()));
    let offset = moof.len() as u32 + 8;
    let tfhd = boxed(b"tfhd", &be32(&[0x0002_0010, 1, 1]));
    let run = boxed(b"trun", &be32(&[0x0000_0001, u32::MAX, offset]));
    let moof = boxed(b"moof", &boxed(b"traf", &[tfhd, run].concat()));
    let greedy = [moof, boxed(b"mdat", &[7u8; 8])].concat();
    let (mut reader, out) = read_segments(&init, &[(FIRST, greedy)], 4096);
    assert_eq!(out.len(), 8, "the samples that lie inside the mdat");
    assert!(reader.take_error().is_some());
}

#[test]
fn a_segment_cut_short_is_said_and_the_next_one_plays() {
    let init = fixture("dash_init-stream0.m4s");
    let first = fixture("dash_stream0-00001.m4s");
    let cut = first[..first.len() / 2].to_vec();
    let segments = vec![(FIRST, cut), (NEXT, fixture("dash_stream0-00002.m4s"))];
    let (mut reader, out) = read_segments(&init, &segments, 4096);
    assert_eq!(reader.take_error(), Some(Mp4Error::Inconsistent("a segment ended inside a box")));
    let whole: Vec<&Out> = out.iter().filter(|s| s.pts_ns >= SECOND).collect();
    assert_eq!(whole.len(), 15, "the second segment is whole");
    assert!(whole[0].sync && whole[0].pts_ns == SECOND);
    assert!(out.len() < 30);
}

#[test]
fn a_stream_that_stops_making_sense_is_lost_until_a_mark() {
    let init = fixture("dash_init-stream0.m4s");
    let mut reader = Mp4Reader::new(&init).expect("resolves");
    reader.push_bytes(&[0, 0, 0, 3, b'j', b'u', b'n', b'k', 1, 2, 3, 4]);
    assert!(reader.next_sample().is_none());
    assert!(matches!(reader.take_error(), Some(Mp4Error::Box(_))));
    assert!(reader.is_lost(), "no source marks the segments of a file");
    reader.mark(false, Some(0), None);
    assert!(!reader.is_lost());
    reader.push_bytes(&fixture("dash_stream0-00001.m4s"));
    let mut out = Vec::new();
    drain(&mut reader, &mut out);
    assert_eq!(out.len(), 15);
}

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "segmentsrc",
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

struct SegmentSrc {
    segments: Vec<(Option<Mark>, Vec<u8>)>,
    chunk: usize,
    at: (usize, usize),
}

impl SegmentSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for SegmentSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let (segment, pos) = self.at;
        let Some((mark, bytes)) = self.segments.get(segment) else {
            return Ok(Flow::Eos);
        };
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = self.chunk.min(buf.memory.capacity()).min(bytes.len() - pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&bytes[pos..pos + n]);
        buf.memory.set_len(n);
        if let (0, Some(mark)) = (pos, mark) {
            buf.flags = BufferFlags::SEGMENT;
            if mark.reset {
                buf.flags |= BufferFlags::DISCONT;
            }
            buf.pts = mark.stream_ns.map_or(Timestamp::NONE, Timestamp);
            buf.dts = mark.media_ns.map_or(Timestamp::NONE, Timestamp);
        }
        ctx.out(Self::OUT).push(buf);
        self.at = if pos + n == bytes.len() { (segment + 1, 0) } else { (segment, pos + n) };
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
static SINK_DESC: ElementDesc = ElementDesc {
    name: "picturesink",
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

#[derive(Default)]
struct Pictures {
    frames: Vec<(u64, i64, i64)>,
    size: (i64, i64),
}

struct PictureSink {
    shared: Arc<Mutex<Pictures>>,
}

impl PictureSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for PictureSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut seen = self.shared.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            let (w, h) = seen.size;
            assert_eq!(
                buf.memory.len() as i64,
                w * h * 3 / 2,
                "a whole I420 picture of the size said"
            );
            seen.frames.push((buf.pts.nanos().expect("a picture carries a pts"), w, h));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(_) = event {
            let f = ctx.negotiated_sink(Self::IN).expect("format installed").clone();
            let get = |name: &str| ctx.field_id(name).and_then(|id| f.get(id));
            if let (Some(Value::Int(w)), Some(Value::Int(h))) = (get("width"), get("height")) {
                self.shared.lock().unwrap().size = (w, h);
            }
        }
        Ok(())
    }
}

fn play(
    head: &[u8],
    segments: Vec<(Option<Mark>, Vec<u8>)>,
    chunk: usize,
) -> (Pictures, Vec<String>) {
    let mut p = Pipeline::new();
    let src = p.add(SegmentSrc { segments, chunk, at: (0, 0) });
    let demux = p.add(Mp4Demux::new(head.to_vec()));
    p.link(src.src(SegmentSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src to demux");
    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 1, "one video track");
    let dec = p.add(H264Dec::new());
    let shared = Arc::new(Mutex::new(Pictures::default()));
    let sink = p.add(PictureSink { shared: Arc::clone(&shared) });
    p.link(added[0].element.src(added[0].pad), dec.sink(H264Dec::IN)).expect("demux to decoder");
    p.link(dec.src(H264Dec::OUT), sink.sink(PictureSink::IN)).expect("decoder to sink");
    p.run().expect("the run ends");
    let mut warnings = Vec::new();
    while let Some(message) = p.bus().try_recv() {
        if let BusMessage::Warning { error, .. } = message {
            warnings.push(error.to_string());
        }
    }
    let pictures = std::mem::take(&mut *shared.lock().unwrap());
    (pictures, warnings)
}

#[test]
fn a_fragmented_file_decodes_through_the_element() {
    let file = fixture("frag_sidx.mp4");
    for chunk in [333usize, 4096, 1 << 20] {
        let (pictures, warnings) = play(&file, vec![(None, file.clone())], chunk);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(pictures.frames.len(), 45, "pieces of {chunk}");
        for (n, (pts, w, h)) in pictures.frames.iter().enumerate() {
            assert!(pts.abs_diff(n as u64 * SECOND / 15) <= 1, "picture {n} at {pts}");
            assert_eq!((*w, *h), (128, 96));
        }
    }
}

#[test]
fn a_switch_of_rendition_decodes_on_one_timeline_in_the_new_size() {
    let init = fixture("dash_init-stream0.m4s");
    let small = [fixture("dash_init-stream1.m4s"), fixture("dash_stream1-00002.m4s")].concat();
    let back = [init.clone(), fixture("dash_stream0-00003.m4s")].concat();
    let segments = vec![
        (Some(FIRST), fixture("dash_stream0-00001.m4s")),
        (Some(SWITCH), small),
        (Some(SWITCH), back),
    ];
    let (pictures, warnings) = play(&init, segments, 2000);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(pictures.frames.len(), 45);
    for (n, (pts, w, h)) in pictures.frames.iter().enumerate() {
        assert!(pts.abs_diff(n as u64 * SECOND / 15) <= 1, "picture {n} at {pts}");
        let expect = if (15..30).contains(&n) { (64, 48) } else { (128, 96) };
        assert_eq!((*w, *h), expect, "picture {n}");
    }
}

#[test]
fn a_fragment_that_does_not_hold_together_is_a_warning_and_the_rest_plays() {
    let init = fixture("dash_init-stream0.m4s");
    let first = fixture("dash_stream0-00001.m4s");
    let cut = first[..first.len() - 100].to_vec();
    let segments = vec![(Some(FIRST), cut), (Some(NEXT), fixture("dash_stream0-00002.m4s"))];
    let (pictures, warnings) = play(&init, segments, 4096);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("a segment ended inside a box"), "{warnings:?}");
    let second: Vec<_> = pictures.frames.iter().filter(|f| f.0 >= SECOND).collect();
    assert_eq!(second.len(), 15);
}

#[test]
fn a_file_that_stops_making_sense_fails_the_element_with_its_reason() {
    let file = fixture("frag_sidx.mp4");
    let mut broken = file.clone();
    let moof = file.windows(4).position(|w| w == b"moof").unwrap() - 4;
    broken[moof..moof + 4].copy_from_slice(&4u32.to_be_bytes());
    let mut p = Pipeline::new();
    let src = p.add(SegmentSrc { segments: vec![(None, broken)], chunk: 4096, at: (0, 0) });
    let demux = p.add(Mp4Demux::new(file));
    p.link(src.src(SegmentSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src to demux");
    let added = p.preroll().expect("preroll").srcs;
    let dec = p.add(H264Dec::new());
    let sink = p.add(PictureSink { shared: Arc::default() });
    p.link(added[0].element.src(added[0].pad), dec.sink(H264Dec::IN)).expect("demux to decoder");
    p.link(dec.src(H264Dec::OUT), sink.sink(PictureSink::IN)).expect("decoder to sink");
    let error = p.run().expect_err("the stream is lost").to_string();
    assert!(error.contains("box size smaller than its own header"), "{error}");
}

#[test]
fn many_short_runs_of_empty_samples_share_one_budget_per_moof() {
    let init = fixture("dash_init-stream0.m4s");
    // Flags 0 and 4096 rows each, under the old per-trun budget, 50k of them in one moof.
    let tfhd = boxed(b"tfhd", &be32(&[0x0002_0000, 1]));
    let trun = boxed(b"trun", &be32(&[0, 4096]));
    let mut traf = tfhd;
    for _ in 0..50_000 {
        traf.extend_from_slice(&trun);
    }
    let moof = boxed(b"moof", &boxed(b"traf", &traf));
    let segment = [moof, boxed(b"mdat", &[7u8; 8])].concat();
    let (mut reader, out) = read_segments(&init, &[(FIRST, segment)], 1 << 20);
    assert!(out.is_empty());
    assert!(matches!(reader.take_error(), Some(Mp4Error::Inconsistent(_))));
}

/// A source that goes back for one track of a fragmented movie. The walk follows it, the other
/// track plays on from the sample it was at and puts nothing out twice, and the track starts
/// at its restart point.
#[test]
fn a_source_that_goes_back_for_one_track_of_a_fragmented_movie_puts_nothing_out_twice() {
    let file = fixture("frag_av.mp4");
    let whole = read_all(&file, 4096);
    let size = frag::mfra_size(&file).unwrap() as usize;
    let mut reader = Mp4Reader::indexed(&file, &file[file.len() - size..]).expect("resolves");
    let fragments = reader.tables().fragments().to_vec();
    reader.select(1, false);
    reader.reset_stream();
    let point = 2 * SECOND + SECOND / 2;
    let lead = reader.tracks()[1].pre_roll_ns();
    for cut in [fragments[2].offset as usize + 3000, file.len() - size - 100] {
        let mut out = Vec::new();
        reader.select(1, false);
        reader.reset_stream();
        for piece in file[..cut].chunks(4096) {
            reader.push_bytes(piece);
            drain(&mut reader, &mut out);
        }
        assert!(out.iter().all(|s| s.track == 0), "the audio is off");
        let restart = reader.tables().restart(point, point, &|_| true);
        assert!(restart.to_byte < cut as u64, "the restart lies behind the walk");
        reader.rejoin(1, restart.to_byte, point);
        for piece in file[restart.to_byte as usize..].chunks(4096) {
            reader.push_bytes(piece);
            drain(&mut reader, &mut out);
        }
        assert_eq!(reader.take_error(), None);
        let of = |out: &[Out], track: usize| -> Vec<Out> {
            out.iter().filter(|s| s.track == track).cloned().collect()
        };
        assert_eq!(of(&out, 0), of(&whole, 0), "every picture once, cut at {cut}");
        let audio = of(&whole, 1);
        let from = audio.iter().rposition(|s| s.pts_ns <= point - lead).expect("a packet");
        assert_eq!(of(&out, 1), audio[from..], "from the restart point, cut at {cut}");
    }
}

/// What ffmpeg writes for `args`, None without it.
fn ffmpeg(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    static MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("pf_mp4_frag_{}_{n}_{name}", std::process::id()));
    let ok = std::process::Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    let file = ok.then(|| std::fs::read(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    if file.is_none() {
        eprintln!("ffmpeg could not make {name}, skipped");
    }
    file
}

/// ffmpeg's fragmented form the sweep found, 6 s at 25 fps in 1 s fragments, the video 80 ms
/// late as an empty edit in a `moov` whose durations are 0, the edit to the media's end.
fn late_fragments(name: &str, flags: &str) -> Option<Vec<u8>> {
    let movflags = format!("frag_keyframe+empty_moov+default_base_moof+delay_moov{flags}");
    ffmpeg(
        name,
        &[
            "-itsoffset",
            "0.08",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=d=6:s=160x96:r=25",
            "-f",
            "lavfi",
            "-i",
            "sine=d=6:r=48000",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-g",
            "25",
            "-c:a",
            "aac",
            "-use_editlist",
            "1",
            "-movflags",
            &movflags,
        ],
    )
}

/// The file up to its first `mdat`, the head an open hands the demuxer.
fn head_of(file: &[u8]) -> &[u8] {
    let mut at = 0usize;
    while &file[at + 4..at + 8] != b"mdat" {
        at += u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
    }
    &file[..at]
}

#[test]
fn an_empty_edit_is_no_length_and_the_last_fragment_ends_the_movie() {
    let Some(file) = late_fragments("dash.mp4", "+dash") else { return };
    let elst = file.windows(4).position(|w| w == b"elst").unwrap() + 4;
    assert_eq!(&file[elst..elst + 8], [0, 0, 0, 0, 0, 0, 0, 2], "version 0, two edits");
    assert_eq!(&file[elst + 8..elst + 16], [0, 0, 0, 80, 0xFF, 0xFF, 0xFF, 0xFF], "80 ms empty");
    let head = head_of(&file);
    assert_eq!(frag::length_stated(head), Some(false), "no mehd");
    let first = pf_mp4::Tables::resolve(head, &[], pf_mp4::Limits::default()).unwrap();
    assert!(first.tracks().iter().all(|t| t.duration_ns == 0), "{:?}", first.tracks());
    let video = &first.tracks()[0];
    assert_eq!((video.frame_ticks, video.timescale), (512, 12_800), "25 fps from the trun rows");
    let one = first.fragmented_duration_ns();
    assert!(one.abs_diff(SECOND) < SECOND / 10, "the first sidx ends at {one}");

    let mut log = Vec::new();
    let mut at = frag::TailAt::new(file.len() as u64);
    let tail = profluens_core::fetch::drive_slice_log(&mut at, &file, &mut log).unwrap();
    assert_eq!(log.len(), 3, "the mfro, the mfra and the last moof, {log:?}");
    assert_eq!(&tail.moof[4..8], b"moof");
    let mut tables = pf_mp4::Tables::resolve(head, &tail.mfra, pf_mp4::Limits::default()).unwrap();
    tables.last_fragment(&tail.moof);
    for t in tables.tracks() {
        assert!(t.duration_ns.abs_diff(6 * SECOND) < SECOND / 10, "{}", t.duration_ns);
    }
    let index = tables.seek_index(&|_| false);
    let times: Vec<u64> = index.entries.iter().map(|e| e.time_ns / (SECOND / 10)).collect();
    assert_eq!(times, [0, 10, 20, 30, 40, 50], "a fragment a second");
    let length = index.duration.nanos().unwrap();
    assert!(length.abs_diff(6 * SECOND) < SECOND / 10, "{length}");
}

#[test]
fn the_tail_of_a_file_without_an_mfra_reads_once() {
    let mut log = Vec::new();
    let file = vec![0u8; 100];
    let mut at = frag::TailAt::new(file.len() as u64);
    let tail = profluens_core::fetch::drive_slice_log(&mut at, &file, &mut log).unwrap();
    assert_eq!((tail, log.len()), (frag::Tail::default(), 1));
}
