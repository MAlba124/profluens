//! vaapih264enc and vaapih265enc with several references a P picture. Each P slice states the
//! pictures since its IDR up to what the driver takes, ffmpeg's software decoder and vaapi's
//! own decode the stream to the same pictures without an error, and those are close to the
//! encoder's input. A slice header that disagrees with what the hardware codes (the ref_idx
//! syntax is there only past one reference) decodes to garbage. Skips without a device.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use pf_harness::Harness;
use profluens_core::element::Element;
use profluens_core::format::ValueDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::vpp::{Target, VaapiPostProc};
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc, VaapiH265Enc, ffi, h264parse, h265parse, va};

use chain::{Codec, Collect, UnitSrc};

/// 80 pictures of a moving test pattern, then bars.
const MOVING: &[u8] = include_bytes!("fixtures/scenecut_320x240.h264");
const W: usize = 320;
const H: usize = 240;
const GOP: u32 = 32;

/// The fixture's pictures as NV12, in order.
fn input() -> Vec<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(1 << 20, 16);
    let ch = GpuFrameChannel::new();
    let src = p.add(UnitSrc::new(Codec::H264.units(MOVING)));
    let dec = p.add(VaapiH264Dec::new_zerocopy(Arc::clone(&ch)));
    let vpp = p.add(VaapiPostProc::new_zerocopy(
        Target { max_width: W as u32, max_height: H as u32, depth: 8 },
        ch,
    ));
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(SinkPad(0))).expect("decoder to vpp");
    p.link(vpp.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("vpp to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.run().expect("the run ends");
    let w = chain::warnings(&p);
    assert!(w.is_empty(), "warned {w:?}");
    drop(p);
    let mut out: Vec<_> = rx.try_iter().collect();
    out.sort_by_key(|u| u.0);
    out.into_iter().map(|u| u.1).collect()
}

/// `frames` through the raw encoder `enc`, one access unit each.
fn encode<E: Element + 'static>(enc: E, frames: &[Vec<u8>]) -> Vec<(Timestamp, Vec<u8>)> {
    let mut h = Harness::with_pool(enc, 256 * 1024, 16);
    h.fix_sink_format(
        SinkPad(0),
        "video/raw",
        &[
            ("width", ValueDesc::Int(W as i64)),
            ("height", ValueDesc::Int(H as i64)),
            ("pixfmt", ValueDesc::Id("nv12")),
            ("fps", ValueDesc::Rat(25, 1)),
        ],
    );
    let mut out = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let mut b = h.alloc(f);
        b.pts = Timestamp::from_nanos(i as u64 * chain::FRAME_NS);
        h.push(b).expect("push");
        out.extend(h.drain_outputs().iter().map(|b| (b.pts, b.memory.data().to_vec())));
    }
    out.extend(h.eos().expect("eos").iter().map(|b| (b.pts, b.memory.data().to_vec())));
    out
}

/// References a P picture takes on the entrypoint the encoder opens, as it picks them.
fn driver_refs(profile: ffi::VAProfile, lp: bool, gpb_capable: bool) -> u32 {
    let caps = pf_vaapi::probe().expect("a device");
    let d = va::Display::open(&caps.device).expect("display");
    let ep = if lp { ffi::VAEntrypointEncSliceLP } else { ffi::VAEntrypointEncSlice };
    let attr = |a| va::config_attribute(&d, profile, ep, a).ok().flatten();
    let v = attr(ffi::VAConfigAttribEncMaxRefFrames).unwrap_or(1 | 1 << 16);
    let gpb = gpb_capable
        && attr(ffi::VAConfigAttribPredictionDirection)
            .is_some_and(|p| p & ffi::VA_PREDICTION_DIRECTION_BI_NOT_EMPTY != 0);
    let (l0, l1) = (v & 0xFFFF, v >> 16);
    3.min(l0).min(if gpb { l1 } else { l0 }).max(1)
}

/// ffmpeg's software decode of `stream`, the luma planes, and what it said on stderr.
fn ffmpeg(stream: &[(Timestamp, Vec<u8>)], ext: &str) -> Option<(Vec<Vec<u8>>, String)> {
    let path = std::env::temp_dir().join(format!("pf_enc_refs_{}.{ext}", std::process::id()));
    std::fs::write(&path, stream.iter().flat_map(|u| u.1.iter().copied()).collect::<Vec<u8>>())
        .expect("write");
    // The devshell LD_LIBRARY_PATH breaks a system ffmpeg.
    let out = std::process::Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
        .output();
    let _ = std::fs::remove_file(&path);
    let out = out.ok()?;
    if !out.status.success() {
        eprintln!("skip the ffmpeg comparison, ffmpeg failed");
        return None;
    }
    let frames = out.stdout.chunks(W * H * 3 / 2).map(|f| f[..W * H].to_vec()).collect();
    Some((frames, String::from_utf8_lossy(&out.stderr).into_owned()))
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

/// The references each coded picture's slice states, 0 for an IDR.
fn stated_refs(codec: Codec, stream: &[(Timestamp, Vec<u8>)]) -> Vec<u32> {
    let mut out = Vec::new();
    match codec {
        Codec::H264 => {
            let (mut sps, mut pps) = (None, None);
            for (_, au) in stream {
                for nal in h264parse::split_nals(au) {
                    let raw = nal.raw(au);
                    match nal.unit_type {
                        h264parse::NAL_SPS => sps = h264parse::parse_sps(raw),
                        h264parse::NAL_PPS => pps = h264parse::parse_pps(raw),
                        1 | 5 => {
                            let (Some(s), Some(p)) = (&sps, &pps) else {
                                panic!("a slice before its sets")
                            };
                            let sh = h264parse::parse_slice_header(raw, nal.unit_type, 1, s, p)
                                .expect("slice");
                            out.push(if sh.is_idr {
                                0
                            } else {
                                sh.num_ref_idx_l0_active_minus1 + 1
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        Codec::H265 => {
            let (mut sps, mut pps) = (None, None);
            for (_, au) in stream {
                for nal in h265parse::split_nals(au) {
                    let raw = nal.raw(au);
                    let ty = (raw[0] >> 1) & 0x3F;
                    match ty {
                        33 => sps = h265parse::parse_sps(raw),
                        34 => pps = h265parse::parse_pps(raw),
                        0..=21 => {
                            let (Some(s), Some(p)) = (&sps, &pps) else {
                                panic!("a slice before its sets")
                            };
                            let sh = h265parse::parse_slice_header(raw, ty, s, p).expect("slice");
                            let n = if h265parse::is_idr(ty) {
                                0
                            } else {
                                sh.num_ref_idx_l0_active_minus1 + 1
                            };
                            if sh.slice_type == h265parse::SliceType::B {
                                assert_eq!(
                                    sh.num_ref_idx_l1_active_minus1 + 1,
                                    n,
                                    "low delay B, both lists alike"
                                );
                            }
                            if n > 0 {
                                assert_eq!(
                                    sh.st_rps.delta_poc_s0.len() as u32,
                                    n,
                                    "the set is the list"
                                );
                            }
                            out.push(n);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    out
}

fn check(codec: Codec, name: &str, refs: u32, input: &[Vec<u8>], stream: &[(Timestamp, Vec<u8>)]) {
    assert_eq!(stream.len(), input.len(), "{name}: every picture");
    let stated = stated_refs(codec, stream);
    let want: Vec<u32> = (0..input.len() as u32).map(|i| (i % GOP).min(refs)).collect();
    assert_eq!(stated, want, "{name}: the pictures since the IDR up to {refs}");
    let (back, warned) = chain::decode(codec, stream);
    assert!(warned.is_empty(), "{name}: vaapi decode warned {warned:?}");
    assert_eq!(back.len(), input.len(), "{name}: vaapi decodes every picture");
    let mut worst = f64::INFINITY;
    for (i, (a, b)) in input.iter().zip(&back).enumerate() {
        let db = psnr(&a[..W * H], &b[..W * H]);
        worst = worst.min(db);
        assert!(db > 36.0, "{name}: picture {i} decodes {db:.1} dB from the encoder's input");
    }
    let ext = if codec == Codec::H264 { "h264" } else { "hevc" };
    if let Some((soft, said)) = ffmpeg(stream, ext) {
        assert!(said.is_empty(), "{name}: ffmpeg said {said}");
        assert_eq!(soft.len(), input.len(), "{name}: ffmpeg decodes every picture");
        let off: Vec<usize> =
            (0..soft.len()).filter(|&i| soft[i][..] != back[i][..W * H]).collect();
        assert!(off.is_empty(), "{name}: pictures {off:?} differ between ffmpeg and vaapi");
    }
    let bytes: usize = stream.iter().map(|u| u.1.len()).sum();
    eprintln!("{name}: {refs} refs, {bytes} bytes, worst luma {worst:.1} dB");
}

#[test]
fn several_references_decode_alike_in_software_and_hardware() {
    let Some(caps) = pf_vaapi::probe() else {
        return eprintln!("skip: no VA-API device");
    };
    if !caps.supports("h264/annexb") {
        return eprintln!("skip: no H.264 decode for the input");
    }
    let frames = input();
    assert_eq!(frames.len(), 80, "the fixture's pictures");
    let row = |n: &str| caps.profiles.iter().find(|p| p.name == n).copied();
    if let Some(p) = row("H264High") {
        for lp in [false, true] {
            // the encoder falls back to the entrypoint there is
            let lp_open = (lp && p.enc_lp) || !p.enc;
            if lp != lp_open {
                continue;
            }
            let refs = driver_refs(ffi::VAProfileH264High, lp_open, false);
            let enc = VaapiH264Enc::new().with_qp(26).with_gop(GOP).with_low_power(lp);
            check(
                Codec::H264,
                &format!("h264 low_power {lp}"),
                refs,
                &frames,
                &encode(enc, &frames),
            );
        }
    }
    if caps.supports("h265/annexb")
        && let Some(p) = row("HEVCMain")
    {
        for lp in [false, true] {
            let lp_open = (lp && p.enc_lp) || !p.enc;
            if lp != lp_open {
                continue;
            }
            let refs = driver_refs(ffi::VAProfileHEVCMain, lp_open, true);
            let enc = VaapiH265Enc::new().with_qp(26).with_gop(GOP).with_low_power(lp);
            check(
                Codec::H265,
                &format!("h265 low_power {lp}"),
                refs,
                &frames,
                &encode(enc, &frames),
            );
        }
    }
}
