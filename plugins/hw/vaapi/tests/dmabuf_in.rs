// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

// Software decoders into vaapipostproc, their udmabuf slots imported against their raw pictures
// uploaded, byte for byte, at one decode thread and at four. Where the driver imports no udmabuf
// the dmabuf chain falls back to the upload and must still match.

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use profluens_core::element::Element;
use profluens_core::format::Value;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::{DmabufFormat, GpuFrameChannel};
use pf_vaapi::vpp::{Target, VaapiPostProc, linear_imports, udmabuf_allocator};

use chain::{Collect, UnitSrc};

struct Run {
    frames: Vec<(Timestamp, Vec<u8>)>,
    warnings: Vec<String>,
    imports: u64,
}

type Make = fn(Option<(&Arc<GpuFrameChannel>, &[DmabufFormat])>) -> Box<dyn Element>;

fn run(make: Make, units: &[Vec<u8>], target: Target, threads: i64, dmabuf: bool) -> Run {
    let (tx, rx) = mpsc::channel();
    let ch = GpuFrameChannel::queued();
    let formats = linear_imports().unwrap_or(&[]);
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units.to_vec()));
    let dec = p.add_boxed(make(dmabuf.then_some((&ch, formats))));
    let vpp = match dmabuf {
        true => VaapiPostProc::new(target).with_dmabuf_input(Arc::clone(&ch)),
        false => VaapiPostProc::new(target),
    };
    let stats = vpp.stats();
    let vpp = p.add(vpp);
    let sink = p.add(Collect(tx));
    p.set(dec, "threads", Value::Int(threads)).expect("a threads prop");
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
    p.link(vpp.src(VaapiPostProc::OUT), sink.sink(SinkPad(0))).expect("vpp to sink");
    match (dmabuf, udmabuf_allocator()) {
        (true, Ok((align, allocator))) => {
            p.set_element_allocator(dec, align, allocator);
            // one page, the slots grow to the pictures as the streamer's do
            p.set_element_pool(dec, align, 6);
        }
        _ => p.set_element_pool(dec, 4 << 20, 6),
    }
    p.set_element_pool(vpp, 256 * 1024, 16);
    p.run().expect("the run ends");
    let warnings = chain::warnings(&p);
    drop(p);
    let mut frames: Vec<_> = rx.try_iter().collect();
    frames.sort_by_key(|f| f.0);
    Run { frames, warnings, imports: stats.imports.load(Ordering::Relaxed) }
}

fn device(test: &str) -> bool {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}: no VA-API device");
        return false;
    };
    if !caps.video_proc {
        eprintln!("skip {test}: no VAEntrypointVideoProc");
        return false;
    }
    true
}

fn check(test: &str, make: Make, units: &[Vec<u8>], target: Target) {
    if !device(test) {
        return;
    }
    let imports = match linear_imports() {
        Ok(f) => {
            eprintln!("{test}: the driver imports {} linear layouts", f.len());
            true
        }
        Err(e) => {
            eprintln!("{test}: no udmabuf import ({e}), the dmabuf chain must upload");
            false
        }
    };
    let first = run(make, units, target, 1, false);
    for threads in [1, 4] {
        let raw = run(make, units, target, threads, false);
        let dma = run(make, units, target, threads, true);
        let tag = format!("{test} at {threads} threads");
        assert!(raw.warnings.is_empty(), "{tag} raw warned {:?}", raw.warnings);
        assert!(dma.warnings.is_empty(), "{tag} dmabuf warned {:?}", dma.warnings);
        assert_eq!(raw.frames.len(), units.len(), "{tag} every picture uploaded");
        assert_eq!(dma.frames.len(), units.len(), "{tag} every picture through the dmabuf chain");
        assert_eq!(raw.imports, 0, "{tag} the raw chain imports nothing");
        match imports {
            true => assert!(
                dma.imports > 0 && dma.imports <= 40,
                "{tag} the slots imported once each, {} imports",
                dma.imports
            ),
            false => assert_eq!(dma.imports, 0, "{tag} fell back to the upload"),
        }
        for (k, ((pa, a), (pb, b))) in raw.frames.iter().zip(&dma.frames).enumerate() {
            assert_eq!(pa, pb, "{tag} picture {k} stamp");
            assert!(a == b, "{tag} picture {k} differs between upload and import");
        }
        for (k, ((_, a), (_, b))) in first.frames.iter().zip(&raw.frames).enumerate() {
            assert!(a == b, "{tag} picture {k} differs from one thread's");
        }
        eprintln!("{tag}: {} pictures identical, {} imports", dma.frames.len(), dma.imports);
    }
}

fn libav_h264(d: Option<(&Arc<GpuFrameChannel>, &[DmabufFormat])>) -> Box<dyn Element> {
    match d {
        Some((ch, f)) => Box::new(pf_libav::LibavH264Dec::new().with_dmabuf(Arc::clone(ch), f)),
        None => Box::new(pf_libav::LibavH264Dec::new()),
    }
}

fn libav_h265(d: Option<(&Arc<GpuFrameChannel>, &[DmabufFormat])>) -> Box<dyn Element> {
    match d {
        Some((ch, f)) => Box::new(pf_libav::LibavH265Dec::new().with_dmabuf(Arc::clone(ch), f)),
        None => Box::new(pf_libav::LibavH265Dec::new()),
    }
}

fn dav1d(d: Option<(&Arc<GpuFrameChannel>, &[DmabufFormat])>) -> Box<dyn Element> {
    match d {
        Some((ch, f)) => Box::new(pf_av1::Av1Dec::new().with_dmabuf(Arc::clone(ch), f)),
        None => Box::new(pf_av1::Av1Dec::new()),
    }
}

fn ivf_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 32;
    while at + 12 <= bytes.len() {
        let n = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        at += 12;
        out.push(bytes[at..at + n as usize].to_vec());
        at += n as usize;
    }
    out
}

#[test]
fn libav_h264_into_vaapipostproc_is_the_same_imported_as_uploaded() {
    let units = chain::Codec::H264.units(include_bytes!("fixtures/refs16_320x240.h264"));
    let target = Target { max_width: 160, max_height: 120, depth: 8 };
    check("libav_h264", libav_h264, &units, target);
}

#[test]
fn libav_hevc_main10_into_vaapipostproc_is_the_same_imported_as_uploaded() {
    let units = chain::Codec::H265.units(include_bytes!("fixtures/hdr10_128x128.hevc"));
    let target = Target { max_width: 64, max_height: 64, depth: 10 };
    check("libav_hevc_main10", libav_h265, &units, target);
}

#[test]
fn dav1d_into_vaapipostproc_is_the_same_imported_as_uploaded() {
    let units = ivf_units(include_bytes!("fixtures/av1_tools_352x288.ivf"));
    let target = Target { max_width: 256, max_height: 144, depth: 8 };
    check("dav1d", dav1d, &units, target);
}

#[test]
fn dav1d_ten_bit_into_vaapipostproc_is_the_same_imported_as_uploaded() {
    let units = ivf_units(include_bytes!("fixtures/av1_p10_160x96.ivf"));
    let target = Target { max_width: 80, max_height: 48, depth: 10 };
    check("dav1d_10", dav1d, &units, target);
}

/// A frame on a linear link whose fd is no dmabuf, a memfd, so the driver refuses the import.
fn memfd_frame(token: u64, bytes: &[u8], (w, h): (u32, u32)) -> pf_vaapi::gpuframe::GpuFrame {
    use pf_vaapi::gpuframe::{DRM_FORMAT_NV12, DmabufFds, ExportedPlane, GpuFrame, Planes};
    // SAFETY: a NUL-terminated name, the fd is the frame's below.
    let fd = unsafe { libc::memfd_create(c"not-a-dmabuf".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "{}", std::io::Error::last_os_error());
    // SAFETY: writes from a live slice into the fd.
    let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(n, bytes.len() as isize);
    let mut fds = DmabufFds::default();
    // SAFETY: the memfd is the frame's alone.
    unsafe { fds.push(fd) };
    let mut planes = Planes::default();
    planes.push(ExportedPlane { object_index: 0, offset: 0, pitch: w });
    planes.push(ExportedPlane { object_index: 0, offset: w * h, pitch: w });
    GpuFrame {
        token,
        fds,
        drm_format: DRM_FORMAT_NV12,
        coded_w: w,
        coded_h: h,
        disp_w: w,
        disp_h: h,
        crop_x: 0,
        crop_y: 0,
        planes,
        epoch: 0,
        lease: None,
    }
}

#[test]
fn a_frame_the_driver_will_not_import_is_copied_in_by_the_cpu() {
    use pf_harness::Harness;
    use pf_vaapi::gpuframe::{
        F_LAYOUT, F_MODIFIER, FAMILY_VIDEO_GPU, GpuFrameHeader, LAYOUT_NV12_DMABUF,
    };
    use profluens_core::format::ValueDesc;
    let test = "a_frame_the_driver_will_not_import_is_copied_in_by_the_cpu";
    if !device(test) {
        return;
    }
    let (w, h) = (64u32, 48u32);
    let target = Target { max_width: 32, max_height: 24, depth: 8 };
    let size = |w: u32, h: u32| {
        [("width", ValueDesc::Int(i64::from(w))), ("height", ValueDesc::Int(i64::from(h)))]
    };
    let pictures: Vec<Vec<u8>> = (0..3u8)
        .map(|k| {
            let n = (w * h * 3 / 2) as usize;
            (0..n).map(|i| (i as u8).wrapping_mul(7).wrapping_add(40 * k)).collect()
        })
        .collect();

    let mut raw = Harness::with_pool(VaapiPostProc::new(target), 1 << 20, 8);
    let [width, height] = size(w, h);
    raw.fix_sink_format(
        VaapiPostProc::IN,
        "video/raw",
        &[width, height, ("pixfmt", ValueDesc::Id("nv12"))],
    );
    let ch = GpuFrameChannel::new();
    let vpp = VaapiPostProc::new(target).with_dmabuf_input(Arc::clone(&ch));
    let stats = vpp.stats();
    let mut gpu = Harness::with_pool(vpp, 1 << 20, 8);
    let fields = [
        width,
        height,
        (F_LAYOUT, ValueDesc::Id(LAYOUT_NV12_DMABUF)),
        (F_MODIFIER, ValueDesc::Int(0)),
    ];
    gpu.fix_sink_format(VaapiPostProc::IN, FAMILY_VIDEO_GPU, &fields);
    gpu.start().expect("start");
    let mut tx = ch.take_tx().expect("the decoder's end");
    for (k, picture) in pictures.iter().enumerate() {
        let pts = Timestamp::from_nanos(k as u64 * chain::FRAME_NS);
        let mut b = raw.alloc(picture);
        b.pts = pts;
        raw.push(b).expect("pushed");
        let want = raw.pull(VaapiPostProc::OUT).expect("uploaded");
        let token = k as u64 + 1;
        let lease = gpu.alloc(&[0]).memory;
        assert!(tx.push(memfd_frame(token, picture, (w, h)), lease));
        let header = GpuFrameHeader {
            token,
            coded_w: w,
            coded_h: h,
            disp_w: w,
            disp_h: h,
            crop_x: 0,
            crop_y: 0,
        };
        let mut b = gpu.alloc(&header.to_bytes());
        b.pts = pts;
        gpu.push(b).expect("the refused import is no failure");
        let got = gpu.pull(VaapiPostProc::OUT).expect("copied in");
        assert!(got.memory.data() == want.memory.data(), "picture {k} as the upload makes it");
    }
    assert_eq!(stats.imports.load(Ordering::Relaxed), 0, "nothing imported");
    assert_eq!(stats.frames.load(Ordering::Relaxed), 3, "every picture through");
}
