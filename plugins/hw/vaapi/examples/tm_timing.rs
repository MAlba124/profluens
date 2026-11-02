//! How long the processor's HDR10 tone map takes on its first picture and after, for one context
//! and the metadata changed, against a plain conversion. 1080p P010 to NV12.
#![allow(clippy::disallowed_methods)]
//! `cargo run --release --example tm_timing`
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pf_vaapi::ffi;
use pf_vaapi::va::{Display, sync_surface};
use pf_vaapi::vpp::format::{NV12, P010};
use pf_vaapi::vpp::proc::{Color, Hdr10, Out, Proc, Src, surfaces};
use std::time::Instant;

fn hdr10(max_nits: u32, cll: u16) -> Hdr10 {
    Hdr10 {
        primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
        white: [15635, 16450],
        max_luminance: max_nits * 10_000,
        min_luminance: 50,
        max_cll: cll,
        max_fall: 400,
    }
}

fn main() {
    let caps = pf_vaapi::probe().expect("a VA device");
    let d = Display::open(&caps.device).expect("open");
    let (w, h) = (1920, 1080);
    let src = surfaces(&d, P010, w, h, 1).expect("src");
    let dst = surfaces(&d, NV12, w, h, 1).expect("dst");
    let pq = Color::explicit(9, 16, 9, ffi::VA_SOURCE_RANGE_REDUCED);
    let bt2020 = Color { standard: ffi::VAProcColorStandardBT2020, ..Color::default() };
    let run = |p: &Proc, tm: Option<&pf_vaapi::vpp::proc::ToneMap>, label: &str| {
        let t = Instant::now();
        let (sc, oc) = if tm.is_some() { (pq, Color::SDR) } else { (bt2020, bt2020) };
        p.run(
            Src { surface: src.ids()[0], at: (0, 0), size: (w, h), color: sc },
            Out { surface: dst.ids()[0], place: (0, 0, w, h), fill: false, color: oc, rotation: 0 },
            tm,
        )
        .expect("run");
        sync_surface(&d, dst.ids()[0]).expect("sync");
        println!("{label}: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    };
    for round in 0..2 {
        let t = Instant::now();
        let p = Proc::new(&d).expect("proc");
        println!("round {round} proc new: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        run(&p, None, "plain first");
        run(&p, None, "plain second");
        let a = p.tone_map(&hdr10(1000, 400)).expect("tm");
        run(&p, Some(&a), "tone map first");
        run(&p, Some(&a), "tone map second");
        let b = p.tone_map(&hdr10(4000, 1000)).expect("tm");
        run(&p, Some(&b), "tone map other metadata");
        run(&p, Some(&b), "tone map other metadata again");
        run(&p, Some(&a), "tone map first metadata again");
    }
}
