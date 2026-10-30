// Tests own their vectors and staging Vecs.
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::codec::nal_head_from_config;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// ffmpeg 8.1 records for x264 Baseline and High, x265 Main and SVT-AV1 at 128x96.
const AVCC_BASELINE: &str =
    "0142c00affe100176742c00ad9020db0110000030001000003001e0f12264801000568cb83cb20";
const AVCC_HIGH: &str =
    "0164000affe100186764000aacd9420db0110000030001000003001e0f12259601000668ebe3cb22c0fdf8f800";
const HVCC_MAIN: &str = "0101600000009000000000001ef000fcfdf8f800000f03200001001840010c01ffff01600000030090000003000003001e959809210001002942010101600000030090000003000003001ea010206165959a4932bc05a020000003002000000301e122000100074401c172b42240";
const AV1C: &str = "81000c000a0a0000000337fbe017c002";

#[test]
fn avcc_from_annex_b_matches_ffmpeg() {
    for rec in [AVCC_BASELINE, AVCC_HIGH] {
        let rec = hex(rec);
        let (head, len) = nal_head_from_config(&rec, false).unwrap();
        assert_eq!(len, 4);
        let mut sets = ParamSets::new(NalCodec::H264);
        sets.absorb(&head).unwrap();
        sets.absorb(&head).unwrap();
        assert_eq!(sets.record().unwrap(), rec);
    }
}

#[test]
fn hvcc_from_annex_b_matches_ffmpeg() {
    let rec = hex(HVCC_MAIN);
    let (head, _) = nal_head_from_config(&rec, true).unwrap();
    let mut sets = ParamSets::new(NalCodec::H265);
    sets.absorb(&head).unwrap();
    let ours = sets.record().unwrap();
    assert_eq!(ours, rec);
    let (again, _) = nal_head_from_config(&ours, true).unwrap();
    assert_eq!(again, head, "the record reads back to the same sets");
    let sps = nals(&head)
        .map(|(a, b)| &head[a..b])
        .find(|n| NalCodec::H265.kind(n[0]) == NalKind::Sps)
        .unwrap();
    let s = hevc_sps(sps).unwrap();
    assert_eq!((s.width, s.height), (128, 96));
}

#[test]
fn missing_sets_are_named() {
    let rec = hex(AVCC_BASELINE);
    let (head, _) = nal_head_from_config(&rec, false).unwrap();
    let pps_at = nals(&head).nth(1).unwrap().0 - 4;
    let mut only_sps = ParamSets::new(NalCodec::H264);
    only_sps.absorb(&head[..pps_at]).unwrap();
    assert_eq!(only_sps.record(), Err(ConfigError::NoPps));
    assert_eq!(ParamSets::new(NalCodec::H264).record(), Err(ConfigError::NoSps));
    let mut h265 = ParamSets::new(NalCodec::H265);
    h265.absorb(&head).unwrap();
    assert_eq!(h265.record(), Err(ConfigError::NoSps), "H.264 units are no H.265 sets");
}

#[test]
fn av1c_from_a_temporal_unit_matches_ffmpeg() {
    let rec = hex(AV1C);
    let mut tu = vec![0x12, 0x00];
    tu.extend_from_slice(&rec[4..]);
    tu.extend_from_slice(&[0x32, 0x03, 0xAA, 0xBB, 0xCC]);
    let (ours, seq) = av1c(&tu).unwrap();
    assert_eq!(ours, rec);
    assert_eq!((seq.width, seq.height, seq.profile), (128, 96, 0));
    assert_eq!(leading_temporal_delimiter(&tu), 2);
    assert_eq!(leading_temporal_delimiter(&tu[2..]), 0);
    assert_eq!(av1c(&[0x32, 0x01, 0x00]), Err(ConfigError::NoSequenceHeader));
    assert_eq!(av1c(&[0x0A, 0x09, 0x00]), Err(ConfigError::BadObu), "a size past the unit");
}

#[test]
fn annex_b_scans_and_converts() {
    let rec = hex(AVCC_BASELINE);
    let (head, _) = nal_head_from_config(&rec, false).unwrap();
    let slice = [0x65u8, 0x88, 0x84, 0x00, 0x21];
    let mut tight = head.clone();
    tight.extend_from_slice(&[0, 0, 0, 1]);
    tight.extend_from_slice(&slice);
    let s = scan(NalCodec::H264, &tight);
    assert!(s.vcl && s.in_place);
    assert_eq!(s.out_len, tight.len());
    let mut copied = vec![0u8; s.out_len];
    assert_eq!(to_length_prefixed(&tight, &mut copied), s.out_len);
    let mut in_place = tight.clone();
    to_length_prefixed_in_place(&mut in_place);
    assert_eq!(in_place, copied);
    let mut annexb = Vec::new();
    crate::Reframer::Nal { length_size: 4 }.reframe_into(&in_place, &mut annexb).unwrap();
    assert_eq!(annexb, tight);

    let mut loose = vec![0xAB, 0, 0, 1];
    loose.extend_from_slice(&slice);
    loose.extend_from_slice(&[0, 0, 0, 0, 1, 0x68, 0xCE, 0, 0]);
    let s = scan(NalCodec::H264, &loose);
    assert!(s.vcl && !s.in_place);
    assert_eq!(s.out_len, 4 + slice.len() + 4 + 2);
    let mut out = vec![0u8; s.out_len];
    to_length_prefixed(&loose, &mut out);
    let mut want = vec![0, 0, 0, slice.len() as u8];
    want.extend_from_slice(&slice);
    want.extend_from_slice(&[0, 0, 0, 2, 0x68, 0xCE]);
    assert_eq!(out, want);

    let s = scan(NalCodec::H264, &head);
    assert!(!s.vcl, "a head of parameter sets alone is no picture");
    assert_eq!(scan(NalCodec::H264, &[1, 2, 3]), Scan { vcl: false, out_len: 0, in_place: false });
}

#[test]
fn truncations_never_panic() {
    for v in [AVCC_BASELINE, AVCC_HIGH, HVCC_MAIN, AV1C] {
        let v = hex(v);
        for n in 0..v.len() {
            let cut = &v[..n];
            let _ = avc_sps(cut);
            let _ = hevc_sps(cut);
            let _ = av1c(cut);
            let _ = av1_sequence_header(cut);
            let _ = scan(NalCodec::H265, cut);
            let mut sets = ParamSets::new(NalCodec::H265);
            let _ = sets.absorb(cut);
            let _ = sets.record();
            let mut copy = cut.to_vec();
            if scan(NalCodec::H264, &copy).in_place {
                to_length_prefixed_in_place(&mut copy);
            }
        }
    }
}
