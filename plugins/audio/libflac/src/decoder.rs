//! Incremental FLAC decoder. Frame headers are found here by sync code and CRC-8 (RFC 9639
//! section 9.1) and libFLAC decodes each frame alone, so the output does not depend on how
//! the input was cut. Corrupt frames are dropped, never returned as errors.

use libflac_sys as sys;
use pf_tags::flacmeta::{self, FlacTags, SeekPoint};

use crate::libflac::{Decoder, Init};

/// STREAMINFO (RFC 9639 section 8.2). The MD5 is never checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub min_block_size: u32,
    pub max_block_size: u32,
    pub min_frame_size: u32,
    pub max_frame_size: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub total_samples: u64,
    pub md5: [u8; 16],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    BadMagic,
    BadStreamInfo,
    UnexpectedEof,
    TooLarge(&'static str),
    Memory,
    Init(i32),
    State(i32),
}

/// STREAMINFO's maximum frame size is a 24-bit byte count (section 8.2).
pub(crate) const MAX_FRAME_BYTES: usize = (1 << 24) - 1;

/// Four maximal metadata blocks (section 8.1).
pub(crate) const MAX_METADATA_BYTES: usize = 4 << 24;

/// Consumed input is dropped once this much has gathered and it is half the buffer.
const DRAIN_AT: usize = 64 * 1024;

const STREAMINFO_HEAD: [u8; 3] = [0, 0, 34];

const ID3_HEAD: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Lead,
    Tag(u64),
    Head,
    Chain,
    Frames,
    Failed(DecodeError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sync {
    Header,
    No,
    Short,
}

/// An incremental FLAC decoder whose memory stays on the order of a frame.
#[derive(Default)]
pub struct StreamDecoder {
    lib: Option<Decoder>,
    phase: Phase,
    tags: FlacTags,
    points: Vec<SeekPoint>,
    audio_start: Option<u64>,
    last: Option<(u64, u64)>,
    lost: bool,
    pos: usize,
    starved: Option<usize>,
    junk: bool,
    errors: u64,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn info(&self) -> Option<&StreamInfo> {
        self.lib.as_ref().and_then(|l| l.feed().info.as_ref())
    }

    pub fn tags(&self) -> &FlacTags {
        &self.tags
    }

    pub fn seek_table(&self) -> &[SeekPoint] {
        &self.points
    }

    pub fn audio_start(&self) -> Option<u64> {
        self.audio_start
    }

    pub fn last_frame(&self) -> Option<(u64, u64)> {
        self.last
    }

    /// Skipped runs, failed frames and chain faults. Zero for an undamaged stream.
    pub fn errors(&self) -> u64 {
        self.errors
    }

    pub fn push(&mut self, mut bytes: &[u8]) {
        if self.lib.is_none() && !matches!(self.phase, Phase::Failed(_)) {
            match Decoder::new() {
                Ok(lib) => self.lib = Some(lib),
                Err(Init::Alloc) => self.phase = Phase::Failed(DecodeError::Memory),
                Err(Init::Status(s)) => self.phase = Phase::Failed(DecodeError::Init(s)),
            }
        }
        let Some(lib) = &mut self.lib else { return };
        let feed = lib.feed_mut();
        if let Phase::Tag(left) = &mut self.phase
            && self.pos == feed.buf.len()
        {
            let n = usize::try_from(*left).unwrap_or(usize::MAX).min(bytes.len());
            feed.base += (feed.buf.len() + n) as u64;
            feed.buf.clear();
            (feed.fed, self.pos) = (0, 0);
            *left -= n as u64;
            bytes = &bytes[n..];
            if *left == 0 {
                self.phase = Phase::Head;
            }
        }
        feed.buf.extend_from_slice(bytes);
    }

    pub fn buffered(&self) -> &[u8] {
        self.lib.as_ref().and_then(|l| l.feed().buf.get(self.pos..)).unwrap_or_default()
    }

    pub fn restart_at(&mut self, byte: u64) {
        if let Some(lib) = &mut self.lib {
            lib.feed_mut().base = byte.saturating_sub(self.pos as u64);
        }
        self.lost = false;
    }

    pub fn seek_reset(&mut self) {
        if let Some(lib) = &mut self.lib {
            if matches!(self.phase, Phase::Chain | Phase::Frames) {
                lib.flush();
                self.phase = Phase::Frames;
            }
            let feed = lib.feed_mut();
            feed.buf.clear();
            feed.fed = 0;
            feed.base = 0;
            feed.limit = usize::MAX;
        }
        self.pos = 0;
        self.starved = None;
        self.junk = false;
        self.lost = true;
        self.last = None;
    }

    /// Decode the next frame into `out`, interleaved. `Ok(None)` when more bytes are needed.
    pub fn pull_into(&mut self, out: &mut Vec<i32>) -> Result<Option<()>, DecodeError> {
        out.clear();
        if let Phase::Failed(e) = self.phase {
            return Err(e);
        }
        match self.step(out) {
            Ok(got) => Ok(got),
            Err(e) => {
                self.phase = Phase::Failed(e);
                Err(e)
            }
        }
    }

    fn step(&mut self, out: &mut Vec<i32>) -> Result<Option<()>, DecodeError> {
        loop {
            let before = self.phase;
            match self.phase {
                Phase::Lead => self.lead()?,
                Phase::Tag(left) => self.tag(left),
                Phase::Head => self.head()?,
                Phase::Chain => self.chain()?,
                Phase::Frames => return self.frame(out),
                Phase::Failed(e) => return Err(e),
            }
            if self.phase == before {
                return Ok(None);
            }
        }
    }

    fn lead(&mut self) -> Result<(), DecodeError> {
        let head = self.buffered();
        if head.len() < 3 && b"ID3".starts_with(head) {
            return Ok(());
        }
        if !head.starts_with(b"ID3") {
            self.phase = Phase::Head;
            return Ok(());
        }
        if head.len() < ID3_HEAD {
            return Ok(());
        }
        let len = flacmeta::id3v2_len(head).ok_or(DecodeError::BadMagic)?;
        self.phase = Phase::Tag(len as u64);
        Ok(())
    }

    fn tag(&mut self, left: u64) {
        let n = usize::try_from(left).unwrap_or(usize::MAX).min(self.buffered().len());
        self.pos += n;
        let left = left - n as u64;
        self.phase = if left == 0 { Phase::Head } else { Phase::Tag(left) };
        self.compact();
    }

    // Cold, once a stream, the tags and SEEKTABLE are kept.
    #[allow(clippy::disallowed_methods)]
    fn head(&mut self) -> Result<(), DecodeError> {
        let Some(lib) = &self.lib else { return Ok(()) };
        let head = lib.feed().buf.get(self.pos..).unwrap_or_default();
        if head.len() >= 4 && &head[..4] != b"fLaC" {
            return Err(DecodeError::BadMagic);
        }
        if head.len() >= 8 && (head[4] & 0x7F != 0 || head[5..8] != STREAMINFO_HEAD) {
            return Err(DecodeError::BadStreamInfo);
        }
        let end = flacmeta::audio_start(head);
        if end.unwrap_or(head.len()) > MAX_METADATA_BYTES {
            return Err(DecodeError::TooLarge("metadata chain exceeds the buffering limit"));
        }
        let Some(end) = end else { return Ok(()) };
        let chain = &head[..end];
        self.tags = flacmeta::parse(chain);
        self.points = flacmeta::seek_table(chain).collect();
        let at = lib.feed().base + (self.pos + end) as u64;
        self.audio_start = (!self.lost).then_some(at);
        let (from, limit) = (self.pos, self.pos + end);
        if let Some(lib) = &mut self.lib {
            let feed = lib.feed_mut();
            only_one_streaminfo(&mut feed.buf[from..limit]);
            feed.fed = from;
            feed.limit = limit;
        }
        self.phase = Phase::Chain;
        Ok(())
    }

    fn chain(&mut self) -> Result<(), DecodeError> {
        let Some(lib) = &mut self.lib else {
            return Ok(());
        };
        loop {
            lib.feed_mut().begin();
            let ok = lib.process();
            let state = lib.state();
            let feed = lib.feed();
            self.errors += u64::from(feed.errors);
            if feed.starved.is_some() {
                return Err(DecodeError::BadStreamInfo);
            }
            match state {
                sys::FLAC__STREAM_DECODER_MEMORY_ALLOCATION_ERROR => {
                    return Err(DecodeError::Memory);
                }
                sys::FLAC__STREAM_DECODER_SEARCH_FOR_METADATA
                | sys::FLAC__STREAM_DECODER_READ_METADATA
                    if !ok =>
                {
                    return Err(DecodeError::BadStreamInfo);
                }
                sys::FLAC__STREAM_DECODER_SEARCH_FOR_METADATA
                | sys::FLAC__STREAM_DECODER_READ_METADATA => {}
                sys::FLAC__STREAM_DECODER_SEARCH_FOR_FRAME_SYNC
                | sys::FLAC__STREAM_DECODER_READ_FRAME => break,
                other => return Err(DecodeError::State(other)),
            }
        }
        let feed = lib.feed_mut();
        self.pos = feed.limit;
        feed.limit = usize::MAX;
        self.phase = Phase::Frames;
        Ok(())
    }

    fn frame(&mut self, out: &mut Vec<i32>) -> Result<Option<()>, DecodeError> {
        if self.lib.as_ref().is_none_or(|l| self.starved == Some(l.feed().buf.len())) {
            return Ok(None);
        }
        loop {
            if !self.sync() {
                return Ok(self.starve());
            }
            let Some(lib) = &mut self.lib else {
                return Ok(None);
            };
            let at = self.pos;
            lib.flush();
            let feed = lib.feed_mut();
            feed.fed = at;
            feed.single = true;
            feed.begin();
            lib.process();
            let state = lib.state();
            let feed = lib.feed_mut();
            feed.single = false;
            if state == sys::FLAC__STREAM_DECODER_MEMORY_ALLOCATION_ERROR {
                return Err(DecodeError::Memory);
            }
            if feed.errors > 0 || (feed.frames == 0 && feed.starved.is_none()) {
                self.errors += 1;
                self.pos = at + 1;
                continue;
            }
            if feed.starved.is_some() {
                if feed.buf.len() - at > MAX_FRAME_BYTES {
                    return Err(DecodeError::TooLarge("frame exceeds the 24-bit frame size"));
                }
                return Ok(self.starve());
            }
            if !matches!(
                state,
                sys::FLAC__STREAM_DECODER_SEARCH_FOR_FRAME_SYNC
                    | sys::FLAC__STREAM_DECODER_READ_FRAME
            ) {
                return Err(DecodeError::State(state));
            }
            let (base, fed, len) = (feed.base, feed.fed, feed.buf.len());
            let end = lib.position().map_or(fed, |p| p.saturating_sub(base) as usize).min(len);
            if end.saturating_sub(at) > MAX_FRAME_BYTES {
                return Err(DecodeError::TooLarge("frame exceeds the 24-bit frame size"));
            }
            let feed = lib.feed_mut();
            let first = first_sample(feed, at);
            self.last = (!self.lost).then_some((first, base + at as u64));
            std::mem::swap(out, &mut feed.pcm);
            self.pos = end.max(at + 1);
            self.junk = false;
            self.starved = None;
            self.compact();
            return Ok(Some(()));
        }
    }

    fn sync(&mut self) -> bool {
        let Some(lib) = &self.lib else { return false };
        let buf = &lib.feed().buf;
        let mut at = self.pos;
        let found = loop {
            let Some(i) = buf.get(at..).and_then(|b| b.iter().position(|&x| x == 0xFF)) else {
                at = buf.len();
                break false;
            };
            at += i;
            match frame_header(&buf[at..]) {
                Sync::Header => break true,
                Sync::Short => break false,
                Sync::No => at += 1,
            }
        };
        if at > self.pos && !self.junk {
            self.errors += 1;
            self.junk = true;
        }
        self.pos = at;
        found
    }

    fn starve(&mut self) -> Option<()> {
        self.compact();
        self.starved = self.lib.as_ref().map(|l| l.feed().buf.len());
        None
    }

    fn compact(&mut self) {
        let Some(lib) = &mut self.lib else { return };
        let feed = lib.feed_mut();
        let all = self.pos == feed.buf.len();
        if !all && (self.pos < DRAIN_AT || self.pos < feed.buf.len() / 2) {
            return;
        }
        if all {
            feed.buf.clear();
        } else {
            feed.buf.drain(..self.pos);
        }
        feed.base += self.pos as u64;
        feed.fed = feed.fed.saturating_sub(self.pos);
        self.pos = 0;
    }
}

/// Whether `b` starts with a frame header whose CRC-8 checks (RFC 9639 section 9.1).
fn frame_header(b: &[u8]) -> Sync {
    let Some(&b1) = b.get(1) else {
        return Sync::Short;
    };
    if b1 & 0xFE != 0xF8 {
        return Sync::No;
    }
    let (Some(&b2), Some(&lead)) = (b.get(2), b.get(4)) else {
        return Sync::Short;
    };
    if b2 & 0x0F == 0x0F {
        return Sync::No;
    }
    // Up to 7 bytes, a 36-bit sample number.
    let n = match lead.leading_ones() {
        0 => 1,
        n @ 2..=7 => n as usize,
        _ => return Sync::No,
    };
    for i in 5..4 + n {
        match b.get(i) {
            None => return Sync::Short,
            Some(&c) if c & 0xC0 != 0x80 => return Sync::No,
            Some(_) => {}
        }
    }
    let size = match b2 >> 4 {
        6 => 1,
        7 => 2,
        _ => 0,
    };
    let rate = match b2 & 0x0F {
        12 => 1,
        13 | 14 => 2,
        _ => 0,
    };
    let len = 4 + n + size + rate;
    match b.get(len) {
        None => Sync::Short,
        Some(&crc) if crc8(&b[..len]) == crc => Sync::Header,
        Some(_) => Sync::No,
    }
}

fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |crc, &b| CRC8[usize::from(crc ^ b)])
}

const CRC8: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Turn every STREAMINFO after the first into PADDING. libFLAC would take the last, flacmeta
/// and pf-flac the first (RFC 9639 section 8.2).
fn only_one_streaminfo(chain: &mut [u8]) {
    let mut pos = 4 + 4 + 34;
    while pos + 4 <= chain.len() {
        let len = u32::from_be_bytes([0, chain[pos + 1], chain[pos + 2], chain[pos + 3]]) as usize;
        if chain[pos] & 0x7F == 0 {
            chain[pos] = (chain[pos] & 0x80) | 1;
        }
        pos += 4 + len;
    }
}

/// RFC 9639 section 9.1.5. libFLAC multiplies by the smallest block size and reads a frame
/// number as a sample number where block sizes differ, so the number is read here.
fn first_sample(feed: &crate::libflac::Feed, start: usize) -> u64 {
    let head = feed.buf.get(start..).unwrap_or_default();
    let (Some(&b1), Some(coded)) = (head.get(1), coded_number(head.get(4..).unwrap_or_default()))
    else {
        return feed.first;
    };
    if b1 & 1 == 1 {
        return feed.first;
    }
    let step = feed.info.map_or(0, |i| i.max_block_size);
    let step = if step > 0 { step } else { feed.block };
    coded.saturating_mul(u64::from(step))
}

fn coded_number(b: &[u8]) -> Option<u64> {
    let lead = *b.first()?;
    let n = lead.leading_ones() as usize;
    if n == 0 {
        return Some(u64::from(lead));
    }
    if !(2..=7).contains(&n) {
        return None;
    }
    let mut v = u64::from(lead & (0x7F >> n));
    for &c in b.get(1..n)? {
        v = (v << 6) | u64::from(c & 0x3F);
    }
    Some(v)
}

pub struct FlacDecoder {
    pub info: StreamInfo,
    pub samples: Vec<i64>,
    pub tags: FlacTags,
    pub errors: u64,
}

impl FlacDecoder {
    // One-shot API off the element path.
    #[allow(clippy::disallowed_methods)]
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut d = StreamDecoder::new();
        d.push(data);
        let mut samples = Vec::new();
        let mut frame = Vec::new();
        while d.pull_into(&mut frame)?.is_some() {
            samples.extend(frame.iter().map(|&s| i64::from(s)));
        }
        let info = d.info().copied().ok_or(DecodeError::UnexpectedEof)?;
        if d.phase != Phase::Frames || d.buffered().len() >= 2 {
            return Err(DecodeError::UnexpectedEof);
        }
        Ok(Self { info, samples, tags: std::mem::take(&mut d.tags), errors: d.errors() })
    }
}

#[cfg(test)]
// Tests build their streams on the heap.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn coded_numbers_read_like_the_rfc() {
        assert_eq!(coded_number(&[0xFE, 0xAF, 0x9F, 0xB5, 0xA3, 0xB8, 0x80]), Some(51_000_000_000));
        assert_eq!(coded_number(&[0x7F]), Some(127));
        assert_eq!(coded_number(&[0xC2, 0x80]), Some(128));
        assert_eq!(coded_number(&[0xC2]), None, "cut short");
        assert_eq!(coded_number(&[0x80]), None, "a continuation byte cannot lead");
    }

    #[test]
    fn decode_example_1_from_spec() {
        // RFC 9639 appendix D.1.
        let file: [u8; 0x39] = [
            0x66, 0x4c, 0x61, 0x43, 0x80, 0x00, 0x00, 0x22, 0x10, 0x00, 0x10, 0x00, 0x00, 0x00,
            0x0f, 0x00, 0x00, 0x0f, 0x0a, 0xc4, 0x42, 0xf0, 0x00, 0x00, 0x00, 0x01, 0x3e, 0x84,
            0xb4, 0x18, 0x07, 0xdc, 0x69, 0x03, 0x07, 0x58, 0x6a, 0x3d, 0xad, 0x1a, 0x2e, 0x0f,
            0xff, 0xf8, 0x69, 0x18, 0x00, 0x00, 0xbf, 0x03, 0x58, 0xfd, 0x03, 0x12, 0x8b, 0xaa,
            0x9a,
        ];
        let dec = FlacDecoder::decode(&file).expect("decode example 1");
        assert_eq!(
            (dec.info.channels, dec.info.bits_per_sample, dec.info.sample_rate),
            (2, 16, 44_100)
        );
        assert_eq!(dec.info.total_samples, 1);
        assert_eq!(dec.samples, vec![25588, 10416]);
        let mut d = StreamDecoder::new();
        let mut out = Vec::new();
        let mut got = Vec::new();
        for b in file {
            d.push(&[b]);
            while d.pull_into(&mut out).expect("valid").is_some() {
                got.extend_from_slice(&out);
                assert_eq!(d.last_frame(), Some((0, 0x2a)));
            }
        }
        assert_eq!(got, [25588, 10416]);
        assert_eq!(d.audio_start(), Some(0x2a));
        assert_eq!(
            FlacDecoder::decode(&file[..file.len() - 1]).err(),
            Some(DecodeError::UnexpectedEof)
        );
        assert_eq!(FlacDecoder::decode(&file[..20]).err(), Some(DecodeError::UnexpectedEof));
    }

    const D1_HEADER: [u8; 7] = [0xFF, 0xF8, 0x69, 0x18, 0x00, 0x00, 0xBF];

    #[test]
    fn frame_headers_are_told_on_their_bytes_alone() {
        assert_eq!(crc8(&D1_HEADER[..6]), 0xBF);
        assert_eq!(frame_header(&D1_HEADER), Sync::Header);
        for n in 1..D1_HEADER.len() {
            assert_eq!(frame_header(&D1_HEADER[..n]), Sync::Short, "cut at {n}");
        }
        let mut bad = D1_HEADER;
        bad[6] ^= 1;
        assert_eq!(frame_header(&bad), Sync::No, "CRC-8");
        assert_eq!(frame_header(&[0xFF, 0xF4, 0x69, 0x18, 0x00]), Sync::No, "reserved sync bit");
        assert_eq!(frame_header(&[0xFF, 0xF8, 0x6F, 0x18, 0x00]), Sync::No, "rate 0b1111");
        assert_eq!(
            frame_header(&[0xFF, 0xF8, 0x69, 0x18, 0x80]),
            Sync::No,
            "a continuation byte leads"
        );
        assert_eq!(frame_header(&[0xFF, 0xF8, 0x69, 0x18, 0xFF]), Sync::No, "eight ones lead");
        assert_eq!(
            frame_header(&[0xFF, 0xF8, 0x69, 0x18, 0xC2, 0x00]),
            Sync::No,
            "not a continuation"
        );
        assert_eq!(frame_header(&[0xFF, 0xF8, 0x69, 0x18, 0xC2]), Sync::Short);
        let mut h = vec![0xFF, 0xF9, 0x7E, 0x18, 0x00, 0x12, 0x34, 0x56, 0x78];
        h.push(crc8(&h));
        assert_eq!(frame_header(&h), Sync::Header);
        assert_eq!(frame_header(&h[..h.len() - 1]), Sync::Short);
    }

    #[test]
    fn junk_is_skipped_once_and_counted_once_however_it_comes() {
        let file: [u8; 0x39] = [
            0x66, 0x4c, 0x61, 0x43, 0x80, 0x00, 0x00, 0x22, 0x10, 0x00, 0x10, 0x00, 0x00, 0x00,
            0x0f, 0x00, 0x00, 0x0f, 0x0a, 0xc4, 0x42, 0xf0, 0x00, 0x00, 0x00, 0x01, 0x3e, 0x84,
            0xb4, 0x18, 0x07, 0xdc, 0x69, 0x03, 0x07, 0x58, 0x6a, 0x3d, 0xad, 0x1a, 0x2e, 0x0f,
            0xff, 0xf8, 0x69, 0x18, 0x00, 0x00, 0xbf, 0x03, 0x58, 0xfd, 0x03, 0x12, 0x8b, 0xaa,
            0x9a,
        ];
        let junk = [0x00, 0xFF, 0xF8, 0x69, 0x18, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x13];
        let stream = [&file[..0x2a], &junk[..], &file[0x2a..], &[0xFF][..]].concat();
        for piece in 1..stream.len() {
            let mut d = StreamDecoder::new();
            let mut out = Vec::new();
            let mut got = Vec::new();
            for c in stream.chunks(piece) {
                d.push(c);
                while d.pull_into(&mut out).expect("valid").is_some() {
                    got.push((out.clone(), d.last_frame()));
                }
            }
            assert_eq!(
                got,
                [(vec![25588, 10416], Some((0, 0x2a + junk.len() as u64)))],
                "pieces of {piece}"
            );
            assert_eq!(d.errors(), 1, "pieces of {piece}");
            assert_eq!(d.buffered(), [0xFF], "a last 0xFF may begin a frame");
        }
    }

    fn tagged(body: usize) -> Vec<u8> {
        let mut tag = b"ID3\x03\x00\x00".to_vec();
        tag.extend((0..4).rev().map(|i| ((body >> (7 * i)) & 0x7F) as u8));
        tag.resize(10 + body, 0xFF);
        tag
    }

    #[test]
    fn an_id3v2_tag_is_dropped_as_it_comes() {
        let file: [u8; 0x39] = [
            0x66, 0x4c, 0x61, 0x43, 0x80, 0x00, 0x00, 0x22, 0x10, 0x00, 0x10, 0x00, 0x00, 0x00,
            0x0f, 0x00, 0x00, 0x0f, 0x0a, 0xc4, 0x42, 0xf0, 0x00, 0x00, 0x00, 0x01, 0x3e, 0x84,
            0xb4, 0x18, 0x07, 0xdc, 0x69, 0x03, 0x07, 0x58, 0x6a, 0x3d, 0xad, 0x1a, 0x2e, 0x0f,
            0xff, 0xf8, 0x69, 0x18, 0x00, 0x00, 0xbf, 0x03, 0x58, 0xfd, 0x03, 0x12, 0x8b, 0xaa,
            0x9a,
        ];
        let tag = tagged(300);
        let stream = [&tag[..], &file[..]].concat();
        for piece in [1, 2, 3, 9, 10, 11, 64, 310, 311, stream.len()] {
            let mut d = StreamDecoder::new();
            let mut out = Vec::new();
            let mut got = Vec::new();
            let mut pushed = 0;
            for c in stream.chunks(piece) {
                d.push(c);
                pushed += c.len();
                while d.pull_into(&mut out).expect("valid").is_some() {
                    got.push((out.clone(), d.last_frame()));
                }
                assert!(pushed > tag.len() || d.buffered().len() < 10, "pieces of {piece}");
            }
            assert_eq!(got, [(vec![25588, 10416], Some((0, 310 + 0x2a)))], "pieces of {piece}");
            assert_eq!((d.audio_start(), d.errors()), (Some(310 + 0x2a), 0), "pieces of {piece}");
        }
        for n in 0..=tag.len() {
            let mut d = StreamDecoder::new();
            d.push(&stream[..n]);
            assert_eq!(d.pull_into(&mut Vec::new()), Ok(None), "cut at {n}");
            assert_eq!(
                FlacDecoder::decode(&stream[..n]).err(),
                Some(DecodeError::UnexpectedEof),
                "cut at {n}"
            );
        }
        let mut bad = stream.clone();
        bad[9] |= 0x80;
        assert_eq!(FlacDecoder::decode(&bad).err(), Some(DecodeError::BadMagic));
        let twice = [&tag[..], &tag[..], &file[..]].concat();
        assert_eq!(FlacDecoder::decode(&twice).err(), Some(DecodeError::BadMagic));
    }

    #[test]
    fn a_stream_that_is_not_flac_is_refused_once_the_marker_is_in() {
        let mut d = StreamDecoder::new();
        let mut out = Vec::new();
        d.push(b"fL");
        assert_eq!(d.pull_into(&mut out), Ok(None));
        d.push(b"ac");
        assert_eq!(d.pull_into(&mut out), Err(DecodeError::BadMagic));
        assert_eq!(d.pull_into(&mut out), Err(DecodeError::BadMagic), "the error stays");
        let mut d = StreamDecoder::new();
        d.push(b"fLaC\x01\x00\x00\x22");
        assert_eq!(d.pull_into(&mut out), Err(DecodeError::BadStreamInfo));
        let mut d = StreamDecoder::new();
        d.push(b"ID");
        assert_eq!(d.pull_into(&mut out), Ok(None));
        let mut d = StreamDecoder::new();
        d.push(b"IE");
        assert_eq!(d.pull_into(&mut out), Ok(None), "short of the marker");
        d.push(b"xx");
        assert_eq!(d.pull_into(&mut out), Err(DecodeError::BadMagic));
    }
}
