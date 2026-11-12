// Test fixtures.
#![allow(clippy::disallowed_methods)]

use profluens_video::h264parse::{
    self as hp, MmcoOp, Nal, RefListMod, SliceHeader, SliceType, Sps,
};

use super::*;
use crate::Event::{Output, Release};

const SPS_1080P: [u8; 27] = [
    0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03,
    0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
];

/// MaxFrameNum 16 and MaxPicOrderCntLsb 256.
fn sps(poc_type: u32, refs: u32, reorder: u32) -> Sps {
    let mut s = hp::parse_sps(&SPS_1080P).unwrap();
    assert_eq!(s.max_frame_num(), 16);
    s.pic_order_cnt_type = poc_type;
    s.log2_max_pic_order_cnt_lsb_minus4 = 4;
    s.max_num_ref_frames = refs;
    s.max_num_reorder_frames = Some(reorder);
    s.max_dec_frame_buffering = Some(16);
    s
}

fn header(ty: SliceType, frame_num: u32, lsb: u32) -> SliceHeader {
    SliceHeader {
        first_mb_in_slice: 0,
        slice_type: ty,
        pic_parameter_set_id: 0,
        frame_num,
        field_pic_flag: false,
        bottom_field_flag: false,
        idr_pic_id: 0,
        is_idr: false,
        pic_order_cnt_lsb: lsb,
        delta_pic_order_cnt_bottom: 0,
        delta_pic_order_cnt: [0; 2],
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        ref_list_mods_l0: Default::default(),
        ref_list_mods_l1: Default::default(),
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        disable_deblocking_filter_idc: 0,
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        direct_spatial_mv_pred_flag: false,
        no_output_of_prior_pics_flag: false,
        long_term_reference_flag: false,
        adaptive_ref_pic_marking_mode_flag: false,
        mmco_ops: Default::default(),
        pred_weights: None,
        header_bits_in_rbsp: 0,
    }
}

fn idr(lsb: u32) -> SliceHeader {
    let mut h = header(SliceType::I, 0, lsb);
    h.is_idr = true;
    h
}

fn with_mmco(mut h: SliceHeader, ops: &[(u32, u32, u32)]) -> SliceHeader {
    h.adaptive_ref_pic_marking_mode_flag = true;
    for &(op, arg1, arg2) in ops {
        h.mmco_ops.push(MmcoOp { op, arg1, arg2 });
    }
    h
}

fn nal(idr: bool, reference: bool) -> Nal {
    Nal {
        ref_idc: reference as u8,
        unit_type: if idr { hp::NAL_SLICE_IDR } else { hp::NAL_SLICE_NON_IDR },
        start: 0,
        end: 0,
    }
}

/// One picture through `begin` and `end` in `slot`, the events of both.
fn picture(
    d: &mut Dpb,
    sps: &Sps,
    reference: bool,
    sh: &SliceHeader,
    slot: u16,
) -> (Picture, Vec<Event>) {
    let mut ev = Events::new();
    let n = nal(sh.is_idr, reference);
    let pic = d.begin(sps, &n, sh, sh.slice_type == SliceType::I, &mut ev).unwrap();
    let mut all = ev.as_slice().to_vec();
    d.end(&pic, Slot(slot), sh, &mut ev).unwrap();
    all.extend_from_slice(ev.as_slice());
    (pic, all)
}

/// A DPB holding (slot, frame_num, POC, LongTermFrameIdx) with FrameNumWrap for `cur`,
/// MaxFrameNum 16.
fn with_refs(refs: &[(u16, u16, i32, Option<u8>)], cur: i32) -> Dpb {
    let mut d = Dpb::new();
    d.max_refs = MAX_REFS as u8;
    for (i, &(slot, frame_num, poc, long_term)) in refs.iter().enumerate() {
        d.refs[i] = Ref {
            slot: Slot(slot),
            frame_num,
            frame_num_wrap: if long_term.is_some() {
                i32::from(frame_num)
            } else {
                wrap(frame_num, cur, 16)
            },
            top_poc: poc,
            bottom_poc: poc,
            long_term,
            non_existing: false,
        };
    }
    d.n_refs = refs.len() as u8;
    d
}

fn current(frame_num: u16, poc: i32) -> Picture {
    Picture {
        fate: Fate::Shown,
        idr: false,
        reference: true,
        mmco5: false,
        frame_num,
        top_poc: poc,
        bottom_poc: poc,
        gop: 1,
        poc,
        key_now: false,
    }
}

fn lists(d: &Dpb, pic: &Picture, sh: &SliceHeader) -> ([u8; MAX_LIST], [u8; MAX_LIST]) {
    let (mut l0, mut l1) = ([0; MAX_LIST], [0; MAX_LIST]);
    d.lists(pic, sh, &mut l0, &mut l1);
    (l0, l1)
}

/// `f` of each listed reference up to the first unused entry.
fn named(d: &Dpb, l: &[u8], f: impl Fn(&Ref) -> i32) -> Vec<i32> {
    l.iter().take_while(|&&i| i != NO_REF).map(|&i| f(&d.refs()[i as usize])).collect()
}

fn slots(d: &Dpb) -> Vec<(u16, u16, Option<u8>)> {
    d.refs().iter().map(|r| (r.slot.0, r.frame_num, r.long_term)).collect()
}

fn flush(d: &mut Dpb) -> Vec<Event> {
    let mut ev = Events::new();
    d.flush(&mut ev);
    ev.as_slice().to_vec()
}

#[test]
fn pictures_go_out_in_order_and_leave_through_the_sliding_window() {
    use SliceType::*;
    let s = sps(0, 2, 1);
    let mut d = Dpb::new();
    let (pic, ev) = picture(&mut d, &s, true, &idr(0), 0);
    assert_eq!((pic.fate, pic.gop, ev), (Fate::Shown, 1, vec![]));
    let (_, ev) = picture(&mut d, &s, true, &header(P, 1, 4), 1);
    assert_eq!(ev, [Output(Slot(0))]);
    let (_, ev) = picture(&mut d, &s, false, &header(B, 2, 2), 2);
    assert_eq!(ev, [Output(Slot(2)), Release(Slot(2))]);
    // two frames held, frame_num 0 leaves for frame_num 2
    let (_, ev) = picture(&mut d, &s, true, &header(P, 2, 8), 3);
    assert_eq!(ev, [Output(Slot(1)), Release(Slot(0))]);
    let (_, ev) = picture(&mut d, &s, false, &header(B, 3, 6), 4);
    assert_eq!(ev, [Output(Slot(4)), Release(Slot(4))]);
    assert_eq!(slots(&d), [(1, 1, None), (3, 2, None)]);
    assert_eq!(flush(&mut d), [Output(Slot(3))]);
}

#[test]
fn p_lists_descend_by_frame_num_wrap_and_modify() {
    let d = with_refs(&[(10, 0, 0, None), (11, 2, 4, None), (12, 1, 2, None)], 3);
    let mut sh = header(SliceType::P, 3, 6);
    sh.num_ref_idx_l0_active_minus1 = 2;
    let (l0, l1) = lists(&d, &current(3, 6), &sh);
    assert_eq!(named(&d, &l0, |r| i32::from(r.frame_num)), [2, 1, 0]);
    assert_eq!(l1, [NO_REF; MAX_LIST]);
    // frame_num 0, named by abs_diff_pic_num_minus1 2, goes first
    sh.ref_list_mods_l0.push(RefListMod { op: 0, value: 2 });
    let (l0, _) = lists(&d, &current(3, 6), &sh);
    assert_eq!(named(&d, &l0, |r| i32::from(r.frame_num)), [0, 2, 1]);
    sh.num_ref_idx_l0_active_minus1 = 1;
    let (l0, _) = lists(&d, &current(3, 6), &sh);
    assert_eq!(named(&d, &l0, |r| i32::from(r.frame_num)), [0, 2]);
    assert_eq!(l0[2], NO_REF);
}

#[test]
fn modifications_repeat_pictures_and_hold_missing_ones() {
    let d = with_refs(&[(1, 3, 6, None), (2, 4, 8, None), (3, 1, 2, Some(0))], 5);
    let mut sh = header(SliceType::P, 5, 10);
    sh.num_ref_idx_l0_active_minus1 = 4;
    let (l0, _) = lists(&d, &current(5, 10), &sh);
    // short-term by FrameNumWrap, then long-term
    assert_eq!(&l0[..4], [1, 0, 2, NO_REF]);
    // frame_num 4 twice (16 back is a full wrap), long-term 1 that is not there, frame_num 3
    for (op, value) in [(0, 0), (0, 15), (2, 1), (0, 0)] {
        sh.ref_list_mods_l0.push(RefListMod { op, value });
    }
    let (l0, _) = lists(&d, &current(5, 10), &sh);
    assert_eq!(&l0[..6], [1, 1, NO_REF, 0, 2, NO_REF]);
}

#[test]
fn b_lists_split_by_poc_with_long_term_last() {
    let d = with_refs(
        &[
            (1, 0, 0, None),
            (2, 1, 2, None),
            (3, 2, 8, None),
            (4, 3, 6, None),
            (5, 4, 10, Some(1)),
            (6, 5, 12, Some(0)),
        ],
        6,
    );
    let mut sh = header(SliceType::B, 6, 4);
    sh.num_ref_idx_l0_active_minus1 = 5;
    sh.num_ref_idx_l1_active_minus1 = 5;
    let (l0, l1) = lists(&d, &current(6, 4), &sh);
    assert_eq!(named(&d, &l0, Ref::poc), [2, 0, 6, 8, 12, 10]);
    assert_eq!(named(&d, &l1, Ref::poc), [6, 8, 2, 0, 12, 10]);
    // all before the current picture, list 1 equal to list 0 swaps its first two
    let d = with_refs(&[(1, 0, 0, None), (2, 1, 2, None)], 2);
    sh.num_ref_idx_l0_active_minus1 = 1;
    sh.num_ref_idx_l1_active_minus1 = 1;
    let (l0, l1) = lists(&d, &current(2, 4), &sh);
    assert_eq!(named(&d, &l0, Ref::poc), [2, 0]);
    assert_eq!(named(&d, &l1, Ref::poc), [0, 2]);
}

#[test]
fn mmco_unmarks_by_pic_num_across_a_frame_num_wrap() {
    // past a frame_num wrap picNumX is negative (8.2.4.1, 8.2.5.4.1)
    let mut d = with_refs(&[(1, 14, 0, None), (2, 15, 2, None), (3, 0, 4, None)], 1);
    let sh = with_mmco(header(SliceType::P, 1, 6), &[(1, 2, 0), (3, 1, 0)]);
    let mut ev = Events::new();
    d.end(&current(1, 6), Slot(9), &sh, &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Release(Slot(1))]);
    assert_eq!(slots(&d), [(2, 15, Some(0)), (3, 0, None), (9, 1, None)]);
}

#[test]
fn a_long_term_index_given_again_unmarks_its_old_holder() {
    let mut d = with_refs(&[(1, 2, 0, Some(0)), (2, 3, 2, None), (3, 4, 4, None)], 5);
    // MMCO 3 moves LongTermFrameIdx 0 to frame_num 3 (picNumX 5 - 2)
    let sh = with_mmco(header(SliceType::P, 5, 10), &[(3, 1, 0)]);
    let mut ev = Events::new();
    d.end(&current(5, 10), Slot(9), &sh, &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Release(Slot(1))]);
    assert_eq!(slots(&d), [(2, 3, Some(0)), (3, 4, None), (9, 5, None)]);
    // MMCO 6 makes the current picture long-term 0, MMCO 4 drops indices from 1 up first
    let mut d = with_refs(&[(1, 2, 0, Some(0)), (2, 3, 2, Some(1))], 4);
    let sh = with_mmco(header(SliceType::P, 4, 10), &[(4, 1, 0), (6, 0, 0)]);
    d.end(&current(4, 10), Slot(9), &sh, &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Release(Slot(2)), Release(Slot(1))]);
    assert_eq!(slots(&d), [(9, 4, Some(0))]);
}

#[test]
fn an_idr_puts_prior_pictures_out_or_drops_them_unseen() {
    use SliceType::*;
    let s = sps(0, 4, 2);
    let mut d = Dpb::new();
    assert!(picture(&mut d, &s, true, &idr(0), 0).1.is_empty());
    assert!(picture(&mut d, &s, true, &header(P, 1, 8), 1).1.is_empty());
    let (_, ev) = picture(&mut d, &s, false, &header(B, 2, 4), 2);
    assert_eq!(ev, [Output(Slot(0))]);
    let (pic, ev) = picture(&mut d, &s, true, &idr(0), 3);
    assert_eq!(pic.gop, 2);
    assert_eq!(
        ev,
        [Output(Slot(2)), Release(Slot(2)), Output(Slot(1)), Release(Slot(0)), Release(Slot(1))]
    );
    assert!(picture(&mut d, &s, true, &header(P, 1, 8), 4).1.is_empty());
    let (_, ev) = picture(&mut d, &s, false, &header(B, 2, 4), 5);
    assert_eq!(ev, [Output(Slot(3))]);
    let mut h = idr(0);
    h.no_output_of_prior_pics_flag = true;
    let (_, ev) = picture(&mut d, &s, true, &h, 6);
    assert_eq!(ev, [Release(Slot(5)), Release(Slot(3)), Release(Slot(4))]);
    assert_eq!(slots(&d), [(6, 0, None)]);
}

#[test]
fn an_mmco5_puts_prior_pictures_out_and_restarts_the_order() {
    use SliceType::*;
    let s = sps(0, 4, 2);
    let mut d = Dpb::new();
    picture(&mut d, &s, true, &idr(0), 0);
    picture(&mut d, &s, true, &header(P, 1, 8), 1);
    picture(&mut d, &s, false, &header(B, 2, 4), 2);
    let sh = with_mmco(header(P, 2, 12), &[(5, 0, 0)]);
    let mut ev = Events::new();
    let pic = d.begin(&s, &nal(false, true), &sh, false, &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Output(Slot(2)), Release(Slot(2)), Output(Slot(1))]);
    // decoded with its order count, stored and put out from 0 in a new sequence
    assert_eq!((pic.mmco5, pic.top_poc, pic.poc, pic.gop), (true, 12, 0, 2));
    d.end(&pic, Slot(3), &sh, &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Release(Slot(0)), Release(Slot(1))]);
    assert_eq!(slots(&d), [(3, 0, None)]);
    assert_eq!(d.refs()[0].poc(), 0);
    // frame_num goes on from 0 without a gap, order counts from 0
    let (pic, ev) = picture(&mut d, &s, true, &header(P, 1, 8), 4);
    assert_eq!((pic.poc, ev), (8, vec![]));
    assert_eq!(slots(&d).len(), 2);
    let (_, ev) = picture(&mut d, &s, false, &header(B, 2, 4), 5);
    assert_eq!(ev, [Output(Slot(3))]);
    assert_eq!(flush(&mut d), [Output(Slot(5)), Release(Slot(5)), Output(Slot(4))]);
}

#[test]
fn a_frame_num_gap_fills_non_existing_frames_in_the_slot_before_it() {
    use SliceType::*;
    // POC type 2 never reorders, three references
    let s = sps(2, 3, 0);
    let mut d = Dpb::new();
    let (_, ev) = picture(&mut d, &s, true, &idr(0), 0);
    assert_eq!(ev, [Output(Slot(0))]);
    assert!(picture(&mut d, &s, true, &header(P, 1, 0), 1).1.is_empty());
    // frame_num 2 and 3 are missing, frame_num 0 leaves for 3
    let (pic, ev) = picture(&mut d, &s, true, &header(P, 4, 0), 2);
    assert_eq!(pic.poc, 8);
    assert_eq!(ev, [Release(Slot(0)), Output(Slot(1))]);
    let fillers: Vec<_> =
        d.refs().iter().map(|r| (r.slot.0, r.frame_num, r.poc(), r.non_existing)).collect();
    assert_eq!(fillers, [(1, 2, 4, true), (1, 3, 6, true), (2, 4, 8, false)]);

    let mut sh = header(P, 5, 0);
    sh.num_ref_idx_l0_active_minus1 = 2;
    let mut ev = Events::new();
    let pic = d.begin(&s, &nal(false, true), &sh, false, &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Output(Slot(2))]);
    let (l0, _) = lists(&d, &pic, &sh);
    assert_eq!(named(&d, &l0, |r| i32::from(r.frame_num)), [4, 3, 2]);
    // the filler of frame_num 2 leaves, slot 1 stays with the one of 3
    d.end(&pic, Slot(3), &sh, &mut ev).unwrap();
    assert!(ev.as_slice().is_empty());
    let (_, ev) = picture(&mut d, &s, true, &header(P, 6, 0), 4);
    assert_eq!(ev, [Output(Slot(3)), Release(Slot(1))]);

    // a gap past the reference count fills only what the sliding window keeps
    let (_, ev) = picture(&mut d, &s, true, &header(P, 15, 0), 5);
    assert_eq!(ev, [Release(Slot(2)), Release(Slot(3)), Output(Slot(4))]);
    let fillers: Vec<_> = d.refs().iter().map(|r| (r.slot.0, r.frame_num, r.poc())).collect();
    assert_eq!(fillers, [(4, 13, 26), (4, 14, 28), (5, 15, 30)]);
}

#[test]
fn after_a_discard_a_non_idr_key_goes_out_at_once_and_hides_its_leading_pictures() {
    use SliceType::*;
    let s = sps(0, 4, 2);
    let mut d = Dpb::new();
    let mut ev = Events::new();
    d.discard(&mut ev);
    assert!(ev.as_slice().is_empty());
    let (pic, ev) = picture(&mut d, &s, true, &header(P, 3, 20), 0);
    assert_eq!((pic.fate, ev), (Fate::Skip, vec![]));
    let (pic, ev) = picture(&mut d, &s, true, &header(I, 5, 32), 1);
    assert_eq!((pic.fate, ev), (Fate::Shown, vec![Output(Slot(1))]));
    // a leading reference picture is decoded and kept, a leading non-reference one skipped
    let (pic, ev) = picture(&mut d, &s, true, &header(B, 6, 28), 2);
    assert_eq!((pic.fate, ev), (Fate::Hidden, vec![]));
    let (pic, ev) = picture(&mut d, &s, false, &header(B, 7, 26), 3);
    assert_eq!((pic.fate, ev), (Fate::Skip, vec![]));
    let (pic, ev) = picture(&mut d, &s, true, &header(P, 7, 40), 4);
    assert_eq!((pic.fate, ev), (Fate::Shown, vec![]));
    let mut ev = Events::new();
    d.discard(&mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(1)), Release(Slot(2)), Release(Slot(4))]);
}

#[test]
fn a_key_of_a_stream_without_reorder_goes_out_at_once() {
    let mut d = Dpb::new();
    let (_, ev) = picture(&mut d, &sps(0, 4, 0), true, &idr(0), 0);
    assert_eq!(ev, [Output(Slot(0))]);
    let mut d = Dpb::new();
    let (_, ev) = picture(&mut d, &sps(0, 4, 2), true, &idr(0), 0);
    assert!(ev.is_empty());
}

#[test]
fn field_pictures_are_refused() {
    let mut sh = idr(0);
    sh.field_pic_flag = true;
    let mut ev = Events::new();
    let r = Dpb::new().begin(&sps(0, 4, 2), &nal(true, true), &sh, true, &mut ev);
    assert_eq!(r, Err(DpbError::Fields));
}

/// (idr, intra, reset, poc, reference) per picture in decode order, the fates that come.
fn run(st: &mut Start, pics: &[(bool, bool, bool, i32, bool)]) -> Vec<Fate> {
    pics.iter()
        .map(
            |&(idr, intra, reset, poc, r)| {
                if st.wants(idr, intra) { st.fate(reset, poc, r) } else { Fate::Skip }
            },
        )
        .collect()
}

#[test]
fn waits_for_a_key_and_hides_leading_pictures() {
    use Fate::*;
    let mut st = Start::AwaitKey;
    // P and B before the key, an open GOP I at 16, a reference B at 12, b at 10 and 14, P 24,
    // B 20
    let f = run(
        &mut st,
        &[
            (false, false, false, 4, true),
            (false, false, false, 2, false),
            (false, true, false, 16, true),
            (false, false, false, 12, true),
            (false, false, false, 10, false),
            (false, false, false, 14, false),
            (false, false, false, 24, true),
            (false, false, false, 20, false),
        ],
    );
    assert_eq!(f, [Skip, Skip, Shown, Hidden, Skip, Skip, Shown, Shown]);
    assert_eq!(st, Start::Leading { poc: 16 });
    // an IDR ends the leading run, order counts restart
    let f = run(&mut st, &[(true, true, true, 0, true), (false, false, false, -2, false)]);
    assert_eq!(f, [Shown, Shown]);
    assert_eq!(st, Start::Running);
}

#[test]
fn idr_and_mmco5_keys_have_no_leading_run() {
    let mut st = Start::AwaitKey;
    let f = run(
        &mut st,
        &[
            (true, true, true, 0, true),
            (false, false, false, 4, true),
            (false, false, false, 2, false),
        ],
    );
    assert_eq!(f, [Fate::Shown; 3]);
    let mut st = Start::AwaitKey;
    // an all intra picture with MMCO 5 restarts the order count like an IDR
    assert_eq!(run(&mut st, &[(false, true, true, 0, true)]), [Fate::Shown]);
    assert_eq!(st, Start::Running);
    // an all intra picture mid stream after the start changes nothing
    let f = run(&mut st, &[(false, true, false, 40, true), (false, false, false, 36, false)]);
    assert_eq!(f, [Fate::Shown; 2]);
}

#[test]
fn mmco5_inside_the_leading_run_ends_it() {
    let mut st = Start::AwaitKey;
    run(&mut st, &[(false, true, false, 100, true)]);
    let f = run(
        &mut st,
        &[
            (false, false, false, 96, true),
            (false, false, true, 0, true),
            (false, false, false, 4, false),
        ],
    );
    assert_eq!(f, [Fate::Hidden, Fate::Shown, Fate::Shown]);
}

/// Output order of pictures in decode order, and how many had come in when the first went
/// out, with `due_before` ahead of each decode or the bump after it only.
fn order(pics: &[(u32, i32)], depth: usize, ahead: bool) -> (Vec<(u32, i32)>, usize) {
    let (mut waiting, mut out, mut first_at) = (Vec::new(), Vec::new(), 0);
    for (n, &p) in pics.iter().enumerate() {
        if ahead {
            while let Some(i) = due_before(&waiting, depth, p) {
                let w: Waiting = waiting.remove(i);
                out.push((w.gop, w.poc));
            }
        }
        if !out.is_empty() && first_at == 0 {
            first_at = n;
        }
        waiting.push(Waiting { slot: Slot(0), gop: p.0, poc: p.1 });
        while waiting.len() > depth {
            let w = waiting.remove(smallest(&waiting).unwrap());
            out.push((w.gop, w.poc));
        }
        if !out.is_empty() && first_at == 0 {
            first_at = n + 1;
        }
    }
    while let Some(i) = smallest(&waiting) {
        let w = waiting.remove(i);
        out.push((w.gop, w.poc));
    }
    (out, first_at)
}

#[test]
fn bumping_ahead_keeps_the_order_and_frees_the_key_sooner() {
    // I0 P4 B2 b1 b3 P8 B6 b5 b7, two reorders deep, then the same in a second GOP
    let gop = [0, 4, 2, 1, 3, 8, 6, 5, 7];
    let pics: Vec<(u32, i32)> =
        gop.iter().map(|&p| (1, p * 2)).chain(gop.iter().map(|&p| (2, p * 2))).collect();
    let (post, at_post) = order(&pics, 2, false);
    let (ahead, at_ahead) = order(&pics, 2, true);
    let mut sorted = pics.clone();
    sorted.sort();
    assert_eq!(post, sorted);
    assert_eq!(ahead, sorted);
    // the key went out once the third picture came instead of after it
    assert_eq!((at_post, at_ahead), (3, 2));
    // I0 P2 b1 one deep, the key is out when P2's header comes
    let (o, at) = order(&[(1, 0), (1, 4), (1, 2), (1, 8), (1, 6)], 1, true);
    assert_eq!(o, [(1, 0), (1, 2), (1, 4), (1, 6), (1, 8)]);
    assert_eq!(at, 1);
}

#[test]
fn bumping_ahead_matches_bumping_after_on_random_streams() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rand = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for _ in 0..500 {
        let depth = 1 + rand(4);
        let mut pics = Vec::new();
        for gop in 0..1 + rand(3) as u32 {
            // display order shuffled within blocks of depth + 1, at most `depth` reorders
            let n = 1 + rand(40);
            let mut poc: Vec<i32> = (0..n as i32).map(|p| p * 2).collect();
            for block in poc[1..].chunks_mut(depth + 1) {
                for i in (1..block.len()).rev() {
                    block.swap(i, rand(i + 1));
                }
            }
            pics.extend(poc.into_iter().map(|p| (gop, p)));
        }
        let (post, at_post) = order(&pics, depth, false);
        let (ahead, at_ahead) = order(&pics, depth, true);
        assert_eq!(post, ahead, "depth {depth} {pics:?}");
        assert!(at_ahead <= at_post);
    }
}

/// `ahead` on the first slice header, then the picture, the events of each.
fn ahead_then_picture(
    d: &mut Dpb,
    sps: &Sps,
    reference: bool,
    sh: &SliceHeader,
    slot: u16,
) -> (Vec<Event>, Vec<Event>) {
    let mut ev = Events::new();
    d.ahead(sps, &nal(sh.is_idr, reference), sh, &mut ev);
    let early = ev.as_slice().to_vec();
    (early, picture(d, sps, reference, sh, slot).1)
}

#[test]
fn ahead_lets_out_what_begin_would_and_begin_then_nothing_more() {
    use SliceType::*;
    let s = sps(0, 4, 2);
    let mut d = Dpb::new();
    picture(&mut d, &s, true, &idr(0), 0);
    let (early, ev) = ahead_then_picture(&mut d, &s, true, &header(P, 1, 8), 1);
    assert_eq!((early, ev), (vec![], vec![]));
    let (early, ev) = ahead_then_picture(&mut d, &s, false, &header(B, 2, 4), 2);
    assert_eq!((early, ev), (vec![Output(Slot(0))], vec![]));
    // a b before the smallest waiting one lets nothing go
    let (early, _) = ahead_then_picture(&mut d, &s, false, &header(B, 2, 2), 3);
    assert_eq!(early, []);
    // an IDR puts every prior picture out, unless it drops them
    let (early, ev) = ahead_then_picture(&mut d, &s, true, &idr(0), 4);
    assert_eq!(early, [Output(Slot(2)), Release(Slot(2)), Output(Slot(1))]);
    assert_eq!(ev, [Release(Slot(0)), Release(Slot(1))]);
    picture(&mut d, &s, true, &header(P, 1, 8), 5);
    let mut h = idr(0);
    h.no_output_of_prior_pics_flag = true;
    let (early, ev) = ahead_then_picture(&mut d, &s, true, &h, 6);
    assert_eq!(early, []);
    assert_eq!(ev, [Release(Slot(4)), Release(Slot(5))]);
    // nothing before the first key
    let mut d = Dpb::new();
    let (early, _) = ahead_then_picture(&mut d, &s, true, &header(P, 3, 8), 0);
    assert_eq!(early, []);
}

#[test]
fn slice_headers_peek() {
    // IDR I slice, first_mb 0 (1), slice_type 7 (0001000), pps 0 (1)
    let i = [0x65, 0b1000_1000, 0b1000_0000];
    assert_eq!((peek_slice_type(&i), peek_pps_id(&i)), (Some(2), Some(0)));
    // P slice, slice_type 5 (00110), pps 1 (010)
    let p = [0x41, 0b1001_1001, 0];
    assert_eq!((peek_slice_type(&p), peek_pps_id(&p)), (Some(0), Some(1)));
    assert_eq!(peek_slice_type(&[0x41]), None);
    assert!(is_slice(hp::NAL_SLICE_IDR) && is_slice(hp::NAL_SLICE_NON_IDR));
    assert!(!is_slice(hp::NAL_SPS));
}

#[test]
fn bump_one_lets_the_smallest_waiting_picture_out_ahead_of_the_reorder() {
    use SliceType::*;
    let s = sps(0, 4, 4);
    let mut d = Dpb::new();
    picture(&mut d, &s, true, &idr(0), 0);
    picture(&mut d, &s, true, &header(P, 1, 8), 1);
    picture(&mut d, &s, false, &header(B, 2, 4), 2);
    let mut ev = Events::new();
    assert!(d.bump_one(&mut ev));
    assert_eq!(ev.as_slice(), [Output(Slot(0))]);
    // a non-reference picture goes with its output
    assert!(d.bump_one(&mut ev));
    assert_eq!(ev.as_slice(), [Output(Slot(2)), Release(Slot(2))]);
    assert!(d.bump_one(&mut ev));
    assert_eq!(ev.as_slice(), [Output(Slot(1))]);
    assert!(!d.bump_one(&mut ev));
    assert!(ev.as_slice().is_empty());
}
