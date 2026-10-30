// A bench's own threads and rings, not an element.
#![allow(clippy::disallowed_methods)]

use std::hint::black_box;
use std::time::Instant;

use profluens_core::memory::{Memory, Pool, PoolConfig};
use profluens_core::ring::spsc;

const ITEMS_ST: usize = 5_000_000;
const ITEMS_X: usize = 2_000_000;
const TRIALS: usize = 7;
const WARMUP: usize = 2;
const RING: usize = 256;

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn trials(mut one: impl FnMut() -> f64) -> f64 {
    let mut samples = Vec::with_capacity(TRIALS);
    for trial in 0..WARMUP + TRIALS {
        let ns = one();
        if trial >= WARMUP {
            samples.push(ns);
        }
    }
    median(samples)
}

fn sized_pool() -> Pool {
    Pool::with_config(PoolConfig { bytes: 1 << 20, ..PoolConfig::heap(64, 1024) })
}

fn take(pool: &Pool, sized: bool) -> Option<Memory> {
    if sized {
        pool.try_acquire_sized(200).expect("within the top class")
    } else {
        pool.try_acquire()
    }
}

fn same_thread(sized: bool) -> f64 {
    let pool = if sized { sized_pool() } else { Pool::bounded(64, 1024) };
    trials(|| {
        let start = Instant::now();
        for _ in 0..ITEMS_ST {
            black_box(take(&pool, sized).expect("a free slot"));
        }
        start.elapsed().as_nanos() as f64 / ITEMS_ST as f64
    })
}

fn same_thread_held(depth: usize) -> f64 {
    let pool = Pool::bounded(64, 1024);
    let mut held: Vec<Memory> = Vec::with_capacity(depth);
    let rounds = ITEMS_ST / depth;
    trials(|| {
        let start = Instant::now();
        for _ in 0..rounds {
            held.extend((0..depth).map(|_| pool.try_acquire().expect("a free slot")));
            held.clear();
        }
        start.elapsed().as_nanos() as f64 / (rounds * depth) as f64
    })
}

fn ring_only() -> f64 {
    trials(|| {
        let (p, c) = spsc::<usize>(RING);
        let start = Instant::now();
        let consumer = std::thread::spawn(move || {
            (0..ITEMS_X).map(|_| c.pop().expect("producer alive")).sum::<usize>()
        });
        (0..ITEMS_X).for_each(|i| p.push(i).expect("consumer alive"));
        black_box(consumer.join().unwrap());
        start.elapsed().as_nanos() as f64 / ITEMS_X as f64
    })
}

fn cross_thread(sized: bool) -> f64 {
    trials(|| {
        let pool = if sized { sized_pool() } else { Pool::bounded(64, 1024) };
        let (p, c) = spsc::<Memory>(RING);
        let start = Instant::now();
        let consumer = std::thread::spawn(move || {
            for _ in 0..ITEMS_X {
                drop(c.pop().expect("producer alive"));
            }
        });
        for _ in 0..ITEMS_X {
            let m = loop {
                match take(&pool, sized) {
                    Some(m) => break m,
                    None => std::hint::spin_loop(),
                }
            };
            if p.push(m).is_err() {
                panic!("consumer gone");
            }
        }
        consumer.join().unwrap();
        start.elapsed().as_nanos() as f64 / ITEMS_X as f64
    })
}

/// Samples of log-uniform sizes from 1 KiB to 4 MiB a 64 MiB budget holds, and their payload share.
fn budget_fill() -> (usize, f64) {
    let pool = Pool::with_config(PoolConfig { bytes: 64 << 20, ..PoolConfig::heap(64, 1 << 20) });
    let mut held: Vec<Memory> = Vec::with_capacity(1 << 16);
    let (mut seed, mut payload) = (0x9E37_79B9_7F4A_7C15u64, 0u64);
    loop {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let n = (1024.0 * 4096f64.powf((seed >> 11) as f64 / (1u64 << 53) as f64)) as usize;
        match pool.try_acquire_sized(n).expect("within the top class") {
            Some(m) => {
                payload += n as u64;
                held.push(m);
            }
            None => break,
        }
    }
    (held.len(), payload as f64 / pool.stats().bytes_out as f64)
}

fn main() {
    use std::io::Write;
    let only = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let wanted = |name: &str| only.as_deref().is_none_or(|o| name.contains(o));
    println!("pool microbench, {TRIALS} trials (median), {WARMUP} warmup, ring {RING}\n");
    let row = |name: &str, ns: f64| {
        println!("{name:<40}: {ns:>7.2} ns/buffer");
        std::io::stdout().flush().ok();
    };
    if wanted("same_slot") {
        row("same_slot   acquire+drop", same_thread(false));
    }
    if wanted("same_sized") {
        row("same_sized  acquire_sized(200)+drop", same_thread(true));
    }
    if wanted("same_held") {
        row("same_held   16 out, then dropped", same_thread_held(16));
    }
    if wanted("budget_fill") {
        let (held, share) = budget_fill();
        println!(
            "{:<40}: {held:>7} samples, payload {:.1}% of class bytes",
            "budget_fill 64 MiB, 1 KiB..4 MiB",
            share * 100.0
        );
        std::io::stdout().flush().ok();
    }
    if wanted("ring_only") {
        row("ring_only   usize push->pop 2-thread", ring_only());
    }
    if wanted("cross_slot") {
        row("cross_slot  acquire->ring->drop", cross_thread(false));
    }
    if wanted("cross_sized") {
        row("cross_sized acquire_sized->ring->drop", cross_thread(true));
    }
}
