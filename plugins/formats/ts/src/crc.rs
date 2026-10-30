//! CRC-32 of the PSI sections, H.222.0 Annex A.

const POLY: u32 = 0x04C1_1DB7;

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 0x8000_0000 != 0 { (c << 1) ^ POLY } else { c << 1 };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// Register starts at all ones, MSB first, no final xor, so a section with its CRC_32 yields zero.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = (c << 8) ^ TABLE[usize::from((c >> 24) as u8 ^ b)];
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_value() {
        assert_eq!(crc32(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn appended_crc_gives_zero() {
        let mut section =
            [0x00u8, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00, 0, 0, 0, 0];
        let crc = crc32(&section[..12]);
        section[12..].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(crc32(&section), 0);
        section[5] ^= 0x02;
        assert_ne!(crc32(&section), 0);
    }

    #[test]
    fn empty_input_is_the_preset() {
        assert_eq!(crc32(&[]), 0xFFFF_FFFF);
    }
}
