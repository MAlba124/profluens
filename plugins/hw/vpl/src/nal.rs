/// H.264 (7.4.1) and H.265 (7.4.2.2) NAL unit types.
pub const AVC_IDR: u8 = 5;
pub const AVC_SPS: u8 = 7;
pub const AVC_PPS: u8 = 8;
pub const HEVC_IDR_W_RADL: u8 = 19;
pub const HEVC_IDR_N_LP: u8 = 20;
pub const HEVC_CRA: u8 = 21;
pub const HEVC_VPS: u8 = 32;
pub const HEVC_SPS: u8 = 33;
pub const HEVC_PPS: u8 = 34;

pub fn nals(unit: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let start = find_start(unit, at)?;
        let begin = start + 3;
        let end = find_start(unit, begin)
            .map_or(unit.len(), |s| if s > begin && unit[s - 1] == 0 { s - 1 } else { s });
        at = end;
        Some(&unit[begin..end])
    })
}

fn find_start(unit: &[u8], from: usize) -> Option<usize> {
    (from..unit.len().saturating_sub(2))
        .find(|&i| unit[i] == 0 && unit[i + 1] == 0 && unit[i + 2] == 1)
}

pub fn avc_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1F)
}

pub fn hevc_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3F)
}

pub fn has_sps(unit: &[u8], hevc: bool) -> bool {
    nals(unit).any(
        |n| {
            if hevc { hevc_type(n) == Some(HEVC_SPS) } else { avc_type(n) == Some(AVC_SPS) }
        },
    )
}

/// An IDR for AVC, an IDR or CRA for HEVC.
pub fn is_keyframe(unit: &[u8], hevc: bool) -> bool {
    nals(unit).any(|n| {
        if hevc {
            matches!(hevc_type(n), Some(HEVC_IDR_W_RADL | HEVC_IDR_N_LP | HEVC_CRA))
        } else {
            avc_type(n) == Some(AVC_IDR)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_three_and_four_byte_start_codes_and_reads_the_types() {
        let unit = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0xCC, 0, 0, 0, 1, 0x65, 0x11];
        let got: Vec<&[u8]> = nals(&unit).collect();
        assert_eq!(got, [&[0x67, 0xAA][..], &[0x68, 0xBB, 0xCC], &[0x65, 0x11]]);
        assert!(has_sps(&unit, false));
        assert!(is_keyframe(&unit, false));
        assert!(!has_sps(&unit, true));
        let hevc = [0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x42, 0x01, 0, 0, 1, 0x26, 0x01];
        let types: Vec<u8> = nals(&hevc).filter_map(hevc_type).collect();
        assert_eq!(types, [HEVC_VPS, HEVC_SPS, HEVC_IDR_W_RADL]);
        assert!(has_sps(&hevc, true) && is_keyframe(&hevc, true));
        assert_eq!(nals(&[0, 0]).count(), 0);
    }
}
