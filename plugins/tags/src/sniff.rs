//! Format sniffing from the head of a file. Independent of `pf_play::probe` to keep deps small.

/// A container/codec family, as far as the first bytes of a file can tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    Flac,
    Mp3,
    Mp4,
    OggOpus,
    OggVorbis,
    OggFlac,
    OggSpeex,
    Wav,
    Aiff,
    Ape,
    WavPack,
    Musepack,
    Mkv,
    #[default]
    Unknown,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
            Format::Mp4 => "mp4",
            Format::OggOpus => "ogg/opus",
            Format::OggVorbis => "ogg/vorbis",
            Format::OggFlac => "ogg/flac",
            Format::OggSpeex => "ogg/speex",
            Format::Wav => "wav",
            Format::Aiff => "aiff",
            Format::Ape => "ape",
            Format::WavPack => "wavpack",
            Format::Musepack => "musepack",
            Format::Mkv => "mkv",
            Format::Unknown => "unknown",
        }
    }

    /// Whether this scanner extracts tags and props for the format.
    pub fn is_parsed(self) -> bool {
        !matches!(self, Format::Unknown)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Sniffed {
    pub format: Format,
    /// Offset of the stream's first byte, non-zero when an ID3v2 tag precedes it.
    pub body: u64,
}

/// Size of a leading ID3v2 tag including header and footer (ID3v2.4.0 3.1).
pub(crate) fn id3v2_len(head: &[u8]) -> Option<u64> {
    let h = head.get(..10)?;
    if &h[0..3] != b"ID3" {
        return None;
    }
    if h[3] == 0xFF || h[4] == 0xFF {
        return None;
    }
    if h[6..10].iter().any(|b| b & 0x80 != 0) {
        return None;
    }
    let size = h[6..10].iter().fold(0u64, |acc, &b| (acc << 7) | u64::from(b & 0x7F));
    let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + size + footer)
}

pub(crate) fn sniff(head: &[u8]) -> Sniffed {
    // Skip a leading ID3v2 tag and re-sniff, falling back to MP3 when nothing recognisable follows.
    if let Some(len) = id3v2_len(head) {
        let body = usize::try_from(len).unwrap_or(usize::MAX);
        return match head.get(body..) {
            Some(rest) if !rest.is_empty() => {
                let inner = sniff_bare(rest);
                Sniffed {
                    format: if inner == Format::Unknown { Format::Mp3 } else { inner },
                    body: len,
                }
            }
            _ => Sniffed { format: Format::Mp3, body: len },
        };
    }
    Sniffed { format: sniff_bare(head), body: 0 }
}

fn sniff_bare(head: &[u8]) -> Format {
    let b = head;
    // RFC 9639 8.
    if b.starts_with(b"fLaC") {
        return Format::Flac;
    }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE" {
        return Format::Wav;
    }
    // ISO/IEC 14496-12 4.3, `ftyp` at offset 4.
    if b.len() >= 12 && &b[4..8] == b"ftyp" {
        return Format::Mp4;
    }
    // AIFF 1.3. The form type is required since `FORM` also starts non-audio IFF documents.
    if b.len() >= 12 && &b[0..4] == b"FORM" && matches!(&b[8..12], b"AIFF" | b"AIFC") {
        return Format::Aiff;
    }
    if b.starts_with(crate::wv::MAGIC) {
        return Format::WavPack;
    }
    if b.starts_with(crate::ape::MAGIC) || b.starts_with(crate::ape::MAGIC_LARGE) {
        return Format::Ape;
    }
    // The SV7 version octet low nibble is the stream version, the high nibble is the PNS flag.
    if b.starts_with(crate::mpc::MAGIC_SV8) {
        return Format::Musepack;
    }
    if b.starts_with(crate::mpc::MAGIC_SV7) && b.get(3).is_some_and(|v| v & 0x0F == 7) {
        return Format::Musepack;
    }
    if b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Format::Mkv;
    }
    if b.starts_with(b"OggS") {
        return ogg_bos(b);
    }
    // ISO/IEC 11172-3 2.4.3.1 sync. The reserved version and layer rule out ADTS AAC.
    if b.len() >= 2 && b[0] == 0xFF && (b[1] & 0xE0) == 0xE0 {
        let version = (b[1] >> 3) & 0x03; // 01 is reserved
        let layer = (b[1] >> 1) & 0x03; // 00 is reserved
        if version != 0b01 && layer != 0b00 {
            return Format::Mp3;
        }
    }
    Format::Unknown
}

fn ogg_bos(page: &[u8]) -> Format {
    let Some(&nsegs) = page.get(26) else {
        return Format::Unknown;
    };
    let payload = 27 + nsegs as usize;
    let Some(id) = page.get(payload..) else {
        return Format::Unknown;
    };
    if id.starts_with(b"OpusHead") {
        return Format::OggOpus;
    }
    if id.starts_with(b"\x01vorbis") {
        return Format::OggVorbis;
    }
    if id.starts_with(b"\x7fFLAC") {
        return Format::OggFlac;
    }
    if id.starts_with(pf_ogg::ident::SPEEX_MAGIC) {
        return Format::OggSpeex;
    }
    Format::Unknown
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;

    fn ogg_page(id_header: &[u8]) -> Vec<u8> {
        let mut p = b"OggS".to_vec();
        p.extend_from_slice(&[0, 2]); // version 0, header_type BOS
        p.extend_from_slice(&[0u8; 8]); // granule
        p.extend_from_slice(&[0u8; 8]); // serial + seq
        p.extend_from_slice(&[0u8; 4]); // crc
        p.push(1); // one lacing value
        p.push(id_header.len() as u8);
        p.extend_from_slice(id_header);
        p
    }

    #[test]
    fn magics() {
        assert_eq!(sniff(b"fLaC\x00\x00\x00\x22").format, Format::Flac);
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0, 0, 0, 0]);
        wav.extend_from_slice(b"WAVEfmt ");
        assert_eq!(sniff(&wav).format, Format::Wav);
        assert_eq!(sniff(b"\x00\x00\x00\x20ftypM4A \x00\x00").format, Format::Mp4);
        assert_eq!(sniff(&[0x1A, 0x45, 0xDF, 0xA3, 1, 2, 3, 4]).format, Format::Mkv);
        assert_eq!(sniff(&ogg_page(b"OpusHead...")).format, Format::OggOpus);
        assert_eq!(sniff(&ogg_page(b"\x01vorbis..")).format, Format::OggVorbis);
        assert_eq!(sniff(&ogg_page(b"\x7fFLAC\x01\x00")).format, Format::OggFlac);
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0x00]).format, Format::Mp3);
    }

    #[test]
    fn magics_of_the_apev2_family_and_the_iff_forms() {
        let mut aiff = b"FORM".to_vec();
        aiff.extend_from_slice(&[0, 0, 1, 0]);
        aiff.extend_from_slice(b"AIFFCOMM");
        assert_eq!(sniff(&aiff).format, Format::Aiff);
        aiff[8..12].copy_from_slice(b"AIFC");
        assert_eq!(sniff(&aiff).format, Format::Aiff);
        aiff[8..12].copy_from_slice(b"8SVX");
        assert_eq!(sniff(&aiff).format, Format::Unknown);

        assert_eq!(sniff(b"wvpk\x20\x01\x00\x00\x02\x04").format, Format::WavPack);
        assert_eq!(sniff(b"MAC \x96\x0f\x00\x00").format, Format::Ape);
        assert_eq!(sniff(b"MACF\x96\x0f\x00\x00").format, Format::Ape);
        assert_eq!(sniff(b"MPCK SH").format, Format::Musepack);
        assert_eq!(sniff(b"MP+\x07\x10\x00\x00\x00").format, Format::Musepack);
        assert_eq!(sniff(b"MP+\x17\x10\x00\x00\x00").format, Format::Musepack, "PNS nibble set");
        assert_eq!(sniff(b"MP+\x08\x10\x00\x00\x00").format, Format::Unknown);
        assert_eq!(sniff(&ogg_page(b"Speex   1.2rc1")).format, Format::OggSpeex);
        assert_eq!(sniff(&ogg_page(b"Speex  x1.2rc1")).format, Format::Unknown);
    }

    #[test]
    fn an_id3_prefixed_apev2_family_file_is_not_mp3() {
        for (magic, want) in [
            (&b"MAC \x96\x0f\x00\x00"[..], Format::Ape),
            (&b"wvpk\x20\x01\x00\x00"[..], Format::WavPack),
            (&b"MPCK SH"[..], Format::Musepack),
            (&b"MP+\x07\x10\x00\x00"[..], Format::Musepack),
        ] {
            let mut f = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
            f.extend_from_slice(&[0u8; 10]);
            f.extend_from_slice(magic);
            let s = sniff(&f);
            assert_eq!(s.format, want, "{magic:?}");
            assert_eq!(s.body, 20, "the parser is pointed at the real header");
        }
    }

    #[test]
    fn adts_aac_is_not_mp3() {
        assert_eq!(sniff(&[0xFF, 0xF1, 0x50, 0x80]).format, Format::Unknown);
    }

    #[test]
    fn id3_prefix_is_skipped() {
        let mut f = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
        f.extend_from_slice(&[0u8; 10]);
        f.extend_from_slice(b"fLaC");
        let s = sniff(&f);
        assert_eq!(s.format, Format::Flac);
        assert_eq!(s.body, 20);

        let mut m = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
        m.extend_from_slice(&[0u8; 10]);
        m.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        assert_eq!(sniff(&m).format, Format::Mp3);
    }

    #[test]
    fn id3_footer_flag_adds_ten() {
        let f = b"ID3\x04\x00\x10\x00\x00\x00\x0a";
        assert_eq!(id3v2_len(f), Some(30));
    }

    #[test]
    fn malformed_heads_never_panic() {
        for len in 0..40usize {
            let mut v = vec![0xFFu8; len];
            assert!(sniff(&v).format.name().is_ascii());
            v.iter_mut().enumerate().for_each(|(i, b)| *b = (i * 7) as u8);
            let _ = sniff(&v);
        }
        assert_eq!(sniff(b"OggS").format, Format::Unknown);
        assert_eq!(sniff(b"ID3").format, Format::Unknown);
    }
}
