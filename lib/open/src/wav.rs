use std::ops::Range;

/// Where a WAV file's `LIST` chunk of form `INFO` lies.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Info {
    /// Its body behind the form, in the bytes walked.
    At(Range<usize>),
    /// The bytes end at this chunk of the file, the walk goes on from there.
    Past(u64),
    None,
}

/// The RIFF chunks of `b`, the file from `base` on a chunk boundary, walked to the `LIST INFO`
/// one. From the top `base` is 0 and the walk starts behind the RIFF header. A `data` chunk of
/// no stated size runs to the end of the file. A chunk of odd size is followed by a pad byte.
pub(crate) fn info(b: &[u8], base: u64, len: u64) -> Info {
    let mut at = if base == 0 { 12 } else { base };
    while at.saturating_add(8) <= len {
        let Some(off) = at.checked_sub(base).and_then(|o| usize::try_from(o).ok()) else {
            return Info::None;
        };
        let Some(h) = b.get(off..off + 8) else { return Info::Past(at) };
        let size = u64::from(u32::from_le_bytes([h[4], h[5], h[6], h[7]]));
        let body = off + 8;
        if &h[..4] == b"LIST" && size >= 4 {
            let Some(form) = b.get(body..body + 4) else { return Info::Past(at) };
            if form == b"INFO" {
                let end = usize::try_from(size).ok().and_then(|n| body.checked_add(n));
                return match end.filter(|&e| e <= b.len()) {
                    Some(end) => Info::At(body + 4..end),
                    None => Info::Past(at),
                };
            }
        }
        // streamed, its size never written
        if &h[..4] == b"data" && (size == 0 || size == u64::from(u32::MAX)) {
            return Info::None;
        }
        at = at.saturating_add(8 + size + (size & 1));
    }
    Info::None
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u32).to_le_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body = [&b"WAVE"[..], &chunks.concat()].concat();
        [&b"RIFF"[..], &(body.len() as u32).to_le_bytes(), &body].concat()
    }

    #[test]
    fn info_before_or_behind_the_data() {
        let list = chunk(b"LIST", b"INFOINAM\x03\x00\x00\x00ab\x00\x00");
        let fmt = chunk(b"fmt ", &[0; 16]);
        let front = riff(&[fmt.clone(), list.clone(), chunk(b"data", &[0; 7])]);
        let len = front.len() as u64;
        assert_eq!(info(&front, 0, len), Info::At(48..60));
        assert_eq!(&front[48..52], b"INAM");
        let back = riff(&[fmt, chunk(b"data", &[0; 7]), chunk(b"bext", &[1; 3]), list]);
        let len = back.len() as u64;
        // the odd data and bext are padded
        let list_at = 12 + 24 + 16 + 12;
        assert_eq!(info(&back[..44], 0, len), Info::Past(52), "the data past the head");
        assert_eq!(info(&back[..list_at + 10], 0, len), Info::Past(list_at as u64), "cut");
        assert_eq!(info(&back, 0, len), Info::At(list_at + 12..back.len()));
        let tail = &back[52..];
        assert_eq!(info(tail, 52, len), Info::At(list_at - 52 + 12..tail.len()));
    }

    #[test]
    fn no_info_or_a_streamed_data_chunk_is_none() {
        let fmt = chunk(b"fmt ", &[0; 16]);
        let plain = riff(&[fmt.clone(), chunk(b"data", &[0; 8])]);
        assert_eq!(info(&plain, 0, plain.len() as u64), Info::None);
        let mut streamed = riff(&[fmt.clone(), chunk(b"data", &[0; 8])]);
        streamed[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(info(&streamed[..44], 0, 1 << 30), Info::None);
        let adtl = riff(&[fmt, chunk(b"LIST", b"adtl"), chunk(b"data", &[0; 2])]);
        assert_eq!(info(&adtl, 0, adtl.len() as u64), Info::None, "another form");
        for n in 0..plain.len() {
            let _ = info(&plain[..n], 0, plain.len() as u64);
        }
        assert_eq!(info(b"", 0, 0), Info::None);
    }
}
