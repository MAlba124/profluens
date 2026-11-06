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

fn au(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter().flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat()).collect()
}

#[test]
fn h264_unreferenced_by_nal_ref_idc() {
    let h = NalCodec::H264;
    let aud: &[u8] = &[0x09, 0xf0];
    assert!(unreferenced(h, &[], &au(&[aud, &[0x01, 0x9e, 0x11]])), "a non-reference slice");
    assert!(!unreferenced(h, &[], &au(&[aud, &[0x41, 0x9a, 0x22]])), "a reference P slice");
    assert!(!unreferenced(h, &[], &au(&[&[0x67, 1], &[0x68, 2], &[0x65, 0x88]])), "an IDR");
    assert!(!unreferenced(h, &[], &au(&[&[0x01, 1], &[0x21, 2]])), "one slice is referenced");
    assert!(!unreferenced(h, &[], &au(&[aud])), "no picture");
    assert!(!unreferenced(h, &[], &[0x01, 0x9e]), "not Annex B");
}

#[test]
fn hevc_unreferenced_by_type_and_the_top_sub_layer() {
    let h = NalCodec::H265;
    // VPS, SPS with sps_max_sub_layers_minus1 0, then 1
    let head0 = au(&[&[0x40, 0x01, 0x0c], &[0x42, 0x01, 0x01, 0x01]]);
    let head1 = au(&[&[0x42, 0x01, 0x03, 0x01]]);
    assert_eq!((hevc_top_layer(&head0), hevc_top_layer(&head1)), (Some(0), Some(1)));
    let trail_n: &[u8] = &[0x00, 0x01, 0xaf];
    let trail_r: &[u8] = &[0x02, 0x01, 0xaf];
    let sei: &[u8] = &[0x4e, 0x01, 0x05];
    assert!(unreferenced(h, &head0, &au(&[sei, trail_n])));
    assert!(!unreferenced(h, &head0, &au(&[trail_r])));
    assert!(!unreferenced(h, &head0, &au(&[&[0x26, 0x01, 0xaf]])), "an IDR");
    assert!(!unreferenced(h, &head1, &au(&[trail_n])), "a higher sub-layer may refer to it");
    assert!(unreferenced(h, &head1, &au(&[&[0x00, 0x02, 0xaf]])), "on the top sub-layer");
    assert!(!unreferenced(h, &[], &au(&[trail_n])), "no SPS to tell");
    assert!(!unreferenced(h, &head0, &au(&[&[0x00]])), "a header cut short");
}

#[test]
fn param_sets_of_an_h264_idr_unit() {
    let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4, 5];
    let mut out = Vec::new();
    annexb_param_sets(NalCodec::H264, &au, &mut out);
    assert_eq!(out, [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
    out.clear();
    annexb_param_sets(NalCodec::H264, &au[12..], &mut out);
    assert!(out.is_empty(), "a slice alone");
}

/// A PPS followed by a four byte start code keeps the zero byte as its own, an encoder's sets
/// read back alone have none, both give the same head.
#[test]
fn trailing_zeros_are_left_off_a_param_set() {
    let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4, 5];
    let alone = [0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3];
    let (mut a, mut b) = (Vec::new(), Vec::new());
    annexb_param_sets(NalCodec::H264, &au, &mut a);
    annexb_param_sets(NalCodec::H264, &alone, &mut b);
    assert_eq!(a, [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
    assert_eq!(a, b);
}

#[test]
fn an_idr_unit_without_its_param_sets() {
    let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4, 5];
    let mut out = Vec::new();
    assert!(strip_param_sets(NalCodec::H264, &au, &mut out));
    assert_eq!(out, [0, 0, 0, 1, 0x65, 4, 5]);
    out.clear();
    assert!(!strip_param_sets(NalCodec::H264, &au[12..], &mut out), "a slice alone stays");
    assert!(out.is_empty());
}

#[test]
fn param_sets_of_an_hevc_idr_unit() {
    let au = [0, 0, 1, 0x40, 1, 9, 0, 0, 1, 0x42, 1, 1, 0, 0, 1, 0x44, 1, 0, 0, 1, 0x26, 1, 8];
    let mut out = Vec::new();
    annexb_param_sets(NalCodec::H265, &au, &mut out);
    assert_eq!(out, [0, 0, 0, 1, 0x40, 1, 9, 0, 0, 0, 1, 0x42, 1, 1, 0, 0, 0, 1, 0x44, 1]);
}

/// The scalar start code search `start_code` replaced, the reference it must agree with.
fn scalar_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(if i > from && data[i - 1] == 0 { (i - 1, 4) } else { (i, 3) });
        } else {
            i += 1;
        }
    }
    None
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// `n` coded bytes, a zero in every `zero_in` drawn, escaped as an encoder escapes (7.4.1).
fn escaped(rng: &mut Rng, n: usize, zero_in: u64, out: &mut Vec<u8>) {
    let mut zeros = 0;
    for _ in 0..n {
        let b = if rng.below(zero_in) == 0 { 0 } else { (rng.next() >> 8) as u8 };
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    // rbsp_trailing_bits, a NAL never ends in a zero byte
    out.push(0x80);
}

#[test]
fn the_start_code_search_agrees_with_the_scalar_one() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..200 {
        let len = rng.below(600) as usize;
        // small bytes only, start codes everywhere
        let data: Vec<u8> = (0..len).map(|_| rng.below(4 + round as u64 % 3) as u8).collect();
        for from in 0..=len + 1 {
            assert_eq!(start_code(&data, from), scalar_start_code(&data, from), "{data:?} {from}");
        }
    }
}

const H264_SETS: [&[u8]; 2] = [&[0x67, 0x42, 0x00, 0x00, 0x03, 0x01, 0x1e], &[0x68, 0xce, 0x38]];
const H265_SETS: [&[u8]; 3] = [
    &[0x40, 0x01, 0x0c, 0x01],
    &[0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00],
    &[0x44, 0x01, 0xc1, 0x72],
];

/// A unit from `nals`, each behind a three or four byte start code, some followed by
/// trailing zero bytes.
fn coded(rng: &mut Rng, nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    if rng.below(4) == 0 {
        out.push(0xAB);
    }
    for n in nals {
        out.extend_from_slice(if rng.below(2) == 0 { &[0, 0, 1][..] } else { &[0, 0, 0, 1] });
        out.extend_from_slice(n);
        let zeros = rng.below(3) as usize;
        out.resize(out.len() + zeros, 0);
    }
    out
}

/// The single walk against `strip_param_sets` and `unreferenced`, and `kept_bytes` cut at
/// every offset and fragment size against the stripped copy.
fn check_walk(codec: NalCodec, heads: &[&[u8]], au: &[u8], kept: &mut Vec<(usize, usize)>) {
    let s = scan_unit(codec, au, kept);
    let mut stripped = Vec::new();
    assert_eq!(s.param_sets, strip_param_sets(codec, au, &mut stripped), "{au:x?}");
    let mut whole = Vec::new();
    extend_kept(au, kept, &mut whole);
    assert_eq!(whole.len(), s.kept_len);
    if s.param_sets {
        assert_eq!(whole, stripped, "{au:x?}");
    }
    for head in heads {
        assert_eq!(scan_unreferenced(codec, head, &s), unreferenced(codec, head, au), "{au:x?}");
    }
    for frag in [1, 3, 5, 64] {
        let mut out = vec![0u8; frag];
        let mut cut = Vec::new();
        let mut at = 0;
        while at < whole.len() {
            let n = kept_bytes(au, kept, at, &mut out);
            assert_eq!(n, frag.min(whole.len() - at));
            cut.extend_from_slice(&out[..n]);
            at += n;
        }
        assert_eq!(cut, whole);
        assert_eq!(kept_bytes(au, kept, whole.len(), &mut out), 0, "past the end");
    }
}

#[test]
fn one_walk_strips_and_judges_an_h264_unit() {
    let h = NalCodec::H264;
    let aud: &[u8] = &[0x09, 0xf0];
    let sei: &[u8] = &[0x06, 0x05, 0x00, 0x00, 0x03, 0x02, 0x80];
    let idr: &[u8] = &[0x65, 0x88, 0x00, 0x00, 0x03, 0x00, 0x21];
    let nonref: &[u8] = &[0x01, 0x9e, 0x00, 0x00, 0x03, 0x01];
    let refp: &[u8] = &[0x41, 0x9a, 0x22];
    let [sps, pps] = H264_SETS;
    let cases: [&[&[u8]]; 9] = [
        &[sps, pps, idr],
        &[aud, sps, pps, sei, idr, idr],
        &[aud, sei, idr, sps, pps, idr],
        &[aud, idr, sps, pps],
        &[aud, nonref, nonref],
        &[aud, nonref, refp],
        &[aud, refp],
        &[aud],
        &[sps, pps],
    ];
    let mut rng = Rng(7);
    let mut kept = Vec::new();
    for nals in cases {
        for _ in 0..16 {
            check_walk(h, &[&[]], &coded(&mut rng, nals), &mut kept);
        }
    }
    check_walk(h, &[&[]], &[0x01, 0x9e], &mut kept);
    check_walk(h, &[&[]], &[], &mut kept);
    let s = scan_unit(h, &au(&[aud, nonref, nonref]), &mut kept);
    assert_eq!((s.param_sets, s.nonref), (false, Some(0)));
    let s = scan_unit(h, &au(&[sps, pps, idr]), &mut kept);
    assert_eq!((s.param_sets, s.nonref, s.kept_len), (true, None, 4 + idr.len()));
}

#[test]
fn one_walk_strips_and_judges_an_hevc_unit() {
    let h = NalCodec::H265;
    let head0 = au(&[&[0x40, 0x01, 0x0c], &[0x42, 0x01, 0x01, 0x01]]);
    let head1 = au(&[&[0x42, 0x01, 0x03, 0x01]]);
    let heads: [&[u8]; 3] = [&head0, &head1, &[]];
    let aud: &[u8] = &[0x46, 0x01, 0x50];
    let sei: &[u8] = &[0x4e, 0x01, 0x05, 0x00, 0x00, 0x03, 0x01];
    let idr: &[u8] = &[0x26, 0x01, 0xaf, 0x00, 0x00, 0x03, 0x02];
    let trail_n: &[u8] = &[0x00, 0x01, 0xaf];
    let trail_n1: &[u8] = &[0x00, 0x02, 0xaf];
    let trail_r: &[u8] = &[0x02, 0x01, 0xaf];
    let [vps, sps, pps] = H265_SETS;
    let cases: [&[&[u8]]; 10] = [
        &[vps, sps, pps, idr],
        &[aud, vps, sps, pps, sei, idr],
        &[aud, idr, vps, sps, pps, idr],
        &[aud, idr, pps],
        &[aud, sei, trail_n, trail_n],
        &[aud, trail_n1],
        &[aud, trail_n, trail_n1],
        &[aud, trail_r],
        &[aud, &[0x00]],
        &[aud],
    ];
    let mut rng = Rng(11);
    let mut kept = Vec::new();
    for nals in cases {
        for _ in 0..16 {
            check_walk(h, &heads, &coded(&mut rng, nals), &mut kept);
        }
    }
}

#[test]
fn random_units_walk_as_the_old_functions_did() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut kept = Vec::new();
    for _ in 0..400 {
        let codec = if rng.below(2) == 0 { NalCodec::H264 } else { NalCodec::H265 };
        let mut nals: Vec<Vec<u8>> = Vec::new();
        for _ in 0..rng.below(6) {
            let mut n = vec![(rng.next() >> 8) as u8 & 0x7E, 1 + rng.below(2) as u8];
            if rng.below(3) == 0 {
                let sets: &[&[u8]] = match codec {
                    NalCodec::H264 => &H264_SETS,
                    NalCodec::H265 => &H265_SETS,
                };
                n = sets[rng.below(sets.len() as u64) as usize].to_vec();
            }
            let len = rng.below(40) as usize;
            escaped(&mut rng, len, 3, &mut n);
            nals.push(n);
        }
        let refs: Vec<&[u8]> = nals.iter().map(Vec::as_slice).collect();
        let head0 = au(&[&[0x42, 0x01, 0x01, 0x01]]);
        let head1 = au(&[&[0x42, 0x01, 0x03, 0x01]]);
        let au = coded(&mut rng, &refs);
        check_walk(codec, &[&head0, &head1, &[]], &au, &mut kept);
    }
}

/// Throughput of the per unit walk over 10 s of a synthetic 80 Mbit H.264 stream, the old
/// scalar `strip_param_sets` path against `scan_unit`. Run with
/// `cargo test --release -p pf-mkv --lib walk_throughput -- --ignored --nocapture`.
#[test]
#[ignore]
fn walk_throughput() {
    use std::time::Instant;
    fn scalar_nals(data: &[u8], mut f: impl FnMut(usize, usize)) {
        let mut next = scalar_start_code(data, 0).map(|(at, len)| at + len);
        while let Some(begin) = next {
            let (mut end, after) = match scalar_start_code(data, begin) {
                Some((at, len)) => (at, Some(at + len)),
                None => (data.len(), None),
            };
            next = after;
            while end > begin && data[end - 1] == 0 {
                end -= 1;
            }
            if end > begin {
                f(begin, end);
            }
        }
    }
    // a 2 s GOP at 30 fps, a 1.2 MB IDR then 59 units of 316 KB, four slices each
    for (label, zero_in) in [("random", 256u64), ("zero heavy", 8)] {
        let mut rng = Rng(42);
        let mut units = Vec::new();
        let mut total = 0usize;
        for i in 0..300 {
            let key = i % 60 == 0;
            let mut u = vec![0, 0, 0, 1, 0x09, 0xf0];
            if key {
                for s in H264_SETS {
                    u.extend_from_slice(&[0, 0, 0, 1]);
                    u.extend_from_slice(s);
                }
            }
            let slice = if key { 300_000 } else { 79_000 };
            for _ in 0..4 {
                u.extend_from_slice(&[0, 0, 1, if key { 0x65 } else { 0x41 }]);
                escaped(&mut rng, slice, zero_in, &mut u);
            }
            total += u.len();
            units.push(u);
        }
        let mb = total as f64 / 1e6;
        let best = |f: &mut dyn FnMut(&[u8]) -> usize| {
            let mut t = f64::MAX;
            let mut sink = 0usize;
            for _ in 0..5 {
                let at = Instant::now();
                for u in &units {
                    sink = sink.wrapping_add(f(u));
                }
                t = t.min(at.elapsed().as_secs_f64());
            }
            std::hint::black_box(sink);
            mb / t
        };
        let mut out = Vec::new();
        let old = best(&mut |u| {
            let mut sets = false;
            scalar_nals(u, |a, _| sets |= is_param_set(NalCodec::H264, u[a]));
            if !sets {
                return 0;
            }
            out.clear();
            scalar_nals(u, |a, b| {
                if !is_param_set(NalCodec::H264, u[a]) {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(&u[a..b]);
                }
            });
            out.len()
        });
        let strip = best(&mut |u| {
            out.clear();
            usize::from(strip_param_sets(NalCodec::H264, u, &mut out))
        });
        let per_call = best(&mut |u| {
            let mut n = 0;
            let mut from = 0;
            while let Some(i) = memchr::memmem::find(&u[from..], &[0, 0, 1]) {
                from += i + 3;
                n += 1;
            }
            n
        });
        let mut kept = Vec::new();
        let new = best(&mut |u| scan_unit(NalCodec::H264, u, &mut kept).kept_len);
        println!(
            "{label} {mb:.0} MB: old scalar strip {old:.0} MB/s, strip_param_sets now \
             {strip:.0} MB/s, memmem::find per call {per_call:.0} MB/s, scan_unit {new:.0} MB/s, \
             at 10 MB/s old {:.3} % new {:.3} % of a core",
            1000.0 / old,
            1000.0 / new
        );
    }
}
