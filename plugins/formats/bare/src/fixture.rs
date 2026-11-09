//! Synthetic native FLAC files, for tests here and in crates that build on this one. The frames
//! carry valid headers and filler, no decoder can play them, the opener and the reader only time
//! them. `profluens_core::fetch::drive_slice` reads them from memory.

#![allow(clippy::disallowed_methods)] // test builders, they allocate freely

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

/// An AC-3 frame of `kbps` at `fscod` with `acmod` and the LFE on, filler to its length.
pub fn ac3_frame(fscod: u8, kbps: u32, odd: bool, acmod: u8) -> Vec<u8> {
    let code = crate::sync::AC3_KBPS.iter().position(|&k| k == kbps).unwrap_or(0) as u8 * 2
        + u8::from(odd);
    // bsid 8, bsmod 0, then acmod and for 3/2 the two mix levels before lfeon
    let mut b = vec![0x0B, 0x77, 0, 0, fscod << 6 | code, 8 << 3, acmod << 5, 0];
    let lfe_at = 51
        + if acmod & 1 == 1 && acmod != 1 { 2 } else { 0 }
        + if acmod & 4 != 0 { 2 } else { 0 }
        + if acmod == 2 { 2 } else { 0 };
    b[lfe_at / 8] |= 0x80 >> (lfe_at % 8);
    let len = crate::sync::frame(crate::sync::SyncKind::Ac3, &b).map_or(8, |f| f.len as usize);
    b.resize(len, 0x55);
    b
}

/// An E-AC-3 frame of `len` bytes, six blocks at 48 kHz, 3/2 with LFE.
pub fn eac3_frame(len: usize, strmtyp: u8, substream: u8) -> Vec<u8> {
    let words = (len / 2 - 1) as u16;
    let mut b = vec![
        0x0B,
        0x77,
        strmtyp << 6 | substream << 3 | (words >> 8) as u8,
        words as u8,
        0b0011_1111,
        16 << 3,
    ];
    b.resize(len, 0x33);
    b
}

/// A DTS core frame in the 16 bit big endian form, 512 samples at 48 kHz, 5.1, 24 bit.
pub fn dts_frame(len: usize) -> Vec<u8> {
    let mut h = [0u8; 16];
    let mut put = |at: usize, n: usize, v: u32| {
        for i in 0..n {
            if v >> (n - 1 - i) & 1 == 1 {
                h[(at + i) / 8] |= 0x80 >> ((at + i) % 8);
            }
        }
    };
    put(0, 32, 0x7FFE_8001);
    put(32, 1, 1);
    put(33, 5, 31);
    put(39, 7, 15);
    put(46, 14, len as u32 - 1);
    put(60, 6, 9);
    put(66, 4, 13);
    put(70, 5, 15);
    put(85, 2, 1);
    put(95, 3, 6);
    let mut b = h.to_vec();
    b.resize(len, 0x11);
    b
}

/// `b` in the 14 bit big endian form, 14 bits in each 16 bit word.
pub fn to_14(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut held) = (0u64, 0u32);
    for &x in b {
        (acc, held) = (acc << 8 | u64::from(x), held + 8);
        while held >= 14 {
            held -= 14;
            let w = (acc >> held) as u16 & 0x3FFF;
            // the sign extension of the 14 bit word, as the sync word shows it
            let w = if w & 0x2000 != 0 { w | 0xC000 } else { w };
            out.extend_from_slice(&w.to_be_bytes());
            acc &= (1 << held) - 1;
        }
    }
    out
}

/// A WAV file of `frames` sample frames, the sample index in every byte, WAVE_FORMAT_EXTENSIBLE
/// with `mask` when given, a `LIST` chunk of `pad` bytes ahead of `data` and one behind it.
pub fn wav_file(
    rate: u32,
    channels: u16,
    bits: u16,
    float: bool,
    frames: u32,
    mask: Option<u32>,
    pad: usize,
) -> Vec<u8> {
    let align = channels * bits / 8;
    let tag: u16 = if float { 3 } else { 1 };
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&if mask.is_some() { 0xFFFEu16 } else { tag }.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    fmt.extend_from_slice(&align.to_le_bytes());
    fmt.extend_from_slice(&bits.to_le_bytes());
    if let Some(m) = mask {
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        fmt.extend_from_slice(&m.to_le_bytes());
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&[0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
    }
    let data: Vec<u8> = (0..frames)
        .flat_map(|i| std::iter::repeat_n((i % 251) as u8, usize::from(align)))
        .collect();
    let mut chunks = Vec::new();
    let mut chunk = |id: &[u8; 4], body: &[u8]| {
        chunks.extend_from_slice(id);
        chunks.extend_from_slice(&(body.len() as u32).to_le_bytes());
        chunks.extend_from_slice(body);
        if body.len() % 2 == 1 {
            chunks.push(0);
        }
    };
    chunk(b"fmt ", &fmt);
    if pad > 0 {
        chunk(b"LIST", &vec![b'x'; pad]);
    }
    chunk(b"data", &data);
    chunk(b"LIST", b"INFOtail");
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&(chunks.len() as u32 + 4).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(&chunks);
    b
}
