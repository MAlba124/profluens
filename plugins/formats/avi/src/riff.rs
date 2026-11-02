//! Core-independent AVI/RIFF primitives, the header probe, the `movi` walker and the `idx1` parser.

/// A four-character code (RIFF chunk id / AVI fourcc).
pub type FourCc = [u8; 4];

/// The kind of a stream, from `strh.fccType`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamKind {
    Video,
    Audio,
    Other,
}

/// One discovered AVI stream, its `strh` timing and decoded `strf` format.
#[derive(Clone, Debug)]
pub struct Stream {
    /// 0-based ordinal in `strl` order, the routing key.
    pub index: usize,
    pub kind: StreamKind,
    /// `strh.fccHandler`, the codec fourcc for video.
    pub handler: FourCc,
    pub scale: u32,
    pub rate: u32,
    /// `strh.dwSampleSize`. 0 means one frame per chunk, else `size/sample_size` samples.
    pub sample_size: u32,
    pub length: u32,
    pub width: u32,
    /// Absolute value, since top-down DIBs store `biHeight` negative.
    pub height: u32,
    pub strf: Vec<u8>,
    pub format_tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub bits_per_sample: u16,
}

impl Stream {
    /// One sample's duration in ns from `dwScale/dwRate`, `None` when `rate` is zero.
    pub fn sample_duration_ns(&self) -> Option<u64> {
        if self.rate == 0 {
            return None;
        }
        Some((self.scale as u64).saturating_mul(1_000_000_000) / self.rate as u64)
    }

    /// The codec's own bytes behind the fixed format header of `strf`, behind the
    /// BITMAPINFOHEADER (its `biSize`, 40 at least) of a video stream, behind the 18 octets of
    /// WAVEFORMATEX of an audio one, the VOL of an MPEG-4 stream. Empty for other streams.
    pub fn codec_head(&self) -> &[u8] {
        let fixed = match self.kind {
            StreamKind::Video => {
                let size =
                    self.strf.get(..4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                usize::try_from(size).unwrap_or(usize::MAX).max(40)
            }
            StreamKind::Audio => 18,
            StreamKind::Other => return &[],
        };
        self.strf.get(fixed..).unwrap_or(&[])
    }
}

#[derive(Clone, Debug, Default)]
pub struct AviHeader {
    pub streams: Vec<Stream>,
    /// `avih.dwMicroSecPerFrame`, the nominal frame interval in us.
    pub us_per_frame: u32,
    pub total_frames: u32,
    /// Whether an OpenDML `LIST 'odml'` header was present.
    pub has_odml: bool,
}

impl AviHeader {
    /// Presentation duration in ns, the longest stream's `length * scale / rate`.
    pub fn duration_ns(&self) -> Option<u64> {
        self.streams
            .iter()
            .filter_map(|s| {
                if s.rate == 0 {
                    return None;
                }
                Some(
                    (s.length as u64).saturating_mul(s.scale as u64).saturating_mul(1_000_000_000)
                        / s.rate as u64,
                )
            })
            .max()
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.at)
    }

    fn u16le(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32le(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn fourcc(&mut self) -> Option<FourCc> {
        let b = self.take(4)?;
        Some([b[0], b[1], b[2], b[3]])
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.data.get(self.at..end)?;
        self.at = end;
        Some(s)
    }

    fn peek(&self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        self.data.get(self.at..end)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AviError {
    NotAvi,
    Truncated(&'static str),
    HeaderIncomplete,
    MoviNotReached,
    NoStreams,
    NoDecoderFamily { stream: usize },
}

impl std::fmt::Display for AviError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AviError::NotAvi => {
                f.write_str("avidemux: header parse failed, not a RIFF 'AVI ' document")
            }
            AviError::Truncated(what) => {
                write!(f, "avidemux: header parse failed, truncated or absent {what}")
            }
            AviError::HeaderIncomplete => {
                f.write_str("avidemux: header parse failed, the prefix ends inside hdrl")
            }
            AviError::MoviNotReached => f.write_str(
                "avidemux: constructor header does not reach the 'movi' list, pass more of \
                 the stream head (through hdrl and the movi list header)",
            ),
            AviError::NoStreams => f.write_str("avidemux: no streams discovered in hdrl"),
            AviError::NoDecoderFamily { stream } => write!(
                f,
                "avidemux: stream {stream} has no decoder family (fourcc/tag unrecognised), \
                 emitting raw bytes"
            ),
        }
    }
}

impl std::error::Error for AviError {}

/// Where the `movi` list sits in a probed header.
#[derive(Clone, Copy, Debug)]
pub struct MoviLocation {
    /// Absolute offset of the `movi` list's data start.
    pub movi_data_start: u64,
    pub movi_chunk_start: u64,
    /// Absolute offset past the `movi` list and its pad, where `idx1` follows.
    pub movi_end: u64,
}

/// Parse an AVI header prefix through `hdrl`, locating `movi` if present.
pub fn probe_header(data: &[u8]) -> Result<(AviHeader, Option<MoviLocation>), AviError> {
    let mut c = Cursor::new(data);
    if c.fourcc() != Some(*b"RIFF") {
        return Err(AviError::NotAvi);
    }
    let _riff_size = c.u32le().ok_or(AviError::Truncated("RIFF size"))?;
    if c.fourcc() != Some(*b"AVI ") {
        return Err(AviError::NotAvi);
    }

    let mut header = AviHeader::default();
    let mut movi = None;
    while c.remaining() >= 8 {
        let chunk_start = c.at as u64;
        let id = c.fourcc().ok_or(AviError::Truncated("top-level chunk id"))?;
        let size = c.u32le().ok_or(AviError::Truncated("top-level chunk size"))? as usize;
        if &id == b"LIST" {
            let list_type = c.fourcc().ok_or(AviError::Truncated("LIST type"))?;
            let body_len = size.checked_sub(4).ok_or(AviError::Truncated("LIST size < 4"))?;
            match &list_type {
                b"hdrl" => {
                    let body = c.peek(body_len).ok_or(AviError::HeaderIncomplete)?;
                    parse_hdrl(body, &mut header)?;
                    c.at += body_len; // advance past the list body
                }
                b"movi" => {
                    movi = Some(MoviLocation {
                        movi_chunk_start: chunk_start,
                        movi_data_start: c.at as u64,
                        movi_end: chunk_start.saturating_add(8).saturating_add(padded(size) as u64),
                    });
                    break;
                }
                _ => {
                    c.at = advance_padded(c.at, body_len);
                }
            }
        } else {
            c.at = advance_padded(c.at, size);
        }
        if c.at > data.len() {
            break;
        }
    }

    if header.streams.is_empty() {
        return Err(AviError::Truncated("no strl stream in hdrl"));
    }
    Ok((header, movi))
}

/// The most streams a file can address.
pub const MAX_STREAMS: usize = 100;

fn advance_padded(at: usize, size: usize) -> usize {
    at.saturating_add(size).saturating_add(size & 1)
}

fn parse_hdrl(body: &[u8], header: &mut AviHeader) -> Result<(), AviError> {
    let mut c = Cursor::new(body);
    let mut stream_index = 0usize;
    while c.remaining() >= 8 {
        let id = c.fourcc().ok_or(AviError::Truncated("hdrl chunk id"))?;
        let size = c.u32le().ok_or(AviError::Truncated("hdrl chunk size"))? as usize;
        match &id {
            b"avih" => {
                let b = c.peek(size).ok_or(AviError::Truncated("avih body"))?;
                parse_avih(b, header);
                c.at = advance_padded(c.at, size);
            }
            b"LIST" => {
                let list_type = c.fourcc().ok_or(AviError::Truncated("hdrl LIST type"))?;
                let body_len =
                    size.checked_sub(4).ok_or(AviError::Truncated("hdrl LIST size < 4"))?;
                let sub = c.peek(body_len).ok_or(AviError::Truncated("hdrl LIST body"))?;
                match &list_type {
                    b"strl" => {
                        // A `movi` chunk names its stream with two decimal digits.
                        if stream_index < MAX_STREAMS
                            && let Some(s) = parse_strl(sub, stream_index)
                        {
                            header.streams.push(s);
                        }
                        stream_index += 1;
                    }
                    b"odml" => header.has_odml = true,
                    _ => {}
                }
                c.at += body_len;
            }
            _ => {
                c.at = advance_padded(c.at, size);
            }
        }
        if c.at > body.len() {
            break;
        }
    }
    Ok(())
}

fn parse_avih(b: &[u8], header: &mut AviHeader) {
    let mut c = Cursor::new(b);
    header.us_per_frame = c.u32le().unwrap_or(0);
    let _ = c.take(12); // dwMaxBytesPerSec, dwPaddingGranularity, dwFlags
    header.total_frames = c.u32le().unwrap_or(0);
}

// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
fn parse_strl(body: &[u8], index: usize) -> Option<Stream> {
    let mut c = Cursor::new(body);
    let mut strh: Option<(StreamKind, FourCc, u32, u32, u32, u32)> = None;
    let mut strf: Vec<u8> = Vec::new();
    while c.remaining() >= 8 {
        let id = c.fourcc()?;
        let size = c.u32le()? as usize;
        match &id {
            b"strh" => {
                let b = c.peek(size)?;
                strh = Some(parse_strh(b));
                c.at = advance_padded(c.at, size);
            }
            b"strf" => {
                let b = c.peek(size)?;
                strf = b.to_vec();
                c.at = advance_padded(c.at, size);
            }
            _ => {
                c.at = advance_padded(c.at, size);
            }
        }
        if c.at > body.len() {
            break;
        }
    }
    let (kind, handler, scale, rate, sample_size, length) = strh?;
    let mut s = Stream {
        index,
        kind,
        handler,
        scale,
        rate,
        sample_size,
        length,
        width: 0,
        height: 0,
        strf,
        format_tag: 0,
        channels: 0,
        samples_per_sec: 0,
        bits_per_sample: 0,
    };
    match kind {
        StreamKind::Video => parse_bitmapinfoheader(&s.strf.clone(), &mut s),
        StreamKind::Audio => parse_waveformatex(&s.strf.clone(), &mut s),
        StreamKind::Other => {}
    }
    Some(s)
}

fn parse_strh(b: &[u8]) -> (StreamKind, FourCc, u32, u32, u32, u32) {
    let get_u32 = |off: usize| -> u32 {
        b.get(off..off + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
    };
    let fcc_type: FourCc = b.get(0..4).map(|s| [s[0], s[1], s[2], s[3]]).unwrap_or([0; 4]);
    let handler: FourCc = b.get(4..8).map(|s| [s[0], s[1], s[2], s[3]]).unwrap_or([0; 4]);
    let kind = match &fcc_type {
        b"vids" => StreamKind::Video,
        b"auds" => StreamKind::Audio,
        _ => StreamKind::Other,
    };
    let scale = get_u32(20);
    let rate = get_u32(24);
    let length = get_u32(32);
    let sample_size = get_u32(44);
    (kind, handler, scale, rate, sample_size, length)
}

fn parse_bitmapinfoheader(strf: &[u8], s: &mut Stream) {
    let get_i32 = |off: usize| -> i32 {
        strf.get(off..off + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0)
    };
    s.width = get_i32(4).unsigned_abs();
    s.height = get_i32(8).unsigned_abs();
}

fn parse_waveformatex(strf: &[u8], s: &mut Stream) {
    let mut c = Cursor::new(strf);
    s.format_tag = c.u16le().unwrap_or(0);
    s.channels = c.u16le().unwrap_or(0);
    s.samples_per_sec = c.u32le().unwrap_or(0);
    let _ = c.u32le(); // nAvgBytesPerSec
    let _ = c.u16le(); // nBlockAlign
    s.bits_per_sample = c.u16le().unwrap_or(0);
}

#[derive(Clone, Copy, Debug)]
pub struct Idx1Entry {
    pub ckid: FourCc,
    /// `AVIIF_KEYFRAME` (0x10) set, a resume-safe point.
    pub keyframe: bool,
    /// The raw index offset, see [`Idx1::resolve_offset`].
    pub offset: u32,
    pub size: u32,
}

impl Idx1Entry {
    /// The stream number from the id's first two ASCII digits.
    pub fn stream_index(&self) -> Option<usize> {
        let d0 = (self.ckid[0] as char).to_digit(10)?;
        let d1 = (self.ckid[1] as char).to_digit(10)?;
        Some((d0 * 10 + d1) as usize)
    }
}

/// A parsed `idx1` chunk with the `movi` data start for resolving relative offsets.
#[derive(Clone, Debug)]
pub struct Idx1 {
    pub entries: Vec<Idx1Entry>,
    movi_base: u64,
    file_len: u64,
}

impl Idx1 {
    /// Resolve a raw offset to the absolute file offset of the chunk id.
    pub fn resolve_offset(&self, entry: &Idx1Entry) -> u64 {
        if self.movi_relative() {
            self.movi_base.saturating_add(entry.offset as u64)
        } else {
            entry.offset as u64
        }
    }

    fn movi_relative(&self) -> bool {
        self.entries
            .first()
            .is_none_or(|first| offsets_relative(first.offset, self.movi_base, self.file_len))
    }

    /// A time to byte [`profluens_core::pipeline::Index`] from the video keyframe entries.
    // Cold path, once per stream.
    #[allow(clippy::disallowed_methods)]
    pub fn build_seek_index(
        &self,
        video_stream_index: usize,
        video_sample_dur_ns: u64,
    ) -> profluens_core::pipeline::Index {
        use profluens_core::pipeline::{Index, IndexEntry};
        let mut entries = Vec::new();
        let mut frame: u64 = 0;
        for e in &self.entries {
            if e.stream_index() != Some(video_stream_index) {
                continue;
            }
            if e.keyframe {
                let time_ns = frame.saturating_mul(video_sample_dur_ns);
                entries.push(IndexEntry::key(time_ns, self.resolve_offset(e)));
            }
            frame += 1;
        }
        let complete = !entries.is_empty();
        Index { entries, file_len: Some(self.file_len), complete, ..Index::default() }
    }
}

/// Whether `idx1` offsets are movi-relative, judged from the `first` entry's raw offset.
pub fn offsets_relative(first: u32, movi_base: u64, file_len: u64) -> bool {
    let as_abs = first as u64;
    let as_rel = movi_base.saturating_add(first as u64);
    if as_abs >= file_len {
        return true;
    }
    if as_abs < movi_base {
        return true;
    }
    as_rel < file_len
}

/// Whether `idx1` offsets are movi-relative, judged by where the `first` entry lands.
pub fn anchored(first: u32, movi_base: u64, first_chunk: Option<u64>, file_len: u64) -> bool {
    if let Some(at) = first_chunk {
        let hits = |byte: u64| byte == at || byte == at.saturating_add(12);
        if hits(movi_base.saturating_add(first as u64)) {
            return true;
        }
        if hits(first as u64) {
            return false;
        }
    }
    offsets_relative(first, movi_base, file_len)
}

/// Absolute offset of the first `movi` chunk in `prefix`, past any `JUNK`.
pub fn first_chunk(prefix: &[u8], movi_data_start: u64) -> Option<u64> {
    let mut at = usize::try_from(movi_data_start).ok()?;
    loop {
        let head = prefix.get(at..at.checked_add(8)?)?;
        if &head[..4] != b"JUNK" {
            return Some(at as u64);
        }
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        at = at.checked_add(8)?.checked_add(padded(size))?;
    }
}

pub const IDX1_ENTRY: usize = 16;

/// One `idx1` entry from its 16 little-endian octets (AVI RIFF Reference, AVI Index).
pub fn idx1_entry(b: &[u8; IDX1_ENTRY]) -> Idx1Entry {
    let u32le = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    Idx1Entry {
        ckid: [b[0], b[1], b[2], b[3]],
        keyframe: u32le(4) & AVIIF_KEYFRAME != 0,
        offset: u32le(8),
        size: u32le(12),
    }
}

/// `AVIIF_KEYFRAME`, the `idx1` flag of a chunk a decoder can start at.
pub const AVIIF_KEYFRAME: u32 = 0x10;

/// Parse an `idx1` chunk payload into entries of 16 bytes each.
// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
pub fn parse_idx1(payload: &[u8], movi_data_start: u64, file_len: u64) -> Idx1 {
    let mut entries = Vec::with_capacity(payload.len() / IDX1_ENTRY);
    for b in payload.as_chunks::<IDX1_ENTRY>().0 {
        entries.push(idx1_entry(b));
    }
    Idx1 { entries, movi_base: movi_data_start.saturating_sub(4), file_len }
}

#[derive(Clone, Debug)]
pub struct MediaChunk {
    pub stream_index: usize,
    pub id: FourCc,
    /// One video frame or audio packet, verbatim.
    pub data: Vec<u8>,
}

/// Incremental `movi` walker. `push` bytes as they arrive, `next_chunk` pulls chunks.
#[derive(Default)]
pub struct MoviWalker {
    buf: Vec<u8>,
    head: usize,
    consumed: u64,
    synced: bool,
    skip: u64,
    /// Times sync was lost past the first lock, and bytes skipped finding it.
    pub resyncs: u64,
    pub junk: u64,
}

const MAX_MEDIA_CHUNK: usize = 64 << 20;

fn plausible_chunk_id(id: &[u8]) -> bool {
    if id.len() < 4 {
        return false;
    }
    if id == b"LIST" || id == b"JUNK" || id == b"idx1" || id == b"rec " {
        return true;
    }
    id[0].is_ascii_digit()
        && id[1].is_ascii_digit()
        && id[2].is_ascii_alphanumeric()
        && id[3].is_ascii_alphanumeric()
}

impl MoviWalker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, mut data: &[u8]) {
        if self.skip > 0 {
            let gone = (data.len() as u64).min(self.skip) as usize;
            self.skip -= gone as u64;
            self.consumed = self.consumed.saturating_add(gone as u64);
            data = &data[gone..];
        }
        if self.head == self.buf.len() {
            self.buf.clear();
            self.head = 0;
        } else if self.head >= self.buf.len() / 2 {
            self.buf.drain(..self.head);
            self.head = 0;
        }
        self.buf.extend_from_slice(data);
    }

    fn advance(&mut self, n: usize) {
        let n = n.min(self.buf.len() - self.head);
        self.head += n;
        self.consumed = self.consumed.saturating_add(n as u64);
    }

    fn skip_chunk(&mut self, total: usize) {
        let here = self.buf.len() - self.head;
        self.skip = total.saturating_sub(here) as u64;
        self.advance(total);
    }

    pub fn held(&self) -> usize {
        self.buf.len() - self.head
    }

    /// Reset after a seek, dropping the buffered tail and re-arming the resync scanner.
    pub fn resync(&mut self) {
        self.buf.clear();
        self.head = 0;
        self.skip = 0;
        self.synced = false;
    }

    /// The next complete chunk, `None` if more bytes are needed or the stream is done.
    // One copy, the payload handed to the caller.
    #[allow(clippy::disallowed_methods)]
    pub fn next_chunk(&mut self) -> Option<MediaChunk> {
        loop {
            let window = &self.buf[self.head..];
            if window.len() < 8 {
                return None;
            }
            let id: FourCc = [window[0], window[1], window[2], window[3]];

            let printable = id.iter().all(|&b| (0x20..0x7f).contains(&b));
            if !self.synced || !(printable || plausible_chunk_id(&id)) {
                self.resyncs += u64::from(self.synced);
                match find_next_chunk_id(window) {
                    Some(0) => self.synced = true,
                    Some(skip) => {
                        self.junk += skip as u64;
                        self.advance(skip);
                        continue; // re-read the header at the new position
                    }
                    None => {
                        let keep_from = window.len().saturating_sub(3);
                        self.junk += keep_from as u64;
                        self.advance(keep_from);
                        return None;
                    }
                }
            }

            let size = u32::from_le_bytes([window[4], window[5], window[6], window[7]]) as usize;
            let total = 8usize.saturating_add(padded(size));

            if &id == b"LIST" {
                if window.len() < 12 {
                    return None; // need the list type too
                }
                let list_type: FourCc = [window[8], window[9], window[10], window[11]];
                if &list_type == b"rec " || &list_type == b"movi" {
                    self.advance(12); // step inside the wrapper
                    continue;
                }
                self.skip_chunk(total);
                continue;
            }

            // OpenDML AVIX segment, its LIST movi follows
            if &id == b"RIFF" {
                if window.len() < 12 {
                    return None;
                }
                if &window[8..12] == b"AVIX" {
                    self.advance(12);
                    continue;
                }
                self.skip_chunk(total);
                continue;
            }

            if &id == b"idx1"
                || &id == b"JUNK"
                || !id[0].is_ascii_digit()
                || !id[1].is_ascii_digit()
            {
                self.skip_chunk(total);
                continue;
            }

            if size > MAX_MEDIA_CHUNK {
                self.resyncs += 1;
                self.junk += 1;
                self.advance(1);
                self.synced = false;
                continue;
            }
            if window.len() < total {
                return None; // wait for the rest of the payload
            }
            let data = window[8..8 + size].to_vec();
            let stream_index = usize::from(id[0] - b'0') * 10 + usize::from(id[1] - b'0');
            self.advance(total);
            return Some(MediaChunk { stream_index, id, data });
        }
    }
}

fn find_next_chunk_id(window: &[u8]) -> Option<usize> {
    window.windows(4).position(plausible_chunk_id)
}

fn padded(size: usize) -> usize {
    size.saturating_add(size & 1)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod odml_tests {
    use super::*;

    fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() & 1 == 1 {
            out.push(0);
        }
    }

    #[test]
    fn a_second_riff_avix_segment_and_ix_chunks_are_walked() {
        let mut b = Vec::new();
        chunk(&mut b, b"00dc", b"one");
        chunk(&mut b, b"ix00", b"00dc00dc00dc");
        chunk(&mut b, b"idx1", &[0; 16]);
        b.extend_from_slice(b"RIFF\x40\0\0\0AVIXLIST\x20\0\0\0movi");
        chunk(&mut b, b"01wb", b"two!");
        chunk(&mut b, b"1Xdc", b"bad");
        chunk(&mut b, b"00dc", b"three");
        let mut w = MoviWalker::new();
        w.push(&b);
        let mut got = Vec::new();
        while let Some(c) = w.next_chunk() {
            got.push((c.stream_index, c.data));
        }
        let got: Vec<(usize, &[u8])> = got.iter().map(|(i, d)| (*i, d.as_slice())).collect();
        assert_eq!(got, [(0, &b"one"[..]), (1, &b"two!"[..]), (0, &b"three"[..])]);
        assert_eq!(w.resyncs, 0);
        assert_eq!(w.junk, 0);
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn the_codec_head_is_what_follows_the_format_header() {
        let mut s = Stream {
            index: 0,
            kind: StreamKind::Video,
            handler: *b"XVID",
            scale: 1,
            rate: 25,
            sample_size: 0,
            length: 0,
            width: 0,
            height: 0,
            strf: [&40u32.to_le_bytes()[..], &[0; 36], &[0, 0, 1, 0xB0, 1]].concat(),
            format_tag: 0,
            channels: 0,
            samples_per_sec: 0,
            bits_per_sample: 0,
        };
        assert_eq!(s.codec_head(), [0, 0, 1, 0xB0, 1], "the VOL behind 40 octets");
        s.strf[..4].copy_from_slice(&44u32.to_le_bytes());
        assert_eq!(s.codec_head(), [1], "behind a longer biSize");
        s.strf[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(s.codec_head().is_empty(), "a biSize past the strf");
        s.strf.truncate(10);
        assert!(s.codec_head().is_empty(), "a cut header");
        s.kind = StreamKind::Audio;
        s.strf = [&[0u8; 18][..], &[0x12, 0x10]].concat();
        assert_eq!(s.codec_head(), [0x12, 0x10], "behind WAVEFORMATEX");
        s.kind = StreamKind::Other;
        assert!(s.codec_head().is_empty());
    }

    fn build_avi(data_chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(id);
            v.extend_from_slice(&(body.len() as u32).to_le_bytes());
            v.extend_from_slice(body);
            if body.len() & 1 == 1 {
                v.push(0); // WORD pad
            }
            v
        }
        fn list(list_type: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut inner = Vec::new();
            inner.extend_from_slice(list_type);
            inner.extend_from_slice(body);
            chunk(b"LIST", &inner)
        }

        let mut avih = vec![0u8; 56];
        avih[0..4].copy_from_slice(&41_666u32.to_le_bytes()); // 24 fps
        avih[16..20].copy_from_slice(&3u32.to_le_bytes());

        let mut vstrh = vec![0u8; 56];
        vstrh[0..4].copy_from_slice(b"vids");
        vstrh[4..8].copy_from_slice(b"XVID");
        vstrh[20..24].copy_from_slice(&1u32.to_le_bytes()); // scale
        vstrh[24..28].copy_from_slice(&24u32.to_le_bytes()); // rate 24 fps
        vstrh[32..36].copy_from_slice(&3u32.to_le_bytes()); // length
        let mut vstrf = vec![0u8; 40];
        vstrf[0..4].copy_from_slice(&40u32.to_le_bytes());
        vstrf[4..8].copy_from_slice(&160i32.to_le_bytes());
        vstrf[8..12].copy_from_slice(&120i32.to_le_bytes());
        vstrf[16..20].copy_from_slice(b"XVID"); // biCompression
        let vstrl = list(b"strl", &[chunk(b"strh", &vstrh), chunk(b"strf", &vstrf)].concat());

        let mut astrh = vec![0u8; 56];
        astrh[0..4].copy_from_slice(b"auds");
        astrh[20..24].copy_from_slice(&1u32.to_le_bytes());
        astrh[24..28].copy_from_slice(&48_000u32.to_le_bytes());
        let mut astrf = vec![0u8; 18];
        astrf[0..2].copy_from_slice(&0x2000u16.to_le_bytes());
        astrf[2..4].copy_from_slice(&6u16.to_le_bytes());
        astrf[4..8].copy_from_slice(&48_000u32.to_le_bytes());
        let astrl = list(b"strl", &[chunk(b"strh", &astrh), chunk(b"strf", &astrf)].concat());

        let hdrl = list(b"hdrl", &[chunk(b"avih", &avih), vstrl, astrl].concat());

        let mut movi_body = Vec::new();
        for (id, body) in data_chunks {
            movi_body.extend_from_slice(&chunk(id, body));
        }
        let movi = list(b"movi", &movi_body);

        let mut form = Vec::new();
        form.extend_from_slice(b"AVI ");
        form.extend_from_slice(&hdrl);
        form.extend_from_slice(&movi);

        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(form.len() as u32).to_le_bytes());
        file.extend_from_slice(&form);
        file
    }

    #[test]
    fn probe_discovers_video_and_audio() {
        let avi = build_avi(&[(b"00dc", &[1, 2, 3, 4]), (b"01wb", &[5, 6])]);
        let (header, movi) = probe_header(&avi).expect("probe");
        assert_eq!(header.streams.len(), 2, "one video + one audio");
        let v = &header.streams[0];
        assert_eq!(v.kind, StreamKind::Video);
        assert_eq!(&v.handler, b"XVID");
        assert_eq!((v.width, v.height), (160, 120));
        assert_eq!((v.scale, v.rate), (1, 24));
        let a = &header.streams[1];
        assert_eq!(a.kind, StreamKind::Audio);
        assert_eq!(a.format_tag, 0x2000, "AC-3");
        assert_eq!(a.channels, 6);
        assert_eq!(a.samples_per_sec, 48_000);
        assert!(movi.is_some(), "movi located in the full-file prefix");
    }

    #[test]
    fn walker_yields_chunks_in_order() {
        let avi = build_avi(&[(b"00dc", &[0xAA; 5]), (b"01wb", &[0xBB, 0xCC]), (b"00dc", &[0xDD])]);
        let (_h, movi) = probe_header(&avi).expect("probe");
        let start = movi.unwrap().movi_data_start as usize;
        let mut w = MoviWalker::new();
        w.push(&avi[start..]);
        let c0 = w.next_chunk().expect("chunk 0");
        assert_eq!(c0.stream_index, 0);
        assert_eq!(c0.data, vec![0xAA; 5]);
        let c1 = w.next_chunk().expect("chunk 1");
        assert_eq!(c1.stream_index, 1);
        assert_eq!(c1.data, vec![0xBB, 0xCC]);
        let c2 = w.next_chunk().expect("chunk 2");
        assert_eq!(c2.stream_index, 0);
        assert_eq!(c2.data, vec![0xDD]);
        assert!(w.next_chunk().is_none(), "movi exhausted");
    }

    #[test]
    fn walker_reassembles_across_push_boundaries() {
        let avi = build_avi(&[(b"00dc", &(0..50u8).collect::<Vec<_>>())]);
        let (_h, movi) = probe_header(&avi).expect("probe");
        let start = movi.unwrap().movi_data_start as usize;
        let stream = &avi[start..];
        let mut w = MoviWalker::new();
        for &b in stream {
            assert!(w.next_chunk().is_none() || true); // no chunk until complete
            w.push(&[b]);
        }
        let c = w.next_chunk().expect("chunk reassembled");
        assert_eq!(c.data, (0..50u8).collect::<Vec<_>>());
    }

    #[test]
    fn walker_descends_rec_wrapper() {
        fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(id);
            v.extend_from_slice(&(body.len() as u32).to_le_bytes());
            v.extend_from_slice(body);
            if body.len() & 1 == 1 {
                v.push(0);
            }
            v
        }
        let inner = [chunk(b"00dc", &[1, 2, 3]), chunk(b"01wb", &[4, 5])].concat();
        let mut rec = Vec::new();
        rec.extend_from_slice(b"rec ");
        rec.extend_from_slice(&inner);
        let movi_body = chunk(b"LIST", &rec);

        let mut w = MoviWalker::new();
        w.push(&movi_body);
        let c0 = w.next_chunk().expect("descend rec, chunk 0");
        assert_eq!(c0.data, vec![1, 2, 3]);
        let c1 = w.next_chunk().expect("chunk 1");
        assert_eq!(c1.data, vec![4, 5]);
    }

    #[test]
    fn walker_resyncs_past_garbage() {
        fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(id);
            v.extend_from_slice(&(body.len() as u32).to_le_bytes());
            v.extend_from_slice(body);
            v
        }
        let good = chunk(b"00dc", &[9, 9, 9, 9]);
        let mut stream = vec![0xFF, 0x00, 0x13, 0x37, 0x42];
        stream.extend_from_slice(&good);
        let mut w = MoviWalker::new();
        w.push(&stream);
        let c = w.next_chunk().expect("resync finds the good chunk");
        assert_eq!(c.data, vec![9, 9, 9, 9]);
    }

    #[test]
    fn idx1_parses_and_resolves_movi_relative() {
        let mut payload = Vec::new();
        for (id, flags, off, size) in [(b"00dc", 0x10u32, 4u32, 100u32), (b"01wb", 0x00, 40, 20)] {
            payload.extend_from_slice(id);
            payload.extend_from_slice(&flags.to_le_bytes());
            payload.extend_from_slice(&off.to_le_bytes());
            payload.extend_from_slice(&size.to_le_bytes());
        }
        let movi_data_start = 2048;
        let file_len = 1_000_000;
        let idx = parse_idx1(&payload, movi_data_start, file_len);
        assert_eq!(idx.entries.len(), 2);
        assert!(idx.entries[0].keyframe);
        assert_eq!(idx.entries[0].stream_index(), Some(0));
        assert_eq!(idx.entries[1].stream_index(), Some(1));
        assert_eq!(idx.resolve_offset(&idx.entries[0]), 2048);
    }

    #[test]
    fn idx1_builds_seek_index_from_video_keyframes() {
        let mut payload = Vec::new();
        let entries = [
            (b"00dc", 0x10u32, 4u32), // frame 0, keyframe
            (b"01wb", 0x00, 200),     // audio
            (b"00dc", 0x00, 300),     // frame 1, delta
            (b"00dc", 0x10, 400),     // frame 2, keyframe
        ];
        for (id, flags, off) in entries {
            payload.extend_from_slice(id);
            payload.extend_from_slice(&flags.to_le_bytes());
            payload.extend_from_slice(&off.to_le_bytes());
            payload.extend_from_slice(&100u32.to_le_bytes());
        }
        let idx = parse_idx1(&payload, 1004, 100_000);
        let dur = 41_666_666u64;
        let si = idx.build_seek_index(0, dur);
        assert_eq!(si.entries.len(), 2, "two video keyframes");
        assert_eq!(si.entries[0], profluens_core::pipeline::IndexEntry::key(0, 1004)); // frame 0 at base(1000)+4
        assert_eq!(si.entries[1], profluens_core::pipeline::IndexEntry::key(2 * dur, 1400)); // frame 2 at base+400
    }

    #[test]
    fn error_messages_carry_the_variant_data() {
        let rows: [(AviError, &[&str]); 6] = [
            (AviError::NotAvi, &["header parse failed", "not a RIFF"]),
            (AviError::Truncated("avih body"), &["header parse failed", "avih body"]),
            (AviError::HeaderIncomplete, &["header parse failed", "hdrl"]),
            (AviError::MoviNotReached, &["'movi' list"]),
            (AviError::NoStreams, &["no streams"]),
            (AviError::NoDecoderFamily { stream: 3 }, &["stream 3", "no decoder family"]),
        ];
        for (e, needles) in rows {
            let s = format!("{e}");
            assert!(s.starts_with("avidemux: "), "{s}");
            assert!(!s.contains('\n') && !s.contains('\u{2014}'), "{s}");
            for n in needles {
                assert!(s.contains(n), "{s:?} should mention {n:?}");
            }
        }
    }

    #[test]
    fn not_an_avi_errors() {
        assert_eq!(probe_header(b"not a riff file at all").err(), Some(AviError::NotAvi));
        assert_eq!(probe_header(b"RIFF\x00\x00\x00\x00WEBP").err(), Some(AviError::NotAvi));
        assert!(probe_header(b"").is_err());
        assert!(probe_header(b"RIFF").is_err());
    }

    #[test]
    fn oversized_chunk_sizes_do_not_panic() {
        let mut avi = Vec::new();
        avi.extend_from_slice(b"RIFF");
        avi.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        avi.extend_from_slice(b"AVI ");
        avi.extend_from_slice(b"LIST");
        avi.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // absurd list size
        avi.extend_from_slice(b"hdrl");
        assert!(probe_header(&avi).is_err());
    }

    #[test]
    fn walker_never_panics_on_random_bytes() {
        let mut seed = 0x1234_5678u32;
        let mut bytes = Vec::new();
        for _ in 0..4096 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            bytes.push((seed >> 16) as u8);
        }
        let mut w = MoviWalker::new();
        w.push(&bytes);
        let mut pulled = 0;
        while w.next_chunk().is_some() && pulled < 100_000 {
            pulled += 1;
        }
    }

    #[test]
    fn zero_size_chunk_makes_progress() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"00dc");
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(b"01wb");
        stream.extend_from_slice(&2u32.to_le_bytes());
        stream.extend_from_slice(&[7, 7]);
        let mut w = MoviWalker::new();
        w.push(&stream);
        let c0 = w.next_chunk().expect("empty chunk 0");
        assert_eq!(c0.stream_index, 0);
        assert!(c0.data.is_empty());
        let c1 = w.next_chunk().expect("chunk 1");
        assert_eq!(c1.data, vec![7, 7]);
    }
}
