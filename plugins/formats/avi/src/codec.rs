//! Stream format to src pad family mapping and per-track offer menus.

use profluens_core::format::OfferDesc;

use crate::riff::{Stream, StreamKind};

/// The `mpeg4/asp` family, MPEG-4 Part 2 Advanced Simple Profile (XviD/DivX).
pub const FAMILY_MPEG4_ASP: &str = "mpeg4/asp";
/// The `h264/annexb` family, H.264 in Annex B framing (rare in AVI).
pub const FAMILY_H264_ANNEXB: &str = "h264/annexb";
/// The `ac3` family, AC-3 (`wFormatTag` 0x2000) and DTS (0x2001).
pub const FAMILY_AC3: &str = "ac3";
/// DTS coherent acoustics, WAVE_FORMAT_DTS, as the mkv demuxer names it.
pub const FAMILY_DTS: &str = "dts";
/// The `mp3` family, `wFormatTag` 0x0055.
pub const FAMILY_MP3: &str = "mp3";
/// The `audio/raw` family, `wFormatTag` 0x0001.
pub const FAMILY_AUDIO_RAW: &str = "audio/raw";
pub const FAMILY_BYTES: &str = "bytes";
/// On2 VP6 and VP7 and Sorenson Spark, decoded by pf-libav.
pub const FAMILY_VP6: &str = "vp6";
pub const FAMILY_VP7: &str = "vp7";
pub const FAMILY_FLV1: &str = "flv1";

/// Every family a stream can be announced as, `bytes` aside.
pub static EMITS: &[&str] = &[
    FAMILY_MPEG4_ASP,
    FAMILY_H264_ANNEXB,
    FAMILY_VP6,
    FAMILY_VP7,
    FAMILY_FLV1,
    FAMILY_AC3,
    FAMILY_DTS,
    FAMILY_MP3,
    FAMILY_AUDIO_RAW,
];

/// The announce family for a discovered [`Stream`].
pub fn family_for(stream: &Stream) -> &'static str {
    match stream.kind {
        StreamKind::Video => video_family(stream.handler, &stream.strf),
        StreamKind::Audio => audio_family(stream.format_tag, stream.bits_per_sample),
        StreamKind::Other => FAMILY_BYTES,
    }
}

fn video_family(handler: crate::riff::FourCc, strf: &[u8]) -> &'static str {
    let bicompression: crate::riff::FourCc =
        strf.get(16..20).map(|s| [s[0], s[1], s[2], s[3]]).unwrap_or([0; 4]);
    // FFmpeg avidec.c names the codec by biCompression alone, a `divx` handler may head DIV3.
    let cc = fourcc_upper(if bicompression == [0; 4] { handler } else { bicompression });
    match &cc {
        // FFmpeg `riff.c`. VP6F and VP6A are left out, their FLV framing is not AVI's.
        b"VP60" | b"VP61" | b"VP62" => FAMILY_VP6,
        b"VP70" | b"VP71" => FAMILY_VP7,
        b"FLV1" | b"S263" => FAMILY_FLV1,
        _ if MPEG4_TAGS.contains(&cc) => FAMILY_MPEG4_ASP,
        _ if H264_TAGS.contains(&cc) => FAMILY_H264_ANNEXB,
        _ => FAMILY_BYTES,
    }
}

/// FFmpeg `riff.c` `ff_codec_bmp_tags` of AV_CODEC_ID_MPEG4 upper cased, and DX40.
/// MP43, DIV3 and the like are MS-MPEG4 v3, which the MPEG-4 Part 2 decoder cannot take.
#[rustfmt::skip]
const MPEG4_TAGS: [crate::riff::FourCc; 52] = [
    *b"FMP4", *b"DIVX", *b"DX50", *b"XVID", *b"MP4S", *b"M4S2", [4, 0, 0, 0], *b"ZMP4", *b"DIV1",
    *b"BLZ0", *b"MP4V", *b"UMP4", *b"WV1F", *b"SEDG", *b"RMP4", *b"3IV2", *b"WAWV", *b"FFDS",
    *b"FVFW", *b"DCOD", *b"MVXM", *b"PM4V", *b"SMP4", *b"DXGM", *b"VIDM", *b"M4T3", *b"GEOX",
    *b"G264", *b"HDX4", *b"DM4V", *b"DMK2", *b"DYM4", *b"DIGI", *b"EPHV", *b"EM4A", *b"M4CC",
    *b"SN40", *b"VSPX", *b"ULDX", *b"GEOV", *b"SIPP", *b"SM4V", *b"XVIX", *b"DREX", *b"QMP4",
    *b"PLV1", *b"GLV4", *b"GMP4", *b"MNM4", *b"GTM4", [4, 0, 0, 16], *b"DX40",
];

/// FFmpeg `riff.c` `ff_codec_bmp_tags` of AV_CODEC_ID_H264 upper cased.
#[rustfmt::skip]
const H264_TAGS: [crate::riff::FourCc; 12] = [
    *b"H264", *b"X264", *b"AVC1", *b"DAVC", *b"SMV2", *b"VSSH", *b"Q264", *b"V264", *b"GAVC",
    *b"UMSV", *b"TSHD", *b"INMC",
];

fn audio_family(format_tag: u16, bits: u16) -> &'static str {
    match (format_tag, bits) {
        (0x2000, _) => FAMILY_AC3,        // WAVE_FORMAT_DOLBY_AC3
        (0x2001, _) => FAMILY_DTS,        // WAVE_FORMAT_DTS
        (0x0050, _) => FAMILY_MP3,        // WAVE_FORMAT_MPEG, layer I and II
        (0x0055, _) => FAMILY_MP3,        // WAVE_FORMAT_MPEGLAYER3
        (0x0001, _) => FAMILY_AUDIO_RAW,  // WAVE_FORMAT_PCM
        (0x0003, 32) => FAMILY_AUDIO_RAW, // WAVE_FORMAT_IEEE_FLOAT, f64 would need narrowing
        _ => FAMILY_BYTES,
    }
}

/// The `audio/raw` sample name of a PCM or float stream, FFmpeg `riff.c` `ff_codec_wav_tags`.
pub fn raw_sample(format_tag: u16, bits: u16) -> &'static str {
    match (format_tag, bits) {
        (0x0003, _) => "f32",
        (_, 8) => "u8",
        (_, 24) => "s24",
        (_, 32) => "s32",
        _ => "s16",
    }
}

fn fourcc_upper(cc: crate::riff::FourCc) -> crate::riff::FourCc {
    [
        cc[0].to_ascii_uppercase(),
        cc[1].to_ascii_uppercase(),
        cc[2].to_ascii_uppercase(),
        cc[3].to_ascii_uppercase(),
    ]
}

/// Whether a video chunk is a picture a decode may start at.
pub fn chunk_is_key(family: &str, data: &[u8]) -> bool {
    let mut at = 0;
    while let Some(i) = data.get(at..).and_then(|d| d.windows(3).position(|w| w == [0, 0, 1])) {
        let p = at + i + 3;
        let Some(&code) = data.get(p) else {
            return false;
        };
        match family {
            FAMILY_MPEG4_ASP if code == 0xB6 => {
                return data.get(p + 1).is_some_and(|b| b >> 6 == 0);
            }
            FAMILY_H264_ANNEXB if code & 0x1f == 5 => return true,
            FAMILY_H264_ANNEXB if matches!(code & 0x1f, 1..=4) => return false,
            _ => at = p,
        }
    }
    false
}

/// The MPEG-4 Part 2 configuration at the front of `chunk`, its bytes up to the first VOP start
/// code when a VOL (start codes 0x20 to 0x2F) comes before it, the whole chunk when no VOP
/// follows, None without a VOL. XviD files keep the VOL in the first key chunk only, a decode
/// started elsewhere needs it as extradata.
pub fn vol_prefix(chunk: &[u8]) -> Option<&[u8]> {
    let mut vol = false;
    let mut at = 0;
    while let Some(i) = chunk.get(at..).and_then(|d| d.windows(3).position(|w| w == [0, 0, 1])) {
        let p = at + i;
        let Some(&code) = chunk.get(p + 3) else { break };
        match code {
            0xB6 => return vol.then(|| &chunk[..p]),
            0x20..=0x2F => vol = true,
            _ => {}
        }
        at = p + 3;
    }
    vol.then_some(chunk)
}

/// Whether audio comes as byte slices cut at the video rate rather than as codec frames.
pub fn reframes(family: &str, sample_size: u32) -> bool {
    sample_size == 1 && matches!(family, FAMILY_AC3 | FAMILY_MP3)
}

const AC3_KBPS: [u32; 19] =
    [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];
const MPA_KBPS: [[u16; 15]; 5] = [
    [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
    [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

/// The codec frame `data` starts with, `None` without a valid frame header.
pub fn frame_len(family: &str, data: &[u8]) -> Option<usize> {
    match family {
        FAMILY_AC3 => ac3_frame_len(data),
        FAMILY_MP3 => mpa_frame_len(data),
        _ => None,
    }
}

fn ac3_frame_len(d: &[u8]) -> Option<usize> {
    if d.len() < 6 || d[0] != 0x0b || d[1] != 0x77 {
        return None;
    }
    let bsid = d[5] >> 3;
    if bsid <= 10 {
        let fscod = (d[4] >> 6) as usize;
        let code = (d[4] & 0x3f) as usize;
        let kbps = *AC3_KBPS.get(code >> 1)? as usize;
        let words = match fscod {
            0 => kbps * 2,
            1 => kbps * 1536 * 1000 / 44_100 / 16 + (code & 1),
            2 => kbps * 3,
            _ => return None,
        };
        Some(words * 2)
    } else if bsid <= 16 {
        let words = (((d[2] & 7) as usize) << 8 | d[3] as usize) + 1;
        Some(words * 2)
    } else {
        None
    }
}

fn mpa_frame_len(d: &[u8]) -> Option<usize> {
    if d.len() < 4 || d[0] != 0xff || d[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (d[1] >> 3) & 3;
    let layer = (d[1] >> 1) & 3;
    let kbps_ix = (d[2] >> 4) as usize;
    let rate_ix = ((d[2] >> 2) & 3) as usize;
    let pad = ((d[2] >> 1) & 1) as usize;
    if version == 1 || layer == 0 || kbps_ix == 0 || kbps_ix == 15 || rate_ix == 3 {
        return None;
    }
    let mpeg1 = version == 3;
    let row = match (mpeg1, layer) {
        (true, 3) => 0,
        (true, 2) => 1,
        (true, 1) => 2,
        (false, 3) => 3,
        (false, _) => 4,
        _ => return None,
    };
    let base = [44_100usize, 48_000, 32_000][rate_ix];
    let rate = match version {
        3 => base,
        2 => base / 2,
        _ => base / 4,
    };
    let kbps = MPA_KBPS[row][kbps_ix] as usize * 1000;
    Some(match layer {
        3 => (12 * kbps / rate + pad) * 4,
        2 => 144 * kbps / rate + pad,
        _ if mpeg1 => 144 * kbps / rate + pad,
        _ => 72 * kbps / rate + pad,
    })
}

/// Whole codec frames out of chunks that slice a stream by bytes.
#[derive(Default)]
pub struct Framer {
    carry: Vec<u8>,
    /// The last push's frames as `(at, len)` in `carry`, the next to hand out and where the
    /// last ends.
    frames: Vec<(usize, usize)>,
    next: usize,
    used: usize,
}

impl Framer {
    /// Cuts the carry and `bytes` into whole frames, read with [`frame`](Self::frame), and
    /// returns the bytes dropped as no part of a frame. Frames of the last push not taken go.
    pub fn push(&mut self, family: &str, bytes: &[u8]) -> usize {
        // A frame may join slices of several chunks, the tail waits in the carry.
        self.carry.drain(..self.used);
        self.frames.clear();
        self.next = 0;
        self.carry.extend_from_slice(bytes);
        let mut dropped = 0;
        let mut at = 0;
        loop {
            let rest = &self.carry[at..];
            let Some(len) = frame_len(family, rest).filter(|&n| n > 0) else {
                if rest.len() < 6 {
                    break;
                }
                at += 1;
                dropped += 1;
                continue;
            };
            if rest.len() < len {
                break;
            }
            if rest.len() >= len + 6 && frame_len(family, &rest[len..]).is_none() {
                at += 1;
                dropped += 1;
                continue;
            }
            self.frames.push((at, len));
            at += len;
        }
        self.used = at;
        dropped
    }

    /// The next whole frame of the last push.
    pub fn frame(&self) -> Option<&[u8]> {
        let &(at, len) = self.frames.get(self.next)?;
        self.carry.get(at..at + len)
    }

    /// Steps past the frame [`frame`](Self::frame) gave.
    pub fn take(&mut self) {
        self.next = (self.next + 1).min(self.frames.len());
    }

    pub fn clear(&mut self) {
        self.carry.clear();
        self.frames.clear();
        (self.next, self.used) = (0, 0);
    }
}

/// Whether a downstream decoder can handle the family.
pub fn is_decodable(family: &str) -> bool {
    family != FAMILY_BYTES
}

pub fn is_video_family(family: &str) -> bool {
    matches!(family, "mpeg4/asp" | "h264/annexb" | FAMILY_VP6 | FAMILY_VP7 | FAMILY_FLV1)
}

/// The per-track offer menu, the stream's [`family_for`] family plus the `bytes` escape.
pub fn offers_for(stream: &Stream) -> &'static [OfferDesc] {
    offers_of(family_for(stream))
}

fn offers_of(family: &str) -> &'static [OfferDesc] {
    static MPEG4: [OfferDesc; 2] = [OfferDesc::any("mpeg4/asp"), OfferDesc::any("bytes")];
    static H264: [OfferDesc; 2] = [OfferDesc::any("h264/annexb"), OfferDesc::any("bytes")];
    static VP6: [OfferDesc; 2] = [OfferDesc::any(FAMILY_VP6), OfferDesc::any("bytes")];
    static VP7: [OfferDesc; 2] = [OfferDesc::any(FAMILY_VP7), OfferDesc::any("bytes")];
    static FLV1: [OfferDesc; 2] = [OfferDesc::any(FAMILY_FLV1), OfferDesc::any("bytes")];
    static AC3: [OfferDesc; 2] = [OfferDesc::any("ac3"), OfferDesc::any("bytes")];
    static DTS: [OfferDesc; 2] = [OfferDesc::any("dts"), OfferDesc::any("bytes")];
    static MP3: [OfferDesc; 2] = [OfferDesc::any("mp3"), OfferDesc::any("bytes")];
    static RAW: [OfferDesc; 2] = [OfferDesc::any("audio/raw"), OfferDesc::any("bytes")];
    static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
    match family {
        FAMILY_MPEG4_ASP => &MPEG4,
        FAMILY_H264_ANNEXB => &H264,
        FAMILY_VP6 => &VP6,
        FAMILY_VP7 => &VP7,
        FAMILY_FLV1 => &FLV1,
        FAMILY_AC3 => &AC3,
        FAMILY_DTS => &DTS,
        FAMILY_MP3 => &MP3,
        FAMILY_AUDIO_RAW => &RAW,
        _ => &BYTES,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    #[test]
    fn emits_names_every_family_a_stream_is_announced_as() {
        let named = |f: &str| f == super::FAMILY_BYTES || super::EMITS.contains(&f);
        for tag in 0..=u16::MAX {
            for bits in [8, 16, 24, 32, 64] {
                assert!(named(super::audio_family(tag, bits)), "{tag:#x}");
            }
        }
        let fixed = [*b"VP60", *b"VP71", *b"FLV1", *b"S263", *b"????"];
        let tags = super::MPEG4_TAGS.iter().chain(&super::H264_TAGS).chain(&fixed);
        for cc in tags {
            let family = super::video_family(*cc, &[]);
            assert!(named(family), "{family}");
        }
        for family in super::EMITS {
            assert_eq!(super::offers_of(family)[0].family, *family);
        }
    }

    fn ac3(kbps: u32, fill: u8) -> Vec<u8> {
        let code = super::AC3_KBPS.iter().position(|&k| k == kbps).unwrap() as u8 * 2;
        let mut f = vec![0x0b, 0x77, 0, 0, code, 8 << 3];
        f.resize(kbps as usize * 4, fill);
        f
    }

    #[test]
    fn frame_lengths_follow_the_headers() {
        assert_eq!(super::frame_len("ac3", &ac3(448, 0)), Some(1792));
        assert_eq!(super::frame_len("ac3", &ac3(640, 0)), Some(2560));
        assert_eq!(super::frame_len("ac3", &[0x0b, 0x77, 0, 0, 0x40, 8 << 3]), Some(138));
        assert_eq!(super::frame_len("ac3", &[0x0b, 0x77, 0, 0, 0x41, 8 << 3]), Some(140));
        assert_eq!(super::frame_len("ac3", &[0x0b, 0x77, 0x01, 0xfe, 0, 16 << 3]), Some(1022));
        assert_eq!(
            super::frame_len("ac3", &[0x0b, 0x77, 0, 0, 38, 8 << 3]),
            None,
            "frmsizecod past 37"
        );
        assert_eq!(super::frame_len("ac3", &[0, 0, 0, 0, 0, 0]), None);
        assert_eq!(super::frame_len("mp3", &[0xff, 0xfb, 0x90, 0]), Some(417));
        assert_eq!(super::frame_len("mp3", &[0xff, 0xfb, 0x92, 0]), Some(418));
        assert_eq!(super::frame_len("mp3", &[0xff, 0xf3, 0x80, 0]), Some(208));
        assert_eq!(super::frame_len("mp3", &[0xff, 0xfd, 0xa4, 0]), Some(576), "layer II");
        assert_eq!(super::frame_len("mp3", &[0xff, 0xff, 0xc4, 0]), Some(384), "layer I");
        assert_eq!(super::frame_len("mp3", &[0xff, 0xff, 0xc6, 0]), Some(388), "layer I padded");
        assert_eq!(super::frame_len("mp3", &[0xff, 0xfb, 0xf0, 0]), None, "bad bit rate index");
        assert_eq!(super::frame_len("dts", &ac3(448, 0)), None);
    }

    #[test]
    fn a_framer_cuts_byte_slices_into_frames() {
        fn push(fr: &mut super::Framer, bytes: &[u8], out: &mut Vec<Vec<u8>>) -> usize {
            let dropped = fr.push("ac3", bytes);
            while let Some(f) = fr.frame() {
                out.push(f.to_vec());
                fr.take();
            }
            dropped
        }
        let frames: Vec<Vec<u8>> = (0..5u8).map(|i| ac3(448, i)).collect();
        let mut stream = vec![9u8; 100];
        for f in &frames {
            stream.extend_from_slice(f);
        }
        let mut fr = super::Framer::default();
        let mut out = Vec::new();
        let mut dropped = 0;
        for slice in stream.chunks(2333) {
            dropped += push(&mut fr, slice, &mut out);
        }
        assert_eq!(dropped, 100, "the bytes before the first header");
        assert_eq!(out, frames, "every frame whole, the last with nothing behind it");
        let mut stray = ac3(448, 1);
        stray[700..702].copy_from_slice(&[0x0b, 0x77]);
        stray[704] = 30;
        stray[705] = 8 << 3;
        out.clear();
        assert_eq!(push(&mut fr, &[stray.clone(), ac3(448, 2)].concat(), &mut out), 0);
        assert_eq!(out, [stray, ac3(448, 2)]);
        fr.clear();
        assert_eq!(
            push(&mut fr, &ac3(448, 9)[6..], &mut out),
            1781,
            "a seek drops the carry and what is no frame, five bytes may still head one"
        );
        assert!(super::reframes("ac3", 1));
        assert!(!super::reframes("ac3", 0));
        assert!(!super::reframes("dts", 1));
    }

    use super::*;
    use crate::riff::{FourCc, Stream, StreamKind};

    fn video(handler: &[u8; 4], bicompression: Option<&[u8; 4]>) -> Stream {
        let mut strf = vec![0u8; 40];
        if let Some(cc) = bicompression {
            strf[16..20].copy_from_slice(cc);
        }
        Stream {
            index: 0,
            kind: StreamKind::Video,
            handler: *handler,
            scale: 1,
            rate: 24,
            sample_size: 0,
            length: 0,
            width: 1280,
            height: 544,
            strf,
            format_tag: 0,
            channels: 0,
            samples_per_sec: 0,
            bits_per_sample: 0,
            avg_bytes_per_sec: 0,
            name: String::new(),
        }
    }

    fn audio(tag: u16) -> Stream {
        audio_bits(tag, 0)
    }

    fn audio_bits(tag: u16, bits: u16) -> Stream {
        Stream {
            index: 1,
            kind: StreamKind::Audio,
            handler: [0; 4],
            scale: 1,
            rate: 48_000,
            sample_size: 0,
            length: 0,
            width: 0,
            height: 0,
            strf: Vec::new(),
            format_tag: tag,
            channels: 6,
            samples_per_sec: 48_000,
            bits_per_sample: bits,
            avg_bytes_per_sec: 0,
            name: String::new(),
        }
    }

    #[test]
    fn xvid_variants_map_to_mpeg4_asp() {
        let ccs = [b"XVID", b"xvid", b"DX50", b"DIVX", b"FMP4", b"MP4V", b"MP4S", b"M4S2", b"3IV2"];
        for cc in ccs.into_iter().chain([b"DIV1", b"divx", b"mp4v", b"DX40", &[4, 0, 0, 0]]) {
            let cc: &[u8; 4] = cc;
            assert_eq!(family_for(&video(cc, None)), "mpeg4/asp", "{cc:?}");
        }
    }

    #[test]
    fn ms_mpeg4_v3_is_no_mpeg4_part_2() {
        for cc in [b"MP43", b"DIV3", b"div3", b"MPG3", b"DIV4", b"MP42", b"MPG4"] {
            let cc: &[u8; 4] = cc;
            assert_eq!(family_for(&video(cc, None)), "bytes", "{cc:?}");
        }
        let divx3 = video(b"divx", Some(b"DIV3"));
        assert_eq!(family_for(&divx3), "bytes", "biCompression names the codec, not the handler");
        assert_eq!(family_for(&video(b"divx", Some(b"DX50"))), "mpeg4/asp");
    }

    #[test]
    fn on2_and_sorenson_fourccs_map_to_their_families() {
        for (cc, family) in [
            (b"VP60", "vp6"),
            (b"vp62", "vp6"),
            (b"VP70", "vp7"),
            (b"VP71", "vp7"),
            (b"FLV1", "flv1"),
            (b"S263", "flv1"),
            (b"VP6F", "bytes"),
            (b"VP6A", "bytes"),
        ] {
            let cc: &[u8; 4] = cc;
            assert_eq!(family_for(&video(cc, None)), family, "{cc:?}");
        }
        assert!(is_video_family("vp6") && is_video_family("vp7") && is_video_family("flv1"));
    }

    #[test]
    fn bicompression_fallback_classifies_video() {
        let s = video(&[0, 0, 0, 0], Some(b"XVID"));
        assert_eq!(family_for(&s), "mpeg4/asp");
    }

    #[test]
    fn h264_fourccs_map_to_annexb() {
        assert_eq!(family_for(&video(b"H264", None)), "h264/annexb");
        assert_eq!(family_for(&video(b"avc1", None)), "h264/annexb");
        assert_eq!(family_for(&video(b"x264", None)), "h264/annexb");
        assert_eq!(family_for(&video(b"MJPG", Some(b"VSSH"))), "h264/annexb");
    }

    #[test]
    fn unknown_video_fourcc_is_bytes() {
        assert_eq!(family_for(&video(b"MJPG", None)), "bytes");
    }

    #[test]
    fn the_vol_prefix_runs_through_the_vol_up_to_the_vop() {
        let vos = [0, 0, 1, 0xB0, 0xF5, 0, 0, 1, 0xB5, 0x09];
        let vol = [0, 0, 1, 0x20, 0x08, 0x80];
        let vop = [0, 0, 1, 0xB6, 0x10, 0];
        let first = [vos.as_slice(), &vol, &vop].concat();
        assert_eq!(vol_prefix(&first), Some(&first[..16]));
        assert_eq!(vol_prefix(&[vol.as_slice(), &vop].concat()), Some(&vol[..]));
        assert_eq!(vol_prefix(&vol), Some(&vol[..]), "a VOL alone");
        assert_eq!(vol_prefix(&vop), None, "a VOP without its VOL");
        assert_eq!(vol_prefix(&[vos.as_slice(), &vop].concat()), None, "no VOL between");
        assert_eq!(vol_prefix(&[]), None);
        assert_eq!(vol_prefix(&[0, 0, 1]), None, "a cut start code");
    }

    #[test]
    fn a_key_chunk_is_an_i_vop_or_an_idr() {
        let vol = [0, 0, 1, 0x20, 0x08, 0x80];
        let vop = |t: u8| [0, 0, 1, 0xB6, t << 6 | 0x10, 0];
        assert!(chunk_is_key("mpeg4/asp", &[vol.as_slice(), &vop(0)].concat()));
        assert!(!chunk_is_key("mpeg4/asp", &[vol.as_slice(), &vop(1)].concat()));
        assert!(!chunk_is_key("mpeg4/asp", &vop(2)));
        assert!(!chunk_is_key("mpeg4/asp", &[0x7f]), "a packed placeholder");
        assert!(!chunk_is_key("mpeg4/asp", &[]));
        assert!(chunk_is_key("h264/annexb", &[0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x65, 0x88]));
        assert!(!chunk_is_key("h264/annexb", &[0, 0, 0, 1, 0x41, 0x9a]));
        assert!(!chunk_is_key("bytes", &vop(0)));
    }

    #[test]
    fn audio_tags_map_to_families() {
        assert_eq!(family_for(&audio(0x2000)), "ac3", "AC-3");
        assert_eq!(family_for(&audio(0x2001)), "dts", "DTS is its own family");
        assert_eq!(family_for(&audio(0x0055)), "mp3");
        assert_eq!(family_for(&audio(0x0050)), "mp3", "MPEG layer I and II");
        assert_eq!(family_for(&audio(0x0001)), "audio/raw");
        assert_eq!(family_for(&audio_bits(0x0003, 32)), "audio/raw", "IEEE float");
        assert_eq!(family_for(&audio_bits(0x0003, 64)), "bytes", "f64 is not narrowed");
        assert_eq!(raw_sample(0x0003, 32), "f32");
        assert_eq!(raw_sample(0x0001, 8), "u8");
        assert_eq!(raw_sample(0x0001, 16), "s16");
        assert_eq!(raw_sample(0x0001, 24), "s24");
        assert_eq!(raw_sample(0x0001, 32), "s32");
        assert_eq!(family_for(&audio(0x00FF)), "bytes", "unknown tag → bytes");
    }

    #[test]
    fn offer_menus_carry_family_plus_bytes() {
        let menu = offers_for(&audio(0x2000));
        assert_eq!(menu.len(), 2);
        assert_eq!(menu[0].family, "ac3");
        assert_eq!(menu[1].family, "bytes");
        let bmenu = offers_for(&audio(0xBEEF));
        assert_eq!(bmenu.len(), 1);
        assert_eq!(bmenu[0].family, "bytes");
    }

    #[test]
    fn decodability_and_video_predicate() {
        assert!(is_decodable("mpeg4/asp"));
        assert!(!is_decodable("bytes"));
        assert!(is_video_family("mpeg4/asp"));
        assert!(is_video_family("h264/annexb"));
        assert!(!is_video_family("ac3"));
    }

    #[allow(dead_code)]
    fn _touch(_: FourCc) {}
}
