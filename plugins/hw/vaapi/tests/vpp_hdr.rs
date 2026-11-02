//! The processor's HDR10 to SDR tone map on a synthetic P010 PQ frame, by the processor and by
//! `vaapipostproc`. Skips without the filter.

// Test code.
#![allow(clippy::disallowed_methods)]

use pf_vaapi::ffi;
use pf_vaapi::va::{Display, ReadbackImage, UploadImage};
use pf_vaapi::vpp::format::{self, Kind, P010};
use pf_harness::Harness;
use pf_vaapi::vpp::proc::{Color, Hdr10, Out, Proc, Src, surfaces};
use pf_vaapi::vpp::{Target, VaapiPostProc};
use profluens_core::format::ValueDesc;
use profluens_video::color::{Primaries, StreamColor, TransferFn};

/// 10 bit limited range PQ codes of 1, 100 and 10000 cd/m2.
const BANDS: [u16; 3] = [195, 509, 940];

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

fn paint_pq(d: &Display, surface: u32, w: u32, h: u32) {
    let mut img =
        UploadImage::acquire_for(d, surface, w, h, P010.surface()).expect("a writable image");
    let row_len = w as usize * 2;
    let band = |x: u32| BANDS[(x * 3 / w) as usize] << 6;
    for r in 0..h {
        let row = img.row_mut(0, r, row_len).expect("a luma row");
        for x in 0..w as usize {
            row[2 * x..2 * x + 2].copy_from_slice(&band(x as u32).to_le_bytes());
        }
    }
    let grey = (512u16 << 6).to_le_bytes();
    for r in 0..h / 2 {
        let row = img.row_mut(1, r, row_len).expect("a chroma row");
        for x in 0..w as usize {
            row[2 * x..2 * x + 2].copy_from_slice(&grey);
        }
    }
    img.finish().expect("the image goes into the surface");
}

fn band_luma(d: &Display, surface: u32, out: Kind, w: u32, h: u32) -> [u16; 3] {
    let mut img = ReadbackImage::new(d, out.fourcc, out.bpp, w, h).expect("a readback image");
    let view = img.read(surface, w, h).expect("read back");
    let wide = out.depth > 8;
    let bps = if wide { 2 } else { 1 };
    let row = view.row(0, h / 2, w as usize * bps).expect("a luma row");
    let get = |x: usize| {
        if wide { u16::from_le_bytes([row[2 * x], row[2 * x + 1]]) >> 8 } else { u16::from(row[x]) }
    };
    [get(w as usize / 6), get(w as usize / 2), get(w as usize * 5 / 6)]
}

fn mastering() -> Hdr10 {
    Hdr10 {
        primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
        white: [15635, 16450],
        max_luminance: 10_000_000,
        min_luminance: 1,
        max_cll: 1000,
        max_fall: 400,
    }
}

#[test]
fn pq_bands_tone_map_into_sdr_range() {
    let test = "pq_bands_tone_map_into_sdr_range";
    let Some(d) = display(test) else { return };
    let p = Proc::new(&d).expect("a processing context");
    eprintln!("driver {} tone maps {}", d.vendor(), p.tone_maps());
    if !p.tone_maps() {
        return eprintln!("skip {test}: no HDR10 to SDR tone mapping");
    }
    let tm = p.tone_map(&mastering()).expect("the tone mapping filter");
    let pq = Color::explicit(9, 16, 9, ffi::VA_SOURCE_RANGE_REDUCED);
    let (w, h) = (384, 216);
    let src = surfaces(&d, P010, w, h, 1).expect("a source surface");
    paint_pq(&d, src.ids()[0], w, h);
    for out in format::OUTPUTS {
        for (ow, oh) in [(w, h), (192, 108)] {
            let dst = surfaces(&d, out, ow, oh, 1).expect("an output surface");
            let r = p.run(
                Src { surface: src.ids()[0], at: (0, 0), size: (w, h), color: pq },
                Out {
                    surface: dst.ids()[0],
                    place: (0, 0, ow, oh),
                    fill: false,
                    color: Color::SDR,
                    rotation: 0,
                },
                Some(&tm),
            );
            eprintln!("{} {ow}x{oh}: {r:?}", out.name);
            r.unwrap_or_else(|e| panic!("tone map to {} {ow}x{oh}: {e}", out.name));
            pf_vaapi::va::sync_surface(&d, dst.ids()[0]).expect("synced");
            let y = band_luma(&d, dst.ids()[0], out, ow, oh);
            eprintln!("{} {ow}x{oh}: bands 1, 100, 10000 cd/m2 came out as Y {y:?}", out.name);
            assert!(
                y[0] < y[1] && y[1] < y[2],
                "{} {ow}x{oh}: the bands keep their order, {y:?}",
                out.name
            );
            assert!(y[0] < 80, "{} {ow}x{oh}: 1 cd/m2 is dark, {y:?}", out.name);
            assert!(y[2] > 180, "{} {ow}x{oh}: the peak is near white, {y:?}", out.name);
        }
    }
}

/// A raw P010 frame of the PQ bands, limited range grey chroma.
fn pq_frame(w: usize, h: usize) -> Vec<u8> {
    let mut f = Vec::with_capacity(w * h * 3);
    for _ in 0..h {
        for x in 0..w {
            f.extend_from_slice(&(BANDS[x * 3 / w] << 6).to_le_bytes());
        }
    }
    for _ in 0..h / 2 * w {
        f.extend_from_slice(&(512u16 << 6).to_le_bytes());
    }
    f
}

/// The element fed the bands as P010 of `transfer` with a mastering display, to `depth` at
/// half size.
fn through_element(depth: u8, transfer: &'static str) -> (Vec<u8>, StreamColor) {
    let (w, h) = (384usize, 216usize);
    let target = Target { max_width: 192, max_height: 108, depth };
    let mut hn = Harness::with_pool(VaapiPostProc::new(target), 2 << 20, 8);
    let m = mastering();
    let pack = |a: [u16; 4]| ((a[0] as u64) << 48 | (a[1] as u64) << 32 | (a[2] as u64) << 16 | a[3] as u64) as i64;
    let [r, g, b] = m.primaries;
    hn.fix_sink_format(
        VaapiPostProc::IN,
        "video/raw",
        &[
            ("width", ValueDesc::Int(w as i64)),
            ("height", ValueDesc::Int(h as i64)),
            ("pixfmt", ValueDesc::Id("p010")),
            ("matrix", ValueDesc::Id("bt2020")),
            ("range", ValueDesc::Id("limited")),
            ("transfer", ValueDesc::Id(transfer)),
            ("primaries", ValueDesc::Id("bt2020")),
            ("mastering-rg", ValueDesc::Int(pack([r[0], r[1], g[0], g[1]]))),
            ("mastering-bw", ValueDesc::Int(pack([b[0], b[1], m.white[0], m.white[1]]))),
            ("mastering-luminance", ValueDesc::Int(((m.max_luminance as u64) << 32 | u64::from(m.min_luminance)) as i64)),
            ("content-light", ValueDesc::Int(i64::from(m.max_cll) << 16 | i64::from(m.max_fall))),
        ],
    );
    let b = hn.alloc(&pq_frame(w, h));
    hn.push(b).expect("pushed");
    let out = hn.pull(VaapiPostProc::OUT).expect("a picture");
    let f = hn.announced().expect("a format");
    (out.memory.data().to_vec(), StreamColor::read_vocabulary(hn.vocabulary(), &f))
}

#[test]
fn the_element_tone_maps_pq_to_an_sdr_picture_that_says_so() {
    let test = "the_element_tone_maps_pq_to_an_sdr_picture_that_says_so";
    let Some(d) = display(test) else { return };
    if !Proc::new(&d).expect("a processing context").tone_maps() {
        return eprintln!("skip {test}: no HDR10 to SDR tone mapping");
    }
    let (pic, c) = through_element(8, "pq");
    let (w, h) = (192usize, 108usize);
    assert_eq!(pic.len(), w * h * 3 / 2, "nv12");
    let row = &pic[h / 2 * w..(h / 2 + 1) * w];
    let y = [row[w / 6], row[w / 2], row[w * 5 / 6]];
    eprintln!("bands 1, 100, 10000 cd/m2 came out as Y {y:?}");
    // the PQ codes as they are would read 48, 127, 235
    assert!(y[0] < y[1] && y[1] < y[2] && y[0] < 35 && y[2] > 180, "tone mapped, {y:?}");
    assert_eq!((c.transfer, c.primaries), (Some(TransferFn::Bt709), Some(Primaries::Bt709)), "{c:?}");
    assert!(c.hdr.is_empty(), "no mastering display past the tone map, {c:?}");
}

#[test]
fn ten_bits_out_keep_the_pq_samples_and_their_colour() {
    let test = "ten_bits_out_keep_the_pq_samples_and_their_colour";
    if display(test).is_none() {
        return;
    }
    let (pic, c) = through_element(10, "pq");
    let (w, h) = (192usize, 108usize);
    assert_eq!(pic.len(), w * h * 3, "p010");
    let at = |x: usize| u16::from_le_bytes([pic[2 * (h / 2 * w + x)], pic[2 * (h / 2 * w + x) + 1]]) >> 6;
    let y = [at(w / 6), at(w / 2), at(w * 5 / 6)];
    for (got, want) in y.iter().zip(BANDS) {
        assert!(got.abs_diff(want) <= 4, "PQ codes kept, {y:?}");
    }
    assert_eq!(c.transfer, Some(TransferFn::Pq), "{c:?}");
    assert!(!c.hdr.is_empty(), "the mastering display goes on, {c:?}");
}

/// iHD 26.1 on Gen9 refuses an HLG source to the filter, the picture then goes on converted
/// without it and states the colour it still has.
#[test]
fn an_hlg_picture_comes_out_either_way_and_says_which() {
    if display("an_hlg_picture_comes_out_either_way_and_says_which").is_none() {
        return;
    }
    let (pic, c) = through_element(8, "hlg");
    assert_eq!(pic.len(), 192 * 108 * 3 / 2, "nv12");
    eprintln!("hlg came out {:?}", c.transfer);
    assert!(matches!(c.transfer, Some(TransferFn::Bt709 | TransferFn::Hlg)), "{c:?}");
}
