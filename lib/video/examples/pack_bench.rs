// Times the software decoder's pack paths per frame.
// `cargo run --release --example pack_bench -p profluens-video [frames]`

// A timing harness, not an element.
#![allow(clippy::disallowed_methods)]

use std::time::Instant;

use profluens_video::PixelFormat;
use profluens_video::decoder::{HeldLayout, PlaneRef, pack_nv12, pack_p010, pack_reduced};

struct Plane {
    bytes: Vec<u8>,
    stride: usize,
}

// Random samples below 1 << bits, rows padded to 64 bytes as libav aligns them.
fn plane(cols: usize, rows: usize, bits: u8, seed: &mut u64) -> Plane {
    let size = 1 + usize::from(bits > 8);
    let stride = (cols * size).next_multiple_of(64);
    let mask = (1u32 << bits) - 1;
    let mut bytes = vec![0; stride * rows];
    for row in bytes.chunks_exact_mut(stride) {
        for s in row[..cols * size].chunks_exact_mut(size) {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            let v = (*seed >> 20) as u32 & mask;
            s.copy_from_slice(&v.to_le_bytes()[..size]);
        }
    }
    Plane { bytes, stride }
}

fn picture(w: usize, h: usize, shift: (u8, u8), bits: u8) -> [Plane; 3] {
    let mut seed = 0x2545_F491_4F6C_DD1D;
    let (cw, ch) = (w.div_ceil(1 << shift.0), h.div_ceil(1 << shift.1));
    [plane(w, h, bits, &mut seed), plane(cw, ch, bits, &mut seed), plane(cw, ch, bits, &mut seed)]
}

fn time(label: &str, frames: u32, mut op: impl FnMut()) {
    op();
    let mut best = f64::INFINITY;
    let mut sum = 0.0;
    for _ in 0..frames {
        let t = Instant::now();
        op();
        let dt = t.elapsed().as_secs_f64();
        best = best.min(dt);
        sum += dt;
    }
    let mean = sum / f64::from(frames);
    println!("{label:<34} best {:>10.0} ns  mean {:>10.0} ns", best * 1e9, mean * 1e9);
}

fn refs(p: &[Plane; 3]) -> [PlaneRef<'_>; 3] {
    p.each_ref().map(|p| PlaneRef { data: &p.bytes, stride: p.stride })
}

fn run(w: usize, h: usize, frames: u32) {
    let (wu, hu) = (w as u32, h as u32);
    let mut dst = vec![0u8; 2 * w * h * 3];
    let mut scratch = vec![0u8; w * h * 3];
    println!("\n{w}x{h}");

    for (name, shift) in [("4:4:4", (0, 0)), ("4:2:2", (1, 0))] {
        let pic = picture(w, h, shift, 10);
        let planes = refs(&pic);
        time(&format!("{name} 10-bit to P010"), frames, || {
            let r = pack_reduced(&mut dst, wu, hu, shift, 10, PixelFormat::P010, &planes);
            std::hint::black_box(r.ok());
        });
    }
    let pic = picture(w, h, (1, 1), 10);
    let planes = refs(&pic);
    time("4:2:0 10-bit to P010", frames, || {
        std::hint::black_box(pack_p010(&mut dst, wu, hu, &planes).ok());
    });

    let pic = picture(w, h, (0, 0), 8);
    let planes = refs(&pic);
    let layout = HeldLayout::Reduce { chroma_shift: (0, 0), bits: 8 };
    time("4:4:4 8-bit to NV12", frames, || {
        let s = Some(&mut scratch);
        let r = pack_nv12(&mut dst, s, layout, PixelFormat::I420, wu, hu, &planes);
        std::hint::black_box(r.ok());
    });
    let pic = picture(w, h, (1, 1), 8);
    let planes = refs(&pic);
    time("4:2:0 8-bit to NV12", frames, || {
        let r = pack_nv12(&mut dst, None, HeldLayout::Native, PixelFormat::I420, wu, hu, &planes);
        std::hint::black_box(r.ok());
    });

    for (name, shift) in [("4:4:4", (0, 0)), ("4:2:0", (1, 1))] {
        let pic = picture(w, h, shift, 10);
        let planes = refs(&pic);
        time(&format!("{name} 10-bit narrowed to I420"), frames, || {
            let r = pack_reduced(&mut dst, wu, hu, shift, 10, PixelFormat::I420, &planes);
            std::hint::black_box(r.ok());
        });
    }
}

fn main() {
    let frames = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(60);
    run(3840, 2160, frames);
    run(1920, 1080, frames);
}
