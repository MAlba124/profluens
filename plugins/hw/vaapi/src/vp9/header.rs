//! VP9 uncompressed header walker, spec v0.7 section 6.2.

use super::Error;
use super::bitreader::BitReader;

pub const MAX_SEGMENTS: usize = 8;

pub const SEG_LVL_MAX: usize = 4;

/// Magnitude bits per segmentation feature (section 6.2.11). Skip is flag only.
pub const SEGMENTATION_FEATURE_BITS: [u32; SEG_LVL_MAX] = [8, 6, 2, 0];

pub const SEGMENTATION_FEATURE_SIGNED: [bool; SEG_LVL_MAX] = [true, true, false, false];

/// Frame type, spec section 7.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    KeyFrame,
    NonKeyFrame,
}

/// `color_space`, spec section 7.2.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Unknown,
    Bt601,
    Bt709,
    Smpte170,
    Smpte240,
    Bt2020,
    Reserved,
    /// Only legal when `Profile >= 1`.
    Rgb,
}

impl ColorSpace {
    fn from_bits(bits: u32) -> Self {
        match bits & 0b111 {
            0 => Self::Unknown,
            1 => Self::Bt601,
            2 => Self::Bt709,
            3 => Self::Smpte170,
            4 => Self::Smpte240,
            5 => Self::Bt2020,
            6 => Self::Reserved,
            _ => Self::Rgb,
        }
    }
}

/// `color_config()`, spec section 6.2.2. Defaults match intra-only Profile 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorConfig {
    pub bit_depth: u8,
    pub color_space: ColorSpace,
    /// Forced to full swing for RGB (section 6.2.2).
    pub color_range_full: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
}

impl ColorConfig {
    const fn default_intra_only_profile0() -> Self {
        Self {
            bit_depth: 8,
            color_space: ColorSpace::Bt601,
            color_range_full: false,
            subsampling_x: true,
            subsampling_y: true,
        }
    }
}

/// `loop_filter_params()`, spec section 6.2.8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopFilterParams {
    pub level: u8,
    pub sharpness: u8,
    pub delta_enabled: bool,
    pub delta_update: bool,
    /// `None` when `update_ref_delta` was 0, the spec keeps the previous value.
    pub ref_deltas: [Option<i8>; 4],
    pub mode_deltas: [Option<i8>; 2],
}

impl LoopFilterParams {
    const fn default_disabled() -> Self {
        Self {
            level: 0,
            sharpness: 0,
            delta_enabled: false,
            delta_update: false,
            ref_deltas: [None; 4],
            mode_deltas: [None; 2],
        }
    }
}

/// `quantization_params()`, spec section 6.2.9, after `read_delta_q()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantizationParams {
    pub base_q_idx: u8,
    pub delta_q_y_dc: i8,
    pub delta_q_uv_dc: i8,
    pub delta_q_uv_ac: i8,
    pub lossless: bool,
}

impl QuantizationParams {
    const fn default_zero() -> Self {
        Self { base_q_idx: 0, delta_q_y_dc: 0, delta_q_uv_dc: 0, delta_q_uv_ac: 0, lossless: true }
    }
}

/// `segmentation_params()`, spec section 6.2.11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentationParams {
    pub enabled: bool,
    pub update_map: bool,
    pub tree_probs: Option<[u8; 7]>,
    pub temporal_update: bool,
    pub pred_prob: Option<[u8; 3]>,
    pub update_data: bool,
    pub abs_or_delta_update: bool,
    pub feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    pub feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
}

impl SegmentationParams {
    pub(crate) const fn default_disabled() -> Self {
        Self {
            enabled: false,
            update_map: false,
            tree_probs: None,
            temporal_update: false,
            pred_prob: None,
            update_data: false,
            abs_or_delta_update: false,
            feature_enabled: [[false; SEG_LVL_MAX]; MAX_SEGMENTS],
            feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
        }
    }
}

/// `tile_info()`, spec section 6.2.13.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileInfo {
    pub tile_cols_log2: u8,
    pub tile_rows_log2: u8,
}

impl TileInfo {
    const fn default_zero() -> Self {
        Self { tile_cols_log2: 0, tile_rows_log2: 0 }
    }
}

/// VP9 uncompressed header, spec section 6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9FrameHeader {
    pub profile: u8,
    pub show_existing_frame: bool,
    pub frame_to_show_map_idx: Option<u8>,
    pub frame_type: FrameType,
    pub show_frame: bool,
    pub error_resilient_mode: bool,
    pub intra_only: bool,
    pub color_config: ColorConfig,
    pub frame_width: u32,
    pub frame_height: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub reset_frame_context: u8,
    pub refresh_frame_flags: u8,
    pub refresh_frame_context: bool,
    pub frame_parallel_decoding_mode: bool,
    pub frame_context_idx: u8,
    pub loop_filter: LoopFilterParams,
    pub quantization: QuantizationParams,
    pub segmentation: SegmentationParams,
    pub tile_info: TileInfo,
    /// Size of the section 6.3 compressed header.
    pub header_size_in_bytes: u16,
    /// Uncompressed header size including the section 6.1.1 trailing bits.
    pub uncompressed_header_size_bytes: usize,
    /// `None` on key and intra-only frames.
    pub ref_frame_idx: Option<[u8; 3]>,
    pub ref_frame_sign_bias: [bool; 3],
    pub allow_high_precision_mv: bool,
    /// Section 6.2.7. `SWITCHABLE = 4`, 0 on key and intra-only frames.
    pub interpolation_filter: u8,
}

#[derive(Clone, Copy, Debug, Default)]
struct InterFields {
    ref_frame_idx: Option<[u8; 3]>,
    ref_frame_sign_bias: [bool; 3],
    allow_high_precision_mv: bool,
    interpolation_filter: u8,
}

/// Reference state an inter frame needs. Its dimensions per section 8.10 slot and
/// the colour config inherited from the last key or intra-only frame.
#[derive(Clone, Copy, Debug)]
pub struct RefFrameState<'a> {
    pub ref_dims: &'a [(u32, u32)],
    pub color_config: ColorConfig,
}

/// Parse an intra frame header. Inter frames return [`Error::Unsupported`].
pub fn parse_uncompressed_header(data: &[u8]) -> Result<Vp9FrameHeader, Error> {
    parse_uncompressed_header_with_refs(data, None)
}

/// Parse a VP9 uncompressed header. Inter frames need `ref_state`.
pub fn parse_uncompressed_header_with_refs(
    data: &[u8],
    ref_state: Option<RefFrameState<'_>>,
) -> Result<Vp9FrameHeader, Error> {
    let mut br = BitReader::new(data);

    // Section 7.2: shall be equal to 2.
    let frame_marker = br.read_bits(2)?;
    if frame_marker != 2 {
        return Err(Error::InvalidBitstream);
    }

    let profile_low_bit = br.read_bits(1)?;
    let profile_high_bit = br.read_bits(1)?;
    let profile = ((profile_high_bit << 1) | profile_low_bit) as u8;
    if profile == 3 {
        let reserved_zero = br.read_bits(1)?;
        if reserved_zero != 0 {
            return Err(Error::InvalidBitstream);
        }
    }

    let show_existing_frame = br.read_flag()?;
    if show_existing_frame {
        let frame_to_show_map_idx = br.read_bits(3)? as u8;
        return Ok(Vp9FrameHeader {
            profile,
            show_existing_frame: true,
            frame_to_show_map_idx: Some(frame_to_show_map_idx),
            frame_type: FrameType::NonKeyFrame,
            show_frame: true,
            error_resilient_mode: false,
            intra_only: false,
            color_config: ColorConfig::default_intra_only_profile0(),
            frame_width: 0,
            frame_height: 0,
            render_width: 0,
            render_height: 0,
            reset_frame_context: 0,
            refresh_frame_flags: 0,
            refresh_frame_context: false,
            frame_parallel_decoding_mode: true,
            frame_context_idx: 0,
            loop_filter: LoopFilterParams::default_disabled(),
            quantization: QuantizationParams::default_zero(),
            segmentation: SegmentationParams::default_disabled(),
            tile_info: TileInfo::default_zero(),
            header_size_in_bytes: 0,
            uncompressed_header_size_bytes: 0,
            ref_frame_idx: None,
            ref_frame_sign_bias: [false; 3],
            allow_high_precision_mv: false,
            interpolation_filter: 0,
        });
    }

    let frame_type =
        if br.read_bits(1)? == 0 { FrameType::KeyFrame } else { FrameType::NonKeyFrame };
    let show_frame = br.read_flag()?;
    let error_resilient_mode = br.read_flag()?;

    let (
        intra_only,
        color_config,
        frame_width,
        frame_height,
        render_width,
        render_height,
        reset_frame_context,
        refresh_frame_flags,
        frame_is_intra,
        inter_fields,
    ) = match frame_type {
        FrameType::KeyFrame => {
            read_frame_sync_code(&mut br)?;
            let color_config = read_color_config(&mut br, profile)?;
            let (frame_width, frame_height) = read_frame_size(&mut br)?;
            let (render_width, render_height) =
                read_render_size(&mut br, frame_width, frame_height)?;
            (
                false,
                color_config,
                frame_width,
                frame_height,
                render_width,
                render_height,
                0u8,
                0xFFu8,
                true,
                InterFields::default(),
            )
        }
        FrameType::NonKeyFrame => {
            let intra_only = if !show_frame { br.read_flag()? } else { false };
            let frame_is_intra = intra_only;
            let reset_frame_context =
                if !error_resilient_mode { br.read_bits(2)? as u8 } else { 0 };

            if intra_only {
                // Profile 0 intra-only frames get BT.601 4:2:0 8-bit defaults.
                read_frame_sync_code(&mut br)?;
                let color_config = if profile > 0 {
                    read_color_config(&mut br, profile)?
                } else {
                    ColorConfig::default_intra_only_profile0()
                };

                let refresh_frame_flags = br.read_bits(8)? as u8;
                let (frame_width, frame_height) = read_frame_size(&mut br)?;
                let (render_width, render_height) =
                    read_render_size(&mut br, frame_width, frame_height)?;
                (
                    true,
                    color_config,
                    frame_width,
                    frame_height,
                    render_width,
                    render_height,
                    reset_frame_context,
                    refresh_frame_flags,
                    frame_is_intra,
                    InterFields::default(),
                )
            } else {
                // Inter frames do not re-read color_config (section 6.2), it is inherited.
                let state = ref_state.ok_or(Error::Unsupported)?;

                let refresh_frame_flags = br.read_bits(8)? as u8;

                let mut ref_frame_idx = [0u8; 3];
                let mut ref_frame_sign_bias = [false; 3];
                for i in 0..3 {
                    ref_frame_idx[i] = br.read_bits(3)? as u8;
                    ref_frame_sign_bias[i] = br.read_flag()?;
                }

                // Section 6.2.5, the first found_ref adopts that slot's dimensions.
                let mut frame_width = 0u32;
                let mut frame_height = 0u32;
                let mut found_ref = false;
                for &idx in ref_frame_idx.iter() {
                    if br.read_flag()? {
                        let (w, h) = state.ref_dims[idx as usize];
                        frame_width = w;
                        frame_height = h;
                        found_ref = true;
                        break;
                    }
                }
                if !found_ref {
                    let (w, h) = read_frame_size(&mut br)?;
                    frame_width = w;
                    frame_height = h;
                }
                let (render_width, render_height) =
                    read_render_size(&mut br, frame_width, frame_height)?;

                let allow_high_precision_mv = br.read_flag()?;
                let interpolation_filter = read_interpolation_filter(&mut br)?;

                (
                    false,
                    state.color_config,
                    frame_width,
                    frame_height,
                    render_width,
                    render_height,
                    reset_frame_context,
                    refresh_frame_flags,
                    frame_is_intra,
                    InterFields {
                        ref_frame_idx: Some(ref_frame_idx),
                        ref_frame_sign_bias,
                        allow_high_precision_mv,
                        interpolation_filter,
                    },
                )
            }
        }
    };

    // Absent when error_resilient_mode, forced to 0 and 1.
    let (refresh_frame_context, frame_parallel_decoding_mode) =
        if !error_resilient_mode { (br.read_flag()?, br.read_flag()?) } else { (false, true) };
    let mut frame_context_idx = br.read_bits(2)? as u8;
    // Section 6.2: setup_past_independence forces frame_context_idx to 0.
    if frame_is_intra || error_resilient_mode {
        frame_context_idx = 0;
    }

    let loop_filter = read_loop_filter_params(&mut br)?;
    let quantization = read_quantization_params(&mut br)?;
    let segmentation = read_segmentation_params(&mut br)?;
    let tile_info = read_tile_info(&mut br, frame_width)?;
    let header_size_in_bytes = br.read_bits(16)? as u16;

    // Section 6.1.1 trailing_bits with the section 7.1.1 zero check.
    br.trailing_bits()?;

    debug_assert_eq!(br.position() & 7, 0);
    let uncompressed_header_size_bytes = br.position() / 8;

    Ok(Vp9FrameHeader {
        profile,
        show_existing_frame: false,
        frame_to_show_map_idx: None,
        frame_type,
        show_frame,
        error_resilient_mode,
        intra_only,
        color_config,
        frame_width,
        frame_height,
        render_width,
        render_height,
        reset_frame_context,
        refresh_frame_flags,
        refresh_frame_context,
        frame_parallel_decoding_mode,
        frame_context_idx,
        loop_filter,
        quantization,
        segmentation,
        tile_info,
        header_size_in_bytes,
        uncompressed_header_size_bytes,
        ref_frame_idx: inter_fields.ref_frame_idx,
        ref_frame_sign_bias: inter_fields.ref_frame_sign_bias,
        allow_high_precision_mv: inter_fields.allow_high_precision_mv,
        interpolation_filter: inter_fields.interpolation_filter,
    })
}

/// Section 6.2.7 `read_interpolation_filter()`.
fn read_interpolation_filter(br: &mut BitReader<'_>) -> Result<u8, Error> {
    const SWITCHABLE: u8 = 4;
    const LITERAL_TO_TYPE: [u8; 4] = [1, 0, 2, 3];
    if br.read_flag()? {
        Ok(SWITCHABLE)
    } else {
        let raw = br.read_bits(2)? as usize;
        Ok(LITERAL_TO_TYPE[raw])
    }
}

fn read_frame_sync_code(br: &mut BitReader<'_>) -> Result<(), Error> {
    // Section 7.2.1: 0x49 0x83 0x42.
    let b0 = br.read_bits(8)?;
    let b1 = br.read_bits(8)?;
    let b2 = br.read_bits(8)?;
    if b0 != 0x49 || b1 != 0x83 || b2 != 0x42 {
        return Err(Error::InvalidBitstream);
    }
    Ok(())
}

fn read_color_config(br: &mut BitReader<'_>, profile: u8) -> Result<ColorConfig, Error> {
    let bit_depth = if profile >= 2 {
        let ten_or_twelve_bit = br.read_bits(1)?;
        if ten_or_twelve_bit == 1 { 12 } else { 10 }
    } else {
        8
    };

    let color_space = ColorSpace::from_bits(br.read_bits(3)?);
    let color_range_full;
    let mut subsampling_x = true;
    let mut subsampling_y = true;

    if !matches!(color_space, ColorSpace::Rgb) {
        color_range_full = br.read_flag()?;
        if profile == 1 || profile == 3 {
            subsampling_x = br.read_flag()?;
            subsampling_y = br.read_flag()?;
            // Section 7.2.2 prohibits 4:2:0 for profiles 1 and 3.
            if (profile & 1) == 1 && subsampling_x && subsampling_y {
                return Err(Error::InvalidBitstream);
            }
            let reserved_zero = br.read_bits(1)?;
            if reserved_zero != 0 {
                return Err(Error::InvalidBitstream);
            }
        }
    } else {
        // Section 7.2.2: RGB is full range 4:4:4.
        color_range_full = true;
        subsampling_x = false;
        subsampling_y = false;
        // Section 7.2.2: no RGB when profile_low_bit is 0.
        if profile == 0 || profile == 2 {
            return Err(Error::InvalidBitstream);
        }
        if profile == 1 || profile == 3 {
            let reserved_zero = br.read_bits(1)?;
            if reserved_zero != 0 {
                return Err(Error::InvalidBitstream);
            }
        }
    }

    Ok(ColorConfig { bit_depth, color_space, color_range_full, subsampling_x, subsampling_y })
}

fn read_frame_size(br: &mut BitReader<'_>) -> Result<(u32, u32), Error> {
    let frame_width_minus_1 = br.read_bits(16)?;
    let frame_height_minus_1 = br.read_bits(16)?;
    Ok((frame_width_minus_1 + 1, frame_height_minus_1 + 1))
}

fn read_render_size(
    br: &mut BitReader<'_>,
    frame_width: u32,
    frame_height: u32,
) -> Result<(u32, u32), Error> {
    let render_and_frame_size_different = br.read_flag()?;
    if render_and_frame_size_different {
        let render_width_minus_1 = br.read_bits(16)?;
        let render_height_minus_1 = br.read_bits(16)?;
        Ok((render_width_minus_1 + 1, render_height_minus_1 + 1))
    } else {
        Ok((frame_width, frame_height))
    }
}

fn read_loop_filter_params(br: &mut BitReader<'_>) -> Result<LoopFilterParams, Error> {
    let level = br.read_bits(6)? as u8;
    let sharpness = br.read_bits(3)? as u8;
    let delta_enabled = br.read_flag()?;
    let mut delta_update = false;
    let mut ref_deltas: [Option<i8>; 4] = [None; 4];
    let mut mode_deltas: [Option<i8>; 2] = [None; 2];
    if delta_enabled {
        delta_update = br.read_flag()?;
        if delta_update {
            for slot in ref_deltas.iter_mut() {
                let update = br.read_flag()?;
                if update {
                    *slot = Some(br.read_signed(6)? as i8);
                }
            }
            for slot in mode_deltas.iter_mut() {
                let update = br.read_flag()?;
                if update {
                    *slot = Some(br.read_signed(6)? as i8);
                }
            }
        }
    }
    Ok(LoopFilterParams { level, sharpness, delta_enabled, delta_update, ref_deltas, mode_deltas })
}

fn read_delta_q(br: &mut BitReader<'_>) -> Result<i8, Error> {
    let delta_coded = br.read_flag()?;
    if delta_coded { Ok(br.read_signed(4)? as i8) } else { Ok(0) }
}

fn read_quantization_params(br: &mut BitReader<'_>) -> Result<QuantizationParams, Error> {
    let base_q_idx = br.read_bits(8)? as u8;
    let delta_q_y_dc = read_delta_q(br)?;
    let delta_q_uv_dc = read_delta_q(br)?;
    let delta_q_uv_ac = read_delta_q(br)?;
    let lossless = base_q_idx == 0 && delta_q_y_dc == 0 && delta_q_uv_dc == 0 && delta_q_uv_ac == 0;
    Ok(QuantizationParams { base_q_idx, delta_q_y_dc, delta_q_uv_dc, delta_q_uv_ac, lossless })
}

fn read_prob(br: &mut BitReader<'_>) -> Result<u8, Error> {
    let prob_coded = br.read_flag()?;
    if prob_coded { Ok(br.read_bits(8)? as u8) } else { Ok(255) }
}

fn read_segmentation_params(br: &mut BitReader<'_>) -> Result<SegmentationParams, Error> {
    let mut out = SegmentationParams::default_disabled();
    out.enabled = br.read_flag()?;
    if !out.enabled {
        return Ok(out);
    }
    out.update_map = br.read_flag()?;
    if out.update_map {
        let mut tree = [0u8; 7];
        for slot in tree.iter_mut() {
            *slot = read_prob(br)?;
        }
        out.tree_probs = Some(tree);
        out.temporal_update = br.read_flag()?;
        let mut pred = [255u8; 3];
        for slot in pred.iter_mut() {
            *slot = if out.temporal_update { read_prob(br)? } else { 255 };
        }
        out.pred_prob = Some(pred);
    }
    out.update_data = br.read_flag()?;
    if out.update_data {
        out.abs_or_delta_update = br.read_flag()?;
        for i in 0..MAX_SEGMENTS {
            for j in 0..SEG_LVL_MAX {
                let feature_enabled = br.read_flag()?;
                out.feature_enabled[i][j] = feature_enabled;
                if feature_enabled {
                    let bits_to_read = SEGMENTATION_FEATURE_BITS[j];
                    let mut feature_value: i32 =
                        if bits_to_read > 0 { br.read_bits(bits_to_read)? as i32 } else { 0 };
                    if SEGMENTATION_FEATURE_SIGNED[j] {
                        let feature_sign = br.read_flag()?;
                        if feature_sign {
                            feature_value = -feature_value;
                        }
                    }
                    out.feature_data[i][j] = feature_value as i16;
                }
            }
        }
    }
    Ok(out)
}

fn read_tile_info(br: &mut BitReader<'_>, frame_width: u32) -> Result<TileInfo, Error> {
    // Section 6.2.6: MiCols = (W+7)>>3, Sb64Cols = (MiCols+7)>>3.
    let mi_cols = (frame_width + 7) >> 3;
    let sb64_cols = (mi_cols + 7) >> 3;
    let min_log2 = calc_min_log2_tile_cols(sb64_cols);
    let max_log2 = calc_max_log2_tile_cols(sb64_cols);
    let mut tile_cols_log2 = min_log2;
    while tile_cols_log2 < max_log2 {
        let increment = br.read_flag()?;
        if increment {
            tile_cols_log2 += 1;
        } else {
            break;
        }
    }
    // Section 7.2.11.
    if tile_cols_log2 > 6 {
        return Err(Error::InvalidBitstream);
    }

    let tile_rows_log2_bit0 = br.read_flag()?;
    let mut tile_rows_log2 = u8::from(tile_rows_log2_bit0);
    if tile_rows_log2_bit0 {
        let increment = br.read_flag()?;
        tile_rows_log2 += u8::from(increment);
    }

    Ok(TileInfo { tile_cols_log2, tile_rows_log2 })
}

fn calc_min_log2_tile_cols(sb64_cols: u32) -> u8 {
    let mut min_log2 = 0u8;
    while (64u32 << min_log2) < sb64_cols {
        min_log2 += 1;
    }
    min_log2
}

fn calc_max_log2_tile_cols(sb64_cols: u32) -> u8 {
    let mut max_log2 = 1u8;
    while (sb64_cols >> max_log2) >= 4 {
        max_log2 += 1;
    }
    max_log2 - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_max_log2_tile_cols_small_frame() {
        let mi_cols = (320u32 + 7) >> 3;
        let sb64_cols = (mi_cols + 7) >> 3;
        assert_eq!(sb64_cols, 5);
        assert_eq!(calc_min_log2_tile_cols(sb64_cols), 0);
        assert_eq!(calc_max_log2_tile_cols(sb64_cols), 0);
    }

    #[test]
    fn min_max_log2_tile_cols_4k_frame() {
        let sb64_cols = (((3840u32 + 7) >> 3) + 7) >> 3;
        assert_eq!(sb64_cols, 60);
        assert_eq!(calc_min_log2_tile_cols(sb64_cols), 0);
        assert_eq!(calc_max_log2_tile_cols(sb64_cols), 3);
    }

    #[test]
    fn inter_frame_header_parses_against_i_then_p_fixture() {
        let p_frame: [u8; 23] = [
            0x86, 0x00, 0x40, 0x92, 0x9c, 0x08, 0x51, 0x80, 0x00, 0x03, 0x60, 0x00, 0x00, 0x7a,
            0x49, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let ref_dims = [(64u32, 64u32); 8];
        let state = RefFrameState {
            ref_dims: &ref_dims,
            color_config: ColorConfig::default_intra_only_profile0(),
        };
        let hdr =
            parse_uncompressed_header_with_refs(&p_frame, Some(state)).expect("inter header parse");

        assert_eq!(hdr.frame_type, FrameType::NonKeyFrame);
        assert!(!hdr.intra_only);
        assert!(hdr.show_frame);
        assert_eq!(hdr.refresh_frame_flags, 0x01);
        assert_eq!(hdr.frame_width, 64);
        assert_eq!(hdr.frame_height, 64);
        assert_eq!(hdr.ref_frame_idx, Some([0, 1, 2]));
        assert_eq!(hdr.ref_frame_sign_bias, [false, false, false]);
        assert!(hdr.allow_high_precision_mv);
        // raw_interpolation_filter=1 maps to EIGHTTAP = 0 via literal_to_type.
        // The trace label uses a different filter numbering.
        assert_eq!(hdr.interpolation_filter, 0);
        assert_eq!(hdr.header_size_in_bytes, 3);
        assert_eq!(hdr.uncompressed_header_size_bytes, 10);
    }

    #[test]
    fn inter_frame_without_ref_state_is_unsupported() {
        let p_frame: [u8; 23] = [
            0x86, 0x00, 0x40, 0x92, 0x9c, 0x08, 0x51, 0x80, 0x00, 0x03, 0x60, 0x00, 0x00, 0x7a,
            0x49, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(matches!(parse_uncompressed_header(&p_frame), Err(Error::Unsupported)));
    }
}
