//! Typefind from a file's first bytes, and the head each container's demuxer is built from.
//! App-side setup before any pipeline exists, the heads read through `profluens-core` fetch
//! machines.

pub mod head;

use std::fmt;

use pf_mp3::frame::Header as MpaHeader;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Mkv,
    Mp4,
    Ogg,
    Flv,
    Avi,
    Ts,
    Ps,
    Flac,
    Mp3,
    AdtsAac,
    Wav,
    Ac3,
    Eac3,
    Dts,
}

impl Kind {
    pub fn is_elementary(self) -> bool {
        matches!(
            self,
            Kind::Flac | Kind::Mp3 | Kind::AdtsAac | Kind::Wav | Kind::Ac3 | Kind::Eac3 | Kind::Dts
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Mkv => "Matroska/WebM",
            Kind::Mp4 => "ISO-BMFF (MP4)",
            Kind::Ogg => "Ogg",
            Kind::Flv => "FLV",
            Kind::Avi => "AVI (RIFF)",
            Kind::Ts => "MPEG-TS",
            Kind::Ps => "MPEG-PS",
            Kind::Flac => "FLAC",
            Kind::Mp3 => "MP3",
            Kind::AdtsAac => "ADTS AAC",
            Kind::Wav => "WAV (RIFF/WAVE)",
            Kind::Ac3 => "AC-3",
            Kind::Eac3 => "E-AC-3",
            Kind::Dts => "DTS",
        }
    }
}

/// Prefix bytes [`probe`] reads. Every magic sits in the first 12, TS needs five packets and a
/// bare MPEG audio stream the headers of three frames of up to 1729 bytes.
pub const PREFIX_LEN: usize = 4096;

/// Frames from byte 0 a bare MPEG audio sync is checked over.
const MPA_RUN: usize = 3;

const SUPPORTED: &str = "Matroska/WebM (.mkv/.webm), MP4/ISO-BMFF (.mp4/.m4a/.mov), Ogg (.ogg/.oga), \
     FLV (.flv), AVI (.avi), MPEG-TS (.ts/.m2ts), MPEG-PS (.mpg/.mpeg/.vob), FLAC (.flac), MP3 (.mp3), ADTS AAC (.aac/.adts), WAV (.wav), \
     AC-3 (.ac3), E-AC-3 (.eac3), DTS (.dts)";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProbeError {
    lead: [u8; 8],
    len: u8,
}

impl ProbeError {
    pub fn lead(&self) -> &[u8] {
        &self.lead[..usize::from(self.len)]
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unrecognized file, leading bytes {:02X?} match no supported container or codec. \
             Supported are {SUPPORTED}",
            self.lead()
        )
    }
}

impl std::error::Error for ProbeError {}

/// Fixed-offset magics go first, the self-syncing MPEG audio words last, so a container that
/// holds an MPEG frame is never taken for one.
pub fn probe(prefix: &[u8]) -> Result<Kind, ProbeError> {
    // EBML header id, RFC 8794 section 5.
    if prefix.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Ok(Kind::Mkv);
    }
    // A leading ftyp box, ISO/IEC 14496-12 section 4.3.
    if prefix.get(4..8) == Some(b"ftyp") {
        return Ok(Kind::Mp4);
    }
    if prefix.starts_with(b"OggS") {
        return Ok(Kind::Ogg);
    }
    // A pack start code in MPEG-2 ('01') or MPEG-1 ('0010') syntax, H.222.0 2.5.3.3 and
    // ISO/IEC 11172-1 2.4.3.2. Elementary video starts 00 00 01 B3 and is not one.
    if prefix.starts_with(&[0, 0, 1, 0xBA])
        && prefix.get(4).is_some_and(|b| b & 0xC0 == 0x40 || b & 0xF0 == 0x20)
    {
        return Ok(Kind::Ps);
    }
    // The signature and version 1, Adobe FLV 10.1 E.2.
    if prefix.starts_with(b"FLV\x01") {
        return Ok(Kind::Flv);
    }
    if prefix.starts_with(b"RIFF") {
        match prefix.get(8..12) {
            Some(b"AVI ") => return Ok(Kind::Avi),
            Some(b"WAVE") => return Ok(Kind::Wav),
            _ => {}
        }
    }
    if let Some(kind) = audio_magic(prefix) {
        return Ok(kind);
    }
    if is_ts(prefix) {
        return Ok(Kind::Ts);
    }
    // ADTS pins layer 00, ISO/IEC 14496-3 section 1.A.2.2.1. Before MP3, which shares FF Fx.
    if is_adts(prefix) {
        return Ok(Kind::AdtsAac);
    }
    if prefix.starts_with(b"ID3") {
        let after = id3v2_len(prefix).and_then(|n| prefix.get(n..)).unwrap_or_default();
        return Ok(behind_id3(after));
    }
    if is_mpeg_audio(prefix) {
        return Ok(Kind::Mp3);
    }
    let mut lead = [0u8; 8];
    let len = prefix.len().min(8);
    lead[..len].copy_from_slice(&prefix[..len]);
    Err(ProbeError { lead, len: len as u8 })
}

/// FLAC, AC-3, E-AC-3 and DTS by the magic or sync word at byte 0.
fn audio_magic(p: &[u8]) -> Option<Kind> {
    if p.starts_with(b"fLaC") {
        return Some(Kind::Flac);
    }
    // The 0B 77 sync word, then bsid, A/52 sections 5.4.1.1 and E.2.3.1.6.
    if p.len() >= 6 && p[..2] == [0x0B, 0x77] {
        match p[5] >> 3 {
            0..=10 => return Some(Kind::Ac3),
            11..=16 => return Some(Kind::Eac3),
            _ => {}
        }
    }
    // DTS core sync in its 16-bit and 14-bit forms, ETSI TS 102 114 section 5.3.1.
    let &[a, b, c, d] = p.get(..4)? else { return None };
    let sync = u32::from_be_bytes([a, b, c, d]);
    matches!(sync, 0x7FFE_8001 | 0xFE7F_0180 | 0x1FFF_E800 | 0xFF1F_00E8).then_some(Kind::Dts)
}

fn is_adts(p: &[u8]) -> bool {
    p.len() >= 2 && p[0] == 0xFF && matches!(p[1], 0xF1 | 0xF9)
}

/// The 0x47 sync byte at 188 (H.222.0 2.4.3), 192 (M2TS) or 204 (RS parity) byte strides. Five
/// packets in a row from any offset, or every packet of a whole shorter file from 0 (4 for M2TS).
fn is_ts(prefix: &[u8]) -> bool {
    const RUN: usize = 5;
    let run = |from: usize, stride: usize| {
        let tail = prefix.get(from..).unwrap_or_default();
        tail.iter().step_by(stride).take_while(|&&b| b == 0x47).count()
    };
    [188, 192, 204].into_iter().any(|stride| {
        if prefix.len() < RUN * stride {
            return [0, 4].into_iter().any(|from| {
                let n = prefix.len().saturating_sub(from).div_ceil(stride);
                n >= 2 && run(from, stride) == n
            });
        }
        (0..stride).any(|from| run(from, stride) >= RUN)
    })
}

/// MPEG audio frames in a row from byte 0, ISO/IEC 11172-3 2.4.1.3, each of the first's version,
/// layer and rate and each at the length the one before states. Every one of [`MPA_RUN`] that fits
/// in the prefix holds, two at least, or one frame is the whole file, an ID3v1 tag behind it. A
/// lone sync word is too common in text and other bytes, a UTF-16 BOM is one.
fn is_mpeg_audio(prefix: &[u8]) -> bool {
    let p = &prefix[..prefix.len().min(PREFIX_LEN)];
    let Some(first) = MpaHeader::parse(p) else { return false };
    let v1 = |at: usize| p.len().checked_sub(128) == Some(at) && p[at..].starts_with(b"TAG");
    let (mut at, mut frames) = (0, 0);
    while frames < MPA_RUN && at + 4 <= p.len() && !v1(at) {
        let same = MpaHeader::parse(&p[at..]).filter(|h| h.same_stream(&first));
        // free format states no length to find the next frame at
        match same.and_then(|h| h.frame_len()).filter(|&n| n > 4) {
            Some(n) => (at, frames) = (at + n, frames + 1),
            None => return false,
        }
    }
    frames >= 2 || (frames == 1 && p.len() < PREFIX_LEN && (at == p.len() || v1(at)))
}

/// The length of an ID3v2 tag leading `prefix`, footer included, id3v2.4.0 section 3.1.
pub fn id3v2_len(prefix: &[u8]) -> Option<usize> {
    let hdr = prefix.get(..10)?;
    if &hdr[..3] != b"ID3" || hdr[3] == 0xFF || hdr[4] == 0xFF {
        return None;
    }
    if hdr[6..10].iter().any(|&b| b & 0x80 != 0) {
        return None;
    }
    let size = hdr[6..10].iter().fold(0usize, |n, &b| n << 7 | usize::from(b));
    let footer = if hdr[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + size + footer)
}

/// The stream behind an ID3v2 tag, MPEG audio unless another's magic or sync word starts it.
pub fn behind_id3(after: &[u8]) -> Kind {
    match audio_magic(after) {
        Some(kind) => kind,
        None if is_adts(after) => Kind::AdtsAac,
        None => Kind::Mp3,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::disallowed_methods)]

    use super::*;

    fn prefix(head: &[u8]) -> [u8; PREFIX_LEN] {
        let mut v = [0u8; PREFIX_LEN];
        v[..head.len()].copy_from_slice(head);
        v
    }

    #[test]
    fn table_of_magics() {
        let cases: &[(&str, &[u8], Kind)] = &[
            ("mkv EBML", &[0x1A, 0x45, 0xDF, 0xA3, 0x01, 0x00], Kind::Mkv),
            ("mp4 ftyp isom", b"\x00\x00\x00\x20ftypisom", Kind::Mp4),
            ("mp4 ftyp mp42", b"\x00\x00\x00\x18ftypmp42", Kind::Mp4),
            ("mov ftyp qt", b"\x00\x00\x00\x14ftypqt  ", Kind::Mp4),
            ("ogg", b"OggS\x00\x02", Kind::Ogg),
            ("flv", b"FLV\x01\x05\x00\x00\x00\x09", Kind::Flv),
            ("flac", b"fLaC\x00\x00\x00\x22", Kind::Flac),
            ("wav", b"RIFF\x24\x08\x00\x00WAVEfmt ", Kind::Wav),
            ("avi", b"RIFF\x00\x00\x00\x00AVI LIST", Kind::Avi),
            ("adts mpeg4", &[0xFF, 0xF1, 0x50, 0x80], Kind::AdtsAac),
            ("adts mpeg2", &[0xFF, 0xF9, 0x4C, 0x80], Kind::AdtsAac),
            ("mp3 id3", b"ID3\x04\x00\x00", Kind::Mp3),
            ("ac3", &[0x0B, 0x77, 0x1F, 0x2B, 0x14, 0x40], Kind::Ac3),
            ("eac3", &[0x0B, 0x77, 0x01, 0x7F, 0x3F, 0x84], Kind::Eac3),
            ("dts be", &[0x7F, 0xFE, 0x80, 0x01], Kind::Dts),
            ("dts le", &[0xFE, 0x7F, 0x01, 0x80], Kind::Dts),
            ("dts 14 be", &[0x1F, 0xFF, 0xE8, 0x00], Kind::Dts),
            ("dts 14 le", &[0xFF, 0x1F, 0x00, 0xE8], Kind::Dts),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(probe(&prefix(bytes)), Ok(*want), "case {name}");
        }
    }

    #[test]
    fn flv_is_its_signature_and_version_one() {
        assert_eq!(probe(&prefix(b"FLV\x01\x04\x00\x00\x00\x09")), Ok(Kind::Flv), "sound alone");
        assert_eq!(probe(&prefix(b"FLV\x01\x00\x00\x00\x00\x0D")), Ok(Kind::Flv), "no flags");
        assert!(probe(b"FLV\x02\x05").is_err(), "no version 2");
        assert!(probe(b"FLV").is_err());
        assert!(!Kind::Flv.is_elementary());
        assert_eq!(Kind::Flv.label(), "FLV");
    }

    #[test]
    fn ps_is_a_pack_header_of_either_syntax() {
        assert_eq!(probe(&prefix(&[0, 0, 1, 0xBA, 0x44, 0, 4, 0, 4, 1])), Ok(Kind::Ps), "MPEG-2");
        assert_eq!(
            probe(&prefix(&[0, 0, 1, 0xBA, 0x21, 0, 1, 0, 1, 0x80])),
            Ok(Kind::Ps),
            "MPEG-1"
        );
        assert!(probe(&prefix(&[0, 0, 1, 0xBA, 0x80])).is_err(), "neither syntax");
        assert!(probe(&prefix(&[0, 0, 1, 0xB3, 0x14, 0, 0xF0])).is_err(), "elementary video");
        assert!(probe(&[0, 0, 1, 0xBA]).is_err(), "cut");
        assert!(!Kind::Ps.is_elementary());
        assert_eq!(Kind::Ps.label(), "MPEG-PS");
    }

    #[test]
    fn an_id3_tag_in_front_of_flac_is_flac() {
        let tagged =
            prefix(b"ID3\x04\x00\x00\x00\x00\x00\x06\x00\x00\x00\x00\x00\x00fLaC\x00\x00\x00\x22");
        assert_eq!(id3v2_len(&tagged), Some(16));
        assert_eq!(probe(&tagged), Ok(Kind::Flac));
        let footer = prefix(b"ID3\x04\x00\x10\x00\x00\x00\x003DI\x04\x00\x10\x00\x00\x00\x00fLaC");
        assert_eq!(probe(&footer), Ok(Kind::Flac));
        let mp3 = prefix(b"ID3\x04\x00\x00\x00\x00\x00\x00\xFF\xFB\x90\x00");
        assert_eq!(probe(&mp3), Ok(Kind::Mp3));
        let long = prefix(b"ID3\x04\x00\x00\x00\x00\x10\x00");
        assert_eq!((probe(&long), id3v2_len(&long)), (Ok(Kind::Mp3), Some(10 + 2048)));
        assert_eq!(behind_id3(b"fLaC\x00"), Kind::Flac);
        assert_eq!(behind_id3(b""), Kind::Mp3);
    }

    /// ID3v2 in front of ADTS, AC-3, E-AC-3 or DTS is that stream, as taggers write them.
    #[test]
    fn an_id3_tag_in_front_of_other_audio_is_that_audio() {
        let tagged = |body: &[u8]| {
            let mut f = b"ID3\x04\x00\x00\x00\x00\x00\x0A".to_vec();
            f.extend_from_slice(&[0; 10]);
            f.extend_from_slice(body);
            f.resize(PREFIX_LEN, 0);
            f
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/out");
        let cases = [
            ("audio.aac", Kind::AdtsAac),
            ("audio.ac3", Kind::Ac3),
            ("audio.eac3", Kind::Eac3),
            ("audio.dts", Kind::Dts),
            ("audio.mp3", Kind::Mp3),
            ("audio.flac", Kind::Flac),
        ];
        for (name, want) in cases {
            let Ok(body) = std::fs::read(root.join(name)) else { continue };
            assert_eq!(probe(&body[..body.len().min(PREFIX_LEN)]), Ok(want), "{name} bare");
            assert_eq!(probe(&tagged(&body[..PREFIX_LEN])), Ok(want), "{name} behind ID3");
        }
        let syncs: [(&[u8], Kind); 4] = [
            (&[0xFF, 0xF1, 0x50, 0x80], Kind::AdtsAac),
            (&[0x0B, 0x77, 0x1F, 0x2B, 0x14, 0x40], Kind::Ac3),
            (&[0x0B, 0x77, 0x01, 0x7F, 0x3F, 0x84], Kind::Eac3),
            (&[0x7F, 0xFE, 0x80, 0x01], Kind::Dts),
        ];
        for (sync, want) in syncs {
            assert_eq!(probe(&tagged(sync)), Ok(want), "{sync:02X?}");
        }
        assert_eq!(probe(&tagged(&[0xFF, 0xFB, 0x90, 0x00])), Ok(Kind::Mp3));
    }

    #[test]
    fn id3_sizes_are_synchsafe() {
        assert_eq!(
            id3v2_len(b"ID3\x04\x00\x00\x01\x02\x03\x04"),
            Some(10 + (1 << 21 | 2 << 14 | 3 << 7 | 4))
        );
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x80\x00\x00\x00"), None, "high bit set");
        assert_eq!(id3v2_len(b"ID3\xFF\x00\x00\x00\x00\x00\x00"), None, "version FF");
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00"), None, "short header");
    }

    #[test]
    fn adts_and_mp3_are_disambiguated_by_layer() {
        assert_eq!(probe(&[0xFF, 0xF1]), Ok(Kind::AdtsAac));
        assert_eq!(probe(&frames(&[0xFF, 0xFB, 0x90, 0x00], 3)), Ok(Kind::Mp3));
        // MPEG-2.5 with the reserved layer 00 and not ADTS's F1 or F9 is neither.
        assert!(probe(&[0xFF, 0xE1]).is_err());
    }

    /// `n` frames of the header `h`, each as long as it states, its body zero.
    fn frames(h: &[u8; 4], n: usize) -> Vec<u8> {
        let len = pf_mp3::frame::Header::parse(h).unwrap().frame_len().unwrap();
        let mut f = vec![0u8; len];
        f[..4].copy_from_slice(h);
        f.repeat(n)
    }

    #[test]
    fn mpeg_audio_is_frames_in_a_row() {
        // MPEG-1 layer III at 128 and 320 kbit/s, MPEG-2 layer III, MPEG-1 layer II at 384 kbit/s
        // and 32 kHz, the longest frame there is
        for h in [[0xFF, 0xFB, 0x90, 0x00], [0xFF, 0xFB, 0xE0, 0x00], [0xFF, 0xF3, 0x82, 0xC4]] {
            assert_eq!(probe(&frames(&h, 20)[..PREFIX_LEN]), Ok(Kind::Mp3), "{h:02X?}");
            assert_eq!(probe(&frames(&h, 2)), Ok(Kind::Mp3), "two whole {h:02X?}");
        }
        let long = frames(&[0xFF, 0xFD, 0xE8, 0x00], 3);
        assert_eq!(long.len() / 3, 1728);
        assert_eq!(probe(&long[..PREFIX_LEN]), Ok(Kind::Mp3), "the third header in the prefix");
        // the first frame's padding moves the second
        let mut padded = frames(&[0xFF, 0xFB, 0x92, 0x00], 1);
        padded.extend(frames(&[0xFF, 0xFB, 0x90, 0x00], 2));
        assert_eq!(probe(&padded), Ok(Kind::Mp3));
    }

    #[test]
    fn a_lone_or_inconsistent_sync_is_no_mpeg_audio() {
        let h = [0xFF, 0xFB, 0x90, 0x00];
        let mut lone = frames(&h, 1);
        lone.resize(PREFIX_LEN, 0x20);
        assert!(probe(&lone).is_err(), "one header, then text");
        let mut layer = frames(&h, 1);
        layer.extend(frames(&[0xFF, 0xFD, 0x90, 0x00], 2));
        assert!(probe(&layer).is_err(), "the second frame of another layer");
        let mut rate = frames(&h, 2);
        rate.extend(frames(&[0xFF, 0xFB, 0x94, 0x00], 1));
        assert!(probe(&rate).is_err(), "the third of another rate");
        let mut free = vec![0u8; 3 * 417];
        free[..4].copy_from_slice(&[0xFF, 0xFB, 0x00, 0x00]);
        assert!(probe(&free).is_err(), "free format states no length");
        assert!(probe(&frames(&h, 1)[..400]).is_err(), "a frame cut short");
    }

    /// A whole file of one frame is MPEG audio, an ID3v1 tag behind it or none.
    #[test]
    fn a_single_whole_frame_is_mpeg_audio() {
        let one = frames(&[0xFF, 0xFB, 0x90, 0x00], 1);
        assert_eq!(probe(&one), Ok(Kind::Mp3));
        let mut tagged = one.clone();
        tagged.extend_from_slice(b"TAG");
        tagged.resize(one.len() + 128, b' ');
        assert_eq!(probe(&tagged), Ok(Kind::Mp3));
        let mut junk = one.clone();
        junk.extend_from_slice(b"xyz");
        assert!(probe(&junk).is_err());
    }

    /// The sweep's subtitle, UTF-16 behind its FF FE mark, reads as an MPEG-1 layer I header.
    #[test]
    fn a_utf16_subtitle_is_no_mpeg_audio() {
        let text = "1\r\n00:00:01,000 --> 00:00:02,500\r\nHello there\r\n\r\n".repeat(100);
        let mut srt = vec![0xFF, 0xFE];
        srt.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        assert!(pf_mp3::frame::Header::parse(&srt).is_some(), "the mark is a header");
        assert!(probe(&srt[..PREFIX_LEN]).is_err());
        assert!(probe(&srt[..300]).is_err(), "a short one");
        assert!(probe(text.as_bytes()).is_err(), "UTF-8");
    }

    /// `n` packets of `stride` bytes after `skip` bytes of a cut packet, sync 0x47 at `sync`.
    fn ts(stride: usize, sync: usize, skip: usize, n: usize) -> Vec<u8> {
        let mut v = vec![0xFFu8; stride * n];
        for k in 0..n {
            v[k * stride + sync] = 0x47;
        }
        v.split_off(skip)
    }

    #[test]
    fn ts_is_found_at_each_packet_size_and_any_offset() {
        for (stride, sync) in [(188, 0), (192, 4), (204, 0)] {
            assert_eq!(probe(&ts(stride, sync, 0, 25)[..PREFIX_LEN]), Ok(Kind::Ts), "{stride}");
            assert_eq!(
                probe(&ts(stride, sync, 77, 26)[..PREFIX_LEN]),
                Ok(Kind::Ts),
                "{stride} cut"
            );
            assert_eq!(probe(&ts(stride, sync, 0, 2)), Ok(Kind::Ts), "{stride} two packets");
        }
    }

    #[test]
    fn a_lone_or_broken_sync_is_no_ts() {
        assert!(probe(&ts(188, 0, 0, 1)).is_err(), "one packet");
        let mut broken = ts(188, 0, 0, 25);
        broken[2 * 188] = 0;
        assert!(probe(&broken[..PREFIX_LEN]).is_err());
        assert!(probe(b"GIF89a").is_err());
    }

    #[test]
    fn committed_fixtures_probe_as_their_extension() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut seen = 0;
        let dirs = ["formats/ts", "formats/tsdemux", "formats/mp4", "formats/avi", "formats/ogg"];
        for dir in dirs.into_iter().chain(["formats/ps"]) {
            let dir = root.join("plugins").join(dir).join("tests/fixtures");
            for e in std::fs::read_dir(&dir).unwrap() {
                let path = e.unwrap().path();
                let ext = path.extension().and_then(|x| x.to_str()).unwrap_or_default();
                let want = match ext {
                    "ts" | "m2ts" => Kind::Ts,
                    "vob" | "mpg" => Kind::Ps,
                    "mp4" => Kind::Mp4,
                    "mkv" | "mka" => Kind::Mkv,
                    "avi" => Kind::Avi,
                    "opus" | "ogg" => Kind::Ogg,
                    _ => continue,
                };
                let bytes = std::fs::read(&path).unwrap();
                let got = probe(&bytes[..bytes.len().min(PREFIX_LEN)]);
                assert_eq!(got, Ok(want), "{}", path.display());
                seen += 1;
            }
        }
        assert!(seen >= 10, "{seen} fixtures");
    }

    #[test]
    fn an_unknown_riff_form_is_refused() {
        assert!(probe(&prefix(b"RIFF\x00\x00\x00\x00XYZ ")).is_err());
    }

    #[test]
    fn unknown_names_its_bytes_and_the_supported_formats() {
        let err = probe(b"\x89PNG\r\n\x1a\n\x00").unwrap_err();
        assert_eq!(err.lead(), b"\x89PNG\r\n\x1a\n");
        let msg = err.to_string();
        assert!(msg.contains("Matroska") && msg.contains("MP4") && msg.contains("89, 50"), "{msg}");
        assert_eq!(probe(b"").unwrap_err().lead(), b"");
    }

    #[test]
    fn short_prefixes_do_not_panic() {
        for n in 0..12 {
            let _ = probe(&[0xFFu8; 12][..n]);
            let _ = probe(&b"RIFF\x00\x00\x00\x00AVI "[..n]);
            let _ = probe(&[0x0B, 0x77, 0, 0, 0, 0][..n.min(6)]);
        }
    }

    #[test]
    fn is_elementary_partitions_the_kinds() {
        for k in [Kind::Flac, Kind::Mp3, Kind::AdtsAac, Kind::Wav, Kind::Ac3, Kind::Eac3, Kind::Dts]
        {
            assert!(k.is_elementary(), "{k:?}");
        }
        for k in [Kind::Mkv, Kind::Mp4, Kind::Ogg, Kind::Avi, Kind::Ts, Kind::Ps] {
            assert!(!k.is_elementary(), "{k:?}");
        }
    }
}
