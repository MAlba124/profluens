//! Hardware-encode a test pattern into an MKV file for external validation with ffprobe and mpv.
//! Usage: hw_encode_mkv <h264|h265|vp8> [out.mkv] [frames]

// An example app, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use pf_mkv::MkvMuxN;
use profluens_core::time::Rational;
use profluens_elements::io::FileSink;
use profluens_video::{PixelFormat, VideoFormat, VideoTestSrc};

fn main() {
    let mut args = std::env::args().skip(1);
    let codec = args.next().unwrap_or_else(|| "h264".to_string());
    let out = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/pf_hw_{codec}.mkv")));
    let frames: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);

    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("no VA-API device available");
        std::process::exit(1);
    };
    let family = match codec.as_str() {
        "h264" => "h264/annexb",
        "h265" => "h265/annexb",
        "vp8" => "vp8",
        other => {
            eprintln!("unknown codec {other:?} (want h264|h265|vp8)");
            std::process::exit(2);
        }
    };
    if !caps.supports_encode(family) {
        eprintln!("device does not advertise {codec} encode");
        std::process::exit(1);
    }

    // 360 is not MB/CTB aligned, so the crop must round-trip through the container.
    let fmt = VideoFormat::new(640, 360, PixelFormat::I420, Rational { num: 30, den: 1 });

    let mut p = pf_reactor::pipeline();
    p.set_pool(1024 * 1024, 16);
    let src = p.add(VideoTestSrc::new(fmt, 7, frames));
    // The mkv writer holds a whole GOP until the next keyframe, so the encoder pool must be deeper.
    use pf_vaapi::{VaapiH264Enc, VaapiH265Enc, VaapiVp8Enc};
    let (enc, enc_in, enc_out) = match codec.as_str() {
        "h264" => (p.add(VaapiH264Enc::new().with_gop(30)), VaapiH264Enc::IN, VaapiH264Enc::OUT),
        "h265" => (p.add(VaapiH265Enc::new().with_gop(30)), VaapiH265Enc::IN, VaapiH265Enc::OUT),
        _ => (p.add(VaapiVp8Enc::new().with_gop(30)), VaapiVp8Enc::IN, VaapiVp8Enc::OUT),
    };
    p.set_element_pool(enc, 512 * 1024, 48);
    let mux = p.add(MkvMuxN::new());
    p.set_element_pool(mux, 512 * 1024, 8);
    let sink = p.add(FileSink::new(&out));
    p.link(src.src(VideoTestSrc::OUT), enc.sink(enc_in)).expect("link src->enc");
    p.link(enc.src(enc_out), mux.sink(MkvMuxN::INS[0])).expect("link enc->mux");
    p.link(mux.src(MkvMuxN::OUT), sink.sink(FileSink::IN)).expect("link mux->sink");

    match p.run() {
        Ok(()) => {
            let n = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!("OK: {codec} -> {} ({n} bytes, {frames} frames)", out.display());
        }
        Err(e) => {
            eprintln!("FAILED: {e:?}");
            std::process::exit(1);
        }
    }
}
