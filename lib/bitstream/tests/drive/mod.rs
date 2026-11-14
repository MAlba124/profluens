// Test and fuzz driver, the model allocates.
#![allow(clippy::disallowed_methods, dead_code)]

use pf_bitstream::av1::parse::obu::OBU_FRAME_HEADER;
use pf_bitstream::av1::{self, write};
use pf_bitstream::h264::{self, MAX_LIST as LIST4, NO_REF as NO_REF4};
use pf_bitstream::h265::{self, MAX_LIST as LIST5, NO_REF as NO_REF5, Seq};
use pf_bitstream::vp9::{self, parse::FrameType as Vp9Type};
use pf_bitstream::{DpbError, Event, Events, Fate, Slot};
use profluens_video::fixed::FixedList;
use profluens_video::h264parse::{self as hp4, MmcoOp, RefListMod, Sps};
use profluens_video::h265parse::{self as hp5, LongTermRef};

/// Slots the driver hands out, past what any DPB can hold at once.
pub const POOL: usize = 48;

/// Whether to narrate each step, PF_FUZZ_SAY in the environment as the fuzz targets take it.
fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}

const SPS_1080P: [u8; 27] = [
    0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03,
    0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
];

/// Bytes a run reads, a fuzz input or a random stream.
pub struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bytes { data, at: 0 }
    }

    fn next(&mut self) -> Option<u8> {
        let b = *self.data.get(self.at)?;
        self.at += 1;
        Some(b)
    }

    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut out = [0; N];
        for o in &mut out {
            *o = self.next()?;
        }
        Some(out)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Held {
    Free,
    /// handed to a decoded picture, `out` once its output came
    Live {
        shown: bool,
        out: bool,
    },
}

/// The caller's side of the event contract. A slot goes to one picture at a time, is put out
/// at most once while it is live and only when shown, and is released exactly once.
pub struct Pool {
    slots: [Held; POOL],
    pub outputs: usize,
    pub releases: usize,
}

impl Pool {
    pub fn new() -> Self {
        Pool { slots: [Held::Free; POOL], outputs: 0, releases: 0 }
    }

    /// A free slot, none is a leak.
    pub fn take(&mut self) -> Slot {
        let i = self.slots.iter().position(|&h| h == Held::Free).expect("every slot is live");
        Slot(i as u16)
    }

    pub fn give(&mut self, slot: Slot, shown: bool) {
        let h = &mut self.slots[slot.0 as usize];
        assert_eq!(*h, Held::Free, "slot {} handed out twice", slot.0);
        *h = Held::Live { shown, out: false };
    }

    pub fn free(&mut self, slot: Slot) {
        self.slots[slot.0 as usize] = Held::Free;
    }

    pub fn live(&self) -> usize {
        self.slots.iter().filter(|&&h| h != Held::Free).count()
    }

    pub fn apply(&mut self, ev: &Events) {
        self.apply_as(ev, false);
    }

    /// As `apply`, a slot may go out again while live as AV1 and VP9 show a frame again.
    pub fn apply_again(&mut self, ev: &Events) {
        self.apply_as(ev, true);
    }

    fn apply_as(&mut self, ev: &Events, again: bool) {
        assert!(!ev.overflowed(), "events past the bound");
        for e in ev.as_slice() {
            match *e {
                Event::Output(s) => {
                    match &mut self.slots[s.0 as usize] {
                        Held::Live { .. } if again => {}
                        Held::Live { shown: true, out } if !*out => *out = true,
                        h => panic!("output of slot {} {h:?}", s.0),
                    }
                    self.outputs += 1;
                }
                Event::Release(s) => {
                    assert_ne!(self.slots[s.0 as usize], Held::Free, "slot {} released twice", s.0);
                    self.slots[s.0 as usize] = Held::Free;
                    self.releases += 1;
                }
            }
        }
    }

    /// Every shown live slot has gone out, true after a flush.
    pub fn all_out(&self) -> bool {
        self.slots.iter().all(|h| !matches!(h, Held::Live { shown: true, out: false }))
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

/// What a run did, for the tests to see it covered what it should.
#[derive(Default, Debug)]
pub struct Tally {
    pub pictures: usize,
    pub skipped: usize,
    pub abandoned: usize,
    pub errors: usize,
    pub discards: usize,
    /// outputs whose order was checked
    pub ordered: usize,
}

/// Output order of a run. In a sequence (gop) that keeps within `reorder`, no picture preceded
/// in decode order by more than that many of the sequence that follow it in output order, the
/// pictures the DPB puts out on its own go out in non-decreasing (gop, POC). Outputs a caller
/// forces, `bump_one` and a flush mid stream, are not checked, nor those of a sequence past
/// the bound. The count runs over every picture `begin` gave `Fate::Shown`, the DPB orders by
/// those before their decode.
pub struct Order {
    reorder: usize,
    keys: [(u32, i32); POOL],
    shown: Vec<(u32, i32)>,
    last: Option<(u32, i32)>,
    /// the last sequence past the bound, its outputs and those before it are not checked
    beyond: Option<u32>,
    checked: usize,
}

impl Order {
    pub fn new(reorder: usize) -> Self {
        Order {
            reorder,
            keys: [(0, 0); POOL],
            shown: Vec::new(),
            last: None,
            beyond: None,
            checked: 0,
        }
    }

    /// A picture `begin` gave `Fate::Shown`.
    fn shown(&mut self, key: (u32, i32)) {
        self.shown.retain(|k| k.0 == key.0);
        if self.shown.iter().filter(|k| k.1 > key.1).count() > self.reorder {
            if say() {
                eprintln!("  beyond {key:?} reorder {} shown {:?}", self.reorder, self.shown);
            }
            self.beyond(key.0);
        }
        self.shown.push(key);
    }

    /// Sequence `gop` is past what the order check holds for.
    fn beyond(&mut self, gop: u32) {
        self.beyond = self.beyond.max(Some(gop));
    }

    fn give(&mut self, slot: Slot, key: (u32, i32)) {
        self.keys[slot.0 as usize] = key;
    }

    /// The outputs of `ev` the DPB chose, before the pool applies it.
    fn check(&mut self, ev: &Events) {
        for s in outs(ev) {
            let k = self.keys[s.0 as usize];
            if self.beyond.is_some_and(|g| k.0 <= g) {
                continue;
            }
            if let Some(last) = self.last {
                assert!(last <= k, "(gop, POC) {k:?} out after {last:?}");
            }
            self.last = Some(k);
            self.checked += 1;
        }
    }
}

/// Order counts of a stream within `reorder`, groups of an anchor and then the pictures before
/// it in output order, falling or rising. The last of a falling group follows the anchor and
/// the rest of its group, `reorder` pictures at most.
struct Conform {
    reorder: usize,
    step: i32,
    base: i32,
    queue: [i32; 16],
    n: usize,
}

impl Conform {
    fn new(reorder: usize, step: i32) -> Self {
        Conform { reorder: reorder.min(16), step, base: 0, queue: [0; 16], n: 0 }
    }

    fn next(&mut self, b: u8) -> i32 {
        if self.n > 0 {
            self.n -= 1;
            return self.queue[self.n];
        }
        let g = 1 + usize::from(b) % (self.reorder + 1);
        let falling = b & 0x80 != 0;
        self.n = g - 1;
        for i in 0..self.n {
            let k = if falling { i + 1 } else { self.n - i };
            self.queue[i] = self.base + self.step * k as i32;
        }
        self.base += self.step * g as i32;
        self.base
    }

    /// Moves what is still to come by `d`, as the order count restarts.
    fn shift(&mut self, d: i32) {
        self.base += d;
        for q in &mut self.queue[..self.n] {
            *q += d;
        }
    }
}

fn h264_sps(poc_type: u32, refs: u32, reorder: u32) -> Sps {
    let mut s = hp4::parse_sps(&SPS_1080P).expect("the SPS parses");
    s.pic_order_cnt_type = poc_type;
    s.log2_max_pic_order_cnt_lsb_minus4 = 4;
    s.max_num_ref_frames = refs;
    s.max_num_reorder_frames = Some(reorder);
    s.max_dec_frame_buffering = Some(16);
    s
}

fn h264_header(ty: hp4::SliceType, frame_num: u32, lsb: u32) -> hp4::SliceHeader {
    hp4::SliceHeader {
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

/// Drives an H.264 DPB with pictures made of 4 bytes each, the first byte of the run picking
/// the SPS, and checks the event contract throughout. Ends with a flush and a discard, after
/// which no slot is live.
pub fn h264(bytes: &mut Bytes) -> Tally {
    h264_as(bytes, false)
}

/// As `h264` with order counts that keep within the reorder depth, `Order` checks most outputs.
pub fn h264_conform(bytes: &mut Bytes) -> Tally {
    h264_as(bytes, true)
}

fn h264_as(bytes: &mut Bytes, conform: bool) -> Tally {
    use hp4::SliceType::{B, I, P};
    let mut t = Tally::default();
    let Some(cfg) = bytes.next() else {
        return t;
    };
    let sps = h264_sps(u32::from(cfg & 3) % 3, u32::from(cfg >> 2 & 15) + 1, u32::from(cfg >> 6));
    let no_reorder = sps.max_num_reorder_frames == Some(0) || sps.pic_order_cnt_type == 2;
    let mut order = Order::new(if no_reorder { 0 } else { sps.reorder_frames() as usize });
    let mut counts = conform.then(|| Conform::new(order.reorder, 2));
    let mut d = h264::Dpb::new();
    let mut pool = Pool::new();
    let mut ev = Events::new();
    let (mut prev_ref_frame_num, mut prev_lsb) = (0u32, 0u32);
    let (mut l0, mut l1) = ([0u8; LIST4], [0u8; LIST4]);
    while let Some([b0, b1, b2, b3]) = bytes.take::<4>() {
        if b0 & 8 != 0 {
            match b1 & 3 {
                2 => d.bump_one(&mut ev),
                3 => {
                    d.flush(&mut ev);
                    true
                }
                _ => {
                    d.discard(&mut ev);
                    prev_ref_frame_num = 0;
                    t.discards += 1;
                    true
                }
            };
            pool.apply(&ev);
        }
        let (idr, ty) = match b0 & 3 {
            0 => (true, I),
            1 => (false, I),
            2 => (false, P),
            _ => (false, B),
        };
        let reference = idr || b0 & 4 != 0;
        let gap = if b0 & 32 != 0 { u32::from(b1 & 3) + 1 } else { 0 };
        let frame_num = if idr {
            0
        } else if reference {
            (prev_ref_frame_num + 1 + gap) % 16
        } else {
            (prev_ref_frame_num + 1) % 16
        };
        let want = counts.as_mut().map(|g| g.next(b1));
        let lsb = match want {
            Some(p) => p.rem_euclid(256) as u32,
            None if idr => u32::from(b1),
            None => (prev_lsb as i32 + i32::from(b1 as i8) / 2).rem_euclid(256) as u32,
        };
        let mut sh = h264_header(ty, frame_num, lsb);
        sh.is_idr = idr;
        // with type 1 and no offsets the order count is delta_pic_order_cnt[0]
        (sh.delta_pic_order_cnt_bottom, sh.delta_pic_order_cnt) = match want {
            Some(p) => (0, [p, 0]),
            None => (i32::from(b2 as i8 >> 4), [i32::from(b2 & 7) - 3, 0]),
        };
        if idr {
            sh.no_output_of_prior_pics_flag = b2 & 1 != 0;
            sh.long_term_reference_flag = b2 & 2 != 0;
        } else if reference && b0 & 64 != 0 {
            sh.adaptive_ref_pic_marking_mode_flag = true;
            let op = u32::from(b2 % 7);
            if op != 0 {
                sh.mmco_ops.push(MmcoOp {
                    op,
                    arg1: u32::from(b3 & 15),
                    arg2: u32::from(b3 >> 4 & 3),
                });
            }
            if b3 & 64 != 0 {
                let op = u32::from(b3 >> 7) + 1;
                sh.mmco_ops.push(MmcoOp { op, arg1: u32::from(b2 >> 4), arg2: 0 });
            }
        }
        sh.num_ref_idx_l0_active_minus1 = u32::from(b3 & 31);
        sh.num_ref_idx_l1_active_minus1 = u32::from(b3 >> 5);
        if b2 & 8 != 0 {
            sh.ref_list_mods_l0
                .push(RefListMod { op: u32::from(b3 % 3), value: u32::from(b3 >> 2) });
        }
        if b2 & 16 != 0 {
            sh.ref_list_mods_l1
                .push(RefListMod { op: u32::from(b2 % 3), value: u32::from(b2 >> 2) });
        }
        let nal = hp4::Nal {
            ref_idc: reference as u8,
            unit_type: if idr { hp4::NAL_SLICE_IDR } else { hp4::NAL_SLICE_NON_IDR },
            start: 0,
            end: 0,
        };
        if b0 & 128 != 0 {
            d.ahead(&sps, &nal, &sh, &mut ev);
            order.check(&ev);
            pool.apply(&ev);
        }
        t.pictures += 1;
        let intra = ty == I;
        if say() {
            eprintln!(
                "h264 {b0:08b} {b1} {b2} {b3} idr {idr} ref {reference} fn {frame_num} lsb {lsb} mmco {:?}",
                sh.mmco_ops
            );
        }
        let pic = match d.begin(&sps, &nal, &sh, intra, &mut ev) {
            Ok(p) => {
                if say() {
                    let refs: Vec<_> = d.refs().iter().map(|r| (r.slot.0, r.frame_num)).collect();
                    eprintln!("  begin {:?} {:?} refs {refs:?}", p.fate, ev.as_slice());
                }
                order.check(&ev);
                pool.apply(&ev);
                if p.fate == Fate::Shown {
                    order.shown((p.gop, p.poc));
                }
                p
            }
            Err(e) => {
                pool.apply(&ev);
                assert_eq!(e, DpbError::Fields);
                t.errors += 1;
                continue;
            }
        };
        if pic.fate == Fate::Skip {
            t.skipped += 1;
            continue;
        }
        prev_lsb = lsb;
        check_lists(d.refs().len(), &pic, &sh, &d, &mut l0, &mut l1);
        for r in d.refs() {
            assert_ne!(pool.slots[r.slot.0 as usize], Held::Free, "a reference in a free slot");
        }
        if b0 & 16 != 0 {
            d.abandon(&pic, &sh, &mut ev);
            pool.apply(&ev);
            t.abandoned += 1;
            continue;
        }
        let mut pic = pic;
        if b3 & 128 != 0 && pic.fate == Fate::Shown {
            pic.fate = Fate::Hidden;
        }
        let slot = pool.take();
        pool.give(slot, pic.fate == Fate::Shown);
        order.give(slot, (pic.gop, pic.poc));
        match d.end(&pic, slot, &sh, &mut ev) {
            Ok(()) => {
                if say() {
                    let refs: Vec<_> = d
                        .refs()
                        .iter()
                        .map(|r| (r.slot.0, r.frame_num, r.long_term, r.non_existing))
                        .collect();
                    eprintln!("  end slot {} {:?} refs {refs:?}", slot.0, ev.as_slice());
                }
                order.check(&ev);
                pool.apply(&ev);
                if reference {
                    prev_ref_frame_num = if pic.mmco5 { 0 } else { frame_num };
                }
                // the order count restarts at the MMCO 5 picture
                if let (Some(g), Some(p)) = (counts.as_mut(), want.filter(|_| pic.mmco5)) {
                    g.shift(-p);
                }
            }
            Err(e) => {
                assert_eq!(e, DpbError::Full);
                pool.free(slot);
                d.abandon(&pic, &sh, &mut ev);
                pool.apply(&ev);
                t.abandoned += 1;
            }
        }
        assert!(pool.live() <= h264::MAX_REFS + 17, "{} slots live", pool.live());
    }
    d.flush(&mut ev);
    order.check(&ev);
    pool.apply(&ev);
    assert!(pool.all_out(), "a shown picture never went out");
    d.discard(&mut ev);
    pool.apply(&ev);
    assert_eq!(pool.live(), 0, "slots leaked");
    t.ordered = order.checked;
    t
}

fn check_lists(
    n: usize,
    pic: &h264::Picture,
    sh: &hp4::SliceHeader,
    d: &h264::Dpb,
    l0: &mut [u8; LIST4],
    l1: &mut [u8; LIST4],
) {
    d.lists(pic, sh, l0, l1);
    for &i in l0.iter().chain(l1.iter()) {
        assert!(i == NO_REF4 || (i as usize) < n, "list entry {i} of {n} references");
    }
}

fn h265_header(lsb: u32, ty: hp5::SliceType) -> hp5::SliceHeader {
    hp5::SliceHeader {
        first_slice_segment_in_pic_flag: true,
        no_output_of_prior_pics_flag: false,
        pps_id: 0,
        dependent_slice_segment_flag: false,
        slice_segment_address: 0,
        slice_type: ty,
        pic_output_flag: true,
        pic_order_cnt_lsb: lsb,
        st_rps: hp5::ShortTermRps::default(),
        st_rps_bits: 0,
        long_term_refs: FixedList::default(),
        slice_temporal_mvp_enabled_flag: false,
        slice_sao_luma_flag: false,
        slice_sao_chroma_flag: false,
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        ref_list_mods: hp5::RefListMods::default(),
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

const NAL_TYPES: [u8; 8] = [
    hp5::NAL_TRAIL_R,
    hp5::NAL_TRAIL_N,
    hp5::NAL_IDR_W_RADL,
    hp5::NAL_IDR_N_LP,
    hp5::NAL_CRA_NUT,
    hp5::NAL_BLA_W_LP,
    hp5::NAL_RASL_N,
    hp5::NAL_RADL_R,
];

/// Sub-layer non-reference, RADL or RASL, never prevTid0Pic (8.3.1).
fn skips_prev_tid0(t: u8) -> bool {
    (t <= 14 && t.is_multiple_of(2)) || (hp5::NAL_RADL_N..=hp5::NAL_RASL_R).contains(&t)
}

const MAX_POC_LSB: i32 = 16;

/// Drives an HEVC DPB with pictures made of 4 bytes each, the first byte of the run picking
/// the sequence, and checks the event contract throughout. The RPS of each picture names a
/// subset of what the DPB holds, so the stream mostly decodes, with a missing reference now
/// and then. Ends with a flush and a discard, after which no slot is live.
pub fn h265(bytes: &mut Bytes) -> Tally {
    h265_as(bytes, false)
}

/// As `h265` with order counts that keep within the reorder depth, `Order` checks most outputs.
pub fn h265_conform(bytes: &mut Bytes) -> Tally {
    h265_as(bytes, true)
}

fn h265_as(bytes: &mut Bytes, conform: bool) -> Tally {
    use hp5::SliceType::{B, I, P};
    let mut t = Tally::default();
    let Some(cfg) = bytes.next() else {
        return t;
    };
    let reorder = if cfg & 8 != 0 { 0 } else { (cfg & 7) + 1 };
    // the latency and dpb size bumps keep the order of a stream within them only, a conform run
    // keeps within the dpb size by its RPS and leaves the latency unbounded
    let (max_latency, dpb_size) = match conform {
        true => (None, h265::MAX_DPB as u8),
        false => {
            let latency = (cfg >> 4 & 3 != 0).then(|| u32::from(reorder + (cfg >> 4 & 3) - 1));
            let size =
                [h265::MAX_DPB as u8, reorder + 1, reorder + 2, reorder + 4][usize::from(cfg >> 6)];
            (latency, size.min(h265::MAX_DPB as u8))
        }
    };
    let seq = Seq { max_poc_lsb: MAX_POC_LSB, reorder, max_latency, dpb_size };
    let mut order = Order::new(usize::from(reorder));
    // references an RPS keeps in a conform run, the waiting pictures fill the rest of the DPB
    let kept = if conform { h265::MAX_DPB - 1 - usize::from(reorder) } else { 13 };
    let mut counts = conform.then(|| Conform::new(order.reorder, 1));
    // the last IRAP's (gop, POC) and whether it has no leading pictures
    let mut irap_at: Option<(u32, i32, bool)> = None;
    let mut d = h265::Dpb::new();
    let mut pool = Pool::new();
    let mut ev = Events::new();
    // prevTid0Pic's order count, and what the DPB decides from its own state
    let (mut prev_tid0, mut awaiting, mut after_eos, mut skip_rasl) = (0i32, true, false, false);
    let (mut l0, mut l1) = ([0u8; LIST5], [0u8; LIST5]);
    while let Some([b0, b1, b2, b3]) = bytes.take::<4>() {
        if b0 & 8 != 0 {
            match b1 & 3 {
                2 => {
                    d.bump_one(&mut ev);
                }
                3 => {
                    // what the next IRAP's C.5.2.2 decides, no events of its own
                    d.end_of_sequence();
                    ev.clear();
                    after_eos = true;
                }
                _ => {
                    d.discard(&mut ev);
                    (awaiting, after_eos, skip_rasl, prev_tid0) = (true, false, false, 0);
                    t.discards += 1;
                }
            }
            pool.apply(&ev);
        }
        let nal_type = NAL_TYPES[usize::from(b0 & 7)];
        let irap = hp5::is_irap(nal_type);
        let temporal_id = u8::from(!irap && b0 & 64 != 0);
        let nal = hp5::Nal { nal_type, temporal_id, start: 0, end: 0 };
        if (awaiting && !irap) || (hp5::is_rasl(nal_type) && skip_rasl) {
            assert!(d.skips(nal_type));
        }
        let no_rasl = hp5::is_idr(nal_type) || hp5::is_bla(nal_type) || awaiting || after_eos;
        let want = counts.as_mut().map(|g| g.next(b1));
        let (poc, lsb) = if irap && no_rasl {
            let lsb = want.map_or(i32::from(b1 & 15), |p| p.rem_euclid(MAX_POC_LSB));
            if let (Some(g), Some(p)) = (counts.as_mut(), want) {
                g.shift(lsb - p);
            }
            (lsb, lsb)
        } else {
            // prevTid0Pic and the MSB derivation hold a step of 7 either way
            let poc = want.map_or(prev_tid0 + i32::from(b1 as i8) / 32, |p| {
                p.clamp(prev_tid0 - 7, prev_tid0 + 7)
            });
            (poc, poc.rem_euclid(MAX_POC_LSB))
        };
        let ty = if irap || b2 & 3 == 0 {
            I
        } else if b2 & 3 == 1 {
            P
        } else {
            B
        };
        let mut sh = h265_header(lsb as u32, ty);
        sh.pic_output_flag = b0 & 32 == 0;
        sh.no_output_of_prior_pics_flag = b2 & 4 != 0;
        if !(irap && no_rasl) {
            // the RPS keeps a subset of the DPB, bit pairs of b2 and b3 per picture
            let keep = u16::from_le_bytes([b2 >> 3, b3]);
            for (i, r) in d.refs().iter().enumerate() {
                let delta = r.poc.wrapping_sub(poc);
                if delta == 0 || i >= kept.min(13) {
                    continue;
                }
                let how = keep >> i & 3;
                if how == 0 {
                    continue;
                }
                let used = how == 3;
                if how == 1 && !r.long_term || r.long_term {
                    let cur_lsb = poc.rem_euclid(MAX_POC_LSB);
                    let cycle =
                        (poc - cur_lsb - (r.poc - r.poc.rem_euclid(MAX_POC_LSB))) / MAX_POC_LSB;
                    sh.long_term_refs.push(LongTermRef {
                        poc_lsb: r.poc.rem_euclid(MAX_POC_LSB) as u32,
                        used_by_curr_pic: used,
                        delta_poc_msb_present: cycle >= 0 && b1 & 1 != 0,
                        delta_poc_msb_cycle: cycle.max(0) as u32,
                    });
                } else if delta < 0 {
                    sh.st_rps.delta_poc_s0.push(delta);
                    sh.st_rps.used_s0.push(used);
                } else {
                    sh.st_rps.delta_poc_s1.push(delta);
                    sh.st_rps.used_s1.push(used);
                }
            }
            if b0 & 128 != 0 {
                // a reference no picture has
                sh.st_rps.delta_poc_s0.push(-1000);
                sh.st_rps.used_s0.push(true);
            }
        }
        sh.num_ref_idx_l0_active_minus1 = u32::from(b3 & 7);
        sh.num_ref_idx_l1_active_minus1 = u32::from(b3 >> 3 & 7);
        if b1 & 2 != 0 {
            sh.ref_list_mods.l0_present = true;
            sh.ref_list_mods.l0.push(u32::from(b3 & 15));
            sh.ref_list_mods.l1_present = true;
            sh.ref_list_mods.l1.push(u32::from(b2 & 15));
        }
        t.pictures += 1;
        let inter = ty != I;
        let pic = match d.begin(&seq, &nal, &sh, inter, &mut ev) {
            Ok(p) => {
                order.check(&ev);
                pool.apply(&ev);
                p
            }
            Err(e) => {
                pool.apply(&ev);
                assert_eq!(e, DpbError::MissingReference);
                assert!(inter);
                t.errors += 1;
                (awaiting, after_eos) = (false, false);
                if temporal_id == 0 && !skips_prev_tid0(nal_type) {
                    prev_tid0 = poc;
                }
                continue;
            }
        };
        if pic.fate == Fate::Skip {
            t.skipped += 1;
            continue;
        }
        if irap {
            skip_rasl = no_rasl;
        }
        (awaiting, after_eos) = (false, false);
        if temporal_id == 0 && !skips_prev_tid0(nal_type) {
            prev_tid0 = poc;
        }
        if say() {
            eprintln!(
                "h265 {b0:08b} {b1} {b2} {b3} type {nal_type} tid {temporal_id} lsb {lsb} poc {poc} got {} {:?} {:?}",
                pic.poc,
                pic.fate,
                ev.as_slice()
            );
        }
        assert_eq!(pic.poc, poc, "the model's order count");
        // only leading pictures precede their IRAP in output order, none of an N_LP one
        let leading =
            hp5::is_rasl(nal_type) || (hp5::NAL_RADL_N..=hp5::NAL_RADL_R).contains(&nal_type);
        if let Some((g, p, n_lp)) = irap_at
            && g == pic.gop
            && poc < p
            && (n_lp || !leading)
        {
            order.beyond(pic.gop);
        }
        if irap {
            let n_lp = matches!(nal_type, hp5::NAL_IDR_N_LP | hp5::NAL_BLA_N_LP);
            irap_at = Some((pic.gop, poc, n_lp));
        }
        if pic.fate == Fate::Shown {
            order.shown((pic.gop, poc));
        }
        d.lists(&pic, &sh, &mut l0, &mut l1);
        let n = d.refs().len();
        for &i in l0.iter().chain(l1.iter()) {
            assert!(i == NO_REF5 || (i as usize) < n, "list entry {i} of {n} references");
        }
        for r in d.refs() {
            assert_ne!(pool.slots[r.slot.0 as usize], Held::Free, "a reference in a free slot");
        }
        if b0 & 16 != 0 {
            d.abandon(&pic, &mut ev);
            pool.apply(&ev);
            t.abandoned += 1;
            awaiting |= pic.irap && pic.no_rasl_output;
            continue;
        }
        let mut pic = pic;
        if b1 & 4 != 0 && pic.fate == Fate::Shown {
            pic.fate = Fate::Hidden;
        }
        let slot = pool.take();
        pool.give(slot, pic.fate == Fate::Shown);
        order.give(slot, (pic.gop, pic.poc));
        match d.end(&pic, slot, &mut ev) {
            Ok(()) => {
                order.check(&ev);
                pool.apply(&ev);
            }
            Err(e) => {
                assert_eq!(e, DpbError::Full);
                pool.free(slot);
                d.abandon(&pic, &mut ev);
                pool.apply(&ev);
                t.abandoned += 1;
            }
        }
        assert!(pool.live() <= h265::MAX_DPB + 17, "{} slots live", pool.live());
    }
    d.flush(&mut ev);
    order.check(&ev);
    pool.apply(&ev);
    assert!(pool.all_out(), "a shown picture never went out");
    d.discard(&mut ev);
    pool.apply(&ev);
    assert_eq!(pool.live(), 0, "slots leaked");
    t.ordered = order.checked;
    t
}

/// The slots `ev` puts out.
fn outs(ev: &Events) -> impl Iterator<Item = Slot> + '_ {
    ev.as_slice().iter().filter_map(|e| match *e {
        Event::Output(s) => Some(s),
        Event::Release(_) => None,
    })
}

/// Drives a VP9 DPB with frames made of 4 bytes each, key, intra only, inter or shown again,
/// and checks the event contract. Frames go out in decode order, a frame its own slot once
/// when shown, a frame shown again the slot it names. Ends with a discard, after which no slot
/// is live.
pub fn vp9(bytes: &mut Bytes) -> Tally {
    let mut t = Tally::default();
    let mut d = vp9::Dpb::new();
    let mut pool = Pool::new();
    let mut ev = Events::new();
    let mut base = d.parse(&[0b1000_1000]).expect("a frame shown again parses");
    (base.frame_width, base.frame_height) = (64, 64);
    while let Some([b0, b1, b2, b3]) = bytes.take::<4>() {
        if b0 & 8 != 0 {
            match b3 & 3 {
                0 => {
                    d.discard(&mut ev);
                    t.discards += 1;
                }
                1 => d.drop_slots(&mut ev),
                _ => {
                    ev.clear();
                    d.await_key();
                }
            }
            pool.apply_again(&ev);
        }
        let mut h = base;
        h.show_existing_frame = b0 & 3 == 3;
        h.frame_to_show_map_idx = h.show_existing_frame.then_some(b1 & 7);
        h.show_frame = h.show_existing_frame || b0 & 4 == 0;
        h.frame_type = if b0 & 3 == 0 { Vp9Type::KeyFrame } else { Vp9Type::NonKeyFrame };
        h.intra_only = b0 & 3 == 1 && b2 & 1 != 0;
        h.refresh_frame_flags = match b0 & 3 {
            0 => 0xff,
            3 => 0,
            _ => b1,
        };
        h.ref_frame_idx = (b0 & 3 != 0 && !h.intra_only && !h.show_existing_frame).then_some([
            b2 >> 1 & 7,
            b2 >> 4 & 7,
            b3 >> 2 & 7,
        ]);
        t.pictures += 1;
        let begun = d.begin(&h, &mut ev);
        let shown_again: Vec<Slot> = outs(&ev).collect();
        pool.apply_again(&ev);
        let fate = match begun {
            Ok(f) => f,
            Err(e) => {
                assert_eq!(e, DpbError::MissingReference);
                t.errors += 1;
                d.await_key();
                continue;
            }
        };
        if fate == Fate::Skip {
            assert!(shown_again.len() <= usize::from(h.show_existing_frame));
            t.skipped += 1;
            continue;
        }
        assert!(shown_again.is_empty() && !h.show_existing_frame);
        for r in d.refs(&h).iter().flatten() {
            assert_ne!(pool.slots[r.slot.0 as usize], Held::Free, "a reference in a free slot");
        }
        if b0 & 16 != 0 {
            d.abandon(&h, &mut ev);
            pool.apply_again(&ev);
            t.abandoned += 1;
            continue;
        }
        let slot = pool.take();
        pool.give(slot, true);
        d.end(&h, fate, slot, &mut ev);
        let out: Vec<Slot> = outs(&ev).collect();
        assert_eq!(out, if h.show_frame { vec![slot] } else { vec![] }, "the frame goes out");
        t.ordered += out.len();
        pool.apply_again(&ev);
        assert!(pool.live() <= vp9::NUM_SLOTS, "{} slots live", pool.live());
    }
    d.discard(&mut ev);
    pool.apply_again(&ev);
    assert_eq!(pool.live(), 0, "slots leaked");
    t
}

const AV1_SEQ: write::Sequence = write::Sequence {
    level_idx: 8,
    tier: 0,
    width: 64,
    height: 64,
    ten_bit: false,
    enable_cdef: false,
    color: None,
    full_range: false,
};

/// A temporal unit of a sequence header and frame `f`, or a frame showing slot `again` again.
fn av1_unit(f: Option<&write::Frame>, again: u8) -> Vec<u8> {
    let tiles = write::Tiles::single(AV1_SEQ.width, AV1_SEQ.height).expect("one tile");
    let mut tu = write::sequence_header(&AV1_SEQ, 2);
    match f {
        Some(f) => {
            tu.extend(write::frame_header(&AV1_SEQ, f, &tiles, 2).bytes);
            tu.extend(write::tile_group(&[0; 4], 2));
        }
        None => tu.extend([OBU_FRAME_HEADER << 3 | 0b010, 1, 0x80 | (again & 7) << 4]),
    }
    tu
}

/// Every slot the DPB holds a frame in holds the frame the parser's slot describes, told apart
/// by order hint, so no frame decodes against a frame its header does not name.
fn av1_slots_agree(d: &av1::Dpb, hints: &[u8; POOL]) {
    for (i, (s, p)) in d.slots().iter().zip(&d.stream().refs).enumerate() {
        if let Some(r) = s {
            let hint = hints[r.slot.0 as usize];
            assert!(
                p.valid && p.order_hint == hint,
                "slot {i} holds hint {hint}, the parser's {} valid {}",
                p.order_hint,
                p.valid
            );
        }
    }
}

/// Drives an AV1 DPB with temporal units of one frame made of 4 bytes each, key, inter or
/// shown again, some hidden, through its parser as the decoder does, failing and skipping
/// frames as the decoder would. Checks the event contract, that frames go out in decode
/// order and that the DPB's slots agree with the parser's. Ends with a discard, after which
/// no slot is live.
pub fn av1(bytes: &mut Bytes) -> Tally {
    let mut t = Tally::default();
    let mut d = av1::Dpb::new();
    let mut pool = Pool::new();
    let mut ev = Events::new();
    let mut hints = [0u8; POOL];
    let mut hint = 0u8;
    while let Some([b0, b1, b2, b3]) = bytes.take::<4>() {
        if b0 & 8 != 0 {
            ev.clear();
            match b3 & 3 {
                0 => {
                    d.discard(&mut ev);
                    t.discards += 1;
                }
                1 => d.drop_frames(&mut ev),
                2 => {
                    let mut at = 0;
                    assert!(d.next_frame(&[0xff; 4], &mut at).is_err());
                    t.errors += 1;
                }
                _ => d.await_key(),
            }
            pool.apply_again(&ev);
        }
        hint = hint.wrapping_add(1);
        let key = b0 & 3 == 0;
        let idx = u32::from_le_bytes([b2, b3, b1, 0]);
        let f = write::Frame {
            key,
            order_hint: hint,
            refresh_frame_flags: if key { 0xff } else { b1 },
            primary_ref_frame: if b3 & 0x80 != 0 { 0 } else { 7 },
            ref_frame_idx: std::array::from_fn(|i| (idx >> (3 * i)) as u8 & 7),
            base_q_idx: 40,
            tx_mode_select: false,
        };
        let tu = av1_unit((b0 & 3 != 3).then_some(&f), b1);
        t.pictures += 1;
        let mut at = 0;
        let frame = match d.next_frame(&tu, &mut at) {
            Ok(Some(fr)) => fr,
            Ok(None) => panic!("a unit without its frame"),
            Err(_) => {
                t.errors += 1;
                continue;
            }
        };
        let mut h = frame.header;
        if b0 & 4 != 0 && !h.show_existing_frame {
            (h.show_frame, h.showable_frame) = (false, true);
        }
        let begun = d.begin(&h, &mut ev);
        let shown_again: Vec<Slot> = outs(&ev).collect();
        pool.apply_again(&ev);
        if !h.show_existing_frame {
            av1_slots_agree(&d, &hints);
        }
        match begun {
            Err(_) => {
                t.errors += 1;
                d.await_key();
            }
            Ok(Fate::Skip) => {
                assert!(shown_again.len() <= usize::from(h.show_existing_frame));
                t.skipped += 1;
            }
            Ok(_) if b0 & 16 != 0 => {
                d.abandon(&h, &mut ev);
                pool.apply_again(&ev);
                t.abandoned += 1;
            }
            Ok(fate) => {
                assert!(shown_again.is_empty() && !h.show_existing_frame);
                for r in d.refs(&h).iter().flatten() {
                    assert_ne!(
                        pool.slots[r.slot.0 as usize],
                        Held::Free,
                        "a reference in a free slot"
                    );
                }
                let slot = pool.take();
                pool.give(slot, true);
                hints[slot.0 as usize] = h.order_hint;
                d.end(&h, fate, slot, &mut ev);
                let out: Vec<Slot> = outs(&ev).collect();
                assert_eq!(
                    out,
                    if h.show_frame { vec![slot] } else { vec![] },
                    "the frame goes out"
                );
                t.ordered += out.len();
                pool.apply_again(&ev);
            }
        }
        // the parser's slots take the frame as the decoder reads the unit to its end
        assert!(matches!(d.next_frame(&tu, &mut at), Ok(None)));
        assert!(pool.live() <= av1::NUM_SLOTS, "{} slots live", pool.live());
    }
    d.discard(&mut ev);
    pool.apply_again(&ev);
    assert_eq!(pool.live(), 0, "slots leaked");
    t
}
