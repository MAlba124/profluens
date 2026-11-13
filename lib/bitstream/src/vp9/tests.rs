// Test fixtures.
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::Event::{Output, Release};

#[derive(Default)]
struct Bits(Vec<bool>);

impl Bits {
    fn u(&mut self, v: u32, n: u32) {
        (0..n).rev().for_each(|i| self.0.push(v >> i & 1 == 1));
    }

    fn flag(&mut self, b: bool) {
        self.0.push(b);
    }

    fn signed(&mut self, v: i32, n: u32) {
        self.u(v.unsigned_abs(), n);
        self.flag(v < 0);
    }

    fn bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; self.0.len().div_ceil(8)];
        for (i, &x) in self.0.iter().enumerate() {
            b[i / 8] |= (x as u8) << (7 - i % 8);
        }
        b
    }
}

type LfDeltas = ([Option<i32>; 4], [Option<i32>; 2]);
/// (segment, feature, value)
type Feature = (usize, usize, i32);

struct Seg {
    tree: [u8; 7],
    pred: Option<[u8; 3]>,
    data: Option<(bool, &'static [Feature])>,
}

/// Loop filter, quantisation, segmentation, tiles and the compressed header size.
#[derive(Default)]
struct Tail {
    lf_level: u32,
    sharpness: u32,
    lf_delta_enabled: bool,
    lf_deltas: Option<LfDeltas>,
    base_q: u32,
    dq: [i32; 3],
    segmentation: Option<Seg>,
    header_size: u32,
}

fn write_tail(w: &mut Bits, t: &Tail) {
    w.u(t.lf_level, 6);
    w.u(t.sharpness, 3);
    w.flag(t.lf_delta_enabled);
    if t.lf_delta_enabled {
        w.flag(t.lf_deltas.is_some());
    }
    if let Some((refs, modes)) = t.lf_deltas.filter(|_| t.lf_delta_enabled) {
        for d in refs.into_iter().chain(modes) {
            w.flag(d.is_some());
            if let Some(d) = d {
                w.signed(d, 6);
            }
        }
    }
    w.u(t.base_q, 8);
    for d in t.dq {
        w.flag(d != 0);
        if d != 0 {
            w.signed(d, 4);
        }
    }
    let prob = |w: &mut Bits, p: u8| {
        w.flag(p != 255);
        if p != 255 {
            w.u(u32::from(p), 8);
        }
    };
    w.flag(t.segmentation.is_some());
    if let Some(seg) = &t.segmentation {
        w.flag(true);
        seg.tree.into_iter().for_each(|p| prob(w, p));
        w.flag(seg.pred.is_some());
        if let Some(pred) = seg.pred {
            pred.into_iter().for_each(|p| prob(w, p));
        }
        w.flag(seg.data.is_some());
        if let Some((abs, features)) = seg.data {
            w.flag(abs);
            const BITS: [u32; 4] = [8, 6, 2, 0];
            const SIGNED: [bool; 4] = [true, true, false, false];
            for i in 0..MAX_SEGMENTS {
                for j in 0..SEG_LVL_MAX {
                    let v = features.iter().find(|f| f.0 == i && f.1 == j).map(|f| f.2);
                    w.flag(v.is_some());
                    if let Some(v) = v {
                        w.u(v.unsigned_abs(), BITS[j]);
                        if SIGNED[j] {
                            w.flag(v < 0);
                        }
                    }
                }
            }
        }
    }
    // one tile column, no tile rows
    w.flag(false);
    w.u(t.header_size, 16);
    while !w.0.len().is_multiple_of(8) {
        w.flag(false);
    }
}

fn key_frame(profile: u32, width: u32, height: u32, t: &Tail) -> Vec<u8> {
    let mut w = Bits::default();
    w.u(2, 2);
    w.u(profile & 1, 1);
    w.u(profile >> 1, 1);
    // not existing, key, shown, not error resilient, sync code
    w.flag(false);
    w.flag(false);
    w.flag(true);
    w.flag(false);
    w.u(0x49_83_42, 24);
    if profile >= 2 {
        // 10 bit
        w.flag(false);
    }
    // BT.709 studio range
    w.u(2, 3);
    w.flag(false);
    w.u(width - 1, 16);
    w.u(height - 1, 16);
    w.flag(false);
    // refresh context, not parallel, context 2
    w.flag(true);
    w.flag(false);
    w.u(2, 2);
    write_tail(&mut w, t);
    w.bytes()
}

/// Refreshes slot 2 from LAST 0, GOLDEN 1 and ALTREF 2, GOLDEN's size.
fn inter_frame(error_resilient: bool, t: &Tail) -> Vec<u8> {
    let mut w = Bits::default();
    w.u(2, 2);
    w.u(0, 2);
    // not existing, inter, shown so no intra_only bit
    w.flag(false);
    w.flag(true);
    w.flag(true);
    w.flag(error_resilient);
    if !error_resilient {
        // reset_frame_context
        w.u(1, 2);
    }
    w.u(0x04, 8);
    for (idx, bias) in [(0, false), (1, false), (2, true)] {
        w.u(idx, 3);
        w.flag(bias);
    }
    // found_ref LAST no, GOLDEN yes, same render size, high precision mv, EIGHTTAP_SHARP
    w.flag(false);
    w.flag(true);
    w.flag(false);
    w.flag(true);
    w.flag(false);
    w.u(2, 2);
    if !error_resilient {
        w.flag(false);
        w.flag(true);
    }
    w.u(3, 2);
    write_tail(&mut w, t);
    w.bytes()
}

/// A hidden 64x48 intra-only frame refreshing slots 0 and 7.
fn intra_only_frame(t: &Tail) -> Vec<u8> {
    let mut w = Bits::default();
    w.u(2, 2);
    w.u(0, 2);
    w.flag(false);
    w.flag(true);
    w.flag(false);
    w.flag(false);
    w.flag(true);
    w.u(3, 2);
    w.u(0x49_83_42, 24);
    w.u(0x81, 8);
    w.u(63, 16);
    w.u(47, 16);
    w.flag(false);
    w.flag(true);
    w.flag(false);
    w.u(1, 2);
    write_tail(&mut w, t);
    w.bytes()
}

fn show_existing(idx: u8) -> Vec<u8> {
    vec![0b1000_1000 | idx]
}

/// One frame through `parse`, `begin` and `end` in `slot`, the fate and events of both.
fn frame(d: &mut Dpb, bytes: &[u8], slot: u16) -> (Fate, Vec<Event>) {
    let h = d.parse(bytes).unwrap();
    run(d, &h, slot)
}

fn run(d: &mut Dpb, h: &Vp9FrameHeader, slot: u16) -> (Fate, Vec<Event>) {
    let mut ev = Events::new();
    let fate = d.begin(h, &mut ev).unwrap();
    let mut all = ev.as_slice().to_vec();
    if fate != Fate::Skip {
        d.end(h, fate, Slot(slot), &mut ev);
        all.extend_from_slice(ev.as_slice());
    }
    (fate, all)
}

fn held(d: &Dpb) -> Vec<Option<u16>> {
    d.slots().iter().map(|s| s.map(|r| r.slot.0)).collect()
}

/// A 352x288 key frame with loop filter deltas and five segment features, decoded into slot 1.
fn feature_key_frame() -> (Vp9FrameHeader, Dpb) {
    let t = Tail {
        lf_level: 36,
        sharpness: 2,
        lf_delta_enabled: true,
        lf_deltas: Some(([Some(1), None, Some(-2), None], [None, Some(3)])),
        base_q: 60,
        dq: [-3, 0, 2],
        segmentation: Some(Seg {
            tree: [10, 20, 255, 40, 50, 60, 70],
            pred: None,
            data: Some((false, &[(1, 0, -10), (2, 1, 5), (3, 2, 2), (4, 3, 0), (5, 0, -100)])),
        }),
        header_size: 777,
    };
    let mut d = Dpb::new();
    let h = d.parse(&key_frame(0, 352, 288, &t)).unwrap();
    assert_eq!(run(&mut d, &h, 1), (Fate::Shown, vec![Output(Slot(1))]));
    (h, d)
}

#[test]
fn a_key_frame_header_parses_to_its_fields() {
    let (h, d) = feature_key_frame();
    assert_eq!(h.frame_type, FrameType::KeyFrame);
    assert_eq!((h.frame_width, h.frame_height), (352, 288));
    assert_eq!(h.color_config.color_space, ColorSpace::Bt709);
    assert_eq!(h.refresh_frame_flags, 0xff);
    assert_eq!(h.frame_context_idx, 0);
    assert_eq!((h.loop_filter.level, h.loop_filter.sharpness), (36, 2));
    assert_eq!((h.quantization.base_q_idx, h.header_size_in_bytes), (60, 777));
    assert_eq!(h.segmentation.tree_probs, Some([10, 20, 255, 40, 50, 60, 70]));
    assert_eq!(d.state().tree_probs, [10, 20, 255, 40, 50, 60, 70]);
    assert_eq!(held(&d), [Some(1); NUM_SLOTS]);
}

#[test]
fn loop_filter_levels_follow_the_deltas_and_the_segment_feature() {
    let (h, d) = feature_key_frame();
    let st = d.state();
    assert_eq!(st.lf_ref_deltas, [1, 0, -2, -1]);
    assert_eq!(st.lf_mode_deltas, [0, 3]);
    assert_eq!(st.filter_levels(&h, 0), [[38, 38], [36, 42], [32, 38], [34, 40]]);
    assert_eq!(st.filter_levels(&h, 2), [[43, 43], [41, 47], [37, 43], [39, 45]]);
}

#[test]
fn dequantiser_scales_follow_the_segment_qindex() {
    let (h, d) = feature_key_frame();
    let st = d.state();
    for (i, q) in [(0, 60), (1, 50), (5, 0)] {
        assert_eq!(st.qindex(&h, i), q);
        let s = st.segment(&h, i);
        assert_eq!(s.luma_ac_quant_scale, ac_q(8, q) as u16);
        assert_eq!(s.luma_dc_quant_scale, dc_q(8, q - 3) as u16);
        assert_eq!(s.chroma_dc_quant_scale, dc_q(8, q) as u16);
        assert_eq!(s.chroma_ac_quant_scale, ac_q(8, q + 2) as u16);
    }
    let s0 = st.segment(&h, 0);
    assert_eq!((s0.luma_dc_quant_scale, s0.luma_ac_quant_scale), (55, 67));
    assert_eq!((s0.reference, s0.skipped), (None, false));
    assert_eq!(st.segment(&h, 3).reference, Some(2));
    assert!(st.segment(&h, 4).skipped);
}

#[test]
fn an_inter_frame_parses_against_the_slots_and_the_held_state() {
    let (_, mut d) = feature_key_frame();
    d.slots[1] = Some(Ref { slot: Slot(2), width: 320, height: 240 });
    let t = Tail {
        lf_level: 10,
        lf_delta_enabled: true,
        base_q: 100,
        header_size: 5,
        ..Tail::default()
    };
    let h = d.parse(&inter_frame(false, &t)).unwrap();
    // GOLDEN's size, the key frame's colour
    assert_eq!((h.frame_width, h.frame_height), (320, 240));
    assert_eq!(h.ref_frame_idx, Some([0, 1, 2]));
    assert_eq!(h.ref_frame_sign_bias, [false, false, true]);
    assert_eq!(h.interpolation_filter, 2);
    assert_eq!((h.frame_context_idx, h.reset_frame_context, h.refresh_frame_flags), (3, 1, 0x04));
    assert_eq!(h.color_config.color_space, ColorSpace::Bt709);
    let refs = d.refs(&h).unwrap().map(|r| (r.slot.0, r.width));
    assert_eq!(refs, [(1, 352), (2, 320), (1, 352)]);
    // slot 2 held the key frame, slots 0 and 3 to 7 still do
    assert_eq!(run(&mut d, &h, 3), (Fate::Shown, vec![Output(Slot(3))]));
    let st = d.state();
    assert_eq!(st.filter_levels(&h, 0), [[11, 11], [10, 13], [8, 11], [9, 12]]);
    // segmentation off, the features are held for a later frame
    assert_eq!(st.qindex(&h, 1), 100);
    assert_eq!(st.feature_data[1][SEG_LVL_ALT_Q], -10);
}

#[test]
fn error_resilience_resets_the_held_state() {
    let (_, mut d) = feature_key_frame();
    let t = Tail {
        lf_level: 40,
        lf_delta_enabled: true,
        lf_deltas: Some(([None; 4], [None; 2])),
        ..Tail::default()
    };
    let h = d.parse(&inter_frame(true, &t)).unwrap();
    assert!(h.error_resilient_mode);
    assert_eq!((h.frame_context_idx, h.reset_frame_context), (0, 0));
    run(&mut d, &h, 2);
    let st = d.state();
    assert_eq!(st.lf_ref_deltas, [1, 0, -1, -1]);
    assert_eq!(st.feature_enabled, [[false; SEG_LVL_MAX]; MAX_SEGMENTS]);
    assert!(!st.abs_or_delta_update);
}

#[test]
fn an_intra_only_frame_takes_profile_0_defaults() {
    let (_, mut d) = feature_key_frame();
    let h = d.parse(&intra_only_frame(&Tail::default())).unwrap();
    assert!(h.intra_only && !h.show_frame);
    assert_eq!((h.frame_width, h.frame_height, h.refresh_frame_flags), (64, 48, 0x81));
    assert_eq!((h.color_config.color_space, h.color_config.bit_depth), (ColorSpace::Bt601, 8));
    assert_eq!(h.ref_frame_idx, None);
    // hidden, slots 0 and 7 take it
    assert_eq!(run(&mut d, &h, 2), (Fate::Hidden, vec![]));
    assert_eq!(d.state().color.color_space, ColorSpace::Bt601);
    assert_eq!(d.state().lf_ref_deltas, [1, 0, -1, -1]);
    let sizes: Vec<_> = d.slots().iter().flatten().map(|r| r.width).collect();
    assert_eq!(sizes, [64, 352, 352, 352, 352, 352, 352, 64]);
}

#[test]
fn a_profile_2_key_frame_is_10_bit() {
    let t = Tail { base_q: 60, ..Tail::default() };
    let mut d = Dpb::new();
    let h = d.parse(&key_frame(2, 64, 64, &t)).unwrap();
    assert_eq!((h.profile, h.color_config.bit_depth), (2, 10));
    run(&mut d, &h, 0);
    let s = d.state().segment(&h, 0);
    assert_eq!(s.luma_ac_quant_scale, ac_q(10, 60) as u16);
    assert_ne!(ac_q(10, 60), ac_q(8, 60));
}

#[test]
fn frames_before_the_first_key_frame_are_skipped() {
    let (_, key) = feature_key_frame();
    let mut d = Dpb::new();
    let inter = key.parse(&inter_frame(false, &Tail::default())).unwrap();
    assert_eq!(run(&mut d, &inter, 0), (Fate::Skip, vec![]));
    let shown = d.parse(&show_existing(5)).unwrap();
    assert_eq!(run(&mut d, &shown, 0), (Fate::Skip, vec![]));
    let (fate, ev) = frame(&mut d, &key_frame(0, 64, 64, &Tail::default()), 1);
    assert_eq!((fate, ev), (Fate::Shown, vec![Output(Slot(1))]));
    assert_eq!(run(&mut d, &inter, 2), (Fate::Shown, vec![Output(Slot(2))]));
}

#[test]
fn refresh_replaces_named_slots_and_releases_what_no_slot_holds() {
    let (_, mut d) = feature_key_frame();
    let mut h = d.parse(&inter_frame(false, &Tail::default())).unwrap();
    h.refresh_frame_flags = 0b1000_0001;
    assert_eq!(run(&mut d, &h, 2), (Fate::Shown, vec![Output(Slot(2))]));
    assert_eq!(held(&d), [Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(2)]);
    // the key frame leaves its last six slots, released once
    h.refresh_frame_flags = 0b0111_1110;
    h.ref_frame_idx = Some([0, 7, 0]);
    let (_, ev) = run(&mut d, &h, 3);
    assert_eq!(ev, [Output(Slot(3)), Release(Slot(1))]);
    // a hidden frame no slot takes is released at once, a shown one after its output
    h.refresh_frame_flags = 0;
    h.show_frame = false;
    assert_eq!(run(&mut d, &h, 4), (Fate::Hidden, vec![Release(Slot(4))]));
    h.show_frame = true;
    let (_, ev) = run(&mut d, &h, 5);
    assert_eq!(ev, [Output(Slot(5)), Release(Slot(5))]);
    let mut ev = Events::new();
    d.discard(&mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(2)), Release(Slot(3))]);
    assert_eq!(held(&d), [None; NUM_SLOTS]);
}

#[test]
fn show_existing_frame_puts_its_slot_out_again() {
    let (_, mut d) = feature_key_frame();
    let h = d.parse(&show_existing(5)).unwrap();
    assert!(h.show_existing_frame);
    assert_eq!(h.frame_to_show_map_idx, Some(5));
    assert_eq!(run(&mut d, &h, 9), (Fate::Skip, vec![Output(Slot(1))]));
    assert_eq!(held(&d), [Some(1); NUM_SLOTS]);
}

#[test]
fn an_empty_slot_is_a_missing_reference_and_changes_nothing() {
    let (_, mut d) = feature_key_frame();
    let inter = d.parse(&inter_frame(false, &Tail::default())).unwrap();
    let shown = d.parse(&show_existing(3)).unwrap();
    d.slots[2] = None;
    d.slots[3] = None;
    let before = *d.state();
    let mut ev = Events::new();
    assert_eq!(d.begin(&inter, &mut ev), Err(DpbError::MissingReference));
    assert_eq!(d.begin(&shown, &mut ev), Err(DpbError::MissingReference));
    assert!(ev.as_slice().is_empty());
    assert_eq!(*d.state(), before);
}

#[test]
fn segment_features_and_filter_deltas() {
    let header = |level, base_q| {
        let t = Tail { lf_level: level, base_q, ..Tail::default() };
        parse::parse_uncompressed_header(&key_frame(0, 64, 64, &t)).unwrap()
    };
    let mut st = StreamState::default();
    let mut hd = header(40, 100);
    hd.loop_filter.delta_enabled = true;
    hd.segmentation.enabled = true;
    hd.segmentation.update_data = true;
    let seg = &mut hd.segmentation;
    seg.feature_enabled[1][SEG_LVL_ALT_Q] = true;
    seg.feature_data[1][SEG_LVL_ALT_Q] = -30;
    seg.feature_enabled[2][SEG_LVL_ALT_L] = true;
    seg.feature_data[2][SEG_LVL_ALT_L] = 30;
    seg.feature_enabled[3][SEG_LVL_REF_FRAME] = true;
    seg.feature_data[3][SEG_LVL_REF_FRAME] = 2;
    seg.feature_enabled[4][SEG_LVL_SKIP] = true;
    st.fold(&hd);
    assert_eq!((st.qindex(&hd, 0), st.qindex(&hd, 1)), (100, 70));
    // level 40 shifts deltas by one, defaults ref [1, 0, -1, -1] mode [0, 0]
    assert_eq!(st.filter_levels(&hd, 0), [[42, 42], [40, 40], [38, 38], [38, 38]]);
    // 40 + 30 clamps to 63 before the deltas
    assert_eq!(st.filter_levels(&hd, 2), [[63, 63], [63, 63], [61, 61], [61, 61]]);
    let s3 = st.segment(&hd, 3);
    assert!(s3.reference == Some(2) && !s3.skipped);
    assert!(st.segment(&hd, 4).skipped);
    assert_eq!(st.segment(&hd, 1).luma_ac_quant_scale, ac_q(8, 70) as u16);
    // the features persist into a frame that does not update them
    let mut next = hd;
    next.frame_type = FrameType::NonKeyFrame;
    next.segmentation.update_data = false;
    next.segmentation.feature_enabled = [[false; SEG_LVL_MAX]; MAX_SEGMENTS];
    st.fold(&next);
    assert_eq!(st.qindex(&next, 1), 70);
    // and stop applying once segmentation is off
    next.segmentation.enabled = false;
    assert_eq!(st.qindex(&next, 1), 100);
    // absolute values, an unfiltered frame stays unfiltered
    let mut abs = hd;
    abs.segmentation.abs_or_delta_update = true;
    st.fold(&abs);
    assert_eq!(st.qindex(&abs, 1), 0);
    assert_eq!(st.filter_levels(&header(0, 10), 2), [[0; 2]; 4]);
}

#[test]
fn key_frame_resets_deltas_and_features() {
    let key = parse::parse_uncompressed_header(&key_frame(0, 64, 64, &Tail::default())).unwrap();
    let mut st = StreamState::default();
    let mut hd = key;
    hd.frame_type = FrameType::NonKeyFrame;
    hd.loop_filter.ref_deltas = [Some(5), None, None, Some(3)];
    hd.segmentation.enabled = true;
    hd.segmentation.update_data = true;
    hd.segmentation.feature_enabled[0][SEG_LVL_ALT_Q] = true;
    hd.segmentation.feature_data[0][SEG_LVL_ALT_Q] = 5;
    st.fold(&hd);
    assert_eq!(st.lf_ref_deltas, [5, 0, -1, 3]);
    st.fold(&key);
    assert_eq!(st.lf_ref_deltas, [1, 0, -1, -1]);
    assert_eq!(st.feature_enabled, [[false; SEG_LVL_MAX]; MAX_SEGMENTS]);
}

#[test]
fn after_a_failed_frame_frames_parse_and_skip_until_a_key() {
    let (_, mut d) = feature_key_frame();
    assert!(!d.awaits_key());
    d.await_key();
    assert!(d.awaits_key());
    let inter = d.parse(&inter_frame(false, &Tail::default())).unwrap();
    assert_eq!((inter.frame_width, inter.frame_height), (352, 288));
    assert_eq!(run(&mut d, &inter, 2), (Fate::Skip, vec![]));
    assert_eq!(held(&d), [Some(1); NUM_SLOTS]);
    // a growing key frame empties the slots first, the state stays
    let key = d.parse(&key_frame(0, 64, 64, &Tail::default())).unwrap();
    let mut ev = Events::new();
    assert_eq!(d.begin(&key, &mut ev), Ok(Fate::Shown));
    let st = *d.state();
    d.drop_slots(&mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(1))]);
    assert_eq!((held(&d), *d.state()), (vec![None; NUM_SLOTS], st));
    d.end(&key, Fate::Shown, Slot(3), &mut ev);
    assert_eq!(ev.as_slice(), [Output(Slot(3))]);
}

#[test]
fn a_segment_level_past_32_keeps_the_frame_level_delta_shift() {
    let t = Tail { lf_level: 30, lf_delta_enabled: true, base_q: 10, ..Tail::default() };
    let mut hd = parse::parse_uncompressed_header(&key_frame(0, 64, 64, &t)).unwrap();
    hd.segmentation.enabled = true;
    hd.segmentation.update_data = true;
    hd.segmentation.feature_enabled[1][SEG_LVL_ALT_L] = true;
    hd.segmentation.feature_data[1][SEG_LVL_ALT_L] = 10;
    let mut st = StreamState::default();
    st.fold(&hd);
    // the segment filters at 40, its deltas shift by the frame's 30 >> 5, not at all
    assert_eq!(st.filter_levels(&hd, 0), [[31, 31], [30, 30], [29, 29], [29, 29]]);
    assert_eq!(st.filter_levels(&hd, 1), [[41, 41], [40, 40], [39, 39], [39, 39]]);
}

#[test]
fn segment_features_outlive_a_frame_without_segmentation() {
    let t = Tail { base_q: 100, ..Tail::default() };
    let mut hd = parse::parse_uncompressed_header(&key_frame(0, 64, 64, &t)).unwrap();
    hd.segmentation.enabled = true;
    hd.segmentation.update_data = true;
    hd.segmentation.feature_enabled[1][SEG_LVL_ALT_Q] = true;
    hd.segmentation.feature_data[1][SEG_LVL_ALT_Q] = -30;
    let mut st = StreamState::default();
    st.fold(&hd);
    assert_eq!(st.qindex(&hd, 1), 70);
    let mut off = hd;
    off.frame_type = FrameType::NonKeyFrame;
    off.segmentation.enabled = false;
    off.segmentation.update_data = false;
    off.segmentation.feature_enabled = [[false; SEG_LVL_MAX]; MAX_SEGMENTS];
    st.fold(&off);
    assert_eq!(st.qindex(&off, 1), 100);
    // on again without new data, the held features apply
    let mut on = off;
    on.segmentation.enabled = true;
    st.fold(&on);
    assert_eq!(st.qindex(&on, 1), 70);
}

#[test]
fn a_skipped_frame_ends_with_nothing() {
    let (_, mut d) = feature_key_frame();
    let shown = d.parse(&show_existing(5)).unwrap();
    let mut ev = Events::new();
    assert_eq!(d.begin(&shown, &mut ev), Ok(Fate::Skip));
    assert_eq!(ev.as_slice(), [Output(Slot(1))]);
    d.end(&shown, Fate::Skip, Slot(9), &mut ev);
    assert_eq!(ev.as_slice(), []);
    assert_eq!(held(&d), [Some(1); NUM_SLOTS]);
}

#[test]
fn an_abandoned_frame_empties_its_slots_and_waits_for_a_key() {
    let (_, mut d) = feature_key_frame();
    let mut h = d.parse(&inter_frame(false, &Tail::default())).unwrap();
    h.refresh_frame_flags = 0b0000_0110;
    let mut ev = Events::new();
    assert_eq!(d.begin(&h, &mut ev), Ok(Fate::Shown));
    d.abandon(&h, &mut ev);
    // the key frame stays in its other slots
    assert_eq!(ev.as_slice(), []);
    assert_eq!(held(&d), [Some(1), None, None, Some(1), Some(1), Some(1), Some(1), Some(1)]);
    assert!(d.awaits_key());
    assert_eq!(run(&mut d, &h, 2), (Fate::Skip, vec![]));
    let (fate, got) = frame(&mut d, &key_frame(0, 64, 64, &Tail::default()), 3);
    assert_eq!((fate, got), (Fate::Shown, vec![Output(Slot(3)), Release(Slot(1))]));
    // a frame that would have taken every slot lets the key frame go
    let mut all = d.parse(&inter_frame(false, &Tail::default())).unwrap();
    all.refresh_frame_flags = 0xff;
    assert_eq!(d.begin(&all, &mut ev), Ok(Fate::Shown));
    d.abandon(&all, &mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(3))]);
    assert_eq!(held(&d), [None; NUM_SLOTS]);
}
