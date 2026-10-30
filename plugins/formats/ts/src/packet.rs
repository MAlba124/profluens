//! Transport packet layer, H.222.0 2.4.3.2 to 2.4.3.5.

pub const PACKET_LEN: usize = 188;
pub const SYNC_BYTE: u8 = 0x47;
pub const PID_PAT: u16 = 0x0000;
pub const PID_FIRST_FREE: u16 = 0x0010;
pub const PID_NULL: u16 = 0x1FFF;
pub const PID_COUNT: usize = 0x2000;

pub const PF_UNIT_START: u8 = 0x01;
pub const PF_PRIORITY: u8 = 0x02;
pub const PF_PAYLOAD: u8 = 0x04;
pub const PF_DISCONTINUITY: u8 = 0x08;
pub const PF_RANDOM_ACCESS: u8 = 0x10;
pub const PF_ES_PRIORITY: u8 = 0x20;
pub const PF_PCR: u8 = 0x40;
/// A PCR is present but its extension is outside 0 to 299.
pub const PF_PCR_INVALID: u8 = 0x80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    /// 27 MHz ticks, valid when [`PF_PCR`] is set.
    pub pcr: u64,
    pub pid: u16,
    /// `PF_*` bits.
    pub flags: u8,
    pub scrambling: u8,
    pub continuity: u8,
    /// [`PACKET_LEN`] when there is no payload.
    pub payload_start: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketError {
    Sync,
    TransportError,
    ReservedControl,
    AdaptationLength,
}

#[inline]
pub fn pid_of(raw: &[u8; PACKET_LEN]) -> u16 {
    (u16::from(raw[1] & 0x1F) << 8) | u16::from(raw[2])
}

#[inline]
pub fn payload<'a>(raw: &'a [u8; PACKET_LEN], pkt: &Packet) -> &'a [u8] {
    raw.get(usize::from(pkt.payload_start)..).unwrap_or(&[])
}

/// Fixed header (H.222.0 Table 2-2) and adaptation field (Table 2-6).
#[inline]
pub fn parse_packet(raw: &[u8; PACKET_LEN]) -> Result<Packet, PacketError> {
    if raw[0] != SYNC_BYTE {
        return Err(PacketError::Sync);
    }
    let b1 = raw[1];
    let b3 = raw[3];
    if b1 & 0x80 != 0 {
        return Err(PacketError::TransportError);
    }
    let mut flags = ((b1 >> 6) & PF_UNIT_START) | ((b1 >> 4) & PF_PRIORITY);
    let mut payload_start = 4usize;
    let mut pcr = 0u64;
    let control = (b3 >> 4) & 0x03;
    match control {
        0 => return Err(PacketError::ReservedControl),
        1 => flags |= PF_PAYLOAD,
        _ => {
            let len = usize::from(raw[4]);
            let max = if control == 3 { 182 } else { 183 };
            if len > max {
                return Err(PacketError::AdaptationLength);
            }
            if len > 0 {
                let f = raw[5];
                flags |= ((f >> 4) & PF_DISCONTINUITY)
                    | ((f >> 2) & PF_RANDOM_ACCESS)
                    | (f & PF_ES_PRIORITY);
                if f & 0x10 != 0 {
                    // One flag byte plus the 48-bit PCR field.
                    if len < 7 {
                        return Err(PacketError::AdaptationLength);
                    }
                    let base = (u64::from(raw[6]) << 25)
                        | (u64::from(raw[7]) << 17)
                        | (u64::from(raw[8]) << 9)
                        | (u64::from(raw[9]) << 1)
                        | u64::from(raw[10] >> 7);
                    let ext = (u64::from(raw[10] & 0x01) << 8) | u64::from(raw[11]);
                    // 2.4.2.2, PCR = base * 300 + ext.
                    if ext < 300 {
                        pcr = base * 300 + ext;
                        flags |= PF_PCR;
                    } else {
                        flags |= PF_PCR_INVALID;
                    }
                }
            }
            if control == 3 {
                flags |= PF_PAYLOAD;
                payload_start = 5 + len;
            } else {
                payload_start = PACKET_LEN;
            }
        }
    }
    Ok(Packet {
        pcr,
        pid: (u16::from(b1 & 0x1F) << 8) | u16::from(raw[2]),
        flags,
        scrambling: b3 >> 6,
        continuity: b3 & 0x0F,
        payload_start: payload_start as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank(pid: u16, b3: u8) -> [u8; PACKET_LEN] {
        let mut p = [0xFFu8; PACKET_LEN];
        p[0] = SYNC_BYTE;
        p[1] = (pid >> 8) as u8 & 0x1F;
        p[2] = pid as u8;
        p[3] = b3;
        p
    }

    #[test]
    fn payload_only() {
        let mut p = blank(0x1234, 0x17);
        p[1] |= 0x40;
        let pkt = parse_packet(&p).unwrap();
        assert_eq!(pkt.pid, 0x1234);
        assert_eq!(pkt.flags, PF_UNIT_START | PF_PAYLOAD);
        assert_eq!(pkt.continuity, 7);
        assert_eq!(pkt.scrambling, 0);
        assert_eq!(payload(&p, &pkt).len(), 184);
        assert_eq!(pid_of(&p), 0x1234);
    }

    #[test]
    fn adaptation_with_pcr() {
        let mut p = blank(0x100, 0x30);
        p[4] = 7;
        p[5] = 0x80 | 0x40 | 0x10;
        let base: u64 = 0x1_2345_6789;
        let ext: u64 = 299;
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = ((base & 1) as u8) << 7 | 0x7E | (ext >> 8) as u8;
        p[11] = ext as u8;
        let pkt = parse_packet(&p).unwrap();
        assert_eq!(pkt.flags, PF_PAYLOAD | PF_DISCONTINUITY | PF_RANDOM_ACCESS | PF_PCR);
        assert_eq!(pkt.pcr, base * 300 + ext);
        assert_eq!(pkt.payload_start, 12);
        p[10] = (p[10] & 0xFE) | 1;
        p[11] = 44;
        let pkt = parse_packet(&p).unwrap();
        assert_eq!(pkt.flags & (PF_PCR | PF_PCR_INVALID), PF_PCR_INVALID);
    }

    #[test]
    fn adaptation_bounds() {
        let mut p = blank(0x100, 0x30);
        p[4] = 0;
        assert_eq!(parse_packet(&p).unwrap().payload_start, 5);
        p[4] = 182;
        p[5] = 0;
        let pkt = parse_packet(&p).unwrap();
        assert_eq!(payload(&p, &pkt).len(), 1);
        p[4] = 183;
        assert_eq!(parse_packet(&p), Err(PacketError::AdaptationLength));
        let mut p = blank(0x100, 0x20);
        p[4] = 183;
        p[5] = 0;
        let pkt = parse_packet(&p).unwrap();
        assert_eq!(pkt.flags & PF_PAYLOAD, 0);
        assert!(payload(&p, &pkt).is_empty());
        p[4] = 184;
        assert_eq!(parse_packet(&p), Err(PacketError::AdaptationLength));
        let mut p = blank(0x100, 0x30);
        p[4] = 6;
        p[5] = 0x10;
        assert_eq!(parse_packet(&p), Err(PacketError::AdaptationLength));
    }

    #[test]
    fn rejects() {
        let mut p = blank(0x100, 0x10);
        p[0] = 0x48;
        assert_eq!(parse_packet(&p), Err(PacketError::Sync));
        let mut p = blank(0x100, 0x10);
        p[1] |= 0x80;
        assert_eq!(parse_packet(&p), Err(PacketError::TransportError));
        let p = blank(0x100, 0x00);
        assert_eq!(parse_packet(&p), Err(PacketError::ReservedControl));
    }

    #[test]
    fn every_byte_pattern_in_the_header_is_safe() {
        let mut p = [0u8; PACKET_LEN];
        p[0] = SYNC_BYTE;
        for b3 in 0..=255u8 {
            for len in 0..=255u8 {
                for f in [0x00u8, 0x10, 0xFF] {
                    p[3] = b3;
                    p[4] = len;
                    p[5] = f;
                    if let Ok(pkt) = parse_packet(&p) {
                        assert!(usize::from(pkt.payload_start) <= PACKET_LEN);
                        assert!(pkt.payload_start >= 4);
                    }
                }
            }
        }
    }
}
