// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

// A std-only bench, its corpus lives on the heap.
#![allow(clippy::disallowed_methods)]

use std::hint::black_box;
use std::time::Instant;

use pf_subparse::cue::ParseContext;
use pf_subparse::format::SubtitleFormat;
use pf_subparse::formats::SubRip;

const SAMPLES: &[&str] = &[
    "{n}\n{a} --> {b}\nPlain single line of dialogue.\n\n",
    "{n}\n{a} --> {b}\n<i>Italic</i> and <b>bold</b> and <u>underline</u>.\n\n",
    "{n}\n{a} --> {b}\nFirst line of two\nSecond line of two\n\n",
    "{n}\n{a} --> {b}\ngave <i>Rock & Roll</i> to the <font \"#ff0\">kids</font>\n\n",
    "{n}\n{a} --> {b}\n<b><i>Unclosed nested styling that must be balanced\n\n",
    "{n}\n{a} --> {b}\nUn éclair de café — naïve? Yes & no.\n\n",
];

fn ts(total_ms: u64) -> String {
    let ms = total_ms % 1000;
    let s = (total_ms / 1000) % 60;
    let m = (total_ms / 60_000) % 60;
    let h = total_ms / 3_600_000;
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

fn build_corpus(target_bytes: usize) -> String {
    let mut out = String::with_capacity(target_bytes + 4096);
    let mut n: u64 = 1;
    let mut t: u64 = 0; // running time in ms
    while out.len() < target_bytes {
        let tmpl = SAMPLES[(n as usize) % SAMPLES.len()];
        let a = ts(t);
        let b = ts(t + 1500);
        let cue = tmpl.replace("{n}", &n.to_string()).replace("{a}", &a).replace("{b}", &b);
        out.push_str(&cue);
        n += 1;
        t += 2000;
    }
    out
}

fn main() {
    let target_bytes = 400 * 1024; // ~400 KB
    let corpus = build_corpus(target_bytes);
    let bytes = corpus.len();
    let ctx = ParseContext::default();

    let cue_count = {
        let mut p = SubRip::default();
        p.parse(&corpus, &ctx).expect("parse").len()
    };
    assert!(cue_count > 0, "benchmark corpus produced no cues");

    let iters: u32 = 200;

    let start = Instant::now();
    let mut total_cues = 0usize;
    for _ in 0..iters {
        let mut p = SubRip::default();
        let cues = p.parse(black_box(&corpus), &ctx).expect("parse");
        total_cues += black_box(cues.len());
    }
    let elapsed = start.elapsed();

    black_box(total_cues);

    let total_ns = elapsed.as_nanos() as f64;
    let ns_per_iter = total_ns / iters as f64;
    let ns_per_cue = ns_per_iter / cue_count as f64;
    let secs = elapsed.as_secs_f64();
    let mb = (bytes as f64 * iters as f64) / (1024.0 * 1024.0);
    let mb_per_s = mb / secs;

    println!("subrip benchmark");
    println!("  corpus:      {bytes} bytes ({:.1} KB)", bytes as f64 / 1024.0);
    println!("  cues/parse:  {cue_count}");
    println!("  iterations:  {iters}");
    println!("  ns/parse:    {ns_per_iter:.0}");
    println!("  ns/cue:      {ns_per_cue:.1}");
    println!("  throughput:  {mb_per_s:.1} MB/s");
}
