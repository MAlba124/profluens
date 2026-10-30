//! The `ogg_demux` fuzzer's rig (fuzz/src/rig.rs) over a fixture, and what it finds replayed.

// Test fixtures allocate freely.
#![allow(clippy::disallowed_methods)]

#[path = "../../../../fuzz/src/ogg.rs"]
mod ogg;
#[path = "../../../../fuzz/src/rig.rs"]
#[allow(dead_code)]
mod rig;

#[global_allocator]
static HEAP: pf_sim::corrupt::Tracking = pf_sim::corrupt::Tracking;

static FIXTURE: &[u8] = include_bytes!("fixtures/celt-fb-stereo-128kbps.opus");

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
            let ran = rig::run(&rig::Plan::decode(&c), FIXTURE, &ogg::TARGET);
            let ran = ran.unwrap_or_else(|e| panic!("{control} variant {variant}: {e}"));
            assert!(
                ran.buffers > 0 && !ran.failed,
                "celt-fb-stereo-128kbps.opus {control} variant {variant}: {ran:?}"
            );
        }
    }
}

static ARTIFACTS: [(&str, &[u8]); 1] = [(
    "pulled_parked_behind_its_tail_b88854a2",
    include_bytes!("fixtures/fuzz/pulled_parked_behind_its_tail_b88854a2"),
)];

#[test]
fn the_artifacts_end() {
    for (name, input) in ARTIFACTS {
        let (control, file) = input.split_at(rig::CONTROL);
        let ran = rig::run(&rig::Plan::decode(control), file, &ogg::TARGET);
        assert!(ran.is_ok(), "{name}: {}", ran.unwrap_err());
    }
}

#[test]
fn a_pulled_tail_of_the_whole_byte_budget_is_taken_behind_a_parked_packet() {
    let file = [FIXTURE, &vec![0u8; (62 << 10) - FIXTURE.len()]].concat();
    for delay in [1u8, 3, 6] {
        for seed in 0..8u8 {
            let mut c = [0u8; rig::CONTROL];
            (c[0], c[1], c[12]) = (1 | 1 << 3, delay << 3, seed);
            let ran = rig::run(&rig::Plan::decode(&c), &file, &ogg::TARGET);
            let ran = ran.unwrap_or_else(|e| panic!("delay {delay} seed {seed}: {e}"));
            assert!(ran.buffers > 0 && !ran.failed, "delay {delay} seed {seed}: {ran:?}");
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
    let ran = rig::run(&plan, file, &ogg::TARGET);
    eprintln!("{plan:?} {} bytes {ran:?}", file.len());
    assert!(ran.is_ok());
}
