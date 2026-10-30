//! `vplenc` against `vaapih265enc` at the same bitrate, PSNR, SSIM and bytes, yuv kept for VMAF.
//! The clip is the fixture looped or `PF_VPL_CLIP`. Skips without both runtimes.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use std::path::Path;

use pf_harness::Harness;
use pf_vpl::{Settings, VplEnc};
use profluens_core::buffer::BufferFlags;
use profluens_core::format::ValueDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

use common::{BoxedElement, Codec, decode_sw, hw, mean_psnr, ssim_luma};

const BITRATE_KBPS: u32 = 1500;
const GOP: u32 = 30;

fn source() -> Option<(Vec<Vec<u8>>, u32, u32, &'static str)> {
    if let Ok(path) = std::env::var("PF_VPL_CLIP") {
        let data = std::fs::read(&path).ok()?;
        let (codec, units) = clip_units(&path, &data)?;
        let d = decode_sw(codec, &units);
        eprintln!(
            "clip {path}: {} units, {} pictures {}x{}",
            units.len(),
            d.frames.len(),
            d.width,
            d.height
        );
        return Some((d.frames, d.width, d.height, "clip"));
    }
    let once = Codec::H265.units(common::IPB_HEVC);
    let units: Vec<Vec<u8>> = (0..4).flat_map(|_| once.iter().cloned()).collect();
    let d = decode_sw(Codec::H265, &units);
    Some((d.frames, d.width, d.height, "ipb_320x240 x4"))
}

fn clip_units(path: &str, data: &[u8]) -> Option<(Codec, Vec<Vec<u8>>)> {
    if path.ends_with(".hevc") || path.ends_with(".h265") || path.ends_with(".265") {
        return Some((Codec::H265, Codec::H265.units(data)));
    }
    if path.ends_with(".h264") || path.ends_with(".264") {
        return Some((Codec::H264, Codec::H264.units(data)));
    }
    let max: usize =
        std::env::var("PF_VPL_CLIP_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let mut reader = pf_mkv::MatroskaReader::new();
    let (chunk, mut fed) = (1 << 20, 0usize);
    while !reader.tracks_ready() && fed < data.len() {
        let end = (fed + chunk).min(data.len());
        reader.push(&data[fed..end]).ok()?;
        fed = end;
    }
    let track = reader
        .tracks()
        .iter()
        .find(|t| t.codec_id == "V_MPEG4/ISO/AVC" || t.codec_id == "V_MPEGH/ISO/HEVC")?
        .clone();
    let hevc = track.codec_id == "V_MPEGH/ISO/HEVC";
    let (head, length_size) = pf_mkv::nal_head_from_config(&track.codec_private, hevc).ok()?;
    let reframer = pf_mkv::Reframer::Nal { length_size };
    let (mut units, mut first, mut scratch) = (Vec::new(), true, Vec::new());
    loop {
        while let Some(frame) = reader.next_frame() {
            if frame.track_number != track.track_number {
                continue;
            }
            let annexb = reframer.reframe_into(&frame.data, &mut scratch).ok()?;
            let mut au = Vec::new();
            if first {
                au.extend_from_slice(&head);
                first = false;
            }
            au.extend_from_slice(annexb);
            units.push(au);
            if units.len() >= max {
                return Some((if hevc { Codec::H265 } else { Codec::H264 }, units));
            }
        }
        if fed >= data.len() {
            break;
        }
        let end = (fed + chunk).min(data.len());
        reader.push(&data[fed..end]).ok()?;
        fed = end;
    }
    (!units.is_empty()).then_some((if hevc { Codec::H265 } else { Codec::H264 }, units))
}

/// Packets are copied off the pool so a bounded pool cycles over a long clip.
fn encode(
    enc: Box<dyn profluens_core::element::Element>,
    frames: &[Vec<u8>],
    w: u32,
    h: u32,
) -> (Vec<(BufferFlags, Vec<u8>)>, Vec<String>) {
    let mut e = Harness::with_pool(BoxedElement(enc), 4 * 1024 * 1024, 16).real_wakes();
    e.fix_sink_format(
        SinkPad(0),
        "video/raw",
        &[
            ("width", ValueDesc::Int(i64::from(w))),
            ("height", ValueDesc::Int(i64::from(h))),
            ("pixfmt", ValueDesc::Id("i420")),
            ("fps", ValueDesc::Rat(25, 1)),
        ],
    );
    e.fix_src_format(
        SrcPad(0),
        "h265/annexb",
        &[("width", ValueDesc::Int(i64::from(w))), ("height", ValueDesc::Int(i64::from(h)))],
    );
    e.start().expect("start");
    let mut out: Vec<(BufferFlags, Vec<u8>)> = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let mut b = e.alloc(f);
        b.pts = Timestamp::from_nanos(i as u64 * common::FRAME_NS);
        b.duration = Timestamp::from_nanos(common::FRAME_NS);
        e.push(b).expect("push");
        while let Some(p) = e.pull(SrcPad(0)) {
            out.push((p.flags, p.memory.data().to_vec()));
        }
    }
    out.extend(e.eos().expect("eos").into_iter().map(|p| (p.flags, p.memory.data().to_vec())));
    let mut warnings = Vec::new();
    for m in e.bus_messages() {
        if let profluens_core::bus::BusMessage::Warning { error, .. }
        | profluens_core::bus::BusMessage::Error { error, .. } = m
        {
            warnings.push(format!("{error}"));
        }
    }
    (out, warnings)
}

fn write_yuv(dir: &Path, name: &str, frames: &[Vec<u8>]) -> Option<std::path::PathBuf> {
    let path = dir.join(name);
    let all: Vec<u8> = frames.iter().flat_map(|f| f.iter().copied()).collect();
    std::fs::write(&path, all).ok().map(|()| path)
}

/// Through the `vmaf` tool on the path or named by `PF_VMAF`.
fn vmaf(reference: &Path, distorted: &Path, w: u32, h: u32) -> Result<String, String> {
    let tool = std::env::var("PF_VMAF").unwrap_or_else(|_| "vmaf".to_string());
    let out = std::process::Command::new(&tool)
        .args([
            "-r",
            &reference.display().to_string(),
            "-d",
            &distorted.display().to_string(),
            "-w",
            &w.to_string(),
            "-h",
            &h.to_string(),
            "-p",
            "420",
            "-b",
            "8",
            "-m",
            "version=vmaf_v0.6.1",
            "-q",
            "--json",
            "-o",
            "/dev/stdout",
        ])
        .output()
        .map_err(|e| {
            format!("{tool} did not run ({e}), set PF_VMAF to the libvmaf tool for VMAF")
        })?;
    if !out.status.success() {
        return Err(format!(
            "{tool} failed: {}",
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mean = text
        .split("\"vmaf\"")
        .nth(1)
        .and_then(|s| s.split("\"mean\":").nth(1))
        .and_then(|s| s.split([',', '}']).next())
        .map(|s| s.trim().to_string());
    mean.ok_or_else(|| "vmaf ran but printed no mean".to_string())
}

#[test]
fn vplenc_against_vaapih265enc_at_the_same_bitrate() {
    let test = "vplenc_against_vaapih265enc_at_the_same_bitrate";
    let Some(caps) = hw(test) else { return };
    if !caps.supports_encode("h265/annexb") {
        common::skip(test, "the VPL runtime does not encode HEVC");
        return;
    }
    let Some(va) = pf_vaapi::probe() else {
        common::skip(test, "no VA-API device for vaapih265enc");
        return;
    };
    if !va.supports_encode("h265/annexb") {
        common::skip(test, "no VA-API HEVC encode");
        return;
    }
    let Some((frames, w, h, name)) = source() else {
        common::skip(test, "no source");
        return;
    };
    let (vpl, vpl_warn) = encode(
        Box::new(VplEnc::new().with_settings(Settings {
            bitrate_kbps: BITRATE_KBPS,
            gop: GOP,
            ..Settings::default()
        })),
        &frames,
        w,
        h,
    );
    let (va_out, va_warn) = encode(
        Box::new(pf_vaapi::VaapiH265Enc::new().with_bitrate(BITRATE_KBPS).with_gop(GOP)),
        &frames,
        w,
        h,
    );
    eprintln!(
        "{test}: source {} frames, vplenc {} packets, vaapi {} packets",
        frames.len(),
        vpl.len(),
        va_out.len()
    );
    assert!(vpl_warn.is_empty(), "{test}: vplenc warned {vpl_warn:?}");
    assert!(va_warn.is_empty(), "{test}: vaapih265enc warned {va_warn:?}");
    assert_eq!(vpl.len(), frames.len(), "{test}: vplenc encoded every picture");
    assert_eq!(va_out.len(), frames.len(), "{test}: vaapih265enc encoded every picture");
    let bytes = |p: &[(BufferFlags, Vec<u8>)]| p.iter().map(|b| b.1.len()).sum::<usize>();
    let units = |p: &[(BufferFlags, Vec<u8>)]| p.iter().map(|b| b.1.clone()).collect::<Vec<_>>();
    let vpl_dec = decode_sw(Codec::H265, &units(&vpl));
    let va_dec = decode_sw(Codec::H265, &units(&va_out));
    assert_eq!(
        vpl_dec.frames.len(),
        frames.len(),
        "{test}: the vplenc stream decodes whole, warnings {:?}",
        vpl_dec.warnings
    );
    assert_eq!(
        va_dec.frames.len(),
        frames.len(),
        "{test}: the vaapi stream decodes whole, warnings {:?}",
        va_dec.warnings
    );
    let (w, h) = (w as usize, h as usize);
    let (vy, vc) = mean_psnr(&frames, &vpl_dec.frames, w, h);
    let (ay, ac) = mean_psnr(&frames, &va_dec.frames, w, h);
    let ssim = |d: &[Vec<u8>]| {
        frames.iter().zip(d).map(|(a, b)| ssim_luma(&a[..w * h], &b[..w * h], w, h)).sum::<f64>()
            / frames.len() as f64
    };
    let (vs, va_s) = (ssim(&vpl_dec.frames), ssim(&va_dec.frames));
    let seconds = frames.len() as f64 / 25.0;
    eprintln!(
        "{test}: {name}, {} pictures {w}x{h}, {BITRATE_KBPS} kbit/s asked, gop {GOP}",
        frames.len()
    );
    eprintln!(
        "  vplenc        {:>8} bytes {:>7.0} kbit/s  PSNR Y {vy:.2} dB C {vc:.2} dB  SSIM Y {vs:.4}  plan {:?}",
        bytes(&vpl),
        bytes(&vpl) as f64 * 8.0 / 1000.0 / seconds,
        pf_vpl::probe::probe().and_then(|c| c.encode_plan(pf_vpl::ffi::MFX_CODEC_HEVC, true))
    );
    eprintln!(
        "  vaapih265enc  {:>8} bytes {:>7.0} kbit/s  PSNR Y {ay:.2} dB C {ac:.2} dB  SSIM Y {va_s:.4}",
        bytes(&va_out),
        bytes(&va_out) as f64 * 8.0 / 1000.0 / seconds
    );
    let dir =
        std::env::var("PF_VPL_OUT").unwrap_or_else(|_| env!("CARGO_TARGET_TMPDIR").to_string());
    let dir = Path::new(&dir);
    let _ = std::fs::create_dir_all(dir);
    let reference = write_yuv(dir, "quality_source.yuv", &frames);
    let dv = write_yuv(dir, "quality_vplenc.yuv", &vpl_dec.frames);
    let da = write_yuv(dir, "quality_vaapi.yuv", &va_dec.frames);
    let _ = std::fs::write(dir.join("quality_vplenc.hevc"), units(&vpl).concat());
    let _ = std::fs::write(dir.join("quality_vaapi.hevc"), units(&va_out).concat());
    match (reference, dv, da) {
        (Some(r), Some(dv), Some(da)) => {
            eprintln!("  yuv at {} ({w}x{h} i420), streams beside them", dir.display());
            match (vmaf(&r, &dv, w as u32, h as u32), vmaf(&r, &da, w as u32, h as u32)) {
                (Ok(a), Ok(b)) => eprintln!("  VMAF vplenc {a} vaapih265enc {b}"),
                (Err(e), _) | (_, Err(e)) => eprintln!("  VMAF not computed, {e}"),
            }
        }
        _ => eprintln!("  yuv not written (set PF_VPL_OUT to a writable dir for VMAF)"),
    }
    assert!(vy > 25.0 && ay > 25.0, "{test}: a PSNR collapsed, vpl {vy:.1} vaapi {ay:.1}");
}
