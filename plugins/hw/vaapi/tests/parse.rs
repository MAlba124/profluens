//! Pure H.264 bitstream-parse tests, no hardware.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_vaapi::h264parse::{self, BitReader, PocState, SliceType, parse_pps, parse_sps, split_nals};

struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    nbits: u32,
}
impl BitWriter {
    fn new() -> Self {
        BitWriter { bytes: Vec::new(), cur: 0, nbits: 0 }
    }
    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b as u8 & 1);
        self.nbits += 1;
        if self.nbits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }
    fn u(&mut self, val: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((val >> i) & 1);
        }
    }
    fn ue(&mut self, val: u32) {
        let code = val + 1;
        let len = 32 - code.leading_zeros();
        for _ in 0..(len - 1) {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit((code >> i) & 1);
        }
    }
    fn se(&mut self, val: i32) {
        let code = if val <= 0 { (-2 * val) as u32 } else { (2 * val - 1) as u32 };
        self.ue(code);
    }
    fn flag(&mut self, b: bool) {
        self.bit(b as u32);
    }
    fn finish_nal(mut self, nal_ref_idc: u8, nal_unit_type: u8) -> Vec<u8> {
        self.bit(1); // rbsp_stop_one_bit
        while self.nbits != 0 {
            self.bit(0);
        }
        let header = ((nal_ref_idc & 3) << 5) | (nal_unit_type & 0x1f);
        let mut out = vec![header];
        out.extend_from_slice(&self.bytes);
        out
    }
}

#[test]
fn exp_golomb_ue_round_trips() {
    for v in [0u32, 1, 2, 3, 7, 8, 15, 16, 100, 255, 1000] {
        let mut w = BitWriter::new();
        w.ue(v);
        w.bit(1); // stop
        while w.nbits != 0 {
            w.bit(0);
        }
        let mut r = BitReader::new(&w.bytes);
        assert_eq!(r.ue(), v, "ue round-trip for {v}");
    }
}

#[test]
fn exp_golomb_se_round_trips() {
    for v in [0i32, 1, -1, 2, -2, 3, -3, 10, -10, 128, -128] {
        let mut w = BitWriter::new();
        w.se(v);
        w.bit(1);
        while w.nbits != 0 {
            w.bit(0);
        }
        let mut r = BitReader::new(&w.bytes);
        assert_eq!(r.se(), v, "se round-trip for {v}");
    }
}

#[test]
fn nal_split_three_and_four_byte_start_codes() {
    let stream = [
        0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, // SPS (type 7)
        0x00, 0x00, 0x01, 0x68, 0xCC, // PPS (type 8)
    ];
    let nals = split_nals(&stream);
    assert_eq!(nals.len(), 2);
    assert_eq!(nals[0].unit_type, 7);
    assert_eq!(nals[0].ref_idc, 3); // 0x67 >> 5 = 3
    assert_eq!(nals[1].unit_type, 8);
    assert_eq!(nals[0].raw(&stream), &[0x67, 0xAA, 0xBB]);
    assert_eq!(nals[1].raw(&stream), &[0x68, 0xCC]);
}

#[test]
fn nal_split_preserves_emulation_prevention_bytes() {
    let stream = [0x00, 0x00, 0x01, 0x41, 0x00, 0x00, 0x03, 0x00, 0x99];
    let nals = split_nals(&stream);
    assert_eq!(nals.len(), 1);
    // The raw NAL keeps the 0x03, VA wants the original bitstream.
    assert_eq!(nals[0].raw(&stream), &[0x41, 0x00, 0x00, 0x03, 0x00, 0x99]);
}

fn build_sps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(66, 8); // profile_idc = 66 (Baseline, not high-profile family)
    w.u(0, 8); // constraint flags
    w.u(30, 8); // level_idc
    w.ue(0); // seq_parameter_set_id
    w.ue(0); // log2_max_frame_num_minus4 -> MaxFrameNum 16
    w.ue(0); // pic_order_cnt_type = 0
    w.ue(4); // log2_max_pic_order_cnt_lsb_minus4 -> MaxPocLsb 256
    w.ue(1); // max_num_ref_frames
    w.flag(false); // gaps_in_frame_num_value_allowed
    w.ue(10); // pic_width_in_mbs_minus1 -> 11 MBs = 176 px
    w.ue(8); // pic_height_in_map_units_minus1 -> 9 MUs
    w.flag(true); // frame_mbs_only_flag -> height 9*16 = 144
    w.flag(true); // direct_8x8_inference_flag
    w.flag(false); // frame_cropping_flag
    w.flag(false); // vui_parameters_present_flag
    w.finish_nal(3, 7)
}

#[test]
fn sps_parse_dimensions_and_poc_type() {
    let nal = build_sps();
    let sps = parse_sps(&nal).expect("SPS parses");
    assert_eq!(sps.seq_parameter_set_id, 0);
    assert_eq!(sps.pic_order_cnt_type, 0);
    assert_eq!(sps.log2_max_frame_num_minus4, 0);
    assert_eq!(sps.max_frame_num(), 16);
    assert_eq!(sps.max_poc_lsb(), 256);
    assert_eq!(sps.width(), 176);
    assert_eq!(sps.height(), 144);
    assert_eq!(sps.width_in_mbs(), 11);
    assert_eq!(sps.height_in_mbs(), 9);
    assert!(sps.frame_mbs_only_flag);
    assert_eq!(sps.chroma_format_idc, 1);
    assert_eq!(sps.max_num_ref_frames, 1);
}

fn build_pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.flag(false); // entropy_coding_mode_flag (CAVLC)
    w.flag(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.flag(false); // weighted_pred_flag
    w.u(0, 2); // weighted_bipred_idc
    w.se(0); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.flag(false); // deblocking_filter_control_present_flag
    w.flag(false); // constrained_intra_pred_flag
    w.flag(false); // redundant_pic_cnt_present_flag
    w.finish_nal(3, 8)
}

#[test]
fn pps_parse_basic() {
    let nal = build_pps();
    let pps = parse_pps(&nal).expect("PPS parses");
    assert_eq!(pps.pic_parameter_set_id, 0);
    assert_eq!(pps.seq_parameter_set_id, 0);
    assert!(!pps.entropy_coding_mode_flag);
    assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 0);
    assert_eq!(pps.pic_init_qp_minus26, 0);
}

#[test]
fn slice_header_parse_idr() {
    let sps = parse_sps(&build_sps()).unwrap();
    let pps = parse_pps(&build_pps()).unwrap();
    let mut w = BitWriter::new();
    w.ue(0); // first_mb_in_slice
    w.ue(7); // slice_type (I)
    w.ue(0); // pps_id
    w.u(0, 4); // frame_num (log2_max_frame_num_minus4+4 = 4 bits)
    w.ue(0); // idr_pic_id
    w.u(0, 8); // pic_order_cnt_lsb (8 bits)
    w.flag(false);
    w.flag(false);
    w.se(0); // slice_qp_delta
    let nal = w.finish_nal(3, 5); // nal_unit_type 5 = IDR
    let sh = h264parse::parse_slice_header(&nal, 5, 3, &sps, &pps).expect("slice header");
    assert_eq!(sh.slice_type, SliceType::I);
    assert!(sh.is_idr);
    assert_eq!(sh.frame_num, 0);
    assert_eq!(sh.pic_order_cnt_lsb, 0);
    assert_eq!(sh.first_mb_in_slice, 0);
}

#[test]
fn poc_type0_ascending_sequence() {
    let sps = parse_sps(&build_sps()).unwrap();
    let pps = parse_pps(&build_pps()).unwrap();
    let mut poc = PocState::new();

    let idr = make_slice(&sps, &pps, 5, 3, 0, 0, SliceType::I);
    let (t, b) = poc.compute(&sps, &idr, true, 3);
    assert_eq!(t.min(b), 0);

    for (frame_num, lsb, expect) in [(1u32, 2u32, 2i32), (2, 4, 4), (3, 6, 6)] {
        let sh = make_slice(&sps, &pps, 1, 2, frame_num, lsb, SliceType::P);
        let (t, b) = poc.compute(&sps, &sh, false, 2);
        assert_eq!(t.min(b), expect, "POC for lsb {lsb}");
    }
}

#[test]
fn poc_type0_msb_wraps() {
    let sps = parse_sps(&build_sps()).unwrap(); // MaxPocLsb = 256
    let pps = parse_pps(&build_pps()).unwrap();
    let mut poc = PocState::new();
    let idr = make_slice(&sps, &pps, 5, 3, 0, 0, SliceType::I);
    assert_eq!(poc.compute(&sps, &idr, true, 3).0, 0);
    let p1 = make_slice(&sps, &pps, 1, 2, 1, 100, SliceType::P);
    assert_eq!(poc.compute(&sps, &p1, false, 2).0, 100);
    let p2 = make_slice(&sps, &pps, 2, 2, 2, 200, SliceType::P);
    assert_eq!(poc.compute(&sps, &p2, false, 2).0, 200);
    // A drop of 190 >= MaxPocLsb/2 is a forward MSB wrap, POC = 256 + 10.
    let p3 = make_slice(&sps, &pps, 3, 2, 3, 10, SliceType::P);
    let (t1, b1) = poc.compute(&sps, &p3, false, 2);
    assert_eq!(t1.min(b1), 266);
}

fn make_slice(
    sps: &h264parse::Sps,
    _pps: &h264parse::Pps,
    unit_type: u8,
    ref_idc: u8,
    frame_num: u32,
    poc_lsb: u32,
    st: SliceType,
) -> h264parse::SliceHeader {
    let mut w = BitWriter::new();
    w.ue(0); // first_mb
    w.ue(slice_type_code(st));
    w.ue(0); // pps_id
    w.u(frame_num, sps.log2_max_frame_num_minus4 + 4);
    if unit_type == 5 {
        w.ue(0); // idr_pic_id
    }
    w.u(poc_lsb, sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
    if st == SliceType::P {
        w.flag(false); // num_ref_idx_active_override_flag
        w.flag(false); // ref_pic_list_modification_flag_l0
    }
    if ref_idc != 0 {
        if unit_type == 5 {
            w.flag(false);
            w.flag(false);
        } else {
            w.flag(false); // adaptive_ref_pic_marking_mode_flag
        }
    }
    w.se(0); // slice_qp_delta
    let nal = w.finish_nal(ref_idc, unit_type);
    h264parse::parse_slice_header(&nal, unit_type, ref_idc, sps, _pps).expect("slice header parses")
}

fn slice_type_code(st: SliceType) -> u32 {
    match st {
        SliceType::P => 0,
        SliceType::B => 1,
        SliceType::I => 2,
        SliceType::Sp => 3,
        SliceType::Si => 4,
    }
}

fn unit(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter().flat_map(|n| [0, 0, 0, 1].into_iter().chain(n.iter().copied())).collect()
}

#[test]
fn h264_unit_of_nal_ref_idc_zero_slices_is_non_reference() {
    // nal_ref_idc is bits 5 and 6 of the header.
    let non_reference = |nals: &[&[u8]]| h264parse::non_reference(&split_nals(&unit(nals)));
    assert!(non_reference(&[&[0x09, 0xF0], &[0x06, 0x05], &[0x01, 0x9A], &[0x01, 0x9A]]));
    assert!(!non_reference(&[&[0x41, 0x9A]]), "a reference slice");
    assert!(!non_reference(&[&[0x01, 0x9A], &[0x41, 0x9A]]), "one reference slice among them");
    assert!(!non_reference(&[&[0x65, 0x88]]), "an IDR slice");
    assert!(!non_reference(&[&[0x68, 0xCE], &[0x01, 0x9A]]), "a parameter set");
    assert!(!non_reference(&[&[0x02, 0x9A]]), "a data partition");
    assert!(!non_reference(&[&[0x09, 0xF0]]), "no slice at all");
    assert!(!non_reference(&[]));
}

#[test]
fn h265_unit_of_n_types_is_non_reference_only_in_the_top_sub_layer() {
    use pf_vaapi::h265parse;
    let nal = |kind: u8, tid: u8| vec![kind << 1, tid + 1, 0x80];
    let non_reference = |nals: &[Vec<u8>], top: u8| {
        let borrowed: Vec<&[u8]> = nals.iter().map(Vec::as_slice).collect();
        h265parse::non_reference(&h265parse::split_nals(&unit(&borrowed)), top)
    };
    for kind in [0, 2, 4, 6, 8, 10, 12, 14] {
        assert!(non_reference(&[nal(35, 0), nal(kind, 0)], 0), "type {kind}");
        assert!(non_reference(&[nal(kind, 2)], 2), "type {kind} in the top of three");
        assert!(
            !non_reference(&[nal(kind, 0)], 1),
            "type {kind}, a higher sub-layer may refer to it"
        );
    }
    for kind in [1, 9, 16, 19, 21, 32, 33, 34, 36, 37] {
        assert!(!non_reference(&[nal(kind, 0), nal(0, 0)], 0), "type {kind}");
    }
    assert!(!non_reference(&[nal(35, 0), nal(39, 0)], 0), "no slice at all");
}
