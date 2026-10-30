// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

// A std-only bench, its corpus lives on the heap.
#![allow(clippy::disallowed_methods)]

use pf_subparse::{Format, ParseContext, parse_with};
use std::time::Instant;

fn corpus() -> String {
    let mut s = String::with_capacity(24 * 1024);
    for i in 0..300u32 {
        let start = i * 3;
        let end = start + 2;
        s.push_str(&format!(
            "[{:02}:{:02}:{:02}]subtitle line {i}[br]second half of line {i}\n",
            start / 3600,
            (start / 60) % 60,
            start % 60,
        ));
        s.push_str(&format!("[{:02}:{:02}:{:02}]\n", end / 3600, (end / 60) % 60, end % 60,));
    }
    s
}

fn main() {
    let body = corpus();
    let ctx = ParseContext::default();
    let iters = 2000;

    let cues = parse_with(Format::Dks, &body, &ctx).unwrap();
    let per_iter_cues = cues.len();

    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        let cues = parse_with(Format::Dks, &body, &ctx).unwrap();
        acc = acc.wrapping_add(cues.len());
    }
    let elapsed = start.elapsed();
    let per_iter_us = elapsed.as_secs_f64() * 1e6 / iters as f64;

    println!(
        "dks: {} bytes, {} cues/iter, {iters} iters, {:.3} ms total, {:.3} us/iter [acc={acc}]",
        body.len(),
        per_iter_cues,
        elapsed.as_secs_f64() * 1e3,
        per_iter_us,
    );
}
