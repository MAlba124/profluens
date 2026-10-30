// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

// A std-only bench, its corpus lives on the heap.
#![allow(clippy::disallowed_methods)]

use std::time::Instant;

use pf_subparse::formats::ssa::{self, Ssa};
use pf_subparse::{ParseContext, SubtitleFormat};

fn make_corpus(n: usize) -> String {
    let mut s = String::with_capacity(n * 96);
    s.push_str("[Script Info]\nTitle: bench\n\n[Events]\n");
    s.push_str("Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
    for i in 0..n {
        let sec = i as u64;
        let (h, m, r) = (sec / 3600, (sec / 60) % 60, sec % 60);
        s.push_str(&format!(
            "Dialogue: 0,{h}:{m:02}:{r:02}.00,{h}:{m:02}:{r:02}.80,Default,,0,0,0,,\
{{\\i1}}Line {i}{{\\i0}}: <tag> & \"stuff\",\\Nsecond\\hhalf\n"
        ));
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
    let n_lines = 500;
    let corpus = make_corpus(n_lines);

    bench("ssa parse (full file)", 3_000, n_lines, || {
        let mut p = Ssa::default();
        p.parse(&corpus, &ctx).map(|c| c.len()).unwrap_or(0)
    });

    let text = "{\\i1}Hello {\\b1}world{\\b0}{\\i0}: <i> & \"q\",\\Nsecond line\\hhalf";
    bench("ssa strip_to_pango_markup", 200_000, 1, || ssa::strip_to_pango_markup(text).len());
}
