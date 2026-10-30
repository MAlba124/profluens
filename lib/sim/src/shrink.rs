//! Delta debugging of a failing script, `ddmin` of Zeller and Hildebrandt, "Simplifying and
//! Isolating Failure-Inducing Input", IEEE TSE 28(2), 2002. The result is 1-minimal.

use crate::action::{Action, ActionKind, ActionList, Anchor, MAX_ACTIONS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShrinkError {
    InputPasses,
}

impl core::fmt::Display for ShrinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the action list to shrink does not fail")
    }
}

impl std::error::Error for ShrinkError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shrunk {
    pub tests: u32,
    pub exhausted: bool,
}

/// Fewer than 64 reductions, and at most 7 doublings from 2 chunks to 64 between two of them.
const MAX_ROUNDS: usize = MAX_ACTIONS * 8;

struct Work {
    source: ActionList,
    kept: [u8; MAX_ACTIONS],
    len: usize,
    scratch: [Action; MAX_ACTIONS],
    tests: u32,
    budget: u32,
}

enum Next {
    Split(usize),
    Minimal,
    Spent,
}

fn chunk(j: usize, chunks: usize, len: usize) -> (usize, usize) {
    (j * len / chunks, (j + 1) * len / chunks)
}

fn test(
    w: &mut Work,
    (lo, hi): (usize, usize),
    complement: bool,
    fails: &mut impl FnMut(&[Action]) -> bool,
) -> Option<bool> {
    if w.tests == w.budget {
        return None;
    }
    w.tests += 1;
    let source = w.source.as_slice();
    let mut n = 0;
    for (position, index) in w.kept[..w.len].iter().enumerate() {
        if (lo..hi).contains(&position) != complement {
            w.scratch[n] = source[*index as usize];
            n += 1;
        }
    }
    Some(fails(&w.scratch[..n]))
}

fn reduce(w: &mut Work, (lo, hi): (usize, usize), complement: bool) {
    if complement {
        w.kept.copy_within(hi..w.len, lo);
        w.len -= hi - lo;
    } else {
        w.kept.copy_within(lo..hi, 0);
        w.len = hi - lo;
    }
}

fn round(w: &mut Work, chunks: usize, fails: &mut impl FnMut(&[Action]) -> bool) -> Next {
    for j in 0..chunks {
        let range = chunk(j, chunks, w.len);
        match test(w, range, false, fails) {
            None => return Next::Spent,
            Some(false) => {}
            Some(true) => {
                reduce(w, range, false);
                return Next::Split(2);
            }
        }
    }
    // With two chunks each complement is the other chunk, already tested.
    let complements = if chunks > 2 { chunks } else { 0 };
    for j in 0..complements {
        let range = chunk(j, chunks, w.len);
        match test(w, range, true, fails) {
            None => return Next::Spent,
            Some(false) => {}
            Some(true) => {
                reduce(w, range, true);
                return Next::Split((chunks - 1).max(2));
            }
        }
    }
    if chunks < w.len { Next::Split(chunks * 2) } else { Next::Minimal }
}

/// `fails` must be deterministic. It is called at most `max_tests` times.
pub fn shrink(
    list: &mut ActionList,
    max_tests: u32,
    mut fails: impl FnMut(&[Action]) -> bool,
) -> Result<Shrunk, ShrinkError> {
    let filler = Action { anchor: Anchor::At(0), kind: ActionKind::Pause };
    let mut w = Work {
        source: *list,
        kept: core::array::from_fn(|index| index as u8),
        len: list.len(),
        scratch: [filler; MAX_ACTIONS],
        tests: 0,
        budget: max_tests,
    };
    let whole = (0, w.len);
    match test(&mut w, whole, false, &mut fails) {
        None => {
            return Ok(Shrunk { tests: 0, exhausted: true });
        }
        Some(false) => return Err(ShrinkError::InputPasses),
        Some(true) => {}
    }
    // The empty script first. When it fails, no action matters.
    let mut exhausted = false;
    match test(&mut w, (0, 0), false, &mut fails) {
        None => exhausted = true,
        Some(false) => {}
        Some(true) => w.len = 0,
    }
    let mut chunks = 2;
    for _ in 0..MAX_ROUNDS {
        if exhausted || w.len < 2 {
            break;
        }
        let chunks_now = chunks.min(w.len);
        match round(&mut w, chunks_now, &mut fails) {
            Next::Split(next) => chunks = next,
            Next::Minimal => break,
            Next::Spent => exhausted = true,
        }
    }
    list.clear();
    for index in &w.kept[..w.len] {
        // Cannot fail, the kept set is a subset of what the list held.
        let _ = list.push(w.source.as_slice()[*index as usize]);
    }
    Ok(Shrunk { tests: w.tests, exhausted })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prng::Prng;

    fn script(len: u64) -> ActionList {
        let mut list = ActionList::new();
        for to in 0..len {
            list.push(Action { anchor: Anchor::At(to), kind: ActionKind::Seek { to } }).unwrap();
        }
        list
    }

    fn ids(actions: &[Action]) -> u64 {
        actions.iter().fold(0, |mask, action| match action.kind {
            ActionKind::Seek { to } => mask | 1 << to,
            _ => mask,
        })
    }

    fn has(actions: &[Action], needed: u64) -> bool {
        ids(actions) & needed == needed
    }

    #[test]
    fn finds_the_single_culprit() {
        for culprit in [0u64, 1, 17, 62, 63] {
            let mut list = script(64);
            let done = shrink(&mut list, u32::MAX, |a| has(a, 1 << culprit)).unwrap();
            assert_eq!(ids(list.as_slice()), 1 << culprit);
            assert!(!done.exhausted);
            assert!(done.tests <= 16, "{} tests for culprit {culprit}", done.tests);
        }
    }

    #[test]
    fn keeps_every_action_a_conjunction_needs_and_their_order() {
        let needed = 1 << 3 | 1 << 20 | 1 << 21 | 1 << 47;
        let mut list = script(50);
        let done = shrink(&mut list, u32::MAX, |a| has(a, needed)).unwrap();
        assert_eq!(ids(list.as_slice()), needed);
        assert!(!done.exhausted);
        let order: [Action; 4] = [3u64, 20, 21, 47]
            .map(|to| Action { anchor: Anchor::At(to), kind: ActionKind::Seek { to } });
        assert_eq!(list.as_slice(), &order);
    }

    #[test]
    fn an_empty_failing_script_shrinks_to_nothing() {
        let mut list = script(30);
        let done = shrink(&mut list, u32::MAX, |_| true).unwrap();
        assert!(list.is_empty());
        assert_eq!(done, Shrunk { tests: 2, exhausted: false });
        let mut empty = ActionList::new();
        assert_eq!(shrink(&mut empty, u32::MAX, |_| true).map(|d| d.exhausted), Ok(false));
    }

    #[test]
    fn a_passing_script_is_refused_and_left_alone() {
        let mut list = script(9);
        assert_eq!(shrink(&mut list, u32::MAX, |_| false), Err(ShrinkError::InputPasses));
        assert_eq!(list, script(9));
    }

    #[test]
    fn nothing_removable_means_nothing_removed() {
        let mut list = script(13);
        let all = ids(list.as_slice());
        let done = shrink(&mut list, u32::MAX, |a| has(a, all)).unwrap();
        assert_eq!(list, script(13));
        assert!(!done.exhausted);
    }

    /// Seeded predicates that are not monotone in general, which is what ddmin is for.
    #[test]
    fn the_result_fails_and_is_one_minimal_for_random_predicates() {
        for seed in 0..300u64 {
            let mut rng = Prng::new(seed);
            let len = rng.range(1, MAX_ACTIONS as u64 + 1);
            let mut clauses = [0u64; 3];
            for clause in clauses.iter_mut().take(rng.range(1, 4) as usize) {
                for _ in 0..rng.range(1, 5) {
                    *clause |= 1 << rng.below(len);
                }
            }
            let fails = |a: &[Action]| clauses.iter().any(|c| *c != 0 && has(a, *c));

            let mut list = script(len);
            let done = shrink(&mut list, u32::MAX, fails).unwrap();
            assert!(!done.exhausted, "seed {seed}");
            assert!(fails(list.as_slice()), "seed {seed} lost the failure");
            let kept = list.as_slice();
            for skip in 0..kept.len() {
                let mut without = ActionList::new();
                for (i, action) in kept.iter().enumerate() {
                    if i != skip {
                        without.push(*action).unwrap();
                    }
                }
                assert!(!fails(without.as_slice()), "seed {seed} can still drop action {skip}");
            }
            // ddmin's worst case is n squared plus 3n tests.
            assert!(
                done.tests as u64 <= len * len + 3 * len + 2,
                "seed {seed}, {} tests",
                done.tests
            );
        }
    }

    #[test]
    fn shrinking_is_deterministic() {
        let needed = 1 << 5 | 1 << 33;
        let run = || {
            let mut list = script(64);
            let mut trace = 0u64;
            let done = shrink(&mut list, u32::MAX, |a| {
                trace = crate::prng::mix(trace ^ ids(a));
                has(a, needed) || has(a, 1 << 60 | 1 << 61)
            });
            (list, done, trace)
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn a_spent_budget_stops_with_a_list_that_still_fails() {
        let needed = 1 << 7 | 1 << 40 | 1 << 41;
        for budget in 0..40u32 {
            let mut list = script(64);
            let mut calls = 0;
            let done = shrink(&mut list, budget, |a| {
                calls += 1;
                has(a, needed)
            })
            .unwrap();
            assert_eq!(calls, done.tests);
            assert!(done.tests <= budget);
            assert!(has(list.as_slice(), needed), "budget {budget} lost the failure");
        }
        let mut list = script(64);
        let unbounded = shrink(&mut list, u32::MAX, |a| has(a, needed)).unwrap();
        assert!(!unbounded.exhausted);
        let mut list = script(64);
        assert!(shrink(&mut list, unbounded.tests - 1, |a| has(a, needed)).unwrap().exhausted);
        let mut list = script(64);
        assert!(!shrink(&mut list, unbounded.tests, |a| has(a, needed)).unwrap().exhausted);
    }
}
