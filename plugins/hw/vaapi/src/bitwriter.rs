//! MSB-first bitstream writer for the H.264/H.265 packed headers VA-API encode takes from the
//! application.

pub struct BitWriter {
    bytes: Vec<u8>,
    free: u32,
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BitWriter {
    // Cold, one writer per parameter set, or one a session keeps and clears per frame.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        BitWriter { bytes: Vec::with_capacity(64), free: 0 }
    }

    /// Empties the writer and keeps its storage.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.free = 0;
    }

    pub fn bit_len(&self) -> u32 {
        self.bytes.len() as u32 * 8 - self.free
    }

    pub fn u(&mut self, val: u32, n: u32) {
        debug_assert!(n <= 32);
        debug_assert!(n == 32 || val < (1u64 << n) as u32, "u({val},{n}) overflows the field");
        for i in (0..n).rev() {
            let bit = (val >> i) & 1;
            if self.free == 0 {
                self.bytes.push(0);
                self.free = 8;
            }
            self.free -= 1;
            if let Some(last) = self.bytes.last_mut() {
                *last |= (bit as u8) << self.free;
            }
        }
    }

    pub fn flag(&mut self, b: bool) {
        self.u(b as u32, 1);
    }

    /// `ue(v)`, H.264 section 9.1.
    pub fn ue(&mut self, v: u32) {
        let code = v as u64 + 1;
        let bits = 64 - code.leading_zeros(); // floor(log2(code)) + 1
        self.u(0, bits - 1);
        if bits > 32 {
            self.u((code >> 32) as u32, bits - 32);
            self.u(code as u32, 32);
        } else {
            self.u(code as u32, bits);
        }
    }

    /// `se(v)`, H.264 section 9.1.1.
    pub fn se(&mut self, v: i32) {
        let code = if v > 0 { (v as u32) * 2 - 1 } else { (-(v as i64) as u32) * 2 };
        self.ue(code);
    }

    pub fn rbsp_trailing_bits(&mut self) {
        self.flag(true);
        while self.free != 0 {
            self.flag(false);
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Appends `rbsp` to `out` with `emulation_prevention_three_byte`s inserted (H.264 7.4.1.1).
pub fn push_ebsp(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0u32;
    for &b in rbsp {
        if zeros >= 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

// Cold, once per parameter set a session writes.
#[allow(clippy::disallowed_methods)]
pub fn annexb_nal(header: &[u8], rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + header.len() + rbsp.len() + 8);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(header);
    push_ebsp(rbsp, &mut out);
    out
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn to_ebsp(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        push_ebsp(rbsp, &mut out);
        out
    }

    #[test]
    fn ue_matches_spec_table() {
        // H.264 Table 9-1.
        let cases: [(u32, &[u8], u32); 6] = [
            (0, &[0b1000_0000], 1), // "1"
            (1, &[0b0100_0000], 3), // "010"
            (2, &[0b0110_0000], 3), // "011"
            (3, &[0b0010_0000], 5), // "00100"
            (4, &[0b0010_1000], 5), // "00101"
            (8, &[0b0001_0010], 7), // "0001001"
        ];
        for (v, bytes, bits) in cases {
            let mut w = BitWriter::new();
            w.ue(v);
            assert_eq!(w.bit_len(), bits, "ue({v}) bit length");
            assert_eq!(w.as_bytes(), bytes, "ue({v}) bits");
        }
    }

    #[test]
    fn se_maps_signed_per_9_1_1() {
        // H.264 Table 9-3.
        for (v, code) in [(0i32, 0u32), (1, 1), (-1, 2), (2, 3), (-2, 4), (3, 5), (-3, 6)] {
            let mut a = BitWriter::new();
            a.se(v);
            let mut b = BitWriter::new();
            b.ue(code);
            assert_eq!(a.as_bytes(), b.as_bytes(), "se({v}) == ue({code})");
        }
    }

    #[test]
    fn trailing_bits_align() {
        let mut w = BitWriter::new();
        w.u(0b101, 3);
        w.rbsp_trailing_bits();
        assert_eq!(w.as_bytes(), &[0b1011_0000]);
        assert_eq!(w.bit_len(), 8);

        let mut w = BitWriter::new();
        w.u(0xAB, 8);
        w.rbsp_trailing_bits();
        assert_eq!(w.as_bytes(), &[0xAB, 0x80]);
    }

    #[test]
    fn ebsp_guards_start_codes() {
        assert_eq!(to_ebsp(&[0, 0, 0]), &[0, 0, 3, 0]);
        assert_eq!(to_ebsp(&[0, 0, 1]), &[0, 0, 3, 1]);
        assert_eq!(to_ebsp(&[0, 0, 2]), &[0, 0, 3, 2]);
        assert_eq!(to_ebsp(&[0, 0, 3]), &[0, 0, 3, 3]);
        assert_eq!(to_ebsp(&[0, 0, 4]), &[0, 0, 4]); // > 3: legal, untouched
        assert_eq!(to_ebsp(&[1, 0, 0]), &[1, 0, 0]); // trailing zeros: no third byte
        assert_eq!(to_ebsp(&[0, 0, 0, 0, 1]), &[0, 0, 3, 0, 0, 3, 1]);
    }

    #[test]
    fn annexb_frames_header_and_payload() {
        let nal = annexb_nal(&[0x67], &[0x42, 0x00, 0x00, 0x00]);
        assert_eq!(nal, &[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x00, 0x03, 0x00]);
    }
}
