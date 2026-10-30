//! Replays what the `mp4_demux` fuzzer found through its own rig (fuzz/src/rig.rs).

// Test fixtures allocate freely.
#![allow(clippy::disallowed_methods)]

#[path = "../../../../fuzz/src/mp4.rs"]
mod mp4;
#[path = "../../../../fuzz/src/rig.rs"]
#[allow(dead_code)]
mod rig;

#[global_allocator]
static HEAP: pf_sim::corrupt::Tracking = pf_sim::corrupt::Tracking;

static TINY_H264: &[u8] = include_bytes!("fixtures/tiny_h264.mp4");
static BFRAMES_H264: &[u8] = include_bytes!("fixtures/bframes_h264.mp4");
static TINY_AV: &[u8] = include_bytes!("fixtures/tiny_av.mp4");

fn play(control: &[u8; rig::CONTROL], file: &[u8]) -> Result<rig::Ran, rig::Broke> {
    rig::run(&rig::Plan::decode(control), file, &mp4::TARGET)
}

fn hex(s: &str) -> [u8; rig::CONTROL] {
    let bytes: Vec<u8> =
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).expect("hex")).collect();
    bytes.try_into().expect("a control block")
}

static ARTIFACTS: [(&str, &[u8]); 2] = [
    (
        "pulled_nothing_wanted_4251c979",
        include_bytes!("fixtures/fuzz/pulled_nothing_wanted_4251c979"),
    ),
    (
        "pulled_nothing_wanted_1adeb652",
        include_bytes!("fixtures/fuzz/pulled_nothing_wanted_1adeb652"),
    ),
];

#[test]
fn the_minimized_artifacts_end() {
    for (name, input) in ARTIFACTS {
        let (control, file) = input.split_at(rig::CONTROL);
        let ran = rig::run(&rig::Plan::decode(control), file, &mp4::TARGET);
        assert!(ran.is_ok(), "{name}: {}", ran.unwrap_err());
    }
}

#[test]
fn a_pulled_byte_seek_into_the_mdat_ends() {
    let control = hex("19 14 00 03 0a 00 00 20 00 00 00 00 06 00 00 00");
    for (name, file) in [("bframes", BFRAMES_H264), ("tiny", TINY_H264), ("tiny_av", TINY_AV)] {
        for variant in 0..4u8 {
            let mut c = control;
            c[0] = (c[0] & !6) | (variant << 1);
            for frac in [0x0800u16, 0x2000, 0x8000, 0xc000, 0xffff] {
                c[6..8].copy_from_slice(&frac.to_le_bytes());
                let ran = play(&c, file);
                assert!(
                    ran.is_ok(),
                    "{name} variant {variant} frac {frac:#x}: {}",
                    ran.unwrap_err()
                );
            }
        }
    }
}

/// Replays the artifact `PF_FUZZ_ARTIFACT` names, `--run-ignored only` to triage one.
#[test]
#[ignore]
fn replay_an_artifact() {
    let Some(path) = std::env::var_os("PF_FUZZ_ARTIFACT") else {
        return;
    };
    let input = std::fs::read(path).expect("the artifact");
    let (control, file) = input.split_at(input.len().min(rig::CONTROL));
    let plan = rig::Plan::decode(control);
    let ran = rig::run(&plan, file, &mp4::TARGET);
    eprintln!("{plan:?} {} bytes {ran:?}", file.len());
    assert!(ran.is_ok());
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    boxed(kind, &[&[0u8; 4], body].concat())
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn one_chunk_movie(entry: &[u8], samples: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
    let n = samples.len() as u32;
    let sizes: Vec<u32> = samples.iter().map(|s| s.len() as u32).collect();
    let moov = |offset: u32| {
        let mut tkhd = vec![0u8; 8];
        tkhd.extend(be32(&[1, 0, 0]));
        tkhd.extend([0u8; 60]);
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend([0u8; 13]);
        let mut stsd = be32(&[1]);
        stsd.extend_from_slice(entry);
        let mut stsz = be32(&[0, n]);
        stsz.extend(be32(&sizes));
        let stbl = [
            full(b"stsd", &stsd),
            full(b"stts", &be32(&[1, n, 1])),
            full(b"stsz", &stsz),
            full(b"stsc", &be32(&[1, 1, n, 1])),
            full(b"stco", &be32(&[1, offset])),
        ]
        .concat();
        let mut mdhd = vec![0u8; 8];
        mdhd.extend(be32(&[1000, n, 0]));
        let mdia =
            [full(b"mdhd", &mdhd), full(b"hdlr", &hdlr), boxed(b"minf", &boxed(b"stbl", &stbl))]
                .concat();
        let trak = boxed(b"trak", &[full(b"tkhd", &tkhd), boxed(b"mdia", &mdia)].concat());
        let mut mvhd = vec![0u8; 8];
        mvhd.extend(be32(&[1000, n]));
        mvhd.extend([0u8; 80]);
        boxed(b"moov", &[full(b"mvhd", &mvhd), trak].concat())
    };
    let ftyp = boxed(b"ftyp", b"isom\0\0\0\0isom");
    let head_len = (ftyp.len() + moov(0).len()) as u32;
    let head = [ftyp, moov(head_len + 8)].concat();
    let file = [head.clone(), boxed(b"mdat", &samples.concat())].concat();
    (head, file)
}

fn ramp(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn a_sample_larger_than_the_source_pool_lets_its_slots_go() {
    let sizes = [100_000, 10, 5_000];
    let samples: Vec<Vec<u8>> = sizes.iter().map(|n| ramp(*n)).collect();
    let (head, file) = one_chunk_movie(&boxed(b"raw ", &[0u8; 8]), &samples);
    let mut reader = pf_mp4::Mp4Reader::new(&head).expect("the movie resolves");
    let pool = profluens_core::memory::Pool::bounded(4096, 6);
    let mut got = Vec::new();
    for at in (0..file.len()).step_by(4096) {
        let mut chunk = pool.try_acquire().expect("the reader holds no slot while a sample waits");
        let n = 4096.min(file.len() - at);
        chunk.as_mut_full()[..n].copy_from_slice(&file[at..at + n]);
        chunk.set_len(n);
        reader.push(chunk);
        while let Some(s) = reader.next_sample() {
            got.push(s.payload.data().to_vec());
        }
    }
    let mdat = head.len() + 8;
    let want: Vec<Vec<u8>> = sizes
        .iter()
        .scan(mdat, |at, &size| {
            let s = file[*at..*at + size].to_vec();
            *at += size;
            Some(s)
        })
        .collect();
    assert_eq!(got, want);
}

#[test]
fn a_pulled_run_of_small_samples_leaves_room_for_their_copies() {
    let samples: Vec<Vec<u8>> = (0..100).map(|i| ramp(1200 + i)).collect();
    let (_, file) = one_chunk_movie(&boxed(b"raw ", &[0u8; 8]), &samples);
    let ran =
        play(&hex("01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00"), &file).expect("the run ends");
    assert_eq!(ran.buffers, 100);
}

#[test]
fn a_large_file_pushed_in_bytes_takes_passes_by_the_chunk_and_not_the_byte() {
    // The fuzzer timed out on files of 400 KiB pushed a byte a pass.
    let file = [TINY_H264, &boxed(b"free", &ramp(400 << 10))].concat();
    let ran =
        play(&hex("18 00 00 00 00 00 00 00 00 00 00 00 03 00 00 00"), &file).expect("the run ends");
    assert!(ran.passes < 3 * 4096, "{} passes", ran.passes);
}

fn avc1() -> Vec<u8> {
    let mut avcc = vec![1u8, 0x42, 0x00, 0x0A, 0xFF, 0xE1, 0, 4, 0x67, 0x42, 0x00, 0x0A];
    avcc.extend_from_slice(&[1, 0, 4, 0x68, 0xCE, 0x3C, 0x80]);
    let mut entry = vec![0u8; 78];
    entry[24..28].copy_from_slice(&[0, 64, 0, 48]);
    entry.extend(boxed(b"avcC", &avcc));
    boxed(b"avc1", &entry)
}

#[test]
fn pulled_samples_of_no_bytes_hold_no_range() {
    let mut samples = vec![Vec::new(); 8];
    samples.push([&100u32.to_be_bytes()[..], &[0x65; 100]].concat());
    let (_, file) = one_chunk_movie(&avc1(), &samples);
    let ran =
        play(&hex("01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00"), &file).expect("the run ends");
    assert_eq!(ran.buffers, 2, "the codec head and the sample");
}
