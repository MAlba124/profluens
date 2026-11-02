//! The bytes a demuxer is built from. The `*_from` and `*_at` forms walk a window the caller
//! already read, so a growing or remote source is never read past what it has.

// App-side setup before any pipeline, one head buffer per open.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// Matroska ids, RFC 9559 section 5.1, as the 32 bit values of their octets.
const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const CLUSTER: u32 = 0x1F43_B675;
const TRACKS: u32 = 0x1654_AE6B;
/// Top level elements walked for the first Cluster.
const MKV_MAX_TOP: usize = 4096;
const MKV_HEAD_MAX: usize = 8 << 20;
const AVI_HEAD_MAX: usize = 16 << 20;
const MP4_WINDOW: usize = 64 << 10;

#[derive(Debug)]
pub enum HeadError {
    Io(io::Error),
    NoCluster,
    NoMovi,
}

impl fmt::Display for HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeadError::Io(e) => write!(f, "read: {e}"),
            HeadError::NoCluster => f.write_str(
                "no Cluster in the bytes read, not a streamable Matroska file or its head is not read yet",
            ),
            HeadError::NoMovi => f.write_str(
                "no 'movi' list in the bytes read, not an AVI file or its head is not read yet",
            ),
        }
    }
}

impl std::error::Error for HeadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HeadError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for HeadError {
    fn from(e: io::Error) -> Self {
        HeadError::Io(e)
    }
}

fn read_file_at(file: &mut File, off: u64, want: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(off))?;
    let mut buf = Vec::with_capacity(want);
    file.take(want as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn read_prefix(path: &str, want: usize) -> io::Result<Vec<u8>> {
    read_file_at(&mut File::open(path)?, 0, want)
}

pub fn mkv_header_prefix(path: &str) -> Result<Vec<u8>, HeadError> {
    mkv_header_from(&read_prefix(path, MKV_HEAD_MAX)?)
}

/// `window` up to the first Cluster, walking the top level elements by their sizes. Once Tracks
/// is read, an element the walk cannot step past in the window ends the head before it,
/// attached fonts as a rule, the demuxer reads those through the SeekHead. A head of a cut
/// window is then always the start of the whole one's.
pub fn mkv_header_from(window: &[u8]) -> Result<Vec<u8>, HeadError> {
    let (ebml, at) = ebml_header(window, 0).ok_or(HeadError::NoCluster)?;
    let at = match (ebml.0, ebml.1) {
        (EBML, Some(size)) => at.checked_add(size).ok_or(HeadError::NoCluster)?,
        _ => return Err(HeadError::NoCluster),
    };
    let (seg, mut at) = ebml_header(window, at).ok_or(HeadError::NoCluster)?;
    if seg.0 != SEGMENT {
        return Err(HeadError::NoCluster);
    }
    let mut tracks = false;
    for _ in 0..MKV_MAX_TOP {
        let Some(((id, size), data)) = ebml_header(window, at) else {
            break;
        };
        if id == CLUSTER {
            return Ok(window[..at as usize].to_vec());
        }
        let Some(end) =
            size.and_then(|s| data.checked_add(s)).filter(|&e| e <= window.len() as u64)
        else {
            break;
        };
        tracks |= id == TRACKS;
        at = end;
    }
    match tracks {
        true => Ok(window[..at.min(window.len() as u64) as usize].to_vec()),
        false => Err(HeadError::NoCluster),
    }
}

/// The id and size of the element at `at`, None for the unknown size, and where its data
/// starts. None when the header is cut or no EBML header (RFC 8794 section 4).
fn ebml_header(b: &[u8], at: u64) -> Option<((u32, Option<u64>), u64)> {
    let at = usize::try_from(at).ok()?;
    let first = *b.get(at)?;
    let id_len = first.leading_zeros() as usize + 1;
    if id_len > 4 {
        return None;
    }
    let id = b.get(at..at + id_len)?.iter().fold(0u32, |v, &x| v << 8 | u32::from(x));
    let s = at + id_len;
    let lead = *b.get(s)?;
    let len = lead.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let raw = b.get(s..s + len)?;
    let mask = if len == 8 { 0 } else { 0xFFu8 >> len };
    let size = raw[1..].iter().fold(u64::from(lead & mask), |v, &x| v << 8 | u64::from(x));
    // all ones is the unknown size
    let unknown = size == (1u64 << (7 * len)) - 1;
    Some(((id, (!unknown).then_some(size)), (s + len) as u64))
}

/// A Matroska file's head as [`mkv_head_at`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MkvHead {
    /// The bytes before the first Cluster. Short of it when an element past the cap follows
    /// Tracks, attached fonts as a rule, the demuxer reads those through the SeekHead.
    pub head: Vec<u8>,
    pub first_cluster: u64,
}

/// `TooLarge` is an element ending past the cap before Tracks is read, `TooMany` no Cluster among
/// the first top level elements.
#[derive(Debug)]
pub enum MkvHeadError<E> {
    NotMatroska,
    TooLarge { at: u64, max: u64 },
    TooMany { at: u64 },
    Malformed { at: u64 },
    Unreadable { at: u64, len: usize, source: E },
}

impl<E: fmt::Display> fmt::Display for MkvHeadError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MkvHeadError::NotMatroska => f.write_str("no EBML header, not a Matroska file"),
            MkvHeadError::TooLarge { at, max } => {
                write!(f, "the element at byte {at} ends past the {max} a head may hold")
            }
            MkvHeadError::TooMany { at } => {
                write!(f, "no Cluster in the {MKV_MAX_TOP} top level elements before byte {at}")
            }
            MkvHeadError::Malformed { at } => write!(f, "no element header at byte {at}"),
            MkvHeadError::Unreadable { at, len, source } => {
                write!(f, "{len} bytes at byte {at} cannot be read, {source}")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for MkvHeadError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MkvHeadError::Unreadable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A 4 octet ID and an 8 octet size, the most an element header takes.
const EL_HEAD: usize = 12;

/// The id, the size and where the data starts of the element at `at`, through `read`.
fn mkv_element_at<E>(
    read: &ReadAt<'_, E>,
    at: u64,
) -> Result<(u32, Option<u64>, u64), MkvHeadError<E>> {
    let b = read(at, EL_HEAD).map_err(|source| MkvHeadError::Unreadable {
        at,
        len: EL_HEAD,
        source,
    })?;
    let ((id, size), data) = ebml_header(&b, 0).ok_or(MkvHeadError::Malformed { at })?;
    Ok((id, size, at.checked_add(data).ok_or(MkvHeadError::Malformed { at })?))
}

/// The head of a Matroska file through `read`, which gives the bytes at an offset and fewer at
/// the end of the file. The top level elements are walked by their headers alone, so attached
/// fonts are never read. Once Tracks is read, an element ending past `max` ends the head before
/// it and the walk goes on past it for the first Cluster. One before Tracks is refused. A walk
/// that breaks past such a cut takes the cut as the first Cluster, the demuxer finds the real one
/// by its id.
pub fn mkv_head_at<E>(read: &ReadAt<'_, E>, max: u64) -> Result<MkvHead, MkvHeadError<E>> {
    let (id, size, data) = mkv_element_at(read, 0)?;
    if id != EBML {
        return Err(MkvHeadError::NotMatroska);
    }
    let seg_at = size.and_then(|s| data.checked_add(s)).ok_or(MkvHeadError::Malformed { at: 0 })?;
    let (id, _, mut at) = mkv_element_at(read, seg_at)?;
    if id != SEGMENT {
        return Err(MkvHeadError::Malformed { at: seg_at });
    }
    let (mut tracks, mut cut) = (false, None);
    let mut bounds = None;
    for _ in 0..MKV_MAX_TOP {
        let (id, size, data) = match mkv_element_at(read, at) {
            Ok(h) => h,
            Err(_) if cut.is_some() => break,
            Err(e) => return Err(e),
        };
        if id == CLUSTER {
            bounds = Some((at, cut.unwrap_or(at)));
            break;
        }
        let end = match size.and_then(|s| data.checked_add(s)) {
            Some(end) => end,
            None if cut.is_some() => break,
            None => return Err(MkvHeadError::Malformed { at }),
        };
        if cut.is_none() && end > max {
            if !tracks {
                return Err(MkvHeadError::TooLarge { at, max });
            }
            cut = Some(at);
        }
        tracks |= id == TRACKS;
        at = end;
    }
    let (first_cluster, end) =
        bounds.or(cut.map(|c| (c, c))).ok_or(MkvHeadError::TooMany { at })?;
    let len = usize::try_from(end).map_err(|_| MkvHeadError::TooLarge { at: 0, max })?;
    let head = read(0, len).map_err(|source| MkvHeadError::Unreadable { at: 0, len, source })?;
    if head.len() != len {
        return Err(MkvHeadError::Malformed { at: head.len() as u64 });
    }
    Ok(MkvHead { head, first_cluster })
}

pub fn avi_head(path: &str) -> Result<Vec<u8>, HeadError> {
    avi_head_from(&read_prefix(path, AVI_HEAD_MAX)?)
}

/// `window` through its `movi` four-CC, so `pf-avi` finds where the chunks start.
pub fn avi_head_from(window: &[u8]) -> Result<Vec<u8>, HeadError> {
    let at = window.windows(4).position(|w| w == b"movi").ok_or(HeadError::NoMovi)?;
    Ok(window[..at + 4].to_vec())
}

pub type ReadAt<'a, E> = dyn Fn(u64, usize) -> Result<Vec<u8>, E> + 'a;

/// Read at a time while looking for the `movi` list.
const AVI_STEP: usize = 256 << 10;

/// `NoMovi` is no `movi` in the first `read` bytes, the whole file or the cap.
#[derive(Debug)]
pub enum AviHeadError<E> {
    NoMovi { read: u64 },
    Unreadable { at: u64, len: usize, source: E },
}

impl<E: fmt::Display> fmt::Display for AviHeadError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AviHeadError::NoMovi { read } => {
                write!(f, "no 'movi' list in the first {read} bytes, not an AVI file")
            }
            AviHeadError::Unreadable { at, len, source } => {
                write!(f, "{len} bytes at byte {at} cannot be read, {source}")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for AviHeadError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AviHeadError::Unreadable { source, .. } => Some(source),
            AviHeadError::NoMovi { .. } => None,
        }
    }
}

/// [`avi_head_from`] through `read`, which gives the bytes at an offset and fewer at the end of
/// the file, a step at a time until the `movi` four-CC shows or `max` bytes are read.
pub fn avi_head_at<E>(read: &ReadAt<'_, E>, max: usize) -> Result<Vec<u8>, AviHeadError<E>> {
    let mut head: Vec<u8> = Vec::new();
    while head.len() < max {
        let at = head.len();
        let len = AVI_STEP.min(max - at);
        let got = read(at as u64, len).map_err(|source| AviHeadError::Unreadable {
            at: at as u64,
            len,
            source,
        })?;
        let n = got.len().min(len);
        head.extend_from_slice(&got[..n]);
        // only the new bytes and the 3 before them can hold a match not looked at yet
        let from = at.saturating_sub(3);
        if let Some(p) = head[from..].windows(4).position(|w| w == b"movi") {
            head.truncate(from + p + 4);
            return Ok(head);
        }
        if n < len {
            break;
        }
    }
    Err(AviHeadError::NoMovi { read: head.len() as u64 })
}

#[derive(Debug)]
pub enum Mp4HeadError<E> {
    NoMoov,
    TooLarge { at: u64, size: u64, max: u64 },
    Unreadable { at: u64, len: usize, source: E },
    Truncated { at: u64, end: u64, len: u64 },
    Malformed { at: u64 },
}

impl<E: fmt::Display> fmt::Display for Mp4HeadError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mp4HeadError::NoMoov => f.write_str("no moov box in the file"),
            Mp4HeadError::TooLarge { at, size, max } => {
                write!(
                    f,
                    "the box at byte {at} is {size} bytes, more than the {max} a head may hold"
                )
            }
            Mp4HeadError::Unreadable { at, len, source } => {
                write!(f, "{len} bytes at byte {at} cannot be read, {source}")
            }
            Mp4HeadError::Truncated { at, end, len } => {
                write!(f, "the box at byte {at} runs to {end}, past the end of the file at {len}")
            }
            Mp4HeadError::Malformed { at } => {
                write!(f, "the box at byte {at} is smaller than its header")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for Mp4HeadError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Mp4HeadError::Unreadable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// More than any `moov` of a file that plays, and the most one read fetches.
pub const MAX_MOOV: u64 = 64 << 20;
/// Read behind the `mdat` in one go, so a trailing `moov` usually costs one request.
const TAIL_FIRST: usize = 1 << 20;
const MAX_BOXES: u32 = 256;
const MAX_MFRA: u64 = 16 << 20;

struct FileBytes<'a, E> {
    window: &'a [u8],
    len: u64,
    read: &'a ReadAt<'a, E>,
    tail: (u64, Vec<u8>),
}

impl<E> FileBytes<'_, E> {
    /// `[at, at + n)` from the window, the tail, or one read of [`TAIL_FIRST`] at least.
    fn get(&mut self, at: u64, n: usize) -> Result<Vec<u8>, Mp4HeadError<E>> {
        let end = at.saturating_add(n as u64);
        let (wlen, tlen) = (self.window.len() as u64, self.tail.1.len() as u64);
        if end <= wlen {
            return Ok(self.window[at as usize..end as usize].to_vec());
        }
        if at < wlen {
            let mut out = self.window[at as usize..].to_vec();
            out.extend(self.get(wlen, (end - wlen) as usize)?);
            return Ok(out);
        }
        if self.tail.0 <= at && end <= self.tail.0 + tlen {
            let from = (at - self.tail.0) as usize;
            return Ok(self.tail.1[from..from + n].to_vec());
        }
        let want = end.max(at.saturating_add(TAIL_FIRST as u64)).min(self.len) - at;
        let got = (self.read)(at, want as usize).map_err(|source| Mp4HeadError::Unreadable {
            at,
            len: n,
            source,
        })?;
        if (got.len() as u64) < n as u64 {
            return Err(Mp4HeadError::Truncated { at, end, len: at + got.len() as u64 });
        }
        self.tail = (at, got);
        Ok(self.tail.1[..n].to_vec())
    }
}

/// The bytes up to the `mdat`, with the `moov` appended when it lies behind it, ISO/IEC
/// 14496-12 section 8.2.1. `read` fetches what `window` does not hold.
pub fn mp4_head_at<E>(
    window: &[u8],
    len: u64,
    read: &ReadAt<'_, E>,
) -> Result<Vec<u8>, Mp4HeadError<E>> {
    let mut bytes = FileBytes { window, len, read, tail: (0, Vec::new()) };
    let (mut at, mut mdat, mut moov) = (0u64, None, None);
    for _ in 0..MAX_BOXES {
        if at + 8 > len {
            break;
        }
        let hdr = bytes.get(at, 16.min((len - at) as usize))?;
        let size = u64::from(u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]));
        // Size 0 runs to the end, size 1 is a 64-bit largesize after the header, section 4.2.
        let (header, size) = match size {
            0 => (8, len - at),
            1 => match hdr.get(8..16) {
                Some(&[a, b, c, d, e, f, g, h]) => {
                    (16, u64::from_be_bytes([a, b, c, d, e, f, g, h]))
                }
                _ => return Err(Mp4HeadError::Truncated { at, end: at + 16, len }),
            },
            n => (8, n),
        };
        if size < header {
            return Err(Mp4HeadError::Malformed { at });
        }
        let Some(end) = at.checked_add(size).filter(|end| *end <= len) else {
            return Err(Mp4HeadError::Truncated { at, end: at.saturating_add(size), len });
        };
        match &hdr[4..8] {
            b"mdat" if mdat.is_none() => mdat = Some(at),
            b"moov" if size > MAX_MOOV => {
                return Err(Mp4HeadError::TooLarge { at, size, max: MAX_MOOV });
            }
            b"moov" => moov = Some((at, size as usize)),
            _ => {}
        }
        if mdat.is_some() && moov.is_some() {
            break;
        }
        at = end;
    }
    let Some((moov_at, moov_len)) = moov else { return Err(Mp4HeadError::NoMoov) };
    let prefix = mdat.unwrap_or(len);
    if prefix > MAX_MOOV {
        return Err(Mp4HeadError::TooLarge { at: 0, size: prefix, max: MAX_MOOV });
    }
    let mut head = bytes.get(0, prefix as usize)?;
    if moov_at >= prefix {
        head.extend(bytes.get(moov_at, moov_len)?);
    }
    Ok(head)
}

/// The size of the `mfra` a file ends in, from the `mfro` in its last 16 bytes, section 8.8.11.
pub fn mfra_size(tail: &[u8]) -> Option<u64> {
    let mfro = tail.get(tail.len().checked_sub(16)?..)?;
    if mfro[..8] != [0, 0, 0, 16, b'm', b'f', b'r', b'o'] {
        return None;
    }
    Some(u64::from(u32::from_be_bytes([mfro[12], mfro[13], mfro[14], mfro[15]])))
}

/// The `mfra` a fragmented MP4 ends in, section 8.8.9, empty when there is none or it is
/// unreadable.
pub fn mp4_mfra_at<E>(len: u64, read: &ReadAt<'_, E>) -> Vec<u8> {
    let Some(at) = len.checked_sub(16) else { return Vec::new() };
    let Ok(tail) = read(at, 16) else { return Vec::new() };
    let Some(size) = mfra_size(&tail).filter(|s| (16..=MAX_MFRA.min(len)).contains(s)) else {
        return Vec::new();
    };
    read(len - size, size as usize).unwrap_or_default()
}

pub fn mp4_head(path: &str) -> Result<Vec<u8>, Mp4HeadError<io::Error>> {
    let io = |source| Mp4HeadError::Unreadable { at: 0, len: 0, source };
    let mut file = File::open(path).map_err(io)?;
    let len = file.metadata().map_err(io)?.len();
    let window = read_file_at(&mut file, 0, MP4_WINDOW).map_err(io)?;
    let file = std::cell::RefCell::new(file);
    mp4_head_at(&window, len, &|off, n| read_file_at(&mut file.borrow_mut(), off, n))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::cell::Cell;

    fn boxes(list: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut v = Vec::new();
        for (k, b) in list {
            v.extend_from_slice(&((8 + b.len()) as u32).to_be_bytes());
            v.extend_from_slice(*k);
            v.extend_from_slice(b);
        }
        v
    }

    #[derive(Debug, PartialEq)]
    enum Refused {
        PastEnd,
        NoRanges,
    }

    fn reads<'a>(
        file: &'a [u8],
        count: &'a Cell<u32>,
    ) -> impl Fn(u64, usize) -> Result<Vec<u8>, Refused> + 'a {
        move |off, n| {
            count.set(count.get() + 1);
            let off = off as usize;
            if off >= file.len() {
                return Err(Refused::PastEnd);
            }
            Ok(file[off..(off + n).min(file.len())].to_vec())
        }
    }

    fn write_tmp(name: &str, bytes: &[u8]) -> String {
        let path = std::env::temp_dir().join(format!("pf-probe-{name}-{}", std::process::id()));
        std::fs::write(&path, bytes).expect("write tmp");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn mp4_head_stops_at_mdat() {
        let file = boxes(&[
            (b"ftyp", b"isom\x00\x00\x02\x00mp41"),
            (b"moov", b"MOOVDATA"),
            (b"mdat", b"media"),
        ]);
        let path = write_tmp("faststart", &file);
        let head = mp4_head(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        assert_eq!(head, file[..file.len() - 13]);
    }

    #[test]
    fn mp4_head_reads_a_trailing_moov_off_disk() {
        let media = vec![0x55u8; 200_000];
        let file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        let path = write_tmp("moovlast", &file);
        let head = mp4_head(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        let mut want = boxes(&[(b"ftyp", b"isom")]);
        want.extend_from_slice(&boxes(&[(b"moov", b"MOOVDATA")]));
        assert_eq!(head, want);
    }

    #[test]
    fn mp4_head_without_mdat_is_the_whole_file() {
        let file = boxes(&[(b"ftyp", b"isom"), (b"moov", b"meta")]);
        let count = Cell::new(0);
        let head = mp4_head_at(&file, file.len() as u64, &reads(&file, &count)).expect("head");
        assert_eq!((head, count.get()), (file, 0));
    }

    #[test]
    fn mp4_head_reads_a_moov_behind_the_mdat_once() {
        let media = vec![0x55u8; 300_000];
        let file =
            boxes(&[(b"ftyp", b"isom"), (b"free", b""), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        let count = Cell::new(0);
        let head =
            mp4_head_at(&file[..4096], file.len() as u64, &reads(&file, &count)).expect("head");
        let mut want = boxes(&[(b"ftyp", b"isom"), (b"free", b"")]);
        want.extend_from_slice(&boxes(&[(b"moov", b"MOOVDATA")]));
        assert_eq!((&head, count.get()), (&want, 1));
        count.set(0);
        let head = mp4_head_at(&file, file.len() as u64, &reads(&file, &count)).expect("head");
        assert_eq!((head, count.get()), (want, 0), "a window holding it all reads nothing");
    }

    #[test]
    fn mp4_head_takes_a_largesize_box() {
        let mut file = boxes(&[(b"ftyp", b"isom")]);
        let body = [0xAAu8; 10];
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(b"free");
        file.extend_from_slice(&(16 + body.len() as u64).to_be_bytes());
        file.extend_from_slice(&body);
        file.extend_from_slice(&boxes(&[(b"moov", b"M"), (b"mdat", b"x")]));
        let count = Cell::new(0);
        let head = mp4_head_at(&file, file.len() as u64, &reads(&file, &count)).expect("head");
        assert_eq!(head, file[..file.len() - 9]);
    }

    #[test]
    fn mp4_head_refuses_what_it_cannot_resolve() {
        let media = vec![0x55u8; 1000];
        let count = Cell::new(0);
        let file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &media)]);
        let err = mp4_head_at(&file[..64], file.len() as u64, &reads(&file, &count)).unwrap_err();
        assert!(matches!(err, Mp4HeadError::NoMoov), "{err:?}");
        let mut file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        file.truncate(file.len() - 5);
        let err = mp4_head_at(&file[..64], file.len() as u64, &reads(&file, &count)).unwrap_err();
        assert!(matches!(err, Mp4HeadError::Truncated { .. }), "{err:?}");
        let file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        let refuse = |_: u64, _: usize| Err(Refused::NoRanges);
        let err = mp4_head_at(&file[..64], file.len() as u64, &refuse).unwrap_err();
        assert!(matches!(err, Mp4HeadError::Unreadable { source: Refused::NoRanges, .. }));
        let mut bad = boxes(&[(b"ftyp", b"isom")]);
        bad.extend_from_slice(&[0, 0, 0, 4, b'f', b'r', b'e', b'e']);
        let err = mp4_head_at(&bad, bad.len() as u64, &refuse).unwrap_err();
        assert!(matches!(err, Mp4HeadError::Malformed { at: 12 }));
        let mut big = boxes(&[(b"ftyp", b"isom")]);
        big.extend_from_slice(&1u32.to_be_bytes());
        big.extend_from_slice(b"moov");
        big.extend_from_slice(&(MAX_MOOV + 1).to_be_bytes());
        let err = mp4_head_at(&big, MAX_MOOV + 64, &refuse).unwrap_err();
        assert!(matches!(err, Mp4HeadError::TooLarge { at: 12, .. }), "{err:?}");
    }

    #[test]
    fn a_fragmented_file_gives_its_mfra() {
        let mfra_body = b"tfra-entries";
        let mfra_len = 8 + mfra_body.len() as u32 + 16;
        let mut file = boxes(&[(b"ftyp", b"isom"), (b"moof", b"x"), (b"mdat", b"y")]);
        file.extend_from_slice(&mfra_len.to_be_bytes());
        file.extend_from_slice(b"mfra");
        file.extend_from_slice(mfra_body);
        file.extend_from_slice(&[0, 0, 0, 16, b'm', b'f', b'r', b'o', 0, 0, 0, 0]);
        file.extend_from_slice(&mfra_len.to_be_bytes());
        let count = Cell::new(0);
        let mfra = mp4_mfra_at(file.len() as u64, &reads(&file, &count));
        assert_eq!(mfra, file[file.len() - mfra_len as usize..]);
        assert_eq!(&mfra[4..8], b"mfra");
        let plain = boxes(&[(b"ftyp", b"isom"), (b"moov", b"m"), (b"mdat", b"y")]);
        assert!(mp4_mfra_at(plain.len() as u64, &reads(&plain, &count)).is_empty());
        let mut lying = plain.clone();
        lying.extend_from_slice(&[0, 0, 0, 16, b'm', b'f', b'r', b'o', 0, 0, 0, 0, 0, 1, 0, 0]);
        assert!(mp4_mfra_at(lying.len() as u64, &reads(&lying, &count)).is_empty());
        assert_eq!(mfra_size(&[0; 15]), None);
        count.set(0);
        assert!(mp4_mfra_at(14, &reads(&plain[..14], &count)).is_empty());
        assert_eq!(count.get(), 0, "a file under 16 bytes is read past its end");
    }

    /// An element of `id` with an 8 octet size.
    fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v = id.to_be_bytes().iter().copied().skip_while(|&b| b == 0).collect::<Vec<_>>();
        v.push(0x01);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    /// The EBML header and a Segment of unknown size, then Info and Tracks.
    fn mkv_lead() -> Vec<u8> {
        let mut f = el(EBML, &el(0x4282, b"matroska"));
        f.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        f.extend(el(0x1549_A966, &[0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40]));
        f.extend(el(TRACKS, &el(0xAE, &[0xD7, 0x81, 0x01])));
        f
    }

    #[test]
    fn mkv_head_stops_at_the_first_cluster() {
        let mut file = mkv_lead();
        // a Cluster id inside an element's data is no Cluster
        file.extend(el(0xEC, &CLUSTER.to_be_bytes()));
        let meta = file.len();
        file.extend(el(CLUSTER, b"media"));
        file.extend(el(CLUSTER, b"media"));
        assert_eq!(mkv_header_from(&file).expect("head"), file[..meta]);
        let path = write_tmp("cluster", &file);
        let head = mkv_header_prefix(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        assert_eq!(head, file[..meta]);
        // a window cut in the Cluster's header after Tracks ends the head there
        assert_eq!(mkv_header_from(&file[..meta + 3]).expect("head"), file[..meta]);
        let tracks_end = mkv_lead().len();
        let err = mkv_header_from(&file[..tracks_end - 1]).unwrap_err();
        assert!(matches!(err, HeadError::NoCluster) && err.to_string().contains("Cluster"));
        assert!(matches!(mkv_header_from(b"segment-metadata"), Err(HeadError::NoCluster)));
    }

    #[test]
    fn mkv_head_ends_before_attachments_past_the_window() {
        let mut file = mkv_lead();
        let tracks_end = file.len();
        // 118 MB of fonts as a header, the window holds a little of them
        file.extend([0x19, 0x41, 0xA4, 0x69, 0x01, 0, 0, 0, 0x07, 0x60, 0, 0]);
        file.extend([0; 4096]);
        assert_eq!(mkv_header_from(&file).expect("head"), file[..tracks_end]);
        // before Tracks the head is not read yet
        let mut early = el(EBML, &el(0x4282, b"matroska"));
        early.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        early.extend([0x19, 0x41, 0xA4, 0x69, 0x01, 0, 0, 0, 0x07, 0x60, 0, 0]);
        early.extend([0; 64]);
        assert!(matches!(mkv_header_from(&early), Err(HeadError::NoCluster)));
    }

    /// `file` read through a callback, each read recorded.
    fn at_reads<'a>(
        file: &'a [u8],
        log: &'a std::cell::RefCell<Vec<(u64, usize)>>,
    ) -> impl Fn(u64, usize) -> Result<Vec<u8>, Refused> + 'a {
        move |off, n| {
            log.borrow_mut().push((off, n));
            let at = (off as usize).min(file.len());
            Ok(file[at..(at + n).min(file.len())].to_vec())
        }
    }

    #[test]
    fn mkv_head_at_reads_element_heads_then_the_head() {
        let mut file = mkv_lead();
        let meta = file.len();
        file.extend(el(CLUSTER, &[7; 4096]));
        file.extend(el(CLUSTER, &[7; 4096]));
        let log = std::cell::RefCell::new(Vec::new());
        let h = mkv_head_at(&at_reads(&file, &log), 1 << 20).expect("head");
        assert_eq!((h.head.as_slice(), h.first_cluster), (&file[..meta], meta as u64));
        let cluster_bytes = |&&(off, n): &&(u64, usize)| off + n as u64 > meta as u64 && n > 12;
        assert!(!log.borrow().iter().any(|r| cluster_bytes(&r)), "{:?}", log.borrow());
        let refuse = |_: u64, _: usize| Err(Refused::NoRanges);
        let err = mkv_head_at(&refuse, 1 << 20).unwrap_err();
        assert!(matches!(err, MkvHeadError::Unreadable { at: 0, source: Refused::NoRanges, .. }));
        let log = std::cell::RefCell::new(Vec::new());
        let err = mkv_head_at(&at_reads(b"not a matroska file", &log), 1 << 20).unwrap_err();
        assert!(matches!(err, MkvHeadError::NotMatroska), "{err:?}");
        let err = mkv_head_at(&at_reads(&file[..meta - 3], &log), 1 << 20).unwrap_err();
        assert!(matches!(err, MkvHeadError::Malformed { .. }), "a file cut in the head, {err:?}");
    }

    /// `prefix`, then `gap` bytes of zeros, then `rest`, read as a sparse file.
    fn sparse<'a>(
        prefix: &'a [u8],
        gap: u64,
        rest: &'a [u8],
    ) -> impl Fn(u64, usize) -> Result<Vec<u8>, Refused> + 'a {
        let behind = prefix.len() as u64 + gap;
        move |off, n| {
            let byte = |at: u64| match at {
                at if at < prefix.len() as u64 => Some(prefix[at as usize]),
                at if at < behind => Some(0),
                at => rest.get((at - behind) as usize).copied(),
            };
            Ok((off..off + n as u64).map_while(byte).collect())
        }
    }

    #[test]
    fn mkv_head_at_ends_before_attachments_past_the_cap() {
        let mut lead = mkv_lead();
        let tracks_end = lead.len();
        // 80 MB of fonts as a header, their body never read
        let size: u64 = 80 << 20;
        lead.extend([0x19, 0x41, 0xA4, 0x69, 0x01]);
        lead.extend_from_slice(&size.to_be_bytes()[1..]);
        let rest = el(CLUSTER, b"media");
        let h = mkv_head_at(&sparse(&lead, size, &rest), 64 << 20).expect("head");
        assert_eq!(h.head, lead[..tracks_end], "the head ends where the Attachments start");
        assert_eq!(h.first_cluster, lead.len() as u64 + size, "the Cluster behind them");
        // a walk that breaks behind the cut takes the cut as the first Cluster
        let h = mkv_head_at(&sparse(&lead, size, &[0x00]), 64 << 20).expect("head");
        assert_eq!(h.first_cluster, tracks_end as u64);
    }

    #[test]
    fn mkv_head_at_refuses_a_head_past_the_cap_before_reading_it() {
        let mut f = el(EBML, &el(0x4282, b"matroska"));
        f.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        let mut big = f.clone();
        // a Void of 2^40 bytes before Tracks, the Cluster behind it
        big.extend([0xEC, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let rest = el(CLUSTER, b"media");
        let err = mkv_head_at(&sparse(&big, 1 << 40, &rest), 64 << 20).unwrap_err();
        assert!(matches!(err, MkvHeadError::TooLarge { .. }), "{err:?}");
        // more empty Voids than the walk takes
        for _ in 0..MKV_MAX_TOP + 10 {
            f.extend([0xEC, 0x80]);
        }
        let err = mkv_head_at(&sparse(&f, 0, &rest), 64 << 20).unwrap_err();
        assert!(matches!(err, MkvHeadError::TooMany { .. }), "{err:?}");
    }

    #[test]
    fn avi_head_keeps_the_movi_fourcc() {
        let file =
            b"RIFF\x00\x00\x00\x00AVI LIST\x10\x00\x00\x00hdrlstrhLIST\x00\x00\x00\x00movi00dc";
        let head = avi_head_from(file).expect("head");
        assert_eq!(head, file[..file.len() - 4]);
        assert!(head.ends_with(b"movi"));
        let path = write_tmp("avi", file);
        let from_disk = avi_head(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        assert_eq!(from_disk, head);
        assert!(matches!(avi_head_from(&file[..20]), Err(HeadError::NoMovi)));
        assert!(matches!(avi_head("/nonexistent/pf-probe"), Err(HeadError::Io(_))));
    }

    #[test]
    fn avi_head_at_reads_a_step_at_a_time_to_the_movi() {
        let mut file = vec![0u8; AVI_STEP + 64];
        file[..12].copy_from_slice(b"RIFF\x00\x00\x00\x00AVI ");
        file[AVI_STEP - 2..AVI_STEP + 2].copy_from_slice(b"movi");
        let log = std::cell::RefCell::new(Vec::new());
        let head = avi_head_at(&at_reads(&file, &log), 16 << 20).expect("head");
        assert_eq!(head, file[..AVI_STEP + 2], "a movi across two reads is found");
        assert_eq!(*log.borrow(), [(0, AVI_STEP), (AVI_STEP as u64, AVI_STEP)]);
        let none = vec![7u8; 1000];
        let err = avi_head_at(&at_reads(&none, &log), 16 << 20).unwrap_err();
        assert!(matches!(err, AviHeadError::NoMovi { read: 1000 }), "{err:?}");
        let long = vec![7u8; 3 * AVI_STEP];
        log.borrow_mut().clear();
        let err = avi_head_at(&at_reads(&long, &log), AVI_STEP + 10).unwrap_err();
        assert!(matches!(err, AviHeadError::NoMovi { read } if read == AVI_STEP as u64 + 10));
        assert_eq!(log.borrow()[1], (AVI_STEP as u64, 10), "no read past the cap");
        let refuse = |_: u64, _: usize| Err(Refused::NoRanges);
        let err = avi_head_at(&refuse, 16 << 20).unwrap_err();
        assert!(matches!(err, AviHeadError::Unreadable { at: 0, source: Refused::NoRanges, .. }));
    }

    #[test]
    fn read_prefix_stops_at_the_end_of_a_short_file() {
        let path = write_tmp("short", b"0123456789");
        let (all, some) =
            (read_prefix(&path, 64).expect("read"), read_prefix(&path, 4).expect("read"));
        let _ = std::fs::remove_file(&path);
        assert_eq!((all.as_slice(), some.as_slice()), (&b"0123456789"[..], &b"0123"[..]));
    }
}
