//! DVD-Video LPCM, big-endian 16, 20 or 24-bit, to interleaved little-endian `s16` or packed `s24`.
//! 20 and 24-bit samples come in groups of four, the high 16 bits of each then their low bits, two
//! samples a group in mono (the layout FFmpeg's pcm-dvd.c reads). 20-bit samples land in `s24`
//! with four zero low bits.

use std::fmt;

/// The three bytes past the frame count and first access unit pointer.
pub const HEADER: usize = 3;

/// The longest block, seven channels of 24-bit groups.
pub const BLOCK_MAX: usize = 4 * 7 * 3;

const RATES: [u32; 4] = [48_000, 96_000, 44_100, 32_000];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lpcm {
    pub rate: u32,
    pub channels: u8,
    pub bits: u8,
}

/// `Depth` is the reserved quantization word, 28 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LpcmError {
    Short,
    Depth,
}

impl fmt::Display for LpcmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LpcmError::Short => f.write_str("LPCM header cut short"),
            LpcmError::Depth => f.write_str("LPCM quantization word is the reserved one"),
        }
    }
}

impl std::error::Error for LpcmError {}

impl Lpcm {
    /// Emphasis, mute and frame number, then quantization, rate and channels, then dynamic range.
    pub fn parse(h: &[u8]) -> Result<Lpcm, LpcmError> {
        let b = *h.get(1).ok_or(LpcmError::Short)?;
        let bits = 16 + (b >> 6) * 4;
        if bits == 28 {
            return Err(LpcmError::Depth);
        }
        Ok(Lpcm { rate: RATES[usize::from(b >> 4 & 3)], channels: (b & 7) + 1, bits })
    }

    pub fn width(&self) -> usize {
        if self.bits == 16 { 2 } else { 3 }
    }

    pub fn sample(&self) -> &'static str {
        if self.bits == 16 { "s16" } else { "s24" }
    }

    fn block_samples(&self) -> usize {
        let ch = usize::from(self.channels);
        match (self.bits, ch) {
            (16, _) => ch,
            (_, 1 | 2 | 4) => 4,
            (_, 8) => 8,
            _ => 4 * ch,
        }
    }

    pub fn block(&self) -> (usize, usize) {
        let n = self.block_samples();
        (n * usize::from(self.bits) / 8, n * self.width())
    }

    /// Converts the whole blocks of `src` into `dst`, which holds them. Returns `(in, out)` bytes.
    pub fn convert(&self, src: &[u8], dst: &mut [u8]) -> (usize, usize) {
        let (bin, bout) = self.block();
        let blocks = (src.len() / bin).min(dst.len() / bout);
        let (src, dst) = (&src[..blocks * bin], &mut dst[..blocks * bout]);
        match self.bits {
            16 => {
                for (s, d) in src.as_chunks::<2>().0.iter().zip(dst.as_chunks_mut::<2>().0) {
                    *d = [s[1], s[0]];
                }
            }
            bits => {
                // Mono groups hold two samples, the others four.
                let per = if self.channels == 1 { 2 } else { 4 };
                let gin = per * usize::from(bits) / 8;
                for (s, d) in src.chunks_exact(gin).zip(dst.chunks_exact_mut(per * 3)) {
                    group(s, d, per, bits);
                }
            }
        }
        (blocks * bin, blocks * bout)
    }
}

fn group(s: &[u8], d: &mut [u8], per: usize, bits: u8) {
    let low = &s[2 * per..];
    for k in 0..per {
        let lo = match bits {
            20 => (low[k / 2] >> (4 * (1 - k % 2))) << 4,
            _ => low[k],
        };
        d[3 * k..3 * k + 3].copy_from_slice(&[lo, s[2 * k + 1], s[2 * k]]);
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test vectors
mod tests {
    use super::*;

    fn s24(d: &[u8]) -> Vec<i32> {
        d.as_chunks::<3>()
            .0
            .iter()
            .map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8)
            .collect()
    }

    #[test]
    fn the_header_names_depth_rate_and_channels() {
        let l = Lpcm::parse(&[0x0C, 0x01, 0x80]).unwrap();
        assert_eq!(l, Lpcm { rate: 48_000, channels: 2, bits: 16 });
        let l = Lpcm::parse(&[0, 0b1001_0101, 0x80]).unwrap();
        assert_eq!(l, Lpcm { rate: 96_000, channels: 6, bits: 24 });
        assert_eq!(Lpcm::parse(&[0, 0xC0, 0]), Err(LpcmError::Depth));
        assert_eq!(Lpcm::parse(&[0]), Err(LpcmError::Short));
        assert_eq!(Lpcm { rate: 48_000, channels: 6, bits: 20 }.block(), (60, 72));
        assert_eq!(Lpcm { rate: 48_000, channels: 8, bits: 24 }.block(), (24, 24));
        assert_eq!(Lpcm { rate: 48_000, channels: 1, bits: 20 }.block(), (10, 12));
    }

    #[test]
    fn sixteen_bits_swap_and_a_partial_block_waits() {
        let l = Lpcm { rate: 48_000, channels: 2, bits: 16 };
        let mut out = [0u8; 8];
        assert_eq!(l.convert(&[0x12, 0x34, 0xFF, 0xFE, 0x01], &mut out), (4, 4));
        assert_eq!(out[..4], [0x34, 0x12, 0xFE, 0xFF]);
    }

    #[test]
    fn twenty_and_twenty_four_bit_groups_regroup() {
        let l = Lpcm { rate: 48_000, channels: 2, bits: 24 };
        let src = [0x12, 0x34, 0x80, 0x00, 0x7F, 0xFF, 0x00, 0x01, 0xAA, 0xBB, 0xCC, 0xDD];
        let mut out = [0u8; 12];
        assert_eq!(l.convert(&src, &mut out), (12, 12));
        assert_eq!(s24(&out), [0x1234AA, -0x800000 + 0xBB, 0x7FFFCC, 0x0001DD]);
        let l = Lpcm { rate: 48_000, channels: 2, bits: 20 };
        let src = [0x12, 0x34, 0x80, 0x00, 0x7F, 0xFF, 0x00, 0x01, 0xAB, 0xCD];
        assert_eq!(l.convert(&src, &mut out), (10, 12));
        assert_eq!(s24(&out), [0x1234A0, -0x800000 + 0xB0, 0x7FFFC0, 0x0001D0]);
        let l = Lpcm { rate: 48_000, channels: 1, bits: 24 };
        let src = [0x00, 0x01, 0x00, 0x02, 0x11, 0x22, 0x00, 0x03, 0x00, 0x04, 0x33, 0x44];
        assert_eq!(l.convert(&src, &mut out), (12, 12));
        assert_eq!(s24(&out), [0x111, 0x222, 0x333, 0x444]);
    }
}
