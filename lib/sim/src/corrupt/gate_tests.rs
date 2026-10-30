//! Gate loop unit tests. They pass an [`Env`] by value, never the process environment.

use super::super::corpus::Input;
use super::super::sites::{mutate, noise};
use super::*;

const SITES: [Site; 2] = [Site::be(0, 4), Site::le(8, 2)];

fn ramp() -> Vec<u8> {
    (0..64).collect()
}

fn make(prng: &mut Prng) -> Case {
    let (data, ops) = mutate(prng, &ramp(), &SITES);
    let chunking = Chunking::pick(prng, data.len());
    Case { base: "ramp", ops, data, chunking, knob: prng.next_u64() }
}

fn noisy(prng: &mut Prng) -> Case {
    let (data, ops) = noise(prng, 64);
    Case { base: "noise", ops, data, chunking: Chunking::Whole, knob: 0 }
}

fn corpus<'a>(inputs: &'a [Input<'a>], iters: u64) -> Corpus<'a> {
    Corpus { name: "unit corpus", inputs, tokens: &[b"fLaC"], iters, sweep: 4, tolerate: &[] }
}

fn fed_by(body: impl FnOnce(Drive) -> Summary) -> Vec<(Vec<u8>, Chunking)> {
    let fed = RefCell::new(Vec::new());
    let summary = body(&|case| {
        fed.borrow_mut().push((case.data.clone(), case.chunking));
        Ok(())
    });
    assert_eq!(summary.cases, fed.borrow().len() as u64);
    fed.into_inner()
}

fn failure_of(body: impl FnOnce() -> Summary) -> String {
    let payload = panic::catch_unwind(AssertUnwindSafe(body)).expect_err("the gate passed");
    payload.downcast_ref::<String>().expect("a report").clone()
}

#[test]
fn a_seeded_gate_feeds_the_same_bytes_for_the_same_seed() {
    let env = Env::default();
    let first = fed_by(|drive| site_gate(&env, "unit", 200, &make, drive));
    let again = fed_by(|drive| site_gate(&env, "unit", 200, &make, drive));
    assert_eq!(first.len(), 200);
    assert_eq!(first, again);
    // A prefix of a longer run, so raising the count only adds cases.
    let longer = fed_by(|drive| site_gate(&env, "unit", 300, &make, drive));
    assert_eq!(first, longer[..200]);
}

#[test]
fn the_base_seed_and_the_gate_name_each_move_every_case() {
    let env = Env::default();
    let moved = Env { seed: Some(7), ..Env::default() };
    let first = fed_by(|drive| site_gate(&env, "unit", 50, &make, drive));
    assert_ne!(first, fed_by(|drive| site_gate(&moved, "unit", 50, &make, drive)));
    assert_ne!(first, fed_by(|drive| site_gate(&env, "other", 50, &make, drive)));
}

#[test]
fn the_environment_count_replaces_the_default() {
    let env = Env { iters: Some(13), ..Env::default() };
    assert_eq!(site_gate(&env, "unit", 200, &make, &|_| Ok(())).cases, 13);
}

#[test]
fn a_replay_runs_exactly_the_one_case_of_its_seed() {
    let seed = 0xDEAD_BEEF;
    let env = Env { case: Some(seed), iters: Some(500), ..Env::default() };
    let want = make(&mut Prng::new(seed));
    let fed = fed_by(|drive| site_gate(&env, "unit", 200, &make, drive));
    assert_eq!(fed, [(want.data, want.chunking)]);
    assert!(site_gate(&env, "unit", 200, &make, &|_| Ok(())).replay);
    assert!(!site_gate(&Env::default(), "unit", 2, &make, &|_| Ok(())).replay);

    let ramp = ramp();
    let swept = fed_by(|drive| site_sweep(&env, "unit", "ramp", &ramp, &SITES, 9, drive));
    assert!(swept.is_empty());

    let inputs = [Input { name: "ramp", bytes: &ramp, fields: &[0, 8] }];
    let corpus = corpus(&inputs, 50);
    let want = corpus::draw(&mut Prng::new(seed), corpus.inputs, corpus.tokens);
    let fed = fed_by(|drive| corpus_run(&env, &corpus, drive));
    assert_eq!(fed, [(want.data, want.chunking)]);
    assert!(corpus_run(&env, &corpus, &|_| Ok(())).replay);
    assert!(!corpus_run(&Env::default(), &corpus, &|_| Ok(())).replay);
}

#[test]
fn a_sweep_takes_its_budget_by_default_and_everything_in_a_soak() {
    let ramp = ramp();
    let all = sweep(&SITES).len() as u64 * 2;
    let run = |env: &Env, sites: &[Site], budget| {
        site_sweep(env, "unit", "ramp", &ramp, sites, budget, &|_| Ok(())).cases
    };
    assert_eq!(run(&Env::default(), &SITES, 9), 9);
    assert_eq!(run(&Env::default(), &SITES, 0), 1);
    assert_eq!(run(&Env::default(), &SITES, u64::MAX), all);
    assert_eq!(run(&Env { iters: Some(1), ..Env::default() }, &SITES, 9), all);
    assert_eq!(run(&Env::default(), &[], 9), 0);
}

#[test]
fn a_corpus_gate_runs_its_plan_then_its_seeded_cases() {
    let ramp = ramp();
    let inputs = [Input { name: "ramp", bytes: &ramp, fields: &[0, 8] }];
    let corpus = corpus(&inputs, 40);
    let env = Env::default();
    let planned = corpus::plan(&inputs, 4, false).len();
    let fed = fed_by(|drive| corpus_run(&env, &corpus, drive));
    assert_eq!(fed.len(), planned + 40);
    assert!(fed[..planned].iter().any(|(data, _)| *data == ramp));
    assert_eq!(fed, fed_by(|drive| corpus_run(&env, &corpus, drive)));
    let soak = Env { iters: Some(40), ..Env::default() };
    assert!(corpus_run(&soak, &corpus, &|_| Ok(())).cases > fed.len() as u64);
}

#[test]
fn a_violation_fails_with_a_seed_that_replays_the_same_bytes() {
    let env = Env::default();
    let too_long = |case: &Case| check_output(case.data.len(), 40);
    let report = failure_of(|| site_gate(&env, "unit long", 500, &noisy, &too_long));
    assert!(report.contains("corruption gate `unit long` failed, "), "{report}");
    assert!(report.contains(" emitted or buffered, limit 40"), "{report}");
    assert!(report.contains("base       noise"), "{report}");
    assert!(report.contains("minimized  41 bytes, fed as Whole"), "{report}");

    let hint = "replay with PF_FUZZ_CASE=0x";
    let digits = &report[report.find(hint).expect("a replay hint") + hint.len()..];
    let digits = &digits[..digits.find(')').expect("the end of the hint")];
    let seed = u64::from_str_radix(digits, 16).expect("a hex seed");
    let replay = Env { case: Some(seed), ..Env::default() };
    let again = failure_of(|| site_gate(&replay, "unit long", 500, &noisy, &too_long));
    let head = |text: &str| text[..text.find("minimized").expect("a minimized line")].to_string();
    assert_eq!(head(&report), head(&again));
}

#[test]
fn a_panic_fails_with_its_message_and_location() {
    let env = Env::default();
    let brittle = |case: &Case| {
        assert!(!case.data.contains(&0x42), "met byte 0x42");
        Ok(())
    };
    let report = failure_of(|| site_gate(&env, "unit panic", 2_000, &noisy, &brittle));
    assert!(report.contains("failed, it panicked"), "{report}");
    assert!(report.contains("panic      met byte 0x42"), "{report}");
    assert!(report.contains("gate_tests.rs:"), "{report}");
    assert!(report.contains("minimized  1 bytes, fed as Whole\n  [0x42]"), "{report}");
}

#[test]
fn a_failing_sweep_case_says_it_has_no_seed() {
    let ramp = ramp();
    let short = |case: &Case| check_steps(64, case.data.len() as u64);
    let report =
        failure_of(|| site_sweep(&Env::default(), "unit", "ramp", &ramp, &SITES, 99, &short));
    assert!(report.contains("seed       none, a deterministic sweep case"), "{report}");
    assert!(report.contains("corruption [Truncate { at: 0 }]"), "{report}");
    assert!(report.contains("64 steps taken, limit 0"), "{report}");
}

#[test]
fn a_tolerated_panic_is_counted_and_any_other_still_fails() {
    let ramp = ramp();
    let inputs = [Input { name: "ramp", bytes: &ramp, fields: &[0] }];
    let always = |_: &Case| -> Result<(), Violation> { panic!("from a dependency") };
    let tolerant = Corpus { tolerate: &["corrupt/gate_tests.rs"], ..corpus(&inputs, 20) };
    let summary = corpus_run(&Env::default(), &tolerant, &always);
    assert!(summary.cases > 20);
    assert_eq!(summary.tolerated, summary.cases);
    assert_eq!(summary.tolerated_at.len(), 1);

    let strict = Corpus { tolerate: &["some/other/crate"], ..corpus(&inputs, 20) };
    let report = failure_of(|| corpus_run(&Env::default(), &strict, &always));
    assert!(report.contains("panic      from a dependency"), "{report}");
    assert!(report.contains("base       ramp"), "{report}");
}

#[test]
fn a_failure_dumps_the_input_and_its_minimized_form() {
    let dir = std::env::temp_dir().join(format!("pf_sim_dump_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let env = Env { dump: Some(dir.to_string_lossy().into_owned()), ..Env::default() };
    let too_long = |case: &Case| check_output(case.data.len(), 40);
    let report = failure_of(|| site_gate(&env, "unit dump/1", 500, &noisy, &too_long));
    let mut sizes = Vec::new();
    for line in report.lines().filter_map(|line| line.strip_prefix("  dumped     ")) {
        assert!(line.contains("/unit_dump_1-"), "{line}");
        sizes.push(std::fs::read(line).expect("the dumped file").len());
    }
    std::fs::remove_dir_all(&dir).expect("cleanup");
    assert!(sizes.len() == 2 && sizes[0] > 40 && sizes[1] == 41, "{sizes:?}");
}

#[test]
fn long_ops_are_cut_in_the_report() {
    let bytes = vec![0xAB; 4096];
    let op = Op::Splice { at: 0, remove: 0, bytes: bytes.clone() };
    let case = Case { base: "x", ops: vec![op], data: bytes, chunking: Chunking::Whole, knob: 0 };
    let text = describe("headline", &Drawn { seed: Some(1), origin: "x", case });
    assert!(text.len() < OPS_SHOWN + 400, "{} characters", text.len());
    assert!(text.contains(" and more, cut here"));
}

#[test]
fn shrinking_keeps_only_what_the_failure_needs() {
    let needs = |probe: &Case| probe.data.contains(&7) && probe.data.contains(&200);
    for len in [0usize, 1, 2, 255, 256, 1000] {
        let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let chunking = Chunking::Whole;
        let mut probe = Case { base: "x", ops: Vec::new(), data, chunking, knob: 0 };
        let failed = needs(&probe);
        shrink(&mut probe, &|probe| needs(probe) == failed);
        let want: &[u8] = if failed { &[7, 200] } else { &[] };
        assert_eq!(probe.data, want, "from {len} bytes");
    }
}

#[test]
fn the_watch_blames_only_a_case_that_is_still_running() {
    let watch = Watch::new();
    assert_eq!(watch.overdue(Duration::ZERO), None);
    watch.begin(3);
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(watch.overdue(Duration::from_millis(1)), Some(3));
    assert_eq!(watch.overdue(Duration::from_secs(3600)), None);
    watch.end();
    assert_eq!(watch.overdue(Duration::ZERO), None);
}

#[test]
fn verdicts_match_by_panic_site_and_by_kind_of_violation() {
    let panic_at = |at: &str| Verdict::Panic { message: String::new(), location: at.to_string() };
    let heap = |grew| Verdict::Violation(Violation::Heap { grew, limit: 1 });
    let invariant = |what| Verdict::Violation(Violation::Invariant(what));
    assert!(panic_at("a:1:1").same(&panic_at("a:1:1")));
    assert!(!panic_at("a:1:1").same(&panic_at("a:2:1")));
    assert!(heap(5).same(&heap(9)));
    assert!(!heap(5).same(&Verdict::Violation(Violation::Steps { taken: 2, limit: 1 })));
    assert!(invariant("x").same(&invariant("x")));
    assert!(!invariant("x").same(&invariant("y")));
    assert!(!heap(5).same(&panic_at("a:1:1")));
    assert!(!Verdict::Pass.same(&Verdict::Pass));
}
