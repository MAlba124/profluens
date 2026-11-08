#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_open::TrackKind;

/// The video track's frame duration, ns.
fn frame_ns(bytes: &[u8]) -> u64 {
    let o = open(bytes);
    o.tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap().frame_ns
}

/// An MP4 frame lasts the `stts` delta most samples take, over the media timescale.
/// `rate_120.mp4` is ffmpeg's testsrc 64x48 at 120 fps, 30 frames of libx264.
#[test]
fn an_mp4_frame_lasts_the_delta_most_samples_take() {
    assert_eq!(frame_ns(&own("rate_120.mp4")), 8_333_333);
    let b = fixture("plugins/formats/mp4/tests/fixtures/tiny_av.mp4");
    assert_eq!(frame_ns(&b), 66_666_666, "15 fps");
}

/// An FLV frame lasts what `onMetaData`'s framerate says, else the mean step of the frame tags
/// in the head, their stamps whole ms.
#[test]
fn an_flv_frame_lasts_its_stated_rate_else_its_tags_step() {
    let mut b = fixture("plugins/formats/flv/tests/fixtures/h264_aac.flv");
    assert_eq!(frame_ns(&b), 66_666_667, "framerate 15");
    patch_last(&mut b, b"framerate", b"framerXte");
    let f = frame_ns(&b);
    assert!(f.abs_diff(66_666_667) < MS, "{f}");
}
