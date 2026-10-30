//! Big endian MSB first bit reader. Reading past the end is `None`.

#[derive(Clone)]
pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Bits<'a> {
        Bits { data, pos: 0 }
    }

    pub fn left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    /// `n` bits, at most 32.
    pub fn u(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        if (n as usize) > self.left() {
            return None;
        }
        let mut v = 0u64;
        for _ in 0..n {
            let byte = self.data[self.pos / 8];
            v = v << 1 | u64::from(byte >> (7 - self.pos % 8) & 1);
            self.pos += 1;
        }
        Some(v as u32)
    }

    pub fn flag(&mut self) -> Option<bool> {
        Some(self.u(1)? == 1)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        if n > self.left() {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// Exp-Golomb `ue(v)` (H.264 9.1), up to 32 bits of value.
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = u64::from(self.u(zeros)?);
        u32::try_from((1u64 << zeros) - 1 + rest).ok()
    }
}

/// Copy `nal` into `out` without emulation prevention bytes (H.264 7.4.1, H.265 7.4.2).
pub fn unescape_into(nal: &[u8], out: &mut [u8]) -> usize {
    let (mut zeros, mut n) = (0, 0);
    for &b in nal {
        if n == out.len() {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out[n] = b;
        n += 1;
    }
    n
}

#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    n: usize,
}

impl BitWriter {
    /// `bits` low bits of `v`, MSB first.
    pub fn put(&mut self, v: u32, bits: u32) -> &mut Self {
        for i in (0..bits).rev() {
            if self.n.is_multiple_of(8) {
                self.out.push(0);
            }
            let last = self.out.len() - 1;
            self.out[last] |= ((v >> i & 1) as u8) << (7 - self.n % 8);
            self.n += 1;
        }
        self
    }

    pub fn ue(&mut self, v: u32) -> &mut Self {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.put(0, len - 1);
        for i in (0..len).rev() {
            self.put((x >> i & 1) as u32, 1);
        }
        self
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first_across_bytes() {
        let mut b = Bits::new(&[0b1010_0000, 0xFF]);
        assert_eq!(b.u(3), Some(0b101));
        assert_eq!(b.u(9), Some(0b0_0000_1111));
        assert_eq!(b.left(), 4);
        assert_eq!(b.u(5), None, "past the end");
    }

    #[test]
    fn exp_golomb_per_h264_table_9_2() {
        let mut b = Bits::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
    }

    #[test]
    fn emulation_prevention_is_removed() {
        let mut out = [0u8; 8];
        let n = unescape_into(&[0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00], &mut out);
        assert_eq!(&out[..n], &[0, 0, 1, 0, 0, 0]);
        assert_eq!(unescape_into(&[1, 2, 3, 4], &mut out[..2]), 2, "bounded by the output");
    }
}
