// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

// A std-only bench, its corpus lives on the heap.
#![allow(clippy::disallowed_methods)]

use pf_subparse::{Format, ParseContext, parse_with};
use std::time::Instant;

fn corpus() -> String {
    let mut s = String::with_capacity(16 * 1024);
    s.push_str("[ti:Benchmark]\n[ar:subparse-formats]\n");
    for i in 0..400u32 {
        let m = i / 60;
        let sec = i % 60;
        let cs = (i * 7) % 100;
        s.push_str(&format!(
            "[{:02}:{:02}.{:02}]lyric line number {i} rolling along\n",
            m, sec, cs
        ));
    }
    s
}

fn main() {
    let body = corpus();
    let ctx = ParseContext::default();
    let iters = 2000;

    let cues = parse_with(Format::Lrc, &body, &ctx).unwrap();
    let per_iter_cues = cues.len();

    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        let cues = parse_with(Format::Lrc, &body, &ctx).unwrap();
        acc = acc.wrapping_add(cues.len());
    }
    let elapsed = start.elapsed();
    let per_iter_us = elapsed.as_secs_f64() * 1e6 / iters as f64;

    println!(
        "lrc: {} bytes, {} cues/iter, {iters} iters, {:.3} ms total, {:.3} us/iter [acc={acc}]",
        body.len(),
        per_iter_cues,
        elapsed.as_secs_f64() * 1e3,
        per_iter_us,
    );
}
