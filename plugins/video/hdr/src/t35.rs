//! ITU-T T.35 messages, carrying HDR10+ and Dolby Vision RPUs.

/// `itu_t_t35_country_code` of the United States.
pub const COUNTRY_US: u8 = 0xB5;
/// The terminal provider code of Samsung, whose application 4 is ST 2094-40 (HDR10+).
pub const PROVIDER_SAMSUNG: u16 = 0x003C;
/// The terminal provider code of Dolby.
pub const PROVIDER_DOLBY: u16 = 0x003B;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum T35Kind<'a> {
    Hdr10Plus { payload: &'a [u8] },
    DoviRpu { payload: &'a [u8] },
    Other,
}

/// One T.35 message from its country code on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct T35<'a> {
    pub country: u8,
    /// `itu_t_t35_country_code_extension_byte`, present when the country is 0xFF.
    pub country_ext: Option<u8>,
    pub rest: &'a [u8],
}

impl<'a> T35<'a> {
    /// Split `msg`, `None` when it has no country code.
    pub fn split(msg: &'a [u8]) -> Option<T35<'a>> {
        let (&country, rest) = msg.split_first()?;
        if country != 0xFF {
            return Some(T35 { country, country_ext: None, rest });
        }
        let (&ext, rest) = rest.split_first()?;
        Some(T35 { country, country_ext: Some(ext), rest })
    }

    /// The kind and payload, [`T35Kind::Other`] when too short for its provider codes.
    pub fn kind(&self) -> T35Kind<'a> {
        let r = self.rest;
        if self.country != COUNTRY_US || r.len() < 2 {
            return T35Kind::Other;
        }
        match u16::from_be_bytes([r[0], r[1]]) {
            PROVIDER_SAMSUNG => match r.get(2..5) {
                Some([0x00, 0x01, 0x04]) if r.len() > 5 => T35Kind::Hdr10Plus { payload: &r[5..] },
                _ => T35Kind::Other,
            },
            PROVIDER_DOLBY => match r.get(2..6) {
                Some([0x00, 0x00, 0x08, 0x00]) if r.len() > 6 => {
                    T35Kind::DoviRpu { payload: &r[6..] }
                }
                _ => T35Kind::Other,
            },
            _ => T35Kind::Other,
        }
    }
}

pub fn classify(msg: &[u8]) -> T35Kind<'_> {
    T35::split(msg).map_or(T35Kind::Other, |t| t.kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr10plus_needs_every_code() {
        let msg = [0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x01, 0x40];
        assert_eq!(classify(&msg), T35Kind::Hdr10Plus { payload: &[0x01, 0x40] });
        for i in 0..6 {
            let mut m = msg;
            m[i] ^= 0x10;
            assert_eq!(classify(&m), T35Kind::Other, "byte {i}");
        }
        assert_eq!(classify(&msg[..6]), T35Kind::Other, "no payload");
    }

    #[test]
    fn the_dolby_rpu_in_av1() {
        let msg = [0xB5, 0x00, 0x3B, 0x00, 0x00, 0x08, 0x00, 0x37, 0xCD];
        assert_eq!(classify(&msg), T35Kind::DoviRpu { payload: &[0x37, 0xCD] });
        let mut m = msg;
        m[5] = 0x09;
        assert_eq!(classify(&m), T35Kind::Other);
    }

    #[test]
    fn a_country_extension_is_kept() {
        let t = T35::split(&[0xFF, 0x12, 0x34]).unwrap();
        assert_eq!((t.country_ext, t.rest), (Some(0x12), &[0x34][..]));
        assert_eq!(t.kind(), T35Kind::Other);
        assert_eq!(T35::split(&[0xFF]), None);
        assert_eq!(T35::split(&[]), None);
    }
}
