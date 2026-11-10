// Test code.
#![allow(clippy::disallowed_methods)]

// The libav decoders in dmabuf mode against raw mode, picture for picture, at one decode thread
// (pictures decoded into udmabuf slots) and at four (pictures copied out of libavcodec's memory).

mod support;

use std::sync::Arc;

use pf_harness::Harness;
use pf_libav::{LibavH264Dec, LibavH265Dec};
use profluens_core::bus::BusMessage;
use profluens_core::element::Element;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::SrcPad;
use profluens_core::memory::PoolConfig;
use profluens_core::time::Timestamp;
use profluens_video::gpuframe::{
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_NV12, DRM_FORMAT_P010, DmabufFormat, F_HEIGHT, F_LAYOUT,
    F_MODIFIER, F_WIDTH, FAMILY_VIDEO_GPU, GpuFrameChannel, GpuFrameHeader, LAYOUT_NV12_DMABUF,
};

const FRAME_NS: u64 = 40_000_000;
const SLOT: usize = 4 * 1024 * 1024;

fn units(
    name: &str,
    size: &str,
    frames: usize,
    pix: &str,
    codec: &[&str],
    hevc: bool,
) -> Option<Vec<Vec<u8>>> {
    let src = format!("testsrc2=size={size}:rate=25");
    let n = frames.to_string();
    let mut args = vec!["-f", "lavfi", "-i", &src, "-frames:v", &n, "-pix_fmt", pix];
    args.extend_from_slice(codec);
    args.extend_from_slice(&["-f", if hevc { "hevc" } else { "h264" }]);
    let bytes = support::make(name, &args)?;
    Some(support::annexb_units(&bytes, hevc))
}

fn other_warnings(h: &mut Harness) -> Vec<String> {
    h.bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } => Some(error.to_string()),
            _ => None,
        })
        // the harness interns no colour names, a pipeline's offers do
        .filter(|w| !w.starts_with("format announcement"))
        .collect()
}

/// Raw mode's pictures, tight I420 or P010, by stamp.
fn raw(dec: impl Element + 'static, threads: i64, units: &[Vec<u8>]) -> Vec<(Timestamp, Vec<u8>)> {
    let mut h = Harness::with_slot_size(dec, SLOT);
    assert!(h.set_prop("threads", Value::Int(threads)));
    let mut out = Vec::new();
    for (i, u) in units.iter().enumerate() {
        let mut b = h.alloc(u);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(b).expect("push");
        out.extend(h.drain_outputs().into_iter().map(|b| (b.pts, b.memory.data().to_vec())));
    }
    out.extend(h.eos().expect("eos").into_iter().map(|b| (b.pts, b.memory.data().to_vec())));
    assert!(other_warnings(&mut h).is_empty(), "{:?}", other_warnings(&mut h));
    out.sort_by_key(|o| o.0);
    out
}

struct Frame {
    pts: Timestamp,
    fourcc: u32,
    coded: (u32, u32),
    planes: Vec<(u32, u32)>,
    bytes: Vec<u8>,
}

fn read(fd: i32, len: usize) -> Vec<u8> {
    let size = len.next_multiple_of(pf_memory::udmabuf::align());
    // SAFETY: the frame's dup of its slot, mapped read-only and unmapped before returning.
    let view =
        unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
    assert_ne!(view, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
    let bytes = unsafe { std::slice::from_raw_parts(view as *const u8, len) }.to_vec();
    unsafe { libc::munmap(view, size) };
    bytes
}

fn formats() -> [DmabufFormat; 2] {
    let linear = |fourcc| DmabufFormat { fourcc, modifiers: vec![DRM_FORMAT_MOD_LINEAR] };
    [linear(DRM_FORMAT_NV12), linear(DRM_FORMAT_P010)]
}

fn drain(
    rx: &mut profluens_video::gpuframe::FrameRx,
    outs: Vec<profluens_core::buffer::Buffer>,
    frames: &mut Vec<Frame>,
) {
    for b in outs {
        let header = GpuFrameHeader::from_bytes(b.memory.data()).expect("a header");
        let f = rx.take(header.token).expect("the frame of the descriptor");
        let rows = |i: usize| if i == 0 { f.coded_h } else { f.coded_h.div_ceil(2) };
        let ends = f.planes.iter().enumerate().map(|(i, p)| p.offset + p.pitch * rows(i));
        let len = ends.max().unwrap_or(0);
        frames.push(Frame {
            pts: b.pts,
            fourcc: f.drm_format,
            coded: (f.coded_w, f.coded_h),
            planes: f.planes.iter().map(|p| (p.offset, p.pitch)).collect(),
            bytes: read(f.fds[0], len as usize),
        });
    }
}

/// Dmabuf mode's pictures on a udmabuf pool of one page slots, which grow to the pictures.
fn dmabuf<E: Element + 'static>(
    make: impl FnOnce(Arc<GpuFrameChannel>, &[DmabufFormat]) -> E,
    threads: i64,
    units: &[Vec<u8>],
) -> Option<Vec<Frame>> {
    let dev = match pf_memory::udmabuf::device() {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("skip: {e}");
            return None;
        }
    };
    let ch = GpuFrameChannel::new();
    let dec = make(Arc::clone(&ch), &formats());
    let page = pf_memory::udmabuf::align();
    let config =
        PoolConfig { align: page, allocator: dev.allocator(), ..PoolConfig::heap(page, 8) };
    let mut h = Harness::with_pool_config(dec, config);
    assert!(h.set_prop("threads", Value::Int(threads)));
    let fixed = [
        (F_WIDTH, ValueDesc::Int(16)),
        (F_HEIGHT, ValueDesc::Int(16)),
        (F_LAYOUT, ValueDesc::Id(LAYOUT_NV12_DMABUF)),
        (F_MODIFIER, ValueDesc::Int(0)),
    ];
    h.fix_src_format(SrcPad(0), FAMILY_VIDEO_GPU, &fixed);
    let mut rx = ch.take_rx().expect("the sink's end");
    let mut frames = Vec::new();
    for (i, u) in units.iter().enumerate() {
        let mut b = h.alloc(u);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(b).expect("push");
        drain(&mut rx, h.drain_outputs(), &mut frames);
    }
    drain(&mut rx, h.eos().expect("eos"), &mut frames);
    assert!(other_warnings(&mut h).is_empty(), "{:?}", other_warnings(&mut h));
    frames.sort_by_key(|f| f.pts);
    Some(frames)
}

/// Tight I420 as NV12 at the even size, the last row and column repeated.
fn nv12_of(i420: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let ew = 2 * cw;
    let mut out = Vec::with_capacity(ew * 2 * ch * 3 / 2);
    for r in 0..2 * ch {
        let row = &i420[r.min(h - 1) * w..][..w];
        out.extend_from_slice(row);
        out.extend(std::iter::repeat_n(row[w - 1], ew - w));
    }
    let (u, v) = i420[w * h..].split_at(cw * ch);
    for r in 0..ch {
        for x in 0..cw {
            out.extend([u[r * cw + x], v[r * cw + x]]);
        }
    }
    out
}

fn check(name: &str, (w, h): (usize, usize), raw: &[(Timestamp, Vec<u8>)], got: &[Frame]) {
    assert_eq!(got.len(), raw.len(), "{name}: every picture");
    for (k, ((pts, want), f)) in raw.iter().zip(got).enumerate() {
        assert_eq!(*pts, f.pts, "{name}: picture {k} stamp");
        let deep = want.len() > w * h * 3 / 2;
        match deep {
            true => {
                assert_eq!(f.fourcc, DRM_FORMAT_P010, "{name}: picture {k} relinked to P010");
                assert_eq!(f.planes, [(0, 2 * w as u32), (2 * (w * h) as u32, 2 * w as u32)]);
                assert!(f.bytes == *want, "{name}: picture {k} differs from raw mode");
            }
            false => {
                assert_eq!(f.fourcc, DRM_FORMAT_NV12, "{name}: picture {k} as NV12");
                let (ew, eh) = (w.next_multiple_of(2), h.next_multiple_of(2));
                assert_eq!(f.coded, (ew as u32, eh as u32));
                assert_eq!(f.planes, [(0, ew as u32), ((ew * eh) as u32, ew as u32)]);
                assert!(f.bytes == nv12_of(want, w, h), "{name}: picture {k} differs from raw");
            }
        }
    }
}

const X264: &[&str] = &["-c:v", "libx264", "-preset", "fast", "-bf", "3", "-g", "12"];
const X265: &[&str] =
    &["-c:v", "libx265", "-preset", "fast", "-x265-params", "bframes=3:keyint=12:log-level=none"];

fn h264(ch: Arc<GpuFrameChannel>, f: &[DmabufFormat]) -> LibavH264Dec {
    LibavH264Dec::new().with_dmabuf(ch, f)
}

fn h265(ch: Arc<GpuFrameChannel>, f: &[DmabufFormat]) -> LibavH265Dec {
    LibavH265Dec::new().with_dmabuf(ch, f)
}

#[test]
fn h264_in_dmabuf_mode_is_raw_mode_as_nv12_at_one_and_four_threads() {
    let Some(units) = units("dmabuf_h264.h264", "320x240", 30, "yuv420p", X264, false) else {
        return;
    };
    for threads in [1, 4] {
        let want = raw(LibavH264Dec::new(), threads, &units);
        let Some(got) = dmabuf(h264, threads, &units) else { return };
        check(&format!("h264 at {threads}"), (320, 240), &want, &got);
    }
}

#[test]
fn hevc_main10_in_dmabuf_mode_is_raw_mode_as_p010_at_one_and_four_threads() {
    let Some(units) = units("dmabuf_main10.hevc", "320x180", 20, "yuv420p10le", X265, true) else {
        return;
    };
    for threads in [1, 4] {
        let want = raw(LibavH265Dec::new(), threads, &units);
        let Some(got) = dmabuf(h265, threads, &units) else { return };
        check(&format!("hevc main10 at {threads}"), (320, 180), &want, &got);
    }
}

#[test]
fn h264_422_in_dmabuf_mode_is_reduced_then_interleaved_at_four_threads() {
    let args =
        &["-c:v", "libx264", "-preset", "fast", "-profile:v", "high422", "-bf", "2", "-g", "12"];
    let Some(units) = units("dmabuf_422.h264", "320x240", 12, "yuv422p", args, false) else {
        return;
    };
    let want = raw(LibavH264Dec::new(), 4, &units);
    let Some(got) = dmabuf(h264, 4, &units) else { return };
    check("h264 4:2:2 at 4", (320, 240), &want, &got);
}
