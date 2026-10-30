// Tests build cases and fixtures on the heap.
#![allow(clippy::disallowed_methods)]

use std::cell::RefCell;
use std::process::Command;

use pf_sim::corrupt::{
    Case, Chunking, Corpus, Input, Site, Tracking, Violation, check_heap, corpus_gate_quiet, gate,
    heap_bounded, heap_during, heap_growth, heap_mark, iterations, mutate, sweep_gate,
};

#[cfg(feature = "harness")]
mod harness;

#[global_allocator]
static HEAP: Tracking = Tracking;

#[test]
fn the_heap_mark_sees_a_peak_that_was_freed_again() {
    let mark = heap_mark();
    drop(vec![0u8; 100_000]);
    let grew = heap_growth(mark);
    assert!((100_000..110_000).contains(&grew), "{grew}");
    assert!(heap_growth(heap_mark()) == 0);
}

#[test]
fn heap_growth_is_per_thread() {
    let mark = heap_mark();
    std::thread::scope(|scope| {
        scope.spawn(|| drop(vec![0u8; 1 << 20]));
    });
    assert!(heap_growth(mark) < 64 * 1024);
}

#[test]
fn heap_during_reports_the_closure_and_its_peak() {
    let (kept, grew) = heap_during(|| {
        drop(vec![0u8; 50_000]);
        vec![0u8; 10]
    });
    assert_eq!(kept.len(), 10);
    assert!((50_000..60_000).contains(&grew), "{grew}");
}

#[test]
fn check_heap_fails_past_the_limit_with_both_numbers() {
    let mark = heap_mark();
    let held = vec![0u8; 4096];
    assert_eq!(check_heap(mark, 8192), Ok(()));
    let Err(Violation::Heap { grew, limit: 1000 }) = check_heap(mark, 1000) else {
        panic!("4096 bytes held under a limit of 1000");
    };
    assert!(grew >= held.len());
}

#[test]
fn heap_bounded_scales_the_limit_with_the_input() {
    let hungry = heap_bounded(64, 2, |bytes, _| drop(vec![0u8; bytes.len() * 3]));
    let case = |len: usize| Case {
        base: "zeros",
        ops: Vec::new(),
        data: vec![0; len],
        chunking: Chunking::Whole,
        knob: 0,
    };
    // 3 bytes a byte against 64 + 2 a byte crosses over at 64 bytes of input.
    assert_eq!(hungry(&case(32)), Ok(()));
    assert!(matches!(hungry(&case(4096)), Err(Violation::Heap { limit: 8256, .. })));
}

const SITES: [Site; 2] = [Site::be(0, 4), Site::le(8, 2)];

fn base() -> Vec<u8> {
    (0..64).collect()
}

fn fed_by_all_gates() -> Vec<Vec<u8>> {
    let fed = RefCell::new(Vec::new());
    let record = |case: &Case| {
        fed.borrow_mut().push(case.data.clone());
        Ok(())
    };
    let base = base();
    let make = |prng: &mut pf_sim::prng::Prng| {
        let (data, ops) = mutate(prng, &base, &SITES);
        Case { base: "ramp", ops, data, chunking: Chunking::pick(prng, 64), knob: prng.next_u64() }
    };
    gate("rig seeded", 50, &make, &record);
    sweep_gate("rig sweep", "ramp", &base, &SITES, 10, &record);
    let corpus = Corpus {
        name: "rig corpus",
        inputs: &[Input { name: "ramp", bytes: &base, fields: &[0, 8] }],
        tokens: &[b"OggS"],
        iters: iterations(50),
        sweep: 4,
        tolerate: &[],
    };
    corpus_gate_quiet(&corpus, &record);
    fed.into_inner()
}

#[test]
fn the_public_gates_feed_the_same_bytes_on_every_run() {
    let first = fed_by_all_gates();
    assert!(!first.is_empty());
    assert_eq!(first, fed_by_all_gates());
}

fn probe(vars: &[(&str, &str)]) -> (bool, String) {
    let mut child = Command::new(std::env::current_exe().expect("this test binary"));
    child.args(["--exact", "a_child_reports_how_many_cases_the_gates_fed", "--nocapture"]);
    for name in ["PF_FUZZ_ITERS", "PF_FUZZ_SEED", "PF_FUZZ_BASE", "PF_FUZZ_CASE"] {
        child.env_remove(name);
    }
    let output = child.env("PF_SIM_PROBE", "1").envs(vars.iter().copied()).output();
    let output = output.expect("run the child");
    let printed = [output.stdout, output.stderr].concat();
    (output.status.success(), String::from_utf8_lossy(&printed).into_owned())
}

#[test]
fn a_child_reports_how_many_cases_the_gates_fed() {
    if std::env::var_os("PF_SIM_PROBE").is_some() {
        println!("probe fed {} cases", fed_by_all_gates().len());
    }
}

#[test]
fn the_variables_reach_the_gates_through_a_real_environment() {
    let fed = |vars: &[(&str, &str)]| {
        let (passed, printed) = probe(vars);
        assert!(passed, "{printed}");
        let count = printed.split("probe fed ").nth(1).and_then(|rest| rest.split(' ').next());
        count.and_then(|count| count.parse::<usize>().ok()).expect("a count from the child")
    };
    let default = fed(&[]);
    assert_eq!(fed(&[("PF_FUZZ_CASE", "0x2A")]), 2);
    assert_eq!(fed(&[("PF_FUZZ_CASE", "42"), ("PF_FUZZ_ITERS", "900")]), 2);
    assert!(fed(&[("PF_FUZZ_ITERS", "1000")]) > default + 1900);
    let (passed, printed) = probe(&[("PF_FUZZ_CASE", "0x2A oops")]);
    assert!(!passed && printed.contains("PF_FUZZ_CASE is `0x2A oops`"), "{printed}");
}
