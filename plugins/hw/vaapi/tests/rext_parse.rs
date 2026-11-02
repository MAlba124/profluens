//! HEVC format range extensions 4:2:0 at 12 bits parse as decodable: the SPS's VUI and HRD walked
//! to its extension flags, the PPS's range extension, every slice header. No hardware.

// Test code.
#![allow(clippy::disallowed_methods)]

use pf_vaapi::h265parse::{self, parse_pps, parse_slice_header_after, parse_sps, split_nals};

/// x265 Main 12 with mastering display SEI, no VUI timing, no extensions.
const X265: &[u8] = include_bytes!("fixtures/main12_128x128.hevc");
/// A 2160p59.94 Main 12 stream's VPS, SPS and PPS, NAL HRD in its VUI, no extensions.
const HRD: &[u8] = include_bytes!("fixtures/rext12_hrd_sets.hevc");

fn sets(stream: &[u8]) -> (h265parse::Sps, h265parse::Pps) {
    let nals = split_nals(stream);
    let sps = nals
        .iter()
        .find(|n| n.nal_type == 33)
        .and_then(|n| parse_sps(n.raw(stream)))
        .expect("an SPS");
    let pps = nals
        .iter()
        .find(|n| n.nal_type == 34)
        .and_then(|n| parse_pps(n.raw(stream)))
        .expect("a PPS");
    (sps, pps)
}

#[test]
fn a_main12_stream_with_hrd_in_its_vui_is_decodable() {
    let (sps, pps) = sets(HRD);
    assert_eq!((sps.depth(), sps.rext, sps.has_extension, sps.range_ext), (12, true, false, 0));
    assert!(sps.supported(), "{sps:?}");
    assert_eq!((sps.width(), sps.height()), (3840, 2160));
    assert!(!pps.other_ext && pps.range == h265parse::PpsRange::default());
}

#[test]
fn x265_main12_parses_through_every_slice() {
    let (sps, pps) = sets(X265);
    assert_eq!((sps.depth(), sps.rext, sps.supported()), (12, true, true));
    let nals = split_nals(X265);
    let mut prev = None;
    let mut slices = 0;
    for n in nals.iter().filter(|n| n.nal_type < 32) {
        let sh = parse_slice_header_after(n.raw(X265), n.nal_type, &sps, &pps, prev.as_ref())
            .expect("a slice header");
        prev = Some(sh);
        slices += 1;
    }
    assert_eq!(slices, 8);
}

/// Main 10 is taken as before whatever its VUI holds, the walk is for range extensions alone.
#[test]
fn main10_is_still_main10() {
    let (sps, _) = sets(include_bytes!("fixtures/hdr10_128x128.hevc"));
    assert_eq!(
        (sps.depth(), sps.rext, sps.has_extension, sps.supported()),
        (10, false, false, true)
    );
}
