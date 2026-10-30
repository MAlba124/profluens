//! `vaapiav1dec` parser, slot state and VA parameters, no device.

// Test vectors.
#![allow(clippy::disallowed_methods)]

use super::header::{
    ALL_FRAMES, DEFAULT_GM_PARAMS, FrameType, PRIMARY_REF_NONE, TX_MODE_LARGEST, TX_MODE_SELECT,
    shear_valid,
};
use super::refs::RefFrame;
use super::*;
use crate::av1enc::obu::{self as enc, Frame as EncFrame, Sequence, Tiles};

const TOOLS: &[u8] = include_bytes!("../../tests/fixtures/av1_tools_352x288.ivf");
const P10: &[u8] = include_bytes!("../../tests/fixtures/av1_p10_160x96.ivf");
const GRAIN: &[u8] = include_bytes!("../../tests/fixtures/av1_grain_176x144.ivf");
const MKV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/out/mkv_av1.mkv");

fn ivf(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut off = usize::from(u16::from_le_bytes([data[6], data[7]]));
    while off + 12 <= data.len() {
        let size =
            u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        out.push(&data[off + 12..off + 12 + size]);
        off += 12 + size;
    }
    out
}

fn frames(stream: &mut Stream, tu: &[u8]) -> Vec<(Frame, Vec<Group>, Vec<obu::Tile>)> {
    let mut at = 0;
    let mut out = Vec::new();
    while let Some(f) = stream.next_frame(tu, &mut at).unwrap() {
        out.push((f, stream.groups.clone(), stream.tiles.clone()));
        stream.finish(&f);
    }
    out
}

fn seq(width: u32, height: u32, ten_bit: bool) -> Sequence {
    Sequence {
        level_idx: 4,
        tier: 0,
        width,
        height,
        ten_bit,
        enable_cdef: true,
        color: Some((1, 1, 1)),
        full_range: false,
    }
}

#[test]
fn the_encoder_frame_headers_round_trip() {
    let s = seq(320, 240, false);
    let t = Tiles::single(320, 240).unwrap();
    let mut stream = Stream::new();
    let mut slot = 0u8;
    let filler: Vec<u8> = (0..100).map(|i| i as u8).collect();
    for n in 0..4u8 {
        let key = n == 0;
        let next = if key { 0 } else { 1 - slot };
        let f = EncFrame {
            key,
            order_hint: n,
            refresh_frame_flags: if key { 0xFF } else { 1 << next },
            primary_ref_frame: slot,
            ref_frame_idx: [slot; 7],
            base_q_idx: 40 + n,
            tx_mode_select: n % 2 == 0,
        };
        let mut tu = if key { enc::sequence_header(&s, 4) } else { Vec::new() };
        tu.extend_from_slice(&enc::frame_header(&s, &f, &t, 4).bytes);
        let tile_at = tu.len() + 5;
        tu.extend_from_slice(&enc::tile_group(&filler, 4));
        let got = frames(&mut stream, &tu);
        assert_eq!(got.len(), 1, "frame {n}");
        let (frame, groups, tiles) = &got[0];
        let h = &frame.header;
        assert_eq!(h.frame_type, if key { FrameType::Key } else { FrameType::Inter }, "frame {n}");
        assert_eq!((h.frame_width, h.frame_height, h.upscaled_width), (320, 240, 320));
        assert_eq!(
            (h.order_hint, h.refresh_frame_flags, h.primary_ref_frame),
            (n, f.refresh_frame_flags, f.primary())
        );
        assert_eq!(h.error_resilient_mode, key);
        assert_eq!(h.quant.base_q_idx, f.base_q_idx);
        assert_eq!(h.tx_mode, if f.tx_mode_select { TX_MODE_SELECT } else { TX_MODE_LARGEST });
        assert_eq!(
            (h.cdef.damping_minus_3, h.cdef.bits, h.cdef.y_pri[0]),
            (0, 0, 0),
            "CDEF of zero strengths"
        );
        assert!(!h.seg.enabled && !h.delta_q_present && !h.reference_select && !h.reduced_tx_set);
        assert_eq!((h.tile_info.cols, h.tile_info.rows), (1, 1));
        if !key {
            assert_eq!(h.ref_frame_idx, [slot; 7]);
            assert_eq!(h.interpolation_filter, 0);
            assert!(!h.allow_high_precision_mv && !h.is_motion_mode_switchable);
            assert_eq!(h.gm.gm_type, [0; 8]);
            assert_eq!(h.order_hints[1], n - 1, "the reference's order hint");
        }
        assert_eq!(groups.len(), 1);
        assert_eq!((groups[0].data_start, groups[0].data_end), (tile_at, tile_at + 100));
        assert_eq!(tiles, &[obu::Tile { offset: 0, size: 100, row: 0, col: 0 }]);
        let refs =
            ref_surfaces(&[Some(Slot { surface: 3, display: 3, width: 320, height: 240, showable: true }); 8]);
        let p = params::pic_param(stream.seq.as_ref().unwrap(), h, refs, 9, 9);
        assert_eq!((p.frame_width_minus1, p.frame_height_minus1), (319, 239));
        assert_eq!(p.ref_frame_map, if key { [ffi::VA_INVALID_SURFACE; 8] } else { [3; 8] });
        assert_eq!((p.current_frame, p.current_display_picture, p.order_hint), (9, 9, n));
        assert_eq!((p.order_hint_bits_minus_1, p.bit_depth_idx, p.matrix_coefficients), (7, 0, 1));
        assert_eq!(p.superres_scale_denominator, 8);
        assert_eq!(
            (p.tile_cols, p.tile_rows, p.width_in_sbs_minus_1[0], p.height_in_sbs_minus_1[0]),
            (1, 1, 4, 3)
        );
        assert_eq!(p.pic_info_fields & 3, h.frame_type as u32);
        assert_ne!(p.pic_info_fields & ffi::AV1_PIC_SHOW_FRAME, 0);
        assert_eq!(p.mode_control_fields >> ffi::AV1_MODE_TX_MODE_SHIFT & 3, u32::from(h.tx_mode));
        assert_ne!(p.seq_info_fields & ffi::AV1_SEQ_ENABLE_ORDER_HINT, 0);
        assert_ne!(p.seq_info_fields & ffi::AV1_SEQ_ENABLE_CDEF, 0);
        let tp = params::tile_param(&tiles[0], &groups[0]);
        assert_eq!(
            (tp.slice_data_size, tp.slice_data_offset, tp.tg_start, tp.tg_end),
            (100, 0, 0, 0)
        );
        slot = next;
    }
}

#[test]
fn the_first_units_of_the_tools_fixture() {
    let units = ivf(TOOLS);
    let mut stream = Stream::new();
    let first = frames(&mut stream, units[0]);
    let s = stream.seq.unwrap();
    assert_eq!(
        (s.seq_profile, s.max_frame_width, s.max_frame_height, s.color.bit_depth),
        (0, 352, 288, 8)
    );
    assert!(s.enable_order_hint && supported(&s));
    assert_eq!(first.len(), 1);
    let (f, groups, tiles) = &first[0];
    let h = &f.header;
    assert!(h.frame_type == FrameType::Key && h.show_frame && h.refresh_frame_flags == ALL_FRAMES);
    assert_eq!((h.tile_info.cols, h.tile_info.rows), (2, 2));
    assert_eq!(groups.len(), 2, "num-tile-groups=2");
    assert_eq!(
        (groups[0].tg_start, groups[0].tg_end, groups[1].tg_start, groups[1].tg_end),
        (0, 1, 2, 3)
    );
    assert_eq!(
        tiles.iter().map(|t| (t.row, t.col)).collect::<Vec<_>>(),
        [(0, 0), (0, 1), (1, 0), (1, 1)]
    );
    assert!(h.seg.enabled, "aq-mode 1");
    assert!(groups[0].data_end <= groups[1].data_start, "the groups in order");
    let p = params::pic_param(&s, h, [ffi::VA_INVALID_SURFACE; 8], 0, 0);
    assert_eq!((p.tile_cols, p.tile_rows), (2, 2));
    // 352 wide is 6 superblocks of 64, uniform halves.
    assert_eq!((p.width_in_sbs_minus_1[0], p.width_in_sbs_minus_1[1]), (2, 2));
    assert_eq!(p.seg_info.segment_info_fields & ffi::AV1_SEG_ENABLED, ffi::AV1_SEG_ENABLED);
    let mut hidden = 0;
    let mut shown = 0;
    for tu in &units[1..4] {
        for (f, _, _) in frames(&mut stream, tu) {
            if f.header.show_frame || f.header.show_existing_frame {
                shown += 1;
            } else {
                hidden += 1;
            }
        }
    }
    assert_eq!(shown, 3, "one shown frame a unit");
    assert!(hidden > 0, "a hidden alt-ref early on");
}

#[test]
fn the_10_bit_fixture_is_profile_0_main_10() {
    let units = ivf(P10);
    let mut stream = Stream::new();
    let got = frames(&mut stream, units[0]);
    let s = stream.seq.unwrap();
    assert_eq!(
        (s.seq_profile, s.color.bit_depth, s.max_frame_width, s.max_frame_height),
        (0, 10, 160, 96)
    );
    assert!(supported(&s));
    let p = params::pic_param(&s, &got[0].0.header, [ffi::VA_INVALID_SURFACE; 8], 0, 0);
    assert_eq!(p.bit_depth_idx, 1);
}

#[test]
fn the_grain_fixture_carries_its_film_grain() {
    let units = ivf(GRAIN);
    let mut stream = Stream::new();
    let got = frames(&mut stream, units[0]);
    let s = stream.seq.unwrap();
    assert!(s.film_grain_params_present);
    let h = &got[0].0.header;
    let g = &h.film_grain;
    assert!(g.apply_grain && g.update_grain, "a key frame codes its grain");
    let p = params::pic_param(&s, h, [ffi::VA_INVALID_SURFACE; 8], 4, 5);
    assert_eq!((p.current_frame, p.current_display_picture), (4, 5));
    let f = &p.film_grain_info;
    assert_ne!(f.film_grain_info_fields & ffi::AV1_FG_APPLY_GRAIN, 0);
    assert_eq!((f.grain_seed, f.num_y_points), (g.grain_seed, g.num_y_points));
    assert_eq!(f.point_y_value, g.point_y_value);
    assert_eq!(f.ar_coeffs_y[0], (i16::from(g.ar_coeffs_y_plus_128[0]) - 128) as i8);
    assert_eq!(
        f.film_grain_info_fields >> ffi::AV1_FG_AR_COEFF_LAG_SHIFT & 3,
        u32::from(g.ar_coeff_lag)
    );
    assert_ne!(p.seq_info_fields & ffi::AV1_SEQ_FILM_GRAIN_PARAMS_PRESENT, 0);
    let mut applied = 0;
    for tu in &units[1..] {
        for (f, _, _) in frames(&mut stream, tu) {
            applied += usize::from(f.header.film_grain.apply_grain);
        }
    }
    assert!(applied > 0);
}

// Test setup reads the fixture.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
#[test]
fn the_first_units_of_the_mkv_fixture() {
    let Ok(data) = std::fs::read(MKV) else {
        eprintln!("skip: {MKV} absent");
        return;
    };
    let mut reader = pf_mkv::MatroskaReader::new();
    reader.push(&data).unwrap();
    let track = reader.tracks().iter().find(|t| t.codec_id == "V_AV1").unwrap().clone();
    let mut units = Vec::new();
    while let Some(frame) = reader.next_frame() {
        if frame.track_number == track.track_number && units.len() < 8 {
            let mut tu =
                if units.is_empty() { track.codec_private[4..].to_vec() } else { Vec::new() };
            tu.extend_from_slice(&frame.data);
            units.push(tu);
        }
    }
    let mut stream = Stream::new();
    let first = frames(&mut stream, &units[0]);
    let s = stream.seq.unwrap();
    assert_eq!((s.max_frame_width, s.max_frame_height, s.color.bit_depth), (320, 180, 8));
    assert!(!s.film_grain_params_present);
    let h = &first[0].0.header;
    assert!(h.frame_type == FrameType::Key && h.show_frame);
    assert_eq!((h.upscaled_width, h.frame_height), (320, 180));
    for (n, tu) in units.iter().enumerate().skip(1) {
        let got = frames(&mut stream, tu);
        let shown = got
            .iter()
            .filter(|(f, _, _)| f.header.show_frame || f.header.show_existing_frame)
            .count();
        assert_eq!(shown, 1, "unit {n} shows one frame");
    }
}

#[test]
fn a_key_frame_shown_again_reloads_every_slot() {
    let s = super::seq::parse_sequence_header(&enc::sequence_header(&seq(64, 64, false), 4)[5..])
        .unwrap();
    let mut slots = [RefFrame::EMPTY; 8];
    slots[3] = RefFrame {
        valid: true,
        frame_type: FrameType::Key,
        order_hint: 9,
        upscaled_width: 64,
        ..RefFrame::EMPTY
    };
    slots[5] =
        RefFrame { valid: true, frame_type: FrameType::Inter, order_hint: 4, ..RefFrame::EMPTY };
    let mut shown_again = header::parse_frame_header(&[0b1011_0000], &s, &slots, 0, 0).unwrap();
    assert!(shown_again.show_existing_frame);
    assert_eq!(
        (
            shown_again.frame_to_show_map_idx,
            shown_again.frame_type,
            shown_again.refresh_frame_flags
        ),
        (3, FrameType::Key, ALL_FRAMES)
    );
    refs::apply(&mut slots, &shown_again, &s);
    assert!(slots.iter().all(|r| r.valid && r.order_hint == 9 && r.frame_type == FrameType::Key));
    // An inter frame shown again refreshes nothing.
    slots[5] =
        RefFrame { valid: true, frame_type: FrameType::Inter, order_hint: 4, ..RefFrame::EMPTY };
    shown_again = header::parse_frame_header(&[0b1101_0000], &s, &slots, 0, 0).unwrap();
    assert_eq!((shown_again.frame_type, shown_again.refresh_frame_flags), (FrameType::Inter, 0));
    let before = slots;
    refs::apply(&mut slots, &shown_again, &s);
    assert_eq!(slots, before);
    assert_eq!(
        header::parse_frame_header(&[0b1001_0000], &s, &[RefFrame::EMPTY; 8], 0, 0),
        Err(Av1Error::MissingRef(1))
    );
    let mut h = shown_again;
    h.show_existing_frame = false;
    h.refresh_frame_flags = 0;
    h.ref_order_hint = Some([9, 9, 9, 7, 9, 9, 9, 9]);
    refs::apply(&mut slots, &h, &s);
    assert_eq!((slots[3].order_hint, slots[3].valid), (7, false));
    assert_eq!((slots[2].order_hint, slots[2].valid), (9, true));
}

#[test]
fn the_slots_take_the_surfaces_the_header_names() {
    let mut h = header::parse_frame_header(
        &[0b1011_0000],
        &super::seq::parse_sequence_header(&enc::sequence_header(&seq(64, 64, false), 4)[5..])
            .unwrap(),
        &[RefFrame { valid: true, ..RefFrame::EMPTY }; 8],
        0,
        0,
    )
    .unwrap();
    let mut slots = [None; 8];
    let a = Slot { surface: 1, display: 2, width: 64, height: 64, showable: true };
    h.show_existing_frame = false;
    h.refresh_frame_flags = 0b0000_1001;
    assert_eq!(refresh_slots(&mut slots, &h, Some(a)), [None; 8]);
    assert_eq!((slots[0], slots[3], slots[1]), (Some(a), Some(a), None));
    assert_eq!(ref_surfaces(&slots)[0], 1, "the reference is the ungrained surface");
    h.show_existing_frame = true;
    h.frame_to_show_map_idx = 3;
    h.refresh_frame_flags = ALL_FRAMES;
    let old = refresh_slots(&mut slots, &h, None);
    assert_eq!((old[0], old[1]), (Some(a), None));
    assert!(slots.iter().all(|s| *s == Some(a)));
}

#[test]
fn global_motion_shear_follows_setup_shear() {
    let id = DEFAULT_GM_PARAMS[1];
    assert!(shear_valid(&id));
    assert!(!shear_valid(&[0, 0, 0, 0, 0, 1 << 16]));
    assert!(!shear_valid(&[0, 0, 1 << 16, 1 << 15, -(1 << 15), 1 << 16]), "a half turn of shear");
    assert!(
        shear_valid(&[100, -50, (1 << 16) + 300, 200, -200, (1 << 16) + 300]),
        "a slight rotation and zoom"
    );
}

#[test]
fn the_stated_colour_and_the_subset() {
    let s = super::seq::parse_sequence_header(
        &enc::sequence_header(
            &Sequence { color: Some((9, 16, 9)), full_range: true, ..seq(64, 64, true) },
            4,
        )[5..],
    )
    .unwrap();
    let c = stated_color(&s.color);
    assert_eq!(
        (c.matrix, c.transfer, c.primaries, c.range),
        (Some(Matrix::Bt2020), Some(TransferFn::Pq), Some(Primaries::Bt2020), Some(Range::Full))
    );
    assert!(supported(&s));
    let mono = SequenceHeader { color: ColorConfig { mono_chrome: true, ..s.color }, ..s };
    assert!(!supported(&mono));
    assert!(!supported(&SequenceHeader { seq_profile: 1, ..s }));
    assert!(!supported(&SequenceHeader { color: ColorConfig { bit_depth: 12, ..s.color }, ..s }));
    assert_eq!(PRIMARY_REF_NONE, 7);
}

#[test]
fn a_new_sequence_header_between_a_frame_header_and_its_tiles_fails_the_frame() {
    let key = EncFrame {
        key: true,
        order_hint: 0,
        refresh_frame_flags: 0xFF,
        primary_ref_frame: 0,
        ref_frame_idx: [0; 7],
        base_q_idx: 40,
        tx_mode_select: false,
    };
    let (a, b) = (seq(320, 240, false), seq(640, 480, false));
    let t = Tiles::single(320, 240).unwrap();
    let unit = |between: &Sequence| {
        let mut tu = enc::sequence_header(&a, 4);
        tu.extend_from_slice(&enc::frame_header(&a, &key, &t, 4).bytes);
        tu.extend_from_slice(&enc::sequence_header(between, 4));
        tu.extend_from_slice(&enc::tile_group(&[0; 100], 4));
        tu
    };
    let same = unit(&a);
    assert_eq!(frames(&mut Stream::new(), &same).len(), 1, "a repeat of the same one is fine");
    let other = unit(&b);
    let mut at = 0;
    assert!(matches!(
        Stream::new().next_frame(&other, &mut at),
        Err(Av1Error::Invalid("sequence header inside a frame"))
    ));
}

#[test]
fn a_frame_size_override_past_the_sequence_maximum_is_rejected() {
    // 64x64 with 7-bit size fields, so an override can name up to 128.
    let s = super::seq::parse_sequence_header(&enc::sequence_header(&seq(64, 64, false), 4)[5..])
        .unwrap();
    assert_eq!((s.frame_width_bits, s.frame_height_bits), (7, 7));
    let key_with_size = |w: u64, h: u64| {
        let mut b = enc::Bits::default();
        b.flag(false); // show_existing_frame
        b.put(0, 2); // frame_type KEY
        b.flag(true); // show_frame
        b.flag(false); // disable_cdf_update
        b.flag(true); // frame_size_override_flag
        b.put(0, 8); // order_hint
        b.put(w - 1, 7); // frame_width_minus_1
        b.put(h - 1, 7); // frame_height_minus_1
        b.put(0, 64); // render_and_frame_size_different and what follows
        b.into_bytes()
    };
    let refs = [RefFrame::EMPTY; 8];
    for (w, h) in [(101, 2), (2, 65), (128, 128)] {
        assert_eq!(
            header::parse_frame_header(&key_with_size(w, h), &s, &refs, 0, 0),
            Err(Av1Error::Invalid("frame_size")),
            "{w}x{h}"
        );
    }
    let h = header::parse_frame_header(&key_with_size(64, 64), &s, &refs, 0, 0).unwrap();
    assert_eq!((h.frame_width, h.frame_height, h.upscaled_width), (64, 64, 64));
}
