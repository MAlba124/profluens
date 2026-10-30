// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libflac::{FlacDec, FlacDecoder, FlacEncoder, SampleFormat, StreamDecoder};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{FixedFormat, OfferDesc, Value};
use profluens_core::id::SinkPad;
use profluens_elements::io::FileSrc;

fn encode_s16(samples: &[i16], channels: u32, rate: u32) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    let (mut enc, mut header) = FlacEncoder::new(rate, channels, SampleFormat::S16).expect("enc");
    let mut frames = Vec::new();
    enc.encode_interleaved(&pcm, &mut frames).expect("encode");
    let body = enc.finish(&mut frames);
    header[pf_libflac::streaminfo_offset()..pf_libflac::streaminfo_offset() + body.len()]
        .copy_from_slice(&body);
    let mut stream = header;
    stream.extend_from_slice(&frames);
    stream
}

fn signal(frames: usize, channels: u32) -> Vec<i16> {
    let mut v = Vec::with_capacity(frames * channels as usize);
    for i in 0..frames {
        for c in 0..channels {
            let phase = 2.0 * std::f64::consts::PI * (3.0 + c as f64) * i as f64 / 200.0;
            v.push((9000.0 * phase.sin()) as i16);
        }
    }
    v
}

fn decode_chunked(stream: &[u8], chunk: usize) -> Vec<i64> {
    let mut sd = StreamDecoder::new();
    let mut got: Vec<i64> = Vec::new();
    let mut frame = Vec::new();
    for part in stream.chunks(chunk) {
        sd.push(part);
        while sd.pull_into(&mut frame).expect("incremental decode").is_some() {
            got.extend(frame.iter().map(|&s| i64::from(s)));
        }
    }
    got
}

#[test]
fn stream_decoder_is_incremental_and_lossless() {
    for &(frames, ch) in &[(9_001usize, 2u32), (5_000, 1), (1, 2)] {
        let samples = signal(frames, ch);
        let expected: Vec<i64> = samples.iter().map(|&s| s as i64).collect();
        let stream = encode_s16(&samples, ch, 44100);

        let reference = FlacDecoder::decode(&stream).expect("decode").samples;
        assert_eq!(reference, expected, "whole-buffer decode lossless (frames={frames} ch={ch})");

        for &chunk in &[13usize, 137, 4096] {
            assert_eq!(
                decode_chunked(&stream, chunk),
                expected,
                "incremental (chunk={chunk}) lossless (frames={frames} ch={ch})"
            );
        }
    }

    // One byte per push, on a small stream since re-attempting a frame per byte is quadratic.
    let samples = signal(600, 2);
    let expected: Vec<i64> = samples.iter().map(|&s| s as i64).collect();
    let stream = encode_s16(&samples, 2, 44100);
    assert_eq!(decode_chunked(&stream, 1), expected, "byte-at-a-time incremental lossless");
}

static PROBE_OFFERS: [OfferDesc; 1] = [OfferDesc::any("audio/raw")];
static PROBE_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &PROBE_OFFERS }];
static PROBE_DESC: ElementDesc = ElementDesc {
    name: "formatprobe",
    sinks: &PROBE_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active, // its own group, so the FormatChange crosses the ring
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct FormatProbe {
    seen: Arc<Mutex<Option<FixedFormat>>>,
}

impl FormatProbe {
    const IN: SinkPad = SinkPad(0);
}

impl Element for FormatProbe {
    fn desc(&self) -> &'static ElementDesc {
        &PROBE_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {} // consume the decoded PCM
        Ok(Flow::Ok)
    }
    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            *self.seen.lock().unwrap() = Some(f.clone());
        }
        Ok(())
    }
}

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("pf_libflacdec_{}_{}.flac", tag, std::process::id()));
    p
}

#[test]
fn flacdec_announces_runtime_audio_format() {
    let samples = signal(10_000, 2);
    let stream = encode_s16(&samples, 2, 44100);
    let path = temp_path("probe");
    std::fs::write(&path, &stream).unwrap();

    let seen = Arc::new(Mutex::new(None));
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&path));
    let dec = p.add(FlacDec::new());
    let probe = p.add(FormatProbe { seen: Arc::clone(&seen) });
    p.link(src.src(FileSrc::OUT), dec.sink(FlacDec::IN)).expect("filesrc->flacdec");
    p.link(dec.src(FlacDec::OUT), probe.sink(FormatProbe::IN)).expect("flacdec->probe");
    p.run().expect("run");

    let f = seen.lock().unwrap().clone().expect("probe received a FormatChange");
    assert_eq!(p.family_name(f.family), Some("audio/raw"), "announced family");
    let rate = p.field_id("rate").expect("rate interned");
    let channels = p.field_id("channels").expect("channels interned");
    let sample = p.field_id("sample").expect("sample interned");
    let s16 = p.value_id("s16").expect("s16 interned");
    assert_eq!(f.get(rate), Some(Value::Int(44100)), "runtime sample rate");
    assert_eq!(f.get(channels), Some(Value::Int(2)), "runtime channel count");
    assert_eq!(f.get(sample), Some(Value::Id(s16)), "runtime sample format");

    let _ = std::fs::remove_file(&path);
}
