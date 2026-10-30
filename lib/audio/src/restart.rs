//! How far ahead of a seek target each audio codec must restart.

/// Restart needs of one codec.
pub struct Restart {
    pub family: &'static str,
    /// The rate the two counts are in, zero for the track's own.
    pub clock: u32,
    /// Samples decoded ahead of a target before output matches an unbroken decode.
    pub primed: u32,
    /// The longest packet in samples, how far ahead of a time its packet may start.
    pub packet: u32,
}

pub static RESTARTS: [Restart; 7] = [
    // Longest block a stream may have (RFC 9639 section 8.2).
    Restart { family: "flac", clock: 0, primed: 0, packet: 65_535 },
    // One block of 256 of the six in a syncframe (ATSC A/52, from memory).
    Restart { family: "ac3", clock: 0, primed: 256, packet: 1_536 },
    // Not measured, taken as AC-3's (A/52 Annex E).
    Restart { family: "eac3", clock: 0, primed: 256, packet: 1_536 },
    // One frame of 1024. A frame with SBR is 2048 (ISO/IEC 14496-3, from memory).
    Restart { family: "aac", clock: 0, primed: 1_024, packet: 2_048 },
    // Three frames of 1152, the bit reservoir and the overlap (ISO/IEC 11172-3).
    Restart { family: "mp3", clock: 0, primed: 3 * 1_152, packet: 1_152 },
    // 200 ms at 48 kHz, the declared 80 ms left it converging. Packets are at most 120 ms.
    Restart { family: "opus", clock: 48_000, primed: 9_600, packet: 5_760 },
    // Half a long block, 2048 at the 4096 libvorbis may take (Vorbis I 4.2.2).
    Restart { family: "vorbis", clock: 0, primed: 2_048, packet: 2_048 },
];

/// How far ahead of a seek's target an audio track is read from, in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lead {
    /// The larger of the container's declared pre-roll and the measured row.
    pub pre_roll: u64,
    pub packet: u64,
}

/// The [`Lead`] of a track of `family` at `rate` Hz. `declared` is the container's pre-roll in ns.
pub fn lead_of(family: &str, rate: u64, declared: u64, packet: Option<u32>) -> Lead {
    let Some(row) = RESTARTS.iter().find(|row| row.family == family) else {
        return Lead { pre_roll: declared, packet: 0 };
    };
    let rate = if row.clock == 0 { rate.max(1) } else { u64::from(row.clock) };
    let span = |samples: u32| (u64::from(samples) * 1_000_000_000).div_ceil(rate);
    Lead { pre_roll: span(row.primed).max(declared), packet: span(packet.unwrap_or(row.packet)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pre_roll_is_the_larger_of_what_is_declared_and_what_was_measured() {
        let lead = |family, rate, declared| lead_of(family, rate, declared, None);
        assert_eq!(lead("aac", 48_000, 0), Lead { pre_roll: 21_333_334, packet: 42_666_667 });
        assert_eq!(lead("aac", 44_100, 0).pre_roll, 23_219_955);
        assert_eq!(lead("aac", 48_000, 50_000_000).pre_roll, 50_000_000);
        assert_eq!(lead("mp3", 48_000, 0), Lead { pre_roll: 72_000_000, packet: 24_000_000 });
        assert_eq!(lead("ac3", 48_000, 0), Lead { pre_roll: 5_333_334, packet: 32_000_000 });
        assert_eq!(lead("eac3", 48_000, 0), lead("ac3", 48_000, 0));
        assert_eq!(
            lead("opus", 44_100, 80_000_000),
            Lead { pre_roll: 200_000_000, packet: 120_000_000 }
        );
        assert_eq!(lead("opus", 0, 300_000_000).pre_roll, 300_000_000);
        assert_eq!(lead("vorbis", 48_000, 0), Lead { pre_roll: 42_666_667, packet: 42_666_667 });
        assert_eq!(lead("dts", 48_000, 7_000_000), Lead { pre_roll: 7_000_000, packet: 0 });
    }

    #[test]
    fn a_stated_packet_and_a_rate_of_zero() {
        assert_eq!(
            lead_of("flac", 48_000, 0, Some(4_608)),
            Lead { pre_roll: 0, packet: 96_000_000 }
        );
        assert_eq!(lead_of("flac", 48_000, 0, None).packet, 1_365_312_500);
        assert_eq!(lead_of("aac", 0, 0, None).pre_roll, 1_024_000_000_000);
    }
}
