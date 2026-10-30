// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Regression tests for the cost of the SubRip/WebVTT markup pipeline.

// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use pf_subparse::formats::{SubRip, WebVtt};
use pf_subparse::{ParseContext, SubtitleFormat};

fn one_huge_cue(timing: &str, unit: &str, tail: &str) -> String {
    const TARGET: usize = 1024 * 1024;

    let mut body = String::with_capacity(TARGET + timing.len() + 64);
    body.push_str(timing);
    while body.len() < TARGET {
        body.push_str(unit);
    }
    body.push_str(tail);
    body
}

fn within(budget: Duration, what: &str, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        f();
        let _ = tx.send(started.elapsed());
    });

    match rx.recv_timeout(budget) {
        Ok(elapsed) => eprintln!("{what} finished in {elapsed:?} (budget {budget:?})"),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "{what} did not finish within {budget:?}; the tag scan is not linear \
             in the length of a cue"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{what} panicked; see the worker's assertion above")
        }
    }
}

const BUDGET: Duration = Duration::from_secs(30);

#[test]
fn subrip_one_huge_cue_with_a_distant_closing_marker_is_linear() {
    let body = one_huge_cue("1\n00:00:01,000 --> 00:00:02,000\n", "<5", ">");

    within(BUDGET, "1 MB SubRip cue of \"<5\"", move || {
        let cues = SubRip::default()
            .parse(&body, &ParseContext::default())
            .expect("subrip parse is infallible");
        assert_eq!(cues.len(), 1, "the body is one unterminated cue");
        assert!(
            cues[0].text.starts_with("&lt;5&lt;5"),
            "text starts {:?}",
            &cues[0].text[..16.min(cues[0].text.len())]
        );
        assert!(cues[0].text.ends_with("&gt;"), "the lone '>' must survive");
    });
}

#[test]
fn subrip_one_huge_cue_without_any_closing_marker_is_linear() {
    let body = one_huge_cue("1\n00:00:01,000 --> 00:00:02,000\n", "<font ", "");

    within(BUDGET, "1 MB SubRip cue of \"<font \"", move || {
        let cues = SubRip::default()
            .parse(&body, &ParseContext::default())
            .expect("subrip parse is infallible");
        assert_eq!(cues.len(), 1, "the body is one unterminated cue");
        assert!(cues[0].text.starts_with("&lt;font &lt;font "));
    });
}

#[test]
fn webvtt_one_huge_cue_with_a_distant_closing_marker_is_linear() {
    let body = one_huge_cue("WEBVTT FILE\n\n00:00:01.000 --> 00:00:02.000\n", "<5", ">");

    within(BUDGET, "1 MB WebVTT cue of \"<5\"", move || {
        let cues = WebVtt::default()
            .parse(&body, &ParseContext::default())
            .expect("webvtt parse is infallible");
        assert_eq!(cues.len(), 1, "the body is one unterminated cue");
        assert!(cues[0].text.starts_with("&lt;5&lt;5"));
        assert!(cues[0].text.ends_with("&gt;"), "the lone '>' must survive");
    });
}

#[test]
fn webvtt_one_huge_cue_without_any_closing_marker_is_linear() {
    let body = one_huge_cue("WEBVTT FILE\n\n00:00:01.000 --> 00:00:02.000\n", "<font ", "");

    within(BUDGET, "1 MB WebVTT cue of \"<font \"", move || {
        let cues = WebVtt::default()
            .parse(&body, &ParseContext::default())
            .expect("webvtt parse is infallible");
        assert_eq!(cues.len(), 1, "the body is one unterminated cue");
        assert!(cues[0].text.starts_with("&lt;font &lt;font "));
    });
}
