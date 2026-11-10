// Test fixtures and a test rig, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::{Arc, Mutex};

use pf_mkv::ebml::{self, id};
use pf_mkv::{MkvDemux, Parsed, parse_cue_tracks, parse_seek_head};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::{Pipeline, SeekMode};
use profluens_core::sched::Stepped;
use profluens_core::time::Timestamp;

const MS: u64 = 1_000_000;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static SRC: ElementDesc = desc("memsrc", &[], &SRC_PADS);
static SINK: ElementDesc = desc("recsink", &SINK_PADS, &[]);
const OUT: SrcPad = SrcPad(0);

/// Serves every range asked whole, short only at the end of the file, as `FileSrc` does.
struct MemSrc {
    file: Arc<[u8]>,
}

impl Element for MemSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.offer_ranges(OUT);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(ask) = ctx.next_ask(OUT) {
            let profluens_core::pull::Ask { offset, len, tag, mut buf } = ask;
            let from = offset.min(self.file.len() as u64) as usize;
            let n = (len as usize).min(self.file.len() - from).min(buf.memory.capacity());
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.file[from..from + n]);
            buf.memory.set_len(n);
            ctx.serve(OUT, tag, buf);
        }
        Ok(if ctx.asks_over(OUT) { Flow::Eos } else { Flow::Ok })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Frame(u64),
    Flush,
}

type Log = Arc<Mutex<Vec<(usize, Seen)>>>;

struct RecSink {
    pad: usize,
    out: Log,
}

impl Element for RecSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            if let (false, Some(pts)) = (buf.flags.contains(BufferFlags::HEADER), buf.pts.nanos()) {
                self.out.lock().unwrap().push((self.pad, Seen::Frame(pts)));
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.out.lock().unwrap().push((self.pad, Seen::Flush));
        }
        Ok(())
    }
}

struct Ran {
    seen: Vec<(usize, Seen)>,
}

impl Ran {
    /// The stamps stream `n` put out after the seek's flush.
    fn after_flush(&self, n: usize) -> Vec<u64> {
        let from = self.seen.iter().rposition(|(p, s)| *p == n && *s == Seen::Flush);
        let after = &self.seen[from.map_or(self.seen.len(), |i| i + 1)..];
        after.iter().filter(|(p, _)| *p == n).filter_map(|(_, s)| s.stamp()).collect()
    }
}

impl Seen {
    fn stamp(&self) -> Option<u64> {
        match self {
            Seen::Frame(pts) => Some(*pts),
            Seen::Flush => None,
        }
    }
}

/// The head parsed with its Cues as an open hands them over, none when the file holds none.
fn parsed(file: &[u8]) -> Parsed {
    let cluster = file.windows(4).position(|w| w == id::CLUSTER).unwrap_or(file.len());
    let head = &file[..cluster];
    let mut p = Parsed::of(head).expect("a head");
    let info = parse_seek_head(head).expect("a Segment");
    let at = info.cues_pos.map(|c| (c + info.segment_data_start) as usize);
    if let Some(bytes) = at.and_then(|at| file.get(at..)) {
        let cues = parse_cue_tracks(bytes, info.segment_data_start, info.timestamp_scale);
        if !cues.is_empty() {
            (p.cues, p.cued_at) = (Arc::from(cues), at.map(|at| at as u64));
        }
    }
    p
}

/// Plays `file` in the sweep's order. The video and audio streams play, the caption stream is
/// switched on at `text_ms` with the restart the index names, then a seek to `seek_ms`.
fn sweep(demux: MkvDemux, file: &[u8], text_ms: Option<u64>, seek_ms: u64) -> Ran {
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    p.set_select_all(false);
    let src = p.add(MemSrc { file: Arc::from(file) });
    let demux = p.add(demux);
    p.set_element_pool(src, 1 << 20, 8);
    p.set_element_pool_bytes(demux, 128 << 10, 1024, 64 << 20);
    p.link(src.src(OUT), demux.sink(MkvDemux::IN)).unwrap();
    p.preroll().unwrap();
    let out: Log = Arc::default();
    let link = |p: &mut Pipeline, n: u32| {
        let pad = p.select(demux, n).unwrap();
        let sink = p.add(RecSink { pad: n as usize, out: Arc::clone(&out) });
        p.link(demux.src(pad), sink.sink(SinkPad(0))).unwrap();
    };
    link(&mut p, 0);
    link(&mut p, 1);
    let mut run: Stepped = p.step(Arc::clone(&shared)).unwrap();
    let go = |run: &mut Stepped, until: Option<u64>| {
        let past = |seen: &[(usize, Seen)], ms: u64| {
            seen.iter().any(|(n, s)| *n == 0 && s.stamp().is_some_and(|t| t >= ms * MS))
        };
        for _ in 0..20_000_000u64 {
            if until.is_some_and(|ms| past(&out.lock().unwrap(), ms)) {
                return;
            }
            run.service();
            let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
                let Some(to) = run.next_deadline() else { return };
                if to > clock.now() {
                    clock.advance(Timestamp(to.0 - clock.now().0));
                }
                continue;
            };
            run.pass(g).expect("a pass");
        }
        panic!("the run did not end");
    };
    go(&mut run, Some(text_ms.unwrap_or(2_000)));
    if let Some(ms) = text_ms {
        let at = Timestamp(ms * MS);
        let restart = p.index(demux).and_then(|ix| ix.restart_for(2, at));
        link(&mut p, 2);
        let targets: Vec<_> =
            restart.into_iter().flat_map(|(b, t)| [(src, b, t), (demux, b, t)]).collect();
        p.commit_at(at, &targets).unwrap();
        go(&mut run, Some(ms + 5_000));
    }
    p.control().seek(Timestamp(seek_ms * MS), SeekMode::Accurate).unwrap();
    go(&mut run, None);
    let _ = run.finish();
    while let Some(m) = p.bus().try_recv() {
        if let BusMessage::Error { error, .. } = m {
            panic!("an error on the bus, {error}");
        }
    }
    drop(p);
    let seen = std::mem::take(&mut *out.lock().unwrap());
    Ran { seen }
}

fn el(out: &mut Vec<u8>, of: &[u8], body: &[u8]) {
    ebml::write_binary(out, of, body);
}

fn uint(out: &mut Vec<u8>, of: &[u8], v: u64) {
    ebml::write_uint(out, of, v);
}

/// A zlib stream of stored deflate blocks.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = data.chunks(65_535).collect();
    for (i, c) in chunks.iter().enumerate() {
        out.push(u8::from(i + 1 == chunks.len()));
        let n = c.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

const END_MS: u64 = 120_000;
const KEY_MS: u64 = 4_000;

/// A DVD remux as mkvmerge writes one, MPEG-2 video keyed every [`KEY_MS`], AC-3 audio and a
/// VobSub track of zlib compressed bitmaps in BlockGroups without a duration, each stored 6 s
/// ahead of its stamp as a DVD's SPU packets are, so its block lies before its key's Cluster.
/// Cues name each video key and caption. A new Cluster at each key.
#[derive(Default)]
struct Film {
    cues: bool,
    /// Damage at Segment level behind the Cluster of this time, read as an element of
    /// Chapters whose size reaches this many octets on.
    damage: Option<(u64, u64)>,
}

impl Film {
    fn build(&self) -> Vec<u8> {
        // (stored, track, stamp, key, payload)
        let mut blocks: Vec<(u64, u64, u64, bool, Vec<u8>)> = Vec::new();
        for t in (0..END_MS).step_by(40) {
            let key = t % KEY_MS == 0;
            blocks.push((t, 1, t, key, vec![0xB5; if key { 40_000 } else { 6_000 }]));
        }
        for t in (0..END_MS).step_by(32) {
            blocks.push((t, 2, t, true, vec![0x0B; 768]));
        }
        for t in (5_000..END_MS).step_by(5_000) {
            blocks.push((t.saturating_sub(6_000), 3, t, true, zlib(&[0x5A; 30_000])));
        }
        blocks.sort_by_key(|b| (b.0, b.1 != 1 || !b.3));
        let track = |out: &mut Vec<u8>, n: u64, kind: u64, codec: &[u8], body: &[u8]| {
            let mut te = Vec::new();
            uint(&mut te, id::TRACK_NUMBER, n);
            uint(&mut te, id::TRACK_UID, n);
            uint(&mut te, id::TRACK_TYPE, kind);
            el(&mut te, id::CODEC_ID, codec);
            te.extend_from_slice(body);
            el(out, id::TRACK_ENTRY, &te);
        };
        let mut tracks = Vec::new();
        let mut v = Vec::new();
        uint(&mut v, id::PIXEL_WIDTH, 720);
        uint(&mut v, id::PIXEL_HEIGHT, 576);
        let mut video = Vec::new();
        el(&mut video, id::VIDEO, &v);
        track(&mut tracks, 1, 1, b"V_MPEG2", &video);
        let mut a = Vec::new();
        ebml::write_f64(&mut a, id::SAMPLING_FREQUENCY, 48_000.0);
        uint(&mut a, id::CHANNELS, 2);
        let mut audio = Vec::new();
        el(&mut audio, id::AUDIO, &a);
        track(&mut tracks, 2, 2, b"A_AC3", &audio);
        let mut comp = Vec::new();
        uint(&mut comp, id::CONTENT_COMP_ALGO, 0);
        let mut enc = Vec::new();
        uint(&mut enc, id::CONTENT_ENCODING_SCOPE, 1);
        el(&mut enc, id::CONTENT_COMPRESSION, &comp);
        let mut encs = Vec::new();
        el(&mut encs, id::CONTENT_ENCODING, &enc);
        let mut sub = Vec::new();
        el(&mut sub, id::CODEC_PRIVATE, b"size: 720x576\n");
        el(&mut sub, id::CONTENT_ENCODINGS, &encs);
        track(&mut tracks, 3, 0x11, b"S_VOBSUB", &sub);
        let mut info = Vec::new();
        uint(&mut info, id::TIMESTAMP_SCALE, MS);
        ebml::write_f64(&mut info, id::DURATION, END_MS as f64);
        let seek_head = |cues_at: u64| {
            let mut s = Vec::new();
            el(&mut s, id::SEEK_ID, id::CUES);
            ebml::write_id(&mut s, id::SEEK_POSITION);
            ebml::write_size(&mut s, 8);
            s.extend_from_slice(&cues_at.to_be_bytes());
            let mut seek = Vec::new();
            el(&mut seek, id::SEEK, &s);
            let mut head = Vec::new();
            el(&mut head, id::SEEK_HEAD, &seek);
            head
        };
        let mut head = Vec::new();
        el(&mut head, id::INFO, &info);
        el(&mut head, id::TRACKS, &tracks);
        let first = (seek_head(0).len() + head.len()) as u64;
        let (mut body, mut cues) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < blocks.len() {
            let n = blocks[i + 1..]
                .iter()
                .position(|b| b.1 == 1 && b.3)
                .map_or(blocks.len(), |n| i + 1 + n);
            let base = blocks[i..n].iter().find(|b| b.1 == 1).map_or(blocks[i].2, |b| b.2);
            let at = first + body.len() as u64;
            let mut cl = Vec::new();
            uint(&mut cl, id::TIMESTAMP, base);
            for (_, track, stamp, key, payload) in &blocks[i..n] {
                let mut block = vec![0x80 | *track as u8];
                block.extend_from_slice(&((*stamp as i64 - base as i64) as i16).to_be_bytes());
                block.push(if *key && *track != 3 { 0x80 } else { 0 });
                block.extend_from_slice(payload);
                match track {
                    3 => {
                        cues.push((*stamp, 3, at, Some(cl.len() as u64)));
                        let mut group = Vec::new();
                        el(&mut group, id::BLOCK, &block);
                        el(&mut cl, id::BLOCK_GROUP, &group);
                    }
                    _ => {
                        if *track == 1 && *key {
                            cues.push((*stamp, 1, at, None));
                        }
                        el(&mut cl, id::SIMPLE_BLOCK, &block);
                    }
                }
            }
            el(&mut body, id::CLUSTER, &cl);
            if let Some((_, reach)) = self.damage.filter(|d| d.0 == base) {
                body.push(0x8F);
                ebml::write_size_fixed(&mut body, reach, 4);
            }
            i = n;
        }
        let mut cue_body = Vec::new();
        cues.sort_by_key(|c| c.0);
        for (t, track, at, relative) in cues {
            let mut tp = Vec::new();
            uint(&mut tp, id::CUE_TRACK, track);
            uint(&mut tp, id::CUE_CLUSTER_POSITION, at);
            if let Some(r) = relative {
                uint(&mut tp, id::CUE_RELATIVE_POSITION, r);
            }
            let mut cp = Vec::new();
            uint(&mut cp, id::CUE_TIME, t);
            el(&mut cp, id::CUE_TRACK_POSITIONS, &tp);
            el(&mut cue_body, id::CUE_POINT, &cp);
        }
        let mut segment = seek_head(first + body.len() as u64);
        segment.extend_from_slice(&head);
        segment.extend_from_slice(&body);
        if self.cues {
            el(&mut segment, id::CUES, &cue_body);
        }
        let mut header = Vec::new();
        uint(&mut header, id::EBML_VERSION, 1);
        el(&mut header, id::DOC_TYPE, b"matroska");
        let mut file = Vec::new();
        el(&mut file, id::EBML, &header);
        ebml::write_id(&mut file, id::SEGMENT);
        ebml::write_size_fixed(&mut file, segment.len() as u64, 8);
        file.extend_from_slice(&segment);
        file
    }
}

fn demuxer(file: &[u8]) -> MkvDemux {
    MkvDemux::with_parsed(Arc::new(parsed(file))).without_extras()
}

/// Every picture from `from` to the end of the film, none skipped.
fn pictures_from(from: u64) -> Vec<u64> {
    (from..END_MS).step_by(40).map(|t| t * MS).collect()
}

/// The sweep's exact seek with a VobSub track on. The caption up at the target was stored
/// ahead of the key's Cluster, so the restart is the key below its block, and the scan from
/// there reaches the target with every picture from the key on.
#[test]
fn a_seek_with_a_vobsub_track_on_restarts_behind_its_key_and_plays_on() {
    let file = Film { cues: true, ..Film::default() }.build();
    for (text_ms, seek_ms) in [(4_100, 61_700), (9_000, 87_100)] {
        let ran = sweep(demuxer(&file), &file, Some(text_ms), seek_ms);
        let key = seek_ms / KEY_MS * KEY_MS;
        assert_eq!(ran.after_flush(0), pictures_from(key), "the pictures of the seek to {seek_ms}");
        let audio = ran.after_flush(1).first().copied();
        // the AC-3 frame up at the target less its pre-roll
        assert!(audio.is_some_and(|t| t.abs_diff(seek_ms * MS) < 64 * MS), "{audio:?}");
        let captions = ran.after_flush(2);
        let (first, last) = (captions.first().copied(), captions.last().copied());
        assert!(first.is_some_and(|t| t <= seek_ms * MS), "the caption up, {first:?}");
        assert_eq!(last, Some(115_000 * MS), "every caption to the end");
    }
}

/// A run of damage behind the restart's Cluster that reads as an element of another level, its
/// size reaching most of the file on, is resynced past and not skipped by that size.
#[test]
fn a_seek_restart_resyncs_past_damage_read_as_an_element() {
    let reach = 7 << 20;
    let file = Film { cues: true, damage: Some((64_000, reach)) }.build();
    let ran = sweep(demuxer(&file), &file, None, 61_700);
    assert_eq!(ran.after_flush(0), pictures_from(60_000), "every picture past the damage");
    assert!(ran.after_flush(1).last().is_some_and(|t| *t >= 119_900 * MS));
}
