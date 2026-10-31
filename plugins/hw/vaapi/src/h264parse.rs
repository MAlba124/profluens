//! H.264 Annex-B parsing for VA-API slice-level decode. SPS, PPS, slice headers and POC
//! (H.264 7.3, 8.2.1). 8-bit 4:2:0 progressive only.

#![allow(clippy::needless_range_loop)]

use profluens_video::fixed::FixedList;

/// A parsed NAL (7.4.1) as offsets into the access unit, header and emulation bytes included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nal {
    pub ref_idc: u8,
    pub unit_type: u8,
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

// NAL unit types (Table 7-1).
pub const NAL_SLICE_NON_IDR: u8 = 1;
pub const NAL_SEI: u8 = 6;
pub const NAL_AUD: u8 = 9;
pub const NAL_FILLER: u8 = 12;

/// Whether the unit is a non-reference picture only (nal_ref_idc 0, 7.4.1), so it can be
/// skipped before the presentation point.
pub fn non_reference(nals: &[Nal]) -> bool {
    let mut slices = 0u32;
    for nal in nals {
        match nal.unit_type {
            NAL_SLICE_NON_IDR if nal.ref_idc == 0 => slices += 1,
            NAL_SEI | NAL_AUD | NAL_FILLER => {}
            _ => return false,
        }
    }
    slices > 0
}
pub const NAL_SLICE_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_END_SEQ: u8 = 10;
pub const NAL_END_STREAM: u8 = 11;
pub const NAL_PPS: u8 = 8;

/// Split an Annex-B access unit into its NALs (B.1.1).
// Cold, tests and one-off callers.
#[allow(clippy::disallowed_methods)]
pub fn split_nals(au: &[u8]) -> Vec<Nal> {
    let mut out = Vec::new();
    split_nals_into(au, &mut out);
    out
}

/// [`split_nals`] into `out`. The trailing 0x00 before a 4-byte start code is dropped.
pub fn split_nals_into(au: &[u8], out: &mut Vec<Nal>) {
    out.clear();
    for_each_nal(au, |n| out.push(n));
}

/// Calls `f` on each NAL of an Annex-B access unit in order, as [`split_nals`] finds them.
pub fn for_each_nal(au: &[u8], mut f: impl FnMut(Nal)) {
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if let Some(s) = open {
                let end = if i > s && au[i - 1] == 0 { i - 1 } else { i };
                push_nal(au, s, end, &mut f);
            }
            open = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(s) = open {
        push_nal(au, s, au.len(), &mut f);
    }
}

fn push_nal(au: &[u8], start: usize, end: usize, f: &mut impl FnMut(Nal)) {
    if end <= start {
        return;
    }
    let header = au[start];
    f(Nal {
        ref_idc: (header >> 5) & 0x3,
        unit_type: header & 0x1f,
        start: start as u32,
        end: end as u32,
    });
}

/// De-emulate an RBSP (7.4.1). The raw NAL still goes to VA.
// Parameter sets and the cold slice header fallback, once per stream or never.
#[allow(clippy::disallowed_methods)]
fn rbsp_of(nal: &[u8]) -> Vec<u8> {
    let body = &nal[1.min(nal.len())..];
    let mut out = vec![0; body.len()];
    let n = de_emulate(body, &mut out);
    out.truncate(n);
    out
}

pub(crate) fn de_emulate(body: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    let mut zeros = 0;
    let mut idx = 0;
    while idx < body.len() && n < out.len() {
        let b = body[idx];
        if zeros >= 2 && b == 0x03 && idx + 1 < body.len() && body[idx + 1] <= 0x03 {
            zeros = 0;
            idx += 1;
            continue;
        }
        out[n] = b;
        n += 1;
        zeros = if b == 0 { zeros + 1 } else { 0 };
        idx += 1;
    }
    n
}

/// Big-endian bit reader with Exp-Golomb helpers (9.1).
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    fn bit(&mut self) -> u32 {
        let byte = self.pos >> 3;
        if byte >= self.data.len() {
            self.pos += 1;
            return 0;
        }
        let shift = 7 - (self.pos & 7);
        self.pos += 1;
        ((self.data[byte] >> shift) & 1) as u32
    }

    pub fn u(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }

    pub fn flag(&mut self) -> bool {
        self.bit() != 0
    }

    /// Unsigned Exp-Golomb `ue(v)` (9.1).
    pub fn ue(&mut self) -> u32 {
        let mut zeros = 0u32;
        while self.bit() == 0 {
            zeros += 1;
            if zeros > 31 {
                return 0; // malformed guard
            }
        }
        if zeros == 0 {
            return 0;
        }
        let suffix = self.u(zeros);
        (1u32 << zeros) - 1 + suffix
    }

    /// Signed Exp-Golomb `se(v)` (9.1.1).
    pub fn se(&mut self) -> i32 {
        let k = self.ue();
        let mag = ((k + 1) >> 1) as i32;
        if k & 1 == 1 { mag } else { -mag }
    }

    /// `more_rbsp_data` (7.2).
    pub fn more_rbsp_data(&self) -> bool {
        let total_bits = self.data.len() * 8;
        let mut last_one = None;
        for i in (0..total_bits).rev() {
            let byte = i >> 3;
            let shift = 7 - (i & 7);
            if (self.data[byte] >> shift) & 1 == 1 {
                last_one = Some(i);
                break;
            }
        }
        match last_one {
            Some(stop) => self.pos < stop,
            None => false,
        }
    }
}

/// Parsed SPS (7.3.2.1).
#[derive(Clone, Debug)]
pub struct Sps {
    pub seq_parameter_set_id: u32,
    pub profile_idc: u8,
    pub constraint_set3_flag: bool,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offsets_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    pub frame_crop_left: u32,
    pub frame_crop_right: u32,
    pub frame_crop_top: u32,
    pub frame_crop_bottom: u32,
    /// VUI `bitstream_restriction` (E.2.1), in frames.
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
    pub colour: profluens_video::decoder::Color,
    /// `seq_scaling_matrix_present_flag` lists, fall-back rule A applied. None is Flat_16.
    pub scaling: Option<ScalingLists>,
}

impl Sps {
    /// MaxDpbFrames (A.3.1 eq. A-2). Unknown levels get 16.
    pub fn max_dpb_frames(&self) -> u32 {
        let max_dpb_mbs: u32 = match self.level_idc {
            9 => 396,
            10 => 396,
            11 if self.constraint_set3_flag && self.profile_idc <= 88 => 396,
            11 => 900,
            12 | 13 | 20 => 2376,
            21 => 4752,
            22 | 30 => 8100,
            31 => 18000,
            32 => 20480,
            40 | 41 => 32768,
            42 => 34816,
            50 => 110_400,
            51 | 52 => 184_320,
            60..=62 => 696_320,
            _ => return 16,
        };
        let frame_mbs = (self.width_in_mbs() * self.height_in_mbs()).max(1);
        (max_dpb_mbs / frame_mbs).clamp(1, 16)
    }

    pub fn dpb_frames(&self) -> u32 {
        let frames = self.max_dec_frame_buffering.unwrap_or_else(|| self.max_dpb_frames());
        frames.max(self.max_num_ref_frames).clamp(1, 16)
    }

    /// Reorder depth. Without VUI E.2.1 infers MaxDpbFrames, up to 16 frames of latency, so
    /// `max_num_ref_frames + 1` is used instead. At least one so readback overlaps decode.
    pub fn reorder_frames(&self) -> u32 {
        let depth = match self.max_num_reorder_frames {
            Some(n) => n.min(self.dpb_frames()),
            None => self.max_num_ref_frames + 1,
        };
        depth.clamp(1, 16)
    }

    pub fn width(&self) -> u32 {
        let w_mbs = self.pic_width_in_mbs_minus1 + 1;
        let chroma = !self.separate_colour_plane_flag;
        let crop_unit_x = if matches!(self.chroma_format_idc, 1 | 2) && chroma { 2 } else { 1 };
        w_mbs * 16 - crop_unit_x * (self.frame_crop_left + self.frame_crop_right)
    }

    pub fn height(&self) -> u32 {
        let frame_mbs_mult = if self.frame_mbs_only_flag { 1 } else { 2 };
        let h_mbs = (self.pic_height_in_map_units_minus1 + 1) * frame_mbs_mult;
        let crop_unit_y = {
            let sub =
                if self.chroma_format_idc == 1 && !self.separate_colour_plane_flag { 2 } else { 1 };
            sub * frame_mbs_mult
        };
        h_mbs * 16 - crop_unit_y * (self.frame_crop_top + self.frame_crop_bottom)
    }

    /// The crop window's top left corner in luma samples (7.4.2.1.1).
    pub fn crop_origin(&self) -> (u32, u32) {
        let chroma = !self.separate_colour_plane_flag;
        let crop_unit_x = if matches!(self.chroma_format_idc, 1 | 2) && chroma { 2 } else { 1 };
        let sub = if self.chroma_format_idc == 1 && chroma { 2 } else { 1 };
        let crop_unit_y = sub * if self.frame_mbs_only_flag { 1 } else { 2 };
        (crop_unit_x * self.frame_crop_left, crop_unit_y * self.frame_crop_top)
    }

    pub fn width_in_mbs(&self) -> u32 {
        self.pic_width_in_mbs_minus1 + 1
    }

    pub fn height_in_mbs(&self) -> u32 {
        let mult = if self.frame_mbs_only_flag { 1 } else { 2 };
        (self.pic_height_in_map_units_minus1 + 1) * mult
    }

    pub fn max_frame_num(&self) -> u32 {
        1u32 << (self.log2_max_frame_num_minus4 + 4)
    }

    pub fn max_poc_lsb(&self) -> u32 {
        1u32 << (self.log2_max_pic_order_cnt_lsb_minus4 + 4)
    }
}

/// Parse an SPS (7.3.2.1) from the raw NAL.
// Once per parameter set, not per frame.
#[allow(clippy::disallowed_methods)]
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    let rbsp = rbsp_of(nal);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.u(8) as u8;
    let constraint_flags = r.u(8);
    let constraint_set3_flag = constraint_flags & 0x10 != 0;
    let level_idc = r.u(8) as u8;
    let seq_parameter_set_id = r.ue();
    if seq_parameter_set_id > 31 {
        return None;
    }

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane_flag = false;
    let mut bit_depth_luma_minus8 = 0;
    let mut bit_depth_chroma_minus8 = 0;
    let mut scaling = None;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue();
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = r.flag();
        }
        bit_depth_luma_minus8 = r.ue();
        bit_depth_chroma_minus8 = r.ue();
        if bit_depth_luma_minus8 > 6 || bit_depth_chroma_minus8 > 6 {
            return None;
        }
        let _qpprime_y_zero_transform_bypass = r.flag();
        if r.flag() {
            // seq_scaling_matrix_present_flag
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            let mut lists = ScalingLists::FLAT;
            let mut present = [false; 12];
            read_scaling_matrix(&mut r, count, &mut lists, &mut present)?;
            fall_back_all(&mut lists, &present, None);
            scaling = Some(lists);
        }
    }

    let log2_max_frame_num_minus4 = r.ue();
    let pic_order_cnt_type = r.ue();
    if log2_max_frame_num_minus4 > 12 || pic_order_cnt_type > 2 {
        return None;
    }
    let mut log2_max_pic_order_cnt_lsb_minus4 = 0;
    let mut delta_pic_order_always_zero_flag = false;
    let mut offset_for_non_ref_pic = 0;
    let mut offset_for_top_to_bottom_field = 0;
    let mut offsets_for_ref_frame = Vec::new();
    if pic_order_cnt_type == 0 {
        log2_max_pic_order_cnt_lsb_minus4 = r.ue();
        if log2_max_pic_order_cnt_lsb_minus4 > 12 {
            return None;
        }
    } else if pic_order_cnt_type == 1 {
        delta_pic_order_always_zero_flag = r.flag();
        offset_for_non_ref_pic = r.se();
        offset_for_top_to_bottom_field = r.se();
        let num_ref_frames_in_cycle = r.ue();
        if num_ref_frames_in_cycle > 255 {
            return None;
        }
        for _ in 0..num_ref_frames_in_cycle {
            offsets_for_ref_frame.push(r.se());
        }
    }

    let max_num_ref_frames = r.ue();
    if max_num_ref_frames > 16 {
        return None;
    }
    let gaps_in_frame_num_value_allowed_flag = r.flag();
    let pic_width_in_mbs_minus1 = r.ue();
    let pic_height_in_map_units_minus1 = r.ue();
    let frame_mbs_only_flag = r.flag();
    let mut mb_adaptive_frame_field_flag = false;
    if !frame_mbs_only_flag {
        mb_adaptive_frame_field_flag = r.flag();
    }
    let direct_8x8_inference_flag = r.flag();
    let frame_cropping_flag = r.flag();
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if frame_cropping_flag {
        cl = r.ue();
        cr = r.ue();
        ct = r.ue();
        cb = r.ue();
    }
    // 16384 luma samples a side, past level 6.2's 1055 MBs as VA is past neither.
    let mult = if frame_mbs_only_flag { 1 } else { 2 };
    let (w_mbs, h_mbs) =
        (u64::from(pic_width_in_mbs_minus1) + 1, u64::from(pic_height_in_map_units_minus1) + 1);
    if w_mbs > 1024 || h_mbs * mult > 1024 {
        return None;
    }
    // CropUnitX and CropUnitY (7-19 to 7-22), the crop leaves at least one sample.
    let unit_x =
        if matches!(chroma_format_idc, 1 | 2) && !separate_colour_plane_flag { 2 } else { 1 };
    let sub_y = if chroma_format_idc == 1 && !separate_colour_plane_flag { 2 } else { 1 };
    if unit_x * (u64::from(cl) + u64::from(cr)) >= w_mbs * 16
        || sub_y * mult * (u64::from(ct) + u64::from(cb)) >= h_mbs * mult * 16
    {
        return None;
    }
    let (mut max_num_reorder_frames, mut max_dec_frame_buffering) = (None, None);
    let mut colour = Default::default();
    if r.flag()
        && let Some((reorder, dpb)) = parse_vui_restriction(&mut r, rbsp.len() * 8, &mut colour)
    {
        max_num_reorder_frames = Some(reorder);
        max_dec_frame_buffering = Some(dpb);
    }

    Some(Sps {
        seq_parameter_set_id,
        profile_idc,
        constraint_set3_flag,
        level_idc,
        chroma_format_idc,
        separate_colour_plane_flag,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4,
        delta_pic_order_always_zero_flag,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom_field,
        offsets_for_ref_frame,
        max_num_ref_frames,
        gaps_in_frame_num_value_allowed_flag,
        pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1,
        frame_mbs_only_flag,
        mb_adaptive_frame_field_flag,
        direct_8x8_inference_flag,
        frame_crop_left: cl,
        frame_crop_right: cr,
        frame_crop_top: ct,
        frame_crop_bottom: cb,
        max_num_reorder_frames,
        max_dec_frame_buffering,
        colour,
        scaling,
    })
}

fn parse_vui_restriction(
    r: &mut BitReader,
    bits: usize,
    colour: &mut profluens_video::decoder::Color,
) -> Option<(u32, u32)> {
    let signal = crate::colour::vui_signal(r);
    if r.bit_pos() <= bits {
        *colour = signal;
    }
    if r.flag() {
        r.u(32); // num_units_in_tick
        r.u(32); // time_scale
        r.flag(); // fixed_frame_rate_flag
    }
    let nal_hrd = r.flag();
    if nal_hrd {
        skip_hrd_parameters(r);
    }
    let vcl_hrd = r.flag();
    if vcl_hrd {
        skip_hrd_parameters(r);
    }
    if nal_hrd || vcl_hrd {
        r.flag(); // low_delay_hrd_flag
    }
    r.flag(); // pic_struct_present_flag
    if !r.flag() {
        return None; // no bitstream_restriction
    }
    r.flag(); // motion_vectors_over_pic_boundaries_flag
    r.ue(); // max_bytes_per_pic_denom
    r.ue(); // max_bits_per_mb_denom
    r.ue(); // log2_max_mv_length_horizontal
    r.ue(); // log2_max_mv_length_vertical
    let reorder = r.ue();
    let dpb = r.ue();
    (r.bit_pos() <= bits && reorder <= 16 && dpb <= 16).then_some((reorder, dpb))
}

fn skip_hrd_parameters(r: &mut BitReader) {
    let cpb_cnt_minus1 = r.ue().min(31);
    r.u(4); // bit_rate_scale
    r.u(4); // cpb_size_scale
    for _ in 0..=cpb_cnt_minus1 {
        r.ue(); // bit_rate_value_minus1
        r.ue(); // cpb_size_value_minus1
        r.flag(); // cbr_flag
    }
    r.u(5); // initial_cpb_removal_delay_length_minus1
    r.u(5); // cpb_removal_delay_length_minus1
    r.u(5); // dpb_output_delay_length_minus1
    r.u(5); // time_offset_length
}

/// Tables 7-3 and 7-4, in the coded (zigzag) order of 7.3.2.1.1.1.
const DEFAULT_4X4_INTRA: [u8; 16] = [6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42];
const DEFAULT_4X4_INTER: [u8; 16] =
    [10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34];
const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];
/// Frame zigzag scans (Tables 8-12 and 8-13), the raster index of each coded position.
const ZIGZAG_4X4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const ZIGZAG_8X8: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Scaling lists in coded order (7.4.2.1.1.1). 4x4: Y, Cb, Cr intra then inter. 8x8: Y intra,
/// Y inter, then Cb and Cr pairs for 4:4:4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScalingLists {
    pub list4x4: [[u8; 16]; 6],
    pub list8x8: [[u8; 64]; 6],
}

impl ScalingLists {
    pub const FLAT: ScalingLists = ScalingLists { list4x4: [[16; 16]; 6], list8x8: [[16; 64]; 6] };

    fn default_4x4(i: usize) -> [u8; 16] {
        if i < 3 { DEFAULT_4X4_INTRA } else { DEFAULT_4X4_INTER }
    }

    fn default_8x8(i: usize) -> [u8; 64] {
        if i.is_multiple_of(2) { DEFAULT_8X8_INTRA } else { DEFAULT_8X8_INTER }
    }
}

/// A coded 4x4 list in the raster order VA takes.
pub fn raster_4x4(coded: &[u8; 16]) -> [u8; 16] {
    let mut out = [0; 16];
    for (i, &v) in coded.iter().enumerate() {
        out[usize::from(ZIGZAG_4X4[i])] = v;
    }
    out
}

/// A coded 8x8 list in the raster order VA takes.
pub fn raster_8x8(coded: &[u8; 64]) -> [u8; 64] {
    let mut out = [0; 64];
    for (i, &v) in coded.iter().enumerate() {
        out[usize::from(ZIGZAG_8X8[i])] = v;
    }
    out
}

/// `scaling_list()` (7.3.2.1.1.1) into `out`. Some(true) is useDefaultScalingMatrixFlag, None a
/// delta_scale out of its 7.4.2.1.1.1 range.
fn read_scaling_list(r: &mut BitReader, out: &mut [u8]) -> Option<bool> {
    let mut last = 8i32;
    let mut next = 8i32;
    for (j, v) in out.iter_mut().enumerate() {
        if next != 0 {
            let delta = r.se();
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta + 256) % 256;
            if j == 0 && next == 0 {
                return Some(true);
            }
        }
        *v = if next == 0 { last as u8 } else { next as u8 };
        last = i32::from(*v);
    }
    Some(false)
}

/// The `count` present flags and lists of an SPS or PPS (7.3.2.1.1, 7.3.2.2). An absent list
/// is left for [`fall_back_all`].
fn read_scaling_matrix(
    r: &mut BitReader,
    count: usize,
    lists: &mut ScalingLists,
    present: &mut [bool; 12],
) -> Option<()> {
    for i in 0..count {
        present[i] = r.flag();
        if !present[i] {
            continue;
        }
        if i < 6 {
            if read_scaling_list(r, &mut lists.list4x4[i])? {
                lists.list4x4[i] = ScalingLists::default_4x4(i);
            }
        } else if read_scaling_list(r, &mut lists.list8x8[i - 6])? {
            lists.list8x8[i - 6] = ScalingLists::default_8x8(i - 6);
        }
    }
    Some(())
}

/// Table 7-2 fall-back for every absent list, rule A from the defaults when `sps` is None, rule
/// B from the SPS lists otherwise.
fn fall_back_all(lists: &mut ScalingLists, present: &[bool; 12], sps: Option<&ScalingLists>) {
    for i in (0..12).filter(|&i| !present[i]) {
        match i {
            0 | 3 => lists.list4x4[i] = sps.map_or(ScalingLists::default_4x4(i), |s| s.list4x4[i]),
            1 | 2 | 4 | 5 => lists.list4x4[i] = lists.list4x4[i - 1],
            6 | 7 => {
                lists.list8x8[i - 6] =
                    sps.map_or(ScalingLists::default_8x8(i - 6), |s| s.list8x8[i - 6]);
            }
            _ => lists.list8x8[i - 6] = lists.list8x8[i - 8],
        }
    }
}

/// The lists a picture dequantises with (7.4.2.2): the PPS lists with their fall-back, else
/// the SPS lists, else flat.
pub fn resolve_scaling(sps: &Sps, pps: &Pps) -> ScalingLists {
    let Some(p) = &pps.scaling else {
        return sps.scaling.unwrap_or(ScalingLists::FLAT);
    };
    let mut lists = p.lists;
    fall_back_all(&mut lists, &p.present, sps.scaling.as_ref());
    lists
}

/// `pic_scaling_matrix_present_flag` lists as coded, the fall-back waits for the SPS.
#[derive(Clone, Copy, Debug)]
pub struct PpsScaling {
    pub lists: ScalingLists,
    pub present: [bool; 12],
}

/// Parsed PPS (7.3.2.2).
#[derive(Clone, Debug)]
pub struct Pps {
    pub pic_parameter_set_id: u32,
    pub seq_parameter_set_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
    pub second_chroma_qp_index_offset: i32,
    pub scaling: Option<PpsScaling>,
}

/// Parse a PPS of a 4:2:0 or 4:2:2 sequence, see [`parse_pps_for`].
pub fn parse_pps(nal: &[u8]) -> Option<Pps> {
    parse_pps_for(nal, 1)
}

/// Parse a PPS (7.3.2.2). The SPS's `chroma_format_idc` sizes the scaling matrix.
pub fn parse_pps_for(nal: &[u8], chroma_format_idc: u32) -> Option<Pps> {
    let rbsp = rbsp_of(nal);
    let mut r = BitReader::new(&rbsp);
    let pic_parameter_set_id = r.ue();
    let seq_parameter_set_id = r.ue();
    if pic_parameter_set_id > 255 || seq_parameter_set_id > 31 {
        return None;
    }
    let entropy_coding_mode_flag = r.flag();
    let bottom_field_pic_order_in_frame_present_flag = r.flag();
    let num_slice_groups_minus1 = r.ue();
    if num_slice_groups_minus1 > 0 {
        // FMO is out of scope, the AU is warn-dropped.
        return None;
    }
    let num_ref_idx_l0_default_active_minus1 = r.ue();
    let num_ref_idx_l1_default_active_minus1 = r.ue();
    let weighted_pred_flag = r.flag();
    let weighted_bipred_idc = r.u(2);
    let pic_init_qp_minus26 = r.se();
    let _pic_init_qs_minus26 = r.se();
    let chroma_qp_index_offset = r.se();
    let deblocking_filter_control_present_flag = r.flag();
    let constrained_intra_pred_flag = r.flag();
    let redundant_pic_cnt_present_flag = r.flag();
    // 7.4.2.2, QpBdOffsetY at most 36.
    let in_range = num_ref_idx_l0_default_active_minus1 <= 31
        && num_ref_idx_l1_default_active_minus1 <= 31
        && weighted_bipred_idc <= 2
        && (-62..=25).contains(&pic_init_qp_minus26)
        && (-12..=12).contains(&chroma_qp_index_offset);
    if !in_range {
        return None;
    }

    let mut transform_8x8_mode_flag = false;
    let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
    let mut scaling = None;
    if r.more_rbsp_data() {
        transform_8x8_mode_flag = r.flag();
        if r.flag() {
            // pic_scaling_matrix_present_flag, 8x8 lists only with the 8x8 transform.
            let per_8x8 = if chroma_format_idc != 3 { 2 } else { 6 };
            let count = 6 + if transform_8x8_mode_flag { per_8x8 } else { 0 };
            let mut lists = ScalingLists::FLAT;
            let mut present = [false; 12];
            read_scaling_matrix(&mut r, count, &mut lists, &mut present)?;
            scaling = Some(PpsScaling { lists, present });
        }
        second_chroma_qp_index_offset = r.se();
        if !(-12..=12).contains(&second_chroma_qp_index_offset) {
            return None;
        }
    }

    Some(Pps {
        pic_parameter_set_id,
        seq_parameter_set_id,
        entropy_coding_mode_flag,
        bottom_field_pic_order_in_frame_present_flag,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        weighted_pred_flag,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        chroma_qp_index_offset,
        deblocking_filter_control_present_flag,
        constrained_intra_pred_flag,
        redundant_pic_cnt_present_flag,
        transform_8x8_mode_flag,
        second_chroma_qp_index_offset,
        scaling,
    })
}

/// Slice type (Table 7-6), reduced mod 5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    fn from_raw(v: u32) -> SliceType {
        match v % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::Sp,
            _ => SliceType::Si,
        }
    }
    pub fn va_value(self) -> u8 {
        match self {
            SliceType::P => 0,
            SliceType::B => 1,
            SliceType::I => 2,
            SliceType::Sp => 3,
            SliceType::Si => 4,
        }
    }
}

/// Modifications a list takes at most, `num_ref_idx_lX_active_minus1 + 1` (7.4.3.1).
pub const REF_LIST_MODS_MAX: usize = 32;
/// MMCO ops a slice header carries at most, ffmpeg's bound.
pub const MMCO_MAX: usize = 66;
/// Weight entries a list carries, the 32 VA takes.
pub const WEIGHTS_MAX: usize = 32;

#[derive(Clone, Copy, Default, Debug)]
pub struct RefListMod {
    pub op: u32,    // modification_of_pic_nums_idc
    pub value: u32, // abs_diff_pic_num_minus1 | long_term_pic_num
}

#[derive(Clone, Copy, Default, Debug)]
pub struct MmcoOp {
    pub op: u32, // memory_management_control_operation
    pub arg1: u32,
    pub arg2: u32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct WeightEntry {
    pub luma: Option<(i32, i32)>,
    pub chroma: Option<[(i32, i32); 2]>,
}

/// `pred_weight_table` (7.3.3.2). Zeros would make the driver multiply every prediction by 0.
#[derive(Clone, Default, Debug)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u32,
    pub chroma_log2_weight_denom: u32,
    pub l0: FixedList<WeightEntry, WEIGHTS_MAX>,
    pub l1: FixedList<WeightEntry, WEIGHTS_MAX>,
}

/// Parsed slice header (7.3.3).
#[derive(Clone, Debug)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    pub pic_parameter_set_id: u32,
    pub frame_num: u32,
    pub field_pic_flag: bool,
    pub bottom_field_flag: bool,
    pub idr_pic_id: u32,
    pub is_idr: bool,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    pub ref_list_mods_l0: FixedList<RefListMod, REF_LIST_MODS_MAX>,
    pub ref_list_mods_l1: FixedList<RefListMod, REF_LIST_MODS_MAX>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    pub direct_spatial_mv_pred_flag: bool,
    pub no_output_of_prior_pics_flag: bool,
    pub long_term_reference_flag: bool,
    pub adaptive_ref_pic_marking_mode_flag: bool,
    pub mmco_ops: FixedList<MmcoOp, MMCO_MAX>,
    pub pred_weights: Option<PredWeightTable>,
    /// Bit position in the de-emulated RBSP after the header, excluding the NAL header byte.
    pub header_bits_in_rbsp: usize,
}

/// Parse a slice header (7.3.3) from a de-emulated prefix on the stack.
pub fn parse_slice_header(
    nal: &[u8],
    unit_type: u8,
    ref_idc: u8,
    sps: &Sps,
    pps: &Pps,
) -> Option<SliceHeader> {
    let mut prefix = [0u8; HEADER_PREFIX];
    let n = de_emulate(&nal[1.min(nal.len())..], &mut prefix);
    let sh = parse_slice_header_rbsp(&prefix[..n], unit_type, ref_idc, sps, pps);
    if n < HEADER_PREFIX || sh.as_ref().is_some_and(|sh| sh.header_bits_in_rbsp <= n * 8) {
        return sh;
    }
    parse_slice_header_rbsp(&rbsp_of(nal), unit_type, ref_idc, sps, pps)
}

fn parse_slice_header_rbsp(
    rbsp: &[u8],
    unit_type: u8,
    ref_idc: u8,
    sps: &Sps,
    pps: &Pps,
) -> Option<SliceHeader> {
    let mut r = BitReader::new(rbsp);
    let is_idr = unit_type == NAL_SLICE_IDR;

    let first_mb_in_slice = r.ue();
    // 7.4.3, the slice starts inside the picture. Fields are refused downstream.
    let mb_pairs = 1 + u32::from(sps.mb_adaptive_frame_field_flag);
    if first_mb_in_slice.checked_mul(mb_pairs)? >= sps.width_in_mbs() * sps.height_in_mbs() {
        return None;
    }
    let slice_type = SliceType::from_raw(r.ue());
    let pic_parameter_set_id = r.ue();
    if sps.separate_colour_plane_flag {
        let _colour_plane_id = r.u(2);
    }
    let frame_num = r.u(sps.log2_max_frame_num_minus4 + 4);

    let mut field_pic_flag = false;
    let mut bottom_field_flag = false;
    if !sps.frame_mbs_only_flag {
        field_pic_flag = r.flag();
        if field_pic_flag {
            bottom_field_flag = r.flag();
        }
    }

    // MaxPicNum (7.4.3), the bound on every picNum difference below.
    let max_pic_num = sps.max_frame_num() << u32::from(field_pic_flag);

    let mut idr_pic_id = 0;
    if is_idr {
        idr_pic_id = r.ue();
        if idr_pic_id > 65_535 {
            return None;
        }
    }

    let mut pic_order_cnt_lsb = 0;
    let mut delta_pic_order_cnt_bottom = 0;
    let mut delta_pic_order_cnt = [0i32; 2];
    if sps.pic_order_cnt_type == 0 {
        pic_order_cnt_lsb = r.u(sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            delta_pic_order_cnt_bottom = r.se();
        }
    } else if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
        delta_pic_order_cnt[0] = r.se();
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            delta_pic_order_cnt[1] = r.se();
        }
    }

    if pps.redundant_pic_cnt_present_flag {
        let _redundant_pic_cnt = r.ue();
    }

    let mut direct_spatial_mv_pred_flag = false;
    if slice_type == SliceType::B {
        direct_spatial_mv_pred_flag = r.flag();
    }

    let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    let mut num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    if matches!(slice_type, SliceType::P | SliceType::Sp | SliceType::B) {
        let num_ref_idx_active_override_flag = r.flag();
        if num_ref_idx_active_override_flag {
            num_ref_idx_l0_active_minus1 = r.ue();
            if slice_type == SliceType::B {
                num_ref_idx_l1_active_minus1 = r.ue();
            }
        }
    }
    if num_ref_idx_l0_active_minus1 > 31 || num_ref_idx_l1_active_minus1 > 31 {
        return None;
    }

    let mut ref_list_mods_l0 = FixedList::default();
    let mut ref_list_mods_l1 = FixedList::default();
    if !matches!(slice_type, SliceType::I | SliceType::Si) && r.flag() {
        loop {
            let op = r.ue();
            if op == 3 {
                break;
            }
            if op > 5 {
                return None;
            }
            let value = r.ue();
            if !ref_list_mod_in_range(op, value, max_pic_num) {
                return None;
            }
            if !ref_list_mods_l0.push(RefListMod { op, value }) {
                return None;
            }
        }
    }
    if slice_type == SliceType::B && r.flag() {
        loop {
            let op = r.ue();
            if op == 3 {
                break;
            }
            if op > 5 {
                return None;
            }
            let value = r.ue();
            if !ref_list_mod_in_range(op, value, max_pic_num) {
                return None;
            }
            if !ref_list_mods_l1.push(RefListMod { op, value }) {
                return None;
            }
        }
    }

    let mut pred_weights = None;
    if (pps.weighted_pred_flag && matches!(slice_type, SliceType::P | SliceType::Sp))
        || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B)
    {
        pred_weights = Some(parse_pred_weight_table(
            &mut r,
            sps.chroma_format_idc,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            slice_type,
        )?);
    }

    let mut no_output_of_prior_pics_flag = false;
    let mut long_term_reference_flag = false;
    let mut adaptive_ref_pic_marking_mode_flag = false;
    let mut mmco_ops = FixedList::default();
    if ref_idc != 0 {
        if is_idr {
            no_output_of_prior_pics_flag = r.flag();
            long_term_reference_flag = r.flag();
        } else {
            adaptive_ref_pic_marking_mode_flag = r.flag();
            if adaptive_ref_pic_marking_mode_flag {
                loop {
                    let op = r.ue();
                    if op == 0 {
                        break;
                    }
                    if op > 6 {
                        return None;
                    }
                    let mut arg1 = 0;
                    let mut arg2 = 0;
                    match op {
                        1 | 3 => arg1 = r.ue(), // difference_of_pic_nums_minus1
                        2 => arg1 = r.ue(),     // long_term_pic_num
                        4 => arg1 = r.ue(),     // max_long_term_frame_idx_plus1
                        6 => arg1 = r.ue(),     // long_term_frame_idx
                        _ => {}
                    }
                    if op == 3 {
                        arg2 = r.ue(); // long_term_frame_idx
                    }
                    // 7.4.3.3 ranges, so the picNum arithmetic stays in i32.
                    let in_range = match op {
                        1 | 3 => arg1 < max_pic_num,
                        2 => arg1 <= 2 * MAX_LONG_TERM_FRAME_IDX + 1,
                        4 => arg1 <= sps.max_num_ref_frames,
                        6 => arg1 <= MAX_LONG_TERM_FRAME_IDX,
                        _ => true,
                    };
                    if !in_range || (op == 3 && arg2 > MAX_LONG_TERM_FRAME_IDX) {
                        return None;
                    }
                    if !mmco_ops.push(MmcoOp { op, arg1, arg2 }) {
                        return None;
                    }
                }
            }
        }
    }

    let mut cabac_init_idc = 0;
    if pps.entropy_coding_mode_flag && !matches!(slice_type, SliceType::I | SliceType::Si) {
        cabac_init_idc = r.ue();
    }
    let slice_qp_delta = r.se();
    // SliceQPY in -QpBdOffsetY..=51 (7.4.3).
    let qp = 26i64 + i64::from(pps.pic_init_qp_minus26) + i64::from(slice_qp_delta);
    if cabac_init_idc > 2 || !(-6 * i64::from(sps.bit_depth_luma_minus8)..=51).contains(&qp) {
        return None;
    }

    let mut disable_deblocking_filter_idc = 0;
    let mut slice_alpha_c0_offset_div2 = 0;
    let mut slice_beta_offset_div2 = 0;
    if pps.deblocking_filter_control_present_flag {
        disable_deblocking_filter_idc = r.ue();
        if disable_deblocking_filter_idc != 1 {
            slice_alpha_c0_offset_div2 = r.se();
            slice_beta_offset_div2 = r.se();
        }
        let offsets = -6..=6;
        if disable_deblocking_filter_idc > 2
            || !offsets.contains(&slice_alpha_c0_offset_div2)
            || !offsets.contains(&slice_beta_offset_div2)
        {
            return None;
        }
    }

    let header_bits_in_rbsp = r.bit_pos();
    // A header read past its bytes is a truncated one.
    if header_bits_in_rbsp > rbsp.len() * 8 {
        return None;
    }

    Some(SliceHeader {
        first_mb_in_slice,
        slice_type,
        pic_parameter_set_id,
        frame_num,
        field_pic_flag,
        bottom_field_flag,
        idr_pic_id,
        is_idr,
        pic_order_cnt_lsb,
        delta_pic_order_cnt_bottom,
        delta_pic_order_cnt,
        num_ref_idx_l0_active_minus1,
        num_ref_idx_l1_active_minus1,
        ref_list_mods_l0,
        ref_list_mods_l1,
        cabac_init_idc,
        slice_qp_delta,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        direct_spatial_mv_pred_flag,
        no_output_of_prior_pics_flag,
        long_term_reference_flag,
        adaptive_ref_pic_marking_mode_flag,
        mmco_ops,
        pred_weights,
        header_bits_in_rbsp,
    })
}

/// Largest LongTermFrameIdx (7.4.3.3), MaxLongTermFrameIdx is below max_num_ref_frames.
pub const MAX_LONG_TERM_FRAME_IDX: u32 = 15;

/// 7.4.3.1 ranges of abs_diff_pic_num_minus1 and long_term_pic_num.
fn ref_list_mod_in_range(op: u32, value: u32, max_pic_num: u32) -> bool {
    match op {
        0 | 1 => value < max_pic_num,
        2 => value <= 2 * MAX_LONG_TERM_FRAME_IDX + 1,
        _ => true,
    }
}

/// 7.4.3.2 weights and offsets are within -128..=127.
fn weight_in_range(v: i32) -> bool {
    (-128..=127).contains(&v)
}

fn parse_pred_weight_table(
    r: &mut BitReader,
    chroma_format_idc: u32,
    num_ref_l0: u32,
    num_ref_l1: u32,
    slice_type: SliceType,
) -> Option<PredWeightTable> {
    let luma_log2_weight_denom = r.ue();
    let chroma_log2_weight_denom = if chroma_format_idc != 0 { r.ue() } else { 0 };
    if luma_log2_weight_denom > 7 || chroma_log2_weight_denom > 7 {
        return None;
    }
    let read_list = |r: &mut BitReader, count: u32| {
        let mut list = FixedList::<WeightEntry, WEIGHTS_MAX>::default();
        // Consume every entry to stay in sync, store at most the 32 VA carries.
        for _ in 0..=count {
            let mut e = WeightEntry::default();
            if r.flag() {
                let (w, o) = (r.se(), r.se());
                if !weight_in_range(w) || !weight_in_range(o) {
                    return None;
                }
                e.luma = Some((w, o));
            }
            if chroma_format_idc != 0 && r.flag() {
                let c = [(r.se(), r.se()), (r.se(), r.se())];
                if c.iter().any(|&(w, o)| !weight_in_range(w) || !weight_in_range(o)) {
                    return None;
                }
                e.chroma = Some(c);
            }
            list.push(e);
        }
        Some(list)
    };
    let l0 = read_list(r, num_ref_l0)?;
    let l1 =
        if slice_type == SliceType::B { read_list(r, num_ref_l1)? } else { FixedList::default() };
    Some(PredWeightTable { luma_log2_weight_denom, chroma_log2_weight_denom, l0, l1 })
}

/// POC state across pictures (8.2.1).
#[derive(Default, Clone)]
pub struct PocState {
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num: i32,
    prev_frame_num_offset: i32,
}

impl PocState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = PocState::default();
    }

    /// POC (8.2.1) as (TopFieldOrderCnt, BottomFieldOrderCnt). Frames only. `ref_idc` is the
    /// NAL's nal_ref_idc, a non-reference picture leaves the prev state and counts apart.
    pub fn compute(
        &mut self,
        sps: &Sps,
        sh: &SliceHeader,
        is_idr: bool,
        ref_idc: u8,
    ) -> (i32, i32) {
        match sps.pic_order_cnt_type {
            0 => self.compute_type0(sps, sh, is_idr, ref_idc),
            2 => self.compute_type2(sps, sh, is_idr, ref_idc),
            _ => self.compute_type1(sps, sh, is_idr, ref_idc),
        }
    }

    fn compute_type0(
        &mut self,
        sps: &Sps,
        sh: &SliceHeader,
        is_idr: bool,
        ref_idc: u8,
    ) -> (i32, i32) {
        let max_poc_lsb = sps.max_poc_lsb() as i32;
        let (prev_msb, prev_lsb) =
            if is_idr { (0, 0) } else { (self.prev_poc_msb, self.prev_poc_lsb) };
        let poc_lsb = sh.pic_order_cnt_lsb as i32;
        // Wrapping, a corrupt stream gets a wrong POC and no overflow.
        let poc_msb = if poc_lsb < prev_lsb && (prev_lsb - poc_lsb) >= max_poc_lsb / 2 {
            prev_msb.wrapping_add(max_poc_lsb)
        } else if poc_lsb > prev_lsb && (poc_lsb - prev_lsb) > max_poc_lsb / 2 {
            prev_msb.wrapping_sub(max_poc_lsb)
        } else {
            prev_msb
        };
        let top = poc_msb.wrapping_add(poc_lsb);
        let bottom = top.wrapping_add(sh.delta_pic_order_cnt_bottom);
        // Only a reference picture carries the prev state forward (8.2.1.1).
        if ref_idc != 0 {
            self.prev_poc_msb = poc_msb;
            self.prev_poc_lsb = poc_lsb;
        }
        (top, bottom)
    }

    fn compute_type2(
        &mut self,
        sps: &Sps,
        sh: &SliceHeader,
        is_idr: bool,
        ref_idc: u8,
    ) -> (i32, i32) {
        let max_frame_num = sps.max_frame_num() as i32;
        let frame_num = sh.frame_num as i32;
        let frame_num_offset = if is_idr {
            0
        } else if self.prev_frame_num > frame_num {
            self.prev_frame_num_offset.wrapping_add(max_frame_num)
        } else {
            self.prev_frame_num_offset
        };
        // 8.2.1.3, a non-reference picture sits one below 2 * FrameNumOffset + frame_num.
        let tmp = frame_num_offset.wrapping_add(frame_num).wrapping_mul(2);
        let poc = if is_idr {
            0
        } else if ref_idc == 0 {
            tmp.wrapping_sub(1)
        } else {
            tmp
        };
        self.prev_frame_num = frame_num;
        self.prev_frame_num_offset = frame_num_offset;
        (poc, poc)
    }

    fn compute_type1(
        &mut self,
        sps: &Sps,
        sh: &SliceHeader,
        is_idr: bool,
        ref_idc: u8,
    ) -> (i32, i32) {
        let max_frame_num = sps.max_frame_num() as i32;
        let frame_num = sh.frame_num as i32;
        let frame_num_offset = if is_idr {
            0
        } else if self.prev_frame_num > frame_num {
            self.prev_frame_num_offset.wrapping_add(max_frame_num)
        } else {
            self.prev_frame_num_offset
        };
        let num_in_cycle = sps.offsets_for_ref_frame.len() as i32;
        let mut expected = 0i32;
        let abs_frame_num =
            if num_in_cycle != 0 { frame_num_offset.wrapping_add(frame_num) } else { 0 };
        let abs_frame_num = if sh.is_idr { 0 } else { abs_frame_num };
        // 8.2.1.2, a non-reference picture counts one frame back.
        let abs_frame_num = if ref_idc == 0 { abs_frame_num - 1 } else { abs_frame_num }.max(0);
        if num_in_cycle != 0 && abs_frame_num > 0 {
            let sum_cycle = sps.offsets_for_ref_frame.iter().fold(0i32, |s, o| s.wrapping_add(*o));
            let poc_cycle_cnt = (abs_frame_num - 1) / num_in_cycle;
            let frame_num_in_cycle = (abs_frame_num - 1) % num_in_cycle;
            expected = poc_cycle_cnt.wrapping_mul(sum_cycle);
            for i in 0..=frame_num_in_cycle {
                expected = expected.wrapping_add(sps.offsets_for_ref_frame[i as usize]);
            }
        }
        if ref_idc == 0 {
            expected = expected.wrapping_add(sps.offset_for_non_ref_pic);
        }
        let top = expected.wrapping_add(sh.delta_pic_order_cnt[0]);
        let bottom = top
            .wrapping_add(sps.offset_for_top_to_bottom_field)
            .wrapping_add(sh.delta_pic_order_cnt[1]);
        self.prev_frame_num = frame_num;
        self.prev_frame_num_offset = frame_num_offset;
        (top, bottom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    const SPS_1080P_HIGH: &str = "67640028acd940780227e5c044000003000400000300f03c60c658";
    const SPS_4K_CB: &str = "6742c033da00f0010fb0110000030001000003003c0f1832a0";
    const PPS_1080P_HIGH: &str = "68ef8fcb";
    const IDR_1080P_HIGH: &str = "6588840047dae3985bd757da423e83899688c51b11acd773c877e17f75b9377c7daf57e6f9a408897c6317eaf79c5cb5";

    #[test]
    fn vui_bitstream_restriction_gives_the_reorder_depth_and_dpb() {
        let sps = parse_sps(&hex(SPS_1080P_HIGH)).expect("parses");
        assert_eq!((sps.width(), sps.height()), (1920, 1080));
        assert_eq!((sps.profile_idc, sps.level_idc), (100, 40));
        assert_eq!(sps.max_num_ref_frames, 4);
        assert_eq!(sps.max_num_reorder_frames, Some(2));
        assert_eq!(sps.max_dec_frame_buffering, Some(4));
        assert_eq!(sps.dpb_frames(), 4);
        assert_eq!(sps.reorder_frames(), 2);
        // Level 4.0 at 1080p, 32768 / (120 * 68) = 4.
        assert_eq!(sps.max_dpb_frames(), 4);

        let sps = parse_sps(&hex(SPS_4K_CB)).expect("parses");
        assert_eq!((sps.width(), sps.height()), (3840, 2160));
        assert_eq!((sps.profile_idc, sps.level_idc), (66, 51));
        assert_eq!(sps.max_num_ref_frames, 1);
        assert_eq!((sps.max_num_reorder_frames, sps.max_dec_frame_buffering), (Some(0), Some(1)));
        assert_eq!(sps.reorder_frames(), 1, "no reorder still keeps one picture in flight");
        // Level 5.1 at 2160p, 184320 / (240 * 135) = 5.
        assert_eq!(sps.max_dpb_frames(), 5);
    }

    #[test]
    fn without_a_vui_the_level_sizes_the_dpb_and_refs_bound_the_reorder() {
        let mut sps = parse_sps(&hex(SPS_1080P_HIGH)).expect("parses");
        sps.max_num_reorder_frames = None;
        sps.max_dec_frame_buffering = None;
        assert_eq!(sps.dpb_frames(), 4, "MaxDpbFrames of level 4.0 at 1080p");
        assert_eq!(sps.reorder_frames(), 5, "max_num_ref_frames + 1");
        sps.level_idc = 51;
        assert_eq!(sps.max_dpb_frames(), 16, "184320 / 8160 = 22, capped at the syntax's 16");
        sps.level_idc = 0;
        assert_eq!(sps.max_dpb_frames(), 16, "an unknown level is not undersized");
        sps.max_num_ref_frames = 16;
        assert_eq!(sps.reorder_frames(), 16, "capped at 16");
        sps.level_idc = 30;
        sps.max_num_ref_frames = 6;
        assert_eq!(sps.dpb_frames(), 6, "never below max_num_ref_frames");
    }

    /// A P slice of 32 active references with `mods` list modifications and a trailing MMCO run.
    fn p_slice(sps: &Sps, pps: &Pps, mods: usize, mmcos: usize) -> Vec<u8> {
        p_slice_with(sps, pps, mods, 0, mmcos, 0)
    }

    fn p_slice_with(
        sps: &Sps,
        pps: &Pps,
        mods: usize,
        mod_value: u32,
        mmcos: usize,
        mmco_arg: u32,
    ) -> Vec<u8> {
        let mut w = crate::bitwriter::BitWriter::new();
        w.ue(0); // first_mb_in_slice
        w.ue(5); // slice_type P
        w.ue(pps.pic_parameter_set_id);
        w.u(0, sps.log2_max_frame_num_minus4 + 4);
        if !sps.frame_mbs_only_flag {
            w.flag(false);
        }
        assert_eq!(sps.pic_order_cnt_type, 0);
        w.u(0, sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
        if pps.bottom_field_pic_order_in_frame_present_flag {
            w.se(0);
        }
        if pps.redundant_pic_cnt_present_flag {
            w.ue(0);
        }
        w.flag(true); // num_ref_idx_active_override_flag
        w.ue(31);
        w.flag(true); // ref_pic_list_modification_flag_l0
        for _ in 0..mods {
            w.ue(0);
            w.ue(mod_value);
        }
        w.ue(3);
        if pps.weighted_pred_flag {
            w.ue(0);
            w.ue(0);
            for _ in 0..32 {
                w.flag(false);
                w.flag(false);
            }
        }
        w.flag(true); // adaptive_ref_pic_marking_mode_flag
        for _ in 0..mmcos {
            w.ue(1);
            w.ue(mmco_arg);
        }
        w.ue(0);
        if pps.entropy_coding_mode_flag {
            w.ue(0);
        }
        w.se(0); // slice_qp_delta
        if pps.deblocking_filter_control_present_flag {
            w.ue(1);
        }
        w.rbsp_trailing_bits();
        w.into_bytes()
    }

    #[test]
    fn slice_lists_past_their_spec_bound_are_malformed() {
        let sps = parse_sps(&hex(SPS_1080P_HIGH)).expect("parses");
        let pps = parse_pps(&hex(PPS_1080P_HIGH)).expect("parses");
        let parse = |mods, mmcos| {
            parse_slice_header_rbsp(&p_slice(&sps, &pps, mods, mmcos), 1, 2, &sps, &pps)
        };
        let sh = parse(REF_LIST_MODS_MAX, MMCO_MAX).expect("lists at their bound parse");
        assert_eq!(sh.ref_list_mods_l0.len(), REF_LIST_MODS_MAX);
        assert_eq!(sh.mmco_ops.len(), MMCO_MAX);
        assert!(sh.ref_list_mods_l1.is_empty());
        assert!(parse(REF_LIST_MODS_MAX + 1, 0).is_none(), "one modification too many");
        assert!(parse(0, MMCO_MAX + 1).is_none(), "one MMCO too many");
    }

    #[test]
    fn pic_num_differences_past_max_pic_num_are_malformed() {
        // MaxFrameNum 16 in this SPS, so MaxPicNum 16 for frames (7.4.3).
        let sps = parse_sps(&hex(SPS_1080P_HIGH)).expect("parses");
        let pps = parse_pps(&hex(PPS_1080P_HIGH)).expect("parses");
        assert_eq!(sps.max_frame_num(), 16);
        let parse = |mods, mod_value, mmcos, mmco_arg| {
            let nal = p_slice_with(&sps, &pps, mods, mod_value, mmcos, mmco_arg);
            parse_slice_header_rbsp(&nal, 1, 2, &sps, &pps)
        };
        assert!(parse(1, 15, 1, 15).is_some(), "the largest in range");
        assert!(parse(1, 16, 0, 0).is_none(), "abs_diff_pic_num_minus1 at MaxPicNum");
        assert!(parse(0, 0, 1, 16).is_none(), "difference_of_pic_nums_minus1 at MaxPicNum");
        assert!(parse(0, 0, 1, u32::MAX - 1).is_none(), "the i32 overflow of the review");
    }

    #[test]
    fn a_slice_header_is_read_from_a_prefix_and_past_it_from_the_whole_nal() {
        let sps = parse_sps(&hex(SPS_1080P_HIGH)).expect("parses");
        let pps = parse_pps(&hex(PPS_1080P_HIGH)).expect("parses");
        let mut nal = hex(IDR_1080P_HIGH);
        nal.extend(std::iter::repeat_n(0x11, 3 * HEADER_PREFIX));
        let sh = parse_slice_header(&nal, NAL_SLICE_IDR, 3, &sps, &pps).expect("parses");
        assert_eq!((sh.first_mb_in_slice, sh.slice_type, sh.is_idr), (0, SliceType::I, true));
        assert_eq!((sh.frame_num, sh.idr_pic_id, sh.pic_order_cnt_lsb), (0, 0, 0));
        let whole =
            parse_slice_header_rbsp(&rbsp_of(&nal), NAL_SLICE_IDR, 3, &sps, &pps).expect("parses");
        assert_eq!(whole.header_bits_in_rbsp, sh.header_bits_in_rbsp);
        assert_eq!(whole.slice_qp_delta, sh.slice_qp_delta);
        let body = [0x88, 0x84, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x01, 0x11];
        let mut out = [0xAA; 6];
        assert_eq!(de_emulate(&body, &mut out), 6, "filled to the buffer's end");
        assert_eq!(out, [0x88, 0x84, 0x00, 0x00, 0x00, 0x00], "the 0x03 dropped");
        let mut out = [0xAA; 16];
        assert_eq!(de_emulate(&body, &mut out), 8, "a short body ends it");
        assert_eq!(&out[..8], &[0x88, 0x84, 0x00, 0x00, 0x00, 0x00, 0x01, 0x11]);
    }

    #[test]
    fn split_nals_into_reuses_the_list() {
        let au = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xCC, 0, 0, 0, 1, 0x65, 0x88, 0];
        let mut out = vec![Nal { ref_idc: 0, unit_type: 0, start: 9, end: 9 }];
        split_nals_into(&au, &mut out);
        assert_eq!(out.len(), 3, "cleared first");
        assert_eq!(out[0].raw(&au), &[0x67, 0xAA], "the 4-byte start code's zero is trimmed");
        assert_eq!(out[1].raw(&au), &[0x68, 0xCC]);
        assert_eq!(out[2].raw(&au), &[0x65, 0x88, 0], "the last NAL keeps its trailing zero");
        assert_eq!((out[2].unit_type, out[2].ref_idc), (NAL_SLICE_IDR, 3));
        assert_eq!(split_nals(&au), out);
        split_nals_into(&[0, 0, 1], &mut out);
        assert!(out.is_empty(), "an empty NAL is none");
    }

    #[test]
    fn a_truncated_vui_is_no_restriction() {
        let full = hex(SPS_1080P_HIGH);
        let sps = parse_sps(&full[..full.len() - 3]).expect("the SPS before the VUI parses");
        assert_eq!(sps.max_num_ref_frames, 4);
        assert_eq!(sps.max_num_reorder_frames, None, "a read past the end is not a value");
    }

    #[test]
    fn a_scaling_delta_past_its_range_is_malformed() {
        // se(v) of 2^31 - 1, 31 zeros, a one, then the 31 bit suffix 0x7FFFFFFE.
        let bits = [0, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFC];
        let mut list = [0u8; 16];
        assert_eq!(read_scaling_list(&mut BitReader::new(&bits), &mut list), None);
    }

    fn write_list(w: &mut crate::bitwriter::BitWriter, values: &[u8]) {
        let mut last = 8i32;
        for &v in values {
            let delta = (i32::from(v) - last + 128).rem_euclid(256) - 128;
            w.se(delta);
            last = i32::from(v);
        }
    }

    /// A High profile SPS of 320x240, scaling lists written by `lists` when given.
    fn high_sps(lists: Option<&dyn Fn(&mut crate::bitwriter::BitWriter)>) -> Vec<u8> {
        let mut w = crate::bitwriter::BitWriter::new();
        w.u(100, 8); // profile_idc
        w.u(0, 8); // constraint flags
        w.u(40, 8); // level_idc
        w.ue(0); // seq_parameter_set_id
        w.ue(1); // chroma_format_idc
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.flag(false); // qpprime_y_zero_transform_bypass_flag
        w.flag(lists.is_some()); // seq_scaling_matrix_present_flag
        if let Some(lists) = lists {
            lists(&mut w);
        }
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // pic_order_cnt_type
        w.ue(1); // max_num_ref_frames
        w.flag(false); // gaps_in_frame_num_value_allowed_flag
        w.ue(19); // pic_width_in_mbs_minus1
        w.ue(14); // pic_height_in_map_units_minus1
        w.flag(true); // frame_mbs_only_flag
        w.flag(true); // direct_8x8_inference_flag
        w.flag(false); // frame_cropping_flag
        w.flag(false); // vui_parameters_present_flag
        w.rbsp_trailing_bits();
        let mut nal = vec![0x67];
        nal.extend_from_slice(w.as_bytes());
        nal
    }

    /// A CABAC PPS with the 8x8 transform, scaling lists written by `lists` when given.
    fn pps_8x8(lists: Option<&dyn Fn(&mut crate::bitwriter::BitWriter)>) -> Vec<u8> {
        let mut w = crate::bitwriter::BitWriter::new();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        w.flag(true); // entropy_coding_mode_flag
        w.flag(false); // bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.flag(false); // weighted_pred_flag
        w.u(0, 2); // weighted_bipred_idc
        w.se(0); // pic_init_qp_minus26
        w.se(0); // pic_init_qs_minus26
        w.se(0); // chroma_qp_index_offset
        w.flag(true); // deblocking_filter_control_present_flag
        w.flag(false); // constrained_intra_pred_flag
        w.flag(false); // redundant_pic_cnt_present_flag
        w.flag(true); // transform_8x8_mode_flag
        w.flag(lists.is_some()); // pic_scaling_matrix_present_flag
        if let Some(lists) = lists {
            lists(&mut w);
        }
        w.se(3); // second_chroma_qp_index_offset
        w.rbsp_trailing_bits();
        let mut nal = vec![0x68];
        nal.extend_from_slice(w.as_bytes());
        nal
    }

    #[test]
    fn scaling_lists_follow_the_fall_back_rules() {
        let c4: [u8; 16] = std::array::from_fn(|i| 8 + i as u8);
        let c8: [u8; 64] = std::array::from_fn(|i| 1 + i as u8);
        let d4: [u8; 16] = std::array::from_fn(|i| 40 + i as u8);
        // SPS: list 0 coded, 1 and 2 absent, 3 absent, 4 coded as the default, 5 absent,
        // 6 coded, 7 absent.
        let sps = parse_sps(&high_sps(Some(&|w| {
            w.flag(true);
            write_list(w, &c4);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(true);
            w.se(-8); // useDefaultScalingMatrixFlag
            w.flag(false);
            w.flag(true);
            write_list(w, &c8);
            w.flag(false);
        })))
        .expect("parses");
        let s = sps.scaling.expect("present");
        assert_eq!((s.list4x4[0], s.list4x4[1], s.list4x4[2]), (c4, c4, c4), "rule A, previous");
        assert_eq!(s.list4x4[3], DEFAULT_4X4_INTER, "rule A, Default_4x4_Inter");
        assert_eq!((s.list4x4[4], s.list4x4[5]), (DEFAULT_4X4_INTER, DEFAULT_4X4_INTER));
        assert_eq!((s.list8x8[0], s.list8x8[1]), (c8, DEFAULT_8X8_INTER));
        assert_eq!(sps.pic_order_cnt_type, 2, "the fields after the lists are in place");

        let plain = parse_pps(&pps_8x8(None)).expect("parses");
        assert_eq!(plain.second_chroma_qp_index_offset, 3);
        assert_eq!(resolve_scaling(&sps, &plain), s, "a PPS without lists takes the SPS ones");

        // PPS: list 0 absent, 1 coded, 2 to 5 absent, 6 absent, 7 coded.
        let pps = parse_pps(&pps_8x8(Some(&|w| {
            w.flag(false);
            w.flag(true);
            write_list(w, &d4);
            for _ in 2..7 {
                w.flag(false);
            }
            w.flag(true);
            write_list(w, &c8);
        })))
        .expect("parses");
        assert_eq!(pps.second_chroma_qp_index_offset, 3, "the 4:2:0 count is 6 + 2");
        let r = resolve_scaling(&sps, &pps);
        assert_eq!(r.list4x4[0], c4, "rule B, the SPS list");
        assert_eq!((r.list4x4[1], r.list4x4[2]), (d4, d4));
        assert_eq!((r.list4x4[3], r.list4x4[4]), (DEFAULT_4X4_INTER, DEFAULT_4X4_INTER));
        assert_eq!((r.list8x8[0], r.list8x8[1]), (c8, c8), "rule B SPS 8x8 intra, coded inter");

        let no_lists = parse_sps(&high_sps(None)).expect("parses");
        assert_eq!(no_lists.scaling, None);
        assert_eq!(resolve_scaling(&no_lists, &plain), ScalingLists::FLAT);
        let r = resolve_scaling(&no_lists, &pps);
        assert_eq!(r.list4x4[0], DEFAULT_4X4_INTRA, "rule A without SPS lists");
        assert_eq!(r.list8x8[0], DEFAULT_8X8_INTRA);

        let raster = raster_4x4(&DEFAULT_4X4_INTRA);
        assert_eq!(raster, [6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42]);
        assert_eq!(&raster_8x8(&DEFAULT_8X8_INTRA)[..8], &[6, 10, 13, 16, 18, 23, 25, 27]);
    }
}
