//! What a fake buffer carries, a tag naming the buffer that its sink reads back into the row, and
//! past the tag a body drawn from the tag and the length, so a truncated, overwritten or lost
//! tail reads as no tag at all.

use crate::prng::mix;

pub const PAYLOAD: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    pub seq: u64,
    pub seek_gen: u32,
    pub size: u32,
    pub stream: u8,
    pub format: u8,
}

/// The tag's check binds the buffer's length too, so a buffer cut to the tag alone is no tag.
fn check(tag: &Tag, len: usize) -> u16 {
    let ids = (tag.stream as u64) << 8 | tag.format as u64 | (len as u64) << 24;
    let word = mix(tag.seq) ^ mix((tag.seek_gen as u64) << 32 | tag.size as u64) ^ mix(ids << 17);
    // The constant keeps a run of zero bytes from reading as tag zero.
    (mix(word ^ 0x51D5_C0DE_51D5_C0DE) >> 48) as u16
}

/// A tag alone, for a buffer of exactly [`PAYLOAD`] octets.
pub fn encode(tag: &Tag, out: &mut [u8]) -> bool {
    encode_len(tag, out, PAYLOAD)
}

fn encode_len(tag: &Tag, out: &mut [u8], len: usize) -> bool {
    let Some(out) = out.get_mut(..PAYLOAD) else {
        return false;
    };
    out[..8].copy_from_slice(&tag.seq.to_le_bytes());
    out[8..12].copy_from_slice(&tag.seek_gen.to_le_bytes());
    out[12..16].copy_from_slice(&tag.size.to_le_bytes());
    out[16] = tag.stream;
    out[17] = tag.format;
    out[18..20].copy_from_slice(&check(tag, len).to_le_bytes());
    true
}

/// Octet `i` past the tag of a buffer `len` long.
fn body_octet(tag: &Tag, len: usize, i: usize) -> u8 {
    let word = (tag.stream as u64) << 56 ^ (len as u64) << 40 ^ (i as u64 / 8) << 8;
    (mix(tag.seq ^ word) >> (8 * (i % 8))) as u8
}

/// The tag, then the body up to `len`. False when `out` holds neither.
pub fn encode_filled(tag: &Tag, out: &mut [u8], len: usize) -> bool {
    if len < PAYLOAD || out.len() < len || !encode_len(tag, out, len) {
        return false;
    }
    for (i, octet) in out[PAYLOAD..len].iter_mut().enumerate() {
        *octet = body_octet(tag, len, i);
    }
    true
}

/// The tag of a buffer whose every octet past it is the tag's own, else `None`.
pub fn read(bytes: &[u8]) -> Option<Tag> {
    if bytes.len() > PAYLOAD * super::scenario::MAX_SWING as usize {
        return None;
    }
    let tag = decode_len(bytes.get(..PAYLOAD)?, bytes.len())?;
    let mut body = bytes[PAYLOAD..].iter().enumerate();
    body.all(|(i, octet)| *octet == body_octet(&tag, bytes.len(), i)).then_some(tag)
}

/// A tag alone, as [`encode`] wrote it.
pub fn decode(bytes: &[u8]) -> Option<Tag> {
    decode_len(bytes, PAYLOAD)
}

fn decode_len(bytes: &[u8], len: usize) -> Option<Tag> {
    let bytes: &[u8; PAYLOAD] = bytes.try_into().ok()?;
    let word = |at: usize| {
        let mut le = [0u8; 4];
        le.copy_from_slice(&bytes[at..at + 4]);
        u32::from_le_bytes(le)
    };
    let seq = (word(4) as u64) << 32 | word(0) as u64;
    let tag = Tag { seq, seek_gen: word(8), size: word(12), stream: bytes[16], format: bytes[17] };
    let sum = u16::from_le_bytes([bytes[18], bytes[19]]);
    (sum == check(&tag, len)).then_some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: Tag =
        Tag { seq: 0x0123_4567_89AB_CDEF, seek_gen: 7, size: 4096, stream: 1, format: 21 };

    #[test]
    fn a_tag_reads_back_as_written() {
        let mut bytes = [0u8; PAYLOAD + 4];
        assert!(encode(&TAG, &mut bytes));
        assert_eq!(decode(&bytes[..PAYLOAD]), Some(TAG));
    }

    #[test]
    fn a_short_buffer_takes_no_tag_and_gives_none() {
        let mut short = [0xAAu8; PAYLOAD - 1];
        assert!(!encode(&TAG, &mut short));
        assert_eq!(short, [0xAAu8; PAYLOAD - 1]);
        assert_eq!(decode(&short), None);
        assert_eq!(decode(&[0u8; PAYLOAD + 1]), None, "a longer buffer is not a tag either");
    }

    #[test]
    fn the_body_binds_the_length_and_every_octet_past_the_tag() {
        let mut bytes = [0u8; 60];
        assert!(encode_filled(&TAG, &mut bytes, 50));
        assert_eq!(read(&bytes[..50]), Some(TAG));
        assert_eq!(read(&bytes[..40]), None, "a lost tail");
        assert_eq!(read(&bytes[..PAYLOAD]), None, "cut to the tag alone");
        assert_eq!(read(&bytes[..51]), None, "a longer buffer");
        for i in PAYLOAD..50 {
            let mut bad = bytes;
            bad[i] ^= 1;
            assert_eq!(read(&bad[..50]), None, "octet {i} overwritten");
        }
        let mut other = [0u8; 60];
        assert!(encode_filled(&TAG, &mut other, 60));
        assert_ne!(bytes[PAYLOAD..50], other[PAYLOAD..50], "the body is the length's too");
        assert!(!encode_filled(&TAG, &mut bytes, 61), "no room for the length");
    }

    #[test]
    fn any_flipped_bit_is_caught() {
        let mut bytes = [0u8; PAYLOAD];
        assert!(encode(&TAG, &mut bytes));
        for bit in 0..PAYLOAD * 8 {
            let mut bad = bytes;
            bad[bit / 8] ^= 1 << (bit % 8);
            assert_eq!(decode(&bad), None, "bit {bit}");
        }
        assert_eq!(decode(&[0u8; PAYLOAD]), None, "zeroes are not a tag");
    }
}
