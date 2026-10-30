use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Pf,
    Named,
    Memfd,
    Unnamed,
    Heap,
    Stack,
    Lib,
    File,
    Kernel,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Pf => "pf",
            Kind::Named => "named anon",
            Kind::Memfd => "memfd",
            Kind::Unnamed => "unnamed anon",
            Kind::Heap => "heap",
            Kind::Stack => "stack",
            Kind::Lib => "libraries",
            Kind::File => "files",
            Kind::Kernel => "kernel",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row<'a> {
    pub kind: Kind,
    pub name: &'a str,
    pub rss_kib: u64,
    pub maps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unnamed<'a> {
    pub range: &'a str,
    pub size_kib: u64,
    pub rss_kib: u64,
}

#[derive(Debug, Default)]
pub struct Report<'a> {
    pub rows: Vec<Row<'a>>,
    pub unnamed: Vec<Unnamed<'a>>,
}

pub const UNNAMED_KEPT: usize = 8;

pub fn classify(path: &str) -> (Kind, &str) {
    let path = path.trim();
    if path.is_empty() {
        return (Kind::Unnamed, "[anon]");
    }
    if path == "[heap]" {
        return (Kind::Heap, path);
    }
    if path == "[stack]" || path.starts_with("[stack:") {
        return (Kind::Stack, "[stack]");
    }
    let named = path.strip_prefix("[anon:").or_else(|| path.strip_prefix("[anon_shmem:"));
    if let Some(name) = named.and_then(|n| n.strip_suffix(']')) {
        return (if name.starts_with("pf:") { Kind::Pf } else { Kind::Named }, name);
    }
    if path.starts_with('[') {
        return (Kind::Kernel, path);
    }
    if let Some(name) = path.strip_prefix("/memfd:") {
        let name = name.strip_suffix(" (deleted)").unwrap_or(name);
        return (if name.starts_with("pf:") { Kind::Pf } else { Kind::Memfd }, name);
    }
    let file = path.strip_suffix(" (deleted)").unwrap_or(path);
    let base = file.rsplit('/').next().unwrap_or(file);
    match base.contains(".so") {
        true => (Kind::Lib, base),
        false => (Kind::File, file),
    }
}

fn header(line: &str) -> Option<(&str, &str)> {
    let range = line.split_ascii_whitespace().next()?;
    let (lo, hi) = range.split_once('-')?;
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex(lo) || !hex(hi) {
        return None;
    }
    let mut rest = line;
    for _ in 0..5 {
        rest = rest.trim_start();
        rest = &rest[rest.find(' ').unwrap_or(rest.len())..];
    }
    Some((range, rest.trim()))
}

fn kib(line: &str, key: &str) -> Option<u64> {
    line.strip_prefix(key)?.trim().strip_suffix("kB")?.trim().parse().ok()
}

// A tool's one pass, the rows are its output.
#[allow(clippy::disallowed_methods)]
pub fn parse(smaps: &str) -> Report<'_> {
    let mut report = Report { rows: Vec::new(), unnamed: Vec::new() };
    let mut at: Option<(Kind, &str, &str)> = None;
    let mut size = 0;
    for line in smaps.lines() {
        if let Some((range, path)) = header(line) {
            let (kind, name) = classify(path);
            at = Some((kind, name, range));
            continue;
        }
        if let Some(n) = kib(line, "Size:") {
            size = n;
        }
        let (Some(rss), Some((kind, name, range))) = (kib(line, "Rss:"), at) else {
            continue;
        };
        match report.rows.iter_mut().find(|r| r.kind == kind && r.name == name) {
            Some(row) => (row.rss_kib, row.maps) = (row.rss_kib + rss, row.maps + 1),
            None => report.rows.push(Row { kind, name, rss_kib: rss, maps: 1 }),
        }
        if kind == Kind::Unnamed {
            report.unnamed.push(Unnamed { range, size_kib: size, rss_kib: rss });
            report.unnamed.sort_by_key(|u| std::cmp::Reverse(u.rss_kib));
            report.unnamed.truncate(UNNAMED_KEPT);
        }
    }
    report.rows.sort_by(|a, b| {
        a.kind.cmp(&b.kind).then(b.rss_kib.cmp(&a.rss_kib)).then(a.name.cmp(b.name))
    });
    report
}

pub fn prefix(name: &str) -> &str {
    match name.match_indices(':').nth(1) {
        Some((at, _)) => &name[..at],
        None => name,
    }
}

fn mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

const NAME_COLUMN: usize = 44;

fn line(out: &mut String, kind: &str, name: &str, kib: u64, maps: u32) {
    let cut = name.len().saturating_sub(NAME_COLUMN);
    let name = name.get(cut..).unwrap_or(name);
    let _ = writeln!(out, "{kind:<13} {name:<NAME_COLUMN$} {:>9.1} {maps:>5}", mib(kib));
}

// A tool's output text.
#[allow(clippy::disallowed_methods)]
pub fn table(report: &Report<'_>, top: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{:<13} {:<NAME_COLUMN$} {:>9} {:>5}", "kind", "name", "rss MiB", "maps");
    let mut kinds = report.rows.iter().map(|r| r.kind).collect::<Vec<_>>();
    kinds.dedup();
    for kind in kinds {
        let rows = report.rows.iter().filter(|r| r.kind == kind);
        let (sum, maps) = rows.clone().fold((0, 0), |(s, m), r| (s + r.rss_kib, m + r.maps));
        line(&mut out, kind.label(), "(total)", sum, maps);
        if kind == Kind::Pf {
            let mut prefixes = rows.clone().map(|r| prefix(r.name)).collect::<Vec<_>>();
            prefixes.sort_unstable();
            prefixes.dedup();
            for p in prefixes {
                let of = rows.clone().filter(|r| prefix(r.name) == p);
                let (sum, maps) = of.fold((0, 0), |(s, m), r| (s + r.rss_kib, m + r.maps));
                line(&mut out, "", &format!("{p}:*"), sum, maps);
            }
        }
        for r in rows.take(if kind == Kind::Unnamed { 0 } else { top }) {
            line(&mut out, "", r.name, r.rss_kib, r.maps);
        }
    }
    for u in report.unnamed.iter().filter(|u| u.rss_kib >= 1024) {
        line(
            &mut out,
            "unnamed",
            &format!("{} of {:.1} MiB", u.range, mib(u.size_kib)),
            u.rss_kib,
            1,
        );
    }
    let (total, maps) = report.rows.iter().fold((0, 0), |(s, m), r| (s + r.rss_kib, m + r.maps));
    line(&mut out, "total", "", total, maps);
    out
}

#[cfg(test)]
mod tests;
