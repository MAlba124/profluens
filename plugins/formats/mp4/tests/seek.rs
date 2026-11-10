//! MP4 seek, each track restarting at its own point. The handmade movie is 4 s of `vp09` at 25 fps
//! beside 48 kHz AAC, audio chunks first and 213 ms long, so a time's audio lies ahead of its
//! video.

// Tests own their fixtures and staging `Vec`s.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_mp4::{Mp4Demux, Mp4Reader, Restart};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::command::{Control, Target};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::{IndexEntry, Pipeline, Rejoin};
use profluens_core::time::Timestamp;

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "seeksrc",
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
enum Jump {
    Nowhere { to_time: Timestamp },
    To { to_time: Timestamp, to_byte: u64, at: Timestamp },
    Each { to_time: Timestamp, to_byte: u64, video_from: Timestamp },
}

struct SeekSrc {
    data: Vec<u8>,
    pos: usize,
    seek: Arc<Control>,
    jump: Jump,
    sought: bool,
    ids: Arc<Mutex<(ElementId, ElementId)>>,
}

impl SeekSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for SeekSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() && !self.sought {
            self.sought = true;
            match self.jump {
                Jump::Nowhere { to_time } => {
                    let _ = self.seek.seek_bytes(to_time, &[]);
                }
                Jump::To { to_time, to_byte, at } => {
                    let _ = self
                        .seek
                        .seek_bytes(to_time, &[(profluens_core::command::ALL, to_byte, at)]);
                }
                Jump::Each { to_time, to_byte, video_from } => {
                    let (source, demux) = *self.ids.lock().unwrap();
                    let targets: [Target; 2] =
                        [(source, to_byte, video_from), (demux, to_byte, video_from)];
                    let _ = self.seek.seek_bytes(to_time, &targets);
                }
            }
            return Ok(Flow::Ok);
        }
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = 512.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.pos = ctx.seek_target().map_or(0, |target| target.to_byte as usize);
        }
        Ok(())
    }
}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
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

#[derive(Clone, PartialEq, Eq, Debug)]
struct Out {
    bytes: Vec<u8>,
    pts: Option<u64>,
    key: bool,
}

#[derive(Default)]
struct Recorded {
    bufs: Vec<Out>,
    flushes: Vec<usize>,
}

impl Recorded {
    fn behind(&self) -> &[Out] {
        assert_eq!(self.flushes.len(), 1, "one seek and one flush");
        &self.bufs[self.flushes[0]..]
    }
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
            let key = buf.flags.contains(BufferFlags::KEYFRAME);
            rec.bufs.push(Out { bytes: buf.memory.data().to_vec(), pts: buf.pts.nanos(), key });
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            let mut rec = self.rec.lock().unwrap();
            let at = rec.bufs.len();
            rec.flushes.push(at);
        }
        Ok(())
    }
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut payload = vec![0u8; 4];
    payload.extend_from_slice(body);
    boxed(kind, &payload)
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

const PICTURES: u32 = 100;
const PACKETS: u32 = 200;
const GOP: u32 = 25;
const CHUNKS: u32 = 20;
const SAMPLE: usize = 8;
const ASC: [u8; 2] = [0x11, 0x90];

fn sample(track: u8, n: u32) -> Vec<u8> {
    let mut bytes = vec![track];
    bytes.extend(n.to_be_bytes());
    bytes.resize(SAMPLE, 0);
    bytes
}

fn picture_ns(n: u32) -> u64 {
    u64::from(n) * 40_000_000
}

fn packet_ns(n: u32) -> u64 {
    u64::from(n) * 1024 * 1_000_000_000 / 48_000
}

fn movie() -> (Vec<u8>, Vec<u8>) {
    let ftyp = boxed(b"ftyp", b"isom\0\0\0\0isom");
    let tkhd = |id: u32, dims: [u32; 2]| {
        let mut body = vec![0u8; 8];
        body.extend(be32(&[id, 0, 0]));
        body.extend([0u8; 52]);
        body.extend(be32(&[dims[0] << 16, dims[1] << 16]));
        full(b"tkhd", &body)
    };
    let mdhd = |timescale: u32, duration: u32| {
        let mut body = vec![0u8; 8];
        body.extend(be32(&[timescale, duration, 0]));
        full(b"mdhd", &body)
    };
    let mut vp09 = vec![0u8; 78];
    vp09[7] = 1;
    vp09[24..28].copy_from_slice(&[0, 64, 0, 48]);
    let mut mp4a = vec![0u8; 28];
    mp4a[7] = 1;
    mp4a[16..18].copy_from_slice(&[0, 2]);
    mp4a[24..26].copy_from_slice(&48_000u16.to_be_bytes());
    let mut esds = vec![0x03, 22, 0, 1, 0, 0x04, 17, 0x40];
    esds.extend([0u8; 12]);
    esds.extend([0x05, 2]);
    esds.extend(ASC);
    mp4a.extend(full(b"esds", &esds));

    let stbl = |entry: Vec<u8>, count: u32, delta: u32, offsets: &[u32], sync: Option<Vec<u32>>| {
        let mut stsd = be32(&[1]);
        stsd.extend(entry);
        let mut stco = be32(&[offsets.len() as u32]);
        stco.extend(be32(offsets));
        let mut stbl = full(b"stsd", &stsd);
        stbl.extend(full(b"stts", &be32(&[1, count, delta])));
        stbl.extend(full(b"stsz", &be32(&[SAMPLE as u32, count])));
        stbl.extend(full(b"stsc", &be32(&[1, 1, count / CHUNKS, 1])));
        stbl.extend(full(b"stco", &stco));
        if let Some(sync) = sync {
            let mut stss = be32(&[sync.len() as u32]);
            stss.extend(be32(&sync));
            stbl.extend(full(b"stss", &stss));
        }
        stbl
    };
    let trak = |tkhd: Vec<u8>, mdhd: Vec<u8>, stbl: Vec<u8>| {
        let mdia = boxed(b"mdia", &[mdhd, boxed(b"minf", &boxed(b"stbl", &stbl))].concat());
        boxed(b"trak", &[tkhd, mdia].concat())
    };
    let head = |video_at: &[u32], audio_at: &[u32]| {
        let sync = (0..PICTURES / GOP).map(|gop| gop * GOP + 1).collect();
        let video = stbl(boxed(b"vp09", &vp09), PICTURES, 40, video_at, Some(sync));
        let audio = stbl(boxed(b"mp4a", &mp4a), PACKETS, 1024, audio_at, None);
        let moov = boxed(
            b"moov",
            &[
                trak(tkhd(1, [64, 48]), mdhd(1_000, PICTURES * 40), video),
                trak(tkhd(2, [0, 0]), mdhd(48_000, PACKETS * 1024), audio),
            ]
            .concat(),
        );
        [ftyp.clone(), moov].concat()
    };
    let zeros = vec![0u32; CHUNKS as usize];
    let data = head(&zeros, &zeros).len() as u32 + 8;
    let (per_video, per_audio) = (PICTURES / CHUNKS, PACKETS / CHUNKS);
    let stride = (per_video + per_audio) * SAMPLE as u32;
    let audio_at: Vec<u32> = (0..CHUNKS).map(|chunk| data + chunk * stride).collect();
    let video_at: Vec<u32> = audio_at.iter().map(|at| at + per_audio * SAMPLE as u32).collect();
    let head = head(&video_at, &audio_at);
    let mut body = Vec::new();
    for chunk in 0..CHUNKS {
        (0..per_audio).for_each(|n| body.extend(sample(b'A', chunk * per_audio + n)));
        (0..per_video).for_each(|n| body.extend(sample(b'V', chunk * per_video + n)));
    }
    let file = [head.clone(), boxed(b"mdat", &body)].concat();
    (file, head)
}

fn picture_at(file_head: &[u8], n: u32) -> u64 {
    let (chunk, within) = (n / 5, n % 5);
    file_head.len() as u64 + 8 + u64::from(chunk * 15 + 10 + within) * SAMPLE as u64
}

fn packet_at(file_head: &[u8], n: u32) -> u64 {
    let (chunk, within) = (n / 10, n % 10);
    file_head.len() as u64 + 8 + u64::from(chunk * 15 + within) * SAMPLE as u64
}

fn run_file(file: Vec<u8>, head: Vec<u8>, jump: Jump) -> Vec<Recorded> {
    let mut p = Pipeline::new();
    let seek = p.control();
    let ids = Arc::new(Mutex::new((ElementId(0), ElementId(0))));
    let src =
        p.add(SeekSrc { data: file, pos: 0, seek, jump, sought: false, ids: Arc::clone(&ids) });
    let demux = p.add(Mp4Demux::new(head));
    *ids.lock().unwrap() = (src, demux);
    p.link(src.src(SeekSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");
    let pads = p.preroll().expect("preroll").srcs;
    assert_eq!(pads.len(), 2, "a video and an audio track");
    let mut recs = Vec::new();
    for pad in &pads {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let sink = p.add(RecSink { rec: Arc::clone(&rec) });
        p.link(pad.element.src(pad.pad), sink.sink(RecSink::IN)).expect("pad -> sink");
        recs.push(rec);
    }
    p.run().expect("the run ends cleanly");
    recs.iter().map(|rec| std::mem::take(&mut *rec.lock().unwrap())).collect()
}

fn run(jump: Jump) -> (Vec<Out>, Vec<Out>) {
    let (file, head) = movie();
    let pads = run_file(file, head, jump);
    (pads[0].behind().to_vec(), pads[1].behind().to_vec())
}

fn pictures(first: u32) -> Vec<Out> {
    (first..PICTURES)
        .map(|n| Out { bytes: sample(b'V', n), pts: Some(picture_ns(n)), key: n % GOP == 0 })
        .collect()
}

fn packets(first: u32) -> Vec<Out> {
    let head = Out { bytes: ASC.to_vec(), pts: Some(packet_ns(first)), key: false };
    let packet = |n| Out { bytes: sample(b'A', n), pts: Some(packet_ns(n)), key: true };
    std::iter::once(head).chain((first..PACKETS).map(packet)).collect()
}

const MS: fn(u64) -> Timestamp = Timestamp::from_millis;

#[test]
fn the_movie_resolves_as_laid_out() {
    let (file, head) = movie();
    let reader = Mp4Reader::new(&head).expect("resolves");
    assert_eq!(reader.tracks().len(), 2);
    assert_eq!(reader.tracks()[1].family(), "aac");
    assert_eq!(reader.samples().len(), (PICTURES + PACKETS) as usize);
    for s in reader.samples() {
        let at = s.offset as usize;
        let n = u32::from_be_bytes(file[at + 1..at + 5].try_into().unwrap());
        let (letter, want) = if s.track_index == 0 {
            (b'V', picture_at(&head, n))
        } else {
            (b'A', packet_at(&head, n))
        };
        assert_eq!((file[at], s.offset), (letter, want), "sample {n} of track {}", s.track_index);
        assert_eq!(s.sync, s.track_index == 1 || n % GOP == 0);
    }
}

#[test]
fn a_seek_that_names_no_byte_starts_the_file_over() {
    let (file, head) = movie();
    let pads = run_file(file, head, Jump::Nowhere { to_time: MS(300) });
    assert_eq!(pads[0].behind(), pictures(0));
    assert_eq!(pads[1].behind(), packets(0));
    for (track, rec) in pads.iter().enumerate() {
        let (before, behind) = rec.bufs.split_at(rec.flushes[0]);
        assert!(behind.starts_with(before), "track {track} plays from its first sample again");
    }
}

#[test]
fn an_ffmpeg_file_starts_over_too() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny_av.mp4");
    let file = std::fs::read(&path).expect("the fixture");
    let mut at = 0usize;
    while at + 8 <= file.len() && &file[at + 4..at + 8] != b"mdat" {
        let size =
            u32::from_be_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]) as usize;
        at += size.max(8);
    }
    let head = file[..at.min(file.len())].to_vec();
    let pads = run_file(file, head, Jump::Nowhere { to_time: MS(300) });
    for (track, rec) in pads.iter().enumerate() {
        let (before, behind) = rec.bufs.split_at(rec.flushes[0]);
        assert!(
            behind.len() > 4,
            "track {track} put out {} buffers behind the flush",
            behind.len()
        );
        assert!(behind.starts_with(before), "track {track} plays from its first sample again");
    }
}

#[test]
fn each_track_restarts_at_its_own_sample() {
    let (_, head) = movie();
    let tables = Mp4Reader::new(&head).expect("resolves").tables();
    let restart = tables.restart(2_500_000_000, 2_000_000_000, &|_| true);
    assert_eq!(restart, Restart { to_byte: picture_at(&head, 50), at_ns: 2_000_000_000 });
    assert!(
        packet_at(&head, 110) > restart.to_byte && packet_at(&head, 110) < packet_at(&head, 116)
    );
    let (video, audio) =
        run(Jump::Each { to_time: MS(2_500), to_byte: restart.to_byte, video_from: MS(2_000) });
    assert_eq!(video, pictures(50));
    assert_eq!(audio, packets(116));
}

#[test]
fn a_target_on_the_sync_sample_restarts_the_file_ahead_of_it_for_the_audio() {
    let (_, head) = movie();
    let tables = Mp4Reader::new(&head).expect("resolves").tables();
    let restart = tables.restart(2_000_000_000, 2_000_000_000, &|_| true);
    // 2000 ms less 21.33 ms is 1978.67 ms, and packet 92 is stamped 1962.67 ms.
    assert_eq!(restart, Restart { to_byte: packet_at(&head, 92), at_ns: packet_ns(92) });
    assert!(
        picture_at(&head, 45) > restart.to_byte && picture_at(&head, 45) < picture_at(&head, 50)
    );
    let (video, audio) =
        run(Jump::Each { to_time: MS(2_000), to_byte: restart.to_byte, video_from: MS(2_000) });
    assert_eq!(video, pictures(50));
    assert_eq!(audio, packets(92));
}

#[test]
fn a_time_inside_a_gop_names_its_sync_sample() {
    let (_, head) = movie();
    let tables = Mp4Reader::new(&head).expect("resolves").tables();
    let restart = tables.restart(1_000_000_000, 1_990_000_000, &|_| true);
    let (video, audio) =
        run(Jump::Each { to_time: MS(1_000), to_byte: restart.to_byte, video_from: MS(1_990) });
    assert_eq!(video, pictures(25), "the sync sample at one second, the last at or before 1.99 s");
    // 1000 ms less 21.33 ms is 978.67 ms, and packet 45 is stamped 960 ms.
    assert_eq!(audio, packets(45));
}

#[test]
fn a_source_restarted_at_the_sync_sample_starts_audio_at_the_first_packet_read() {
    let (_, head) = movie();
    let to_byte = picture_at(&head, 50);
    let (video, audio) = run(Jump::To { to_time: MS(2_000), to_byte, at: MS(2_000) });
    assert_eq!(video, pictures(50));
    assert_eq!(audio, packets(110));
}

#[test]
fn a_byte_inside_a_sample_starts_each_track_at_its_next() {
    let (_, head) = movie();
    let to_byte = picture_at(&head, 50) + 3;
    let (video, audio) = run(Jump::To { to_time: MS(2_000), to_byte, at: MS(2_000) });
    assert_eq!(
        video,
        pictures(75),
        "picture 50 began before the byte, and 75 is the next sync sample"
    );
    assert_eq!(audio, packets(110));
}

#[test]
fn a_target_past_the_end_hands_on_the_last_gop_and_the_last_packet() {
    let (_, head) = movie();
    let tables = Mp4Reader::new(&head).expect("resolves").tables();
    let restart = tables.restart(9_000_000_000, 9_000_000_000, &|_| true);
    assert_eq!(restart.to_byte, picture_at(&head, 75));
    let (video, audio) =
        run(Jump::Each { to_time: MS(9_000), to_byte: restart.to_byte, video_from: MS(9_000) });
    assert_eq!(video, pictures(75));
    assert_eq!(audio, packets(199));
}

#[test]
fn a_byte_past_the_end_puts_nothing_out() {
    let (file, _) = movie();
    let to_byte = file.len() as u64;
    let (video, audio) = run(Jump::To { to_time: MS(2_000), to_byte, at: MS(2_000) });
    assert!(
        video.is_empty() && audio.is_empty(),
        "{} pictures, {} packets",
        video.len(),
        audio.len()
    );
}

#[test]
fn the_restart_is_the_rule_for_every_target() {
    let (_, head) = movie();
    let tables = Mp4Reader::new(&head).expect("resolves").tables();
    for target_ms in (0..4_200u64).step_by(10) {
        let target = target_ms * 1_000_000;
        let key = (target_ms / 1_000).min(3) as u32 * GOP;
        let line = target.saturating_sub(21_333_334);
        let packet = (0..PACKETS).rev().find(|n| packet_ns(*n) <= line).unwrap_or(0);
        let want = Restart {
            to_byte: picture_at(&head, key).min(packet_at(&head, packet)),
            at_ns: picture_ns(key).min(packet_ns(packet)),
        };
        assert_eq!(
            tables.restart(target, picture_ns(key), &|_| true),
            want,
            "target {target_ms} ms"
        );
    }
}

#[test]
fn the_demuxer_publishes_its_sync_samples_and_where_each_restarts() {
    let (_, head) = movie();
    let mut p = Pipeline::new();
    let demux = p.add(Mp4Demux::new(head.clone()));
    assert!(p.index(demux).is_none(), "nothing before preroll");
    p.preroll().expect("preroll");
    let index = p.index(demux).expect("published at preroll");
    let want: Vec<IndexEntry> = (0..PICTURES / GOP)
        .map(|k| {
            let key = k * GOP;
            let line = picture_ns(key).saturating_sub(21_333_334);
            let packet = (0..PACKETS).rev().find(|n| packet_ns(*n) <= line).unwrap_or(0);
            let (time_ns, byte) = (picture_ns(key), picture_at(&head, key));
            let (restart, restart_ns) =
                (byte.min(packet_at(&head, packet)), time_ns.min(packet_ns(packet)));
            IndexEntry { time_ns, byte, restart, restart_ns }
        })
        .collect();
    assert_eq!(index.entries, want);
    assert!(want.iter().all(|e| e.restart < e.byte), "the audio a keyframe needs lies ahead of it");
    assert_eq!((index.complete, index.file_len), (true, None), "every sync sample, and no length");
    assert_eq!(index.duration, Timestamp(picture_ns(PICTURES).max(packet_ns(PACKETS))));
    let tables = Mp4Reader::new(&head).expect("resolves").tables();
    for e in &index.entries {
        assert_eq!(
            tables.restart(e.time_ns, e.time_ns, &|_| true),
            Restart { to_byte: e.restart, at_ns: e.restart_ns }
        );
    }
}

fn drain(reader: &mut pf_mp4::Mp4Reader) -> Vec<(usize, u32)> {
    let number = |s: pf_mp4::ResolvedSample| {
        let bytes = s.payload.data();
        (s.track_index, u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]))
    };
    std::iter::from_fn(|| reader.next_sample()).map(number).collect()
}

#[test]
fn a_track_that_is_off_is_read_past_and_its_bytes_are_not_waited_for() {
    let (file, head) = movie();
    let mut reader = pf_mp4::Mp4Reader::new(&head).expect("tables");
    reader.select(1, false);
    reader.reset_stream();
    reader.push_bytes(&file);
    let out = drain(&mut reader);
    assert_eq!(out, (0..PICTURES).map(|n| (0, n)).collect::<Vec<_>>());

    let last = picture_at(&head, PICTURES - GOP);
    reader.seek_stream(last, None, picture_ns(PICTURES - GOP));
    reader.push_bytes(&file[last as usize..]);
    assert_eq!(drain(&mut reader), (PICTURES - GOP..PICTURES).map(|n| (0, n)).collect::<Vec<_>>());
}

#[test]
fn a_source_that_goes_back_for_one_track_puts_nothing_out_twice() {
    let (file, head) = movie();
    let mut reader = pf_mp4::Mp4Reader::new(&head).expect("tables");
    reader.select(1, false);
    reader.reset_stream();
    let half = picture_at(&head, 50) as usize;
    reader.push_bytes(&file[..half]);
    let before = drain(&mut reader);
    assert_eq!(before, (0..50).map(|n| (0, n)).collect::<Vec<_>>());

    let point = 1_500_000_000;
    let restart = reader.tables().track_restart(1, point, &|_| true);
    // AAC is primed 1024 samples ahead, so the packet is the last at or before the point less one.
    let line = point - packet_ns(1) - 1;
    let first = (0..PACKETS).rev().find(|n| packet_ns(*n) <= line).expect("a packet");
    assert_eq!((restart.to_byte, restart.at_ns), (packet_at(&head, first), packet_ns(first)));
    reader.rejoin(1, restart.to_byte, point);
    reader.push_bytes(&file[restart.to_byte as usize..]);
    let after = drain(&mut reader);
    let of = |track: usize| -> Vec<u32> {
        after.iter().filter(|(t, _)| *t == track).map(|(_, n)| *n).collect()
    };
    assert_eq!(of(0), (50..PICTURES).collect::<Vec<_>>(), "on from where the walk was");
    assert_eq!(
        of(1),
        (first..PACKETS).collect::<Vec<_>>(),
        "from the packet the point restarts at"
    );
}

#[test]
fn a_track_turned_on_starts_at_its_next_sample_and_video_at_its_next_sync_sample() {
    let (file, head) = movie();
    let mut reader = pf_mp4::Mp4Reader::new(&head).expect("tables");
    reader.select(0, false);
    reader.reset_stream();
    let some = packet_at(&head, 60) as usize;
    reader.push_bytes(&file[..some]);
    assert_eq!(drain(&mut reader), (0..60).map(|n| (1, n)).collect::<Vec<_>>());
    reader.select(0, true);
    reader.push_bytes(&file[some..]);
    let after = drain(&mut reader);
    let pictures: Vec<u32> = after.iter().filter(|(t, _)| *t == 0).map(|(_, n)| *n).collect();
    assert_eq!(pictures, (50..PICTURES).collect::<Vec<_>>());
    assert_eq!(after.iter().filter(|(t, _)| *t == 1).count(), (PACKETS - 60) as usize);
}

#[test]
fn a_pipeline_selects_one_track_and_the_other_costs_nothing() {
    use profluens_core::ctx::StreamKind;
    let (file, head) = movie();
    let mut p = Pipeline::new();
    p.set_select_all(false);
    let seek = p.control();
    let ids = Arc::new(Mutex::new((ElementId(0), ElementId(0))));
    let jump = Jump::Nowhere { to_time: Timestamp::ZERO };
    let src = p.add(SeekSrc { data: file, pos: 0, seek, jump, sought: true, ids });
    let demux = p.add(Mp4Demux::new(head));
    p.link(src.src(SeekSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");
    assert!(p.preroll().expect("preroll").srcs.is_empty(), "describing tracks adds no pad");
    let kinds: Vec<_> = p.streams(demux).iter().map(|s| (s.kind, s.codec, &*s.name)).collect();
    assert_eq!(
        kinds,
        [(StreamKind::Video, "vp9", "src_track1"), (StreamKind::Audio, "aac", "src_track2")]
    );
    let pad = p.select(demux, 0).expect("select");
    let rec = Arc::new(Mutex::new(Recorded::default()));
    let sink = p.add(RecSink { rec: Arc::clone(&rec) });
    p.link(demux.src(pad), sink.sink(RecSink::IN)).expect("pad -> sink");
    p.run().expect("the run ends cleanly");
    assert_eq!(rec.lock().unwrap().bufs, pictures(0));
    let counters = p.counters(demux);
    assert_eq!((counters.buffers_out, counters.drops), (u64::from(PICTURES), 0));
}

fn tiny_head() -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny_av.mp4");
    let file = std::fs::read(&path).expect("the fixture");
    let mut at = 0usize;
    while at + 8 <= file.len() && &file[at + 4..at + 8] != b"mdat" {
        let size =
            u32::from_be_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]) as usize;
        at += size.max(8);
    }
    file[..at.min(file.len())].to_vec()
}

fn find(head: &[u8], needle: &[u8], n: usize) -> usize {
    let hits = head.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle);
    hits.map(|(at, _)| at).nth(n).unwrap_or_else(|| panic!("{needle:?} number {n}"))
}

#[test]
fn a_track_says_its_language_and_its_title() {
    let head = tiny_head();
    let reader = Mp4Reader::new(&head).expect("resolves");
    let said: Vec<_> = reader.tracks().iter().map(|t| (t.language, t.title.as_str())).collect();
    assert_eq!(said, [(*b"und", ""), (*b"und", "")], "ffmpeg's own words name nothing");

    let mut named = head.clone();
    // The second `mdhd`, version 0. The language sits behind the timescale and duration.
    let mdhd = find(&named, b"mdhd", 1);
    assert_eq!(named[mdhd + 4], 0, "version 0");
    let packed: u16 = b"nob".iter().fold(0, |code, c| code << 5 | u16::from(c - 0x60));
    named[mdhd + 24..mdhd + 26].copy_from_slice(&packed.to_be_bytes());
    let name = find(&named, b"SoundHandler", 0);
    named[name..name + 12].copy_from_slice(b"Kommentarer\0");
    let reader = Mp4Reader::new(&named).expect("resolves");
    let said: Vec<_> = reader.tracks().iter().map(|t| (t.language, t.title.as_str())).collect();
    assert_eq!(said, [(*b"und", ""), (*b"nob", "Kommentarer")]);

    named[mdhd + 24..mdhd + 26].copy_from_slice(&[0xff, 0xff]);
    named[name..name + 4].copy_from_slice(&[0xff, 0xfe, 0xfd, 0xfc]);
    let reader = Mp4Reader::new(&named).expect("resolves");
    assert_eq!((reader.tracks()[1].language, reader.tracks()[1].title.as_str()), (*b"und", ""));
}

#[test]
fn the_streams_carry_the_language_and_the_title() {
    let mut head = tiny_head();
    let mdhd = find(&head, b"mdhd", 1);
    let packed: u16 = b"deu".iter().fold(0, |code, c| code << 5 | u16::from(c - 0x60));
    head[mdhd + 24..mdhd + 26].copy_from_slice(&packed.to_be_bytes());
    let title = "Regiekommentar";
    let mut udta = Vec::new();
    udta.extend_from_slice(&(16 + title.len() as u32).to_be_bytes());
    udta.extend_from_slice(b"udta");
    udta.extend_from_slice(&(8 + title.len() as u32).to_be_bytes());
    udta.extend_from_slice(b"name");
    udta.extend_from_slice(title.as_bytes());
    let (moov, trak) = (find(&head, b"moov", 0) - 4, find(&head, b"trak", 1) - 4);
    let size = |head: &[u8], at: usize| {
        u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]])
    };
    let end = trak + size(&head, trak) as usize;
    for at in [moov, trak] {
        let grown = size(&head, at) + udta.len() as u32;
        head[at..at + 4].copy_from_slice(&grown.to_be_bytes());
    }
    head.splice(end..end, udta);

    let mut p = Pipeline::new();
    let demux = p.add(Mp4Demux::new(head));
    p.preroll().expect("preroll");
    let said: Vec<_> = p.streams(demux).iter().map(|s| (&*s.language, &*s.title)).collect();
    assert_eq!(said, [("und", ""), ("deu", "Regiekommentar")]);
}

struct Feed {
    data: Vec<u8>,
    pos: usize,
}

impl Element for Feed {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = 512.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

#[test]
fn a_link_that_joins_a_pad_that_plays_is_greeted_with_the_codec_head() {
    use profluens_core::clock::MockClock;
    use profluens_core::sched::Outcome;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny_av.mp4");
    let file = std::fs::read(&path).expect("the fixture");
    let head = tiny_head();
    let reader = Mp4Reader::new(&head).expect("resolves");
    let asc = reader.tracks()[1].codec_head().to_vec();
    assert!(reader.tracks()[1].family() == "aac" && asc.len() >= 2, "an AudioSpecificConfig");

    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p =
        Pipeline::with(profluens_core::log::DebugSpec::default(), false, Arc::new(clock.clone()));
    let src = p.add(Feed { data: file, pos: 0 });
    let demux = p.add(Mp4Demux::new(head));
    p.link(src.src(SrcPad(0)), demux.sink(Mp4Demux::IN)).expect("src -> demux");
    let pads = p.preroll().expect("preroll").srcs;
    let recs: Vec<Arc<Mutex<Recorded>>> = (0..3).map(|_| Arc::default()).collect();
    for (pad, rec) in pads.iter().zip(&recs) {
        let sink = p.add(RecSink { rec: Arc::clone(rec) });
        p.link(pad.element.src(pad.pad), sink.sink(RecSink::IN)).expect("pad -> sink");
    }
    let (plays, joins) = (&recs[1], &recs[2]);
    let mut run = p.step(Arc::new(clock)).expect("step");
    let pass = |run: &mut profluens_core::sched::Stepped| {
        run.service();
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        for g in &ready {
            run.pass(*g).expect("pass");
        }
        !ready.is_empty()
    };
    while plays.lock().unwrap().bufs.len() < 12 {
        assert!(pass(&mut run), "the stream runs");
    }
    let tap = p.add(RecSink { rec: Arc::clone(joins) });
    p.link(pads[1].element.src(pads[1].pad), tap.sink(RecSink::IN))
        .expect("a second link on the pad");
    p.commit().expect("commit");
    while pass(&mut run) {}
    assert!((0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit)), "the run ends");
    run.finish().expect("finish");

    let (plays, joins) = (plays.lock().unwrap(), joins.lock().unwrap());
    let played: Vec<&[u8]> = plays.bufs.iter().map(|b| &b.bytes[..]).collect();
    assert_eq!(played.len(), 1 + 48, "the head and forty eight packets");
    assert_eq!(
        played.iter().filter(|b| **b == &asc[..]).count(),
        1,
        "the link that plays is handed the head once"
    );
    let joined: Vec<&[u8]> = joins.bufs.iter().map(|b| &b.bytes[..]).collect();
    assert_eq!(joined[0], &asc[..], "the link that joins is handed the head first");
    let packets = &joined[1..];
    assert_eq!(packets.len(), 48 - 11, "from the packet behind the eleven that had been played");
    assert_eq!(
        packets,
        &played[played.len() - packets.len()..],
        "and then the packets the other is handed, each whole"
    );

    let found = std::process::Command::new("which")
        .arg("ffmpeg")
        .output()
        .ok()
        .filter(|out| out.status.success());
    let Some(ffmpeg) = found.map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    else {
        eprintln!("no ffmpeg, the packets are not handed to it");
        return;
    };
    // Object type, sampling index and channels out of the head's two bytes.
    let (object, index, channels) =
        (asc[0] >> 3, (asc[0] & 7) << 1 | asc[1] >> 7, (asc[1] >> 3) & 15);
    let mut adts = Vec::new();
    for packet in packets {
        let len = packet.len() + 7;
        adts.extend_from_slice(&[
            0xff,
            0xf1,
            (object - 1) << 6 | index << 2 | channels >> 2,
            (channels & 3) << 6 | (len >> 11) as u8,
            (len >> 3) as u8,
            ((len & 7) as u8) << 5 | 0x1f,
            0xfc,
        ]);
        adts.extend_from_slice(packet);
    }
    let framed = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("joined.aac");
    std::fs::write(&framed, adts).expect("write the framed packets");
    let decoded = std::process::Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&framed)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(
        decoded.status.success() && decoded.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
}

/// Keeps every buffer it takes until it is taken out, a sink whose link waits.
struct Hold {
    rec: Arc<Mutex<Recorded>>,
    kept: Vec<profluens_core::buffer::Buffer>,
}

impl Element for Hold {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let key = buf.flags.contains(BufferFlags::KEYFRAME);
            let out = Out { bytes: buf.memory.data().to_vec(), pts: buf.pts.nanos(), key };
            self.rec.lock().unwrap().bufs.push(out);
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

/// An audio sink that keeps its packets dries the pool, and the demuxer parks the next sample
/// it read, of the audio or of the video. The audio moved to another sink with a restart starts
/// at its restart point behind its head, not at a packet it parked ahead, and the video loses no
/// picture it parked. The pool sizes park a sample of each.
#[test]
fn a_restart_drops_what_the_track_parked_and_the_other_track_keeps_its_own() {
    use profluens_core::clock::MockClock;
    use profluens_core::topology::Leave;
    let (file, head) = movie();
    for slots in 60..68 {
        let clock = MockClock::starting_at(Timestamp::from_secs(1));
        let debug = profluens_core::log::DebugSpec::default();
        let mut p = Pipeline::with(debug, false, Arc::new(clock.clone()));
        p.set_pool(4096, slots);
        p.set_select_all(false);
        let ids = Arc::new(Mutex::new((ElementId(0), ElementId(0))));
        let jump = Jump::Nowhere { to_time: Timestamp::ZERO };
        let feed =
            SeekSrc { data: file.clone(), pos: 0, seek: p.control(), jump, sought: true, ids };
        let src = p.add(feed);
        let demux = p.add(Mp4Demux::new(head.clone()));
        p.link(src.src(SeekSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");
        p.preroll().expect("preroll");
        let (video, held, moved) = (Arc::default(), Arc::default(), Arc::default());
        let pad = p.select(demux, 0).expect("select the video");
        let sink = p.add(RecSink { rec: Arc::clone(&video) });
        p.link(demux.src(pad), sink.sink(RecSink::IN)).expect("pad -> sink");
        let pad = p.select(demux, 1).expect("select the audio");
        let hold = p.add(Hold { rec: Arc::clone(&held), kept: Vec::new() });
        p.link(demux.src(pad), hold.sink(RecSink::IN)).expect("pad -> hold");
        let mut run = p.step(Arc::new(clock)).expect("step");
        assert!(!step_through(&mut run), "the pool runs dry, {slots} slots");

        let position = held.lock().unwrap().bufs.last().and_then(|b| b.pts).expect("held");
        let index = p.index(demux).expect("an index");
        let Rejoin::At(byte, at) = index.restart_for(1, Timestamp(position)) else {
            panic!("a sync sample, {slots} slots")
        };
        p.remove(hold, Leave::Cut).expect("remove");
        let sink = p.add(RecSink { rec: Arc::clone(&moved) });
        p.link(demux.src(pad), sink.sink(RecSink::IN)).expect("pad -> sink");
        p.commit_at(Timestamp(position), &[(src, byte, at), (demux, byte, at)]).expect("commit");
        assert!(step_through(&mut run), "the run ends, {slots} slots");
        run.finish().expect("finish");

        assert_eq!(video.lock().unwrap().bufs, pictures(0), "every picture once, {slots} slots");
        assert!(at.0 > 0, "the restart is a sync sample past the first");
        let moved = std::mem::take(&mut moved.lock().unwrap().bufs);
        let led = moved.first().is_some_and(|b| b.bytes == ASC);
        assert!(led, "the head leads, not a packet parked ahead, {slots} slots");
        let first = (0..PACKETS).find(|n| packet_at(&head, *n) >= byte).expect("a packet");
        assert_eq!(moved, packets(first), "on from the restart, {slots} slots");
    }
}
