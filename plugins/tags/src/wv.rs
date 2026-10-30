//! WavPack properties, per `spec/WavPack5FileFormat.pdf` and `spec/WAVPACK.md`.

use crate::Props;

const HEADER_LEN: usize = 32;

pub(crate) const MAGIC: &[u8; 4] = b"wvpk";

const VERSION_MIN: u16 = 0x402;
const VERSION_MAX: u16 = 0x410;

const MAX_BLOCKS: usize = 8;

const MAX_SUBBLOCKS: usize = 64;

/// The 40-bit form reserves any value whose low 32 bits are all set, so only the low word is
/// tested.
const UNKNOWN_LENGTH: u32 = 0xFFFF_FFFF;

const MONO_FLAG: u32 = 1 << 2;
const SRATE_LSB: u32 = 23;
const SRATE_MASK: u32 = 0xF << SRATE_LSB;
const SRATE_CUSTOM: u32 = 0xF;

const ID_FUNCTION: u8 = 0x3F;
const ID_ODD_SIZE: u8 = 0x40;
const ID_LARGE: u8 = 0x80;

const ID_CHANNEL_INFO: u8 = 0x0D;
const ID_SAMPLE_RATE: u8 = 0x27;

/// Indexed by flags bits 26-23. Table from the reference `src/common_utils.c`. Index 15 means
/// custom.
const SAMPLE_RATES: [u32; 15] = [
    6_000, 8_000, 9_600, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000,
    88_200, 96_000, 192_000,
];

pub(crate) fn props(window: &[u8], body: u64) -> Props {
    let mut props = Props::default();
    let Ok(mut at) = usize::try_from(body) else {
        return props;
    };

    let mut total: Option<u64> = None;
    let mut format_seen = false;

    for _ in 0..MAX_BLOCKS {
        let Some(h) = window.get(at..).and_then(|r| r.get(..HEADER_LEN)) else {
            break;
        };
        if &h[..4] != MAGIC {
            break;
        }
        let le32 = |o: usize| u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
        let ck_size = le32(4);
        let version = u16::from_le_bytes([h[8], h[9]]);
        if !(VERSION_MIN..=VERSION_MAX).contains(&version) {
            break;
        }
        let block_index_u8 = h[10];
        let total_samples_u8 = h[11];
        let total_samples = le32(12);
        let block_index = le32(16);
        let block_samples = le32(20);
        let flags = le32(24);

        let index = u64::from(block_index) | (u64::from(block_index_u8) << 32);
        if total.is_none() && index == 0 && total_samples != UNKNOWN_LENGTH {
            let value = (u64::from(total_samples) + (u64::from(total_samples_u8) << 32))
                .saturating_sub(u64::from(total_samples_u8));
            // A non-audio block stating zero states nothing, as in the reference `open_utils.c`.
            if block_samples != 0 || value != 0 {
                total = Some(value);
            }
        }

        if !format_seen && block_samples != 0 {
            format_seen = true;
            let payload = window
                .get(at + HEADER_LEN..)
                .map(|r| &r[..r.len().min(block_body_len(ck_size))])
                .unwrap_or(&[]);
            let (rate_meta, channels_meta) = sub_blocks(payload);

            let index = (flags & SRATE_MASK) >> SRATE_LSB;
            props.sample_rate = if index == SRATE_CUSTOM {
                rate_meta
            } else {
                SAMPLE_RATES.get(index as usize).copied()
            };
            // Multichannel files carry the true count in `ID_CHANNEL_INFO`, which wins over the
            // header.
            props.channels = channels_meta.or(Some(if flags & MONO_FLAG != 0 { 1 } else { 2 }));
        }

        if format_seen && total.is_some() {
            break;
        }
        let Some(next) =
            usize::try_from(u64::from(ck_size) + 8).ok().and_then(|n| at.checked_add(n))
        else {
            break;
        };
        if next <= at {
            break;
        }
        at = next;
    }

    if let (Some(total), Some(rate)) = (total, props.sample_rate.filter(|r| *r > 0)) {
        // u128 since `total * 1e9` overflows u64 for 40-bit counts.
        props.duration_ns =
            u64::try_from(u128::from(total) * 1_000_000_000 / u128::from(rate)).ok();
        props.duration_exact = true;
    }
    props
}

fn block_body_len(ck_size: u32) -> usize {
    usize::try_from(u64::from(ck_size).saturating_add(8).saturating_sub(HEADER_LEN as u64))
        .unwrap_or(usize::MAX)
}

fn sub_blocks(payload: &[u8]) -> (Option<u32>, Option<u32>) {
    let mut rate = None;
    let mut channels = None;
    let mut at = 0usize;

    for _ in 0..MAX_SUBBLOCKS {
        let Some(&id) = payload.get(at) else { break };
        let large = id & ID_LARGE != 0;
        let (words, head) = if large {
            let Some(w) = payload.get(at + 1..at + 4) else {
                break;
            };
            (u32::from(w[0]) | (u32::from(w[1]) << 8) | (u32::from(w[2]) << 16), 4usize)
        } else {
            let Some(&w) = payload.get(at + 1) else { break };
            (u32::from(w), 2usize)
        };
        let stored = usize::try_from(u64::from(words) * 2).unwrap_or(usize::MAX);
        let len = stored - usize::from(id & ID_ODD_SIZE != 0 && stored > 0);
        let Some(data) = payload.get(at + head..).map(|r| &r[..r.len().min(len)]) else {
            break;
        };

        match id & ID_FUNCTION {
            ID_SAMPLE_RATE if data.len() == 3 || data.len() == 4 => {
                let mut v =
                    u32::from(data[0]) | (u32::from(data[1]) << 8) | (u32::from(data[2]) << 16);
                if let Some(&top) = data.get(3) {
                    v |= u32::from(top & 0x7F) << 24;
                }
                rate = Some(v).filter(|&v| v > 0);
            }
            // 1-5 bytes is count then mask. 6-7 bytes is the 5.0 form with a 12-bit count biased by
            // one.
            ID_CHANNEL_INFO if !data.is_empty() => {
                channels = Some(if data.len() >= 6 {
                    (u32::from(data[0]) | (u32::from(data[2] & 0x0F) << 8)) + 1
                } else {
                    u32::from(data[0])
                })
                .filter(|&c| c > 0);
            }
            _ => {}
        }

        let Some(next) = at.checked_add(head).and_then(|n| n.checked_add(stored)) else {
            break;
        };
        if next <= at {
            break;
        }
        at = next;
    }
    (rate, channels)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use crate::fixture::{wavpack, wavpack_block, wv_sub_block};

    #[test]
    fn header_props_and_exact_duration() {
        let f = wavpack(44_100, 2, 88_200, &[]);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(44_100));
        assert_eq!(p.channels, Some(2));
        assert_eq!(p.duration_ns, Some(2_000_000_000));
        assert!(p.duration_exact, "a declared frame count is authoritative");
    }

    #[test]
    fn the_mono_flag_is_the_channel_count_when_nothing_qualifies_it() {
        let p = props(&wavpack(48_000, 1, 24_000, &[]), 0);
        assert_eq!(p.channels, Some(1));
        assert_eq!(p.sample_rate, Some(48_000));
        assert_eq!(p.duration_ns, Some(500_000_000));
    }

    #[test]
    fn every_standard_rate_index_round_trips() {
        for (index, &rate) in SAMPLE_RATES.iter().enumerate() {
            let f = wavpack(rate, 2, rate, &[]);
            let p = props(&f, 0);
            assert_eq!(p.sample_rate, Some(rate), "index {index}");
            assert_eq!(p.duration_ns, Some(1_000_000_000));
        }
    }

    #[test]
    fn a_custom_rate_comes_from_the_sub_block() {
        let sub = wv_sub_block(ID_SAMPLE_RATE, &37_800u32.to_le_bytes()[..3]);
        let f = wavpack(0, 2, 37_800, &[sub]);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(37_800));
        assert_eq!(p.duration_ns, Some(1_000_000_000));
        assert!(p.duration_exact);

        let big = wv_sub_block(ID_SAMPLE_RATE, &20_000_000u32.to_le_bytes());
        let g = wavpack(0, 2, 40_000_000, &[big]);
        assert_eq!(props(&g, 0).sample_rate, Some(20_000_000));

        let h = wavpack(0, 2, 1_000, &[]);
        let p = props(&h, 0);
        assert_eq!(p.sample_rate, None);
        assert_eq!(p.duration_ns, None, "no rate means no duration, not a guessed one");
    }

    #[test]
    fn channel_info_beats_the_mono_flag() {
        let sub = wv_sub_block(ID_CHANNEL_INFO, &[6, 0x3F, 0x00, 0x00]);
        let p = props(&wavpack(48_000, 2, 48_000, &[sub]), 0);
        assert_eq!(p.channels, Some(6));

        let wide = wv_sub_block(ID_CHANNEL_INFO, &[7, 3, 0x00, 0x3F, 0x00, 0x00]);
        let p = props(&wavpack(48_000, 2, 48_000, &[wide]), 0);
        assert_eq!(p.channels, Some(8), "(7 | (0 << 8)) + 1");
    }

    #[test]
    fn an_unknown_length_yields_no_duration() {
        let mut f = wavpack(44_100, 2, 44_100, &[]);
        f[12..16].copy_from_slice(&UNKNOWN_LENGTH.to_le_bytes());
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(44_100), "the format is still known");
        assert_eq!(p.channels, Some(2));
        assert_eq!(p.duration_ns, None);
        assert!(!p.duration_exact);
    }

    #[test]
    fn a_leading_metadata_block_is_stepped_over() {
        let meta = wavpack_block(0x410, 0, 0, 0, 0, &[]);
        let audio = wavpack(48_000, 1, 96_000, &[]);
        let mut f = meta;
        f.extend_from_slice(&audio);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(48_000));
        assert_eq!(p.channels, Some(1));
        assert_eq!(p.duration_ns, Some(2_000_000_000));
    }

    #[test]
    fn a_bad_version_or_magic_yields_nothing() {
        let mut f = wavpack(44_100, 2, 44_100, &[]);
        f[8..10].copy_from_slice(&0x0401u16.to_le_bytes()); // below 0x402
        assert_eq!(props(&f, 0), Props::default());
        let mut g = wavpack(44_100, 2, 44_100, &[]);
        g[8..10].copy_from_slice(&0x0411u16.to_le_bytes()); // above 0x410
        assert_eq!(props(&g, 0), Props::default());
        let mut h = wavpack(44_100, 2, 44_100, &[]);
        h[0] = b'W';
        assert_eq!(props(&h, 0), Props::default());
        assert_eq!(props(b"", 0), Props::default());
    }

    #[test]
    fn truncation_never_panics() {
        let f = wavpack(
            44_100,
            2,
            88_200,
            &[wv_sub_block(ID_CHANNEL_INFO, &[2, 3]), wv_sub_block(0x0A, &[0u8; 40])],
        );
        for n in 0..f.len() {
            let _ = props(&f[..n], 0);
        }
    }

    #[test]
    fn single_byte_corruption_never_panics() {
        let base = wavpack(0, 2, 44_100, &[wv_sub_block(ID_SAMPLE_RATE, &[0x44, 0xAC, 0x00])]);
        for i in 0..base.len() {
            for bit in [0x01u8, 0x40, 0x80, 0xFF] {
                let mut f = base.clone();
                f[i] ^= bit;
                let _ = props(&f, 0);
            }
        }
    }

    #[test]
    fn an_absurd_sub_block_length_is_bounded() {
        let mut f = wavpack(44_100, 2, 44_100, &[wv_sub_block(ID_CHANNEL_INFO, &[2, 3, 0, 0])]);
        let at = HEADER_LEN;
        f[at] = ID_LARGE | ID_CHANNEL_INFO;
        f[at + 1] = 0xFF;
        f[at + 2] = 0xFF;
        f[at + 3] = 0xFF;
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(44_100), "the header still parsed");
        assert_eq!(p.duration_ns, Some(1_000_000_000));
    }

    #[test]
    fn an_audio_block_stating_zero_is_believed() {
        let flags = 0x01 | (9 << 23) | (1 << 11) | (1 << 12);
        let f = wavpack_block(0x410, 0, 0, 100, flags, &[]);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(44_100));
        assert_eq!(p.duration_ns, Some(0));
        assert!(p.duration_exact);
    }
}
