// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

// A std-only bench, its corpus lives on the heap.
#![allow(clippy::disallowed_methods)]

use std::hint::black_box;
use std::time::Instant;

use pf_subparse::cue::ParseContext;
use pf_subparse::format::SubtitleFormat;
use pf_subparse::formats::Sami;

const BLOCKS: &[&str] = &[
    "    <SYNC Start={t}>\n        <P Class=CC>\n            Plain single line of dialogue.<br>\n",
    "    <SYNC Start={t}>\n        <P Class=CC>\n            <i>Italic</i> and <font color=red>red</font> and normal.<br>\n",
    "    <SYNC Start={t}>\n        <P Class=CC>\n            First line of two<br>\n            Second line of two<br>\n",
    "    <SYNC Start={t}>\n        <P Class=CC>\n            Entities: &nbsp; &amp; &lt;tag&gt; &copy; &#177; &#x20ac;<br>\n",
    "    <SYNC Start={t}>\n        <P Class=CC>\n            <font color=aqua face=Arial>Coloured &amp; faced</font> text.<br>\n",
    "    <SYNC Start={t}>\n        <P Class=CC>\n            <ruby>base<rt>anno</rt></ruby> and a bare &amp; ampersand.<br>\n",
    "    <SYNC Start={t}>\n        <P Class=CC>\n            Malformed <font color=notacolor>bits</i> and <unclosed<br>\n",
];

const HEAD: &str = "<SAMI>\n\
<HEAD>\n\
    <TITLE>Benchmark</TITLE>\n\
    <STYLE TYPE=\"text/css\">\n\
    <!--\n\
        P {margin-left:8pt; margin-right:8pt; text-align:center; font-size:12pt; color:black;}\n\
        .CC {Name:English; lang:en-US; SAMIType:CC;}\n\
    -->\n\
    </Style>\n\
</HEAD>\n\
<BODY>\n";

fn build_corpus(target_bytes: usize) -> String {
    let mut out = String::with_capacity(target_bytes + 4096);
    out.push_str(HEAD);
    let mut n: u64 = 0;
    let mut t: u64 = 0; // running time in ms
    while out.len() < target_bytes {
        let tmpl = BLOCKS[(n as usize) % BLOCKS.len()];
        out.push_str(&tmpl.replace("{t}", &t.to_string()));
        n += 1;
        t += 2000;
    }
    out.push_str("</BODY>\n</SAMI>\n");
    out
}

fn main() {
    let target_bytes = 400 * 1024; // ~400 KB
    let corpus = build_corpus(target_bytes);
    let bytes = corpus.len();
    let ctx = ParseContext::default();

    let cue_count = {
        let mut p = Sami::default();
        p.parse(&corpus, &ctx).expect("parse").len()
    };
    assert!(cue_count > 0, "benchmark corpus produced no cues");

    let iters: u32 = 200;

    let start = Instant::now();
    let mut total_cues = 0usize;
    for _ in 0..iters {
        let mut p = Sami::default();
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

    println!("sami benchmark");
    println!("  corpus:      {bytes} bytes ({:.1} KB)", bytes as f64 / 1024.0);
    println!("  cues/parse:  {cue_count}");
    println!("  iterations:  {iters}");
    println!("  ns/parse:    {ns_per_iter:.0}");
    println!("  ns/cue:      {ns_per_cue:.1}");
    println!("  throughput:  {mb_per_s:.1} MB/s");
}
