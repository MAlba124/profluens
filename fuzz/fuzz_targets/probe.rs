//! pf-probe typefind and the head readers over a file held in memory. Input is a knob byte then
//! the file. Knob bits 0-2 pick the window `Mp4HeadAt` is handed (0, 8, 16, 100, 1024, 4096,
//! 65536 bytes or all of it), bits 3-4 how its reads are answered (0 exact, 1 with nothing, 2
//! half of what is asked, 3 refused), bits 5-7 where the file is cut for the prefix checks, at
//! `len * k / 7`. A head is the file's own bytes, what a box walk over the whole file gives
//! however the window and the reads fall, and no read leaves the file.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_probe::head::{
    AviHeadAt, AviHeadError, HeadError, MAX_MOOV, MkvHeadAt, Mp4HeadAt, Mp4HeadError, Mp4MfraAt,
    avi_head_from, mfra_size, mkv_header_from,
};
use pf_probe::{Kind, PREFIX_LEN, behind_id3, id3v2_len, probe};
use profluens_core::fetch::{Fetch, Need, Step};

const WINDOWS: [usize; 8] = [0, 8, 16, 100, 1024, 4096, 65536, usize::MAX];
const MAX_BOXES: usize = 256;
const MAX_MFRA: u64 = 16 << 20;
const TAIL_FIRST: usize = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reads {
    Exact,
    Nothing,
    Half,
    Refuse,
}

/// What [`drive`] ends with besides the machine's answer.
#[derive(Debug, PartialEq, Eq)]
enum Driven<E> {
    Fetch(E),
    Refused,
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
        [Reads::Exact, Reads::Nothing, Reads::Half, Reads::Refuse][usize::from(knob >> 3 & 3)];
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
    if let Ok(h) = &got {
        assert!(file.starts_with(h), "a head that is not the file's start");
    }
    if let Ok(h) = mkv_header_from(&file[..cut]) {
        let Ok(whole) = &got else { panic!("a cut file found a head the whole has not") };
        assert!(whole.starts_with(&h), "a cut file found another head");
    }
    let mut log = Vec::new();
    let mut m = MkvHeadAt::new(1 << 20);
    let read = drive(&mut m, file, Reads::Exact, &mut log);
    if let Ok(h) = &read {
        assert!(file.starts_with(&h.head), "a read head that is not the file's start");
    }
    assert_eq!(read, drive(&mut m, file, Reads::Exact, &mut log), "a head read twice differs");
    assert!(log.iter().all(|n| n.len <= 12 || n.at == 0), "a read past the heads {log:?}");
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
    let mut log = Vec::new();
    let read = drive(&mut AviHeadAt::new(file.len() + 1), file, Reads::Exact, &mut log);
    match (&read, &got) {
        (Ok(r), Ok(h)) => assert_eq!(r, h, "a read head other than the window's"),
        (Err(Driven::Fetch(AviHeadError::NoMovi { read })), Err(_)) => {
            assert_eq!(*read, file.len() as u64);
        }
        _ => panic!("{read:?} where the window gives {got:?}"),
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

/// No read of `log` leaves `file`.
fn inside(file: &[u8], log: &[Need]) {
    let len = file.len() as u64;
    for &Need { at, len: n } in log {
        assert!(at <= len && n as u64 <= len - at, "a read of {n} at {at} past {len}");
    }
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

/// Runs `m` over `file`, each Need logged and answered as `reads` says, short past its end.
fn drive<F: Fetch>(
    m: &mut F,
    file: &[u8],
    reads: Reads,
    log: &mut Vec<Need>,
) -> Result<F::Out, Driven<F::Err>> {
    loop {
        match m.poll().map_err(Driven::Fetch)? {
            Step::Done(out) => return Ok(out),
            Step::Need(need) => {
                log.push(need);
                let at = usize::try_from(need.at).map_or(file.len(), |at| at.min(file.len()));
                let held = need.len.min(file.len() - at);
                let buf = m.buf();
                assert_eq!(buf.len(), need.len, "a buffer other than the read");
                let n = match reads {
                    Reads::Exact => held,
                    Reads::Nothing => 0,
                    Reads::Half => held / 2,
                    Reads::Refuse => return Err(Driven::Refused),
                };
                buf[..n].copy_from_slice(&file[at..at + n]);
                m.filled(n);
            }
        }
    }
}

fn mp4(file: &[u8], window: &[u8], reads: Reads) -> Walk {
    let mut log = Vec::new();
    let got = drive(&mut Mp4HeadAt::new(window, file.len() as u64), file, reads, &mut log);
    let again = drive(&mut Mp4HeadAt::new(window, file.len() as u64), file, reads, &mut log);
    assert_eq!(format!("{got:?}"), format!("{again:?}"), "an mp4 head is not deterministic");
    let want = walk(file);
    inside(file, &log);
    assert!(log.len() <= 2 * (MAX_BOXES + 2), "{} reads", log.len());
    for n in &log {
        assert!(n.len <= MAX_MOOV as usize + TAIL_FIRST, "a read of {}", n.len);
    }
    let lossy = !matches!(reads, Reads::Exact);
    match got {
        Ok(h) => {
            assert!(h.len() as u64 <= 2 * MAX_MOOV && h.len() <= 2 * file.len(), "{}", h.len());
            assert_eq!(Walk::Head(h), want, "a head the walk does not give");
        }
        Err(Driven::Fetch(Mp4HeadError::NoMoov)) => assert_eq!(want, Walk::NoMoov),
        Err(Driven::Fetch(Mp4HeadError::TooLarge { size, max, .. })) => {
            assert!(size > max && max == MAX_MOOV);
            assert_eq!(want, Walk::TooLarge);
        }
        Err(Driven::Fetch(Mp4HeadError::Malformed { .. })) => assert_eq!(want, Walk::Malformed),
        Err(Driven::Fetch(Mp4HeadError::Truncated { .. })) => {
            assert!(lossy || want == Walk::Truncated);
        }
        Err(Driven::Refused) => assert!(reads == Reads::Refuse),
    }
    want
}

fn mfra(file: &[u8], reads: Reads) -> usize {
    let mut log = Vec::new();
    let len = file.len() as u64;
    let got = drive(&mut Mp4MfraAt::new(len), file, reads, &mut log).unwrap_or_default();
    assert!(log.len() <= 2, "{} reads for an mfra", log.len());
    inside(file, &log);
    let size = mfra_size(file).filter(|s| (16..=MAX_MFRA.min(len)).contains(s));
    if let Some(n) = mfra_size(file) {
        let t = &file[file.len() - 16..];
        assert_eq!(t[..8], [0, 0, 0, 16, b'm', b'f', b'r', b'o']);
        assert_eq!(n, u64::from(u32::from_be_bytes([t[12], t[13], t[14], t[15]])));
    }
    let want = size.map_or(&[][..], |s| &file[(len - s) as usize..]);
    match reads {
        Reads::Exact => assert_eq!(got, want, "an mfra other than the file's"),
        Reads::Nothing | Reads::Half | Reads::Refuse => assert!(got.is_empty() || got == want),
    }
    got.len()
}
