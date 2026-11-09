/// The family of sync frames a stream is made of, AC-3 (A/52 5.4), E-AC-3 (A/52 Annex E) or DTS
/// (ETSI TS 102 114 5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    Ac3,
    Eac3,
    Dts,
}

impl SyncKind {
    pub fn family(self) -> &'static str {
        match self {
            SyncKind::Ac3 => "ac3",
            SyncKind::Eac3 => "eac3",
            SyncKind::Dts => "dts",
        }
    }
}

/// A sync frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// Bytes on the wire.
    pub len: u32,
    pub samples: u32,
    pub rate: u32,
    pub channels: u8,
    /// Bits a source sample, DTS states it, 0 elsewhere.
    pub depth: u8,
    /// What every primary frame of one stream shares, the rate and the channel mode, and for DTS
    /// the word form.
    pub key: u32,
    /// An E-AC-3 dependent or further substream, or a DTS-HD extension, that belongs to the
    /// primary frame before it and adds no samples.
    pub attached: bool,
    /// The nominal kbit/s of an AC-3 frame, 0 elsewhere.
    pub kbps: u32,
}

/// The AC-3 bit rates by `frmsizecod >> 1`, A/52 Table 5.18.
pub(crate) const AC3_KBPS: [u32; 19] =
    [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];
const AC3_RATES: [u32; 3] = [48_000, 44_100, 32_000];
const EAC3_HALF_RATES: [u32; 3] = [24_000, 22_050, 16_000];
/// Full bandwidth channels by `acmod`, 1+1 dual mono counted as two.
const ACMOD_CHANNELS: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
const DTS_RATES: [u32; 16] =
    [0, 8_000, 16_000, 32_000, 0, 0, 11_025, 22_050, 44_100, 0, 0, 12_000, 24_000, 48_000, 0, 0];
const DTS_CHANNELS: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
/// Source resolution by PCMR, Table 5-13, 0 for the reserved codes.
const DTS_DEPTH: [u8; 8] = [16, 16, 20, 20, 0, 24, 24, 0];
const DTS_HD_SYNC: u32 = 0x6458_2025;

/// The `n` bits of `b` from bit `at`, MSB first, 0 past its end.
fn bits(b: &[u8], at: usize, n: usize) -> u32 {
    (at..at + n)
        .fold(0, |v, i| v << 1 | u32::from(b.get(i / 8).map_or(0, |x| x >> (7 - i % 8) & 1)))
}

/// The frame at the start of `b` when it is one of a `kind` stream.
pub fn frame(kind: SyncKind, b: &[u8]) -> Option<Frame> {
    match kind {
        SyncKind::Ac3 => ac3(b),
        SyncKind::Eac3 => eac3(b),
        SyncKind::Dts => dts(b),
    }
}

/// Whether `b0 b1` may start a primary frame of `kind`, the first two bytes of a sync word.
pub fn is_sync(kind: SyncKind, b0: u8, b1: u8) -> bool {
    match kind {
        SyncKind::Ac3 | SyncKind::Eac3 => b0 == 0x0B && b1 == 0x77,
        SyncKind::Dts => {
            matches!((b0, b1), (0x7F, 0xFE) | (0xFE, 0x7F) | (0x1F, 0xFF) | (0xFF, 0x1F))
        }
    }
}

fn ac3(b: &[u8]) -> Option<Frame> {
    let h = b.get(..8)?;
    if h[..2] != [0x0B, 0x77] || h[5] >> 3 > 10 {
        return None;
    }
    let (fscod, code) = (usize::from(h[4] >> 6), usize::from(h[4] & 0x3F));
    let kbps = *AC3_KBPS.get(code >> 1)?;
    let rate = *AC3_RATES.get(fscod)?;
    let words = match fscod {
        0 => kbps * 2,
        1 => kbps * 96_000 / 44_100 + (code & 1) as u32,
        _ => kbps * 3,
    };
    let acmod = usize::from(h[6] >> 5);
    // cmixlev, surmixlev and dsurmod come before lfeon as the mode has them, A/52 5.4.2
    let mut at = 51;
    if acmod & 1 == 1 && acmod != 1 {
        at += 2;
    }
    if acmod & 4 != 0 {
        at += 2;
    }
    if acmod == 2 {
        at += 2;
    }
    let lfe = bits(h, at, 1) as u8;
    Some(Frame {
        len: words * 2,
        samples: 1536,
        rate,
        channels: ACMOD_CHANNELS[acmod] + lfe,
        depth: 0,
        key: (fscod as u32) << 8 | (acmod as u32) << 1 | u32::from(lfe),
        attached: false,
        kbps,
    })
}

fn eac3(b: &[u8]) -> Option<Frame> {
    let h = b.get(..6)?;
    if h[..2] != [0x0B, 0x77] || !(11..=16).contains(&(h[5] >> 3)) {
        return None;
    }
    let (strmtyp, substream) = (h[2] >> 6, h[2] >> 3 & 7);
    if strmtyp == 3 {
        return None;
    }
    let len = ((u32::from(h[2] & 7) << 8 | u32::from(h[3])) + 1) * 2;
    let fscod = usize::from(h[4] >> 6);
    let (rate, blocks) = match fscod {
        3 => (*EAC3_HALF_RATES.get(usize::from(h[4] >> 4 & 3))?, 6),
        f => (AC3_RATES[f], [1, 2, 3, 6][usize::from(h[4] >> 4 & 3)]),
    };
    let acmod = usize::from(h[4] >> 1 & 7);
    let lfe = h[4] & 1;
    Some(Frame {
        len,
        samples: blocks * 256,
        rate,
        channels: ACMOD_CHANNELS[acmod] + lfe,
        depth: 0,
        key: (fscod as u32) << 8 | (acmod as u32) << 1 | u32::from(lfe),
        attached: strmtyp == 1 || substream != 0,
        kbps: 0,
    })
}

/// The word forms of DTS, 16 bit big and little endian and 14 bit in 16 bit words.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Be16,
    Le16,
    Be14,
    Le14,
}

/// The first `N` bytes of a DTS frame as the 16 bit big endian form puts them.
fn canonical<const N: usize>(b: &[u8], form: Form) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    let word = |i: usize| -> Option<u16> {
        let w = b.get(2 * i..2 * i + 2)?;
        Some(match form {
            Form::Be16 | Form::Be14 => u16::from_be_bytes([w[0], w[1]]),
            Form::Le16 | Form::Le14 => u16::from_le_bytes([w[0], w[1]]),
        })
    };
    match form {
        Form::Be16 | Form::Le16 => {
            for i in 0..N / 2 {
                out[2 * i..2 * i + 2].copy_from_slice(&word(i)?.to_be_bytes());
            }
        }
        Form::Be14 | Form::Le14 => {
            let (mut acc, mut held, mut put, mut i) = (0u32, 0u32, 0usize, 0usize);
            while put < N {
                acc = acc << 14 | u32::from(word(i)? & 0x3FFF);
                (held, i) = (held + 14, i + 1);
                while held >= 8 && put < N {
                    held -= 8;
                    out[put] = (acc >> held) as u8;
                    put += 1;
                }
                acc &= (1 << held) - 1;
            }
        }
    }
    Some(out)
}

fn dts(b: &[u8]) -> Option<Frame> {
    let sync = u32::from_be_bytes(b.get(..4)?.try_into().ok()?);
    let form = match sync {
        0x7FFE_8001 => Form::Be16,
        0xFE7F_0180 => Form::Le16,
        0x1FFF_E800 => Form::Be14,
        0xFF1F_00E8 => Form::Le14,
        DTS_HD_SYNC => return dts_hd(b),
        _ => return None,
    };
    let h = canonical::<16>(b, form)?;
    let nblks = bits(&h, 39, 7);
    let fsize = bits(&h, 46, 14) + 1;
    let amode = bits(&h, 60, 6) as usize;
    let sfreq = bits(&h, 66, 4) as usize;
    let lff = bits(&h, 85, 2);
    let crc = bits(&h, 38, 1) as usize;
    let pcmr = bits(&h, 95 + 16 * crc, 3) as usize;
    let rate = DTS_RATES[sfreq];
    // a core frame holds at least 6 blocks of 32 samples and 96 bytes, 5.3.1
    if rate == 0 || nblks < 5 || fsize < 96 || lff == 3 {
        return None;
    }
    let len = match form {
        Form::Be16 | Form::Le16 => fsize,
        _ if fsize.is_multiple_of(7) => fsize / 7 * 8,
        _ => return None,
    };
    let channels = DTS_CHANNELS.get(amode).copied().unwrap_or(0) + u8::from(lff != 0);
    Some(Frame {
        len,
        samples: (nblks + 1) * 32,
        rate,
        channels,
        depth: DTS_DEPTH[pcmr],
        key: (form as u32) << 16 | (sfreq as u32) << 8 | (amode as u32) << 2 | lff,
        attached: false,
        kbps: 0,
    })
}

/// A DTS-HD extension substream, its size from its header, ETSI TS 102 114 7.5.
fn dts_hd(b: &[u8]) -> Option<Frame> {
    let h = b.get(..12)?;
    let long = bits(h, 42, 1) == 1;
    let len = match long {
        false => bits(h, 51, 16),
        true => bits(h, 55, 20),
    } + 1;
    (len >= 16).then_some(Frame {
        len,
        samples: 0,
        rate: 0,
        channels: 0,
        depth: 0,
        key: 0,
        attached: true,
        kbps: 0,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::fixture::{ac3_frame, dts_frame, eac3_frame, to_14};

    #[test]
    fn ac3_headers_state_size_rate_and_channels() {
        let f = ac3(&ac3_frame(0, 448, false, 7)).unwrap();
        assert_eq!((f.len, f.samples, f.rate, f.channels, f.kbps), (1792, 1536, 48_000, 6, 448));
        let f = ac3(&ac3_frame(1, 192, true, 2)).unwrap();
        assert_eq!((f.len, f.rate, f.channels), (2 * (192 * 96_000 / 44_100 + 1), 44_100, 3));
        let f = ac3(&ac3_frame(2, 96, false, 1)).unwrap();
        assert_eq!((f.len, f.channels), (576, 2), "mono and the LFE");
        let mut b = ac3_frame(0, 448, false, 7);
        b[4] = 0xC0;
        assert_eq!(ac3(&b), None, "fscod 3");
        b[4] = 0x3F;
        assert_eq!(ac3(&b), None, "frmsizecod 63");
        b[4] = 0;
        b[5] = 16 << 3;
        assert_eq!(ac3(&b), None, "an E-AC-3 bsid");
        assert_eq!(ac3(&[0x0B, 0x77, 0]), None, "cut");
    }

    #[test]
    fn eac3_headers_tell_primary_from_attached() {
        let f = eac3(&eac3_frame(768, 0, 0)).unwrap();
        assert_eq!(
            (f.len, f.samples, f.rate, f.channels, f.attached),
            (768, 1536, 48_000, 6, false)
        );
        assert!(eac3(&eac3_frame(512, 1, 0)).unwrap().attached, "dependent");
        assert!(eac3(&eac3_frame(512, 0, 2)).unwrap().attached, "another substream");
        assert_eq!(eac3(&eac3_frame(512, 3, 0)), None, "reserved type");
        let mut b = eac3_frame(512, 0, 0);
        b[4] = 0b1110_0000;
        let f = eac3(&b).unwrap();
        assert_eq!((f.rate, f.samples), (16_000, 1536), "half rate, six blocks");
        b[5] = 8 << 3;
        assert_eq!(eac3(&b), None, "an AC-3 bsid");
    }

    #[test]
    fn dts_cores_read_in_every_word_form() {
        let be = dts_frame(2012);
        let f = dts(&be).unwrap();
        assert_eq!((f.len, f.samples, f.rate, f.channels, f.depth), (2012, 512, 48_000, 6, 24));
        let le: Vec<u8> = be.chunks(2).flat_map(|w| [w[1], w[0]]).collect();
        assert_eq!(dts(&le).map(|f| (f.len, f.channels)), Some((2012, 6)));
        // 14 bit frames are 8/7 the size on the wire
        let be14 = dts_frame(2016);
        let w14 = to_14(&be14[..32]);
        assert_eq!(&w14[..4], &[0x1F, 0xFF, 0xE8, 0x00]);
        let f = dts(&w14).unwrap();
        assert_eq!((f.len, f.rate, f.channels), (2304, 48_000, 6));
        let le14: Vec<u8> = w14.chunks(2).flat_map(|w| [w[1], w[0]]).collect();
        assert_eq!(dts(&le14).map(|f| f.len), Some(2304));
        assert_ne!(f.key, dts(&be).unwrap().key, "the form is part of the stream");
        let mut bad = be.clone();
        bad[8] &= 0xC3;
        assert_eq!(dts(&bad), None, "sampling frequency 0");
        assert_eq!(dts(&be[..10]), None, "cut");
    }

    #[test]
    fn dts_hd_substreams_attach() {
        // user bits, index 0, short header of 32, size 4000
        let mut b = vec![0x64, 0x58, 0x20, 0x25, 0];
        let mut put = |at: usize, n: usize, v: u32| {
            while b.len() < 12 {
                b.push(0);
            }
            for i in 0..n {
                if v >> (n - 1 - i) & 1 == 1 {
                    b[(at + i) / 8] |= 0x80 >> ((at + i) % 8);
                }
            }
        };
        put(43, 8, 31);
        put(51, 16, 3999);
        let f = dts(&b).unwrap();
        assert_eq!((f.len, f.attached, f.samples), (4000, true, 0));
        b[5] |= 0x20;
        assert!(dts(&b).is_some(), "the long form reads 20 bits");
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut x = 0x1234_5678u32;
        let mut buf = [0u8; 24];
        for _ in 0..20_000 {
            for b in &mut buf {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *b = (x >> 24) as u8;
            }
            buf[..2].copy_from_slice(&[0x0B, 0x77]);
            if let Some(f) = frame(SyncKind::Ac3, &buf).or(frame(SyncKind::Eac3, &buf)) {
                assert!(f.len >= 2 && f.channels <= 6);
            }
            buf[..4].copy_from_slice(&[0x7F, 0xFE, 0x80, 0x01]);
            if let Some(f) = frame(SyncKind::Dts, &buf) {
                assert!(f.len >= 96 && f.samples >= 192);
            }
        }
    }
}
