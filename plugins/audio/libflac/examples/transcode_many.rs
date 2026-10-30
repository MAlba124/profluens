//! Re-encode N FLAC files concurrently in one pipeline, one chain per file.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pf_libflac::{FlacDec, FlacEnc, SampleFormat};
use profluens_elements::io::{FileSink, FileSrc};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(out_dir) = args.next().map(PathBuf::from) else {
        eprintln!("usage: transcode_many OUT_DIR IN1.flac [IN2.flac …]");
        std::process::exit(2);
    };
    let inputs: Vec<PathBuf> = args.map(PathBuf::from).collect();
    if inputs.is_empty() {
        eprintln!("usage: transcode_many OUT_DIR IN1.flac [IN2.flac …]");
        std::process::exit(2);
    }
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    // flacenc parameters are placeholders, each encoder learns the format from its decoder.
    let mut p = pf_reactor::pipeline();
    let n = inputs.len() as u32;
    p.set_pool(128 * 1024, n * 32 + 32);

    let mut sinks = Vec::new();
    for input in &inputs {
        let name = input.file_name().unwrap_or_default();
        let out = out_dir.join(Path::new(name));
        let src = p.add(FileSrc::new(input));
        let dec = p.add(FlacDec::new());
        let enc = p.add(FlacEnc::new(44_100, 2, SampleFormat::S16));
        let sink = p.add(FileSink::new(&out));
        p.link(src.src(FileSrc::OUT), dec.sink(FlacDec::IN)).expect("link src!dec");
        p.link(dec.src(FlacDec::OUT), enc.sink(FlacEnc::IN)).expect("link dec!enc");
        p.link(enc.src(FlacEnc::OUT), sink.sink(FileSink::IN)).expect("link enc!sink");
        sinks.push(sink);
    }

    println!(
        "transcoding {} file(s) in one pipeline ({} elements, ~{} threads)…",
        inputs.len(),
        inputs.len() * 4,
        inputs.len() * 2,
    );

    let tap = p.tap_handle();
    let stop = Arc::new(AtomicBool::new(false));
    let observer = {
        let tap = tap.clone();
        let sinks = sinks.clone();
        let names: Vec<String> = inputs
            .iter()
            .map(|i| i.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                // Not `sleep`, so the unpark below ends the wait early.
                std::thread::park_timeout(Duration::from_millis(150));
                let mut line = String::from("\r");
                for (sink, name) in sinks.iter().zip(&names) {
                    let bytes = tap.snapshot(*sink).map_or(0, |s| s.bytes_in);
                    line.push_str(&format!(
                        "{name}: {:>8.2} MiB  ",
                        bytes as f64 / (1 << 20) as f64
                    ));
                }
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(line.as_bytes());
                let _ = err.flush();
            }
        })
    };

    let t0 = Instant::now();
    let result = p.run();
    let wall = t0.elapsed();
    stop.store(true, Ordering::Release);
    observer.thread().unpark(); // `stop` is published before this, so the wake cannot be lost
    let _ = observer.join();
    eprintln!();

    match result {
        Ok(()) => {
            let total: u64 =
                sinks.iter().filter_map(|s| tap.snapshot(*s)).map(|s| s.bytes_in).sum();
            println!(
                "done: {} file(s), {:.2} MiB written in {:.2}s ({:.1} MiB/s aggregate)",
                inputs.len(),
                total as f64 / (1 << 20) as f64,
                wall.as_secs_f64(),
                total as f64 / (1 << 20) as f64 / wall.as_secs_f64().max(1e-9),
            );
        }
        Err(e) => {
            eprintln!("run error: {e:?}");
            std::process::exit(1);
        }
    }
}
