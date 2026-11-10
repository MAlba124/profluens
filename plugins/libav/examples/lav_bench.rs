// A tool, allocation is fine.
#![allow(clippy::disallowed_methods)]

use std::time::Instant;

use pf_libav::lav::{Codec, Decoder, Frame, NONE, Received, Sent, Shape, Supply};
use profluens_core::memory::Pool;
use profluens_video::decoder::{HeldLayout, pack_gray, pack_p010, pack_planes, pack_reduced};
use profluens_video::geometry::frame_size;

/// Annex B H.264 cut before each access unit's first slice or its leading SEI and parameter sets.
fn units(bytes: &[u8]) -> Vec<&[u8]> {
    let (mut cuts, mut vcl) = (vec![0], false);
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i..i + 3] != [0, 0, 1] {
            i += 1;
            continue;
        }
        let t = bytes[i + 3] & 0x1F;
        let start = if i > 0 && bytes[i - 1] == 0 { i - 1 } else { i };
        if vcl && (matches!(t, 6..=9) || (matches!(t, 1 | 5) && bytes[i + 4] & 0x80 != 0)) {
            cuts.push(start);
            vcl = false;
        }
        vcl |= matches!(t, 1 | 5);
        i += 3;
    }
    cuts.push(bytes.len());
    cuts.windows(2).map(|w| &bytes[w[0]..w[1]]).collect()
}

/// The copy the element makes of every picture into a pool slot.
fn pack(f: &Frame, dst: &mut Vec<u8>) {
    let Shape::Named(pixfmt, layout) = f.shape() else { panic!("unpacked shape") };
    let (w, h) = (f.width(), f.height());
    dst.resize(frame_size(pixfmt, w, h), 0);
    let planes = [f.plane(0), f.plane(1), f.plane(2)];
    match layout {
        HeldLayout::Native => pack_planes(dst, pixfmt, w, h, &planes),
        HeldLayout::Planar10 => pack_p010(dst, w, h, &planes),
        HeldLayout::Reduce { chroma_shift, bits } => {
            pack_reduced(dst, w, h, chroma_shift, bits, pixfmt, &planes)
        }
        HeldLayout::Gray => pack_gray(dst, w, h, &planes),
    }
    .expect("pack");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("lav_bench <file.h264> [threads]");
    let threads: u32 = args.next().map_or(1, |s| s.parse().expect("threads"));
    let bytes = std::fs::read(&path).expect("read");
    let units = units(&bytes);
    let pool = Pool::new(64 << 20);
    let supply = Supply::new();
    let mut dec = Decoder::open(Codec::H264, &[], Some(&supply), false, threads).expect("open");
    let mut direct = |_: usize| Some(pool.acquire());
    let (mut dst, mut frames) = (Vec::new(), 0usize);
    let start = Instant::now();
    for (i, u) in units.iter().enumerate() {
        let mut sent = dec.send(u, i as i64, NONE, 0, &mut direct);
        loop {
            while let Ok(Received::Frame(f)) = dec.receive(&mut direct) {
                pack(&f, &mut dst);
                frames += 1;
            }
            match sent {
                Ok(Sent::Full) => sent = dec.send(u, i as i64, NONE, 0, &mut direct),
                _ => break,
            }
        }
    }
    dec.drain().expect("drain");
    while let Ok(Received::Frame(f)) = dec.receive(&mut direct) {
        pack(&f, &mut dst);
        frames += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    let name = std::path::Path::new(&path).file_name().unwrap().to_string_lossy();
    println!(
        "{name} threads={} delay={}: {} units, {frames} frames, {:.1} fps, {} fallbacks",
        dec.threads(),
        dec.delay(),
        units.len(),
        frames as f64 / secs,
        dec.fallbacks(),
    );
}
