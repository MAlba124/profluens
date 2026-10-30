//! pf-probe typefind and the head readers over a file held in memory. Input is a knob byte then
//! the file. Knob bits 0-2 pick the window `mp4_head_at` is handed (0, 8, 16, 100, 1024, 4096,
//! 65536 bytes or all of it), bits 3-4 how its `read_at` answers (0 exact, 1 on to the end of
//! the file, 2 half of what is asked, 3 refusing), bits 5-7 where the file is cut for the
//! prefix checks, at `len * k / 7`. A head is the file's own bytes, what a box walk over the
//! whole file gives however the window and the reads fall, and no read leaves the file.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use pf_probe::head::{
    HeadError, MAX_MOOV, Mp4HeadError, avi_head_from, mfra_size, mkv_header_from, mp4_head_at,
    mp4_mfra_at,
};
use pf_probe::{Kind, PREFIX_LEN, behind_id3, id3v2_len, probe};

const WINDOWS: [usize; 8] = [0, 8, 16, 100, 1024, 4096, 65536, usize::MAX];
const CLUSTER: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
const MAX_BOXES: usize = 256;
const MAX_MFRA: u64 = 16 << 20;
const TAIL_FIRST: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    Always,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reads {
    Exact,
    ToEnd,
    Half,
    Refuse,
}

fuzz_target!(|data: &[u8]| {
    let Some((&knob, file)) = data.split_first() else {
        return;
    };
    let cut = file.len() * usize::from(knob >> 5) / 7;
    typefind(file);
    mkv(file, cut);
    avi(file, cut);
    let reads =
        [Reads::Exact, Reads::ToEnd, Reads::Half, Reads::Refuse][usize::from(knob >> 3 & 3)];
    let window = &file[..WINDOWS[usize::from(knob & 7)].min(file.len())];
    let head = mp4(file, window, reads);
    let mfra = mfra(file, reads);
    if say() {
        let kind = probe(&file[..file.len().min(PREFIX_LEN)]);
        let head = match head {
            Walk::Head(h) => format!("head {}", h.len()),
            w => format!("{w:?}"),
        };
        eprintln!("probe {kind:?} mp4 {head} mfra {mfra}");
    }
});

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}

fn typefind(file: &[u8]) {
    let prefix = &file[..file.len().min(PREFIX_LEN)];
    let got = probe(prefix);
    assert_eq!(got, probe(prefix), "a probe is not deterministic");
    if file.len() > PREFIX_LEN && !file.starts_with(b"ID3") {
        assert_eq!(probe(file), got, "bytes past the prefix changed the kind");
    }
    let at = |r: std::ops::Range<usize>| prefix.get(r).unwrap_or_default();
    let behind = id3(prefix);
    let ebml = prefix.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]);
    assert_eq!(ebml, got == Ok(Kind::Mkv), "EBML and Matroska disagree");
    let fits = match got {
        Err(e) => return assert_eq!(e.lead(), at(0..prefix.len().min(8))),
        Ok(Kind::Mkv) => true,
        Ok(Kind::Mp4) => at(4..8) == b"ftyp",
        Ok(Kind::Ogg) => prefix.starts_with(b"OggS"),
        Ok(Kind::Ps) => prefix.starts_with(&[0, 0, 1, 0xBA]) && prefix.len() > 4,
        Ok(Kind::Flv) => prefix.starts_with(b"FLV\x01"),
        Ok(Kind::Avi) => prefix.starts_with(b"RIFF") && at(8..12) == b"AVI ",
        Ok(Kind::Wav) => prefix.starts_with(b"RIFF") && at(8..12) == b"WAVE",
        Ok(Kind::Ac3 | Kind::Eac3) => prefix.len() >= 6 && prefix[..2] == [0x0B, 0x77],
        Ok(Kind::Dts) => prefix.len() >= 4,
        Ok(Kind::Ts) => prefix.contains(&0x47),
        Ok(Kind::AdtsAac) => prefix.len() >= 2 && prefix[0] == 0xFF,
        Ok(Kind::Flac) => prefix.starts_with(b"fLaC") || behind == Some(Kind::Flac),
        Ok(Kind::Mp3) => prefix.starts_with(b"ID3") || prefix.first() == Some(&0xFF),
    };
    assert!(fits, "{got:?} for {:02X?}", at(0..prefix.len().min(16)));
}

/// What sits behind an ID3v2 tag, rechecked against a synchsafe size read here.
fn id3(prefix: &[u8]) -> Option<Kind> {
    let got = id3v2_len(prefix);
    let want = prefix.get(..10).filter(|h| {
        &h[..3] == b"ID3" && h[3] != 0xFF && h[4] != 0xFF && h[6..].iter().all(|b| b & 0x80 == 0)
    });
    let Some(h) = want else {
        assert_eq!(got, None, "an ID3v2 length without a header");
        return None;
    };
    let size = (h[6] as usize) << 21 | (h[7] as usize) << 14 | (h[8] as usize) << 7 | h[9] as usize;
    assert_eq!(got, Some(10 + size + if h[5] & 0x10 != 0 { 10 } else { 0 }));
    let after = got.and_then(|n| prefix.get(n..)).unwrap_or_default();
    let kind = behind_id3(after);
    assert_eq!(kind == Kind::Flac, after.starts_with(b"fLaC"));
    Some(kind)
}

fn mkv(file: &[u8], cut: usize) {
    let got = mkv_header_from(file);
    let first = file.windows(4).position(|w| w == CLUSTER);
    match (&got, first) {
        (Ok(h), Some(at)) => assert_eq!(h[..], file[..at], "a head other than up to the Cluster"),
        (Err(HeadError::NoCluster), None) => {}
        _ => panic!("{got:?} with a first Cluster at {first:?}"),
    }
    if let Ok(h) = mkv_header_from(&file[..cut]) {
        assert_eq!(Some(&h), got.as_ref().ok(), "a cut file found another head");
    }
}

fn avi(file: &[u8], cut: usize) {
    let got = avi_head_from(file);
    let first = file.windows(4).position(|w| w == b"movi");
    match (&got, first) {
        (Ok(h), Some(at)) => assert_eq!(h[..], file[..at + 4], "a head other than through movi"),
        (Err(HeadError::NoMovi), None) => {}
        _ => panic!("{got:?} with a first movi at {first:?}"),
    }
    if let Ok(h) = avi_head_from(&file[..cut]) {
        assert_eq!(Some(&h), got.as_ref().ok(), "a cut file found another head");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Walk {
    Head(Vec<u8>),
    NoMoov,
    TooLarge,
    Truncated,
    Malformed,
}

/// ISO/IEC 14496-12 section 4.2 boxes walked over the whole file in place.
fn walk(file: &[u8]) -> Walk {
    let len = file.len() as u64;
    let be32 =
        |a: usize| u64::from(u32::from_be_bytes([file[a], file[a + 1], file[a + 2], file[a + 3]]));
    let (mut at, mut mdat, mut moov) = (0u64, None, None);
    for _ in 0..MAX_BOXES {
        if at + 8 > len {
            break;
        }
        let a = at as usize;
        let (header, size) = match be32(a) {
            0 => (8, len - at),
            1 if at + 16 > len => return Walk::Truncated,
            1 => (16, be32(a + 8) << 32 | be32(a + 12)),
            n => (8, n),
        };
        if size < header {
            return Walk::Malformed;
        }
        let Some(end) = at.checked_add(size).filter(|&e| e <= len) else {
            return Walk::Truncated;
        };
        match &file[a + 4..a + 8] {
            b"mdat" if mdat.is_none() => mdat = Some(at),
            b"moov" if size > MAX_MOOV => return Walk::TooLarge,
            b"moov" => moov = Some((at, size)),
            _ => {}
        }
        if mdat.is_some() && moov.is_some() {
            break;
        }
        at = end;
    }
    let Some((moov_at, moov_len)) = moov else {
        return Walk::NoMoov;
    };
    let prefix = mdat.unwrap_or(len);
    if prefix > MAX_MOOV {
        return Walk::TooLarge;
    }
    let mut head = file[..prefix as usize].to_vec();
    if moov_at >= prefix {
        head.extend_from_slice(&file[moov_at as usize..(moov_at + moov_len) as usize]);
    }
    Walk::Head(head)
}

/// A `read_at` over `file` that logs each request and answers as `reads` says.
fn reader<'a>(
    file: &'a [u8],
    reads: Reads,
    log: &'a RefCell<Vec<(u64, usize)>>,
) -> impl Fn(u64, usize) -> Result<Vec<u8>, Refused> + 'a {
    move |off, n| {
        log.borrow_mut().push((off, n));
        let len = file.len() as u64;
        assert!(off <= len && n as u64 <= len - off, "a read of {n} at {off} past {len}");
        let off = off as usize;
        match reads {
            Reads::Exact => Ok(file[off..off + n].to_vec()),
            Reads::ToEnd => Ok(file[off..].to_vec()),
            Reads::Half => Ok(file[off..off + n / 2].to_vec()),
            Reads::Refuse => Err(Refused::Always),
        }
    }
}

fn mp4(file: &[u8], window: &[u8], reads: Reads) -> Walk {
    let log = RefCell::new(Vec::new());
    let read = reader(file, reads, &log);
    let got = mp4_head_at(window, file.len() as u64, &read);
    let again = mp4_head_at(window, file.len() as u64, &read);
    assert_eq!(format!("{got:?}"), format!("{again:?}"), "an mp4 head is not deterministic");
    let want = walk(file);
    drop(read);
    let log = log.into_inner();
    assert!(log.len() <= 2 * (MAX_BOXES + 2), "{} reads", log.len());
    for &(_, n) in &log {
        assert!(n <= MAX_MOOV as usize + TAIL_FIRST, "a read of {n}");
    }
    let lossy = matches!(reads, Reads::Half | Reads::Refuse);
    match got {
        Ok(h) => {
            assert!(h.len() as u64 <= 2 * MAX_MOOV && h.len() <= 2 * file.len(), "{}", h.len());
            assert_eq!(Walk::Head(h), want, "a head the walk does not give");
        }
        Err(Mp4HeadError::NoMoov) => assert_eq!(want, Walk::NoMoov),
        Err(Mp4HeadError::TooLarge { size, max, .. }) => {
            assert!(size > max && max == MAX_MOOV);
            assert_eq!(want, Walk::TooLarge);
        }
        Err(Mp4HeadError::Malformed { .. }) => assert_eq!(want, Walk::Malformed),
        Err(Mp4HeadError::Truncated { .. }) => assert!(lossy || want == Walk::Truncated),
        Err(Mp4HeadError::Unreadable { source, .. }) => {
            assert!(reads == Reads::Refuse && source == Refused::Always);
        }
    }
    want
}

fn mfra(file: &[u8], reads: Reads) -> usize {
    let log = RefCell::new(Vec::new());
    let read = reader(file, reads, &log);
    let len = file.len() as u64;
    let got = mp4_mfra_at(len, &read);
    assert!(log.borrow().len() <= 2, "{} reads for an mfra", log.borrow().len());
    let size = mfra_size(file).filter(|s| (16..=MAX_MFRA.min(len)).contains(s));
    if let Some(n) = mfra_size(file) {
        let t = &file[file.len() - 16..];
        assert_eq!(t[..8], [0, 0, 0, 16, b'm', b'f', b'r', b'o']);
        assert_eq!(n, u64::from(u32::from_be_bytes([t[12], t[13], t[14], t[15]])));
    }
    let want = size.map_or(&[][..], |s| &file[(len - s) as usize..]);
    match reads {
        Reads::Exact | Reads::ToEnd => assert_eq!(got, want, "an mfra other than the file's"),
        Reads::Half | Reads::Refuse => assert!(got.is_empty() || got == want),
    }
    got.len()
}
