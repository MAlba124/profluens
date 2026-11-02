//! Codec configuration records (avcC, hvcC, av1C) built from the stream a decoder takes, for a
//! muxer. Untrusted bytes, bounds checked.

use std::fmt;

use profluens_core::ctx::Ctx;
use profluens_core::memory::Memory;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    NoSps,
    NoPps,
    NoVps,
    ShortSps,
    SetTooLarge { len: usize },
    TooManySets,
    NoSequenceHeader,
    BadObu,
    ShortSequenceHeader,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSps => f.write_str("no sequence parameter set ahead of the first picture"),
            Self::NoPps => f.write_str("no picture parameter set ahead of the first picture"),
            Self::NoVps => f.write_str("no video parameter set ahead of the first picture"),
            Self::ShortSps => f.write_str("a sequence parameter set ends early"),
            Self::SetTooLarge { len } => write!(f, "a parameter set of {len} bytes, past 65535"),
            Self::TooManySets => write!(f, "more than {MAX_SETS} parameter sets of one kind"),
            Self::NoSequenceHeader => {
                f.write_str("the first temporal unit has no sequence header OBU")
            }
            Self::BadObu => f.write_str("an OBU runs past its temporal unit"),
            Self::ShortSequenceHeader => f.write_str("the sequence header OBU ends early"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalCodec {
    H264,
    H265,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalKind {
    Vps,
    Sps,
    Pps,
    Vcl,
    Other,
}

impl NalCodec {
    pub fn kind(self, b0: u8) -> NalKind {
        match self {
            Self::H264 => match b0 & 0x1F {
                1..=5 => NalKind::Vcl,
                7 => NalKind::Sps,
                8 => NalKind::Pps,
                _ => NalKind::Other,
            },
            Self::H265 => match (b0 >> 1) & 0x3F {
                0..=31 => NalKind::Vcl,
                32 => NalKind::Vps,
                33 => NalKind::Sps,
                34 => NalKind::Pps,
                _ => NalKind::Other,
            },
        }
    }
}

fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(if i > from && data[i - 1] == 0 { (i - 1, 4) } else { (i, 3) });
        } else {
            i += 1;
        }
    }
    None
}

pub struct Nals<'a> {
    data: &'a [u8],
    next: Option<usize>,
}

pub fn nals(data: &[u8]) -> Nals<'_> {
    Nals { data, next: start_code(data, 0).map(|(at, len)| at + len) }
}

impl Iterator for Nals<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        loop {
            let begin = self.next?;
            let (mut end, after) = match start_code(self.data, begin) {
                Some((at, len)) => (at, Some(at + len)),
                None => (self.data.len(), None),
            };
            self.next = after;
            while end > begin && self.data[end - 1] == 0 {
                end -= 1;
            }
            if end > begin {
                return Some((begin, end));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub vcl: bool,
    pub out_len: usize,
    /// Every unit sits behind a four byte start code, so lengths overwrite them in place.
    pub in_place: bool,
}

pub fn scan(codec: NalCodec, data: &[u8]) -> Scan {
    let mut s = Scan { vcl: false, out_len: 0, in_place: true };
    let mut prev_end = 0usize;
    for (begin, end) in nals(data) {
        s.vcl |= codec.kind(data[begin]) == NalKind::Vcl;
        s.out_len += 4 + (end - begin);
        s.in_place &= begin == prev_end + 4;
        prev_end = end;
    }
    s.in_place &= prev_end == data.len() && s.out_len == data.len();
    s
}

pub fn to_length_prefixed_in_place(data: &mut [u8]) {
    let mut at = 0usize;
    while at + 4 <= data.len() {
        // Read before the write, the next start code is found from the unit's start.
        let next = start_code(data, at + 4).map_or(data.len(), |(n, _)| n);
        let len = (next - at - 4) as u32;
        data[at..at + 4].copy_from_slice(&len.to_be_bytes());
        at = next;
    }
}

pub fn to_length_prefixed(data: &[u8], out: &mut [u8]) -> usize {
    let mut w = 0usize;
    for (begin, end) in nals(data) {
        let n = end - begin;
        if w + 4 + n > out.len() {
            break;
        }
        out[w..w + 4].copy_from_slice(&(n as u32).to_be_bytes());
        out[w + 4..w + 4 + n].copy_from_slice(&data[begin..end]);
        w += 4 + n;
    }
    w
}

/// An Annex B access unit as a length-prefixed sample, `None` for a unit with no picture or one
/// past the exact cap.
pub fn length_prefixed_sample(ctx: &mut Ctx, codec: NalCodec, mut mem: Memory) -> Option<Memory> {
    let s = scan(codec, mem.data());
    if !s.vcl {
        return None;
    }
    if s.in_place {
        let n = mem.len();
        to_length_prefixed_in_place(&mut mem.as_mut_full()[..n]);
        return Some(mem);
    }
    let mut out = ctx.alloc_exact(s.out_len)?;
    let n = to_length_prefixed(mem.data(), out.memory.as_mut_full());
    out.memory.set_len(n);
    Some(out.memory)
}

fn is_param_set(codec: NalCodec, b0: u8) -> bool {
    matches!(codec.kind(b0), NalKind::Vps | NalKind::Sps | NalKind::Pps)
}

/// The parameter sets of an Annex B access unit onto `out`, each behind a four byte start code,
/// the head a decoder opens with. Trailing zero bytes are left off, so the same sets give the
/// same head whatever start code followed them.
pub fn annexb_param_sets(codec: NalCodec, au: &[u8], out: &mut Vec<u8>) {
    for (a, b) in nals(au) {
        if is_param_set(codec, au[a]) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&au[a..b]);
        }
    }
}

/// An Annex B access unit onto `out` without its parameter sets, each unit behind a four byte
/// start code. False with `out` untouched when it carries none.
pub fn strip_param_sets(codec: NalCodec, au: &[u8], out: &mut Vec<u8>) -> bool {
    if !nals(au).any(|(a, _)| is_param_set(codec, au[a])) {
        return false;
    }
    out.reserve(au.len());
    for (a, b) in nals(au) {
        if !is_param_set(codec, au[a]) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&au[a..b]);
        }
    }
    true
}

/// The highest TemporalId of an HEVC stream, sps_max_sub_layers_minus1 of the first SPS in the
/// Annex B `head` (7.3.2.2.1). The field sits ahead of any emulation prevention byte.
pub fn hevc_top_layer(head: &[u8]) -> Option<u8> {
    let (a, b) = nals(head).find(|&(a, _)| NalCodec::H265.kind(head[a]) == NalKind::Sps)?;
    head[a..b].get(2).map(|v| (v >> 1) & 7)
}

/// Whether every picture of an Annex B access unit is one nothing refers to, H.264 slices with
/// nal_ref_idc 0 and HEVC sub-layer non-reference pictures of the highest sub-layer. A decoder
/// seeking past such a picture may drop it undecoded. `head` holds the stream's parameter sets,
/// an HEVC unit is never judged so without an SPS there.
pub fn unreferenced(codec: NalCodec, head: &[u8], au: &[u8]) -> bool {
    let top = match codec {
        NalCodec::H264 => 0,
        NalCodec::H265 => match hevc_top_layer(head) {
            Some(t) => t,
            None => return false,
        },
    };
    let mut vcl = false;
    for (a, b) in nals(au) {
        let n = &au[a..b];
        if codec.kind(n[0]) != NalKind::Vcl {
            continue;
        }
        let free = match codec {
            NalCodec::H264 => n[0] & 0x60 == 0,
            // The _N types are the even ones up to RSV_VCL_N14 (7.4.2.2).
            NalCodec::H265 => {
                let t = (n[0] >> 1) & 0x3F;
                n.get(1).is_some_and(|h| t <= 14 && t & 1 == 0 && (h & 7).saturating_sub(1) == top)
            }
        };
        if !free {
            return false;
        }
        vcl = true;
    }
    vcl
}

pub fn av1_sample(mem: Memory) -> Option<Memory> {
    let td = leading_temporal_delimiter(mem.data());
    match td {
        0 => Some(mem),
        n if n < mem.len() => mem.slice(n, mem.len() - n),
        _ => None,
    }
}

pub const MAX_SETS: usize = 16;

#[derive(Clone, Debug)]
pub struct ParamSets {
    codec: NalCodec,
    bytes: Vec<u8>,
    spans: Vec<(NalKind, usize, usize)>,
}

impl ParamSets {
    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    pub fn new(codec: NalCodec) -> Self {
        Self { codec, bytes: Vec::new(), spans: Vec::new() }
    }

    pub fn codec(&self) -> NalCodec {
        self.codec
    }

    fn sets(&self, kind: NalKind) -> impl Iterator<Item = &[u8]> {
        self.spans.iter().filter(move |s| s.0 == kind).map(|&(_, a, b)| &self.bytes[a..b])
    }

    pub fn absorb(&mut self, data: &[u8]) -> Result<(), ConfigError> {
        for (begin, end) in nals(data) {
            let nal = &data[begin..end];
            let kind = self.codec.kind(nal[0]);
            if !matches!(kind, NalKind::Vps | NalKind::Sps | NalKind::Pps) {
                continue;
            }
            if nal.len() > u16::MAX as usize {
                return Err(ConfigError::SetTooLarge { len: nal.len() });
            }
            if self.sets(kind).any(|s| s == nal) {
                continue;
            }
            if self.sets(kind).count() >= MAX_SETS {
                return Err(ConfigError::TooManySets);
            }
            let at = self.bytes.len();
            self.bytes.extend_from_slice(nal);
            self.spans.push((kind, at, self.bytes.len()));
        }
        Ok(())
    }

    pub fn record(&self) -> Result<Vec<u8>, ConfigError> {
        match self.codec {
            NalCodec::H264 => self.avcc(),
            NalCodec::H265 => self.hvcc(),
        }
    }

    /// The coded size from the first SPS. H.264's would take a full SPS parse, so it is `None`.
    pub fn size(&self) -> Option<(u32, u32)> {
        match self.codec {
            NalCodec::H265 => {
                let sps = hevc_sps(self.sets(NalKind::Sps).next()?).ok()?;
                Some((sps.width, sps.height))
            }
            NalCodec::H264 => None,
        }
    }

    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    fn avcc(&self) -> Result<Vec<u8>, ConfigError> {
        let sps = self.sets(NalKind::Sps).next().ok_or(ConfigError::NoSps)?;
        if self.sets(NalKind::Pps).next().is_none() {
            return Err(ConfigError::NoPps);
        }
        let info = avc_sps(sps)?;
        let mut out = Vec::with_capacity(16 + self.bytes.len());
        out.extend_from_slice(&[1, info.profile, info.compat, info.level, 0xFF]);
        out.push(0xE0 | self.sets(NalKind::Sps).count() as u8);
        for s in self.sets(NalKind::Sps) {
            out.extend_from_slice(&(s.len() as u16).to_be_bytes());
            out.extend_from_slice(s);
        }
        out.push(self.sets(NalKind::Pps).count() as u8);
        for s in self.sets(NalKind::Pps) {
            out.extend_from_slice(&(s.len() as u16).to_be_bytes());
            out.extend_from_slice(s);
        }
        // Fields past the PPS for profiles other than Baseline, Main or Extended, as ffmpeg writes
        // them (section 5.3.3.1.2).
        if !matches!(info.profile, 66 | 77 | 88) {
            out.push(0xFC | info.chroma);
            out.push(0xF8 | info.luma_bits);
            out.push(0xF8 | info.chroma_bits);
            out.push(0);
        }
        Ok(out)
    }

    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    fn hvcc(&self) -> Result<Vec<u8>, ConfigError> {
        let sps = self.sets(NalKind::Sps).next().ok_or(ConfigError::NoSps)?;
        if self.sets(NalKind::Vps).next().is_none() {
            return Err(ConfigError::NoVps);
        }
        if self.sets(NalKind::Pps).next().is_none() {
            return Err(ConfigError::NoPps);
        }
        let h = hevc_sps(sps)?;
        let mut out = Vec::with_capacity(32 + self.bytes.len());
        out.push(1);
        out.push(h.ptl[0]);
        out.extend_from_slice(&h.ptl[1..12]);
        out.extend_from_slice(&[0xF0, 0x00, 0xFC]);
        out.push(0xFC | h.chroma);
        out.push(0xF8 | h.luma_bits);
        out.push(0xF8 | h.chroma_bits);
        out.extend_from_slice(&[0, 0]);
        out.push((h.layers << 3) | (u8::from(h.nested) << 2) | 3);
        out.push(3);
        for (kind, t) in [(NalKind::Vps, 32u8), (NalKind::Sps, 33), (NalKind::Pps, 34)] {
            // array_completeness 0, the sets may come in band too (`hev1`).
            out.push(t);
            out.extend_from_slice(&(self.sets(kind).count() as u16).to_be_bytes());
            for s in self.sets(kind) {
                out.extend_from_slice(&(s.len() as u16).to_be_bytes());
                out.extend_from_slice(s);
            }
        }
        Ok(out)
    }
}

/// Payload bytes with emulation prevention removed (H.264 section 7.4.1).
fn unescape(nal: &[u8], out: &mut [u8]) -> usize {
    let (mut w, mut zeros) = (0usize, 0u32);
    for &b in nal {
        if w == out.len() {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out[w] = b;
        w += 1;
    }
    w
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let b = *self.data.get(self.at / 8)?;
        let v = (b >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(v))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.at = self.at.checked_add(n)?;
        (self.at <= self.data.len() * 8).then_some(())
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcSps {
    pub profile: u8,
    pub compat: u8,
    pub level: u8,
    pub chroma: u8,
    pub luma_bits: u8,
    pub chroma_bits: u8,
}

pub fn avc_sps(nal: &[u8]) -> Result<AvcSps, ConfigError> {
    let mut raw = [0u8; 64];
    let n = unescape(nal, &mut raw);
    let f = || -> Option<AvcSps> {
        let mut r = Bits::new(&raw[..n]);
        r.skip(8)?;
        let profile = r.bits(8)? as u8;
        let compat = r.bits(8)? as u8;
        let level = r.bits(8)? as u8;
        r.ue()?;
        let (mut chroma, mut luma_bits, mut chroma_bits) = (1u8, 0u8, 0u8);
        if matches!(
            profile,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            chroma = r.ue()?.min(3) as u8;
            if chroma == 3 {
                r.bit()?;
            }
            luma_bits = r.ue()?.min(7) as u8;
            chroma_bits = r.ue()?.min(7) as u8;
        }
        Some(AvcSps { profile, compat, level, chroma, luma_bits, chroma_bits })
    };
    f().ok_or(ConfigError::ShortSps)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcSps {
    pub ptl: [u8; 12],
    pub chroma: u8,
    pub luma_bits: u8,
    pub chroma_bits: u8,
    pub layers: u8,
    pub nested: bool,
    pub width: u32,
    pub height: u32,
}

pub fn hevc_sps(nal: &[u8]) -> Result<HevcSps, ConfigError> {
    let mut raw = [0u8; 256];
    let n = unescape(nal, &mut raw);
    let f = || -> Option<HevcSps> {
        let mut r = Bits::new(&raw[..n]);
        r.skip(16)?;
        r.bits(4)?;
        let sub = r.bits(3)? as usize;
        let nested = r.bit()? == 1;
        let mut ptl = [0u8; 12];
        for b in ptl.iter_mut() {
            *b = r.bits(8)? as u8;
        }
        let mut present = [(false, false); 8];
        for p in present.iter_mut().take(sub) {
            *p = (r.bit()? == 1, r.bit()? == 1);
        }
        if sub > 0 {
            r.skip(2 * (8 - sub))?;
        }
        for &(profile, level) in present.iter().take(sub) {
            r.skip(if profile { 88 } else { 0 } + if level { 8 } else { 0 })?;
        }
        r.ue()?;
        let chroma = r.ue()?.min(3) as u8;
        if chroma == 3 {
            r.bit()?;
        }
        let width = r.ue()?;
        let height = r.ue()?;
        if r.bit()? == 1 {
            for _ in 0..4 {
                r.ue()?;
            }
        }
        let luma_bits = r.ue()?.min(7) as u8;
        let chroma_bits = r.ue()?.min(7) as u8;
        Some(HevcSps {
            ptl,
            chroma,
            luma_bits,
            chroma_bits,
            layers: (sub as u8 + 1).min(7), // numTemporalLayers is 3 bits
            nested,
            width,
            height,
        })
    };
    f().ok_or(ConfigError::ShortSps)
}

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;

fn obu_at(data: &[u8], at: usize) -> Result<(u8, usize, usize, usize), ConfigError> {
    let h = *data.get(at).ok_or(ConfigError::BadObu)?;
    let kind = (h >> 3) & 0x0F;
    let ext = h & 0x04 != 0;
    let has_size = h & 0x02 != 0;
    let mut p = at + 1 + usize::from(ext);
    let size = if has_size {
        let mut v = 0u64;
        let mut i = 0;
        loop {
            let b = *data.get(p).ok_or(ConfigError::BadObu)?;
            p += 1;
            v |= u64::from(b & 0x7F) << (7 * i);
            i += 1;
            if b & 0x80 == 0 {
                break;
            }
            if i == 8 {
                return Err(ConfigError::BadObu);
            }
        }
        usize::try_from(v).map_err(|_| ConfigError::BadObu)?
    } else {
        data.len().checked_sub(p).ok_or(ConfigError::BadObu)?
    };
    let end = p.checked_add(size).filter(|&e| e <= data.len()).ok_or(ConfigError::BadObu)?;
    Ok((kind, at, p, end))
}

pub fn leading_temporal_delimiter(data: &[u8]) -> usize {
    match obu_at(data, 0) {
        Ok((OBU_TEMPORAL_DELIMITER, _, _, end)) => end,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1Seq {
    pub profile: u8,
    pub level: u8,
    pub tier: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub mono: bool,
    pub sub_x: bool,
    pub sub_y: bool,
    pub chroma_position: u8,
    pub width: u32,
    pub height: u32,
}

pub fn av1_sequence_header(p: &[u8]) -> Result<Av1Seq, ConfigError> {
    let f = || -> Option<Av1Seq> {
        let mut r = Bits::new(p);
        let profile = r.bits(3)? as u8;
        r.bit()?; // still_picture
        let reduced = r.bit()? == 1;
        let (mut level, mut tier) = (0u8, 0u8);
        let mut timing = false;
        let mut decoder_model = false;
        let mut delay_len = 0u32;
        if reduced {
            level = r.bits(5)? as u8;
        } else {
            timing = r.bit()? == 1;
            if timing {
                r.skip(64)?;
                if r.bit()? == 1 {
                    let mut zeros = 0u32;
                    while r.bit()? == 0 {
                        zeros += 1;
                        if zeros >= 32 {
                            return None;
                        }
                    }
                    r.skip(zeros as usize)?;
                }
                decoder_model = r.bit()? == 1;
                if decoder_model {
                    delay_len = r.bits(5)? + 1;
                    r.skip(32 + 5 + 5)?;
                }
            }
            let initial_display = r.bit()? == 1;
            let points = r.bits(5)? + 1;
            for i in 0..points {
                r.skip(12)?;
                let l = r.bits(5)? as u8;
                let t = if l > 7 { r.bit()? as u8 } else { 0 };
                if i == 0 {
                    (level, tier) = (l, t);
                }
                if decoder_model && r.bit()? == 1 {
                    r.skip(2 * delay_len as usize + 1)?;
                }
                if initial_display && r.bit()? == 1 {
                    r.skip(4)?;
                }
            }
        }
        let wbits = r.bits(4)? + 1;
        let hbits = r.bits(4)? + 1;
        let width = r.bits(wbits)? + 1;
        let height = r.bits(hbits)? + 1;
        if !reduced && r.bit()? == 1 {
            r.skip(4 + 3)?;
        }
        r.skip(3)?; // 128x128 superblock, filter intra, intra edge filter
        if !reduced {
            r.skip(4)?; // interintra, masked compound, warped motion, dual filter
            let order_hint = r.bit()? == 1;
            if order_hint {
                r.skip(2)?;
            }
            let force_screen = if r.bit()? == 1 { 2 } else { r.bit()? };
            if force_screen > 0 && r.bit()? == 0 {
                r.bit()?;
            }
            if order_hint {
                r.skip(3)?;
            }
        }
        r.skip(3)?; // superres, cdef, restoration
        let high_bitdepth = r.bit()? == 1;
        let twelve_bit = profile == 2 && high_bitdepth && r.bit()? == 1;
        let mono = profile != 1 && r.bit()? == 1;
        let (mut cp, mut tc, mut mc) = (2u32, 2u32, 2u32);
        if r.bit()? == 1 {
            cp = r.bits(8)?;
            tc = r.bits(8)?;
            mc = r.bits(8)?;
        }
        let (mut sub_x, mut sub_y, mut chroma_position) = (true, true, 0u8);
        if mono {
            r.bit()?;
        } else if cp == 1 && tc == 13 && mc == 0 {
            (sub_x, sub_y) = (false, false);
        } else {
            r.bit()?; // color_range
            match profile {
                0 => {}
                1 => (sub_x, sub_y) = (false, false),
                _ if twelve_bit => {
                    sub_x = r.bit()? == 1;
                    sub_y = sub_x && r.bit()? == 1;
                }
                _ => (sub_x, sub_y) = (true, false),
            }
            if sub_x && sub_y {
                chroma_position = r.bits(2)? as u8;
            }
        }
        let _ = timing;
        Some(Av1Seq {
            profile,
            level,
            tier,
            high_bitdepth,
            twelve_bit,
            mono,
            sub_x,
            sub_y,
            chroma_position,
            width,
            height,
        })
    };
    f().ok_or(ConfigError::ShortSequenceHeader)
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
pub fn av1c(tu: &[u8]) -> Result<(Vec<u8>, Av1Seq), ConfigError> {
    let mut at = 0usize;
    while at < tu.len() {
        let (kind, start, payload, end) = obu_at(tu, at)?;
        if kind == OBU_SEQUENCE_HEADER {
            let s = av1_sequence_header(&tu[payload..end])?;
            let mut out = Vec::with_capacity(4 + end - start);
            out.push(0x81);
            out.push((s.profile << 5) | (s.level & 0x1F));
            out.push(
                (s.tier << 7)
                    | (u8::from(s.high_bitdepth) << 6)
                    | (u8::from(s.twelve_bit) << 5)
                    | (u8::from(s.mono) << 4)
                    | (u8::from(s.sub_x) << 3)
                    | (u8::from(s.sub_y) << 2)
                    | s.chroma_position,
            );
            out.push(0);
            // The OBU with its size field (section 2.3.3).
            out.extend_from_slice(&tu[start..end]);
            return Ok((out, s));
        }
        at = end;
    }
    Err(ConfigError::NoSequenceHeader)
}

#[cfg(test)]
mod tests;
