//! A minimal EBML reader for the structural round-trip tests.

#![allow(dead_code, clippy::disallowed_methods)] // shared helpers; not every test uses every accessor

pub fn read_vint(data: &[u8], off: &mut usize) -> (u64, usize, bool) {
    let first = data[*off];
    assert_ne!(first, 0x00, "a VINT's first octet cannot be zero (length > 8)");
    let len = first.leading_zeros() as usize + 1;
    assert!(len <= 8, "VINT length out of range");
    let mut value = (first as u64) & ((0xFFu16 >> len) as u64);
    let mut all_ones = value == ((0xFFu16 >> len) as u64);
    for i in 1..len {
        let b = data[*off + i];
        value = (value << 8) | b as u64;
        all_ones &= b == 0xFF;
    }
    *off += len;
    (value, len, all_ones)
}

pub fn read_id(data: &[u8], off: &mut usize) -> Vec<u8> {
    let first = data[*off];
    let len = first.leading_zeros() as usize + 1;
    let id = data[*off..*off + len].to_vec();
    *off += len;
    id
}

#[derive(Clone, Debug)]
pub struct Element {
    pub id: Vec<u8>,
    pub data: Option<(usize, usize)>,
}

pub fn read_element_header(data: &[u8], off: &mut usize) -> Element {
    let id = read_id(data, off);
    let (size, _len, all_ones) = read_vint(data, off);
    if all_ones {
        Element { id, data: None } // unknown size (streamed master)
    } else {
        let start = *off;
        Element { id, data: Some((start, start + size as usize)) }
    }
}

pub fn walk(data: &[u8], masters: &[&[u8]]) -> Vec<(usize, Element)> {
    let mut out = Vec::new();
    walk_range(data, 0, data.len(), 0, masters, &mut out);
    out
}

fn is_master(id: &[u8], masters: &[&[u8]]) -> bool {
    masters.contains(&id)
}

fn walk_range(
    data: &[u8],
    start: usize,
    end: usize,
    depth: usize,
    masters: &[&[u8]],
    out: &mut Vec<(usize, Element)>,
) {
    let mut off = start;
    while off < end {
        let el = read_element_header(data, &mut off);
        let id = el.id.clone();
        match el.data {
            Some((ds, de)) => {
                out.push((depth, el));
                if is_master(&id, masters) {
                    walk_range(data, ds, de, depth + 1, masters, out);
                }
                off = de; // skip past the element's data
            }
            None => {
                out.push((depth, el));
                let child_end = next_streamed_master(data, off, end, &id, masters);
                walk_range(data, off, child_end, depth + 1, masters, out);
                off = child_end;
            }
        }
    }
}

fn next_streamed_master(
    data: &[u8],
    start: usize,
    end: usize,
    open_id: &[u8],
    masters: &[&[u8]],
) -> usize {
    use pf_mkv::ebml::id;
    if open_id == id::CLUSTER {
        let mut off = start;
        while off < end {
            let here = off;
            let child = read_element_header(data, &mut off);
            if child.id == id::CLUSTER {
                return here; // next cluster starts here, this cluster's children end here
            }
            match child.data {
                Some((_ds, de)) => off = de,
                None => return here, // another streamed master terminates
            }
            let _ = masters;
        }
        end
    } else {
        end
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SimpleBlock {
    pub track: u64,
    pub rel_ts: i16,
    pub keyframe: bool,
    pub frame: Vec<u8>,
}

pub fn parse_simple_block(data: &[u8]) -> SimpleBlock {
    let mut off = 0;
    let (track, _len, _ao) = read_vint(data, &mut off);
    let rel_ts = i16::from_be_bytes([data[off], data[off + 1]]);
    off += 2;
    let flags = data[off];
    off += 1;
    SimpleBlock { track, rel_ts, keyframe: flags & 0x80 != 0, frame: data[off..].to_vec() }
}

#[test]
fn vint_reader_matches_writer() {
    use pf_mkv::ebml;
    for &v in &[0u64, 1, 126, 127, 128, 16_000, 1_000_000] {
        let mut buf = Vec::new();
        ebml::write_size(&mut buf, v);
        let mut off = 0;
        let (got, _len, _ao) = read_vint(&buf, &mut off);
        assert_eq!(got, v);
        assert_eq!(off, buf.len());
    }
    let mut off = 0;
    let (_v, len, all_ones) = read_vint(&[0xFF], &mut off);
    assert_eq!(len, 1);
    assert!(all_ones);
}
