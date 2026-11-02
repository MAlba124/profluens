//! Sample entries for [`crate::writer`] from elementary streams, the samples in the form those
//! entries want, and the RFC 6381 `codecs` names a playlist lists them by. H.264 and H.265 from
//! Annex B, VP9 from a keyframe's uncompressed header, AV1 from an `av1C` or a sequence header.

use pf_mkv::config::{self, ConfigError, NalCodec, NalKind, ParamSets, nals};

use crate::writer::Entry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryError {
    /// No usable SPS or PPS in what should hold them.
    ParamSets(ConfigError),
    /// No VP9 keyframe header where one should be.
    Vp9Head,
    /// No AV1 sequence header where one should be.
    Av1(ConfigError),
}

impl std::fmt::Display for EntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParamSets(e) => write!(f, "parameter sets: {e}"),
            Self::Vp9Head => f.write_str("no VP9 keyframe header"),
            Self::Av1(e) => write!(f, "av1C: {e}"),
        }
    }
}

impl std::error::Error for EntryError {}

/// The NAL codec of a family, None for one without NAL units.
pub fn nal_codec(family: &str) -> Option<NalCodec> {
    match family {
        "h264/annexb" => Some(NalCodec::H264),
        "h265/annexb" => Some(NalCodec::H265),
        _ => None,
    }
}

/// Access unit delimiters, H.264 type 9 and H.265 type 35.
fn is_aud(codec: NalCodec, b0: u8) -> bool {
    match codec {
        NalCodec::H264 => b0 & 0x1F == 9,
        NalCodec::H265 => (b0 >> 1) & 0x3F == 35,
    }
}

/// An Annex B access unit length-prefixed onto `out`, the parameter sets and delimiters left out
/// since the sample entry holds the sets.
pub fn length_prefixed(codec: NalCodec, au: &[u8], out: &mut Vec<u8>) {
    for (a, b) in nals(au) {
        let nal = &au[a..b];
        if matches!(codec.kind(nal[0]), NalKind::Vps | NalKind::Sps | NalKind::Pps)
            || is_aud(codec, nal[0])
        {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
}

/// Whether an Annex B buffer holds a coded slice, a codec head holds only parameter sets.
pub fn has_picture(codec: NalCodec, au: &[u8]) -> bool {
    nals(au).any(|(a, _)| codec.kind(au[a]) == NalKind::Vcl)
}

/// The sample entry of an H.264 or H.265 track from Annex B bytes that hold its sets.
pub fn nal_entry(codec: NalCodec, annexb: &[u8]) -> Result<Entry, EntryError> {
    let mut ps = ParamSets::new(codec);
    ps.absorb(annexb).map_err(EntryError::ParamSets)?;
    let record = ps.record().map_err(EntryError::ParamSets)?;
    Ok(match codec {
        NalCodec::H264 => Entry::Avc(record),
        NalCodec::H265 => Entry::Hevc { record, inband: false },
    })
}

/// The RFC 6381 `codecs` name of a sample entry, None for one without a name here.
pub fn codecs(entry: &Entry) -> Option<String> {
    match entry {
        Entry::Avc(r) if r.len() >= 4 => Some(format!("avc1.{:02X}{:02X}{:02X}", r[1], r[2], r[3])),
        Entry::Hevc { record, inband } => {
            hevc_codecs(record, if *inband { "hev1" } else { "hvc1" })
        }
        Entry::Aac(asc) => {
            let aot = asc.first()? >> 3;
            Some(format!("mp4a.40.{aot}"))
        }
        Entry::Mp3 => Some("mp4a.6B".into()),
        Entry::Opus(_) => Some("opus".into()),
        Entry::Flac(_) => Some("fLaC".into()),
        Entry::Vp9(v) => vp9_codecs(v),
        Entry::Av1(r) => av1_codecs(r),
        _ => None,
    }
}

/// ISO/IEC 14496-15 annex E.3 from an `HEVCDecoderConfigurationRecord`.
fn hevc_codecs(r: &[u8], tag: &str) -> Option<String> {
    let b = r.get(..13)?;
    let space = ["", "A", "B", "C"][usize::from(b[1] >> 6)];
    let tier = if b[1] & 0x20 != 0 { 'H' } else { 'L' };
    let profile = b[1] & 0x1F;
    let compat = u32::from_be_bytes([b[2], b[3], b[4], b[5]]).reverse_bits();
    let mut s = format!("{tag}.{space}{profile}.{compat:X}.{tier}{}", b[12]);
    let flags = &b[6..12];
    let n = flags.iter().rposition(|&f| f != 0).map_or(0, |i| i + 1);
    for f in &flags[..n] {
        s.push_str(&format!(".{f:X}"));
    }
    Some(s)
}

/// What a VP9 keyframe's uncompressed header says of the stream (VP9 bitstream 6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Head {
    pub profile: u8,
    pub depth: u8,
    /// `vpcC` chromaSubsampling, 1 for 4:2:0, 2 for 4:2:2, 3 for 4:4:4.
    pub chroma: u8,
    pub full: bool,
    /// The bitstream's color_space, 7 for RGB.
    pub space: u8,
}

/// The container's side of a VP9 entry, the H.273 code points as Matroska gives them, 2 for
/// unspecified, and the rate for the level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Meta {
    pub width: u32,
    pub height: u32,
    pub fps_milli: u32,
    /// matrix, primaries, transfer, range.
    pub colour: [u8; 4],
}

/// Big endian bits from the front of a buffer.
struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn get(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            let byte = *self.b.get(self.at / 8)?;
            v = v << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Some(v)
    }
}

/// The frames of a VP9 block into `out`, a superframe's from its index at the end (VP9 annex B),
/// the count returned.
fn vp9_frames<'a>(data: &'a [u8], out: &mut [&'a [u8]; 8]) -> usize {
    let one = |out: &mut [&'a [u8]; 8]| {
        out[0] = data;
        1
    };
    let Some(&m) = data.last() else { return 0 };
    if m & 0xE0 != 0xC0 {
        return one(out);
    }
    let n = usize::from(m & 7) + 1;
    let mag = usize::from(m >> 3 & 3) + 1;
    let idx = 2 + mag * n;
    if data.len() < idx || data[data.len() - idx] != m {
        return one(out);
    }
    let sizes = &data[data.len() - idx + 1..data.len() - 1];
    let mut at = 0;
    for (i, s) in sizes.chunks_exact(mag).enumerate() {
        let len = s.iter().rev().fold(0usize, |v, &b| v << 8 | usize::from(b));
        let Some(f) = data.get(at..at + len).filter(|_| at + len <= data.len() - idx) else {
            return 0;
        };
        out[i] = f;
        at += len;
    }
    n
}

/// The header of a VP9 keyframe, None for any other frame.
fn vp9_key(f: &[u8]) -> Option<Vp9Head> {
    let mut b = Bits { b: f, at: 0 };
    if b.get(2)? != 2 {
        return None;
    }
    let low = b.get(1)?;
    let profile = (b.get(1)? << 1 | low) as u8;
    if profile == 3 && b.get(1)? != 0 {
        return None;
    }
    // show_existing_frame, then frame_type 0 for a keyframe
    if b.get(1)? != 0 || b.get(1)? != 0 {
        return None;
    }
    b.get(2)?;
    if b.get(24)? != 0x49_8342 {
        return None;
    }
    let depth = if profile >= 2 { 10 + 2 * b.get(1)? as u8 } else { 8 };
    let space = b.get(3)? as u8;
    let odd = profile & 1 == 1;
    let (full, sub) = if space == 7 {
        (true, (0, 0))
    } else {
        let full = b.get(1)? == 1;
        (full, if odd { (b.get(1)?, b.get(1)?) } else { (1, 1) })
    };
    let chroma = match sub {
        (1, 1) => 1,
        (1, 0) => 2,
        (0, 0) => 3,
        _ => return None,
    };
    Some(Vp9Head { profile, depth, chroma, full, space })
}

/// The header of the first keyframe in a VP9 block, superframe or not.
pub fn vp9_head(block: &[u8]) -> Option<Vp9Head> {
    let mut fr: [&[u8]; 8] = [&[]; 8];
    let n = vp9_frames(block, &mut fr);
    fr[..n].iter().find_map(|f| vp9_key(f))
}

/// The VP9 level of a picture size and rate, FFmpeg's `get_vp9_level` table, 0 past level 6.2.
pub fn vp9_level(width: u32, height: u32, fps_milli: u32) -> u8 {
    const LEVELS: [(u64, u64, u8); 14] = [
        (829_440, 36_864, 10),
        (2_764_800, 73_728, 11),
        (4_608_000, 122_880, 20),
        (9_216_000, 245_760, 21),
        (20_736_000, 552_960, 30),
        (36_864_000, 983_040, 31),
        (83_558_400, 2_228_224, 40),
        (160_432_128, 2_228_224, 41),
        (311_951_360, 8_912_896, 50),
        (588_251_136, 8_912_896, 51),
        (1_176_502_272, 8_912_896, 52),
        (1_176_502_272, 35_651_584, 60),
        (2_353_004_544, 35_651_584, 61),
        (4_706_009_088, 35_651_584, 62),
    ];
    let size = u64::from(width) * u64::from(height);
    if size == 0 {
        return 0;
    }
    let rate = size * u64::from(fps_milli) / 1000;
    LEVELS.iter().find(|&&(r, s, _)| rate <= r && size <= s).map_or(0, |l| l.2)
}

/// The sample entry of a VP9 track from a block that holds a keyframe.
pub fn vp9_entry(block: &[u8], m: &Vp9Meta) -> Result<Entry, EntryError> {
    let h = vp9_head(block).ok_or(EntryError::Vp9Head)?;
    // VP9 color_space to H.273 matrix as FFmpeg's decoder maps it
    const MATRIX: [u8; 8] = [2, 5, 1, 6, 7, 9, 2, 0];
    let [matrix, primaries, transfer, _] = m.colour;
    let matrix = if matrix == 2 { MATRIX[usize::from(h.space & 7)] } else { matrix };
    Ok(Entry::Vp9(vec![
        h.profile,
        vp9_level(m.width, m.height, m.fps_milli),
        h.depth << 4 | h.chroma << 1 | u8::from(h.full),
        primaries,
        transfer,
        matrix,
        0,
        0,
    ]))
}

/// The `vp09` codecs name (VP Codec ISO Media File Format Binding, codecs parameter string), the
/// short form unless chroma, colour or range differ from its defaults, unspecified code points
/// taken as those defaults.
fn vp9_codecs(v: &[u8]) -> Option<String> {
    let b = v.get(..6)?;
    let (depth, chroma, full) = (b[2] >> 4, b[2] >> 1 & 7, b[2] & 1);
    let mut s = format!("vp09.{:02}.{:02}.{depth:02}", b[0], b[1]);
    let cp = |x: u8| if x == 2 { 1 } else { x };
    let (p, t, mx) = (cp(b[3]), cp(b[4]), cp(b[5]));
    if (chroma, p, t, mx, full) != (1, 1, 1, 1, 0) {
        s.push_str(&format!(".{chroma:02}.{p:02}.{t:02}.{mx:02}.{full:02}"));
    }
    Some(s)
}

/// The sample entry of an AV1 track, a Matroska CodecPrivate that is an `av1C` record as it is,
/// else from the sequence header in `key`, a temporal unit.
// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
pub fn av1_entry(private: &[u8], key: &[u8]) -> Result<Entry, EntryError> {
    if private.len() > 4 && private[0] == 0x81 {
        return Ok(Entry::Av1(private.to_vec()));
    }
    config::av1c(key).map(|(record, _)| Entry::Av1(record)).map_err(EntryError::Av1)
}

/// A temporal unit as an ISOBMFF sample, its temporal delimiter left out (AV1-ISOBMFF 2.4).
pub fn av1_sample(tu: &[u8]) -> &[u8] {
    let n = config::leading_temporal_delimiter(tu);
    if n < tu.len() { &tu[n..] } else { tu }
}

/// AV1-ISOBMFF's codecs parameter string from an `av1C` record, its short form.
fn av1_codecs(r: &[u8]) -> Option<String> {
    let b = r.get(..3)?;
    let (profile, level) = (b[1] >> 5, b[1] & 0x1F);
    let tier = if b[2] & 0x80 != 0 { 'H' } else { 'M' };
    let depth = match (b[2] & 0x40 != 0, b[2] & 0x20 != 0) {
        (true, true) => 12,
        (true, false) => 10,
        _ => 8,
    };
    Some(format!("av01.{profile}.{level:02}{tier}.{depth:02}"))
}

#[cfg(test)]
mod tests;
