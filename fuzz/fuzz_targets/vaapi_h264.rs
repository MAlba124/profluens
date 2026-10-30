//! The pf-vaapi H.264 parser, no display. Input is a knob byte then the bytes. `knob & 3` picks
//! the mode, 0 and 1 an Annex B stream, 2 the bit reader against an Exp-Golomb writer, 3 an SPS,
//! a PPS and a slice NAL each behind a length byte. Parsed values must stay in the spec's ranges,
//! and a slice header must not depend on the bytes past it.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_vaapi::colour;
use pf_vaapi::h264parse::{
    self as p, BitReader, Nal, PocState, Pps, SliceHeader, SliceType, Sps, non_reference,
    parse_pps, parse_slice_header, parse_sps, split_nals, split_nals_into,
};
use profluens_video::color::HdrStatic;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    match knob & 3 {
        0 | 1 => stream(rest),
        2 => bits(rest),
        _ => units(rest),
    }
});

fn stream(au: &[u8]) {
    let nals = split_nals(au);
    check_split(au, &nals);
    assert_eq!(non_reference(&nals), model_non_reference(&nals));
    let (mut sps, mut pps): (Option<Sps>, Option<Pps>) = (None, None);
    let mut poc = PocState::new();
    for n in &nals {
        let raw = n.raw(au);
        match n.unit_type {
            p::NAL_SPS => {
                // A new SPS only activates at an IDR, which resets the POC.
                if let Some(s) = parse_sps(raw).inspect(check_sps) {
                    (sps, poc) = (Some(s), PocState::new());
                }
            }
            p::NAL_PPS => pps = parse_pps(raw).inspect(check_pps).or(pps),
            p::NAL_SEI => {
                let mut hdr = HdrStatic::default();
                let _ = colour::sei_hdr(&de_emulate(&raw[1..]), &mut hdr);
            }
            p::NAL_SLICE_NON_IDR | p::NAL_SLICE_IDR => {
                if let (Some(s), Some(q)) = (&sps, &pps) {
                    slice(raw, n.unit_type, n.ref_idc, s, q, &mut poc);
                }
            }
            _ => {}
        }
    }
}

fn units(b: &[u8]) {
    let mut at = 0;
    let mut take = || {
        let n = usize::from(*b.get(at)?);
        let unit = b.get(at + 1..).map(|r| &r[..n.min(r.len())])?;
        at += 1 + unit.len();
        Some(unit)
    };
    let (Some(s), Some(q)) = (take(), take()) else {
        return;
    };
    let rest = &b[at.min(b.len())..];
    let sps = parse_sps(s).inspect(check_sps);
    let pps = parse_pps(q).inspect(check_pps);
    // Every prefix of a parameter set parses or not, it never panics.
    for cut in [s.len() / 2, s.len().saturating_sub(1)] {
        let _ = parse_sps(&s[..cut]).inspect(check_sps);
    }
    if let (Some(sps), Some(pps), Some(&h)) = (sps, pps, rest.first()) {
        let mut poc = PocState::new();
        slice(rest, h & 0x1f, h >> 5 & 3, &sps, &pps, &mut poc);
        slice(rest, p::NAL_SLICE_NON_IDR, h >> 5 & 3, &sps, &pps, &mut poc);
    }
}

fn check_split(au: &[u8], nals: &[Nal]) {
    let mut reused = vec![Nal { ref_idc: 9, unit_type: 99, start: 1, end: 0 }; 3];
    split_nals_into(au, &mut reused);
    assert_eq!(reused, nals, "split_nals_into kept a stale NAL");
    let mut last = 0usize;
    for n in nals {
        let (s, e) = (n.start as usize, n.end as usize);
        assert!(s >= 3 && s < e && e <= au.len(), "{n:?} outside the unit");
        assert!(s >= last, "NALs overlap or are out of order");
        assert_eq!(au[s - 3..s], [0, 0, 1], "a NAL not behind a start code");
        assert_eq!((n.unit_type, n.ref_idc), (au[s] & 0x1f, au[s] >> 5 & 3));
        assert!(au[s..e].windows(3).all(|w| w != [0, 0, 1]), "a start code inside a NAL");
        last = e;
    }
    let starts = au.windows(3).enumerate().filter(|(i, w)| *w == [0, 0, 1] && i + 3 < au.len());
    assert!(nals.len() <= starts.count());
}

fn model_non_reference(nals: &[Nal]) -> bool {
    let slice = |n: &Nal| n.unit_type == p::NAL_SLICE_NON_IDR && n.ref_idc == 0;
    let filler = |n: &Nal| matches!(n.unit_type, p::NAL_SEI | p::NAL_AUD | p::NAL_FILLER);
    nals.iter().all(|n| slice(n) || filler(n)) && nals.iter().any(slice)
}

fn check_sps(s: &Sps) {
    assert!(s.seq_parameter_set_id <= 31, "sps id {}", s.seq_parameter_set_id);
    assert!(s.chroma_format_idc <= 3);
    assert!(!s.separate_colour_plane_flag || s.chroma_format_idc == 3);
    assert!(s.bit_depth_luma_minus8 <= 6 && s.bit_depth_chroma_minus8 <= 6);
    assert!(s.log2_max_frame_num_minus4 <= 12 && s.log2_max_pic_order_cnt_lsb_minus4 <= 12);
    assert!(s.pic_order_cnt_type <= 2 && s.offsets_for_ref_frame.len() <= 255);
    assert!(s.max_num_ref_frames <= 16);
    let (wm, hm) = (s.width_in_mbs(), s.height_in_mbs());
    assert!((1..=1024).contains(&wm) && (1..=1024).contains(&hm), "{wm}x{hm} MBs");
    assert!(s.frame_mbs_only_flag || hm % 2 == 0);
    let (w, h) = (s.width(), s.height());
    assert!((1..=wm * 16).contains(&w) && (1..=hm * 16).contains(&h), "{w}x{h} of {wm}x{hm}");
    for v in [s.max_dpb_frames(), s.dpb_frames(), s.reorder_frames()] {
        assert!((1..=16).contains(&v));
    }
    assert!(s.dpb_frames() >= s.max_num_ref_frames.max(1));
    assert!(s.max_frame_num().is_power_of_two() && (16..=1 << 16).contains(&s.max_frame_num()));
    assert!(s.max_poc_lsb().is_power_of_two() && (16..=1 << 16).contains(&s.max_poc_lsb()));
    assert!(s.max_num_reorder_frames.is_none_or(|n| n <= 16));
    assert!(s.max_dec_frame_buffering.is_none_or(|n| n <= 16));
}

fn check_pps(q: &Pps) {
    assert!(q.pic_parameter_set_id <= 255 && q.seq_parameter_set_id <= 31);
    assert!(q.num_ref_idx_l0_default_active_minus1 <= 31);
    assert!(q.num_ref_idx_l1_default_active_minus1 <= 31);
    assert!(q.weighted_bipred_idc <= 2);
    assert!((-62..=25).contains(&q.pic_init_qp_minus26));
    assert!((-12..=12).contains(&q.chroma_qp_index_offset));
    assert!((-12..=12).contains(&q.second_chroma_qp_index_offset));
}

fn slice(nal: &[u8], unit_type: u8, ref_idc: u8, sps: &Sps, pps: &Pps, poc: &mut PocState) {
    let got = parse_slice_header(nal, unit_type, ref_idc, sps, pps);
    if say() {
        eprintln!("h264 slice type {unit_type} len {} parsed {}", nal.len(), got.is_some());
    }
    let Some(sh) = got else {
        return;
    };
    check_slice(&sh, sps, pps);
    // Bytes past the header do not change it. 0xFF never completes an emulation prevention byte.
    let rbsp = de_emulate(nal.get(1..).unwrap_or_default());
    if sh.header_bits_in_rbsp <= rbsp.len() * 8 {
        let mut longer = nal.to_vec();
        longer.resize(nal.len() + 1 + (nal.len() & 0x1fff), 0xFF);
        let again = parse_slice_header(&longer, unit_type, ref_idc, sps, pps)
            .expect("a header that parsed refused with more bytes behind it");
        assert_eq!(format!("{again:?}"), format!("{sh:?}"), "bytes past the header changed it");
    }
    let is_idr = unit_type == p::NAL_SLICE_IDR;
    let (top, bottom) = poc.compute(sps, &sh, is_idr, ref_idc);
    match sps.pic_order_cnt_type {
        0 => {
            let m = sps.max_poc_lsb();
            assert_eq!((top as u32).wrapping_sub(sh.pic_order_cnt_lsb) % m, 0, "POC msb off grid");
        }
        2 => {
            assert!(top == bottom && top.rem_euclid(2) == i32::from(ref_idc == 0 && !is_idr));
            if is_idr {
                assert_eq!(top, 0);
            }
        }
        _ => {}
    }
}

fn check_slice(sh: &SliceHeader, sps: &Sps, pps: &Pps) {
    assert_eq!(sh.is_idr, sh.is_idr && sh.idr_pic_id <= 65_535);
    assert!(sh.frame_num < sps.max_frame_num() && sh.pic_order_cnt_lsb < sps.max_poc_lsb());
    assert!(sh.num_ref_idx_l0_active_minus1 <= 31 && sh.num_ref_idx_l1_active_minus1 <= 31);
    let mods = p::REF_LIST_MODS_MAX;
    assert!(sh.ref_list_mods_l0.len() <= mods && sh.ref_list_mods_l1.len() <= mods);
    let mmco = p::MMCO_MAX;
    assert!(sh.mmco_ops.len() <= mmco && sh.mmco_ops.iter().all(|m| (1..=6).contains(&m.op)));
    assert!(sh.ref_list_mods_l0.iter().chain(&sh.ref_list_mods_l1).all(|m| m.op <= 5));
    assert!(sh.cabac_init_idc <= 2 && sh.disable_deblocking_filter_idc <= 2);
    assert!((-6..=6).contains(&sh.slice_alpha_c0_offset_div2));
    assert!((-6..=6).contains(&sh.slice_beta_offset_div2));
    let qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
    let bd = 6 * sps.bit_depth_luma_minus8 as i32;
    assert!((-bd..=51).contains(&qp), "slice QP {qp}");
    assert!(sps.frame_mbs_only_flag || !sh.bottom_field_flag || sh.field_pic_flag);
    if let Some(w) = &sh.pred_weights {
        assert!(w.luma_log2_weight_denom <= 7 && w.chroma_log2_weight_denom <= 7);
        assert!(w.l0.len() <= 32 && w.l1.len() <= 32);
        assert_eq!(w.l0.len(), sh.num_ref_idx_l0_active_minus1 as usize + 1);
        assert!(w.l1.is_empty() || sh.slice_type == SliceType::B);
    }
}

/// A plain 7.4.1 de-emulation, the reference for the parser's own.
fn de_emulate(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut zeros = 0;
    for (i, &b) in body.iter().enumerate() {
        if zeros >= 2 && b == 3 && body.get(i + 1).is_some_and(|&n| n <= 3) {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

#[derive(Debug, Clone, Copy)]
enum Op {
    U(u32, u32),
    Flag(bool),
    Ue(u32),
    Se(i32),
}

/// Ops from the input, written by a reference Exp-Golomb writer, must read back the same. Then
/// the same ops over the raw bytes must not panic and must advance the position as coded.
fn bits(b: &[u8]) {
    use Op::*;
    let mut ops = Vec::new();
    for c in b.chunks(5) {
        let mut v = [0u8; 4];
        v[..c.len() - 1].copy_from_slice(&c[1..]);
        let v = u32::from_le_bytes(v);
        ops.push(match c[0] & 3 {
            0 => {
                let n = u32::from(c[0] >> 2) % 33;
                U(n, if n == 32 { v } else { v & ((1 << n) - 1) })
            }
            1 => Flag(v & 1 != 0),
            2 => Ue(v.min(u32::MAX - 1)),
            _ => Se((v as i32).clamp(-i32::MAX, i32::MAX)),
        });
    }
    let mut w = Writer::default();
    for &op in &ops {
        match op {
            U(n, v) => w.put(u64::from(v), n),
            Flag(f) => w.put(u64::from(f), 1),
            Ue(v) => w.ue(v),
            Se(v) => w.ue(if v > 0 { 2 * v as u32 - 1 } else { 2 * v.unsigned_abs() }),
        }
    }
    let total = w.bits;
    let bytes = w.finish();
    let mut r = BitReader::new(&bytes);
    for &op in &ops {
        match op {
            U(n, v) => assert_eq!(r.u(n), v, "u({n})"),
            Flag(f) => assert_eq!(r.flag(), f),
            Ue(v) => assert_eq!(r.ue(), v, "ue"),
            Se(v) => assert_eq!(r.se(), v, "se"),
        }
    }
    assert_eq!(r.bit_pos(), total);
    let last_one = (0..b.len() * 8).rev().find(|&i| b[i >> 3] >> (7 - (i & 7)) & 1 == 1);
    let mut r = BitReader::new(b);
    for &op in &ops {
        let at = r.bit_pos();
        let more = r.more_rbsp_data();
        assert_eq!(more, last_one.is_some_and(|l| at < l));
        let step = match op {
            U(n, _) => {
                let v = r.u(n);
                assert!(n == 32 || v < 1 << n);
                n as usize
            }
            Flag(_) => {
                r.flag();
                1
            }
            Ue(_) => {
                let v = r.ue();
                let used = r.bit_pos() - at;
                assert!(v == 0 || used == 2 * (v + 1).ilog2() as usize + 1, "ue {v} in {used}");
                used
            }
            Se(_) => {
                let v = r.se();
                assert!(v != i32::MIN);
                r.bit_pos() - at
            }
        };
        assert!(step <= 64 && r.bit_pos() == at + step);
    }
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    bits: usize,
}

impl Writer {
    fn put(&mut self, v: u64, n: u32) {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.out.push(0);
            }
            let bit = (v >> i & 1) as u8;
            *self.out.last_mut().expect("pushed") |= bit << (7 - self.bits % 8);
            self.bits += 1;
        }
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.put(0, len - 1);
        self.put(x, len);
    }

    fn finish(self) -> Vec<u8> {
        self.out
    }
}

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}
