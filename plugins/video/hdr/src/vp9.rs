//! The colour a VP9 frame header states (VP9 spec 6.2.2 `color_config`).

use crate::bits::Bits;
use crate::color::{Matrix, Range, StreamColor};

const CS_RGB: u32 = 7;
const SYNC: [u32; 3] = [0x49, 0x83, 0x42];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Vp9Colour {
    pub profile: u8,
    pub bit_depth: u8,
    pub color_space: u8,
    pub full_range: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
}

impl Vp9Colour {
    /// `color_space` as an H.273 matrix, mapped as FFmpeg vp9.c does.
    pub fn matrix(&self) -> Option<Matrix> {
        match self.color_space {
            1 => Some(Matrix::Bt470Bg),
            2 => Some(Matrix::Bt709),
            3 => Some(Matrix::Bt601),
            4 => Some(Matrix::Smpte240),
            5 => Some(Matrix::Bt2020),
            7 => Some(Matrix::Identity),
            _ => None,
        }
    }

    pub fn stream_color(&self) -> StreamColor {
        StreamColor {
            matrix: self.matrix(),
            range: Some(Range::from_flag(self.full_range)),
            ..StreamColor::default()
        }
    }
}

/// The colour of one frame, `None` when it states none or is cut short.
pub fn colour(frame: &[u8]) -> Option<Vp9Colour> {
    let mut b = Bits::new(frame);
    if b.u(2)? != 2 {
        return None; // frame_marker
    }
    let low = b.u(1)?;
    let high = b.u(1)?;
    let profile = (high << 1 | low) as u8;
    if profile == 3 {
        b.u(1)?; // reserved_zero
    }
    if b.flag()? {
        return None; // show_existing_frame
    }
    let key = !b.flag()?; // frame_type, KEY_FRAME is 0
    let show_frame = b.flag()?;
    let error_resilient = b.flag()?;
    if !key {
        let intra_only = if show_frame { false } else { b.flag()? };
        if !error_resilient {
            b.u(2)?; // reset_frame_context
        }
        if !intra_only || profile == 0 {
            return None;
        }
    }
    for s in SYNC {
        if b.u(8)? != s {
            return None;
        }
    }
    let bit_depth = match profile >= 2 {
        true => match b.flag()? {
            true => 12,
            false => 10,
        },
        false => 8,
    };
    let color_space = b.u(3)?;
    let odd = profile == 1 || profile == 3;
    let (full_range, sx, sy) = if color_space != CS_RGB {
        let full = b.flag()?;
        match odd {
            true => (full, b.flag()?, b.flag()?),
            false => (full, true, true),
        }
    } else {
        (true, false, false)
    };
    Some(Vp9Colour {
        profile,
        bit_depth,
        color_space: color_space as u8,
        full_range,
        subsampling_x: sx,
        subsampling_y: sy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;

    fn key_header(profile: u32, color_space: u32, full: bool) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(2, 2).put(profile & 1, 1).put(profile >> 1, 1);
        if profile == 3 {
            w.put(0, 1);
        }
        w.put(0, 1).put(0, 1).put(1, 1).put(0, 1); // show_existing, key, shown, not resilient
        for s in SYNC {
            w.put(s, 8);
        }
        if profile >= 2 {
            w.put(0, 1);
        }
        w.put(color_space, 3);
        if color_space != CS_RGB {
            w.put(u32::from(full), 1);
            if profile == 1 || profile == 3 {
                w.put(1, 1).put(0, 1).put(0, 1);
            }
        }
        w.put(0, 16);
        w.into_bytes()
    }

    #[test]
    fn a_key_frame_states_its_colour_space_and_range() {
        let c = colour(&key_header(2, 5, false)).unwrap();
        assert_eq!((c.profile, c.bit_depth, c.color_space, c.full_range), (2, 10, 5, false));
        assert_eq!(c.matrix(), Some(Matrix::Bt2020));
        assert_eq!(c.stream_color().range, Some(Range::Limited));
        let c = colour(&key_header(0, 2, true)).unwrap();
        assert_eq!((c.bit_depth, c.matrix(), c.full_range), (8, Some(Matrix::Bt709), true));
        let c = colour(&key_header(1, 1, false)).unwrap();
        assert_eq!(
            (c.subsampling_x, c.subsampling_y, c.matrix()),
            (true, false, Some(Matrix::Bt470Bg))
        );
        let rgb = colour(&key_header(1, 7, false)).unwrap();
        assert_eq!((rgb.matrix(), rgb.full_range), (Some(Matrix::Identity), true));
    }

    #[test]
    fn inter_and_broken_headers_state_nothing() {
        let mut w = BitWriter::default();
        w.put(2, 2).put(0, 2).put(0, 1).put(1, 1).put(1, 1).put(0, 1).put(0, 16);
        assert_eq!(colour(&w.into_bytes()), None, "an inter frame");
        let mut bad = key_header(0, 2, false);
        bad[1] ^= 0x01;
        assert_eq!(colour(&bad), None, "a sync code off by a bit");
        assert_eq!(colour(&[0x00]), None, "no frame marker");
        assert_eq!(colour(&key_header(0, 2, false)[..2]), None, "cut");
    }
}
