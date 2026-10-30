// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::{Arc, Mutex};

use pf_avi::AviDemux;
use pf_avi::riff::{self, MoviWalker, StreamKind};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_elements::io::FileSrc;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.avi");

#[allow(clippy::disallowed_methods)] // test setup reads the fixture directly
fn header_prefix(path: &str) -> Vec<u8> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).expect("open fixture");
    let mut buf = vec![0u8; 1024 * 1024];
    let n = f.read(&mut buf).expect("read header");
    buf.truncate(n);
    buf
}

static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any("bytes"), OfferDesc::any("mpeg4/asp")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "capturesink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active, // its own group, so buffers cross a real ring
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct Capture {
    buffers: Vec<(usize, i64)>,
    total_bytes: u64,
    flushed_at: Option<usize>,
}

struct CaptureSink {
    cap: Arc<Mutex<Capture>>,
    pace: std::time::Duration,
}

impl CaptureSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for CaptureSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut taken = 0;
        let mut cap = self.cap.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            let len = buf.memory.data().len();
            cap.total_bytes += len as u64;
            cap.buffers.push((len, buf.pts.nanos().map(|n| n as i64).unwrap_or(-1)));
            taken += 1;
        }
        drop(cap);
        std::thread::sleep(self.pace * taken);
        Ok(Flow::Ok)
    }
    fn event(&mut self, _ctx: &mut Ctx, event: &profluens_core::event::Event) -> Result<(), Error> {
        if matches!(event, profluens_core::event::Event::FlushStart) {
            let mut cap = self.cap.lock().unwrap();
            cap.flushed_at = Some(cap.buffers.len());
        }
        Ok(())
    }
}

fn run_demux() -> Vec<Arc<Mutex<Capture>>> {
    let mut p = pf_reactor::pipeline();
    p.set_pool(1024 * 1024, 32);
    let header = header_prefix(TINY);
    let src = p.add(FileSrc::new(TINY));
    let demux = p.add(AviDemux::new(header));
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");

    let added = p.preroll().expect("preroll").srcs;
    let mut caps = Vec::new();
    for ap in &added {
        let cap = Arc::new(Mutex::new(Capture::default()));
        let sink = p.add(CaptureSink { cap: Arc::clone(&cap), pace: std::time::Duration::ZERO });
        p.link(ap.element.src(ap.pad), sink.sink(CaptureSink::IN)).expect("demux ! sink");
        caps.push(cap);
    }
    p.run().expect("run");
    caps
}

#[test]
fn after_a_seek_the_chunks_are_stamped_on_from_the_seeks_time() {
    use profluens_core::time::Timestamp;
    use std::time::{Duration, Instant};

    let mut p = pf_reactor::pipeline();
    p.set_pool(1024 * 1024, 32);
    let src = p.add(FileSrc::new(TINY));
    let demux = p.add(AviDemux::new(header_prefix(TINY)));
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");
    let added = p.preroll().expect("preroll").srcs;
    let mut caps = Vec::new();
    for ap in &added {
        let cap = Arc::new(Mutex::new(Capture::default()));
        let sink = p.add(CaptureSink { cap: Arc::clone(&cap), pace: Duration::from_millis(5) });
        p.link(ap.element.src(ap.pad), sink.sink(CaptureSink::IN)).expect("demux ! sink");
        caps.push(cap);
    }
    let (seek, stop) = (p.control(), p.control());
    let run = std::thread::spawn(move || p.run());
    let wait = |what: &str, cond: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < Duration::from_secs(10), "never {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    wait("got going", &|| caps.iter().all(|c| c.lock().unwrap().buffers.len() >= 2));
    let to = Timestamp::from_secs(2);
    let _ = seek.seek_bytes(to, &[(profluens_core::command::ALL, 100_000, to)]);
    let behind = |c: &Arc<Mutex<Capture>>| {
        let c = c.lock().unwrap();
        c.flushed_at.and_then(|at| c.buffers.get(at).map(|b| b.1))
    };
    wait("put a chunk out behind the seek", &|| caps.iter().all(|c| behind(c).is_some()));
    stop.stop();
    let _ = run.join().expect("the run thread");
    for (pad, cap) in caps.iter().enumerate() {
        let first = behind(cap).expect("a chunk behind the seek");
        assert!((first - to.0 as i64).abs() <= 40_000_000, "pad {pad} resumed at {first} ns");
    }
}

#[test]
fn preroll_discovers_video_and_audio_pads() {
    let mut p = pf_reactor::pipeline();
    let header = header_prefix(TINY);
    let src = p.add(FileSrc::new(TINY));
    let demux = p.add(AviDemux::new(header));
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");
    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 2, "two src pads (video + audio)");
    assert_eq!(added[0].name, "src_stream0");
    assert_eq!(added[1].name, "src_stream1");
}

#[test]
fn demux_emits_every_chunk_with_correct_counts() {
    let (v_chunks, v_bytes, a_chunks, a_bytes) = parser_counts();
    assert_eq!(v_chunks, 75, "ffprobe reports 75 video frames");
    assert_eq!(a_chunks, 87, "ffprobe reports 87 audio frames");

    let caps = run_demux();
    assert_eq!(caps.len(), 2);
    let v = caps[0].lock().unwrap();
    let a = caps[1].lock().unwrap();
    assert_eq!(v.buffers.len(), v_chunks, "one output buffer per video chunk");
    assert_eq!(v.total_bytes, v_bytes, "no video bytes lost");
    assert_eq!(a.buffers.len(), a_chunks, "one output buffer per audio chunk");
    assert_eq!(a.total_bytes, a_bytes, "no audio bytes lost");
}

#[test]
fn video_pts_advances_monotonically_at_frame_rate() {
    let caps = run_demux();
    let v = caps[0].lock().unwrap();
    assert_eq!(v.buffers[0].1, 0, "first video frame at PTS 0");
    let mut prev = -1i64;
    for (i, &(_, pts)) in v.buffers.iter().enumerate() {
        assert!(pts >= 0, "video frame {i} carries a PTS");
        assert!(pts > prev, "video PTS strictly increasing at frame {i}");
        prev = pts;
    }
    let dt = v.buffers[1].1 - v.buffers[0].1;
    assert!((39_000_000..=41_000_000).contains(&dt), "≈40 ms between video frames, got {dt} ns");
}

#[test]
fn audio_pts_is_monotonic() {
    let caps = run_demux();
    let a = caps[1].lock().unwrap();
    let mut prev = -1i64;
    for (i, &(_, pts)) in a.buffers.iter().enumerate() {
        assert!(pts >= 0, "audio frame {i} carries a PTS");
        assert!(pts > prev || i == 0, "audio PTS non-decreasing at frame {i}");
        prev = pts;
    }
}

#[test]
fn seek_index_from_idx1_has_video_keyframes() {
    let header = header_prefix(TINY);
    let file_len = std::fs::metadata(TINY).unwrap().len();
    let (idx1_off, idx1_size) = find_idx1(TINY, file_len).expect("tiny.avi has an idx1");
    let payload = read_at(TINY, idx1_off + 8, idx1_size as usize);
    let si = pf_avi::build_seek_index(&header, &payload, file_len).expect("build seek index");
    assert!(!si.entries.is_empty(), "at least one video keyframe indexed");
    assert_eq!(si.entries[0].time_ns, 0, "first keyframe at time 0");
    assert_eq!(si.file_len, Some(file_len));
    let mut prev_t = 0u64;
    for &profluens_core::pipeline::IndexEntry { time_ns: t, byte: b, .. } in &si.entries {
        assert!(t >= prev_t, "keyframe times ascend");
        assert!(b < file_len, "keyframe byte offset inside the file");
        prev_t = t;
    }
    let (_h, movi) = riff::probe_header(&header).unwrap();
    assert_eq!(si.entries[0].byte, movi.unwrap().movi_data_start, "frame 0 → movi data start");
}

#[test]
fn truncated_avi_does_not_panic() {
    #[allow(clippy::disallowed_methods)] // test setup reads the fixture directly
    let full = std::fs::read(TINY).unwrap();
    for cut in [0usize, 4, 12, 64, 256, 4096, full.len() / 2, full.len() - 1] {
        let _ = riff::probe_header(&full[..cut.min(full.len())]);
    }
    let mut w = MoviWalker::new();
    w.push(&full[full.len() / 3..]);
    let mut n = 0;
    while w.next_chunk().is_some() && n < 1_000_000 {
        n += 1;
    }
}

fn parser_counts() -> (usize, u64, usize, u64) {
    #[allow(clippy::disallowed_methods)] // test setup reads the fixture directly
    let full = std::fs::read(TINY).unwrap();
    let (avi, movi) = riff::probe_header(&full).expect("probe");
    let movi = movi.unwrap();
    let video_idx = avi.streams.iter().find(|s| matches!(s.kind, StreamKind::Video)).unwrap().index;
    let audio_idx = avi.streams.iter().find(|s| matches!(s.kind, StreamKind::Audio)).unwrap().index;
    let mut w = MoviWalker::new();
    w.push(&full[movi.movi_data_start as usize..]);
    let (mut vc, mut vb, mut ac, mut ab) = (0usize, 0u64, 0usize, 0u64);
    while let Some(c) = w.next_chunk() {
        if c.stream_index == video_idx {
            vc += 1;
            vb += c.data.len() as u64;
        } else if c.stream_index == audio_idx {
            ac += 1;
            ab += c.data.len() as u64;
        }
    }
    (vc, vb, ac, ab)
}

#[allow(clippy::disallowed_methods)] // test setup reads the fixture directly
fn read_at(path: &str, off: u64, len: usize) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).unwrap();
    let mut buf = vec![0u8; len];
    let n = f.read_at(&mut buf, off).unwrap();
    buf.truncate(n);
    buf
}

#[allow(clippy::disallowed_methods)] // test setup reads the fixture directly
fn find_idx1(path: &str, file_len: u64) -> Option<(u64, u32)> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).ok()?;
    let window = (4 * 1024 * 1024).min(file_len) as usize;
    let start = file_len - window as u64;
    let mut buf = vec![0u8; window];
    let n = f.read_at(&mut buf, start).ok()?;
    buf.truncate(n);
    let mut found = None;
    let mut i = 0;
    while i + 8 <= buf.len() {
        if &buf[i..i + 4] == b"idx1" {
            let size = u32::from_le_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]]);
            let abs = start + i as u64;
            if abs + 8 + size as u64 <= file_len {
                found = Some((abs, size));
            }
        }
        i += 1;
    }
    found
}

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

fn strh(
    kind: &[u8; 4],
    handler: &[u8; 4],
    scale: u32,
    rate: u32,
    length: u32,
    sample_size: u32,
) -> Vec<u8> {
    let mut body = vec![0u8; 56];
    body[0..4].copy_from_slice(kind);
    body[4..8].copy_from_slice(handler);
    body[20..24].copy_from_slice(&scale.to_le_bytes());
    body[24..28].copy_from_slice(&rate.to_le_bytes());
    body[32..36].copy_from_slice(&length.to_le_bytes());
    body[44..48].copy_from_slice(&sample_size.to_le_bytes());
    chunk(b"strh", &body)
}

fn ac3_frame(n: u8) -> Vec<u8> {
    let mut f = vec![0x0b, 0x77, 0, 0, 30, 8 << 3];
    f.resize(1792, n);
    f
}

fn byte_sliced_ac3_file(frames: u8) -> Vec<u8> {
    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&41_708u32.to_le_bytes());
    avih[16..20].copy_from_slice(&2u32.to_le_bytes());
    let mut bitmap = vec![0u8; 40];
    bitmap[0..4].copy_from_slice(&40u32.to_le_bytes());
    bitmap[4..8].copy_from_slice(&160i32.to_le_bytes());
    bitmap[8..12].copy_from_slice(&120i32.to_le_bytes());
    bitmap[16..20].copy_from_slice(b"XVID");
    let mut wave = 0x2000u16.to_le_bytes().to_vec();
    wave.extend_from_slice(&6u16.to_le_bytes());
    wave.extend_from_slice(&48_000u32.to_le_bytes());
    wave.extend_from_slice(&56_000u32.to_le_bytes());
    wave.extend_from_slice(&1u16.to_le_bytes());
    wave.extend_from_slice(&[0u8; 4]);
    let mut audio = Vec::new();
    for n in 0..frames {
        audio.extend_from_slice(&ac3_frame(n));
    }
    let slices: Vec<&[u8]> = audio.chunks(2333).collect();
    let hdrl = [
        chunk(b"avih", &avih),
        list(
            b"strl",
            &[
                strh(b"vids", b"XVID", 1001, 24_000, slices.len() as u32, 0),
                chunk(b"strf", &bitmap),
            ]
            .concat(),
        ),
        list(
            b"strl",
            &[strh(b"auds", &[0; 4], 1, 56_000, audio.len() as u32, 1), chunk(b"strf", &wave)]
                .concat(),
        ),
    ]
    .concat();
    let mut movi = Vec::new();
    for slice in &slices {
        movi.extend_from_slice(&chunk(b"00dc", &[0, 0, 1, 0xb6, 0x00, 7, 7, 7]));
        movi.extend_from_slice(&chunk(b"01wb", slice));
    }
    chunk(b"RIFF", &[b"AVI ".to_vec(), list(b"hdrl", &hdrl), list(b"movi", &movi)].concat())
}

#[test]
fn byte_sliced_ac3_chunks_come_out_as_frames_stamped_by_their_bytes() {
    let file = byte_sliced_ac3_file(20);
    let path = std::env::temp_dir().join(format!("pf-avi-sliced-{}.avi", std::process::id()));
    std::fs::write(&path, &file).expect("write");
    let path = path.to_string_lossy().into_owned();

    let mut p = pf_reactor::pipeline();
    p.set_pool(1024 * 1024, 32);
    let src = p.add(FileSrc::new(&path));
    let demux = p.add(AviDemux::new(file.clone()));
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");
    let added = p.preroll().expect("preroll").srcs;
    let mut caps = Vec::new();
    for ap in &added {
        let cap = Arc::new(Mutex::new(Capture::default()));
        let sink = p.add(CaptureSink { cap: Arc::clone(&cap), pace: std::time::Duration::ZERO });
        p.link(ap.element.src(ap.pad), sink.sink(CaptureSink::IN)).expect("demux ! sink");
        caps.push(cap);
    }
    p.run().expect("run");
    let _ = std::fs::remove_file(&path);

    let audio = caps[1].lock().unwrap();
    let lens: Vec<usize> = audio.buffers.iter().map(|b| b.0).collect();
    assert!(lens.len() >= 19, "whole frames, not slices, got {lens:?}");
    assert!(lens.iter().all(|&n| n == 1792), "{lens:?}");
    let pts: Vec<i64> = audio.buffers.iter().map(|b| b.1).collect();
    assert_eq!(pts[0], 0);
    assert_eq!(
        pts[1],
        1792 * (1_000_000_000 / 56_000),
        "a frame is 32 ms of bytes, a byte's tick rounded down"
    );
    assert!(pts.windows(2).all(|w| w[1] - w[0] == pts[1]), "{pts:?}");
    assert_eq!(audio.total_bytes, lens.len() as u64 * 1792);
}
