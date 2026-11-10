// A test's own fixtures and sinks, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pf_avi::AviDemux;
use pf_avi::riff::{self, MoviWalker, Stream, StreamKind};
use profluens_audio::layout::negotiated_layout;
use profluens_audio::{AudioFormat, SampleFormat, negotiated_audio_format};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::SinkPad;
use profluens_elements::io::FileSrc;

/// One second of testsrc as MJPEG beside a sine at `rate` coded with `audio`, made once by
/// ffmpeg, None without it.
fn made(name: &str, rate: u32, audio: &[&str]) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("pf_avi_extensible_{name}_v1.avi"));
    if path.exists() {
        return Some(path);
    }
    // tests make them at once, each its own copy until the rename
    static MADE: AtomicU32 = AtomicU32::new(0);
    let n = MADE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{n}.avi", std::process::id()));
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=10:duration=1"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:sample_rate={rate}:duration=1"))
        .args(["-c:v", "mjpeg"])
        .args(audio)
        .args(["-f", "avi"])
        .arg(&tmp)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: ffmpeg could not make {name}");
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    std::fs::rename(&tmp, &path).expect("the fixture in place");
    Some(path)
}

static SAMPLES: [ValueDesc; 5] = [
    ValueDesc::Id("u8"),
    ValueDesc::Id("s16"),
    ValueDesc::Id("s24"),
    ValueDesc::Id("s32"),
    ValueDesc::Id("f32"),
];
// a field or value no offer of the pipeline names is not known, an announcement of it is dropped
static RAW_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "sample", allowed: ConstraintDesc::Set(&SAMPLES), preferred: None },
];
static SINK_OFFERS: [OfferDesc; 3] = [
    OfferDesc { family: "audio/raw", fields: &RAW_FIELDS },
    OfferDesc { family: "ac3", fields: RAW_FIELDS.split_at(2).0 },
    OfferDesc::wildcard(),
];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "unitsink",
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

/// What a pad announced and the length and stamp of each unit behind it.
#[derive(Debug, Default)]
struct Got {
    family: String,
    rate_channels: (i64, i64),
    format: Option<AudioFormat>,
    layout: Option<u64>,
    units: Vec<(usize, i64)>,
}

struct UnitSink(Arc<Mutex<Got>>);

impl UnitSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for UnitSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut got = self.0.lock().unwrap();
        if let Some(fixed) = ctx.negotiated_sink(Self::IN) {
            got.family = ctx.family_name(fixed.family).unwrap_or_default().to_owned();
            let int = |name: &str| match ctx.field_id(name).and_then(|f| fixed.get(f)) {
                Some(Value::Int(n)) => n,
                _ => 0,
            };
            got.rate_channels = (int("rate"), int("channels"));
            got.format = negotiated_audio_format(ctx, Self::IN);
            got.layout = negotiated_layout(ctx, Self::IN);
        }
        while let Some(buf) = inputs.pop() {
            let pts = buf.pts.nanos().map_or(-1, |n| n as i64);
            got.units.push((buf.memory.data().len(), pts));
        }
        Ok(Flow::Ok)
    }
}

/// The file's audio stream and what its pad put out.
fn demux(path: &PathBuf) -> (Stream, Got) {
    let file = std::fs::read(path).expect("the fixture");
    let (header, _) = riff::probe_header(&file).expect("probe");
    let audio = header.streams.iter().find(|s| s.kind == StreamKind::Audio).expect("audio");
    let mut p = pf_reactor::pipeline();
    p.set_pool(1 << 20, 32);
    let src = p.add(FileSrc::new(path));
    let demux = p.add(AviDemux::new(file.clone()));
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");
    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 2, "video and audio");
    let mut gots = Vec::new();
    for ap in &added {
        let got = Arc::new(Mutex::new(Got::default()));
        let sink = p.add(UnitSink(Arc::clone(&got)));
        p.link(ap.element.src(ap.pad), sink.sink(UnitSink::IN)).expect("demux ! sink");
        gots.push(got);
    }
    p.run().expect("run");
    let video = gots[0].lock().unwrap();
    assert_eq!(video.family, "bytes", "MJPEG has no family here");
    assert_eq!(video.units.len(), 10, "ten pictures");
    let got = std::mem::take(&mut *gots[audio.index].lock().unwrap());
    (audio.clone(), got)
}

/// The audio chunks of the file as the walker finds them.
fn chunk_lens(path: &PathBuf, stream: usize) -> Vec<usize> {
    let file = std::fs::read(path).expect("the fixture");
    let movi = riff::probe_header(&file).unwrap().1.unwrap();
    let mut w = MoviWalker::new();
    w.push(&file[movi.movi_data_start as usize..]);
    let mut lens = Vec::new();
    while let Some(c) = w.next_chunk() {
        if c.stream_index == stream {
            lens.push(c.data.len());
        }
    }
    lens
}

/// One second of PCM in whole frames, each unit a chunk, stamped by the frames before it.
fn assert_pcm(path: &PathBuf, a: &Stream, got: &Got, fmt: AudioFormat) {
    assert_eq!(got.family, "audio/raw");
    assert_eq!(got.format, Some(fmt));
    let stride = fmt.frame_stride();
    let lens: Vec<usize> = got.units.iter().map(|u| u.0).collect();
    assert_eq!(lens, chunk_lens(path, a.index), "one unit a chunk, no byte lost");
    assert!(lens.iter().all(|n| n % stride == 0), "whole frames of {stride} bytes, {lens:?}");
    assert_eq!(lens.iter().sum::<usize>(), fmt.sample_rate as usize * stride, "one second");
    let mut frames = 0u64;
    for &(len, pts) in &got.units {
        let want = (frames * 1_000_000_000 / u64::from(fmt.sample_rate)) as i64;
        assert!((pts - want).abs() < 1_000_000, "{pts} for {frames} frames");
        frames += (len / stride) as u64;
    }
}

#[test]
fn an_extensible_s24_five_one_stream_is_raw_s24_with_its_speakers() {
    let Some(path) = made("s24x6", 48_000, &["-c:a", "pcm_s24le", "-ac", "6"]) else { return };
    let (a, got) = demux(&path);
    let ext = a.extensible().expect("ffmpeg writes WAVE_FORMAT_EXTENSIBLE past 16 bits");
    assert_eq!((ext.tag, ext.valid_bits, ext.channel_mask), (Some(1), 24, 0x3F));
    assert_eq!((a.format_tag, a.bits_per_sample, a.valid_bits()), (1, 24, 24));
    assert_eq!(pf_avi::family_for(&a), "audio/raw");
    assert_pcm(&path, &a, &got, AudioFormat::new(48_000, 6, SampleFormat::S24));
    assert_eq!(got.layout, Some(0x3F), "FL FR FC LFE BL BR");
}

#[test]
fn an_extensible_float_stereo_stream_is_raw_f32() {
    // past 48 kHz ffmpeg writes a stereo float stream as WAVE_FORMAT_EXTENSIBLE
    let Some(path) = made("f32x2", 96_000, &["-c:a", "pcm_f32le", "-ac", "2"]) else { return };
    let (a, got) = demux(&path);
    let ext = a.extensible().expect("WAVE_FORMAT_EXTENSIBLE");
    assert_eq!((ext.tag, ext.channel_mask), (Some(3), 0x3));
    assert_eq!((a.format_tag, a.bits_per_sample), (3, 32));
    assert_pcm(&path, &a, &got, AudioFormat::new(96_000, 2, SampleFormat::F32));
    assert_eq!(got.layout, Some(0x3));
}

#[test]
fn plain_s16_pcm_is_raw_s16_without_speakers() {
    let Some(path) = made("s16x2", 48_000, &["-c:a", "pcm_s16le", "-ac", "2"]) else { return };
    let (a, got) = demux(&path);
    assert_eq!((a.extensible(), a.format_tag, a.valid_bits()), (None, 1, 16));
    assert_pcm(&path, &a, &got, AudioFormat::new(48_000, 2, SampleFormat::S16));
    assert_eq!(got.layout, None, "WAVEFORMATEX states no speakers");
}

#[test]
fn an_extensible_ac3_stream_is_ac3() {
    let Some(path) = made("ac3x6", 48_000, &["-c:a", "ac3", "-ac", "6"]) else { return };
    let (a, got) = demux(&path);
    assert_eq!(a.extensible().and_then(|e| e.tag), Some(0x2000), "WAVE_FORMAT_DOLBY_AC3");
    assert_eq!(a.format_tag, 0x2000);
    assert_eq!((got.family.as_str(), got.rate_channels), ("ac3", (48_000, 6)));
    assert_eq!((got.format, got.layout), (None, None), "the frames state the speakers");
    let lens: Vec<usize> = got.units.iter().map(|u| u.0).collect();
    assert_eq!(lens, chunk_lens(&path, a.index));
    assert!(lens.len() >= 31 && lens.iter().all(|&n| n == 1792), "448 kbit/s frames, {lens:?}");
}
