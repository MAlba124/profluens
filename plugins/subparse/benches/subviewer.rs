// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

// A std-only bench, its corpus lives on the heap.
#![allow(clippy::disallowed_methods)]

use pf_subparse::{Format, ParseContext, parse_with};
use std::time::Instant;

fn corpus() -> String {
    let mut s = String::with_capacity(32 * 1024);
    s.push_str(
        "[INFORMATION]\n[TITLE]Benchmark\n[AUTHOR]subparse-formats\n[END INFORMATION]\n[SUBTITLE]\n",
    );
    for i in 0..300u32 {
        let start = i * 4;
        let end = start + 3;
        s.push_str(&format!(
            "{:02}:{:02}:{:02}.00,{:02}:{:02}:{:02}.50\n",
            start / 3600,
            (start / 60) % 60,
            start % 60,
            end / 3600,
            (end / 60) % 60,
            end % 60,
        ));
        s.push_str(&format!("subtitle line {i} first half[br]subtitle line {i} second half\n",));
        s.push_str("and a third line for good measure\n\n");
    }
    s
}

fn main() {
    let body = corpus();
    let ctx = ParseContext::default();
    let iters = 2000;

    let cues = parse_with(Format::SubViewer, &body, &ctx).unwrap();
    let per_iter_cues = cues.len();

    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        let cues = parse_with(Format::SubViewer, &body, &ctx).unwrap();
        acc = acc.wrapping_add(cues.len());
    }
    let elapsed = start.elapsed();
    let per_iter_us = elapsed.as_secs_f64() * 1e6 / iters as f64;

    println!(
        "subviewer: {} bytes, {} cues/iter, {iters} iters, {:.3} ms total, {:.3} us/iter [acc={acc}]",
        body.len(),
        per_iter_cues,
        elapsed.as_secs_f64() * 1e3,
        per_iter_us,
    );
}
