// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use pf_avi::riff::{self, MoviWalker, StreamKind};
use std::io::Read;

const HEADER_PREFIX: usize = 1024 * 1024;

#[allow(clippy::disallowed_methods)] // probe tooling reads the file directly
fn read_prefix(path: &str, n: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    let got = f.read(&mut buf)?;
    buf.truncate(got);
    Ok(buf)
}

#[allow(clippy::disallowed_methods)] // probe tooling reads the file directly
fn read_at(path: &str, off: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; len];
    let got = f.read_at(&mut buf, off)?;
    buf.truncate(got);
    Ok(buf)
}

#[allow(clippy::disallowed_methods)] // probe tooling reads the file directly
fn walk_movi(
    path: &str,
    movi_data_start: u64,
    nstreams: usize,
) -> std::io::Result<Vec<(u64, u64)>> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(movi_data_start))?;
    let mut walker = MoviWalker::new();
    let mut counts = vec![(0u64, 0u64); nstreams + 8]; // (chunks, bytes), slack for stray ids
    let mut block = vec![0u8; 4 * 1024 * 1024];
    loop {
        let got = f.read(&mut block)?;
        if got == 0 {
            break;
        }
        walker.push(&block[..got]);
        while let Some(chunk) = walker.next_chunk() {
            if chunk.stream_index < counts.len() {
                counts[chunk.stream_index].0 += 1;
                counts[chunk.stream_index].1 += chunk.data.len() as u64;
            }
        }
    }
    counts.truncate(nstreams);
    Ok(counts)
}

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: probe FILE.avi");
            std::process::exit(2);
        }
    };
    let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let header = read_prefix(&path, HEADER_PREFIX).expect("read header");
    let (avi, movi) = match riff::probe_header(&header) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("not a parseable AVI: {e:?}");
            std::process::exit(1);
        }
    };
    let movi = movi.expect("movi located in the 1 MiB prefix");

    println!("file: {path}  ({file_len} bytes)");
    println!(
        "movi data starts at byte {}  |  OpenDML header present: {}",
        movi.movi_data_start, avi.has_odml
    );
    println!("{} stream(s):", avi.streams.len());
    for s in &avi.streams {
        let family = pf_avi::codec::family_for(s);
        match s.kind {
            StreamKind::Video => {
                let fps = if s.scale != 0 { s.rate as f64 / s.scale as f64 } else { 0.0 };
                println!(
                    "  [{}] VIDEO family={family} fourcc={} {}x{}  {:.3} fps  declared frames={}",
                    s.index,
                    String::from_utf8_lossy(&s.handler),
                    s.width,
                    s.height,
                    fps,
                    s.length,
                );
            }
            StreamKind::Audio => {
                println!(
                    "  [{}] AUDIO family={family} tag=0x{:04x} {} Hz  {} ch  {} bits  sample_size={}",
                    s.index,
                    s.format_tag,
                    s.samples_per_sec,
                    s.channels,
                    s.bits_per_sample,
                    s.sample_size,
                );
            }
            StreamKind::Other => {
                println!(
                    "  [{}] OTHER family={family} handler={}",
                    s.index,
                    String::from_utf8_lossy(&s.handler)
                );
            }
        }
    }
    if let Some(ns) = avi.duration_ns() {
        println!("duration (from header): {:.3} s", ns as f64 / 1e9);
    }

    println!("\nwalking movi (this reads the whole file)…");
    let counts = walk_movi(&path, movi.movi_data_start, avi.streams.len()).expect("walk movi");
    for s in &avi.streams {
        let (chunks, bytes) = counts.get(s.index).copied().unwrap_or((0, 0));
        let kind = match s.kind {
            StreamKind::Video => "video",
            StreamKind::Audio => "audio",
            StreamKind::Other => "other",
        };
        println!("  stream {} ({kind}): {chunks} chunks, {bytes} bytes", s.index);
    }

    if let Some((idx1_off, idx1_size)) = find_idx1(&path, file_len) {
        let payload = read_at(&path, idx1_off + 8, idx1_size as usize).expect("read idx1");
        if let Some(si) = pf_avi::build_seek_index(&header, &payload, file_len) {
            println!(
                "\nidx1 at byte {idx1_off} ({idx1_size} bytes) → Index with {} video keyframe entries",
                si.entries.len()
            );
            if let (Some(first), Some(last)) = (si.entries.first(), si.entries.last()) {
                println!(
                    "  first keyframe: {:.3} s → byte {}   last: {:.3} s → byte {}",
                    first.time_ns as f64 / 1e9,
                    first.byte,
                    last.time_ns as f64 / 1e9,
                    last.byte,
                );
            }
        }
    } else {
        println!("\nno idx1 found (would rely on the proportional seek fallback + resync)");
    }
}

#[allow(clippy::disallowed_methods)] // probe tooling reads the file directly
fn find_idx1(path: &str, file_len: u64) -> Option<(u64, u32)> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).ok()?;
    let window = (16 * 1024 * 1024).min(file_len) as usize;
    let start = file_len - window as u64;
    let mut buf = vec![0u8; window];
    let got = f.read_at(&mut buf, start).ok()?;
    buf.truncate(got);
    let mut found = None;
    let mut i = 0;
    while i + 8 <= buf.len() {
        if &buf[i..i + 4] == b"idx1" {
            let size = u32::from_le_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]]);
            let abs = start + i as u64;
            if abs + 8 + size as u64 <= file_len {
                found = Some((abs, size));
            }
        }
        i += 1;
    }
    found
}
