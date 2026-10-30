//! A track's `ContentEncodings` (RFC 9559 section 5.1.4.1.31). zlib and header stripping are
//! undone, encryption, bzlib and lzo1x are refused by name. Steps run from the highest order down.

use crate::ebml::{self, ReadError, id};
use crate::inflate::{InflateError, zlib_inflate};
use crate::reader::read_uint;

pub const MAX_DECODED: usize = 64 << 20;
/// Largest a zlib step inflates one frame to, what the pool sends on mid-stream. Past it the
/// frame would be dropped at the pool anyway, having cost its inflate on the heap.
pub const MAX_INFLATED: usize = profluens_core::memory::SIZED_TOP;
/// Largest CodecPrivate a ContentEncoding may undo to. Real records are a few KB, so a zlib bomb
/// in the header costs at most this per track.
pub const MAX_PRIVATE: usize = 1 << 20;
/// Undone CodecPrivate octets across one Tracks element, the header is parsed and held several
/// times over.
pub const MAX_PRIVATE_TOTAL: usize = 16 << 20;

const MAX_STEPS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Zlib,
    Strip(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Encrypted,
    Algo(u64),
}

impl Refusal {
    pub fn family(self) -> &'static str {
        match self {
            Refusal::Encrypted => "encrypted",
            Refusal::Algo(_) => "compressed",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Encrypted => f.write_str("encrypted (ContentEncodingType 1)"),
            Refusal::Algo(1) => f.write_str("compressed with bzlib (ContentCompAlgo 1)"),
            Refusal::Algo(2) => f.write_str("compressed with lzo1x (ContentCompAlgo 2)"),
            Refusal::Algo(n) => write!(f, "compressed with unknown ContentCompAlgo {n}"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coding {
    pub frames: Vec<Step>,
    pub refused: Option<Refusal>,
}

impl Coding {
    pub fn transforms(&self) -> bool {
        self.refused.is_none() && !self.frames.is_empty()
    }
}

struct Entry {
    order: u64,
    scope: u64,
    step: Result<Step, Refusal>,
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
pub(crate) fn parse(data: &[u8]) -> Result<(Coding, Vec<Step>), ReadError> {
    let mut entries: Vec<Entry> = Vec::new();
    for_each(data, "ContentEncodings", |h, body| {
        if h == id::CONTENT_ENCODING {
            if entries.len() == MAX_STEPS {
                return Err(ReadError::Malformed("more ContentEncodings than the reader takes"));
            }
            entries.push(entry(body)?);
        }
        Ok(())
    })?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.order));
    let mut coding = Coding::default();
    let mut private = Vec::new();
    for e in entries {
        match e.step {
            Err(why) => coding.refused = coding.refused.or(Some(why)),
            Ok(step) => {
                if e.scope & 0x2 != 0 {
                    private.push(step.clone());
                }
                if e.scope & 0x1 != 0 {
                    coding.frames.push(step);
                }
            }
        }
    }
    Ok((coding, private))
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
fn entry(data: &[u8]) -> Result<Entry, ReadError> {
    let (mut order, mut scope, mut kind) = (0, 1, 0);
    let (mut algo, mut settings) = (0, Vec::new());
    for_each(data, "ContentEncoding", |h, body| {
        if h == id::CONTENT_ENCODING_ORDER {
            order = read_uint(body);
        } else if h == id::CONTENT_ENCODING_SCOPE {
            scope = read_uint(body);
        } else if h == id::CONTENT_ENCODING_TYPE {
            kind = read_uint(body);
        } else if h == id::CONTENT_COMPRESSION {
            for_each(body, "ContentCompression", |h, body| {
                if h == id::CONTENT_COMP_ALGO {
                    algo = read_uint(body);
                } else if h == id::CONTENT_COMP_SETTINGS {
                    settings = body.to_vec();
                }
                Ok(())
            })?;
        }
        Ok(())
    })?;
    let step = match (kind, algo) {
        (0, 0) => Ok(Step::Zlib),
        (0, 3) => Ok(Step::Strip(settings)),
        (0, n) => Err(Refusal::Algo(n)),
        _ => Err(Refusal::Encrypted),
    };
    Ok(Entry { order, scope, step })
}

fn for_each<'a>(
    data: &'a [u8],
    what: &'static str,
    mut f: impl FnMut(&[u8], &'a [u8]) -> Result<(), ReadError>,
) -> Result<(), ReadError> {
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h
            .data_end()?
            .ok_or(ReadError::Malformed("unknown-size child in a ContentEncodings master"))?;
        if end > data.len() {
            return Err(ReadError::Malformed(what));
        }
        f(h.id, &data[h.data_start..end])?;
        at = end;
    }
    Ok(())
}

/// Undo `steps` on `src` into `out`, no step producing more than `limit` octets.
pub fn decode(
    steps: &[Step],
    src: &[u8],
    out: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    limit: usize,
) -> Result<(), InflateError> {
    out.clear();
    let Some((first, rest)) = steps.split_first() else {
        out.extend_from_slice(src);
        return Ok(());
    };
    // The first step writes where the last one ends up in `out`.
    let (mut a, mut b) = if steps.len() % 2 == 1 { (out, scratch) } else { (scratch, out) };
    a.clear();
    apply(first, src, a, limit)?;
    for step in rest {
        b.clear();
        apply(step, a, b, limit)?;
        std::mem::swap(&mut a, &mut b);
    }
    Ok(())
}

pub fn decoded_hint(steps: &[Step], len: usize) -> usize {
    steps.iter().fold(len, |n, step| match step {
        Step::Zlib => n.saturating_mul(3).min(MAX_INFLATED),
        Step::Strip(prefix) => n.saturating_add(prefix.len()),
    })
}

fn apply(step: &Step, src: &[u8], dst: &mut Vec<u8>, limit: usize) -> Result<(), InflateError> {
    match step {
        Step::Zlib => zlib_inflate(src, dst, limit.min(MAX_INFLATED)),
        Step::Strip(prefix) => {
            let total = prefix.len().saturating_add(src.len());
            if total > limit {
                return Err(InflateError::TooLarge { limit });
            }
            dst.reserve(total);
            dst.extend_from_slice(prefix);
            dst.extend_from_slice(src);
            Ok(())
        }
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::ebml::{write_binary, write_uint};

    fn master(id: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = children.concat();
        let mut out = Vec::new();
        write_binary(&mut out, id, &body);
        out
    }

    fn uint(id: &[u8], v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        write_uint(&mut out, id, v);
        out
    }

    fn encoding(order: u64, scope: u64, kind: u64, algo: u64, settings: &[u8]) -> Vec<u8> {
        let mut comp = vec![uint(id::CONTENT_COMP_ALGO, algo)];
        if !settings.is_empty() {
            let mut s = Vec::new();
            write_binary(&mut s, id::CONTENT_COMP_SETTINGS, settings);
            comp.push(s);
        }
        let mut children =
            vec![uint(id::CONTENT_ENCODING_ORDER, order), uint(id::CONTENT_ENCODING_SCOPE, scope)];
        children.push(uint(id::CONTENT_ENCODING_TYPE, kind));
        children.push(match kind {
            0 => master(id::CONTENT_COMPRESSION, &comp),
            _ => master(id::CONTENT_ENCRYPTION, &[]),
        });
        master(id::CONTENT_ENCODING, &children)
    }

    fn body(entries: &[Vec<u8>]) -> Vec<u8> {
        entries.concat()
    }

    #[test]
    fn entries_are_undone_from_the_highest_order_down_by_scope() {
        let b = body(&[encoding(0, 1, 0, 3, b"\x0b\x77"), encoding(1, 3, 0, 0, &[])]);
        let (coding, private) = parse(&b).unwrap();
        assert_eq!(coding.frames, [Step::Zlib, Step::Strip(vec![0x0b, 0x77])]);
        assert_eq!(private, [Step::Zlib]);
        assert!(coding.transforms() && coding.refused.is_none());
    }

    #[test]
    fn defaults_are_zlib_on_frames() {
        let b = master(id::CONTENT_ENCODING, &[master(id::CONTENT_COMPRESSION, &[])]);
        let (coding, private) = parse(&b).unwrap();
        assert_eq!((coding.frames, private), (vec![Step::Zlib], vec![]));
    }

    #[test]
    fn encryption_and_other_algorithms_are_refused_by_name() {
        let (coding, _) = parse(&encoding(0, 1, 1, 0, &[])).unwrap();
        assert_eq!(coding.refused, Some(Refusal::Encrypted));
        assert!(!coding.transforms());
        let (coding, _) = parse(&encoding(0, 1, 0, 2, &[])).unwrap();
        assert_eq!(coding.refused, Some(Refusal::Algo(2)));
        assert_eq!(Refusal::Algo(2).to_string(), "compressed with lzo1x (ContentCompAlgo 2)");
        assert_eq!(Refusal::Encrypted.family(), "encrypted");
    }

    #[test]
    fn too_many_or_broken_entries_are_malformed() {
        let five: Vec<Vec<u8>> = (0..5).map(|k| encoding(k, 1, 0, 0, &[])).collect();
        assert!(parse(&body(&five)).is_err());
        let mut cut = encoding(0, 1, 0, 3, b"abc");
        cut.truncate(cut.len() - 1);
        assert!(parse(&cut).is_err());
    }

    #[test]
    fn a_chain_decodes_through_both_buffers_into_out() {
        let z: &[u8] = &[
            0x78, 0x9c, 0xcb, 0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x01, 0x00, 0x0d, 0xd2,
            0x03, 0x24,
        ];
        let (mut out, mut scratch) = (Vec::new(), Vec::new());
        decode(&[Step::Zlib], z, &mut out, &mut scratch, MAX_DECODED).unwrap();
        assert_eq!(out, b"lo world");
        decode(&[Step::Zlib, Step::Strip(b"hel".to_vec())], z, &mut out, &mut scratch, MAX_DECODED)
            .unwrap();
        assert_eq!(out, b"hello world");
        let strips =
            [Step::Strip(b"c".to_vec()), Step::Strip(b"b".to_vec()), Step::Strip(b"a".to_vec())];
        decode(&strips, b"!", &mut out, &mut scratch, MAX_DECODED).unwrap();
        assert_eq!(out, b"abc!");
        decode(&[], b"as is", &mut out, &mut scratch, MAX_DECODED).unwrap();
        assert_eq!(out, b"as is");
        assert!(decode(&[Step::Zlib], b"not zlib", &mut out, &mut scratch, MAX_DECODED).is_err());
        assert_eq!(
            decode(&[Step::Zlib], z, &mut out, &mut scratch, 4),
            Err(InflateError::TooLarge { limit: 4 }),
            "the limit is the caller's"
        );
    }
}
