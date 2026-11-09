// A test's own fixtures, not an element.
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::panic)]

use pf_bare::fixture::{ac3_frame, dts_frame, eac3_frame, to_14, wav_file};
use pf_bare::{BareError, BareHead, BareKind, Codec, OpenAt, Reader, Unit};
use profluens_core::fetch::{drive_slice, drive_slice_log};

fn open(bytes: &[u8], kind: BareKind) -> Result<BareHead, BareError> {
    drive_slice(&mut OpenAt::new(bytes.len() as u64, kind), bytes).map(|(h, _)| h)
}

fn drain(h: &BareHead, bytes: &[u8], rd: &mut Reader) -> Vec<Unit> {
    let mut out = Vec::new();
    while let Some(u) = drive_slice(&mut rd.next(h), bytes).unwrap() {
        assert_eq!(rd.bytes(&u), &bytes[u.off as usize..(u.off + u64::from(u.len)) as usize]);
        out.push(u);
    }
    out
}

fn seek_to(h: &BareHead, bytes: &[u8], rd: &mut Reader, ns: i64) -> Option<Unit> {
    drive_slice(&mut rd.seek(h, ns), bytes).unwrap();
    drive_slice(&mut rd.next(h), bytes).unwrap()
}

/// `n` frames of `frame`, each its index in a filler byte past the header so none is alike.
fn frames(n: usize, frame: impl Fn(usize) -> Vec<u8>, at: usize) -> Vec<u8> {
    (0..n)
        .flat_map(|i| {
            let mut f = frame(i);
            if f.len() > at {
                f[at] = i as u8;
            }
            f
        })
        .collect()
}

#[test]
fn ac3_opens_on_its_line_and_seeks_exactly() {
    let b = frames(500, |_| ac3_frame(0, 448, false, 7), 20);
    let h = open(&b, BareKind::Ac3).unwrap();
    assert_eq!((h.family, h.rate, h.spf, h.channels(), h.depth()), ("ac3", 48_000, 1536, 6, 16));
    assert_eq!((h.first, h.end, h.samples), (0, b.len() as u64, 500 * 1536));
    assert_eq!(h.duration_ns, 16_000_000_000);
    assert_eq!((h.cbr, h.bitrate()), (Some((448_000 * 1536, 384_000)), 448_000));
    let mut rd = Reader::new(&h);
    let all = drain(&h, &b, &mut rd);
    assert_eq!(all.len(), 500);
    assert!(all.iter().enumerate().all(|(i, u)| u.off == i as u64 * 1792 && u.len == 1792));
    assert_eq!(all[499].pts_ns, h.ns(499 * 1536));
    for k in [0u64, 1, 250, 499] {
        let u = seek_to(&h, &b, &mut rd, h.ns(k * 1536 + 100)).unwrap();
        assert!(rd.exact());
        assert_eq!((u.off, u.pts_ns), (k * 1792, h.ns(k * 1536)), "frame {k}");
    }
}

/// At 44.1 kHz an encoder pads a frame a word longer as the nominal rate needs, the line is
/// that rate.
#[test]
fn ac3_at_44k1_seeks_on_the_mean_frame() {
    let (mut b, num, den) = (Vec::new(), 192_000 * 1536u64, 44_100 * 8u64);
    for i in 0..400u64 {
        let padded = (b.len() as u64) + 835 < (i + 1) * num / den;
        b.extend_from_slice(&ac3_frame(1, 192, padded, 2));
    }
    let h = open(&b, BareKind::Ac3).unwrap();
    assert_eq!((h.rate, h.channels(), h.samples), (44_100, 3, 400 * 1536));
    let mut rd = Reader::new(&h);
    let offs: Vec<u64> = drain(&h, &b, &mut rd).iter().map(|u| u.off).collect();
    assert_eq!(offs.len(), 400);
    for k in [3usize, 200, 399] {
        let u = seek_to(&h, &b, &mut rd, h.ns(k as u64 * 1536)).unwrap();
        assert_eq!((u.off, u.pts_ns), (offs[k], h.ns(k as u64 * 1536)), "frame {k}");
    }
}

/// A dependent substream goes out in the unit of the frame before it and adds no samples.
#[test]
fn eac3_units_carry_their_dependent_substreams() {
    let unit = |_| [eac3_frame(768, 0, 0), eac3_frame(384, 1, 0)].concat();
    let b = frames(300, unit, 20);
    let h = open(&b, BareKind::Eac3).unwrap();
    assert_eq!((h.family, h.rate, h.spf, h.channels()), ("eac3", 48_000, 1536, 6));
    assert_eq!(h.cbr, None, "attached frames, no line");
    assert_eq!(h.samples, 300 * 1536);
    let mut rd = Reader::new(&h);
    let all = drain(&h, &b, &mut rd);
    assert_eq!(all.len(), 300);
    assert!(all.iter().enumerate().all(|(i, u)| u.off == i as u64 * 1152 && u.len == 1152));
    let u = seek_to(&h, &b, &mut rd, h.ns(150 * 1536)).unwrap();
    assert_eq!(u.len, 1152, "lands on a primary frame");
    assert_eq!(u.off % 1152, 0);
    let lone = frames(100, |_| eac3_frame(512, 0, 0), 10);
    let h = open(&lone, BareKind::Eac3).unwrap();
    assert_eq!(h.cbr, Some((512, 1)));
}

#[test]
fn dts_opens_in_its_word_forms_and_skips_hd_extensions() {
    let b = frames(200, |_| dts_frame(2012), 20);
    let h = open(&b, BareKind::Dts).unwrap();
    assert_eq!((h.family, h.rate, h.spf, h.channels(), h.depth()), ("dts", 48_000, 512, 6, 24));
    assert_eq!((h.cbr, h.samples), (Some((2012, 1)), 200 * 512));
    let mut rd = Reader::new(&h);
    assert_eq!(drain(&h, &b, &mut rd).len(), 200);
    let u = seek_to(&h, &b, &mut rd, h.ns(120 * 512)).unwrap();
    assert_eq!((u.off, u.pts_ns), (120 * 2012, h.ns(120 * 512)));
    // 14 bit words, the whole frame packed
    let core = dts_frame(2016);
    let b14 = frames(50, |_| to_14(&core), 30);
    let h = open(&b14, BareKind::Dts).unwrap();
    assert_eq!((h.cbr, h.samples), (Some((2304, 1)), 50 * 512));
    assert_eq!(drain(&h, &b14, &mut Reader::new(&h)).len(), 50);
    // a DTS-HD extension substream behind each core frame
    let mut ext = vec![0x64, 0x58, 0x20, 0x25, 0, 0, 0, 0, 0, 0, 0, 0];
    let size: u32 = 1000 - 1;
    for i in 0..16 {
        if size >> (15 - i) & 1 == 1 {
            ext[(51 + i) / 8] |= 0x80 >> ((51 + i) % 8);
        }
    }
    ext.resize(1000, 0x22);
    let hd = frames(100, |_| [dts_frame(2012), ext.clone()].concat(), 20);
    let h = open(&hd, BareKind::Dts).unwrap();
    assert_eq!((h.cbr, h.samples), (None, 100 * 512));
    let all = drain(&h, &hd, &mut Reader::new(&h));
    assert_eq!(all.len(), 100);
    assert!(all.iter().all(|u| u.len == 3012 && u.off % 3012 == 0));
}

#[test]
fn wav_pcm_is_timed_and_seeked_by_byte_arithmetic() {
    let b = wav_file(48_000, 2, 16, false, 48_000 * 3 + 7, None, 3000);
    let h = open(&b, BareKind::Wav).unwrap();
    assert_eq!((h.family, h.rate, h.channels(), h.depth()), ("audio/raw", 48_000, 2, 16));
    let Codec::Pcm { align, .. } = h.codec else { panic!("{:?}", h.codec) };
    assert_eq!((align, h.samples, h.spf), (4, 48_000 * 3 + 7, 4096));
    assert_eq!(h.end - h.first, u64::from(align) * h.samples, "the LIST behind the data left out");
    assert_eq!(h.bitrate(), 48_000 * 32);
    assert_eq!(h.layout(), 0);
    let mut rd = Reader::new(&h);
    let all = drain(&h, &b, &mut rd);
    let total: u64 = all.iter().map(|u| u64::from(u.len)).sum();
    assert_eq!(total, h.end - h.first);
    assert_eq!(all.last().unwrap().len, 4 * ((48_000 * 3 + 7) % 4096));
    let u = seek_to(&h, &b, &mut rd, 1_500_000_000).unwrap();
    assert_eq!((u.off, u.pts_ns), (h.first + 4 * 72_000, 1_500_000_000));
    assert_eq!(seek_to(&h, &b, &mut rd, 99_000_000_000).map(|u| u.len), Some(4), "the last frame");
    // float, extensible, the speakers stated
    let f = wav_file(44_100, 6, 32, true, 1000, Some(0x3F), 0);
    let h = open(&f, BareKind::Wav).unwrap();
    let Codec::Pcm { sample, align, .. } = h.codec else { panic!() };
    assert_eq!((sample.caps_name(), align, h.layout()), ("f32", 24, 0x3F));
    let odd = wav_file(44_100, 2, 16, false, 10, Some(0x7), 0);
    assert_eq!(open(&odd, BareKind::Wav).unwrap().layout(), 0, "three speakers for two");
}

#[test]
fn a_wav_head_past_the_first_read_is_grown_to() {
    let b = wav_file(8_000, 1, 8, false, 4000, None, 300 << 10);
    let mut log = Vec::new();
    let (h, _) =
        drive_slice_log(&mut OpenAt::new(b.len() as u64, BareKind::Wav), &b, &mut log).unwrap();
    assert!(h.first > 300 << 10 && h.samples == 4000);
    assert!(log.len() >= 2 && log.windows(2).all(|w| w[1].at >= w[0].at + w[0].len as u64));
}

#[test]
fn broken_sync_and_wav_files_are_refused() {
    let junk = b"nothing like audio at all".repeat(100);
    for kind in [BareKind::Ac3, BareKind::Eac3, BareKind::Dts, BareKind::Wav] {
        assert!(open(&junk, kind).is_err(), "{kind:?}");
    }
    let mut w = wav_file(48_000, 2, 16, false, 100, None, 0);
    w[20] = 0x55;
    assert_eq!(open(&w, BareKind::Wav), Err(BareError::Unsupported("wav format")));
    let w = wav_file(48_000, 2, 16, false, 100, None, 0);
    assert!(matches!(open(&w[..30], BareKind::Wav), Err(BareError::Malformed(_))), "cut");
    // a data chunk that claims more than the file holds ends with it
    let mut big = w.clone();
    let at = big.windows(4).position(|x| x == b"data").unwrap();
    big[at + 4..at + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    let h = open(&big, BareKind::Wav).unwrap();
    assert_eq!(h.end - h.first, (big.len() as u64 - h.first) / 4 * 4);
    let mut huge = w.clone();
    huge[16..20].copy_from_slice(&0x7FFF_FFF0u32.to_le_bytes());
    assert!(open(&huge, BareKind::Wav).is_err(), "a fmt chunk past the file");
}

/// Random damage over every kind ends, whatever it is.
#[test]
fn damaged_sync_files_never_hang_or_panic() {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let files = [
        (frames(60, |_| ac3_frame(0, 192, false, 2), 9), BareKind::Ac3),
        (
            frames(60, |_| [eac3_frame(512, 0, 0), eac3_frame(256, 1, 0)].concat(), 9),
            BareKind::Eac3,
        ),
        (frames(60, |_| dts_frame(1006), 20), BareKind::Dts),
        (wav_file(8_000, 1, 16, false, 20_000, None, 0), BareKind::Wav),
    ];
    for (f, kind) in &files {
        for _ in 0..200 {
            let mut b = f.clone();
            for _ in 0..(rnd() % 40) {
                let at = (rnd() as usize) % b.len();
                b[at] = rnd() as u8;
            }
            b.truncate(b.len() - (rnd() as usize % 512));
            let Ok(h) = open(&b, *kind) else { continue };
            let mut rd = Reader::new(&h);
            for _ in 0..200 {
                if !matches!(drive_slice(&mut rd.next(&h), &b), Ok(Some(_))) {
                    break;
                }
            }
            let ns = (rnd() % 3_000_000_000) as i64;
            if drive_slice(&mut rd.seek(&h, ns), &b).is_ok() {
                let _ = drive_slice(&mut rd.next(&h), &b);
            }
        }
    }
}
