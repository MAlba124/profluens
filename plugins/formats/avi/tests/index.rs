// A test's own fixtures and threads, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pf_avi::{AviDemux, AviIndex, Idx1At};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::feedback::{Bitrate, Feedback, RateKind};
use profluens_core::fetch::drive_slice_log;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::{Index, IndexEntry, Pipeline, SeekMode, SeekPlan};
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

const MS: u64 = 1_000_000;
const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.avi");

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    chunk(b"LIST", &[kind.to_vec(), body.to_vec()].concat())
}

fn strh(kind: &[u8; 4], handler: &[u8; 4], scale: u32, rate: u32, length: u32) -> Vec<u8> {
    let mut body = vec![0u8; 56];
    body[0..4].copy_from_slice(kind);
    body[4..8].copy_from_slice(handler);
    body[20..24].copy_from_slice(&scale.to_le_bytes());
    body[24..28].copy_from_slice(&rate.to_le_bytes());
    body[32..36].copy_from_slice(&length.to_le_bytes());
    chunk(b"strh", &body)
}

#[derive(Clone, Copy, PartialEq)]
enum Tail {
    Idx1,
    JunkIdx1,
    Junk,
}

#[derive(Clone, Copy, Debug)]
struct Laid {
    stream: u8,
    ordinal: u32,
    at: u64,
    key: bool,
}

struct Film {
    data: Vec<u8>,
    chunks: Vec<Laid>,
    len: u64,
}

const GOP: u32 = 25;

fn film(secs: u32, preload: u64, tail: Tail) -> Film {
    let (frames, audio) = (secs * 25, secs * 1_000 / 32);
    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&40_000u32.to_le_bytes());
    avih[16..20].copy_from_slice(&frames.to_le_bytes());
    let mut bitmap = vec![0u8; 40];
    bitmap[0..4].copy_from_slice(&40u32.to_le_bytes());
    bitmap[4..8].copy_from_slice(&160i32.to_le_bytes());
    bitmap[8..12].copy_from_slice(&120i32.to_le_bytes());
    bitmap[16..20].copy_from_slice(b"XVID");
    let mut wave = vec![0u8; 18];
    wave[0..2].copy_from_slice(&0x2000u16.to_le_bytes());
    wave[2..4].copy_from_slice(&2u16.to_le_bytes());
    wave[4..8].copy_from_slice(&48_000u32.to_le_bytes());
    let hdrl = list(
        b"hdrl",
        &[
            chunk(b"avih", &avih),
            list(
                b"strl",
                &[strh(b"vids", b"XVID", 1, 25, frames), chunk(b"strf", &bitmap)].concat(),
            ),
            list(
                b"strl",
                &[strh(b"auds", &[0; 4], 1_536, 48_000, audio), chunk(b"strf", &wave)].concat(),
            ),
        ]
        .concat(),
    );
    let mut order: Vec<(i64, u8, u32)> = (0..frames).map(|n| (n as i64 * 40, 0, n)).collect();
    order.extend((0..audio).map(|n| (n as i64 * 32 - preload as i64, 1, n)));
    order.sort();
    let head = [b"RIFF".to_vec(), 0u32.to_le_bytes().to_vec(), b"AVI ".to_vec(), hdrl].concat();
    let movi_data = head.len() as u64 + 12;
    let mut movi = Vec::new();
    let mut chunks = Vec::new();
    for (_, stream, ordinal) in order {
        let at = movi_data + movi.len() as u64;
        let body: Vec<u8> = match stream {
            0 => [&[0xD0, (ordinal >> 8) as u8, ordinal as u8][..], &[ordinal as u8; 120]].concat(),
            _ => [&[0xA0, (ordinal >> 8) as u8, ordinal as u8][..], &[ordinal as u8; 40]].concat(),
        };
        movi.extend_from_slice(&chunk(if stream == 0 { b"00dc" } else { b"01wb" }, &body));
        chunks.push(Laid { stream, ordinal, at, key: stream == 1 || ordinal % GOP == 0 });
    }
    let mut idx1 = Vec::new();
    for c in &chunks {
        idx1.extend_from_slice(if c.stream == 0 { b"00dc" } else { b"01wb" });
        idx1.extend_from_slice(&(if c.key { 0x10u32 } else { 0 }).to_le_bytes());
        idx1.extend_from_slice(&((c.at - (movi_data - 4)) as u32).to_le_bytes());
        idx1.extend_from_slice(&(if c.stream == 0 { 123u32 } else { 43 }).to_le_bytes());
    }
    let tail = match tail {
        Tail::Idx1 => chunk(b"idx1", &idx1),
        Tail::JunkIdx1 => [chunk(b"JUNK", &[0; 24]), chunk(b"idx1", &idx1)].concat(),
        Tail::Junk => chunk(b"JUNK", &[0; 16]),
    };
    let mut data = [head, list(b"movi", &movi), tail].concat();
    let riff = (data.len() - 8) as u32;
    data[4..8].copy_from_slice(&riff.to_le_bytes());
    let len = data.len() as u64;
    Film { data, chunks, len }
}

fn keyframes(film: &Film) -> Vec<IndexEntry> {
    let lead = 5_333_334;
    film.chunks
        .iter()
        .filter(|c| c.stream == 0 && c.key)
        .map(|k| {
            let time = u64::from(k.ordinal) * 40 * MS;
            let need = time.saturating_sub(lead);
            let audio = film
                .chunks
                .iter()
                .find(|c| c.stream == 1 && u64::from(c.ordinal) * 32 * MS + 32 * MS > need);
            match audio {
                Some(a) if a.at < k.at => IndexEntry {
                    time_ns: time,
                    byte: k.at,
                    restart: a.at,
                    restart_ns: u64::from(a.ordinal) * 32 * MS,
                },
                _ => IndexEntry::key(time, k.at),
            }
        })
        .collect()
}

fn write(name: &str, data: &[u8]) -> String {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("avi_index");
    std::fs::create_dir_all(&dir).expect("a tmp dir");
    let path = dir.join(name);
    std::fs::write(&path, data).expect("write the fixture");
    path.to_str().expect("utf-8").to_owned()
}

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static SINK: ElementDesc = ElementDesc {
    name: "rec",
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

type Row = Option<(Option<u64>, u32, Vec<u8>)>;

struct Rec {
    rows: Arc<Mutex<Vec<Row>>>,
    pace: Duration,
}

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut n = 0;
        let mut rows = self.rows.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rows.push(Some((buf.pts.nanos(), buf.flags.0, buf.memory.data().to_vec())));
            n += 1;
        }
        drop(rows);
        std::thread::sleep(self.pace * n);
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.rows.lock().unwrap().push(None);
        }
        Ok(())
    }
}

const IN: SinkPad = SinkPad(0);

fn build(
    p: &mut Pipeline,
    src: ElementId,
    demux: AviDemux,
    pace: Duration,
) -> (ElementId, Vec<Arc<Mutex<Vec<Row>>>>) {
    let demux = p.add(demux);
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");
    let added = p.preroll().expect("preroll").srcs;
    let mut rows = Vec::new();
    for pad in &added {
        let rec = Arc::new(Mutex::new(Vec::new()));
        let sink = p.add(Rec { rows: Arc::clone(&rec), pace });
        p.link(pad.element.src(pad.pad), sink.sink(IN)).expect("pad ! sink");
        rows.push(rec);
    }
    (demux, rows)
}

fn play(path: &str, head: &[u8], len: u64, push: bool) -> (Vec<Vec<Row>>, Arc<Index>) {
    play_by(path, head, len, push, |d| d)
}

fn play_by(
    path: &str,
    head: &[u8],
    len: u64,
    push: bool,
    make: impl FnOnce(AviDemux) -> AviDemux,
) -> (Vec<Vec<Row>>, Arc<Index>) {
    let mut p = pf_reactor::pipeline();
    let src = match push {
        true => p.add(FileSrc::new(path).push_only()),
        false => p.add(FileSrc::new(path)),
    };
    let demux = make(AviDemux::new(head.to_vec()).with_len(Some(len)));
    let (demux, rows) = build(&mut p, src, demux, Duration::ZERO);
    p.run().expect("run");
    let rows = rows.iter().map(|r| r.lock().unwrap().clone()).collect();
    (rows, p.index(demux).expect("published"))
}

fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let began = Instant::now();
    while !cond() {
        assert!(began.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn the_published_index_is_the_idx1_keyframes_and_where_each_restarts() {
    let film = film(4, 300, Tail::Idx1);
    let path = write("film.avi", &film.data);
    let (_, index) = play(&path, &film.data[..4096], film.len, false);
    let want = keyframes(&film);
    assert_eq!(want.len(), 4);
    assert!(
        want[1..].iter().all(|e| e.restart < e.byte),
        "the audio is ahead of every keyframe but the first"
    );
    assert_eq!(index.entries, want);
    assert!(index.complete);
    assert_eq!((index.file_len, index.duration), (Some(film.len), Timestamp(4_000 * MS)));
    assert!(index.grouped && index.stream.is_none());
}

#[test]
fn a_long_idx1_is_read_in_pieces() {
    let film = film(150, 300, Tail::Idx1);
    let path = write("long.avi", &film.data);
    let (_, index) = play(&path, &film.data[..4096], film.len, false);
    assert!(film.chunks.len() * 16 > 2 * pf_avi::element::IDX1_PIECE as usize, "three pieces");
    assert_eq!(index.entries.len(), 150);
    assert_eq!(index.entries, keyframes(&film));
    assert!(index.complete);
}

#[test]
fn a_chunk_ahead_of_idx1_is_hopped_over() {
    let film = film(2, 0, Tail::JunkIdx1);
    let path = write("junk_idx1.avi", &film.data);
    let (_, index) = play(&path, &film.data[..4096], film.len, false);
    assert_eq!(index.entries, keyframes(&film));
    assert!(index.complete);
}

#[test]
fn an_index_read_once_is_published_by_each_run_handed_it() {
    let film = film(150, 300, Tail::JunkIdx1);
    let path = write("shared.avi", &film.data);
    let head = &film.data[..4096];
    let (want_rows, want) = play(&path, head, film.len, false);
    let movi = pf_avi::probe_header(head).unwrap().1.unwrap();
    let mut log = Vec::new();
    let mut m = Idx1At::new(movi.movi_end, film.len);
    let idx1 = drive_slice_log(&mut m, &film.data, &mut log).unwrap();
    let piece = pf_avi::element::IDX1_PIECE as usize;
    assert_eq!(
        log.len(),
        2,
        "the demuxer reads {} pieces",
        (film.chunks.len() * 16).div_ceil(piece)
    );
    let index = Arc::new(AviIndex::build(head, idx1.as_ref(), Some(film.len)).unwrap());
    let shared = |d: AviDemux| d.with_index(Arc::clone(&index));
    let (rows, got) = play_by(&path, head, film.len, false, shared);
    assert_eq!((&got.entries, got.complete), (&want.entries, true));
    assert_eq!(got.entries, keyframes(&film));
    assert!(rows == want_rows, "the same frames");
    // a file with no idx1 shares the estimate
    let bare = film_without_idx1();
    let path = write("shared_none.avi", &bare.data);
    let index = Arc::new(AviIndex::build(&bare.data[..4096], None, Some(bare.len)).unwrap());
    let (_, got) = play_by(&path, &bare.data[..4096], bare.len, false, |d| d.with_index(index));
    assert!(got.entries.is_empty() && !got.complete);
    assert!(AviIndex::build(b"RIFF", None, None).is_none(), "no header");
}

fn film_without_idx1() -> Film {
    film(2, 0, Tail::Junk)
}

#[test]
fn a_file_without_idx1_publishes_the_estimate() {
    let film = film(2, 0, Tail::Junk);
    let path = write("no_idx1.avi", &film.data);
    let (pulled, index) = play(&path, &film.data[..4096], film.len, false);
    assert!(index.entries.is_empty() && !index.complete);
    assert_eq!((index.file_len, index.duration), (Some(film.len), Timestamp(2_000 * MS)));
    let (pushed, _) = play(&path, &film.data[..4096], film.len, true);
    assert_eq!(pulled, pushed, "the read is the same without an index");
}

#[test]
fn a_pushed_stream_publishes_the_estimate() {
    let film = film(2, 0, Tail::Idx1);
    let path = write("pushed.avi", &film.data);
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&path).push_only());
    let (demux, _) = build(
        &mut p,
        src,
        AviDemux::new(film.data[..4096].to_vec()).with_len(Some(film.len)),
        Duration::ZERO,
    );
    let estimate = Index {
        file_len: Some(film.len),
        duration: Timestamp(2_000 * MS),
        grouped: true,
        ..Index::default()
    };
    assert_eq!(p.index(demux).as_deref(), Some(&estimate), "published at preroll");
    p.run().expect("run");
    assert_eq!(p.index(demux).as_deref(), Some(&estimate), "and nothing else");
    let (byte, at) = estimate.resolve(Timestamp(1_000 * MS)).expect("an estimate");
    assert_eq!((byte, at), (film.len / 2, Timestamp(1_000 * MS)));
}

#[test]
fn the_fixture_pulled_puts_out_what_it_puts_out_pushed() {
    let data = std::fs::read(TINY).expect("the fixture");
    let len = data.len() as u64;
    let (pulled, index) = play(TINY, &data[..64 << 10], len, false);
    let (pushed, estimate) = play(TINY, &data[..64 << 10], len, true);
    assert_eq!(pulled.len(), 2);
    let header = profluens_core::buffer::BufferFlags::HEADER.0;
    let head = pulled[0][0].as_ref().map(|r| r.1 & header != 0);
    assert_eq!(head, Some(true), "the VOL ahead of the pictures");
    assert_eq!(pulled[0].len(), 76, "every picture behind it");
    assert_eq!(pulled[1].len(), 87, "every audio frame");
    assert_eq!(pulled, pushed);
    assert!(estimate.entries.is_empty() && !estimate.complete);
    let (header, movi) = pf_avi::probe_header(&data).expect("probe");
    let movi = movi.expect("movi");
    let at = movi.movi_end as usize;
    assert_eq!(&data[at..at + 4], b"idx1", "idx1 follows movi");
    let size = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap()) as usize;
    let tick = header.streams[0].sample_duration_ns().expect("a frame rate");
    let mut frame = 0u64;
    let mut keys = Vec::new();
    for e in data[at + 8..at + 8 + size].as_chunks::<16>().0 {
        if &e[..4] != b"00dc" {
            continue;
        }
        let flags = u32::from_le_bytes(e[4..8].try_into().unwrap());
        let off = u64::from(u32::from_le_bytes(e[8..12].try_into().unwrap()));
        if flags & 0x10 != 0 {
            keys.push((frame * tick, movi.movi_data_start - 4 + off));
        }
        frame += 1;
    }
    let got: Vec<(u64, u64)> = index.entries.iter().map(|e| (e.time_ns, e.byte)).collect();
    assert!(keys.len() > 1);
    assert_eq!(got, keys);
    assert!(index.complete);
    assert!(index.entries.iter().all(|e| e.restart <= e.byte && e.restart_ns <= e.time_ns));
    assert_eq!(&data[keys[1].1 as usize..][..4], b"00dc", "a keyframe's byte is its chunk");
}

#[test]
fn a_seek_restarts_at_the_audio_ahead_of_the_keyframe_with_exact_stamps() {
    let film = film(8, 300, Tail::Idx1);
    let path = write("seek.avi", &film.data);
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&path));
    let demux = AviDemux::new(film.data[..4096].to_vec()).with_len(Some(film.len));
    let (demux, rows) = build(&mut p, src, demux, Duration::from_millis(4));
    let (control, tap) = (p.control(), p.tap_handle());
    let run = std::thread::spawn(move || p.run());
    until("the index", || tap.index(demux).is_some_and(|index| index.complete));
    until("the first frames", || rows.iter().all(|r| r.lock().unwrap().len() > 4));
    let plan = control.seek(Timestamp(3_500 * MS), SeekMode::Keyframe).expect("a seek");
    assert!(
        matches!(plan, SeekPlan::Seek { key: Timestamp(k), .. } if k == 3_000 * MS),
        "{plan:?}"
    );
    let first = |r: &Arc<Mutex<Vec<Row>>>| {
        let rows = r.lock().unwrap();
        let flush = rows.iter().rposition(|row| row.is_none())?;
        rows.get(flush + 1).cloned().flatten()
    };
    until("both pads behind the seek", || rows.iter().all(|r| first(r).is_some()));
    control.stop();
    let _ = run.join().expect("the run thread");
    let (video, audio) = (first(&rows[0]).unwrap(), first(&rows[1]).unwrap());
    assert_eq!(video.0, Some(3_000 * MS));
    assert_eq!(&video.2[..3], &[0xD0, 0, 75]);
    let entry = keyframes(&film)[3];
    let restart = film.chunks.iter().find(|c| c.at == entry.restart).expect("a chunk");
    assert_eq!((restart.stream, restart.ordinal), (1, 93));
    assert_eq!(audio.0, Some(93 * 32 * MS), "stamped as from the start");
    assert_eq!(&audio.2[..3], &[0xA0, 0, 93]);
}

#[test]
fn a_seek_to_the_start_reads_from_the_first_chunk() {
    let film = film(8, 0, Tail::Idx1);
    let path = write("restart.avi", &film.data);
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&path).push_only());
    let demux = AviDemux::new(film.data[..4096].to_vec()).with_len(Some(film.len));
    let (_, rows) = build(&mut p, src, demux, Duration::from_millis(4));
    let control = p.control();
    let run = std::thread::spawn(move || p.run());
    until("the first frames", || rows[0].lock().unwrap().len() > 8);
    let _ =
        control.seek_bytes(Timestamp::ZERO, &[(profluens_core::command::ALL, 0, Timestamp::ZERO)]);
    let first = |r: &Arc<Mutex<Vec<Row>>>| {
        let rows = r.lock().unwrap();
        let flush = rows.iter().rposition(|row| row.is_none())?;
        rows.get(flush + 1).cloned().flatten()
    };
    until("a picture behind the seek", || first(&rows[0]).is_some());
    control.stop();
    let _ = run.join().expect("the run thread");
    let video = first(&rows[0]).unwrap();
    assert_eq!((video.0, &video.2[..3]), (Some(0), &[0xD0, 0, 0][..]));
}

static TOLD_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &ANY }];
static TOLD: ElementDesc = ElementDesc {
    name: "told",
    sinks: &[],
    srcs: &TOLD_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Told {
    data: Vec<u8>,
    pos: usize,
    ranges: bool,
    rates: Arc<Mutex<Vec<Option<Bitrate>>>>,
}

impl Element for Told {
    fn desc(&self) -> &'static ElementDesc {
        &TOLD
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.ranges {
            ctx.offer_ranges(SrcPad(0));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if ctx.pulled(SrcPad(0)) {
            while let Some(mut ask) = ctx.next_ask(SrcPad(0)) {
                let from = (ask.offset as usize).min(self.data.len());
                let n = (ask.len as usize).min(self.data.len() - from);
                ask.buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[from..from + n]);
                ask.buf.memory.set_len(n);
                ctx.serve(SrcPad(0), ask.tag, ask.buf);
            }
            if ctx.asks_over(SrcPad(0)) {
                return Ok(Flow::Eos);
            }
            ctx.wake_within(Duration::from_millis(1));
            return Ok(Flow::Ok);
        }
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = 4096.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Feedback { feedback: Feedback::Bitrate(b), .. } => {
                self.rates.lock().unwrap().push(Some(*b))
            }
            Event::FlushStart => self.rates.lock().unwrap().push(None),
            _ => {}
        }
        Ok(())
    }
}

fn told(film: &Film, ranges: bool, len: Option<u64>) -> Vec<Bitrate> {
    let rates = Arc::new(Mutex::new(Vec::new()));
    let mut p = pf_reactor::pipeline();
    let src = p.add(Told { data: film.data.clone(), pos: 0, ranges, rates: Arc::clone(&rates) });
    let _ =
        build(&mut p, src, AviDemux::new(film.data[..4096].to_vec()).with_len(len), Duration::ZERO);
    p.run().expect("run");

    rates.lock().unwrap().iter().flatten().copied().collect()
}

fn near(film: &Film, secs: u64, b: &Bitrate) -> bool {
    within(film, secs, b, 5)
}

fn within(film: &Film, secs: u64, b: &Bitrate, part: u64) -> bool {
    let (first, last) = (film.chunks[0].at, film.chunks[film.chunks.len() - 1].at);
    let want = (last - first) * 8 / secs;
    b.bits_per_second.abs_diff(want) <= want / part
}

fn exact(film: &Film, secs: u64, b: &Bitrate) -> bool {
    b.kind == RateKind::Local && within(film, secs, b, 50)
}

#[test]
fn the_rate_is_told_from_idx1_from_the_length_and_as_measured() {
    let secs = 12;
    let film = film(secs as u32, 300, Tail::Idx1);
    let pulled = told(&film, true, Some(film.len));
    assert!(pulled.len() >= 3, "a rate every two seconds, {pulled:?}");
    assert!(pulled.iter().all(|b| near(&film, secs, b)), "{pulled:?}");
    assert!(
        pulled.last().is_some_and(|b| exact(&film, secs, b)),
        "the slope at the end, {pulled:?}"
    );
    let pushed = told(&film, false, Some(film.len));
    assert_eq!(pushed[0].kind, RateKind::Average, "the length over the duration first, {pushed:?}");
    assert!(pushed.iter().all(|b| near(&film, secs, b)), "{pushed:?}");
    assert!(pushed[1..].iter().all(|b| exact(&film, secs, b)), "then measured, {pushed:?}");
    let blind = told(&film, false, None);
    assert!(!blind.is_empty() && blind.iter().all(|b| exact(&film, secs, b)), "{blind:?}");
}

#[test]
fn behind_a_seek_back_the_rate_is_told_again() {
    let film = film(8, 300, Tail::Idx1);
    let rates = Arc::new(Mutex::new(Vec::new()));
    let mut p = pf_reactor::pipeline();
    let src =
        p.add(Told { data: film.data.clone(), pos: 0, ranges: true, rates: Arc::clone(&rates) });
    let demux = AviDemux::new(film.data[..4096].to_vec()).with_len(Some(film.len));
    let (demux, rows) = build(&mut p, src, demux, Duration::from_millis(4));
    let (control, tap) = (p.control(), p.tap_handle());
    let run = std::thread::spawn(move || p.run());
    until("the index", || tap.index(demux).is_some_and(|index| index.complete));
    until("six seconds", || rows[0].lock().unwrap().len() > 150);
    control.seek(Timestamp(500 * MS), SeekMode::Keyframe).expect("a seek");
    let _ = run.join().expect("the run thread");
    let rates = rates.lock().unwrap().clone();
    let behind = rates.iter().rposition(Option::is_none).map_or(0, |at| rates.len() - at - 1);
    assert!(behind >= 3, "told at the seek's place and every two seconds on, {rates:?}");
}
