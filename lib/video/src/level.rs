//! The levels encoders state, the lowest whose limits a picture size and frame rate fit.

/// level_idc, MaxMBPS, MaxFS and MaxDpbMbs (H.264 Table A-1).
const H264: [(u8, u64, u32, u32); 12] = [
    (10, 1_485, 99, 396),
    (11, 3_000, 396, 900),
    (12, 6_000, 396, 2_376),
    (13, 11_880, 396, 2_376),
    (21, 19_800, 792, 4_752),
    (22, 20_250, 1_620, 8_100),
    (30, 40_500, 1_620, 8_100),
    (31, 108_000, 3_600, 18_000),
    (32, 216_000, 5_120, 20_480),
    (41, 245_760, 8_192, 32_768),
    (42, 522_240, 8_704, 34_816),
    (51, 983_040, 36_864, 184_320),
];

/// general_level_idc, MaxLumaPs and MaxLumaSr (H.265 Table A.8).
const HEVC: [(u8, u64, u64); 8] = [
    (60, 122_880, 3_686_400),
    (63, 245_760, 7_372_800),
    (90, 552_960, 16_588_800),
    (93, 983_040, 33_177_600),
    (120, 2_228_224, 66_846_720),
    (123, 2_228_224, 133_693_440),
    (150, 8_912_896, 267_386_880),
    (153, 8_912_896, 534_773_760),
];

/// Whole frames a second rounded up, 30 for a rate of 0.
fn per_second((num, den): (u32, u32)) -> u64 {
    match num {
        0 => 30,
        n => u64::from(n.div_ceil(den.max(1))),
    }
}

/// The H.264 level_idc of an `mbs_w` x `mbs_h` macroblock picture at `fps`, 52 past 5.1.
pub fn h264(mbs_w: u32, mbs_h: u32, fps: (u32, u32)) -> u8 {
    let fs = mbs_w.saturating_mul(mbs_h);
    let mbps = u64::from(fs) * per_second(fps);
    H264.iter().find(|l| fs <= l.2 && mbps <= l.1).map_or(52, |l| l.0)
}

/// MaxDpbMbs of H.264 level `idc`, 5.1's for a level past the table.
pub fn h264_dpb_mbs(idc: u8) -> u32 {
    H264.iter().find(|l| l.0 == idc).map_or(184_320, |l| l.3)
}

/// The HEVC general_level_idc of a `coded_w` x `coded_h` picture at `fps`, 156 (5.2) past 5.1.
pub fn hevc(coded_w: u32, coded_h: u32, fps: (u32, u32)) -> u8 {
    let ps = u64::from(coded_w) * u64::from(coded_h);
    let sr = ps * per_second(fps);
    HEVC.iter().find(|l| ps <= l.1 && sr <= l.2).map_or(156, |l| l.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_pictures_take_the_lowest_level_that_holds_them() {
        let cases = [
            ((11, 9), (30, 1), 11),
            ((80, 45), (30, 1), 31),
            ((120, 68), (30, 1), 41),
            ((120, 68), (60, 1), 42),
            ((120, 68), (30_000, 1001), 41),
            ((240, 135), (30, 1), 51),
            ((240, 135), (60, 1), 52),
            ((120, 68), (0, 0), 41),
            ((120, 68), (60, 0), 42),
        ];
        for ((w, h), fps, want) in cases {
            assert_eq!(h264(w, h, fps), want, "{w}x{h} mbs at {fps:?}");
        }
    }

    #[test]
    fn h264_dpb_follows_the_level() {
        assert_eq!(h264_dpb_mbs(41), 32_768);
        assert_eq!(h264_dpb_mbs(52), 184_320);
    }

    #[test]
    fn hevc_pictures_take_the_lowest_level_that_holds_them() {
        let cases = [
            ((320, 240), (30, 1), 60),
            ((1280, 720), (30, 1), 93),
            ((1920, 1088), (30, 1), 120),
            ((1920, 1088), (60, 1), 123),
            ((3840, 2160), (30, 1), 150),
            ((3840, 2160), (60, 1), 153),
            ((7680, 4320), (30, 1), 156),
            ((1920, 1088), (0, 0), 120),
            ((1920, 1088), (60, 0), 123),
        ];
        for ((w, h), fps, want) in cases {
            assert_eq!(hevc(w, h, fps), want, "{w}x{h} at {fps:?}");
        }
    }
}
