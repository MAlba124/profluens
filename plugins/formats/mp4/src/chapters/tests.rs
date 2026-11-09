use super::*;
use profluens_core::fetch::{drive_slice, drive_slice_log};

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

fn full(version: u8, rest: &[u8]) -> Vec<u8> {
    [&[version, 0, 0, 0][..], rest].concat()
}

fn tkhd(id: u32) -> Vec<u8> {
    let mut b = full(0, &[0; 8]);
    b.extend_from_slice(&id.to_be_bytes());
    b.resize(84, 0);
    boxed(b"tkhd", &b)
}

fn table(kind: &[u8; 4], rows: &[&[u32]]) -> Vec<u8> {
    let mut b = full(0, &(rows.len() as u32).to_be_bytes());
    rows.iter().flat_map(|r| r.iter()).for_each(|v| b.extend_from_slice(&v.to_be_bytes()));
    boxed(kind, &b)
}

/// A text track of `id` whose samples of `sizes` sit in chunks at `chunks`, `per_chunk` each,
/// timed by the stts `runs`.
fn text_trak(
    id: u32,
    handler: &[u8; 4],
    runs: &[&[u32]],
    sizes: &[u32],
    chunks: &[u32],
) -> Vec<u8> {
    let mut mdhd = full(0, &[0; 8]);
    mdhd.extend_from_slice(&1000u32.to_be_bytes());
    mdhd.extend_from_slice(&5000u32.to_be_bytes());
    // "fre" packed, each letter less 0x60
    let lang = (6u16 << 10) | (18 << 5) | 5;
    mdhd.extend_from_slice(&lang.to_be_bytes());
    mdhd.extend_from_slice(&[0, 0]);
    let mut stsz = full(0, &0u32.to_be_bytes());
    stsz.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    sizes.iter().for_each(|s| stsz.extend_from_slice(&s.to_be_bytes()));
    let per = (sizes.len() / chunks.len().max(1)) as u32;
    let stbl = [
        table(b"stts", runs),
        table(b"stsc", &[&[1, per, 1]]),
        boxed(b"stsz", &stsz),
        table(b"stco", &chunks.iter().map(std::slice::from_ref).collect::<Vec<_>>()),
    ]
    .concat();
    let hdlr = [&[0u8; 8][..], handler, &[0; 12]].concat();
    let minf = boxed(b"minf", &boxed(b"stbl", &stbl));
    let mdia = [boxed(b"mdhd", &mdhd), boxed(b"hdlr", &hdlr), minf].concat();
    boxed(b"trak", &[tkhd(id), boxed(b"mdia", &mdia)].concat())
}

fn audio_trak(chap: &[u32]) -> Vec<u8> {
    let ids: Vec<u8> = chap.iter().flat_map(|c| c.to_be_bytes()).collect();
    boxed(b"trak", &[tkhd(1), boxed(b"tref", &boxed(b"chap", &ids))].concat())
}

fn chpl_box(version: u8, entries: &[(u64, &str)]) -> Vec<u8> {
    let mut b = full(version, if version == 0 { &[] } else { &[0; 4] });
    b.push(entries.len() as u8);
    for (start, title) in entries {
        b.extend_from_slice(&start.to_be_bytes());
        b.push(title.len() as u8);
        b.extend_from_slice(title.as_bytes());
    }
    boxed(b"chpl", &b)
}

#[test]
fn chpl_titles_start_in_100_ns_units() {
    for version in [0, 1] {
        let udta = boxed(b"udta", &chpl_box(version, &[(0, "Intro"), (15_000_000, "Zwei")]));
        let moov = boxed(b"moov", &udta);
        let got = chpl(&moov);
        assert_eq!(
            got,
            [
                ChplEntry { start_ns: 0, title: b"Intro" },
                ChplEntry { start_ns: 1_500_000_000, title: b"Zwei" }
            ],
            "version {version}"
        );
        assert_eq!(chpl(&moov[..moov.len() - 2]), [], "a cut box is no box");
    }
    // a count past the entries keeps what is there
    let mut b = chpl_box(1, &[(7, "a")]);
    b[16] = 200;
    let moov = boxed(b"moov", &boxed(b"udta", &b));
    assert_eq!(chpl(&moov), [ChplEntry { start_ns: 700, title: b"a" }]);
    assert!(chpl(&boxed(b"moov", &[])).is_empty());
}

#[test]
fn the_chapter_track_is_the_text_track_chap_names() {
    // an image chapter track first, passed over, then the text track
    let image = text_trak(3, b"vide", &[&[1, 10]], &[4], &[900]);
    let text = text_trak(2, b"text", &[&[1, 1500], &[2, 1750]], &[7, 8, 9], &[100]);
    let moov = boxed(b"moov", &[audio_trak(&[3, 2]), image, text].concat());
    let t = chapter_track(&moov).unwrap();
    assert_eq!((t.track_id, &t.language), (2, b"fre"));
    let spans: Vec<(u64, u64, u64, u32)> =
        t.samples.iter().map(|s| (s.start_ns, s.end_ns, s.at, s.len)).collect();
    assert_eq!(
        spans,
        [
            (0, 1_500_000_000, 100, 7),
            (1_500_000_000, 3_250_000_000, 107, 8),
            (3_250_000_000, 5_000_000_000, 115, 9)
        ]
    );
    assert_eq!(chapter_track(&boxed(b"moov", &audio_trak(&[9]))), None, "no such track");
    let none = boxed(b"moov", &[audio_trak(&[3]), text_trak(3, b"vide", &[], &[1], &[0])].concat());
    assert_eq!(chapter_track(&none), None, "an image track alone");
}

#[test]
fn adjacent_samples_are_one_read_and_a_cut_file_reads_zeros() {
    let mut file = vec![0u8; 64];
    file[10..17].copy_from_slice(b"\0\x05Intro");
    file[17..23].copy_from_slice(b"\0\x04Zwei");
    file[40..45].copy_from_slice(b"\0\x03End");
    // two chunks, the first holding two samples
    let trak = text_trak(2, b"text", &[&[3, 1000]], &[7, 6, 5, 0], &[10, 40]);
    let moov = boxed(b"moov", &[audio_trak(&[2]), trak].concat());
    let t = chapter_track(&moov).unwrap();
    let mut log = Vec::new();
    let read = drive_slice_log(&mut TextAt::new(t), &file, &mut log).unwrap();
    assert_eq!(log, [Need { at: 10, len: 13 }, Need { at: 40, len: 5 }]);
    let texts: Vec<&[u8]> = read.texts().map(|(_, b)| b).collect();
    assert_eq!(texts, [&b"\0\x05Intro"[..], b"\0\x04Zwei", b"\0\x03End", b""]);
    assert_eq!(read.samples[3].end_ns, read.samples[3].start_ns, "past the stts runs");
    let cut = drive_slice(&mut TextAt::new(chapter_track(&moov).unwrap()), &file[..42]).unwrap();
    assert_eq!(cut.texts().nth(2).map(|(_, b)| b), Some(&b"\0\x03\0\0\0"[..]));
}

#[test]
fn hostile_tables_are_capped_and_never_panic() {
    // more samples than are kept, a giant one, and the tables cut at every length
    let sizes = vec![1u32; MAX_SAMPLES + 10];
    let trak = text_trak(2, b"text", &[&[u32::MAX, u32::MAX]], &sizes, &[0]);
    let moov = boxed(b"moov", &[audio_trak(&[2]), trak].concat());
    let t = chapter_track(&moov).unwrap();
    assert_eq!(t.samples.len(), MAX_SAMPLES);
    let last = t.samples[MAX_SAMPLES - 1];
    assert!(last.end_ns > last.start_ns);
    let big = text_trak(2, b"text", &[&[1, 1]], &[u32::MAX], &[u32::MAX]);
    let moov = boxed(b"moov", &[audio_trak(&[2]), big].concat());
    assert_eq!(chapter_track(&moov).unwrap().samples[0].len, MAX_SAMPLE_READ);
    let trak = text_trak(2, b"text", &[&[2, 10]], &[3, 4], &[0]);
    let moov = boxed(b"moov", &[audio_trak(&[2]), trak, chpl_box(1, &[(1, "x")])].concat());
    for n in 0..moov.len() {
        let _ = chapter_track(&moov[..n]);
        let _ = chpl(&moov[..n]);
    }
    let mut garbage = moov.clone();
    for i in (8..garbage.len()).step_by(7) {
        garbage[i] ^= 0x5A;
        let _ = chapter_track(&garbage);
        let _ = chpl(&garbage);
    }
}
