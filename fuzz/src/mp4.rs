//! `mp4demux` made as the player makes it.

// Head copied once per input.
#![allow(clippy::disallowed_methods)]

use pf_mp4::{Mp4Demux, Mp4Reader};
use profluens_core::element::Element;

use crate::rig::{Bounds, Made, Target};

pub const TARGET: Target = Target {
    make,
    bounds: Bounds {
        heap_per_byte: 136,
        heap_per_head: 258,
        heap_fixed: 0,
        out_per_byte: 3,
        out_per_head: 3,
    },
};

const MAX_MOOV: u64 = 64 << 20;
const MAX_BOXES: u32 = 256;
const MAX_MFRA: u64 = 16 << 20;

/// `lib/play`'s `mp4_head_at` over a file in memory.
pub fn head(file: &[u8]) -> Option<Vec<u8>> {
    let len = file.len() as u64;
    let (mut at, mut mdat, mut moov) = (0u64, None, None);
    for _ in 0..MAX_BOXES {
        if at + 8 > len {
            break;
        }
        let h = &file[at as usize..];
        let size = u64::from(u32::from_be_bytes([h[0], h[1], h[2], h[3]]));
        let kind = &h[4..8];
        let (header, size) = match size {
            0 => (8, len - at),
            1 if h.len() < 16 => return None,
            1 => (16, u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]])),
            n => (8, n),
        };
        if size < header {
            return None;
        }
        let end = at.checked_add(size).filter(|end| *end <= len)?;
        if kind == b"mdat" && mdat.is_none() {
            mdat = Some(at);
        }
        if kind == b"moov" {
            if size > MAX_MOOV {
                return None;
            }
            moov = Some((at as usize, size as usize));
        }
        if mdat.is_some() && moov.is_some() {
            break;
        }
        at = end;
    }
    let (moov_at, moov_len) = moov?;
    let prefix = mdat.unwrap_or(len);
    if prefix > MAX_MOOV {
        return None;
    }
    let mut head = file[..prefix as usize].to_vec();
    if moov_at as u64 >= prefix {
        head.extend_from_slice(&file[moov_at..moov_at + moov_len]);
    }
    Some(head)
}

fn mfra(file: &[u8]) -> &[u8] {
    let len = file.len() as u64;
    let tail = &file[file.len().saturating_sub(16)..];
    let size = pf_mp4::frag::mfra_size(tail).filter(|size| (16..=MAX_MFRA.min(len)).contains(size));
    size.map_or(&[], |size| &file[(len - size) as usize..])
}

/// Variants 0 and 1 resolve the tables first, the player's path. 2 is `new`, 3 `passthrough`.
fn make(file: &[u8], variant: u8) -> Option<Made> {
    let head = head(file)?;
    let head_len = head.len();
    let element: Box<dyn Element> = match variant {
        2 => Box::new(Mp4Demux::new(head)),
        3 => Box::new(Mp4Demux::passthrough(head)),
        _ => {
            let mut reader = Mp4Reader::new(&head).ok()?;
            if reader.is_fragmented() {
                reader = Mp4Reader::indexed(&head, mfra(file)).ok()?;
            }
            Box::new(Mp4Demux::resolved(reader))
        }
    };
    Some(Made { element, sink: Mp4Demux::IN, head_len, statics: &[], out_scale: 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mp4_head_appends_a_moov_behind_the_mdat() {
        let boxed = |kind: &[u8; 4], body: &[u8]| {
            let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(body);
            b
        };
        let (ftyp, mdat, moov) =
            (boxed(b"ftyp", b"isom"), boxed(b"mdat", &[7; 20]), boxed(b"moov", &[1; 4]));
        let file = [ftyp.clone(), mdat, moov.clone()].concat();
        assert_eq!(head(&file), Some([ftyp, moov].concat()));
        assert_eq!(head(&file[..30]), None);
    }
}
