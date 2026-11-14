// A tool, allocation is fine.
#![allow(clippy::disallowed_methods)]

use std::time::Instant;

use pf_av1::av1dec::FRAME_SIZE_LIMIT;
use pf_av1::dav1d::{Decoder, PictureSupply, Settings, Stamps};
use profluens_video::PixelFormats;
use profluens_video::decoder::{
    HeldLayout, pack_gray, pack_gray_wide, pack_p010, pack_planes, pack_reduced,
};
use profluens_video::geometry::frame_size;

fn ivf_units(bytes: &[u8]) -> Vec<&[u8]> {
    assert_eq!(&bytes[..4], b"DKIF", "not an IVF file");
    let mut out = Vec::new();
    let mut at = 32;
    while at + 12 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 12;
        out.push(&bytes[at..at + n]);
        at += n;
    }
    out
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("av1_bench <file.ivf> [threads] [frame_delay]");
    let threads: u32 = args.next().map_or(0, |s| s.parse().expect("threads"));
    let frame_delay: u32 = args.next().map_or(0, |s| s.parse().expect("frame_delay"));
    let bytes = std::fs::read(&path).expect("read");
    let units = ivf_units(&bytes);

    let settings = Settings { threads, frame_delay, frame_size_limit: FRAME_SIZE_LIMIT };
    let mut dec = Decoder::open(&settings, &PictureSupply::new()).expect("open");
    let mut dst: Vec<u8> = Vec::new();
    let mut unit_ms = Vec::with_capacity(units.len());
    let mut copy_ms = Vec::with_capacity(units.len());
    let mut frames = 0usize;
    let mut failed = 0usize;
    let mut pack = |pic: pf_av1::dav1d::Picture, copy_ms: &mut Vec<f64>| {
        let Some((pixfmt, layout)) = pic.pixfmt(PixelFormats::NONE) else {
            return;
        };
        let (w, h) = (pic.width(), pic.height());
        let need = frame_size(pixfmt, w, h);
        if dst.len() < need {
            dst.resize(need, 0);
        }
        let t = Instant::now();
        let planes = [pic.plane(0), pic.plane(1), pic.plane(2)];
        match layout {
            HeldLayout::Native => pack_planes(&mut dst, pixfmt, w, h, &planes).expect("pack"),
            HeldLayout::Planar10 => pack_p010(&mut dst, w, h, &planes).expect("pack"),
            HeldLayout::Reduce { chroma_shift, bits } => {
                pack_reduced(&mut dst, w, h, chroma_shift, bits, pixfmt, &planes).expect("pack")
            }
            HeldLayout::Gray => pack_gray(&mut dst, w, h, &planes).expect("pack"),
            HeldLayout::GrayWide { bits } => {
                pack_gray_wide(&mut dst, (w, h), bits, pixfmt, &planes).expect("pack")
            }
        };
        copy_ms.push(t.elapsed().as_secs_f64() * 1e3);
        frames += 1;
    };

    let start = Instant::now();
    for (i, u) in units.iter().enumerate() {
        let t = Instant::now();
        let stamps = Stamps { timestamp: i as i64, duration: 0, serial: i as i64 };
        if dec.decode(u, stamps, |pic| pack(pic, &mut copy_ms)).is_err() {
            failed += 1;
        }
        unit_ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let t = Instant::now();
    dec.drain(|pic| pack(pic, &mut copy_ms)).expect("drain");
    let drain_ms = t.elapsed().as_secs_f64() * 1e3;
    let total = start.elapsed().as_secs_f64();

    unit_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    copy_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let name = std::path::Path::new(&path).file_name().unwrap().to_string_lossy();
    println!(
        "{name} threads={threads} frame_delay={frame_delay} ({} in flight): {} units, {frames} frames, \
         {failed} failed, {:.1} fps, unit median {:.2} ms p95 {:.2} ms p99 {:.2} ms, drain {:.1} ms, \
         copy median {:.3} ms p95 {:.3} ms",
        dec.frame_delay(),
        units.len(),
        frames as f64 / total,
        percentile(&unit_ms, 0.5),
        percentile(&unit_ms, 0.95),
        percentile(&unit_ms, 0.99),
        drain_ms,
        percentile(&copy_ms, 0.5),
        percentile(&copy_ms, 0.95),
    );
}
