//! Audio and video tag headers (Annex E.4.2, E.4.3, Enhanced RTMP v1 and v2), the codecs they
//! name and the families the decoders take. Bodies are untrusted, `None` for one that is short.

use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc};

use crate::tag::{ENHANCED, be24};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Video {
    Jpeg,
    Sorenson,
    Screen,
    Vp6,
    Vp6Alpha,
    Screen2,
    H264,
    Hevc,
    Av1,
    Vp9,
    Vp8,
    Vvc,
    Unknown,
}

impl Video {
    /// The 4 bit CodecID, 12 for HEVC as FFmpeg and the Chinese CDNs write it.
    pub fn from_id(id: u8) -> Video {
        match id {
            1 => Video::Jpeg,
            2 => Video::Sorenson,
            3 => Video::Screen,
            4 => Video::Vp6,
            5 => Video::Vp6Alpha,
            6 => Video::Screen2,
            7 => Video::H264,
            12 => Video::Hevc,
            _ => Video::Unknown,
        }
    }

    pub fn from_fourcc(cc: &[u8]) -> Video {
        match cc {
            b"avc1" => Video::H264,
            b"hvc1" => Video::Hevc,
            b"av01" => Video::Av1,
            b"vp09" => Video::Vp9,
            b"vp08" => Video::Vp8,
            b"vvc1" => Video::Vvc,
            _ => Video::Unknown,
        }
    }

    /// `videocodecid`, a CodecID or, in Enhanced RTMP, the FourCC as a number.
    pub fn from_meta(v: f64) -> Video {
        match v {
            v if !(0.0..=f64::from(u32::MAX)).contains(&v) => Video::Unknown,
            v if v < 16.0 => Video::from_id(v as u8),
            v => Video::from_fourcc(&(v as u32).to_be_bytes()),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Video::Jpeg => "JPEG",
            Video::Sorenson => "Sorenson H.263",
            Video::Screen => "Screen video",
            Video::Vp6 => "On2 VP6",
            Video::Vp6Alpha => "On2 VP6 with alpha",
            Video::Screen2 => "Screen video v2",
            Video::H264 => "H.264",
            Video::Hevc => "HEVC",
            Video::Av1 => "AV1",
            Video::Vp9 => "VP9",
            Video::Vp8 => "VP8",
            Video::Vvc => "VVC",
            Video::Unknown => "unknown video",
        }
    }

    /// `None` for a codec nothing here decodes.
    pub fn family(self) -> Option<&'static str> {
        match self {
            Video::H264 => Some("h264/annexb"),
            Video::Hevc => Some("h265/annexb"),
            Video::Av1 => Some("av1"),
            Video::Vp9 => Some("vp9"),
            Video::Vp8 => Some("vp8"),
            Video::Sorenson => Some("flv1"),
            Video::Vp6 => Some("vp6f"),
            Video::Vp6Alpha => Some("vp6a"),
            _ => None,
        }
    }

    /// Length-prefixed NAL units behind a decoder configuration record.
    pub fn nal(self) -> bool {
        matches!(self, Video::H264 | Video::Hevc)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audio {
    Pcm,
    Adpcm,
    Mp3,
    PcmLe,
    Nelly16k,
    Nelly8k,
    Nelly,
    Alaw,
    Mulaw,
    Enhanced,
    Aac,
    Speex,
    Mp3At8k,
    Device,
    Reserved,
}

impl Audio {
    pub fn from_format(f: u8) -> Audio {
        match f {
            0 => Audio::Pcm,
            1 => Audio::Adpcm,
            2 => Audio::Mp3,
            3 => Audio::PcmLe,
            4 => Audio::Nelly16k,
            5 => Audio::Nelly8k,
            6 => Audio::Nelly,
            7 => Audio::Alaw,
            8 => Audio::Mulaw,
            9 => Audio::Enhanced,
            10 => Audio::Aac,
            11 => Audio::Speex,
            14 => Audio::Mp3At8k,
            15 => Audio::Device,
            _ => Audio::Reserved,
        }
    }

    pub fn from_meta(v: f64) -> Audio {
        match v {
            v if (0.0..16.0).contains(&v) => Audio::from_format(v as u8),
            _ => Audio::Reserved,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Audio::Pcm | Audio::PcmLe => "linear PCM",
            Audio::Adpcm => "ADPCM",
            Audio::Mp3 | Audio::Mp3At8k => "MP3",
            Audio::Nelly16k | Audio::Nelly8k | Audio::Nelly => "Nellymoser",
            Audio::Alaw => "G.711 A-law",
            Audio::Mulaw => "G.711 mu-law",
            Audio::Enhanced => "Enhanced RTMP audio",
            Audio::Aac => "AAC",
            Audio::Speex => "Speex",
            Audio::Device => "device-specific sound",
            Audio::Reserved => "a reserved sound format",
        }
    }

    pub fn family(self) -> Option<&'static str> {
        match self {
            Audio::Pcm | Audio::PcmLe => Some("audio/raw"),
            Audio::Mp3 | Audio::Mp3At8k => Some("mp3"),
            Audio::Aac => Some("aac"),
            _ => None,
        }
    }
}

/// The offer menu of a family, the one Matroska puts its tracks of that codec on.
pub fn offers(family: &str) -> &'static [OfferDesc] {
    static SIZE: [FieldDesc; 2] = [
        FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    ];
    const fn video(family: &'static str) -> [OfferDesc; 2] {
        [OfferDesc { family, fields: &SIZE }, OfferDesc::any("bytes")]
    }
    static FLV1: [OfferDesc; 2] = video("flv1");
    static VP6F: [OfferDesc; 2] = video("vp6f");
    static VP6A: [OfferDesc; 2] = video("vp6a");
    match family {
        "flv1" => return &FLV1,
        "vp6f" => return &VP6F,
        "vp6a" => return &VP6A,
        _ => {}
    }
    pf_mkv::codec::offers_for(match family {
        "h264/annexb" => "V_MPEG4/ISO/AVC",
        "h265/annexb" => "V_MPEGH/ISO/HEVC",
        "av1" => "V_AV1",
        "vp9" => "V_VP9",
        "vp8" => "V_VP8",
        "aac" => "A_AAC",
        "mp3" => "A_MPEG/L3",
        "audio/raw" => "A_PCM/INT/LIT",
        _ => "",
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet {
    Config,
    Frame,
    End,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTag {
    pub codec: Video,
    pub enhanced: bool,
    pub frame: u8,
    pub packet: Packet,
    /// CompositionTime, ms, PTS less DTS.
    pub cts: i32,
    /// A ModEx TimestampOffsetNano.
    pub nanos: u32,
    pub from: usize,
    pub to: usize,
}

impl VideoTag {
    pub fn key(&self) -> bool {
        matches!(self.frame, 1 | 4)
    }
}

const MOD_EX: u8 = 7;
const MULTITRACK: u8 = 6;
const ONE_TRACK: u8 = 0x00;
const MANY_CODECS: u8 = 0x20;
const INFO_FRAME: u8 = 5;

fn si24(b: &[u8]) -> i32 {
    ((be24(b) << 8) as i32) >> 8
}

pub fn video_tag(body: &[u8]) -> Option<VideoTag> {
    let b0 = *body.first()?;
    if b0 & ENHANCED != 0 {
        return enhanced(body);
    }
    let codec = Video::from_id(b0 & 0x0F);
    let frame = b0 >> 4;
    let mut t = VideoTag {
        codec,
        enhanced: false,
        frame,
        packet: Packet::Frame,
        cts: 0,
        nanos: 0,
        from: 1,
        to: body.len(),
    };
    if frame == INFO_FRAME {
        t.packet = Packet::Skip;
    } else if codec.nal() {
        let h = body.get(1..5)?;
        t.packet = match h[0] {
            0 => Packet::Config,
            1 => Packet::Frame,
            2 => Packet::End,
            _ => Packet::Skip,
        };
        (t.cts, t.from) = (si24(&h[1..4]), 5);
    }
    Some(t)
}

/// ExVideoTagHeader. Of a multitrack tag only track 0 is read.
fn enhanced(body: &[u8]) -> Option<VideoTag> {
    let frame = body[0] >> 4 & 7;
    let (mut pkt, mut at, mut nanos) = (body[0] & 0x0F, 1usize, 0u32);
    let mut mods = 0;
    while pkt == MOD_EX {
        mods += 1;
        let mut size = usize::from(*body.get(at)?) + 1;
        at += 1;
        if size == 256 {
            let b = body.get(at..at + 2)?;
            (size, at) = (usize::from(u16::from_be_bytes([b[0], b[1]])) + 1, at + 2);
        }
        let data = body.get(at..at + size)?;
        let kind = *body.get(at + size)?;
        at += size + 1;
        // TimestampOffsetNano, the only ModEx type there is.
        if kind & 0xF0 == 0 && size == 3 {
            nanos = be24(data);
        }
        pkt = kind & 0x0F;
        if mods > 4 {
            return None;
        }
    }
    let skip = VideoTag {
        codec: Video::Unknown,
        enhanced: true,
        frame,
        packet: Packet::Skip,
        cts: 0,
        nanos,
        from: body.len(),
        to: body.len(),
    };
    if frame == INFO_FRAME {
        return Some(skip);
    }
    let multi = match pkt {
        MULTITRACK => {
            let t = *body.get(at)?;
            (pkt, at) = (t & 0x0F, at + 1);
            Some(t & 0xF0)
        }
        _ => None,
    };
    let mut codec = Video::from_fourcc(body.get(at..at + 4)?);
    at += 4;
    let (from, to) = match multi {
        None => (at, body.len()),
        Some(kind) => {
            let mut first = true;
            loop {
                if !first && kind == MANY_CODECS {
                    codec = Video::from_fourcc(body.get(at..at + 4)?);
                    at += 4;
                }
                first = false;
                let id = *body.get(at)?;
                at += 1;
                let len = match kind {
                    ONE_TRACK => body.len() - at,
                    _ => {
                        let n = be24(body.get(at..at + 3)?) as usize;
                        at += 3;
                        n
                    }
                };
                let end = at.checked_add(len).filter(|e| *e <= body.len())?;
                if id == 0 {
                    break (at, end);
                }
                if kind == ONE_TRACK || end >= body.len() {
                    return Some(VideoTag { codec, ..skip });
                }
                at = end;
            }
        }
    };
    let mut t = VideoTag { codec, from, to, ..skip };
    t.packet = match pkt {
        0 => Packet::Config,
        1 | 3 => Packet::Frame,
        2 => Packet::End,
        _ => Packet::Skip,
    };
    // Only CodedFrames of the NAL codecs carry a CompositionTime.
    if pkt == 1 && codec.nal() {
        let h = body.get(from..from + 3).filter(|_| from + 3 <= to)?;
        (t.cts, t.from) = (si24(h), from + 3);
    }
    Some(t)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioTag {
    pub codec: Audio,
    pub flags: u8,
    pub packet: Packet,
    pub from: usize,
}

pub fn audio_tag(body: &[u8]) -> Option<AudioTag> {
    let flags = *body.first()?;
    let codec = Audio::from_format(flags >> 4);
    let (packet, from) = match codec {
        Audio::Aac => match *body.get(1)? {
            0 => (Packet::Config, 2),
            1 => (Packet::Frame, 2),
            _ => (Packet::Skip, 2),
        },
        Audio::Enhanced => (Packet::Skip, body.len()),
        _ => (Packet::Frame, 1),
    };
    Some(AudioTag { codec, flags, packet, from })
}

/// Rate, channels and sample name of linear PCM from the tag flags. 8 bit samples are unsigned.
pub fn pcm_form(flags: u8) -> (u32, u32, &'static str) {
    let rate = [5_512, 11_025, 22_050, 44_100][usize::from(flags >> 2 & 3)];
    let sample = if flags & 2 != 0 { "s16" } else { "u8" };
    (rate, 1 + u32::from(flags & 1), sample)
}

const AAC_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Rate and channels of an AudioSpecificConfig (ISO/IEC 14496-3 1.6.2.1), 0 channels for a PCE.
pub fn aac_form(asc: &[u8]) -> Option<(u32, u32)> {
    let mut word = [0u8; 8];
    let n = asc.len().min(8);
    word[..n].copy_from_slice(&asc[..n]);
    let word = u64::from_be_bytes(word);
    let mut at = 0u32;
    let mut take = |bits: u32| {
        at += bits;
        (word >> (64 - at) & ((1 << bits) - 1)) as u32
    };
    let mut need = 13;
    if take(5) == 31 {
        take(6);
        need += 6;
    }
    let rate = match take(4) {
        15 => {
            need += 24;
            take(24)
        }
        i => *AAC_RATES.get(i as usize)?,
    };
    let channels = take(4);
    (asc.len() * 8 >= need && rate > 0).then_some((rate, channels))
}

/// Rate and channels of an MPEG audio frame header (ISO/IEC 11172-3 2.4.2.3, 13818-3).
pub fn mp3_form(frame: &[u8]) -> Option<(u32, u32)> {
    let h = frame.get(..4)?;
    let version = h[1] >> 3 & 3;
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 || version == 1 || h[1] >> 1 & 3 == 0 {
        return None;
    }
    let base = *[44_100, 48_000, 32_000].get(usize::from(h[2] >> 2 & 3))?;
    let rate = match version {
        3 => base,
        2 => base / 2,
        _ => base / 4,
    };
    Some((rate, if h[3] >> 6 == 3 { 1 } else { 2 }))
}

/// Whether every NAL unit of `b` has a 4 octet length, is not empty and ends inside.
pub fn nal4_whole(b: &[u8]) -> bool {
    let mut at = 0usize;
    while at < b.len() {
        let Some(n) = b.get(at..at + 4).map(|l| u32::from_be_bytes([l[0], l[1], l[2], l[3]]))
        else {
            return false;
        };
        match (at + 4).checked_add(n as usize) {
            Some(end) if n > 0 && end <= b.len() => at = end,
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_avc_carries_its_composition_time() {
        let t = video_tag(&[0x27, 0x01, 0xFF, 0xFF, 0xBE, 9, 9]).unwrap();
        assert_eq!((t.codec, t.frame, t.packet, t.cts), (Video::H264, 2, Packet::Frame, -66));
        assert_eq!((t.from, t.to, t.key()), (5, 7, false));
        let c = video_tag(&[0x17, 0x00, 0, 0, 0, 1]).unwrap();
        assert_eq!((c.packet, c.key()), (Packet::Config, true));
        assert_eq!(video_tag(&[0x17, 0x01, 0]), None, "short");
        let s = video_tag(&[0x22, 0, 0, 0x84]).unwrap();
        assert_eq!((s.codec, s.packet, s.from), (Video::Sorenson, Packet::Frame, 1));
        assert_eq!(video_tag(&[0x57, 1]).unwrap().packet, Packet::Skip, "an info frame");
    }

    #[test]
    fn enhanced_headers_name_their_fourcc() {
        let seq = video_tag(b"\x90hvc1\x01\x02").unwrap();
        assert_eq!(
            (seq.codec, seq.enhanced, seq.packet, seq.from),
            (Video::Hevc, true, Packet::Config, 5)
        );
        let coded = video_tag(b"\x91hvc1\x00\x00\x21\xAA").unwrap();
        assert_eq!(
            (coded.packet, coded.cts, coded.from, coded.key()),
            (Packet::Frame, 33, 8, true)
        );
        let x = video_tag(b"\xA3hvc1\xAA").unwrap();
        assert_eq!((x.packet, x.cts, x.from, x.key()), (Packet::Frame, 0, 5, false));
        let av1 = video_tag(b"\x91av01\x12\x00").unwrap();
        assert_eq!((av1.codec, av1.cts, av1.from), (Video::Av1, 0, 5), "no CompositionTime");
        assert_eq!(video_tag(b"\x92vp09").unwrap().packet, Packet::End);
        assert_eq!(video_tag(b"\xD4hvc1\x02").unwrap().packet, Packet::Skip, "colour info");
        assert_eq!(video_tag(b"\x91hvc1\x00"), None, "short");
    }

    #[test]
    fn mod_ex_and_multitrack_are_read_to_track_zero() {
        let modex = video_tag(b"\x97\x02\x00\x00\x07\x03av01\xAA").unwrap();
        assert_eq!(
            (modex.nanos, modex.packet, modex.codec, modex.from),
            (7, Packet::Frame, Video::Av1, 10)
        );
        let one = video_tag(b"\x96\x03av01\x00\xAA\xBB").unwrap();
        assert_eq!((one.codec, one.packet, one.from, one.to), (Video::Av1, Packet::Frame, 7, 9));
        let many = video_tag(b"\x96\x13vp09\x01\x00\x00\x01\xEE\x00\x00\x00\x02\xAA\xBB").unwrap();
        assert_eq!((many.codec, many.from, many.to), (Video::Vp9, 15, 17));
        let codecs =
            video_tag(b"\x96\x23vp09\x01\x00\x00\x01\xEEav01\x00\x00\x00\x01\xAA").unwrap();
        assert_eq!((codecs.codec, codecs.from, codecs.to), (Video::Av1, 19, 20));
        let none = video_tag(b"\x96\x13vp09\x01\x00\x00\x01\xEE").unwrap();
        assert_eq!(none.packet, Packet::Skip, "no track 0");
        assert_eq!(video_tag(b"\x96\x13vp09\x00\x00\x00\x09\xEE"), None, "a track past the tag");
    }

    #[test]
    fn audio_forms() {
        let a = audio_tag(&[0xAF, 0x00, 0x12, 0x10]).unwrap();
        assert_eq!((a.codec, a.packet, a.from), (Audio::Aac, Packet::Config, 2));
        assert_eq!(aac_form(&[0x12, 0x10]), Some((44_100, 2)));
        assert_eq!(aac_form(&[0x11, 0x90]), Some((48_000, 2)));
        assert_eq!(aac_form(&[0x12]), None);
        assert_eq!(
            aac_form(&[0x17, 0x80, 0x5D, 0xC0, 0x10]),
            Some((48_000, 2)),
            "an explicit rate"
        );
        assert_eq!(aac_form(&[0xF8, 0x06, 0x20]), Some((48_000, 1)), "an escaped object type");
        assert_eq!(mp3_form(&[0xFF, 0xFB, 0x90, 0x44]), Some((44_100, 2)));
        assert_eq!(mp3_form(&[0xFF, 0xF3, 0x84, 0xC4]), Some((24_000, 1)));
        assert_eq!(mp3_form(&[0xFF, 0xFB, 0x9C, 0x44]), None, "a reserved rate");
        assert_eq!(pcm_form(0x3F), (44_100, 2, "s16"));
        assert_eq!(pcm_form(0x00), (5_512, 1, "u8"));
        assert_eq!(audio_tag(&[0x9F, 1, 2]).unwrap().packet, Packet::Skip);
    }

    #[test]
    fn codec_ids_from_metadata() {
        assert_eq!(Video::from_meta(7.0), Video::H264);
        assert_eq!(Video::from_meta(f64::from(u32::from_be_bytes(*b"hvc1"))), Video::Hevc);
        assert_eq!(Video::from_meta(-1.0), Video::Unknown);
        assert_eq!(Video::from_meta(f64::NAN), Video::Unknown);
        assert_eq!(Audio::from_meta(10.0), Audio::Aac);
        assert_eq!(Audio::from_meta(1e9), Audio::Reserved);
        assert!(Video::Screen.family().is_none() && Audio::Speex.family().is_none());
    }

    #[test]
    fn nal_lengths_are_checked_whole() {
        assert!(nal4_whole(&[0, 0, 0, 1, 0x65, 0, 0, 0, 2, 1, 2]));
        assert!(!nal4_whole(&[0, 0, 0, 0]), "empty");
        assert!(!nal4_whole(&[0, 0, 0, 3, 1]), "past the end");
        assert!(!nal4_whole(&[0, 0, 1]), "a short length");
        assert!(nal4_whole(&[]));
    }
}
