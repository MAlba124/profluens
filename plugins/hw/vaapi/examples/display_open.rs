//! How long a VA display takes to open (the DRM node, vaGetDisplayDRM, vaInitialize), the first
//! in the process and after. `cargo run --example display_open -- [/dev/dri/renderD128] [n]`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
use pf_vaapi::va::Display;
use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let node = args.get(1).map_or("/dev/dri/renderD128", |s| s.as_str());
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut kept = Vec::new();
    for i in 0..n {
        let t = Instant::now();
        let d = Display::open(Path::new(node)).expect("open");
        let open = t.elapsed();
        let t = Instant::now();
        drop(d);
        println!(
            "open {i}: {:.2} ms, close {:.2} ms",
            open.as_secs_f64() * 1e3,
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    for i in 0..3 {
        let t = Instant::now();
        kept.push(Display::open(Path::new(node)).expect("open"));
        println!(
            "open {i} with {} held: {:.2} ms",
            kept.len() - 1,
            t.elapsed().as_secs_f64() * 1e3
        );
    }
}
