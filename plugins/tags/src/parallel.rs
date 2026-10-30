//! Fan a path list across N threads sharing one `Pool` and one atomic cursor.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use profluens_core::io::ReactorFactory;

use crate::scanner::default_pool;
use crate::{ScanConfig, ScanOutcome, Scanner};

/// Scan `paths` across `threads` workers, calling `emit` from whichever finished the file.
pub fn scan_parallel(
    paths: Vec<PathBuf>,
    threads: usize,
    cfg: ScanConfig,
    emit: impl Fn(ScanOutcome<'_>) + Sync,
) {
    scan_parallel_with(paths, threads, cfg, None, emit);
}

/// [`scan_parallel`] with an explicit reactor factory. `None` uses the default backend.
pub fn scan_parallel_with(
    paths: Vec<PathBuf>,
    threads: usize,
    cfg: ScanConfig,
    factory: Option<ReactorFactory>,
    emit: impl Fn(ScanOutcome<'_>) + Sync,
) {
    let threads = threads.max(1);
    let pool = default_pool(cfg, threads);
    let cursor = AtomicUsize::new(0);
    let paths = &paths[..];
    let emit = &emit;
    let factory = factory.as_ref();

    std::thread::scope(|s| {
        for _ in 0..threads {
            let pool = pool.clone();
            let cursor = &cursor;
            s.spawn(move || {
                let mut scanner = match factory {
                    Some(f) => Scanner::with_reactor(cfg, pool, f),
                    None => Scanner::with_pool(cfg, pool),
                };
                scanner.scan(Cursor { paths, next: cursor }, emit);
            });
        }
    });
}

/// Hands out borrowed paths so no path is cloned per file.
struct Cursor<'a> {
    paths: &'a [PathBuf],
    next: &'a AtomicUsize,
}

impl<'a> Iterator for Cursor<'a> {
    type Item = &'a Path;

    fn next(&mut self) -> Option<&'a Path> {
        // Relaxed suffices. `fetch_add` alone keeps indices unique and nothing is published through
        // it.
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        self.paths.get(i).map(PathBuf::as_path)
    }
}
