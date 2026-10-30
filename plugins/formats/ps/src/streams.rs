//! What each stream of a program stream carries, from its id, the program stream map and, for
//! video without a map, its first bytes. And the format family it leaves under.

use pf_mpegts::psi::codec_of_stream_type;
use profluens_core::ctx::StreamKind;

use crate::demux::key_of;
use crate::es::Video;
use crate::pack::{PRIVATE_1, Sub, sub_of};

/// `MpegAudio` is ISO/IEC 11172-3 or 13818-3, any layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Mpeg1Video,
    Mpeg2Video,
    Mpeg4Visual,
    H264,
    Hevc,
    MpegAudio,
    Ac3,
    Dts,
    Lpcm,
    Subpicture,
}

/// The families the Matroska and transport stream demuxers emit and the decoders take. MPEG-1 video
/// had none and is `mpeg1`, which libavmpeg2dec takes beside `mpeg2`.
pub fn family_of(codec: Codec) -> (&'static str, StreamKind) {
    match codec {
        Codec::Mpeg1Video => ("mpeg1", StreamKind::Video),
        Codec::Mpeg2Video => ("mpeg2", StreamKind::Video),
        Codec::Mpeg4Visual => ("mpeg4/asp", StreamKind::Video),
        Codec::H264 => ("h264/annexb", StreamKind::Video),
        Codec::Hevc => ("h265/annexb", StreamKind::Video),
        Codec::MpegAudio => ("mp3", StreamKind::Audio),
        Codec::Ac3 => ("ac3", StreamKind::Audio),
        Codec::Dts => ("dts", StreamKind::Audio),
        Codec::Lpcm => (profluens_audio::FAMILY, StreamKind::Audio),
        Codec::Subpicture => ("subtitle/vobsub", StreamKind::Text),
    }
}

pub fn video_of(codec: Codec) -> Option<Video> {
    match codec {
        Codec::Mpeg1Video | Codec::Mpeg2Video => Some(Video::Mpeg12),
        Codec::Mpeg4Visual => Some(Video::Mpeg4),
        Codec::H264 => Some(Video::H264),
        Codec::Hevc => Some(Video::Hevc),
        _ => None,
    }
}

/// The codec of the stream of `key`, `None` for one nothing here reads. `psm_type` is zero without
/// a map, `first` its first payload bytes, `mpeg2` whether the packs are MPEG-2.
pub fn codec_of(key: u16, psm_type: u8, first: &[u8], mpeg2: bool) -> Option<Codec> {
    let (id, sub) = ((key >> 8) as u8, key as u8);
    if id == PRIVATE_1 {
        return match sub_of(sub) {
            Sub::Subpicture => Some(Codec::Subpicture),
            Sub::Ac3 => Some(Codec::Ac3),
            Sub::Dts => Some(Codec::Dts),
            Sub::Lpcm => Some(Codec::Lpcm),
            Sub::Unknown => None,
        };
    }
    if psm_type != 0 {
        return match codec_of_stream_type(psm_type) {
            pf_mpegts::Codec::Mpeg1Video => Some(Codec::Mpeg1Video),
            pf_mpegts::Codec::Mpeg2Video => Some(Codec::Mpeg2Video),
            pf_mpegts::Codec::Mpeg4Visual => Some(Codec::Mpeg4Visual),
            pf_mpegts::Codec::H264 => Some(Codec::H264),
            pf_mpegts::Codec::Hevc => Some(Codec::Hevc),
            pf_mpegts::Codec::MpegAudio => Some(Codec::MpegAudio),
            pf_mpegts::Codec::Ac3 => Some(Codec::Ac3),
            _ => None,
        };
    }
    match id {
        0xC0..=0xDF => Some(Codec::MpegAudio),
        0xE0..=0xEF => Some(sniff(first, mpeg2)),
        _ => None,
    }
}

fn codes(b: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    b.windows(3).enumerate().filter(|(_, w)| *w == [0, 0, 1]).filter_map(|(i, _)| {
        let rest = b.get(i + 3..)?;
        Some((*rest.first()?, rest))
    })
}

/// Video by its start codes, as FFmpeg probes a stream without a map. MPEG-2 by default.
fn sniff(first: &[u8], mpeg2: bool) -> Codec {
    if let Some((_, seq)) = codes(first).find(|(c, _)| *c == 0xB3) {
        // A sequence extension follows the sequence header in MPEG-2 (ISO/IEC 13818-2 6.2.2).
        let mut after = codes(&seq[1..]).map(|(c, rest)| (c, rest.get(1).map(|b| b >> 4)));
        return match after.next() {
            Some((0xB5, Some(1))) => Codec::Mpeg2Video,
            Some(_) => Codec::Mpeg1Video,
            None if mpeg2 => Codec::Mpeg2Video,
            None => Codec::Mpeg1Video,
        };
    }
    if codes(first).any(|(c, _)| matches!(c, 0xB0 | 0xB6)) {
        return Codec::Mpeg4Visual;
    }
    for (c, rest) in codes(first) {
        match c {
            // An H.264 SPS or AUD (Table 7-1), an H.265 VPS, SPS or AUD (Table 7-1).
            _ if c & 0x9F == 0x07 || c & 0x9F == 0x09 => return Codec::H264,
            0x40 | 0x42 | 0x46 if rest.get(1) == Some(&1) => return Codec::Hevc,
            _ => {}
        }
    }
    match codes(first).any(|(c, _)| (0x20..=0x2F).contains(&c)) {
        true => Codec::Mpeg4Visual,
        false => Codec::Mpeg2Video,
    }
}

/// The keys in the order a DVD menu lists them, video, then audio and subpictures by id.
pub fn rank(key: u16) -> (u8, u16) {
    let class = match (key >> 8) as u8 {
        0xE0..=0xEF => 0,
        0xC0..=0xDF => 1,
        PRIVATE_1 => match sub_of(key as u8) {
            Sub::Subpicture => 2,
            _ => 1,
        },
        _ => 3,
    };
    (class, key)
}

pub const VIDEO_0: u16 = key_of(0xE0, 0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_name_the_dvd_sub_streams_and_mpeg_audio() {
        assert_eq!(codec_of(key_of(0xBD, 0x80), 0, &[], true), Some(Codec::Ac3));
        assert_eq!(codec_of(key_of(0xBD, 0x8A), 0, &[], true), Some(Codec::Dts));
        assert_eq!(codec_of(key_of(0xBD, 0xA0), 0, &[], true), Some(Codec::Lpcm));
        assert_eq!(codec_of(key_of(0xBD, 0x3F), 0, &[], true), Some(Codec::Subpicture));
        assert_eq!(codec_of(key_of(0xBD, 0x60), 0, &[], true), None);
        assert_eq!(codec_of(key_of(0xC3, 0), 0, &[], false), Some(Codec::MpegAudio));
        assert_eq!(codec_of(key_of(0xF0, 0), 0, &[], false), None);
    }

    #[test]
    fn a_map_overrides_and_refuses_what_is_not_read() {
        assert_eq!(codec_of(VIDEO_0, 0x1B, &[0, 0, 1, 0xB3], true), Some(Codec::H264));
        assert_eq!(codec_of(VIDEO_0, 0x24, &[], true), Some(Codec::Hevc));
        assert_eq!(codec_of(VIDEO_0, 0x10, &[], true), Some(Codec::Mpeg4Visual));
        assert_eq!(codec_of(VIDEO_0, 0x01, &[], true), Some(Codec::Mpeg1Video));
        assert_eq!(codec_of(key_of(0xC0, 0), 0x0F, &[], true), None, "AAC");
    }

    #[test]
    fn video_without_a_map_is_told_by_its_start_codes() {
        let seq = [0, 0, 1, 0xB3, 0x14, 0, 0xF0, 0x13, 0xFF, 0xFF, 0xE0, 0x18];
        let ext = [0, 0, 1, 0xB5, 0x14, 0x8A];
        let gop = [0, 0, 1, 0xB8, 0, 8];
        let m2: Vec<u8> = [&seq[..], &ext].concat();
        let m1: Vec<u8> = [&seq[..], &gop].concat();
        assert_eq!(sniff(&m2, false), Codec::Mpeg2Video);
        assert_eq!(sniff(&m1, true), Codec::Mpeg1Video);
        assert_eq!(sniff(&seq, true), Codec::Mpeg2Video, "cut, the packs decide");
        assert_eq!(sniff(&seq, false), Codec::Mpeg1Video);
        assert_eq!(sniff(&[0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x67], true), Codec::H264);
        assert_eq!(sniff(&[0, 0, 0, 1, 0x46, 0x01, 0x50], true), Codec::Hevc);
        assert_eq!(sniff(&[0, 0, 1, 0xB0, 1, 0, 0, 1, 0xB5], true), Codec::Mpeg4Visual);
        assert_eq!(sniff(&[1, 2, 3], false), Codec::Mpeg2Video);
    }

    #[test]
    fn ranks_put_video_first() {
        let mut keys = [key_of(0xBD, 0x20), key_of(0xBD, 0x80), key_of(0xC0, 0), VIDEO_0];
        keys.sort_by_key(|k| rank(*k));
        assert_eq!(keys, [VIDEO_0, key_of(0xBD, 0x80), key_of(0xC0, 0), key_of(0xBD, 0x20)]);
    }
}
