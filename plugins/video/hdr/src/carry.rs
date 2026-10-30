//! Side data carried from a unit to its picture through a decoder, keyed by pts.

use crate::side::SIDE_CAP;

pub const ENTRIES: usize = 32;

const FREE: u64 = u64::MAX;

pub struct Carry {
    keys: [u64; ENTRIES],
    lens: [u16; ENTRIES],
    ages: [u64; ENTRIES],
    clock: u64,
    arena: Box<[u8]>,
    pub evicted: u64,
}

impl Default for Carry {
    fn default() -> Self {
        Carry::new()
    }
}

impl Carry {
    // One arena per decoder, at its construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Carry {
        Carry {
            keys: [FREE; ENTRIES],
            lens: [0; ENTRIES],
            ages: [0; ENTRIES],
            clock: 0,
            arena: vec![0u8; ENTRIES * SIDE_CAP].into_boxed_slice(),
            evicted: 0,
        }
    }

    fn seat(&mut self, pts: u64) -> usize {
        if let Some(i) = self.keys.iter().position(|&k| k == pts) {
            return i;
        }
        if let Some(i) = self.keys.iter().position(|&k| k == FREE) {
            return i;
        }
        let oldest = (0..ENTRIES).min_by_key(|&i| self.ages[i]).unwrap_or(0);
        self.evicted += 1;
        oldest
    }

    /// Keep `side` for the picture of `pts`, replacing what that pts had.
    pub fn put(&mut self, pts: u64, side: &[u8]) {
        if side.is_empty() || pts == FREE {
            return;
        }
        let i = self.seat(pts);
        let n = fit(side);
        self.arena[i * SIDE_CAP..i * SIDE_CAP + n].copy_from_slice(&side[..n]);
        self.keys[i] = pts;
        self.lens[i] = n as u16;
        self.clock += 1;
        self.ages[i] = self.clock;
    }

    /// The side data of the picture of `pts`, freeing its entry.
    pub fn take(&mut self, pts: u64) -> Option<&[u8]> {
        if pts == FREE {
            return None;
        }
        let i = self.keys.iter().position(|&k| k == pts)?;
        self.keys[i] = FREE;
        let n = usize::from(self.lens[i]);
        Some(&self.arena[i * SIDE_CAP..i * SIDE_CAP + n])
    }

    pub fn clear(&mut self) {
        self.keys = [FREE; ENTRIES];
    }

    pub fn len(&self) -> usize {
        self.keys.iter().filter(|&&k| k != FREE).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn fit(side: &[u8]) -> usize {
    if side.len() <= SIDE_CAP {
        return side.len();
    }
    let mut end = 0;
    for (_, body) in crate::side::records(side) {
        let next = end + 3 + body.len();
        if next > SIDE_CAP {
            break;
        }
        end = next;
    }
    end
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::side::{Kind, Writer};

    fn record(byte: u8, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len + 3];
        let mut w = Writer::new(&mut buf);
        w.push(Kind::T35, &vec![byte; len]);
        buf
    }

    #[test]
    fn a_picture_takes_its_own_units_side_data_in_any_order() {
        let mut c = Carry::new();
        c.put(40, &record(4, 10));
        c.put(0, &record(0, 10));
        c.put(20, &record(2, 10));
        assert_eq!(c.take(0).unwrap()[3], 0);
        assert_eq!(c.take(20).unwrap()[3], 2);
        assert_eq!(c.take(20), None, "taken once");
        assert_eq!(c.take(40).unwrap()[3], 4);
        assert!(c.is_empty());
    }

    #[test]
    fn the_oldest_goes_when_full_and_a_flush_clears() {
        let mut c = Carry::new();
        for pts in 0..ENTRIES as u64 + 1 {
            c.put(pts, &record(pts as u8, 4));
        }
        assert_eq!(c.evicted, 1);
        assert_eq!(c.take(0), None, "the oldest went");
        assert_eq!(c.take(1).unwrap()[3], 1);
        c.clear();
        assert_eq!(c.take(5), None);
    }

    #[test]
    fn nothing_is_kept_for_no_pts_or_no_bytes() {
        let mut c = Carry::new();
        c.put(u64::MAX, &record(1, 4));
        c.put(7, &[]);
        assert!(c.is_empty());
        assert_eq!(c.take(u64::MAX), None);
    }

    #[test]
    fn too_much_is_cut_at_a_record() {
        let mut big = record(1, SIDE_CAP - 3 - 10);
        big.extend(record(2, 20));
        let mut c = Carry::new();
        c.put(1, &big);
        assert_eq!(c.take(1).unwrap().len(), SIDE_CAP - 10);
    }
}
