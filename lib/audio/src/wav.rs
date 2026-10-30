//! WAV (RIFF/WAVE) header parser and the [`WavParse`] element.

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::time::Timestamp;

use crate::bytes_in::ByteIn;
use crate::error::{AudioErrorKind as K, failed};
use crate::format::{AudioFormat, SampleFormat};
use crate::trim::{span_at, time_at};

const MAX_HEADER: usize = 1 << 20;

const CUT_FRAMES: usize = 4096;

const CUTS_PER_PASS: u32 = 4;

/// A parsed WAV header, the format plus where the PCM payload begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavHeader {
    pub format: AudioFormat,
    /// Byte offset of the first PCM sample.
    pub data_offset: usize,
    /// Declared payload length, or `None` for a streaming/unknown size (`0` or `0xFFFFFFFF`).
    pub data_len: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WavError {
    NeedMore,
    NotRiff,
    NotWave,
    BadFmt,
    NoFmt,
    Unsupported { tag: u16, bits: u16 },
    Malformed,
}

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

type Fmt = (u16, u16, u32, u16);

const RIFF_HEAD: usize = 12;

const CHUNK_PEEK: usize = 8 + 26;

enum Chunk {
    Fmt(Fmt),
    Data(u32),
    Other,
}

fn check_riff(b: &[u8]) -> Result<(), WavError> {
    if b.len() < RIFF_HEAD {
        return Err(WavError::NeedMore);
    }
    if &b[0..4] != b"RIFF" {
        return Err(WavError::NotRiff);
    }
    if &b[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }
    Ok(())
}

fn chunk_at(b: &[u8]) -> Result<(Chunk, u64), WavError> {
    let Some((head, body)) = b.split_first_chunk::<8>() else {
        return Err(WavError::NeedMore);
    };
    let size = le_u32(head, 4);
    let chunk = match &head[..4] {
        b"fmt " => {
            if size < 16 {
                return Err(WavError::BadFmt);
            }
            if body.len() < 16 {
                return Err(WavError::NeedMore);
            }
            let tag = le_u16(body, 0);
            let channels = le_u16(body, 2);
            let rate = le_u32(body, 4);
            let bits = le_u16(body, 14);
            let real_tag = if tag == 0xFFFE {
                if size < 26 {
                    return Err(WavError::BadFmt);
                }
                if body.len() < 26 {
                    return Err(WavError::NeedMore);
                }
                le_u16(body, 24)
            } else {
                tag
            };
            Chunk::Fmt((real_tag, channels, rate, bits))
        }
        b"data" => Chunk::Data(size),
        _ => Chunk::Other,
    };
    Ok((chunk, 8 + size as u64 + (size & 1) as u64))
}

fn data_header(fmt: Option<Fmt>, size32: u32, data_offset: usize) -> Result<WavHeader, WavError> {
    let (tag, channels, rate, bits) = fmt.ok_or(WavError::NoFmt)?;
    if channels == 0 || rate == 0 {
        return Err(WavError::BadFmt);
    }
    let sf = SampleFormat::from_wav(tag, bits).ok_or(WavError::Unsupported { tag, bits })?;
    let format = AudioFormat::new(rate, channels, sf);
    let data_len = if size32 == 0 || size32 == u32::MAX { None } else { Some(size32 as u64) };
    Ok(WavHeader { format, data_offset, data_len })
}

/// Parse a WAV header from the front of `b`, scanning chunks until `data` is reached.
pub fn parse_wav_header(b: &[u8]) -> Result<WavHeader, WavError> {
    check_riff(b)?;
    let mut pos = RIFF_HEAD;
    let mut fmt = None;
    loop {
        let rest = b.get(pos..).ok_or(WavError::NeedMore)?;
        let (chunk, next) = chunk_at(rest)?;
        match chunk {
            Chunk::Fmt(f) => fmt = Some(f),
            Chunk::Data(size) => return data_header(fmt, size, pos + 8),
            Chunk::Other => {}
        }
        pos = usize::try_from(next)
            .ok()
            .and_then(|n| pos.checked_add(n))
            .ok_or(WavError::Malformed)?;
    }
}

/// Write a canonical 44-byte-header PCM WAV wrapping `pcm`.
// Cold path, tests and examples.
#[allow(clippy::disallowed_methods)]
pub fn write_pcm_wav(format: &AudioFormat, pcm: &[u8]) -> Vec<u8> {
    let stride = format.frame_stride();
    let block_align = u16::try_from(stride).unwrap_or(u16::MAX);
    let byte_rate = format.sample_rate.saturating_mul(u32::try_from(stride).unwrap_or(u32::MAX));
    let data_len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&data_len.saturating_add(36).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    let tag: u16 = if format.format.is_float() { 3 } else { 1 };
    w.extend_from_slice(&tag.to_le_bytes());
    w.extend_from_slice(&format.channels.to_le_bytes());
    w.extend_from_slice(&format.sample_rate.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&block_align.to_le_bytes());
    w.extend_from_slice(&(format.format.bits() as u16).to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

/// Nominal spacing of a WAV index's entries.
pub const WAV_GRID_NS: u64 = 100_000_000;
/// Most entries a WAV index has, the grid thinned to fit long files.
pub const WAV_MAX_ENTRIES: u64 = 8_192;

/// The seek index of a WAV file.
// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
pub fn wav_index(h: &WavHeader, len: Option<u64>) -> Index {
    let none = Index { file_len: len, ..Index::default() };
    let (stride, rate) = (h.format.frame_stride() as u64, h.format.sample_rate);
    let start = h.data_offset as u64;
    if stride == 0 || rate == 0 || len.is_some_and(|len| start >= len) {
        return none;
    }
    let held = len.map(|len| len - start);
    let Some(data) = h.data_len.map(|d| held.map_or(d, |held| d.min(held))).or(held) else {
        return none;
    };
    let ns = |frames: u64| {
        (u128::from(frames) * 1_000_000_000 / u128::from(rate)).min(u128::from(u64::MAX)) as u64
    };
    let duration = ns(data / stride);
    let byte_rate = u128::from(rate) * u128::from(stride);
    let step = WAV_GRID_NS.max(duration / WAV_MAX_ENTRIES + 1);
    let mut entries: Vec<IndexEntry> = Vec::with_capacity((duration / step + 1) as usize);
    let mut at = 0u64;
    while at < duration {
        let byte = (u128::from(at) * byte_rate / 1_000_000_000) as u64;
        let frame = byte / stride;
        let time = ns(frame);
        if entries.last().is_none_or(|e| time > e.time_ns) {
            entries.push(IndexEntry::key(time, start + frame * stride));
        }
        at = at.saturating_add(step);
    }
    let complete = h.data_len.is_some();
    Index { entries, file_len: len, duration: Timestamp(duration), complete, ..Index::default() }
}

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "wavparse",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(WavParse::new())),
    io: false,
    couplings: &[],
};

/// Parses a WAV stream and emits its interleaved PCM payload.
#[derive(Default)]
pub struct WavParse {
    header: Option<WavHeader>,
    acc: Vec<u8>,
    scanned: u64,
    riff: bool,
    fmt: Option<Fmt>,
    skip: u64,
    data_emitted: u64,
    held: Option<Buffer>,
    at: usize,
    part: Vec<u8>,
    len: Option<u64>,
    published: bool,
    input: ByteIn,
}

impl WavParse {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::default()
    }

    /// A parser of a source `len` bytes long.
    pub fn with_len(len: Option<u64>) -> Self {
        Self { len, ..Self::default() }
    }

    fn publish(&mut self, ctx: &mut Ctx, header: &WavHeader) {
        if std::mem::replace(&mut self.published, true) {
            return;
        }
        let index = wav_index(header, self.len);
        if let Some(ns) = index.duration.nanos() {
            ctx.set_duration(ns);
        }
        ctx.publish_index(index);
    }

    pub fn format(&self) -> Option<AudioFormat> {
        self.header.as_ref().map(|h| h.format)
    }

    fn rebase_on_flush(&mut self, target: Option<SeekTarget>) {
        self.acc.clear();
        self.held = None;
        self.part.clear();
        self.data_emitted = 0;
        let (Some(t), Some(h)) = (target, self.header.as_ref()) else {
            self.header = None;
            self.reset_scan();
            return;
        };
        let data_offset = h.data_offset as u64;
        if t.to_byte >= data_offset {
            self.data_emitted = t.to_byte - data_offset;
        } else {
            self.header = None;
            self.reset_scan();
        }
    }

    fn reset_scan(&mut self) {
        self.acc.clear();
        self.scanned = 0;
        self.riff = false;
        self.fmt = None;
        self.skip = 0;
    }

    fn scan_header<'a>(&mut self, mut data: &'a [u8]) -> Result<&'a [u8], WavError> {
        loop {
            if self.skip > 0 {
                let n = self.skip.min(data.len() as u64) as usize;
                data = &data[n..];
                self.skip -= n as u64;
                self.scanned += n as u64;
                if self.skip > 0 {
                    return Ok(data);
                }
            }
            let take = (CHUNK_PEEK - self.acc.len()).min(data.len());
            self.acc.extend_from_slice(&data[..take]);
            data = &data[take..];

            if !self.riff {
                match check_riff(&self.acc) {
                    Ok(()) => {}
                    Err(WavError::NeedMore) => return Ok(data),
                    Err(e) => return Err(e),
                }
                self.riff = true;
                self.acc.drain(..RIFF_HEAD);
                self.scanned += RIFF_HEAD as u64;
                continue;
            }
            let (chunk, next) = match chunk_at(&self.acc) {
                Ok(step) => step,
                Err(WavError::NeedMore) if self.acc.len() < CHUNK_PEEK => return Ok(data),
                Err(e) => return Err(e),
            };
            match chunk {
                Chunk::Fmt(f) => self.fmt = Some(f),
                Chunk::Data(size) => {
                    let offset = self.scanned.saturating_add(8);
                    let offset = usize::try_from(offset).map_err(|_| WavError::Malformed)?;
                    self.header = Some(data_header(self.fmt, size, offset)?);
                    return Ok(data);
                }
                Chunk::Other => {}
            }
            let held = (self.acc.len() as u64).min(next);
            self.acc.drain(..held as usize);
            self.scanned += held;
            self.skip = next - held;
        }
    }

    fn take_input(&mut self, ctx: &mut Ctx, buf: Buffer) -> Result<(), Error> {
        if self.header.is_some() {
            self.at = 0;
            self.held = Some(buf);
            return Ok(());
        }
        let len = buf.memory.len();
        let rest = match self.scan_header(buf.memory.data()) {
            Ok(rest) => rest.len(),
            Err(e) => return Err(failed(ctx, DESC.name, K::Wav(e))),
        };
        if let Some(header) = self.header {
            log!(
                &*ctx,
                Level::Info,
                "header",
                rate = header.format.sample_rate,
                channels = header.format.channels,
                sample = header.format.format.caps_name(),
                data_len = header.data_len.unwrap_or(0)
            );
            self.publish(ctx, &header);
            let head = self.acc.get(8..).unwrap_or(&[]);
            let n = (head.len() as u64).min(header.data_len.unwrap_or(u64::MAX)) as usize;
            self.part.extend_from_slice(&head[..n]);
            self.data_emitted = n as u64;
            self.acc.clear();
            self.at = len - rest;
            self.held = Some(buf);
            return Ok(());
        }
        let scanned = self.scanned.saturating_add(self.acc.len() as u64);
        if scanned > MAX_HEADER as u64 {
            let kind = K::WavHeaderBudget {
                buffered: usize::try_from(scanned).unwrap_or(usize::MAX),
                budget: MAX_HEADER,
            };
            return Err(failed(ctx, DESC.name, kind));
        }
        Ok(())
    }

    fn cut(&mut self, ctx: &mut Ctx) -> Result<Cut, Error> {
        let (Some(header), Some(held)) = (self.header, self.held.take()) else {
            return Ok(Cut::Dry);
        };
        let (stride, rate) = (header.format.frame_stride().max(1), header.format.sample_rate);
        let data = held.memory.data();
        let left =
            header.data_len.map_or(u64::MAX, |total| total.saturating_sub(self.data_emitted));
        let mut avail = ((data.len() - self.at) as u64).min(left) as usize;
        if self.part.is_empty() {
            let into = (self.data_emitted % stride as u64) as usize;
            if into != 0 {
                let skip = (stride - into).min(avail);
                log!(&*ctx, Level::Debug, "realign", skip = skip, emitted = self.data_emitted);
                self.at += skip;
                self.data_emitted += skip as u64;
                avail -= skip;
            }
        }
        if self.part.len() + avail < stride {
            self.part.extend_from_slice(&data[self.at..self.at + avail]);
            self.data_emitted += avail as u64;
            return Ok(Cut::Dry);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            self.held = Some(held);
            return Ok(Cut::Starved);
        };
        let capacity = buf.memory.capacity();
        let room = CUT_FRAMES.saturating_mul(stride).min(capacity - capacity % stride);
        if room == 0 {
            return Err(failed(ctx, DESC.name, K::SlotTooSmall { capacity, stride }));
        }
        let have = self.part.len() + avail;
        let n = room.min(have - have % stride);
        let from_part = self.part.len().min(n);
        let from_held = n - from_part;
        let dst = buf.memory.as_mut_full();
        dst[..from_part].copy_from_slice(&self.part[..from_part]);
        dst[from_part..n].copy_from_slice(&data[self.at..self.at + from_held]);
        buf.memory.set_len(n);
        let frame = (self.data_emitted - self.part.len() as u64) / stride as u64;
        (buf.pts, buf.duration) = span_at(time_at(frame, rate), 0, n / stride, rate);
        self.part.drain(..from_part);
        self.at += from_held;
        self.data_emitted += from_held as u64;
        ctx.out(Self::OUT).push(buf);
        self.held = Some(held);
        Ok(Cut::Pushed)
    }
}

impl WavParse {
    fn log_eos(&self, ctx: &Ctx) {
        let Some(header) = self.header else {
            let seen = self.scanned.saturating_add(self.acc.len() as u64);
            if seen > 0 {
                log!(ctx, Level::Error, "eos_before_data", scanned = seen, riff = self.riff);
            }
            return;
        };
        if !self.part.is_empty() {
            log!(ctx, Level::Warn, "eos_partial_frame", bytes = self.part.len());
        }
        if let Some(declared) = header.data_len.filter(|d| self.data_emitted < *d) {
            log!(ctx, Level::Warn, "truncated", emitted = self.data_emitted, declared = declared);
        }
        log!(ctx, Level::Info, "eos", emitted = self.data_emitted);
    }
}

enum Cut {
    Pushed,
    Dry,
    Starved,
}

impl WavParse {
    fn pass(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        let mut cuts = 0;
        loop {
            if self.held.is_none() {
                let Some(buf) = self.input.next(ctx, inputs) else {
                    return Ok(Flow::Ok);
                };
                self.take_input(ctx, buf)?;
                continue;
            }
            if cuts == CUTS_PER_PASS {
                ctx.hold_end();
                return Ok(Flow::Ok);
            }
            match self.cut(ctx)? {
                Cut::Pushed => cuts += 1,
                Cut::Dry => {}
                Cut::Starved => {
                    ctx.hold_end();
                    return Ok(Flow::Ok);
                }
            }
        }
    }
}

impl Element for WavParse {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.will_index();
        self.published = false;
        self.header = None;
        self.reset_scan();
        self.data_emitted = 0;
        self.held = None;
        self.part.clear();
        self.input.start(ctx, self.len);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let flow = self.pass(ctx, &mut inputs);
        self.input.ask(ctx);
        flow
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart => {
                let target = ctx.seek_target();
                log!(
                    &*ctx,
                    Level::Debug,
                    "flush",
                    to_byte = target.map_or(0, |t| t.to_byte),
                    had_header = self.header.is_some()
                );
                self.rebase_on_flush(target);
            }
            Event::Eos => self.log_eos(ctx),
            _ => {}
        }
        Ok(())
    }

    // Cold path, teardown.
    #[allow(clippy::disallowed_methods)]
    fn stop(&mut self, _ctx: &mut Ctx) {
        self.header = None;
        self.reset_scan();
        self.acc = Vec::new();
        self.data_emitted = 0;
        self.held = None;
        self.part = Vec::new();
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use profluens_core::time::Timestamp;

    fn wav_header(format: &AudioFormat, data_len: u32) -> Vec<u8> {
        let block_align = format.frame_stride() as u16;
        let byte_rate = format.sample_rate * block_align as u32;
        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + data_len).to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        let tag: u16 = if format.format.is_float() { 3 } else { 1 };
        h.extend_from_slice(&tag.to_le_bytes());
        h.extend_from_slice(&format.channels.to_le_bytes());
        h.extend_from_slice(&format.sample_rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&(format.format.bits() as u16).to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        h
    }

    #[test]
    fn zero_rate_is_refused() {
        let mut wav = wav_header(&AudioFormat::new(44_100, 2, SampleFormat::S16), 8);
        wav[24..28].copy_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 8]);
        assert!(matches!(parse_wav_header(&wav), Err(WavError::BadFmt)));
    }

    #[test]
    fn parses_canonical_pcm_header() {
        let fmt = AudioFormat::new(44_100, 2, SampleFormat::S16);
        let mut wav = wav_header(&fmt, 8);
        wav.extend_from_slice(&[0u8; 8]);
        let h = parse_wav_header(&wav).expect("valid header");
        assert_eq!(h.format, fmt);
        assert_eq!(h.data_offset, 44);
        assert_eq!(h.data_len, Some(8));
    }

    #[test]
    fn skips_unknown_chunks_before_data() {
        let fmt = AudioFormat::new(48_000, 1, SampleFormat::S24);
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes()); // riff size unused by the parser
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // 1ch
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&(48_000u32 * 3).to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes()); // block align
        wav.extend_from_slice(&24u16.to_le_bytes()); // bits
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&5u32.to_le_bytes());
        wav.extend_from_slice(b"hello"); // 5 bytes
        wav.push(0); // pad to even
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&6u32.to_le_bytes());
        let data_at = wav.len();
        wav.extend_from_slice(&[0u8; 6]);

        let h = parse_wav_header(&wav).expect("valid with LIST chunk");
        assert_eq!(h.format, fmt);
        assert_eq!(h.data_offset, data_at);
        assert_eq!(h.data_len, Some(6));
    }

    #[test]
    fn incomplete_header_reports_needmore() {
        let fmt = AudioFormat::new(44_100, 2, SampleFormat::S16);
        let wav = wav_header(&fmt, 100);
        for n in 0..wav.len() {
            assert_eq!(parse_wav_header(&wav[..n]), Err(WavError::NeedMore), "len {n}");
        }
        assert!(parse_wav_header(&wav).is_ok());
    }

    #[test]
    fn rejects_non_riff_and_unsupported() {
        assert_eq!(parse_wav_header(b"NOPExxxxWAVE"), Err(WavError::NotRiff));
        let mut not_wave = b"RIFF\x00\x00\x00\x00AVI ".to_vec();
        not_wave.resize(12, 0);
        assert_eq!(parse_wav_header(&not_wave), Err(WavError::NotWave));
        let fmt = AudioFormat::new(44_100, 1, SampleFormat::S16);
        let mut wav = wav_header(&fmt, 0);
        wav[34] = 12; // overwrite bits-per-sample field
        assert!(matches!(parse_wav_header(&wav), Err(WavError::Unsupported { bits: 12, .. })));
    }

    fn parsed(data_offset: usize, data_len: u64, emitted: u64) -> WavParse {
        let mut w = WavParse::new();
        w.header = Some(WavHeader {
            format: AudioFormat::new(48_000, 2, SampleFormat::S16),
            data_offset,
            data_len: Some(data_len),
        });
        w.data_emitted = emitted;
        w
    }

    #[test]
    fn flush_rebases_the_payload_cursor_onto_the_seek_target() {
        let mut w = parsed(44, 1_000, 900);
        w.rebase_on_flush(Some(SeekTarget { to_byte: 44 + 400, at: Timestamp::from_secs(1) }));
        assert_eq!(w.data_emitted, 400, "cursor = to_byte - data_offset");
        assert!(w.format().is_some(), "a mid-payload seek keeps the parsed header");
    }

    #[test]
    fn repeated_flushes_do_not_accumulate_the_cursor() {
        let mut w = parsed(44, 1_000, 0);
        for pass in 0..10 {
            w.data_emitted += 900; // most of a pass over the payload
            w.rebase_on_flush(Some(SeekTarget { to_byte: 44, at: Timestamp::ZERO }));
            assert_eq!(w.data_emitted, 0, "pass {pass}: a seek to the payload start consumed 0");
        }
        let remaining = w.header.unwrap().data_len.unwrap() - w.data_emitted;
        assert_eq!(remaining, 1_000, "the full data chunk is still playable");
    }

    #[test]
    fn flush_before_the_data_chunk_re_parses_the_header() {
        let mut w = parsed(44, 1_000, 500);
        w.rebase_on_flush(Some(SeekTarget { to_byte: 0, at: Timestamp::ZERO }));
        assert_eq!(w.data_emitted, 0);
        assert!(w.format().is_none(), "the header must be re-parsed from the incoming bytes");
    }

    #[test]
    fn a_seek_with_no_target_starts_the_scan_over_as_its_source_starts_over() {
        let mut w = parsed(44, 1_000, 900);
        w.rebase_on_flush(None);
        assert_eq!(w.data_emitted, 0);
        assert!(w.format().is_none(), "the header must be re-parsed from the incoming bytes");
    }

    #[test]
    fn flush_past_the_payload_end_yields_no_output_rather_than_wrapping() {
        let mut w = parsed(44, 1_000, 0);
        w.rebase_on_flush(Some(SeekTarget { to_byte: 44 + 5_000, at: Timestamp::ZERO }));
        assert_eq!(w.data_emitted, 5_000);
        let total = w.header.unwrap().data_len.unwrap();
        assert_eq!(total.saturating_sub(w.data_emitted), 0, "nothing left to emit");
    }

    fn pairs(index: &Index) -> Vec<(u64, u64)> {
        index.entries.iter().map(|e| (e.time_ns, e.byte)).collect()
    }

    #[test]
    fn the_index_is_the_line_on_a_tenth_of_a_second_on_whole_frames() {
        let format = AudioFormat::new(44_100, 2, SampleFormat::S16);
        let file = write_pcm_wav(&format, &vec![0u8; 44_100 * 4 * 3]);
        let len = file.len() as u64;
        let index = wav_index(&parse_wav_header(&file).unwrap(), Some(len));
        assert_eq!(index.duration, Timestamp(3_000_000_000));
        assert!(index.complete);
        assert_eq!(index.file_len, Some(len));
        let p = pairs(&index);
        assert_eq!(p.len(), 30);
        assert_eq!(p[0], (0, 44));
        assert_eq!(p[1], (100_000_000, 44 + 17_640));
        assert_eq!(p[15], (1_500_000_000, 44 + 66_150 * 4));
        for &(time, byte) in &p {
            assert_eq!((byte - 44) % 4, 0, "whole frames");
            assert_eq!(time, (byte - 44) / 4 * 1_000_000_000 / 44_100, "the time of the byte");
        }
        let odd =
            write_pcm_wav(&AudioFormat::new(22_050, 1, SampleFormat::S16), &vec![0u8; 22_050]);
        let index = wav_index(&parse_wav_header(&odd).unwrap(), Some(odd.len() as u64));
        assert_eq!(pairs(&index)[3], (3 * 2_205 * 1_000_000_000 / 22_050, 44 + 3 * 2_205 * 2));
    }

    #[test]
    fn a_long_file_thins_the_grid() {
        let hour = write_pcm_wav(&AudioFormat::new(8_000, 1, SampleFormat::U8), &[]);
        let index = wav_index(&parse_wav_header(&hour).unwrap(), Some(44 + 8_000 * 3_600));
        assert_eq!(index.duration, Timestamp(3_600_000_000_000));
        assert!(!index.complete);
        assert!(index.entries.len() <= WAV_MAX_ENTRIES as usize + 1 && index.entries.len() > 4_000);
    }

    #[test]
    fn the_size_is_the_headers_clamped_to_the_source() {
        let format = AudioFormat::new(48_000, 2, SampleFormat::S16);
        let file = write_pcm_wav(&format, &vec![0u8; 48_000 * 4 * 2]);
        let h = parse_wav_header(&file).unwrap();
        assert_eq!(wav_index(&h, None).duration, Timestamp(2_000_000_000));
        let cut = wav_index(&h, Some(44 + 48_000 * 4));
        assert_eq!(cut.duration, Timestamp(1_000_000_000));
        assert!(cut.entries.iter().all(|e| e.byte < 44 + 48_000 * 4));
        let sizeless = WavHeader { data_len: None, ..h };
        let none = wav_index(&sizeless, None);
        assert!(none.entries.is_empty() && none.duration.is_none());
        assert!(wav_index(&h, Some(40)).entries.is_empty(), "the data starts past the end");
    }
}
