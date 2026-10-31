//! The processor's HDR10 to SDR tone map on a synthetic P010 PQ frame. Skips without the filter.

// Test code.
#![allow(clippy::disallowed_methods)]

use pf_vaapi::ffi;
use pf_vaapi::va::{Display, ReadbackImage, UploadImage};
use pf_vaapi::vpp::format::{self, Kind, P010};
use pf_vaapi::vpp::proc::{Color, Hdr10, Out, Proc, Src, surfaces};

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
