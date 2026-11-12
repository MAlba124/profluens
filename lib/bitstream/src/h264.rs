use profluens_video::bits::{BitReader, de_emulate};
use profluens_video::h264parse::{
    self as hp, MmcoOp, Nal, PocState, RefListMod, SliceHeader, SliceType, Sps,
};

use crate::{DpbError, Event, Events, Fate, Slot};

/// Reference frames the DPB holds, max_num_ref_frames is at most 16.
pub const MAX_REFS: usize = 16;
/// num_ref_idx_lX_active_minus1 is at most 31.
pub const MAX_LIST: usize = 32;
/// An unused RefPicList entry.
pub const NO_REF: u8 = 0xff;
/// Pictures waiting for output, a reorder depth of 16 and the current one.
const MAX_WAITING: usize = MAX_REFS + 1;

/// slice_type of a slice NAL mod 5 (Table 7-6, 2 is I, 4 SI), read without the slice header
/// parse. None for a header too short.
pub fn peek_slice_type(nal: &[u8]) -> Option<u32> {
    let mut body = [0u8; 16];
    let mut r = slice_head(nal, &mut body)?;
    r.ue();
    Some(r.ue() % 5)
}

/// pic_parameter_set_id of a slice NAL, what its full parse needs. None for a header too
/// short.
pub fn peek_pps_id(nal: &[u8]) -> Option<u32> {
    let mut body = [0u8; 16];
    let mut r = slice_head(nal, &mut body)?;
    r.ue();
    r.ue();
    Some(r.ue())
}

/// A slice of a non-IDR or an IDR picture.
pub fn is_slice(unit_type: u8) -> bool {
    matches!(unit_type, hp::NAL_SLICE_NON_IDR | hp::NAL_SLICE_IDR)
}

/// The start of a slice header past the NAL header byte, de-emulated into `body`.
fn slice_head<'a>(nal: &[u8], body: &'a mut [u8; 16]) -> Option<BitReader<'a>> {
    if nal.len() < 2 {
        return None;
    }
    let n = de_emulate(&nal[1..], body);
    Some(BitReader::new(&body[..n]))
}

/// A reference frame as the hardware takes it (VA's ReferenceFrames, MFX's frame stores).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ref {
    pub slot: Slot,
    pub frame_num: u16,
    /// FrameNumWrap for the current picture (8.2.4.1), PicNum of a short-term frame
    pub frame_num_wrap: i32,
    pub top_poc: i32,
    pub bottom_poc: i32,
    /// LongTermFrameIdx when marked long-term
    pub long_term: Option<u8>,
    /// a frame_num gap filler (8.2.5.2) in the slot of the reference before it, a correct
    /// stream never predicts from it
    pub non_existing: bool,
}

impl Ref {
    const EMPTY: Ref = Ref {
        slot: Slot(0),
        frame_num: 0,
        frame_num_wrap: 0,
        top_poc: 0,
        bottom_poc: 0,
        long_term: None,
        non_existing: false,
    };

    /// PicOrderCnt of the frame (8-1).
    pub fn poc(&self) -> i32 {
        self.top_poc.min(self.bottom_poc)
    }

    fn short_term(&self) -> bool {
        self.long_term.is_none()
    }

    fn long_term_is(&self, idx: u32) -> bool {
        self.long_term.is_some_and(|l| u32::from(l) == idx)
    }
}

/// Where decoding stands after `new` or `discard`. Pictures wait for an IDR or an all intra
/// picture, and a non-IDR key's leading pictures, those before it in output order, may
/// reference pictures never decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    AwaitKey,
    /// started at a non-IDR key of order count `poc`, until the next IDR or MMCO 5
    Leading {
        poc: i32,
    },
    Running,
}

impl Start {
    /// Whether a picture is decoded at all, before its order count is known.
    fn wants(self, idr: bool, intra: bool) -> bool {
        self != Start::AwaitKey || idr || intra
    }

    /// The fate of a picture `wants` took. `reset` is an IDR or MMCO 5 picture. A non-reference
    /// leading picture is skipped, a reference one decoded so the DPB and lists stay as coded.
    fn fate(&mut self, reset: bool, poc: i32, reference: bool) -> Fate {
        match *self {
            _ if reset => {
                *self = Start::Running;
                Fate::Shown
            }
            Start::AwaitKey => {
                *self = Start::Leading { poc };
                Fate::Shown
            }
            Start::Leading { poc: key } if poc < key => {
                if reference {
                    Fate::Hidden
                } else {
                    Fate::Skip
                }
            }
            _ => Fate::Shown,
        }
    }
}

/// The current picture as `begin` derived it, handed back to `lists` and `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Picture {
    pub fate: Fate,
    pub idr: bool,
    /// nal_ref_idc != 0
    pub reference: bool,
    /// memory_management_control_operation 5, the stored frame takes frame_num 0 and order
    /// counts from 0
    pub mmco5: bool,
    pub frame_num: u16,
    /// order counts the picture decodes with
    pub top_poc: i32,
    pub bottom_poc: i32,
    /// IDRs, MMCO 5s and discards so far, output order is (gop, poc)
    pub gop: u32,
    /// PicOrderCnt for output, 0 for an MMCO 5 picture
    pub poc: i32,
    /// the key decoding started at with nothing to reorder before it, it goes out at once
    key_now: bool,
}

impl Picture {
    const SKIP: Picture = Picture {
        fate: Fate::Skip,
        idr: false,
        reference: false,
        mmco5: false,
        frame_num: 0,
        top_poc: 0,
        bottom_poc: 0,
        gop: 0,
        poc: 0,
        key_now: false,
    };
}

#[derive(Clone, Copy)]
struct Waiting {
    slot: Slot,
    gop: u32,
    poc: i32,
}

/// The waiting picture a bump puts out next, the smallest (gop, POC).
fn smallest(waiting: &[Waiting]) -> Option<usize> {
    waiting.iter().enumerate().min_by_key(|(_, w)| (w.gop, w.poc)).map(|(i, _)| i)
}

/// The picture the reorder rule lets go once the next picture's (gop, POC) is known, before
/// that picture is decoded. The depth counting it is reached and the smallest waiting picture
/// precedes it, a bump after its decode would put out the same one.
fn due_before(waiting: &[Waiting], depth: usize, next: (u32, i32)) -> Option<usize> {
    if waiting.len() < depth {
        return None;
    }
    let i = smallest(waiting)?;
    ((waiting[i].gop, waiting[i].poc) < next).then_some(i)
}

/// No picture after a key of this stream precedes it in output order.
fn no_reorder(sps: &Sps) -> bool {
    sps.max_num_reorder_frames == Some(0) || sps.pic_order_cnt_type == 2
}

/// FrameNumWrap of `frame_num` for a current picture of `cur` (8-27).
fn wrap(frame_num: u16, cur: i32, max_frame_num: i32) -> i32 {
    let f = i32::from(frame_num);
    if f > cur { f - max_frame_num } else { f }
}

/// A reference list under construction, indices into `Dpb::refs`, `NO_REF` for a missing one.
#[derive(Clone, Copy, PartialEq, Eq)]
struct List {
    idx: [u8; MAX_LIST],
    len: u8,
}

impl List {
    const EMPTY: List = List { idx: [NO_REF; MAX_LIST], len: 0 };

    fn push(&mut self, i: u8) {
        if (self.len as usize) < MAX_LIST {
            self.idx[self.len as usize] = i;
            self.len += 1;
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.idx[..self.len as usize]
    }

    /// Appends the references `keep` takes in ascending `key`, ties in DPB order.
    fn sorted(&mut self, refs: &[Ref], keep: impl Fn(&Ref) -> bool, key: impl Fn(&Ref) -> i64) {
        let start = self.len as usize;
        for (i, r) in refs.iter().enumerate() {
            if keep(r) {
                self.push(i as u8);
            }
        }
        let end = self.len as usize;
        self.idx[start..end].sort_unstable_by_key(|&i| (key(&refs[i as usize]), i));
    }

    fn extend(&mut self, o: &List) {
        for &i in o.as_slice() {
            self.push(i);
        }
    }

    /// The modified list (8.2.4.3), the pictures the modifications name in order and the rest
    /// of the initial list after them. A name no picture has holds `NO_REF`.
    fn modify(&self, mods: &[RefListMod], refs: &[Ref], cur: i32, max_frame_num: i32) -> List {
        let mut out = List::EMPTY;
        let mut pred = cur;
        for m in mods {
            let found = match m.op {
                0 | 1 => {
                    let d = m.value as i32 + 1;
                    let mut n = if m.op == 0 { pred - d } else { pred + d };
                    if n < 0 {
                        n += max_frame_num;
                    } else if n >= max_frame_num {
                        n -= max_frame_num;
                    }
                    pred = n;
                    refs.iter().position(|r| r.short_term() && i32::from(r.frame_num) == n)
                }
                2 => refs.iter().position(|r| r.long_term_is(m.value)),
                // MVC view modifications
                _ => continue,
            };
            out.push(found.map_or(NO_REF, |i| i as u8));
        }
        for &i in self.as_slice() {
            if !out.as_slice().contains(&i) {
                out.push(i);
            }
        }
        out
    }

    /// The first `active` entries into a hardware list, `NO_REF` past them.
    fn write(&self, active: usize, out: &mut [u8; MAX_LIST]) {
        for (d, &s) in out.iter_mut().zip(self.as_slice()).take(active) {
            *d = s;
        }
    }
}

/// Reference marking, reference lists and output order of an H.264 frame stream (8.2.1,
/// 8.2.4, 8.2.5 and C.4.5.3). No allocation, no hardware, the caller decodes.
pub struct Dpb {
    refs: [Ref; MAX_REFS],
    n_refs: u8,
    waiting: [Waiting; MAX_WAITING],
    n_waiting: u8,
    poc: PocState,
    start: Start,
    gop: u32,
    /// Max(max_num_ref_frames, 1)
    max_refs: u8,
    max_frame_num: i32,
    reorder: u8,
    /// PrevRefFrameNum, unknown from an IDR or a discard to the next reference picture
    prev_ref_frame_num: Option<u16>,
}

impl Dpb {
    pub fn new() -> Self {
        Dpb {
            refs: [Ref::EMPTY; MAX_REFS],
            n_refs: 0,
            waiting: [Waiting { slot: Slot(0), gop: 0, poc: 0 }; MAX_WAITING],
            n_waiting: 0,
            poc: PocState::new(),
            start: Start::AwaitKey,
            gop: 0,
            max_refs: 1,
            max_frame_num: 16,
            reorder: 1,
            prev_ref_frame_num: None,
        }
    }

    /// Starts a picture from its first slice header, once every slice of the access unit is
    /// parsed (`intra`: all of them I or SI). An IDR puts out or drops the pictures before it,
    /// a frame_num gap is filled with non-existing frames (8.2.5.2), then POC (8.2.1). Puts out
    /// what the reorder rule lets go before this picture, so a key is out before the next
    /// decode is queued behind it. Pictures before the first key and non-reference leading
    /// pictures of a non-IDR key are `Fate::Skip`.
    pub fn begin(
        &mut self,
        sps: &Sps,
        nal: &Nal,
        sh: &SliceHeader,
        intra: bool,
        ev: &mut Events,
    ) -> Result<Picture, DpbError> {
        ev.clear();
        if sh.field_pic_flag {
            return Err(DpbError::Fields);
        }
        let idr = nal.unit_type == hp::NAL_SLICE_IDR;
        if !self.start.wants(idr, intra) {
            return Ok(Picture::SKIP);
        }
        let reference = nal.ref_idc != 0;
        self.max_refs = sps.max_num_ref_frames.clamp(1, MAX_REFS as u32) as u8;
        self.max_frame_num = sps.max_frame_num() as i32;
        self.reorder = sps.reorder_frames().clamp(1, MAX_REFS as u32) as u8;
        let key = self.start == Start::AwaitKey;
        if idr {
            // earlier pictures go out before the order count restarts, unless the IDR drops them
            if sh.no_output_of_prior_pics_flag {
                self.drop_waiting(ev);
            } else {
                self.bump(true, ev);
            }
            self.drop_refs(|_| false, ev);
            self.poc.reset();
            self.gop += 1;
            self.prev_ref_frame_num = None;
        } else {
            self.fill_gap(sps, sh, ev);
        }
        let (top, bottom) = self.poc.compute(sps, sh, idr, nal.ref_idc);
        let mmco5 = !idr
            && reference
            && sh.adaptive_ref_pic_marking_mode_flag
            && sh.mmco_ops.iter().any(|o| o.op == 5);
        if mmco5 {
            // the prior pictures go out as at an IDR (C.4.4), the order count restarts after it
            self.bump(true, ev);
            self.gop += 1;
        }
        let poc = if mmco5 { 0 } else { top.min(bottom) };
        let cur = sh.frame_num as i32;
        let max = self.max_frame_num;
        for r in &mut self.refs[..self.n_refs as usize] {
            r.frame_num_wrap = match r.long_term {
                None => wrap(r.frame_num, cur, max),
                Some(_) => i32::from(r.frame_num),
            };
        }
        let fate = self.start.fate(idr || mmco5, poc, reference);
        if fate == Fate::Skip {
            return Ok(Picture::SKIP);
        }
        if fate == Fate::Shown {
            while let Some(i) = due_before(self.waiting(), self.reorder as usize, (self.gop, poc)) {
                self.put_out(i, ev);
            }
        }
        Ok(Picture {
            fate,
            idr,
            reference,
            mmco5,
            frame_num: sh.frame_num as u16,
            top_poc: top,
            bottom_poc: bottom,
            gop: self.gop,
            poc,
            key_now: key && (no_reorder(sps) || !(idr || mmco5)),
        })
    }

    /// Puts out what `begin` would let go before the picture whose first slice header `sh`
    /// just came, ahead of its other slices, so a key goes out before the next picture is
    /// parsed whole. Changes nothing else, `begin` then finds those out already.
    pub fn ahead(&mut self, sps: &Sps, nal: &Nal, sh: &SliceHeader, ev: &mut Events) {
        ev.clear();
        if nal.unit_type == hp::NAL_SLICE_IDR {
            if !sh.no_output_of_prior_pics_flag {
                self.bump(true, ev);
            }
            return;
        }
        if self.start == Start::AwaitKey || self.n_waiting == 0 || sh.field_pic_flag {
            return;
        }
        let reference = nal.ref_idc != 0;
        if reference
            && sh.adaptive_ref_pic_marking_mode_flag
            && sh.mmco_ops.iter().any(|o| o.op == 5)
        {
            self.bump(true, ev);
            return;
        }
        // a frame_num gap leaves the order count as it is
        let (top, bottom) = self.poc.clone().compute(sps, sh, false, nal.ref_idc);
        let poc = top.min(bottom);
        let mut start = self.start;
        if start.fate(false, poc, reference) != Fate::Shown {
            return;
        }
        let depth = sps.reorder_frames().clamp(1, MAX_REFS as u32) as usize;
        while let Some(i) = due_before(self.waiting(), depth, (self.gop, poc)) {
            self.put_out(i, ev);
        }
    }

    /// References the current picture may use in a stable order, valid from `begin` to `end`.
    pub fn refs(&self) -> &[Ref] {
        &self.refs[..self.n_refs as usize]
    }

    /// RefPicList0 and 1 of one slice (8.2.4) as indices into `refs`, `NO_REF` past the active
    /// count, past the references there are and on a list the slice type has not.
    pub fn lists(
        &self,
        pic: &Picture,
        sh: &SliceHeader,
        l0: &mut [u8; MAX_LIST],
        l1: &mut [u8; MAX_LIST],
    ) {
        *l0 = [NO_REF; MAX_LIST];
        *l1 = [NO_REF; MAX_LIST];
        let refs = self.refs();
        let cur = i32::from(pic.frame_num);
        let max = self.max_frame_num;
        let mut long = List::EMPTY;
        long.sorted(refs, |r| !r.short_term(), |r| i64::from(r.long_term.unwrap_or(0)));
        let n0 = sh.num_ref_idx_l0_active_minus1 as usize + 1;
        match sh.slice_type {
            SliceType::P | SliceType::Sp => {
                let mut a = List::EMPTY;
                a.sorted(refs, Ref::short_term, |r| -i64::from(r.frame_num_wrap));
                a.extend(&long);
                a.modify(&sh.ref_list_mods_l0, refs, cur, max).write(n0, l0);
            }
            SliceType::B => {
                let cur_poc = pic.top_poc.min(pic.bottom_poc);
                let mut less = List::EMPTY;
                less.sorted(refs, |r| r.short_term() && r.poc() < cur_poc, |r| -i64::from(r.poc()));
                let mut greater = List::EMPTY;
                greater.sorted(
                    refs,
                    |r| r.short_term() && r.poc() > cur_poc,
                    |r| i64::from(r.poc()),
                );
                let (mut a, mut b) = (less, greater);
                a.extend(&greater);
                a.extend(&long);
                b.extend(&less);
                b.extend(&long);
                if b.len > 1 && a == b {
                    b.idx.swap(0, 1);
                }
                a.modify(&sh.ref_list_mods_l0, refs, cur, max).write(n0, l0);
                let n1 = sh.num_ref_idx_l1_active_minus1 as usize + 1;
                b.modify(&sh.ref_list_mods_l1, refs, cur, max).write(n1, l1);
            }
            SliceType::I | SliceType::Si => {}
        }
    }

    /// Ends a decoded picture in `slot`. A reference picture is marked (sliding window or
    /// MMCO, 8.2.5) and stored, a shown one queued for output, then bumps (C.4.5.3). A
    /// `Fate::Skip` picture changes nothing. `Full` when more pictures wait for output than
    /// the DPB holds, nothing changes then.
    pub fn end(
        &mut self,
        pic: &Picture,
        slot: Slot,
        sh: &SliceHeader,
        ev: &mut Events,
    ) -> Result<(), DpbError> {
        ev.clear();
        if pic.fate == Fate::Skip {
            return Ok(());
        }
        let shown = pic.fate == Fate::Shown;
        if shown && self.n_waiting as usize == MAX_WAITING {
            return Err(DpbError::Full);
        }
        let stored = pic.reference && self.store(pic, slot, sh, ev);
        if shown {
            self.waiting[self.n_waiting as usize] = Waiting { slot, gop: pic.gop, poc: pic.poc };
            self.n_waiting += 1;
        } else if !stored {
            ev.push(Event::Release(slot));
        }
        self.bump(pic.key_now, ev);
        Ok(())
    }

    /// A seek or a stream error. Releases every picture without output and waits for a key.
    /// A non-IDR key goes out as soon as it is decoded, its leading pictures are `Hidden` or
    /// `Skip`.
    pub fn discard(&mut self, ev: &mut Events) {
        ev.clear();
        self.drop_waiting(ev);
        self.drop_refs(|_| false, ev);
        self.poc.reset();
        self.gop += 1;
        self.start = Start::AwaitKey;
        self.prev_ref_frame_num = None;
    }

    /// Puts out the next waiting picture ahead of the reorder depth, for a caller out of
    /// surfaces (the bumping of C.4.5.3 with no empty frame buffer). False when none waits.
    pub fn bump_one(&mut self, ev: &mut Events) -> bool {
        ev.clear();
        let Some(i) = smallest(self.waiting()) else {
            return false;
        };
        self.put_out(i, ev);
        true
    }

    /// End of stream. Puts out every waiting picture in order, the references stay.
    pub fn flush(&mut self, ev: &mut Events) {
        ev.clear();
        self.bump(true, ev);
    }

    fn waiting(&self) -> &[Waiting] {
        &self.waiting[..self.n_waiting as usize]
    }

    /// Fills frame_num values skipped since PrevRefFrameNum with non-existing short-term
    /// frames (8.2.5.2), each through the sliding window. They take the slot and, with POC
    /// type 0, the order counts of the last stored reference.
    fn fill_gap(&mut self, sps: &Sps, sh: &SliceHeader, ev: &mut Events) {
        let Some(prev) = self.prev_ref_frame_num else {
            return;
        };
        let (max, cur, prev) = (self.max_frame_num, sh.frame_num as i32, i32::from(prev));
        if cur == prev || cur == (prev + 1) % max || self.n_refs == 0 {
            return;
        }
        let last = self.refs[self.n_refs as usize - 1];
        let gap = (cur - prev - 1).rem_euclid(max);
        // types 1 and 2 count the fillers like frames without delta_pic_order_cnt
        let mut counted = (sps.pic_order_cnt_type != 0).then(|| sh.clone());
        if let Some(s) = &mut counted {
            s.delta_pic_order_cnt = [0; 2];
        }
        // fillers before the last max_refs leave through the sliding window anyway
        for k in (gap - i32::from(self.max_refs)).max(0)..gap {
            let f = (prev + 1 + k) % max;
            let (top, bottom) = match &mut counted {
                Some(s) => {
                    s.frame_num = f as u32;
                    self.poc.compute(sps, s, false, 1)
                }
                None => (last.top_poc, last.bottom_poc),
            };
            if self.n_refs >= self.max_refs {
                self.evict_oldest(f, self.n_refs as usize, ev);
            }
            if self.n_refs as usize == MAX_REFS {
                break;
            }
            self.refs[self.n_refs as usize] = Ref {
                slot: last.slot,
                frame_num: f as u16,
                frame_num_wrap: f,
                top_poc: top,
                bottom_poc: bottom,
                long_term: None,
                non_existing: true,
            };
            self.n_refs += 1;
        }
        self.prev_ref_frame_num = Some((cur - 1).rem_euclid(max) as u16);
    }

    /// Marks for the current reference picture and stores it. False when only long-term
    /// frames fill the DPB and it is not stored.
    fn store(&mut self, pic: &Picture, slot: Slot, sh: &SliceHeader, ev: &mut Events) -> bool {
        let cur = i32::from(pic.frame_num);
        let mut long_term = None;
        if pic.idr {
            long_term = sh.long_term_reference_flag.then_some(0);
        } else if sh.adaptive_ref_pic_marking_mode_flag {
            for op in sh.mmco_ops.iter() {
                self.mmco(op, cur, ev);
            }
            if let Some(op) = sh.mmco_ops.iter().find(|o| o.op == 6) {
                // 8.2.5.4.6, the frame that held the index is unmarked first
                self.drop_refs(|r| !r.long_term_is(op.arg1), ev);
                long_term = Some(op.arg1 as u8);
            }
        } else if self.n_refs >= self.max_refs {
            self.evict_oldest(cur, self.n_refs as usize, ev);
        }
        let (mut top, mut bottom, mut frame_num) = (pic.top_poc, pic.bottom_poc, pic.frame_num);
        if pic.mmco5 {
            // 8.2.1, the frame stored after an MMCO 5 counts from 0
            let t = top.min(bottom);
            top -= t;
            bottom -= t;
            frame_num = 0;
            self.poc.reset();
        }
        if self.n_refs as usize == MAX_REFS
            && !self.evict_oldest(i32::from(frame_num), MAX_REFS, ev)
        {
            return false;
        }
        self.refs[self.n_refs as usize] = Ref {
            slot,
            frame_num,
            frame_num_wrap: i32::from(frame_num),
            top_poc: top,
            bottom_poc: bottom,
            long_term,
            non_existing: false,
        };
        self.n_refs += 1;
        // at most max_refs frames are marked (7.4.3.3), past it the oldest short-term goes
        while self.n_refs > self.max_refs
            && self.evict_oldest(i32::from(frame_num), self.n_refs as usize - 1, ev)
        {}
        self.prev_ref_frame_num = Some(frame_num);
        true
    }

    /// One memory_management_control_operation (8.2.5.4), 6 is the caller's.
    fn mmco(&mut self, op: &MmcoOp, cur: i32, ev: &mut Events) {
        match op.op {
            1 => {
                let x = cur - (op.arg1 as i32 + 1);
                self.drop_refs(|r| !(r.short_term() && r.frame_num_wrap == x), ev);
            }
            2 => self.drop_refs(|r| !r.long_term_is(op.arg1), ev),
            3 => {
                let x = cur - (op.arg1 as i32 + 1);
                self.drop_refs(|r| !r.long_term_is(op.arg2), ev);
                if let Some(r) = self.refs[..self.n_refs as usize]
                    .iter_mut()
                    .find(|r| r.short_term() && r.frame_num_wrap == x)
                {
                    r.long_term = Some(op.arg2 as u8);
                }
            }
            4 => self.drop_refs(|r| r.long_term.is_none_or(|l| u32::from(l) < op.arg1), ev),
            5 => self.drop_refs(|_| false, ev),
            _ => {}
        }
    }

    /// Unmarks the short-term frame of smallest FrameNumWrap among the first `upto`, false
    /// when there is none.
    fn evict_oldest(&mut self, cur: i32, upto: usize, ev: &mut Events) -> bool {
        let max = self.max_frame_num;
        let Some(at) = self.refs[..upto]
            .iter()
            .enumerate()
            .filter(|(_, r)| r.short_term())
            .min_by_key(|(_, r)| wrap(r.frame_num, cur, max))
            .map(|(i, _)| i)
        else {
            return false;
        };
        self.remove_ref(at, ev);
        true
    }

    fn waits(&self, slot: Slot) -> bool {
        self.waiting().iter().any(|w| w.slot == slot)
    }

    fn is_ref(&self, slot: Slot) -> bool {
        self.refs().iter().any(|r| r.slot == slot)
    }

    /// Removes reference `at` in DPB order, releasing its slot unless a filler shares it or
    /// it waits for output.
    fn remove_ref(&mut self, at: usize, ev: &mut Events) {
        let slot = self.refs[at].slot;
        self.refs.copy_within(at + 1..self.n_refs as usize, at);
        self.n_refs -= 1;
        if !self.waits(slot) && !self.is_ref(slot) {
            ev.push(Event::Release(slot));
        }
    }

    /// Removes the references `keep` refuses.
    fn drop_refs(&mut self, keep: impl Fn(&Ref) -> bool, ev: &mut Events) {
        let mut i = 0;
        while i < self.n_refs as usize {
            if keep(&self.refs[i]) {
                i += 1;
            } else {
                self.remove_ref(i, ev);
            }
        }
    }

    /// Drops the pictures waiting for output without putting them out.
    fn drop_waiting(&mut self, ev: &mut Events) {
        while self.n_waiting > 0 {
            self.n_waiting -= 1;
            let slot = self.waiting[self.n_waiting as usize].slot;
            if !self.is_ref(slot) {
                ev.push(Event::Release(slot));
            }
        }
    }

    fn put_out(&mut self, at: usize, ev: &mut Events) {
        let slot = self.waiting[at].slot;
        self.waiting.copy_within(at + 1..self.n_waiting as usize, at);
        self.n_waiting -= 1;
        ev.push(Event::Output(slot));
        if !self.is_ref(slot) {
            ev.push(Event::Release(slot));
        }
    }

    /// Puts out the smallest (gop, POC) while more than the reorder depth wait, or all.
    fn bump(&mut self, all: bool, ev: &mut Events) {
        while self.n_waiting > 0 && (all || self.n_waiting > self.reorder) {
            let Some(i) = smallest(self.waiting()) else {
                break;
            };
            self.put_out(i, ev);
        }
    }
}

impl Default for Dpb {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
