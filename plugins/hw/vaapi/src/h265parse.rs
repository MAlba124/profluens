//! H.265 Annex-B parsing for VA-API slice-level decode. VPS/SPS/PPS, RPS, slice segment headers
//! and POC (H.265 7.3, 8.3.1). Main and Main 10 4:2:0 progressive only.

#![allow(clippy::needless_range_loop)]

use profluens_video::fixed::FixedList;

use crate::h264parse::BitReader;

// NAL unit types (Table 7-1). The header is 2 bytes.
pub const NAL_TRAIL_N: u8 = 0;
pub const NAL_TRAIL_R: u8 = 1;
pub const NAL_TSA_N: u8 = 2;
pub const NAL_TSA_R: u8 = 3;
pub const NAL_STSA_N: u8 = 4;
pub const NAL_STSA_R: u8 = 5;
pub const NAL_RADL_N: u8 = 6;
pub const NAL_RADL_R: u8 = 7;
pub const NAL_RASL_N: u8 = 8;
pub const NAL_RASL_R: u8 = 9;
pub const NAL_BLA_W_LP: u8 = 16;
pub const NAL_BLA_W_RADL: u8 = 17;
pub const NAL_BLA_N_LP: u8 = 18;
pub const NAL_IDR_W_RADL: u8 = 19;
pub const NAL_IDR_N_LP: u8 = 20;
pub const NAL_CRA_NUT: u8 = 21;
pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;
pub const NAL_EOS: u8 = 36;
pub const NAL_EOB: u8 = 37;
pub const NAL_FD: u8 = 38;
pub const NAL_PREFIX_SEI: u8 = 39;
pub const NAL_SUFFIX_SEI: u8 = 40;

pub fn is_slice_nal(nal_type: u8) -> bool {
    nal_type <= 31
}

/// Whether the unit is a sub-layer non-reference picture of the highest sub-layer only (7.4.2.2),
/// so it can be skipped before the presentation point.
pub fn non_reference(nals: &[Nal], top_sub_layer: u8) -> bool {
    let mut slices = 0u32;
    for nal in nals {
        match nal.nal_type {
            NAL_AUD | NAL_FD | NAL_PREFIX_SEI | NAL_SUFFIX_SEI => {}
            0..=15 if nal.nal_type % 2 == 0 && nal.temporal_id == top_sub_layer => slices += 1,
            _ => return false,
        }
    }
    slices > 0
}

pub fn is_irap(nal_type: u8) -> bool {
    (NAL_BLA_W_LP..=23).contains(&nal_type)
}

pub fn is_idr(nal_type: u8) -> bool {
    nal_type == NAL_IDR_W_RADL || nal_type == NAL_IDR_N_LP
}

pub fn is_bla(nal_type: u8) -> bool {
    (NAL_BLA_W_LP..=NAL_BLA_N_LP).contains(&nal_type)
}

pub fn is_rasl(nal_type: u8) -> bool {
    nal_type == NAL_RASL_N || nal_type == NAL_RASL_R
}

/// A parsed NAL (7.3.1.2) as offsets into the access unit, header and emulation bytes included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nal {
    pub nal_type: u8,
    pub temporal_id: u8,
    pub start: u32,
    pub end: u32,
}

impl Nal {
    #[inline]
    pub fn raw<'a>(&self, au: &'a [u8]) -> &'a [u8] {
        &au[self.start as usize..self.end as usize]
    }
}

/// Header parse window. A header past it is parsed again from the whole NAL.
const HEADER_PREFIX: usize = 2048;

/// Split an Annex-B access unit into its NALs (B.2.2). The trailing 0x00 of a 4-byte start code is
/// trimmed.
// Cold, tests and one-off callers.
#[allow(clippy::disallowed_methods)]
pub fn split_nals(au: &[u8]) -> Vec<Nal> {
    let mut out = Vec::new();
    split_nals_into(au, &mut out);
    out
}

/// [`split_nals`] into `out`, cleared first, which a decoder keeps across units. One pass.
pub fn split_nals_into(au: &[u8], out: &mut Vec<Nal>) {
    out.clear();
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if let Some(s) = open {
                let end = if i > s && au[i - 1] == 0 { i - 1 } else { i };
                push_nal(au, s, end, out);
            }
            open = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(s) = open {
        push_nal(au, s, au.len(), out);
    }
}

fn push_nal(au: &[u8], start: usize, end: usize, out: &mut Vec<Nal>) {
    if end < start + 2 {
        return;
    }
    let nal_type = (au[start] >> 1) & 0x3f;
    let temporal_id = (au[start + 1] & 0x07).wrapping_sub(1);
    out.push(Nal { nal_type, temporal_id, start: start as u32, end: end as u32 });
}

/// De-emulate an RBSP past the 2-byte header (7.3.1.1), with the count of bytes removed.
// Parameter sets and the cold slice header fallback, once per stream or never.
#[allow(clippy::disallowed_methods)]
fn rbsp_of(nal: &[u8]) -> (Vec<u8>, u16) {
    let body = &nal[2.min(nal.len())..];
    let mut out = vec![0; body.len()];
    let n = crate::h264parse::de_emulate(body, &mut out);
    out.truncate(n);
    (out, emulation_bytes(body))
}

fn emulation_bytes(body: &[u8]) -> u16 {
    emulation_bytes_before(body, usize::MAX)
}

fn emulation_bytes_before(body: &[u8], at: usize) -> u16 {
    let (mut emu, mut zeros, mut kept, mut idx) = (0u16, 0, 0usize, 0);
    while idx < body.len() && kept <= at {
        let b = body[idx];
        if zeros >= 2 && b == 0x03 && idx + 1 < body.len() && body[idx + 1] <= 0x03 {
            emu = emu.saturating_add(1);
            zeros = 0;
            idx += 1;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        kept += 1;
        idx += 1;
    }
    emu
}

/// Tile column widths or row heights in CTBs minus one (6.5.1).
pub fn tile_sizes_minus1(uniform: bool, coded: &[u32], count: u32, ctbs: u32, out: &mut [u16]) {
    let n = (count as usize).min(out.len());
    let mut used = 0u32;
    for (i, slot) in out.iter_mut().enumerate().take(n) {
        let size = if uniform {
            ((i as u32 + 1) * ctbs) / count - (i as u32 * ctbs) / count
        } else if i + 1 < n {
            coded.get(i).map_or(1, |m| m.saturating_add(1))
        } else {
            ctbs.saturating_sub(used).max(1)
        };
        used = used.saturating_add(size);
        *slot = size.saturating_sub(1).min(u32::from(u16::MAX)) as u16;
    }
}

/// Short-term reference picture set (7.4.8), signed POC deltas and their used_by_curr flags.
/// Inter-RPS prediction yields one past [`MAX_RPS`] a side before the set is checked.
pub const RPS_SIDE_MAX: usize = MAX_RPS + 1;

#[derive(Clone, Debug, Default)]
pub struct ShortTermRps {
    pub delta_poc_s0: FixedList<i32, RPS_SIDE_MAX>,
    pub used_s0: FixedList<bool, RPS_SIDE_MAX>,
    pub delta_poc_s1: FixedList<i32, RPS_SIDE_MAX>,
    pub used_s1: FixedList<bool, RPS_SIDE_MAX>,
}

impl ShortTermRps {
    fn num_negative(&self) -> usize {
        self.delta_poc_s0.len()
    }
    fn num_positive(&self) -> usize {
        self.delta_poc_s1.len()
    }
}

/// Parsed SPS (7.3.2.2.1). Unsupported syntax is reported by [`Sps::supported`].
#[derive(Clone, Debug)]
pub struct Sps {
    pub sps_id: u32,
    pub max_sub_layers_minus1: u8,
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub pic_width_in_luma_samples: u32,
    pub pic_height_in_luma_samples: u32,
    /// Conformance-window offsets (7.4.3.2.1) in chroma sample units.
    pub conf_win_left_offset: u32,
    pub conf_win_right_offset: u32,
    pub conf_win_top_offset: u32,
    pub conf_win_bottom_offset: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    /// DPB size minus one and reorder depth of the highest sub-layer (7.4.3.2.1).
    pub sps_max_dec_pic_buffering_minus1: u32,
    pub sps_max_num_reorder_pics: u32,
    pub log2_min_luma_coding_block_size_minus3: u32,
    pub log2_diff_max_min_luma_coding_block_size: u32,
    pub log2_min_transform_block_size_minus2: u32,
    pub log2_diff_max_min_transform_block_size: u32,
    pub max_transform_hierarchy_depth_inter: u32,
    pub max_transform_hierarchy_depth_intra: u32,
    pub scaling_list_enabled_flag: bool,
    pub scaling_lists: Option<ScalingLists>,
    pub amp_enabled_flag: bool,
    pub sample_adaptive_offset_enabled_flag: bool,
    pub pcm_enabled_flag: bool,
    pub pcm_sample_bit_depth_luma_minus1: u32,
    pub pcm_sample_bit_depth_chroma_minus1: u32,
    pub log2_min_pcm_luma_coding_block_size_minus3: u32,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u32,
    pub pcm_loop_filter_disabled_flag: bool,
    pub num_short_term_ref_pic_sets: u32,
    pub st_rps: Vec<ShortTermRps>,
    pub long_term_ref_pics_present_flag: bool,
    pub num_long_term_ref_pics_sps: u32,
    pub lt_ref_pic_poc_lsb_sps: Vec<u32>,
    pub used_by_curr_pic_lt_sps_flag: Vec<bool>,
    pub sps_temporal_mvp_enabled_flag: bool,
    pub strong_intra_smoothing_enabled_flag: bool,
    pub has_extension: bool,
    /// The sps_range_extension flags in VA's order (`range_extension_pic_fields` bits 0 to 8),
    /// 0 without one.
    pub range_ext: u32,
    /// A format range extensions profile (A.3.5), decoded as VA's Main 12.
    pub rext: bool,
    pub colour: profluens_video::decoder::Color,
}

/// `range_ext` bits iHD decodes none of, extended_precision_processing_flag and
/// cabac_bypass_alignment_enabled_flag.
const RANGE_EXT_REFUSED: u32 = 1 << 4 | 1 << 8;
/// `range_ext`'s high_precision_offsets_enabled_flag, it widens the weighted prediction offsets.
pub const RANGE_EXT_HIGH_PRECISION_OFFSETS: u32 = 1 << 6;

impl Sps {
    pub fn ref_frames(&self) -> u32 {
        self.sps_max_dec_pic_buffering_minus1.clamp(1, 16)
    }
    /// `sps_max_num_reorder_pics` of the highest sub-layer, at least one so readback overlaps
    /// decode.
    pub fn reorder_frames(&self) -> u32 {
        self.sps_max_num_reorder_pics.min(self.sps_max_dec_pic_buffering_minus1).clamp(1, 16)
    }
    pub fn ctb_log2_size_y(&self) -> u32 {
        self.log2_min_luma_coding_block_size_minus3
            + 3
            + self.log2_diff_max_min_luma_coding_block_size
    }
    fn ctb_size_y(&self) -> u32 {
        1 << self.ctb_log2_size_y()
    }
    pub fn pic_width_in_ctbs(&self) -> u32 {
        self.pic_width_in_luma_samples.div_ceil(self.ctb_size_y())
    }
    pub fn pic_height_in_ctbs(&self) -> u32 {
        self.pic_height_in_luma_samples.div_ceil(self.ctb_size_y())
    }
    pub fn max_poc_lsb(&self) -> i32 {
        1 << (self.log2_max_pic_order_cnt_lsb_minus4 + 4)
    }
    /// SubWidthC and SubHeightC (Table 6-1).
    fn sub_wh_c(&self) -> (u32, u32) {
        match self.chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        }
    }
    /// Conformance-cropped display width (7.4.3.2.1), luma samples.
    pub fn display_width(&self) -> u32 {
        let (sub_w, _) = self.sub_wh_c();
        let crop = sub_w * (self.conf_win_left_offset + self.conf_win_right_offset);
        self.pic_width_in_luma_samples.saturating_sub(crop)
    }
    /// Conformance-cropped display height (7.4.3.2.1), luma samples.
    pub fn display_height(&self) -> u32 {
        let (_, sub_h) = self.sub_wh_c();
        let crop = sub_h * (self.conf_win_top_offset + self.conf_win_bottom_offset);
        self.pic_height_in_luma_samples.saturating_sub(crop)
    }
    /// Top-left of the display picture in the coded surface, luma samples.
    pub fn crop_origin(&self) -> (u32, u32) {
        let (sub_w, sub_h) = self.sub_wh_c();
        (sub_w * self.conf_win_left_offset, sub_h * self.conf_win_top_offset)
    }
    pub fn width(&self) -> u32 {
        self.display_width()
    }
    pub fn height(&self) -> u32 {
        self.display_height()
    }
    /// Coded (CTB-multiple) width before cropping, luma samples.
    pub fn coded_width(&self) -> u32 {
        self.pic_width_in_luma_samples
    }
    pub fn coded_height(&self) -> u32 {
        self.pic_height_in_luma_samples
    }
    /// 4:2:0 at 8, 10 or 12 bits, Main, Main 10 or a range extensions profile without the
    /// tools iHD lacks, no multilayer, 3D or SCC extension (A.3.2, A.3.3, A.3.5).
    pub fn supported(&self) -> bool {
        self.chroma_format_idc == 1
            && !self.separate_colour_plane_flag
            && matches!(self.bit_depth_luma_minus8, 0 | 2 | 4)
            && self.bit_depth_chroma_minus8 == self.bit_depth_luma_minus8
            && !self.has_extension
            && self.range_ext & RANGE_EXT_REFUSED == 0
    }

    pub fn is_ten_bit(&self) -> bool {
        self.bit_depth_luma_minus8 == 2
    }

    /// Bits a luma sample.
    pub fn depth(&self) -> u8 {
        (self.bit_depth_luma_minus8 + 8).min(16) as u8
    }
}

/// Parsed PPS (7.3.2.3.1).
#[derive(Clone, Debug)]
pub struct Pps {
    pub pps_id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled_flag: bool,
    pub output_flag_present_flag: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding_enabled_flag: bool,
    pub cabac_init_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub init_qp_minus26: i32,
    pub constrained_intra_pred_flag: bool,
    pub transform_skip_enabled_flag: bool,
    pub cu_qp_delta_enabled_flag: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub pps_cb_qp_offset: i32,
    pub pps_cr_qp_offset: i32,
    pub pps_slice_chroma_qp_offsets_present_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_flag: bool,
    pub transquant_bypass_enabled_flag: bool,
    pub tiles_enabled_flag: bool,
    pub entropy_coding_sync_enabled_flag: bool,
    pub num_tile_columns_minus1: u32,
    pub num_tile_rows_minus1: u32,
    pub uniform_spacing_flag: bool,
    pub column_width_minus1: Vec<u32>,
    pub row_height_minus1: Vec<u32>,
    pub loop_filter_across_tiles_enabled_flag: bool,
    pub pps_loop_filter_across_slices_enabled_flag: bool,
    pub deblocking_filter_control_present_flag: bool,
    pub deblocking_filter_override_enabled_flag: bool,
    pub pps_deblocking_filter_disabled_flag: bool,
    pub pps_beta_offset_div2: i32,
    pub pps_tc_offset_div2: i32,
    pub pps_scaling_list_data_present_flag: bool,
    pub scaling_lists: Option<ScalingLists>,
    pub lists_modification_present_flag: bool,
    pub log2_parallel_merge_level_minus2: u32,
    pub slice_segment_header_extension_present_flag: bool,
    pub range: PpsRange,
    /// A multilayer, 3D or SCC extension, decoded by none of these.
    pub other_ext: bool,
}

/// pps_range_extension() (7.3.2.3.2), zeros without one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PpsRange {
    pub log2_max_transform_skip_block_size_minus2: u32,
    pub cross_component_prediction_enabled_flag: bool,
    pub chroma_qp_offset_list_enabled_flag: bool,
    pub diff_cu_chroma_qp_offset_depth: u32,
    pub chroma_qp_offset_list_len_minus1: u32,
    pub cb_qp_offset_list: [i8; 6],
    pub cr_qp_offset_list: [i8; 6],
    pub log2_sao_offset_scale_luma: u32,
    pub log2_sao_offset_scale_chroma: u32,
}

/// The PPS extensions after slice_segment_header_extension_present_flag (7.3.2.3.1), the range
/// one parsed, whether another is present. None out of range (7.4.3.3.2).
fn pps_extensions(r: &mut BitReader, transform_skip: bool) -> Option<(PpsRange, bool)> {
    let mut p = PpsRange::default();
    if !r.flag() {
        // pps_extension_present_flag
        return Some((p, false));
    }
    let range = r.flag();
    let others = r.u(3) != 0; // multilayer, 3d, scc
    let _four = r.u(4); // pps_extension_4bits
    if range {
        if transform_skip {
            p.log2_max_transform_skip_block_size_minus2 = r.ue();
        }
        p.cross_component_prediction_enabled_flag = r.flag();
        p.chroma_qp_offset_list_enabled_flag = r.flag();
        if p.chroma_qp_offset_list_enabled_flag {
            p.diff_cu_chroma_qp_offset_depth = r.ue();
            p.chroma_qp_offset_list_len_minus1 = r.ue();
            if p.chroma_qp_offset_list_len_minus1 > 5 {
                return None;
            }
            for i in 0..=p.chroma_qp_offset_list_len_minus1 as usize {
                let (cb, cr) = (r.se(), r.se());
                if !(-12..=12).contains(&cb) || !(-12..=12).contains(&cr) {
                    return None;
                }
                (p.cb_qp_offset_list[i], p.cr_qp_offset_list[i]) = (cb as i8, cr as i8);
            }
        }
        p.log2_sao_offset_scale_luma = r.ue();
        p.log2_sao_offset_scale_chroma = r.ue();
        if p.log2_max_transform_skip_block_size_minus2 > 3 || p.log2_sao_offset_scale_luma > 6 || p.log2_sao_offset_scale_chroma > 6 {
            return None;
        }
    }
    Some((p, others))
}

/// Slice type (7.4.7.1). The values VA's LongSliceFlags.slice_type carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SliceType {
    B,
    P,
    I,
}

impl SliceType {
    fn from_u32(v: u32) -> Option<SliceType> {
        match v {
            0 => Some(SliceType::B),
            1 => Some(SliceType::P),
            2 => Some(SliceType::I),
            _ => None,
        }
    }
    pub fn va_value(self) -> u32 {
        match self {
            SliceType::B => 0,
            SliceType::P => 1,
            SliceType::I => 2,
        }
    }
    pub fn is_inter(self) -> bool {
        matches!(self, SliceType::P | SliceType::B)
    }
}

/// Explicit weight entry (7.3.6.3). HEVC codes deltas and VA takes them verbatim.
#[derive(Clone, Copy, Default)]
pub struct WeightEntry {
    pub luma_flag: bool,
    pub delta_luma_weight: i32,
    pub luma_offset: i32,
    pub chroma_flag: bool,
    pub delta_chroma_weight: [i32; 2],
    pub chroma_offset: [i32; 2],
}

/// Entries a reference list holds at most, `num_ref_idx_lX_active_minus1` is at most 14 (7.4.7.1).
pub const REFS_MAX: usize = 15;
/// Long-term entries a slice header carries at most, the bound the parser checks.
pub const LONG_TERM_MAX: usize = 32;

#[derive(Clone, Default)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u32,
    pub delta_chroma_log2_weight_denom: i32,
    pub l0: FixedList<WeightEntry, REFS_MAX>,
    pub l1: FixedList<WeightEntry, REFS_MAX>,
}

#[derive(Clone, Copy, Default)]
pub struct LongTermRef {
    pub poc_lsb: u32,
    pub used_by_curr_pic: bool,
    pub delta_poc_msb_present: bool,
    pub delta_poc_msb_cycle: u32,
}

#[derive(Clone, Default)]
pub struct RefListMods {
    pub l0_present: bool,
    pub l0: FixedList<u32, REFS_MAX>,
    pub l1_present: bool,
    pub l1: FixedList<u32, REFS_MAX>,
}

/// Parsed slice segment header (7.3.6.1).
#[derive(Clone)]
pub struct SliceHeader {
    pub first_slice_segment_in_pic_flag: bool,
    pub no_output_of_prior_pics_flag: bool,
    pub pps_id: u32,
    pub dependent_slice_segment_flag: bool,
    pub slice_segment_address: u32,
    pub slice_type: SliceType,
    pub pic_output_flag: bool,
    pub pic_order_cnt_lsb: u32,
    pub st_rps: ShortTermRps,
    /// Bits of the in-slice `short_term_ref_pic_set()`, 0 if from the SPS (VA `st_rps_bits`).
    pub st_rps_bits: u32,
    pub long_term_refs: FixedList<LongTermRef, LONG_TERM_MAX>,
    pub slice_temporal_mvp_enabled_flag: bool,
    pub slice_sao_luma_flag: bool,
    pub slice_sao_chroma_flag: bool,
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    pub ref_list_mods: RefListMods,
    pub mvd_l1_zero_flag: bool,
    pub cabac_init_flag: bool,
    pub collocated_from_l0_flag: bool,
    pub collocated_ref_idx: u32,
    pub pred_weights: Option<PredWeightTable>,
    pub five_minus_max_num_merge_cand: u32,
    pub slice_qp_delta: i32,
    pub slice_cb_qp_offset: i32,
    pub slice_cr_qp_offset: i32,
    /// Read when the PPS's chroma QP offset list is on (7.3.6.1).
    pub cu_chroma_qp_offset_enabled_flag: bool,
    pub deblocking_filter_override_flag: bool,
    pub slice_deblocking_filter_disabled_flag: bool,
    pub slice_beta_offset_div2: i32,
    pub slice_tc_offset_div2: i32,
    pub slice_loop_filter_across_slices_enabled_flag: bool,
    pub num_entry_point_offsets: u32,
    /// slice_data() offset in the de-emulated NAL, header included (VA `slice_data_byte_offset`).
    pub slice_data_byte_offset: u32,
    /// Emulation bytes ahead of `slice_data_byte_offset` (VA `slice_data_num_emu_prevn_bytes`).
    pub num_emu_prev_bytes: u16,
}

/// Skip `profile_tier_level` (7.3.3). True for a Main-family profile.
/// Whether the general profile is Main or Main 10, and whether it is a format range extensions
/// one (4), by its idc or its compatibility flags.
fn skip_profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: u32) -> (bool, bool) {
    let _profile_space = r.u(2);
    let _tier = r.flag();
    let general_profile_idc = r.u(5);
    let (mut compat_main, mut compat_rext) = (false, false);
    for j in 0..32 {
        let f = r.flag();
        if j == 1 || j == 2 {
            compat_main |= f; // Main (1) or Main10 (2) compatibility
        }
        compat_rext |= j == 4 && f;
    }
    let _progressive = r.flag();
    let _interlaced = r.flag();
    let _non_packed = r.flag();
    let _frame_only = r.flag();
    r.u(32);
    r.u(12);
    let _level = r.u(8);

    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        if i < 8 {
            sub_profile[i] = r.flag();
            sub_level[i] = r.flag();
        }
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.u(2);
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if i < 8 && sub_profile[i] {
            // 88 bits (7.3.3): space, tier, idc, 32 compatibility flags, 4 source flags, 44
            // reserved.
            r.u(2); // sub_layer_profile_space
            r.flag(); // sub_layer_tier_flag
            r.u(5); // sub_layer_profile_idc
            r.u(32); // sub_layer_profile_compatibility_flag
            r.u(32);
            r.u(16);
        }
        if i < 8 && sub_level[i] {
            r.u(8); // sub_layer_level_idc
        }
    }
    (general_profile_idc == 1 || general_profile_idc == 2 || compat_main, general_profile_idc == 4 || compat_rext)
}

/// The VUI past the chroma location, where [`crate::colour::vui_signal`] leaves the reader, to
/// the end (E.2.1).
fn skip_vui_rest(r: &mut BitReader, max_sub_layers_minus1: u32) {
    r.flag(); // neutral_chroma_indication_flag
    r.flag(); // field_seq_flag
    r.flag(); // frame_field_info_present_flag
    if r.flag() {
        // default_display_window_flag, left, right, top, bottom offsets
        for _ in 0..4 {
            r.ue();
        }
    }
    if r.flag() {
        // vui_timing_info_present_flag
        r.u(32); // vui_num_units_in_tick
        r.u(32); // vui_time_scale
        if r.flag() {
            r.ue(); // vui_num_ticks_poc_diff_one_minus1
        }
        if r.flag() {
            skip_hrd(r, max_sub_layers_minus1);
        }
    }
    if r.flag() {
        // bitstream_restriction_flag
        r.u(3); // tiles_fixed_structure, motion_vectors_over_pic_boundaries, restricted_ref_pic_lists
        for _ in 0..5 {
            r.ue(); // min_spatial_segmentation_idc to log2_max_mv_length_vertical
        }
    }
}

/// hrd_parameters(1, max_sub_layers_minus1) (E.2.2).
fn skip_hrd(r: &mut BitReader, max_sub_layers_minus1: u32) {
    let nal = r.flag();
    let vcl = r.flag();
    let mut sub_pic = false;
    if nal || vcl {
        sub_pic = r.flag();
        if sub_pic {
            r.u(8 + 5 + 1 + 5); // tick_divisor_minus2 to dpb_output_delay_du_length_minus1
        }
        r.u(4 + 4); // bit_rate_scale, cpb_size_scale
        if sub_pic {
            r.u(4); // cpb_size_du_scale
        }
        r.u(5 + 5 + 5); // the three delay lengths
    }
    for _ in 0..=max_sub_layers_minus1.min(7) {
        let general = r.flag(); // fixed_pic_rate_general_flag
        let within = general || r.flag(); // fixed_pic_rate_within_cvs_flag
        let low_delay = if within {
            r.ue(); // elemental_duration_in_tc_minus1
            false
        } else {
            r.flag()
        };
        let cpb_cnt = if low_delay { 0 } else { r.ue().min(31) };
        for _ in 0..u32::from(nal) + u32::from(vcl) {
            // sub_layer_hrd_parameters (E.2.3)
            for _ in 0..=cpb_cnt {
                r.ue(); // bit_rate_value_minus1
                r.ue(); // cpb_size_value_minus1
                if sub_pic {
                    r.ue(); // cpb_size_du_value_minus1
                    r.ue(); // bit_rate_du_value_minus1
                }
                r.flag(); // cbr_flag
            }
        }
    }
}

/// The sps_range_extension flags in VA's order, and whether any extension other than the range
/// one is present (7.3.2.2.1, 7.3.2.2.2).
fn sps_extensions(r: &mut BitReader) -> (u32, bool) {
    if !r.flag() {
        // sps_extension_present_flag
        return (0, false);
    }
    let range = r.flag();
    // multilayer, 3d, scc, then sps_extension_4bits whose data ends the RBSP
    let others = r.u(3) != 0 || r.u(4) != 0;
    let mut flags = 0;
    if range {
        for i in 0..9 {
            flags |= u32::from(r.flag()) << i;
        }
    }
    (flags, others)
}

const DEFAULT_INTRA: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19, 20,
    21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31,
    29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];
const DEFAULT_INTER: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20, 20,
    20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28,
    28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

/// Scaling lists (7.3.4, 7.4.5) in up-right diagonal order. 16x16 and 32x32 as 8x8 plus DC.
/// The 32x32 lists are matrixId 0 and 3, held at 0 and 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScalingLists {
    pub list4x4: [[u8; 16]; 6],
    pub list8x8: [[u8; 64]; 6],
    pub list16x16: [[u8; 64]; 6],
    pub list32x32: [[u8; 64]; 2],
    pub dc16x16: [u8; 6],
    pub dc32x32: [u8; 2],
}

impl ScalingLists {
    /// Tables 7-5 and 7-6, used when lists are enabled and none are sent.
    pub fn default_lists() -> Self {
        ScalingLists {
            list4x4: [[16; 16]; 6],
            list8x8: [
                DEFAULT_INTRA,
                DEFAULT_INTRA,
                DEFAULT_INTRA,
                DEFAULT_INTER,
                DEFAULT_INTER,
                DEFAULT_INTER,
            ],
            list16x16: [
                DEFAULT_INTRA,
                DEFAULT_INTRA,
                DEFAULT_INTRA,
                DEFAULT_INTER,
                DEFAULT_INTER,
                DEFAULT_INTER,
            ],
            list32x32: [DEFAULT_INTRA, DEFAULT_INTER],
            dc16x16: [16; 6],
            dc32x32: [16; 2],
        }
    }

    fn default_8x8(matrix_id: usize) -> [u8; 64] {
        if matrix_id < 3 { DEFAULT_INTRA } else { DEFAULT_INTER }
    }

    fn get(&self, size_id: usize, matrix_id: usize) -> ([u8; 64], u8) {
        match size_id {
            0 => {
                let mut l = [0; 64];
                l[..16].copy_from_slice(&self.list4x4[matrix_id]);
                (l, 16)
            }
            1 => (self.list8x8[matrix_id], 16),
            2 => (self.list16x16[matrix_id], self.dc16x16[matrix_id]),
            _ => (self.list32x32[matrix_id / 3], self.dc32x32[matrix_id / 3]),
        }
    }

    fn set(&mut self, size_id: usize, matrix_id: usize, list: &[u8; 64], dc: u8) {
        match size_id {
            0 => self.list4x4[matrix_id].copy_from_slice(&list[..16]),
            1 => self.list8x8[matrix_id] = *list,
            2 => {
                self.list16x16[matrix_id] = *list;
                self.dc16x16[matrix_id] = dc;
            }
            _ => {
                self.list32x32[matrix_id / 3] = *list;
                self.dc32x32[matrix_id / 3] = dc;
            }
        }
    }
}

/// Parse `scaling_list_data()` (7.3.4). `None` for a value out of its 7.4.5 range.
fn parse_scaling_list_data(r: &mut BitReader) -> Option<ScalingLists> {
    let mut out = ScalingLists::default_lists();
    for size_id in 0..4usize {
        let step = if size_id == 3 { 3 } else { 1 };
        let coef_num = 64usize.min(1 << (4 + (size_id << 1)));
        let mut matrix_id = 0usize;
        while matrix_id < 6 {
            let pred_mode = r.flag(); // scaling_list_pred_mode_flag
            if !pred_mode {
                let delta = r.ue() as usize; // scaling_list_pred_matrix_id_delta
                if delta > matrix_id / step {
                    return None;
                }
                let delta = delta * step;
                let (list, dc) = if delta == 0 {
                    (if size_id == 0 { [16; 64] } else { ScalingLists::default_8x8(matrix_id) }, 16)
                } else {
                    out.get(size_id, matrix_id.saturating_sub(delta))
                };
                out.set(size_id, matrix_id, &list, dc);
            } else {
                let mut next = 8i32;
                let mut dc = 16u8;
                if size_id > 1 {
                    let dc_minus8 = r.se(); // scaling_list_dc_coef_minus8
                    if !(-7..=247).contains(&dc_minus8) {
                        return None;
                    }
                    next = dc_minus8 + 8;
                    dc = next as u8;
                }
                let mut list = [0u8; 64];
                for v in list.iter_mut().take(coef_num) {
                    let delta = r.se(); // scaling_list_delta_coef
                    if !(-128..=127).contains(&delta) {
                        return None;
                    }
                    next = (next + delta + 256).rem_euclid(256);
                    // ScalingFactor is never 0 (7.4.5).
                    if next == 0 {
                        return None;
                    }
                    *v = next as u8;
                }
                out.set(size_id, matrix_id, &list, dc);
            }
            matrix_id += step;
        }
    }
    Some(out)
}

/// Up-right diagonal scan of a `size` square block (6.5.3), raster index per coded position.
pub fn diag_scan(size: usize, out: &mut [usize]) {
    let (mut i, mut x, mut y) = (0usize, 0usize, 0usize);
    while i < size * size {
        let mut yy = y as isize;
        while yy >= 0 {
            if x < size && (yy as usize) < size {
                out[i] = yy as usize * size + x;
                i += 1;
            }
            yy -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
}

/// A coded list in raster order, as `VAIQMatrixBufferHEVC` takes it.
pub fn to_raster<const N: usize>(coded: &[u8; N]) -> [u8; N] {
    let size = if N == 16 { 4 } else { 8 };
    let mut scan = [0usize; 64];
    diag_scan(size, &mut scan[..N]);
    let mut out = [0u8; N];
    for (i, &v) in coded.iter().enumerate() {
        out[scan[i]] = v;
    }
    out
}

/// Most pictures an RPS holds, sps_max_dec_pic_buffering_minus1 at most 15 (7.4.8).
const MAX_RPS: usize = 16;
/// delta_poc_s0_minus1, delta_poc_s1_minus1 and abs_delta_rps_minus1 are at most 2^15 - 1.
const MAX_DELTA_MINUS1: u32 = (1 << 15) - 1;

/// Parse `short_term_ref_pic_set(stRpsIdx)` (7.3.7), resolving inter-RPS prediction. `None` for a
/// value out of its 7.4.8 range or a set past [`MAX_RPS`] pictures.
fn parse_short_term_rps(
    r: &mut BitReader,
    st_rps_idx: u32,
    num_st_rps: u32,
    rps_list: &[ShortTermRps],
) -> Option<ShortTermRps> {
    let mut inter_pred = false;
    if st_rps_idx != 0 {
        inter_pred = r.flag(); // inter_ref_pic_set_prediction_flag
    }
    if inter_pred {
        // Inter-RPS prediction (7.4.8).
        let delta_idx_minus1 = if st_rps_idx == num_st_rps { r.ue() } else { 0 };
        if delta_idx_minus1 >= st_rps_idx {
            return None;
        }
        let ref_idx = st_rps_idx as i64 - (delta_idx_minus1 as i64 + 1);
        let delta_rps_sign = r.flag();
        let abs_delta_rps_minus1 = r.ue();
        if abs_delta_rps_minus1 > MAX_DELTA_MINUS1 {
            return None;
        }
        let delta_rps = (1 - 2 * delta_rps_sign as i32) * (abs_delta_rps_minus1 as i32 + 1);

        let empty = ShortTermRps::default();
        let ref_rps = rps_list.get(ref_idx.clamp(0, i64::MAX) as usize).unwrap_or(&empty);
        let num_ref = ref_rps.num_negative() + ref_rps.num_positive();

        let mut ref_deltas = [0i32; 33];
        let mut ref_used = [false; 33];
        let mut rd_len = 0usize;
        for j in (0..ref_rps.num_negative()).rev() {
            if rd_len < ref_deltas.len() {
                ref_deltas[rd_len] = ref_rps.delta_poc_s0[j];
                ref_used[rd_len] = ref_rps.used_s0[j];
                rd_len += 1;
            }
        }
        for j in 0..ref_rps.num_positive() {
            if rd_len < ref_deltas.len() {
                ref_deltas[rd_len] = ref_rps.delta_poc_s1[j];
                ref_used[rd_len] = ref_rps.used_s1[j];
                rd_len += 1;
            }
        }

        // Writes clamp to capacity so a corrupt count cannot index out of bounds.
        let mut used_by_curr = [false; 33];
        let mut use_delta = [true; 33];
        for j in 0..=num_ref {
            let u = r.flag();
            if j < used_by_curr.len() {
                used_by_curr[j] = u;
            }
            if !u {
                let d = r.flag();
                if j < use_delta.len() {
                    use_delta[j] = d;
                }
            }
        }

        let mut out = ShortTermRps::default();
        // S1 side, eq. 7-59 and 7-60.
        {
            let mut i = 0i32;
            let mut j = ref_rps.num_positive() as i32 - 1;
            while j >= 0 {
                let d_poc = ref_rps.delta_poc_s1[j as usize] + delta_rps;
                let k = ref_rps.num_negative() + j as usize;
                if d_poc < 0 && use_delta[k] {
                    out.delta_poc_s0.push(d_poc);
                    out.used_s0.push(used_by_curr[k]);
                    i += 1;
                }
                j -= 1;
            }
            let _ = i;
            if delta_rps < 0 && use_delta[num_ref] {
                out.delta_poc_s0.push(delta_rps);
                out.used_s0.push(used_by_curr[num_ref]);
            }
            for j in 0..ref_rps.num_negative() {
                let d_poc = ref_rps.delta_poc_s0[j] + delta_rps;
                if d_poc < 0 && use_delta[j] {
                    out.delta_poc_s0.push(d_poc);
                    out.used_s0.push(used_by_curr[j]);
                }
            }
        }
        {
            let mut j = ref_rps.num_negative() as i32 - 1;
            while j >= 0 {
                let d_poc = ref_rps.delta_poc_s0[j as usize] + delta_rps;
                if d_poc > 0 && use_delta[j as usize] {
                    out.delta_poc_s1.push(d_poc);
                    out.used_s1.push(used_by_curr[j as usize]);
                }
                j -= 1;
            }
            if delta_rps > 0 && use_delta[num_ref] {
                out.delta_poc_s1.push(delta_rps);
                out.used_s1.push(used_by_curr[num_ref]);
            }
            for j in 0..ref_rps.num_positive() {
                let d_poc = ref_rps.delta_poc_s1[j] + delta_rps;
                let k = ref_rps.num_negative() + j;
                if d_poc > 0 && use_delta[k] {
                    out.delta_poc_s1.push(d_poc);
                    out.used_s1.push(used_by_curr[k]);
                }
            }
        }
        let _ = ref_deltas;
        let _ = ref_used;
        let _ = rd_len;
        (out.num_negative() + out.num_positive() <= MAX_RPS).then_some(out)
    } else {
        let num_negative = r.ue();
        let num_positive = r.ue();
        if u64::from(num_negative) + u64::from(num_positive) > MAX_RPS as u64 {
            return None;
        }
        let mut out = ShortTermRps::default();
        let mut prev = 0i32;
        for _ in 0..num_negative {
            let delta_poc_s0_minus1 = r.ue();
            let used = r.flag();
            if delta_poc_s0_minus1 > MAX_DELTA_MINUS1 {
                return None;
            }
            prev -= delta_poc_s0_minus1 as i32 + 1;
            out.delta_poc_s0.push(prev);
            out.used_s0.push(used);
        }
        let mut prev = 0i32;
        for _ in 0..num_positive {
            let delta_poc_s1_minus1 = r.ue();
            let used = r.flag();
            if delta_poc_s1_minus1 > MAX_DELTA_MINUS1 {
                return None;
            }
            prev += delta_poc_s1_minus1 as i32 + 1;
            out.delta_poc_s1.push(prev);
            out.used_s1.push(used);
        }
        Some(out)
    }
}

// Once per parameter set, not per frame.
#[allow(clippy::disallowed_methods)]
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    if nal.len() < 3 {
        return None;
    }
    let (rbsp, _emu) = rbsp_of(nal);
    let mut r = BitReader::new(&rbsp);

    let _sps_video_parameter_set_id = r.u(4);
    let sps_max_sub_layers_minus1 = r.u(3);
    let _sps_temporal_id_nesting_flag = r.flag();
    let (ok_profile, rext) = skip_profile_tier_level(&mut r, sps_max_sub_layers_minus1);

    let sps_id = r.ue();
    if sps_id >= 16 {
        return None;
    }
    let chroma_format_idc = r.ue();
    if chroma_format_idc > 3 {
        return None;
    }
    let separate_colour_plane_flag = if chroma_format_idc == 3 { r.flag() } else { false };
    let pic_width_in_luma_samples = r.ue();
    let pic_height_in_luma_samples = r.ue();
    // Bound dimensions so corrupt input cannot request an absurd surface.
    if pic_width_in_luma_samples == 0
        || pic_height_in_luma_samples == 0
        || pic_width_in_luma_samples > 16384
        || pic_height_in_luma_samples > 16384
    {
        return None;
    }
    let conformance_window_flag = r.flag();
    let (mut conf_win_left_offset, mut conf_win_right_offset) = (0, 0);
    let (mut conf_win_top_offset, mut conf_win_bottom_offset) = (0, 0);
    if conformance_window_flag {
        conf_win_left_offset = r.ue();
        conf_win_right_offset = r.ue();
        conf_win_top_offset = r.ue();
        conf_win_bottom_offset = r.ue();
    }
    let bit_depth_luma_minus8 = r.ue();
    let bit_depth_chroma_minus8 = r.ue();
    let log2_max_pic_order_cnt_lsb_minus4 = r.ue();
    if log2_max_pic_order_cnt_lsb_minus4 > 12 || bit_depth_luma_minus8 > 8 {
        return None;
    }
    if bit_depth_chroma_minus8 > 8 {
        return None;
    }
    // SubWidthC and SubHeightC (Table 6-1), the window leaves at least one sample.
    let (sub_w, sub_h) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let crop_w = sub_w * (u64::from(conf_win_left_offset) + u64::from(conf_win_right_offset));
    let crop_h = sub_h * (u64::from(conf_win_top_offset) + u64::from(conf_win_bottom_offset));
    if crop_w >= u64::from(pic_width_in_luma_samples)
        || crop_h >= u64::from(pic_height_in_luma_samples)
    {
        return None;
    }

    let sps_sub_layer_ordering_info_present_flag = r.flag();
    let start =
        if sps_sub_layer_ordering_info_present_flag { 0 } else { sps_max_sub_layers_minus1 };
    let mut sps_max_dec_pic_buffering_minus1 = 0;
    let mut sps_max_num_reorder_pics = 0;
    for _ in start..=sps_max_sub_layers_minus1 {
        sps_max_dec_pic_buffering_minus1 = r.ue();
        sps_max_num_reorder_pics = r.ue();
        r.ue(); // sps_max_latency_increase_plus1
    }

    if sps_max_dec_pic_buffering_minus1 > 15
        || sps_max_num_reorder_pics > sps_max_dec_pic_buffering_minus1
    {
        return None;
    }

    let log2_min_luma_coding_block_size_minus3 = r.ue();
    let log2_diff_max_min_luma_coding_block_size = r.ue();
    let log2_min_transform_block_size_minus2 = r.ue();
    let log2_diff_max_min_transform_block_size = r.ue();
    let max_transform_hierarchy_depth_inter = r.ue();
    let max_transform_hierarchy_depth_intra = r.ue();
    // CtbLog2SizeY 4 to 6, MinTbLog2SizeY below MinCbLog2SizeY, MaxTbLog2SizeY at most 5 and the
    // CTB, and the picture a whole number of minimum CBs (7.4.3.2.1).
    let min_cb = u64::from(log2_min_luma_coding_block_size_minus3) + 3;
    let ctb = min_cb + u64::from(log2_diff_max_min_luma_coding_block_size);
    let min_tb = u64::from(log2_min_transform_block_size_minus2) + 2;
    let max_tb = min_tb + u64::from(log2_diff_max_min_transform_block_size);
    let depth =
        u64::from(max_transform_hierarchy_depth_inter.max(max_transform_hierarchy_depth_intra));
    if !(4..=6).contains(&ctb) || min_tb >= min_cb || max_tb > ctb.min(5) || depth > ctb - min_tb {
        return None;
    }
    let min_cb_mask = (1u32 << min_cb) - 1;
    if pic_width_in_luma_samples & min_cb_mask != 0 || pic_height_in_luma_samples & min_cb_mask != 0
    {
        return None;
    }

    let scaling_list_enabled_flag = r.flag();
    let mut scaling_lists = None;
    if scaling_list_enabled_flag {
        let sps_scaling_list_data_present_flag = r.flag();
        if sps_scaling_list_data_present_flag {
            scaling_lists = Some(parse_scaling_list_data(&mut r)?);
        }
    }
    let amp_enabled_flag = r.flag();
    let sample_adaptive_offset_enabled_flag = r.flag();
    let pcm_enabled_flag = r.flag();
    let mut pcm_sample_bit_depth_luma_minus1 = 0;
    let mut pcm_sample_bit_depth_chroma_minus1 = 0;
    let mut log2_min_pcm_luma_coding_block_size_minus3 = 0;
    let mut log2_diff_max_min_pcm_luma_coding_block_size = 0;
    let mut pcm_loop_filter_disabled_flag = false;
    if pcm_enabled_flag {
        pcm_sample_bit_depth_luma_minus1 = r.u(4);
        pcm_sample_bit_depth_chroma_minus1 = r.u(4);
        log2_min_pcm_luma_coding_block_size_minus3 = r.ue();
        log2_diff_max_min_pcm_luma_coding_block_size = r.ue();
        pcm_loop_filter_disabled_flag = r.flag();
        let min_pcm = u64::from(log2_min_pcm_luma_coding_block_size_minus3) + 3;
        let max_pcm = min_pcm + u64::from(log2_diff_max_min_pcm_luma_coding_block_size);
        let in_range = pcm_sample_bit_depth_luma_minus1 <= bit_depth_luma_minus8 + 7
            && pcm_sample_bit_depth_chroma_minus1 <= bit_depth_chroma_minus8 + 7
            && min_pcm >= min_cb.min(5)
            && max_pcm <= ctb.min(5);
        if !in_range {
            return None;
        }
    }

    let num_short_term_ref_pic_sets = r.ue();
    if num_short_term_ref_pic_sets > 64 {
        return None;
    }
    let mut st_rps: Vec<ShortTermRps> = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
    for i in 0..num_short_term_ref_pic_sets {
        let rps = parse_short_term_rps(&mut r, i, num_short_term_ref_pic_sets, &st_rps)?;
        st_rps.push(rps);
    }

    let long_term_ref_pics_present_flag = r.flag();
    let mut num_long_term_ref_pics_sps = 0;
    let mut lt_ref_pic_poc_lsb_sps = Vec::new();
    let mut used_by_curr_pic_lt_sps_flag = Vec::new();
    if long_term_ref_pics_present_flag {
        num_long_term_ref_pics_sps = r.ue().min(32);
        let lsb_bits = log2_max_pic_order_cnt_lsb_minus4 + 4;
        for _ in 0..num_long_term_ref_pics_sps {
            lt_ref_pic_poc_lsb_sps.push(r.u(lsb_bits));
            used_by_curr_pic_lt_sps_flag.push(r.flag());
        }
    }

    let sps_temporal_mvp_enabled_flag = r.flag();
    let strong_intra_smoothing_enabled_flag = r.flag();
    let vui_parameters_present_flag = r.flag();
    let colour = match vui_parameters_present_flag {
        true => crate::colour::vui_signal(&mut r),
        false => Default::default(),
    };
    // Main and Main 10 refuse no extension as before. A range extensions profile is taken only
    // when the VUI and the extension flags read within the RBSP and name the range one alone.
    let (has_extension, range_ext) = match (ok_profile, rext) {
        (true, _) => (false, 0),
        (false, true) => {
            if vui_parameters_present_flag {
                skip_vui_rest(&mut r, sps_max_sub_layers_minus1);
            }
            let (flags, others) = sps_extensions(&mut r);
            // the stop bit right after, else the walk went wrong
            let within = others || (r.flag() && r.bit_pos() <= rbsp.len() * 8);
            (others || !within, if within { flags } else { 0 })
        }
        (false, false) => (true, 0),
    };

    Some(Sps {
        sps_id,
        max_sub_layers_minus1: sps_max_sub_layers_minus1 as u8,
        chroma_format_idc,
        separate_colour_plane_flag,
        pic_width_in_luma_samples,
        pic_height_in_luma_samples,
        conf_win_left_offset,
        conf_win_right_offset,
        conf_win_top_offset,
        conf_win_bottom_offset,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb_minus4,
        sps_max_dec_pic_buffering_minus1,
        sps_max_num_reorder_pics,
        log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size,
        max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra,
        scaling_list_enabled_flag,
        scaling_lists,
        amp_enabled_flag,
        sample_adaptive_offset_enabled_flag,
        pcm_enabled_flag,
        pcm_sample_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1,
        log2_min_pcm_luma_coding_block_size_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size,
        pcm_loop_filter_disabled_flag,
        num_short_term_ref_pic_sets,
        st_rps,
        long_term_ref_pics_present_flag,
        num_long_term_ref_pics_sps,
        lt_ref_pic_poc_lsb_sps,
        used_by_curr_pic_lt_sps_flag,
        sps_temporal_mvp_enabled_flag,
        strong_intra_smoothing_enabled_flag,
        has_extension,
        range_ext,
        rext: rext && !ok_profile,
        colour,
    })
}

// Once per parameter set, not per frame.
#[allow(clippy::disallowed_methods)]
pub fn parse_pps(nal: &[u8]) -> Option<Pps> {
    if nal.len() < 3 {
        return None;
    }
    let (rbsp, _emu) = rbsp_of(nal);
    let mut r = BitReader::new(&rbsp);

    let pps_id = r.ue();
    let sps_id = r.ue();
    if pps_id >= 64 || sps_id >= 16 {
        return None;
    }
    let dependent_slice_segments_enabled_flag = r.flag();
    let output_flag_present_flag = r.flag();
    let num_extra_slice_header_bits = r.u(3);
    let sign_data_hiding_enabled_flag = r.flag();
    let cabac_init_present_flag = r.flag();
    let num_ref_idx_l0_default_active_minus1 = r.ue();
    let num_ref_idx_l1_default_active_minus1 = r.ue();
    let init_qp_minus26 = r.se();
    let constrained_intra_pred_flag = r.flag();
    let transform_skip_enabled_flag = r.flag();
    let cu_qp_delta_enabled_flag = r.flag();
    let diff_cu_qp_delta_depth = if cu_qp_delta_enabled_flag { r.ue() } else { 0 };
    let pps_cb_qp_offset = r.se();
    let pps_cr_qp_offset = r.se();
    let pps_slice_chroma_qp_offsets_present_flag = r.flag();
    // 7.4.3.3.1, QpBdOffsetY at most 48 and the CTB at most 3 CB depths past the minimum.
    let in_range = num_ref_idx_l0_default_active_minus1 <= 14
        && num_ref_idx_l1_default_active_minus1 <= 14
        && (-74..=25).contains(&init_qp_minus26)
        && diff_cu_qp_delta_depth <= 3
        && (-12..=12).contains(&pps_cb_qp_offset)
        && (-12..=12).contains(&pps_cr_qp_offset);
    if !in_range {
        return None;
    }
    let weighted_pred_flag = r.flag();
    let weighted_bipred_flag = r.flag();
    let transquant_bypass_enabled_flag = r.flag();
    let tiles_enabled_flag = r.flag();
    let entropy_coding_sync_enabled_flag = r.flag();

    let mut num_tile_columns_minus1 = 0;
    let mut num_tile_rows_minus1 = 0;
    let mut uniform_spacing_flag = true;
    let mut column_width_minus1 = Vec::new();
    let mut row_height_minus1 = Vec::new();
    let mut loop_filter_across_tiles_enabled_flag = true;
    if tiles_enabled_flag {
        num_tile_columns_minus1 = r.ue().min(18);
        num_tile_rows_minus1 = r.ue().min(20);
        uniform_spacing_flag = r.flag();
        if !uniform_spacing_flag {
            for _ in 0..num_tile_columns_minus1 {
                column_width_minus1.push(r.ue());
            }
            for _ in 0..num_tile_rows_minus1 {
                row_height_minus1.push(r.ue());
            }
        }
        loop_filter_across_tiles_enabled_flag = r.flag();
    }
    let pps_loop_filter_across_slices_enabled_flag = r.flag();
    let deblocking_filter_control_present_flag = r.flag();
    let mut deblocking_filter_override_enabled_flag = false;
    let mut pps_deblocking_filter_disabled_flag = false;
    let mut pps_beta_offset_div2 = 0;
    let mut pps_tc_offset_div2 = 0;
    if deblocking_filter_control_present_flag {
        deblocking_filter_override_enabled_flag = r.flag();
        pps_deblocking_filter_disabled_flag = r.flag();
        if !pps_deblocking_filter_disabled_flag {
            pps_beta_offset_div2 = r.se();
            pps_tc_offset_div2 = r.se();
        }
        if !(-6..=6).contains(&pps_beta_offset_div2) || !(-6..=6).contains(&pps_tc_offset_div2) {
            return None;
        }
    }
    let pps_scaling_list_data_present_flag = r.flag();
    let scaling_lists = if pps_scaling_list_data_present_flag {
        Some(parse_scaling_list_data(&mut r)?)
    } else {
        None
    };
    let lists_modification_present_flag = r.flag();
    let log2_parallel_merge_level_minus2 = r.ue();
    if log2_parallel_merge_level_minus2 > 4 {
        return None;
    }
    let slice_segment_header_extension_present_flag = r.flag();
    let (range, other_ext) = pps_extensions(&mut r, transform_skip_enabled_flag)?;

    Some(Pps {
        range,
        other_ext,
        pps_id,
        sps_id,
        dependent_slice_segments_enabled_flag,
        output_flag_present_flag,
        num_extra_slice_header_bits,
        sign_data_hiding_enabled_flag,
        cabac_init_present_flag,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        init_qp_minus26,
        constrained_intra_pred_flag,
        transform_skip_enabled_flag,
        cu_qp_delta_enabled_flag,
        diff_cu_qp_delta_depth,
        pps_cb_qp_offset,
        pps_cr_qp_offset,
        pps_slice_chroma_qp_offsets_present_flag,
        weighted_pred_flag,
        weighted_bipred_flag,
        transquant_bypass_enabled_flag,
        tiles_enabled_flag,
        entropy_coding_sync_enabled_flag,
        num_tile_columns_minus1,
        num_tile_rows_minus1,
        uniform_spacing_flag,
        column_width_minus1,
        row_height_minus1,
        loop_filter_across_tiles_enabled_flag,
        pps_loop_filter_across_slices_enabled_flag,
        deblocking_filter_control_present_flag,
        deblocking_filter_override_enabled_flag,
        pps_deblocking_filter_disabled_flag,
        pps_beta_offset_div2,
        pps_tc_offset_div2,
        pps_scaling_list_data_present_flag,
        scaling_lists,
        lists_modification_present_flag,
        log2_parallel_merge_level_minus2,
        slice_segment_header_extension_present_flag,
    })
}

/// 7.4.7.3, delta weights are within -128..=127.
fn weight_in_range(v: i32) -> bool {
    (-128..=127).contains(&v)
}

/// 7.4.7.3, offsets are bounded by 4 * wpOffsetHalfRangeC, at most 2^17 at 16 bits with high
/// precision offsets.
fn offset_in_range(v: i32) -> bool {
    (-(1 << 17)..(1 << 17)).contains(&v)
}

fn parse_pred_weight_table(
    r: &mut BitReader,
    slice_type: SliceType,
    chroma_format_idc: u32,
    num_l0: usize,
    num_l1: usize,
) -> Option<PredWeightTable> {
    let mut t = PredWeightTable { luma_log2_weight_denom: r.ue(), ..Default::default() };
    if chroma_format_idc != 0 {
        t.delta_chroma_log2_weight_denom = r.se();
    }
    // Both denominators 0 to 7 (7.4.7.3).
    let chroma = i64::from(t.luma_log2_weight_denom) + i64::from(t.delta_chroma_log2_weight_denom);
    if t.luma_log2_weight_denom > 7 || !(0..=7).contains(&chroma) {
        return None;
    }
    let read_list = |r: &mut BitReader, n: usize| {
        let n = n.min(REFS_MAX);
        let mut luma_flags = [false; REFS_MAX];
        let mut chroma_flags = [false; REFS_MAX];
        for i in 0..n {
            luma_flags[i] = r.flag();
        }
        if chroma_format_idc != 0 {
            for i in 0..n {
                chroma_flags[i] = r.flag();
            }
        }
        let mut out = FixedList::<WeightEntry, REFS_MAX>::default();
        for i in 0..n {
            let mut e = WeightEntry {
                luma_flag: luma_flags[i],
                chroma_flag: chroma_flags[i],
                ..Default::default()
            };
            if luma_flags[i] {
                e.delta_luma_weight = r.se();
                e.luma_offset = r.se();
                if !weight_in_range(e.delta_luma_weight) || !offset_in_range(e.luma_offset) {
                    return None;
                }
            }
            if chroma_flags[i] {
                for c in 0..2 {
                    e.delta_chroma_weight[c] = r.se();
                    e.chroma_offset[c] = r.se();
                    if !weight_in_range(e.delta_chroma_weight[c])
                        || !offset_in_range(e.chroma_offset[c])
                    {
                        return None;
                    }
                }
            }
            out.push(e);
        }
        Some(out)
    };
    t.l0 = read_list(r, num_l0)?;
    if slice_type == SliceType::B {
        t.l1 = read_list(r, num_l1)?;
    }
    Some(t)
}

/// Parse a slice segment header (7.3.6.1). `None` when malformed or out of subset.
/// Parsed from a de-emulated prefix on the stack.
pub fn parse_slice_header(nal: &[u8], nal_type: u8, sps: &Sps, pps: &Pps) -> Option<SliceHeader> {
    parse_slice_header_after(nal, nal_type, sps, pps, None)
}

/// [`parse_slice_header`] with the picture's previous segment header, which a dependent
/// slice segment inherits (7.4.7.1). `None` for a dependent segment without one.
pub fn parse_slice_header_after(
    nal: &[u8],
    nal_type: u8,
    sps: &Sps,
    pps: &Pps,
    prev: Option<&SliceHeader>,
) -> Option<SliceHeader> {
    if nal.len() < 3 {
        return None;
    }
    let mut prefix = [0u8; HEADER_PREFIX];
    let n = crate::h264parse::de_emulate(&nal[2..], &mut prefix);
    let mut sh = parse_slice_header_rbsp(&prefix[..n], nal_type, sps, pps, prev);
    if n == HEADER_PREFIX && sh.as_ref().is_none_or(|sh| sh.slice_data_byte_offset as usize > n + 2)
    {
        let (rbsp, _) = rbsp_of(nal);
        sh = parse_slice_header_rbsp(&rbsp, nal_type, sps, pps, prev);
    }
    let mut sh = sh?;
    sh.num_emu_prev_bytes =
        emulation_bytes_before(&nal[2..], (sh.slice_data_byte_offset as usize).saturating_sub(2));
    Some(sh)
}

/// Entry points, the header extension and `byte_alignment()` (7.3.6.1), then the
/// `slice_data()` bit offset. `None` for a count past 7.4.7.1 or a header past the RBSP.
fn slice_segment_tail(
    r: &mut BitReader,
    sps: &Sps,
    pps: &Pps,
    rbsp_len: usize,
) -> Option<(u32, usize)> {
    let mut num_entry_point_offsets = 0;
    if pps.tiles_enabled_flag || pps.entropy_coding_sync_enabled_flag {
        num_entry_point_offsets = r.ue();
        // 7.4.7.1, one per tile, CTB row or both, less one.
        let (cols, rows) = (pps.num_tile_columns_minus1 + 1, pps.num_tile_rows_minus1 + 1);
        let ctb_rows = sps.pic_height_in_ctbs();
        let most = match (pps.tiles_enabled_flag, pps.entropy_coding_sync_enabled_flag) {
            (true, true) => cols * ctb_rows,
            (true, false) => cols * rows,
            _ => ctb_rows,
        };
        if num_entry_point_offsets >= most {
            return None;
        }
        if num_entry_point_offsets > 0 {
            let offset_len_minus1 = r.ue().min(31);
            for _ in 0..num_entry_point_offsets {
                r.u(offset_len_minus1 + 1);
            }
        }
    }
    if pps.slice_segment_header_extension_present_flag {
        let ext_len = r.ue();
        if ext_len > 256 {
            return None;
        }
        for _ in 0..ext_len {
            r.u(8);
        }
    }

    r.flag();
    while !r.bit_pos().is_multiple_of(8) {
        r.flag();
    }
    let slice_data_bits = r.bit_pos();
    // A header read past its bytes is a truncated one.
    if slice_data_bits > rbsp_len * 8 {
        return None;
    }
    Some((num_entry_point_offsets, slice_data_bits))
}

#[allow(unused_assignments)]
fn parse_slice_header_rbsp(
    rbsp: &[u8],
    nal_type: u8,
    sps: &Sps,
    pps: &Pps,
    prev: Option<&SliceHeader>,
) -> Option<SliceHeader> {
    let mut r = BitReader::new(rbsp);

    let first_slice_segment_in_pic_flag = r.flag();
    let mut no_output_of_prior_pics_flag = false;
    if is_irap(nal_type) {
        no_output_of_prior_pics_flag = r.flag();
    }
    let pps_id = r.ue();
    if pps_id != pps.pps_id {
        return None;
    }

    let mut dependent_slice_segment_flag = false;
    let mut slice_segment_address = 0;
    if !first_slice_segment_in_pic_flag {
        if pps.dependent_slice_segments_enabled_flag {
            dependent_slice_segment_flag = r.flag();
        }
        let pic_size_in_ctbs = sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs();
        let bits = ceil_log2(pic_size_in_ctbs);
        slice_segment_address = r.u(bits);
        if slice_segment_address >= pic_size_in_ctbs {
            return None;
        }
    }

    let mut slice_type = SliceType::I;
    let mut pic_output_flag = true;
    let mut pic_order_cnt_lsb = 0;
    let mut st_rps = ShortTermRps::default();
    let mut st_rps_bits = 0u32;
    let mut long_term_refs = FixedList::<LongTermRef, LONG_TERM_MAX>::default();
    let mut slice_temporal_mvp_enabled_flag = false;
    let mut slice_sao_luma_flag = false;
    let mut slice_sao_chroma_flag = false;
    let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    let mut num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    let mut ref_list_mods = RefListMods::default();
    let mut mvd_l1_zero_flag = false;
    let mut cabac_init_flag = false;
    let mut collocated_from_l0_flag = true;
    let mut collocated_ref_idx = 0;
    let mut pred_weights = None;
    let mut five_minus_max_num_merge_cand = 0;
    let mut slice_cb_qp_offset = 0;
    let mut slice_cr_qp_offset = 0;
    let mut deblocking_filter_override_flag = false;
    let mut cu_chroma_qp_offset_enabled_flag = false;
    let mut slice_deblocking_filter_disabled_flag = pps.pps_deblocking_filter_disabled_flag;
    let mut slice_beta_offset_div2 = pps.pps_beta_offset_div2;
    let mut slice_tc_offset_div2 = pps.pps_tc_offset_div2;
    let mut slice_loop_filter_across_slices_enabled_flag =
        pps.pps_loop_filter_across_slices_enabled_flag;

    if !dependent_slice_segment_flag {
        for _ in 0..pps.num_extra_slice_header_bits {
            r.flag();
        }
        let st = r.ue();
        slice_type = SliceType::from_u32(st)?;
        if pps.output_flag_present_flag {
            pic_output_flag = r.flag();
        }
        if sps.separate_colour_plane_flag {
            r.u(2); // colour_plane_id
        }

        if !is_idr(nal_type) {
            let lsb_bits = sps.log2_max_pic_order_cnt_lsb_minus4 + 4;
            pic_order_cnt_lsb = r.u(lsb_bits);
            let short_term_ref_pic_set_sps_flag = r.flag();
            if !short_term_ref_pic_set_sps_flag {
                let bits_before = r.bit_pos();
                st_rps = parse_short_term_rps(
                    &mut r,
                    sps.num_short_term_ref_pic_sets,
                    sps.num_short_term_ref_pic_sets,
                    &sps.st_rps,
                )?;
                st_rps_bits = (r.bit_pos() - bits_before) as u32;
            } else if sps.num_short_term_ref_pic_sets > 0 {
                let n_bits = ceil_log2(sps.num_short_term_ref_pic_sets);
                let idx = if n_bits > 0 { r.u(n_bits) } else { 0 };
                st_rps = sps.st_rps.get(idx as usize).cloned().unwrap_or_default();
            }

            if sps.long_term_ref_pics_present_flag {
                let num_long_term_sps = if sps.num_long_term_ref_pics_sps > 0 { r.ue() } else { 0 };
                let num_long_term_pics = r.ue();
                let total = u64::from(num_long_term_sps) + u64::from(num_long_term_pics);
                if num_long_term_sps > sps.num_long_term_ref_pics_sps || total > 32 {
                    return None;
                }
                let total = total as u32;
                let lsb_bits = sps.log2_max_pic_order_cnt_lsb_minus4 + 4;
                let mut prev_msb_cycle = 0u32;
                for i in 0..total {
                    let mut lt = LongTermRef::default();
                    if i < num_long_term_sps {
                        let idx = if sps.num_long_term_ref_pics_sps > 1 {
                            r.u(ceil_log2(sps.num_long_term_ref_pics_sps))
                        } else {
                            0
                        } as usize;
                        lt.poc_lsb = sps.lt_ref_pic_poc_lsb_sps.get(idx).copied().unwrap_or(0);
                        lt.used_by_curr_pic =
                            sps.used_by_curr_pic_lt_sps_flag.get(idx).copied().unwrap_or(false);
                    } else {
                        lt.poc_lsb = r.u(lsb_bits);
                        lt.used_by_curr_pic = r.flag();
                    }
                    lt.delta_poc_msb_present = r.flag();
                    if lt.delta_poc_msb_present {
                        let cycle = r.ue();
                        // 7.4.7.1 range, then eq. 7-52 accumulates across the entries of
                        // each of the two lists.
                        if cycle >= 1 << (32 - lsb_bits) {
                            return None;
                        }
                        lt.delta_poc_msb_cycle = match i == 0 || i == num_long_term_sps {
                            true => cycle,
                            false => prev_msb_cycle.checked_add(cycle)?,
                        };
                    }
                    prev_msb_cycle = lt.delta_poc_msb_cycle;
                    long_term_refs.push(lt);
                }
            }

            if sps.sps_temporal_mvp_enabled_flag {
                slice_temporal_mvp_enabled_flag = r.flag();
            }
        }

        if sps.sample_adaptive_offset_enabled_flag {
            slice_sao_luma_flag = r.flag();
            if sps.chroma_format_idc != 0 {
                slice_sao_chroma_flag = r.flag();
            }
        }

        if slice_type.is_inter() {
            let num_ref_idx_active_override_flag = r.flag();
            if num_ref_idx_active_override_flag {
                num_ref_idx_l0_active_minus1 = r.ue().min(14);
                if slice_type == SliceType::B {
                    num_ref_idx_l1_active_minus1 = r.ue().min(14);
                }
            }

            let num_pic_total_curr = num_pic_total_curr(&st_rps, &long_term_refs, pps);

            if pps.lists_modification_present_flag && num_pic_total_curr > 1 {
                let n_bits = ceil_log2(num_pic_total_curr);
                let l0_flag = r.flag();
                ref_list_mods.l0_present = l0_flag;
                if l0_flag {
                    for _ in 0..=num_ref_idx_l0_active_minus1 {
                        ref_list_mods.l0.push(r.u(n_bits));
                    }
                }
                if slice_type == SliceType::B {
                    let l1_flag = r.flag();
                    ref_list_mods.l1_present = l1_flag;
                    if l1_flag {
                        for _ in 0..=num_ref_idx_l1_active_minus1 {
                            ref_list_mods.l1.push(r.u(n_bits));
                        }
                    }
                }
            }

            if slice_type == SliceType::B {
                mvd_l1_zero_flag = r.flag();
            }
            if pps.cabac_init_present_flag {
                cabac_init_flag = r.flag();
            }
            if slice_temporal_mvp_enabled_flag {
                if slice_type == SliceType::B {
                    collocated_from_l0_flag = r.flag();
                }
                let active = if collocated_from_l0_flag {
                    num_ref_idx_l0_active_minus1
                } else {
                    num_ref_idx_l1_active_minus1
                };
                if active > 0 {
                    collocated_ref_idx = r.ue();
                    if collocated_ref_idx > active {
                        return None;
                    }
                }
            }
            let weighted = (pps.weighted_pred_flag && slice_type == SliceType::P)
                || (pps.weighted_bipred_flag && slice_type == SliceType::B);
            if weighted {
                pred_weights = Some(parse_pred_weight_table(
                    &mut r,
                    slice_type,
                    sps.chroma_format_idc,
                    num_ref_idx_l0_active_minus1 as usize + 1,
                    num_ref_idx_l1_active_minus1 as usize + 1,
                )?);
            }
            five_minus_max_num_merge_cand = r.ue();
            if five_minus_max_num_merge_cand > 4 {
                return None;
            }
        }

        let slice_qp_delta = r.se();
        if pps.pps_slice_chroma_qp_offsets_present_flag {
            slice_cb_qp_offset = r.se();
            slice_cr_qp_offset = r.se();
        }
        // SliceQpY in -QpBdOffsetY..=51, the chroma offsets -12 to 12 alone and with the PPS's.
        let qp = 26 + i64::from(pps.init_qp_minus26) + i64::from(slice_qp_delta);
        let bd = 6 * i64::from(sps.bit_depth_luma_minus8);
        let chroma = |s: i32, p: i32| (-12..=12).contains(&s) && (-12..=12).contains(&(s + p));
        let in_range = (-bd..=51).contains(&qp)
            && chroma(slice_cb_qp_offset, pps.pps_cb_qp_offset)
            && chroma(slice_cr_qp_offset, pps.pps_cr_qp_offset);
        if !in_range {
            return None;
        }
        if pps.range.chroma_qp_offset_list_enabled_flag {
            cu_chroma_qp_offset_enabled_flag = r.flag();
        }
        let deblocking_filter_override = pps.deblocking_filter_override_enabled_flag;
        if deblocking_filter_override {
            deblocking_filter_override_flag = r.flag();
        }
        if deblocking_filter_override_flag {
            slice_deblocking_filter_disabled_flag = r.flag();
            if !slice_deblocking_filter_disabled_flag {
                slice_beta_offset_div2 = r.se();
                slice_tc_offset_div2 = r.se();
                if !(-6..=6).contains(&slice_beta_offset_div2)
                    || !(-6..=6).contains(&slice_tc_offset_div2)
                {
                    return None;
                }
            }
        }
        if pps.pps_loop_filter_across_slices_enabled_flag
            && (slice_sao_luma_flag
                || slice_sao_chroma_flag
                || !slice_deblocking_filter_disabled_flag)
        {
            slice_loop_filter_across_slices_enabled_flag = r.flag();
        }

        let slice_qp_delta_val = slice_qp_delta;

        let (num_entry_point_offsets, slice_data_bits) =
            slice_segment_tail(&mut r, sps, pps, rbsp.len())?;

        return Some(SliceHeader {
            first_slice_segment_in_pic_flag,
            no_output_of_prior_pics_flag,
            pps_id,
            dependent_slice_segment_flag,
            slice_segment_address,
            slice_type,
            pic_output_flag,
            pic_order_cnt_lsb,
            st_rps,
            st_rps_bits,
            long_term_refs,
            slice_temporal_mvp_enabled_flag,
            slice_sao_luma_flag,
            slice_sao_chroma_flag,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            ref_list_mods,
            mvd_l1_zero_flag,
            cabac_init_flag,
            collocated_from_l0_flag,
            collocated_ref_idx,
            pred_weights,
            five_minus_max_num_merge_cand,
            slice_qp_delta: slice_qp_delta_val,
            slice_cb_qp_offset,
            slice_cr_qp_offset,
            cu_chroma_qp_offset_enabled_flag,
            deblocking_filter_override_flag,
            slice_deblocking_filter_disabled_flag,
            slice_beta_offset_div2,
            slice_tc_offset_div2,
            slice_loop_filter_across_slices_enabled_flag,
            num_entry_point_offsets,
            // Add the 2-byte NAL header to the RBSP bit count.
            slice_data_byte_offset: (slice_data_bits as u32 + 16) / 8,
            num_emu_prev_bytes: 0,
        });
    }

    // A dependent segment carries the preceding segment's values (7.4.7.1), the header holds
    // only its address and tail.
    let mut sh = prev?.clone();
    sh.first_slice_segment_in_pic_flag = false;
    sh.no_output_of_prior_pics_flag = no_output_of_prior_pics_flag;
    sh.dependent_slice_segment_flag = true;
    sh.slice_segment_address = slice_segment_address;
    let (num_entry_point_offsets, slice_data_bits) =
        slice_segment_tail(&mut r, sps, pps, rbsp.len())?;
    sh.num_entry_point_offsets = num_entry_point_offsets;
    sh.slice_data_byte_offset = (slice_data_bits as u32 + 16) / 8;
    sh.num_emu_prev_bytes = 0;
    Some(sh)
}

/// PicOrderCntVal state (8.3.1).
pub struct PocState {
    prev_poc_lsb: i32,
    prev_poc_msb: i32,
}

impl PocState {
    pub fn new() -> Self {
        PocState { prev_poc_lsb: 0, prev_poc_msb: 0 }
    }
    pub fn reset(&mut self) {
        self.prev_poc_lsb = 0;
        self.prev_poc_msb = 0;
    }
    /// PicOrderCntVal (8.3.1). An IRAP with NoRaslOutputFlag has POC MSB 0.
    pub fn compute(
        &mut self,
        sps: &Sps,
        sh: &SliceHeader,
        is_idr: bool,
        is_irap_no_rasl: bool,
        temporal_id: u8,
    ) -> i32 {
        if is_idr {
            self.prev_poc_lsb = 0;
            self.prev_poc_msb = 0;
            return 0;
        }
        let max_poc_lsb = sps.max_poc_lsb();
        let poc_lsb = sh.pic_order_cnt_lsb as i32;
        let poc_msb = if is_irap_no_rasl {
            0
        } else {
            let prev_lsb = self.prev_poc_lsb;
            let prev_msb = self.prev_poc_msb;
            // Wrapping, a corrupt stream gets a wrong POC and no overflow.
            if poc_lsb < prev_lsb && (prev_lsb - poc_lsb) >= max_poc_lsb / 2 {
                prev_msb.wrapping_add(max_poc_lsb)
            } else if poc_lsb > prev_lsb && (poc_lsb - prev_lsb) > max_poc_lsb / 2 {
                prev_msb.wrapping_sub(max_poc_lsb)
            } else {
                prev_msb
            }
        };
        let poc = poc_msb.wrapping_add(poc_lsb);
        // Spec updates prev only for TId 0 non-RASL/RADL/sub-layer pictures (8.3.1). Approximated
        // by TId 0.
        if temporal_id == 0 {
            self.prev_poc_lsb = poc_lsb;
            self.prev_poc_msb = poc_msb;
        }
        poc
    }
}

impl Default for PocState {
    fn default() -> Self {
        Self::new()
    }
}

/// NumPicTotalCurr (7.4.7.2).
fn num_pic_total_curr(st_rps: &ShortTermRps, lt: &[LongTermRef], _pps: &Pps) -> u32 {
    let mut n = 0u32;
    for &u in &st_rps.used_s0 {
        n += u as u32;
    }
    for &u in &st_rps.used_s1 {
        n += u as u32;
    }
    for l in lt {
        n += l.used_by_curr_pic as u32;
    }
    n
}

/// Ceil(Log2(n)), the HEVC index field width.
pub fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        return 0;
    }
    32 - (n - 1).leading_zeros()
}

impl ShortTermRps {
    /// RefPicSetStCurrBefore deltas (8.3.2).
    pub fn curr_before(&self) -> Vec<i32> {
        self.delta_poc_s0
            .iter()
            .zip(self.used_s0.iter())
            .filter(|(_, u)| **u)
            .map(|(d, _)| *d)
            .collect()
    }
    /// RefPicSetStCurrAfter deltas (8.3.2).
    pub fn curr_after(&self) -> Vec<i32> {
        self.delta_poc_s1
            .iter()
            .zip(self.used_s1.iter())
            .filter(|(_, u)| **u)
            .map(|(d, _)| *d)
            .collect()
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn the_sps_carries_the_dpb_size_and_reorder_depth_of_the_top_sub_layer() {
        let main8 = "420101016000000300900000030000030078a003c0801107cb965654a4c2f0169c20000003002000000303c1";
        let main10 = "420101022000000300900000030000030078a003c08010e4d96566924caf016808000003000800000300f040";
        for (bytes, ten_bit) in [(main8, false), (main10, true)] {
            let sps = parse_sps(&hex(bytes)).expect("parses");
            assert_eq!((sps.width(), sps.height()), (1920, 1080));
            assert_eq!(sps.is_ten_bit(), ten_bit);
            assert_eq!(sps.sps_max_dec_pic_buffering_minus1, 4);
            assert_eq!(sps.sps_max_num_reorder_pics, 2);
            assert_eq!((sps.ref_frames(), sps.reorder_frames()), (4, 2));
        }
        let mut sps = stub_sps();
        sps.sps_max_num_reorder_pics = 9;
        assert_eq!(sps.reorder_frames(), 4, "the DPB bounds the reorder depth");
        sps.sps_max_num_reorder_pics = 0;
        assert_eq!(sps.reorder_frames(), 1, "one picture stays in flight");
        sps.sps_max_dec_pic_buffering_minus1 = 0;
        assert_eq!(sps.ref_frames(), 1);
    }

    #[test]
    fn emulation_bytes_are_counted_up_to_a_point() {
        let body = [0x11, 0, 0, 3, 1, 0x22, 0, 0, 3, 0, 0x33];
        assert_eq!(emulation_bytes(&body), 2);
        assert_eq!(
            emulation_bytes_before(&body, 2),
            0,
            "kept bytes 0 to 2 lie ahead of the first 3"
        );
        assert_eq!(emulation_bytes_before(&body, 3), 1, "the 3 lies ahead of kept byte 3");
        assert_eq!(emulation_bytes_before(&body, 6), 1);
        assert_eq!(emulation_bytes_before(&body, 7), 2);
    }

    /// Uniform tiles split as H.265 (6-3) and (6-4) do, explicit ones end in the rest.
    #[test]
    fn tile_sizes_follow_6_5_1() {
        let mut cols = [0u16; 19];
        tile_sizes_minus1(true, &[], 2, 30, &mut cols);
        assert_eq!(&cols[..3], &[14, 14, 0], "1920 wide, 64 CTBs");
        let mut rows = [0u16; 21];
        tile_sizes_minus1(true, &[], 2, 17, &mut rows);
        assert_eq!(&rows[..2], &[7, 8], "1080 high");
        tile_sizes_minus1(true, &[], 3, 10, &mut rows);
        assert_eq!(&rows[..3], &[2, 2, 3]);
        let mut cols = [0u16; 19];
        tile_sizes_minus1(false, &[3, 9], 3, 30, &mut cols);
        assert_eq!(&cols[..4], &[3, 9, 15, 0], "the last takes the 16 left");
        tile_sizes_minus1(false, &[40], 2, 30, &mut cols);
        assert_eq!(&cols[..2], &[40, 0], "too wide a coded tile leaves one CTB");
    }

    #[test]
    fn a_sub_layer_profile_tier_level_is_88_bits() {
        // All ones, every sub-layer has its profile and level present.
        let ones = [0xFFu8; 64];
        let mut r = BitReader::new(&ones);
        assert!(skip_profile_tier_level(&mut r, 0).0);
        assert_eq!(r.bit_pos(), 88 + 8, "general profile and level");
        let mut r = BitReader::new(&ones);
        skip_profile_tier_level(&mut r, 2);
        // 96 general, 2 flag pairs, 6 reserved pairs, then 88 + 8 per sub-layer.
        assert_eq!(r.bit_pos(), 96 + 4 + 12 + 2 * 96);
    }

    #[test]
    fn a_dependent_slice_segment_inherits_the_previous_header() {
        let sps = parse_sps(&hex("420101016000000300900000030000030078a003c0801107cb965654a4c2f0169c20000003002000000303c1")).expect("sps");
        let mut pps = parse_pps(&hex("4401c073c189")).expect("pps");
        pps.dependent_slice_segments_enabled_flag = true;
        let mut first = hex(
            "2801ac2104431240a04d9cac867864c9c8ad49e87dc3103943ec16461c7fc88428004003c03a893002efefc7102ebf04",
        );
        first.extend(std::iter::repeat_n(0x11, 3 * HEADER_PREFIX));
        let prev = parse_slice_header(&first, NAL_IDR_W_RADL, &sps, &pps).expect("parses");
        let pic_size = sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs();
        let address = pic_size / 2;
        let mut w = crate::bitwriter::BitWriter::new();
        w.flag(false); // first_slice_segment_in_pic_flag
        w.ue(pps.pps_id);
        w.flag(true); // dependent_slice_segment_flag
        w.u(address, ceil_log2(pic_size));
        if pps.tiles_enabled_flag || pps.entropy_coding_sync_enabled_flag {
            w.ue(0); // num_entry_point_offsets
        }
        if pps.slice_segment_header_extension_present_flag {
            w.ue(0); // slice_segment_header_extension_length
        }
        w.rbsp_trailing_bits(); // byte_alignment()
        let mut nal = vec![NAL_TRAIL_R << 1, 1];
        nal.extend_from_slice(w.as_bytes());
        let header_len = nal.len();
        nal.extend_from_slice(&[0x5A; 24]);
        assert!(
            parse_slice_header_after(&nal, NAL_TRAIL_R, &sps, &pps, None).is_none(),
            "no segment to inherit from"
        );
        let sh =
            parse_slice_header_after(&nal, NAL_TRAIL_R, &sps, &pps, Some(&prev)).expect("parses");
        assert!(sh.dependent_slice_segment_flag && !sh.first_slice_segment_in_pic_flag);
        assert_eq!(sh.slice_segment_address, address);
        assert_eq!(sh.slice_data_byte_offset as usize, header_len);
        assert_eq!((sh.slice_type, sh.slice_qp_delta), (prev.slice_type, prev.slice_qp_delta));
        assert_eq!(sh.num_entry_point_offsets, 0);
        // Without the PPS allowing it, the flag is not read and the address bits fail to parse
        // as an independent header or parse as one with a wrong address, never as dependent.
        pps.dependent_slice_segments_enabled_flag = false;
        let plain = parse_slice_header_after(&nal, NAL_TRAIL_R, &sps, &pps, Some(&prev));
        assert!(plain.is_none_or(|sh| !sh.dependent_slice_segment_flag));
    }

    #[test]
    fn a_slice_header_is_read_from_a_prefix() {
        let sps = parse_sps(&hex("420101016000000300900000030000030078a003c0801107cb965654a4c2f0169c20000003002000000303c1")).expect("sps");
        let pps = parse_pps(&hex("4401c073c189")).expect("pps");
        let mut nal = hex(
            "2801ac2104431240a04d9cac867864c9c8ad49e87dc3103943ec16461c7fc88428004003c03a893002efefc7102ebf04",
        );
        nal.extend(std::iter::repeat_n(0x11, 3 * HEADER_PREFIX));
        nal.extend([0, 0, 3, 0, 0, 3, 1]);
        let sh = parse_slice_header(&nal, NAL_IDR_W_RADL, &sps, &pps).expect("parses");
        assert!(sh.first_slice_segment_in_pic_flag && sh.slice_type == SliceType::I);
        assert_eq!(sh.num_emu_prev_bytes, 0, "none in the header, two in the slice data");
        let (rbsp, _) = rbsp_of(&nal);
        let whole =
            parse_slice_header_rbsp(&rbsp, NAL_IDR_W_RADL, &sps, &pps, None).expect("parses");
        assert_eq!(whole.slice_data_byte_offset, sh.slice_data_byte_offset);
        assert_eq!(whole.slice_qp_delta, sh.slice_qp_delta);
        assert_eq!(rbsp.len(), nal.len() - 4, "two bytes of header and two emulation bytes");
    }

    #[test]
    fn split_nals_into_reuses_the_list() {
        let au = [0, 0, 0, 1, 0x40, 0x01, 0xAA, 0, 0, 1, 0x42, 0x01, 0, 0, 0, 1, 0x26, 0x01, 0];
        let mut out = vec![Nal { nal_type: 0, temporal_id: 0, start: 9, end: 9 }];
        split_nals_into(&au, &mut out);
        assert_eq!(out.len(), 3, "cleared first");
        assert_eq!(out[0].raw(&au), &[0x40, 0x01, 0xAA], "the 4-byte start code's zero is trimmed");
        assert_eq!((out[1].nal_type, out[1].temporal_id), (NAL_SPS, 0));
        assert_eq!(out[2].raw(&au), &[0x26, 0x01, 0], "the last NAL keeps its trailing zero");
        assert_eq!(split_nals(&au), out);
        split_nals_into(&[0, 0, 1, 0x40], &mut out);
        assert!(out.is_empty(), "a NAL short of its header is none");
    }

    #[test]
    fn ceil_log2_matches_spec() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(33), 6);
    }

    #[test]
    fn nal_header_decodes_type_and_tid() {
        let au = [0, 0, 1, 0x40, 0x01, 0xAA];
        let nals = split_nals(&au);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].nal_type, NAL_VPS);
        assert_eq!(nals[0].temporal_id, 0);
    }

    #[test]
    fn idr_poc_resets_to_zero() {
        let mut poc = PocState::new();
        let sps = stub_sps();
        let sh = stub_slice(0);
        let v = poc.compute(&sps, &sh, true, true, 0);
        assert_eq!(v, 0, "IDR POC must reset to 0");
    }

    #[test]
    fn poc_msb_wraps_forward() {
        let mut poc = PocState::new();
        let sps = stub_sps(); // log2_max_poc_lsb_minus4 = 0 -> max_poc_lsb = 16
        // Step via lsb 8 first, 14 - 0 > 8 would spuriously wrap.
        let _ = poc.compute(&sps, &stub_slice(0), true, true, 0);
        let _ = poc.compute(&sps, &stub_slice(8), false, false, 0); // poc 8
        let _ = poc.compute(&sps, &stub_slice(14), false, false, 0); // poc 14
        // prev_lsb 14, 14 - 1 >= 8, so msb += 16 and poc is 17.
        let v = poc.compute(&sps, &stub_slice(1), false, false, 0);
        assert_eq!(v, 17, "POC MSB must advance on lsb wrap");
    }

    fn stub_sps() -> Sps {
        Sps {
            sps_id: 0,
            max_sub_layers_minus1: 0,
            chroma_format_idc: 1,
            separate_colour_plane_flag: false,
            pic_width_in_luma_samples: 64,
            pic_height_in_luma_samples: 64,
            conf_win_left_offset: 0,
            conf_win_right_offset: 0,
            conf_win_top_offset: 0,
            conf_win_bottom_offset: 0,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0, // max_poc_lsb = 16
            sps_max_dec_pic_buffering_minus1: 4,
            sps_max_num_reorder_pics: 2,
            log2_min_luma_coding_block_size_minus3: 0,
            log2_diff_max_min_luma_coding_block_size: 3,
            log2_min_transform_block_size_minus2: 0,
            log2_diff_max_min_transform_block_size: 3,
            max_transform_hierarchy_depth_inter: 0,
            max_transform_hierarchy_depth_intra: 0,
            scaling_list_enabled_flag: false,
            scaling_lists: None,
            amp_enabled_flag: true,
            sample_adaptive_offset_enabled_flag: true,
            pcm_enabled_flag: false,
            pcm_sample_bit_depth_luma_minus1: 0,
            pcm_sample_bit_depth_chroma_minus1: 0,
            log2_min_pcm_luma_coding_block_size_minus3: 0,
            log2_diff_max_min_pcm_luma_coding_block_size: 0,
            pcm_loop_filter_disabled_flag: false,
            num_short_term_ref_pic_sets: 0,
            st_rps: Vec::new(),
            long_term_ref_pics_present_flag: false,
            num_long_term_ref_pics_sps: 0,
            lt_ref_pic_poc_lsb_sps: Vec::new(),
            used_by_curr_pic_lt_sps_flag: Vec::new(),
            sps_temporal_mvp_enabled_flag: true,
            strong_intra_smoothing_enabled_flag: false,
            has_extension: false,
            range_ext: 0,
            rext: false,
            colour: Default::default(),
        }
    }

    fn stub_slice(lsb: u32) -> SliceHeader {
        SliceHeader {
            first_slice_segment_in_pic_flag: true,
            no_output_of_prior_pics_flag: false,
            pps_id: 0,
            dependent_slice_segment_flag: false,
            slice_segment_address: 0,
            slice_type: SliceType::P,
            pic_output_flag: true,
            pic_order_cnt_lsb: lsb,
            st_rps: ShortTermRps::default(),
            st_rps_bits: 0,
            long_term_refs: Default::default(),
            slice_temporal_mvp_enabled_flag: false,
            slice_sao_luma_flag: false,
            slice_sao_chroma_flag: false,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            ref_list_mods: RefListMods::default(),
            mvd_l1_zero_flag: false,
            cabac_init_flag: false,
            collocated_from_l0_flag: true,
            collocated_ref_idx: 0,
            pred_weights: None,
            five_minus_max_num_merge_cand: 0,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            cu_chroma_qp_offset_enabled_flag: false,
            deblocking_filter_override_flag: false,
            slice_deblocking_filter_disabled_flag: false,
            slice_beta_offset_div2: 0,
            slice_tc_offset_div2: 0,
            slice_loop_filter_across_slices_enabled_flag: true,
            num_entry_point_offsets: 0,
            slice_data_byte_offset: 0,
            num_emu_prev_bytes: 0,
        }
    }

    use crate::bitwriter::BitWriter;

    fn pred(w: &mut BitWriter, delta: u32) {
        w.flag(false);
        w.ue(delta);
    }

    fn explicit(w: &mut BitWriter, dc: Option<i32>, n: usize, start: i32) {
        w.flag(true);
        let mut next = 8;
        if let Some(dc) = dc {
            w.se(dc - 8);
            next = dc;
        }
        for i in 0..n {
            let v = start + i as i32;
            w.se(v - next);
            next = v;
        }
    }

    fn lists_with(f: impl Fn(&mut BitWriter, usize, usize) -> bool) -> ScalingLists {
        let mut w = BitWriter::new();
        for size_id in 0..4 {
            let step = if size_id == 3 { 3 } else { 1 };
            let mut m = 0;
            while m < 6 {
                if !f(&mut w, size_id, m) {
                    pred(&mut w, 0);
                }
                m += step;
            }
        }
        w.rbsp_trailing_bits();
        let bytes = w.into_bytes();
        parse_scaling_list_data(&mut BitReader::new(&bytes)).expect("lists in range")
    }

    #[test]
    fn predicted_with_delta_zero_gives_the_default_tables() {
        let l = lists_with(|_, _, _| false);
        assert_eq!(l, ScalingLists::default_lists());
        assert!(l.list4x4.iter().all(|m| *m == [16; 16]));
        assert_eq!((l.list8x8[0][0], l.list8x8[0][63], l.list8x8[3][63]), (16, 115, 91));
        assert_eq!((l.list16x16[2], l.list16x16[5]), (DEFAULT_INTRA, DEFAULT_INTER));
        assert_eq!(
            (l.list32x32, l.dc16x16, l.dc32x32),
            ([DEFAULT_INTRA, DEFAULT_INTER], [16; 6], [16; 2])
        );
    }

    #[test]
    fn explicit_lists_are_delta_coded_from_eight_or_their_dc() {
        let l = lists_with(|w, size_id, m| match (size_id, m) {
            (0, 1) => {
                explicit(w, None, 16, 20);
                true
            }
            (2, 4) => {
                explicit(w, Some(30), 64, 40);
                true
            }
            (3, 3) => {
                explicit(w, Some(12), 64, 100);
                true
            }
            _ => false,
        });
        let want: Vec<u8> = (20..36).collect();
        assert_eq!(&l.list4x4[1][..], &want[..]);
        assert_eq!(l.list4x4[0], [16; 16], "the rest stay default");
        assert_eq!(l.list16x16[4][..4], [40, 41, 42, 43]);
        assert_eq!((l.list16x16[4][63], l.dc16x16[4]), (103, 30));
        assert_eq!((l.list32x32[1][0], l.list32x32[1][63], l.dc32x32[1]), (100, 163, 12));
        assert_eq!((l.list32x32[0], l.dc32x32[0]), (DEFAULT_INTRA, 16));
    }

    #[test]
    fn a_coefficient_wraps_modulo_256() {
        let l = lists_with(|w, size_id, m| {
            if (size_id, m) != (1, 0) {
                return false;
            }
            w.flag(true);
            w.se(-9); // 8 - 9 wraps to 255
            for _ in 1..64 {
                w.se(0);
            }
            true
        });
        assert!(l.list8x8[0].iter().all(|&v| v == 255));
    }

    #[test]
    fn predicted_from_a_reference_copies_the_list_and_its_dc() {
        let l = lists_with(|w, size_id, m| match (size_id, m) {
            (1, 0) => {
                explicit(w, None, 64, 50);
                true
            }
            (1, 2) => {
                pred(w, 2); // matrixId 0
                true
            }
            (2, 1) => {
                explicit(w, Some(77), 64, 10);
                true
            }
            (2, 2) => {
                pred(w, 1); // matrixId 1, its DC too
                true
            }
            (3, 0) => {
                explicit(w, Some(99), 64, 60);
                true
            }
            (3, 3) => {
                pred(w, 1); // 32x32 steps by 3, matrixId 0
                true
            }
            _ => false,
        });
        assert_eq!(l.list8x8[2], l.list8x8[0]);
        assert_eq!(l.list8x8[2][0], 50);
        assert_eq!(l.list8x8[1], DEFAULT_INTRA, "the one between is untouched");
        assert_eq!((l.list16x16[2], l.dc16x16[2]), (l.list16x16[1], 77));
        assert_eq!((l.list32x32[1], l.dc32x32[1]), (l.list32x32[0], 99));
        assert_eq!(l.list32x32[1][0], 60);
    }

    #[test]
    fn the_sps_and_pps_carry_their_lists() {
        let sps = stub_sps();
        assert!(sps.scaling_lists.is_none());
        let l = lists_with(|_, _, _| false);
        assert_eq!(l, ScalingLists::default_lists());
    }

    #[test]
    fn the_diagonal_scan_is_the_specs_and_raster_puts_coefficients_in_place() {
        let mut scan = [0usize; 16];
        diag_scan(4, &mut scan);
        assert_eq!(scan, [0, 4, 1, 8, 5, 2, 12, 9, 6, 3, 13, 10, 7, 14, 11, 15]);
        let mut scan8 = [0usize; 64];
        diag_scan(8, &mut scan8);
        assert_eq!(&scan8[..6], &[0, 8, 1, 16, 9, 2]);
        assert_eq!(scan8[63], 63);
        let mut seen = [false; 64];
        for &i in &scan8 {
            seen[i] = true;
        }
        assert!(seen.iter().all(|&b| b), "every position once");
        let coded: [u8; 16] = core::array::from_fn(|i| i as u8);
        let r = to_raster(&coded);
        assert_eq!((r[4], r[1], r[0], r[15]), (1, 2, 0, 15));
        let flat = to_raster(&[16u8; 64]);
        assert_eq!(flat, [16; 64]);
    }
}
