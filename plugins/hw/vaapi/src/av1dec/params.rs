//! VA-API AV1 decode parameters, as `va_dec_av1.h` describes and ffmpeg's `vaapi_av1.c` passes
//! them.

use super::header::{FrameHeader, FrameType, NUM_REF_FRAMES};
use super::obu::Tile;
use super::seq::SequenceHeader;
use super::stream::Group;
use crate::ffi;

pub fn seq_fields(seq: &SequenceHeader) -> u32 {
    let flag = |b: bool, bit: u32| if b { bit } else { 0 };
    let c = &seq.color;
    flag(seq.still_picture, ffi::AV1_SEQ_STILL_PICTURE)
        | flag(seq.use_128x128_superblock, ffi::AV1_SEQ_USE_128X128_SUPERBLOCK)
        | flag(seq.enable_filter_intra, ffi::AV1_SEQ_ENABLE_FILTER_INTRA)
        | flag(seq.enable_intra_edge_filter, ffi::AV1_SEQ_ENABLE_INTRA_EDGE_FILTER)
        | flag(seq.enable_interintra_compound, ffi::AV1_SEQ_ENABLE_INTERINTRA_COMPOUND)
        | flag(seq.enable_masked_compound, ffi::AV1_SEQ_ENABLE_MASKED_COMPOUND)
        | flag(seq.enable_dual_filter, ffi::AV1_SEQ_ENABLE_DUAL_FILTER)
        | flag(seq.enable_order_hint, ffi::AV1_SEQ_ENABLE_ORDER_HINT)
        | flag(seq.enable_jnt_comp, ffi::AV1_SEQ_ENABLE_JNT_COMP)
        | flag(seq.enable_cdef, ffi::AV1_SEQ_ENABLE_CDEF)
        | flag(c.mono_chrome, ffi::AV1_SEQ_MONO_CHROME)
        | flag(c.color_range, ffi::AV1_SEQ_COLOR_RANGE)
        | flag(c.subsampling_x, ffi::AV1_SEQ_SUBSAMPLING_X)
        | flag(c.subsampling_y, ffi::AV1_SEQ_SUBSAMPLING_Y)
        // One bit, the low bit of the two coded, as ffmpeg's assignment leaves it.
        | flag(c.chroma_sample_position & 1 == 1, ffi::AV1_SEQ_CHROMA_SAMPLE_POSITION)
        | flag(seq.film_grain_params_present, ffi::AV1_SEQ_FILM_GRAIN_PARAMS_PRESENT)
}

pub fn pic_fields(h: &FrameHeader) -> u32 {
    let flag = |b: bool, bit: u32| if b { bit } else { 0 };
    (h.frame_type as u32) << ffi::AV1_PIC_FRAME_TYPE_SHIFT
        | flag(h.show_frame, ffi::AV1_PIC_SHOW_FRAME)
        | flag(h.showable_frame, ffi::AV1_PIC_SHOWABLE_FRAME)
        | flag(h.error_resilient_mode, ffi::AV1_PIC_ERROR_RESILIENT_MODE)
        | flag(h.disable_cdf_update, ffi::AV1_PIC_DISABLE_CDF_UPDATE)
        | flag(h.allow_screen_content_tools, ffi::AV1_PIC_ALLOW_SCREEN_CONTENT_TOOLS)
        | flag(h.force_integer_mv, ffi::AV1_PIC_FORCE_INTEGER_MV)
        | flag(h.allow_intrabc, ffi::AV1_PIC_ALLOW_INTRABC)
        | flag(h.use_superres, ffi::AV1_PIC_USE_SUPERRES)
        | flag(h.allow_high_precision_mv, ffi::AV1_PIC_ALLOW_HIGH_PRECISION_MV)
        | flag(h.is_motion_mode_switchable, ffi::AV1_PIC_IS_MOTION_MODE_SWITCHABLE)
        | flag(h.use_ref_frame_mvs, ffi::AV1_PIC_USE_REF_FRAME_MVS)
        | flag(h.disable_frame_end_update_cdf, ffi::AV1_PIC_DISABLE_FRAME_END_UPDATE_CDF)
        | flag(h.tile_info.uniform, ffi::AV1_PIC_UNIFORM_TILE_SPACING_FLAG)
        | flag(h.allow_warped_motion, ffi::AV1_PIC_ALLOW_WARPED_MOTION)
}

pub fn mode_fields(h: &FrameHeader) -> u32 {
    let flag = |b: bool, bit: u32| if b { bit } else { 0 };
    flag(h.delta_q_present, ffi::AV1_MODE_DELTA_Q_PRESENT)
        | u32::from(h.delta_q_res & 3) << ffi::AV1_MODE_LOG2_DELTA_Q_RES_SHIFT
        | flag(h.delta_lf_present, ffi::AV1_MODE_DELTA_LF_PRESENT)
        | u32::from(h.delta_lf_res & 3) << ffi::AV1_MODE_LOG2_DELTA_LF_RES_SHIFT
        | flag(h.delta_lf_multi, ffi::AV1_MODE_DELTA_LF_MULTI)
        | u32::from(h.tx_mode & 3) << ffi::AV1_MODE_TX_MODE_SHIFT
        | flag(h.reference_select, ffi::AV1_MODE_REFERENCE_SELECT)
        | flag(h.reduced_tx_set, ffi::AV1_MODE_REDUCED_TX_SET_USED)
        | flag(h.skip_mode_present, ffi::AV1_MODE_SKIP_MODE_PRESENT)
}

fn seg_info(h: &FrameHeader) -> ffi::VASegmentationStructAV1 {
    let s = &h.seg;
    let flag = |b: bool, bit: u32| if b { bit } else { 0 };
    ffi::VASegmentationStructAV1 {
        segment_info_fields: flag(s.enabled, ffi::AV1_SEG_ENABLED)
            | flag(s.update_map, ffi::AV1_SEG_UPDATE_MAP)
            | flag(s.temporal_update, ffi::AV1_SEG_TEMPORAL_UPDATE)
            | flag(s.update_data, ffi::AV1_SEG_UPDATE_DATA),
        feature_data: s.feature_data,
        feature_mask: s.feature_enabled,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

/// The AR coefficients go as ffmpeg passes them, less 128.
fn film_grain(h: &FrameHeader) -> ffi::VAFilmGrainStructAV1 {
    // SAFETY: integers and arrays of them, all zero is valid.
    let mut f: ffi::VAFilmGrainStructAV1 = unsafe { std::mem::zeroed() };
    let g = &h.film_grain;
    if !g.apply_grain {
        return f;
    }
    let flag = |b: bool, bit: u32| if b { bit } else { 0 };
    f.film_grain_info_fields = ffi::AV1_FG_APPLY_GRAIN
        | flag(g.chroma_scaling_from_luma, ffi::AV1_FG_CHROMA_SCALING_FROM_LUMA)
        | u32::from(g.grain_scaling_minus_8 & 3) << ffi::AV1_FG_GRAIN_SCALING_MINUS_8_SHIFT
        | u32::from(g.ar_coeff_lag & 3) << ffi::AV1_FG_AR_COEFF_LAG_SHIFT
        | u32::from(g.ar_coeff_shift_minus_6 & 3) << ffi::AV1_FG_AR_COEFF_SHIFT_MINUS_6_SHIFT
        | u32::from(g.grain_scale_shift & 3) << ffi::AV1_FG_GRAIN_SCALE_SHIFT_SHIFT
        | flag(g.overlap_flag, ffi::AV1_FG_OVERLAP_FLAG)
        | flag(g.clip_to_restricted_range, ffi::AV1_FG_CLIP_TO_RESTRICTED_RANGE);
    f.grain_seed = g.grain_seed;
    (f.num_y_points, f.point_y_value, f.point_y_scaling) =
        (g.num_y_points, g.point_y_value, g.point_y_scaling);
    (f.num_cb_points, f.point_cb_value, f.point_cb_scaling) =
        (g.num_cb_points, g.point_cb_value, g.point_cb_scaling);
    (f.num_cr_points, f.point_cr_value, f.point_cr_scaling) =
        (g.num_cr_points, g.point_cr_value, g.point_cr_scaling);
    let less_128 = |v: u8| (i16::from(v) - 128) as i8;
    for (d, &s) in f.ar_coeffs_y.iter_mut().zip(&g.ar_coeffs_y_plus_128) {
        *d = less_128(s);
    }
    for (d, &s) in f.ar_coeffs_cb.iter_mut().zip(&g.ar_coeffs_cb_plus_128) {
        *d = less_128(s);
    }
    for (d, &s) in f.ar_coeffs_cr.iter_mut().zip(&g.ar_coeffs_cr_plus_128) {
        *d = less_128(s);
    }
    (f.cb_mult, f.cb_luma_mult, f.cb_offset) = (g.cb_mult, g.cb_luma_mult, g.cb_offset);
    (f.cr_mult, f.cr_luma_mult, f.cr_offset) = (g.cr_mult, g.cr_luma_mult, g.cr_offset);
    f
}

pub fn pic_param(
    seq: &SequenceHeader,
    h: &FrameHeader,
    refs: [ffi::VASurfaceID; NUM_REF_FRAMES],
    current: ffi::VASurfaceID,
    display: ffi::VASurfaceID,
) -> ffi::VADecPictureParameterBufferAV1 {
    // SAFETY: integers, arrays of them and a null pointer, all zero is valid.
    let mut p: ffi::VADecPictureParameterBufferAV1 = unsafe { std::mem::zeroed() };
    p.profile = seq.seq_profile;
    p.order_hint_bits_minus_1 = seq.order_hint_bits.saturating_sub(1);
    p.bit_depth_idx = match seq.color.bit_depth {
        8 => 0,
        10 => 1,
        _ => 2,
    };
    p.matrix_coefficients = seq.color.matrix_coefficients;
    p.seq_info_fields = seq_fields(seq);
    p.current_frame = current;
    p.current_display_picture = display;
    p.anchor_frames_list = std::ptr::null_mut();
    p.frame_width_minus1 = (h.upscaled_width.saturating_sub(1)) as u16;
    p.frame_height_minus1 = (h.frame_height.saturating_sub(1)) as u16;
    // A shown key frame refers to nothing, as ffmpeg passes it.
    p.ref_frame_map = if h.frame_type == FrameType::Key && h.show_frame {
        [ffi::VA_INVALID_SURFACE; NUM_REF_FRAMES]
    } else {
        refs
    };
    p.ref_frame_idx = h.ref_frame_idx;
    p.primary_ref_frame = h.primary_ref_frame;
    p.order_hint = h.order_hint;
    p.seg_info = seg_info(h);
    p.film_grain_info = film_grain(h);
    let t = &h.tile_info;
    p.tile_cols = t.cols as u8;
    p.tile_rows = t.rows as u8;
    for (d, &s) in p.width_in_sbs_minus_1.iter_mut().zip(&t.width_in_sbs).take(usize::from(t.cols))
    {
        *d = s.saturating_sub(1);
    }
    for (d, &s) in
        p.height_in_sbs_minus_1.iter_mut().zip(&t.height_in_sbs).take(usize::from(t.rows))
    {
        *d = s.saturating_sub(1);
    }
    p.context_update_tile_id = t.context_update_tile_id;
    p.pic_info_fields = pic_fields(h);
    p.superres_scale_denominator = h.superres_denom;
    p.interp_filter = h.interpolation_filter;
    let lf = &h.lf;
    p.filter_level = [lf.level[0], lf.level[1]];
    (p.filter_level_u, p.filter_level_v) = (lf.level[2], lf.level[3]);
    p.loop_filter_info_fields = (lf.sharpness & 7) << ffi::AV1_LF_SHARPNESS_SHIFT
        | if lf.delta_enabled { ffi::AV1_LF_MODE_REF_DELTA_ENABLED } else { 0 }
        | if lf.delta_update { ffi::AV1_LF_MODE_REF_DELTA_UPDATE } else { 0 };
    p.ref_deltas = lf.ref_deltas;
    p.mode_deltas = lf.mode_deltas;
    let q = &h.quant;
    p.base_qindex = q.base_q_idx;
    (p.y_dc_delta_q, p.u_dc_delta_q, p.u_ac_delta_q) =
        (q.delta_q_y_dc, q.delta_q_u_dc, q.delta_q_u_ac);
    (p.v_dc_delta_q, p.v_ac_delta_q) = (q.delta_q_v_dc, q.delta_q_v_ac);
    if q.using_qmatrix {
        p.qmatrix_fields = ffi::AV1_QM_USING_QMATRIX
            | u16::from(q.qm_y & 15) << ffi::AV1_QM_Y_SHIFT
            | u16::from(q.qm_u & 15) << ffi::AV1_QM_U_SHIFT
            | u16::from(q.qm_v & 15) << ffi::AV1_QM_V_SHIFT;
    }
    p.mode_control_fields = mode_fields(h);
    let c = &h.cdef;
    p.cdef_damping_minus_3 = c.damping_minus_3;
    p.cdef_bits = c.bits;
    for i in 0..1usize << c.bits {
        p.cdef_y_strengths[i] = c.y_pri[i] << 2 | c.y_sec[i] & 3;
        p.cdef_uv_strengths[i] = c.uv_pri[i] << 2 | c.uv_sec[i] & 3;
    }
    let lr = &h.lr;
    p.loop_restoration_fields = u16::from(lr.frame_type[0] & 3) << ffi::AV1_LR_Y_TYPE_SHIFT
        | u16::from(lr.frame_type[1] & 3) << ffi::AV1_LR_CB_TYPE_SHIFT
        | u16::from(lr.frame_type[2] & 3) << ffi::AV1_LR_CR_TYPE_SHIFT
        | u16::from(lr.unit_shift & 3) << ffi::AV1_LR_UNIT_SHIFT_SHIFT
        | if lr.uv_shift != 0 { ffi::AV1_LR_UV_SHIFT } else { 0 };
    for (i, wm) in p.wm.iter_mut().enumerate() {
        let rf = i + 1;
        wm.wmtype = i32::from(h.gm.gm_type[rf]);
        wm.wmmat[..6].copy_from_slice(&h.gm.params[rf]);
        wm.invalid = u8::from(h.gm.invalid[rf]);
    }
    p
}

pub fn tile_param(t: &Tile, g: &Group) -> ffi::VASliceParameterBufferAV1 {
    ffi::VASliceParameterBufferAV1 {
        slice_data_size: t.size,
        slice_data_offset: t.offset,
        slice_data_flag: ffi::VA_SLICE_DATA_FLAG_ALL,
        tile_row: t.row,
        tile_column: t.col,
        tg_start: g.tg_start,
        tg_end: g.tg_end,
        anchor_frame_idx: 0,
        tile_idx_in_tile_list: 0,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}
