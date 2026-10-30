//! The NAL units of an Annex B byte stream (ITU-T H.264 and H.265 Annex B).
//! Reads headers only, emulation prevention bytes are left in place.

pub struct Nals<'a> {
    rest: &'a [u8],
}

/// Walk the NAL units of `unit`, skipping bytes ahead of the first start code.
pub fn nals(unit: &[u8]) -> Nals<'_> {
    let rest = start_code(unit).map_or(&[][..], |at| &unit[at + 3..]);
    Nals { rest }
}

/// Where the next `00 00 01` start code prefix begins (B.1.1).
fn start_code(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|w| w == [0, 0, 1])
}

impl<'a> Iterator for Nals<'a> {
    type Item = &'a [u8];

    // Trailing zero bytes stay on the unit, they may lead the next start code.
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let (nal, rest) = match start_code(self.rest) {
            Some(end) => (&self.rest[..end], &self.rest[end + 3..]),
            None => (self.rest, &[][..]),
        };
        self.rest = rest;
        Some(nal)
    }
}

#[cfg(test)]
mod tests {
    // Test code.
    #![allow(clippy::disallowed_methods)]

    use super::*;

    fn walk(unit: &[u8]) -> Vec<&[u8]> {
        nals(unit).collect()
    }

    #[test]
    fn units_are_split_at_three_and_four_byte_start_codes() {
        let unit = [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x41, 0x9A, 0, 0, 0, 1, 0x01];
        assert_eq!(walk(&unit), [&[0x09, 0xF0][..], &[0x41, 0x9A, 0][..], &[0x01][..]]);
    }

    #[test]
    fn bytes_with_no_start_code_are_no_unit() {
        assert!(walk(&[]).is_empty());
        assert!(walk(&[0x41, 0x9A, 0x00]).is_empty());
        assert!(walk(&[0, 0, 1]).is_empty(), "a start code and nothing after");
        assert_eq!(walk(&[0xFF, 0, 0, 1, 0x41]), [&[0x41][..]], "junk ahead is passed over");
    }

    #[test]
    fn back_to_back_start_codes_yield_an_empty_unit() {
        assert_eq!(walk(&[0, 0, 1, 0, 0, 1, 0x41]), [&[][..], &[0x41][..]]);
    }
}
