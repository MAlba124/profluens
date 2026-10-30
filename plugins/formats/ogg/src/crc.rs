//! The Ogg page checksum (RFC 3533 section 6, field 7).

const POLY: u32 = 0x04C1_1DB7;

struct Table([[u32; 256]; SLICES]);

/// Bytes the sliced inner loop folds per iteration (slicing-by-8, Kounavis and Berry).
const SLICES: usize = 8;

impl Table {
    fn build() -> Self {
        let mut table = [[0u32; 256]; SLICES];
        let mut i = 0usize;
        while i < 256 {
            let mut crc = (i as u32) << 24;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ POLY } else { crc << 1 };
                bit += 1;
            }
            table[0][i] = crc;
            i += 1;
        }
        let mut s = 1usize;
        while s < SLICES {
            let mut j = 0usize;
            while j < 256 {
                let prev = table[s - 1][j];
                table[s][j] = (prev << 8) ^ table[0][(prev >> 24) as usize];
                j += 1;
            }
            s += 1;
        }
        Self(table)
    }
}

fn table() -> &'static [[u32; 256]; SLICES] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Table> = OnceLock::new();
    &TABLE.get_or_init(Table::build).0
}

/// Incremental Ogg CRC-32 state (init 0).
#[derive(Clone, Copy, Debug)]
pub struct Crc32 {
    state: u32,
}

impl Crc32 {
    pub fn new() -> Self {
        Self { state: 0 }
    }

    /// Fold `data` into the running CRC (MSB-first table step, section 6).
    pub fn update(&mut self, data: &[u8]) {
        let table = table();
        let mut crc = self.state;

        let (blocks, rest) = data.as_chunks::<SLICES>();
        for c in blocks {
            let a = u32::from_be_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
            let b = u32::from_be_bytes([c[4], c[5], c[6], c[7]]);
            crc = table[7][(a >> 24) as u8 as usize]
                ^ table[6][(a >> 16) as u8 as usize]
                ^ table[5][(a >> 8) as u8 as usize]
                ^ table[4][a as u8 as usize]
                ^ table[3][(b >> 24) as u8 as usize]
                ^ table[2][(b >> 16) as u8 as usize]
                ^ table[1][(b >> 8) as u8 as usize]
                ^ table[0][b as u8 as usize];
        }

        for &byte in rest {
            let idx = ((crc >> 24) as u8 ^ byte) as usize;
            crc = (crc << 8) ^ table[0][idx];
        }
        self.state = crc;
    }

    /// The running CRC value.
    pub fn value(&self) -> u32 {
        self.state
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot Ogg CRC-32 over `data`, init 0, no reflection, no final XOR (section 6).
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn table_step_matches_bitwise() {
        fn bitwise(state: u32, byte: u8) -> u32 {
            let mut crc = state ^ ((byte as u32) << 24);
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ POLY } else { crc << 1 };
            }
            crc
        }
        for top in 0u32..256 {
            for byte in 0u8..=255 {
                let state = top << 24;
                let mut c = Crc32 { state };
                c.update(&[byte]);
                assert_eq!(c.value(), bitwise(state, byte), "top={top} byte={byte}");
            }
        }
    }

    #[test]
    fn sliced_matches_byte_at_a_time_at_every_length() {
        fn byte_at_a_time(data: &[u8]) -> u32 {
            let t = table();
            let mut crc = 0u32;
            for &b in data {
                crc = (crc << 8) ^ t[0][((crc >> 24) as u8 ^ b) as usize];
            }
            crc
        }
        let data: Vec<u8> =
            (0..600u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for len in 0..data.len() {
            assert_eq!(crc32(&data[..len]), byte_at_a_time(&data[..len]), "len={len}");
        }
    }

    #[test]
    fn incremental_equals_oneshot() {
        let data: Vec<u8> = (0..500u32).map(|i| (i.wrapping_mul(31) ^ (i >> 3)) as u8).collect();
        let whole = crc32(&data);
        for split in [0usize, 1, 27, 255, 256, 499, 500] {
            let mut c = Crc32::new();
            c.update(&data[..split]);
            c.update(&data[split..]);
            assert_eq!(c.value(), whole, "split at {split}");
        }
    }

    #[test]
    fn known_answer_ascii_123456789() {
        assert_eq!(crc32(b"123456789"), 0x89A1_897F);
    }

    #[test]
    fn known_answer_single_bytes() {
        assert_eq!(crc32(&[0x00]), 0);
        assert_eq!(crc32(&[0x80]), table()[0][0x80]);
        assert_eq!(crc32(&[0xFF]), table()[0][0xFF]);
    }
}
