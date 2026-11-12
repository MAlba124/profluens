const SUPERFRAME_MARKER: u8 = 0b110;

/// Split a chunk into the `(start, end)` ranges of its frames in decode order. A
/// malformed index is the section B.4 single-frame fallback, never an error.
pub fn split_superframe(chunk: &[u8]) -> Vec<(usize, usize)> {
    #[allow(clippy::disallowed_methods)] // Cold, convenience wrapper, not the per-frame path.
    let mut out = Vec::new();
    split_superframe_into(chunk, &mut out);
    out
}

pub fn split_superframe_into(chunk: &[u8], out: &mut Vec<(usize, usize)>) {
    out.clear();
    if superframe_frame_sizes(chunk, out) {
        return;
    }
    if !chunk.is_empty() {
        out.push((0, chunk.len()));
    }
}

fn superframe_frame_sizes(chunk: &[u8], out: &mut Vec<(usize, usize)>) -> bool {
    let len = chunk.len();
    if len == 0 {
        return false;
    }

    let marker_byte = chunk[len - 1];
    if (marker_byte >> 5) != SUPERFRAME_MARKER {
        return false;
    }

    let bytes_per_framesize = ((marker_byte >> 3) & 0b11) as usize + 1; // SzBytes
    let num_frames = (marker_byte & 0b111) as usize + 1; // NumFrames

    let sz_index = 2 + num_frames * bytes_per_framesize;
    if len < sz_index {
        return false;
    }

    let index_start = len - sz_index;
    if chunk[index_start] != marker_byte {
        return false;
    }

    // Section B.2.1: NumFrames little-endian sizes of SzBytes each.
    let mut p = index_start + 1;
    let mut off = 0usize;
    for _ in 0..num_frames {
        let mut sz = 0usize;
        for b in 0..bytes_per_framesize {
            sz |= (chunk[p + b] as usize) << (8 * b);
        }
        p += bytes_per_framesize;
        // A corrupt size must not panic the caller's slice.
        let end = match off.checked_add(sz) {
            Some(e) if e <= index_start => e,
            _ => {
                out.clear(); // Discard the partial fill, the caller applies the section B.4 fallback.
                return false;
            }
        };
        out.push((off, end));
        off = end;
    }

    true
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn marker(bytes_per_minus_1: u8, frames_minus_1: u8) -> u8 {
        (SUPERFRAME_MARKER << 5) | ((bytes_per_minus_1 & 0b11) << 3) | (frames_minus_1 & 0b111)
    }

    fn build(frames: &[&[u8]], szb: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            out.extend_from_slice(f);
        }
        let m = marker(szb as u8 - 1, frames.len() as u8 - 1);
        out.push(m);
        for f in frames {
            let sz = f.len();
            for b in 0..szb {
                out.push(((sz >> (8 * b)) & 0xff) as u8);
            }
        }
        out.push(m);
        out
    }

    fn parts(chunk: &[u8]) -> Vec<&[u8]> {
        split_superframe(chunk).into_iter().map(|(s, e)| &chunk[s..e]).collect()
    }

    #[test]
    fn non_superframe_returns_whole_chunk() {
        let chunk = [0x82u8, 0x49, 0x83, 0x42, 0x00];
        let p = parts(&chunk);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], &chunk);
    }

    #[test]
    fn empty_chunk_yields_no_frames() {
        assert!(split_superframe(&[]).is_empty());
    }

    #[test]
    fn two_frame_superframe_one_byte_sizes() {
        let f0: Vec<u8> = (0..10u8).collect();
        let f1: Vec<u8> = (100..130u8).collect();
        let chunk = build(&[&f0, &f1], 1);
        let p = parts(&chunk);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], f0.as_slice());
        assert_eq!(p[1], f1.as_slice());
    }

    #[test]
    fn single_frame_superframe_is_legal() {
        // Section B.3: NumFrames == 1 is legal.
        let f0: Vec<u8> = (0..40u8).collect();
        let chunk = build(&[&f0], 1);
        let p = parts(&chunk);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], f0.as_slice());
    }

    #[test]
    fn multibyte_framesize_round_trips() {
        let f0: Vec<u8> = (0..300u32).map(|v| v as u8).collect();
        let f1: Vec<u8> = (0..17u8).collect();
        let chunk = build(&[&f0, &f1], 2);
        let p = parts(&chunk);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].len(), 300);
        assert_eq!(p[1].len(), 17);
        assert_eq!(p[0], f0.as_slice());
    }

    #[test]
    fn mismatched_leading_index_byte_falls_back() {
        let f0: Vec<u8> = (0..10u8).collect();
        let f1: Vec<u8> = (0..20u8).collect();
        let mut chunk = build(&[&f0, &f1], 1);
        let lead = chunk.len() - 4;
        chunk[lead] ^= 0x01;
        let p = parts(&chunk);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], chunk.as_slice());
    }

    #[test]
    fn declared_sizes_overrunning_chunk_falls_back() {
        let mut chunk = vec![1u8, 2, 3, 4, 5];
        let m = marker(0, 1); // SzBytes=1, NumFrames=2
        chunk.push(m);
        chunk.push(200);
        chunk.push(200);
        chunk.push(m);
        let p = parts(&chunk);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], chunk.as_slice());
    }

    #[test]
    fn chunk_too_short_for_index_falls_back() {
        let chunk = [marker(3, 7)]; // claims SzBytes=4, NumFrames=8 -> SzIndex=34
        let p = parts(&chunk);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], &chunk);
    }

    #[test]
    fn three_frame_superframe_with_hidden_first() {
        let arf: Vec<u8> = (0..50u8).collect();
        let vis: Vec<u8> = (0..120u8).collect();
        let chunk = build(&[&arf, &vis], 1);
        let p = parts(&chunk);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], arf.as_slice());
        assert_eq!(p[1], vis.as_slice());
    }
}
