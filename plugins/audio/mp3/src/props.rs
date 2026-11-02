//! No-decode MP3 property probing, duration included, from the Xing/Info or VBRI header in the
//! first frame (ISO/IEC 11172-3 2.4.1.3, 2.4.2.3).

use crate::frame::{Framed, Header, next_frame};

/// Xing/Info 100-point seek table (flags bit 2). Entry `i` is the byte position of `i`/100 of the
/// duration, in 256ths of the stream bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XingToc {
    pub points: [u8; XingToc::LEN],
}

impl XingToc {
    pub const LEN: usize = 100;

    /// Byte offset of seek point `i` from the first audio frame. `None` for `i >= 100`.
    pub fn byte_at(&self, i: usize, stream_bytes: u64) -> Option<u64> {
        let frac = u128::from(*self.points.get(i)?);
        Some((frac * u128::from(stream_bytes) / 256).min(u128::from(u64::MAX)) as u64)
    }

    /// Stream time of seek point `i`. `None` for `i >= 100`.
    pub fn time_at(&self, i: usize, duration_ns: u64) -> Option<u64> {
        if i >= Self::LEN {
            return None;
        }
        Some((i as u128 * u128::from(duration_ns) / Self::LEN as u128) as u64)
    }
}

/// What an MP3 stream declares about itself, read from its first frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioProps {
    /// `None` for a free-format stream with no VBR header.
    pub duration_ns: Option<u64>,
    /// True when `duration_ns` came from a VBR header frame count, less any LAME delay and
    /// padding.
    pub exact: bool,
    pub sample_rate: u32,
    pub channels: u32,
    /// The first frame's bitrate, `None` for free format. Not the VBR average.
    pub bitrate_kbps: Option<u32>,
    /// Offset of the first sync word within the probed window.
    pub first_frame_offset: usize,
    pub toc: Option<XingToc>,
    /// Byte count stated by the VBR header, the base of [`XingToc::byte_at`].
    pub stream_bytes: Option<u32>,
    /// LAME encoder delay in samples. FFmpeg drops it plus the decoder's 529 at the start.
    pub delay: Option<u32>,
    /// LAME encoder padding in samples at the end.
    pub padding: Option<u32>,
    pub frames: Option<u32>,
}

/// Probe an MP3's properties without decoding. `audio` starts at the first post-tag byte and
/// should hold the whole first frame, `audio_len` excludes tags. `None` when no frame is found.
pub fn probe_props(audio: &[u8], audio_len: u64) -> Option<AudioProps> {
    // `NeedMore` still yields the header fields and the CBR estimate, but not the VBR header.
    let (offset, header) = match next_frame(audio) {
        Framed::Frame { offset, header, .. } => (offset, header),
        Framed::NeedMore { offset } => (offset, Header::parse(audio.get(offset..)?)?),
        Framed::NoSync => return None,
    };
    if header.sample_rate_hz == 0 {
        return None;
    }
    let mut props = AudioProps {
        duration_ns: None,
        exact: false,
        sample_rate: header.sample_rate_hz,
        channels: u32::from(header.channel_count()),
        bitrate_kbps: header.bitrate_kbps,
        first_frame_offset: offset,
        toc: None,
        stream_bytes: None,
        delay: None,
        padding: None,
        frames: None,
    };

    let frame = header
        .frame_len()
        .and_then(|len| offset.checked_add(len))
        .and_then(|end| audio.get(offset..end));
    if let Some(vbr) = frame.and_then(|f| vbr_header(f, &header)) {
        props.toc = vbr.toc;
        props.stream_bytes = vbr.bytes;
        props.delay = vbr.delay;
        props.padding = vbr.padding;
        props.frames = Some(vbr.frames);
        let coded = u64::from(vbr.frames) * u64::from(header.samples_per_frame());
        let trim = u64::from(vbr.trim);
        let samples = if trim < coded { coded - trim } else { coded };
        let ns = u128::from(samples) * 1_000_000_000 / u128::from(header.sample_rate_hz);
        props.duration_ns = Some(ns.min(u128::from(u64::MAX)) as u64);
        props.exact = true;
        return Some(props);
    }

    if let Some(kbps) = header.bitrate_kbps.filter(|&k| k > 0) {
        let ns = u128::from(audio_len) * 8_000_000 / u128::from(kbps);
        props.duration_ns = Some(ns.min(u128::from(u64::MAX)) as u64);
    }
    Some(props)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Vbr {
    frames: u32,
    /// Encoder delay plus padding from the LAME extension, 0 without one.
    trim: u32,
    delay: Option<u32>,
    padding: Option<u32>,
    bytes: Option<u32>,
    toc: Option<XingToc>,
}

/// The VBR header inside `frame`, Xing/Info after the side info or VBRI at a fixed 32 bytes.
/// VBRI's delay is a byte count, so nothing is trimmed on that path.
fn vbr_header(frame: &[u8], header: &Header) -> Option<Vbr> {
    let side_info = match (header.lsf, header.channel_count()) {
        (false, 1) => 17,
        (false, _) => 32,
        (true, 1) => 9,
        (true, _) => 17,
    };
    let at = 4 + usize::from(header.crc) * 2 + side_info;
    if let Some(xing) = frame.get(at..)
        && (xing.starts_with(b"Xing") || xing.starts_with(b"Info"))
    {
        let flags = be32(xing.get(4..8)?);
        if flags & 0x1 == 0 {
            return None;
        }
        // Optional fields are present only when flagged, in flag order, so they are walked.
        let frames = be32(xing.get(8..12)?);
        let mut field = 12;
        let mut bytes = None;
        if flags & 0x2 != 0 {
            bytes = xing.get(field..field + 4).map(be32);
            field += 4;
        }
        let mut toc = None;
        if flags & 0x4 != 0 {
            toc = xing
                .get(field..field + XingToc::LEN)
                .and_then(|t| <[u8; XingToc::LEN]>::try_from(t).ok())
                .map(|points| XingToc { points });
            field += XingToc::LEN;
        }
        if flags & 0x8 != 0 {
            field += 4; // the quality indicator, which tells a player nothing
        }
        let lame = xing.get(field..).and_then(lame_trim);
        let trim = lame.map_or(0, |(delay, padding)| delay + padding);
        let (delay, padding) = (lame.map(|(delay, _)| delay), lame.map(|(_, padding)| padding));
        return Some(Vbr { frames, trim, delay, padding, bytes, toc });
    }
    let vbri = frame.get(36..)?;
    if !vbri.starts_with(b"VBRI") {
        return None;
    }
    Some(Vbr {
        frames: be32(vbri.get(14..18)?),
        trim: 0,
        delay: None,
        padding: None,
        bytes: vbri.get(10..14).map(be32),
        toc: None,
    })
}

/// Encoder delay and padding from the LAME extension at 0x15 (LAME "MP3 Info Tag" rev 1).
/// Only LAME, Lavc and Lavf are trusted, as in ffmpeg's `mp3_parse_info_tag`. The decoder's
/// 529-sample delay cancels at both ends, so it is not in the sum.
fn lame_trim(ext: &[u8]) -> Option<(u32, u32)> {
    let writer = ext.get(..4)?;
    if writer != b"LAME" && writer != b"Lavc" && writer != b"Lavf" {
        return None;
    }
    let d = ext.get(0x15..0x18)?;
    let delay = (u32::from(d[0]) << 4) | u32::from(d[1] >> 4);
    let padding = (u32::from(d[1] & 0x0F) << 8) | u32::from(d[2]);
    Some((delay, padding))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagFrame {
    Need,
    Not,
    Frame(usize),
}

pub const TAG_FRAME_PROBE: usize = 54;

/// kbit/s of layer III by `lsf` and `bitrate_index` (FFmpeg `ff_mpa_bitrate_tab`).
const L3_KBPS: [[u16; 15]; 2] = [
    [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

/// The VBR tag frame at the front of `audio`, as FFmpeg 8.1.2's `mp3_parse_vbr_tags` finds it.
/// `end` when the stream ends inside the probe, past bytes read as zeros as `avio` does.
pub fn tag_frame(audio: &[u8], end: bool) -> TagFrame {
    if audio.len() < TAG_FRAME_PROBE && !end {
        return TagFrame::Need;
    }
    let mut b = [0u8; TAG_FRAME_PROBE];
    let n = audio.len().min(TAG_FRAME_PROBE);
    b[..n].copy_from_slice(&audio[..n]);
    let h = be32(&b[..4]);
    // `ff_mpa_check_header`.
    let bad = h & 0xFFE0_0000 != 0xFFE0_0000
        || h & (3 << 19) == 1 << 19
        || h & (3 << 17) == 0
        || h & (0xF << 12) == 0xF << 12
        || h & (3 << 10) == 3 << 10;
    let layer = 4 - ((h >> 17) & 3);
    let index = ((h >> 12) & 0xF) as usize;
    if bad || layer != 3 || index == 0 {
        return TagFrame::Not;
    }
    let (lsf, mpeg25) = if h & (1 << 20) != 0 { (h & (1 << 19) == 0, false) } else { (true, true) };
    let rate = [44_100u32, 48_000, 32_000][((h >> 10) & 3) as usize]
        >> (u32::from(lsf) + u32::from(mpeg25));
    let padding = (h >> 9) & 1;
    let size =
        u32::from(L3_KBPS[usize::from(lsf)][index]) * 144_000 / (rate << u32::from(lsf)) + padding;
    let mono = (h >> 6) & 3 == 3;
    let side = [[32, 17], [17, 9]][usize::from(lsf)][usize::from(mono)];
    let xing = 4 + side;
    let (mut frames, mut bytes) = (0, 0);
    if &b[xing..xing + 4] == b"Xing" || &b[xing..xing + 4] == b"Info" {
        let flags = be32(&b[xing + 4..]);
        let mut at = xing + 8;
        if flags & 1 != 0 {
            frames = be32(&b[at..]);
            at += 4;
        }
        if flags & 2 != 0 {
            bytes = be32(&b[at..]);
        }
    }
    if &b[36..40] == b"VBRI" && u16::from_be_bytes([b[40], b[41]]) == 1 {
        (bytes, frames) = (be32(&b[46..]), be32(&b[50..]));
    }
    match frames != 0 || bytes != 0 {
        true => TagFrame::Frame(size as usize),
        false => TagFrame::Not,
    }
}

fn be32(b: &[u8]) -> u32 {
    b.get(..4).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}
