//! Synthetic native FLAC files and an in-memory read callback, for tests here and in crates
//! that build on this one. The frames carry valid headers and filler, no decoder can play
//! them, the opener and the reader only time them.

#![allow(clippy::disallowed_methods)] // test builders, they allocate freely

/// A read callback over bytes in memory.
pub fn reader(file: &[u8]) -> impl FnMut(u64, &mut [u8]) -> std::io::Result<usize> + '_ {
    move |off, buf| {
        let s = (off as usize).min(file.len());
        let n = buf.len().min(file.len() - s);
        buf[..n].copy_from_slice(&file[s..s + n]);
        Ok(n)
    }
}

/// A FLAC frame header, fixed blocking, frame number `num`, `block` samples, CRC-8 set.
pub fn flac_frame_header(si: &crate::flac::StreamInfo, num: u64, block: u32) -> Vec<u8> {
    let bs = match block {
        4096 => 12,
        1152 => 3,
        256..=32768 if block.is_power_of_two() => block.trailing_zeros() as u8,
        _ => 7,
    };
    let sr = match si.rate {
        44_100 => 9,
        48_000 => 10,
        _ => 0,
    };
    let depth = match si.bits {
        8 => 1,
        16 => 4,
        24 => 6,
        _ => 0,
    };
    let mut h = vec![0xFF, 0xF8, (bs << 4) | sr, ((si.channels - 1) << 4) | (depth << 1)];
    if num < 0x80 {
        h.push(num as u8);
    } else {
        // the UTF-8 form, enough bytes for 31 bits
        let n = (1..=6).find(|&n| num < 1u64 << (5 * n + 1)).unwrap_or(6) as u32;
        h.push(((0xFF00u16 >> n) as u8) | (num >> (6 * (n - 1))) as u8);
        for i in (0..n - 1).rev() {
            h.push(0x80 | ((num >> (6 * i)) & 0x3F) as u8);
        }
    }
    if bs == 7 {
        h.extend_from_slice(&((block - 1) as u16).to_be_bytes());
    }
    h.push(crate::flac::crc8(&h));
    h
}

/// A synthetic native FLAC file and where its frames are.
pub struct SynthFlac {
    pub bytes: Vec<u8>,
    /// Frame offset, length and first sample.
    pub frames: Vec<(u64, u32, u64)>,
    pub streaminfo: [u8; 34],
}

/// `n` frames of 4096 samples at 44.1 kHz stereo 16 bit, the last `last` samples long, the
/// metadata blocks `meta` (type, body) after STREAMINFO. Frame bodies hold fake syncs, a
/// header that fails its CRC and one whose CRC holds but whose number is wrong.
pub fn flac_file(n: usize, last: u32, meta: &[(u8, Vec<u8>)], seektable: bool) -> SynthFlac {
    let total = (n as u64 - 1) * 4096 + u64::from(last);
    let mut si = [0u8; 34];
    si[..2].copy_from_slice(&4096u16.to_be_bytes());
    si[2..4].copy_from_slice(&4096u16.to_be_bytes());
    let packed = (44_100u64 << 44) | (1 << 41) | (15 << 36) | total;
    si[10..18].copy_from_slice(&packed.to_be_bytes());
    let info = crate::flac::stream_info(&si).unwrap_or_default();
    let mut body: Vec<u8> = Vec::new();
    let mut frames = Vec::new();
    for i in 0..n {
        let block = if i + 1 == n { last } else { 4096 };
        let at = body.len() as u64;
        body.extend(flac_frame_header(&info, i as u64, block));
        let len = 900 + (i * 397) % 2500;
        body.extend((0..len).map(|k| ((i * 131 + k * 17) % 251) as u8));
        if i % 3 == 1 {
            // a sync whose CRC fails
            body.extend([0xFF, 0xF8, 0xC9, 0x18, 0x00, 0x00]);
        }
        if i % 4 == 2 {
            // a valid header of a frame far away, a false sync the number gives away
            body.extend(flac_frame_header(&info, (i + 40) as u64, 4096));
        }
        body.extend([0xAB, 0xCD]);
        frames.push((at, (body.len() as u64 - at) as u32, i as u64 * 4096));
    }
    // the frame sizes as an encoder states them
    let max = frames.iter().map(|f| f.1).max().unwrap_or(0);
    let min = frames.iter().map(|f| f.1).min().unwrap_or(0);
    si[4..7].copy_from_slice(&min.to_be_bytes()[1..]);
    si[7..10].copy_from_slice(&max.to_be_bytes()[1..]);
    let mut blocks: Vec<(u8, Vec<u8>)> = vec![(0, si.to_vec())];
    if seektable {
        // a point every 10 frames, offsets known once the metadata length is
        let pts: Vec<(u64, u64)> = frames.iter().step_by(10).map(|f| (f.2, f.0)).collect();
        let mut t = Vec::new();
        for (s, o) in pts {
            t.extend(s.to_be_bytes());
            t.extend(o.to_be_bytes());
            t.extend(4096u16.to_be_bytes());
        }
        t.extend(u64::MAX.to_be_bytes());
        t.extend([0u8; 10]);
        blocks.push((3, t));
    }
    blocks.extend(meta.iter().cloned());
    let mut bytes = b"fLaC".to_vec();
    let last_ix = blocks.len() - 1;
    for (i, (kind, b)) in blocks.iter().enumerate() {
        bytes.push(if i == last_ix { 0x80 | kind } else { *kind });
        bytes.extend(&(b.len() as u32).to_be_bytes()[1..]);
        bytes.extend(b);
    }
    let first = bytes.len() as u64;
    bytes.extend(body);
    for f in &mut frames {
        f.0 += first;
    }
    SynthFlac { bytes, frames, streaminfo: si }
}
