//! The bytes a demuxer is built from. The `*_from` and `*_at` forms walk a window the caller
//! already read, so a growing or remote source is never read past what it has.

// App-side setup before any pipeline, one head buffer per open.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// Matroska Cluster id, RFC 9559 section 5.1.3.
const CLUSTER: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
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

pub fn mkv_header_from(window: &[u8]) -> Result<Vec<u8>, HeadError> {
    let at = window.windows(4).position(|w| w == CLUSTER).ok_or(HeadError::NoCluster)?;
    Ok(window[..at].to_vec())
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

    #[test]
    fn mkv_head_stops_at_the_first_cluster() {
        let mut file = vec![0x1A, 0x45, 0xDF, 0xA3];
        file.extend_from_slice(b"segment-metadata");
        let meta = file.len();
        file.extend_from_slice(&CLUSTER);
        file.extend_from_slice(b"media");
        file.extend_from_slice(&CLUSTER);
        assert_eq!(mkv_header_from(&file).expect("head"), file[..meta]);
        let path = write_tmp("cluster", &file);
        let head = mkv_header_prefix(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        assert_eq!(head, file[..meta]);
        let err = mkv_header_from(&file[..meta + 3]).unwrap_err();
        assert!(matches!(err, HeadError::NoCluster) && err.to_string().contains("Cluster"));
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
    fn read_prefix_stops_at_the_end_of_a_short_file() {
        let path = write_tmp("short", b"0123456789");
        let (all, some) =
            (read_prefix(&path, 64).expect("read"), read_prefix(&path, 4).expect("read"));
        let _ = std::fs::remove_file(&path);
        assert_eq!((all.as_slice(), some.as_slice()), (&b"0123456789"[..], &b"0123"[..]));
    }
}
