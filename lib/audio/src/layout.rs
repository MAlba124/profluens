//! Channel layouts of `audio/raw`, a WAVE `dwChannelMask` style bit per speaker.

use profluens_core::ctx::Ctx;
use profluens_core::format::{ConstraintDesc, FieldDesc, Value};
use profluens_core::id::SinkPad;

/// The channel mask (`Value::Int`).
pub const FIELD_LAYOUT: &str = "layout";

pub const LAYOUT_FIELD: FieldDesc =
    FieldDesc { field: FIELD_LAYOUT, allowed: ConstraintDesc::Any, preferred: None };

pub const FL: u64 = 0x1;
pub const FR: u64 = 0x2;
pub const FC: u64 = 0x4;
pub const LFE: u64 = 0x8;
pub const BL: u64 = 0x10;
pub const BR: u64 = 0x20;
pub const FLC: u64 = 0x40;
pub const FRC: u64 = 0x80;
pub const BC: u64 = 0x100;
pub const SL: u64 = 0x200;
pub const SR: u64 = 0x400;
pub const TC: u64 = 0x800;
pub const TFL: u64 = 0x1000;
pub const TFC: u64 = 0x2000;
pub const TFR: u64 = 0x4000;
pub const TBL: u64 = 0x8000;
pub const TBC: u64 = 0x10000;
pub const TBR: u64 = 0x20000;

pub const STEREO: u64 = FL | FR;

/// The default layout of a channel count, FLAC's (RFC 9639 9.1.3) and Vorbis's (RFC 7845 5.1.1.2).
pub const fn default_layout(channels: u32) -> u64 {
    match channels {
        1 => FC,
        2 => STEREO,
        3 => FL | FR | FC,
        4 => FL | FR | BL | BR,
        5 => FL | FR | FC | BL | BR,
        6 => FL | FR | FC | LFE | BL | BR,
        7 => FL | FR | FC | LFE | BC | SL | SR,
        8 => FL | FR | FC | LFE | BL | BR | SL | SR,
        _ => 0,
    }
}

/// `said` where it names `channels` positions, else the default of the count.
pub const fn layout_or_default(said: Option<u64>, channels: u32) -> u64 {
    match said {
        Some(mask) if mask.count_ones() == channels => mask,
        _ => default_layout(channels),
    }
}

pub fn negotiated_layout(ctx: &Ctx, pad: SinkPad) -> Option<u64> {
    let fixed = ctx.negotiated_sink(pad)?;
    match fixed.get(ctx.field_id(FIELD_LAYOUT)?)? {
        Value::Int(n) => Some(n as u64),
        _ => None,
    }
}

/// The positions of `mask` in channel order, one bit each.
pub fn positions(mask: u64) -> impl Iterator<Item = u64> {
    (0..64).map(|b| 1u64 << b).filter(move |p| mask & p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_names_as_many_positions_as_channels() {
        for n in 1..=8 {
            assert_eq!(default_layout(n).count_ones(), n, "{n} channels");
        }
        assert_eq!(default_layout(0), 0);
        assert_eq!(default_layout(9), 0, "unpositioned");
    }

    #[test]
    fn a_layout_of_another_count_is_not_taken() {
        let side = FL | FR | FC | LFE | SL | SR;
        assert_eq!(layout_or_default(Some(side), 6), side);
        assert_eq!(layout_or_default(Some(STEREO), 6), default_layout(6));
        assert_eq!(layout_or_default(None, 2), STEREO);
    }

    #[test]
    fn positions_run_in_bit_order() {
        let got: Vec<u64> = positions(default_layout(8)).collect();
        assert_eq!(got, [FL, FR, FC, LFE, BL, BR, SL, SR]);
    }
}
