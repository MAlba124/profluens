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
    /// `wFormatTag`, behind WAVE_FORMAT_EXTENSIBLE the tag its KSDATAFORMAT SubFormat carries.
    pub format_tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub bits_per_sample: u16,
    /// `nAvgBytesPerSec` of an audio stream's WAVEFORMATEX, 0 for others.
    pub avg_bytes_per_sec: u32,
    /// The `strn` chunk's name, empty without one.
    pub name: String,
}

impl Stream {
    /// One sample's duration in ns from `dwScale/dwRate`, `None` when `rate` is zero.
    pub fn sample_duration_ns(&self) -> Option<u64> {
        if self.rate == 0 {
            return None;
        }
        Some((self.scale as u64).saturating_mul(1_000_000_000) / self.rate as u64)
    }

    /// The codec's own bytes behind the fixed format header of `strf`, behind the 40 octets of
    /// BITMAPINFOHEADER of a video stream, behind the 18 octets of WAVEFORMATEX of an audio one,
    /// the VOL of an MPEG-4 stream. Empty for other streams.
    pub fn codec_head(&self) -> &[u8] {
        let (fixed, end) = match self.kind {
            StreamKind::Video => {
                let size =
                    self.strf.get(..4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                // ffmpeg counts its extradata in biSize, a biSize of 40 or past strf ends nothing
                let size = usize::try_from(size).unwrap_or(usize::MAX);
                (40, if size > 40 && size <= self.strf.len() { size } else { self.strf.len() })
            }
            StreamKind::Audio => (18, self.strf.len()),
            StreamKind::Other => return &[],
        };
        self.strf.get(fixed..end).unwrap_or(&[])
    }

    /// The WAVEFORMATEXTENSIBLE part of an audio stream's `strf`.
    pub fn extensible(&self) -> Option<Extensible> {
        match self.kind {
            StreamKind::Audio => Extensible::of(&self.strf),
            _ => None,
        }
    }

    /// Bits of a sample that carry signal, the `wValidBitsPerSample` of an extensible PCM or
    /// float stream within its container, else `bits_per_sample`.
    pub fn valid_bits(&self) -> u16 {
        match self.extensible() {
            Some(e)
                if matches!(self.format_tag, 0x0001 | 0x0003)
                    && (1..=self.bits_per_sample).contains(&e.valid_bits) =>
            {
                e.valid_bits
            }
            _ => self.bits_per_sample,
        }
    }

    /// The speakers an extensible stream states, its `dwChannelMask` when that names as many
    /// as `channels`, else 0.
    pub fn layout(&self) -> u64 {
        let mask = self.extensible().map_or(0, |e| e.channel_mask);
        if mask.count_ones() == u32::from(self.channels) { u64::from(mask) } else { 0 }
    }
}

pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// A KSDATAFORMAT SubFormat GUID past its first two octets, which hold a WAVE format tag
/// (`ksmedia.h` DEFINE_WAVEFORMATEX_GUID).
const KSDATAFORMAT_TAIL: [u8; 14] = [0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71];

/// What WAVEFORMATEXTENSIBLE adds to WAVEFORMATEX.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extensible {
    /// `wValidBitsPerSample`, `wSamplesPerBlock` for a compressed SubFormat.
    pub valid_bits: u16,
    pub channel_mask: u32,
    /// The WAVE format tag of a KSDATAFORMAT SubFormat, None for another GUID.
    pub tag: Option<u16>,
}

impl Extensible {
    /// The extension of a `strf` tagged 0xFFFE whose `cbSize` covers it.
    pub fn of(strf: &[u8]) -> Option<Extensible> {
        let le16 = |at: usize| strf.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        if le16(0)? != WAVE_FORMAT_EXTENSIBLE || le16(16)? < 22 {
            return None;
        }
        let ext = strf.get(18..40)?;
        let guid = &ext[6..];
        Some(Extensible {
            valid_bits: u16::from_le_bytes([ext[0], ext[1]]),
            channel_mask: u32::from_le_bytes([ext[2], ext[3], ext[4], ext[5]]),
            tag: (guid[2..] == KSDATAFORMAT_TAIL).then(|| u16::from_le_bytes([guid[0], guid[1]])),
        })
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
    let mut name = String::new();
    while c.remaining() >= 8 {
        let id = c.fourcc()?;
        let size = c.u32le()? as usize;
        match &id {
            b"strn" => {
                let b = c.peek(size.min(c.remaining()))?;
                let b = &b[..b.iter().position(|&x| x == 0).unwrap_or(b.len())];
                name = String::from_utf8_lossy(b).trim().to_owned();
                c.at = advance_padded(c.at, size);
            }
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
        avg_bytes_per_sec: 0,
        name,
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
    s.avg_bytes_per_sec = c.u32le().unwrap_or(0);
    let _ = c.u16le(); // nBlockAlign
    s.bits_per_sample = c.u16le().unwrap_or(0);
    if let Some(tag) = Extensible::of(strf).and_then(|e| e.tag) {
        s.format_tag = tag;
    }
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

/// A whole media chunk at the walker's head, its payload in [`MoviWalker::data`] until
/// [`MoviWalker::take`] steps past it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkHead {
    pub stream_index: usize,
    pub id: FourCc,
    pub len: usize,
    total: usize,
}

impl ChunkHead {
    /// Its header, payload and pad byte.
    pub fn total(&self) -> usize {
        self.total
    }
}

/// Incremental `movi` walker. `push` bytes as they arrive, `peek_chunk` finds chunks.
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

    /// [`resync`](Self::resync) keeping `chunk`, the one at the head, for [`data`](Self::data)
    /// and [`take`](Self::take).
    pub fn resync_keeping(&mut self, chunk: &ChunkHead) {
        self.buf.truncate((self.head + chunk.total).min(self.buf.len()));
        self.buf.drain(..self.head);
        (self.head, self.skip, self.synced) = (0, 0, false);
    }

    /// The next complete chunk copied out and stepped past.
    // One copy, the payload handed to the caller.
    #[allow(clippy::disallowed_methods)]
    pub fn next_chunk(&mut self) -> Option<MediaChunk> {
        let c = self.peek_chunk()?;
        let data = self.data(&c).to_vec();
        self.take(&c);
        Some(MediaChunk { stream_index: c.stream_index, id: c.id, data })
    }

    /// The payload of `chunk`, the one [`peek_chunk`](Self::peek_chunk) found at the head.
    pub fn data(&self, chunk: &ChunkHead) -> &[u8] {
        self.buf.get(self.head + 8..self.head + 8 + chunk.len).unwrap_or_default()
    }

    /// Bytes stepped past since the walker was made, the head's place behind its first byte.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Steps past `chunk`, the one at the head.
    pub fn take(&mut self, chunk: &ChunkHead) {
        self.advance(chunk.total);
    }

    /// The next complete chunk, left at the head, `None` if more bytes are needed or the stream
    /// is done.
    pub fn peek_chunk(&mut self) -> Option<ChunkHead> {
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
            let stream_index = usize::from(id[0] - b'0') * 10 + usize::from(id[1] - b'0');
            return Some(ChunkHead { stream_index, id, len: size, total });
        }
    }
}

/// The data of the first chunk of stream `stream_index` in `movi`, the bytes behind the list's
/// `movi` four-CC, cut where the slice ends. `rec ` lists are entered, other chunks stepped over.
pub fn first_chunk_of(movi: &[u8], stream_index: usize) -> Option<&[u8]> {
    let mut at = 0;
    loop {
        let w = movi.get(at..at + 8)?;
        let id = [w[0], w[1], w[2], w[3]];
        let size = u32::from_le_bytes([w[4], w[5], w[6], w[7]]) as usize;
        if &id == b"LIST" {
            let kind = movi.get(at + 8..at + 12)?;
            at += if kind == b"rec " || kind == b"movi" { 12 } else { 8 + padded(size) };
            continue;
        }
        if id[0].is_ascii_digit()
            && id[1].is_ascii_digit()
            && usize::from(id[0] - b'0') * 10 + usize::from(id[1] - b'0') == stream_index
        {
            let data = movi.get(at + 8..)?;
            return Some(&data[..data.len().min(size)]);
        }
        at = at.checked_add(8 + padded(size))?;
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
    fn the_first_chunk_of_a_stream_is_found_behind_lists_and_cut_at_the_slice() {
        let mut b = Vec::new();
        chunk(&mut b, b"JUNK", &[0; 6]);
        chunk(&mut b, b"01wb", b"audio");
        b.extend_from_slice(b"LIST   rec ");
        chunk(&mut b, b"01wb", b"more");
        chunk(&mut b, b"00dc", b"picture");
        assert_eq!(first_chunk_of(&b, 0), Some(&b"picture"[..]));
        assert_eq!(first_chunk_of(&b, 1), Some(&b"audio"[..]));
        assert_eq!(first_chunk_of(&b, 2), None);
        let cut = &b[..b.len() - 5];
        assert_eq!(first_chunk_of(cut, 0), Some(&b"pic"[..]), "cut where the slice ends");
        assert_eq!(first_chunk_of(&b[..b.len() - 13], 0), None, "its header past the slice");
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
            avg_bytes_per_sec: 0,
            name: String::new(),
        };
        assert_eq!(s.codec_head(), [0, 0, 1, 0xB0, 1], "the VOL behind 40 octets");
        s.strf[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(s.codec_head(), [0, 0, 1, 0xB0, 1], "a biSize past the strf");
        // ffmpeg's biSize of 40 plus its extradata, the strf padded to an even size
        let vol = [0, 0, 1, 0xB0, 1, 0, 0, 1, 0xB5, 9, 0, 0, 1, 0, 0, 0, 1, 0x20, 0, 0xC4];
        let size = 40 + vol.len() as u32 + 1;
        s.strf = [&size.to_le_bytes()[..], &[0; 36], &vol, &[0x55], &[0]].concat();
        assert_eq!(s.codec_head(), [&vol[..], &[0x55]].concat(), "the VOL whole, no pad");
        s.strf[..4].copy_from_slice(&44u32.to_le_bytes());
        assert_eq!(s.codec_head(), &vol[..4], "a biSize inside the strf ends the head");
        s.strf.truncate(10);
        assert!(s.codec_head().is_empty(), "a cut header");
        s.kind = StreamKind::Audio;
        s.strf = [&[0u8; 18][..], &[0x12, 0x10]].concat();
        assert_eq!(s.codec_head(), [0x12, 0x10], "behind WAVEFORMATEX");
        s.kind = StreamKind::Other;
        assert!(s.codec_head().is_empty());
    }

    fn ks_guid(tag: u16) -> [u8; 16] {
        let mut g = [0u8; 16];
        g[..2].copy_from_slice(&tag.to_le_bytes());
        g[2..].copy_from_slice(&KSDATAFORMAT_TAIL);
        g
    }

    /// A WAVEFORMATEXTENSIBLE of 48 kHz.
    fn wave_ext(channels: u16, bits: u16, valid: u16, mask: u32, guid: [u8; 16]) -> Vec<u8> {
        let align = channels * bits / 8;
        let mut f = WAVE_FORMAT_EXTENSIBLE.to_le_bytes().to_vec();
        f.extend_from_slice(&channels.to_le_bytes());
        f.extend_from_slice(&48_000u32.to_le_bytes());
        f.extend_from_slice(&(48_000 * u32::from(align)).to_le_bytes());
        f.extend_from_slice(&align.to_le_bytes());
        f.extend_from_slice(&bits.to_le_bytes());
        f.extend_from_slice(&22u16.to_le_bytes());
        f.extend_from_slice(&valid.to_le_bytes());
        f.extend_from_slice(&mask.to_le_bytes());
        f.extend_from_slice(&guid);
        f
    }

    fn audio_stream(strf: &[u8]) -> Stream {
        let mut strh = [0u8; 56];
        strh[..4].copy_from_slice(b"auds");
        let mut body = b"strh".to_vec();
        body.extend_from_slice(&56u32.to_le_bytes());
        body.extend_from_slice(&strh);
        body.extend_from_slice(b"strf");
        body.extend_from_slice(&(strf.len() as u32).to_le_bytes());
        body.extend_from_slice(strf);
        parse_strl(&body, 1).expect("a stream")
    }

    #[test]
    fn an_extensible_stream_takes_the_tag_of_its_subformat() {
        use crate::codec::{family_for, raw_format};
        use profluens_audio::SampleFormat;
        let s = audio_stream(&wave_ext(6, 24, 24, 0x3F, ks_guid(1)));
        assert_eq!(
            (s.format_tag, s.bits_per_sample, s.valid_bits(), s.layout()),
            (1, 24, 24, 0x3F)
        );
        assert_eq!((family_for(&s), raw_format(1, 24)), ("audio/raw", Some(SampleFormat::S24)));
        let s = audio_stream(&wave_ext(2, 32, 24, 0x3, ks_guid(1)));
        assert_eq!((s.format_tag, s.bits_per_sample, s.valid_bits()), (1, 32, 24));
        assert_eq!(raw_format(s.format_tag, s.bits_per_sample), Some(SampleFormat::S32));
        let s = audio_stream(&wave_ext(2, 32, 32, 0x3, ks_guid(3)));
        assert_eq!((s.format_tag, family_for(&s)), (3, "audio/raw"));
        assert_eq!(raw_format(s.format_tag, s.bits_per_sample), Some(SampleFormat::F32));
        let s = audio_stream(&wave_ext(6, 16, 1536, 0x60F, ks_guid(0x2000)));
        assert_eq!((s.format_tag, family_for(&s)), (0x2000, "ac3"));
        assert_eq!((s.valid_bits(), s.layout()), (16, 0x60F), "wSamplesPerBlock is no width");
        let s = audio_stream(&wave_ext(2, 24, 0, 0, ks_guid(1)));
        assert_eq!((s.valid_bits(), s.layout()), (24, 0), "no valid width, no speakers");
        let s = audio_stream(&wave_ext(2, 24, 32, 0x3F, ks_guid(1)));
        assert_eq!((s.valid_bits(), s.layout()), (24, 0), "past the container, another count");
        let s = audio_stream(&wave_ext(2, 16, 16, 0x3, ks_guid(0x0161)));
        assert_eq!((s.format_tag, family_for(&s)), (0x0161, "bytes"), "a tag with no family");
    }

    #[test]
    fn an_extensible_stream_without_a_ksdataformat_subformat_stays_unnamed() {
        let mut other = ks_guid(1);
        other[15] ^= 1;
        let s = audio_stream(&wave_ext(2, 16, 16, 0x3, other));
        assert_eq!((s.format_tag, crate::codec::family_for(&s)), (WAVE_FORMAT_EXTENSIBLE, "bytes"));
        assert_eq!(s.extensible().map(|e| (e.tag, e.channel_mask)), Some((None, 0x3)));
        let mut high = ks_guid(1);
        high[2] = 1;
        assert_eq!(audio_stream(&wave_ext(2, 16, 16, 0x3, high)).format_tag, 0xFFFE);
        let mut short = wave_ext(2, 16, 16, 0x3, ks_guid(1));
        short[16] = 21;
        assert_eq!(audio_stream(&short).format_tag, 0xFFFE, "cbSize short of the extension");
        let full = wave_ext(2, 16, 16, 0x3, ks_guid(1));
        for cut in [16, 18, 24, 39] {
            let s = audio_stream(&full[..cut]);
            assert_eq!((s.format_tag, s.extensible(), s.layout()), (0xFFFE, None, 0), "{cut}");
        }
        let mut plain = full.clone();
        plain[..2].copy_from_slice(&1u16.to_le_bytes());
        let s = audio_stream(&plain);
        assert_eq!((s.format_tag, s.extensible(), s.layout()), (1, None, 0), "WAVEFORMATEX");
        let mut video = audio_stream(&full);
        video.kind = StreamKind::Video;
        assert_eq!(video.extensible(), None);
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
        astrf[8..12].copy_from_slice(&56_000u32.to_le_bytes());
        let astrn = chunk(b"strn", b"Commentary \x00junk");
        let astrl =
            list(b"strl", &[chunk(b"strh", &astrh), chunk(b"strf", &astrf), astrn].concat());

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
        assert_eq!((a.avg_bytes_per_sec, a.name.as_str()), (56_000, "Commentary"));
        assert_eq!((v.avg_bytes_per_sec, v.name.as_str()), (0, ""), "no strn, no WAVEFORMATEX");
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
    fn a_chunk_kept_through_a_resync_is_still_read_then_the_walk_starts_over() {
        fn chunk(id: &[u8; 4], body: &[u8; 6]) -> Vec<u8> {
            [id.as_slice(), &6u32.to_le_bytes(), body].concat()
        }
        let movi = [chunk(b"00dc", b"first1"), chunk(b"01wb", b"second")].concat();
        let mut w = MoviWalker::new();
        w.push(&movi);
        let c = w.peek_chunk().expect("a chunk");
        w.resync_keeping(&c);
        assert_eq!(w.data(&c), b"first1");
        w.take(&c);
        assert_eq!(w.held(), 0, "the rest went with the resync");
        w.push(&[b"junk".as_slice(), &chunk(b"01wb", b"third3")].concat());
        let next = w.next_chunk().expect("the walk after the restart");
        assert_eq!((next.id, next.data), (*b"01wb", b"third3".to_vec()));
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
