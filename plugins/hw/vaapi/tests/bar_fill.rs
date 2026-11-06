//! Bars painted on the GPU, every VPP pass paints the background colour over its whole output,
//! the decoder's twins are painted once, both into surfaces filled with garbage first. Skips
//! without VAEntrypointVideoProc, and where the driver paints no background (the CPU fills).

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::{Arc, mpsc};

use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

use pf_vaapi::decproc;
use pf_vaapi::ffi;
use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::va::{Display, ReadbackImage, UploadImage};
use pf_vaapi::vpp::format::{Kind, NV12, P010};
use pf_vaapi::vpp::plan::Plan;
use pf_vaapi::vpp::proc::{Color, Hdr10, Out, Proc, Src, surfaces};
use pf_vaapi::vpp::{Rect, Target, VaapiPostProc};
use pf_vaapi::{VaapiH265Dec, probe};

use chain::{Codec, Collect, UnitSrc};

const GARBAGE: [u8; 3] = [235, 40, 220];
const PICTURE: [u8; 3] = [180, 100, 160];
const BLACK: [u8; 3] = [16, 128, 128];

fn display(test: &str) -> Option<Display> {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}, no VA-API device");
        return None;
    };
    if !caps.video_proc {
        eprintln!("skip {test}, no VAEntrypointVideoProc");
        return None;
    }
    Some(Display::open(&caps.device).expect("the probed device opens"))
}

/// The processor, None where the driver paints no background. iHD does, its passes are what
/// the bars rest on.
fn painting(d: &Display, test: &str) -> Option<Proc> {
    let p = Proc::new(d).expect("a processing context");
    let paints = p.paints_background(d);
    assert!(paints || !d.vendor().contains("iHD"), "iHD paints the background");
    if !paints {
        eprintln!("skip {test}, {} paints no background", d.vendor());
    }
    paints.then_some(p)
}

/// Every pixel of `s` set to `v`, 8 bit values, P010 holds them in the high byte.
fn fill(d: &Display, s: ffi::VASurfaceID, k: Kind, (w, h): (u32, u32), v: [u8; 3]) {
    let wide = k.depth > 8;
    let mut img = UploadImage::acquire_for(d, s, w, h, k.surface()).expect("mapped");
    let row = w as usize * if wide { 2 } else { 1 };
    let put = |line: &mut [u8], even: u8, odd: u8| {
        let n = if wide { 2 } else { 1 };
        for (i, c) in line.chunks_exact_mut(n).enumerate() {
            let v = if i % 2 == 0 { even } else { odd };
            match wide {
                true => c.copy_from_slice(&(u16::from(v) << 8).to_le_bytes()),
                false => c[0] = v,
            }
        }
    };
    for r in 0..h {
        put(img.row_mut(0, r, row).expect("a luma row"), v[0], v[0]);
    }
    for r in 0..h / 2 {
        put(img.row_mut(1, r, row).expect("a chroma row"), v[1], v[2]);
    }
    img.finish().expect("unmapped");
}

/// Y, U and V of every pixel, 8 bits.
fn read(d: &Display, s: ffi::VASurfaceID, k: Kind, (w, h): (u32, u32)) -> Vec<[u8; 3]> {
    let wide = k.depth > 8;
    let mut img = ReadbackImage::new(d, k.fourcc, k.bpp, w, h).expect("a readback image");
    let view = img.read(s, w, h).expect("read back");
    let row = w as usize * if wide { 2 } else { 1 };
    let at = |l: &[u8], i: usize| if wide { l[2 * i + 1] } else { l[i] };
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let (ly, lc) = (view.row(0, y, row).expect("luma"), view.row(1, y / 2, row).expect("cbcr"));
        out.extend((0..w as usize).map(|x| [at(ly, x), at(lc, x & !1), at(lc, (x & !1) + 1)]));
    }
    out
}

/// Every pixel out of `place` black, the alignment past the plan too, and every pixel in it
/// `picture` when one is given. A scaled or turned edge may land a pixel off, 2 either side of
/// it are left out.
fn check(px: &[[u8; 3]], coded: (u32, u32), place: Rect, picture: Option<[u8; 3]>, what: &str) {
    let (x0, y0, w, h) = place;
    let near = |a: [u8; 3], b: [u8; 3]| (0..3).all(|c| a[c].abs_diff(b[c]) <= 2);
    for y in 0..coded.1 {
        for x in 0..coded.0 {
            let v = px[(y * coded.0 + x) as usize];
            let bar = x + 2 < x0 || x >= x0 + w + 2 || y + 2 < y0 || y >= y0 + h + 2;
            let inner = x >= x0 + 2 && x + 2 < x0 + w && y >= y0 + 2 && y + 2 < y0 + h;
            assert!(!bar || near(v, BLACK), "{what} the bar at {x},{y} is {v:?}");
            if let (Some(p), true) = (picture, inner) {
                assert!(near(v, p), "{what} the picture at {x},{y} is {v:?}");
            }
        }
    }
}

fn t(max_width: u32, max_height: u32, depth: u8) -> Target {
    Target { max_width, max_height, depth }
}

/// Each pass, scaled, converted, tone mapped and turned, paints the bars and the alignment rows
/// of an output full of garbage.
#[test]
fn every_vpp_pass_paints_the_bars_of_a_garbage_surface() {
    let test = "every_vpp_pass_paints_the_bars_of_a_garbage_surface";
    let Some(d) = display(test) else {
        return;
    };
    let Some(p) = painting(&d, test) else {
        return;
    };
    let pq = Color::explicit(9, 16, 9, ffi::VA_SOURCE_RANGE_REDUCED);
    let tone = p.tone_maps().then(|| p.tone_map(&Hdr10::default()).expect("a tone map"));
    let (sw, sh) = (1280, 720);
    for (input, rotation, box_, mapped) in [
        (NV12, 0, t(640, 480, 8), false),
        (P010, 0, t(640, 480, 8), false),
        (P010, 0, t(640, 480, 10), false),
        (NV12, 0, t(640, 640, 8), false),
        (NV12, 90, t(640, 480, 8), false),
        (P010, 270, t(640, 480, 10), false),
        (P010, 0, t(640, 480, 8), true),
    ] {
        let out = if input.depth.min(box_.depth) > 8 { P010 } else { NV12 };
        let what = format!("{} to {} turned {rotation} tone mapped {mapped}", input.name, out.name);
        if mapped && tone.is_none() {
            eprintln!("{what}, the driver maps no tones, left out");
            continue;
        }
        let plan = Plan::turned(sw, sh, input, box_, rotation, true).expect("a plan");
        assert!(plan.bars() && plan.out == out, "{what} {plan:?}");
        let coded = (plan.width.next_multiple_of(64), plan.height.next_multiple_of(64) + 16);
        let src = surfaces(&d, input, sw, sh, 1).expect("a source");
        fill(&d, src.ids()[0], input, (sw, sh), PICTURE);
        let dst = surfaces(&d, out, coded.0, coded.1, 1).expect("an output");
        fill(&d, dst.ids()[0], out, coded, GARBAGE);
        let color = if mapped { pq } else { Color::SDR };
        p.run(
            Src { surface: src.ids()[0], at: (0, 0), size: (sw, sh), color },
            Out {
                surface: dst.ids()[0],
                place: plan.place,
                fill: plan.bars(),
                color: Color::SDR,
                rotation: plan.va_rotation(),
            },
            if mapped { tone.as_ref() } else { None },
        )
        .expect("processed");
        let px = read(&d, dst.ids()[0], out, coded);
        check(&px, coded, plan.place, (!mapped).then_some(PICTURE), &what);
    }
}

/// The twins of a barred plan, garbage at first, are black out of the decoder's rectangle once
/// painted, and stay so as a decode writes only the rectangle (a pass that paints nothing stands
/// in for it). One twin has no sibling to copy, the CPU fills it.
#[test]
fn twins_are_painted_once_and_keep_their_bars_through_a_decode() {
    let test = "twins_are_painted_once_and_keep_their_bars_through_a_decode";
    let Some(d) = display(test) else {
        return;
    };
    let Some(p) = painting(&d, test) else {
        return;
    };
    let (sw, sh) = (1280, 536);
    for (depth, count) in [(8, 4), (10, 4), (8, 1), (10, 1)] {
        let plan = decproc::plan(sw, sh, depth, t(640, 360, depth), true).expect("a plan");
        let (k, place) = (plan.out, plan.place);
        assert!(plan.bars(), "{plan:?}");
        let coded = (plan.width.next_multiple_of(16), plan.height.next_multiple_of(16));
        let what = format!("{} twins {count}", k.name);
        let twins = surfaces(&d, k, coded.0, coded.1, count).expect("twins");
        for &s in twins.ids() {
            fill(&d, s, k, coded, GARBAGE);
        }
        let painter = decproc::black_bars(&d, twins.ids(), k, coded, place).expect("painted");
        assert_eq!(painter.is_some(), count > 1, "{what} painted on the GPU with a sibling");
        let src = surfaces(&d, k, sw, sh, 1).expect("a decoded picture");
        fill(&d, src.ids()[0], k, (sw, sh), PICTURE);
        for &s in twins.ids() {
            check(&read(&d, s, k, coded), coded, place, None, &what);
            p.run(
                Src { surface: src.ids()[0], at: (0, 0), size: (sw, sh), color: Color::SDR },
                Out { surface: s, place, fill: false, color: Color::SDR, rotation: 0 },
                None,
            )
            .expect("decoded");
            check(&read(&d, s, k, coded), coded, place, Some(PICTURE), &what);
        }
    }
}

/// vaapipostproc onto GPU frames in a taller box, its surfaces made black by its passes alone,
/// read back by a second one that changes nothing.
#[test]
fn gpu_frames_in_a_taller_box_have_black_bars() {
    let test = "gpu_frames_in_a_taller_box_have_black_bars";
    let Some(d) = display(test) else {
        return;
    };
    if painting(&d, test).is_none() || !probe().is_some_and(|c| c.decodes_profile("HEVCMain")) {
        return;
    }
    let units = Codec::H265.units(include_bytes!("fixtures/ipb_320x240.hevc"));
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units.clone()));
    let (a, b) = (GpuFrameChannel::new(), GpuFrameChannel::new());
    let dec = p.add(VaapiH265Dec::new_zerocopy(Arc::clone(&a)));
    let boxed = VaapiPostProc::new_zerocopy(t(320, 320, 8), a).with_fixed_size();
    let v1 = p.add(boxed.with_gpu_output(Arc::clone(&b)));
    let v2 = p.add(VaapiPostProc::new_zerocopy(t(320, 320, 8), b).with_fixed_size());
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), v1.sink(VaapiPostProc::IN)).expect("decoder to vpp");
    p.link(v1.src(VaapiPostProc::OUT), v2.sink(VaapiPostProc::IN)).expect("vpp to vpp");
    p.link(v2.src(VaapiPostProc::OUT), sink.sink(SinkPad(0))).expect("vpp to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(v1, 256 * 1024, 16);
    p.set_element_pool(v2, 256 * 1024, 16);
    p.run().expect("the run ends");
    assert!(chain::warnings(&p).is_empty(), "{test} warned");
    drop(p);
    let frames: Vec<_> = rx.try_iter().collect();
    assert_eq!(frames.len(), units.len(), "{test} every picture");
    // NV12 320x320, the picture at rows 40 to 280
    let (w, bar) = (320usize, 40usize);
    for (pts, f) in &frames {
        let (y, uv) = f.split_at(w * w);
        let rows = |r: std::ops::Range<usize>| r.flat_map(|r| r * w..(r + 1) * w);
        let luma = rows(0..bar).chain(rows(w - bar..w)).all(|i| y[i].abs_diff(16) <= 1);
        let chroma =
            rows(0..bar / 2).chain(rows((w - bar) / 2..w / 2)).all(|i| uv[i].abs_diff(128) <= 1);
        assert!(luma && chroma, "{test} bars black at {pts:?}");
    }
}

/// The start of a decoder session shrinking 3840x1600 HEVC Main 10 into a 1080p box, its config,
/// surfaces, context and twins, the median of 9 after 3 warm ones.
#[test]
#[ignore = "timing, run alone with --nocapture"]
fn a_barred_1080p_p010_decoder_session_starts() {
    use pf_vaapi::decproc::Twins;
    use pf_vaapi::va::{Config, Context, SurfaceFormat, Surfaces};
    use std::time::Instant;
    let Some(d) = display("a_barred_1080p_p010_decoder_session_starts") else {
        return;
    };
    let plan = decproc::plan(3840, 1600, 10, t(1920, 1080, 10), true).expect("a plan");
    assert!(plan.bars());
    let (w, h, count) = (3840u32, 1600u32, 13u32);
    let (mut total, mut twins_ms) = (Vec::new(), Vec::new());
    for i in 0..12 {
        let t0 = Instant::now();
        let config =
            Config::new_decode_processing(&d, ffi::VAProfileHEVCMain10, SurfaceFormat::P010)
                .expect("a config");
        let surfaces = Surfaces::new(&d, SurfaceFormat::P010, w, h, count).expect("surfaces");
        let context = Context::new(&d, &config, w as i32, h as i32, &surfaces).expect("a context");
        let t1 = Instant::now();
        let twins = Twins::new(&d, plan, count, (0, 0, w, h), (w, h)).expect("twins");
        let t2 = Instant::now();
        drop((twins, context, surfaces, config));
        if i == 0 {
            eprintln!("first start, the process's paint probe in it, {:?}", t2 - t0);
        }
        if i >= 3 {
            total.push((t2 - t0).as_secs_f64() * 1e3);
            twins_ms.push((t2 - t1).as_secs_f64() * 1e3);
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    eprintln!(
        "session start median {:.2} ms, twins {:.2} ms of it, twins {twins_ms:?}",
        median(&mut total),
        median(&mut twins_ms)
    );
}
