use super::*;

const FRAME: usize = 417;

const RATE: u64 = 44_100;

fn frames(count: usize, tag: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..count.max(2) {
        let mut f = vec![0xFF, 0xFB, 0x90, 0x00];
        f.resize(FRAME, 0);
        if i == 0 {
            f[36..36 + tag.len()].copy_from_slice(tag);
        }
        out.extend_from_slice(&f);
    }
    out
}

fn xing(count: u32, toc: &[u8; 100]) -> Vec<u8> {
    let mut b = b"Xing".to_vec();
    b.extend_from_slice(&5u32.to_be_bytes());
    b.extend_from_slice(&count.to_be_bytes());
    b.extend_from_slice(toc);
    b
}

fn id3v2(body: usize) -> Vec<u8> {
    let n = body as u32;
    let mut t = b"ID3\x03\x00\x00".to_vec();
    t.extend_from_slice(&[
        (n >> 21) as u8 & 0x7F,
        (n >> 14) as u8 & 0x7F,
        (n >> 7) as u8 & 0x7F,
        n as u8 & 0x7F,
    ]);
    t.resize(10 + body, 0);
    t
}

fn linear() -> [u8; 100] {
    std::array::from_fn(|i| (i * 256 / 100) as u8)
}

fn scan_of(file: &[u8], chunk: usize) -> (Index, Option<Layout>) {
    let mut scan = HeadScan::new(Some(file.len() as u64));
    for part in file.chunks(chunk) {
        if let Some(index) = scan.feed(part) {
            assert!(scan.done());
            return (index, scan.layout());
        }
    }
    let index = scan.end().expect("the end gives what was read");
    (index, scan.layout())
}

fn scanned(file: &[u8], chunk: usize) -> Index {
    scan_of(file, chunk).0
}

fn pairs(index: &Index) -> Vec<(u64, u64)> {
    index.entries.iter().map(|e| (e.time_ns, e.byte)).collect()
}

struct Noise(u64);

impl Noise {
    fn byte(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as u8).min(0xFE)
    }
}

const KBPS: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];

fn head(kbps: u32, pad: bool) -> [u8; 4] {
    let index = KBPS.iter().position(|&k| k == kbps).expect("a bitrate of the ladder") as u8;
    [0xFF, 0xFB, (index << 4) | (u8::from(pad) << 1), 0x00]
}

fn vbr_frame(
    word: &[u8; 4],
    kbps: u32,
    count: u32,
    bytes: u32,
    delay: u16,
    padding: u16,
) -> Vec<u8> {
    let mut f = head(kbps, false).to_vec();
    f.resize((144_000 * u64::from(kbps) / RATE) as usize, 0);
    let mut x = word.to_vec();
    x.extend_from_slice(&0x0Fu32.to_be_bytes());
    x.extend_from_slice(&count.to_be_bytes());
    x.extend_from_slice(&bytes.to_be_bytes());
    x.extend_from_slice(&linear());
    x.extend_from_slice(&[0; 4]);
    let mut lame = b"LAME3.100".to_vec();
    lame.resize(0x15, 0);
    lame.extend_from_slice(&[
        (delay >> 4) as u8,
        ((delay & 0xF) << 4) as u8 | (padding >> 8) as u8,
        padding as u8,
    ]);
    lame.resize(36, 0);
    x.extend_from_slice(&lame);
    f[36..36 + x.len()].copy_from_slice(&x);
    f
}

struct Stream {
    bytes: Vec<u8>,
    starts: Vec<u64>,
}

fn stream(
    tag: usize,
    word: Option<&[u8; 4]>,
    header_kbps: u32,
    rates: &[u32],
    count: usize,
    phase: u64,
) -> Stream {
    let mut noise = Noise(count as u64 ^ phase);
    let mut audio = Vec::new();
    let mut starts = Vec::new();
    for m in 0..count as u64 {
        let kbps = rates[m as usize % rates.len()];
        let num = 144_000 * u64::from(kbps);
        let len = ((m + 1) * num + phase) / RATE - (m * num + phase) / RATE;
        starts.push(audio.len() as u64);
        let mut f = head(kbps, len > num / RATE).to_vec();
        f.extend((4..len).map(|_| noise.byte()));
        audio.extend_from_slice(&f);
    }
    let mut bytes = if tag > 0 { id3v2(tag - 10) } else { Vec::new() };
    let mut lead = Vec::new();
    if let Some(word) = word {
        let size = (144_000 * u64::from(header_kbps) / RATE) as u32;
        lead.push(bytes.len() as u64);
        bytes.extend_from_slice(&vbr_frame(
            word,
            header_kbps,
            count as u32,
            size + audio.len() as u32,
            576,
            300,
        ));
    }
    let at = bytes.len() as u64;
    bytes.extend_from_slice(&audio);
    lead.extend(starts.iter().map(|s| s + at));
    Stream { bytes, starts: lead }
}

fn land(clock: &mut Clock, file: &[u8], at: u64) -> Option<(u64, Option<u64>)> {
    clock.restart();
    let mut base = at;
    loop {
        match clock.resync(&file[base as usize..], base) {
            Resync::Drop(0) => return None,
            Resync::Drop(n) => base += n as u64,
            Resync::Frame { skip, k } => return Some((base + skip as u64, k)),
            Resync::GaveUp => return None,
        }
    }
}

#[test]
fn a_toc_is_a_hundred_points_past_the_tag_and_complete() {
    let tag = id3v2(500);
    let mut file = tag.clone();
    file.extend_from_slice(&frames(8, &xing(1000, &linear())));
    file.resize(tag.len() + 400_000, 0);
    let (len, start) = (file.len() as u64, tag.len() as u64);
    let want = 1000 * 1152 * 1_000_000_000 / 44_100;
    for chunk in [1, 7, 4096, 1 << 20] {
        let index = scanned(&file, chunk);
        assert_eq!(index.duration, Timestamp(want), "chunk {chunk}");
        assert!(index.complete);
        assert_eq!(index.file_len, Some(len));
        let p = pairs(&index);
        assert_eq!(p.len(), 100);
        assert_eq!(p[0], (0, start));
        assert_eq!(p[50], (want / 2, start + (len - start) / 2));
        assert_eq!(p[99], (want * 99 / 100, start + 253 * (len - start) / 256));
        assert!(index.entries.iter().all(|e| e.restart == e.byte && e.restart_ns == e.time_ns));
    }
}

#[test]
fn a_constant_bitrate_stream_is_a_line_of_frames_and_incomplete() {
    let tag = id3v2(1_000);
    let mut file = tag.clone();
    file.extend_from_slice(&frames(8, &[]));
    file.resize(tag.len() + 160_000, 0);
    let start = tag.len() as u64;
    let (index, layout) = scan_of(&file, 333);
    let layout = layout.expect("a layout");
    assert_eq!(index.duration, Timestamp(10_000_000_000));
    assert!(!index.complete, "the length is an estimate");
    assert_eq!((layout.first, layout.audio, layout.header, layout.lead), (start, start, false, 0));
    // 511 bytes of reservoir over frames of 417, two and two.
    assert_eq!(layout.preroll, 4);
    let size = (144 * 128_000, 44_100);
    assert_eq!(layout.cbr, Some(size));
    let p = pairs(&index);
    assert_eq!(p.len(), 11);
    assert_eq!(p[0], (0, start));
    for (i, e) in index.entries.iter().enumerate().skip(1) {
        let r = 38 * i as u64;
        let byte = start + r * size.0 / size.1 - 1;
        assert_eq!((e.byte, e.restart), (byte, byte), "entry {i}");
        assert_eq!(e.restart_ns, r * 1152 * 1_000_000_000 / RATE);
        assert_eq!(e.time_ns, (r + 4) * 1152 * 1_000_000_000 / RATE);
    }
}

#[test]
fn trailing_tags_are_counted_as_audio() {
    let mut file = frames(8, &[]);
    file.resize(160_000, 0);
    file.extend_from_slice(b"TAG");
    file.resize(160_128, 0);
    let index = scanned(&file, 4096);
    assert_eq!(index.duration, Timestamp(10_008_000_000));
}

#[test]
fn without_a_length_only_a_table_maps() {
    let mut cbr = frames(8, &[]);
    cbr.resize(160_000, 0);
    let mut scan = HeadScan::new(None);
    let index = scan.feed(&cbr).expect("the window is in");
    assert!(index.entries.is_empty() && index.duration.is_none(), "{index:?}");
    assert_eq!(scan.feed(&cbr), None, "given once");

    let mut vbr = frames(8, &xing(1000, &linear()));
    vbr.resize(160_000, 0);
    let index = HeadScan::new(None).feed(&vbr).expect("the window is in");
    assert!(index.entries.is_empty() && index.duration.is_some(), "{index:?}");
}

#[test]
fn a_short_stream_gives_what_it_holds_at_the_end() {
    let mut scan = HeadScan::new(Some(3));
    assert_eq!(scan.feed(b"ID3"), None);
    let index = scan.end().expect("given at the end");
    assert!(index.entries.is_empty());
    assert_eq!(scan.end(), None);
}

#[test]
fn a_line_of_one_bitrate_lands_and_stamps_every_frame_exactly() {
    let info = Some(b"Info");
    for (tag, word, header_kbps, kbps) in [
        (0, None, 0, 128),
        (39_532, info, 320, 320),
        (45, info, 128, 128),
        (2_000, info, 64, 32),
        (0, None, 0, 320),
    ] {
        for phase in [0, RATE - 1, 20_000] {
            let s = stream(tag, word, header_kbps, &[kbps], 1_500, phase);
            let name = format!("tag {tag} header {header_kbps} kbps {kbps} phase {phase}");
            let (index, layout) = scan_of(&s.bytes, 4096);
            let layout = layout.expect("a layout");
            assert!(layout.cbr.is_some(), "{name}");
            assert_eq!(index.complete, word.is_some(), "{name}: a header counts the frames");
            assert_eq!(layout.first, tag as u64, "{name}");
            assert_eq!(layout.audio, s.starts[usize::from(layout.header)], "{name}");
            let lead = if word.is_some() { 1152 + 576 + DECODER_DELAY } else { 0 };
            assert_eq!(layout.lead, lead, "{name}");
            if word.is_some() {
                assert_eq!(
                    index.duration,
                    Timestamp((1500 * 1152 - 876) * 1_000_000_000 / RATE),
                    "{name}"
                );
            }
            assert_eq!(index.entries[0], IndexEntry::key(0, tag as u64), "{name}");
            assert!(
                index
                    .entries
                    .windows(2)
                    .all(|w| w[0].time_ns < w[1].time_ns && w[0].byte < w[1].byte)
            );
            let mut clock = Clock::new(layout, &index);
            for e in &index.entries[1..] {
                let (at, k) = land(&mut clock, &s.bytes, e.restart).expect("a frame");
                let k = k.unwrap_or_else(|| panic!("{name}: the frame at {at} is named"));
                assert_eq!(at, s.starts[k as usize], "{name}: entry {e:?}");
                assert!(
                    e.restart < at && at - e.restart <= MARGIN + 1,
                    "{name}: {} bytes ahead",
                    at - e.restart
                );
                assert_eq!(layout.time_ns(k), e.restart_ns, "{name}");
                assert_eq!(layout.time_ns(k + layout.preroll), e.time_ns, "{name}");
            }
            for (k, &at) in s.starts.iter().enumerate() {
                assert_eq!(layout.frame_at(at), Some(k as u64), "{name}: frame {k}");
            }
            let mut noise = Noise(phase + 7);
            for _ in 0..300 {
                let at = u64::from(noise.byte()) << 16
                    | u64::from(noise.byte()) << 8
                    | u64::from(noise.byte());
                let at = at % s.bytes.len() as u64;
                let want = s.starts.iter().position(|&f| f >= at.max(tag as u64));
                match (land(&mut clock, &s.bytes, at), want) {
                    (Some((got, k)), Some(want)) => {
                        assert_eq!(got, s.starts[want], "{name}: a seek to {at}");
                        assert_eq!(k, Some(want as u64), "{name}: a seek to {at}");
                    }
                    (None, None) => {}
                    (got, want) => panic!("{name}: a seek to {at} found {got:?}, not {want:?}"),
                }
            }
        }
    }
}

#[test]
fn many_bitrates_grow_exact_restarts_from_the_frames_read() {
    let s = stream(300, Some(b"Xing"), 128, &[128, 160, 192, 96, 320], 1_200, 0);
    let (index, layout) = scan_of(&s.bytes, 1000);
    let layout = layout.expect("a layout");
    assert_eq!(layout.cbr, None);
    assert!(index.complete && index.entries.len() == 100, "the table");
    let mut clock = Clock::new(layout, &index);
    assert_eq!(land(&mut clock, &s.bytes, 0), Some((300, Some(0))), "past the tag");
    let (mut dues, mut grown) = (0, None);
    let mut at = 300u64;
    let mut noise = Noise(3);
    while (at as usize) < s.bytes.len() {
        let n = (usize::from(noise.byte()) * 17 + 1).min(s.bytes.len() - at as usize);
        if clock.walk(&s.bytes[at as usize..at as usize + n], at) {
            dues += 1;
            grown = clock.index();
        }
        at += n as u64;
    }
    assert_eq!(clock.index(), None, "nothing new");
    let grown = grown.expect("grown");
    assert_eq!(dues, 32 / GROW_EVERY, "published every {GROW_EVERY}");
    assert_eq!(grown.entries.iter().filter(|e| s.starts.contains(&e.restart)).count(), 32);
    let restarts: Vec<&IndexEntry> = grown.entries.iter().take(32).collect();
    for (i, e) in restarts.iter().enumerate() {
        let r = 38 * i as u64;
        assert_eq!(e.restart, s.starts[r as usize], "restart {i}");
        assert_eq!(**e, layout.entry(r, e.restart));
        assert_eq!(land(&mut clock, &s.bytes, e.restart), Some((e.restart, Some(r))));
    }
    assert!(grown.complete, "the table holds past them");
    assert!(
        grown.entries.windows(2).all(|w| w[0].time_ns < w[1].time_ns && w[0].byte <= w[1].byte)
    );
    let toc = index.entries[50].byte;
    let (found, k) = land(&mut clock, &s.bytes, toc).expect("a frame");
    assert!(s.starts.contains(&found));
    assert_eq!(k, None);
}

#[test]
fn a_frame_of_another_bitrate_breaks_the_line() {
    let mut rates = vec![128u32; 400];
    rates[200] = 160;
    let s = stream(0, None, 0, &rates, 400, 0);
    let (index, layout) = scan_of(&s.bytes, 4096);
    let layout = layout.expect("a layout");
    assert!(layout.cbr.is_some(), "the head is of one bitrate");
    let mut clock = Clock::new(layout, &index);
    assert_eq!(land(&mut clock, &s.bytes, 0), Some((0, Some(0))));
    let cut = s.starts[150] as usize;
    assert!(!clock.walk(&s.bytes[..cut], 0));
    assert_eq!(clock.index(), None);
    assert!(clock.walk(&s.bytes[cut..], cut as u64), "the break is published");
    assert_eq!(clock.layout().cbr, None);
    let after = clock.index().expect("republished");
    assert!(!after.complete);
    let grown: Vec<&IndexEntry> =
        after.entries.iter().filter(|e| s.starts.contains(&e.restart)).collect();
    assert!(grown.iter().any(|e| e.restart > s.starts[200]), "restarts past the break");
    assert_eq!(clock.frame_at(s.starts[300] + 1), None);
}

#[test]
fn hostile_heads_never_panic_or_leave_the_file() {
    let mut mp3 = id3v2(64);
    mp3.extend_from_slice(&frames(4, &xing(1000, &linear())));
    mp3.resize(mp3.len() + 4_000, 0);
    let garbage: Vec<u8> = (0..3_000).map(|i| (i as u8).wrapping_mul(31) | 1).collect();
    for src in [&mp3, &garbage] {
        for n in 0..=src.len() {
            let mut scan = HeadScan::new(Some(n as u64));
            let index = scan.feed(&src[..n]).or_else(|| scan.end()).expect("an index");
            assert!(index.entries.iter().all(|e| e.byte < n as u64), "cut {n}");
            assert!(index.entries.windows(2).all(|w| w[0].time_ns < w[1].time_ns), "cut {n}");
        }
    }
}

#[test]
fn hostile_bytes_never_panic_a_clock() {
    let s = stream(100, Some(b"Info"), 128, &[128], 300, 0);
    let (index, layout) = scan_of(&s.bytes, 4096);
    let layout = layout.expect("a layout");
    let len = s.bytes.len();
    for seed in 0..64u64 {
        let mut noise = Noise(seed);
        // Sync words are common, the frames found in the noise mostly false.
        let mut bytes = s.bytes.clone();
        for _ in 0..(seed as usize * 40) {
            let at = (usize::from(noise.byte()) << 8 | usize::from(noise.byte())) * 7 % len;
            bytes[at] = if noise.byte() < 128 { 0xFF } else { noise.byte() | 0xE0 };
        }
        let mut clock = Clock::new(layout, &index);
        let mut at = (u64::from(noise.byte()) * 97) % len as u64;
        clock.restart();
        while (at as usize) < len {
            let n = (usize::from(noise.byte()) * 5 + 1).min(len - at as usize);
            let piece = &bytes[at as usize..at as usize + n];
            if clock.seeking() {
                match clock.resync(piece, at) {
                    Resync::Drop(d) => at += d.max(1) as u64,
                    Resync::Frame { skip, .. } => {
                        at += skip as u64;
                        let _ = clock.walk(&bytes[at as usize..at as usize + n - skip], at);
                        at += (n - skip) as u64;
                    }
                    Resync::GaveUp => at += n as u64,
                }
            } else {
                let _ = clock.walk(piece, at);
                at += n as u64;
            }
            if noise.byte() == 7 {
                clock.restart();
            }
            if let Some(index) = clock.index() {
                assert!(index.entries.iter().all(|e| e.byte < len as u64));
                assert!(
                    index
                        .entries
                        .windows(2)
                        .all(|w| w[0].time_ns < w[1].time_ns && w[0].byte <= w[1].byte)
                );
            }
        }
    }
}
