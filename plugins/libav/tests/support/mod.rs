//! Test streams made by the system ffmpeg, and its decode as the oracle.

// Test code.
#![allow(clippy::disallowed_methods, dead_code)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

pub fn tmp(name: &str) -> PathBuf {
    let base = option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = base.join("pf-libav");
    std::fs::create_dir_all(&dir).expect("tmp dir");
    dir.join(name)
}

/// nextest runs tests in separate processes, two making the same file must not read a truncated
/// one.
fn staging(name: &str) -> PathBuf {
    static NTH: AtomicUsize = AtomicUsize::new(0);
    let nth = NTH.fetch_add(1, Ordering::Relaxed);
    tmp(&format!(".{}-{nth}-{name}", std::process::id()))
}

pub fn make(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    let part = staging(name);
    let p = part.to_str().expect("utf-8 path");
    let mut all: Vec<&str> = args.to_vec();
    all.push(p);
    if !ffmpeg(&all) {
        eprintln!("skip: ffmpeg could not make {name}");
        return None;
    }
    std::fs::rename(&part, tmp(name)).ok()?;
    std::fs::read(tmp(name)).ok()
}

pub fn oracle(input: &str, out_name: &str, args: &[&str]) -> Option<Vec<u8>> {
    let part = staging(out_name);
    let p = part.to_str().expect("utf-8 path");
    let src = tmp(input);
    let s = src.to_str().expect("utf-8 path");
    let mut all = vec!["-i", s, "-fps_mode", "passthrough"];
    all.extend_from_slice(args);
    all.push(p);
    if !ffmpeg(&all) {
        eprintln!("skip: ffmpeg could not decode {input}");
        return None;
    }
    std::fs::rename(&part, tmp(out_name)).ok()?;
    std::fs::read(tmp(out_name)).ok()
}

pub fn ivf_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    assert_eq!(&bytes[..4], b"DKIF");
    let mut out = Vec::new();
    let mut at = 32;
    while at + 12 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 12;
        out.push(bytes[at..at + n].to_vec());
        at += n;
    }
    out
}

fn nals(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            let s = if i > 0 && bytes[i - 1] == 0 { i - 1 } else { i };
            starts.push((s, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
}

/// Annex B cut into access units (H.264 7.4.1.2.3, H.265 7.4.2.4.4).
pub fn annexb_units(bytes: &[u8], hevc: bool) -> Vec<Vec<u8>> {
    let n = nals(bytes);
    let mut cuts = Vec::new();
    let mut seen_vcl = false;
    for (k, &(start, payload)) in n.iter().enumerate() {
        let Some(&h) = bytes.get(payload) else {
            continue;
        };
        let (vcl, first, prefix) = if hevc {
            let t = (h >> 1) & 0x3F;
            let first = bytes.get(payload + 2).is_some_and(|b| b & 0x80 != 0);
            (t < 32, first, matches!(t, 32..=35 | 39))
        } else {
            let t = h & 0x1F;
            let first = bytes.get(payload + 1).is_some_and(|b| b & 0x80 != 0);
            (matches!(t, 1 | 5), first, matches!(t, 6..=9))
        };
        if k == 0 || (seen_vcl && (prefix || (vcl && first))) {
            cuts.push(start);
            seen_vcl = false;
        }
        seen_vcl |= vcl;
    }
    cuts.push(bytes.len());
    cuts.windows(2).map(|w| bytes[w[0]..w[1]].to_vec()).collect()
}

/// MPEG-4 Part 2 cut before each VOP, headers ahead of one riding with it (14496-2 6.2).
pub fn mpeg4_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut cuts = vec![0];
    let (mut picture, mut lead) = (false, None);
    for (start, payload) in nals(bytes) {
        let start = if start < payload - 3 { start + 1 } else { start };
        match bytes.get(payload) {
            Some(0xB6) => {
                if picture {
                    cuts.push(lead.unwrap_or(start));
                }
                picture = true;
                lead = None;
            }
            Some(_) if picture && lead.is_none() => lead = Some(start),
            _ => {}
        }
    }
    cuts.push(bytes.len());
    cuts.windows(2).map(|w| bytes[w[0]..w[1]].to_vec()).collect()
}

/// MPEG-2 cut into pictures (H.262 6.2).
pub fn mpeg2_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut cuts = vec![0];
    let (mut picture, mut lead) = (false, None);
    for (start, payload) in nals(bytes) {
        let start = if start < payload - 3 { start + 1 } else { start };
        match bytes.get(payload) {
            Some(0xB3 | 0xB8) if picture && lead.is_none() => lead = Some(start),
            Some(0x00) => {
                if picture {
                    cuts.push(lead.unwrap_or(start));
                }
                picture = true;
                lead = None;
            }
            _ => {}
        }
    }
    cuts.push(bytes.len());
    cuts.windows(2).map(|w| bytes[w[0]..w[1]].to_vec()).collect()
}

pub fn adts_to_raw(bytes: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut units = Vec::new();
    let mut asc = Vec::new();
    let mut at = 0;
    while at + 7 <= bytes.len() {
        let h = &bytes[at..at + 7];
        assert!(h[0] == 0xFF && h[1] & 0xF0 == 0xF0, "adts sync at {at}");
        let protection_absent = h[1] & 1;
        let profile = (h[2] >> 6) + 1;
        let sri = (h[2] >> 2) & 0xF;
        let chan = ((h[2] & 1) << 2) | (h[3] >> 6);
        let len = (((h[3] & 3) as usize) << 11) | ((h[4] as usize) << 3) | ((h[5] >> 5) as usize);
        let head = if protection_absent == 1 { 7 } else { 9 };
        if asc.is_empty() {
            let v: u16 =
                (u16::from(profile) << 11) | (u16::from(sri) << 7) | (u16::from(chan) << 3);
            asc = v.to_be_bytes().to_vec();
        }
        units.push(bytes[at + head..at + len].to_vec());
        at += len;
    }
    (asc, units)
}
