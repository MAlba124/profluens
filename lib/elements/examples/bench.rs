//! Rough throughput benchmarks, page-cache warm. Run in release.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::Instant;

use profluens_elements::io::{FileSink, FileSrc};
use profluens_elements::testing::{TestSink, TestSrc};

fn gbps(bytes: u64, secs: f64) -> f64 {
    bytes as f64 / secs / 1e9
}

fn main() {
    {
        use profluens_core::ring::spsc;
        let n = 200_000_000u64;
        let (p, c) = spsc::<u64>(1024);
        let prod = std::thread::spawn(move || {
            for i in 0..n {
                while p.try_push(i).is_err() {}
            }
        });
        let t = Instant::now();
        let cons = std::thread::spawn(move || {
            let mut got = 0u64;
            while got < n {
                if c.try_pop().is_some() {
                    got += 1;
                }
            }
        });
        prod.join().unwrap();
        cons.join().unwrap();
        let dt = t.elapsed().as_secs_f64();
        println!(
            "ring SPSC (u64)          : {:7.1} M items/s   ({:.1} ns/item)",
            n as f64 / dt / 1e6,
            dt * 1e9 / n as f64
        );
    }

    {
        let n = 2u64 << 30; // 2 GiB
        let (sink, stats) = TestSink::new();
        let mut p = pf_reactor::pipeline();
        let src = p.add(TestSrc::new(n));
        let snk = p.add(sink);
        p.link(src.src(TestSrc::OUT), snk.sink(TestSink::IN)).unwrap();
        let t = Instant::now();
        p.run().unwrap();
        let dt = t.elapsed().as_secs_f64();
        assert_eq!(stats.bytes(), n);
        let bufs = p.counters(src).buffers_out;
        println!(
            "pipeline testsrc!testsink: {:7.2} GB/s      ({:.1} ns/buffer, {} buffers)",
            gbps(n, dt),
            dt * 1e9 / bufs as f64,
            bufs
        );
    }

    let n: u64 = 1 << 30; // 1 GiB
    let inp = std::env::temp_dir().join("pf_bench_in.bin");
    let outp = std::env::temp_dir().join("pf_bench_out.bin");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&inp).unwrap();
        let chunk = vec![0xABu8; 1 << 20];
        let mut w = 0u64;
        while w < n {
            let take = ((n - w) as usize).min(chunk.len());
            f.write_all(&chunk[..take]).unwrap();
            w += take as u64;
        }
        f.sync_all().unwrap();
    }

    {
        let mut p = pf_reactor::pipeline();
        let s = p.add(FileSrc::new(&inp));
        let k = p.add(FileSink::new(&outp));
        p.link(s.src(FileSrc::OUT), k.sink(FileSink::IN)).unwrap();
        let t = Instant::now();
        p.run().unwrap();
        println!("file copy (OS backend)   : {:7.2} GB/s", gbps(n, t.elapsed().as_secs_f64()));
    }

    {
        let t = Instant::now();
        std::process::Command::new("cp").arg(&inp).arg(&outp).status().unwrap();
        println!("file copy (cp reference) : {:7.2} GB/s", gbps(n, t.elapsed().as_secs_f64()));
    }

    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
}
