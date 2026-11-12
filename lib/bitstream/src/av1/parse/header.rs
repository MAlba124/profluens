use super::obu::{Av1Error, Bits};
use super::refs::RefFrame;
use super::seq::{SELECT, SequenceHeader};

pub const NUM_REF_FRAMES: usize = 8;
pub const REFS_PER_FRAME: usize = 7;
pub const PRIMARY_REF_NONE: u8 = 7;
pub const ALL_FRAMES: u8 = 0xFF;
pub const SWITCHABLE: u8 = 4;
pub const ONLY_4X4: u8 = 0;
pub const TX_MODE_LARGEST: u8 = 1;
pub const TX_MODE_SELECT: u8 = 2;
pub const IDENTITY: u8 = 0;
pub const TRANSLATION: u8 = 1;
pub const ROTZOOM: u8 = 2;
pub const AFFINE: u8 = 3;
pub const RESTORE_NONE: u8 = 0;
pub const RESTORE_WIENER: u8 = 1;
pub const RESTORE_SGRPROJ: u8 = 2;
pub const RESTORE_SWITCHABLE: u8 = 3;
const REMAP_LR_TYPE: [u8; 4] = [RESTORE_NONE, RESTORE_SWITCHABLE, RESTORE_WIENER, RESTORE_SGRPROJ];

const SUPERRES_NUM: u32 = 8;
const SUPERRES_DENOM_MIN: u32 = 9;
const MAX_TILE_WIDTH: u32 = 4096;
const MAX_TILE_AREA: u32 = 4096 * 2304;
const MAX_TILE_ROWS: u32 = 64;
const MAX_TILE_COLS: u32 = 64;
const WARPEDMODEL_PREC_BITS: u32 = 16;
const GM_ABS_TRANS_BITS: u32 = 12;
const GM_ABS_TRANS_ONLY_BITS: u32 = 9;
const GM_ABS_ALPHA_BITS: u32 = 12;
const GM_ALPHA_PREC_BITS: u32 = 15;
const GM_TRANS_PREC_BITS: u32 = 6;
const GM_TRANS_ONLY_PREC_BITS: u32 = 3;
const WARP_PARAM_REDUCE_BITS: u32 = 6;
const DIV_LUT_BITS: u32 = 8;
const DIV_LUT_PREC_BITS: u32 = 14;
pub const MAX_SEGMENTS: usize = 8;
pub const SEG_LVL_MAX: usize = 8;
pub const SEG_LVL_ALT_Q: usize = 0;
const SEGMENTATION_FEATURE_BITS: [u32; SEG_LVL_MAX] = [8, 6, 6, 6, 6, 3, 0, 0];
const SEGMENTATION_FEATURE_SIGNED: [bool; SEG_LVL_MAX] =
    [true, true, true, true, true, false, false, false];
pub const SEGMENTATION_FEATURE_MAX: [i16; SEG_LVL_MAX] = [255, 63, 63, 63, 63, 7, 0, 0];
const DEFAULT_LF_REF_DELTAS: [i8; 8] = [1, 0, 0, 0, -1, 0, -1, -1];
pub const DEFAULT_GM_PARAMS: [[i32; 6]; 8] =
    [[0, 0, 1 << WARPEDMODEL_PREC_BITS, 0, 0, 1 << WARPEDMODEL_PREC_BITS]; 8];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    Key = 0,
    Inter = 1,
    IntraOnly = 2,
    Switch = 3,
}

impl FrameType {
    fn of(v: u8) -> FrameType {
        match v & 3 {
            0 => FrameType::Key,
            1 => FrameType::Inter,
            2 => FrameType::IntraOnly,
            _ => FrameType::Switch,
        }
    }

    pub fn is_intra(self) -> bool {
        matches!(self, FrameType::Key | FrameType::IntraOnly)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileInfo {
    pub uniform: bool,
    pub cols: u16,
    pub rows: u16,
    pub cols_log2: u8,
    pub rows_log2: u8,
    pub width_in_sbs: [u16; 64],
    pub height_in_sbs: [u16; 64],
    pub context_update_tile_id: u16,
    pub size_bytes: u8,
}

impl TileInfo {
    const ONE: TileInfo = TileInfo {
        uniform: true,
        cols: 1,
        rows: 1,
        cols_log2: 0,
        rows_log2: 0,
        width_in_sbs: [0; 64],
        height_in_sbs: [0; 64],
        context_update_tile_id: 0,
        size_bytes: 4,
    };

    pub fn layout(&self) -> super::obu::TileLayout {
        super::obu::TileLayout {
            cols: self.cols,
            rows: self.rows,
            cols_log2: self.cols_log2,
            rows_log2: self.rows_log2,
            size_bytes: self.size_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quant {
    pub base_q_idx: u8,
    pub delta_q_y_dc: i8,
    pub delta_q_u_dc: i8,
    pub delta_q_u_ac: i8,
    pub delta_q_v_dc: i8,
    pub delta_q_v_ac: i8,
    pub using_qmatrix: bool,
    pub qm_y: u8,
    pub qm_u: u8,
    pub qm_v: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segmentation {
    pub enabled: bool,
    pub update_map: bool,
    pub temporal_update: bool,
    pub update_data: bool,
    pub feature_enabled: [u8; MAX_SEGMENTS],
    pub feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopFilter {
    pub level: [u8; 4],
    pub sharpness: u8,
    pub delta_enabled: bool,
    pub delta_update: bool,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cdef {
    pub damping_minus_3: u8,
    pub bits: u8,
    pub y_pri: [u8; 8],
    pub y_sec: [u8; 8],
    pub uv_pri: [u8; 8],
    pub uv_sec: [u8; 8],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restoration {
    pub frame_type: [u8; 3],
    pub unit_shift: u8,
    pub uv_shift: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlobalMotion {
    pub gm_type: [u8; 8],
    pub params: [[i32; 6]; 8],
    /// The shear of section 7.11.3.6 is not valid.
    pub invalid: [bool; 8],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilmGrain {
    pub apply_grain: bool,
    pub grain_seed: u16,
    pub update_grain: bool,
    pub film_grain_params_ref_idx: u8,
    pub num_y_points: u8,
    pub point_y_value: [u8; 14],
    pub point_y_scaling: [u8; 14],
    pub chroma_scaling_from_luma: bool,
    pub num_cb_points: u8,
    pub point_cb_value: [u8; 10],
    pub point_cb_scaling: [u8; 10],
    pub num_cr_points: u8,
    pub point_cr_value: [u8; 10],
    pub point_cr_scaling: [u8; 10],
    pub grain_scaling_minus_8: u8,
    pub ar_coeff_lag: u8,
    pub ar_coeffs_y_plus_128: [u8; 24],
    pub ar_coeffs_cb_plus_128: [u8; 25],
    pub ar_coeffs_cr_plus_128: [u8; 25],
    pub ar_coeff_shift_minus_6: u8,
    pub grain_scale_shift: u8,
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cb_offset: u16,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub cr_offset: u16,
    pub overlap_flag: bool,
    pub clip_to_restricted_range: bool,
}

impl FilmGrain {
    pub const NONE: FilmGrain = FilmGrain {
        apply_grain: false,
        grain_seed: 0,
        update_grain: false,
        film_grain_params_ref_idx: 0,
        num_y_points: 0,
        point_y_value: [0; 14],
        point_y_scaling: [0; 14],
        chroma_scaling_from_luma: false,
        num_cb_points: 0,
        point_cb_value: [0; 10],
        point_cb_scaling: [0; 10],
        num_cr_points: 0,
        point_cr_value: [0; 10],
        point_cr_scaling: [0; 10],
        grain_scaling_minus_8: 0,
        ar_coeff_lag: 0,
        ar_coeffs_y_plus_128: [0; 24],
        ar_coeffs_cb_plus_128: [0; 25],
        ar_coeffs_cr_plus_128: [0; 25],
        ar_coeff_shift_minus_6: 0,
        grain_scale_shift: 0,
        cb_mult: 0,
        cb_luma_mult: 0,
        cb_offset: 0,
        cr_mult: 0,
        cr_luma_mult: 0,
        cr_offset: 0,
        overlap_flag: false,
        clip_to_restricted_range: false,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub show_existing_frame: bool,
    pub frame_to_show_map_idx: u8,
    pub frame_type: FrameType,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub error_resilient_mode: bool,
    pub disable_cdf_update: bool,
    pub allow_screen_content_tools: bool,
    /// With `FrameIsIntra` folded in, as the decoding process uses it.
    pub force_integer_mv: bool,
    pub current_frame_id: u32,
    pub frame_size_override_flag: bool,
    pub order_hint: u8,
    pub primary_ref_frame: u8,
    pub refresh_frame_flags: u8,
    pub ref_order_hint: Option<[u8; NUM_REF_FRAMES]>,
    pub frame_width: u32,
    pub frame_height: u32,
    pub upscaled_width: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub use_superres: bool,
    pub superres_denom: u8,
    pub allow_intrabc: bool,
    pub frame_refs_short_signaling: bool,
    pub ref_frame_idx: [u8; REFS_PER_FRAME],
    pub allow_high_precision_mv: bool,
    pub interpolation_filter: u8,
    pub is_motion_mode_switchable: bool,
    pub use_ref_frame_mvs: bool,
    pub order_hints: [u8; 8],
    pub ref_frame_sign_bias: [bool; 8],
    pub disable_frame_end_update_cdf: bool,
    pub tile_info: TileInfo,
    pub quant: Quant,
    pub seg: Segmentation,
    pub delta_q_present: bool,
    pub delta_q_res: u8,
    pub delta_lf_present: bool,
    pub delta_lf_res: u8,
    pub delta_lf_multi: bool,
    pub coded_lossless: bool,
    pub all_lossless: bool,
    pub lf: LoopFilter,
    pub cdef: Cdef,
    pub lr: Restoration,
    pub tx_mode: u8,
    pub reference_select: bool,
    pub skip_mode_present: bool,
    pub allow_warped_motion: bool,
    pub reduced_tx_set: bool,
    pub gm: GlobalMotion,
    pub film_grain: FilmGrain,
    pub header_bits: usize,
}

impl FrameHeader {
    const EMPTY: FrameHeader = FrameHeader {
        show_existing_frame: false,
        frame_to_show_map_idx: 0,
        frame_type: FrameType::Key,
        show_frame: true,
        showable_frame: false,
        error_resilient_mode: false,
        disable_cdf_update: false,
        allow_screen_content_tools: false,
        force_integer_mv: false,
        current_frame_id: 0,
        frame_size_override_flag: false,
        order_hint: 0,
        primary_ref_frame: PRIMARY_REF_NONE,
        refresh_frame_flags: 0,
        ref_order_hint: None,
        frame_width: 0,
        frame_height: 0,
        upscaled_width: 0,
        render_width: 0,
        render_height: 0,
        use_superres: false,
        superres_denom: SUPERRES_NUM as u8,
        allow_intrabc: false,
        frame_refs_short_signaling: false,
        ref_frame_idx: [0; REFS_PER_FRAME],
        allow_high_precision_mv: false,
        interpolation_filter: 0,
        is_motion_mode_switchable: false,
        use_ref_frame_mvs: false,
        order_hints: [0; 8],
        ref_frame_sign_bias: [false; 8],
        disable_frame_end_update_cdf: true,
        tile_info: TileInfo::ONE,
        quant: Quant {
            base_q_idx: 0,
            delta_q_y_dc: 0,
            delta_q_u_dc: 0,
            delta_q_u_ac: 0,
            delta_q_v_dc: 0,
            delta_q_v_ac: 0,
            using_qmatrix: false,
            qm_y: 0,
            qm_u: 0,
            qm_v: 0,
        },
        seg: Segmentation {
            enabled: false,
            update_map: false,
            temporal_update: false,
            update_data: false,
            feature_enabled: [0; MAX_SEGMENTS],
            feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
        },
        delta_q_present: false,
        delta_q_res: 0,
        delta_lf_present: false,
        delta_lf_res: 0,
        delta_lf_multi: false,
        coded_lossless: false,
        all_lossless: false,
        lf: LoopFilter {
            level: [0; 4],
            sharpness: 0,
            delta_enabled: false,
            delta_update: false,
            ref_deltas: DEFAULT_LF_REF_DELTAS,
            mode_deltas: [0; 2],
        },
        cdef: Cdef {
            damping_minus_3: 0,
            bits: 0,
            y_pri: [0; 8],
            y_sec: [0; 8],
            uv_pri: [0; 8],
            uv_sec: [0; 8],
        },
        lr: Restoration { frame_type: [RESTORE_NONE; 3], unit_shift: 0, uv_shift: 0 },
        tx_mode: ONLY_4X4,
        reference_select: false,
        skip_mode_present: false,
        allow_warped_motion: false,
        reduced_tx_set: false,
        gm: GlobalMotion { gm_type: [IDENTITY; 8], params: DEFAULT_GM_PARAMS, invalid: [false; 8] },
        film_grain: FilmGrain::NONE,
        header_bits: 0,
    };

    pub fn is_intra(&self) -> bool {
        self.frame_type.is_intra()
    }

    /// The tile group of a frame OBU starts at this byte, the header byte aligned.
    pub fn header_bytes(&self) -> usize {
        self.header_bits.div_ceil(8)
    }
}

pub fn relative_dist(seq: &SequenceHeader, a: u8, b: u8) -> i32 {
    if !seq.enable_order_hint {
        return 0;
    }
    let bits = u32::from(seq.order_hint_bits);
    let diff = i32::from(a) - i32::from(b);
    let m = 1i32 << (bits - 1);
    (diff & (m - 1)) - (diff & m)
}

fn tile_log2(blk: u32, target: u32) -> u32 {
    let mut k = 0;
    while (blk << k) < target {
        k += 1;
    }
    k
}

struct Previous {
    gm_params: [[i32; 6]; 8],
    lf_ref_deltas: [i8; 8],
    lf_mode_deltas: [i8; 2],
    feature_enabled: [u8; MAX_SEGMENTS],
    feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
}

/// `uncompressed_header()`, section 5.9.2.
pub fn parse_frame_header(
    payload: &[u8],
    seq: &SequenceHeader,
    refs: &[RefFrame; NUM_REF_FRAMES],
    temporal_id: u8,
    spatial_id: u8,
) -> Result<FrameHeader, Av1Error> {
    let r = &mut Bits::new(payload);
    let mut h = FrameHeader::EMPTY;
    let id_len = seq.id_len();
    if seq.reduced_still_picture_header {
        h.error_resilient_mode = true;
    } else {
        h.show_existing_frame = r.flag()?;
        if h.show_existing_frame {
            return show_existing(r, seq, refs, h);
        }
        h.frame_type = FrameType::of(r.u8(2)?);
        h.show_frame = r.flag()?;
        if h.show_frame && seq.decoder_model_info_present_flag && !seq.equal_picture_interval {
            r.f(u32::from(seq.frame_presentation_time_length_minus_1) + 1)?;
        }
        h.showable_frame = if h.show_frame { h.frame_type != FrameType::Key } else { r.flag()? };
        h.error_resilient_mode = match h.frame_type {
            FrameType::Switch => true,
            FrameType::Key if h.show_frame => true,
            _ => r.flag()?,
        };
    }
    let intra = h.is_intra();
    let mut ref_hint = [0u8; NUM_REF_FRAMES];
    for (hint, slot) in ref_hint.iter_mut().zip(refs) {
        *hint = slot.order_hint;
    }
    if h.frame_type == FrameType::Key && h.show_frame {
        ref_hint = [0; NUM_REF_FRAMES];
    }
    h.disable_cdf_update = r.flag()?;
    h.allow_screen_content_tools = match seq.seq_force_screen_content_tools {
        SELECT => r.flag()?,
        v => v == 1,
    };
    if h.allow_screen_content_tools {
        h.force_integer_mv = match seq.seq_force_integer_mv {
            SELECT => r.flag()?,
            v => v == 1,
        };
    }
    if intra {
        h.force_integer_mv = true;
    }
    if seq.frame_id_numbers_present_flag {
        h.current_frame_id = r.f(id_len)?;
    }
    h.frame_size_override_flag = match h.frame_type {
        FrameType::Switch => true,
        _ if seq.reduced_still_picture_header => false,
        _ => r.flag()?,
    };
    h.order_hint = r.f(u32::from(seq.order_hint_bits))? as u8;
    h.primary_ref_frame = if intra || h.error_resilient_mode { PRIMARY_REF_NONE } else { r.u8(3)? };
    if seq.decoder_model_info_present_flag && r.flag()? {
        buffer_removal_times(r, seq, temporal_id, spatial_id)?;
    }
    h.refresh_frame_flags = match h.frame_type {
        FrameType::Switch => ALL_FRAMES,
        FrameType::Key if h.show_frame => ALL_FRAMES,
        _ => r.u8(8)?,
    };
    if (!intra || h.refresh_frame_flags != ALL_FRAMES)
        && h.error_resilient_mode
        && seq.enable_order_hint
    {
        let mut hints = [0u8; NUM_REF_FRAMES];
        for (coded, held) in hints.iter_mut().zip(ref_hint.iter_mut()) {
            *coded = r.f(u32::from(seq.order_hint_bits))? as u8;
            *held = *coded;
        }
        h.ref_order_hint = Some(hints);
    }
    if intra {
        frame_size(r, seq, &mut h)?;
        render_size(r, &mut h)?;
        if h.allow_screen_content_tools && h.upscaled_width == h.frame_width {
            h.allow_intrabc = r.flag()?;
        }
    } else {
        inter_refs(r, seq, refs, &ref_hint, &mut h)?;
    }
    h.disable_frame_end_update_cdf =
        seq.reduced_still_picture_header || h.disable_cdf_update || r.flag()?;
    let prev = previous(refs, &h)?;
    h.tile_info = tile_info(r, seq, &h)?;
    h.quant = quantization_params(r, seq)?;
    h.seg = segmentation_params(r, &h, &prev)?;
    if h.quant.base_q_idx > 0 {
        h.delta_q_present = r.flag()?;
    }
    if h.delta_q_present {
        h.delta_q_res = r.u8(2)?;
        if !h.allow_intrabc {
            h.delta_lf_present = r.flag()?;
        }
        if h.delta_lf_present {
            h.delta_lf_res = r.u8(2)?;
            h.delta_lf_multi = r.flag()?;
        }
    }
    h.coded_lossless = coded_lossless(&h);
    h.all_lossless = h.coded_lossless && h.frame_width == h.upscaled_width;
    h.lf = loop_filter_params(r, seq, &h, &prev)?;
    h.cdef = cdef_params(r, seq, &h)?;
    h.lr = lr_params(r, seq, &h)?;
    h.tx_mode = match h.coded_lossless {
        true => ONLY_4X4,
        false if r.flag()? => TX_MODE_SELECT,
        false => TX_MODE_LARGEST,
    };
    h.reference_select = !intra && r.flag()?;
    if skip_mode_allowed(seq, &h, &ref_hint) {
        h.skip_mode_present = r.flag()?;
    }
    if !intra && !h.error_resilient_mode && seq.enable_warped_motion {
        h.allow_warped_motion = r.flag()?;
    }
    h.reduced_tx_set = r.flag()?;
    h.gm = global_motion_params(r, &h, &prev.gm_params)?;
    h.film_grain = film_grain_params(r, seq, refs, &h)?;
    h.header_bits = r.pos();
    Ok(h)
}

/// A key frame shown again refreshes every slot (section 7.21).
fn show_existing(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    refs: &[RefFrame; NUM_REF_FRAMES],
    mut h: FrameHeader,
) -> Result<FrameHeader, Av1Error> {
    h.frame_to_show_map_idx = r.u8(3)?;
    if seq.decoder_model_info_present_flag && !seq.equal_picture_interval {
        r.f(u32::from(seq.frame_presentation_time_length_minus_1) + 1)?;
    }
    if seq.frame_id_numbers_present_flag {
        r.f(seq.id_len())?;
    }
    let slot = &refs[usize::from(h.frame_to_show_map_idx)];
    if !slot.valid {
        return Err(Av1Error::MissingRef(h.frame_to_show_map_idx));
    }
    h.frame_type = slot.frame_type;
    h.show_frame = true;
    h.refresh_frame_flags = if h.frame_type == FrameType::Key { ALL_FRAMES } else { 0 };
    (h.upscaled_width, h.frame_width, h.frame_height) =
        (slot.upscaled_width, slot.frame_width, slot.frame_height);
    (h.render_width, h.render_height) = (slot.render_width, slot.render_height);
    h.order_hint = slot.order_hint;
    if seq.film_grain_params_present {
        h.film_grain = slot.film_grain;
    }
    h.header_bits = r.pos();
    Ok(h)
}

fn buffer_removal_times(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    temporal_id: u8,
    spatial_id: u8,
) -> Result<(), Av1Error> {
    for op in 0..=usize::from(seq.operating_points_cnt_minus_1) {
        if !seq.decoder_model_present_for_this_op[op] {
            continue;
        }
        let idc = seq.operating_point_idc[op];
        let in_temporal = idc >> temporal_id & 1 == 1;
        let in_spatial = idc >> (u16::from(spatial_id) + 8) & 1 == 1;
        if idc == 0 || (in_temporal && in_spatial) {
            r.f(u32::from(seq.buffer_removal_time_length_minus_1) + 1)?;
        }
    }
    Ok(())
}

fn inter_refs(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    refs: &[RefFrame; NUM_REF_FRAMES],
    ref_hint: &[u8; NUM_REF_FRAMES],
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    if seq.enable_order_hint {
        h.frame_refs_short_signaling = r.flag()?;
    }
    if h.frame_refs_short_signaling {
        let last = r.u8(3)?;
        let gold = r.u8(3)?;
        h.ref_frame_idx = set_frame_refs(seq, last, gold, ref_hint, h.order_hint);
    }
    for i in 0..REFS_PER_FRAME {
        if !h.frame_refs_short_signaling {
            h.ref_frame_idx[i] = r.u8(3)?;
        }
        if seq.frame_id_numbers_present_flag {
            r.f(u32::from(seq.delta_frame_id_length_minus_2) + 2)?;
        }
    }
    if h.frame_size_override_flag && !h.error_resilient_mode {
        frame_size_with_refs(r, seq, refs, h)?;
    } else {
        frame_size(r, seq, h)?;
        render_size(r, h)?;
    }
    h.allow_high_precision_mv = !h.force_integer_mv && r.flag()?;
    h.interpolation_filter = if r.flag()? { SWITCHABLE } else { r.u8(2)? };
    h.is_motion_mode_switchable = r.flag()?;
    h.use_ref_frame_mvs = !h.error_resilient_mode && seq.enable_ref_frame_mvs && r.flag()?;
    for i in 0..REFS_PER_FRAME {
        let hint = ref_hint[usize::from(h.ref_frame_idx[i])];
        h.order_hints[i + 1] = hint;
        h.ref_frame_sign_bias[i + 1] =
            seq.enable_order_hint && relative_dist(seq, hint, h.order_hint) > 0;
    }
    Ok(())
}

/// `set_frame_refs()`, section 7.8.
pub fn set_frame_refs(
    seq: &SequenceHeader,
    last: u8,
    gold: u8,
    ref_hint: &[u8; NUM_REF_FRAMES],
    order_hint: u8,
) -> [u8; REFS_PER_FRAME] {
    const NONE: i32 = -1;
    let mut idx = [NONE; REFS_PER_FRAME];
    idx[0] = i32::from(last);
    idx[3] = i32::from(gold);
    let mut used = [false; NUM_REF_FRAMES];
    used[usize::from(last & 7)] = true;
    used[usize::from(gold & 7)] = true;
    let cur = 1i32 << (seq.order_hint_bits.max(1) - 1);
    let mut shifted = [0i32; NUM_REF_FRAMES];
    for (s, &hint) in shifted.iter_mut().zip(ref_hint) {
        *s = cur + relative_dist(seq, hint, order_hint);
    }
    let find = |used: &[bool; NUM_REF_FRAMES], backward: bool, latest: bool| {
        let mut best = NONE;
        let mut best_hint = 0;
        for (i, &hint) in shifted.iter().enumerate() {
            let side = if backward { hint >= cur } else { hint < cur };
            let better = if latest { hint >= best_hint } else { hint < best_hint };
            if !used[i] && side && (best < 0 || better) {
                (best, best_hint) = (i as i32, hint);
            }
        }
        best
    };
    // ALTREF_FRAME, BWDREF_FRAME, ALTREF2_FRAME.
    for (name, latest) in [(6usize, true), (4, false), (5, false)] {
        let r = find(&used, true, latest);
        if r >= 0 {
            idx[name] = r;
            used[r as usize] = true;
        }
    }
    // Ref_Frame_List, LAST2_FRAME to ALTREF_FRAME.
    for name in [1usize, 2, 4, 5, 6] {
        if idx[name] < 0 {
            let r = find(&used, false, true);
            if r >= 0 {
                idx[name] = r;
                used[r as usize] = true;
            }
        }
    }
    let mut earliest = NONE;
    let mut earliest_hint = 0;
    for (i, &hint) in shifted.iter().enumerate() {
        if earliest < 0 || hint < earliest_hint {
            (earliest, earliest_hint) = (i as i32, hint);
        }
    }
    let mut out = [0u8; REFS_PER_FRAME];
    for (o, &i) in out.iter_mut().zip(&idx) {
        *o = if i < 0 { earliest as u8 } else { i as u8 };
    }
    out
}

fn superres_params(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    h.use_superres = seq.enable_superres && r.flag()?;
    let denom = if h.use_superres { r.f(3)? + SUPERRES_DENOM_MIN } else { SUPERRES_NUM };
    h.superres_denom = denom as u8;
    h.upscaled_width = h.frame_width;
    h.frame_width = (h.upscaled_width * SUPERRES_NUM + denom / 2) / denom;
    Ok(())
}

fn frame_size(r: &mut Bits<'_>, seq: &SequenceHeader, h: &mut FrameHeader) -> Result<(), Av1Error> {
    if h.frame_size_override_flag {
        h.frame_width = r.f(u32::from(seq.frame_width_bits))? + 1;
        h.frame_height = r.f(u32::from(seq.frame_height_bits))? + 1;
        // 5.9.5, the override never exceeds the sequence maximum the surfaces are sized for.
        if h.frame_width > seq.max_frame_width || h.frame_height > seq.max_frame_height {
            return Err(Av1Error::Invalid("frame_size"));
        }
    } else {
        (h.frame_width, h.frame_height) = (seq.max_frame_width, seq.max_frame_height);
    }
    superres_params(r, seq, h)
}

fn render_size(r: &mut Bits<'_>, h: &mut FrameHeader) -> Result<(), Av1Error> {
    if r.flag()? {
        h.render_width = r.f(16)? + 1;
        h.render_height = r.f(16)? + 1;
    } else {
        (h.render_width, h.render_height) = (h.upscaled_width, h.frame_height);
    }
    Ok(())
}

fn frame_size_with_refs(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    refs: &[RefFrame; NUM_REF_FRAMES],
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    for i in 0..REFS_PER_FRAME {
        if !r.flag()? {
            continue;
        }
        let at = h.ref_frame_idx[i];
        let s = &refs[usize::from(at)];
        if !s.valid {
            return Err(Av1Error::MissingRef(at));
        }
        h.frame_width = s.upscaled_width;
        h.frame_height = s.frame_height;
        (h.render_width, h.render_height) = (s.render_width, s.render_height);
        return superres_params(r, seq, h);
    }
    frame_size(r, seq, h)?;
    render_size(r, h)
}

fn tile_info(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    h: &FrameHeader,
) -> Result<TileInfo, Av1Error> {
    let mi_cols = 2 * h.frame_width.div_ceil(8);
    let mi_rows = 2 * h.frame_height.div_ceil(8);
    let (sb_cols, sb_rows, sb_shift) = match seq.use_128x128_superblock {
        true => (mi_cols.div_ceil(32), mi_rows.div_ceil(32), 5),
        false => (mi_cols.div_ceil(16), mi_rows.div_ceil(16), 4),
    };
    let sb_size = sb_shift + 2;
    let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
    let mut max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
    let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
    let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
    let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
    let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
    let mut t = TileInfo::ONE;
    t.uniform = r.flag()?;
    if t.uniform {
        let mut cols_log2 = min_log2_tile_cols;
        while cols_log2 < max_log2_tile_cols && r.flag()? {
            cols_log2 += 1;
        }
        let tile_width_sb = (sb_cols + (1 << cols_log2) - 1) >> cols_log2;
        t.cols = uniform_sizes(sb_cols, tile_width_sb, &mut t.width_in_sbs);
        let min_log2_tile_rows = min_log2_tiles.saturating_sub(cols_log2);
        let mut rows_log2 = min_log2_tile_rows;
        while rows_log2 < max_log2_tile_rows && r.flag()? {
            rows_log2 += 1;
        }
        let tile_height_sb = (sb_rows + (1 << rows_log2) - 1) >> rows_log2;
        t.rows = uniform_sizes(sb_rows, tile_height_sb, &mut t.height_in_sbs);
        (t.cols_log2, t.rows_log2) = (cols_log2 as u8, rows_log2 as u8);
    } else {
        let mut widest_tile_sb = 0;
        let mut start = 0;
        let mut i = 0;
        while start < sb_cols {
            if i >= MAX_TILE_COLS as usize {
                return Err(Av1Error::Invalid("tile columns"));
            }
            let max_width = (sb_cols - start).min(max_tile_width_sb);
            let size = r.ns(max_width)? + 1;
            t.width_in_sbs[i] = size as u16;
            widest_tile_sb = widest_tile_sb.max(size);
            start += size;
            i += 1;
        }
        t.cols = i as u16;
        t.cols_log2 = tile_log2(1, i as u32) as u8;
        max_tile_area_sb = match min_log2_tiles {
            0 => sb_rows * sb_cols,
            n => (sb_rows * sb_cols) >> (n + 1),
        };
        let max_tile_height_sb = (max_tile_area_sb / widest_tile_sb.max(1)).max(1);
        let mut start = 0;
        let mut i = 0;
        while start < sb_rows {
            if i >= MAX_TILE_ROWS as usize {
                return Err(Av1Error::Invalid("tile rows"));
            }
            let max_height = (sb_rows - start).min(max_tile_height_sb);
            let size = r.ns(max_height)? + 1;
            t.height_in_sbs[i] = size as u16;
            start += size;
            i += 1;
        }
        t.rows = i as u16;
        t.rows_log2 = tile_log2(1, i as u32) as u8;
    }
    if t.cols_log2 > 0 || t.rows_log2 > 0 {
        t.context_update_tile_id = r.f(u32::from(t.cols_log2) + u32::from(t.rows_log2))? as u16;
        t.size_bytes = r.u8(2)? + 1;
        if u32::from(t.context_update_tile_id) >= u32::from(t.cols) * u32::from(t.rows) {
            return Err(Av1Error::Invalid("context_update_tile_id"));
        }
    }
    Ok(t)
}

fn uniform_sizes(total: u32, size: u32, out: &mut [u16; 64]) -> u16 {
    let mut n = 0usize;
    let mut start = 0;
    while start < total && n < out.len() {
        out[n] = size.min(total - start) as u16;
        start += size;
        n += 1;
    }
    n as u16
}

fn delta_q(r: &mut Bits<'_>) -> Result<i8, Av1Error> {
    Ok(if r.flag()? { r.su(7)? as i8 } else { 0 })
}

fn quantization_params(r: &mut Bits<'_>, seq: &SequenceHeader) -> Result<Quant, Av1Error> {
    let mut q = Quant { base_q_idx: r.u8(8)?, delta_q_y_dc: delta_q(r)?, ..Quant::default() };
    if seq.color.num_planes() > 1 {
        let diff_uv_delta = seq.color.separate_uv_delta_q && r.flag()?;
        q.delta_q_u_dc = delta_q(r)?;
        q.delta_q_u_ac = delta_q(r)?;
        if diff_uv_delta {
            q.delta_q_v_dc = delta_q(r)?;
            q.delta_q_v_ac = delta_q(r)?;
        } else {
            (q.delta_q_v_dc, q.delta_q_v_ac) = (q.delta_q_u_dc, q.delta_q_u_ac);
        }
    }
    q.using_qmatrix = r.flag()?;
    if q.using_qmatrix {
        q.qm_y = r.u8(4)?;
        q.qm_u = r.u8(4)?;
        q.qm_v = if seq.color.separate_uv_delta_q { r.u8(4)? } else { q.qm_u };
    }
    Ok(q)
}

fn previous(refs: &[RefFrame; NUM_REF_FRAMES], h: &FrameHeader) -> Result<Previous, Av1Error> {
    if h.primary_ref_frame == PRIMARY_REF_NONE {
        return Ok(Previous {
            gm_params: DEFAULT_GM_PARAMS,
            lf_ref_deltas: DEFAULT_LF_REF_DELTAS,
            lf_mode_deltas: [0; 2],
            feature_enabled: [0; MAX_SEGMENTS],
            feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
        });
    }
    let at = h.ref_frame_idx[usize::from(h.primary_ref_frame & 7).min(REFS_PER_FRAME - 1)];
    let p = &refs[usize::from(at)];
    if !p.valid {
        return Err(Av1Error::MissingRef(at));
    }
    Ok(Previous {
        gm_params: p.gm_params,
        lf_ref_deltas: p.lf_ref_deltas,
        lf_mode_deltas: p.lf_mode_deltas,
        feature_enabled: p.feature_enabled,
        feature_data: p.feature_data,
    })
}

fn segmentation_params(
    r: &mut Bits<'_>,
    h: &FrameHeader,
    prev: &Previous,
) -> Result<Segmentation, Av1Error> {
    let mut s = Segmentation { enabled: r.flag()?, ..Segmentation::default() };
    if !s.enabled {
        return Ok(s);
    }
    if h.primary_ref_frame == PRIMARY_REF_NONE {
        (s.update_map, s.temporal_update, s.update_data) = (true, false, true);
    } else {
        s.update_map = r.flag()?;
        s.temporal_update = s.update_map && r.flag()?;
        s.update_data = r.flag()?;
    }
    if !s.update_data {
        (s.feature_enabled, s.feature_data) = (prev.feature_enabled, prev.feature_data);
        return Ok(s);
    }
    for i in 0..MAX_SEGMENTS {
        for j in 0..SEG_LVL_MAX {
            if !r.flag()? {
                continue;
            }
            s.feature_enabled[i] |= 1 << j;
            let bits = SEGMENTATION_FEATURE_BITS[j];
            let limit = SEGMENTATION_FEATURE_MAX[j];
            s.feature_data[i][j] = match SEGMENTATION_FEATURE_SIGNED[j] {
                true => (r.su(1 + bits)? as i16).clamp(-limit, limit),
                false => (r.f(bits)? as i16).clamp(0, limit),
            };
        }
    }
    Ok(s)
}

/// `get_qindex(1, segmentId)` of section 7.12.2.
pub fn qindex(h: &FrameHeader, segment: usize) -> i32 {
    let base = i32::from(h.quant.base_q_idx);
    if h.seg.enabled && h.seg.feature_enabled[segment] & 1 << SEG_LVL_ALT_Q != 0 {
        return (base + i32::from(h.seg.feature_data[segment][SEG_LVL_ALT_Q])).clamp(0, 255);
    }
    base
}

fn coded_lossless(h: &FrameHeader) -> bool {
    let q = &h.quant;
    let deltas = [q.delta_q_y_dc, q.delta_q_u_ac, q.delta_q_u_dc, q.delta_q_v_ac, q.delta_q_v_dc];
    deltas.iter().all(|&d| d == 0) && (0..MAX_SEGMENTS).all(|s| qindex(h, s) == 0)
}

fn loop_filter_params(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    h: &FrameHeader,
    prev: &Previous,
) -> Result<LoopFilter, Av1Error> {
    let mut lf = LoopFilter {
        ref_deltas: prev.lf_ref_deltas,
        mode_deltas: prev.lf_mode_deltas,
        ..LoopFilter::default()
    };
    if h.coded_lossless || h.allow_intrabc {
        (lf.ref_deltas, lf.mode_deltas) = (DEFAULT_LF_REF_DELTAS, [0; 2]);
        return Ok(lf);
    }
    lf.level[0] = r.u8(6)?;
    lf.level[1] = r.u8(6)?;
    if seq.color.num_planes() > 1 && (lf.level[0] != 0 || lf.level[1] != 0) {
        lf.level[2] = r.u8(6)?;
        lf.level[3] = r.u8(6)?;
    }
    lf.sharpness = r.u8(3)?;
    lf.delta_enabled = r.flag()?;
    if !lf.delta_enabled {
        return Ok(lf);
    }
    lf.delta_update = r.flag()?;
    if !lf.delta_update {
        return Ok(lf);
    }
    for d in lf.ref_deltas.iter_mut() {
        if r.flag()? {
            *d = r.su(7)? as i8;
        }
    }
    for d in lf.mode_deltas.iter_mut() {
        if r.flag()? {
            *d = r.su(7)? as i8;
        }
    }
    Ok(lf)
}

fn cdef_params(r: &mut Bits<'_>, seq: &SequenceHeader, h: &FrameHeader) -> Result<Cdef, Av1Error> {
    let mut c = Cdef::default();
    if h.coded_lossless || h.allow_intrabc || !seq.enable_cdef {
        return Ok(c);
    }
    c.damping_minus_3 = r.u8(2)?;
    c.bits = r.u8(2)?;
    for i in 0..1usize << c.bits {
        c.y_pri[i] = r.u8(4)?;
        c.y_sec[i] = r.u8(2)?;
        if seq.color.num_planes() > 1 {
            c.uv_pri[i] = r.u8(4)?;
            c.uv_sec[i] = r.u8(2)?;
        }
    }
    Ok(c)
}

fn lr_params(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    h: &FrameHeader,
) -> Result<Restoration, Av1Error> {
    let mut lr = Restoration::default();
    if h.all_lossless || h.allow_intrabc || !seq.enable_restoration {
        return Ok(lr);
    }
    let (mut uses_lr, mut uses_chroma_lr) = (false, false);
    for i in 0..usize::from(seq.color.num_planes()) {
        lr.frame_type[i] = REMAP_LR_TYPE[usize::from(r.u8(2)?)];
        if lr.frame_type[i] != RESTORE_NONE {
            uses_lr = true;
            uses_chroma_lr |= i > 0;
        }
    }
    if !uses_lr {
        return Ok(lr);
    }
    if seq.use_128x128_superblock {
        lr.unit_shift = r.u8(1)? + 1;
    } else {
        lr.unit_shift = r.u8(1)?;
        if lr.unit_shift != 0 {
            lr.unit_shift += r.u8(1)?;
        }
    }
    if seq.color.subsampling_x && seq.color.subsampling_y && uses_chroma_lr {
        lr.uv_shift = r.u8(1)?;
    }
    Ok(lr)
}

/// Whether `skip_mode_params()` of section 5.9.22 reads `skip_mode_present`.
fn skip_mode_allowed(
    seq: &SequenceHeader,
    h: &FrameHeader,
    ref_hint: &[u8; NUM_REF_FRAMES],
) -> bool {
    if h.is_intra() || !h.reference_select || !seq.enable_order_hint {
        return false;
    }
    let dist = |a: u8, b: u8| relative_dist(seq, a, b);
    let mut forward: Option<u8> = None;
    let mut backward: Option<u8> = None;
    for &i in &h.ref_frame_idx {
        let hint = ref_hint[usize::from(i)];
        let d = dist(hint, h.order_hint);
        if d < 0 && forward.is_none_or(|f| dist(hint, f) > 0) {
            forward = Some(hint);
        } else if d > 0 && backward.is_none_or(|b| dist(hint, b) < 0) {
            backward = Some(hint);
        }
    }
    let Some(forward) = forward else { return false };
    if backward.is_some() {
        return true;
    }
    h.ref_frame_idx.iter().any(|&i| dist(ref_hint[usize::from(i)], forward) < 0)
}

fn inverse_recenter(r: i32, v: i32) -> i32 {
    if v > 2 * r {
        v
    } else if v & 1 == 1 {
        r - ((v + 1) >> 1)
    } else {
        r + (v >> 1)
    }
}

fn decode_subexp(r: &mut Bits<'_>, num_syms: i32) -> Result<i32, Av1Error> {
    let (mut i, mut mk, k) = (0u32, 0i32, 3u32);
    loop {
        let b2 = if i > 0 { k + i - 1 } else { k };
        let a = 1i32 << b2;
        if num_syms <= mk + 3 * a {
            return Ok(r.ns((num_syms - mk) as u32)? as i32 + mk);
        }
        if !r.flag()? {
            return Ok(r.f(b2)? as i32 + mk);
        }
        i += 1;
        mk += a;
    }
}

fn read_global_param(
    r: &mut Bits<'_>,
    ty: u8,
    idx: usize,
    prev: i32,
    hp: bool,
) -> Result<i32, Av1Error> {
    let (mut abs_bits, mut prec_bits) = (GM_ABS_ALPHA_BITS, GM_ALPHA_PREC_BITS);
    if idx < 2 {
        (abs_bits, prec_bits) = match ty {
            TRANSLATION => {
                (GM_ABS_TRANS_ONLY_BITS - u32::from(!hp), GM_TRANS_ONLY_PREC_BITS - u32::from(!hp))
            }
            _ => (GM_ABS_TRANS_BITS, GM_TRANS_PREC_BITS),
        };
    }
    let prec_diff = WARPEDMODEL_PREC_BITS - prec_bits;
    let (round, sub) =
        if idx % 3 == 2 { (1i32 << WARPEDMODEL_PREC_BITS, 1i32 << prec_bits) } else { (0, 0) };
    let mx = 1i32 << abs_bits;
    let reference = (prev >> prec_diff) - sub;
    // decode_signed_subexp_with_ref(-mx, mx + 1, r) of section 5.9.26.
    let (low, high) = (-mx, mx + 1);
    let (n, rr) = (high - low, reference - low);
    let v = decode_subexp(r, n)?;
    let x = if (rr << 1) <= n {
        inverse_recenter(rr, v)
    } else {
        n - 1 - inverse_recenter(n - 1 - rr, v)
    };
    Ok(((x + low) << prec_diff) + round)
}

fn global_motion_params(
    r: &mut Bits<'_>,
    h: &FrameHeader,
    prev: &[[i32; 6]; 8],
) -> Result<GlobalMotion, Av1Error> {
    let mut gm =
        GlobalMotion { gm_type: [IDENTITY; 8], params: DEFAULT_GM_PARAMS, invalid: [false; 8] };
    if h.is_intra() {
        return Ok(gm);
    }
    for (rf, prev) in prev.iter().enumerate().skip(1) {
        // is_global, is_rot_zoom, is_translation.
        let ty = if !r.flag()? {
            IDENTITY
        } else if r.flag()? {
            ROTZOOM
        } else if r.flag()? {
            TRANSLATION
        } else {
            AFFINE
        };
        gm.gm_type[rf] = ty;
        let hp = h.allow_high_precision_mv;
        let p = &mut gm.params[rf];
        if ty >= ROTZOOM {
            p[2] = read_global_param(r, ty, 2, prev[2], hp)?;
            p[3] = read_global_param(r, ty, 3, prev[3], hp)?;
            if ty == AFFINE {
                p[4] = read_global_param(r, ty, 4, prev[4], hp)?;
                p[5] = read_global_param(r, ty, 5, prev[5], hp)?;
            } else {
                (p[4], p[5]) = (-p[3], p[2]);
            }
        }
        if ty >= TRANSLATION {
            p[0] = read_global_param(r, ty, 0, prev[0], hp)?;
            p[1] = read_global_param(r, ty, 1, prev[1], hp)?;
        }
        gm.invalid[rf] = !shear_valid(p);
    }
    Ok(gm)
}

#[rustfmt::skip]
const DIV_LUT: [u16; 257] = [
    16384, 16320, 16257, 16194, 16132, 16070, 16009, 15948, 15888, 15828, 15768,
    15709, 15650, 15592, 15534, 15477, 15420, 15364, 15308, 15252, 15197, 15142,
    15087, 15033, 14980, 14926, 14873, 14821, 14769, 14717, 14665, 14614, 14564,
    14513, 14463, 14413, 14364, 14315, 14266, 14218, 14170, 14122, 14075, 14028,
    13981, 13935, 13888, 13843, 13797, 13752, 13707, 13662, 13618, 13574, 13530,
    13487, 13443, 13400, 13358, 13315, 13273, 13231, 13190, 13148, 13107, 13066,
    13026, 12985, 12945, 12906, 12866, 12827, 12788, 12749, 12710, 12672, 12633,
    12596, 12558, 12520, 12483, 12446, 12409, 12373, 12336, 12300, 12264, 12228,
    12193, 12157, 12122, 12087, 12053, 12018, 11984, 11950, 11916, 11882, 11848,
    11815, 11782, 11749, 11716, 11683, 11651, 11619, 11586, 11555, 11523, 11491,
    11460, 11429, 11398, 11367, 11336, 11305, 11275, 11245, 11215, 11185, 11155,
    11125, 11096, 11067, 11038, 11009, 10980, 10951, 10923, 10894, 10866, 10838,
    10810, 10782, 10755, 10727, 10700, 10673, 10645, 10618, 10592, 10565, 10538,
    10512, 10486, 10460, 10434, 10408, 10382, 10356, 10331, 10305, 10280, 10255,
    10230, 10205, 10180, 10156, 10131, 10107, 10082, 10058, 10034, 10010, 9986,
    9963,  9939,  9916,  9892,  9869,  9846,  9823,  9800,  9777,  9754,  9732,
    9709,  9687,  9664,  9642,  9620,  9598,  9576,  9554,  9533,  9511,  9489,
    9468,  9447,  9425,  9404,  9383,  9362,  9341,  9321,  9300,  9279,  9259,
    9239,  9218,  9198,  9178,  9158,  9138,  9118,  9098,  9079,  9059,  9039,
    9020,  9001,  8981,  8962,  8943,  8924,  8905,  8886,  8867,  8849,  8830,
    8812,  8793,  8775,  8756,  8738,  8720,  8702,  8684,  8666,  8648,  8630,
    8613,  8595,  8577,  8560,  8542,  8525,  8508,  8490,  8473,  8456,  8439,
    8422,  8405,  8389,  8372,  8355,  8339,  8322,  8306,  8289,  8273,  8257,
    8240,  8224,  8208,  8192,
];

fn round2_signed(x: i64, n: u32) -> i64 {
    if n == 0 {
        return x;
    }
    let r = |v: i64| (v + (1 << (n - 1))) >> n;
    if x >= 0 { r(x) } else { -r(-x) }
}

fn resolve_divisor(d: u32) -> (u32, i64) {
    let n = 31 - d.leading_zeros();
    let e = d - (1 << n);
    let f = if n > DIV_LUT_BITS {
        round2_signed(i64::from(e), n - DIV_LUT_BITS) as u32
    } else {
        e << (DIV_LUT_BITS - n)
    };
    (n + DIV_LUT_PREC_BITS, i64::from(DIV_LUT[(f as usize).min(256)]))
}

/// `warpValid` of `setup_shear()`, section 7.11.3.6, as ffmpeg's `vaapi_av1` takes it.
pub fn shear_valid(p: &[i32; 6]) -> bool {
    if p[2] <= 0 {
        return false;
    }
    let clip = |v: i64| v.clamp(-32768, 32767);
    let one = 1i64 << WARPEDMODEL_PREC_BITS;
    let alpha0 = clip(i64::from(p[2]) - one);
    let beta0 = clip(i64::from(p[3]));
    let (shift, factor) = resolve_divisor(p[2] as u32);
    let v = i64::from(p[4]) << WARPEDMODEL_PREC_BITS;
    let gamma0 = clip(round2_signed(v * factor, shift));
    let w = i64::from(p[3]) * i64::from(p[4]);
    let delta0 = clip(i64::from(p[5]) - round2_signed(w * factor, shift) - one);
    let reduce = |v: i64| round2_signed(v, WARP_PARAM_REDUCE_BITS) << WARP_PARAM_REDUCE_BITS;
    let (alpha, beta, gamma, delta) =
        (reduce(alpha0), reduce(beta0), reduce(gamma0), reduce(delta0));
    4 * alpha.abs() + 7 * beta.abs() < one && 4 * gamma.abs() + 4 * delta.abs() < one
}

fn film_grain_params(
    r: &mut Bits<'_>,
    seq: &SequenceHeader,
    refs: &[RefFrame; NUM_REF_FRAMES],
    h: &FrameHeader,
) -> Result<FilmGrain, Av1Error> {
    if !seq.film_grain_params_present || (!h.show_frame && !h.showable_frame) || !r.flag()? {
        return Ok(FilmGrain::NONE);
    }
    let grain_seed = r.f(16)? as u16;
    let update_grain = h.frame_type != FrameType::Inter || r.flag()?;
    if !update_grain {
        let at = r.u8(3)?;
        let slot = &refs[usize::from(at)];
        if !slot.valid {
            return Err(Av1Error::MissingRef(at));
        }
        // The loaded parameters with this frame's seed, applied, as dav1d takes them.
        return Ok(FilmGrain {
            apply_grain: true,
            grain_seed,
            update_grain,
            film_grain_params_ref_idx: at,
            ..slot.film_grain
        });
    }
    let mut g = FilmGrain { apply_grain: true, grain_seed, update_grain, ..FilmGrain::NONE };
    g.num_y_points = r.u8(4)?;
    if g.num_y_points > 14 {
        return Err(Av1Error::Invalid("num_y_points"));
    }
    for i in 0..usize::from(g.num_y_points) {
        g.point_y_value[i] = r.u8(8)?;
        g.point_y_scaling[i] = r.u8(8)?;
    }
    let c = &seq.color;
    g.chroma_scaling_from_luma = !c.mono_chrome && r.flag()?;
    if !(c.mono_chrome
        || g.chroma_scaling_from_luma
        || (c.subsampling_x && c.subsampling_y && g.num_y_points == 0))
    {
        g.num_cb_points = r.u8(4)?;
        if g.num_cb_points > 10 {
            return Err(Av1Error::Invalid("num_cb_points"));
        }
        for i in 0..usize::from(g.num_cb_points) {
            g.point_cb_value[i] = r.u8(8)?;
            g.point_cb_scaling[i] = r.u8(8)?;
        }
        g.num_cr_points = r.u8(4)?;
        if g.num_cr_points > 10 {
            return Err(Av1Error::Invalid("num_cr_points"));
        }
        for i in 0..usize::from(g.num_cr_points) {
            g.point_cr_value[i] = r.u8(8)?;
            g.point_cr_scaling[i] = r.u8(8)?;
        }
    }
    g.grain_scaling_minus_8 = r.u8(2)?;
    g.ar_coeff_lag = r.u8(2)?;
    let num_pos_luma = 2 * usize::from(g.ar_coeff_lag) * (usize::from(g.ar_coeff_lag) + 1);
    let num_pos_chroma = if g.num_y_points > 0 { num_pos_luma + 1 } else { num_pos_luma };
    if g.num_y_points > 0 {
        for v in g.ar_coeffs_y_plus_128.iter_mut().take(num_pos_luma) {
            *v = r.u8(8)?;
        }
    }
    if g.chroma_scaling_from_luma || g.num_cb_points > 0 {
        for v in g.ar_coeffs_cb_plus_128.iter_mut().take(num_pos_chroma) {
            *v = r.u8(8)?;
        }
    }
    if g.chroma_scaling_from_luma || g.num_cr_points > 0 {
        for v in g.ar_coeffs_cr_plus_128.iter_mut().take(num_pos_chroma) {
            *v = r.u8(8)?;
        }
    }
    g.ar_coeff_shift_minus_6 = r.u8(2)?;
    g.grain_scale_shift = r.u8(2)?;
    if g.num_cb_points > 0 {
        g.cb_mult = r.u8(8)?;
        g.cb_luma_mult = r.u8(8)?;
        g.cb_offset = r.f(9)? as u16;
    }
    if g.num_cr_points > 0 {
        g.cr_mult = r.u8(8)?;
        g.cr_luma_mult = r.u8(8)?;
        g.cr_offset = r.f(9)? as u16;
    }
    g.overlap_flag = r.flag()?;
    g.clip_to_restricted_range = r.flag()?;
    Ok(g)
}
