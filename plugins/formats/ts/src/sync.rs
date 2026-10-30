//! Packet framing, 188, 192 (BDAV M2TS) or 204 (RS(204,188), EN 300 421 4.4.2) bytes.
//! The size is found from the sync byte spacing. Pure functions over a byte window.

use crate::packet::{PACKET_LEN, SYNC_BYTE};

/// BDAV M2TS TP_extra_header, layout from memory.
pub const EXTRA_HEADER_LEN: usize = 4;
pub const PARITY_LEN: usize = 16;
pub const FRAMED_MAX: usize = PACKET_LEN + PARITY_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketSize {
    Ts188,
    M2ts192,
    /// Parity bytes are skipped and not checked.
    Fec204,
}

/// Tie-break order.
pub const PACKET_SIZES: [PacketSize; 3] =
    [PacketSize::Ts188, PacketSize::M2ts192, PacketSize::Fec204];

pub const ANY_SIZE: u8 = 0x07;

#[inline]
pub const fn only(size: PacketSize) -> u8 {
    1 << size as u8
}

#[inline]
pub const fn framed_len(size: PacketSize) -> usize {
    match size {
        PacketSize::Ts188 => PACKET_LEN,
        PacketSize::M2ts192 => PACKET_LEN + EXTRA_HEADER_LEN,
        PacketSize::Fec204 => PACKET_LEN + PARITY_LEN,
    }
}

#[inline]
pub const fn sync_offset(size: PacketSize) -> usize {
    match size {
        PacketSize::M2ts192 => EXTRA_HEADER_LEN,
        PacketSize::Ts188 | PacketSize::Fec204 => 0,
    }
}

/// Low 30 bits in 27 MHz ticks, top two are copy permission. BDAV M2TS, from memory.
#[inline]
pub const fn arrival_time_stamp(header: &[u8; EXTRA_HEADER_LEN]) -> u32 {
    u32::from_be_bytes(*header) & 0x3FFF_FFFF
}

#[inline]
fn column(window: &[u8], at: usize, stride: usize, confirm: usize) -> bool {
    (0..confirm).all(|k| window.get(at + k * stride) == Some(&SYNC_BYTE))
}

#[inline]
fn any_in_column(window: &[u8], at: usize, stride: usize, confirm: usize) -> bool {
    (0..confirm).any(|k| window.get(at + k * stride) == Some(&SYNC_BYTE))
}

/// Bytes from a sync position on that must be present before `size` can be judged there.
pub fn lookahead(confirm: usize, size: PacketSize) -> usize {
    // The 192-byte test also reads the byte three after each sync byte.
    let last = if size == PacketSize::M2ts192 { 4 } else { 1 };
    confirm.saturating_sub(1) * framed_len(size) + last
}

enum Verdict {
    Starts(PacketSize),
    Nothing,
    Wait,
}

fn judge(window: &[u8], at: usize, confirm: usize, allowed: u8) -> Verdict {
    for size in PACKET_SIZES {
        if allowed & only(size) == 0 {
            continue;
        }
        if size == PacketSize::M2ts192 && at < EXTRA_HEADER_LEN {
            continue;
        }
        if at + lookahead(confirm, size) > window.len() {
            return Verdict::Wait;
        }
        let stride = framed_len(size);
        // The time stamp byte can pass for 0x47 three bytes early, which frames the real sync byte
        // as adaptation_field_control 00 (reserved, Table 2-5). One such byte refuses the window.
        if size == PacketSize::M2ts192 && any_in_column(window, at + 3, stride, confirm) {
            continue;
        }
        if column(window, at, stride, confirm) {
            return Verdict::Starts(size);
        }
    }
    Verdict::Nothing
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scan {
    /// The sync position, else the first position not judged yet.
    pub at: usize,
    pub size: Option<PacketSize>,
}

/// Judges at most `limit` positions from `from`. A size is judged only with its whole
/// [`lookahead`] present, so chunking never changes the answer. Ties go to the smallest size.
pub fn scan(window: &[u8], from: usize, confirm: usize, allowed: u8, limit: usize) -> Scan {
    let end = window.len().min(from.saturating_add(limit)).max(from);
    let mut at = from;
    while at < end {
        let Some(next) = window.get(at..end).and_then(|w| w.iter().position(|&b| b == SYNC_BYTE))
        else {
            break;
        };
        at += next;
        match judge(window, at, confirm, allowed) {
            Verdict::Starts(size) => return Scan { at, size: Some(size) },
            Verdict::Wait => return Scan { at, size: None },
            Verdict::Nothing => at += 1,
        }
    }
    Scan { at: end, size: None }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test fixtures
mod tests {
    use super::*;

    fn framed(size: PacketSize, count: usize) -> Vec<u8> {
        let mut v = vec![0xFFu8; count * framed_len(size)];
        for k in 0..count {
            v[k * framed_len(size) + sync_offset(size)] = SYNC_BYTE;
        }
        v
    }

    #[test]
    fn sizes() {
        assert_eq!(PACKET_SIZES.map(framed_len), [188, 192, 204]);
        assert_eq!(PACKET_SIZES.map(sync_offset), [0, 4, 0]);
        assert_eq!(
            only(PacketSize::Ts188) | only(PacketSize::M2ts192) | only(PacketSize::Fec204),
            ANY_SIZE
        );
        assert_eq!(arrival_time_stamp(&[0xFF, 0x00, 0x00, 0x01]), 0x3F00_0001);
        assert_eq!(arrival_time_stamp(&[0xC0, 0x00, 0x00, 0x00]), 0);
    }

    #[test]
    fn lookahead_per_size() {
        assert_eq!(lookahead(5, PacketSize::Ts188), 4 * 188 + 1);
        assert_eq!(lookahead(5, PacketSize::M2ts192), 4 * 192 + 4);
        assert_eq!(lookahead(5, PacketSize::Fec204), 4 * 204 + 1);
        assert_eq!(lookahead(1, PacketSize::Ts188), 1);
        assert_eq!(lookahead(1, PacketSize::M2ts192), 4);
    }

    #[test]
    fn each_size_is_found_at_its_first_sync_byte() {
        for size in PACKET_SIZES {
            let mut data = vec![0u8; 9];
            data.extend_from_slice(&framed(size, 6));
            let got = scan(&data, 0, 5, ANY_SIZE, usize::MAX);
            assert_eq!(got, Scan { at: 9 + sync_offset(size), size: Some(size) }, "{size:?}");
        }
    }

    #[test]
    fn a_position_waits_for_its_whole_lookahead() {
        for size in PACKET_SIZES {
            let data = framed(size, 6);
            let at = sync_offset(size);
            let need = at + lookahead(5, size);
            assert_eq!(
                scan(&data[..need - 1], 0, 5, ANY_SIZE, usize::MAX),
                Scan { at, size: None }
            );
            assert_eq!(
                scan(&data[..need], 0, 5, ANY_SIZE, usize::MAX),
                Scan { at, size: Some(size) }
            );
        }
    }

    #[test]
    fn four_sync_bytes_are_not_enough() {
        for size in PACKET_SIZES {
            let mut data = framed(size, 4);
            data.extend_from_slice(&[0xFF; 3 * FRAMED_MAX]);
            assert_eq!(scan(&data, 0, 5, ANY_SIZE, usize::MAX).size, None, "{size:?}");
        }
    }

    #[test]
    fn the_smallest_size_wins_a_tie() {
        let mut data = vec![0xFFu8; 6 * FRAMED_MAX];
        for k in 0..5 {
            data[4 + k * 188] = SYNC_BYTE;
            data[4 + k * 192] = SYNC_BYTE;
            data[4 + k * 204] = SYNC_BYTE;
        }
        assert_eq!(
            scan(&data, 0, 5, ANY_SIZE, usize::MAX),
            Scan { at: 4, size: Some(PacketSize::Ts188) }
        );
        data[4 + 4 * 188] = 0xFF;
        assert_eq!(
            scan(&data, 0, 5, ANY_SIZE, usize::MAX),
            Scan { at: 4, size: Some(PacketSize::M2ts192) }
        );
        data[4 + 4 * 192] = 0xFF;
        assert_eq!(
            scan(&data, 0, 5, ANY_SIZE, usize::MAX),
            Scan { at: 4, size: Some(PacketSize::Fec204) }
        );
        data[4 + 4 * 204] = 0xFF;
        assert_eq!(scan(&data, 0, 5, ANY_SIZE, usize::MAX).size, None);
    }

    #[test]
    fn the_earliest_position_wins_over_a_smaller_size() {
        let mut data = vec![0xFFu8; 7 * FRAMED_MAX];
        for k in 0..5 {
            data[10 + k * 204] = SYNC_BYTE;
            data[11 + k * 188] = SYNC_BYTE;
        }
        assert_eq!(
            scan(&data, 0, 5, ANY_SIZE, usize::MAX),
            Scan { at: 10, size: Some(PacketSize::Fec204) }
        );
        assert_eq!(
            scan(&data, 0, 5, only(PacketSize::Ts188), usize::MAX),
            Scan { at: 11, size: Some(PacketSize::Ts188) }
        );
    }

    #[test]
    fn a_192_byte_lock_needs_its_header() {
        let data = framed(PacketSize::M2ts192, 7);
        for cut in 1..=4 {
            let got = scan(&data[cut..], 0, 5, ANY_SIZE, usize::MAX);
            assert_eq!(
                got,
                Scan { at: 192 + 4 - cut, size: Some(PacketSize::M2ts192) },
                "cut {cut}"
            );
        }
        assert_eq!(scan(&data, 0, 5, ANY_SIZE, usize::MAX).at, 4);
    }

    #[test]
    fn a_time_stamp_byte_does_not_pass_for_the_sync_byte() {
        let mut data = framed(PacketSize::M2ts192, 8);
        for k in 0..8 {
            data[k * 192 + 1] = SYNC_BYTE;
        }
        let got = scan(&data[100..], 0, 5, ANY_SIZE, usize::MAX);
        assert_eq!(got, Scan { at: 192 - 100 + 4, size: Some(PacketSize::M2ts192) });
        data[2 * 192 + 4] = 0xFF;
        let got = scan(&data[100..], 0, 5, ANY_SIZE, usize::MAX);
        assert_eq!(got, Scan { at: 3 * 192 - 100 + 4, size: Some(PacketSize::M2ts192) });
        for k in 0..8 {
            data[k * 192 + 4] = 0xFF;
        }
        let got = scan(&data[100..], 0, 5, ANY_SIZE, usize::MAX);
        assert_eq!(got, Scan { at: 192 - 100 + 1, size: Some(PacketSize::M2ts192) });
    }

    #[test]
    fn the_candidate_set_and_the_limit_bound_the_search() {
        let mut data = vec![0u8; 50];
        data.extend_from_slice(&framed(PacketSize::Fec204, 6));
        let only_188 = only(PacketSize::Ts188);
        let got = scan(&data, 0, 5, only_188, usize::MAX);
        assert!(50 + 3 * 204 + lookahead(5, PacketSize::Ts188) > data.len());
        assert_eq!(got, Scan { at: 50 + 3 * 204, size: None });
        assert_eq!(scan(&[0u8; 300], 0, 5, ANY_SIZE, usize::MAX), Scan { at: 300, size: None });
        assert_eq!(scan(&data, 0, 5, ANY_SIZE, 50), Scan { at: 50, size: None });
        assert_eq!(
            scan(&data, 0, 5, ANY_SIZE, 51),
            Scan { at: 50, size: Some(PacketSize::Fec204) }
        );
        assert_eq!(scan(&data, 20, 5, ANY_SIZE, 30), Scan { at: 50, size: None });
        assert_eq!(scan(&[], 0, 5, ANY_SIZE, usize::MAX), Scan { at: 0, size: None });
        assert_eq!(scan(&data, data.len() + 9, 5, ANY_SIZE, usize::MAX).size, None);
    }

    #[test]
    fn one_confirmation_always_means_188() {
        for size in PACKET_SIZES {
            let data = framed(size, 3);
            let got = scan(&data, 0, 1, ANY_SIZE, usize::MAX);
            assert_eq!(got, Scan { at: sync_offset(size), size: Some(PacketSize::Ts188) });
        }
    }
}
