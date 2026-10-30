//! `vplvpp` with a fixed output box, the picture centred with bars.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use pf_harness::Harness;
use pf_vpl::{Target, VplVpp};
use profluens_core::format::ValueDesc;
use profluens_core::time::Timestamp;

const LEFT: [u8; 3] = [60, 90, 200];
const RIGHT: [u8; 3] = [180, 160, 80];
const BLACK: [u8; 3] = [16, 128, 128];

fn i420(w: usize, h: usize) -> Vec<u8> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut f = Vec::with_capacity(w * h + 2 * cw * ch);
    for _ in 0..h {
        f.extend((0..w).map(|x| if x < w / 2 { LEFT[0] } else { RIGHT[0] }));
    }
    for c in 1..3 {
        for _ in 0..ch {
            f.extend((0..cw).map(|x| if x < cw / 2 { LEFT[c] } else { RIGHT[c] }));
        }
    }
    f
}

fn fix(h: &mut Harness, w: u32, ht: u32) {
    h.fix_sink_format(
        VplVpp::IN,
        "video/raw",
        &[
            ("width", ValueDesc::Int(i64::from(w))),
            ("height", ValueDesc::Int(i64::from(ht))),
            ("pixfmt", ValueDesc::Id("i420")),
        ],
    );
}

fn at(data: &[u8], w: usize, h: usize, x: usize, y: usize) -> [u8; 3] {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    [
        data[y * w + x],
        data[w * h + (y / 2) * cw + x / 2],
        data[w * h + cw * ch + (y / 2) * cw + x / 2],
    ]
}

fn through(test: &str, w: u32, h: u32, ow: u32, oh: u32, frames: u64) -> Vec<Vec<u8>> {
    let target = Target { max_width: ow, max_height: oh, depth: 8 };
    let mut hn = Harness::with_pool(VplVpp::new(target).with_fixed_size(), 2 << 20, 8).real_wakes();
    fix(&mut hn, w, h);
    let frame = i420(w as usize, h as usize);
    for i in 0..frames {
        let mut b = hn.alloc(&frame);
        b.pts = Timestamp(i * 40_000_000);
        hn.push(b).expect("pushed");
    }
    let mut out: Vec<_> = std::iter::from_fn(|| hn.pull(VplVpp::OUT)).collect();
    out.extend(hn.eos().expect("eos"));
    let warnings: Vec<String> = hn
        .bus_messages()
        .iter()
        .filter_map(|m| match m {
            profluens_core::bus::BusMessage::Warning { error, .. } => Some(error.to_string()),
            _ => None,
        })
        .collect();
    assert!(warnings.is_empty(), "{test}: warned {warnings:?}");
    assert_eq!(out.len(), frames as usize, "{test}: every frame out");
    assert_eq!(hn.format_changes(), 1, "{test}: one shape announced");
    out.iter().map(|b| b.memory.data().to_vec()).collect()
}

#[test]
fn a_fixed_box_centres_the_picture_in_black_bars() {
    let test = "a_fixed_box_centres_the_picture_in_black_bars";
    if pf_vpl::probe().is_none() {
        return common::skip(test, "no VPL runtime");
    }
    let (ow, oh) = (640usize, 480usize);
    let near = |got: [u8; 3], want: [u8; 3]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 3);
    // 1280x720 in 640x480 sits at y 60 to 420.
    for (i, data) in through(test, 1280, 720, ow as u32, oh as u32, 2).iter().enumerate() {
        assert_eq!(data.len(), ow * oh * 3 / 2, "{test}: frame {i} is the box in i420");
        let probe = |x, y| at(data, ow, oh, x, y);
        assert!(near(probe(160, 240), LEFT), "{test}: frame {i} left {:?}", probe(160, 240));
        assert!(near(probe(480, 240), RIGHT), "{test}: frame {i} right {:?}", probe(480, 240));
        assert!(near(probe(320, 20), BLACK), "{test}: frame {i} bar above {:?}", probe(320, 20));
        assert!(near(probe(320, 460), BLACK), "{test}: frame {i} bar below {:?}", probe(320, 460));
    }
    // 360x640 in 640x480 is 270x480 at x 184 to 454.
    for (i, data) in through(test, 360, 640, ow as u32, oh as u32, 1).iter().enumerate() {
        assert_eq!(data.len(), ow * oh * 3 / 2, "{test}: tall frame {i} is the box in i420");
        let probe = |x, y| at(data, ow, oh, x, y);
        assert!(near(probe(250, 240), LEFT), "{test}: tall frame {i} left {:?}", probe(250, 240));
        assert!(near(probe(390, 240), RIGHT), "{test}: tall frame {i} right {:?}", probe(390, 240));
        assert!(
            near(probe(40, 240), BLACK),
            "{test}: tall frame {i} bar left {:?}",
            probe(40, 240)
        );
        assert!(
            near(probe(600, 240), BLACK),
            "{test}: tall frame {i} bar right {:?}",
            probe(600, 240)
        );
    }
}
