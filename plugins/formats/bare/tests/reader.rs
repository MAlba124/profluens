//! Bare FLAC, MP3 and ADTS files opened, read through and seeked. The fixtures are ffmpeg's,
//! each bare file beside the same stream remuxed into Matroska, the heads and units must be the
//! Matroska track's.

// A test's own fixtures and timing, not an element.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use pf_bare::fixture::{flac_file, flac_frame_header};
use pf_bare::{BareError, BareHead, BareKind, Codec, OpenAt, RawTags, Reader, Unit};
use profluens_core::fetch::{Fetch, Step, drive_file, drive_slice, drive_slice_log};

const MS: i64 = 1_000_000;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn open(bytes: &[u8], kind: BareKind) -> Result<(BareHead, RawTags), BareError> {
    drive_slice(&mut OpenAt::new(bytes.len() as u64, kind), bytes)
}

fn opened(bytes: &[u8], kind: BareKind) -> (BareHead, RawTags) {
    open(bytes, kind).unwrap()
}

fn head(bytes: &[u8], kind: BareKind) -> BareHead {
    opened(bytes, kind).0
}

/// Every unit from where `rd` is, with its bytes.
fn next(h: &BareHead, bytes: &[u8], rd: &mut Reader) -> Result<Option<Unit>, BareError> {
    drive_slice(&mut rd.next(h), bytes)
}

fn drain(h: &BareHead, rd: &mut Reader, bytes: &[u8]) -> Vec<(Unit, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(u) = next(h, bytes, rd).unwrap() {
        out.push((u, rd.bytes(&u).to_vec()));
    }
    assert!(next(h, bytes, rd).unwrap().is_none(), "the end stays the end");
    out
}

fn seek(h: &BareHead, bytes: &[u8], rd: &mut Reader, ns: i64) -> Result<(), BareError> {
    drive_slice(&mut rd.seek(h, ns), bytes)
}

/// The CodecPrivate and the frames of the one track of a Matroska file.
fn mkv(bytes: &[u8]) -> (Vec<u8>, Vec<(i64, Vec<u8>)>) {
    let mut r = pf_mkv::reader::MatroskaReader::new();
    r.push(bytes).unwrap();
    let mut frames = Vec::new();
    while let Some(f) = r.next_frame() {
        frames.push((f.pts_ns as i64, f.data));
    }
    (r.tracks()[0].codec_private.clone(), frames)
}

fn seek_to(h: &BareHead, bytes: &[u8], rd: &mut Reader, ns: i64) -> Unit {
    seek(h, bytes, rd, ns).unwrap();
    next(h, bytes, rd).unwrap().unwrap()
}

#[test]
fn flac_is_the_matroska_track_frame_for_frame() {
    let b = fixture("tone.flac");
    let h = head(&b, BareKind::Flac);
    let (mkv_head, frames) = mkv(&fixture("flac.mka"));
    assert_eq!((h.family, h.channels(), h.rate, h.depth()), ("flac", 1, 48_000, 16));
    assert!(h.bitrate() > 50_000 && h.bitrate() < 400_000, "{}", h.bitrate());
    assert_eq!(h.duration_ns, 2_000 * MS);
    assert_eq!(h.head, mkv_head, "fLaC and the STREAMINFO block as Matroska holds it");
    assert_eq!(&h.head[..8], b"fLaC\x80\x00\x00\x22");
    let units = drain(&h, &mut Reader::new(&h), &b);
    assert_eq!(units.len(), 21);
    assert_eq!(
        units.iter().map(|u| u.1.clone()).collect::<Vec<_>>(),
        frames.iter().map(|f| f.1.clone()).collect::<Vec<_>>(),
        "every frame byte for byte"
    );
    assert_eq!(
        units.iter().map(|u| u.0.pts_ns).collect::<Vec<_>>(),
        frames.iter().map(|f| f.0).collect::<Vec<_>>(),
        "and stamped alike"
    );
    assert_eq!(units[1].0.pts_ns, 96 * MS, "4608 samples a frame at 48 kHz");
    assert_eq!(units[0].0.dur_ns, 96_000_000);
    // the target in frame 12, 1152 ms to 1248 ms
    let mut rd = Reader::new(&h);
    let u = seek_to(&h, &b, &mut rd, 1_234 * MS);
    assert_eq!((u.pts_ns, rd.bytes(&u)), (1_152 * MS, &frames[12].1[..]));
    let u = seek_to(&h, &b, &mut rd, 60_000 * MS);
    assert_eq!(rd.bytes(&u), &frames[20].1[..], "past the end, the last frame");
    assert!(next(&h, &b, &mut rd).unwrap().is_none());
}

#[test]
fn mp3_is_the_matroska_track_frame_for_frame() {
    let b = fixture("tone.mp3");
    let h = head(&b, BareKind::Mp3);
    let (mkv_head, frames) = mkv(&fixture("mp3.mka"));
    assert_eq!(
        (h.family, h.channels(), h.rate, h.bitrate(), h.depth()),
        ("mp3", 2, 44_100, 128_000, 16)
    );
    assert!(h.head.is_empty() && mkv_head.is_empty(), "no head for MPEG audio");
    assert!(h.cbr.is_some(), "one bitrate, seeks on the line");
    let units = drain(&h, &mut Reader::new(&h), &b);
    assert_eq!(units.len(), 78, "the Info frame is no unit");
    assert_eq!(
        units.iter().map(|u| u.1.clone()).collect::<Vec<_>>(),
        frames.iter().map(|f| f.1.clone()).collect::<Vec<_>>()
    );
    assert_eq!(units[1].0.pts_ns, 26_122_448, "1152 samples a frame at 44.1 kHz");
    assert!((h.duration_ns - 78 * 26_122_448).abs() < MS, "{}", h.duration_ns);
    let (delay, padding) = h.gapless;
    eprintln!("mp3 gapless delay={delay} padding={padding}");
    let mut rd = Reader::new(&h);
    let u = seek_to(&h, &b, &mut rd, 1_000 * MS);
    assert_eq!(
        (u.pts_ns, rd.bytes(&u)),
        (h.ns(38 * 1152), &frames[38].1[..]),
        "frame 38 holds 1 s"
    );
    assert!(rd.exact());
}

#[test]
fn adts_is_the_matroska_track_frame_for_frame() {
    let b = fixture("tone.aac");
    let h = head(&b, BareKind::Adts);
    let (mkv_head, frames) = mkv(&fixture("aac.mka"));
    assert_eq!((h.family, h.channels(), h.rate), ("aac", 1, 48_000));
    assert_eq!(h.head, mkv_head, "the AudioSpecificConfig");
    let units = drain(&h, &mut Reader::new(&h), &b);
    assert_eq!(units.len(), 94);
    assert_eq!(
        units.iter().map(|u| u.1.clone()).collect::<Vec<_>>(),
        frames.iter().map(|f| f.1.clone()).collect::<Vec<_>>(),
        "raw frames, the ADTS headers off"
    );
    assert!(
        (h.duration_ns - 94 * 21_333_333).abs() < 50 * MS,
        "estimated from the head, {}",
        h.duration_ns
    );
    // within reach of the start a seek walks there and counts
    let mut rd = Reader::new(&h);
    let u = seek_to(&h, &b, &mut rd, 1_000 * MS);
    assert_eq!((u.pts_ns, rd.bytes(&u)), (h.ns(46 * 1024), &frames[46].1[..]));
    assert!(rd.exact());
}

/// The opener hands back the tag bytes it read, the caller parses them.
#[test]
fn the_tag_bytes_come_back_raw() {
    let (_, t) = opened(&fixture("tone.flac"), BareKind::Flac);
    assert_eq!((t.comments.len(), t.pictures.len()), (1, 1));
    let c = t.span(&t.comments[0]);
    let vendor = u32::from_le_bytes(c[..4].try_into().unwrap()) as usize;
    assert!(c.len() > 4 + vendor, "a VORBIS_COMMENT body, the vendor string first");
    let p = t.span(&t.pictures[0]);
    assert_eq!(u32::from_be_bytes(p[..4].try_into().unwrap()), 3, "a PICTURE body, a front cover");
    let (_, t) = opened(&fixture("tone.mp3"), BareKind::Mp3);
    assert!(t.id3v2().starts_with(b"ID3"));
    assert!(t.comments.is_empty() && t.pictures.is_empty());
    // an ID3v1 tag at the end, the audio ends before it
    let mut b = fixture("tone.mp3");
    let len = b.len() as u64;
    let mut v1 = b"TAG".to_vec();
    v1.resize(128, b' ');
    b.extend(&v1);
    let (h, t) = opened(&b, BareKind::Mp3);
    assert_eq!((t.v1, h.end), (v1, len));
}

/// `m` over `bytes`, and the bytes it read.
fn counted<F: Fetch>(m: &mut F, bytes: &[u8]) -> (Result<F::Out, F::Err>, u64) {
    let mut log = Vec::new();
    let out = drive_slice_log(m, bytes, &mut log);
    (out, log.iter().map(|n| (n.len as u64).min((bytes.len() as u64).saturating_sub(n.at))).sum())
}

#[test]
fn flac_opens_and_seeks_without_reading_the_file() {
    for table in [false, true] {
        // about 5 MB, 2000 frames of 4096 samples
        let f = flac_file(2000, 777, &[], table);
        let len = f.bytes.len() as u64;
        let (opened, n) = counted(&mut OpenAt::new(len, BareKind::Flac), &f.bytes);
        let (h, _) = opened.unwrap();
        assert!(n <= (128 << 10) + 160, "the head and the tail, {n} bytes");
        assert_eq!(h.points.len(), if table { 200 } else { 0 });
        assert_eq!(h.duration_ns, h.ns(1999 * 4096 + 777));
        let mut rd = Reader::new(&h);
        for target_frame in [0usize, 1, 7, 500, 1234, 1998, 1999] {
            let at = h.ns(f.frames[target_frame].2 + 1000);
            let (sought, n) = counted(&mut rd.seek(&h, at), &f.bytes);
            sought.unwrap();
            let u = next(&h, &f.bytes, &mut rd).unwrap().unwrap();
            let (off, flen, sample) = f.frames[target_frame];
            assert_eq!(
                (u.off, u.len, u.pts_ns),
                (off, flen, h.ns(sample)),
                "table={table} frame {target_frame}"
            );
            assert!(n < 2 << 20, "table={table} frame {target_frame} read {n} bytes");
        }
        // the rest plays on from a seek, to the short last frame
        seek(&h, &f.bytes, &mut rd, h.ns(1990 * 4096)).unwrap();
        let rest = drain(&h, &mut rd, &f.bytes);
        assert_eq!(rest.len(), 10);
        assert_eq!(
            rest.last().unwrap().0.dur_ns as i64,
            h.ns(1999 * 4096 + 777) - h.ns(1999 * 4096)
        );
    }
}

#[test]
fn flac_frames_survive_false_syncs_and_damage() {
    let mut f = flac_file(40, 4096, &[], false);
    // a damaged frame header in the middle, the frame before runs to the next good one
    let (off, _, _) = f.frames[20];
    f.bytes[off as usize + 4] ^= 0x55;
    let h = head(&f.bytes, BareKind::Flac);
    let units = drain(&h, &mut Reader::new(&h), &f.bytes);
    let pts: Vec<i64> = units.iter().map(|u| u.0.pts_ns).collect();
    assert_eq!(units.len(), 39, "one frame lost");
    assert!(
        !pts.contains(&h.ns(20 * 4096)) && pts.contains(&h.ns(21 * 4096)),
        "stamps stay exact behind it"
    );
}

/// An MPEG-1 layer III header at 44.1 kHz, `kbps` of the ladder.
fn mpa_header(kbps: u32) -> [u8; 4] {
    const KBPS: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    let idx = KBPS.iter().position(|&k| k == kbps).unwrap() as u8;
    [0xFF, 0xFB, idx << 4, 0x00]
}

/// A VBR stream of `n` frames cycling bitrates behind a Xing frame with a TOC, and the
/// offset of every audio frame.
fn vbr(n: usize) -> (Vec<u8>, Vec<u64>) {
    let rates = [64, 128, 192, 256, 320];
    let mut body = Vec::new();
    let mut offs = Vec::new();
    for i in 0..n {
        let k = rates[(i / 7) % rates.len()];
        let len = 144 * k as usize * 1000 / 44_100;
        offs.push(body.len() as u64);
        let mut fr = vec![0u8; len];
        fr[..4].copy_from_slice(&mpa_header(k));
        body.extend(fr);
    }
    let xlen = 144 * 128 * 1000 / 44_100;
    let total = (xlen + body.len()) as u64;
    let mut x = vec![0u8; xlen];
    x[..4].copy_from_slice(&mpa_header(128));
    x[36..40].copy_from_slice(b"Xing");
    x[40..44].copy_from_slice(&7u32.to_be_bytes());
    x[44..48].copy_from_slice(&(n as u32).to_be_bytes());
    x[48..52].copy_from_slice(&(total as u32).to_be_bytes());
    for i in 0..100 {
        let at = xlen as u64 + offs[i * n / 100];
        x[52 + i] = (at * 256 / total) as u8;
    }
    let mut file = x;
    file.extend(body);
    (file, offs.iter().map(|o| o + xlen as u64).collect())
}

#[test]
fn vbr_mp3_seeks_by_its_table_then_counts() {
    let (b, offs) = vbr(3000);
    let h = head(&b, BareKind::Mp3);
    assert!(h.cbr.is_none());
    assert_eq!(h.samples, 3000 * 1152);
    assert!(!h.points.is_empty());
    let spf = 26_122_448i64;
    let at = 50_000 * MS;
    // a fresh reader far from the start goes by the table, near the target
    let mut rd = Reader::new(&h);
    let u = seek_to(&h, &b, &mut rd, at);
    assert!(!rd.exact());
    let k = (at / spf) as usize;
    let landed = offs.iter().position(|&o| o == u.off).expect("a frame start");
    assert!(landed.abs_diff(k) <= 30, "within 1% of the file, frame {landed} for {k}");
    // read through once, the counted points make the seek exact
    let mut rd = Reader::new(&h);
    let all = drain(&h, &mut rd, &b);
    assert_eq!(all.len(), 3000);
    assert_eq!(all.iter().map(|u| u.0.off).collect::<Vec<_>>(), offs);
    let u = seek_to(&h, &b, &mut rd, at);
    assert!(rd.exact());
    assert_eq!((u.off, u.pts_ns), (offs[k], h.ns(k as u64 * 1152)));
    let u = seek_to(&h, &b, &mut rd, 0);
    assert_eq!((u.off, u.pts_ns), (offs[0], 0));
}

#[test]
fn broken_files_are_refused() {
    let junk = b"nothing like audio at all".repeat(100);
    for kind in [BareKind::Flac, BareKind::Mp3, BareKind::Adts] {
        assert!(open(&junk, kind).is_err(), "{kind:?}");
    }
    let mut f = flac_file(3, 100, &[], false).bytes;
    f.truncate(30);
    assert!(matches!(open(&f, BareKind::Flac), Err(BareError::Malformed(_))));
    let flac = flac_file(3, 100, &[], false);
    let h = head(&flac.bytes, BareKind::Flac);
    assert!(matches!(h.codec, Codec::Flac { variable: false, .. }));
}

/// Opening, reading through and seeking damaged files ends, whatever the damage.
#[test]
fn damage_never_hangs_or_panics() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let files = [
        (flac_file(60, 300, &[], true).bytes, BareKind::Flac),
        (fixture("tone.flac"), BareKind::Flac),
        (fixture("tone.mp3"), BareKind::Mp3),
        (fixture("tone.aac"), BareKind::Adts),
        (vbr(400).0, BareKind::Mp3),
    ];
    for (clean, kind) in &files {
        for round in 0..40 {
            let mut b = clean.clone();
            let flips = 1 + rnd() % 64;
            for _ in 0..flips {
                let i = (rnd() % b.len() as u64) as usize;
                b[i] = rnd() as u8;
            }
            if round % 4 == 0 {
                b.truncate((rnd() % b.len() as u64) as usize);
            }
            let Ok((h, _)) = open(&b, *kind) else { continue };
            let mut rd = Reader::new(&h);
            let mut n = 0;
            while let Ok(Some(_)) = next(&h, &b, &mut rd) {
                n += 1;
                assert!(n < 100_000, "{kind:?} round {round} never ends");
            }
            for at in [0, 700 * MS, 1_500 * MS, 90_000 * MS] {
                let _ = seek(&h, &b, &mut rd, at);
                let _ = next(&h, &b, &mut rd);
            }
        }
    }
}

fn seektable(points: &[(u64, u64)]) -> Vec<u8> {
    let mut t = Vec::new();
    for &(s, o) in points {
        t.extend(s.to_be_bytes());
        t.extend(o.to_be_bytes());
        t.extend(4096u16.to_be_bytes());
    }
    t
}

#[test]
fn a_seek_point_past_u64_is_dropped() {
    let table = seektable(&[(0, 0), (40_960, u64::MAX - 10)]);
    let f = flac_file(30, 1000, &[(3, table)], false);
    let h = head(&f.bytes, BareKind::Flac);
    assert_eq!(h.points, [(0, h.first)]);
    let mut rd = Reader::new(&h);
    let u = seek_to(&h, &f.bytes, &mut rd, 2_000 * MS);
    let k = f.frames.iter().rposition(|x| h.ns(x.2) <= 2_000 * MS).unwrap();
    assert_eq!(u.off, f.frames[k].0);
}

#[test]
fn endless_metadata_is_refused() {
    let pad: Vec<(u8, Vec<u8>)> = (0..2000).map(|_| (1, Vec::new())).collect();
    let f = flac_file(3, 100, &pad, false);
    assert!(matches!(open(&f.bytes, BareKind::Flac), Err(BareError::Malformed(_))));
}

/// A FLAC whose first frame is followed by megabytes of headers that pass their CRC but
/// number no frame after it, each would send the search after the end on.
#[test]
fn headers_that_never_chain_end_in_bounded_time() {
    let f = flac_file(2, 4096, &[], false);
    let h = head(&f.bytes, BareKind::Flac);
    let Codec::Flac { si, .. } = h.codec else { panic!() };
    let mut b = f.bytes[..f.frames[0].0 as usize + 20].to_vec();
    // max_frame and the total 0, the search for an end then runs MAX_SCAN wide and no
    // frame is known to be the last
    let si_at = 8;
    b[si_at + 7..si_at + 10].copy_from_slice(&[0, 0, 0]);
    b[si_at + 13] &= 0xF0;
    b[si_at + 14..si_at + 18].copy_from_slice(&[0, 0, 0, 0]);
    let fake = flac_frame_header(&si, 1000, 4096);
    while b.len() < 6 << 20 {
        b.extend(&fake);
    }
    let h = head(&b, BareKind::Flac);
    let t = std::time::Instant::now();
    let mut rd = Reader::new(&h);
    let mut n = 0;
    while let Ok(Some(_)) = next(&h, &b, &mut rd) {
        n += 1;
        assert!(n < 1000);
    }
    let _ = seek(&h, &b, &mut rd, 30_000 * MS);
    let _ = next(&h, &b, &mut rd);
    assert!(t.elapsed() < std::time::Duration::from_secs(20), "took {:?}", t.elapsed());
}

#[test]
fn a_long_frame_at_a_low_rate_saturates_its_duration() {
    let mut f = flac_file(3, 4096, &[], false);
    // the rate field of STREAMINFO as 1 Hz, the frame headers say 0, take it from STREAMINFO
    let packed = u64::from_be_bytes(f.streaminfo[10..18].try_into().unwrap());
    let packed = (packed & ((1 << 44) - 1)) | (1 << 44);
    f.bytes[8 + 10..8 + 18].copy_from_slice(&packed.to_be_bytes());
    let si = pf_bare::flac::stream_info(&f.bytes[8..42]).unwrap();
    let mut b = f.bytes[..f.frames[0].0 as usize].to_vec();
    for (i, x) in f.frames.iter().enumerate() {
        let hd = flac_frame_header(&si, i as u64, 4096);
        b.extend(&hd);
        b.extend(&f.bytes[x.0 as usize + hd.len()..(x.0 + u64::from(x.1)) as usize]);
    }
    let h = head(&b, BareKind::Flac);
    let mut rd = Reader::new(&h);
    let u = next(&h, &b, &mut rd).unwrap().unwrap();
    assert_eq!((u.pts_ns, u.dur_ns), (0, u32::MAX));
}

#[test]
fn a_xing_frame_past_the_end_leaves_no_audio() {
    let (b, offs) = vbr(20);
    for cut in 40..offs[0] as usize + 8 {
        let b = &b[..cut];
        if let Ok((h, _)) = open(b, BareKind::Mp3) {
            assert!(
                h.first <= h.end && h.end <= b.len() as u64,
                "cut {cut}, first {} end {}",
                h.first,
                h.end
            );
        }
    }
}

#[test]
fn seeks_to_the_ends_of_time_stay_in_the_file() {
    for (name, kind) in
        [("tone.flac", BareKind::Flac), ("tone.mp3", BareKind::Mp3), ("tone.aac", BareKind::Adts)]
    {
        let b = fixture(name);
        let h = head(&b, kind);
        let mut rd = Reader::new(&h);
        for at in [i64::MIN, -1, i64::MAX] {
            seek(&h, &b, &mut rd, at).unwrap();
            if let Some(u) = next(&h, &b, &mut rd).unwrap() {
                assert!(u.off + u64::from(u.len) <= h.end, "{name} at {at}");
            }
        }
    }
}

/// The file driver reads what the slice driver does, a read error is the caller's.
#[test]
fn a_file_on_disk_reads_alike_and_its_errors_are_io() {
    let b = fixture("tone.flac");
    let path = std::env::temp_dir().join(format!("pf-bare-disk-{}.flac", std::process::id()));
    std::fs::write(&path, &b).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let (h, tags) = drive_file(&mut OpenAt::new(b.len() as u64, BareKind::Flac), &file).unwrap();
    assert_eq!((&h, &tags), (&opened(&b, BareKind::Flac).0, &opened(&b, BareKind::Flac).1));
    let mut rd = Reader::new(&h);
    let mut from_disk = Vec::new();
    while let Some(u) = drive_file(&mut rd.next(&h), &file).map_err(BareError::from).unwrap() {
        from_disk.push((u, rd.bytes(&u).to_vec()));
    }
    assert_eq!(from_disk, drain(&h, &mut Reader::new(&h), &b));
    drive_file(&mut rd.seek(&h, 1_234 * MS), &file).unwrap();
    let u = drive_file(&mut rd.next(&h), &file).unwrap().unwrap();
    assert_eq!(u, seek_to(&h, &b, &mut Reader::new(&h), 1_234 * MS));
    // a file open for writing alone refuses reads
    let write_only = std::fs::File::options().append(true).open(&path).unwrap();
    let e = drive_file(&mut Reader::new(&h).next(&h), &write_only).unwrap_err();
    let _ = std::fs::remove_file(&path);
    assert!(matches!(BareError::from(e), BareError::Io(_)), "{e:?}");
}

/// A read the caller never answered leaves the reader as it was before it.
#[test]
fn a_machine_dropped_half_way_leaves_the_reader_whole() {
    // about 900 KB, the window holds a seventh of it
    let f = flac_file(400, 1000, &[], false);
    let h = opened(&f.bytes, BareKind::Flac).0;
    let mut rd = Reader::new(&h);
    {
        let mut m = rd.next(&h);
        assert!(matches!(m.poll(), Ok(Step::Need(_))), "a read first");
    }
    let first = next(&h, &f.bytes, &mut rd).unwrap().unwrap();
    assert_eq!((first.off, first.len), (f.frames[0].0, f.frames[0].1), "the fill read again");
    {
        let mut m = rd.seek(&h, h.ns(300 * 4096));
        assert!(matches!(m.poll(), Ok(Step::Need(_))), "a probe past the window");
    }
    seek(&h, &f.bytes, &mut rd, 0).unwrap();
    assert_eq!(next(&h, &f.bytes, &mut rd).unwrap(), Some(first));
    let rest = drain(&h, &mut rd, &f.bytes);
    assert_eq!(rest.len(), 399);
    assert!(rest.iter().all(|(u, b)| b[..] == f.bytes[u.off as usize..][..u.len as usize]));
}
