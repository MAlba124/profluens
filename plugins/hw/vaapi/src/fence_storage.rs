// The fence an encoder polls for a picture is of the storage the picture writes. iHD gives a
// 10 bit HEVC reconstruction surface other storage when its first picture is queued, an export
// from before that watches storage nothing writes. Skips without VA-API encode.

// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use profluens_core::element::Element;
use profluens_core::format::ValueDesc;
use profluens_core::id::SinkPad;
use profluens_core::time::Timestamp;

use crate::enc::WATCHED;
use crate::{VaapiH264Enc, VaapiH265Enc};

const SIZE: (u32, u32) = (320, 240);
/// Every reconstruction surface written three times over, five surfaces at most.
const FRAMES: u64 = 16;

/// The pictures out, and the pictures queued on an exported surface, live and stale.
fn run(el: impl Element + 'static, pixfmt: &'static str) -> (usize, (u32, u32)) {
    WATCHED.set((0, 0));
    let (w, h) = SIZE;
    let bytes = (w * h * 3 / 2) as usize * if pixfmt == "p010" { 2 } else { 1 };
    let mut hx = pf_harness::Harness::with_pool(el, 512 * 1024, 32);
    hx.fix_sink_format(
        SinkPad(0),
        "video/raw",
        &[
            ("width", ValueDesc::Int(w.into())),
            ("height", ValueDesc::Int(h.into())),
            ("pixfmt", ValueDesc::Id(pixfmt)),
            ("fps", ValueDesc::Rat(25, 1)),
        ],
    );
    let mut out = 0;
    for i in 0..FRAMES {
        let picture: Vec<u8> = (0..bytes).map(|p| ((p as u64 + i * 7) % 96 + 64) as u8).collect();
        let mut b = hx.alloc(&picture);
        b.pts = Timestamp::from_nanos(i * 40_000_000);
        hx.push(b).expect("pushed");
        out += hx.drain_outputs().len();
    }
    out += hx.eos().expect("the end").len();
    (out, WATCHED.get())
}

fn watches_live_storage(
    name: &str,
    family: &str,
    pixfmt: &'static str,
    el: impl Element + 'static,
) {
    if !crate::probe().is_some_and(|c| c.supports_encode(family)) {
        eprintln!("skip {name} {pixfmt}: no VA-API {family} encode");
        return;
    }
    let (out, (live, stale)) = run(el, pixfmt);
    if out == 0 {
        eprintln!("skip {name} {pixfmt}: the device codes no {pixfmt}");
        return;
    }
    assert_eq!(out as u64, FRAMES, "{name} {pixfmt} every picture");
    assert_eq!(stale, 0, "{name} {pixfmt} a fence of storage the surface no longer has");
    assert!(live > 0, "{name} {pixfmt} no picture was queued on an exported surface");
}

#[test]
fn h265enc_fences_watch_the_storage_a_ten_bit_picture_writes() {
    watches_live_storage("vaapih265enc", "h265/annexb", "p010", VaapiH265Enc::new());
}

#[test]
fn h265enc_fences_watch_the_storage_an_eight_bit_picture_writes() {
    watches_live_storage("vaapih265enc", "h265/annexb", "nv12", VaapiH265Enc::new());
}

#[test]
fn h264enc_fences_watch_the_storage_a_picture_writes() {
    watches_live_storage("vaapih264enc", "h264/annexb", "nv12", VaapiH264Enc::new());
}
