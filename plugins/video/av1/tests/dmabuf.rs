// Test code.
#![allow(clippy::disallowed_methods)]

// dav1d in dmabuf mode against raw mode, picture for picture, at one thread and at four.

use std::sync::Arc;

use pf_av1::Av1Dec;
use pf_harness::Harness;
use profluens_core::buffer::Buffer;
use profluens_core::bus::BusMessage;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::SrcPad;
use profluens_core::memory::PoolConfig;
use profluens_core::time::Timestamp;
use profluens_video::gpuframe::{
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_NV12, DRM_FORMAT_P010, DmabufFormat, F_HEIGHT, F_LAYOUT,
    F_MODIFIER, F_WIDTH, FAMILY_VIDEO_GPU, FrameRx, GpuFrameChannel, GpuFrameHeader,
    LAYOUT_NV12_DMABUF,
};

const FRAME_NS: u64 = 40_000_000;

fn ivf_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 32;
    while at + 12 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 12;
        out.push(bytes[at..at + n].to_vec());
        at += n;
    }
    out
}

fn units(name: &str) -> Vec<Vec<u8>> {
    let path = format!("{}/tests/fixtures/{name}.ivf", env!("CARGO_MANIFEST_DIR"));
    ivf_units(&std::fs::read(path).expect("fixture ivf"))
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

fn push_all(h: &mut Harness, units: &[Vec<u8>], mut each: impl FnMut(&mut Harness, Vec<Buffer>)) {
    for (i, u) in units.iter().enumerate() {
        let mut b = h.alloc(u);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        b.duration = Timestamp::from_nanos(FRAME_NS);
        h.push(b).expect("push");
        let out = h.drain_outputs();
        each(h, out);
    }
    let out = h.eos().expect("eos");
    each(h, out);
}

fn raw(threads: i64, units: &[Vec<u8>]) -> Vec<(Timestamp, Vec<u8>)> {
    let mut h = Harness::with_slot_size(Av1Dec::new(), 64 * 1024);
    assert!(h.set_prop("threads", Value::Int(threads)));
    let mut out = Vec::new();
    push_all(&mut h, units, |_, b| {
        out.extend(b.into_iter().map(|b| (b.pts, b.memory.data().to_vec())))
    });
    assert!(other_warnings(&mut h).is_empty(), "{:?}", other_warnings(&mut h));
    out
}

struct Frame {
    pts: Timestamp,
    fourcc: u32,
    coded: (u32, u32),
    shown: (u32, u32),
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

fn drain(rx: &mut FrameRx, outs: Vec<Buffer>, frames: &mut Vec<Frame>) {
    for b in outs {
        let header = GpuFrameHeader::from_bytes(b.memory.data()).expect("a header");
        let f = rx.take(header.token).expect("the frame of the descriptor");
        let rows = |i: usize| if i == 0 { f.coded_h } else { f.coded_h.div_ceil(2) };
        let ends = f.planes.iter().enumerate().map(|(i, p)| p.offset + p.pitch * rows(i));
        let len = ends.max().unwrap_or(0) as usize;
        frames.push(Frame {
            pts: b.pts,
            fourcc: f.drm_format,
            coded: (f.coded_w, f.coded_h),
            shown: (f.disp_w, f.disp_h),
            bytes: read(f.fds[0], len),
        });
    }
}

fn dmabuf(threads: i64, units: &[Vec<u8>]) -> Option<Vec<Frame>> {
    let dev = match pf_memory::udmabuf::device() {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("skip: {e}");
            return None;
        }
    };
    let linear = |fourcc| DmabufFormat { fourcc, modifiers: vec![DRM_FORMAT_MOD_LINEAR] };
    let ch = GpuFrameChannel::new();
    let formats = [linear(DRM_FORMAT_NV12), linear(DRM_FORMAT_P010)];
    let dec = Av1Dec::new().with_dmabuf(Arc::clone(&ch), &formats);
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
    push_all(&mut h, units, |_, b| drain(&mut rx, b, &mut frames));
    assert!(other_warnings(&mut h).is_empty(), "{:?}", other_warnings(&mut h));
    Some(frames)
}

/// Raw mode's tight I420, or gray with neutral chroma, as NV12 at the even size.
fn nv12_of(raw: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let ew = 2 * cw;
    let mut out = Vec::with_capacity(ew * 2 * ch * 3 / 2);
    for r in 0..2 * ch {
        let row = &raw[r.min(h - 1) * w..][..w];
        out.extend_from_slice(row);
        out.extend(std::iter::repeat_n(row[w - 1], ew - w));
    }
    if raw.len() == w * h {
        out.extend(std::iter::repeat_n(128, ew * ch));
        return out;
    }
    let (u, v) = raw[w * h..].split_at(cw * ch);
    for r in 0..ch {
        for x in 0..cw {
            out.extend([u[r * cw + x], v[r * cw + x]]);
        }
    }
    out
}

fn check(name: &str) {
    let units = units(name);
    for threads in [1, 4] {
        let want = raw(threads, &units);
        let Some(got) = dmabuf(threads, &units) else { return };
        assert_eq!(got.len(), want.len(), "{name} at {threads}: every picture");
        for (k, ((pts, raw), f)) in want.iter().zip(&got).enumerate() {
            let (w, h) = (f.shown.0 as usize, f.shown.1 as usize);
            assert_eq!(*pts, f.pts, "{name} at {threads}: picture {k} stamp");
            let deep = raw.len() > w * h + 2 * w.div_ceil(2) * h.div_ceil(2);
            match deep {
                true => {
                    assert_eq!(f.fourcc, DRM_FORMAT_P010, "{name}: picture {k} as P010");
                    assert!(f.bytes == *raw, "{name} at {threads}: picture {k} differs");
                }
                false => {
                    assert_eq!(f.fourcc, DRM_FORMAT_NV12, "{name}: picture {k} as NV12");
                    let even = (w.next_multiple_of(2) as u32, h.next_multiple_of(2) as u32);
                    assert_eq!(f.coded, even, "{name}: picture {k} coded even");
                    let nv12 = nv12_of(raw, w, h);
                    assert!(f.bytes == nv12, "{name} at {threads}: picture {k} differs");
                }
            }
        }
    }
}

#[test]
fn eight_bit_av1_in_dmabuf_mode_is_raw_mode_as_nv12() {
    check("inter_64x48");
}

#[test]
fn an_odd_av1_picture_goes_out_even_as_nv12() {
    check("odd_61x37");
}

#[test]
fn monochrome_av1_goes_out_as_nv12_with_neutral_chroma() {
    check("mono_64x48");
}

#[test]
fn a_resized_av1_stream_grows_its_dmabuf_slots() {
    check("resize_64x48_96x64");
}

#[test]
fn ten_bit_av1_in_dmabuf_mode_is_raw_mode_as_p010() {
    check("tenbit_64x48");
}

#[test]
fn av1_422_is_reduced_then_interleaved() {
    check("i422_64x48");
}
