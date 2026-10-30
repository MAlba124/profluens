//! The AV1 parser of vaapiav1dec held against dav1d field by field. No device needed.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/av1.rs"]
mod av1;
#[path = "common/dav1d_layout.rs"]
mod d;

use libdav1d_sys as sys;
use pf_vaapi::av1dec::header::{
    FrameHeader, FrameType, NUM_REF_FRAMES, RESTORE_NONE, RESTORE_SGRPROJ, RESTORE_SWITCHABLE,
    RESTORE_WIENER, qindex,
};
use pf_vaapi::av1dec::{SequenceHeader, Stream};

fn at<T: Copy>(base: &[u8], off: usize) -> T {
    assert!(off + std::mem::size_of::<T>() <= base.len());
    // SAFETY: in bounds, T a plain integer or array of them read unaligned
    unsafe { std::ptr::read_unaligned(base.as_ptr().add(off).cast::<T>()) }
}

struct Shown {
    fh: Vec<u8>,
    w: i32,
    h: i32,
}

struct Dav1d(*mut sys::Dav1dContext);

impl Dav1d {
    fn new() -> Dav1d {
        // SAFETY: zero is a valid settings struct for dav1d_default_settings to fill
        let mut s: sys::Dav1dSettings = unsafe { std::mem::zeroed() };
        // SAFETY: s is a valid out pointer
        unsafe { sys::dav1d_default_settings(&mut s) };
        (s.n_threads, s.max_frame_delay, s.apply_grain) = (1, 1, 0);
        s.logger.callback = None;
        let mut c = std::ptr::null_mut();
        // SAFETY: c and s valid
        assert_eq!(unsafe { sys::dav1d_open(&mut c, &s) }, 0);
        Dav1d(c)
    }

    fn take(&mut self, out: &mut Vec<Shown>) {
        loop {
            // SAFETY: zero is a valid empty picture
            let mut pic: sys::Dav1dPicture = unsafe { std::mem::zeroed() };
            // SAFETY: the context and the picture are valid
            if unsafe { sys::dav1d_get_picture(self.0, &mut pic) } != 0 {
                return;
            }
            assert!(!pic.frame_hdr.is_null());
            // SAFETY: frame_hdr points at a live Dav1dFrameHeader for as long as pic
            let fh = unsafe { std::slice::from_raw_parts(pic.frame_hdr.cast::<u8>(), d::FH_SIZE) }
                .to_vec();
            out.push(Shown { fh, w: pic.p.w, h: pic.p.h });
            // SAFETY: pic is ours, released once
            unsafe { sys::dav1d_picture_unref(&mut pic) };
        }
    }

    fn unit(&mut self, tu: &[u8]) -> Vec<Shown> {
        let mut out = Vec::new();
        // SAFETY: zero is a valid empty Dav1dData
        let mut data: sys::Dav1dData = unsafe { std::mem::zeroed() };
        // SAFETY: dav1d allocates tu.len() bytes and hands them back
        let buf = unsafe { sys::dav1d_data_create(&mut data, tu.len()) };
        assert!(!buf.is_null());
        // SAFETY: buf holds tu.len() bytes
        unsafe { std::ptr::copy_nonoverlapping(tu.as_ptr(), buf, tu.len()) };
        while data.sz > 0 {
            // SAFETY: the context and the data are valid, dav1d takes what it consumes
            let r = unsafe { sys::dav1d_send_data(self.0, &mut data) };
            if r != 0 && r != -11 {
                panic!("dav1d refused the unit, {r}");
            }
            self.take(&mut out);
        }
        self.take(&mut out);
        out
    }
}

impl Drop for Dav1d {
    fn drop(&mut self) {
        // SAFETY: the context is ours, closed once
        unsafe { sys::dav1d_close(&mut self.0) };
    }
}

#[derive(Default, Debug)]
struct Seen {
    frames: usize,
    shown: usize,
    hidden: usize,
    show_existing: usize,
    multi_tile: usize,
    tile_groups: usize,
    segmentation: usize,
    film_grain: usize,
    superres: usize,
    cdef: usize,
    restoration: usize,
    global_motion: usize,
    error_resilient: usize,
    sizes: Vec<(u32, u32)>,
}

fn lr_type(t: u8) -> i32 {
    match t {
        RESTORE_NONE => 0,
        RESTORE_SWITCHABLE => 1,
        RESTORE_WIENER => 2,
        RESTORE_SGRPROJ => 3,
        _ => -1,
    }
}

fn check(name: &str, n: usize, seq: &SequenceHeader, h: &FrameHeader, fh: &[u8]) {
    let ctx = format!("{name}: unit {n}");
    let intra = h.frame_type.is_intra();
    assert_eq!(at::<i32>(fh, d::FH_FRAME_TYPE), h.frame_type as i32, "{ctx} frame_type");
    assert_eq!(
        at::<[i32; 2]>(fh, d::FH_WIDTH),
        [h.frame_width as i32, h.upscaled_width as i32],
        "{ctx} width"
    );
    assert_eq!(at::<i32>(fh, d::FH_HEIGHT), h.frame_height as i32, "{ctx} height");
    assert_eq!(at::<i32>(fh, d::FH_RENDER_WIDTH), h.render_width as i32, "{ctx} render_width");
    assert_eq!(at::<i32>(fh, d::FH_RENDER_HEIGHT), h.render_height as i32, "{ctx} render_height");
    assert_eq!(at::<u8>(fh, d::FH_FRAME_OFFSET), h.order_hint, "{ctx} order_hint");
    assert_eq!(at::<u8>(fh, d::FH_SHOW_FRAME), u8::from(h.show_frame), "{ctx} show_frame");
    assert_eq!(
        at::<u8>(fh, d::FH_SHOWABLE_FRAME),
        u8::from(h.showable_frame),
        "{ctx} showable_frame"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_ERROR_RESILIENT_MODE),
        u8::from(h.error_resilient_mode),
        "{ctx} error_resilient_mode"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_DISABLE_CDF_UPDATE),
        u8::from(h.disable_cdf_update),
        "{ctx} disable_cdf_update"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_ALLOW_SCREEN_CONTENT_TOOLS),
        u8::from(h.allow_screen_content_tools),
        "{ctx} sct"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_FORCE_INTEGER_MV),
        u8::from(h.force_integer_mv),
        "{ctx} force_integer_mv"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_FRAME_SIZE_OVERRIDE),
        u8::from(h.frame_size_override_flag),
        "{ctx} frame_size_override"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_PRIMARY_REF_FRAME),
        h.primary_ref_frame,
        "{ctx} primary_ref_frame"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_REFRESH_FRAME_FLAGS),
        h.refresh_frame_flags,
        "{ctx} refresh_frame_flags"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_SUPER_RES_ENABLED),
        u8::from(h.use_superres),
        "{ctx} use_superres"
    );
    assert_eq!(at::<u8>(fh, d::FH_SUPER_RES_DENOM), h.superres_denom, "{ctx} superres_denom");
    assert_eq!(at::<u8>(fh, d::FH_ALLOW_INTRABC), u8::from(h.allow_intrabc), "{ctx} allow_intrabc");
    if !intra {
        assert_eq!(
            at::<[i8; 7]>(fh, d::FH_REFIDX),
            h.ref_frame_idx.map(|i| i as i8),
            "{ctx} ref_frame_idx"
        );
        assert_eq!(
            at::<u8>(fh, d::FH_HP),
            u8::from(h.allow_high_precision_mv),
            "{ctx} allow_high_precision_mv"
        );
        assert_eq!(
            at::<i32>(fh, d::FH_SUBPEL_FILTER_MODE),
            i32::from(h.interpolation_filter),
            "{ctx} interpolation_filter"
        );
        assert_eq!(
            at::<u8>(fh, d::FH_SWITCHABLE_MOTION_MODE),
            u8::from(h.is_motion_mode_switchable),
            "{ctx} motion mode"
        );
        assert_eq!(
            at::<u8>(fh, d::FH_USE_REF_FRAME_MVS),
            u8::from(h.use_ref_frame_mvs),
            "{ctx} use_ref_frame_mvs"
        );
    }
    assert_eq!(
        at::<u8>(fh, d::FH_REFRESH_CONTEXT),
        u8::from(!h.disable_frame_end_update_cdf),
        "{ctx} refresh_context"
    );
    let t = &h.tile_info;
    assert_eq!(at::<u8>(fh, d::FH_TILING_UNIFORM), u8::from(t.uniform), "{ctx} uniform");
    assert_eq!(
        at::<[u8; 2]>(fh, d::FH_TILING_LOG2_COLS),
        [t.cols_log2, t.cols as u8],
        "{ctx} log2_cols, cols"
    );
    assert_eq!(
        at::<[u8; 2]>(fh, d::FH_TILING_LOG2_ROWS),
        [t.rows_log2, t.rows as u8],
        "{ctx} log2_rows, rows"
    );
    let mut start = 0u16;
    for i in 0..usize::from(t.cols) {
        assert_eq!(
            at::<u16>(fh, d::FH_TILING_COL_START_SB + 2 * i),
            start,
            "{ctx} col_start_sb[{i}]"
        );
        start += t.width_in_sbs[i];
    }
    let mut start = 0u16;
    for i in 0..usize::from(t.rows) {
        assert_eq!(
            at::<u16>(fh, d::FH_TILING_ROW_START_SB + 2 * i),
            start,
            "{ctx} row_start_sb[{i}]"
        );
        start += t.height_in_sbs[i];
    }
    if t.cols_log2 > 0 || t.rows_log2 > 0 {
        assert_eq!(at::<u8>(fh, d::FH_TILING_N_BYTES), t.size_bytes, "{ctx} tile_size_bytes");
        assert_eq!(
            at::<u16>(fh, d::FH_TILING_UPDATE),
            t.context_update_tile_id,
            "{ctx} context_update_tile_id"
        );
    }
    let q = &h.quant;
    let want = [
        q.base_q_idx as i8,
        q.delta_q_y_dc,
        q.delta_q_u_dc,
        q.delta_q_u_ac,
        q.delta_q_v_dc,
        q.delta_q_v_ac,
        i8::from(q.using_qmatrix),
    ];
    assert_eq!(at::<[i8; 7]>(fh, d::FH_QUANT_YAC), want, "{ctx} quantization");
    if q.using_qmatrix {
        assert_eq!(at::<[u8; 3]>(fh, d::FH_QUANT_QM + 1), [q.qm_y, q.qm_u, q.qm_v], "{ctx} qm");
    }
    let s = &h.seg;
    assert_eq!(
        at::<u8>(fh, d::FH_SEGMENTATION_ENABLED),
        u8::from(s.enabled),
        "{ctx} segmentation_enabled"
    );
    if s.enabled {
        let flags = [u8::from(s.update_map), u8::from(s.temporal_update), u8::from(s.update_data)];
        assert_eq!(
            at::<[u8; 3]>(fh, d::FH_SEGMENTATION_ENABLED + 1),
            flags,
            "{ctx} segmentation flags"
        );
        for i in 0..8 {
            let base = d::FH_SEG_DATA + i * d::SEG_DATA_SIZE;
            let on = |j: usize| s.feature_enabled[i] >> j & 1 == 1;
            let v = |j: usize| if on(j) { s.feature_data[i][j] } else { 0 };
            assert_eq!(at::<i16>(fh, base), v(0), "{ctx} segment {i} alt q");
            let lf = [v(1) as i8, v(2) as i8, v(3) as i8, v(4) as i8];
            assert_eq!(
                at::<[i8; 4]>(fh, base + d::SEG_DELTA_LF_Y_V),
                lf,
                "{ctx} segment {i} alt lf"
            );
            assert_eq!(
                at::<i8>(fh, base + d::SEG_REF),
                if on(5) { v(5) as i8 } else { -1 },
                "{ctx} segment {i} ref"
            );
            assert_eq!(
                at::<[u8; 2]>(fh, base + d::SEG_SKIP),
                [u8::from(on(6)), u8::from(on(7))],
                "{ctx} segment {i} skip, globalmv"
            );
        }
    }
    let deltas_zero =
        [q.delta_q_y_dc, q.delta_q_u_dc, q.delta_q_u_ac, q.delta_q_v_dc, q.delta_q_v_ac]
            .iter()
            .all(|&x| x == 0);
    for i in 0..8 {
        let lossless = qindex(h, i) == 0 && deltas_zero;
        assert_eq!(
            at::<u8>(fh, d::FH_SEG_LOSSLESS + i),
            u8::from(lossless),
            "{ctx} segment {i} lossless"
        );
    }
    assert_eq!(
        at::<[u8; 2]>(fh, d::FH_DELTA_Q_PRESENT),
        [u8::from(h.delta_q_present), h.delta_q_res],
        "{ctx} delta_q"
    );
    let dlf = [u8::from(h.delta_lf_present), h.delta_lf_res, u8::from(h.delta_lf_multi)];
    assert_eq!(at::<[u8; 3]>(fh, d::FH_DELTA_LF_PRESENT), dlf, "{ctx} delta_lf");
    assert_eq!(
        at::<u8>(fh, d::FH_ALL_LOSSLESS),
        u8::from(h.coded_lossless),
        "{ctx} coded_lossless"
    );
    let lf = &h.lf;
    if !h.coded_lossless && !h.allow_intrabc {
        assert_eq!(
            at::<[u8; 4]>(fh, d::FH_LOOPFILTER_LEVEL_Y),
            lf.level,
            "{ctx} loop_filter_level"
        );
        assert_eq!(
            at::<u8>(fh, d::FH_LOOPFILTER_LEVEL_Y + 4),
            u8::from(lf.delta_enabled),
            "{ctx} delta_enabled"
        );
        if lf.delta_enabled {
            assert_eq!(
                at::<u8>(fh, d::FH_LOOPFILTER_LEVEL_Y + 5),
                u8::from(lf.delta_update),
                "{ctx} delta_update"
            );
        }
        assert_eq!(at::<u8>(fh, d::FH_LOOPFILTER_SHARPNESS), lf.sharpness, "{ctx} sharpness");
    }
    assert_eq!(at::<[i8; 2]>(fh, d::FH_LF_MODE_DELTA), lf.mode_deltas, "{ctx} mode_deltas");
    assert_eq!(at::<[i8; 8]>(fh, d::FH_LF_REF_DELTA), lf.ref_deltas, "{ctx} ref_deltas");
    if seq.enable_cdef && !h.coded_lossless && !h.allow_intrabc {
        let c = &h.cdef;
        assert_eq!(
            at::<[u8; 2]>(fh, d::FH_CDEF_DAMPING),
            [c.damping_minus_3 + 3, c.bits],
            "{ctx} cdef damping, bits"
        );
        for i in 0..1usize << c.bits {
            assert_eq!(
                at::<u8>(fh, d::FH_CDEF_Y_STRENGTH + i),
                c.y_pri[i] << 2 | c.y_sec[i],
                "{ctx} cdef y {i}"
            );
            assert_eq!(
                at::<u8>(fh, d::FH_CDEF_UV_STRENGTH + i),
                c.uv_pri[i] << 2 | c.uv_sec[i],
                "{ctx} cdef uv {i}"
            );
        }
    }
    let lr = &h.lr;
    assert_eq!(
        at::<[i32; 3]>(fh, d::FH_RESTORATION_TYPE),
        lr.frame_type.map(lr_type),
        "{ctx} lr_type"
    );
    if lr.frame_type.iter().any(|&t| t != RESTORE_NONE) {
        let unit = 6 + lr.unit_shift;
        assert_eq!(
            at::<[u8; 2]>(fh, d::FH_RESTORATION_UNIT_SIZE),
            [unit, unit - lr.uv_shift],
            "{ctx} lr unit size"
        );
    }
    assert_eq!(at::<i32>(fh, d::FH_TXFM_MODE), i32::from(h.tx_mode), "{ctx} tx_mode");
    assert_eq!(
        at::<u8>(fh, d::FH_SWITCHABLE_COMP_REFS),
        u8::from(h.reference_select),
        "{ctx} reference_select"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_SKIP_MODE_ENABLED),
        u8::from(h.skip_mode_present),
        "{ctx} skip_mode_present"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_WARP_MOTION),
        u8::from(h.allow_warped_motion),
        "{ctx} allow_warped_motion"
    );
    assert_eq!(
        at::<u8>(fh, d::FH_REDUCED_TXTP_SET),
        u8::from(h.reduced_tx_set),
        "{ctx} reduced_tx_set"
    );
    for i in 0..7 {
        let base = d::FH_GMV + i * d::GMV_SIZE;
        assert_eq!(at::<i32>(fh, base), i32::from(h.gm.gm_type[i + 1]), "{ctx} gm type {i}");
        assert_eq!(
            at::<[i32; 6]>(fh, base + d::GMV_MATRIX),
            h.gm.params[i + 1],
            "{ctx} gm params {i}"
        );
    }
    check_grain(&ctx, h, fh);
}

fn check_grain(ctx: &str, h: &FrameHeader, fh: &[u8]) {
    let g = &h.film_grain;
    assert_eq!(
        at::<u8>(fh, d::FH_FILM_GRAIN_PRESENT),
        u8::from(g.apply_grain),
        "{ctx} apply_grain"
    );
    if !g.apply_grain {
        return;
    }
    assert_eq!(
        at::<u8>(fh, d::FH_FILM_GRAIN_UPDATE),
        u8::from(g.update_grain),
        "{ctx} update_grain"
    );
    let fg = |off: usize| d::FH_FILM_GRAIN_DATA + off;
    assert_eq!(at::<u32>(fh, fg(0)), u32::from(g.grain_seed), "{ctx} grain_seed");
    assert_eq!(
        at::<i32>(fh, fg(d::FG_NUM_Y_POINTS)),
        i32::from(g.num_y_points),
        "{ctx} num_y_points"
    );
    for i in 0..usize::from(g.num_y_points) {
        assert_eq!(
            at::<[u8; 2]>(fh, fg(d::FG_Y_POINTS) + 2 * i),
            [g.point_y_value[i], g.point_y_scaling[i]],
            "{ctx} y point {i}"
        );
    }
    assert_eq!(
        at::<i32>(fh, fg(d::FG_CHROMA_SCALING_FROM_LUMA)),
        i32::from(g.chroma_scaling_from_luma),
        "{ctx} csfl"
    );
    let uv = [
        (g.num_cb_points, g.point_cb_value, g.point_cb_scaling),
        (g.num_cr_points, g.point_cr_value, g.point_cr_scaling),
    ];
    for (pl, (num, value, scaling)) in uv.iter().enumerate() {
        assert_eq!(
            at::<i32>(fh, fg(d::FG_NUM_UV_POINTS) + 4 * pl),
            i32::from(*num),
            "{ctx} num uv points {pl}"
        );
        for i in 0..usize::from(*num) {
            assert_eq!(
                at::<[u8; 2]>(fh, fg(d::FG_UV_POINTS) + 20 * pl + 2 * i),
                [value[i], scaling[i]],
                "{ctx} uv point {pl} {i}"
            );
        }
    }
    assert_eq!(
        at::<i32>(fh, fg(d::FG_SCALING_SHIFT)),
        i32::from(g.grain_scaling_minus_8) + 8,
        "{ctx} scaling_shift"
    );
    assert_eq!(
        at::<i32>(fh, fg(d::FG_AR_COEFF_LAG)),
        i32::from(g.ar_coeff_lag),
        "{ctx} ar_coeff_lag"
    );
    let lag = usize::from(g.ar_coeff_lag);
    let num_y = 2 * lag * (lag + 1);
    if g.num_y_points > 0 {
        for i in 0..num_y {
            assert_eq!(
                at::<i8>(fh, fg(d::FG_AR_COEFFS_Y) + i),
                (i16::from(g.ar_coeffs_y_plus_128[i]) - 128) as i8,
                "{ctx} ar y {i}"
            );
        }
    }
    let num_uv = num_y + usize::from(g.num_y_points > 0);
    for (pl, coeffs) in [g.ar_coeffs_cb_plus_128, g.ar_coeffs_cr_plus_128].iter().enumerate() {
        if uv[pl].0 > 0 || g.chroma_scaling_from_luma {
            for (i, &c) in coeffs.iter().enumerate().take(num_uv) {
                let got = at::<i8>(fh, fg(d::FG_AR_COEFFS_UV) + 28 * pl + i);
                assert_eq!(got, (i16::from(c) - 128) as i8, "{ctx} ar uv {pl} {i}");
            }
        }
    }
    assert_eq!(
        at::<u64>(fh, fg(d::FG_AR_COEFF_SHIFT)),
        u64::from(g.ar_coeff_shift_minus_6) + 6,
        "{ctx} ar_coeff_shift"
    );
    assert_eq!(
        at::<i32>(fh, fg(d::FG_GRAIN_SCALE_SHIFT)),
        i32::from(g.grain_scale_shift),
        "{ctx} grain_scale_shift"
    );
    let mults = [
        (g.num_cb_points, g.cb_mult, g.cb_luma_mult, g.cb_offset),
        (g.num_cr_points, g.cr_mult, g.cr_luma_mult, g.cr_offset),
    ];
    for (pl, (num, mult, luma, offset)) in mults.iter().enumerate() {
        if *num > 0 {
            assert_eq!(
                at::<i32>(fh, fg(d::FG_UV_MULT) + 4 * pl),
                i32::from(*mult) - 128,
                "{ctx} uv_mult {pl}"
            );
            assert_eq!(
                at::<i32>(fh, fg(d::FG_UV_LUMA_MULT) + 4 * pl),
                i32::from(*luma) - 128,
                "{ctx} uv_luma_mult {pl}"
            );
            assert_eq!(
                at::<i32>(fh, fg(d::FG_UV_OFFSET) + 4 * pl),
                i32::from(*offset) - 256,
                "{ctx} uv_offset {pl}"
            );
        }
    }
    assert_eq!(
        at::<i32>(fh, fg(d::FG_OVERLAP_FLAG)),
        i32::from(g.overlap_flag),
        "{ctx} overlap_flag"
    );
    assert_eq!(
        at::<i32>(fh, fg(d::FG_CLIP_TO_RESTRICTED_RANGE)),
        i32::from(g.clip_to_restricted_range),
        "{ctx} clip"
    );
}

fn hold(name: &str, units: &[Vec<u8>]) -> Seen {
    let mut stream = Stream::new();
    let mut dav1d = Dav1d::new();
    let mut slots: [Option<FrameHeader>; NUM_REF_FRAMES] = [None; NUM_REF_FRAMES];
    let mut seen = Seen::default();
    for (n, tu) in units.iter().enumerate() {
        let mut at = 0;
        let mut shown = Vec::new();
        while let Some(frame) =
            stream.next_frame(tu, &mut at).unwrap_or_else(|e| panic!("{name}: unit {n}: {e}"))
        {
            let h = frame.header;
            if h.show_existing_frame {
                seen.show_existing += 1;
                let original = slots[usize::from(h.frame_to_show_map_idx)]
                    .unwrap_or_else(|| panic!("{name}: unit {n} shows an empty slot"));
                shown.push(original);
                if h.frame_type == FrameType::Key {
                    slots = [Some(original); NUM_REF_FRAMES];
                }
            } else {
                seen.frames += 1;
                let t = &h.tile_info;
                let tiles = usize::from(t.cols) * usize::from(t.rows);
                assert_eq!(stream.tiles.len(), tiles, "{name}: unit {n} tiles");
                let covered: usize = stream.groups.iter().map(|g| g.count).sum();
                assert_eq!(covered, tiles, "{name}: unit {n} tile groups");
                for g in &stream.groups {
                    assert!(
                        g.data_start <= g.data_end && g.data_end <= tu.len(),
                        "{name}: unit {n} group bytes"
                    );
                    for tile in &stream.tiles[g.first..g.first + g.count] {
                        assert!(
                            g.data_start + (tile.offset + tile.size) as usize <= g.data_end,
                            "{name}: unit {n} tile bytes"
                        );
                    }
                }
                seen.multi_tile += usize::from(tiles > 1);
                seen.tile_groups += usize::from(stream.groups.len() > 1);
                seen.segmentation += usize::from(h.seg.enabled);
                seen.film_grain += usize::from(h.film_grain.apply_grain);
                seen.superres += usize::from(h.use_superres);
                seen.cdef +=
                    usize::from(h.cdef.bits > 0 || h.cdef.y_pri[0] > 0 || h.cdef.y_sec[0] > 0);
                seen.restoration += usize::from(h.lr.frame_type.iter().any(|&t| t != RESTORE_NONE));
                seen.global_motion += usize::from(h.gm.gm_type.iter().any(|&t| t != 0));
                seen.error_resilient += usize::from(h.error_resilient_mode && !h.is_intra());
                if !seen.sizes.contains(&(h.frame_width, h.frame_height)) {
                    seen.sizes.push((h.frame_width, h.frame_height));
                }
                if h.show_frame {
                    shown.push(h);
                } else {
                    seen.hidden += 1;
                }
                for (i, s) in slots.iter_mut().enumerate() {
                    if h.refresh_frame_flags >> i & 1 == 1 {
                        *s = Some(h);
                    }
                }
            }
            stream.finish(&frame);
        }
        let pics = dav1d.unit(tu);
        assert_eq!(
            pics.len(),
            shown.len(),
            "{name}: unit {n} shows {} frames, dav1d {}",
            shown.len(),
            pics.len()
        );
        let seq = stream.seq.expect("a sequence header");
        for (h, pic) in shown.iter().zip(&pics) {
            check(name, n, &seq, h, &pic.fh);
            assert_eq!(
                (pic.w, pic.h),
                (h.upscaled_width as i32, h.frame_height as i32),
                "{name}: unit {n} picture size"
            );
        }
        seen.shown += shown.len();
    }
    eprintln!("{name}: {seen:?}");
    seen
}

#[test]
fn the_parser_matches_dav1d_on_every_fixture() {
    for (name, ivf) in av1::IVF {
        let seen = hold(name, &av1::demux_ivf(ivf));
        assert!(seen.shown > 0, "{name}: nothing shown");
        match name {
            "tools_352x288" => {
                assert!(seen.multi_tile > 0 && seen.tile_groups > 0, "{name}: tiles in two groups");
                assert!(
                    seen.hidden > 0 && seen.show_existing > 0,
                    "{name}: hidden alt-refs shown again"
                );
                assert!(
                    seen.segmentation > 0 && seen.global_motion > 0,
                    "{name}: segmentation, global motion"
                );
            }
            "grain_176x144" => assert!(seen.film_grain > 0, "{name}: film grain"),
            "superres_256x144" => assert!(
                seen.superres > 0 && seen.restoration > 0,
                "{name}: superres, loop restoration"
            ),
            "resize_192x128" => assert!(seen.sizes.len() > 1, "{name}: frames of several sizes"),
            "resilient_176x144" => {
                assert!(seen.error_resilient > 0, "{name}: error resilient inter frames")
            }
            _ => {}
        }
    }
}

#[test]
fn the_parser_matches_dav1d_on_the_mkv_fixture() {
    let Some(units) = av1::mkv_units() else {
        return;
    };
    let seen = hold("mkv_av1", &units);
    assert_eq!(seen.shown, units.len(), "one shown frame a unit");
}
