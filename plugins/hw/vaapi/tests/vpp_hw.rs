//! vaapipostproc on the machine's VA device. Skips without VAEntrypointVideoProc.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use pf_harness::Harness;
use profluens_core::format::ValueDesc;
use profluens_core::time::Timestamp;

use pf_vaapi::ffi;
use pf_vaapi::va::{Display, ImageView, ReadbackImage, UploadImage};
use pf_vaapi::vpp::format::{self, Kind, NV12, P010, P012, P444};
use pf_vaapi::vpp::plan::plan_fixed;
use pf_vaapi::vpp::proc::{Color, Out, Proc, Src, surfaces};
use pf_vaapi::vpp::{Target, VaapiPostProc};

fn display(test: &str) -> Option<Display> {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}: no VA-API device");
        return None;
    };
    if !caps.video_proc {
        eprintln!("skip {test}: no VAEntrypointVideoProc");
        return None;
    }
    Some(Display::open(&caps.device).expect("the probed device opens"))
}

const BT709: Color = Color {
    standard: ffi::VAProcColorStandardBT709,
    range: ffi::VA_SOURCE_RANGE_REDUCED,
    primaries: 0,
    transfer: 0,
    matrix: 0,
};

#[test]
fn every_layout_converts_while_scaling() {
    let Some(d) = display("every_layout_converts_while_scaling") else {
        return;
    };
    let p = Proc::new(&d).expect("a processing context");
    let listed: Vec<&str> =
        format::ALL.iter().filter(|k| p.caps.inputs.contains(&k.fourcc)).map(|k| k.name).collect();
    eprintln!("driver {} inputs {listed:?} caps {:?}", d.vendor(), p.caps);
    for out in format::OUTPUTS {
        assert!(p.caps.outputs.contains(&out.fourcc), "{} out", out.name);
    }
    // The list is a hint, iHD leaves out layouts it converts.
    for k in format::ALL {
        let src = match surfaces(&d, k, 3840, 2160, 1) {
            Ok(s) => s,
            Err(e) if !p.caps.inputs.contains(&k.fourcc) => {
                eprintln!("{} not listed and not allocated ({e}), left out", k.name);
                continue;
            }
            Err(e) => panic!("{} is listed and does not allocate: {e}", k.name),
        };
        for out in format::OUTPUTS {
            let dst = surfaces(&d, out, 1920, 1080, 1).expect("an output surface");
            let whole = Out {
                surface: dst.ids()[0],
                place: (0, 0, 1920, 1080),
                fill: false,
                color: BT709,
                rotation: 0,
            };
            p.run(
                Src { surface: src.ids()[0], at: (0, 0), size: (3840, 2160), color: BT709 },
                whole,
                None,
            )
            .unwrap_or_else(|e| panic!("{} to {}: {e}", k.name, out.name));
            pf_vaapi::va::sync_surface(&d, dst.ids()[0])
                .unwrap_or_else(|e| panic!("{} to {}: {e}", k.name, out.name));
        }
    }
}

const LEFT: [u16; 3] = [60, 90, 200];
const RIGHT: [u16; 3] = [180, 160, 80];

fn sample(v8: u16, depth: u8) -> u16 {
    if depth > 8 { v8 << 8 } else { v8 }
}

fn paint(d: &Display, surface: u32, k: Kind, w: u32, h: u32) {
    let mut img =
        UploadImage::acquire_for(d, surface, w, h, k.surface()).expect("a writable image");
    let side = |x: u32| if x < w / 2 { LEFT } else { RIGHT };
    let wide = k.depth > 8;
    let put = |row: &mut [u8], i: usize, v: u16| {
        if wide {
            row[2 * i..2 * i + 2].copy_from_slice(&v.to_le_bytes());
        } else {
            row[i] = v as u8;
        }
    };
    let bps = if wide { 2 } else { 1 };
    match k.chroma {
        format::Chroma::C420 => {
            for r in 0..h {
                let row = img.row_mut(0, r, w as usize * bps).expect("a luma row");
                for x in 0..w {
                    put(row, x as usize, sample(side(x)[0], k.depth));
                }
            }
            for r in 0..h / 2 {
                let row = img.row_mut(1, r, w as usize * bps).expect("a chroma row");
                for x in 0..w / 2 {
                    let s = side(x * 2);
                    put(row, 2 * x as usize, sample(s[1], k.depth));
                    put(row, 2 * x as usize + 1, sample(s[2], k.depth));
                }
            }
        }
        format::Chroma::C444 => {
            assert_eq!(img.num_planes(), 3, "planar 4:4:4");
            for p in 0..3 {
                for r in 0..h {
                    let row = img.row_mut(p, r, w as usize * bps).expect("a plane row");
                    for x in 0..w {
                        put(row, x as usize, sample(side(x)[p], k.depth));
                    }
                }
            }
        }
        format::Chroma::C422 => panic!("no 4:2:2 painting"),
    }
    img.finish().expect("the image goes into the surface");
}

fn read_at(view: &ImageView<'_>, out: Kind, w: u32, x: u32, y: u32) -> [u16; 3] {
    let wide = out.depth > 8;
    let bps = if wide { 2 } else { 1 };
    let get = |row: &[u8], i: usize| {
        if wide { u16::from_le_bytes([row[2 * i], row[2 * i + 1]]) >> 8 } else { u16::from(row[i]) }
    };
    let yrow = view.row(0, y, w as usize * bps).expect("a luma row");
    let crow = view.row(1, y / 2, w as usize * bps).expect("a chroma row");
    let cx = (x / 2) as usize;
    [get(yrow, x as usize), get(crow, 2 * cx), get(crow, 2 * cx + 1)]
}

#[test]
fn known_colours_come_back_through_scale_chroma_and_depth() {
    let Some(d) = display("known_colours_come_back_through_scale_chroma_and_depth") else {
        return;
    };
    let p = Proc::new(&d).expect("a processing context");
    let (w, h) = (1280, 720);
    let (ow, oh) = (640, 360);
    for k in [NV12, P010, P012, P444] {
        let src = surfaces(&d, k, w, h, 1).expect("a source surface");
        paint(&d, src.ids()[0], k, w, h);
        for out in format::OUTPUTS {
            let dst = surfaces(&d, out, ow, oh, 1).expect("an output surface");
            p.run(
                Src { surface: src.ids()[0], at: (0, 0), size: (w, h), color: BT709 },
                Out {
                    surface: dst.ids()[0],
                    place: (0, 0, ow, oh),
                    fill: false,
                    color: BT709,
                    rotation: 0,
                },
                None,
            )
            .expect("processed");
            let mut img =
                ReadbackImage::new(&d, out.fourcc, out.bpp, ow, oh).expect("a readback image");
            let view = img.read(dst.ids()[0], ow, oh).expect("read back");
            for (x, want) in [(ow / 4, LEFT), (3 * ow / 4, RIGHT)] {
                let got = read_at(&view, out, ow, x, oh / 2);
                for c in 0..3 {
                    assert!(
                        got[c].abs_diff(want[c]) <= 2,
                        "{} to {} at x={x} component {c}, got {got:?} want {want:?}",
                        k.name,
                        out.name
                    );
                }
            }
        }
    }
}

const BLACK: [u16; 3] = [16, 128, 128];

#[test]
fn a_placed_picture_sits_in_black_bars() {
    let Some(d) = display("a_placed_picture_sits_in_black_bars") else {
        return;
    };
    let p = Proc::new(&d).expect("a processing context");
    let (w, h) = (1280, 720);
    let plan = plan_fixed(w, h, NV12, Target { max_width: 640, max_height: 480, depth: 8 })
        .expect("a plan");
    assert_eq!((plan.width, plan.height, plan.place), (640, 480, (0, 60, 640, 360)));
    let src = surfaces(&d, NV12, w, h, 1).expect("a source surface");
    paint(&d, src.ids()[0], NV12, w, h);
    for out in format::OUTPUTS {
        let dst = surfaces(&d, out, plan.width, plan.height, 1).expect("an output surface");
        p.run(
            Src { surface: src.ids()[0], at: (0, 0), size: (w, h), color: BT709 },
            Out {
                surface: dst.ids()[0],
                place: plan.place,
                fill: plan.bars(),
                color: BT709,
                rotation: 0,
            },
            None,
        )
        .expect("processed");
        let mut img = ReadbackImage::new(&d, out.fourcc, out.bpp, plan.width, plan.height)
            .expect("a readback image");
        let view = img.read(dst.ids()[0], plan.width, plan.height).expect("read back");
        let at = |x, y| read_at(&view, out, plan.width, x, y);
        for (x, y, want, what) in [
            (160, 240, LEFT, "left"),
            (480, 240, RIGHT, "right"),
            (320, 20, BLACK, "the bar above"),
            (320, 460, BLACK, "the bar below"),
        ] {
            let got = at(x, y);
            for c in 0..3 {
                assert!(
                    got[c].abs_diff(want[c]) <= 2,
                    "{} at {what} x={x} y={y} component {c}, got {got:?} want {want:?}",
                    out.name
                );
            }
        }
    }
}

fn i420(w: usize, h: usize) -> Vec<u8> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut f = Vec::with_capacity(w * h + 2 * cw * ch);
    for _ in 0..h {
        f.extend((0..w).map(|x| if x < w / 2 { LEFT[0] } else { RIGHT[0] } as u8));
    }
    for c in 1..3 {
        for _ in 0..ch {
            f.extend((0..cw).map(|x| if x < cw / 2 { LEFT[c] } else { RIGHT[c] } as u8));
        }
    }
    f
}

fn fix(h: &mut Harness, w: u32, ht: u32, pixfmt: &'static str) {
    h.fix_sink_format(
        VaapiPostProc::IN,
        "video/raw",
        &[
            ("width", ValueDesc::Int(i64::from(w))),
            ("height", ValueDesc::Int(i64::from(ht))),
            ("pixfmt", ValueDesc::Id(pixfmt)),
        ],
    );
}

#[test]
fn the_element_scales_i420_into_nv12() {
    if display("the_element_scales_i420_into_nv12").is_none() {
        return;
    }
    let target = Target { max_width: 640, max_height: 360, depth: 8 };
    let mut h = Harness::with_pool(VaapiPostProc::new(target), 2 << 20, 8);
    fix(&mut h, 1280, 720, "i420");
    let frame = i420(1280, 720);
    for i in 0..3u64 {
        let mut b = h.alloc(&frame);
        b.pts = Timestamp(i * 40_000_000);
        h.push(b).expect("pushed");
    }
    let out: Vec<_> = std::iter::from_fn(|| h.pull(VaapiPostProc::OUT)).collect();
    assert_eq!(out.len(), 3);
    assert_eq!(h.format_changes(), 1, "announced once");
    let (ow, oh) = (640usize, 360usize);
    for (i, b) in out.iter().enumerate() {
        assert_eq!(b.pts, Timestamp(i as u64 * 40_000_000));
        let data = b.memory.data();
        assert_eq!(data.len(), ow * oh * 3 / 2, "nv12 640x360");
        let (y, uv) = data.split_at(ow * oh);
        let row = oh / 2;
        assert!(
            y[row * ow + ow / 4].abs_diff(LEFT[0] as u8) <= 2
                && y[row * ow + 3 * ow / 4].abs_diff(RIGHT[0] as u8) <= 2
        );
        let c = &uv[(oh / 4) * ow..(oh / 4 + 1) * ow];
        let (l, r) = (2 * (ow / 8), 2 * (3 * ow / 8));
        assert!(
            c[l].abs_diff(LEFT[1] as u8) <= 2 && c[l + 1].abs_diff(LEFT[2] as u8) <= 2,
            "left {:?}",
            &c[l..l + 2]
        );
        assert!(
            c[r].abs_diff(RIGHT[1] as u8) <= 2 && c[r + 1].abs_diff(RIGHT[2] as u8) <= 2,
            "right {:?}",
            &c[r..r + 2]
        );
    }
}

#[test]
fn a_frame_that_fits_goes_on_as_it_came() {
    let target = Target { max_width: 1920, max_height: 1080, depth: 8 };
    let mut h = Harness::with_pool(VaapiPostProc::new(target), 2 << 20, 8);
    fix(&mut h, 640, 360, "nv12");
    let frame: Vec<u8> = (0..640 * 360 * 3 / 2).map(|i| (i % 251) as u8).collect();
    let b = h.alloc(&frame);
    h.push(b).expect("pushed");
    let out = h.pull(VaapiPostProc::OUT).expect("a frame");
    assert_eq!(out.memory.data(), &frame[..]);
}
