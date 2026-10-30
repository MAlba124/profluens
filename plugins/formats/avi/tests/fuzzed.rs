// Test fixtures allocate freely.
#![allow(clippy::disallowed_methods)]

#[path = "../../../../fuzz/src/avi.rs"]
mod avi;
#[path = "../../../../fuzz/src/rig.rs"]
#[allow(dead_code)]
mod rig;

#[global_allocator]
static HEAP: pf_sim::corrupt::Tracking = pf_sim::corrupt::Tracking;

static FIXTURE: &[u8] = include_bytes!("fixtures/tiny.avi");

fn hex(s: &str) -> [u8; rig::CONTROL] {
    let bytes: Vec<u8> =
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).expect("hex")).collect();
    bytes.try_into().expect("a control block")
}

const CONTROLS: [&str; 5] = [
    "00 04 00 00 00 00 00 00 00 00 00 00 01 00 00 00",
    "08 01 00 00 00 00 00 00 00 00 00 00 03 00 00 00",
    "11 1c 00 00 00 00 00 00 00 00 00 00 02 00 00 00",
    "01 0c 00 01 05 00 00 80 00 00 00 00 04 00 00 00",
    "10 05 00 0e 14 00 00 40 40 00 00 c0 05 00 00 00",
];

#[test]
fn every_constructor_plays_the_fixture_through_the_rig() {
    for control in CONTROLS {
        for variant in 0..4u8 {
            let mut c = hex(control);
            c[0] = (c[0] & !6) | (variant << 1);
            let ran = rig::run(&rig::Plan::decode(&c), FIXTURE, &avi::TARGET);
            let ran = ran.unwrap_or_else(|e| panic!("{control} variant {variant}: {e}"));
            assert!(
                ran.buffers > 0 && !ran.failed,
                "tiny.avi {control} variant {variant}: {ran:?}"
            );
        }
    }
}

#[test]
#[ignore]
fn replay_an_artifact() {
    let Some(path) = std::env::var_os("PF_FUZZ_ARTIFACT") else {
        return;
    };
    let input = std::fs::read(path).expect("the artifact");
    let (control, file) = input.split_at(input.len().min(rig::CONTROL));
    let plan = rig::Plan::decode(control);
    let ran = rig::run(&plan, file, &avi::TARGET);
    eprintln!("{plan:?} {} bytes {ran:?}", file.len());
    assert!(ran.is_ok());
}

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    chunk(b"LIST", &[kind.to_vec(), body.to_vec()].concat())
}

fn one_stream(sizes: &[usize], junk: usize) -> Vec<u8> {
    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&40_000u32.to_le_bytes());
    avih[16..20].copy_from_slice(&(sizes.len() as u32).to_le_bytes());
    let mut strh = vec![0u8; 56];
    strh[0..4].copy_from_slice(b"vids");
    strh[4..8].copy_from_slice(b"XVID");
    strh[20..24].copy_from_slice(&1u32.to_le_bytes());
    strh[24..28].copy_from_slice(&25u32.to_le_bytes());
    strh[32..36].copy_from_slice(&(sizes.len() as u32).to_le_bytes());
    let mut bitmap = vec![0u8; 40];
    bitmap[0..4].copy_from_slice(&40u32.to_le_bytes());
    bitmap[4..8].copy_from_slice(&160i32.to_le_bytes());
    bitmap[8..12].copy_from_slice(&120i32.to_le_bytes());
    bitmap[16..20].copy_from_slice(b"XVID");
    let strl = list(b"strl", &[chunk(b"strh", &strh), chunk(b"strf", &bitmap)].concat());
    let hdrl = list(b"hdrl", &[chunk(b"avih", &avih), strl].concat());
    let (mut movi, mut idx1) = (Vec::new(), Vec::new());
    for (i, size) in sizes.iter().enumerate() {
        idx1.extend_from_slice(b"00dc");
        idx1.extend_from_slice(&0x10u32.to_le_bytes());
        idx1.extend_from_slice(&(4 + movi.len() as u32).to_le_bytes());
        idx1.extend_from_slice(&(*size as u32).to_le_bytes());
        movi.extend(chunk(b"00dc", &vec![i as u8; *size]));
    }
    for _ in 0..junk {
        idx1.extend_from_slice(b"JUNK");
        idx1.extend_from_slice(&[0u8; 12]);
    }
    chunk(b"RIFF", &[b"AVI ".to_vec(), hdrl, list(b"movi", &movi), chunk(b"idx1", &idx1)].concat())
}

#[test]
fn a_pulled_frame_of_the_whole_byte_budget_waits_for_no_piece_of_idx1() {
    for (pools, big) in [(0u8, 120 << 10), (0, 127 << 10), (2, 1900 << 10), (2, 2000 << 10)] {
        let file = one_stream(&[big, 100, 3000, 50, 4000, 70], 70_000);
        for delay in [0u8, 1, 6] {
            for seed in 0..4u8 {
                let mut c = [0u8; rig::CONTROL];
                c[0] = 1 | (pools << 3);
                c[1] = delay << 3;
                c[12] = seed;
                let ran = rig::run(&rig::Plan::decode(&c), &file, &avi::TARGET);
                let ran = ran.unwrap_or_else(|e| {
                    panic!("pools {pools} frame {big} delay {delay} seed {seed}: {e}")
                });
                assert_eq!(
                    ran.buffers, 6,
                    "pools {pools} frame {big} delay {delay} seed {seed}: {ran:?}"
                );
            }
        }
    }
}
