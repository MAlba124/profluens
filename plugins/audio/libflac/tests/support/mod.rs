// Each test uses its own part of this, and builds on the heap.
#![allow(dead_code, clippy::disallowed_methods)]

/// Header and frame byte ranges, found by CRC-16 apart from the decoder (RFC 9639 section 9.3).
pub fn split_frames(stream: &[u8]) -> (std::ops::Range<usize>, Vec<std::ops::Range<usize>>) {
    let head = pf_tags::flacmeta::audio_start(stream).expect("a whole header");
    let sync = |at: usize| {
        stream.len() == at
            || (stream.len() > at + 1 && stream[at] == 0xFF && stream[at + 1] & 0xFE == 0xF8)
    };
    let mut frames = Vec::new();
    let mut start = head;
    while start < stream.len() {
        assert!(sync(start), "no frame sync at {start}");
        let mut crc = 0u16;
        let mut end = None;
        for (i, &b) in stream[start..].iter().enumerate() {
            crc = crc16_step(crc, b);
            let at = start + i + 1;
            if i >= 4 && crc == 0 && sync(at) {
                end = Some(at);
                break;
            }
        }
        let end = end.expect("a frame whose CRC checks");
        frames.push(start..end);
        start = end;
    }
    (0..head, frames)
}

#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    acc: u64,
    bits: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_bits(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1);
            self.bits += 1;
            if self.bits == 8 {
                self.bytes.push(self.acc as u8);
                self.acc = 0;
                self.bits = 0;
            }
        }
    }

    pub fn write_signed(&mut self, value: i64, n: u32) {
        let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        self.write_bits(value as u64 & mask, n);
    }

    pub fn write_unary(&mut self, q: u32) {
        for _ in 0..q {
            self.write_bits(0, 1);
        }
        self.write_bits(1, 1);
    }

    pub fn align(&mut self) {
        while self.bits != 0 {
            self.write_bits(0, 1);
        }
    }

    pub fn as_aligned_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        self.align();
        self.bytes
    }
}

pub fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |crc, &b| {
        let mut c = crc ^ b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
        }
        c
    })
}

pub fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0u16, |crc, &b| crc16_step(crc, b))
}

fn crc16_step(crc: u16, b: u8) -> u16 {
    let mut c = crc ^ (u16::from(b) << 8);
    for _ in 0..8 {
        c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
    }
    c
}

pub fn id3_tag(body: usize, footer: bool) -> Vec<u8> {
    let size = |v: &mut Vec<u8>| v.extend((0..4).rev().map(|i| ((body >> (7 * i)) & 0x7F) as u8));
    let mut tag = b"ID3\x04\x00".to_vec();
    tag.push(if footer { 0x10 } else { 0 });
    size(&mut tag);
    // Filler that looks like frame syncs, which must not be searched.
    tag.extend((0..body).map(|i| if i % 2 == 0 { 0xFF } else { 0xF8 }));
    if footer {
        tag.extend_from_slice(b"3DI\x04\x00\x10");
        size(&mut tag);
    }
    tag
}

pub fn verbatim_stream(bits: u32, samples: &[i64]) -> Vec<u8> {
    let n = samples.len() as u64;
    let mut w = BitWriter::new();
    w.write_bits(0x664C_6143, 32);
    w.write_bits(0x8000_0022, 32);
    w.write_bits(n, 16);
    w.write_bits(n, 16);
    w.write_bits(0, 48);
    w.write_bits(44_100, 20);
    w.write_bits(0, 3);
    w.write_bits(u64::from(bits) - 1, 5);
    w.write_bits(n, 36);
    w.write_bits(0, 64);
    w.write_bits(0, 64);
    let mut out = w.into_bytes();
    let mut f = BitWriter::new();
    f.write_bits(0xFFF8, 16);
    f.write_bits(0b0111, 4);
    f.write_bits(0b0000, 4);
    f.write_bits(0b0000, 4);
    f.write_bits(0b000, 3);
    f.write_bits(0, 1);
    f.write_bits(0, 8);
    f.write_bits(n - 1, 16);
    let crc = crc8(f.as_aligned_bytes());
    f.write_bits(u64::from(crc), 8);
    f.write_bits(0b0000_0010, 8);
    for &s in samples {
        f.write_signed(s, bits);
    }
    f.align();
    let crc = crc16(f.as_aligned_bytes());
    f.write_bits(u64::from(crc), 16);
    out.extend_from_slice(&f.into_bytes());
    out
}
