//! The pf-vaapi H.265 parser, no display. Input is a knob byte then the bytes. `knob & 3` picks
//! the mode, 0 and 1 an Annex B stream, 2 the tile, scan and log helpers, 3 an SPS, a PPS and a
//! slice NAL each behind a `u16` LE length. Parsed values must stay in the spec's ranges, and a
//! slice header must not depend on the bytes past it.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_vaapi::colour;
use pf_vaapi::h264parse::BitReader;
use pf_vaapi::h265parse::{
    self as p, Nal, PocState, Pps, ScalingLists, ShortTermRps, SliceHeader, Sps, ceil_log2,
    diag_scan, is_idr, is_irap, non_reference, parse_pps, parse_slice_header, parse_sps,
    split_nals, split_nals_into, tile_sizes_minus1, to_raster,
};
use profluens_video::color::HdrStatic;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    match knob & 3 {
        0 | 1 => stream(rest, knob >> 2 & 7),
        2 => helpers(rest),
        _ => units(rest),
    }
});

fn stream(au: &[u8], top: u8) {
    let nals = split_nals(au);
    check_split(au, &nals);
    assert_eq!(non_reference(&nals, top), model_non_reference(&nals, top));
    let mut sps: Vec<Option<Sps>> = vec![None; 16];
    let mut pps: Vec<Option<Pps>> = vec![None; 64];
    let mut poc = PocState::new();
    let mut active = None;
    for n in &nals {
        let raw = n.raw(au);
        match n.nal_type {
            p::NAL_SPS => {
                if let Some(s) = parse_sps(raw).inspect(check_sps) {
                    let id = s.sps_id as usize;
                    sps[id] = Some(s);
                    poc.reset();
                }
            }
            p::NAL_PPS => {
                if let Some(q) = parse_pps(raw).inspect(check_pps) {
                    let id = q.pps_id as usize;
                    pps[id] = Some(q);
                }
            }
            p::NAL_PREFIX_SEI | p::NAL_SUFFIX_SEI => {
                let mut hdr = HdrStatic::default();
                let _ = colour::sei_hdr(&de_emulate(&raw[2..]), &mut hdr);
            }
            t if p::is_slice_nal(t) => {
                let Some(q) = peek_pps_id(raw, is_irap(t)).and_then(|id| pps.get(id)?.as_ref())
                else {
                    continue;
                };
                if let Some(s) = sps[q.sps_id as usize].as_ref() {
                    // Another SPS activates at an IRAP, which starts the POC over.
                    if active.replace(q.sps_id) != Some(q.sps_id) {
                        poc.reset();
                    }
                    slice(raw, t, n.temporal_id, s, q, &mut poc);
                }
            }
            _ => {}
        }
    }
}

fn units(b: &[u8]) {
    let mut at = 0;
    let mut take = || {
        let n = usize::from(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]));
        let unit = b.get(at + 2..).map(|r| &r[..n.min(r.len())])?;
        at += 2 + unit.len();
        Some(unit)
    };
    let (Some(s), Some(q)) = (take(), take()) else {
        return;
    };
    let rest = &b[at.min(b.len())..];
    let sps = parse_sps(s).inspect(check_sps);
    let pps = parse_pps(q).inspect(check_pps);
    for cut in [s.len() / 2, s.len().saturating_sub(1)] {
        let _ = parse_sps(&s[..cut]).inspect(check_sps);
    }
    let (Some(sps), Some(pps), Some(&h)) = (sps, pps, rest.first()) else {
        return;
    };
    let mut poc = PocState::new();
    let t = h >> 1 & 0x3f;
    for t in [t, p::NAL_TRAIL_R, p::NAL_CRA_NUT] {
        slice(rest, t, 0, &sps, &pps, &mut poc);
    }
}

fn peek_pps_id(nal: &[u8], irap: bool) -> Option<usize> {
    let rbsp = de_emulate(nal.get(2..)?);
    let mut r = BitReader::new(&rbsp);
    r.flag();
    if irap {
        r.flag();
    }
    Some(r.ue() as usize)
}

fn check_split(au: &[u8], nals: &[Nal]) {
    let stale = Nal { nal_type: 99, temporal_id: 9, start: 1, end: 0 };
    let mut reused = vec![stale; 3];
    split_nals_into(au, &mut reused);
    assert_eq!(reused, nals, "split_nals_into kept a stale NAL");
    let mut last = 0usize;
    for n in nals {
        let (s, e) = (n.start as usize, n.end as usize);
        assert!(s >= 3 && s + 2 <= e && e <= au.len(), "{n:?} outside the unit");
        assert!(s >= last, "NALs overlap or are out of order");
        assert_eq!(au[s - 3..s], [0, 0, 1], "a NAL not behind a start code");
        assert_eq!(n.nal_type, au[s] >> 1 & 0x3f);
        assert_eq!(n.temporal_id, (au[s + 1] & 7).wrapping_sub(1));
        assert_eq!(p::layer_id([au[s], au[s + 1]]), 0, "a NAL past the base layer");
        assert!(au[s..e].windows(3).all(|w| w != [0, 0, 1]), "a start code inside a NAL");
        last = e;
    }
}

fn model_non_reference(nals: &[Nal], top: u8) -> bool {
    let skip = [p::NAL_AUD, p::NAL_FD, p::NAL_PREFIX_SEI, p::NAL_SUFFIX_SEI];
    let slice = |n: &Nal| n.nal_type <= 14 && n.nal_type.is_multiple_of(2) && n.temporal_id == top;
    nals.iter().all(|n| slice(n) || skip.contains(&n.nal_type)) && nals.iter().any(slice)
}

fn check_sps(s: &Sps) {
    assert!(s.sps_id < 16 && s.chroma_format_idc <= 3 && s.max_sub_layers_minus1 <= 7);
    assert!(!s.separate_colour_plane_flag || s.chroma_format_idc == 3);
    assert!(s.bit_depth_luma_minus8 <= 8 && s.bit_depth_chroma_minus8 <= 8);
    let (cw, ch) = (s.coded_width(), s.coded_height());
    assert!((1..=16384).contains(&cw) && (1..=16384).contains(&ch), "{cw}x{ch}");
    let (w, h) = (s.width(), s.height());
    let (x, y) = s.crop_origin();
    assert!(w >= 1 && h >= 1 && x + w <= cw && y + h <= ch, "{w}x{h} at {x},{y} of {cw}x{ch}");
    assert!((4..=6).contains(&s.ctb_log2_size_y()));
    let ctbs = s.pic_width_in_ctbs() * s.pic_height_in_ctbs();
    assert!((1..=(16384 / 16) * (16384 / 16)).contains(&ctbs));
    assert!((1..=16).contains(&s.ref_frames()) && (1..=16).contains(&s.reorder_frames()));
    let m = s.max_poc_lsb() as u32;
    assert!(m.is_power_of_two() && (16..=1 << 16).contains(&m));
    assert_eq!(s.st_rps.len(), s.num_short_term_ref_pic_sets as usize);
    assert!(s.num_short_term_ref_pic_sets <= 64);
    s.st_rps.iter().for_each(check_rps);
    let lt = s.num_long_term_ref_pics_sps as usize;
    assert!(lt <= 32 && s.lt_ref_pic_poc_lsb_sps.len() == lt);
    assert_eq!(s.used_by_curr_pic_lt_sps_flag.len(), lt);
    assert!(s.lt_ref_pic_poc_lsb_sps.iter().all(|&l| l < s.max_poc_lsb() as u32));
    if let Some(l) = &s.scaling_lists {
        check_lists(l);
    }
    let _ = (s.supported(), s.is_ten_bit());
}

fn check_rps(r: &ShortTermRps) {
    assert!(r.delta_poc_s0.len() + r.delta_poc_s1.len() <= 16, "{r:?}");
    assert_eq!(r.used_s0.len(), r.delta_poc_s0.len());
    assert_eq!(r.used_s1.len(), r.delta_poc_s1.len());
    assert!(r.delta_poc_s0.iter().all(|&d| d < 0) && r.delta_poc_s1.iter().all(|&d| d > 0));
    let before = r.curr_before();
    let after = r.curr_after();
    assert_eq!(before.len(), r.used_s0.iter().filter(|u| **u).count());
    assert_eq!(after.len(), r.used_s1.iter().filter(|u| **u).count());
}

fn check_lists(l: &ScalingLists) {
    let all = l.list4x4.iter().flatten().chain(l.list8x8.iter().flatten());
    let all = all.chain(l.list16x16.iter().flatten()).chain(l.list32x32.iter().flatten());
    assert!(all.chain(&l.dc16x16).chain(&l.dc32x32).all(|&v| v > 0), "a zero scaling factor");
    for list in &l.list4x4 {
        let mut a = to_raster(list).to_vec();
        let mut b = list.to_vec();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "to_raster is not a permutation");
    }
}

fn check_pps(q: &Pps) {
    assert!(q.pps_id < 64 && q.sps_id < 16 && q.num_extra_slice_header_bits <= 7);
    assert!(q.num_ref_idx_l0_default_active_minus1 <= 14);
    assert!(q.num_ref_idx_l1_default_active_minus1 <= 14);
    assert!((-74..=25).contains(&q.init_qp_minus26) && q.diff_cu_qp_delta_depth <= 3);
    assert!((-12..=12).contains(&q.pps_cb_qp_offset) && (-12..=12).contains(&q.pps_cr_qp_offset));
    assert!((-6..=6).contains(&q.pps_beta_offset_div2) && (-6..=6).contains(&q.pps_tc_offset_div2));
    assert!(q.log2_parallel_merge_level_minus2 <= 4);
    assert!(q.num_tile_columns_minus1 < p::TILE_COLUMNS_MAX);
    assert!(q.num_tile_rows_minus1 < p::TILE_ROWS_MAX);
    if q.tiles_enabled_flag && !q.uniform_spacing_flag {
        assert_eq!(q.column_width_minus1.len(), q.num_tile_columns_minus1 as usize);
        assert_eq!(q.row_height_minus1.len(), q.num_tile_rows_minus1 as usize);
    }
    if let Some(l) = &q.scaling_lists {
        check_lists(l);
    }
}

/// The fields a header is compared by.
fn key(sh: &SliceHeader) -> [i64; 14] {
    let lt = sh.long_term_refs.len();
    let mods = sh.ref_list_mods.l0.len() + sh.ref_list_mods.l1.len();
    [
        sh.pps_id.into(),
        sh.slice_segment_address.into(),
        sh.slice_type.va_value().into(),
        sh.pic_order_cnt_lsb.into(),
        sh.st_rps_bits.into(),
        sh.collocated_ref_idx.into(),
        sh.slice_qp_delta.into(),
        sh.slice_cb_qp_offset.into(),
        sh.slice_beta_offset_div2.into(),
        sh.num_entry_point_offsets.into(),
        sh.slice_data_byte_offset.into(),
        sh.num_emu_prev_bytes.into(),
        lt as i64,
        mods as i64,
    ]
}

fn slice(nal: &[u8], t: u8, temporal_id: u8, sps: &Sps, pps: &Pps, poc: &mut PocState) {
    let got = parse_slice_header(nal, t, sps, pps);
    if say() {
        eprintln!("h265 slice type {t} len {} parsed {}", nal.len(), got.is_some());
    }
    let Some(sh) = got else {
        return;
    };
    check_slice(&sh, nal, sps, pps);
    // Bytes past the header do not change it. 0xFF never completes an emulation prevention byte.
    let mut longer = nal.to_vec();
    longer.resize(nal.len() + 1 + (nal.len() & 0x1fff), 0xFF);
    let again = parse_slice_header(&longer, t, sps, pps)
        .expect("a header that parsed refused with more bytes behind it");
    assert_eq!(key(&again), key(&sh), "bytes past the header changed it");
    let got = poc.compute(sps, &sh, t, is_irap(t), temporal_id);
    if is_idr(t) {
        assert_eq!(got, 0);
    } else {
        let m = sps.max_poc_lsb() as u32;
        assert_eq!((got as u32).wrapping_sub(sh.pic_order_cnt_lsb) % m, 0, "POC msb off grid");
    }
}

fn check_slice(sh: &SliceHeader, nal: &[u8], sps: &Sps, pps: &Pps) {
    assert_eq!(sh.pps_id, pps.pps_id);
    assert!(sh.slice_segment_address < sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs());
    assert!(sh.pic_order_cnt_lsb < sps.max_poc_lsb() as u32);
    check_rps(&sh.st_rps);
    assert!(sh.long_term_refs.len() <= 32);
    let (l0, l1) = (sh.num_ref_idx_l0_active_minus1, sh.num_ref_idx_l1_active_minus1);
    assert!(l0 <= 14 && l1 <= 14);
    let mods = &sh.ref_list_mods;
    assert!(mods.l0.len() <= l0 as usize + 1 && mods.l1.len() <= l1 as usize + 1);
    assert!(sh.five_minus_max_num_merge_cand <= 4);
    let active = if sh.collocated_from_l0_flag { l0 } else { l1 };
    assert!(sh.collocated_ref_idx <= active);
    let qp = 26 + pps.init_qp_minus26 + sh.slice_qp_delta;
    assert!((-6 * sps.bit_depth_luma_minus8 as i32..=51).contains(&qp), "slice QP {qp}");
    for (s, q) in [
        (sh.slice_cb_qp_offset, pps.pps_cb_qp_offset),
        (sh.slice_cr_qp_offset, pps.pps_cr_qp_offset),
    ] {
        assert!((-12..=12).contains(&s) && (-12..=12).contains(&(s + q)));
    }
    assert!((-6..=6).contains(&sh.slice_beta_offset_div2));
    assert!((-6..=6).contains(&sh.slice_tc_offset_div2));
    let off = sh.slice_data_byte_offset as usize;
    assert!(off >= 3 && off <= nal.len(), "slice data at {off} of {}", nal.len());
    assert!(usize::from(sh.num_emu_prev_bytes) <= nal.len());
    if let Some(w) = &sh.pred_weights {
        let chroma = w.luma_log2_weight_denom as i32 + w.delta_chroma_log2_weight_denom;
        assert!(w.luma_log2_weight_denom <= 7 && (0..=7).contains(&chroma));
        assert_eq!(w.l0.len(), l0 as usize + 1);
    }
    let (cols, rows) = (pps.num_tile_columns_minus1 + 1, pps.num_tile_rows_minus1 + 1);
    let ctb_rows = sps.pic_height_in_ctbs();
    let most = match (pps.tiles_enabled_flag, pps.entropy_coding_sync_enabled_flag) {
        (true, true) => cols * ctb_rows,
        (true, false) => cols * rows,
        (false, true) => ctb_rows,
        (false, false) => 1,
    };
    assert!(sh.num_entry_point_offsets < most, "{} entry points", sh.num_entry_point_offsets);
}

/// A plain 7.3.1.1 de-emulation, the reference for the parser's own.
fn de_emulate(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut zeros = 0;
    for (i, &b) in body.iter().enumerate() {
        if zeros >= 2 && b == 3 && body.get(i + 1).is_none_or(|&n| n <= 3) {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn helpers(b: &[u8]) {
    let mut c = [0u8; 8];
    let n = b.len().min(8);
    c[..n].copy_from_slice(&b[..n]);
    let rest = &b[n..];

    // Tiles as the decoder asks for them into VA's 19 widths or 21 heights, up to 20 columns or
    // 22 rows of a 1024 CTB picture. A full grid leaves its last tile to the rest.
    let uniform = c[0] & 1 != 0;
    let (len, most) = if c[0] & 2 == 0 { (19, p::TILE_COLUMNS_MAX) } else { (21, p::TILE_ROWS_MAX) };
    let count = u32::from(c[1]) % most + 1;
    let ctbs = (u32::from(u16::from_le_bytes([c[2], c[3]])) % 1024 + 1).max(count);
    let coded: Vec<u32> = rest.chunks(2).map(|w| u32::from(w[0]) % 64).collect();
    let mut out = [0xAAAAu16; 21];
    tile_sizes_minus1(uniform, &coded, count, ctbs, &mut out[..len]);
    let n = (count as usize).min(len);
    let whole = n == count as usize;
    assert!(out[n..].iter().all(|&v| v == 0xAAAA), "a tile past the count written");
    let sizes: Vec<u32> = out[..n].iter().map(|&v| u32::from(v) + 1).collect();
    if uniform {
        let (lo, hi) = (ctbs / count, ctbs.div_ceil(count));
        assert!(sizes.iter().all(|s| (lo..=hi).contains(s)), "{sizes:?} of {ctbs}");
        let left = ctbs.checked_sub(sizes.iter().sum()).expect("uniform tiles past the picture");
        assert!(if whole { left == 0 } else { (lo..=hi).contains(&left) }, "{sizes:?} of {ctbs}");
    } else {
        let explicit = if whole { n - 1 } else { n };
        for (i, s) in sizes[..explicit].iter().enumerate() {
            assert_eq!(*s, coded.get(i).map_or(1, |c| c + 1));
        }
        if whole {
            let used: u32 = sizes[..n - 1].iter().sum();
            assert_eq!(sizes[n - 1], ctbs.saturating_sub(used).max(1));
        }
    }

    // The diagonal scan is a permutation that walks anti-diagonals.
    for size in [1usize, 2, 4, 8] {
        let mut scan = vec![usize::MAX; size * size];
        diag_scan(size, &mut scan);
        let mut seen = vec![false; size * size];
        let mut last_diag = 0;
        for &r in &scan {
            assert!(r < size * size && !seen[r], "scan {size} {scan:?}");
            seen[r] = true;
            let diag = r % size + r / size;
            assert!(diag >= last_diag, "scan {size} steps back a diagonal");
            last_diag = diag;
        }
    }
    let mut l16 = [0u8; 16];
    let mut l64 = [0u8; 64];
    let (n16, n64) = (rest.len().min(16), rest.len().min(64));
    l16[..n16].copy_from_slice(&rest[..n16]);
    l64[..n64].copy_from_slice(&rest[..n64]);
    let (r16, r64) = (to_raster(&l16), to_raster(&l64));
    assert_eq!((r16[0], r64[0]), (l16[0], l64[0]), "the DC moved");
    assert_eq!((r16[15], r64[63]), (l16[15], l64[63]), "the last coefficient moved");
    let mut s = [r16.to_vec(), l16.to_vec(), r64.to_vec(), l64.to_vec()];
    s.iter_mut().for_each(|v| v.sort_unstable());
    assert!(s[0] == s[1] && s[2] == s[3], "to_raster is not a permutation");

    let v = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
    let r = ceil_log2(v);
    if v > 1 {
        assert!(u64::from(v) <= 1 << r && u64::from(v) > 1 << (r - 1), "ceil_log2({v}) = {r}");
    } else {
        assert_eq!(r, 0);
    }
}

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}
