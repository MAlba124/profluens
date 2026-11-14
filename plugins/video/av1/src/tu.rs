//! What a temporal unit shows, read from its OBU headers, and the last shown picture of each unit
//! picked out of what dav1d puts out.
//!
//! A temporal unit shows one frame per spatial layer and the decoder outputs the last of them,
//! the operating point's highest layer (AV1 7.18). dav1d's own pick, `all_layers` 0, keys on
//! spatial ids and the temporal delimiter's flag, so it drops pictures of streams whose operating
//! point names layers its frames do not carry, or whose unit starts with a hidden frame. dav1d
//! runs with `all_layers` 1 and [`Last`] keeps the last picture of every unit that shows more
//! than one.

/// What a unit's frame headers are read against, from the last sequence header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seq {
    /// `reduced_still_picture_header`, one shown key frame and no `show_existing_frame` bit.
    pub reduced: bool,
    /// `operating_point_idc[0]`, the layers of operating point 0 that dav1d decodes.
    pub idc: u16,
}

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;

/// MSB first reader over one OBU's payload, `None` past its end.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.at >> 3)?;
        let v = (byte >> (7 - (self.at & 7))) & 1;
        self.at += 1;
        Some(u32::from(v))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0, |v, _| Some(v << 1 | self.bit()?))
    }

    /// uvlc() of AV1 4.10.3.
    fn uvlc(&mut self) -> Option<()> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return None;
            }
        }
        self.bits(zeros).map(drop)
    }
}

/// leb128() of AV1 4.10.5 at the front of `data`, the value and its length.
fn leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut v = 0u64;
    for (i, b) in data.iter().take(8).enumerate() {
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some((usize::try_from(v).ok()?, i + 1));
        }
    }
    None
}

/// sequence_header_obu (AV1 5.5.1) as far as the first operating point.
fn sequence(payload: &[u8]) -> Option<Seq> {
    let mut r = Bits { data: payload, at: 0 };
    r.bits(4)?; // seq_profile, still_picture
    if r.bit()? == 1 {
        return Some(Seq { reduced: true, idc: 0 });
    }
    if r.bit()? == 1 {
        r.bits(32)?; // num_units_in_display_tick
        r.bits(32)?; // time_scale
        if r.bit()? == 1 {
            r.uvlc()?; // num_ticks_per_picture_minus_1
        }
        if r.bit()? == 1 {
            r.bits(5 + 32 + 5 + 5)?; // decoder_model_info
        }
    }
    r.bits(1 + 5)?; // initial_display_delay_present_flag, operating_points_cnt_minus_1
    let idc = r.bits(12)? as u16;
    Some(Seq { reduced: false, idc })
}

/// Whether a frame header payload shows a frame (AV1 5.9.2). A frame OBU never shows an
/// existing frame, dav1d refuses one that says so.
fn shows(payload: &[u8], seq: Seq, frame_obu: bool) -> bool {
    if seq.reduced {
        return true;
    }
    let mut r = Bits { data: payload, at: 0 };
    match r.bit() {
        Some(1) => !frame_obu,
        Some(_) => r.bits(2).and_then(|_| r.bit()) == Some(1),
        None => false,
    }
}

/// Frames `unit` shows to operating point 0, as dav1d decodes them. A sequence header in it
/// updates `seq` for what follows. A frame header repeated between tile groups counts again,
/// which [`Last`] only holds back the longer for.
pub fn shown(unit: &[u8], seq: &mut Seq) -> u32 {
    let (mut at, mut count) = (0, 0);
    while let Some(&h) = unit.get(at) {
        let kind = (h >> 3) & 0xF;
        let ext = unit.get(at + 1).copied().filter(|_| h & 0x04 != 0);
        let head = at + 1 + usize::from(ext.is_some());
        let (size, n) = match h & 0x02 {
            0 => (unit.len().saturating_sub(head), 0),
            _ => match unit.get(head..).and_then(leb128) {
                Some(s) => s,
                None => break,
            },
        };
        let Some(payload) = unit.get(head + n..head + n + size) else {
            break;
        };
        at = head + n + size;
        if kind == OBU_SEQUENCE_HEADER {
            if let Some(s) = sequence(payload) {
                *seq = s;
            }
            continue;
        }
        // dav1d skips an OBU of a layer the operating point leaves out (obu.c).
        if let Some(e) = ext
            && seq.idc != 0
        {
            let (temporal, spatial) = (e >> 5, (e >> 3) & 3);
            if seq.idc >> temporal & 1 == 0 || seq.idc >> (spatial + 8) & 1 == 0 {
                continue;
            }
        }
        let frame_obu = kind == OBU_FRAME;
        if (frame_obu || kind == OBU_FRAME_HEADER) && shows(payload, *seq, frame_obu) {
            count += 1;
        }
    }
    count
}

/// Units in flight that show more than one frame, a ring of the newest.
const MULTI: usize = 16;

/// The last picture of each unit, pictures tagged with their unit's serial. A unit that shows one
/// frame passes its picture at once. One that shows more holds each back until the next of the
/// same unit, its count reached or a later unit's picture coming, then lets the last go.
pub struct Last<T> {
    /// Serial and shown frames of the units that show more than one, oldest first.
    multi: [(i64, u32); MULTI],
    len: usize,
    /// The newest picture of a unit in `multi`, its serial and the pictures still to come.
    pending: Option<(i64, u32, T)>,
}

impl<T> Default for Last<T> {
    fn default() -> Self {
        Last { multi: [(0, 0); MULTI], len: 0, pending: None }
    }
}

impl<T> Last<T> {
    /// Unit `serial` was sent and shows `frames`.
    pub fn note(&mut self, serial: i64, frames: u32) {
        if frames < 2 {
            return;
        }
        if self.len == MULTI {
            self.multi.copy_within(1.., 0);
            self.len -= 1;
        }
        self.multi[self.len] = (serial, frames);
        self.len += 1;
    }

    /// The frames unit `serial` shows when it shows more than one. Older units drop out.
    fn take_multi(&mut self, serial: i64) -> Option<u32> {
        let older = self.multi[..self.len].iter().take_while(|(s, _)| *s < serial).count();
        self.multi.copy_within(older..self.len, 0);
        self.len -= older;
        match self.multi[..self.len].first() {
            Some(&(s, frames)) if s == serial => {
                self.multi.copy_within(1..self.len, 0);
                self.len -= 1;
                Some(frames)
            }
            _ => None,
        }
    }

    /// Takes picture `pic` of unit `serial`, `emit` gets what goes out now, in order.
    pub fn take(&mut self, serial: i64, pic: T, mut emit: impl FnMut(T)) {
        let left = match self.pending.take() {
            Some((s, left, _)) if s == serial => left.saturating_sub(1),
            Some((_, _, old)) => {
                emit(old);
                self.fresh(serial)
            }
            None => self.fresh(serial),
        };
        match left {
            0 => emit(pic),
            _ => self.pending = Some((serial, left, pic)),
        }
    }

    /// Pictures still to come of unit `serial` behind its first, 0 for a unit showing one.
    fn fresh(&mut self, serial: i64) -> u32 {
        self.take_multi(serial).map_or(0, |frames| frames - 1)
    }

    /// The end of the stream, the held picture goes.
    pub fn flush(&mut self, emit: impl FnOnce(T)) {
        self.len = 0;
        if let Some((_, _, pic)) = self.pending.take() {
            emit(pic);
        }
    }

    /// A flush of the decoder, everything held drops.
    pub fn clear(&mut self) {
        self.len = 0;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    // Test code, bit fields grouped by header field.
    #![allow(clippy::disallowed_methods, clippy::unusual_byte_groupings)]

    use super::*;

    /// One OBU with a size field, `ext` its extension byte.
    fn obu(out: &mut Vec<u8>, kind: u8, ext: Option<u8>, payload: &[u8]) {
        out.push(kind << 3 | 0x02 | if ext.is_some() { 0x04 } else { 0 });
        out.extend(ext);
        out.push(payload.len() as u8);
        out.extend_from_slice(payload);
    }

    #[test]
    fn a_sequence_header_gives_its_first_operating_point() {
        // profile 0, no timing, one operating point of idc 0x547
        let bits = 0b000_0_0_0_0_00000_010101000111u32 << 8;
        let seq = sequence(&bits.to_be_bytes()).expect("parsed");
        assert_eq!(seq, Seq { reduced: false, idc: 0x547 });
        let reduced = [0b0000_1000, 0];
        assert_eq!(sequence(&reduced), Some(Seq { reduced: true, idc: 0 }));
        // timing info, an equal picture interval of uvlc 2 and a decoder model ahead of idc 1
        let fields = [(5, 0), (1, 1), (64, 7), (1, 1), (3, 0b011), (1, 1), (47, 3), (6, 0)];
        let bytes = pack(&[&fields[..], &[(12, 1)]].concat());
        assert_eq!(sequence(&bytes), Some(Seq { reduced: false, idc: 1 }));
        assert_eq!(sequence(&bytes[..10]), None, "a cut header is no header");
    }

    /// Fields of `(bits, value)` packed MSB first.
    fn pack(fields: &[(u32, u64)]) -> Vec<u8> {
        let mut bits = Vec::new();
        for &(n, v) in fields {
            bits.extend((0..n).rev().map(|i| (v >> i & 1) as u8));
        }
        bits.chunks(8).map(|c| c.iter().enumerate().fold(0, |b, (i, x)| b | x << (7 - i))).collect()
    }

    #[test]
    fn shown_frames_count_shown_and_existing_frames_alone() {
        let mut seq = Seq::default();
        let mut tu = Vec::new();
        obu(&mut tu, 2, None, &[]);
        obu(&mut tu, OBU_FRAME, None, &[0b0_01_0_0000, 0xAA]); // hidden
        obu(&mut tu, OBU_FRAME, None, &[0b0_01_1_0000, 0xAA]); // shown
        obu(&mut tu, OBU_FRAME_HEADER, None, &[0b1_000_0000]); // shows an existing one
        obu(&mut tu, OBU_FRAME, None, &[0b1_000_0000]); // refused by dav1d
        obu(&mut tu, 7, None, &[0b0_00_1_0000]); // redundant
        assert_eq!(shown(&tu, &mut seq), 2);
        seq.reduced = true;
        assert_eq!(shown(&tu, &mut seq), 4, "every frame of a reduced still header shows");
    }

    #[test]
    fn layers_outside_operating_point_0_are_not_counted() {
        let mut seq = Seq::default();
        let mut tu = Vec::new();
        let head = 0b000_0_0_0_0_00000_000100000001u32 << 8; // spatial 0, temporal 0
        obu(&mut tu, OBU_SEQUENCE_HEADER, None, &head.to_be_bytes());
        obu(&mut tu, OBU_FRAME, Some(0), &[0b0_00_1_0000]);
        obu(&mut tu, OBU_FRAME, Some(1 << 3), &[0b0_01_1_0000]); // spatial 1
        obu(&mut tu, OBU_FRAME, Some(1 << 5), &[0b0_01_1_0000]); // temporal 1
        obu(&mut tu, OBU_FRAME, None, &[0b0_01_1_0000]);
        assert_eq!(shown(&tu, &mut seq), 2);
        assert_eq!(seq.idc, 0x101);
        assert_eq!(shown(&[0x32, 0x09, 1], &mut seq), 0, "a cut OBU ends the scan");
    }

    fn run(last: &mut Last<u32>, pics: &[(i64, u32)]) -> Vec<u32> {
        let mut out = Vec::new();
        for &(serial, pic) in pics {
            last.take(serial, pic, |p| out.push(p));
        }
        out
    }

    #[test]
    fn only_the_last_picture_of_a_unit_goes_and_none_waits_for_a_unit_of_one() {
        let mut last = Last::default();
        last.note(0, 1);
        last.note(1, 3);
        last.note(2, 0);
        last.note(3, 2);
        let out = run(&mut last, &[(0, 10), (1, 11), (1, 12)]);
        assert_eq!(out, [10], "unit 1 holds its pictures until its third");
        let out = run(&mut last, &[(1, 13)]);
        assert_eq!(out, [13]);
        // unit 3 lost a picture, the next unit's lets the one held go
        let out = run(&mut last, &[(3, 30), (4, 40)]);
        assert_eq!(out, [30, 40]);
        last.note(5, 2);
        assert_eq!(run(&mut last, &[(5, 50)]), Vec::<u32>::new());
        let mut out = Vec::new();
        last.flush(|p| out.push(p));
        assert_eq!(out, [50], "the end lets the held one go");
        last.note(6, 2);
        run(&mut last, &[(6, 60)]);
        last.clear();
        assert_eq!(run(&mut last, &[(6, 61)]), [61], "a flush forgets every unit");
    }

    #[test]
    fn the_ring_keeps_the_newest_units() {
        let mut last = Last::default();
        for s in 0..MULTI as i64 + 4 {
            last.note(s, 2);
        }
        assert_eq!(run(&mut last, &[(0, 1)]), [1], "the oldest fell out and passes at once");
        assert_eq!(run(&mut last, &[(4, 2)]), Vec::<u32>::new());
    }
}
