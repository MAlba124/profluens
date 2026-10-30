//! vaapivp9enc and vaapiav1enc on whatever this machine has. Skipped without a VA device.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use pf_harness::Harness;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::element::Element;
use profluens_core::format::ValueDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

use pf_vaapi::{Settings, VaapiAv1Enc, VaapiVp9Enc};

fn check(test: &str, el: impl Element + 'static, family: &str, profile: &str) {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}: no VA-API device");
        return;
    };
    let (w, h) = (128u32, 64u32);
    let mut hx = Harness::with_pool(el, 256 * 1024, 16);
    hx.fix_sink_format(
        SinkPad(0),
        "video/raw",
        &[
            ("width", ValueDesc::Int(w.into())),
            ("height", ValueDesc::Int(h.into())),
            ("pixfmt", ValueDesc::Id("nv12")),
        ],
    );
    for i in 0..12u64 {
        let frame: Vec<u8> =
            (0..(w * h * 3 / 2) as usize).map(|p| ((p as u64 + i * 7) % 220 + 16) as u8).collect();
        let mut b = hx.alloc(&frame);
        b.pts = Timestamp(i * 40_000_000);
        hx.push(b).expect("pushed");
    }
    let out: Vec<_> = std::iter::from_fn(|| hx.pull(SrcPad(0))).collect();
    let warnings: Vec<String> = hx
        .bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } => Some(format!("{error}")),
            _ => None,
        })
        .collect();
    if caps.supports_encode(family) {
        assert!(warnings.is_empty(), "{test} warned {warnings:?}");
        assert_eq!(out.len(), 12, "{test} every frame coded");
        assert!(out[0].flags.0 & BufferFlags::KEYFRAME.0 != 0, "{test} a key frame first");
        assert!(out[10].flags.0 & BufferFlags::KEYFRAME.0 != 0, "{test} and every gop");
        eprintln!(
            "{test}: {family} coded, {} bytes",
            out.iter().map(|b| b.memory.data().len()).sum::<usize>()
        );
    } else {
        assert!(out.is_empty(), "{test} nothing coded");
        assert_eq!(warnings.len(), 1, "{test} said once, {warnings:?}");
        assert!(warnings[0].contains(&format!("encodes no {profile}")), "{test} {warnings:?}");
        eprintln!("{test}: no {family} encode here, said once: {}", warnings[0]);
    }
}

#[test]
fn vp9_encodes_where_the_driver_does_and_says_so_where_not() {
    let el = VaapiVp9Enc::new(Settings { gop: 10, ..Settings::default() });
    check("vp9_encodes_where_the_driver_does_and_says_so_where_not", el, "vp9", "VP9Profile0");
}

#[test]
fn av1_encodes_where_the_driver_does_and_says_so_where_not() {
    let el = VaapiAv1Enc::new(Settings { gop: 10, ..Settings::default() });
    check("av1_encodes_where_the_driver_does_and_says_so_where_not", el, "av1", "AV1Profile0");
}

#[test]
fn the_registry_has_each_only_where_the_driver_lists_it() {
    let mut r = pf_registry::Registry::new();
    pf_vaapi::register(&mut r);
    let caps = pf_vaapi::probe();
    let has = |name: &str| r.get(name).is_some();
    assert_eq!(has("vaapivp9enc"), caps.is_some_and(|c| c.supports_encode("vp9")));
    assert_eq!(has("vaapiav1enc"), caps.is_some_and(|c| c.supports_encode("av1")));
    assert_eq!(has("vaapipostproc"), caps.is_some_and(|c| c.video_proc));
}
