use pf_hdr::emit::{self, Dropped, Keep};
use pf_hdr::rpu::Fit;
use pf_hdr::side::SIDE_CAP;
use profluens_video::color::{StreamColor, TransferFn};

use crate::enc::DEPTH_MAX;

const FREE: u64 = u64::MAX;
// escaping grows a record by half at most, the static SEI fits in the slack
const BEFORE: usize = 2 * SIDE_CAP + 256;
const AFTER: usize = 2 * SIDE_CAP;

/// What a session writes of its pictures' side records, HDR10+ where its VUI states PQ and
/// Dolby Vision RPUs, where `dovi` asks for them, under PQ or HLG. A tone mapped output
/// carries none.
pub(crate) fn keeps(vui: &StreamColor, dovi: Option<Fit>) -> Keep {
    Keep {
        hdr10plus: vui.transfer == Some(TransferFn::Pq),
        dovi: dovi.filter(|_| vui.transfer.is_some_and(|t| t.is_hdr())),
    }
}

/// The side records of the pictures in flight, a slot each by its seq, and the room the NAL
/// units they become are written in.
pub(crate) struct Sides {
    seqs: [u64; DEPTH_MAX],
    lens: [u16; DEPTH_MAX],
    bytes: Box<[u8]>,
    before: Box<[u8]>,
    after: Box<[u8]>,
    /// The session's, set where it opens.
    pub rule: Keep,
    pub dropped: Dropped,
}

impl Default for Sides {
    fn default() -> Self {
        Sides::new()
    }
}

impl Sides {
    // Once per encoder.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Sides {
        Sides {
            seqs: [FREE; DEPTH_MAX],
            lens: [0; DEPTH_MAX],
            bytes: vec![0u8; DEPTH_MAX * SIDE_CAP].into_boxed_slice(),
            before: vec![0u8; BEFORE].into_boxed_slice(),
            after: vec![0u8; AFTER].into_boxed_slice(),
            rule: Keep::default(),
            dropped: Dropped::default(),
        }
    }

    fn slot(seq: u64) -> usize {
        (seq % DEPTH_MAX as u64) as usize
    }

    /// Keeps `side` for picture `seq`, nothing where the rule takes none. Every picture queued
    /// calls it, so a slot a flushed picture left is written over before it is read.
    pub fn keep(&mut self, seq: u64, side: &[u8]) {
        let i = Self::slot(seq);
        let n = if self.rule.any() { side.len().min(SIDE_CAP) } else { 0 };
        self.bytes[i * SIDE_CAP..i * SIDE_CAP + n].copy_from_slice(&side[..n]);
        self.seqs[i] = if n == 0 { FREE } else { seq };
        self.lens[i] = n as u16;
    }

    pub fn has(&self, seq: u64) -> bool {
        self.seqs[Self::slot(seq)] == seq
    }

    /// Puts `sei` and the NAL units of picture `seq`'s records in front of the slices of `au`,
    /// an Annex B access unit, and those that go behind them at its end. Frees the slot.
    pub fn dress(&mut self, seq: u64, sei: Option<&[u8]>, au: &mut Vec<u8>) {
        let sei = sei.unwrap_or(&[]);
        let mut before = sei.len().min(BEFORE);
        self.before[..before].copy_from_slice(&sei[..before]);
        let mut after = 0;
        let i = Self::slot(seq);
        if self.seqs[i] == seq {
            self.seqs[i] = FREE;
            let side = &self.bytes[i * SIDE_CAP..i * SIDE_CAP + usize::from(self.lens[i])];
            let w = emit::h265_side(
                side,
                &self.rule,
                &mut self.before[before..],
                &mut self.after,
                &mut self.dropped,
            );
            before += w.before;
            after = w.after;
        }
        if before > 0 {
            insert_before_slices(au, &self.before[..before]);
        }
        au.extend_from_slice(&self.after[..after]);
    }
}

/// `nal` put in Annex B `au` before its first VCL NAL, after the parameter sets.
pub(crate) fn insert_before_slices(au: &mut Vec<u8>, nal: &[u8]) {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i..i + 3] == [0, 0, 1] {
            if (au[i + 3] >> 1) & 0x3F < 32 {
                let at = if i > 0 && au[i - 1] == 0 { i - 1 } else { i };
                au.splice(at..at, nal.iter().copied());
                return;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    au.extend_from_slice(nal);
}

#[cfg(test)]
// Test vectors.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use pf_hdr::h26x::{Codec, Scan, annexb_nals, scan_annexb};
    use pf_hdr::rpu::{Sample, nal_form, sample};
    use pf_hdr::side::{FrameMeta, Kind, Writer};

    const PLUS: Keep = Keep { hdr10plus: true, dovi: None };
    const HALF: Fit = Fit { from: (3840, 2160), turns: 0, to: (1920, 1080) };

    fn sides(rule: Keep) -> Sides {
        Sides { rule, ..Sides::new() }
    }

    const VPS: u8 = 32;
    const SPS: u8 = 33;
    const PPS: u8 = 34;
    const SEI: u8 = 39;
    const IDR: u8 = 19;
    const TRAIL: u8 = 1;

    fn nal(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut n = vec![0, 0, 0, 1, kind << 1, 1];
        n.extend_from_slice(body);
        n
    }

    fn kinds(au: &[u8]) -> Vec<u8> {
        annexb_nals(au).map(|n| n[0] >> 1 & 0x3F).collect()
    }

    fn hdr10plus_side(maxscl: u32) -> Vec<u8> {
        let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
        t35.extend(pf_hdr::hdr10plus::sample_payload([maxscl, 1, 1], 1000));
        let mut buf = [0u8; 1024];
        let mut w = Writer::new(&mut buf);
        assert!(w.push(Kind::T35, &t35));
        let n = w.len();
        buf[..n].to_vec()
    }

    fn maxscl_of(au: &[u8]) -> Option<u32> {
        let (mut side, mut scratch) = ([0u8; 2048], [0u8; 2048]);
        let mut scan = Scan::new(Writer::new(&mut side));
        scan_annexb(Codec::H265, au, &mut scratch, &mut scan);
        let n = scan.side.len();
        let m = FrameMeta::read(&side[..n]).ok()?;
        Some(m.hdr10plus()?.ok()?.windows[0].maxscl[0])
    }

    fn static_sei() -> Vec<u8> {
        let mut rbsp = vec![144, 4, 0x03, 0xE8, 0x01, 0x90, 0x80];
        let mut n = vec![0, 0, 0, 1, SEI << 1, 1];
        n.append(&mut rbsp);
        n
    }

    #[test]
    fn a_key_picture_has_its_sets_the_static_sei_its_hdr10plus_then_the_slice() {
        let mut s = sides(PLUS);
        s.keep(7, &hdr10plus_side(40_000));
        let sei = static_sei();
        let mut au =
            [nal(VPS, &[1]), nal(SPS, &[2]), nal(PPS, &[3]), nal(IDR, &[0xAF, 0])].concat();
        s.dress(7, Some(&sei), &mut au);
        assert_eq!(kinds(&au), [VPS, SPS, PPS, SEI, SEI, IDR]);
        let nals: Vec<&[u8]> = annexb_nals(&au).collect();
        assert_eq!(nals[3], &sei[4..], "the static SEI first");
        assert_eq!(maxscl_of(&au), Some(40_000));
        assert!(!s.has(7), "the slot is freed");
    }

    #[test]
    fn a_picture_between_keys_carries_its_hdr10plus_alone_before_each_slice() {
        let mut s = sides(PLUS);
        s.keep(2, &hdr10plus_side(1000));
        let mut au = [nal(TRAIL, &[0x80, 1]), nal(TRAIL, &[0x40, 2])].concat();
        s.dress(2, None, &mut au);
        assert_eq!(kinds(&au), [SEI, TRAIL, TRAIL]);
        assert_eq!(maxscl_of(&au), Some(1000));
    }

    #[test]
    fn a_picture_without_records_is_left_as_it_is() {
        let mut s = sides(PLUS);
        s.keep(3, &[]);
        let mut au = nal(TRAIL, &[0x80]);
        let was = au.clone();
        assert!(!s.has(3));
        s.dress(3, None, &mut au);
        assert_eq!(au, was);
    }

    #[test]
    fn each_picture_in_flight_takes_its_own_records_and_a_reused_slot_is_not_read() {
        let mut s = sides(PLUS);
        for seq in 0..DEPTH_MAX as u64 {
            s.keep(seq, &hdr10plus_side(1000 + seq as u32));
        }
        for seq in 0..DEPTH_MAX as u64 {
            let mut au = nal(TRAIL, &[0x80]);
            s.dress(seq, None, &mut au);
            assert_eq!(maxscl_of(&au), Some(1000 + seq as u32));
        }
        // a flushed picture's slot, the next to land there wrote it over
        s.keep(1, &hdr10plus_side(5));
        s.keep(1 + DEPTH_MAX as u64, &[]);
        let mut au = nal(TRAIL, &[0x80]);
        s.dress(1, None, &mut au);
        assert_eq!(kinds(&au), [TRAIL]);
    }

    #[test]
    fn a_multi_window_record_leaves_the_static_sei_and_is_counted() {
        let mut w = pf_hdr::bits::BitWriter::default();
        w.put(1, 8).put(3, 2);
        for _ in 0..2 {
            for v in [0u32, 0, 100, 100, 50, 50] {
                w.put(v, 16);
            }
            w.put(0, 8).put(10, 16).put(20, 16).put(10, 16).put(0, 1);
        }
        w.put(1000, 27).put(0, 1);
        for _ in 0..3 {
            for v in [100u32, 200, 300, 150] {
                w.put(v, 17);
            }
            w.put(0, 4).put(1, 10);
        }
        w.put(0, 1);
        for _ in 0..3 {
            w.put(0, 1).put(0, 1);
        }
        let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
        t35.extend(w.into_bytes());
        let mut buf = [0u8; 512];
        let mut rec = Writer::new(&mut buf);
        assert!(rec.push(Kind::T35, &t35));
        let n = rec.len();
        let mut s = sides(PLUS);
        s.keep(0, &buf[..n]);
        let sei = static_sei();
        let mut au = [nal(VPS, &[1]), nal(IDR, &[0xAF])].concat();
        s.dress(0, Some(&sei), &mut au);
        assert_eq!(kinds(&au), [VPS, SEI, IDR]);
        assert_eq!(maxscl_of(&au), None);
        assert_eq!(s.dropped.windows, 1);
    }

    #[test]
    fn an_rpu_goes_behind_the_last_slice_and_hdr10plus_before_the_first() {
        let mut buf = [0u8; 2048];
        let mut w = Writer::new(&mut buf);
        let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
        t35.extend(pf_hdr::hdr10plus::sample_payload([9000, 1, 1], 1000));
        assert!(
            w.push(Kind::DoviRpu, &nal_form(&sample(Sample { nlq: Some(false), ..Sample::P8 })))
        );
        assert!(w.push(Kind::DoviEl, &[]));
        assert!(w.push(Kind::T35, &t35));
        let n = w.len();
        let mut s = sides(Keep { hdr10plus: true, dovi: Some(HALF) });
        s.keep(4, &buf[..n]);
        let mut au = [nal(TRAIL, &[0x80, 1]), nal(TRAIL, &[0x40, 2])].concat();
        s.dress(4, None, &mut au);
        assert_eq!(kinds(&au), [SEI, TRAIL, TRAIL, 62]);
        assert_eq!(maxscl_of(&au), Some(9000));
        let (mut side, mut scratch) = ([0u8; 2048], [0u8; 2048]);
        let mut scan = Scan::new(Writer::new(&mut side));
        scan_annexb(Codec::H265, &au, &mut scratch, &mut scan);
        let n = scan.side.len();
        let m = FrameMeta::read(&side[..n]).unwrap();
        let want = sample(Sample { area: Some([0, 0, 138, 138]), ..Sample::P8 });
        assert_eq!(m.dovi_rpu().map(|(_, b)| b.to_vec()), Some(nal_form(&want)));
        assert!(!m.dovi_el());
        assert_eq!(s.dropped, Dropped::default());
    }

    #[test]
    fn a_session_that_keeps_nothing_holds_no_records() {
        let mut s = sides(Keep::default());
        s.keep(1, &hdr10plus_side(1000));
        assert!(!s.has(1));
        let mut au = nal(TRAIL, &[0x80]);
        let was = au.clone();
        s.dress(1, None, &mut au);
        assert_eq!(au, was);
    }

    #[test]
    fn hdr10plus_goes_under_pq_and_rpus_under_pq_or_hlg_where_asked() {
        let with = |t| StreamColor { transfer: Some(t), ..StreamColor::default() };
        let fit = Some(HALF);
        assert_eq!(keeps(&with(TransferFn::Pq), None), PLUS);
        assert_eq!(keeps(&with(TransferFn::Pq), fit), Keep { hdr10plus: true, dovi: fit });
        assert_eq!(keeps(&with(TransferFn::Hlg), fit), Keep { hdr10plus: false, dovi: fit });
        assert_eq!(keeps(&with(TransferFn::Hlg), None), Keep::default());
        assert_eq!(keeps(&with(TransferFn::Bt709), fit), Keep::default());
        assert_eq!(keeps(&StreamColor::default(), fit), Keep::default());
    }
}
