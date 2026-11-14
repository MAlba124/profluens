use profluens_video::bits::{BitReader, de_emulate};
use profluens_video::h265parse::{self as hp, Nal, SliceHeader, Sps};

use crate::{DpbError, Event, Events, Fate, Slot};

/// Pictures the DPB holds, the current one included (sps_max_dec_pic_buffering is at most 16).
pub const MAX_DPB: usize = 16;
/// num_ref_idx_lX_active_minus1 is at most 14.
pub const MAX_LIST: usize = 15;
/// An unused RefPicList entry.
pub const NO_REF: u8 = 0xff;
/// Pictures waiting for output, a full DPB and the current one.
const MAX_WAITING: usize = MAX_DPB + 1;
/// NumRpsCurrTempListX before it is cut, list entries cycle up to this.
const MAX_TEMP: usize = 48;

/// The RPS subset of a DPB picture for the current picture (8.3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpsSet {
    /// not in the current RPS, only seen inside `begin`, which drops such pictures
    None,
    StCurrBefore,
    StCurrAfter,
    LtCurr,
    /// a reference of later pictures only
    Foll,
}

/// A DPB picture as the hardware takes it (VA's ReferenceFrames, HCP's frame stores).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ref {
    pub slot: Slot,
    pub poc: i32,
    pub long_term: bool,
    pub set: RpsSet,
}

/// RefPicSetStCurrBefore, StCurrAfter and LtCurr in spec order, as indices into `Dpb::refs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CurrSets {
    before: [u8; MAX_DPB],
    n_before: u8,
    after: [u8; MAX_DPB],
    n_after: u8,
    lt: [u8; MAX_DPB],
    n_lt: u8,
}

impl CurrSets {
    const EMPTY: CurrSets = CurrSets {
        before: [0; MAX_DPB],
        n_before: 0,
        after: [0; MAX_DPB],
        n_after: 0,
        lt: [0; MAX_DPB],
        n_lt: 0,
    };

    pub fn before(&self) -> &[u8] {
        &self.before[..self.n_before as usize]
    }

    pub fn after(&self) -> &[u8] {
        &self.after[..self.n_after as usize]
    }

    pub fn lt(&self) -> &[u8] {
        &self.lt[..self.n_lt as usize]
    }

    /// NumPicTotalCurr without pps_curr_pic_ref.
    pub fn total(&self) -> usize {
        (self.n_before + self.n_after + self.n_lt) as usize
    }

    /// RefPicListTemp0 or 1 (8.3.4) before cycling.
    fn order(&self, list: usize, out: &mut [u8; MAX_TEMP]) -> usize {
        let (x, y) =
            if list == 0 { (self.before(), self.after()) } else { (self.after(), self.before()) };
        let mut n = 0;
        for &v in x.iter().chain(y).chain(self.lt()) {
            out[n] = v;
            n += 1;
        }
        n
    }
}

fn push_set(list: &mut [u8; MAX_DPB], n: &mut u8, v: usize) {
    if (*n as usize) < MAX_DPB {
        list[*n as usize] = v as u8;
        *n += 1;
    }
}

/// What the DPB takes from the active SPS, the values of its highest sub-layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seq {
    /// MaxPicOrderCntLsb
    pub max_poc_lsb: i32,
    /// sps_max_num_reorder_pics, pictures waiting for output before the smallest goes
    pub reorder: u8,
    /// SpsMaxLatencyPictures, None for no bound
    pub max_latency: Option<u32>,
    /// sps_max_dec_pic_buffering_minus1 + 1, the pictures stored before the current one goes in
    pub dpb_size: u8,
}

impl Seq {
    pub fn of(sps: &Sps) -> Seq {
        let size = sps.sps_max_dec_pic_buffering_minus1.saturating_add(1);
        Seq {
            max_poc_lsb: sps.max_poc_lsb(),
            reorder: sps.sps_max_num_reorder_pics.min(MAX_DPB as u32) as u8,
            max_latency: sps.max_latency_pictures(),
            dpb_size: size.min(MAX_DPB as u32) as u8,
        }
    }
}

/// The current picture as `begin` derived it, handed back to `lists` and `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Picture {
    pub fate: Fate,
    pub poc: i32,
    /// IRAPs with NoRaslOutputFlag and discards so far, output order is (gop, POC)
    pub gop: u32,
    pub irap: bool,
    /// NoRaslOutputFlag, an IDR, a BLA, the first picture or a CRA after an end of sequence
    pub no_rasl_output: bool,
    pub sets: CurrSets,
    /// the key decoding started at with nothing to reorder behind it, it goes out at once
    key_now: bool,
}

impl Picture {
    const SKIP: Picture = Picture {
        fate: Fate::Skip,
        poc: 0,
        gop: 0,
        irap: false,
        no_rasl_output: false,
        sets: CurrSets::EMPTY,
        key_now: false,
    };
}

/// slice_pic_parameter_set_id of a segment NAL (7.3.6.1), what its full parse needs. `irap`
/// for the no_output_of_prior_pics_flag before it. None for a header too short.
pub fn peek_pps_id(nal: &[u8], irap: bool) -> Option<u32> {
    if nal.len() < 3 {
        return None;
    }
    let mut body = [0u8; 16];
    let n = de_emulate(&nal[2..nal.len().min(18)], &mut body);
    let mut r = BitReader::new(&body[..n]);
    r.flag();
    if irap {
        r.flag();
    }
    Some(r.ue())
}

/// no_output_of_prior_pics_flag of an IRAP's first segment NAL, the bit after
/// first_slice_segment_in_pic_flag.
pub fn peek_no_output_of_prior_pics(nal: &[u8]) -> bool {
    nal.get(2).is_some_and(|b| b & 0x40 != 0)
}

/// An end of sequence or end of bitstream NAL, `Dpb::end_of_sequence` takes either.
pub fn is_eos(nal_type: u8) -> bool {
    matches!(nal_type, hp::NAL_EOS | hp::NAL_EOB)
}

/// Whether an IRAP with NoRaslOutputFlag lets the pictures before it out (C.5.2.2), as ffmpeg
/// decides it: the slice's no_output_of_prior_pics_flag, inferred 1 for a CRA after an end of
/// sequence. A size change does not infer it.
pub fn prior_pics_out(nal_type: u8, no_output_of_prior_pics_flag: bool, after_eos: bool) -> bool {
    !no_output_of_prior_pics_flag && !(nal_type == hp::NAL_CRA_NUT && after_eos)
}

/// The key a discard restarts at, put out as decoded ahead of the reorder, so a seek's first
/// picture is not queued behind the decodes after it. `new` starts `Off`, so a stream's first
/// key waits for its RADL pictures and goes out after them, as the stream orders it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lead {
    Off,
    /// a discard came and no IRAP since
    Flushed,
    /// the key of this sequence and order count went out ahead
    Out(u32, i32),
}

impl Lead {
    /// Whether a picture comes before the key that went out ahead, a decodable leading one.
    fn leads(self, gop: u32, poc: i32) -> bool {
        matches!(self, Lead::Out(g, p) if g == gop && poc < p)
    }
}

/// Sub-layer non-reference, RADL or RASL, none of which may be prevTid0Pic.
fn skips_prev_tid0(nal_type: u8) -> bool {
    (nal_type <= 14 && nal_type.is_multiple_of(2))
        || (hp::NAL_RADL_N..=hp::NAL_RASL_R).contains(&nal_type)
}

/// PicOrderCntVal (8.3.1) with prevTid0Pic as the spec defines it.
#[derive(Clone, Copy, Debug, Default)]
struct PocState {
    prev_lsb: i32,
    prev_msb: i32,
}

impl PocState {
    const ZERO: PocState = PocState { prev_lsb: 0, prev_msb: 0 };

    fn compute(
        &mut self,
        max_poc_lsb: i32,
        poc_lsb: u32,
        nal_type: u8,
        temporal_id: u8,
        irap_no_rasl: bool,
    ) -> i32 {
        let lsb = poc_lsb as i32;
        let msb = if irap_no_rasl {
            0
        } else if lsb < self.prev_lsb && self.prev_lsb - lsb >= max_poc_lsb / 2 {
            self.prev_msb.wrapping_add(max_poc_lsb)
        } else if lsb > self.prev_lsb && lsb - self.prev_lsb > max_poc_lsb / 2 {
            self.prev_msb.wrapping_sub(max_poc_lsb)
        } else {
            self.prev_msb
        };
        if temporal_id == 0 && !skips_prev_tid0(nal_type) {
            self.prev_lsb = lsb;
            self.prev_msb = msb;
        }
        msb.wrapping_add(lsb)
    }
}

#[derive(Clone, Copy)]
struct Waiting {
    slot: Slot,
    gop: u32,
    poc: i32,
    /// PicLatencyCount (C.5.2.3)
    latency: u32,
}

/// Short-term RPS entries of a header in spec order: delta, used and the subset when used.
fn short_term(sh: &SliceHeader) -> impl Iterator<Item = (i32, bool, RpsSet)> + '_ {
    let s0 = sh
        .st_rps
        .delta_poc_s0
        .iter()
        .zip(sh.st_rps.used_s0.iter())
        .map(|(d, u)| (*d, *u, RpsSet::StCurrBefore));
    let s1 = sh
        .st_rps
        .delta_poc_s1
        .iter()
        .zip(sh.st_rps.used_s1.iter())
        .map(|(d, u)| (*d, *u, RpsSet::StCurrAfter));
    s0.chain(s1)
}

/// The PicOrderCntVal a long-term entry names with its MSB (8-5), in i64 as the cycle times
/// MaxPicOrderCntLsb passes i32.
fn lt_target(lt: &hp::LongTermRef, cur_poc: i32, max_poc_lsb: i32) -> i64 {
    let cur_lsb = cur_poc.rem_euclid(max_poc_lsb);
    i64::from(cur_poc)
        - i64::from(lt.delta_poc_msb_cycle) * i64::from(max_poc_lsb)
        - i64::from(cur_lsb - lt.poc_lsb as i32)
}

/// Whether DPB picture `r`, still in subset `set`, is long-term entry `lt`.
fn lt_matches(r: &Ref, set: RpsSet, lt: &hp::LongTermRef, cur_poc: i32, max_poc_lsb: i32) -> bool {
    r.set == set
        && if lt.delta_poc_msb_present {
            i64::from(r.poc) == lt_target(lt, cur_poc, max_poc_lsb)
        } else {
            r.poc.rem_euclid(max_poc_lsb) == lt.poc_lsb as i32
        }
}

/// RPS marking, reference lists and output order of an HEVC stream (8.3.1, 8.3.2, 8.3.4 and
/// C.5.2). No allocation, no hardware, the caller decodes.
pub struct Dpb {
    refs: [Ref; MAX_DPB],
    n_refs: u8,
    waiting: [Waiting; MAX_WAITING],
    n_waiting: u8,
    poc: PocState,
    gop: u32,
    /// no IRAP seen yet, or a discard came
    awaiting_irap: bool,
    lead: Lead,
    after_eos: bool,
    skip_rasl: bool,
    reorder: u8,
    max_latency: Option<u32>,
    dpb_size: u8,
}

impl Dpb {
    /// Waits for an IRAP. Unlike after `discard`, its RADL pictures go out before it as the
    /// stream orders them, so the first picture is out later than after a seek.
    pub const fn new() -> Self {
        Dpb {
            refs: [Ref { slot: Slot(0), poc: 0, long_term: false, set: RpsSet::None }; MAX_DPB],
            n_refs: 0,
            waiting: [Waiting { slot: Slot(0), gop: 0, poc: 0, latency: 0 }; MAX_WAITING],
            n_waiting: 0,
            poc: PocState::ZERO,
            gop: 0,
            awaiting_irap: true,
            lead: Lead::Off,
            after_eos: false,
            skip_rasl: false,
            reorder: 0,
            max_latency: None,
            dpb_size: MAX_DPB as u8,
        }
    }

    /// Starts a picture from its first slice segment header, `inter` when any of its segments
    /// is P or B. Derives POC (8.3.1), marks the RPS (8.3.2) releasing what left it, puts out
    /// or drops what an IRAP ends and bumps for room otherwise (C.5.2.2). Pictures before the
    /// first IRAP and RASL ones of an IRAP decoding started at are `Fate::Skip`, with nothing
    /// changed. A used reference
    /// missing is `MissingReference`, the one error that changes state: the marking, the order
    /// count and the sequence count stay as the stream has them and `ev` holds what the
    /// marking let go. A picture taken here is ended with `end` or `abandon`.
    pub fn begin(
        &mut self,
        seq: &Seq,
        nal: &Nal,
        sh: &SliceHeader,
        inter: bool,
        ev: &mut Events,
    ) -> Result<Picture, DpbError> {
        ev.clear();
        let t = nal.nal_type;
        let irap = hp::is_irap(t);
        if (self.awaiting_irap && !irap) || (hp::is_rasl(t) && self.skip_rasl) {
            return Ok(Picture::SKIP);
        }
        let no_rasl = hp::is_idr(t) || hp::is_bla(t) || self.awaiting_irap || self.after_eos;
        // a key with no leading pictures goes out at once
        let key_now = self.awaiting_irap && matches!(t, hp::NAL_IDR_N_LP | hp::NAL_BLA_N_LP);
        (self.reorder, self.max_latency, self.dpb_size) =
            (seq.reorder, seq.max_latency, seq.dpb_size);
        if irap {
            self.skip_rasl = no_rasl;
        }
        if irap && no_rasl {
            // earlier pictures go out before the order count restarts, unless the IRAP drops them
            if prior_pics_out(t, sh.no_output_of_prior_pics_flag, self.after_eos) {
                self.bump(true, ev);
            } else {
                self.drop_waiting(ev);
            }
            self.drop_refs(|_| false, ev);
            self.gop += 1;
        }
        self.awaiting_irap = false;
        self.after_eos = false;
        let poc = self.poc.compute(
            seq.max_poc_lsb,
            sh.pic_order_cnt_lsb,
            t,
            nal.temporal_id,
            irap && no_rasl,
        );
        let complete = self.mark(sh, poc, seq.max_poc_lsb);
        self.drop_refs(|r| r.set != RpsSet::None, ev);
        // a lost reference (a picture dropped before) leaves the inter pictures after it
        // undecodable up to the next IRAP
        if !complete && inter {
            return Err(DpbError::MissingReference);
        }
        if !(irap && no_rasl) {
            // C.5.2.2, the DPB makes room for the current picture
            while self.n_waiting > 0 && (self.over() || self.fullness() >= self.dpb_size as usize) {
                self.put_out_smallest(ev);
            }
        }
        let shown = sh.pic_output_flag && !self.lead.leads(self.gop, poc);
        Ok(Picture {
            fate: if shown { Fate::Shown } else { Fate::Hidden },
            poc,
            gop: self.gop,
            irap,
            no_rasl_output: no_rasl,
            sets: self.curr_sets(sh, poc, seq.max_poc_lsb),
            key_now,
        })
    }

    /// Whether `begin` skips a picture of `nal_type` before anything else, before the first
    /// IRAP or a RASL picture of the IRAP decoding started at. Lets a caller skip the header
    /// parse.
    pub fn skips(&self, nal_type: u8) -> bool {
        (self.awaiting_irap && !hp::is_irap(nal_type)) || (hp::is_rasl(nal_type) && self.skip_rasl)
    }

    /// The DPB from `begin` to `end`, each picture with its subset for the current one.
    pub fn refs(&self) -> &[Ref] {
        &self.refs[..self.n_refs as usize]
    }

    /// RefPicList0 and 1 of one slice (8.3.4) as indices into `refs`, `NO_REF` past the
    /// active count and on a list the slice type has not. Cycles through the subsets and
    /// applies list_entry_lX, an entry past the temporary list takes its last.
    pub fn lists(
        &self,
        pic: &Picture,
        sh: &SliceHeader,
        l0: &mut [u8; MAX_LIST],
        l1: &mut [u8; MAX_LIST],
    ) {
        *l0 = [NO_REF; MAX_LIST];
        *l1 = [NO_REF; MAX_LIST];
        let m = &sh.ref_list_mods;
        if sh.slice_type.is_inter() {
            let n = sh.num_ref_idx_l0_active_minus1 as usize + 1;
            build_list(&pic.sets, 0, n, m.l0_present.then_some(&m.l0[..]), l0);
        }
        if sh.slice_type == hp::SliceType::B {
            let n = sh.num_ref_idx_l1_active_minus1 as usize + 1;
            build_list(&pic.sets, 1, n, m.l1_present.then_some(&m.l1[..]), l1);
        }
    }

    /// Ends a decoded picture in `slot`. Stores it, queues it for output when it is
    /// `Fate::Shown` and bumps (C.5.2.3). A `Fate::Skip` picture changes nothing. `Full` when
    /// every frame store holds a reference or 17 pictures wait for output, nothing changes then
    /// and the picture is lost unless the caller calls `abandon`. `begin` bumps to the stream's
    /// dpb size (C.5.2.2), a caller out of surfaces before that uses `bump_one`.
    pub fn end(&mut self, pic: &Picture, slot: Slot, ev: &mut Events) -> Result<(), DpbError> {
        ev.clear();
        if pic.fate == Fate::Skip {
            return Ok(());
        }
        let shown = pic.fate == Fate::Shown;
        if self.n_refs as usize == MAX_DPB || (shown && self.n_waiting as usize == MAX_WAITING) {
            return Err(DpbError::Full);
        }
        // every picture enters the DPB, the next RPS decides whether it stays
        self.refs[self.n_refs as usize] =
            Ref { slot, poc: pic.poc, long_term: false, set: RpsSet::Foll };
        self.n_refs += 1;
        if shown {
            // C.5.2.3, the waiting pictures the current one goes out before are a picture later
            for w in &mut self.waiting[..self.n_waiting as usize] {
                if (w.gop, w.poc) > (pic.gop, pic.poc) {
                    w.latency = w.latency.saturating_add(1);
                }
            }
            self.waiting[self.n_waiting as usize] =
                Waiting { slot, gop: pic.gop, poc: pic.poc, latency: 0 };
            self.n_waiting += 1;
        }
        // the key a discard restarts at goes out now, so its readers are queued before the
        // next decode, nothing of its sequence waits before it
        let ahead = pic.irap && self.lead == Lead::Flushed && shown;
        if ahead {
            self.lead = Lead::Out(pic.gop, pic.poc);
        } else if pic.irap {
            self.lead = Lead::Off;
        }
        self.bump(pic.key_now || ahead, ev);
        Ok(())
    }

    /// A picture `begin` took that was never decoded, in place of `end`. The caller keeps its
    /// slot. The picture never enters the DPB, so a later RPS using it is `MissingReference`.
    /// An IRAP with NoRaslOutputFlag waits for an IRAP again, so what followed it is `Skip`
    /// instead of failing one by one.
    pub fn abandon(&mut self, pic: &Picture, ev: &mut Events) {
        ev.clear();
        if pic.fate != Fate::Skip && pic.irap && pic.no_rasl_output {
            self.awaiting_irap = true;
        }
    }

    /// An end of sequence or bitstream NAL. The next IRAP has NoRaslOutputFlag, and a CRA then
    /// puts out no prior pictures.
    pub fn end_of_sequence(&mut self) {
        self.after_eos = true;
    }

    /// A seek or a stream error. Releases every picture without output and waits for an IRAP,
    /// which goes out as soon as it is decoded, its decodable leading pictures are `Hidden`.
    pub fn discard(&mut self, ev: &mut Events) {
        ev.clear();
        self.drop_waiting(ev);
        self.drop_refs(|_| false, ev);
        self.poc = PocState::ZERO;
        self.gop += 1;
        self.awaiting_irap = true;
        self.lead = Lead::Flushed;
        self.after_eos = false;
        self.skip_rasl = false;
    }

    /// End of stream. Puts out every waiting picture in order, the references stay.
    pub fn flush(&mut self, ev: &mut Events) {
        ev.clear();
        self.bump(true, ev);
    }

    /// Puts out the next waiting picture ahead of the reorder depth, for a caller out of
    /// surfaces (the bumping of C.5.2.2 with no empty frame buffer). False when none waits.
    pub fn bump_one(&mut self, ev: &mut Events) -> bool {
        ev.clear();
        if self.n_waiting == 0 {
            return false;
        }
        self.put_out_smallest(ev);
        true
    }

    /// Drops every picture waiting for output without putting it out, the references stay.
    pub fn drop_output(&mut self, ev: &mut Events) {
        ev.clear();
        self.drop_waiting(ev);
    }

    /// Marks every DPB picture with its RPS subset for `sh` (8.3.2). Long-term entries are
    /// matched first against any picture, short-term ones only against pictures not marked
    /// long-term. False when a used reference is missing.
    fn mark(&mut self, sh: &SliceHeader, cur_poc: i32, max_poc_lsb: i32) -> bool {
        let refs = &mut self.refs[..self.n_refs as usize];
        for r in refs.iter_mut() {
            r.set = RpsSet::None;
        }
        let mut complete = true;
        for lt in &sh.long_term_refs {
            match refs.iter_mut().find(|r| lt_matches(r, RpsSet::None, lt, cur_poc, max_poc_lsb)) {
                Some(r) => {
                    r.set = if lt.used_by_curr_pic { RpsSet::LtCurr } else { RpsSet::Foll };
                    r.long_term = true;
                }
                None => complete &= !lt.used_by_curr_pic,
            }
        }
        for (delta, used, set) in short_term(sh) {
            let poc = cur_poc.wrapping_add(delta);
            // pictures still unset and long-term were long-term before and are dropped
            match refs.iter_mut().find(|r| r.set == RpsSet::None && !r.long_term && r.poc == poc) {
                Some(r) => r.set = if used { set } else { RpsSet::Foll },
                None => complete &= !used,
            }
        }
        complete
    }

    /// The used subsets in header order, as `mark` left the DPB.
    fn curr_sets(&self, sh: &SliceHeader, cur_poc: i32, max_poc_lsb: i32) -> CurrSets {
        let refs = self.refs();
        let mut c = CurrSets::default();
        for (delta, used, set) in short_term(sh) {
            if !used {
                continue;
            }
            let poc = cur_poc.wrapping_add(delta);
            let Some(i) = refs.iter().position(|r| r.set == set && r.poc == poc) else {
                continue;
            };
            if set == RpsSet::StCurrAfter {
                push_set(&mut c.after, &mut c.n_after, i);
            } else {
                push_set(&mut c.before, &mut c.n_before, i);
            }
        }
        for lt in sh.long_term_refs.iter().filter(|l| l.used_by_curr_pic) {
            if let Some(i) =
                refs.iter().position(|r| lt_matches(r, RpsSet::LtCurr, lt, cur_poc, max_poc_lsb))
            {
                push_set(&mut c.lt, &mut c.n_lt, i);
            }
        }
        c
    }

    fn waits(&self, slot: Slot) -> bool {
        self.waiting[..self.n_waiting as usize].iter().any(|w| w.slot == slot)
    }

    fn is_ref(&self, slot: Slot) -> bool {
        self.refs().iter().any(|r| r.slot == slot)
    }

    /// Removes the pictures `keep` refuses, in DPB order so frame stores keep theirs.
    fn drop_refs(&mut self, keep: impl Fn(&Ref) -> bool, ev: &mut Events) {
        let mut i = 0;
        while i < self.n_refs as usize {
            if keep(&self.refs[i]) {
                i += 1;
                continue;
            }
            let slot = self.refs[i].slot;
            self.refs.copy_within(i + 1..self.n_refs as usize, i);
            self.n_refs -= 1;
            if !self.waits(slot) {
                ev.push(Event::Release(slot));
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

    /// More than the reorder depth wait, or one waited the most pictures it may (C.5.2.3).
    fn over(&self) -> bool {
        let waiting = &self.waiting[..self.n_waiting as usize];
        self.n_waiting > self.reorder
            || self.max_latency.is_some_and(|m| waiting.iter().any(|w| w.latency >= m))
    }

    /// Pictures stored, references and those waiting for output alone.
    fn fullness(&self) -> usize {
        let waiting = &self.waiting[..self.n_waiting as usize];
        self.n_refs as usize + waiting.iter().filter(|w| !self.is_ref(w.slot)).count()
    }

    /// Puts out the smallest (gop, POC) while `over`, or all.
    fn bump(&mut self, all: bool, ev: &mut Events) {
        while self.n_waiting > 0 && (all || self.over()) {
            self.put_out_smallest(ev);
        }
    }

    fn put_out_smallest(&mut self, ev: &mut Events) {
        let n = self.n_waiting as usize;
        let mut at = 0;
        for i in 1..n {
            let (w, b) = (&self.waiting[i], &self.waiting[at]);
            if (w.gop, w.poc) < (b.gop, b.poc) {
                at = i;
            }
        }
        let slot = self.waiting[at].slot;
        self.waiting.copy_within(at + 1..n, at);
        self.n_waiting -= 1;
        ev.push(Event::Output(slot));
        if !self.is_ref(slot) {
            ev.push(Event::Release(slot));
        }
    }
}

impl Default for Dpb {
    fn default() -> Self {
        Self::new()
    }
}

/// RefPicList0 or 1 (8.3.4). `mods` are list_entry_lX when present.
fn build_list(
    c: &CurrSets,
    list: usize,
    active: usize,
    mods: Option<&[u32]>,
    out: &mut [u8; MAX_LIST],
) {
    *out = [NO_REF; MAX_LIST];
    let mut base = [0u8; MAX_TEMP];
    let n = c.order(list, &mut base);
    if n == 0 {
        return;
    }
    // NumRpsCurrTempListX entries, cycling through the subsets
    let len = active.max(n).min(MAX_TEMP);
    let mut temp = [0u8; MAX_TEMP];
    for (i, t) in temp.iter_mut().enumerate().take(len) {
        *t = base[i % n];
    }
    for (i, d) in out.iter_mut().enumerate().take(active.min(MAX_LIST)) {
        let idx = match mods {
            Some(m) => m.get(i).copied().unwrap_or(0) as usize,
            None => i,
        };
        *d = temp[idx.min(len - 1)];
    }
}

#[cfg(test)]
mod tests;
