//! Musepack properties from the SV8 `SH` packet or the SV7 fixed header (in-tree `spec/MPC.md`).
//! Tags are the APEv2 and ID3v1 trailer, handled by `tail.rs`.

use crate::Props;

pub(crate) const MAGIC_SV8: &[u8; 4] = b"MPCK";
pub(crate) const MAGIC_SV7: &[u8; 3] = b"MP+";
const SV7_VERSION: u8 = 7;
const SV8_VERSION: u8 = 8;

const MAX_PACKETS: usize = 16;

/// Longest size varint, "n*8, 0 < n < 10" octets.
const MAX_VARINT: usize = 9;

/// `MPC_FRAME_LENGTH` (`spec/libmpc-mpcdec.h`).
const FRAME_LENGTH: u64 = 36 * 32;

/// `MPC_DECODER_SYNTH_DELAY`, removed from a non-gapless SV7 sample count.
const SYNTH_DELAY: u64 = 481;

/// `samplefreqs` (`spec/libmpc-streaminfo.c`). Indices 4 to 7 are zero there and rejected.
const SAMPLE_RATES: [u32; 4] = [44_100, 48_000, 37_800, 32_000];

pub(crate) fn props(window: &[u8], body: u64) -> Props {
    let mut props = Props::default();
    let Ok(at) = usize::try_from(body) else {
        return props;
    };
    let Some(h) = window.get(at..) else {
        return props;
    };
    if h.starts_with(MAGIC_SV8) {
        sv8(&h[MAGIC_SV8.len()..], &mut props);
    } else if h.starts_with(MAGIC_SV7) {
        sv7(h, &mut props);
    }
    props
}

/// Walk SV8 packets for `SH`. The packet size includes the key and the size octets.
fn sv8(mut rest: &[u8], props: &mut Props) {
    for _ in 0..MAX_PACKETS {
        let Some(key) = rest.get(..2) else { return };
        if !key.iter().all(|c| c.is_ascii_uppercase()) {
            return;
        }
        let Some((size, size_len)) = varint(&rest[2..]) else {
            return;
        };
        let head = 2 + size_len;
        let Ok(size) = usize::try_from(size) else {
            return;
        };
        if size < head {
            return; // a size that does not even cover its own header
        }
        if key == b"SH" {
            // The payload may be zero-padded past its fields, so advance by `size`.
            let end = size.min(rest.len());
            if let Some(payload) = rest.get(head..end) {
                stream_header(payload, props);
            }
            return;
        }
        if key == b"SE" {
            return;
        }
        let Some(next) = rest.get(size..) else { return };
        rest = next;
    }
}

/// The `SH` payload (`spec/MPC.md`). The CRC is not verified.
fn stream_header(payload: &[u8], props: &mut Props) {
    let Some(&version) = payload.get(4) else {
        return;
    };
    if version != SV8_VERSION {
        return;
    }
    let Some((samples, n)) = varint(&payload[5..]) else {
        return;
    };
    let Some((silence, m)) = varint(&payload[5 + n..]) else {
        return;
    };
    let Some(packed) = payload.get(5 + n + m..5 + n + m + 2) else {
        return;
    };

    let rate_index = usize::from(packed[0] >> 5);
    // Both counts are stored biased by one.
    let max_band = u32::from(packed[0] & 0x1F) + 1;
    let channels = u32::from(packed[1] >> 4) + 1;

    // libmpc caps channels at 2 for its decoder. The format allows 16.
    if max_band == 0 || max_band >= 32 || channels == 0 || channels > 16 {
        return;
    }
    let Some(&rate) = SAMPLE_RATES.get(rate_index) else {
        return; // indices 4-7 are undefined, the reference reader fails on them too
    };

    props.sample_rate = Some(rate);
    props.channels = Some(channels);
    // Saturating so silence longer than the stream yields zero, not a wrapped length.
    finish(props, rate, samples.saturating_sub(silence), true);
}

/// A big-endian base-128 varint, returning value and octet count. Bounded at `MAX_VARINT`.
fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    for i in 0..MAX_VARINT {
        let byte = *b.get(i)?;
        value = value.checked_shl(7)?.checked_add(u64::from(byte & 0x7F))?;
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// The fixed SV7 header (`spec/MPC.md`). Little-endian words read MSB-first. Channels are always 2.
fn sv7(h: &[u8], props: &mut Props) {
    let Some(&version) = h.get(3) else { return };
    if version & 0x0F != SV7_VERSION {
        return;
    }
    let le32 = |at: usize| -> Option<u32> {
        h.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (Some(frames), Some(w2), Some(w5)) = (le32(4), le32(8), le32(20)) else {
        return;
    };

    let Some(&rate) = SAMPLE_RATES.get(((w2 >> 16) & 3) as usize) else {
        return;
    };
    let true_gapless = (w5 >> 31) & 1 == 1;
    let last_frame_samples = u64::from((w5 >> 20) & 0x7FF);

    // `streaminfo_read_header_sv7` verbatim. The 0 to `FRAME_LENGTH` normalisation precedes the
    // branch.
    let last = if last_frame_samples == 0 {
        FRAME_LENGTH
    } else if last_frame_samples > FRAME_LENGTH {
        return; // the reference reader fails the file here
    } else {
        last_frame_samples
    };
    let total = u64::from(frames) * FRAME_LENGTH;
    let samples = if true_gapless {
        total.saturating_sub(FRAME_LENGTH - last)
    } else {
        total.saturating_sub(SYNTH_DELAY)
    };

    props.channels = Some(2);
    // Exact only with TrueGapless. Otherwise the 481-sample latency is within one frame.
    finish(props, rate, samples, true_gapless);
}

fn finish(props: &mut Props, rate: u32, samples: u64, exact: bool) {
    props.sample_rate = Some(rate);
    // u128 since an SV8 sample count is a 63-bit varint.
    props.duration_ns = u64::try_from(u128::from(samples) * 1_000_000_000 / u128::from(rate)).ok();
    props.duration_exact = exact;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use crate::fixture::{mpc_sv7, mpc_sv8, mpc_varint};

    #[test]
    fn the_varint_is_big_endian_base_128_with_an_msb_continuation() {
        assert_eq!(varint(&[0x00]), Some((0, 1)));
        assert_eq!(varint(&[0x7F]), Some((127, 1)));
        assert_eq!(varint(&[0x81, 0x00]), Some((128, 2)));
        assert_eq!(varint(&[0xFF, 0x7F]), Some((16_383, 2)));
        assert_eq!(varint(&mpc_varint(10_395_840)), Some((10_395_840, 4)));
        for shift in 0..62 {
            let v = 1u64 << shift;
            assert_eq!(varint(&mpc_varint(v)), Some((v, mpc_varint(v).len())), "1 << {shift}");
        }
        assert_eq!(varint(&[0x81]), None);
        assert_eq!(varint(&[]), None);
        assert_eq!(varint(&[0xFF; 16]), None, "capped at nine octets");
    }

    #[test]
    fn sv8_stream_header_props_and_exact_duration() {
        let f = mpc_sv8(0, 2, 88_200, 0, &[]);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(44_100));
        assert_eq!(p.channels, Some(2));
        assert_eq!(p.duration_ns, Some(2_000_000_000));
        assert!(p.duration_exact);
    }

    #[test]
    fn sv8_beginning_silence_is_subtracted() {
        let f = mpc_sv8(1, 1, 48_000 + 1_200, 1_200, &[]);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(48_000));
        assert_eq!(p.channels, Some(1));
        assert_eq!(p.duration_ns, Some(1_000_000_000), "silence comes off the front");
    }

    #[test]
    fn sv8_every_defined_rate_index_and_undefined_ones_are_rejected() {
        for (index, &rate) in SAMPLE_RATES.iter().enumerate() {
            let f = mpc_sv8(index as u8, 2, u64::from(rate), 0, &[]);
            let p = props(&f, 0);
            assert_eq!(p.sample_rate, Some(rate), "index {index}");
            assert_eq!(p.duration_ns, Some(1_000_000_000));
        }
        for index in 4..8u8 {
            let f = mpc_sv8(index, 2, 44_100, 0, &[]);
            assert_eq!(props(&f, 0), Props::default(), "index {index} is undefined");
        }
    }

    #[test]
    fn the_channel_count_is_stored_biased_by_one() {
        for channels in 1..=16u32 {
            let f = mpc_sv8(0, channels as u8, 44_100, 0, &[]);
            assert_eq!(props(&f, 0).channels, Some(channels), "{channels} channels");
        }
    }

    #[test]
    fn the_walk_finds_sh_behind_other_packets() {
        let f = mpc_sv8(0, 2, 44_100, 0, &[(*b"EI", vec![0x50, 1, 23, 0])]);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(44_100));
        assert_eq!(p.duration_ns, Some(1_000_000_000));
    }

    #[test]
    fn sv8_junk_and_bad_versions_yield_nothing() {
        let mut f = mpc_sv8(0, 2, 44_100, 0, &[]);
        f[4] = b'1';
        assert_eq!(props(&f, 0), Props::default());
        let mut g = mpc_sv8(0, 2, 44_100, 0, &[]);
        let sh_payload = 4 + 2 + 1; // MPCK + key + one size octet
        g[sh_payload + 4] = 7;
        assert_eq!(props(&g, 0), Props::default());
        assert_eq!(props(b"MPCK", 0), Props::default());
        assert_eq!(props(b"", 0), Props::default());
    }

    #[test]
    fn sv7_true_gapless_is_exact_and_the_old_form_is_not() {
        let f = mpc_sv7(0, 100, true, 500);
        let p = props(&f, 0);
        assert_eq!(p.sample_rate, Some(44_100));
        assert_eq!(p.channels, Some(2), "SV7 is always stereo");
        let samples = 100 * 1152 - (1152 - 500);
        assert_eq!(p.duration_ns, Some(samples * 1_000_000_000 / 44_100));
        assert!(p.duration_exact);

        let g = mpc_sv7(0, 100, false, 0);
        let q = props(&g, 0);
        assert_eq!(q.duration_ns, Some((100 * 1152 - 481) * 1_000_000_000 / 44_100));
        assert!(!q.duration_exact, "the last frame's real length is unknown");
    }

    #[test]
    fn sv7_rate_indices_and_bad_versions() {
        for (index, &rate) in SAMPLE_RATES.iter().enumerate() {
            let f = mpc_sv7(index as u8, 100, true, 1_152);
            assert_eq!(props(&f, 0).sample_rate, Some(rate), "index {index}");
        }
        let mut f = mpc_sv7(0, 10, true, 1_152);
        f[3] = 0x17;
        assert_eq!(props(&f, 0).sample_rate, Some(44_100), "PNS set is still SV7");
        f[3] = 0x08;
        assert_eq!(props(&f, 0), Props::default());
        let g = mpc_sv7(0, 10, true, 1_153);
        assert_eq!(props(&g, 0), Props::default());
    }

    #[test]
    fn truncation_never_panics() {
        for f in [
            mpc_sv8(0, 2, 44_100, 0, &[(*b"RG", vec![1, 0, 0, 0, 0, 0, 0, 0, 0])]),
            mpc_sv7(1, 500, true, 900),
        ] {
            for n in 0..f.len() {
                let _ = props(&f[..n], 0);
            }
        }
    }

    #[test]
    fn single_byte_corruption_never_panics() {
        for base in [mpc_sv8(0, 2, 10_395_840, 0, &[]), mpc_sv7(2, 1_000, false, 0)] {
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
