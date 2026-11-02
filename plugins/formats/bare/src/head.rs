//! What a bare file is, read once at open from its head and its tail, [`OpenAt`] asks the caller
//! for the bytes. A FLAC file's metadata blocks, an MPEG audio file's Xing frame, an ADTS file's
//! first frames, and the trailing ID3v1 and APEv2 tags that the audio ends before.

// Cold, the opener runs once per file.
#![allow(clippy::disallowed_methods)]

use crate::BareError;
use crate::flac::{self, MAX_HEADER, STREAMINFO_LEN, StreamInfo};
use crate::reader::header;
use pf_mp3::frame::Header as MpaHeader;
use profluens_core::fetch::{Fetch, Need, Step};
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

/// The read in flight of [`OpenAt`], or the step it runs next when it needs none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenState {
    Start,
    /// The file's first [`HEAD`] bytes.
    Head,
    /// FLAC, the bytes through the `fLaC` magic.
    Magic,
    /// FLAC, the header of the `n`th metadata block at `p`.
    Block {
        p: u64,
        n: usize,
    },
    /// FLAC, the body of the block at `p` and the header behind it.
    Body {
        p: u64,
        n: usize,
    },
    /// MPEG audio and ADTS, the ID3v2 tag and a window of audio behind it.
    Window,
    /// The tail, the audio from `from` on.
    Tail {
        from: u64,
    },
    /// An APEv2 tag that ends `cut` bytes before the end, before an ID3v1 tag of `v1`.
    Ape {
        cut: u64,
        v1: usize,
    },
    /// FLAC, the first frame's header, the audio ending at `end`.
    Frame {
        end: u64,
    },
}

/// Opens a bare FLAC, MP3 or ADTS file of `len` bytes, the head, the trailing tags, and the tag
/// bytes the file holds. The head grows a read at a time, the tail is read once.
#[derive(Debug)]
pub struct OpenAt {
    len: u64,
    kind: BareKind,
    state: OpenState,
    tags: RawTags,
    /// The tail read, the ID3v1 tag once the audio's end is known.
    tail: Vec<u8>,
    /// Where the read in flight starts in its buffer.
    from: usize,
    /// The ID3v2 tag's length.
    tag: u64,
    si: Option<StreamInfo>,
    si_bytes: [u8; STREAMINFO_LEN],
    seek: Vec<(u64, u64)>,
    first: u64,
    /// The file's first bytes read already, the head the first run starts from.
    seed: Vec<u8>,
}

impl OpenAt {
    pub fn new(len: u64, kind: BareKind) -> Self {
        OpenAt::with_head(len, kind, Vec::new())
    }

    /// Starts from `head`, the file's first bytes read already, and reads only past it. Consumed
    /// by the first run.
    pub fn with_head(len: u64, kind: BareKind, mut head: Vec<u8>) -> Self {
        head.truncate(usize::try_from(len).unwrap_or(usize::MAX));
        OpenAt {
            seed: head,
            len,
            kind,
            state: OpenState::Start,
            tags: RawTags::default(),
            tail: Vec::new(),
            from: 0,
            tag: 0,
            si: None,
            si_bytes: [0; STREAMINFO_LEN],
            seek: Vec::new(),
            first: 0,
        }
    }

    /// Grows the head, the file from byte 0, to `to` bytes in one read, then runs `state`. The
    /// read, None when the head holds them.
    fn extend(&mut self, state: OpenState, to: u64) -> Result<Option<Need>, BareError> {
        self.state = state;
        let (to, have) = (to.min(self.len), self.tags.head.len());
        if to as usize <= have {
            return Ok(None);
        }
        if to > MAX_META {
            return Err(BareError::TooLarge);
        }
        self.tags.head.resize(to as usize, 0);
        self.from = have;
        Ok(Some(Need { at: have as u64, len: to as usize - have }))
    }

    /// The metadata block at `p`, the `n`th.
    fn block(&mut self, p: u64, n: usize) -> Result<Option<Need>, BareError> {
        if n == MAX_BLOCKS {
            return Err(BareError::Malformed(p));
        }
        self.extend(OpenState::Block { p, n }, p + 4)
    }

    /// The block at `p` read, the next one or the audio behind the last.
    fn after_block(&mut self, p: u64, n: usize) -> Result<Option<Need>, BareError> {
        let (last, _, size) = self.block_head(p)?;
        let p = p + 4 + size;
        if !(last || p >= self.len) {
            return self.block(p, n + 1);
        }
        if self.si.is_none() {
            return Err(BareError::BadHead("flac"));
        }
        self.first = p;
        Ok(self.tail(p))
    }

    /// Whether the block at `p` is the last, its type and its size.
    fn block_head(&self, p: u64) -> Result<(bool, u8, u64), BareError> {
        let Some(b) = self.tags.head.get(p as usize..p as usize + 4) else {
            return Err(BareError::Malformed(p));
        };
        Ok((b[0] & 0x80 != 0, b[0] & 0x7F, u64::from(u32::from_be_bytes([0, b[1], b[2], b[3]]))))
    }

    /// The trailing ID3v1 and APEv2 tags of the audio from `from` on, in the tail read.
    fn tail(&mut self, from: u64) -> Option<Need> {
        let from = from.min(self.len);
        let at = self.len.saturating_sub(TAIL).max(from);
        self.state = OpenState::Tail { from };
        self.tail.clear();
        self.tail.resize((self.len - at) as usize, 0);
        self.from = 0;
        // a head that holds the whole file holds the tail too
        if let Some(b) = self.tags.head.get(at as usize..).filter(|b| b.len() == self.tail.len()) {
            self.tail.copy_from_slice(b);
            return None;
        }
        (at < self.len).then_some(Need { at, len: (self.len - at) as usize })
    }

    /// The trailing tags read, the audio ends `cut` bytes before the file does.
    fn after_tail(&mut self, cut: u64, v1: usize) -> Result<Step<(BareHead, RawTags)>, BareError> {
        let (len, tag) = (self.len, self.tag);
        self.tail.drain(..self.tail.len() - v1);
        self.tags.v1 = std::mem::take(&mut self.tail);
        let end = len - cut;
        let h = match self.kind {
            BareKind::Flac => {
                if let Some(need) =
                    self.extend(OpenState::Frame { end }, self.first + MAX_HEADER as u64)?
                {
                    return Ok(Step::Need(need));
                }
                return self.flac(end);
            }
            BareKind::Adts => {
                open_adts(self.tags.head.get(tag as usize..).unwrap_or(&[]), tag, end)?
            }
            BareKind::Mp3 => open_mp3(self.tags.head.get(tag as usize..).unwrap_or(&[]), tag, end)?,
        };
        self.state = OpenState::Start;
        Ok(Step::Done((h, std::mem::take(&mut self.tags))))
    }

    /// The FLAC head once the first frame's header is read.
    fn flac(&mut self, end: u64) -> Result<Step<(BareHead, RawTags)>, BareError> {
        let (first, si) = (self.first, self.si.unwrap_or_default());
        let f = self
            .tags
            .head
            .get(first as usize..)
            .and_then(|b| flac::frame(b, &si))
            .ok_or(BareError::Malformed(first))?;
        let h = BareHead {
            codec: Codec::Flac { si, variable: f.variable },
            family: "flac",
            head: flac::codec_head(&self.si_bytes),
            first,
            end,
            rate: si.rate,
            spf: 0,
            samples: si.total,
            duration_ns: 0,
            points: self
                .seek
                .iter()
                .filter_map(|&(s, o)| Some((s, first.checked_add(o)?)))
                .filter(|p| p.1 < end)
                .collect(),
            cbr: None,
            gapless: (0, 0),
        };
        let duration_ns = h.ns(si.total);
        self.state = OpenState::Start;
        Ok(Step::Done((BareHead { duration_ns, ..h }, std::mem::take(&mut self.tags))))
    }

    fn step(&mut self) -> Result<Step<(BareHead, RawTags)>, BareError> {
        loop {
            let need = match self.state {
                OpenState::Start => {
                    (self.tags, self.si, self.seek) = (RawTags::default(), None, Vec::new());
                    self.tags.head = std::mem::take(&mut self.seed);
                    self.extend(OpenState::Head, HEAD)?
                }
                OpenState::Head => {
                    self.tag = pf_mp3::id3::v2_total_len(&self.tags.head).map_or(0, |n| n as u64);
                    match self.kind {
                        BareKind::Flac => self.extend(OpenState::Magic, self.tag + 8)?,
                        // the whole ID3v2 tag for its text and pictures, and 8 KiB of audio
                        // behind it
                        _ => {
                            self.extend(OpenState::Window, self.tag + pf_mp3::index::WINDOW as u64)?
                        }
                    }
                }
                OpenState::Magic => {
                    let tag = self.tag as usize;
                    if self.tags.head.get(tag..tag + 4) != Some(b"fLaC") {
                        return Err(BareError::NoStreams);
                    }
                    self.tags.id3v2 = tag;
                    self.block(self.tag + 4, 0)?
                }
                OpenState::Block { p, n } => {
                    let (_, kind, size) = self.block_head(p)?;
                    let body = p + 4;
                    // padding, application blocks and an oversized picture are not read, the
                    // rest are
                    match matches!(kind, 0 | 3 | 4 | 6) && size <= MAX_PICTURE_BLOCK {
                        true => self.extend(OpenState::Body { p, n }, body + size + 4)?,
                        false => self.after_block(p, n)?,
                    }
                }
                OpenState::Body { p, n } => {
                    let (_, kind, size) = self.block_head(p)?;
                    let body = p + 4;
                    let span = body as usize..(body + size) as usize;
                    let Some(d) = self.tags.head.get(span.clone()) else {
                        return Err(BareError::Malformed(body));
                    };
                    match kind {
                        0 if d.len() >= STREAMINFO_LEN => {
                            self.si_bytes.copy_from_slice(&d[..STREAMINFO_LEN]);
                            self.si = flac::stream_info(d);
                        }
                        3 => self.seek = flac::seek_points(d),
                        4 => self.tags.comments.push(span),
                        6 => self.tags.pictures.push(span),
                        _ => {}
                    }
                    self.after_block(p, n)?
                }
                OpenState::Window => {
                    // a tag that claims more than the file holds ends with it
                    self.tag = self.tag.min(self.tags.head.len() as u64);
                    self.tags.id3v2 = self.tag as usize;
                    self.tail(self.tag)
                }
                OpenState::Tail { from } => {
                    let (len, tail) = (self.len, &self.tail);
                    let v1 = if tail.len() >= 128 && tail[tail.len() - 128..].starts_with(b"TAG") {
                        128
                    } else {
                        0
                    };
                    let before = &tail[..tail.len() - v1];
                    let ape = pf_mp3::id3::ape_tail_len(before)
                        .map_or(0, |n| n as u64)
                        .min(len - from - (v1 as u64).min(len - from));
                    let cut = v1 as u64 + ape;
                    if ape == 0 || ape > MAX_APE {
                        return self.after_tail(cut, v1);
                    }
                    self.state = OpenState::Ape { cut, v1 };
                    self.tags.ape.clear();
                    self.tags.ape.resize(ape as usize, 0);
                    self.from = 0;
                    Some(Need { at: len - cut, len: ape as usize })
                }
                OpenState::Ape { cut, v1 } => return self.after_tail(cut, v1),
                OpenState::Frame { end } => return self.flac(end),
            };
            if let Some(need) = need {
                return Ok(Step::Need(need));
            }
        }
    }
}

impl Fetch for OpenAt {
    type Out = (BareHead, RawTags);
    type Err = BareError;

    fn poll(&mut self) -> Result<Step<(BareHead, RawTags)>, BareError> {
        let step = self.step();
        if step.is_err() {
            self.state = OpenState::Start;
        }
        step
    }

    fn buf(&mut self) -> &mut [u8] {
        let b = match self.state {
            OpenState::Tail { .. } => &mut self.tail,
            OpenState::Ape { .. } => &mut self.tags.ape,
            _ => &mut self.tags.head,
        };
        let from = self.from.min(b.len());
        &mut b[from..]
    }

    fn filled(&mut self, n: usize) {
        let b = match self.state {
            OpenState::Tail { .. } => &mut self.tail,
            OpenState::Ape { .. } => &mut self.tags.ape,
            _ => &mut self.tags.head,
        };
        let from = self.from.min(b.len());
        b.truncate(from + n.min(b.len() - from));
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
