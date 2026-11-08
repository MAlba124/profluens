#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_open::TrackKind;

/// The video track's pixel aspect ratio.
fn par(name: &str) -> (u32, u32) {
    let o = open(&own(name));
    let v = o.tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap();
    assert_eq!((v.width, v.height), (64, 48), "{name}");
    v.par
}

/// Made with ffmpeg from a 64x48 libx264 picture. The container's ratio wins over the SPS's
/// as ffmpeg and mpv take it, the SPS's fills in where the container states none.
#[test]
fn the_container_states_the_pixel_aspect_else_the_sps() {
    // pasp 32:27, the SPS rewritten to 1:1
    assert_eq!(par("par_pasp.mp4"), (32, 27));
    // pasp 1:1 over an SPS of 32:27
    assert_eq!(par("par_pasp_square.mp4"), (1, 1));
    // no DisplayWidth, the SPS's 32:27
    assert_eq!(par("par_sps.mkv"), (32, 27));
    // DisplayWidth and DisplayHeight of 8:9 pixels, narrower than square
    assert_eq!(par("par_narrow.mkv"), (8, 9));
}
