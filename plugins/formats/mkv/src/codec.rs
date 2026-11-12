//! CodecID to family mapping and the length-prefixed to Annex B NAL reframing (RFC 9559 section 12,
//! ISO/IEC 14496-15). CodecPrivate and Block bytes are untrusted, errors never panic.

use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc};

use crate::reader::Track;

/// The announce family of each CodecID (RFC 9559 section 12), matching the decoders' sink offers.
pub static CODEC_IDS: [(&str, &str); 35] = [
    ("A_FLAC", "flac"),
    ("A_AAC", "aac"),
    ("A_AC3", "ac3"),
    ("A_EAC3", "eac3"),
    ("A_DTS", "dts"),
    ("A_MPEG/L1", "mp3"),
    ("A_MPEG/L2", "mp3"),
    ("A_MPEG/L3", "mp3"),
    ("A_OPUS", "opus"),
    ("A_VORBIS", "vorbis"),
    ("A_ALAC", "alac"),
    ("A_TRUEHD", "truehd"),
    ("A_MLP", "mlp"),
    ("A_PCM/INT/LIT", "audio/raw"),
    ("A_PCM/INT/BIG", "audio/raw"),
    ("A_PCM/FLOAT/IEEE", "audio/raw"),
    ("V_VP8", "vp8"),
    ("V_VP9", "vp9"),
    ("V_AV1", "av1"),
    ("V_MPEG4/ISO/AVC", "h264/annexb"),
    ("V_MPEGH/ISO/HEVC", "h265/annexb"),
    ("V_MPEG4/ISO/SP", "mpeg4/asp"),
    ("V_MPEG4/ISO/ASP", "mpeg4/asp"),
    ("V_MPEG4/ISO/AP", "mpeg4/asp"),
    ("V_MPEG2", "mpeg2"),
    ("V_MS/VFW/FOURCC", "vfw"),
    ("S_TEXT/UTF8", "subtitle/srt"),
    ("S_TEXT/ASS", "subtitle/ass"),
    ("S_TEXT/SSA", "subtitle/ass"),
    ("S_TEXT/WEBVTT", "subtitle/vtt"),
    // WebM's, the descriptions and metadata kinds show nothing.
    ("D_WEBVTT/SUBTITLES", "subtitle/vtt"),
    ("D_WEBVTT/CAPTIONS", "subtitle/vtt"),
    ("S_HDMV/PGS", "subtitle/pgs"),
    ("S_VOBSUB", "subtitle/vobsub"),
    ("S_DVBSUB", "subtitle/dvb"),
];

/// The announce family for a track's CodecID, `bytes` for anything [`CODEC_IDS`] leaves out.
pub fn family_for(codec_id: &str) -> &'static str {
    CODEC_IDS.iter().find(|e| e.0 == codec_id).map_or("bytes", |e| e.1)
}

/// Every family a track can be announced as, from [`CODEC_IDS`], the VFW fourccs and the refusals.
pub static EMITS: &[&str] = &[
    "flac",
    "aac",
    "ac3",
    "eac3",
    "dts",
    "mp3",
    "opus",
    "vorbis",
    "alac",
    "truehd",
    "mlp",
    "audio/raw",
    "pcm",
    "vp8",
    "vp9",
    "av1",
    "h264/annexb",
    "h265/annexb",
    "h264/avcc",
    "h265/hvcc",
    "mpeg4/asp",
    "mpeg2",
    "vfw",
    "vp6",
    "vp7",
    "flv1",
    "subtitle/srt",
    "subtitle/ass",
    "subtitle/vtt",
    "subtitle/pgs",
    "subtitle/vobsub",
    "subtitle/dvb",
    "encrypted",
    "compressed",
];

pub fn is_audio_family(family: &str) -> bool {
    matches!(
        family,
        "flac"
            | "aac"
            | "ac3"
            | "eac3"
            | "dts"
            | "mp3"
            | "opus"
            | "vorbis"
            | "alac"
            | "truehd"
            | "mlp"
            | "audio/raw"
            | "pcm"
    )
}

/// [`family_for`] refined by the track, for a refused encoding, a VFW fourcc or an unnamed PCM
/// depth.
pub fn track_family(track: &Track) -> &'static str {
    if let Some(why) = track.coding.refused {
        return why.family();
    }
    match family_for(&track.codec_id) {
        "vfw" => vfw_family(&track.codec_private),
        "audio/raw" if pcm_sample(track).is_none() => "pcm",
        family => family,
    }
}

pub fn track_offers(track: &Track) -> &'static [OfferDesc] {
    static ENCRYPTED: [OfferDesc; 2] = [OfferDesc::any("encrypted"), OfferDesc::any("bytes")];
    static COMPRESSED: [OfferDesc; 2] = [OfferDesc::any("compressed"), OfferDesc::any("bytes")];
    static PCM: [OfferDesc; 2] = [OfferDesc::any("pcm"), OfferDesc::any("bytes")];
    const fn video(family: &'static str) -> [OfferDesc; 3] {
        [
            OfferDesc::any(family),
            OfferDesc { family, fields: &VIDEO_FIELDS },
            OfferDesc::any("bytes"),
        ]
    }
    // The stored family behind the others. A pad whose consumers name it first announces it, see
    // `takes_stored` in the element, and an announced name some offer has to declare.
    const fn nal(annexb: &'static str, stored: &'static str) -> [OfferDesc; 5] {
        [
            OfferDesc::any(annexb),
            OfferDesc { family: annexb, fields: &VIDEO_FIELDS },
            OfferDesc::any("bytes"),
            OfferDesc::any(stored),
            OfferDesc { family: stored, fields: &VIDEO_FIELDS },
        ]
    }
    static VP6: [OfferDesc; 3] = video("vp6");
    static VP7: [OfferDesc; 3] = video("vp7");
    static FLV1: [OfferDesc; 3] = video("flv1");
    static H264: [OfferDesc; 5] = nal("h264/annexb", "h264/avcc");
    static H265: [OfferDesc; 5] = nal("h265/annexb", "h265/hvcc");
    let stores = |hevc| nal_head_from_config(&track.codec_private, hevc).is_ok();
    match track_family(track) {
        "encrypted" => &ENCRYPTED,
        "compressed" => &COMPRESSED,
        "pcm" => &PCM,
        "mpeg4/asp" => offers_for("V_MPEG4/ISO/ASP"),
        "h264/annexb" if track.codec_id == "V_MPEG4/ISO/AVC" && stores(false) => &H264,
        "h265/annexb" if stores(true) => &H265,
        "h264/annexb" => offers_for("V_MPEG4/ISO/AVC"),
        "vp6" => &VP6,
        "vp7" => &VP7,
        "flv1" => &FLV1,
        _ => offers_for(&track.codec_id),
    }
}

/// The family an Annex B family's units leave as when they leave as stored, length prefixed and
/// led by the track's configuration record.
pub fn stored_family(family: &str) -> Option<&'static str> {
    match family {
        "h264/annexb" => Some("h264/avcc"),
        "h265/annexb" => Some("h265/hvcc"),
        _ => None,
    }
}

/// The family of a VFW track from the BITMAPINFOHEADER `biCompression` fourcc at offset 16.
pub fn vfw_family(bitmapinfoheader: &[u8]) -> &'static str {
    let Some(cc) = bitmapinfoheader.get(16..20) else {
        return "vfw";
    };
    match [cc[0], cc[1], cc[2], cc[3]].map(|b| b.to_ascii_uppercase()) {
        [b'X', b'V', b'I', b'D']
        | [b'D', b'X', b'5', b'0']
        | [b'D', b'I', b'V', b'X']
        | [b'D', b'X', b'4', b'0'] => "mpeg4/asp",
        [b'F', b'M', b'P', b'4'] | [b'M', b'P', b'4', b'V'] => "mpeg4/asp",
        [b'H', b'2', b'6', b'4'] | [b'A', b'V', b'C', b'1'] | [b'X', b'2', b'6', b'4'] => {
            "h264/annexb"
        }
        // FFmpeg `riff.c`. VP6F and VP6A are left out, their FLV framing is not VFW's.
        [b'V', b'P', b'6', b'0' | b'1' | b'2'] => "vp6",
        [b'V', b'P', b'7', b'0' | b'1'] => "vp7",
        [b'F', b'L', b'V', b'1'] | [b'S', b'2', b'6', b'3'] => "flv1",
        _ => "vfw",
    }
}

pub fn vfw_extradata(bitmapinfoheader: &[u8]) -> &[u8] {
    let Some(size) = bitmapinfoheader.get(..4) else {
        return &[];
    };
    let size = u32::from_le_bytes([size[0], size[1], size[2], size[3]]) as usize;
    bitmapinfoheader.get(size.max(40)..).unwrap_or(&[])
}

/// The `audio/raw` sample name of a PCM track. Integer samples are signed but for 8 bits.
/// An integer sample of any depth up to 32 is named by the octets it takes. A depth short of
/// them, 20 in 3, is taken left aligned in them, as a WAV's samples are and mkvmerge copies them.
pub fn pcm_sample(track: &Track) -> Option<&'static str> {
    match (track.codec_id.as_str(), track.bit_depth) {
        ("A_PCM/INT/LIT" | "A_PCM/INT/BIG", 1..=32) => match track.bit_depth.div_ceil(8) {
            1 => Some("u8"),
            2 => Some("s16"),
            3 => Some("s24"),
            _ => Some("s32"),
        },
        ("A_PCM/FLOAT/IEEE", 32 | 64) => Some("f32"),
        _ => None,
    }
}

pub fn pcm_reframer(track: &Track) -> Reframer {
    match (track.codec_id.as_str(), track.bit_depth) {
        ("A_PCM/INT/BIG", 9..=32) => Reframer::Swap { width: track.bit_depth.div_ceil(8) as u8 },
        ("A_PCM/FLOAT/IEEE", 64) => Reframer::Narrow { big: false },
        _ => Reframer::Passthrough,
    }
}

static RAW_SAMPLES: [profluens_core::format::ValueDesc; 5] = [
    profluens_core::format::ValueDesc::Id("u8"),
    profluens_core::format::ValueDesc::Id("s16"),
    profluens_core::format::ValueDesc::Id("s24"),
    profluens_core::format::ValueDesc::Id("s32"),
    profluens_core::format::ValueDesc::Id("f32"),
];
static RAW_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: profluens_audio::FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: profluens_audio::FIELD_CHANNELS,
        allowed: ConstraintDesc::Any,
        preferred: None,
    },
    FieldDesc {
        field: profluens_audio::FIELD_SAMPLE,
        allowed: ConstraintDesc::Set(&RAW_SAMPLES),
        preferred: None,
    },
    FieldDesc { field: FIELD_DELAY, allowed: ConstraintDesc::Any, preferred: None },
];

pub const FIELD_DELAY: &str = profluens_audio::FIELD_DELAY;

static VIDEO_FIELDS: [FieldDesc; 12] = {
    let c = profluens_video::format::COLOR_OFFER_FIELDS;
    [
        FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
        c[0],
        c[1],
        c[2],
        c[3],
        c[4],
        c[5],
        c[6],
        c[7],
        c[8],
        c[9],
    ]
};
// Every name an audio announcement states. A copy links no decoder to name the rate, and an
// announcement with a name no offer declared is dropped whole.
static AUDIO_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: profluens_audio::FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: profluens_audio::FIELD_CHANNELS,
        allowed: ConstraintDesc::Any,
        preferred: None,
    },
    FieldDesc { field: FIELD_DELAY, allowed: ConstraintDesc::Any, preferred: None },
];

/// One menu per track so link-time intersection cannot admit a wrong decoder.
pub fn offers_for(codec_id: &str) -> &'static [OfferDesc] {
    const fn audio(family: &'static str) -> [OfferDesc; 2] {
        [OfferDesc { family, fields: &AUDIO_FIELDS }, OfferDesc::any("bytes")]
    }
    static FLAC: [OfferDesc; 2] = audio("flac");
    static AAC: [OfferDesc; 2] = audio("aac");
    static AC3: [OfferDesc; 2] = audio("ac3");
    static EAC3: [OfferDesc; 2] = audio("eac3");
    static DTS: [OfferDesc; 2] = audio("dts");
    static MP3: [OfferDesc; 2] = audio("mp3");
    static OPUS: [OfferDesc; 2] = audio("opus");
    static VORBIS: [OfferDesc; 2] = audio("vorbis");
    static ALAC: [OfferDesc; 2] = audio("alac");
    static TRUEHD: [OfferDesc; 2] = audio("truehd");
    static MLP: [OfferDesc; 2] = audio("mlp");
    static RAW: [OfferDesc; 2] =
        [OfferDesc { family: "audio/raw", fields: &RAW_FIELDS }, OfferDesc::any("bytes")];
    // The family is listed bare first, which a link fixes, then with every colour name, which
    // interns them. Without the second an announcement of a declared colour was dropped whole.
    const fn video(family: &'static str) -> [OfferDesc; 3] {
        [
            OfferDesc::any(family),
            OfferDesc { family, fields: &VIDEO_FIELDS },
            OfferDesc::any("bytes"),
        ]
    }
    static VP8: [OfferDesc; 3] = video("vp8");
    static VP9: [OfferDesc; 3] = video("vp9");
    static AV1: [OfferDesc; 3] = video("av1");
    static H264: [OfferDesc; 3] = video("h264/annexb");
    static H265: [OfferDesc; 3] = video("h265/annexb");
    static MPEG4: [OfferDesc; 3] = video("mpeg4/asp");
    static MPEG2: [OfferDesc; 3] = video("mpeg2");
    static VFW: [OfferDesc; 3] = video("vfw");
    static SRT: [OfferDesc; 2] = [OfferDesc::any("subtitle/srt"), OfferDesc::any("bytes")];
    static ASS: [OfferDesc; 2] = [OfferDesc::any("subtitle/ass"), OfferDesc::any("bytes")];
    static VTT: [OfferDesc; 2] = [OfferDesc::any("subtitle/vtt"), OfferDesc::any("bytes")];
    static PGS: [OfferDesc; 2] = [OfferDesc::any("subtitle/pgs"), OfferDesc::any("bytes")];
    static VOBSUB: [OfferDesc; 2] = [OfferDesc::any("subtitle/vobsub"), OfferDesc::any("bytes")];
    static DVBSUB: [OfferDesc; 2] = [OfferDesc::any("subtitle/dvb"), OfferDesc::any("bytes")];
    static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
    match codec_id {
        "A_FLAC" => &FLAC,
        "A_AAC" => &AAC,
        "A_AC3" => &AC3,
        "A_EAC3" => &EAC3,
        "A_DTS" => &DTS,
        "A_MPEG/L1" | "A_MPEG/L2" | "A_MPEG/L3" => &MP3,
        "A_OPUS" => &OPUS,
        "A_VORBIS" => &VORBIS,
        "A_ALAC" => &ALAC,
        "A_TRUEHD" => &TRUEHD,
        "A_MLP" => &MLP,
        "A_PCM/INT/LIT" | "A_PCM/INT/BIG" | "A_PCM/FLOAT/IEEE" => &RAW,
        "V_VP8" => &VP8,
        "V_VP9" => &VP9,
        "V_AV1" => &AV1,
        "V_MPEG4/ISO/AVC" => &H264,
        "V_MPEGH/ISO/HEVC" => &H265,
        "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/AP" => &MPEG4,
        "V_MPEG2" => &MPEG2,
        "V_MS/VFW/FOURCC" => &VFW,
        "S_TEXT/UTF8" => &SRT,
        "S_TEXT/ASS" | "S_TEXT/SSA" => &ASS,
        "S_TEXT/WEBVTT" | "D_WEBVTT/SUBTITLES" | "D_WEBVTT/CAPTIONS" => &VTT,
        "S_HDMV/PGS" => &PGS,
        "S_VOBSUB" => &VOBSUB,
        "S_DVBSUB" => &DVBSUB,
        _ => &BYTES,
    }
}

pub fn is_video_family(family: &str) -> bool {
    matches!(
        family,
        "vp8"
            | "vp9"
            | "av1"
            | "h264/annexb"
            | "h265/annexb"
            | "h264/avcc"
            | "h265/hvcc"
            | "mpeg4/asp"
            | "mpeg2"
            | "vfw"
            | "vp6"
            | "vp7"
            | "flv1"
    )
}

pub use profluens_audio::Lead;

/// The length of an Opus packet in ns, from its TOC byte and frame count (RFC 6716 section 3.1).
pub fn opus_ns(packet: &[u8]) -> Option<u64> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    // Table 2 of RFC 6716, frame sizes in 48 kHz samples.
    let frame: u64 = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u64::from(*packet.get(1)? & 0x3F),
    };
    // A packet holds at most 120 ms (RFC 6716 section 3.2.5).
    let samples = frame * frames;
    (frames > 0 && samples <= 5760).then(|| samples * 1_000_000_000 / 48_000)
}

/// The track's `SeekPreRoll` (RFC 9559 section 5.1.4.1.26) or the measured row, whichever is
/// larger.
pub fn lead_of(track: &Track) -> Lead {
    let family = family_for(&track.codec_id);
    let packet = if family == "flac" { flac_block(&track.codec_private) } else { None };
    profluens_audio::lead_of(family, track_rate(track), track.seek_pre_roll_ns, packet)
}

/// A missing or nonsense rate has the default of 8000 (RFC 9559 section 5.1.4.1.29.1).
fn track_rate(track: &Track) -> u64 {
    let hz = track.sampling_frequency;
    if hz.is_finite() && hz >= 1.0 { hz as u64 } else { 8_000 }
}

/// The longest block of a FLAC stream, the second 16 bits of STREAMINFO (RFC 9639 section 8.2).
fn flac_block(head: &[u8]) -> Option<u32> {
    let max = head.get(10..12)?;
    let block = u32::from(u16::from_be_bytes([max[0], max[1]]));
    (head.starts_with(b"fLaC") && block > 0).then_some(block)
}

const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReframeError {
    Truncated(&'static str),
    Malformed(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reframer {
    Passthrough,
    Nal {
        length_size: usize,
    },
    Swap {
        width: u8,
    },
    /// f64 samples to f32, `big` for big-endian input.
    Narrow {
        big: bool,
    },
    /// Samples of `width` octets with the sign bit flipped, little-endian out. Signed 8-bit to
    /// the `u8` of `audio/raw`, unsigned 16, 24 and 32-bit to `s16`, `s24` and `s32`.
    Flip {
        width: u8,
        big: bool,
    },
}

impl Reframer {
    pub fn for_codec(codec_id: &str) -> Reframer {
        match codec_id {
            "V_MPEG4/ISO/AVC" | "V_MPEGH/ISO/HEVC" => Reframer::Nal { length_size: 4 },
            _ => Reframer::Passthrough,
        }
    }

    /// Reframe one Block's payload into the reused `out`, so the per-frame path allocates nothing.
    pub fn reframe_into<'a>(
        &self,
        block: &'a [u8],
        out: &'a mut Vec<u8>,
    ) -> Result<&'a [u8], ReframeError> {
        match self {
            Reframer::Passthrough => Ok(block),
            Reframer::Nal { length_size } => {
                length_prefixed_to_annex_b_into(block, *length_size, out)?;
                Ok(out)
            }
            Reframer::Swap { width } => {
                let w = *width as usize;
                if w == 0 || !block.len().is_multiple_of(w) {
                    return Err(ReframeError::Malformed("PCM block not a whole number of samples"));
                }
                out.clear();
                out.reserve(block.len());
                for sample in block.chunks_exact(w) {
                    out.extend(sample.iter().rev());
                }
                Ok(out)
            }
            Reframer::Narrow { big } => {
                let (samples, []) = block.as_chunks::<8>() else {
                    return Err(ReframeError::Malformed(
                        "f64 PCM block not a whole number of samples",
                    ));
                };
                out.clear();
                out.reserve(block.len() / 2);
                for &sample in samples {
                    let v =
                        if *big { f64::from_be_bytes(sample) } else { f64::from_le_bytes(sample) };
                    out.extend_from_slice(&(v as f32).to_le_bytes());
                }
                Ok(out)
            }
            Reframer::Flip { width, big } => {
                let w = *width as usize;
                if w == 0 || !block.len().is_multiple_of(w) {
                    return Err(ReframeError::Malformed("PCM block not a whole number of samples"));
                }
                out.clear();
                out.extend_from_slice(block);
                flip(out, w, *big);
                Ok(out)
            }
        }
    }

    /// Reframes `block` in place when lengths are four octets and no NAL is empty, else returns
    /// `false`.
    pub fn reframe_in_place(&self, block: &mut [u8]) -> bool {
        let length_size = match self {
            Reframer::Passthrough => return true,
            Reframer::Narrow { .. } => return false,
            Reframer::Flip { width, big } => {
                let w = *width as usize;
                if w == 0 || !block.len().is_multiple_of(w) {
                    return false;
                }
                flip(block, w, *big);
                return true;
            }
            Reframer::Swap { width } => {
                let w = *width as usize;
                if w == 0 || !block.len().is_multiple_of(w) {
                    return false;
                }
                block.chunks_exact_mut(w).for_each(|sample| sample.reverse());
                return true;
            }
            Reframer::Nal { length_size } => length_size,
        };
        let unit = |b: &[u8], at: usize| {
            b.get(at..at + 4).map(|l| u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize)
        };
        // Checked whole before a byte changes, so a block that fails is left for the copy.
        let mut at = 0usize;
        while at < block.len() {
            let Some(n) = unit(block, at).filter(|n| *length_size == 4 && *n > 0) else {
                return false;
            };
            match (at + 4).checked_add(n) {
                Some(end) if end <= block.len() => at = end,
                _ => return false,
            }
        }
        at = 0;
        while let Some(n) = unit(block, at) {
            block[at..at + 4].copy_from_slice(&START_CODE);
            at += 4 + n;
        }
        true
    }
}

/// The Annex B parameter-set head of an avcC or hvcC record and the NAL length size.
pub fn nal_head_from_config(
    codec_private: &[u8],
    is_hevc: bool,
) -> Result<(Vec<u8>, usize), ReframeError> {
    if is_hevc { parse_hvcc(codec_private) } else { parse_avcc(codec_private) }
}

/// The bits of a luma sample a codec configuration states, 8 where it states none. An avcC or
/// hvcC states it by the SPS it carries, an hvcC also by bitDepthLumaMinus8 (ISO/IEC 14496-15
/// section 8.3.3.1), which some muxers leave at 8. An av1C the high_bitdepth and twelve_bit flags
/// (AV1 in ISOBMFF section 2.3.3), a Matroska VP9 CodecPrivate is a feature list whose id 3 is
/// the bit depth (WebM codec mapping).
pub fn config_depth(family: &str, codec_private: &[u8]) -> u8 {
    match family {
        "h264/annexb" => sps_depth(false, codec_private).unwrap_or(8),
        "h265/annexb" => sps_depth(true, codec_private)
            .unwrap_or_else(|| codec_private.get(17).map_or(8, |b| (b & 7) + 8)),
        "av1" => match codec_private.get(2) {
            Some(b) if b & 0x20 != 0 => 12,
            Some(b) if b & 0x40 != 0 => 10,
            _ => 8,
        },
        "vp9" => {
            let mut at = 0;
            while let (Some(&id), Some(&len)) = (codec_private.get(at), codec_private.get(at + 1)) {
                let value = codec_private.get(at + 2..at + 2 + usize::from(len));
                if id == 3
                    && let Some(&d) = value.and_then(|v| v.first())
                {
                    return d;
                }
                at += 2 + usize::from(len);
            }
            8
        }
        _ => 8,
    }
}

/// The luma depth the first SPS of an avcC or hvcC states.
fn sps_depth(hevc: bool, record: &[u8]) -> Option<u8> {
    let (head, _) = nal_head_from_config(record, hevc).ok()?;
    for (from, to) in crate::config::nals(&head) {
        let nal = head.get(from..to)?;
        match (hevc, nal.first()?) {
            (true, h) if (h >> 1) & 0x3F == 33 => {
                return profluens_video::h265parse::parse_sps(nal).map(|s| s.depth());
            }
            (false, h) if h & 0x1F == 7 => {
                let sps = profluens_video::h264parse::parse_sps(nal)?;
                return Some((sps.bit_depth_luma_minus8 + 8).min(16) as u8);
            }
            _ => {}
        }
    }
    None
}

/// `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15 section 5.3.3.1).
// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
fn parse_avcc(data: &[u8]) -> Result<(Vec<u8>, usize), ReframeError> {
    let mut r = Cursor::new(data);
    let version = r.u8("avcC configurationVersion")?;
    if version != 1 {
        return Err(ReframeError::Malformed("avcC configurationVersion != 1"));
    }
    r.skip(3, "avcC profile/compat/level")?; // profile, compatibility, level
    let length_size = (r.u8("avcC lengthSizeMinusOne")? & 0x03) as usize + 1;
    let num_sps = r.u8("avcC numOfSequenceParameterSets")? & 0x1F;
    let mut head = Vec::new();
    for _ in 0..num_sps {
        append_nal(&mut head, r.length_prefixed_u16("avcC SPS")?);
    }
    let num_pps = r.u8("avcC numOfPictureParameterSets")?;
    for _ in 0..num_pps {
        append_nal(&mut head, r.length_prefixed_u16("avcC PPS")?);
    }
    Ok((head, length_size))
}

/// `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15 section 8.3.3.1).
// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
fn parse_hvcc(data: &[u8]) -> Result<(Vec<u8>, usize), ReframeError> {
    let mut r = Cursor::new(data);
    let version = r.u8("hvcC configurationVersion")?;
    if version != 1 {
        return Err(ReframeError::Malformed("hvcC configurationVersion != 1"));
    }
    // Octet 21's low 2 bits are lengthSizeMinusOne.
    let fixed = r.take(21, "hvcC fixed prefix")?;
    let length_size = (fixed[20] & 0x03) as usize + 1;
    let num_arrays = r.u8("hvcC numOfArrays")?;
    let mut head = Vec::new();
    for _ in 0..num_arrays {
        let _type_byte = r.u8("hvcC array type")?; // array_completeness | reserved | NAL type
        let num_nalus = r.u16("hvcC numNalus")?;
        for _ in 0..num_nalus {
            append_nal(&mut head, r.length_prefixed_u16("hvcC NAL")?);
        }
    }
    Ok((head, length_size))
}

fn length_prefixed_to_annex_b_into(
    block: &[u8],
    length_size: usize,
    out: &mut Vec<u8>,
) -> Result<(), ReframeError> {
    debug_assert!((1..=4).contains(&length_size));
    out.clear();
    out.reserve(block.len() + START_CODE.len());
    let mut at = 0usize;
    while at < block.len() {
        let end = at
            .checked_add(length_size)
            .ok_or(ReframeError::Malformed("NAL length offset overflow"))?;
        let len_bytes = block.get(at..end).ok_or(ReframeError::Truncated("NAL length field"))?;
        let mut nal_len = 0usize;
        for &b in len_bytes {
            nal_len = (nal_len << 8) | b as usize;
        }
        at = end;
        let nal_end =
            at.checked_add(nal_len).ok_or(ReframeError::Malformed("NAL length overflow"))?;
        let nal = block
            .get(at..nal_end)
            .ok_or(ReframeError::Truncated("NAL unit runs past the block"))?;
        // A zero-length NAL is skipped rather than emitted as a bare start code.
        if !nal.is_empty() {
            append_nal(out, nal);
        }
        at = nal_end;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
fn length_prefixed_to_annex_b(block: &[u8], length_size: usize) -> Result<Vec<u8>, ReframeError> {
    let mut out = Vec::new();
    length_prefixed_to_annex_b_into(block, length_size, &mut out)?;
    Ok(out)
}

fn append_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, ReframeError> {
        let b = *self.data.get(self.at).ok_or(ReframeError::Truncated(what))?;
        self.at += 1;
        Ok(b)
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, ReframeError> {
        let hi = self.u8(what)? as u16;
        let lo = self.u8(what)? as u16;
        Ok((hi << 8) | lo)
    }

    fn skip(&mut self, n: usize, what: &'static str) -> Result<(), ReframeError> {
        let _ = self.take(n, what)?;
        Ok(())
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], ReframeError> {
        let end = self.at.checked_add(n).ok_or(ReframeError::Malformed(what))?;
        let s = self.data.get(self.at..end).ok_or(ReframeError::Truncated(what))?;
        self.at = end;
        Ok(s)
    }

    fn length_prefixed_u16(&mut self, what: &'static str) -> Result<&'a [u8], ReframeError> {
        let len = self.u16(what)? as usize;
        self.take(len, what)
    }
}

/// The BlockAddID of an `S_TEXT/WEBVTT` block's cue settings and identifier, the default one
/// mkvmerge leaves unwritten.
pub const WEBVTT_ADD_ID: u64 = 1;

/// A WebVTT cue in WebM's `D_WEBVTT` block form, its identifier, a line feed, its settings, a
/// line feed, its text, which every `subtitle/vtt` buffer holds. From an `S_TEXT/WEBVTT`
/// block's text and its BlockAdditional, which holds the settings line then the identifier
/// line, comments after them left off.
pub fn webm_vtt(text: &[u8], addition: &[u8], out: &mut Vec<u8>) {
    let mut lines = addition.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let settings = lines.next().unwrap_or_default();
    let id = lines.next().unwrap_or_default();
    out.clear();
    out.reserve(id.len() + settings.len() + text.len() + 2);
    out.extend_from_slice(id);
    out.push(b'\n');
    out.extend_from_slice(settings);
    out.push(b'\n');
    out.extend_from_slice(text);
}

/// Little-endian samples of `w` octets with the sign bit of each flipped, in place.
fn flip(block: &mut [u8], w: usize, big: bool) {
    for sample in block.chunks_exact_mut(w) {
        if big {
            sample.reverse();
        }
        sample[w - 1] ^= 0x80;
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    // Test fixtures allocate freely.
    #[allow(clippy::disallowed_methods)]
    fn reframing_in_place_gives_the_bytes_of_the_copy_or_leaves_the_block() {
        let nal = |len: u32, fill: u8| [&len.to_be_bytes()[..], &vec![fill; len as usize]].concat();
        let four = Reframer::Nal { length_size: 4 };
        let good = [nal(5, 7), nal(1, 9), nal(300, 3)].concat();
        let mut out = Vec::new();
        let want = four.reframe_into(&good, &mut out).expect("reframes").to_vec();
        let mut block = good.clone();
        assert!(four.reframe_in_place(&mut block));
        assert_eq!(block, want);
        for bad in [
            [nal(5, 7), nal(0, 0)].concat(),
            [nal(5, 7), vec![0, 0, 1]].concat(),
            [nal(5, 7), nal(9, 1)[..8].to_vec()].concat(),
        ] {
            let mut block = bad.clone();
            assert!(
                !four.reframe_in_place(&mut block),
                "an empty unit, a cut length or a cut unit"
            );
            assert_eq!(block, bad, "left as it was");
        }
        let mut two = [&[0u8, 3][..], b"abc"].concat();
        assert!(
            !Reframer::Nal { length_size: 2 }.reframe_in_place(&mut two),
            "a shorter length grows the block"
        );
        assert!(Reframer::Passthrough.reframe_in_place(&mut two));
        let mut empty: [u8; 0] = [];
        assert!(four.reframe_in_place(&mut empty));
    }

    #[test]
    fn pcm_reframers_flip_signed_bytes_and_narrow_either_byte_order() {
        let mut out = Vec::new();
        let s8 = [0x80u8, 0xFF, 0x00, 0x7F];
        let u8s = [0x00u8, 0x7F, 0x80, 0xFF];
        let one = Reframer::Flip { width: 1, big: false };
        assert_eq!(one.reframe_into(&s8, &mut out).expect("flips"), u8s);
        let mut block = s8;
        assert!(one.reframe_in_place(&mut block));
        assert_eq!(block, u8s);
        let u16s = [0u16, 0x7FFF, 0x8000, 0xFFFF];
        let s16 = [i16::MIN, -1, 0, i16::MAX].map(i16::to_le_bytes).concat();
        let le = u16s.map(u16::to_le_bytes).concat();
        let be = u16s.map(u16::to_be_bytes).concat();
        assert_eq!(
            Reframer::Flip { width: 2, big: false }.reframe_into(&le, &mut out).unwrap(),
            s16
        );
        assert_eq!(
            Reframer::Flip { width: 2, big: true }.reframe_into(&be, &mut out).unwrap(),
            s16
        );
        let mut block = be.clone();
        assert!(Reframer::Flip { width: 2, big: true }.reframe_in_place(&mut block));
        assert_eq!(block, s16);
        let u24be = [0x80u8, 0x00, 0x01];
        assert_eq!(
            Reframer::Flip { width: 3, big: true }.reframe_into(&u24be, &mut out).unwrap(),
            [1, 0, 0]
        );
        assert!(Reframer::Flip { width: 2, big: false }.reframe_into(&le[..3], &mut out).is_err());
        assert!(!Reframer::Flip { width: 2, big: false }.reframe_in_place(&mut block[..3]));
        let v = [-0.5f64, 1.25];
        let f32le = [(-0.5f32).to_le_bytes(), 1.25f32.to_le_bytes()].concat();
        let le = [v[0].to_le_bytes(), v[1].to_le_bytes()].concat();
        let be = [v[0].to_be_bytes(), v[1].to_be_bytes()].concat();
        assert_eq!(Reframer::Narrow { big: false }.reframe_into(&le, &mut out).expect("le"), f32le);
        assert_eq!(Reframer::Narrow { big: true }.reframe_into(&be, &mut out).expect("be"), f32le);
        assert!(Reframer::Narrow { big: true }.reframe_into(&be[..9], &mut out).is_err());
    }

    #[test]
    fn pcm_of_a_depth_between_named_samples_is_named_by_its_octets() {
        let pcm = |codec: &str, depth: u32| {
            let mut t = track(codec, 48_000.0, 0, &[]);
            t.bit_depth = depth;
            (track_family(&t), pcm_sample(&t), pcm_reframer(&t))
        };
        let lit = pcm("A_PCM/INT/LIT", 20);
        assert_eq!(lit, ("audio/raw", Some("s24"), Reframer::Passthrough));
        assert_eq!(
            pcm("A_PCM/INT/BIG", 20),
            ("audio/raw", Some("s24"), Reframer::Swap { width: 3 })
        );
        assert_eq!(pcm("A_PCM/INT/LIT", 12).1, Some("s16"));
        assert_eq!(pcm("A_PCM/INT/BIG", 12).2, Reframer::Swap { width: 2 });
        assert_eq!(pcm("A_PCM/INT/LIT", 8), ("audio/raw", Some("u8"), Reframer::Passthrough));
        assert_eq!(pcm("A_PCM/INT/LIT", 0).0, "pcm", "no depth");
        assert_eq!(pcm("A_PCM/INT/LIT", 40).0, "pcm", "past 32 bits");
        assert_eq!(pcm("A_PCM/FLOAT/IEEE", 24).0, "pcm", "no float of 24 bits");
    }

    fn track(codec_id: &str, rate: f64, pre_roll_ns: u64, private: &[u8]) -> Track {
        use crate::writer::{MatroskaWriter, TrackConfig};
        let mut config = TrackConfig::audio(1, codec_id, private.to_vec(), rate, 2, 0);
        config.seek_pre_roll_ns = pre_roll_ns;
        let mut stream = Vec::new();
        MatroskaWriter::new(vec![config]).write_header(&mut stream).unwrap();
        let mut reader = crate::reader::MatroskaReader::new();
        reader.push(&stream).unwrap();
        reader.tracks()[0].clone()
    }

    #[test]
    fn a_tracks_pre_roll_is_the_larger_of_what_it_declares_and_what_was_measured() {
        let lead = |codec_id, rate, declared| lead_of(&track(codec_id, rate, declared, &[]));
        assert_eq!(lead("A_AAC", 48_000.0, 0), Lead { pre_roll: 21_333_334, packet: 42_666_667 });
        assert_eq!(lead("A_AAC", 44_100.0, 0).pre_roll, 23_219_955);
        assert_eq!(lead("A_AAC", 48_000.0, 50_000_000).pre_roll, 50_000_000);
        assert_eq!(
            lead("A_MPEG/L3", 48_000.0, 0),
            Lead { pre_roll: 72_000_000, packet: 24_000_000 }
        );
        assert_eq!(lead("A_MPEG/L3", 44_100.0, 0).pre_roll, 78_367_347);
        assert_eq!(lead("A_AC3", 48_000.0, 0), Lead { pre_roll: 5_333_334, packet: 32_000_000 });
        assert_eq!(lead("A_EAC3", 48_000.0, 0), lead("A_AC3", 48_000.0, 0));
        assert_eq!(
            lead("A_OPUS", 48_000.0, 80_000_000),
            Lead { pre_roll: 200_000_000, packet: 120_000_000 }
        );
        assert_eq!(lead("A_OPUS", 44_100.0, 0).pre_roll, 200_000_000);
        assert_eq!(lead("A_OPUS", 48_000.0, 300_000_000).pre_roll, 300_000_000);
        assert_eq!(
            lead("A_VORBIS", 48_000.0, 0),
            Lead { pre_roll: 42_666_667, packet: 42_666_667 }
        );
        assert_eq!(lead("A_DTS", 48_000.0, 0), Lead { pre_roll: 0, packet: 0 });
        assert_eq!(lead("A_DTS", 48_000.0, 7_000_000), Lead { pre_roll: 7_000_000, packet: 0 });
    }

    #[test]
    fn a_flac_tracks_packet_is_the_longest_block_of_its_streaminfo() {
        let mut head = b"fLaC\x80\x00\x00\x22".to_vec();
        head.extend_from_slice(&[0x12, 0x00, 0x12, 0x00]);
        head.resize(8 + 34, 0);
        assert_eq!(
            lead_of(&track("A_FLAC", 48_000.0, 0, &head)),
            Lead { pre_roll: 0, packet: 96_000_000 }
        );
        assert_eq!(lead_of(&track("A_FLAC", 48_000.0, 0, b"fLaC")).packet, 1_365_312_500);
        assert_eq!(lead_of(&track("A_FLAC", 48_000.0, 0, &[0; 42])).packet, 1_365_312_500);
    }

    #[test]
    fn a_track_with_no_rate_has_the_default() {
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY, 0.5] {
            let mut t = track("A_AAC", 48_000.0, 0, &[]);
            t.sampling_frequency = rate;
            assert_eq!(lead_of(&t).pre_roll, 128_000_000, "1024 samples at 8 kHz, for {rate}");
        }
    }

    #[test]
    fn emits_names_every_family_a_track_is_announced_as() {
        use crate::encoding::Refusal;
        let mut bih = [0u8; 40];
        let vfw = [*b"VP60", *b"VP70", *b"FLV1", *b"XVID", *b"H264", *b"????"].map(|cc| {
            bih[16..20].copy_from_slice(&cc);
            vfw_family(&bih)
        });
        let refused = [Refusal::Encrypted, Refusal::Algo(1)].map(Refusal::family);
        let table = CODEC_IDS.iter().map(|e| e.1);
        for family in table.chain(vfw).chain(refused).chain(["pcm"]) {
            assert!(EMITS.contains(&family), "{family} missing from EMITS");
        }
        for &(id, family) in &CODEC_IDS {
            assert_eq!(offers_for(id)[0].family, family, "{id}");
        }
    }

    #[test]
    fn an_aac_offer_declares_every_name_its_announcement_states() {
        let declared = offers_for("A_AAC")[0].fields;
        for name in ["rate", "channels", "delay"] {
            assert!(declared.iter().any(|f| f.field == name), "{name}");
        }
    }

    #[test]
    fn vfw_fourccs_name_the_on2_and_sorenson_families() {
        let bih = |cc: &[u8; 4]| {
            let mut b = [0u8; 40];
            b[16..20].copy_from_slice(cc);
            b
        };
        for (cc, family) in [
            (b"VP62", "vp6"),
            (b"vp60", "vp6"),
            (b"VP71", "vp7"),
            (b"FLV1", "flv1"),
            (b"S263", "flv1"),
            (b"VP6F", "vfw"),
        ] {
            assert_eq!(vfw_family(&bih(cc)), family, "{cc:?}");
        }
        assert!(is_video_family("vp6") && is_video_family("vp7") && is_video_family("flv1"));
    }

    #[test]
    fn family_mapping_covers_the_codec_ids() {
        assert_eq!(family_for("V_VP8"), "vp8");
        assert_eq!(family_for("V_VP9"), "vp9");
        assert_eq!(family_for("V_AV1"), "av1");
        assert_eq!(family_for("V_MPEG4/ISO/AVC"), "h264/annexb");
        assert_eq!(family_for("V_MPEGH/ISO/HEVC"), "h265/annexb");
        assert_eq!(family_for("A_FLAC"), "flac");
        assert_eq!(family_for("S_TEXT/UTF8"), "subtitle/srt");
        assert_eq!(family_for("S_TEXT/ASS"), "subtitle/ass");
        assert_eq!(family_for("S_TEXT/SSA"), "subtitle/ass", "legacy SSA shares the ASS family");
        assert_eq!(family_for("S_TEXT/WEBVTT"), "subtitle/vtt");
        assert_eq!(family_for("D_WEBVTT/SUBTITLES"), "subtitle/vtt");
        assert_eq!(family_for("D_WEBVTT/CAPTIONS"), "subtitle/vtt");
        assert_eq!(family_for("D_WEBVTT/METADATA"), "bytes", "shows nothing");
        assert_eq!(family_for("S_HDMV/PGS"), "subtitle/pgs", "PGS bitmap subtitle");
        assert_eq!(family_for("S_VOBSUB"), "subtitle/vobsub");
        assert_eq!(family_for("S_DVBSUB"), "subtitle/dvb");
        assert_eq!(family_for("A_AC3"), "ac3", "Dolby Digital");
        assert_eq!(family_for("A_EAC3"), "eac3", "Dolby Digital Plus");
        assert_eq!(family_for("A_DTS"), "dts");
        assert_eq!(family_for("A_MPEG/L3"), "mp3");
        assert_eq!((family_for("A_MPEG/L1"), family_for("A_MPEG/L2")), ("mp3", "mp3"));
        assert_eq!(offers_for("A_MPEG/L2")[0].family, "mp3");
        assert_eq!((family_for("A_MLP"), offers_for("A_MLP")[0].family), ("mlp", "mlp"));
        assert_eq!(family_for("A_OPUS"), "opus", "named so autoplug reports 'no decoder for opus'");
        assert_eq!(family_for("A_VORBIS"), "vorbis");
        assert_eq!(family_for("A_ALAC"), "alac");
        assert_eq!(family_for("V_REAL/RV40"), "bytes", "unknown → bytes fallback");
        assert_eq!(family_for(""), "bytes", "empty codec id → bytes fallback");
    }

    #[test]
    fn subtitle_offer_menus_carry_family_plus_bytes() {
        for (id, family) in [
            ("S_TEXT/UTF8", "subtitle/srt"),
            ("S_TEXT/ASS", "subtitle/ass"),
            ("S_TEXT/SSA", "subtitle/ass"),
            ("S_TEXT/WEBVTT", "subtitle/vtt"),
            ("D_WEBVTT/SUBTITLES", "subtitle/vtt"),
            ("S_HDMV/PGS", "subtitle/pgs"),
            ("S_VOBSUB", "subtitle/vobsub"),
            ("S_DVBSUB", "subtitle/dvb"),
        ] {
            let offers = offers_for(id);
            assert_eq!(offers.len(), 2, "{id}: family + bytes escape");
            assert_eq!(offers[0].family, family, "{id} first offer is its family");
            assert_eq!(offers[1].family, "bytes", "{id} second offer is the bytes escape");
        }
    }

    /// mkvmerge's BlockAdditional holds the settings line then the identifier line, WebM's
    /// block the identifier first.
    #[test]
    fn a_webvtt_block_and_its_addition_go_in_webm_form() {
        let mut out = Vec::new();
        webm_vtt(b"<i>Hi</i>\nthere", b"align:start line:10%\ncue-1\n", &mut out);
        assert_eq!(out, b"cue-1\nalign:start line:10%\n<i>Hi</i>\nthere");
        webm_vtt(b"plain", b"", &mut out);
        assert_eq!(out, b"\n\nplain");
        webm_vtt(b"x", b"size:50%\r\nid\r\nNOTE a comment\n", &mut out);
        assert_eq!(out, b"id\nsize:50%\nx");
    }

    #[test]
    fn only_nal_codecs_reframe() {
        assert!(matches!(Reframer::for_codec("V_MPEG4/ISO/AVC"), Reframer::Nal { length_size: 4 }));
        assert!(matches!(
            Reframer::for_codec("V_MPEGH/ISO/HEVC"),
            Reframer::Nal { length_size: 4 }
        ));
        assert!(matches!(Reframer::for_codec("V_VP8"), Reframer::Passthrough));
        assert!(matches!(Reframer::for_codec("A_FLAC"), Reframer::Passthrough));
    }

    #[test]
    fn avcc_parses_to_annex_b_head() {
        let sps = [0x67, 0x42, 0x00, 0x0A];
        let pps = [0x68, 0xCE, 0x3C, 0x80];
        let mut cfg = vec![
            1, // configurationVersion
            0x42, 0x00, 0x0A, // profile, compat, level
            0xFF, // 111111 | lengthSizeMinusOne=3, lengthSize 4
            0xE1, // 111 | numOfSPS=1
        ];
        cfg.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        cfg.extend_from_slice(&sps);
        cfg.push(1); // numOfPPS
        cfg.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        cfg.extend_from_slice(&pps);

        let (head, length_size) = parse_avcc(&cfg).expect("avcC parses");
        assert_eq!(length_size, 4);
        let mut want = Vec::new();
        want.extend_from_slice(&START_CODE);
        want.extend_from_slice(&sps);
        want.extend_from_slice(&START_CODE);
        want.extend_from_slice(&pps);
        assert_eq!(head, want, "SPS then PPS as 4-byte start-code NALs");
    }

    #[test]
    fn hvcc_parses_to_annex_b_head() {
        let vps = [0x40, 0x01, 0x0C];
        let sps = [0x42, 0x01, 0x01];
        let pps = [0x44, 0x01, 0xC0];
        let mut cfg = vec![1u8]; // configurationVersion
        cfg.extend_from_slice(&[0u8; 20]); // 20 octets of profile/tier/level/format info
        cfg.push(0xFC | 0x03); // octet 21: lengthSizeMinusOne=3 in the low 2 bits
        cfg.push(3); // numOfArrays
        for (nal_type, nal) in [(32u8, &vps[..]), (33, &sps[..]), (34, &pps[..])] {
            cfg.push(nal_type); // array_completeness|reserved|NAL_unit_type
            cfg.extend_from_slice(&1u16.to_be_bytes()); // numNalus = 1
            cfg.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            cfg.extend_from_slice(nal);
        }
        let (head, length_size) = parse_hvcc(&cfg).expect("hvcC parses");
        assert_eq!(length_size, 4);
        let mut want = Vec::new();
        for nal in [&vps[..], &sps[..], &pps[..]] {
            want.extend_from_slice(&START_CODE);
            want.extend_from_slice(nal);
        }
        assert_eq!(head, want, "VPS, SPS, PPS as 4-byte start-code NALs");
    }

    #[test]
    fn length_prefixed_block_reframes() {
        let n1 = [0x65, 0x11, 0x22];
        let n2 = [0x41, 0x33];
        let mut block = Vec::new();
        block.extend_from_slice(&(n1.len() as u32).to_be_bytes());
        block.extend_from_slice(&n1);
        block.extend_from_slice(&(n2.len() as u32).to_be_bytes());
        block.extend_from_slice(&n2);

        let out = length_prefixed_to_annex_b(&block, 4).expect("reframe");
        let mut want = Vec::new();
        want.extend_from_slice(&START_CODE);
        want.extend_from_slice(&n1);
        want.extend_from_slice(&START_CODE);
        want.extend_from_slice(&n2);
        assert_eq!(out, want);
    }

    #[test]
    fn length_size_two_reframes() {
        let nal = [0x67, 0xAB];
        let mut block = Vec::new();
        block.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        block.extend_from_slice(&nal);
        let out = length_prefixed_to_annex_b(&block, 2).expect("reframe");
        let mut want = Vec::new();
        want.extend_from_slice(&START_CODE);
        want.extend_from_slice(&nal);
        assert_eq!(out, want);
    }

    #[test]
    fn malformed_records_error_not_panic() {
        let cases: &[(&str, Vec<u8>, bool)] = &[
            ("empty avcC", vec![], false),
            ("avcC wrong version", vec![2, 0, 0, 0, 0xFF, 0xE0], false),
            ("avcC truncated before SPS count", vec![1, 0x42, 0x00, 0x0A, 0xFF], false),
            (
                "avcC SPS length runs past end",
                vec![1, 0x42, 0x00, 0x0A, 0xFF, 0xE1, 0xFF, 0xFF, 0x00],
                false,
            ),
            ("empty hvcC", vec![], true),
            ("hvcC wrong version", vec![9], true),
            ("hvcC truncated prefix", vec![1, 0, 0, 0], true),
        ];
        for (name, bytes, is_hevc) in cases {
            let r = nal_head_from_config(bytes, *is_hevc);
            assert!(r.is_err(), "{name} must error, not parse");
        }
    }

    #[test]
    fn depth_from_the_codec_configs() {
        let mut hvcc = vec![
            1u8, 2, 0x20, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x99, 0xf0, 0, 0xfc, 0xfd, 0xfa, 0xfa, 0,
        ];
        assert_eq!(config_depth("h265/annexb", &hvcc), 10);
        hvcc[17] = 0xf8;
        assert_eq!(config_depth("h265/annexb", &hvcc), 8);
        assert_eq!(config_depth("h265/annexb", &[1, 2]), 8, "a short config is 8 bit");
        assert_eq!(config_depth("av1", &[0x81, 0x00, 0x4c, 0x00]), 10);
        assert_eq!(config_depth("av1", &[0x81, 0x00, 0x6c, 0x00]), 12);
        assert_eq!(config_depth("av1", &[0x81, 0x00, 0x0c, 0x00]), 8);
        assert_eq!(config_depth("vp9", &[1, 1, 2, 2, 1, 31, 3, 1, 10, 4, 1, 1]), 10);
        assert_eq!(config_depth("vp9", &[1, 1, 0]), 8);
        assert_eq!(config_depth("vp9", &[3, 0]), 8, "an empty value");
        assert_eq!(config_depth("h264/annexb", &[1, 100, 0, 31]), 8);
        assert_eq!(config_depth("vp8", &[]), 8);
    }

    #[test]
    fn malformed_blocks_error_not_panic() {
        let block = vec![0x00, 0x00, 0x00, 0xFF, 0x41];
        assert!(length_prefixed_to_annex_b(&block, 4).is_err());
        let block = vec![0x00, 0x00];
        assert!(length_prefixed_to_annex_b(&block, 4).is_err());
        assert_eq!(length_prefixed_to_annex_b(&[], 4).unwrap(), Vec::<u8>::new());
    }
}
