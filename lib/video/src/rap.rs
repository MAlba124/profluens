use crate::annexb;
use crate::h265parse::is_irap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalSyntax {
    Avc,
    Hevc,
}

/// The NAL length field size an avcC or hvcC states, 4 when it states none.
pub fn length_size(syntax: NalSyntax, config: &[u8]) -> u8 {
    let at = match syntax {
        NalSyntax::Avc => 4,
        NalSyntax::Hevc => 21,
    };
    config.get(at).map_or(4, |b| (b & 3) + 1)
}

/// Whether a sample opens at a random access point. IDR, CRA and BLA pictures are one, so is
/// an H.264 picture a recovery point SEI leads, an I slice of a trailing picture is not.
/// None when its NAL units do not parse or hold no picture. Length prefixed units are tried
/// first, Annex B start codes when the lengths do not tile the sample.
pub fn random_access(syntax: NalSyntax, len_size: u8, sample: &[u8]) -> Option<bool> {
    if tiles(len_size, sample) {
        return classify(syntax, Prefixed { rest: sample, len: usize::from(len_size) });
    }
    classify(syntax, annexb::nals(sample))
}

fn tiles(len_size: u8, sample: &[u8]) -> bool {
    let len = usize::from(len_size);
    if !(1..=4).contains(&len) || sample.is_empty() {
        return false;
    }
    let mut at = 0usize;
    while at < sample.len() {
        let Some(n) = prefix(&sample[at..], len) else { return false };
        at = at.saturating_add(len).saturating_add(n);
    }
    at == sample.len()
}

fn prefix(b: &[u8], len: usize) -> Option<usize> {
    let field = b.get(..len)?;
    Some(field.iter().fold(0usize, |v, &x| (v << 8) | usize::from(x)))
}

struct Prefixed<'a> {
    rest: &'a [u8],
    len: usize,
}

impl<'a> Iterator for Prefixed<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let n = prefix(self.rest, self.len)?;
        let nal = self.rest.get(self.len..self.len.checked_add(n)?)?;
        self.rest = &self.rest[self.len + n..];
        Some(nal)
    }
}

fn classify<'a>(syntax: NalSyntax, nals: impl Iterator<Item = &'a [u8]>) -> Option<bool> {
    let mut recovery = false;
    for nal in nals {
        let Some(&h) = nal.first() else { continue };
        if h & 0x80 != 0 {
            return None;
        }
        match syntax {
            NalSyntax::Hevc => {
                let t = (h >> 1) & 0x3f;
                if t <= 31 {
                    return Some(is_irap(t));
                }
            }
            NalSyntax::Avc => match h & 0x1f {
                5 => return Some(true),
                1..=4 => return Some(recovery),
                6 => recovery |= recovery_point(&nal[1..]),
                _ => {}
            },
        }
    }
    None
}

/// Whether an H.264 SEI holds a recovery point message (D.1.8), as ffmpeg keys on one.
fn recovery_point(sei: &[u8]) -> bool {
    let mut r = Rbsp { b: sei, at: 0, zeros: 0 };
    loop {
        let Some(kind) = r.field() else { return false };
        let Some(size) = r.field() else { return false };
        if kind == 6 {
            return true;
        }
        for _ in 0..size {
            if r.next().is_none() {
                return false;
            }
        }
    }
}

/// The RBSP bytes of a NAL payload, emulation prevention bytes dropped.
struct Rbsp<'a> {
    b: &'a [u8],
    at: usize,
    zeros: u8,
}

impl Rbsp<'_> {
    fn next(&mut self) -> Option<u8> {
        let mut x = *self.b.get(self.at)?;
        self.at += 1;
        if self.zeros >= 2 && x == 3 {
            x = *self.b.get(self.at)?;
            self.at += 1;
        }
        self.zeros = if x == 0 { self.zeros.saturating_add(1) } else { 0 };
        Some(x)
    }

    fn field(&mut self) -> Option<u32> {
        let mut v = 0u32;
        loop {
            let b = self.next()?;
            v = v.saturating_add(u32::from(b));
            if b != 0xFF {
                return Some(v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Test code.
    #![allow(clippy::disallowed_methods)]

    use super::*;

    fn prefixed(len: usize, nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&n.len().to_be_bytes()[8 - len..]);
            out.extend_from_slice(n);
        }
        out
    }

    const HEVC_VPS: &[u8] = &[0x40, 0x01, 0x0c];
    const HEVC_CRA: &[u8] = &[21 << 1, 0x01, 0xaf, 0x00];
    const HEVC_IDR: &[u8] = &[19 << 1, 0x01, 0xaf];
    const HEVC_TRAIL_R: &[u8] = &[1 << 1, 0x01, 0xd0];
    const HEVC_SEI: &[u8] = &[39 << 1, 0x01, 0x05, 0x01, 0x00, 0x80];
    const AVC_SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1f];
    const AVC_IDR: &[u8] = &[0x65, 0x88, 0x84];
    const AVC_I: &[u8] = &[0x41, 0x9a, 0x00];
    // user data unregistered of 2 bytes, then a recovery point.
    const AVC_SEI_RECOVERY: &[u8] = &[0x06, 0x05, 0x02, 0xaa, 0xbb, 0x06, 0x01, 0xc4, 0x80];
    const AVC_SEI_OTHER: &[u8] = &[0x06, 0x05, 0x02, 0xaa, 0xbb, 0x80];

    #[test]
    fn hevc_irap_pictures_are_random_access_and_trail_pictures_are_not() {
        for (nals, want) in [
            (&[HEVC_VPS, HEVC_SEI, HEVC_CRA][..], Some(true)),
            (&[HEVC_IDR][..], Some(true)),
            (&[HEVC_SEI, HEVC_TRAIL_R][..], Some(false)),
            (&[HEVC_VPS, HEVC_SEI][..], None),
        ] {
            assert_eq!(random_access(NalSyntax::Hevc, 4, &prefixed(4, nals)), want);
        }
        let bla = [16u8 << 1, 0x01, 0x00];
        assert_eq!(random_access(NalSyntax::Hevc, 2, &prefixed(2, &[&bla])), Some(true));
    }

    #[test]
    fn avc_idr_and_recovery_point_pictures_are_random_access() {
        let at = |nals: &[&[u8]]| random_access(NalSyntax::Avc, 4, &prefixed(4, nals));
        assert_eq!(at(&[AVC_SPS, AVC_IDR]), Some(true));
        assert_eq!(at(&[AVC_I]), Some(false), "a non-IDR I slice alone");
        assert_eq!(at(&[AVC_SEI_RECOVERY, AVC_I]), Some(true));
        assert_eq!(at(&[AVC_SEI_OTHER, AVC_I]), Some(false));
        assert_eq!(at(&[AVC_SPS]), None);
    }

    #[test]
    fn an_sei_size_counts_rbsp_bytes_past_emulation_prevention() {
        // a 3 byte payload of 00 00 00 written 00 00 03 00, then the recovery point.
        let sei = [0x06, 0x05, 0x03, 0x00, 0x00, 0x03, 0x00, 0x06, 0x01, 0x84, 0x80];
        assert_eq!(random_access(NalSyntax::Avc, 4, &prefixed(4, &[&sei, AVC_I])), Some(true));
    }

    #[test]
    fn annex_b_samples_are_read_when_the_lengths_do_not_tile() {
        let mut s = vec![0, 0, 0, 1];
        s.extend_from_slice(HEVC_VPS);
        s.extend_from_slice(&[0, 0, 1]);
        s.extend_from_slice(HEVC_CRA);
        assert_eq!(random_access(NalSyntax::Hevc, 4, &s), Some(true));
    }

    #[test]
    fn unreadable_samples_are_unknown() {
        assert_eq!(random_access(NalSyntax::Hevc, 4, &[]), None);
        assert_eq!(random_access(NalSyntax::Hevc, 4, &[0, 0, 0, 9, 0x26]), None);
        assert_eq!(random_access(NalSyntax::Avc, 4, &prefixed(4, &[&[0xe5, 0x00]])), None);
        assert_eq!(random_access(NalSyntax::Avc, 0, &prefixed(4, &[AVC_IDR])), None);
    }

    #[test]
    fn the_length_size_comes_from_the_configuration_record() {
        let mut avcc = [0u8; 7];
        avcc[4] = 0xfd;
        assert_eq!(length_size(NalSyntax::Avc, &avcc), 2);
        let mut hvcc = [0u8; 23];
        hvcc[21] = 0x0f;
        assert_eq!(length_size(NalSyntax::Hevc, &hvcc), 4);
        assert_eq!(length_size(NalSyntax::Hevc, &[]), 4);
    }
}
