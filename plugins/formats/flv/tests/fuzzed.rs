//! The `flv_demux` fuzzer rig (fuzz/src/rig.rs) over the fixtures, and replay of what it finds.

#![allow(clippy::disallowed_methods)] // test fixtures

#[path = "../../../../fuzz/src/flv.rs"]
mod flv;
#[path = "../../../../fuzz/src/rig.rs"]
#[allow(dead_code, clippy::collapsible_if)]
mod rig;

#[global_allocator]
static HEAP: pf_sim::corrupt::Tracking = pf_sim::corrupt::Tracking;

static FIXTURES: [&[u8]; 3] = [
    include_bytes!("fixtures/h264_aac.flv"),
    include_bytes!("fixtures/h264_mp3.flv"),
    include_bytes!("fixtures/hevc_aac.flv"),
];

fn hex(s: &str) -> [u8; rig::CONTROL] {
    let bytes: Vec<u8> =
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).expect("hex")).collect();
    bytes.try_into().expect("a control block")
}

/// Seed control blocks of `cargo xtask seeds` (xtask/src/seeds.rs).
const CONTROLS: [&str; 5] = [
    "00 04 00 00 00 00 00 00 00 00 00 00 01 00 00 00",
    "08 01 00 00 00 00 00 00 00 00 00 00 03 00 00 00",
    "11 1c 00 00 00 00 00 00 00 00 00 00 02 00 00 00",
    "01 0c 00 01 05 00 00 80 00 00 00 00 04 00 00 00",
    "10 05 00 0e 14 00 00 40 40 00 00 c0 05 00 00 00",
];

#[test]
fn every_head_plays_the_fixtures_through_the_rig() {
    for (n, file) in FIXTURES.iter().enumerate() {
        for control in CONTROLS {
            for variant in 0..4u8 {
                let mut c = hex(control);
                c[0] = (c[0] & !6) | (variant << 1);
                let ran = rig::run(&rig::Plan::decode(&c), file, &flv::TARGET);
                let ran = ran.unwrap_or_else(|e| panic!("{n} {control} variant {variant}: {e}"));
                assert!(ran.buffers > 0 && !ran.failed, "{n} {control} variant {variant}: {ran:?}");
            }
        }
    }
}

/// Replays the artifact `PF_FUZZ_ARTIFACT` names, `--run-ignored only` to triage one.
#[test]
#[ignore]
fn replay_an_artifact() {
    let Some(path) = std::env::var_os("PF_FUZZ_ARTIFACT") else { return };
    let input = std::fs::read(path).expect("the artifact");
    let (control, file) = input.split_at(input.len().min(rig::CONTROL));
    let plan = rig::Plan::decode(control);
    let ran = rig::run(&plan, file, &flv::TARGET);
    eprintln!("{plan:?} {} bytes {ran:?}", file.len());
    assert!(ran.is_ok());
}
