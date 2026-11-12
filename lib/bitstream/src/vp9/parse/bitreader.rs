use super::Error;

#[derive(Debug)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.bit_pos
    }

    pub(crate) fn read_bits(&mut self, n: u32) -> Result<u32, Error> {
        debug_assert!(n <= 32);
        let mut value: u32 = 0;
        for _ in 0..n {
            value = (value << 1) | self.read_bit()?;
        }
        Ok(value)
    }

    pub(crate) fn read_flag(&mut self) -> Result<bool, Error> {
        Ok(self.read_bit()? != 0)
    }

    /// `s(n)`, spec section 4.9.2: magnitude then sign bit. `n` <= 31.
    pub(crate) fn read_signed(&mut self, n: u32) -> Result<i32, Error> {
        debug_assert!(n <= 31);
        let magnitude = self.read_bits(n)? as i32;
        let sign = self.read_bit()? != 0;
        Ok(if sign { -magnitude } else { magnitude })
    }

    /// `trailing_bits()`, spec section 6.1.1. Set padding bits are invalid (section 7.1.1).
    pub(crate) fn trailing_bits(&mut self) -> Result<(), Error> {
        while self.bit_pos & 7 != 0 {
            if self.read_bit()? != 0 {
                return Err(Error::InvalidBitstream);
            }
        }
        Ok(())
    }

    fn read_bit(&mut self) -> Result<u32, Error> {
        let byte_index = self.bit_pos >> 3;
        if byte_index >= self.data.len() {
            return Err(Error::UnexpectedEof);
        }
        let bit_in_byte = 7 - (self.bit_pos & 7);
        let bit = (self.data[byte_index] >> bit_in_byte) & 1;
        self.bit_pos += 1;
        Ok(bit as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msb_first_byte_aligned() {
        let mut r = BitReader::new(&[0xA5]);
        assert_eq!(r.read_bits(8).unwrap(), 0xA5);
        assert_eq!(r.position(), 8);
    }

    #[test]
    fn msb_first_across_byte_boundary() {
        let mut r = BitReader::new(&[0xA5, 0x5A]);
        assert_eq!(r.read_bits(4).unwrap(), 0xA);
        assert_eq!(r.read_bits(8).unwrap(), 0x55);
        assert_eq!(r.read_bits(4).unwrap(), 0xA);
        assert_eq!(r.position(), 16);
    }

    #[test]
    fn eof_returns_error() {
        let mut r = BitReader::new(&[0xFF]);
        assert!(r.read_bits(8).is_ok());
        assert_eq!(r.read_bits(1).unwrap_err(), Error::UnexpectedEof);
    }

    #[test]
    fn signed_round_trips_positive_and_negative() {
        // +10 then -3 as two s(6) values, 14 bits.
        let bits: [u32; 14] = [
            0, 0, 1, 0, 1, 0, // magnitude 10
            0, // sign +
            0, 0, 0, 0, 1, 1, // magnitude 3
            1, // sign -
        ];
        let mut bytes = vec![0u8; 2];
        for (i, b) in bits.iter().enumerate() {
            let bit_in_byte = 7 - (i & 7);
            bytes[i / 8] |= (*b as u8) << bit_in_byte;
        }
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_signed(6).unwrap(), 10);
        assert_eq!(r.read_signed(6).unwrap(), -3);
    }

    #[test]
    fn trailing_bits_accepts_zero_padding() {
        let mut r = BitReader::new(&[0b1010_0000]);
        let _ = r.read_bits(3).unwrap();
        r.trailing_bits().unwrap();
        assert_eq!(r.position(), 8);
    }

    #[test]
    fn trailing_bits_rejects_nonzero_padding() {
        let mut r = BitReader::new(&[0b1010_0100]);
        let _ = r.read_bits(3).unwrap();
        assert_eq!(r.trailing_bits().unwrap_err(), Error::InvalidBitstream);
    }

    #[test]
    fn trailing_bits_noop_when_aligned() {
        let mut r = BitReader::new(&[0xFF, 0x00]);
        let _ = r.read_bits(8).unwrap();
        r.trailing_bits().unwrap();
        assert_eq!(r.position(), 8);
    }
}
