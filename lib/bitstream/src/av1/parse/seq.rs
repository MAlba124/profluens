use super::obu::{Av1Error, Bits};

pub const SELECT: u8 = 2;
const CP_BT_709: u8 = 1;
const TC_SRGB: u8 = 13;
const MC_IDENTITY: u8 = 0;
const UNSPECIFIED: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorConfig {
    pub bit_depth: u8,
    pub mono_chrome: bool,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub color_range: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub separate_uv_delta_q: bool,
}

impl ColorConfig {
    pub fn num_planes(&self) -> u8 {
        if self.mono_chrome { 1 } else { 3 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    pub still_picture: bool,
    pub reduced_still_picture_header: bool,
    pub decoder_model_info_present_flag: bool,
    pub equal_picture_interval: bool,
    pub buffer_removal_time_length_minus_1: u8,
    pub frame_presentation_time_length_minus_1: u8,
    pub operating_points_cnt_minus_1: u8,
    pub operating_point_idc: [u16; 32],
    pub decoder_model_present_for_this_op: [bool; 32],
    pub seq_level_idx_0: u8,
    pub frame_width_bits: u8,
    pub frame_height_bits: u8,
    pub max_frame_width: u32,
    pub max_frame_height: u32,
    pub frame_id_numbers_present_flag: bool,
    pub delta_frame_id_length_minus_2: u8,
    pub additional_frame_id_length_minus_1: u8,
    pub use_128x128_superblock: bool,
    pub enable_filter_intra: bool,
    pub enable_intra_edge_filter: bool,
    pub enable_interintra_compound: bool,
    pub enable_masked_compound: bool,
    pub enable_warped_motion: bool,
    pub enable_dual_filter: bool,
    pub enable_order_hint: bool,
    pub enable_jnt_comp: bool,
    pub enable_ref_frame_mvs: bool,
    pub seq_force_screen_content_tools: u8,
    pub seq_force_integer_mv: u8,
    pub order_hint_bits: u8,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub color: ColorConfig,
    pub film_grain_params_present: bool,
}

impl SequenceHeader {
    pub fn operating_point_idc(&self) -> u16 {
        self.operating_point_idc[0]
    }

    pub fn id_len(&self) -> u32 {
        u32::from(self.additional_frame_id_length_minus_1)
            + u32::from(self.delta_frame_id_length_minus_2)
            + 3
    }

    /// A repeat may differ only in its operating parameters, section 7.5.
    pub fn same_sequence(&self, other: &SequenceHeader) -> bool {
        let strip = |s: &SequenceHeader| SequenceHeader {
            decoder_model_present_for_this_op: [false; 32],
            ..*s
        };
        strip(self) == strip(other)
    }
}

fn color_config(r: &mut Bits<'_>, seq_profile: u8) -> Result<ColorConfig, Av1Error> {
    let high_bitdepth = r.flag()?;
    let bit_depth = if seq_profile == 2 && high_bitdepth {
        if r.flag()? { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let mono_chrome = if seq_profile == 1 { false } else { r.flag()? };
    let (mut cp, mut tc, mut mc) = (UNSPECIFIED, UNSPECIFIED, UNSPECIFIED);
    if r.flag()? {
        cp = r.u8(8)?;
        tc = r.u8(8)?;
        mc = r.u8(8)?;
    }
    let mut c = ColorConfig {
        bit_depth,
        mono_chrome,
        color_primaries: cp,
        transfer_characteristics: tc,
        matrix_coefficients: mc,
        color_range: false,
        subsampling_x: true,
        subsampling_y: true,
        chroma_sample_position: 0,
        separate_uv_delta_q: false,
    };
    if mono_chrome {
        c.color_range = r.flag()?;
        return Ok(c);
    }
    if cp == CP_BT_709 && tc == TC_SRGB && mc == MC_IDENTITY {
        c.color_range = true;
        (c.subsampling_x, c.subsampling_y) = (false, false);
    } else {
        c.color_range = r.flag()?;
        match seq_profile {
            0 => {}
            1 => (c.subsampling_x, c.subsampling_y) = (false, false),
            _ if bit_depth == 12 => {
                c.subsampling_x = r.flag()?;
                c.subsampling_y = c.subsampling_x && r.flag()?;
            }
            _ => (c.subsampling_x, c.subsampling_y) = (true, false),
        }
        if c.subsampling_x && c.subsampling_y {
            c.chroma_sample_position = r.u8(2)?;
        }
    }
    c.separate_uv_delta_q = r.flag()?;
    Ok(c)
}

pub fn parse_sequence_header(payload: &[u8]) -> Result<SequenceHeader, Av1Error> {
    let r = &mut Bits::new(payload);
    let seq_profile = r.u8(3)?;
    if seq_profile > 2 {
        return Err(Av1Error::Profile(seq_profile));
    }
    let still_picture = r.flag()?;
    let reduced_still_picture_header = r.flag()?;
    let mut s = SequenceHeader {
        seq_profile,
        still_picture,
        reduced_still_picture_header,
        decoder_model_info_present_flag: false,
        equal_picture_interval: false,
        buffer_removal_time_length_minus_1: 0,
        frame_presentation_time_length_minus_1: 0,
        operating_points_cnt_minus_1: 0,
        operating_point_idc: [0; 32],
        decoder_model_present_for_this_op: [false; 32],
        seq_level_idx_0: 0,
        frame_width_bits: 0,
        frame_height_bits: 0,
        max_frame_width: 0,
        max_frame_height: 0,
        frame_id_numbers_present_flag: false,
        delta_frame_id_length_minus_2: 0,
        additional_frame_id_length_minus_1: 0,
        use_128x128_superblock: false,
        enable_filter_intra: false,
        enable_intra_edge_filter: false,
        enable_interintra_compound: false,
        enable_masked_compound: false,
        enable_warped_motion: false,
        enable_dual_filter: false,
        enable_order_hint: false,
        enable_jnt_comp: false,
        enable_ref_frame_mvs: false,
        seq_force_screen_content_tools: SELECT,
        seq_force_integer_mv: SELECT,
        order_hint_bits: 0,
        enable_superres: false,
        enable_cdef: false,
        enable_restoration: false,
        color: ColorConfig {
            bit_depth: 8,
            mono_chrome: false,
            color_primaries: UNSPECIFIED,
            transfer_characteristics: UNSPECIFIED,
            matrix_coefficients: UNSPECIFIED,
            color_range: false,
            subsampling_x: true,
            subsampling_y: true,
            chroma_sample_position: 0,
            separate_uv_delta_q: false,
        },
        film_grain_params_present: false,
    };
    if reduced_still_picture_header {
        s.seq_level_idx_0 = r.u8(5)?;
    } else {
        operating_points(r, &mut s)?;
    }
    s.frame_width_bits = r.u8(4)? + 1;
    s.frame_height_bits = r.u8(4)? + 1;
    s.max_frame_width = r.f(u32::from(s.frame_width_bits))? + 1;
    s.max_frame_height = r.f(u32::from(s.frame_height_bits))? + 1;
    if !reduced_still_picture_header {
        s.frame_id_numbers_present_flag = r.flag()?;
    }
    if s.frame_id_numbers_present_flag {
        s.delta_frame_id_length_minus_2 = r.u8(4)?;
        s.additional_frame_id_length_minus_1 = r.u8(3)?;
    }
    s.use_128x128_superblock = r.flag()?;
    s.enable_filter_intra = r.flag()?;
    s.enable_intra_edge_filter = r.flag()?;
    if !reduced_still_picture_header {
        s.enable_interintra_compound = r.flag()?;
        s.enable_masked_compound = r.flag()?;
        s.enable_warped_motion = r.flag()?;
        s.enable_dual_filter = r.flag()?;
        s.enable_order_hint = r.flag()?;
        if s.enable_order_hint {
            s.enable_jnt_comp = r.flag()?;
            s.enable_ref_frame_mvs = r.flag()?;
        }
        let choose_sct = r.flag()?;
        s.seq_force_screen_content_tools = if choose_sct { SELECT } else { r.u8(1)? };
        if s.seq_force_screen_content_tools > 0 {
            let choose_imv = r.flag()?;
            s.seq_force_integer_mv = if choose_imv { SELECT } else { r.u8(1)? };
        } else {
            s.seq_force_integer_mv = SELECT;
        }
        if s.enable_order_hint {
            s.order_hint_bits = r.u8(3)? + 1;
        }
    }
    s.enable_superres = r.flag()?;
    s.enable_cdef = r.flag()?;
    s.enable_restoration = r.flag()?;
    s.color = color_config(r, seq_profile)?;
    s.film_grain_params_present = r.flag()?;
    Ok(s)
}

fn operating_points(r: &mut Bits<'_>, s: &mut SequenceHeader) -> Result<(), Av1Error> {
    let mut buffer_delay_length = 0;
    if r.flag()? {
        // timing_info()
        r.f(32)?;
        r.f(32)?;
        s.equal_picture_interval = r.flag()?;
        if s.equal_picture_interval {
            r.uvlc()?;
        }
        s.decoder_model_info_present_flag = r.flag()?;
        if s.decoder_model_info_present_flag {
            buffer_delay_length = r.f(5)? + 1;
            r.f(32)?;
            s.buffer_removal_time_length_minus_1 = r.u8(5)?;
            s.frame_presentation_time_length_minus_1 = r.u8(5)?;
        }
    }
    let initial_display_delay_present = r.flag()?;
    s.operating_points_cnt_minus_1 = r.u8(5)?;
    for i in 0..=usize::from(s.operating_points_cnt_minus_1) {
        s.operating_point_idc[i] = r.f(12)? as u16;
        let level = r.u8(5)?;
        if i == 0 {
            s.seq_level_idx_0 = level;
        }
        if level > 7 {
            r.flag()?; // seq_tier
        }
        if s.decoder_model_info_present_flag {
            s.decoder_model_present_for_this_op[i] = r.flag()?;
            if s.decoder_model_present_for_this_op[i] {
                r.f(buffer_delay_length)?; // decoder_buffer_delay
                r.f(buffer_delay_length)?; // encoder_buffer_delay
                r.flag()?; // low_delay_mode_flag
            }
        }
        if initial_display_delay_present && r.flag()? {
            r.f(4)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1::write::{Sequence, sequence_header};

    fn payload(bytes: &[u8]) -> &[u8] {
        &bytes[5..]
    }

    #[test]
    fn the_encoder_sequence_header_round_trips() {
        let s = Sequence {
            level_idx: 13,
            tier: 1,
            width: 3840,
            height: 2160,
            ten_bit: true,
            enable_cdef: true,
            color: Some((9, 16, 9)),
            full_range: true,
        };
        let h = parse_sequence_header(payload(&sequence_header(&s, 4))).unwrap();
        assert_eq!(
            (h.seq_profile, h.still_picture, h.reduced_still_picture_header),
            (0, false, false)
        );
        assert_eq!(
            (h.operating_points_cnt_minus_1, h.operating_point_idc(), h.seq_level_idx_0),
            (0, 0, 13)
        );
        assert_eq!((h.max_frame_width, h.max_frame_height), (3840, 2160));
        assert_eq!((h.frame_width_bits, h.frame_height_bits), (12, 12));
        assert!(!h.frame_id_numbers_present_flag && !h.use_128x128_superblock);
        assert!(h.enable_order_hint && !h.enable_jnt_comp && !h.enable_ref_frame_mvs);
        assert_eq!((h.seq_force_screen_content_tools, h.seq_force_integer_mv), (0, SELECT));
        assert_eq!(h.order_hint_bits, 8);
        assert!(h.enable_cdef && !h.enable_superres && !h.enable_restoration);
        let c = h.color;
        assert_eq!(
            (c.bit_depth, c.mono_chrome, c.subsampling_x, c.subsampling_y),
            (10, false, true, true)
        );
        assert_eq!(
            (c.color_primaries, c.transfer_characteristics, c.matrix_coefficients),
            (9, 16, 9)
        );
        assert!(c.color_range && !c.separate_uv_delta_q);
        assert!(!h.film_grain_params_present);
        let plain = parse_sequence_header(payload(&sequence_header(
            &Sequence { color: None, ten_bit: false, ..s },
            4,
        )))
        .unwrap();
        assert_eq!(plain.color.bit_depth, 8);
        assert_eq!(plain.color.matrix_coefficients, UNSPECIFIED);
        assert!(plain.same_sequence(&plain));
        assert!(!plain.same_sequence(&h));
    }

    #[test]
    fn a_reduced_still_picture_header_and_srgb() {
        use crate::av1::write::Bits as W;
        let mut w = W::default();
        w.put(1, 3); // seq_profile
        w.flag(true); // still_picture
        w.flag(true); // reduced_still_picture_header
        w.put(4, 5); // seq_level_idx[0]
        w.put(7, 4); // frame_width_bits_minus_1
        w.put(6, 4); // frame_height_bits_minus_1
        w.put(199, 8);
        w.put(99, 7);
        w.flag(false); // use_128x128_superblock
        w.flag(true); // enable_filter_intra
        w.flag(true); // enable_intra_edge_filter
        w.flag(false); // enable_superres
        w.flag(true); // enable_cdef
        w.flag(false); // enable_restoration
        w.flag(false); // high_bitdepth
        w.flag(true); // color_description_present_flag
        w.put(1, 8);
        w.put(13, 8);
        w.put(0, 8);
        w.flag(true); // separate_uv_delta_q
        w.flag(true); // film_grain_params_present
        let h = parse_sequence_header(&w.into_bytes()).unwrap();
        assert_eq!(
            (h.seq_profile, h.seq_level_idx_0, h.max_frame_width, h.max_frame_height),
            (1, 4, 200, 100)
        );
        assert_eq!(
            (h.seq_force_screen_content_tools, h.seq_force_integer_mv, h.order_hint_bits),
            (SELECT, SELECT, 0)
        );
        assert!(h.enable_filter_intra && h.enable_cdef);
        let c = h.color;
        assert!(c.color_range && !c.subsampling_x && !c.subsampling_y && c.separate_uv_delta_q);
        assert!(h.film_grain_params_present);
        assert_eq!(parse_sequence_header(&[0b1110_0000]), Err(Av1Error::Profile(7)));
        assert_eq!(parse_sequence_header(&[0]), Err(Av1Error::Truncated));
    }
}
