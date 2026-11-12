use crate::bits::{BitWriter, Bits, unescape_into};
use crate::dovi::{DoviError, RpuForm, unwrap_emdf};
use crate::side::SIDE_CAP;

/// How the picture an RPU was written for lies in the one it goes out with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fit {
    /// The source picture as coded.
    pub from: (u32, u32),
    /// Quarter turns clockwise from the source to the output.
    pub turns: u8,
    pub to: (u32, u32),
}

const AREA_MAX: u32 = (1 << 13) - 1;

impl Fit {
    /// The source's active area offsets left, right, top and bottom as the output's.
    pub fn area(&self, [l, r, t, b]: [u32; 4]) -> [u32; 4] {
        let ([l, r, t, b], (w, h)) = match self.turns % 4 {
            0 => ([l, r, t, b], self.from),
            1 => ([b, t, l, r], (self.from.1, self.from.0)),
            2 => ([r, l, b, t], self.from),
            _ => ([t, b, r, l], (self.from.1, self.from.0)),
        };
        let scale = |v: u32, to: u32, from: u32| {
            if from == 0 || from == to {
                return v;
            }
            let fitted = (u64::from(v) * u64::from(to) + u64::from(from) / 2) / u64::from(from);
            fitted.min(u64::from(AREA_MAX)) as u32
        };
        [
            scale(l, self.to.0, w),
            scale(r, self.to.0, w),
            scale(t, self.to.1, h),
            scale(b, self.to.1, h),
        ]
    }
}

const CRC: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000_0000 != 0 { c << 1 ^ 0x04C1_1DB7 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

/// CRC-32/MPEG-2, 0 over an RPU's bytes with the CRC they end in.
pub fn crc32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(u32::MAX, |c, &b| c << 8 ^ CRC[(c >> 24 ^ u32::from(b)) as usize])
}

// BT.2020 PQ YCbCr to RGB and RGB to LMS, what a profile 8.1 RPU states
const YCC_TO_RGB: [i16; 9] = [9574, 0, 13802, 9574, -1540, -5348, 9574, 17610, 0];
const YCC_OFFSET: [u32; 3] = [16_777_216, 134_217_728, 134_217_728];
const RGB_TO_LMS: [i16; 9] = [7222, 8771, 390, 2654, 12430, 1300, 0, 422, 15962];

const PIECES_MAX: u32 = 8;
const LEVEL_ACTIVE_AREA: u32 = 5;
// padding, CRC and the 0x80
const TAIL_BITS: usize = 48;

#[derive(Clone, Copy)]
struct Coef {
    float: bool,
    denom: u32,
}

/// Reads an RPU and writes each field read on, but those muted.
struct Tee<'a, 'b> {
    r: Bits<'a>,
    out: &'b mut [u8],
    /// Bits written.
    n: usize,
    mute: bool,
    /// Nothing is written, the pass that learns the shape.
    dry: bool,
}

impl Tee<'_, '_> {
    fn read(&mut self, n: u32) -> Result<u32, DoviError> {
        self.r.u(n).ok_or(DoviError::Cut)
    }

    fn put(&mut self, v: u32, n: u32) -> Result<(), DoviError> {
        if self.dry {
            return Ok(());
        }
        if self.n + n as usize > self.out.len() * 8 {
            return Err(DoviError::Room);
        }
        for i in (0..n).rev() {
            let at = self.n / 8;
            if self.n.is_multiple_of(8) {
                self.out[at] = 0;
            }
            self.out[at] |= ((v >> i & 1) as u8) << (7 - self.n % 8);
            self.n += 1;
        }
        Ok(())
    }

    fn put_ue(&mut self, v: u32) -> Result<(), DoviError> {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.put(0, len - 1)?;
        self.put((x >> 32) as u32, len.saturating_sub(32))?;
        self.put(x as u32, len.min(32))
    }

    fn u(&mut self, n: u32) -> Result<u32, DoviError> {
        let v = self.read(n)?;
        if !self.mute {
            self.put(v, n)?;
        }
        Ok(v)
    }

    fn flag(&mut self) -> Result<bool, DoviError> {
        Ok(self.u(1)? == 1)
    }

    fn ue(&mut self) -> Result<u32, DoviError> {
        let v = self.r.ue().ok_or(DoviError::Cut)?;
        if !self.mute {
            self.put_ue(v)?;
        }
        Ok(v)
    }

    fn copy(&mut self, mut bits: usize) -> Result<(), DoviError> {
        while bits > 0 {
            let n = bits.min(32);
            self.u(n as u32)?;
            bits -= n;
        }
        Ok(())
    }

    fn align(&mut self) -> Result<(), DoviError> {
        self.r.skip((8 - self.r.pos() % 8) % 8).ok_or(DoviError::Cut)?;
        self.put(0, ((8 - self.n % 8) % 8) as u32)
    }

    /// One coefficient, its integer part and its fraction. A signed one reads the same bits.
    fn coef(&mut self, c: Coef) -> Result<(u32, u32), DoviError> {
        let int = if c.float { 0 } else { self.ue()? };
        Ok((int, self.u(c.denom)?))
    }

    fn put_coef(&mut self, c: Coef, one: bool) -> Result<(), DoviError> {
        if c.float {
            return self.put(if one { 1f32.to_bits() } else { 0 }, 32);
        }
        // se(1) and ue(1) are the same code
        self.put_ue(u32::from(one))?;
        self.put(0, c.denom)
    }
}

#[derive(Clone, Copy, Default)]
struct Seen {
    /// The enhancement layer carries a residual, its quantizer not the minimal layer's.
    fel: bool,
}

/// One RPU through `t`, to the byte its CRC starts at. The two layer flags come out as a single
/// layer's, the enhancement layer's quantizer goes, `flat` writes the mapping that changes
/// nothing in place of the source's, and the active area is `fit`'s.
fn walk(t: &mut Tee<'_, '_>, flat: bool, fit: &Fit) -> Result<Seen, DoviError> {
    let rpu_type = t.u(6)?;
    if rpu_type != 2 {
        return Err(DoviError::RpuType(rpu_type as u8));
    }
    let format = t.u(11)?;
    t.u(8)?; // vdr_rpu_profile, vdr_rpu_level
    if !t.flag()? {
        return Err(DoviError::Unsupported("no vdr_seq_info"));
    }
    if format & 0x700 != 0 {
        return Err(DoviError::Unsupported("rpu_format"));
    }
    t.u(1)?; // chroma_resampling_explicit_filter_flag
    let c = match t.u(2)? {
        0 => {
            let denom = t.ue()?;
            if !(13..=32).contains(&denom) {
                return Err(DoviError::Range("coef_log2_denom"));
            }
            Coef { float: false, denom }
        }
        1 => Coef { float: true, denom: 32 },
        _ => return Err(DoviError::Range("coef_data_type")),
    };
    t.u(3)?; // vdr_rpu_normalized_idc, bl_video_full_range_flag
    let bl = t.ue()?;
    // ext_mapping_idc is in the bits past the low 8
    let el = t.ue()? & 0xFF;
    let vdr = t.ue()?;
    if bl > 8 || el > 8 || vdr > 8 {
        return Err(DoviError::Range("bit depth"));
    }
    let (bl, el) = (bl + 8, el + 8);
    t.u(1)?; // spatial_resampling_filter_flag
    if t.u(3)? != 0 {
        return Err(DoviError::Unsupported("dm_compression"));
    }
    // el_spatial_resampling_filter_flag off, disable_residual_flag on
    let layered = t.read(2)? & 1 == 0;
    t.put(0b01, 2)?;
    let dm = t.flag()?;
    let mut seen = Seen::default();
    if t.flag()? {
        t.ue()?; // prev_vdr_rpu_id
    } else {
        t.ue()?; // vdr_rpu_id
        t.ue()?; // mapping_color_space
        t.ue()?; // mapping_chroma_format_idc
        let mut pieces = [0u32; 3];
        t.mute = flat;
        for p in &mut pieces {
            let more = t.ue()?;
            if more >= PIECES_MAX {
                return Err(DoviError::Range("num_pivots"));
            }
            *p = more + 1;
            for _ in 0..more + 2 {
                t.u(bl)?;
            }
        }
        t.mute = false;
        if flat {
            for _ in 0..3 {
                t.put_ue(0)?;
                t.put(0, bl)?;
                t.put((1 << bl) - 1, bl)?;
            }
        }
        t.mute = layered;
        if layered {
            if t.u(3)? != 0 {
                return Err(DoviError::Range("nlq_method_idc"));
            }
            t.u(bl)?;
            t.u(bl)?;
        }
        t.ue()?; // num_x_partitions_minus1
        t.ue()?; // num_y_partitions_minus1
        t.mute = false;
        if layered {
            t.put_ue(0)?;
            t.put_ue(0)?;
        }
        t.mute = flat;
        for &n in &pieces {
            for _ in 0..n {
                match t.ue()? {
                    0 => {
                        let order = t.ue()?;
                        if order > 1 {
                            return Err(DoviError::Range("poly_order"));
                        }
                        if order == 0 && t.flag()? {
                            return Err(DoviError::Unsupported("linear interpolation"));
                        }
                        for _ in 0..order + 2 {
                            t.coef(c)?;
                        }
                    }
                    1 => {
                        let order = t.u(2)?;
                        if order > 2 {
                            return Err(DoviError::Range("mmr_order"));
                        }
                        for _ in 0..1 + (order + 1) * 7 {
                            t.coef(c)?;
                        }
                    }
                    _ => return Err(DoviError::Range("mapping_idc")),
                }
            }
        }
        t.mute = false;
        if flat {
            for _ in 0..3 {
                t.put_ue(0)?; // mapping_idc, a polynomial
                t.put_ue(0)?; // poly_order_minus1
                t.put(0, 1)?; // linear_interp_flag
                t.put_coef(c, false)?;
                t.put_coef(c, true)?;
            }
        }
        if layered {
            t.mute = true;
            for _ in 0..3 {
                let offset = t.u(el)?;
                let max = t.coef(c)?;
                let slope = t.coef(c)?;
                let threshold = t.coef(c)?;
                seen.fel |= c.float
                    || offset != 0
                    || max != (1, 0)
                    || slope != (0, 0)
                    || threshold != (0, 0);
            }
            t.mute = false;
        }
    }
    if dm {
        t.ue()?; // affected_dm_metadata_id
        t.ue()?; // current_dm_metadata_id
        t.ue()?; // scene_refresh_flag
        t.mute = layered;
        t.copy(9 * 16 + 3 * 32 + 9 * 16)?;
        t.mute = false;
        if layered {
            for v in YCC_TO_RGB {
                t.put(u32::from(v as u16), 16)?;
            }
            for v in YCC_OFFSET {
                t.put(v, 32)?;
            }
            for v in RGB_TO_LMS {
                t.put(u32::from(v as u16), 16)?;
            }
        }
        t.copy(16 + 16 + 16 + 32 + 5)?; // signal_eotf and its params, signal_bit_depth
        let space = t.read(2)?;
        t.put(if layered { 0 } else { space }, 2)?;
        t.copy(2 + 2 + 12 + 12 + 10)?; // chroma format, range, source min, max and diagonal
        blocks(t, Some(fit))?;
        if t.r.left() > TAIL_BITS {
            blocks(t, None)?;
        }
    }
    t.align()?;
    Ok(seen)
}

/// One list of extension blocks, the first list's active area fitted where `fit` is given.
fn blocks(t: &mut Tee<'_, '_>, fit: Option<&Fit>) -> Result<(), DoviError> {
    let n = t.ue()?;
    t.align()?;
    for _ in 0..n {
        let len = t.ue()?;
        let level = t.u(8)?;
        let mut bits = len as usize * 8;
        if bits > t.r.left() {
            return Err(DoviError::Cut);
        }
        if let Some(fit) = fit.filter(|_| level == LEVEL_ACTIVE_AREA && bits >= 4 * 13) {
            let area = [t.read(13)?, t.read(13)?, t.read(13)?, t.read(13)?];
            for v in fit.area(area) {
                t.put(v, 13)?;
            }
            bits -= 4 * 13;
        }
        t.copy(bits)?;
    }
    Ok(())
}

/// `bytes`, an RPU as `form` wraps it, as the RPU of a single layer stream onto `out`, from its
/// `rpu_type` through the 0x80 behind its CRC, its length. An enhancement layer's quantizer
/// goes, with a full enhancement layer the mapping too, and the active area is `fit`'s. An RPU
/// of one layer whose picture keeps its size comes out bit for bit.
pub fn single_layer(
    form: RpuForm,
    bytes: &[u8],
    fit: &Fit,
    out: &mut [u8],
) -> Result<usize, DoviError> {
    let mut raw = [0u8; SIDE_CAP];
    let n = match form {
        RpuForm::Nal => {
            let (&first, rest) = bytes.split_first().ok_or(DoviError::Cut)?;
            if first != 0x19 {
                return Err(DoviError::NoPrefix(first));
            }
            unescape_into(rest, &mut raw)
        }
        RpuForm::Av1Emdf => unwrap_emdf(bytes, &mut raw)?,
    };
    // a NAL may end in zero bytes
    let n = raw[..n].iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let rpu = &raw[..n];
    if n < 5 || rpu[n - 1] != 0x80 {
        return Err(DoviError::Cut);
    }
    let mut dry = Tee { r: Bits::new(rpu), out: &mut [], n: 0, mute: false, dry: true };
    let seen = walk(&mut dry, false, fit)?;
    let end = dry.r.pos() / 8;
    if end + 5 != n {
        return Err(DoviError::Unsupported("bytes behind the CRC"));
    }
    if crc32(&rpu[..end + 4]) != 0 {
        return Err(DoviError::Crc);
    }
    let mut t = Tee { r: Bits::new(rpu), out, n: 0, mute: false, dry: false };
    walk(&mut t, seen.fel, fit)?;
    let len = t.n / 8;
    let tail = out.get_mut(len..len + 5).ok_or(DoviError::Room)?;
    tail[4] = 0x80;
    let crc = crc32(&out[..len]);
    out[len..len + 4].copy_from_slice(&crc.to_be_bytes());
    Ok(len + 5)
}

/// An RPU for tests, built field by field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sample {
    /// An enhancement layer's quantizer, `true` one with a residual.
    pub nlq: Option<bool>,
    /// The mapping that changes nothing, else two luma pieces, an MMR and a polynomial.
    pub flat: bool,
    /// The BT.2020 matrices and colour space 0, else another's.
    pub bt2020: bool,
    pub partitions: u32,
    pub area: Option<[u32; 4]>,
    pub second_list: bool,
}

impl Sample {
    /// One layer, as profile 8.1 has it.
    pub const P8: Sample = Sample {
        nlq: None,
        flat: false,
        bt2020: true,
        partitions: 0,
        area: Some([0, 0, 276, 276]),
        second_list: true,
    };
}

fn sample_coef(w: &mut BitWriter, int: u32, frac: u32) {
    w.ue(int);
    w.put(frac, 23);
}

/// `s` from its `rpu_type` through the 0x80 behind its CRC.
pub fn sample(s: Sample) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(2, 6).put(18, 11).put(1, 4).put(0, 4);
    w.put(1, 1).put(0, 1).put(0, 2).ue(23);
    w.put(1, 2).put(0, 1);
    w.ue(2).ue(2).ue(4);
    w.put(0, 1).put(0, 3);
    w.put(u32::from(s.nlq.is_some()), 1).put(u32::from(s.nlq.is_none()), 1);
    w.put(1, 1).put(0, 1);
    w.ue(0).ue(0).ue(1);
    if s.flat {
        for _ in 0..3 {
            w.ue(0).put(0, 10).put(1023, 10);
        }
    } else {
        w.ue(1).put(0, 10).put(512, 10).put(511, 10);
        w.ue(0).put(64, 10).put(896, 10);
        w.ue(0).put(64, 10).put(896, 10);
    }
    if s.nlq.is_some() {
        w.put(0, 3).put(0, 10).put(1023, 10);
    }
    w.ue(s.partitions).ue(s.partitions);
    if s.flat {
        for _ in 0..3 {
            w.ue(0).ue(0).put(0, 1);
            sample_coef(&mut w, 0, 0);
            sample_coef(&mut w, 1, 0);
        }
    } else {
        // luma, a second order piece and a first order one
        w.ue(0).ue(1);
        sample_coef(&mut w, 0, 0x1234);
        sample_coef(&mut w, 1, 0x7F_FFFF);
        sample_coef(&mut w, 2, 1);
        w.ue(0).ue(0).put(0, 1);
        sample_coef(&mut w, 4, 0);
        sample_coef(&mut w, 0, 0);
        // chroma, a first order MMR and a polynomial
        w.ue(1).put(0, 2);
        for i in 0..8 {
            sample_coef(&mut w, i, i << 3);
        }
        w.ue(0).ue(0).put(0, 1);
        sample_coef(&mut w, 0, 0);
        sample_coef(&mut w, 1, 0);
    }
    if let Some(fel) = s.nlq {
        for _ in 0..3 {
            w.put(if fel { 512 } else { 0 }, 10);
            sample_coef(&mut w, 1, 0);
            sample_coef(&mut w, 0, if fel { 77 } else { 0 });
            sample_coef(&mut w, 0, 0);
        }
    }
    w.ue(0).ue(0).ue(1);
    if s.bt2020 {
        for v in YCC_TO_RGB {
            w.put(u32::from(v as u16), 16);
        }
        for v in YCC_OFFSET {
            w.put(v, 32);
        }
        for v in RGB_TO_LMS {
            w.put(u32::from(v as u16), 16);
        }
    } else {
        for i in 0..9 {
            w.put(1000 + i, 16);
        }
        for i in 0..3 {
            w.put(5_000_000 + i, 32);
        }
        for i in 0..9 {
            w.put(2000 + i, 16);
        }
    }
    w.put(0xFFFF, 16).put(1, 16).put(2, 16).put(3, 32).put(12, 5);
    w.put(if s.bt2020 { 0 } else { 2 }, 2).put(0, 2).put(1, 2);
    w.put(62, 12).put(3079, 12).put(42, 10);
    w.ue(1 + u32::from(s.area.is_some()));
    let mut bits = w.into_bytes();
    let mut w = BitWriter::default();
    w.ue(5).put(1, 8).put(0, 12).put(3000, 12).put(1200, 12).put(0, 4);
    if let Some(a) = s.area {
        w.ue(7).put(5, 8);
        for v in a {
            w.put(v, 13);
        }
        w.put(0, 4);
    }
    if s.second_list {
        w.ue(1);
        let mut first = w.into_bytes();
        bits.append(&mut first);
        w = BitWriter::default();
        w.ue(2).put(254, 8).put(0, 8).put(2, 8);
    }
    bits.append(&mut w.into_bytes());
    let crc = crc32(&bits);
    bits.extend_from_slice(&crc.to_be_bytes());
    bits.push(0x80);
    bits
}

/// `rpu` as its NAL unit carries it behind the header, 0x19 in front and escaped.
pub fn nal_form(rpu: &[u8]) -> Vec<u8> {
    let (mut out, mut zeros) = (vec![0x19], 0);
    for &b in rpu {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;
    const P8: Sample = Sample::P8;
    use crate::dovi::RpuHeader;

    const SAME: Fit = Fit { from: (3840, 2160), turns: 0, to: (3840, 2160) };

    fn emdf(rpu: &[u8]) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(0x01be_6841, 27);
        if rpu.len() > 0xFF {
            w.put((rpu.len() >> 8) as u32 - 1, 8).put(1, 1);
        }
        w.put(rpu.len() as u32 & 0xFF, 8).put(0, 1);
        for &b in rpu {
            w.put(u32::from(b), 8);
        }
        w.put(0x400, 17).put(0x1F, 5);
        w.into_bytes()
    }

    fn single(form: RpuForm, bytes: &[u8], fit: &Fit) -> Result<Vec<u8>, DoviError> {
        let mut out = [0u8; SIDE_CAP];
        single_layer(form, bytes, fit, &mut out).map(|n| out[..n].to_vec())
    }

    #[test]
    fn the_crc_is_the_mpeg2_one() {
        assert_eq!(crc32(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn a_single_layer_rpu_at_its_own_size_comes_out_bit_for_bit() {
        for shape in [P8, Sample { area: None, ..P8 }, Sample { second_list: false, ..P8 }] {
            let src = sample(shape);
            assert!(nal_form(&src).len() > src.len() + 1, "escaping is under test");
            assert_eq!(single(RpuForm::Nal, &nal_form(&src), &SAME).unwrap(), src);
            let mut padded = nal_form(&src);
            padded.extend_from_slice(&[0, 0]);
            assert_eq!(single(RpuForm::Nal, &padded, &SAME).unwrap(), src, "trailing zeros");
        }
    }

    #[test]
    fn the_active_area_follows_the_picture_and_the_crc_holds() {
        let half = Fit { from: (3840, 2160), turns: 0, to: (1920, 1080) };
        let got = single(RpuForm::Nal, &nal_form(&sample(P8)), &half).unwrap();
        assert_eq!(got, sample(Sample { area: Some([0, 0, 138, 138]), ..P8 }));
        assert_eq!(crc32(&got[..got.len() - 1]), 0);
        // what it wrote reads back as it is
        assert_eq!(single(RpuForm::Nal, &nal_form(&got), &SAME).unwrap(), got);
    }

    #[test]
    fn offsets_turn_with_the_picture_then_scale_each_axis() {
        let fit = |turns, to| Fit { from: (4000, 2000), turns, to };
        assert_eq!(fit(0, (2000, 500)).area([40, 80, 100, 200]), [20, 40, 25, 50]);
        assert_eq!(fit(1, (2000, 4000)).area([40, 80, 100, 200]), [200, 100, 40, 80]);
        assert_eq!(fit(2, (4000, 2000)).area([40, 80, 100, 200]), [80, 40, 200, 100]);
        assert_eq!(fit(3, (1000, 2000)).area([40, 80, 100, 200]), [50, 100, 40, 20]);
        assert_eq!(fit(0, (4000, 3000)).area([0, 0, 1, 3]), [0, 0, 2, 5], "rounded");
        assert_eq!(fit(0, (40_000, 2000)).area([4000, 0, 0, 0])[0], AREA_MAX, "13 bits");
        let unknown = Fit { from: (0, 0), turns: 0, to: (1920, 1080) };
        assert_eq!(unknown.area([1, 2, 3, 4]), [1, 2, 3, 4]);
    }

    #[test]
    fn a_minimal_enhancement_layer_goes_and_the_mapping_stays() {
        let mel = Sample { nlq: Some(false), bt2020: false, partitions: 3, ..P8 };
        let got = single(RpuForm::Nal, &nal_form(&sample(mel)), &SAME).unwrap();
        assert_eq!(got, sample(P8));
        let h = RpuHeader::parse(RpuForm::Nal, &nal_form(&got)).unwrap();
        assert_eq!(h.guess_profile(), Some(8));
        let before = RpuHeader::parse(RpuForm::Nal, &nal_form(&sample(mel))).unwrap();
        assert!(!before.seq.unwrap().disable_residual);
    }

    #[test]
    fn a_full_enhancement_layer_takes_the_mapping_with_it() {
        let fel = Sample { nlq: Some(true), ..P8 };
        let got = single(RpuForm::Nal, &nal_form(&sample(fel)), &SAME).unwrap();
        assert_eq!(got, sample(Sample { flat: true, ..P8 }));
        let half = Fit { from: (3840, 2160), turns: 0, to: (1920, 1080) };
        let got = single(RpuForm::Nal, &nal_form(&sample(fel)), &half).unwrap();
        assert_eq!(got, sample(Sample { flat: true, area: Some([0, 0, 138, 138]), ..P8 }));
    }

    #[test]
    fn an_av1_rpu_comes_out_as_the_nal_form_does() {
        let src = sample(P8);
        assert_eq!(single(RpuForm::Av1Emdf, &emdf(&src), &SAME).unwrap(), src);
    }

    #[test]
    fn a_damaged_rpu_is_refused() {
        let src = sample(P8);
        let mut flipped = src.clone();
        let at = flipped.len() - 7;
        flipped[at] ^= 0x10;
        assert_eq!(single(RpuForm::Nal, &nal_form(&flipped), &SAME), Err(DoviError::Crc));
        let cut = &src[..src.len() / 2];
        assert_eq!(single(RpuForm::Nal, &nal_form(cut), &SAME), Err(DoviError::Cut));
        assert_eq!(single(RpuForm::Nal, &src, &SAME), Err(DoviError::NoPrefix(src[0])));
        assert_eq!(single(RpuForm::Nal, &[], &SAME), Err(DoviError::Cut));
        let mut out = [0u8; 64];
        assert_eq!(
            single_layer(RpuForm::Nal, &nal_form(&src), &SAME, &mut out),
            Err(DoviError::Room)
        );
        let mut whole = [0u8; SIDE_CAP];
        let n = single_layer(RpuForm::Nal, &nal_form(&src), &SAME, &mut whole).unwrap();
        let mut tight = vec![0u8; n - 1];
        assert_eq!(
            single_layer(RpuForm::Nal, &nal_form(&src), &SAME, &mut tight),
            Err(DoviError::Room)
        );
    }

    #[test]
    fn what_the_walk_does_not_know_is_refused_not_guessed() {
        let mut compressed = BitWriter::default();
        compressed.put(2, 6).put(18, 11).put(1, 4).put(0, 4);
        compressed.put(1, 1).put(0, 1).put(0, 2).ue(23);
        compressed.put(1, 2).put(0, 1);
        compressed.ue(2).ue(2).ue(4);
        compressed.put(0, 1).put(1, 3).put(0, 32).put(0, 32);
        let mut bytes = compressed.into_bytes();
        bytes.push(0x80);
        assert_eq!(
            single(RpuForm::Nal, &nal_form(&bytes), &SAME),
            Err(DoviError::Unsupported("dm_compression"))
        );
        let mut other = sample(P8);
        other[0] = 3 << 2;
        assert_eq!(single(RpuForm::Nal, &nal_form(&other), &SAME), Err(DoviError::RpuType(3)));
    }
}
