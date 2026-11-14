// Test fixtures.
#![allow(clippy::disallowed_methods)]

use profluens_video::fixed::FixedList;
use profluens_video::h265parse::{self as hp, Nal, SliceHeader, SliceType};

use super::*;
use crate::Event::{Output, Release};

const SEQ: Seq = Seq { max_poc_lsb: 16, reorder: 1, max_latency: None, dpb_size: MAX_DPB as u8 };

fn nal(t: u8) -> Nal {
    Nal { nal_type: t, temporal_id: 0, start: 0, end: 0 }
}

fn header(
    lsb: u32,
    ty: SliceType,
    s0: &[(i32, bool)],
    s1: &[(i32, bool)],
    lt: &[(u32, bool)],
) -> SliceHeader {
    let mut st = hp::ShortTermRps::default();
    for &(d, u) in s0 {
        st.delta_poc_s0.push(d);
        st.used_s0.push(u);
    }
    for &(d, u) in s1 {
        st.delta_poc_s1.push(d);
        st.used_s1.push(u);
    }
    let mut long_term_refs = FixedList::default();
    for &(poc_lsb, used_by_curr_pic) in lt {
        long_term_refs.push(hp::LongTermRef {
            poc_lsb,
            used_by_curr_pic,
            delta_poc_msb_present: false,
            delta_poc_msb_cycle: 0,
        });
    }
    SliceHeader {
        first_slice_segment_in_pic_flag: true,
        no_output_of_prior_pics_flag: false,
        pps_id: 0,
        dependent_slice_segment_flag: false,
        slice_segment_address: 0,
        slice_type: ty,
        pic_output_flag: true,
        pic_order_cnt_lsb: lsb,
        st_rps: st,
        st_rps_bits: 0,
        long_term_refs,
        slice_temporal_mvp_enabled_flag: false,
        slice_sao_luma_flag: false,
        slice_sao_chroma_flag: false,
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        ref_list_mods: hp::RefListMods::default(),
        mvd_l1_zero_flag: false,
        cabac_init_flag: false,
        collocated_from_l0_flag: true,
        collocated_ref_idx: 0,
        pred_weights: None,
        five_minus_max_num_merge_cand: 0,
        slice_qp_delta: 0,
        slice_cb_qp_offset: 0,
        slice_cr_qp_offset: 0,
        cu_chroma_qp_offset_enabled_flag: false,
        deblocking_filter_override_flag: false,
        slice_deblocking_filter_disabled_flag: false,
        slice_beta_offset_div2: 0,
        slice_tc_offset_div2: 0,
        slice_loop_filter_across_slices_enabled_flag: false,
        num_entry_point_offsets: 0,
        slice_data_byte_offset: 0,
        num_emu_prev_bytes: 0,
    }
}

fn intra(lsb: u32) -> SliceHeader {
    header(lsb, SliceType::I, &[], &[], &[])
}

/// Decodes one picture into `slot`, the events of `begin` and `end` together.
fn picture(d: &mut Dpb, seq: &Seq, t: u8, sh: &SliceHeader, slot: u16) -> (Picture, Vec<Event>) {
    let mut ev = Events::new();
    let p = d.begin(seq, &nal(t), sh, sh.slice_type.is_inter(), &mut ev).expect("begin");
    let mut all = ev.as_slice().to_vec();
    d.end(&p, Slot(slot), &mut ev).expect("end");
    all.extend_from_slice(ev.as_slice());
    (p, all)
}

fn slots(d: &Dpb) -> Vec<u16> {
    d.refs().iter().map(|r| r.slot.0).collect()
}

fn entry(slot: u16, poc: i32) -> Ref {
    Ref { slot: Slot(slot), poc, long_term: false, set: RpsSet::Foll }
}

fn with_refs(refs: &[Ref]) -> Dpb {
    let mut d = Dpb::new();
    d.refs[..refs.len()].copy_from_slice(refs);
    d.n_refs = refs.len() as u8;
    d
}

#[test]
fn only_pictures_before_the_key_let_out_ahead_are_leading() {
    assert!(!Lead::Off.leads(1, -1) && !Lead::Flushed.leads(1, -1));
    let out = Lead::Out(3, 8);
    assert!(out.leads(3, 7) && !out.leads(3, 8) && !out.leads(3, 9));
    assert!(!out.leads(4, 0), "the next sequence orders anew");
}

#[test]
fn rps_marks_and_orders_the_current_sets() {
    // DPB in decode order: 0, 8, 4, 2, 33 (long-term), cur 6
    let mut lt = entry(4, 33);
    lt.long_term = true;
    let mut d = with_refs(&[entry(0, 0), entry(1, 8), entry(2, 4), entry(3, 2), lt]);
    let sh = header(
        6,
        SliceType::B,
        &[(-2, true), (-4, true), (-6, false)],
        &[(2, true)],
        &[(33 % 16, true)],
    );
    assert!(d.mark(&sh, 6, 16));
    let sets: Vec<_> = d.refs().iter().map(|r| r.set).collect();
    assert_eq!(
        sets,
        [
            RpsSet::Foll,
            RpsSet::StCurrAfter,
            RpsSet::StCurrBefore,
            RpsSet::StCurrBefore,
            RpsSet::LtCurr
        ]
    );
    // the sets follow the RPS order, as DPB indices
    let c = d.curr_sets(&sh, 6, 16);
    assert_eq!(c.before(), &[2, 3]);
    assert_eq!(c.after(), &[1]);
    assert_eq!(c.lt(), &[4]);
    assert_eq!(c.total(), 4);
    // a used picture that is gone is reported, an unused one is not
    let mut small = with_refs(&[entry(0, 4)]);
    assert!(!small.mark(&header(6, SliceType::P, &[(-4, true)], &[], &[]), 6, 16));
    let sh = header(8, SliceType::P, &[(-2, false), (-4, false)], &[], &[]);
    assert!(small.mark(&sh, 8, 16));
    assert_eq!(small.refs()[0].set, RpsSet::Foll);
}

#[test]
fn short_term_entries_never_match_long_term_pictures() {
    let mut lt = entry(0, 4);
    lt.long_term = true;
    let mut d = with_refs(&[lt]);
    assert!(!d.mark(&header(6, SliceType::P, &[(-2, true)], &[], &[]), 6, 16));
    assert_eq!(d.refs()[0].set, RpsSet::None);
}

#[test]
fn prior_pictures_go_out_unless_the_irap_says_not_to() {
    assert!(prior_pics_out(hp::NAL_IDR_W_RADL, false, false));
    assert!(!prior_pics_out(hp::NAL_IDR_N_LP, true, false));
    assert!(!prior_pics_out(hp::NAL_BLA_W_LP, true, true));
    // a CRA after an end of sequence drops them whatever the flag says
    assert!(prior_pics_out(hp::NAL_CRA_NUT, false, false));
    assert!(!prior_pics_out(hp::NAL_CRA_NUT, false, true));
    assert!(prior_pics_out(hp::NAL_IDR_W_RADL, false, true));
}

#[test]
fn poc_wraps_and_skips_non_reference_pictures() {
    let mut p = PocState::default();
    // IDR, then lsb wraps at 16
    assert_eq!(p.compute(16, 0, hp::NAL_IDR_W_RADL, 0, true), 0);
    assert_eq!(p.compute(16, 8, hp::NAL_TRAIL_R, 0, false), 8);
    assert_eq!(p.compute(16, 14, hp::NAL_TRAIL_R, 0, false), 14);
    // a TRAIL_N far ahead does not become prevTid0Pic
    assert_eq!(p.compute(16, 6, hp::NAL_TRAIL_N, 0, false), 22);
    assert_eq!(p.compute(16, 2, hp::NAL_TRAIL_R, 0, false), 18);
    // RASL with a smaller lsb stays in the same cycle
    assert_eq!(p.compute(16, 15, hp::NAL_RASL_N, 0, false), 15);
    assert_eq!(p.compute(16, 4, hp::NAL_TRAIL_R, 0, false), 20);
}

#[test]
fn ref_lists_cycle_and_modify() {
    let mut c = CurrSets::default();
    c.before[..2].copy_from_slice(&[3, 1]);
    c.n_before = 2;
    c.after[0] = 0;
    c.n_after = 1;
    c.lt[0] = 2;
    c.n_lt = 1;
    let mut out = [0u8; MAX_LIST];
    build_list(&c, 0, 6, None, &mut out);
    assert_eq!(&out[..7], &[3, 1, 0, 2, 3, 1, NO_REF]);
    build_list(&c, 1, 3, None, &mut out);
    assert_eq!(&out[..4], &[0, 3, 1, NO_REF]);
    build_list(&c, 0, 2, Some(&[3, 0]), &mut out);
    assert_eq!(&out[..3], &[2, 3, NO_REF]);
    // entries past NumPicTotalCurr come from the cycled temp list
    build_list(&c, 1, 6, Some(&[5, 4, 0, 0, 0, 0]), &mut out);
    assert_eq!(&out[..2], &[3, 0]);
    let empty = CurrSets::default();
    build_list(&empty, 0, 2, None, &mut out);
    assert_eq!(out, [NO_REF; MAX_LIST]);
}

#[test]
fn pictures_go_out_in_order_and_are_released_when_the_rps_drops_them() {
    let mut d = Dpb::new();
    let (_, ev) = picture(&mut d, &SEQ, hp::NAL_IDR_W_RADL, &intra(0), 0);
    assert_eq!(ev, []);
    let p4 = header(4, SliceType::P, &[(-4, true)], &[], &[]);
    let (_, ev) = picture(&mut d, &SEQ, hp::NAL_TRAIL_R, &p4, 1);
    assert_eq!(ev, [Output(Slot(0))]);
    // B at 2 between 0 and 4
    let b2 = header(2, SliceType::B, &[(-2, true)], &[(2, true)], &[]);
    let mut ev = Events::new();
    let p = d.begin(&SEQ, &nal(hp::NAL_TRAIL_N), &b2, true, &mut ev).unwrap();
    assert_eq!((p.poc, p.fate), (2, Fate::Shown));
    assert_eq!((p.sets.before(), p.sets.after()), (&[0u8][..], &[1u8][..]));
    let (mut l0, mut l1) = ([0; MAX_LIST], [0; MAX_LIST]);
    d.lists(&p, &b2, &mut l0, &mut l1);
    assert_eq!((l0[..2].to_vec(), l1[..2].to_vec()), (vec![0, NO_REF], vec![1, NO_REF]));
    d.end(&p, Slot(2), &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Output(Slot(2))]);
    // P at 8 keeps only 4, 0 and 2 leave the DPB having gone out
    let p8 = header(8, SliceType::P, &[(-4, true)], &[], &[]);
    let (p, ev) = picture(&mut d, &SEQ, hp::NAL_TRAIL_R, &p8, 3);
    assert_eq!(ev, [Release(Slot(0)), Release(Slot(2)), Output(Slot(1))]);
    assert_eq!(p.sets.before(), &[0]);
    assert_eq!(slots(&d), [1, 3]);
    // flush puts 8 out and keeps the references, a discard releases them
    let mut ev = Events::new();
    d.flush(&mut ev);
    assert_eq!(ev.as_slice(), [Output(Slot(3))]);
    d.discard(&mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(1)), Release(Slot(3))]);
    assert_eq!(slots(&d), Vec::<u16>::new());
}

#[test]
fn a_picture_waiting_for_output_is_released_only_once_it_is_out() {
    let mut d = Dpb::new();
    let seq = Seq { reorder: 2, ..SEQ };
    picture(&mut d, &seq, hp::NAL_IDR_W_RADL, &intra(0), 0);
    // 8 then 4, 0 leaves the RPS at 4 while 8 waits
    let p8 = header(8, SliceType::P, &[(-8, true)], &[], &[]);
    let (_, ev) = picture(&mut d, &seq, hp::NAL_TRAIL_R, &p8, 1);
    assert_eq!(ev, []);
    let p4 = header(4, SliceType::P, &[], &[(4, true)], &[]);
    let (_, ev) = picture(&mut d, &seq, hp::NAL_TRAIL_R, &p4, 2);
    // 0 waits so leaving the DPB releases nothing, going out releases it
    assert_eq!(ev, [Output(Slot(0)), Release(Slot(0))]);
}

#[test]
fn a_missing_reference_fails_inter_pictures_only() {
    let mut d = Dpb::new();
    picture(&mut d, &SEQ, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let mut ev = Events::new();
    let lost = header(4, SliceType::P, &[(-2, true)], &[], &[]);
    assert_eq!(
        d.begin(&SEQ, &nal(hp::NAL_TRAIL_R), &lost, true, &mut ev),
        Err(DpbError::MissingReference)
    );
    // the marking still applied, 0 left the DPB
    assert_eq!(ev.as_slice(), []);
    assert_eq!(slots(&d), Vec::<u16>::new());
    let mut i = lost.clone();
    i.slice_type = SliceType::I;
    i.pic_order_cnt_lsb = 6;
    let p = d.begin(&SEQ, &nal(hp::NAL_TRAIL_R), &i, false, &mut ev).unwrap();
    assert_eq!(p.fate, Fate::Shown);
}

#[test]
fn an_irap_puts_prior_pictures_out_or_drops_them_unseen() {
    let seq = Seq { reorder: 4, ..SEQ };
    let run = |t: u8, no_output: bool, eos: bool| {
        let mut d = Dpb::new();
        picture(&mut d, &seq, hp::NAL_IDR_W_RADL, &intra(0), 0);
        let p4 = header(4, SliceType::P, &[(-4, true)], &[], &[]);
        picture(&mut d, &seq, hp::NAL_TRAIL_R, &p4, 1);
        if eos {
            d.end_of_sequence();
        }
        let mut key = intra(0);
        key.no_output_of_prior_pics_flag = no_output;
        let mut ev = Events::new();
        let p = d.begin(&seq, &nal(t), &key, false, &mut ev).unwrap();
        assert!(p.no_rasl_output && p.irap);
        assert_eq!(p.gop, 2);
        ev.as_slice().to_vec()
    };
    let out = [Output(Slot(0)), Output(Slot(1)), Release(Slot(0)), Release(Slot(1))];
    let dropped = [Release(Slot(0)), Release(Slot(1))];
    assert_eq!(run(hp::NAL_IDR_W_RADL, false, false), out);
    assert_eq!(run(hp::NAL_IDR_N_LP, true, false), dropped);
    assert_eq!(run(hp::NAL_CRA_NUT, false, true), dropped);
}

#[test]
fn a_key_with_nothing_to_reorder_goes_out_at_once() {
    let mut d = Dpb::new();
    let (_, ev) = picture(&mut d, &SEQ, hp::NAL_IDR_N_LP, &intra(0), 0);
    assert_eq!(ev, [Output(Slot(0))]);
    let mut d = Dpb::new();
    let seq = Seq { reorder: 0, ..SEQ };
    let (_, ev) = picture(&mut d, &seq, hp::NAL_CRA_NUT, &intra(0), 0);
    assert_eq!(ev, [Output(Slot(0))]);
}

#[test]
fn after_a_discard_the_cra_goes_out_ahead_and_its_rasl_are_skipped() {
    let mut d = Dpb::new();
    picture(&mut d, &SEQ, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let mut ev = Events::new();
    d.discard(&mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(0))]);
    // nothing decodes before an IRAP
    let p = d.begin(&SEQ, &nal(hp::NAL_TRAIL_R), &intra(3), false, &mut ev).unwrap();
    assert_eq!(p.fate, Fate::Skip);
    let (p, ev) = picture(&mut d, &SEQ, hp::NAL_CRA_NUT, &intra(0), 5);
    assert_eq!((p.poc, p.fate), (0, Fate::Shown));
    assert_eq!(ev, [Output(Slot(5))], "the key is out before the next decode");
    // RASL reference pictures before the CRA that were never decoded
    let rasl = header(14, SliceType::B, &[(-4, true)], &[(2, true)], &[]);
    let mut ev = Events::new();
    let p = d.begin(&SEQ, &nal(hp::NAL_RASL_N), &rasl, true, &mut ev).unwrap();
    assert_eq!(p.fate, Fate::Skip);
    // a RADL decodes for references but leads the key already out
    let radl = header(15, SliceType::P, &[], &[(1, true)], &[]);
    let (p, ev) = picture(&mut d, &SEQ, hp::NAL_RADL_R, &radl, 6);
    assert_eq!((p.poc, p.fate), (-1, Fate::Hidden));
    assert_eq!(ev, []);
    // the RADL leaves the RPS unseen
    let t = header(4, SliceType::P, &[(-4, true)], &[], &[]);
    let (p, ev) = picture(&mut d, &SEQ, hp::NAL_TRAIL_R, &t, 7);
    assert_eq!(p.fate, Fate::Shown);
    assert_eq!(ev, [Release(Slot(6))]);
    assert_eq!(slots(&d), [5, 7]);
}

#[test]
fn a_full_dpb_refuses_the_picture_and_changes_nothing() {
    let refs: Vec<Ref> = (0..MAX_DPB as u16).map(|i| entry(i, i as i32)).collect();
    let mut d = with_refs(&refs);
    d.awaiting_irap = false;
    let mut ev = Events::new();
    let p = Picture {
        fate: Fate::Shown,
        poc: 99,
        gop: 0,
        irap: false,
        no_rasl_output: false,
        sets: CurrSets::default(),
        key_now: false,
    };
    assert_eq!(d.end(&p, Slot(99), &mut ev), Err(DpbError::Full));
    assert_eq!(d.refs().len(), MAX_DPB);
    assert_eq!(d.n_waiting, 0);
}

#[test]
fn the_va_decoders_list_cases_hold() {
    // its fill_ref_list over a temporary list of two, then of three with list_entry_l0
    let mut c = CurrSets::default();
    c.before[..2].copy_from_slice(&[3, 5]);
    c.n_before = 2;
    let mut out = [0u8; MAX_LIST];
    build_list(&c, 0, 4, None, &mut out);
    assert_eq!(&out[..5], &[3, 5, 3, 5, NO_REF]);
    c.before[..3].copy_from_slice(&[10, 20, 30]);
    c.n_before = 3;
    build_list(&c, 0, 3, Some(&[2, 0, 1]), &mut out);
    assert_eq!(&out[..3], &[30, 10, 20]);
}

#[test]
fn dropped_output_is_released_once_no_reference_holds_it() {
    let mut d = Dpb::new();
    let seq = Seq { reorder: 2, ..SEQ };
    picture(&mut d, &seq, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let p8 = header(8, SliceType::P, &[(-8, true)], &[], &[]);
    picture(&mut d, &seq, hp::NAL_TRAIL_R, &p8, 1);
    // both wait and both are references
    let mut ev = Events::new();
    d.drop_output(&mut ev);
    assert_eq!(ev.as_slice(), []);
    d.flush(&mut ev);
    assert_eq!(ev.as_slice(), []);
    // an RPS without them releases both, neither goes out
    let i = header(12, SliceType::I, &[], &[], &[]);
    let (_, ev) = picture(&mut d, &seq, hp::NAL_TRAIL_R, &i, 2);
    assert_eq!(ev, [Release(Slot(0)), Release(Slot(1))]);
}

#[test]
fn segment_headers_peek() {
    // first_slice 1, pps 0 (1)
    assert_eq!(peek_pps_id(&[0x02, 0x01, 0b1100_0000], false), Some(0));
    // first_slice 1, no_output_of_prior 0, pps 2 (011)
    assert_eq!(peek_pps_id(&[0x26, 0x01, 0b1001_1000], true), Some(2));
    assert_eq!(peek_pps_id(&[0x26, 0x01], true), None);
    assert!(peek_no_output_of_prior_pics(&[0x26, 0x01, 0b1100_0000]));
    assert!(!peek_no_output_of_prior_pics(&[0x26, 0x01, 0b1001_1000]));
    assert!(!peek_no_output_of_prior_pics(&[0x26, 0x01]));
    assert!(is_eos(hp::NAL_EOS) && is_eos(hp::NAL_EOB) && !is_eos(hp::NAL_AUD));
}

#[test]
fn bump_one_lets_the_smallest_waiting_picture_out_ahead_of_the_reorder() {
    let mut d = Dpb::new();
    let seq = Seq { reorder: 4, ..SEQ };
    picture(&mut d, &seq, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let p8 = header(8, SliceType::P, &[(-8, true)], &[], &[]);
    picture(&mut d, &seq, hp::NAL_TRAIL_R, &p8, 1);
    let b4 = header(4, SliceType::B, &[(-4, true)], &[(4, true)], &[]);
    picture(&mut d, &seq, hp::NAL_TRAIL_N, &b4, 2);
    let mut ev = Events::new();
    assert!(d.bump_one(&mut ev));
    assert_eq!(ev.as_slice(), [Output(Slot(0))]);
    // the b stays in the DPB until the next RPS drops it
    assert!(d.bump_one(&mut ev));
    assert_eq!(ev.as_slice(), [Output(Slot(2))]);
    assert!(d.bump_one(&mut ev));
    assert_eq!(ev.as_slice(), [Output(Slot(1))]);
    assert!(!d.bump_one(&mut ev));
    assert_eq!(ev.as_slice(), []);
}

#[test]
fn skips_names_what_begin_skips_before_the_headers() {
    let mut d = Dpb::new();
    assert!(d.skips(hp::NAL_TRAIL_R) && !d.skips(hp::NAL_CRA_NUT));
    picture(&mut d, &SEQ, hp::NAL_CRA_NUT, &intra(0), 0);
    assert!(d.skips(hp::NAL_RASL_N) && !d.skips(hp::NAL_TRAIL_R));
}

#[test]
fn a_long_term_entry_with_its_msb_names_one_cycle_apart() {
    // 17 and 1 share lsb 1, cur 20
    let mut d = with_refs(&[entry(0, 17), entry(1, 1), entry(2, 16)]);
    let mut sh = header(4, SliceType::B, &[(-4, true)], &[], &[]);
    sh.long_term_refs.push(hp::LongTermRef {
        poc_lsb: 1,
        used_by_curr_pic: true,
        delta_poc_msb_present: true,
        delta_poc_msb_cycle: 1,
    });
    assert!(d.mark(&sh, 20, 16));
    let sets: Vec<_> = d.refs().iter().map(|r| (r.set, r.long_term)).collect();
    assert_eq!(
        sets,
        [(RpsSet::None, false), (RpsSet::LtCurr, true), (RpsSet::StCurrBefore, false)]
    );
    assert_eq!(d.curr_sets(&sh, 20, 16).lt(), &[1]);
    // without the MSB the first picture with that lsb takes it
    let mut d = with_refs(&[entry(0, 17), entry(1, 1), entry(2, 16)]);
    sh.long_term_refs.clear();
    sh.long_term_refs.push(hp::LongTermRef {
        poc_lsb: 1,
        used_by_curr_pic: true,
        delta_poc_msb_present: false,
        delta_poc_msb_cycle: 0,
    });
    assert!(d.mark(&sh, 20, 16));
    assert_eq!(d.curr_sets(&sh, 20, 16).lt(), &[0]);
}

#[test]
fn a_picture_of_a_higher_temporal_layer_is_not_prev_tid0_pic() {
    let mut p = PocState::default();
    assert_eq!(p.compute(16, 0, hp::NAL_IDR_W_RADL, 0, true), 0);
    assert_eq!(p.compute(16, 7, hp::NAL_TRAIL_R, 1, false), 7);
    // from 7 this would be 15, prevTid0Pic is still the IDR
    assert_eq!(p.compute(16, 15, hp::NAL_TRAIL_R, 0, false), -1);
    let mut d = Dpb::new();
    picture(&mut d, &SEQ, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let mut ev = Events::new();
    let p7 = header(7, SliceType::P, &[(-7, true)], &[], &[]);
    let n = Nal { nal_type: hp::NAL_TRAIL_R, temporal_id: 1, start: 0, end: 0 };
    let p = d.begin(&SEQ, &n, &p7, true, &mut ev).unwrap();
    assert_eq!(p.poc, 7);
    d.end(&p, Slot(1), &mut ev).unwrap();
    let p15 = header(15, SliceType::P, &[], &[(1, true)], &[]);
    let p = d.begin(&SEQ, &nal(hp::NAL_TRAIL_R), &p15, true, &mut ev).unwrap();
    assert_eq!((p.poc, p.sets.after()), (-1, &[0u8][..]));
}

#[test]
fn rasl_pictures_of_a_cra_mid_stream_are_decoded_and_shown() {
    let mut d = Dpb::new();
    picture(&mut d, &SEQ, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let p4 = header(4, SliceType::P, &[(-4, true)], &[], &[]);
    picture(&mut d, &SEQ, hp::NAL_TRAIL_R, &p4, 1);
    let cra = header(8, SliceType::I, &[(-4, false)], &[], &[]);
    let (p, ev) = picture(&mut d, &SEQ, hp::NAL_CRA_NUT, &cra, 2);
    assert!(p.irap && !p.no_rasl_output);
    assert_eq!((p.fate, p.gop, ev), (Fate::Shown, 1, vec![Release(Slot(0)), Output(Slot(1))]));
    assert!(!d.skips(hp::NAL_RASL_N));
    let rasl = header(6, SliceType::B, &[(-2, true)], &[(2, true)], &[]);
    let (p, ev) = picture(&mut d, &SEQ, hp::NAL_RASL_N, &rasl, 3);
    assert_eq!((p.fate, p.poc), (Fate::Shown, 6));
    assert_eq!((p.sets.before(), p.sets.after()), (&[0u8][..], &[1u8][..]));
    assert_eq!(ev, [Output(Slot(3))]);
}

#[test]
fn after_a_discard_an_idr_or_bla_with_radl_goes_out_ahead_and_hides_them() {
    for t in [hp::NAL_IDR_W_RADL, hp::NAL_BLA_W_RADL] {
        let mut d = Dpb::new();
        picture(&mut d, &SEQ, hp::NAL_IDR_W_RADL, &intra(0), 0);
        let mut ev = Events::new();
        d.discard(&mut ev);
        let (p, ev) = picture(&mut d, &SEQ, t, &intra(0), 5);
        assert_eq!((p.fate, ev), (Fate::Shown, vec![Output(Slot(5))]), "type {t}");
        let radl = header(15, SliceType::P, &[], &[(1, true)], &[]);
        let (p, ev) = picture(&mut d, &SEQ, hp::NAL_RADL_R, &radl, 6);
        assert_eq!((p.poc, p.fate, ev), (-1, Fate::Hidden, vec![]), "type {t}");
    }
}

#[test]
fn a_picture_not_for_output_leaves_the_rps_without_going_out() {
    let mut d = Dpb::new();
    picture(&mut d, &SEQ, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let mut p4 = header(4, SliceType::P, &[(-4, true)], &[], &[]);
    p4.pic_output_flag = false;
    let (p, ev) = picture(&mut d, &SEQ, hp::NAL_TRAIL_R, &p4, 1);
    assert_eq!((p.fate, ev), (Fate::Hidden, vec![]));
    assert_eq!(slots(&d), [0, 1]);
    let p8 = header(8, SliceType::P, &[(-8, true)], &[], &[]);
    let (_, ev) = picture(&mut d, &SEQ, hp::NAL_TRAIL_R, &p8, 2);
    assert_eq!(ev, [Release(Slot(1)), Output(Slot(0))]);
    assert_eq!(slots(&d), [0, 2]);
}

#[test]
fn an_abandoned_picture_is_missing_and_an_abandoned_key_waits_for_an_irap() {
    use SliceType::P;
    let mut d = Dpb::new();
    picture(&mut d, &SEQ, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let mut ev = Events::new();
    let p4 = header(4, P, &[(-4, true)], &[], &[]);
    let p = d.begin(&SEQ, &nal(hp::NAL_TRAIL_R), &p4, true, &mut ev).unwrap();
    d.abandon(&p, &mut ev);
    assert_eq!(ev.as_slice(), []);
    assert_eq!(slots(&d), [0]);
    let p8 = header(8, P, &[(-4, true)], &[], &[]);
    let r = d.begin(&SEQ, &nal(hp::NAL_TRAIL_R), &p8, true, &mut ev);
    assert_eq!(r, Err(DpbError::MissingReference));
    assert!(!d.skips(hp::NAL_TRAIL_R));
    // a CRA mid-stream abandoned leaves decoding on
    let cra = d.begin(&SEQ, &nal(hp::NAL_CRA_NUT), &intra(12), false, &mut ev).unwrap();
    assert!(!cra.no_rasl_output);
    d.abandon(&cra, &mut ev);
    assert!(!d.skips(hp::NAL_TRAIL_R));
    // an IDR abandoned waits for an IRAP again
    let idr = d.begin(&SEQ, &nal(hp::NAL_IDR_W_RADL), &intra(0), false, &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Output(Slot(0)), Release(Slot(0))]);
    d.abandon(&idr, &mut ev);
    assert_eq!(ev.as_slice(), []);
    assert!(d.skips(hp::NAL_TRAIL_R) && !d.skips(hp::NAL_IDR_N_LP));
    let (p, ev) = picture(&mut d, &SEQ, hp::NAL_IDR_N_LP, &intra(0), 3);
    assert_eq!((p.fate, ev), (Fate::Shown, vec![Output(Slot(3))]));
}

/// BUMPING_A_ericsson, C.5.2.2 bumps before a picture when the DPB holds its dpb size.
#[test]
fn a_dpb_at_its_size_lets_the_smallest_out_before_the_next_picture() {
    let seq = Seq { reorder: 4, dpb_size: 3, ..SEQ };
    let mut d = Dpb::new();
    picture(&mut d, &seq, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let p8 = header(8, SliceType::P, &[(-8, true)], &[], &[]);
    picture(&mut d, &seq, hp::NAL_TRAIL_R, &p8, 1);
    let b4 = header(4, SliceType::B, &[(-4, true)], &[(4, true)], &[]);
    let (_, ev) = picture(&mut d, &seq, hp::NAL_TRAIL_N, &b4, 2);
    assert_eq!(ev, [], "three wait, within the reorder depth");
    // 8 stays a reference, 0 and 4 wait alone, three stored is the dpb size
    let p16 = header(16, SliceType::P, &[(-8, true)], &[], &[]);
    let mut ev = Events::new();
    let p = d.begin(&seq, &nal(hp::NAL_TRAIL_R), &p16, true, &mut ev).unwrap();
    assert_eq!(ev.as_slice(), [Output(Slot(0)), Release(Slot(0))]);
    d.end(&p, Slot(3), &mut ev).unwrap();
    assert_eq!(ev.as_slice(), []);
    // the same stream with room to spare keeps all four waiting
    let seq = Seq { reorder: 4, ..SEQ };
    let mut d = Dpb::new();
    for (t, sh, slot) in [
        (hp::NAL_IDR_W_RADL, intra(0), 0),
        (hp::NAL_TRAIL_R, p8, 1),
        (hp::NAL_TRAIL_N, b4, 2),
        (hp::NAL_TRAIL_R, p16, 3),
    ] {
        assert_eq!(picture(&mut d, &seq, t, &sh, slot).1, []);
    }
}

/// C.5.2.3, a picture waiting while SpsMaxLatencyPictures pictures ahead of it in output order
/// were decoded goes out, with every smaller one before it.
#[test]
fn a_picture_past_its_latency_goes_out_ahead_of_the_reorder_depth() {
    let seq = Seq { reorder: 4, max_latency: Some(2), ..SEQ };
    let mut d = Dpb::new();
    picture(&mut d, &seq, hp::NAL_IDR_W_RADL, &intra(0), 0);
    let p8 = header(8, SliceType::P, &[(-8, true)], &[], &[]);
    let (_, ev) = picture(&mut d, &seq, hp::NAL_TRAIL_R, &p8, 1);
    assert_eq!(ev, [], "8 follows nothing decoded after it");
    let b4 = header(4, SliceType::B, &[(-4, true)], &[(4, true)], &[]);
    let (_, ev) = picture(&mut d, &seq, hp::NAL_TRAIL_R, &b4, 2);
    assert_eq!(ev, [], "8 waited one picture");
    let b2 = header(2, SliceType::B, &[(-2, true)], &[(2, true), (6, true)], &[]);
    let (_, ev) = picture(&mut d, &seq, hp::NAL_TRAIL_N, &b2, 3);
    // 8 waited two, 4 one, out go 0, 2, 4 and 8 in order
    assert_eq!(ev, [Output(Slot(0)), Output(Slot(3)), Output(Slot(2)), Output(Slot(1))]);
    // a picture later in output order adds nothing to those before it
    let mut d = Dpb::new();
    picture(&mut d, &seq, hp::NAL_IDR_W_RADL, &intra(0), 0);
    for (i, lsb) in [2u32, 4, 6].into_iter().enumerate() {
        let p = header(lsb, SliceType::P, &[(-2, true)], &[], &[]);
        let (_, ev) = picture(&mut d, &seq, hp::NAL_TRAIL_R, &p, i as u16 + 1);
        assert_eq!(ev, [], "{lsb}");
    }
}

/// NoOutPrior_A_Qualcomm and RAP_B_Bossen, an end of sequence puts nothing out itself, the CRA
/// after it drops what still waits and an IDR keeps its flag's choice.
#[test]
fn after_an_end_of_sequence_the_next_irap_decides_the_waiting_pictures() {
    let seq = Seq { reorder: 2, ..SEQ };
    for (t, no_output, out) in [
        (hp::NAL_CRA_NUT, false, false),
        (hp::NAL_IDR_W_RADL, false, true),
        (hp::NAL_IDR_N_LP, true, false),
    ] {
        let mut d = Dpb::new();
        picture(&mut d, &seq, hp::NAL_IDR_W_RADL, &intra(0), 0);
        let p8 = header(8, SliceType::P, &[(-8, true)], &[], &[]);
        picture(&mut d, &seq, hp::NAL_TRAIL_R, &p8, 1);
        d.end_of_sequence();
        let mut key = intra(0);
        key.no_output_of_prior_pics_flag = no_output;
        let (p, ev) = picture(&mut d, &seq, t, &key, 2);
        assert!(p.no_rasl_output, "type {t}");
        let prior = match out {
            true => vec![Output(Slot(0)), Output(Slot(1)), Release(Slot(0)), Release(Slot(1))],
            false => vec![Release(Slot(0)), Release(Slot(1))],
        };
        assert_eq!(ev[..prior.len()], prior[..], "type {t}");
    }
}
