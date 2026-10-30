//! Monkey's Audio (`.ape`) properties, per `spec/APE.md`.

use crate::Props;

pub(crate) const MAGIC: &[u8; 4] = b"MAC ";
pub(crate) const MAGIC_LARGE: &[u8; 4] = b"MACF";

const NEW_FORMAT: u16 = 3980;

const DESCRIPTOR_LEN: usize = 52;
const HEADER_LEN: usize = 24;

const OLD_HEADER_LEN: usize = 32;

const LEVEL_INSANE: u16 = 5000;

const MAX_BLOCKS_PER_FRAME: u32 = 1_000_000;
const MAX_BLOCKS_PER_FRAME_INSANE: u32 = 10_000_000;
const MAX_CHANNELS: u16 = 32;

const FLAG_8_BIT: u16 = 1;
const FLAG_24_BIT: u16 = 8;

pub(crate) fn props(window: &[u8], body: u64) -> Props {
    let mut props = Props::default();
    let Ok(at) = usize::try_from(body) else {
        return props;
    };
    let Some(h) = window.get(at..) else {
        return props;
    };
    if !h.starts_with(MAGIC) && !h.starts_with(MAGIC_LARGE) {
        return props;
    }
    let Some(v) = h.get(4..6) else { return props };
    let version = u16::from_le_bytes([v[0], v[1]]);
    if version >= NEW_FORMAT {
        modern(h, &mut props);
    } else {
        legacy(h, version, &mut props);
    }
    props
}

fn le16(h: &[u8], at: usize) -> Option<u16> {
    h.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}
fn le32(h: &[u8], at: usize) -> Option<u32> {
    h.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// `nPadding` at octet 6 shifts every later field. The header is reached by `nDescriptorBytes`,
/// clamped up so the walk never moves backwards.
fn modern(h: &[u8], props: &mut Props) {
    let Some(descriptor_bytes) = le32(h, 8) else {
        return;
    };
    let header_at = usize::try_from(descriptor_bytes).unwrap_or(DESCRIPTOR_LEN).max(DESCRIPTOR_LEN);
    let Some(a) = h.get(header_at..).filter(|r| r.len() >= HEADER_LEN) else {
        return;
    };

    let (Some(level), Some(blocks_per_frame), Some(final_frame_blocks), Some(total_frames)) =
        (le16(a, 0), le32(a, 4), le32(a, 8), le32(a, 12))
    else {
        return;
    };
    let (Some(channels), Some(rate)) = (le16(a, 18), le32(a, 20)) else {
        return;
    };

    if !plausible(level, blocks_per_frame, final_frame_blocks, channels) {
        return;
    }
    props.channels = Some(u32::from(channels));
    finish(props, rate, total_blocks(total_frames, blocks_per_frame, final_frame_blocks));
}

fn legacy(h: &[u8], version: u16, props: &mut Props) {
    if h.len() < OLD_HEADER_LEN {
        return;
    }
    let (Some(level), Some(flags), Some(channels), Some(rate)) =
        (le16(h, 6), le16(h, 8), le16(h, 10), le32(h, 12))
    else {
        return;
    };
    let (Some(total_frames), Some(final_frame_blocks)) = (le32(h, 24), le32(h, 28)) else {
        return;
    };

    // SDK derivation. 9216 before 3.90, 73728 from 3.90, 294912 from 3.95 (overrides).
    let mut blocks_per_frame =
        if version >= 3900 || (version >= 3800 && level == 4000) { 73_728 } else { 9_216 };
    if version >= 3950 {
        blocks_per_frame = 73_728 * 4;
    }
    let _depth = if flags & FLAG_8_BIT != 0 {
        8
    } else if flags & FLAG_24_BIT != 0 {
        24
    } else {
        16
    };

    if !plausible(level, blocks_per_frame, final_frame_blocks, channels) {
        return;
    }
    // The SDK refuses zero-frame files on the old path.
    if total_frames == 0 {
        return;
    }
    props.channels = Some(u32::from(channels));
    finish(props, rate, total_blocks(total_frames, blocks_per_frame, final_frame_blocks));
}

fn plausible(level: u16, blocks_per_frame: u32, final_frame_blocks: u32, channels: u16) -> bool {
    let ceiling =
        if level >= LEVEL_INSANE { MAX_BLOCKS_PER_FRAME_INSANE } else { MAX_BLOCKS_PER_FRAME };
    blocks_per_frame > 0
        && blocks_per_frame <= ceiling
        && final_frame_blocks <= blocks_per_frame
        && (1..=MAX_CHANNELS).contains(&channels)
}

/// The zero guard stops `nTotalFrames - 1` from underflowing.
fn total_blocks(total_frames: u32, blocks_per_frame: u32, final_frame_blocks: u32) -> u64 {
    if total_frames == 0 {
        return 0;
    }
    u64::from(total_frames - 1) * u64::from(blocks_per_frame) + u64::from(final_frame_blocks)
}

fn finish(props: &mut Props, rate: u32, blocks: u64) {
    if rate == 0 {
        return;
    }
    props.sample_rate = Some(rate);
    // u128 since `blocks * 1e9` overflows u64.
    props.duration_ns = u64::try_from(u128::from(blocks) * 1_000_000_000 / u128::from(rate)).ok();
    props.duration_exact = true;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use crate::fixture::{ape_file, ape_file_old};

    #[test]
    fn modern_header_props_and_exact_duration() {
        let f = ape_file(3990, 44_100, 2, 73_728, 3, 4_644);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(44_100));
        assert_eq!(p.channels, Some(2));
        let blocks = 2 * 73_728 + 4_644;
        assert_eq!(p.duration_ns, Some(blocks * 1_000_000_000 / 44_100));
        assert!(p.duration_exact, "a declared block count is authoritative");
    }

    #[test]
    fn macf_is_the_same_layout() {
        let mut f = ape_file(3990, 48_000, 1, 73_728, 2, 73_728);
        f[..4].copy_from_slice(MAGIC_LARGE);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(48_000));
        assert_eq!(p.channels, Some(1));
        assert_eq!(p.duration_ns, Some(2 * 73_728 * 1_000_000_000 / 48_000));
    }

    #[test]
    fn the_header_is_reached_by_the_declared_descriptor_size() {
        let mut f = ape_file(3990, 44_100, 2, 73_728, 2, 73_728);
        let grown = (DESCRIPTOR_LEN + 8) as u32;
        f[8..12].copy_from_slice(&grown.to_le_bytes());
        let header: Vec<u8> = f[DESCRIPTOR_LEN..DESCRIPTOR_LEN + HEADER_LEN].to_vec();
        let mut g = f[..DESCRIPTOR_LEN].to_vec();
        g.extend_from_slice(&[0u8; 8]); // the eight new descriptor octets
        g.extend_from_slice(&header);
        let p = props(&g, 0);
        assert_eq!(p.sample_rate, Some(44_100));
        assert_eq!(p.duration_ns, Some(2 * 73_728 * 1_000_000_000 / 44_100));

        let mut small = f.clone();
        small[8..12].copy_from_slice(&4u32.to_le_bytes());
        let p = props(&small, 0);
        assert_eq!(p.sample_rate, Some(44_100), "clamped to the known size");
    }

    #[test]
    fn the_old_header_derives_its_blocks_per_frame() {
        let f = ape_file_old(3790, 2000, 0, 44_100, 2, 4, 9_216);
        assert_eq!(props(&f, 0).duration_ns, Some(4 * 9_216 * 1_000_000_000 / 44_100));
        let g = ape_file_old(3800, 4000, 0, 44_100, 2, 2, 73_728);
        assert_eq!(props(&g, 0).duration_ns, Some(2 * 73_728 * 1_000_000_000 / 44_100));
        let h = ape_file_old(3800, 2000, 0, 44_100, 2, 4, 9_216);
        assert_eq!(props(&h, 0).duration_ns, Some(4 * 9_216 * 1_000_000_000 / 44_100));
        let i = ape_file_old(3900, 2000, 0, 44_100, 2, 2, 73_728);
        assert_eq!(props(&i, 0).duration_ns, Some(2 * 73_728 * 1_000_000_000 / 44_100));
        let j = ape_file_old(3950, 2000, 0, 44_100, 2, 2, 294_912);
        assert_eq!(props(&j, 0).duration_ns, Some(2 * 294_912 * 1_000_000_000 / 44_100));
        assert_eq!(props(&j, 0).channels, Some(2));
        assert_eq!(props(&j, 0).sample_rate, Some(44_100));
    }

    #[test]
    fn zero_total_frames_never_underflows() {
        assert_eq!(total_blocks(0, 73_728, 1_000), 0);
        let f = ape_file(3990, 44_100, 2, 73_728, 0, 0);
        let p = props(&f, 0);
        assert_eq!(p.duration_ns, Some(0), "zero blocks, not 2.8 million years");
        assert_eq!(p.sample_rate, Some(44_100));
        let g = ape_file_old(3950, 2000, 0, 44_100, 2, 0, 294_912);
        assert_eq!(props(&g, 0), Props::default());
    }

    #[test]
    fn implausible_headers_report_nothing() {
        let a = ape_file(3990, 44_100, 2, 0, 4, 0);
        assert_eq!(props(&a, 0), Props::default());
        let b = ape_file(3990, 44_100, 2, 2_000_000, 4, 100);
        assert_eq!(props(&b, 0), Props::default());
        let c = ape_file(3990, 44_100, 2, 2_000_000, 4, 100);
        let mut c = c;
        c[DESCRIPTOR_LEN..DESCRIPTOR_LEN + 2].copy_from_slice(&LEVEL_INSANE.to_le_bytes());
        assert_eq!(props(&c, 0).sample_rate, Some(44_100));
        let d = ape_file(3990, 44_100, 2, 73_728, 4, 73_729);
        assert_eq!(props(&d, 0), Props::default());
        let e = ape_file(3990, 44_100, 0, 73_728, 4, 100);
        assert_eq!(props(&e, 0), Props::default());
        let g = ape_file(3990, 44_100, 33, 73_728, 4, 100);
        assert_eq!(props(&g, 0), Props::default());
        let i = ape_file(3990, 0, 2, 73_728, 4, 100);
        assert_eq!(props(&i, 0).duration_ns, None);
    }

    #[test]
    fn a_bad_magic_yields_nothing() {
        let mut f = ape_file(3990, 44_100, 2, 73_728, 2, 100);
        f[0] = b'X';
        assert_eq!(props(&f, 0), Props::default());
        assert_eq!(props(b"", 0), Props::default());
        assert_eq!(props(b"MAC ", 0), Props::default());
    }

    #[test]
    fn truncation_never_panics() {
        for f in [
            ape_file(3990, 44_100, 2, 73_728, 9, 1_000),
            ape_file_old(3950, 2000, 4, 44_100, 2, 9, 294_912),
        ] {
            for n in 0..f.len() {
                let _ = props(&f[..n], 0);
            }
        }
    }

    #[test]
    fn single_byte_corruption_never_panics() {
        for base in [
            ape_file(3990, 44_100, 2, 73_728, 9, 1_000),
            ape_file_old(3800, 4000, 20, 48_000, 1, 5, 73_728),
        ] {
            for i in 0..base.len() {
                for bit in [0x01u8, 0x40, 0x80, 0xFF] {
                    let mut f = base.clone();
                    f[i] ^= bit;
                    let _ = props(&f, 0);
                }
            }
        }
    }
}
