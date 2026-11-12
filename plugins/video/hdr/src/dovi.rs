//! Dolby Vision configuration record and RPU header.

use crate::bits::{Bits, unescape_into};

/// The decoder configuration record, `Copy` and small enough for one format field.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct DoviConfig {
    pub major: u8,
    pub minor: u8,
    pub profile: u8,
    pub level: u8,
    pub rpu: bool,
    pub el: bool,
    pub bl: bool,
    /// 0 when the record is too short to say.
    pub compat_id: u8,
    pub compression: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoviError {
    Short {
        len: usize,
        need: usize,
    },
    NoPrefix(u8),
    Emdf(u32),
    RpuType(u8),
    Range(&'static str),
    /// The RPU ends inside a field, or before its 0x80.
    Cut,
    Crc,
    /// Syntax the reader does not walk.
    Unsupported(&'static str),
    /// The output is too small.
    Room,
}

impl std::fmt::Display for DoviError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DoviError::Short { len, need } => write!(f, "{len} bytes where {need} are needed"),
            DoviError::NoPrefix(b) => write!(f, "RPU prefix 0x{b:02x}, not 0x19"),
            DoviError::Emdf(v) => write!(f, "EMDF header 0x{v:07x}, not 0x01be6841"),
            DoviError::RpuType(t) => write!(f, "rpu_type {t}"),
            DoviError::Range(what) => write!(f, "{what} out of range"),
            DoviError::Cut => write!(f, "the RPU is cut short"),
            DoviError::Crc => write!(f, "the RPU's CRC does not match"),
            DoviError::Unsupported(what) => write!(f, "{what} is not read"),
            DoviError::Room => write!(f, "no room for the RPU"),
        }
    }
}

impl std::error::Error for DoviError {}

impl DoviConfig {
    /// From the 24 byte body of a `dvcC`, `dvvC` or `dvwC` box.
    pub fn from_record(body: &[u8]) -> Result<DoviConfig, DoviError> {
        let fixed = body.get(..4).ok_or(DoviError::Short { len: body.len(), need: 4 })?;
        let mut c = DoviConfig::fixed([fixed[0], fixed[1], fixed[2], fixed[3]]);
        if let Some(&b) = body.get(4) {
            c.tail(b);
        }
        Ok(c)
    }

    /// From a TS `DOVI_video_stream_descriptor` (tag 0xB0) body.
    pub fn from_ts_descriptor(body: &[u8]) -> Result<DoviConfig, DoviError> {
        let fixed = body.get(..4).ok_or(DoviError::Short { len: body.len(), need: 4 })?;
        let mut c = DoviConfig::fixed([fixed[0], fixed[1], fixed[2], fixed[3]]);
        let mut at = 4;
        if !c.bl && body.len() >= at + 2 {
            at += 2;
        }
        if let Some(&b) = body.get(at) {
            c.tail(b);
        }
        Ok(c)
    }

    /// The base layer PID of an enhancement layer TS stream.
    pub fn ts_dependency_pid(body: &[u8]) -> Option<u16> {
        let c = DoviConfig::from_ts_descriptor(body).ok()?;
        let d = body.get(4..6).filter(|_| !c.bl)?;
        Some(u16::from_be_bytes([d[0], d[1]]) >> 3)
    }

    fn fixed(b: [u8; 4]) -> DoviConfig {
        let bits = u16::from_be_bytes([b[2], b[3]]);
        DoviConfig {
            major: b[0],
            minor: b[1],
            profile: (bits >> 9 & 0x7F) as u8,
            level: (bits >> 3 & 0x3F) as u8,
            rpu: bits >> 2 & 1 == 1,
            el: bits >> 1 & 1 == 1,
            bl: bits & 1 == 1,
            compat_id: 0,
            compression: 0,
        }
    }

    fn tail(&mut self, b: u8) {
        self.compat_id = b >> 4 & 0x0F;
        self.compression = b >> 2 & 0x03;
    }

    /// The 24 byte record, reserved bits zero, as `ff_isom_put_dvcc_dvvc` writes it.
    pub fn to_record(&self) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[0] = self.major;
        out[1] = self.minor;
        let bits = u16::from(self.profile & 0x7F) << 9
            | u16::from(self.level & 0x3F) << 3
            | u16::from(self.rpu) << 2
            | u16::from(self.el) << 1
            | u16::from(self.bl);
        out[2..4].copy_from_slice(&bits.to_be_bytes());
        out[4] = (self.compat_id & 0x0F) << 4 | (self.compression & 0x03) << 2;
        out
    }

    /// `dvwC` past profile 10, `dvvC` past 7, else `dvcC`.
    pub fn box_name(&self) -> [u8; 4] {
        match self.profile {
            p if p > 10 => *b"dvwC",
            p if p > 7 => *b"dvvC",
            _ => *b"dvcC",
        }
    }

    /// Packed as major 0..8, minor 8..16, profile 16..23, level 23..29, flags 29..32,
    /// compatibility 32..36, compression 36..38.
    pub fn pack(self) -> i64 {
        (u64::from(self.major)
            | u64::from(self.minor) << 8
            | u64::from(self.profile & 0x7F) << 16
            | u64::from(self.level & 0x3F) << 23
            | u64::from(self.rpu) << 29
            | u64::from(self.el) << 30
            | u64::from(self.bl) << 31
            | u64::from(self.compat_id & 0x0F) << 32
            | u64::from(self.compression & 0x03) << 36) as i64
    }

    pub fn unpack(v: i64) -> DoviConfig {
        let v = v as u64;
        DoviConfig {
            major: v as u8,
            minor: (v >> 8) as u8,
            profile: (v >> 16 & 0x7F) as u8,
            level: (v >> 23 & 0x3F) as u8,
            rpu: v >> 29 & 1 == 1,
            el: v >> 30 & 1 == 1,
            bl: v >> 31 & 1 == 1,
            compat_id: (v >> 32 & 0x0F) as u8,
            compression: (v >> 36 & 0x03) as u8,
        }
    }

    /// The record of a single layer profile 8 stream, `width` by `height` pictures at
    /// `fps_milli`, 25 a second where that is 0, `compat_id` its base layer's. The last level
    /// past the table.
    pub fn profile8(width: u32, height: u32, fps_milli: u32, compat_id: u8) -> DoviConfig {
        let fps_milli = if fps_milli == 0 { 25_000 } else { fps_milli };
        let pps_milli = u64::from(width) * u64::from(height) * u64::from(fps_milli);
        let level = LEVELS
            .iter()
            .position(|&(pps, max_width)| pps_milli <= pps * 1000 && width <= max_width)
            .unwrap_or(LEVELS.len() - 1);
        DoviConfig {
            major: 1,
            minor: 0,
            profile: 8,
            level: level as u8 + 1,
            rpu: true,
            el: false,
            bl: true,
            compat_id,
            compression: 0,
        }
    }

    /// A record guessed from an RPU where the container states none.
    pub fn derived(rpu: &RpuHeader, el: bool) -> Option<DoviConfig> {
        Some(DoviConfig {
            major: 1,
            profile: rpu.guess_profile()?,
            rpu: true,
            el,
            bl: true,
            ..DoviConfig::default()
        })
    }
}

/// Levels 1 to 13, the pixels a second and the width each takes at most.
const LEVELS: [(u64, u32); 13] = [
    (1280 * 720 * 24, 1280),
    (1280 * 720 * 30, 1280),
    (1920 * 1080 * 24, 1920),
    (1920 * 1080 * 30, 2560),
    (1920 * 1080 * 60, 3840),
    (3840 * 2160 * 24, 3840),
    (3840 * 2160 * 30, 3840),
    (3840 * 2160 * 48, 3840),
    (3840 * 2160 * 60, 3840),
    (3840 * 2160 * 120, 3840),
    (3840 * 2160 * 120, 7680),
    (7680 * 4320 * 60, 7680),
    (7680 * 4320 * 120, 7680),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RpuForm {
    Nal,
    Av1Emdf,
}

/// The RPU header through the VDR sequence info.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct RpuHeader {
    pub rpu_type: u8,
    pub rpu_format: u16,
    pub vdr_rpu_profile: u8,
    pub vdr_rpu_level: u8,
    /// `None` where `vdr_seq_info_present_flag` is 0.
    pub seq: Option<RpuSeqInfo>,
}

/// `vdr_seq_info`, for `rpu_format & 0x700 == 0`, the one defined layout.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct RpuSeqInfo {
    pub chroma_resampling_explicit_filter: bool,
    /// 0 fixed point, 1 float.
    pub coef_data_type: u8,
    pub coef_log2_denom: u8,
    pub vdr_rpu_normalized_idc: u8,
    pub bl_video_full_range: bool,
    pub bl_bit_depth: u8,
    pub el_bit_depth: u8,
    pub vdr_bit_depth: u8,
    pub ext_mapping_idc: u8,
    pub spatial_resampling_filter: bool,
    pub dm_compression: u8,
    pub el_spatial_resampling_filter: bool,
    pub disable_residual: bool,
}

const HEAD: usize = 64;

impl RpuHeader {
    pub fn parse(form: RpuForm, bytes: &[u8]) -> Result<RpuHeader, DoviError> {
        let mut head = [0u8; HEAD];
        let n = match form {
            RpuForm::Nal => {
                let first = *bytes.first().ok_or(DoviError::Short { len: 0, need: 5 })?;
                if first != 0x19 {
                    return Err(DoviError::NoPrefix(first));
                }
                if bytes.len() < 5 {
                    return Err(DoviError::Short { len: bytes.len(), need: 5 });
                }
                unescape_into(&bytes[1..], &mut head)
            }
            RpuForm::Av1Emdf => unwrap_emdf(bytes, &mut head)?,
        };
        RpuHeader::read(&head[..n])
    }

    fn read(rpu: &[u8]) -> Result<RpuHeader, DoviError> {
        let short = DoviError::Short { len: rpu.len(), need: 4 };
        let mut b = Bits::new(rpu);
        let rpu_type = b.u(6).ok_or(short)? as u8;
        if rpu_type != 2 {
            return Err(DoviError::RpuType(rpu_type));
        }
        let mut h = RpuHeader { rpu_type, ..RpuHeader::default() };
        h.rpu_format = b.u(11).ok_or(short)? as u16;
        h.vdr_rpu_profile = b.u(4).ok_or(short)? as u8;
        h.vdr_rpu_level = b.u(4).ok_or(short)? as u8;
        if !b.flag().ok_or(short)? {
            return Ok(h);
        }
        let mut s = RpuSeqInfo {
            chroma_resampling_explicit_filter: b.flag().ok_or(short)?,
            ..RpuSeqInfo::default()
        };
        s.coef_data_type = b.u(2).ok_or(short)? as u8;
        s.coef_log2_denom = match s.coef_data_type {
            0 => {
                let d = b.ue().ok_or(short)?;
                if !(13..=32).contains(&d) {
                    return Err(DoviError::Range("coef_log2_denom"));
                }
                d as u8
            }
            1 => 32,
            _ => return Err(DoviError::Range("coef_data_type")),
        };
        s.vdr_rpu_normalized_idc = b.u(2).ok_or(short)? as u8;
        s.bl_video_full_range = b.flag().ok_or(short)?;
        if h.rpu_format & 0x700 == 0 {
            let bl = b.ue().ok_or(short)?;
            let el = b.ue().ok_or(short)?;
            let vdr = b.ue().ok_or(short)?;
            let (el, ext) = (el & 0xFF, el >> 8);
            if bl > 8 || el > 8 || vdr > 8 || ext > 0xFF {
                return Err(DoviError::Range("bit depth"));
            }
            (s.bl_bit_depth, s.el_bit_depth, s.vdr_bit_depth) =
                (bl as u8 + 8, el as u8 + 8, vdr as u8 + 8);
            s.ext_mapping_idc = ext as u8;
            s.spatial_resampling_filter = b.flag().ok_or(short)?;
            s.dm_compression = b.u(3).ok_or(short)? as u8;
            s.el_spatial_resampling_filter = b.flag().ok_or(short)?;
            s.disable_residual = b.flag().ok_or(short)?;
        }
        h.seq = Some(s);
        Ok(h)
    }

    /// The profile `ff_dovi_guess_profile_hevc` derives.
    pub fn guess_profile(&self) -> Option<u8> {
        let s = self.seq?;
        match self.vdr_rpu_profile {
            0 if s.bl_video_full_range => Some(5),
            1 if s.el_spatial_resampling_filter && !s.disable_residual => {
                Some(if s.vdr_bit_depth == 12 { 7 } else { 4 })
            }
            1 => Some(8),
            _ => None,
        }
    }
}

const EMDF_MAGIC: u32 = 0x01be_6841;

/// The RPU inside AV1's EMDF wrapping onto `out`, as much as fits, its length.
pub(crate) fn unwrap_emdf(bytes: &[u8], out: &mut [u8]) -> Result<usize, DoviError> {
    let mut b = Bits::new(bytes);
    let short = DoviError::Short { len: bytes.len(), need: 6 };
    let magic = b.u(27).ok_or(short)?;
    if magic != EMDF_MAGIC {
        return Err(DoviError::Emdf(magic));
    }
    let mut size = b.u(8).ok_or(short)?;
    while b.flag().ok_or(short)? {
        size = (size + 1) << 8 | b.u(8).ok_or(short)?;
        if size > 512 {
            return Err(DoviError::Range("emdf_payload_size"));
        }
    }
    if !(6..=512).contains(&size) {
        return Err(DoviError::Range("emdf_payload_size"));
    }
    if (size as usize) * 8 > b.left() {
        return Err(DoviError::Short { len: bytes.len(), need: size as usize });
    }
    let n = (size as usize).min(out.len());
    for slot in &mut out[..n] {
        *slot = b.u(8).ok_or(short)? as u8;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;

    fn record_8_1() -> [u8; 24] {
        let mut r = [0u8; 24];
        r[0] = 1;
        let bits: u16 = 8 << 9 | 6 << 3 | 1 << 2 | 1;
        r[2..4].copy_from_slice(&bits.to_be_bytes());
        r[4] = 1 << 4;
        r
    }

    #[test]
    fn a_dvvc_record_reads_every_field() {
        let c = DoviConfig::from_record(&record_8_1()).unwrap();
        assert_eq!(
            c,
            DoviConfig {
                major: 1,
                minor: 0,
                profile: 8,
                level: 6,
                rpu: true,
                el: false,
                bl: true,
                compat_id: 1,
                compression: 0
            }
        );
        assert_eq!(c.box_name(), *b"dvvC");
        assert_eq!(c.to_record(), record_8_1());
    }

    #[test]
    fn each_field_comes_from_its_own_bits() {
        let base = DoviConfig::from_record(&record_8_1()).unwrap();
        let flip = |byte: usize, mask: u8| {
            let mut r = record_8_1();
            r[byte] ^= mask;
            DoviConfig::from_record(&r).unwrap()
        };
        assert_eq!(flip(0, 0x02).major, 3);
        assert_eq!(flip(1, 0x01).minor, 1);
        assert_eq!(flip(2, 0x80).profile, 8 | 64);
        assert_eq!(flip(2, 0x02).profile, 9);
        assert_eq!(flip(2, 0x01).level, 6 | 32);
        assert_eq!(flip(3, 0x08).level, 7);
        assert!(!flip(3, 0x04).rpu);
        assert!(flip(3, 0x02).el);
        assert!(!flip(3, 0x01).bl);
        assert_eq!(flip(4, 0x80).compat_id, 9);
        assert_eq!(flip(4, 0x04).compression, 1);
        assert_eq!(flip(4, 0x08).compression, 2);
        let same = flip(5, 0xFF);
        assert_eq!(same, base, "reserved bytes are not read");
    }

    #[test]
    fn a_short_record_takes_what_it_has() {
        let c = DoviConfig::from_record(&record_8_1()[..4]).unwrap();
        assert_eq!((c.profile, c.compat_id), (8, 0));
        assert_eq!(DoviConfig::from_record(&[1, 0, 0]), Err(DoviError::Short { len: 3, need: 4 }));
        assert_eq!(DoviConfig { profile: 5, ..c }.box_name(), *b"dvcC");
        assert_eq!(DoviConfig { profile: 20, ..c }.box_name(), *b"dvwC");
    }

    #[test]
    fn the_ts_descriptor_skips_the_dependency_pid_of_an_el_stream() {
        let bits: u16 = 7 << 9 | 6 << 3 | 1 << 2 | 1 << 1;
        let mut d = vec![1, 0];
        d.extend_from_slice(&bits.to_be_bytes());
        d.extend_from_slice(&((0x100u16 << 3) | 0x7).to_be_bytes());
        d.push(6 << 4);
        let c = DoviConfig::from_ts_descriptor(&d).unwrap();
        assert_eq!((c.profile, c.el, c.bl, c.compat_id), (7, true, false, 6));
        assert_eq!(DoviConfig::ts_dependency_pid(&d), Some(0x100));
        let bl: u16 = 8 << 9 | 6 << 3 | 1 << 2 | 1;
        let mut d = vec![1, 0];
        d.extend_from_slice(&bl.to_be_bytes());
        d.push(4 << 4);
        let c = DoviConfig::from_ts_descriptor(&d).unwrap();
        assert_eq!((c.profile, c.compat_id), (8, 4));
        assert_eq!(DoviConfig::ts_dependency_pid(&d), None);
    }

    #[test]
    fn pack_round_trips_and_keeps_fields_apart() {
        let c = DoviConfig {
            major: 2,
            minor: 1,
            profile: 127,
            level: 63,
            rpu: true,
            el: true,
            bl: true,
            compat_id: 15,
            compression: 3,
        };
        assert_eq!(DoviConfig::unpack(c.pack()), c);
        for one in [
            DoviConfig { major: 1, ..DoviConfig::default() },
            DoviConfig { profile: 1, ..DoviConfig::default() },
            DoviConfig { level: 1, ..DoviConfig::default() },
            DoviConfig { el: true, ..DoviConfig::default() },
            DoviConfig { compat_id: 1, ..DoviConfig::default() },
            DoviConfig { compression: 1, ..DoviConfig::default() },
        ] {
            assert_eq!(DoviConfig::unpack(one.pack()), one);
        }
        assert!(c.pack() > 0, "fits below the sign bit");
    }

    fn rpu_bits(
        vdr_profile: u32,
        full_range: bool,
        el_filter: bool,
        disable_residual: bool,
        vdr_minus8: u32,
    ) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(2, 6);
        w.put(18, 11);
        w.put(vdr_profile, 4);
        w.put(0, 4);
        w.put(1, 1); // vdr_seq_info_present_flag
        w.put(0, 1); // chroma_resampling_explicit_filter_flag
        w.put(0, 2); // coef_data_type fixed
        w.ue(23); // coef_log2_denom
        w.put(1, 2); // vdr_rpu_normalized_idc
        w.put(u32::from(full_range), 1);
        w.ue(2); // bl_bit_depth_minus8
        w.ue(2); // el_bit_depth_minus8
        w.ue(vdr_minus8);
        w.put(0, 1); // spatial_resampling_filter_flag
        w.put(0, 3); // dm_compression
        w.put(u32::from(el_filter), 1);
        w.put(u32::from(disable_residual), 1);
        w.put(0, 16);
        w.into_bytes()
    }

    #[test]
    fn an_rpu_nal_header_reads_and_guesses_its_profile() {
        let mut nal = vec![0x19];
        nal.extend(rpu_bits(1, false, true, false, 4));
        let h = RpuHeader::parse(RpuForm::Nal, &nal).unwrap();
        assert_eq!((h.rpu_type, h.rpu_format, h.vdr_rpu_profile, h.vdr_rpu_level), (2, 18, 1, 0));
        let s = h.seq.unwrap();
        assert_eq!(
            (s.coef_log2_denom, s.bl_bit_depth, s.el_bit_depth, s.vdr_bit_depth),
            (23, 10, 10, 12)
        );
        assert_eq!(h.guess_profile(), Some(7));
        let mut nal = vec![0x19];
        nal.extend(rpu_bits(1, false, false, true, 4));
        assert_eq!(RpuHeader::parse(RpuForm::Nal, &nal).unwrap().guess_profile(), Some(8));
        let mut nal = vec![0x19];
        nal.extend(rpu_bits(0, true, false, true, 4));
        assert_eq!(RpuHeader::parse(RpuForm::Nal, &nal).unwrap().guess_profile(), Some(5));
        let mut nal = vec![0x19];
        nal.extend(rpu_bits(1, false, true, false, 2));
        assert_eq!(RpuHeader::parse(RpuForm::Nal, &nal).unwrap().guess_profile(), Some(4));
        let d = DoviConfig::derived(&RpuHeader::parse(RpuForm::Nal, &nal).unwrap(), false).unwrap();
        assert_eq!((d.profile, d.rpu, d.bl, d.el), (4, true, true, false));
    }

    #[test]
    fn a_profile_8_record_takes_the_level_its_pictures_need() {
        let level = |w, h, fps| DoviConfig::profile8(w, h, fps, 1).level;
        assert_eq!(level(1280, 720, 23_976), 1);
        assert_eq!(level(1280, 720, 30_000), 2);
        assert_eq!(level(1920, 1080, 23_976), 3);
        assert_eq!(level(1920, 804, 60_000), 5);
        assert_eq!(level(2560, 720, 24_000), 4, "past level 3's width");
        assert_eq!(level(3840, 2160, 24_000), 6);
        assert_eq!(level(3840, 2160, 60_000), 9);
        assert_eq!(level(3840, 2160, 0), 7, "25 a second where the rate is unknown");
        assert_eq!(level(7680, 4320, 240_000), 13, "the last past the table");
        let c = DoviConfig::profile8(1920, 1080, 24_000, 4);
        assert_eq!((c.profile, c.rpu, c.el, c.bl, c.compat_id), (8, true, false, true, 4));
        assert_eq!(c.box_name(), *b"dvvC");
    }

    #[test]
    fn a_wrong_prefix_or_type_is_refused() {
        let body = rpu_bits(1, false, false, true, 4);
        assert_eq!(RpuHeader::parse(RpuForm::Nal, &body), Err(DoviError::NoPrefix(body[0])));
        let mut nal = vec![0x19];
        nal.extend(rpu_bits(1, false, false, true, 4));
        nal[1] = (nal[1] & 0x03) | 3 << 2;
        assert_eq!(RpuHeader::parse(RpuForm::Nal, &nal), Err(DoviError::RpuType(3)));
    }

    #[test]
    fn an_av1_emdf_rpu_is_unwrapped_off_its_bit() {
        let rpu = rpu_bits(1, false, false, true, 4);
        let mut w = BitWriter::default();
        w.put(EMDF_MAGIC, 27);
        w.put(rpu.len() as u32, 8);
        w.put(0, 1); // read_more
        for b in &rpu {
            w.put(u32::from(*b), 8);
        }
        w.put(0x400, 17); // emdf protection
        let h = RpuHeader::parse(RpuForm::Av1Emdf, &w.into_bytes()).unwrap();
        assert_eq!(h.guess_profile(), Some(8));
        assert!(matches!(RpuHeader::parse(RpuForm::Av1Emdf, &[0xFF; 16]), Err(DoviError::Emdf(_))));
    }
}
