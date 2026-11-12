pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_TILE_GROUP: u8 = 4;

const PRIMARY_REF_NONE: u8 = 7;
const MAX_TILE_WIDTH: u32 = 4096;
const MAX_TILE_AREA: u32 = 4096 * 2304;
const MAX_TILE_COLS: u32 = 64;
const MAX_TILE_ROWS: u32 = 64;
const SB_SIZE_LOG2: u32 = 4 + 2;

#[derive(Debug, Default)]
pub struct Bits {
    buf: Vec<u8>,
    len: usize,
}

impl Bits {
    pub fn put(&mut self, v: u64, n: u32) {
        for i in (0..n).rev() {
            if self.len.is_multiple_of(8) {
                self.buf.push(0);
            }
            if v >> i & 1 == 1 {
                let last = self.buf.len() - 1;
                self.buf[last] |= 0x80 >> (self.len % 8);
            }
            self.len += 1;
        }
    }

    pub fn flag(&mut self, b: bool) {
        self.put(u64::from(b), 1);
    }

    pub fn pos(&self) -> usize {
        self.len
    }

    fn trailing(&mut self) {
        let pad = 8 - self.len % 8;
        self.put(1, 1);
        self.put(0, pad as u32 - 1);
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequence {
    pub level_idx: u8,
    pub tier: u8,
    pub width: u32,
    pub height: u32,
    pub ten_bit: bool,
    pub enable_cdef: bool,
    pub color: Option<(u8, u8, u8)>,
    pub full_range: bool,
}

impl Sequence {
    fn width_bits(&self) -> u32 {
        32 - self.width.leading_zeros()
    }

    fn height_bits(&self) -> u32 {
        32 - self.height.leading_zeros()
    }
}

fn tile_log2(blk: u32, target: u32) -> u32 {
    let mut k = 0;
    while (blk << k) < target {
        k += 1;
    }
    k
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tiles {
    pub sb_cols: u32,
    pub sb_rows: u32,
    pub max_log2_cols: u32,
    pub max_log2_rows: u32,
}

impl Tiles {
    pub fn single(width: u32, height: u32) -> Option<Tiles> {
        let mi_cols = 2 * width.div_ceil(8);
        let mi_rows = 2 * height.div_ceil(8);
        let (sb_cols, sb_rows) = (mi_cols.div_ceil(16), mi_rows.div_ceil(16));
        let max_tile_width_sb = MAX_TILE_WIDTH >> SB_SIZE_LOG2;
        let max_tile_area_sb = MAX_TILE_AREA >> (2 * SB_SIZE_LOG2);
        let min_log2_cols = tile_log2(max_tile_width_sb, sb_cols);
        let min_log2_tiles = min_log2_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
        (min_log2_tiles == 0).then_some(Tiles {
            sb_cols,
            sb_rows,
            max_log2_cols: tile_log2(1, sb_cols.min(MAX_TILE_COLS)),
            max_log2_rows: tile_log2(1, sb_rows.min(MAX_TILE_ROWS)),
        })
    }
}

fn obu<T>(obu_type: u8, size_len: usize, payload: impl FnOnce(&mut Bits) -> T) -> (Vec<u8>, T) {
    let mut w = Bits::default();
    // obu_forbidden_bit, obu_type, obu_extension_flag, obu_has_size_field, obu_reserved_1bit
    w.put(u64::from(obu_type) << 3 | 0b010, 8);
    w.put(0, 8 * size_len as u32);
    let at = payload(&mut w);
    w.trailing();
    let mut bytes = w.into_bytes();
    let mut size = bytes.len() - 1 - size_len;
    // leb128 at the fixed length, every byte but the last carrying the continuation bit
    for i in 0..size_len {
        bytes[1 + i] = (size & 0x7F) as u8 | if i + 1 < size_len { 0x80 } else { 0 };
        size >>= 7;
    }
    (bytes, at)
}

pub fn sequence_header(s: &Sequence, size_len: usize) -> Vec<u8> {
    obu(OBU_SEQUENCE_HEADER, size_len, |w| {
        w.put(0, 3); // seq_profile, Main
        w.flag(false); // still_picture
        w.flag(false); // reduced_still_picture_header
        w.flag(false); // timing_info_present_flag
        w.flag(false); // initial_display_delay_present_flag
        w.put(0, 5); // operating_points_cnt_minus_1
        w.put(0, 12); // operating_point_idc[0]
        w.put(u64::from(s.level_idx), 5); // seq_level_idx[0]
        if s.level_idx > 7 {
            w.put(u64::from(s.tier), 1); // seq_tier[0]
        }
        let (wb, hb) = (s.width_bits(), s.height_bits());
        w.put(u64::from(wb - 1), 4); // frame_width_bits_minus_1
        w.put(u64::from(hb - 1), 4); // frame_height_bits_minus_1
        w.put(u64::from(s.width - 1), wb); // max_frame_width_minus_1
        w.put(u64::from(s.height - 1), hb); // max_frame_height_minus_1
        w.flag(false); // frame_id_numbers_present_flag
        w.flag(false); // use_128x128_superblock
        w.flag(false); // enable_filter_intra
        w.flag(false); // enable_intra_edge_filter
        w.flag(false); // enable_interintra_compound
        w.flag(false); // enable_masked_compound
        w.flag(false); // enable_warped_motion
        w.flag(false); // enable_dual_filter
        w.flag(true); // enable_order_hint
        w.flag(false); // enable_jnt_comp
        w.flag(false); // enable_ref_frame_mvs
        w.flag(false); // seq_choose_screen_content_tools
        w.put(0, 1); // seq_force_screen_content_tools
        w.put(7, 3); // order_hint_bits_minus_1
        w.flag(false); // enable_superres
        w.flag(s.enable_cdef); // enable_cdef
        w.flag(false); // enable_restoration
        w.flag(s.ten_bit); // high_bitdepth
        w.flag(false); // mono_chrome
        w.flag(s.color.is_some()); // color_description_present_flag
        if let Some((p, t, m)) = s.color {
            w.put(u64::from(p), 8);
            w.put(u64::from(t), 8);
            w.put(u64::from(m), 8);
        }
        w.flag(s.full_range); // color_range, BT.709 primaries with sRGB and identity are no YUV
        w.put(0, 2); // chroma_sample_position, unknown
        w.flag(false); // separate_uv_delta_q
        w.flag(false); // film_grain_params_present
    })
    .0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub key: bool,
    pub order_hint: u8,
    pub refresh_frame_flags: u8,
    pub primary_ref_frame: u8,
    pub ref_frame_idx: [u8; 7],
    pub base_q_idx: u8,
    pub tx_mode_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    pub bytes: Vec<u8>,
    pub bits: usize,
    pub qindex_offset: usize,
    pub loopfilter_offset: usize,
    pub cdef_offset: usize,
    pub cdef_size: usize,
}

impl Frame {
    pub fn primary(&self) -> u8 {
        if self.key { PRIMARY_REF_NONE } else { self.primary_ref_frame }
    }
}

pub fn frame_header(s: &Sequence, f: &Frame, t: &Tiles, size_len: usize) -> FrameHeader {
    let (bytes, (q, lf, cdef)) = obu(OBU_FRAME_HEADER, size_len, |w| {
        w.flag(false); // show_existing_frame
        w.put(if f.key { 0 } else { 1 }, 2); // frame_type, KEY or INTER
        w.flag(true); // show_frame
        if !f.key {
            // a shown key frame is error resilient without saying so
            w.flag(false); // error_resilient_mode
        }
        w.flag(false); // disable_cdf_update
        w.flag(false); // frame_size_override_flag
        w.put(u64::from(f.order_hint), 8); // order_hint
        if !f.key {
            w.put(u64::from(f.primary_ref_frame), 3); // primary_ref_frame
            w.put(u64::from(f.refresh_frame_flags), 8); // refresh_frame_flags
        }
        if f.key {
            w.flag(false); // render_and_frame_size_different
        } else {
            w.flag(false); // frame_refs_short_signaling
            for &i in &f.ref_frame_idx {
                w.put(u64::from(i), 3); // ref_frame_idx[i]
            }
            w.flag(false); // render_and_frame_size_different
            w.flag(false); // allow_high_precision_mv
            w.flag(false); // is_filter_switchable
            w.put(0, 2); // interpolation_filter, EIGHTTAP
            w.flag(false); // is_motion_mode_switchable
        }
        w.flag(false); // disable_frame_end_update_cdf
        w.flag(true); // uniform_tile_spacing_flag
        if t.max_log2_cols > 0 {
            w.flag(false); // increment_tile_cols_log2
        }
        if t.max_log2_rows > 0 {
            w.flag(false); // increment_tile_rows_log2
        }
        let q = w.pos();
        w.put(u64::from(f.base_q_idx), 8); // base_q_idx
        w.flag(false); // delta_coded, delta_q_y_dc
        w.flag(false); // delta_coded, delta_q_u_dc
        w.flag(false); // delta_coded, delta_q_u_ac
        w.flag(false); // using_qmatrix
        w.flag(false); // segmentation_enabled
        if f.base_q_idx > 0 {
            w.flag(false); // delta_q_present
        }
        // loop_filter_params, levels 0 leave the chroma levels out
        let lf = w.pos();
        w.put(0, 6); // loop_filter_level[0]
        w.put(0, 6); // loop_filter_level[1]
        w.put(0, 3); // loop_filter_sharpness
        w.flag(false); // loop_filter_delta_enabled
        let cdef = s.enable_cdef.then(|| {
            let at = w.pos();
            w.put(0, 2); // cdef_damping_minus_3
            w.put(0, 2); // cdef_bits
            w.put(0, 4); // cdef_y_pri_strength[0]
            w.put(0, 2); // cdef_y_sec_strength[0]
            w.put(0, 4); // cdef_uv_pri_strength[0]
            w.put(0, 2); // cdef_uv_sec_strength[0]
            (at, w.pos() - at)
        });
        w.flag(f.tx_mode_select); // tx_mode_select, SELECT or LARGEST
        if !f.key {
            w.flag(false); // reference_select
        }
        w.flag(false); // reduced_tx_set
        if !f.key {
            for _ in 0..7 {
                w.flag(false); // is_global[ref]
            }
        }
        (q, lf, cdef)
    });
    let (cdef_offset, cdef_size) = cdef.unwrap_or((0, 0));
    FrameHeader {
        bits: bytes.len() * 8,
        bytes,
        qindex_offset: q,
        loopfilter_offset: lf,
        cdef_offset,
        cdef_size,
    }
}

pub fn tile_group(data: &[u8], size_len: usize) -> Vec<u8> {
    let mut bytes = vec![OBU_TILE_GROUP << 3 | 0b010];
    let mut size = data.len();
    for i in 0..size_len {
        bytes.push((size & 0x7F) as u8 | if i + 1 < size_len { 0x80 } else { 0 });
        size >>= 7;
    }
    bytes.extend_from_slice(data);
    bytes
}

pub fn level_idx(width: u32, height: u32, fps: f64) -> u8 {
    // (level_idx, MaxPicSize, MaxHSize, MaxVSize, MaxDisplayRate)
    const LEVELS: [(u8, u64, u32, u32, u64); 14] = [
        (0, 147_456, 2048, 1152, 4_423_680),
        (1, 278_784, 2816, 1584, 8_363_520),
        (4, 665_856, 4352, 2448, 19_975_680),
        (5, 1_065_024, 5504, 3096, 31_950_720),
        (8, 2_359_296, 6144, 3456, 70_778_880),
        (9, 2_359_296, 6144, 3456, 141_557_760),
        (12, 8_912_896, 8192, 4352, 267_386_880),
        (13, 8_912_896, 8192, 4352, 534_773_760),
        (14, 8_912_896, 8192, 4352, 1_069_547_520),
        (15, 8_912_896, 8192, 4352, 1_069_547_520),
        (16, 35_651_584, 16384, 8704, 1_069_547_520),
        (17, 35_651_584, 16384, 8704, 2_139_095_040),
        (18, 35_651_584, 16384, 8704, 4_278_190_080),
        (19, 35_651_584, 16384, 8704, 4_278_190_080),
    ];
    let pic = u64::from(width) * u64::from(height);
    let rate = (pic as f64 * fps) as u64;
    // past the table the maximum parameters level, 31
    LEVELS
        .iter()
        .find(|l| pic <= l.1 && width <= l.2 && height <= l.3 && rate <= l.4)
        .map_or(31, |l| l.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq() -> Sequence {
        Sequence {
            level_idx: 8,
            tier: 0,
            width: 1920,
            height: 1080,
            ten_bit: false,
            enable_cdef: false,
            color: None,
            full_range: false,
        }
    }

    #[test]
    fn an_obu_is_its_header_its_fixed_size_field_and_whole_bytes() {
        let s = sequence_header(&seq(), 4);
        assert_eq!(s[0], 0x0A, "a sequence header with a size field");
        let size = s[1..5]
            .iter()
            .enumerate()
            .fold(0usize, |a, (i, b)| a | usize::from(b & 0x7F) << (7 * i));
        assert!(
            s[1..4].iter().all(|b| b & 0x80 != 0) && s[4] & 0x80 == 0,
            "continuation on all but the last"
        );
        assert_eq!(size, s.len() - 5);
        assert_ne!(s[s.len() - 1], 0, "the trailing one is in the last byte");
    }

    #[test]
    fn the_rate_control_offsets_point_at_their_fields() {
        let t = Tiles::single(1920, 1080).unwrap();
        let s = Sequence { enable_cdef: true, ..seq() };
        let f = Frame {
            key: true,
            order_hint: 0,
            refresh_frame_flags: 0xFF,
            primary_ref_frame: 0,
            ref_frame_idx: [0; 7],
            base_q_idx: 0xA5,
            tx_mode_select: true,
        };
        let h = frame_header(&s, &f, &t, 4);
        let read = |at: usize, n: usize| {
            (0..n).fold(0u32, |a, i| {
                a << 1 | u32::from(h.bytes[(at + i) / 8] >> (7 - (at + i) % 8) & 1)
            })
        };
        assert_eq!(read(h.qindex_offset, 8), 0xA5, "base_q_idx where it says");
        assert_eq!(h.cdef_size, 16, "damping, bits and one strength pair for luma and chroma");
        assert_eq!(
            h.cdef_offset,
            h.loopfilter_offset + 16,
            "cdef right behind two levels, sharpness and the delta flag"
        );
        assert_eq!(h.bits, h.bytes.len() * 8);
    }

    #[test]
    fn one_tile_up_to_4k_and_not_past_it() {
        let t = Tiles::single(3840, 2160).unwrap();
        assert_eq!((t.sb_cols, t.sb_rows), (60, 34));
        assert!(Tiles::single(4096, 2304).is_some());
        assert!(Tiles::single(7680, 4320).is_none(), "8K takes several tiles");
        assert!(Tiles::single(4160, 2160).is_none(), "past the widest tile");
    }

    #[test]
    fn levels_follow_the_table() {
        assert_eq!(level_idx(1920, 1080, 30.0), 8, "4.0");
        assert_eq!(level_idx(1920, 1080, 60.0), 9, "4.1");
        assert_eq!(level_idx(3840, 2160, 60.0), 13, "5.1");
        assert_eq!(level_idx(1280, 720, 30.0), 5, "3.1");
    }
}
