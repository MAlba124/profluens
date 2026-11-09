//! Presentation stamps for an AVI's video. AVI places a chunk by its index alone, in decode
//! order, so a stream with B-frames presents out of it. Each unit keeps its place in decode order
//! and is stamped with the slot of the unit at its place in presentation order, the k-th picture
//! shown takes the k-th unit's chunk time. DivX and XviD packed B-frames are unpacked first, the
//! second VOP of a packed chunk takes the place and the time of the chunk after it, the
//! placeholder N-VOP that chunk holds is dropped.

use std::collections::VecDeque;

use profluens_video::bits::{BitReader, de_emulate};
use profluens_video::h264parse::{self, Nal, PocState, Pps, Sps};

/// How a stream's presentation order follows from its units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// MPEG-4 part 2, a B-VOP shows as it is decoded, an I, P or S VOP once the next of them is.
    Vop,
    /// H.264 by picture order count, the lowest waiting shows once more than the reorder depth
    /// wait, an IDR shows all before it first.
    Poc,
}

impl Order {
    pub fn of(family: &str) -> Option<Self> {
        match family {
            crate::codec::FAMILY_MPEG4_ASP => Some(Self::Vop),
            "h264/annexb" => Some(Self::Poc),
            _ => None,
        }
    }
}

/// What becomes of a chunk's bytes, in the order given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// Bytes `from..to` of the chunk are the next unit, `key` its keyframe flag, None to keep
    /// the chunk's.
    Unit { from: usize, to: usize, key: Option<bool> },
    /// The stashed B-VOP is the next unit, never a key.
    Unstash,
    /// Bytes `from..to` wait as the stash for the chunk that holds their place.
    Stash { from: usize, to: usize },
    /// The stash goes, the placeholder holding its place never came.
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vop {
    I,
    P,
    B,
    S,
}

/// VOPs a chunk holds that are looked at, a packed one holds two.
const MOST_VOPS: usize = 3;

/// Units held at most, past it the oldest shows at once. A real stream holds a B-frame run and
/// a reorder depth, a corrupt one could hold the first unit for good.
pub const MOST_HELD: usize = 32;

/// The stamps of one video stream's units.
pub struct Retime {
    order: Order,
    /// Per unit not yet out, decode order, its stamp once shown.
    stamps: VecDeque<Option<u64>>,
    /// Chunk times of the units not yet shown, decode order.
    slots: VecDeque<u64>,
    /// Decode index of the first of `stamps`, and of the next unit.
    base: u64,
    next: u64,
    /// The last slot taken, a unit shown with none left follows it by `step`.
    last: Option<u64>,
    /// The last chunk time given.
    last_in: Option<u64>,
    step: u64,
    /// Bits of vop_time_increment, from the last VOL.
    time_bits: Option<u32>,
    /// The I, P or S VOP waiting for the next one.
    pending: Option<u64>,
    stashed: bool,
    /// Pictures not shown, IDR epoch, POC and decode index.
    waiting: Vec<(u32, i32, u64)>,
    epoch: u32,
    /// A memory_management_control_operation 5 ended the epoch, the next picture starts one.
    mmco5: bool,
    poc: PocState,
    sps: Vec<Sps>,
    pps: Vec<Pps>,
    nals: Vec<Nal>,
}

impl Retime {
    /// A stream of `order` whose chunks are `step` ns apart, `head` its codec head, a VOL or
    /// parameter sets, in band ones are read as they come.
    // Cold path, once per stream.
    #[allow(clippy::disallowed_methods)]
    pub fn new(order: Order, step: u64, head: &[u8]) -> Self {
        let mut r = Self {
            order,
            stamps: VecDeque::with_capacity(MOST_HELD + 4),
            slots: VecDeque::with_capacity(MOST_HELD + 4),
            base: 0,
            next: 0,
            last: None,
            last_in: None,
            step,
            time_bits: None,
            pending: None,
            stashed: false,
            waiting: Vec::with_capacity(17),
            epoch: 0,
            mmco5: false,
            poc: PocState::new(),
            sps: Vec::new(),
            pps: Vec::new(),
            nals: Vec::with_capacity(16),
        };
        match order {
            Order::Vop => r.time_bits = starts(head).2.and_then(|at| vol_time_bits(head, at)),
            Order::Poc => r.param_sets(head),
        }
        r
    }

    /// Forgets the units of the run before a seek, the parameter sets and the VOL stay.
    pub fn reset(&mut self) {
        self.stamps.clear();
        self.slots.clear();
        (self.base, self.next, self.last, self.last_in) = (0, 0, None, None);
        (self.pending, self.stashed) = (None, false);
        self.waiting.clear();
        (self.epoch, self.mmco5) = (0, false);
        self.poc.reset();
    }

    /// Units given and not yet out.
    pub fn held(&self) -> usize {
        self.stamps.len()
    }

    /// The chunk `data` at `slot` ns, what becomes of its bytes onto `acts`.
    pub fn chunk(&mut self, data: &[u8], slot: u64, acts: &mut Vec<Act>) {
        match self.order {
            Order::Vop => self.vop_chunk(data, slot, acts),
            Order::Poc => self.poc_chunk(data, slot, acts),
        }
        if self.stamps.len() > MOST_HELD {
            self.show_front();
        }
    }

    /// The first unit not yet out shows now if it waits, a held queue that must drain.
    pub fn show_front(&mut self) {
        if self.stamps.front() != Some(&None) {
            return;
        }
        let idx = self.base;
        if self.pending == Some(idx) {
            self.pending = None;
        }
        self.waiting.retain(|w| w.2 != idx);
        self.show(idx);
    }

    /// The stamp of the first unit not yet out once it is shown.
    pub fn peek(&self) -> Option<u64> {
        *self.stamps.front()?
    }

    /// The stamp of the first unit not yet out once it is shown, which then is out.
    pub fn take(&mut self) -> Option<u64> {
        let stamp = (*self.stamps.front()?)?;
        self.stamps.pop_front();
        self.base += 1;
        Some(stamp)
    }

    /// The stream ended, every unit is shown, a stash left goes as a unit onto `acts`.
    pub fn finish(&mut self, acts: &mut Vec<Act>) {
        if std::mem::take(&mut self.stashed) {
            acts.push(Act::Unstash);
            let slot = self.last_in.map_or(0, |l| l + self.step);
            self.decoded(slot, Some(Vop::B));
        }
        if let Some(p) = self.pending.take() {
            self.show(p);
        }
        self.show_waiting(0);
    }

    fn vop_chunk(&mut self, data: &[u8], slot: u64, acts: &mut Vec<Act>) {
        let (vops, n, vol) = starts(data);
        if let Some(bits) = vol.and_then(|at| vol_time_bits(data, at)) {
            self.time_bits = Some(bits);
        }
        let kind = |at: usize| match data.get(at + 4).map(|b| b >> 6) {
            Some(0) => Vop::I,
            Some(2) => Vop::B,
            Some(3) => Vop::S,
            _ => Vop::P,
        };
        match n {
            // a DivX placeholder, a stuffing byte, holds a stashed B-VOP's place
            0 if self.stashed => {
                self.stashed = false;
                acts.push(Act::Unstash);
                self.decoded(slot, Some(Vop::B));
            }
            0 if data.len() <= 1 => {}
            // no VOP to order by, it goes as it came and shows as decoded
            0 => {
                acts.push(Act::Unit { from: 0, to: data.len(), key: None });
                self.decoded(slot, Some(Vop::B));
            }
            1 => {
                let coded = self.coded(data, vops[0]);
                if self.stashed {
                    acts.push(Act::Unstash);
                    self.decoded(slot, Some(Vop::B));
                    // a packed stream's next B-VOP waits a chunk in turn, an N-VOP ends it
                    match coded {
                        true => acts.push(Act::Stash { from: 0, to: data.len() }),
                        false => self.stashed = false,
                    }
                    return;
                }
                // not coded, the picture before shows on
                if !coded {
                    return;
                }
                let k = kind(vops[0]);
                acts.push(Act::Unit { from: 0, to: data.len(), key: Some(k == Vop::I) });
                self.decoded(slot, Some(k));
            }
            _ => {
                if self.stashed {
                    acts.push(Act::Drop);
                }
                let k = kind(vops[0]);
                acts.push(Act::Unit { from: 0, to: vops[1], key: Some(k == Vop::I) });
                self.decoded(slot, Some(k));
                acts.push(Act::Stash { from: vops[1], to: data.len() });
                self.stashed = true;
            }
        }
    }

    /// Whether the VOP at `at` is coded, vop_coded (ISO/IEC 14496-2 6.2.5). Without a VOL to
    /// size its time increment, an N-VOP is told by its few bytes.
    fn coded(&self, data: &[u8], at: usize) -> bool {
        let Some(bits) = self.time_bits else {
            return data.len() - at > 7;
        };
        let mut r = BitReader::new(&data[at + 4..]);
        r.u(2);
        let mut seconds = 0;
        while r.flag() {
            seconds += 1;
            if seconds > 60 {
                return true;
            }
        }
        r.u(1);
        r.u(bits);
        r.u(1);
        r.flag()
    }

    fn poc_chunk(&mut self, data: &[u8], slot: u64, acts: &mut Vec<Act>) {
        acts.push(Act::Unit { from: 0, to: data.len(), key: None });
        let mut nals = std::mem::take(&mut self.nals);
        h264parse::split_nals_into(data, &mut nals);
        let mut picture = None;
        for n in &nals {
            match n.unit_type {
                h264parse::NAL_SPS | h264parse::NAL_PPS => self.param_set(n.raw(data)),
                h264parse::NAL_SLICE_NON_IDR | h264parse::NAL_SLICE_IDR if picture.is_none() => {
                    picture = Some(self.picture_order(n, data));
                }
                _ => {}
            }
        }
        self.nals = nals;
        let idx = self.decoded(slot, None);
        let Some(Some((epoch, poc, reorder))) = picture else {
            // no picture to order, it shows after all before it
            self.show_waiting(0);
            self.show(idx);
            return;
        };
        if epoch != self.epoch {
            self.show_waiting(0);
            self.epoch = epoch;
        }
        let at = self.waiting.partition_point(|&(e, p, _)| (e, p) <= (epoch, poc));
        self.waiting.insert(at, (epoch, poc, idx));
        self.show_waiting(reorder);
    }

    /// The epoch, POC and reorder depth of the slice `n`, None when its sets are unknown. An
    /// IDR or a memory_management_control_operation 5 starts an epoch.
    fn picture_order(&mut self, n: &Nal, data: &[u8]) -> Option<(u32, i32, usize)> {
        let raw = n.raw(data);
        let mut prefix = [0u8; 24];
        let len = de_emulate(raw.get(1..)?, &mut prefix);
        let mut r = BitReader::new(&prefix[..len]);
        r.ue();
        r.ue();
        let pps_id = r.ue();
        let pps = self.pps.iter().find(|p| p.pic_parameter_set_id == pps_id)?;
        let sps = self.sps.iter().find(|s| s.seq_parameter_set_id == pps.seq_parameter_set_id)?;
        let sh = h264parse::parse_slice_header(raw, n.unit_type, n.ref_idc, sps, pps)?;
        let idr = n.unit_type == h264parse::NAL_SLICE_IDR;
        let (top, bottom) = self.poc.compute(sps, &sh, idr, n.ref_idc);
        let epoch = self.epoch + u32::from(idr || std::mem::take(&mut self.mmco5));
        if sh.mmco_ops.iter().any(|m| m.op == 5) {
            self.mmco5 = true;
            self.poc.reset();
        }
        let reorder = sps.reorder_frames() as usize;
        Some((epoch, top.min(bottom), reorder))
    }

    fn param_sets(&mut self, head: &[u8]) {
        let mut nals = std::mem::take(&mut self.nals);
        h264parse::split_nals_into(head, &mut nals);
        for n in &nals {
            if matches!(n.unit_type, h264parse::NAL_SPS | h264parse::NAL_PPS) {
                self.param_set(n.raw(head));
            }
        }
        self.nals = nals;
    }

    /// An SPS or PPS takes the place of the one of its id.
    fn param_set(&mut self, nal: &[u8]) {
        match nal.first().map(|b| b & 0x1F) {
            Some(h264parse::NAL_SPS) => {
                if let Some(s) = h264parse::parse_sps(nal) {
                    self.sps.retain(|o| o.seq_parameter_set_id != s.seq_parameter_set_id);
                    self.sps.push(s);
                }
            }
            Some(h264parse::NAL_PPS) => {
                if let Some(p) = h264parse::parse_pps(nal) {
                    self.pps.retain(|o| o.pic_parameter_set_id != p.pic_parameter_set_id);
                    self.pps.push(p);
                }
            }
            _ => {}
        }
    }

    /// A unit at `slot` in decode order, shown at once or later by its kind. Its index.
    fn decoded(&mut self, slot: u64, kind: Option<Vop>) -> u64 {
        let idx = self.next;
        self.next += 1;
        self.stamps.push_back(None);
        self.slots.push_back(slot);
        self.last_in = Some(slot);
        match kind {
            Some(Vop::B) => self.show(idx),
            Some(_) => {
                if let Some(p) = self.pending.replace(idx) {
                    self.show(p);
                }
            }
            None => {}
        }
        idx
    }

    /// The pictures waiting past `keep`, lowest first.
    fn show_waiting(&mut self, keep: usize) {
        while self.waiting.len() > keep {
            let (_, _, idx) = self.waiting.remove(0);
            self.show(idx);
        }
    }

    /// The unit `idx` is the next shown, it takes the first slot left.
    fn show(&mut self, idx: u64) {
        let slot = match self.slots.pop_front() {
            Some(s) => s,
            None => self.last.map_or(0, |l| l + self.step),
        };
        self.last = Some(slot);
        if let Some(s) = idx.checked_sub(self.base).and_then(|i| self.stamps.get_mut(i as usize)) {
            *s = Some(slot);
        }
    }
}

/// The VOP start codes of `data`, up to `MOST_VOPS` of them and how many, and where a VOL
/// starts.
fn starts(data: &[u8]) -> ([usize; MOST_VOPS], usize, Option<usize>) {
    let (mut vops, mut n, mut vol) = ([0usize; MOST_VOPS], 0usize, None);
    let mut at = 0usize;
    while let Some(i) = data.get(at..).and_then(|d| d.windows(3).position(|w| w == [0, 0, 1])) {
        let p = at + i;
        match data.get(p + 3) {
            Some(0xB6) if n < MOST_VOPS => {
                vops[n] = p;
                n += 1;
            }
            Some(0xB6) => n += 1,
            Some(0x20..=0x2F) if vol.is_none() => vol = Some(p),
            _ => {}
        }
        at = p + 3;
    }
    (vops, n, vol)
}

/// The bits of vop_time_increment the VOL at `at` sizes, from its vop_time_increment_resolution
/// (ISO/IEC 14496-2 6.2.3).
fn vol_time_bits(data: &[u8], at: usize) -> Option<u32> {
    let mut r = BitReader::new(data.get(at + 4..)?);
    r.u(1);
    r.u(8);
    let verid = if r.flag() {
        let v = r.u(4);
        r.u(3);
        v
    } else {
        1
    };
    if r.u(4) == 15 {
        r.u(16);
    }
    if r.flag() {
        r.u(3);
        if r.flag() {
            // the VBV parameters
            for n in [15, 1, 15, 1, 15, 1, 3, 11, 1, 15, 1] {
                r.u(n);
            }
        }
    }
    if r.u(2) == 3 && verid != 1 {
        r.u(4);
    }
    r.u(1);
    let resolution = r.u(16);
    match resolution {
        0 => None,
        1 => Some(1),
        n => Some(32 - (n - 1).leading_zeros()),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests;
