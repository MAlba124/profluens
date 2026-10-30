//! Stream duration from the last page's granule position (RFC 3533 section 6, RFC 7845 section 4).

use crate::page::{CAPTURE_PATTERN, GRANULE_NONE, PageHeader};

/// Opus granule positions count 48 kHz samples whatever the input rate (RFC 7845 section 4).
pub const OPUS_GRANULE_RATE: u32 = 48_000;

/// The bytes at the end of a file read for its last page, more than the largest page (65 307 bytes,
/// section 6), so a whole page is always among them.
pub const OGG_TAIL_LEN: usize = 64 * 1024;

/// The bytes at the end read first by a demuxer that pulls, a page or two of a stream's last
/// second, so a slow link is not held on the whole tail ahead of the stream.
pub const OGG_TAIL_FIRST: usize = 16 * 1024;

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Scan `tail` backwards for the last valid Ogg page and return its granule position (RFC 3533
/// section 6, field 4).
pub fn last_granule(tail: &[u8], serial: Option<u32>) -> Option<u64> {
    if tail.len() < CAPTURE_PATTERN.len() {
        return None;
    }
    for start in (0..=tail.len() - CAPTURE_PATTERN.len()).rev() {
        if tail[start] != CAPTURE_PATTERN[0] || tail[start..start + 4] != CAPTURE_PATTERN {
            continue;
        }
        let Ok(page) = PageHeader::parse(&tail[start..]) else {
            continue;
        };
        if page.n_segments() == 0 {
            continue;
        }
        let granule = page.granule_position();
        if granule == GRANULE_NONE {
            continue;
        }
        if serial.is_some_and(|s| s != page.serial()) {
            continue;
        }
        return Some(granule);
    }
    None
}

/// Duration of an Opus stream in nanoseconds, from its last granule position and the `pre_skip` of
/// its ID header (RFC 7845 section 4 and section 5.1).
pub fn opus_duration_ns(last_granule: u64, pre_skip: u16) -> u64 {
    let samples = last_granule.saturating_sub(pre_skip as u64);
    let ns = samples as u128 * NANOS_PER_SEC / OPUS_GRANULE_RATE as u128;
    ns.min(u64::MAX as u128) as u64
}

/// Duration of a Vorbis stream in nanoseconds from its last granule and its header's sample rate.
pub fn vorbis_duration_ns(last_granule: u64, sample_rate: u32) -> Option<u64> {
    if sample_rate == 0 {
        return None;
    }
    let ns = last_granule as u128 * NANOS_PER_SEC / sample_rate as u128;
    Some(ns.min(u64::MAX as u128) as u64)
}

const OGG_FLAC_MAGIC: [u8; 5] = [0x7F, b'F', b'L', b'A', b'C'];

const OGG_FLAC_STREAMINFO: usize = 9 + 4 + 4;

/// The duration of the logical stream `serial` whose identification packet, the first of its bos
/// page, is `bos`, with `tail` the last bytes of the file, nanoseconds.
pub fn stream_duration_ns(bos: &[u8], tail: &[u8], serial: u32) -> Option<u64> {
    let clock = granule_clock(bos)?;
    let total = flac_streaminfo(bos).map_or(0, |(_, total)| total);
    let samples = if total > 0 { total } else { last_granule(tail, Some(serial))? };
    clock.ns(samples)
}

/// What a granule position of a logical stream counts, read off its identification packet, so a
/// demuxer has the stream time of a page as it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GranuleClock {
    /// 48 kHz samples, the first `pre_skip` of them priming (RFC 7845 section 4).
    Opus {
        pre_skip: u16,
    },
    Rate(u32),
}

impl GranuleClock {
    /// The stream time of `granule`, nanoseconds.
    pub fn ns(self, granule: u64) -> Option<u64> {
        match self {
            GranuleClock::Opus { pre_skip } => Some(opus_duration_ns(granule, pre_skip)),
            GranuleClock::Rate(rate) => vorbis_duration_ns(granule, rate),
        }
    }
}

/// The clock of the stream whose identification packet is `bos`, `None` for another mapping.
pub fn granule_clock(bos: &[u8]) -> Option<GranuleClock> {
    if let Some(id) = crate::ident::parse_opus_head(bos) {
        return Some(GranuleClock::Opus { pre_skip: id.pre_skip });
    }
    if let Some(id) = crate::ident::parse_vorbis_ident(bos) {
        return Some(GranuleClock::Rate(id.sample_rate));
    }
    flac_streaminfo(bos).map(|(rate, _)| GranuleClock::Rate(rate))
}

fn flac_streaminfo(bos: &[u8]) -> Option<(u32, u64)> {
    if !bos.starts_with(&OGG_FLAC_MAGIC) || bos.get(9..13) != Some(b"fLaC") {
        return None;
    }
    let b = bos.get(OGG_FLAC_STREAMINFO..OGG_FLAC_STREAMINFO + 18)?;
    let rate = u32::from(b[10]) << 12 | u32::from(b[11]) << 4 | u32::from(b[12]) >> 4;
    let total =
        u64::from(b[13] & 0x0F) << 32 | u64::from(u32::from_be_bytes([b[14], b[15], b[16], b[17]]));
    Some((rate, total))
}

#[cfg(test)]
// Tests build page fixtures.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::page::{flags, write_page};

    fn garbage(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| seed.wrapping_add(i as u8).wrapping_mul(31) | 1).collect()
    }

    fn payload_with_fake_capture() -> Vec<u8> {
        let mut p = b"OggS\x00\x00".to_vec();
        p.extend_from_slice(&garbage(60, 7));
        p
    }

    #[test]
    fn finds_the_last_page_in_a_noisy_tail() {
        let mut lead = Vec::new();
        write_page(&mut lead, 0, 1_000, 0xAAAA, 1, &[10], &garbage(10, 3));
        let mut tail = lead[13..].to_vec(); // cut mid-header: no capture pattern survives

        let body = payload_with_fake_capture();
        write_page(&mut tail, 0, 2_000, 0xAAAA, 2, &[body.len() as u8], &body);
        write_page(&mut tail, flags::EOS, 3_000, 0xAAAA, 3, &[4], b"last");
        tail.extend_from_slice(&garbage(37, 11));

        assert_eq!(last_granule(&tail, None), Some(3_000));
        assert_eq!(last_granule(&tail, Some(0xAAAA)), Some(3_000));
    }

    #[test]
    fn a_fake_capture_pattern_in_a_payload_is_rejected_by_the_crc() {
        let body = payload_with_fake_capture();
        let mut tail = Vec::new();
        write_page(&mut tail, 0, 4_242, 7, 0, &[body.len() as u8], &body);
        assert_eq!(last_granule(&tail, None), Some(4_242));
    }

    #[test]
    fn a_run_of_zero_bytes_is_not_a_page() {
        let mut tail = Vec::new();
        write_page(&mut tail, 0, 5_000, 1, 0, &[4], b"real");
        tail.extend_from_slice(b"OggS");
        tail.extend_from_slice(&[0u8; 32]);
        assert!(PageHeader::parse(&tail[tail.len() - 36..]).is_err());
        assert_eq!(last_granule(&tail, None), Some(5_000));
    }

    #[test]
    fn a_zero_segment_page_is_skipped() {
        let mut tail = Vec::new();
        write_page(&mut tail, 0, 6_000, 1, 0, &[4], b"real");
        write_page(&mut tail, 0, 7_777, 1, 1, &[], &[]);
        assert!(PageHeader::parse(&tail[tail.len() - 27..]).is_ok(), "the impostor is CRC-valid");
        assert_eq!(last_granule(&tail, None), Some(6_000));
    }

    #[test]
    fn skips_pages_with_no_finishing_packet() {
        let mut tail = Vec::new();
        write_page(&mut tail, 0, 9_000, 1, 0, &[4], b"aaaa");
        write_page(&mut tail, 0, GRANULE_NONE, 1, 1, &[4], b"bbbb");
        assert_eq!(last_granule(&tail, None), Some(9_000));
    }

    #[test]
    fn serial_filtering_picks_the_right_logical_stream() {
        let mut tail = Vec::new();
        write_page(&mut tail, 0, 48_000, 0xA1, 5, &[4], b"aud1");
        write_page(&mut tail, 0, 90_000, 0xB2, 5, &[4], b"vid1");
        write_page(&mut tail, 0, 96_000, 0xA1, 6, &[4], b"aud2");
        write_page(&mut tail, flags::EOS, 180_000, 0xB2, 6, &[4], b"vid2");

        assert_eq!(last_granule(&tail, None), Some(180_000));
        assert_eq!(last_granule(&tail, Some(0xA1)), Some(96_000));
        assert_eq!(last_granule(&tail, Some(0xB2)), Some(180_000));
        assert_eq!(last_granule(&tail, Some(0xC3)), None); // no such stream
    }

    #[test]
    fn a_page_whose_body_runs_past_the_slice_is_not_trusted() {
        let mut whole = Vec::new();
        write_page(&mut whole, 0, 100, 1, 0, &[4], b"aaaa");
        let complete = whole.len();
        write_page(&mut whole, 0, 200, 1, 1, &[8], b"bbbbbbbb");

        for cut in complete + 1..whole.len() {
            assert_eq!(last_granule(&whole[..cut], None), Some(100), "cut at {cut}");
        }
        assert_eq!(last_granule(&whole, None), Some(200));
    }

    #[test]
    fn truncations_and_junk_never_panic() {
        let mut tail = Vec::new();
        write_page(&mut tail, 0, 1, 1, 0, &[4], b"aaaa");
        write_page(&mut tail, 0, 2, 1, 1, &[4], b"bbbb");
        for n in 0..tail.len() {
            let _ = last_granule(&tail[..n], None);
            let _ = last_granule(&tail[n..], Some(1));
        }
        assert_eq!(last_granule(&[], None), None);
        assert_eq!(last_granule(b"Ogg", None), None);
        assert_eq!(last_granule(&garbage(1_000, 5), None), None);
        assert_eq!(last_granule(&[b'O'; 500], None), None);
    }

    #[test]
    fn opus_duration_subtracts_pre_skip_at_48khz() {
        assert_eq!(opus_duration_ns(48_000 * 10 + 312, 312), 10_000_000_000);
        assert_eq!(opus_duration_ns(960 + 312, 312), 20_000_000);
        assert_eq!(opus_duration_ns(100, 312), 0);
        assert_eq!(opus_duration_ns(0, 0), 0);
        assert_eq!(opus_duration_ns(48_000, 0), 1_000_000_000);
        assert!(opus_duration_ns(u64::MAX / 2, 0) > 0);
    }

    #[test]
    fn vorbis_duration_uses_the_ident_rate() {
        assert_eq!(vorbis_duration_ns(44_100, 44_100), Some(1_000_000_000));
        assert_eq!(vorbis_duration_ns(44_100 * 191 + 22_050, 44_100), Some(191_500_000_000));
        assert_eq!(vorbis_duration_ns(0, 48_000), Some(0));
        assert_eq!(vorbis_duration_ns(1, 44_100), Some(22_675)); // truncating, not rounding
        assert_eq!(vorbis_duration_ns(1_000, 0), None);
        assert!(vorbis_duration_ns(u64::MAX / 2, 8_000).is_some());
    }

    #[test]
    fn opus_head_plus_last_granule_gives_the_duration() {
        use crate::ident::parse_opus_head;

        const PRE_SKIP: u16 = 356;
        let mut head = b"OpusHead".to_vec();
        head.push(1); // version
        head.push(2); // channels
        head.extend_from_slice(&PRE_SKIP.to_le_bytes());
        head.extend_from_slice(&44_100u32.to_le_bytes()); // original input rate
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // mapping family

        let want_ns = 221_000_000_000u64;
        let granule = 48_000 * 221 + PRE_SKIP as u64;

        let mut file = Vec::new();
        write_page(&mut file, flags::BOS, 0, 0x1234, 0, &[head.len() as u8], &head);
        write_page(&mut file, 0, 0, 0x1234, 1, &[4], b"tags");
        write_page(&mut file, flags::EOS, granule, 0x1234, 2, &[4], b"audi");

        let bos = PageHeader::parse(&file).expect("bos page");
        let head = parse_opus_head(bos.payload()).expect("OpusHead");
        assert_eq!(head.pre_skip, PRE_SKIP);
        assert_eq!(head.input_sample_rate, 44_100);

        let last = last_granule(&file, Some(0x1234)).expect("last granule");
        assert_eq!(last, granule);
        assert_eq!(opus_duration_ns(last, head.pre_skip), want_ns);
    }

    fn page(flags: u8, granule: u64, serial: u32, seq: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        write_page(&mut v, flags, granule, serial, seq, &[body.len() as u8], body);
        v
    }

    fn opus_head(pre_skip: u16) -> Vec<u8> {
        let mut h = b"OpusHead".to_vec();
        h.extend_from_slice(&[1, 2]);
        h.extend_from_slice(&pre_skip.to_le_bytes());
        h.extend_from_slice(&44_100u32.to_le_bytes());
        h.extend_from_slice(&0i16.to_le_bytes());
        h.push(0);
        h
    }

    fn ogg_flac(rate: u32, total: u64) -> Vec<u8> {
        let mut si = [0u8; 34];
        si[10] = (rate >> 12) as u8;
        si[11] = (rate >> 4) as u8;
        si[12] = ((rate << 4) as u8) | (1 << 1);
        si[13] = (0x0F << 4) | ((total >> 32) as u8 & 0x0F);
        si[14..18].copy_from_slice(&(total as u32).to_be_bytes());
        let mut p = vec![0x7F, b'F', b'L', b'A', b'C', 1, 0, 0, 1];
        p.extend_from_slice(b"fLaC");
        p.extend_from_slice(&[0x80, 0, 0, 34]);
        p.extend_from_slice(&si);
        p
    }

    #[test]
    fn the_duration_follows_the_mapping() {
        const SERIAL: u32 = 0xABCD;
        let tail = page(flags::EOS, 48_000 * 90 + 312, SERIAL, 9, b"audio");
        assert_eq!(stream_duration_ns(&opus_head(312), &tail, SERIAL), Some(90_000_000_000));
        assert_eq!(stream_duration_ns(&opus_head(312), &[], SERIAL), None, "no tail, no last page");
        let mut vorbis = vec![0x01];
        vorbis.extend_from_slice(b"vorbis");
        vorbis.extend_from_slice(&0u32.to_le_bytes());
        vorbis.push(2);
        vorbis.extend_from_slice(&44_100u32.to_le_bytes());
        vorbis.extend_from_slice(&[0u8; 16]);
        let tail = page(flags::EOS, 44_100 * 30, SERIAL, 9, b"audio");
        assert_eq!(stream_duration_ns(&vorbis, &tail, SERIAL), Some(30_000_000_000));
        assert_eq!(
            stream_duration_ns(&ogg_flac(48_000, 48_000 * 7), &[], SERIAL),
            Some(7_000_000_000)
        );
        let tail = page(flags::EOS, 48_000 * 3, SERIAL, 9, b"audio");
        assert_eq!(stream_duration_ns(&ogg_flac(48_000, 0), &tail, SERIAL), Some(3_000_000_000));
        assert_eq!(stream_duration_ns(b"\x80theora", &tail, SERIAL), None);
    }

    #[test]
    fn a_granule_clock_follows_the_mapping() {
        let opus = granule_clock(&opus_head(312)).expect("Opus");
        assert_eq!(opus, GranuleClock::Opus { pre_skip: 312 });
        assert_eq!(opus.ns(48_000 * 2 + 312), Some(2_000_000_000), "48 kHz less the pre-skip");
        let flac = granule_clock(&ogg_flac(44_100, 0)).expect("FLAC");
        assert_eq!(flac.ns(44_100 * 3), Some(3_000_000_000));
        let mut vorbis = vec![0x01];
        vorbis.extend_from_slice(b"vorbis");
        vorbis.extend_from_slice(&0u32.to_le_bytes());
        vorbis.push(2);
        vorbis.extend_from_slice(&32_000u32.to_le_bytes());
        vorbis.extend_from_slice(&[0u8; 16]);
        assert_eq!(granule_clock(&vorbis).and_then(|c| c.ns(16_000)), Some(500_000_000));
        assert_eq!(granule_clock(b"\x80theora"), None);
    }

    #[test]
    fn the_last_granule_is_of_the_stream_timed() {
        let mut tail = page(0, 48_000 * 10, 0x0A, 8, b"aud");
        tail.extend_from_slice(&page(flags::EOS, 90_000 * 60, 0x0B, 8, b"vid"));
        assert_eq!(stream_duration_ns(&opus_head(0), &tail, 0x0A), Some(10_000_000_000));
    }
}
