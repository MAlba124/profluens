// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! A generative sweep of the cue-ir path, which the fuzz targets do not reach.

// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use std::{
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use pf_subparse::{
    ir::CueBuilder,
    ssastyle::{SsaDialogue, SsaStyles, dialogue_into},
};

const CASES: u64 = 20_000;

const BUDGET: Duration = Duration::from_secs(30);

const PIECES: &[&str] = &[
    "{",
    "}",
    "\\",
    "{\\",
    "\\}",
    "}{",
    "\\\\",
    "{}",
    "\\N",
    "\\n",
    "\\h",
    "an8",
    "a2",
    "pos(1,2)",
    "move(1,2,3,4)",
    "b1",
    "i1",
    "u1",
    "s1",
    "p1",
    "k50",
    "c&H00FF00&",
    "1c&HFFFFFF&",
    "fs20",
    "fn Arial",
    "fscx200",
    "r",
    "rDefault",
    "t(0,100,\\fs40)",
    "shad2",
    "bord0",
    "alpha&H80&",
    "<",
    ">",
    "</",
    "<b>",
    "</b>",
    "<i>",
    "<font",
    "<font ",
    "color=",
    "face=",
    "size=",
    "size=+",
    "size=-",
    "\"",
    "'",
    "=",
    "<3",
    "< junk>",
    "&amp;",
    "0",
    "1",
    "7",
    "9",
    "-1",
    "+1",
    "2147483647",
    "-2147483648",
    "9223372036854775807",
    "999999999999999999999999999999",
    "a",
    "word",
    " ",
    "  ",
    "\n",
    "\r\n",
    "\t",
    ",",
    ".",
    ";",
    ":",
    "\u{e9}",
    "\u{439}",
    "\u{4e00}",
    "\u{1f3ac}",
    "\u{200b}",
    "\u{feff}",
    "Hello\\",
    "a\\\u{e9}",
    "<font size=+2147483647>",
    "<font size=+9223372036854775807>",
    "<font size=-2147483648>",
    "<font size=enormous>",
    "{\\an8}x\\",
    "{\\b1}{\\i1}\\",
];

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn generated(seed: u64) -> String {
    let mut lcg = Lcg(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1));
    let pieces = 1 + (lcg.next() % 12) as usize;
    let mut out = String::new();
    for _ in 0..pieces {
        out.push_str(PIECES[(lcg.next() as usize) % PIECES.len()]);
    }
    out
}

enum Failure {
    Panicked(String),
    Wedged(String),
}

fn assert_survives(label: &str, parse: impl Fn(&str) + Send + 'static) {
    let current: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let (done, finished) = mpsc::channel::<Option<Failure>>();

    let worker_current = Arc::clone(&current);
    std::thread::spawn(move || {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut failure = None;
        for seed in 0..CASES {
            let text = generated(seed);
            *worker_current.lock().unwrap() = text.clone();
            if std::panic::catch_unwind(AssertUnwindSafe(|| parse(&text))).is_err() {
                failure = Some(Failure::Panicked(text));
                break;
            }
        }
        std::panic::set_hook(previous);
        let _ = done.send(failure);
    });

    let outcome = finished
        .recv_timeout(BUDGET)
        .unwrap_or_else(|_| Some(Failure::Wedged(current.lock().unwrap().clone())));

    match outcome {
        None => {}
        Some(Failure::Panicked(case)) => {
            panic!("{label} PANICKED on {case:?}")
        }
        Some(Failure::Wedged(case)) => {
            panic!("{label} did not finish {CASES} cases in {BUDGET:?}, stuck on {case:?}")
        }
    }
}

#[test]
fn the_ass_override_scanner_survives_generated_cue_text() {
    assert_survives("dialogue_into", |text| {
        let dialogue = SsaDialogue {
            raw_text: text.to_owned(),
            style: "Default".to_owned(),
            ..SsaDialogue::default()
        };
        dialogue_into(&dialogue.event(), &SsaStyles::default(), 0, &mut CueBuilder::new());
    });
}

#[test]
fn the_subrip_markup_scanner_survives_generated_cue_text() {
    assert_survives("CueBuilder::srt_text", |text| {
        CueBuilder::new().srt_text(text);
    });
}

#[test]
fn the_generator_reaches_the_shapes_the_sweep_is_for() {
    let cases: Vec<String> = (0..CASES).map(generated).collect();

    let any = |predicate: &dyn Fn(&str) -> bool| cases.iter().any(|case| predicate(case));

    assert!(any(&|case| case.ends_with('\\')), "no case ended in a backslash, which is the hang");
    assert!(
        any(&|case: &str| {
            case.char_indices().any(|(at, ch)| {
                ch == '\\' && case[at + 1..].chars().next().is_some_and(|next| next.len_utf8() > 1)
            })
        }),
        "no case put a multi-byte character after a backslash, which is the panic"
    );
    assert!(
        any(&|case| case.contains("size=+")),
        "no case carried a relative font size, which is the overflow"
    );
    assert!(
        any(&|case| case.contains('{') && case.contains('}')),
        "no case carried a closed override block"
    );
    assert!(
        any(&|case| case.contains('{') && !case.contains('}')),
        "no case carried an UNCLOSED override block"
    );
    assert!(any(&|case| case.contains('\n')), "no case carried a line break");
    assert!(
        cases.iter().filter(|case| case.len() > 40).count() > CASES as usize / 20,
        "the cases are too short to put much next to anything"
    );
}

#[test]
fn deeply_nested_input_does_not_exhaust_the_stack() {
    for depth in [100usize, 10_000, 100_000] {
        let ass_balanced = "{\\b1}".repeat(depth) + "x";
        let ass_unclosed = "{".repeat(depth) + "x";
        let ass_escapes = "\\".repeat(depth) + "x";
        for text in [&ass_balanced, &ass_unclosed, &ass_escapes] {
            let dialogue = SsaDialogue {
                raw_text: text.clone(),
                style: "Default".to_owned(),
                ..SsaDialogue::default()
            };
            dialogue_into(&dialogue.event(), &SsaStyles::default(), 0, &mut CueBuilder::new());
        }

        let srt_balanced = "<b>".repeat(depth) + "x" + &"</b>".repeat(depth);
        let srt_unclosed = "<font size=+1>".repeat(depth) + "x";
        for text in [&srt_balanced, &srt_unclosed] {
            CueBuilder::new().srt_text(text);
        }
    }
}
