// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

// A std-only bench, its corpus lives on the heap.
#![allow(clippy::disallowed_methods)]

use std::time::Instant;

use pf_subparse::formats::MicroDvd;
use pf_subparse::{ParseContext, SubtitleFormat};

fn make_corpus(n: usize) -> String {
    let mut s = String::with_capacity(n * 64);
    s.push_str("{1}{1}25.000\n");
    for i in 0..n {
        let a = 100 + i * 100;
        let b = a + 80;
        match i % 4 {
            0 => s.push_str(&format!("{{{a}}}{{{b}}}- Plain line {i}.\n")),
            1 => s.push_str(&format!("{{{a}}}{{{b}}}{{y:i}}/Italic {i}/|Second half\n")),
            2 => s.push_str(&format!("{{{a}}}{{{b}}}{{s:24}}Big & bold, isn't it?\n")),
            _ => s.push_str(&format!("{{{a}}}{{{b}}}<tag> line {i} | more\n")),
        }
    }
    s
}

fn bench<F: FnMut() -> usize>(label: &str, iters: u32, units: usize, mut f: F) {
    let mut sink = 0usize;
    for _ in 0..(iters / 10).max(1) {
        sink = sink.wrapping_add(f());
    }
    let t = Instant::now();
    for _ in 0..iters {
        sink = sink.wrapping_add(f());
    }
    let elapsed = t.elapsed().as_nanos() as f64;
    let per_call = elapsed / iters as f64;
    let per_unit = per_call / units as f64;
    println!(
        "{label:<28} {per_call:>10.0} ns/op  {per_unit:>8.1} ns/unit  (iters={iters}, sink={})",
        sink & 0xff
    );
}

fn main() {
    let ctx = ParseContext::default();
    let n = 500usize;
    let corpus = make_corpus(n);

    bench("microdvd parse (full file)", 3_000, n, || {
        let mut p = MicroDvd::default();
        p.parse(&corpus, &ctx).map(|c| c.len()).unwrap_or(0)
    });
}
