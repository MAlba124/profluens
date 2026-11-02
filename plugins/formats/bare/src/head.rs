//! What a bare file is, read once at open from its head and its tail through the caller's
//! callback. A FLAC file's metadata blocks, an MPEG audio file's Xing frame, an ADTS file's
//! first frames, and the trailing ID3v1 and APEv2 tags that the audio ends before.

// Cold, the opener runs once per file.
#![allow(clippy::disallowed_methods)]

use crate::flac::{self, MAX_HEADER, STREAMINFO_LEN, StreamInfo};
use crate::reader::header;
use crate::{BareError, ReadAt};
use pf_mp3::frame::Header as MpaHeader;
use std::ops::Range;

/// The head read at open, the metadata or the ID3v2 tag and the first frames.
const HEAD: u64 = 128 << 10;
/// Metadata blocks or an ID3v2 tag past this are not read.
const MAX_META: u64 = 64 << 20;
/// The bytes the tail read covers, an ID3v1 tag and an APEv2 footer before it.
const TAIL: u64 = 160;
/// An APEv2 tag past this is not read.
const MAX_APE: u64 = 1 << 20;
/// FLAC metadata blocks read at open, a real file has a handful.
const MAX_BLOCKS: usize = 1024;
/// A FLAC PICTURE block past this is not read, a 16 MiB image and its fields.
const MAX_PICTURE_BLOCK: u64 = (16 << 20) + (64 << 10);
/// ADTS sampling frequencies by index, ISO/IEC 14496-3 1.6.3.4.
const ADTS_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// A bare file by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareKind {
    Flac,
    Mp3,
    Adts,
}

/// What frames of the stream must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Flac { si: StreamInfo, variable: bool },
    Mpa { stream: MpaHeader },
    Adts { profile: u8, sfi: u8, channels: u8 },
}

/// What is kept of a bare file, read once at open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BareHead {
    pub codec: Codec,
    pub family: &'static str,
    /// The codec head a run opens with, empty for MPEG audio.
    pub head: Vec<u8>,
    /// The first audio frame, past tags and the Xing frame.
    pub first: u64,
    /// Audio ends here, trailing tags cut off.
    pub end: u64,
    pub rate: u32,
    /// Samples per frame, 0 for FLAC.
    pub spf: u32,
    /// Samples in the stream, 0 unknown.
    pub samples: u64,
    pub duration_ns: i64,
    /// Seek points, sample and byte, of a SEEKTABLE or a Xing table, ascending.
    pub points: Vec<(u64, u64)>,
    /// Bytes per frame as a fraction when every frame is of one bitrate.
    pub cbr: Option<(u64, u64)>,
    /// LAME encoder delay and padding in samples, the gapless trim, not applied to stamps.
    pub gapless: (u32, u32),
}

impl BareHead {
    pub fn ns(&self, sample: u64) -> i64 {
        (u128::from(sample) * 1_000_000_000 / u128::from(self.rate.max(1))).min(i64::MAX as u128)
            as i64
    }

    /// The sample at `ns`, rounded, so the stamp of a sample gives that sample back.
    pub(crate) fn sample_at(&self, ns: i64) -> u64 {
        ((u128::from(ns.max(0) as u64) * u128::from(self.rate) + 500_000_000) / 1_000_000_000)
            as u64
    }

    pub fn channels(&self) -> u8 {
        match self.codec {
            Codec::Flac { si, .. } => si.channels,
            Codec::Mpa { stream } => stream.channel_count(),
            Codec::Adts { channels, .. } => channels,
        }
    }

    /// Bits a sample, 16 for the lossy codecs.
    pub fn depth(&self) -> u8 {
        match self.codec {
            Codec::Flac { si, .. } => si.bits,
            Codec::Mpa { .. } | Codec::Adts { .. } => 16,
        }
    }

    /// Bits a second, on the line of a CBR stream, else the audio over the duration, 0 unknown.
    pub fn bitrate(&self) -> u32 {
        match (self.cbr, self.duration_ns) {
            (Some((num, den)), _) => {
                (u128::from(num) * u128::from(self.rate) * 8
                    / u128::from(den.max(1))
                    / u128::from(self.spf.max(1))) as u32
            }
            (None, d) if d > 0 => {
                (u128::from(self.end.saturating_sub(self.first)) * 8_000_000_000 / d as u128) as u32
            }
            _ => 0,
        }
    }
}

/// The tag bytes an opener read on the way, for the caller to parse. Spans index `head`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawTags {
    /// The file from byte 0 as far as the opener read it.
    pub head: Vec<u8>,
    /// The ID3v2 tag is `head[..id3v2]`.
    pub id3v2: usize,
    /// A trailing ID3v1 tag, 128 bytes or none.
    pub v1: Vec<u8>,
    /// A trailing APEv2 tag, none past 1 MiB.
    pub ape: Vec<u8>,
    /// FLAC VORBIS_COMMENT block bodies.
    pub comments: Vec<Range<usize>>,
    /// FLAC PICTURE block bodies, an oversized one left out.
    pub pictures: Vec<Range<usize>>,
}

impl RawTags {
    pub fn id3v2(&self) -> &[u8] {
        self.head.get(..self.id3v2).unwrap_or(&[])
    }

    /// The bytes of a span of `head`.
    pub fn span(&self, r: &Range<usize>) -> &[u8] {
        self.head.get(r.clone()).unwrap_or(&[])
    }
}

/// An ADTS header, profile, frequency index, channels, frame length, header length and
/// samples, ISO/IEC 14496-3 1.A.2.2.
pub(crate) fn adts(b: &[u8]) -> Option<(u8, u8, u8, u64, u64, u32)> {
    let b = b.get(..7)?;
    if b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return None;
    }
    let skip = if b[1] & 1 == 1 { 7 } else { 9 };
    let (profile, sfi) = (b[2] >> 6, (b[2] >> 2) & 0xF);
    let ch = ((b[2] & 1) << 2) | (b[3] >> 6);
    let len = (u64::from(b[3] & 3) << 11) | (u64::from(b[4]) << 3) | u64::from(b[5] >> 5);
    let blocks = u32::from(b[6] & 3) + 1;
    (sfi < 13 && len > skip).then_some((profile, sfi, ch, len, skip, blocks * 1024))
}

/// Appends up to `n` bytes of the file from `off` to `buf`, fewer at its end.
fn append(read: &mut ReadAt<'_>, off: u64, buf: &mut Vec<u8>, n: usize) -> Result<(), BareError> {
    let start = buf.len();
    buf.resize(start + n, 0);
    let mut got = 0;
    while got < n {
        let Some(at) = off.checked_add(got as u64) else { break };
        let k = read(at, &mut buf[start + got..]).map_err(|e| BareError::Io(e.kind()))?;
        if k == 0 {
            break;
        }
        got += k.min(n - got);
    }
    buf.truncate(start + got);
    Ok(())
}

/// Grows `head`, the file from byte 0, to `to` bytes in one read.
fn extend(read: &mut ReadAt<'_>, head: &mut Vec<u8>, to: u64, len: u64) -> Result<(), BareError> {
    let to = to.min(len);
    if to as usize <= head.len() {
        return Ok(());
    }
    if to > MAX_META {
        return Err(BareError::TooLarge);
    }
    append(read, head.len() as u64, head, to as usize - head.len())
}

/// Reads the trailing ID3v1 and APEv2 tags into `tags`, where the audio ends.
fn trailers(
    read: &mut ReadAt<'_>,
    len: u64,
    from: u64,
    tags: &mut RawTags,
) -> Result<u64, BareError> {
    let from = from.min(len);
    let at = len.saturating_sub(TAIL).max(from);
    let mut tail = Vec::new();
    append(read, at, &mut tail, (len - at) as usize)?;
    let v1 =
        if tail.len() >= 128 && tail[tail.len() - 128..].starts_with(b"TAG") { 128 } else { 0 };
    let before = &tail[..tail.len() - v1];
    let ape = pf_mp3::id3::ape_tail_len(before)
        .map_or(0, |n| n as u64)
        .min(len - from - (v1 as u64).min(len - from));
    let cut = v1 as u64 + ape;
    if ape > 0 && ape <= MAX_APE {
        append(read, len - cut, &mut tags.ape, ape as usize)?;
    }
    tail.drain(..tail.len() - v1);
    tags.v1 = tail;
    Ok(len - cut)
}

/// Opens a bare FLAC, MP3 or ADTS file of `len` bytes through `read`, the head, the trailing
/// tags, and the tag bytes the file holds.
pub fn open(
    read: &mut ReadAt<'_>,
    len: u64,
    kind: BareKind,
) -> Result<(BareHead, RawTags), BareError> {
    let mut tags = RawTags::default();
    extend(read, &mut tags.head, HEAD, len)?;
    let tag = pf_mp3::id3::v2_total_len(&tags.head).map_or(0, |n| n as u64);
    let h = match kind {
        BareKind::Flac => open_flac(read, len, tag, &mut tags)?,
        BareKind::Mp3 | BareKind::Adts => open_frames(read, len, tag, kind, &mut tags)?,
    };
    Ok((h, tags))
}

fn open_flac(
    read: &mut ReadAt<'_>,
    len: u64,
    tag: u64,
    tags: &mut RawTags,
) -> Result<BareHead, BareError> {
    extend(read, &mut tags.head, tag + 8, len)?;
    if tags.head.get(tag as usize..tag as usize + 4) != Some(b"fLaC") {
        return Err(BareError::NoStreams);
    }
    tags.id3v2 = tag as usize;
    let (mut si, mut si_bytes, mut seek) = (None, [0u8; STREAMINFO_LEN], Vec::new());
    let mut p = tag + 4;
    for blocks in 0.. {
        if blocks == MAX_BLOCKS {
            return Err(BareError::Malformed(p));
        }
        extend(read, &mut tags.head, p + 4, len)?;
        let Some(b) = tags.head.get(p as usize..p as usize + 4) else {
            return Err(BareError::Malformed(p));
        };
        let (last, kind) = (b[0] & 0x80 != 0, b[0] & 0x7F);
        let size = u64::from(u32::from_be_bytes([0, b[1], b[2], b[3]]));
        let body = p + 4;
        // padding, application blocks and an oversized picture are not read, the rest are
        if matches!(kind, 0 | 3 | 4 | 6) && size <= MAX_PICTURE_BLOCK {
            extend(read, &mut tags.head, body + size + 4, len)?;
            let span = body as usize..(body + size) as usize;
            let Some(d) = tags.head.get(span.clone()) else {
                return Err(BareError::Malformed(body));
            };
            match kind {
                0 if d.len() >= STREAMINFO_LEN => {
                    si_bytes.copy_from_slice(&d[..STREAMINFO_LEN]);
                    si = flac::stream_info(d);
                }
                3 => seek = flac::seek_points(d),
                4 => tags.comments.push(span),
                6 => tags.pictures.push(span),
                _ => {}
            }
        }
        p = body + size;
        if last || p >= len {
            break;
        }
    }
    let si = si.ok_or(BareError::BadHead("flac"))?;
    let first = p;
    let end = trailers(read, len, first, tags)?;
    extend(read, &mut tags.head, first + MAX_HEADER as u64, len)?;
    let f = tags
        .head
        .get(first as usize..)
        .and_then(|b| flac::frame(b, &si))
        .ok_or(BareError::Malformed(first))?;
    let h = BareHead {
        codec: Codec::Flac { si, variable: f.variable },
        family: "flac",
        head: flac::codec_head(&si_bytes),
        first,
        end,
        rate: si.rate,
        spf: 0,
        samples: si.total,
        duration_ns: 0,
        points: seek
            .iter()
            .filter_map(|&(s, o)| Some((s, first.checked_add(o)?)))
            .filter(|p| p.1 < end)
            .collect(),
        cbr: None,
        gapless: (0, 0),
    };
    let duration_ns = h.ns(si.total);
    Ok(BareHead { duration_ns, ..h })
}

fn open_frames(
    read: &mut ReadAt<'_>,
    len: u64,
    tag: u64,
    kind: BareKind,
    tags: &mut RawTags,
) -> Result<BareHead, BareError> {
    // the whole ID3v2 tag for its text and pictures, and 8 KiB of audio behind it
    extend(read, &mut tags.head, tag + pf_mp3::index::WINDOW as u64, len)?;
    // a tag that claims more than the file holds ends with it
    let tag = tag.min(tags.head.len() as u64);
    tags.id3v2 = tag as usize;
    let end = trailers(read, len, tag, tags)?;
    let audio = tags.head.get(tag as usize..).unwrap_or(&[]);
    match kind {
        BareKind::Adts => open_adts(audio, tag, end),
        _ => open_mp3(audio, tag, end),
    }
}

fn open_mp3(audio: &[u8], tag: u64, end: u64) -> Result<BareHead, BareError> {
    let (_, layout) = pf_mp3::index::head_of(audio, tag, Some(end));
    let mut l = layout.ok_or(BareError::NoStreams)?;
    l.cbr = l.cbr.filter(|&(num, den)| num > 0 && den > 0);
    // a Xing frame that runs past the audio leaves none
    if l.audio > end {
        return Err(BareError::Malformed(l.first));
    }
    let props = pf_mp3::probe_props(audio, end - tag).ok_or(BareError::NoStreams)?;
    if l.stream.frame_len().is_none() {
        return Err(BareError::Unsupported("mp3 free format"));
    }
    let spf = u64::from(l.spf);
    let span = end.saturating_sub(l.audio);
    let frames = match (props.frames, l.cbr) {
        (Some(f), _) => u64::from(f),
        (None, Some((num, den))) => (u128::from(span) * u128::from(den) / u128::from(num)) as u64,
        (None, None) => 0,
    };
    let samples = frames * spf;
    let duration_ns = match (samples, props.duration_ns) {
        (0, Some(d)) => d.min(i64::MAX as u64) as i64,
        (s, _) => {
            (u128::from(s) * 1_000_000_000 / u128::from(l.rate.max(1))).min(i64::MAX as u128) as i64
        }
    };
    let bytes = props
        .stream_bytes
        .map_or(end.saturating_sub(l.first), u64::from)
        .min(end.saturating_sub(l.first));
    let mut points = Vec::new();
    if let (Some(toc), true) = (props.toc, frames > 0 && l.cbr.is_none()) {
        for i in 0..pf_mp3::XingToc::LEN {
            let s = (i as u64 * frames / 100) * spf;
            let b = l.first.saturating_add(toc.byte_at(i, bytes).unwrap_or(0));
            if points.last().is_none_or(|p: &(u64, u64)| s > p.0 && b > p.1) && b < end {
                points.push((s, b));
            }
        }
    }
    // the LAME delay and padding of the Xing or Info frame, as ffmpeg reads them
    let gapless = match (l.header, props.delay, props.padding) {
        (true, Some(d), Some(p)) => (d, p),
        _ => (0, 0),
    };
    Ok(BareHead {
        codec: Codec::Mpa { stream: l.stream },
        family: "mp3",
        head: Vec::new(),
        first: l.audio,
        end,
        rate: l.rate,
        spf: l.spf,
        samples,
        duration_ns,
        points,
        cbr: l.cbr,
        gapless,
    })
}

fn open_adts(audio: &[u8], tag: u64, end: u64) -> Result<BareHead, BareError> {
    let at = audio
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] & 0xF6 == 0xF0)
        .ok_or(BareError::NoStreams)?;
    let (profile, sfi, channels, _, _, _) = adts(&audio[at..]).ok_or(BareError::NoStreams)?;
    if channels == 0 {
        return Err(BareError::Unsupported("aac with a PCE"));
    }
    let codec = Codec::Adts { profile, sfi, channels };
    let rate = ADTS_RATES[usize::from(sfi)];
    // the frames the head holds give the average size
    let (mut p, mut n, mut bytes) = (at, 0u64, 0u64);
    while let Some(hd) = audio.get(p..).and_then(|b| header(&codec, b)) {
        if p as u64 + hd.len > audio.len() as u64 {
            break;
        }
        (p, n, bytes) = (p + hd.len as usize, n + 1, bytes + hd.len);
    }
    if n == 0 {
        return Err(BareError::NoStreams);
    }
    let first = tag + at as u64;
    let frames = u128::from(end.saturating_sub(first)) * u128::from(n) / u128::from(bytes);
    let samples = (frames * 1024) as u64;
    let aot = profile + 1;
    let asc = vec![(aot << 3) | (sfi >> 1), ((sfi & 1) << 7) | (channels << 3)];
    let h = BareHead {
        codec,
        family: "aac",
        head: asc,
        first,
        end,
        rate,
        spf: 1024,
        samples,
        duration_ns: 0,
        points: Vec::new(),
        cbr: None,
        gapless: (0, 0),
    };
    Ok(BareHead { duration_ns: h.ns(samples), ..h })
}
