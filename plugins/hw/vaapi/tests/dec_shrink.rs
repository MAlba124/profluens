//! A zero-copy decoder that scales in the decode call against the decoder plus vaapipostproc,
//! the same pictures within the scalers' difference, bars black. Skips where the driver does
//! no decode processing.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::element::Element;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::vpp::{Target, VaapiPostProc};
use pf_vaapi::{VaapiH264Dec, VaapiH265Dec, VaapiVp9Dec};

use chain::{Codec, Collect, UnitSrc};

/// What a test decodes, Annex B or VP9 in IVF.
#[derive(Clone, Copy)]
enum Dec {
    Nal(Codec),
    Vp9,
}

impl Dec {
    fn family(self) -> &'static str {
        match self {
            Dec::Nal(c) => c.family(),
            Dec::Vp9 => "vp9",
        }
    }

    fn units(self, stream: &[u8]) -> Vec<Vec<u8>> {
        match self {
            Dec::Nal(c) => c.units(stream),
            Dec::Vp9 => {
                // the file header's length at 6, each frame a 12 byte header with its size first
                let mut at = usize::from(u16::from_le_bytes([stream[6], stream[7]]));
                let mut out = Vec::new();
                while at + 12 <= stream.len() {
                    let n = u32::from_le_bytes([stream[at], stream[at + 1], stream[at + 2], stream[at + 3]]) as usize;
                    out.push(stream[at + 12..at + 12 + n].to_vec());
                    at += 12 + n;
                }
                out
            }
        }
    }

    fn make(self, ch: &Arc<GpuFrameChannel>, shrink: Option<Target>) -> Box<dyn Element> {
        let ch = Arc::clone(ch);
        match (self, shrink) {
            (Dec::Nal(Codec::H264), Some(t)) => Box::new(VaapiH264Dec::new_zerocopy(ch).with_shrink(t, true)),
            (Dec::Nal(Codec::H265), Some(t)) => Box::new(VaapiH265Dec::new_zerocopy(ch).with_shrink(t, true)),
            (Dec::Vp9, Some(t)) => Box::new(VaapiVp9Dec::new_zerocopy(ch).with_shrink(t, true)),
            (Dec::Nal(Codec::H264), None) => Box::new(VaapiH264Dec::new_zerocopy(ch)),
            (Dec::Nal(Codec::H265), None) => Box::new(VaapiH265Dec::new_zerocopy(ch)),
            (Dec::Vp9, None) => Box::new(VaapiVp9Dec::new_zerocopy(ch)),
        }
    }
}

/// The pictures of `units` decoded, scaled into `shrink` by the decoder when it is set, then into
/// `target`'s box and read back by vaapipostproc.
fn run(codec: Dec, units: &[Vec<u8>], shrink: Option<Target>, target: Target) -> (Vec<(Timestamp, Vec<u8>)>, Vec<String>) {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units.to_vec()));
    let ch = GpuFrameChannel::new();
    let dec = p.add_boxed(codec.make(&ch, shrink));
    let vpp = p.add(VaapiPostProc::new_zerocopy(target, ch).with_fixed_size());
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
    p.link(vpp.src(VaapiPostProc::OUT), sink.sink(SinkPad(0))).expect("vpp to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(vpp, 256 * 1024, 16);
    p.run().expect("the run ends");
    let warnings = chain::warnings(&p);
    drop(p);
    let mut frames: Vec<_> = rx.try_iter().collect();
    frames.sort_by_key(|f| f.0);
    (frames, warnings)
}

/// Luma PSNR, dB, of two pictures of `n` luma samples, 8 bit or P010.
fn psnr(a: &[u8], b: &[u8], n: usize, wide: bool) -> f64 {
    let s = |x: &[u8], i: usize| match wide {
        true => f64::from(u16::from_le_bytes([x[2 * i], x[2 * i + 1]]) >> 6),
        false => f64::from(x[i]),
    };
    let peak = if wide { 1023.0f64 } else { 255.0 };
    let mse = (0..n).map(|i| (s(a, i) - s(b, i)).powi(2)).sum::<f64>() / n as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (peak * peak / mse).log10() }
}

fn shrinks(codec: Dec, profile: &str, test: &str) -> bool {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}: no VA-API device");
        return false;
    };
    if !caps.shrinks(codec.family()) || !caps.decodes_profile(profile) || !caps.video_proc {
        eprintln!("skip {test}: no decode processing of {profile}");
        return false;
    }
    true
}

/// Rows of luma, `rows` of `width` samples, that are black.
fn black_rows(pic: &[u8], width: usize, rows: std::ops::Range<usize>, wide: bool) -> bool {
    let bps = if wide { 2 } else { 1 };
    rows.flat_map(|r| (0..width).map(move |x| (r, x))).all(|(r, x)| {
        let i = (r * width + x) * bps;
        let v = if wide { u16::from_le_bytes([pic[i], pic[i + 1]]) >> 8 } else { u16::from(pic[i]) };
        v.abs_diff(16) <= 1
    })
}

/// The decoder shrinking into `shrink` then vaapipostproc into `target`, against vaapipostproc
/// alone, `bar` rows black above and below in vaapipostproc's picture, `2 * bar` below in the
/// decoder's, which places at the top left.
fn check(test: &str, codec: Dec, profile: &str, stream: &[u8], shrink: Target, target: Target, bar: usize) {
    if !shrinks(codec, profile, test) {
        return;
    }
    let units = codec.units(stream);
    let (vpp, vw) = run(codec, &units, None, target);
    let (dec, dw) = run(codec, &units, Some(shrink), target);
    assert!(vw.is_empty() && dw.is_empty(), "{test} warned {vw:?} {dw:?}");
    assert_eq!((vpp.len(), dec.len()), (units.len(), units.len()), "{test} every picture both ways");
    let (w, h) = (target.max_width as usize, target.max_height as usize);
    let wide = target.depth > 8;
    let mut worst = f64::INFINITY;
    // DUMP=DIR writes the first pair's luma as PGM
    if let (Some(dir), Some((_, a)), Some((_, b))) = (std::env::var_os("DUMP"), vpp.first(), dec.first()) {
        for (name, pic) in [("vpp", a), ("dec", b)] {
            let mut out = format!("P5 {w} {h} 255\n").into_bytes();
            out.extend((0..w * h).map(|i| if wide { pic[2 * i + 1] } else { pic[i] }));
            std::fs::write(std::path::Path::new(&dir).join(format!("{test}_{name}.pgm")), out).expect("dump");
        }
    }
    for ((pa, a), (pb, b)) in vpp.iter().zip(&dec) {
        assert_eq!((pa, a.len()), (pb, b.len()), "{test} same stamp and size");
        // the picture rows of each, the decoder's from the top
        let bps = if wide { 2 } else { 1 };
        let (ra, rb) = (&a[bar * w * bps..(h - bar) * w * bps], &b[..(h - 2 * bar) * w * bps]);
        worst = worst.min(psnr(ra, rb, w * (h - 2 * bar), wide));
        let luma: Vec<u16> = (0..w * h).map(|i| if wide { u16::from_le_bytes([b[2 * i], b[2 * i + 1]]) >> 8 } else { u16::from(b[i]) }).collect();
        let (lo, hi) = (luma.iter().copied().min().unwrap_or(0), luma.iter().copied().max().unwrap_or(0));
        assert!(hi - lo > 40, "{test} a picture, not a flat field, {lo}..{hi}");
        assert!(black_rows(a, w, 0..bar, wide) && black_rows(b, w, h - 2 * bar..h, wide), "{test} bars black at {pb:?}");
    }
    eprintln!("{test}: {} pictures, worst luma psnr {worst:.1} dB", dec.len());
    // the same scaler on Gen9, about 33 dB apart on Gen12, a misplaced picture is near 15
    assert!(worst > 28.0, "{test} the decoder's scale is the VPP's within the scalers' difference, {worst:.1} dB");
}

const IPB: &[u8] = include_bytes!("fixtures/ipb_320x240.hevc");
const PQ: &[u8] = include_bytes!("fixtures/hdr10_128x128.hevc");
const SDR10: &[u8] = include_bytes!("fixtures/sdr10_128x128.hevc");

fn t(max_width: u32, max_height: u32, depth: u8) -> Target {
    Target { max_width, max_height, depth }
}

#[test]
fn h265_shrunk_in_the_decoder_is_the_vpps_picture() {
    check("h265_shrunk", Dec::Nal(Codec::H265), "HEVCMain", IPB, t(160, 120, 8), t(160, 120, 8), 0);
}

#[test]
fn h265_shrunk_into_a_taller_box_sits_in_black_bars() {
    check("h265_bars", Dec::Nal(Codec::H265), "HEVCMain", IPB, t(160, 160, 8), t(160, 160, 8), 20);
}

#[test]
fn main10_shrunk_in_the_decoder_stays_ten_bit() {
    check("main10_shrunk", Dec::Nal(Codec::H265), "HEVCMain10", PQ, t(64, 64, 10), t(64, 64, 10), 0);
}

/// A 10 bit SDR source to 8 bits, the decoder's scaler converts.
#[test]
fn sdr_main10_shrunk_to_eight_bits_in_the_decoder() {
    check("sdr10_to_8", Dec::Nal(Codec::H265), "HEVCMain10", SDR10, t(64, 64, 8), t(64, 64, 8), 0);
}

#[test]
fn h264_shrunk_in_the_decoder_is_the_vpps_picture() {
    let ipb = include_bytes!("fixtures/ipb_320x240.h264");
    check("h264_shrunk", Dec::Nal(Codec::H264), "H264High", ipb, t(160, 120, 8), t(160, 120, 8), 0);
    check("h264_bars", Dec::Nal(Codec::H264), "H264High", ipb, t(160, 160, 8), t(160, 160, 8), 20);
}

/// PQ to SDR as a transcode runs it, shrunk at 10 bits by the decoder then tone mapped, is the
/// VPP scaling at 10 bits and tone mapping after. One VPP pass that does both differs from
/// either by about 30 dB on this fixture.
#[test]
fn pq_shrunk_then_tone_mapped_is_the_vpp_scaling_then_tone_mapping() {
    let test = "pq_shrunk_then_tone_mapped";
    if !shrinks(Dec::Nal(Codec::H265), "HEVCMain10", test) {
        return;
    }
    let units = Codec::H265.units(PQ);
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units.clone()));
    let (a, b) = (GpuFrameChannel::new(), GpuFrameChannel::new());
    let dec = p.add(VaapiH265Dec::new_zerocopy(Arc::clone(&a)));
    let v1 = p.add(VaapiPostProc::new_zerocopy(t(64, 64, 10), a).with_fixed_size().with_gpu_output(Arc::clone(&b)));
    let v2 = p.add(VaapiPostProc::new_zerocopy(t(64, 64, 8), b).with_fixed_size());
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("l");
    p.link(dec.src(SrcPad(0)), v1.sink(VaapiPostProc::IN)).expect("l");
    p.link(v1.src(VaapiPostProc::OUT), v2.sink(VaapiPostProc::IN)).expect("l");
    p.link(v2.src(VaapiPostProc::OUT), sink.sink(SinkPad(0))).expect("l");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(v1, 256 * 1024, 16);
    p.set_element_pool(v2, 256 * 1024, 16);
    p.run().expect("run");
    assert!(chain::warnings(&p).is_empty(), "{test} two passes warned");
    drop(p);
    let mut two: Vec<_> = rx.try_iter().collect();
    two.sort_by_key(|f| f.0);
    let (one, _) = run(Dec::Nal(Codec::H265), &units, None, t(64, 64, 8));
    let (dec, w) = run(Dec::Nal(Codec::H265), &units, Some(t(64, 64, 10)), t(64, 64, 8));
    assert!(w.is_empty(), "{test} warned {w:?}");
    assert_eq!((two.len(), dec.len()), (units.len(), units.len()), "{test} every picture");
    for ((pt, a), (pd, b)) in two.iter().zip(&dec) {
        assert_eq!((pt, a.len()), (pd, b.len()));
        let db = psnr(a, b, 64 * 64, false);
        assert!(db > 50.0, "{test} at {pd:?} {db:.1} dB");
    }
    let one_db = one.iter().zip(&dec).map(|(a, b)| psnr(&a.1, &b.1, 64 * 64, false)).fold(f64::INFINITY, f64::min);
    eprintln!("{test}: one VPP pass against the decoder's shrink, worst {one_db:.1} dB");
}

/// A picture cropped at the origin, 236 rows shown of 240 decoded. iHD on Gen9 reads all 240
/// whatever the region says, the decoder reads them whole and stretches the destination.
#[test]
fn a_cropped_picture_is_the_vpps_picture() {
    let hevc = include_bytes!("fixtures/crop_320x236.hevc");
    let avc = include_bytes!("fixtures/crop_320x236.h264");
    check("hevc_crop_1to1", Dec::Nal(Codec::H265), "HEVCMain", hevc, t(320, 236, 8), t(320, 236, 8), 0);
    check("hevc_crop_half", Dec::Nal(Codec::H265), "HEVCMain", hevc, t(160, 118, 8), t(160, 118, 8), 0);
    check("h264_crop_1to1", Dec::Nal(Codec::H264), "H264High", avc, t(320, 236, 8), t(320, 236, 8), 0);
    check("h264_crop_half", Dec::Nal(Codec::H264), "H264High", avc, t(160, 118, 8), t(160, 118, 8), 0);
}

/// VP9 profile 0 and the 10 bit profile 2, shrunk and in bars, hidden frames and superframes in
/// the 352x288 stream.
#[test]
fn vp9_shrunk_in_the_decoder_is_the_vpps_picture() {
    let features = include_bytes!("fixtures/features_512x288.ivf");
    let hidden = include_bytes!("fixtures/alloc_352x288.ivf");
    let p2 = include_bytes!("fixtures/p2_160x96.ivf");
    check("vp9_shrunk", Dec::Vp9, "VP9Profile0", features, t(256, 144, 8), t(256, 144, 8), 0);
    check("vp9_bars", Dec::Vp9, "VP9Profile0", features, t(256, 176, 8), t(256, 176, 8), 16);
    check("vp9_hidden", Dec::Vp9, "VP9Profile0", hidden, t(176, 144, 8), t(176, 144, 8), 0);
    check("vp9_p2_shrunk", Dec::Vp9, "VP9Profile2", p2, t(80, 48, 10), t(80, 48, 10), 0);
}
