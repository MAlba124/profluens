//! Matroska seeds no encoder writes. A zlib ContentEncoding track, and a committed file with a
//! zero padded tail.

use std::path::Path;

use crate::error::Error;
use crate::util;

fn size(out: &mut Vec<u8>, n: u64) {
    let len = (1..=8u32).find(|l| n < (1u64 << (7 * l)) - 1).unwrap_or(8);
    let marked = n | (1u64 << (7 * len));
    out.extend_from_slice(&marked.to_be_bytes()[8 - len as usize..]);
}

fn el(out: &mut Vec<u8>, id: &[u8], body: &[u8]) {
    out.extend_from_slice(id);
    size(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn uint(out: &mut Vec<u8>, id: &[u8], v: u64) {
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take(7).take_while(|b| **b == 0).count();
    el(out, id, &bytes[skip..]);
}

fn master(out: &mut Vec<u8>, id: &[u8], fill: impl FnOnce(&mut Vec<u8>)) {
    let mut body = Vec::new();
    fill(&mut body);
    el(out, id, &body);
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01, 0x01];
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, len: u32) {
        self.acc |= u64::from(value) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            (self.acc, self.n) = (self.acc >> 8, self.n - 8);
        }
    }

    /// Huffman codes go out most significant bit first (RFC 1951 section 3.1.1).
    fn code(&mut self, value: u32, len: u32) {
        self.put(value.reverse_bits() >> (32 - len), len);
    }
}

/// zlib of `1 + 258 k` zeros, a fixed Huffman literal then `k` matches of 258 at distance 1.
fn zlib_zeros(k: usize) -> Vec<u8> {
    let mut w = Bits { out: vec![0x78, 0x01], acc: 0, n: 0 };
    w.put(0b011, 3);
    w.code(0x30, 8);
    for _ in 0..k {
        w.code(0b1100_0101, 8);
        w.code(0, 5);
    }
    w.code(0, 7);
    if w.n > 0 {
        w.out.push(w.acc as u8);
    }
    w.out.extend_from_slice(&adler32(&vec![0; 1 + 258 * k]).to_be_bytes());
    w.out
}

const EBML: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
const SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const INFO: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
const TRACKS: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
const CLUSTER: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];

/// An unknown-size Segment with a zlib coded S_TEXT/UTF8 track, plain and inflating frames.
fn zlib_text() -> Vec<u8> {
    let mut f = Vec::new();
    master(&mut f, EBML, |b| el(b, &[0x42, 0x82], b"matroska"));
    f.extend_from_slice(SEGMENT);
    f.push(0xFF);
    master(&mut f, INFO, |i| uint(i, &[0x2A, 0xD7, 0xB1], 1_000_000));
    master(&mut f, TRACKS, |t| {
        master(t, &[0xAE], |e| {
            uint(e, &[0xD7], 1);
            uint(e, &[0x83], 0x11);
            el(e, &[0x86], b"S_TEXT/UTF8");
            master(e, &[0x6D, 0x80], |encodings| {
                master(encodings, &[0x62, 0x40], |c| {
                    uint(c, &[0x50, 0x31], 0);
                    uint(c, &[0x50, 0x32], 1);
                    uint(c, &[0x50, 0x33], 0);
                    master(c, &[0x50, 0x34], |z| uint(z, &[0x42, 0x54], 0));
                });
            });
        });
    });
    for cluster in 0..2u64 {
        master(&mut f, CLUSTER, |c| {
            uint(c, &[0xE7], cluster * 1_000);
            for k in 0..4u16 {
                // 33 KiB from about 100 octets, the rig scales its output bound for zlib.
                let frame = match k {
                    3 => zlib_zeros(128),
                    _ => zlib_stored(format!("line {k} of cluster {cluster}").as_bytes()),
                };
                master(c, &[0xA0], |g| {
                    let mut block = vec![0x81];
                    block.extend_from_slice(&(k * 200).to_be_bytes());
                    block.push(0);
                    block.extend_from_slice(&frame);
                    el(g, &[0xA1], &block);
                    uint(g, &[0x9B], 150);
                });
            }
        });
    }
    f
}

/// Seeds written by hand into `raw/mkv`.
pub fn mkv(root: &Path, raw: &Path) -> Result<(), Error> {
    let dir = raw.join("mkv");
    util::write(&dir.join("zlib_text.mkv"), &zlib_text())?;
    let sines = root.join("plugins/audio/opus/tests/fixtures/sines_5_1.mka");
    if sines.is_file() {
        let mut padded = util::read(&sines)?;
        padded.extend_from_slice(&[0; 64]);
        util::write(&dir.join("zero_tail.mka"), &padded)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_take_the_shortest_vint() {
        let enc = |n| {
            let mut v = Vec::new();
            size(&mut v, n);
            v
        };
        assert_eq!(enc(0), [0x80]);
        assert_eq!(enc(126), [0xFE]);
        assert_eq!(enc(127), [0x40, 0x7F]);
        assert_eq!(enc(1000), [0x43, 0xE8]);
    }

    #[test]
    fn adler32_matches_rfc_1950() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn the_zlib_seed_carries_its_encoding_and_clusters() {
        let f = zlib_text();
        let has = |id: &[u8]| f.windows(id.len()).any(|w| w == id);
        assert!(f.starts_with(EBML) && has(&[0x6D, 0x80]) && has(&[0x50, 0x34]));
        assert_eq!(f.windows(4).filter(|w| *w == CLUSTER).count(), 2);
        assert_eq!(zlib_zeros(1).len(), 2 + 4 + 4);
    }
}
