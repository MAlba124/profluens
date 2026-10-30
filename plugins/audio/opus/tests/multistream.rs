//! 5.1 Opus through `bytesrc ! mkvdemux ! opusdec ! pcmsink`, one sine per channel in mask order,
//! checked against ffmpeg's decode when ffmpeg is present.

#![allow(clippy::disallowed_methods)] // test fixtures and the ffmpeg oracle, not a hot path

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use pf_mkv::MkvDemux;
use pf_mkv::ebml::id;
use pf_opus::OpusDec;
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{OfferDesc, Value};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const RATE: f64 = 48_000.0;
const TONES: [f64; 6] = [440.0, 660.0, 880.0, 60.0, 1100.0, 1320.0];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
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
        let n = buf.memory.capacity().min(1024).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("audio/raw")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "pcmsink",
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
struct Got {
    pcm: Vec<u8>,
    channels: Option<i64>,
}

struct PcmSink {
    got: Arc<Mutex<Got>>,
}

impl PcmSink {
    const IN: SinkPad = SinkPad(0);

    fn learn(&self, ctx: &Ctx) {
        let Some(fixed) = ctx.negotiated_sink(Self::IN) else { return };
        let channels = ctx.field_id("channels").and_then(|f| fixed.get(f));
        if let Some(Value::Int(n)) = channels {
            self.got.lock().unwrap().channels = Some(n);
        }
    }
}

impl Element for PcmSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.learn(ctx);
        let mut got = self.got.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            got.pcm.extend_from_slice(buf.memory.data());
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(_) = event {
            self.learn(ctx);
        }
        Ok(())
    }
}

fn header_prefix(stream: &[u8]) -> Vec<u8> {
    let cluster = stream.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster");
    stream[..cluster].to_vec()
}

fn decode(file: &[u8]) -> (i64, Vec<i16>) {
    let got = Arc::new(Mutex::new(Got::default()));
    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: file.to_vec(), pos: 0 });
    let demux = p.add(MkvDemux::new(header_prefix(file)));
    p.link(src.src(ByteSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 1, "one A_OPUS track");
    let dec = p.add(OpusDec::new());
    let sink = p.add(PcmSink { got: Arc::clone(&got) });
    p.link(added[0].element.src(added[0].pad), dec.sink(OpusDec::IN)).expect("demux -> opusdec");
    p.link(dec.src(OpusDec::OUT), sink.sink(PcmSink::IN)).expect("opusdec -> sink");
    p.run().expect("run");
    let got = got.lock().unwrap();
    let pcm = got.pcm.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
    (got.channels.expect("a format reached the sink"), pcm)
}

fn power(pcm: &[i16], channels: usize, c: usize, freq: f64) -> f64 {
    let w = std::f64::consts::TAU * freq / RATE;
    let k = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for frame in pcm.chunks_exact(channels) {
        let s = f64::from(frame[c]) + k * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    s1 * s1 + s2 * s2 - k * s1 * s2
}

fn snr_db(ours: &[i16], theirs: &[i16], channels: usize) -> Vec<f64> {
    let frames = ours.len().min(theirs.len()) / channels;
    (0..channels)
        .map(|c| {
            let (mut sig, mut err) = (0.0f64, 0.0f64);
            for i in 0..frames {
                let t = f64::from(theirs[i * channels + c]);
                let d = f64::from(ours[i * channels + c]) - t;
                sig += t * t;
                err += d * d;
            }
            if err == 0.0 { f64::INFINITY } else { 10.0 * (sig / err).log10() }
        })
        .collect()
}

fn ffmpeg_decode(path: &std::path::Path) -> Option<Vec<i16>> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-c:a", "libopus", "-i"])
        .arg(path)
        .args(["-f", "s16le", "-"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect())
}

struct Counting;

thread_local! {
    static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn count() {
    let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
}

// SAFETY: forwards to `System` unchanged and bumps a const thread local.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        count();
        unsafe { std::alloc::System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new: usize) -> *mut u8 {
        count();
        unsafe { std::alloc::System.realloc(p, l, new) }
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

fn track(file: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut reader = pf_mkv::MatroskaReader::new();
    reader.push(file).expect("the fixture parses");
    let head = reader.tracks()[0].codec_private.clone();
    (head, std::iter::from_fn(|| reader.next_frame()).map(|f| f.data).collect())
}

fn steady_allocs(head: &[u8], packets: &[Vec<u8>]) -> Vec<usize> {
    let mut h = pf_harness::Harness::new(OpusDec::new());
    let mut feed = |bytes: &[u8]| {
        let buf = h.alloc(bytes);
        let before = ALLOCS.with(std::cell::Cell::get);
        h.push(buf).expect("push");
        let n = ALLOCS.with(std::cell::Cell::get) - before;
        drop(h.drain_outputs());
        n
    };
    feed(head);
    for p in &packets[..5] {
        feed(p);
    }
    packets[5..].iter().map(|p| feed(p)).collect()
}

fn stereo(n: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
    let cfg = pf_opus::EncoderConfig::new(2);
    let mut enc = pf_opus::libopus::LibopusEncoder::new(cfg).expect("encoder");
    let frame_bytes = enc.frame_bytes();
    let pcm: Vec<u8> = (0..n * frame_bytes / 2)
        .flat_map(|i| (((i as f64 * 0.03).sin() * 8000.0) as i16).to_le_bytes())
        .collect();
    let packets = pcm
        .chunks_exact(frame_bytes)
        .map(|f| {
            let mut packet = Vec::new();
            enc.encode(f, &mut packet).expect("encode");
            packet
        })
        .collect();
    (head, packets)
}

#[test]
fn a_steady_5_1_decode_allocates_what_stereo_does() {
    let file = std::fs::read(fixture("sines_5_1.mka")).expect("the fixture");
    let (head, packets) = track(&file);
    assert_eq!(packets.len(), 26, "half a second and the pre-skip in 20 ms packets");
    let surround = steady_allocs(&head, &packets);
    let (head, packets) = stereo(26);
    let baseline = steady_allocs(&head, &packets);
    eprintln!("allocations per steady packet, 5.1 {surround:?}, stereo {baseline:?}");
    assert_eq!(surround, baseline);
}

#[test]
fn a_5_1_matroska_track_decodes_in_canonical_order() {
    let path = fixture("sines_5_1.mka");
    let file = std::fs::read(&path).expect("the fixture");
    let (channels, pcm) = decode(&file);
    assert_eq!(channels, 6, "the head's 6 channels are announced");
    let ch = channels as usize;
    assert_eq!(pcm.len() % ch, 0, "whole frames");
    // The last Block's DiscardPadding of 13.5 ms is cut, so exactly the half second.
    let frames = pcm.len() / ch;
    assert_eq!(frames, 24_000, "{frames} frames");

    for c in 0..ch {
        let p: Vec<f64> = TONES.iter().map(|&f| power(&pcm, ch, c, f)).collect();
        let own = p[c];
        let next =
            p.iter().enumerate().filter(|&(i, _)| i != c).map(|(_, &v)| v).fold(0.0, f64::max);
        let margin = 10.0 * (own / next.max(1.0)).log10();
        eprintln!("channel {c}: {} Hz leads by {margin:.1} dB", TONES[c]);
        assert!(margin > 30.0, "channel {c} should carry {} Hz, powers {p:?}", TONES[c]);
    }

    let Some(theirs) = ffmpeg_decode(&path) else {
        eprintln!("skip the ffmpeg gate, no ffmpeg with libopus");
        return;
    };
    assert_eq!(theirs.len(), 24_000 * ch, "ffmpeg trims to the half second");
    let snr = snr_db(&pcm, &theirs, ch);
    eprintln!("SNR against ffmpeg per channel L R C LFE Ls Rs: {snr:.1?}");
    // Measured 109 to 119 dB.
    let floor = 90.0;
    for (c, s) in snr.iter().enumerate() {
        assert!(*s >= floor, "channel {c}: {s:.1} dB against ffmpeg, floor {floor} dB");
    }
}
