//! The speakers an audio track's head states, as a WAVE `dwChannelMask`.

use profluens_audio::layout::{
    BC, BL, BR, FC, FL, FLC, FR, FRC, LFE, SL, SR, STEREO, TFL, TFR, default_layout,
};

/// The layout of a `family` track of `channels` whose codec head is `head`, 0 when the head
/// states none or one of another count. Vorbis and FLAC fix the order of each count (Vorbis I
/// 4.3.9, RFC 9639 9.1.3), Opus by its mapping family (RFC 7845 5.1.1.2), AAC by the
/// AudioSpecificConfig's channelConfiguration (ISO/IEC 14496-3 1.6.3.4), MPEG audio is mono or
/// stereo. AC-3, DTS and TrueHD state theirs in the frames, not the head.
pub(crate) fn of(family: &str, head: &[u8], channels: u32) -> u64 {
    let said = match family {
        "vorbis" | "flac" | "flac/ogg" => default_layout(channels),
        "opus" => opus(head),
        "aac" => asc_config(head).map_or(0, aac),
        "mp3" if channels <= 2 => default_layout(channels),
        _ => 0,
    };
    if said.count_ones() == channels { said } else { 0 }
}

/// An OpusHead's mapping family 0 is mono or stereo, 1 the Vorbis order, others unpositioned.
fn opus(head: &[u8]) -> u64 {
    let Some(h) = head.strip_prefix(b"OpusHead").and_then(|h| h.get(..11)) else { return 0 };
    match (h[10], u32::from(h[1])) {
        (0, n @ (1 | 2)) | (1, n) => default_layout(n),
        _ => 0,
    }
}

/// The channelConfiguration of an AudioSpecificConfig, behind the object type and the sampling
/// frequency, each with its escape.
fn asc_config(asc: &[u8]) -> Option<u8> {
    let bit = |at: usize| asc.get(at / 8).map(|b| b >> (7 - at % 8) & 1);
    let bits =
        |at: usize, n: usize| (at..at + n).try_fold(0u32, |v, i| Some(v << 1 | u32::from(bit(i)?)));
    let mut at = 5;
    if bits(0, 5)? == 31 {
        at += 6;
    }
    at += if bits(at, 4)? == 15 { 4 + 24 } else { 4 };
    bits(at, 4).map(|c| c as u8)
}

/// The speakers of a channelConfiguration, as FFmpeg's AAC decoder lays them out. 0, a program
/// config element, and 13, 22.2, state none here.
fn aac(config: u8) -> u64 {
    let back = FL | FR | FC | LFE | BL | BR;
    match config {
        1 => FC,
        2 => STEREO,
        3 => FL | FR | FC,
        4 => FL | FR | FC | BC,
        5 => FL | FR | FC | BL | BR,
        6 => back,
        7 => back | FLC | FRC,
        11 => back | BC,
        12 => back | SL | SR,
        14 => back | TFL | TFR,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus_head(channels: u8, family: u8) -> [u8; 19] {
        let mut h = [0u8; 19];
        h[..8].copy_from_slice(b"OpusHead");
        (h[8], h[9], h[18]) = (1, channels, family);
        h
    }

    #[test]
    fn an_opus_head_states_its_mapping_familys_layout() {
        assert_eq!(of("opus", &opus_head(2, 0), 2), STEREO);
        assert_eq!(of("opus", &opus_head(1, 0), 1), FC);
        assert_eq!(of("opus", &opus_head(6, 1), 6), FL | FR | FC | LFE | BL | BR);
        assert_eq!(of("opus", &opus_head(8, 1), 8), default_layout(8));
        assert_eq!(of("opus", &opus_head(4, 255), 4), 0, "unpositioned");
        assert_eq!(of("opus", &opus_head(6, 1), 2), 0, "another count");
        assert_eq!(of("opus", b"OpusHea", 2), 0, "cut");
    }

    #[test]
    fn an_audio_specific_config_states_its_channel_configuration() {
        // AAC LC, 48 kHz, 2 channels
        assert_eq!(of("aac", &[0x11, 0x90], 2), STEREO);
        // AAC LC, 48 kHz, 6 channels
        assert_eq!(of("aac", &[0x11, 0xB0], 6), FL | FR | FC | LFE | BL | BR);
        // object type 31 escapes to 32 + 6 bits, frequency index 15 to 24 bits, config 1
        let esc = [0xF8, 0x1F, 0xFF, 0xFF, 0xFE, 0x20];
        assert_eq!(asc_config(&esc), Some(1));
        assert_eq!(of("aac", &esc, 1), FC);
        // a program config element states the speakers elsewhere
        assert_eq!(of("aac", &[0x11, 0x80], 2), 0);
        // HE-AAC v2 says mono, its parametric stereo decodes to two
        assert_eq!(of("aac", &[0x11, 0x88], 2), 0);
        assert_eq!(of("aac", &[0x11], 2), 0, "cut");
        assert_eq!(aac(7), FL | FR | FC | LFE | BL | BR | FLC | FRC);
        assert_eq!(aac(12), FL | FR | FC | LFE | BL | BR | SL | SR);
        assert_eq!(aac(13), 0, "22.2");
        for c in [1, 2, 3, 4, 5, 6, 7, 11, 12, 14] {
            let n = [0, 1, 2, 3, 4, 5, 6, 8, 0, 0, 0, 7, 8, 0, 8][c as usize];
            assert_eq!(aac(c).count_ones(), n, "config {c}");
        }
    }

    #[test]
    fn vorbis_flac_and_mpeg_audio_take_the_order_of_their_count() {
        assert_eq!(of("vorbis", &[], 6), FL | FR | FC | LFE | BL | BR);
        assert_eq!(of("flac", &[], 2), STEREO);
        assert_eq!(of("flac/ogg", &[], 1), FC);
        assert_eq!(of("flac", &[], 9), 0, "unpositioned");
        assert_eq!(of("mp3", &[], 1), FC);
        assert_eq!(of("mp3", &[], 3), 0);
        assert_eq!(of("ac3", &[], 6), 0, "in the frames");
    }
}
