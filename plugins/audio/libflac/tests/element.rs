// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use pf_libflac::{FlacDecoder, FlacEnc, SampleFormat};
use profluens_elements::io::{FileSink, FileSrc};

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("pf_libflac_elem_{}_{}.bin", tag, std::process::id()));
    p
}

fn gen_s16(n: usize, channels: u32) -> (Vec<u8>, Vec<i64>) {
    let mut bytes = Vec::with_capacity(n * channels as usize * 2);
    let mut expected = Vec::with_capacity(n * channels as usize);
    for i in 0..n {
        for c in 0..channels {
            let phase = 2.0 * std::f64::consts::PI * (3.0 + c as f64) * (i as f64) / 512.0;
            let s = (12000.0 * phase.sin()).round() as i64;
            let s = s.clamp(-32768, 32767);
            expected.push(s);
            bytes.extend_from_slice(&(s as i16).to_le_bytes());
        }
    }
    (bytes, expected)
}

fn run_case(n: usize, channels: u32, rate: u32) {
    let inp = temp_path(&format!("in_{n}_{channels}"));
    let outp = temp_path(&format!("out_{n}_{channels}"));
    let (pcm, expected) = gen_s16(n, channels);
    std::fs::write(&inp, &pcm).expect("write pcm");

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&inp));
    let enc = p.add(FlacEnc::new(rate, channels, SampleFormat::S16));
    let sink = p.add(FileSink::new(&outp));
    p.link(src.src(FileSrc::OUT), enc.sink(FlacEnc::IN)).expect("link src->enc");
    p.link(enc.src(FlacEnc::OUT), sink.sink(FileSink::IN)).expect("link enc->sink");
    p.run().expect("run pipeline");

    let flac = std::fs::read(&outp).expect("read flac");
    assert!(flac.starts_with(b"fLaC"), "output is a FLAC stream");
    let dec = FlacDecoder::decode(&flac).expect("decode produced flac");
    assert_eq!(dec.info.channels, channels);
    assert_eq!(dec.info.sample_rate, rate);
    assert_eq!(dec.info.bits_per_sample, 16);
    assert_eq!(dec.samples, expected, "element round trip must be lossless (n={n} ch={channels})");

    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
}

#[test]
fn flacenc_pipeline_mono_lossless() {
    run_case(20_000, 1, 44100);
}

#[test]
fn flacenc_pipeline_stereo_lossless() {
    run_case(13_337, 2, 48000);
}

#[test]
fn flacenc_pipeline_small_lossless() {
    run_case(100, 2, 44100);
}
