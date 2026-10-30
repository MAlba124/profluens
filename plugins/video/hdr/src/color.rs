//! Colorimetry vocabulary and its ITU-T H.273 code points.

use profluens_core::ctx::Ctx;
use profluens_core::format::{FixedFormat, Value, ValueDesc, Vocabulary};
use profluens_core::id::{FieldId, ValueId};

use crate::dovi::DoviConfig;

/// Y'CbCr to R'G'B' matrix coefficients (H.273 section 8.3, `MatrixCoefficients`).
pub const FIELD_MATRIX: &str = "matrix";
/// Quantization range (H.273 `VideoFullRangeFlag`, mkv `Range`).
pub const FIELD_RANGE: &str = "range";
/// Transfer characteristics (H.273 section 8.2, `TransferCharacteristics`).
pub const FIELD_TRANSFER: &str = "transfer";
/// Colour primaries (H.273 section 8.1, `ColourPrimaries`).
pub const FIELD_PRIMARIES: &str = "primaries";
/// Chroma sample location of 4:2:0 content, a [`ChromaSite`] name.
pub const FIELD_CHROMA_SITE: &str = "chroma-site";
/// The Dolby Vision configuration record, one `Int`, [`DoviConfig::pack`].
pub const FIELD_DOVI: &str = "dovi";

macro_rules! code_points {
    ($(#[$m:meta])* $ty:ident { $($(#[$vm:meta])* $v:ident = $cp:literal, $name:literal;)+ }) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum $ty {
            $($(#[$vm])* $v,)+
        }

        impl $ty {
            /// Every value, BT.709 first since a link fixes the first name early.
            pub const ALL: [$ty; [$($cp),+].len()] = [$($ty::$v),+];
            pub const VALUES: [ValueDesc; [$($cp),+].len()] = [$(ValueDesc::Id($name)),+];

            pub const fn caps_name(self) -> &'static str {
                match self { $($ty::$v => $name,)+ }
            }

            pub fn from_caps_name(s: &str) -> Option<Self> {
                match s { $($name => Some($ty::$v),)+ _ => None }
            }

            pub const fn to_h273(self) -> u8 {
                match self { $($ty::$v => $cp,)+ }
            }

            /// `None` for 2 (unspecified) and reserved code points.
            pub const fn from_h273(cp: u8) -> Option<Self> {
                match cp { $($cp => Some($ty::$v),)+ _ => None }
            }
        }
    };
}

code_points! {
    Matrix {
        Bt709 = 1, "bt709";
        Identity = 0, "identity";
        Fcc = 4, "fcc";
        Bt470Bg = 5, "bt470bg";
        Bt601 = 6, "bt601";
        Smpte240 = 7, "smpte240m";
        YCgCo = 8, "ycgco";
        Bt2020 = 9, "bt2020";
        Bt2020Cl = 10, "bt2020c";
        Smpte2085 = 11, "smpte2085";
        ChromaNcl = 12, "chroma-derived-nc";
        ChromaCl = 13, "chroma-derived-c";
        ICtCp = 14, "ictcp";
        IptC2 = 15, "ipt-c2";
        YCgCoRe = 16, "ycgco-re";
        YCgCoRo = 17, "ycgco-ro";
    }
}

code_points! {
    TransferFn {
        Bt709 = 1, "bt709";
        Gamma22 = 4, "gamma22";
        Gamma28 = 5, "gamma28";
        Smpte170 = 6, "smpte170m";
        Smpte240 = 7, "smpte240m";
        Linear = 8, "linear";
        Log100 = 9, "log100";
        Log316 = 10, "log316";
        Iec61966_2_4 = 11, "iec61966-2-4";
        Bt1361 = 12, "bt1361e";
        Srgb = 13, "srgb";
        Bt2020_10 = 14, "bt2020-10";
        Bt2020_12 = 15, "bt2020-12";
        Pq = 16, "pq";
        Smpte428 = 17, "smpte428";
        Hlg = 18, "hlg";
    }
}

code_points! {
    Primaries {
        Bt709 = 1, "bt709";
        Bt470M = 4, "bt470m";
        Bt470Bg = 5, "bt470bg";
        Bt601 = 6, "bt601";
        Smpte240 = 7, "smpte240m";
        Film = 8, "film";
        Bt2020 = 9, "bt2020";
        Smpte428 = 10, "smpte428";
        DciP3 = 11, "dci-p3";
        DisplayP3 = 12, "smpte432";
        Ebu3213 = 22, "ebu3213";
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Range {
    Limited,
    Full,
}

impl Range {
    pub const ALL: [Range; 2] = [Range::Limited, Range::Full];
    pub const VALUES: [ValueDesc; 2] = [ValueDesc::Id("limited"), ValueDesc::Id("full")];

    pub const fn caps_name(self) -> &'static str {
        match self {
            Range::Limited => "limited",
            Range::Full => "full",
        }
    }

    pub fn from_caps_name(s: &str) -> Option<Range> {
        match s {
            "limited" => Some(Range::Limited),
            "full" => Some(Range::Full),
            _ => None,
        }
    }

    pub const fn from_flag(full: bool) -> Range {
        if full { Range::Full } else { Range::Limited }
    }

    /// From mkv `Colour\Range` (RFC 9559). 1 is limited, 2 full, 0 and 3 unspecified.
    pub fn from_mkv(v: u8) -> Option<Range> {
        Some(match v {
            1 => Range::Limited,
            2 => Range::Full,
            _ => return None,
        })
    }

    pub const fn to_mkv(self) -> u8 {
        match self {
            Range::Limited => 1,
            Range::Full => 2,
        }
    }
}

impl Matrix {
    /// `None` for a matrix that is not a plain Y'CbCr weighting.
    pub const fn class(self) -> Option<Matrix> {
        Some(match self {
            Matrix::Identity => Matrix::Identity,
            Matrix::Bt709 | Matrix::Smpte240 => Matrix::Bt709,
            Matrix::Fcc | Matrix::Bt470Bg | Matrix::Bt601 => Matrix::Bt601,
            Matrix::Bt2020 | Matrix::Bt2020Cl => Matrix::Bt2020,
            _ => return None,
        })
    }
}

impl TransferFn {
    pub const fn class(self) -> TransferFn {
        match self {
            TransferFn::Pq => TransferFn::Pq,
            TransferFn::Hlg => TransferFn::Hlg,
            TransferFn::Linear => TransferFn::Linear,
            TransferFn::Srgb | TransferFn::Gamma22 | TransferFn::Iec61966_2_4 => TransferFn::Srgb,
            _ => TransferFn::Bt709,
        }
    }

    pub const fn is_hdr(self) -> bool {
        matches!(self, TransferFn::Pq | TransferFn::Hlg)
    }
}

impl Primaries {
    pub const fn class(self) -> Primaries {
        match self {
            Primaries::Bt2020 => Primaries::Bt2020,
            Primaries::DciP3 | Primaries::DisplayP3 => Primaries::DciP3,
            Primaries::Bt470M
            | Primaries::Bt470Bg
            | Primaries::Bt601
            | Primaries::Smpte240
            | Primaries::Ebu3213 => Primaries::Bt601,
            Primaries::Bt709 | Primaries::Film | Primaries::Smpte428 => Primaries::Bt709,
        }
    }
}

/// 4:2:0 chroma siting, `chroma_sample_loc_type` 0 to 5 (H.264 E.2.1, H.265 E.3.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChromaSite {
    Left,
    Center,
    TopLeft,
    Top,
    BottomLeft,
    Bottom,
}

impl ChromaSite {
    pub const ALL: [ChromaSite; 6] = [
        ChromaSite::Left,
        ChromaSite::Center,
        ChromaSite::TopLeft,
        ChromaSite::Top,
        ChromaSite::BottomLeft,
        ChromaSite::Bottom,
    ];
    pub const VALUES: [ValueDesc; 6] = [
        ValueDesc::Id("left"),
        ValueDesc::Id("center"),
        ValueDesc::Id("top-left"),
        ValueDesc::Id("top"),
        ValueDesc::Id("bottom-left"),
        ValueDesc::Id("bottom"),
    ];

    pub const fn caps_name(self) -> &'static str {
        match self {
            ChromaSite::Left => "left",
            ChromaSite::Center => "center",
            ChromaSite::TopLeft => "top-left",
            ChromaSite::Top => "top",
            ChromaSite::BottomLeft => "bottom-left",
            ChromaSite::Bottom => "bottom",
        }
    }

    pub fn from_caps_name(s: &str) -> Option<ChromaSite> {
        ChromaSite::ALL.into_iter().find(|c| c.caps_name() == s)
    }

    pub const fn from_h26x(t: u32) -> Option<ChromaSite> {
        Some(match t {
            0 => ChromaSite::Left,
            1 => ChromaSite::Center,
            2 => ChromaSite::TopLeft,
            3 => ChromaSite::Top,
            4 => ChromaSite::BottomLeft,
            5 => ChromaSite::Bottom,
            _ => return None,
        })
    }

    pub const fn to_h26x(self) -> u8 {
        self as u8
    }

    /// From mkv `ChromaSitingHorz` and `ChromaSitingVert` (RFC 9559 5.1.4.1.31.8 and .9).
    pub const fn from_mkv(horz: u8, vert: u8) -> Option<ChromaSite> {
        Some(match (horz, vert) {
            (1, 2) => ChromaSite::Left,
            (2, 2) => ChromaSite::Center,
            (1, 1) => ChromaSite::TopLeft,
            (2, 1) => ChromaSite::Top,
            _ => return None,
        })
    }

    /// `None` for the two bottom sites mkv cannot express.
    pub const fn to_mkv(self) -> Option<(u8, u8)> {
        Some(match self {
            ChromaSite::Left => (1, 2),
            ChromaSite::Center => (2, 2),
            ChromaSite::TopLeft => (1, 1),
            ChromaSite::Top => (2, 1),
            ChromaSite::BottomLeft | ChromaSite::Bottom => return None,
        })
    }

    /// From AV1 `chroma_sample_position` (AV1 6.4.2), mapped as FFmpeg av1dec.c does.
    pub const fn from_av1(csp: u32) -> Option<ChromaSite> {
        match csp {
            1 => Some(ChromaSite::Left),
            2 => Some(ChromaSite::TopLeft),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Colorimetry {
    pub matrix: Matrix,
    pub range: Range,
    pub transfer: TransferFn,
    pub primaries: Primaries,
}

impl Colorimetry {
    /// H.273 unspecified defaults. BT.709 for HD, BT.601 for SD, limited range, SDR.
    pub fn default_for(height: u32) -> Colorimetry {
        if height >= 720 {
            Colorimetry {
                matrix: Matrix::Bt709,
                range: Range::Limited,
                transfer: TransferFn::Bt709,
                primaries: Primaries::Bt709,
            }
        } else {
            Colorimetry {
                matrix: Matrix::Bt601,
                range: Range::Limited,
                transfer: TransferFn::Bt709,
                primaries: Primaries::Bt601,
            }
        }
    }
}

/// Mastering red and green primaries, four 16-bit words high to low.
pub const FIELD_MASTERING_RG: &str = "mastering-rg";
/// Mastering blue primary and white point, four 16-bit words.
pub const FIELD_MASTERING_BW: &str = "mastering-bw";
/// Mastering max luminance in the high 32 bits, min in the low 32.
pub const FIELD_MASTERING_LUMINANCE: &str = "mastering-luminance";
/// Content light level, MaxCLL in bits 16..32, MaxFALL in bits 0..16.
pub const FIELD_CONTENT_LIGHT: &str = "content-light";

pub const FIELDS: [&str; 10] = [
    FIELD_MATRIX,
    FIELD_RANGE,
    FIELD_TRANSFER,
    FIELD_PRIMARIES,
    FIELD_MASTERING_RG,
    FIELD_MASTERING_BW,
    FIELD_MASTERING_LUMINANCE,
    FIELD_CONTENT_LIGHT,
    FIELD_CHROMA_SITE,
    FIELD_DOVI,
];

/// Mastering display colour volume, SMPTE ST 2086, in H.265 D.3.28 units.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mastering {
    /// CIE 1931 xy of the R, G, B primaries in steps of 0.00002.
    pub primaries: [[u16; 2]; 3],
    /// White point xy in steps of 0.00002.
    pub white: [u16; 2],
    /// Max and min luminance in 0.0001 cd/m2.
    pub max_luminance: u32,
    pub min_luminance: u32,
}

/// Content light level, CTA-861.3 section 6.9 and H.265 D.3.35, in cd/m2.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ContentLight {
    pub max_cll: u16,
    pub max_fall: u16,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct HdrStatic {
    pub mastering: Mastering,
    pub light: ContentLight,
}

impl Mastering {
    /// From an H.265 D.2.28 `mastering_display_colour_volume` payload, also an ISOBMFF `mdcv` body.
    pub fn from_st2086(payload: &[u8]) -> Option<Mastering> {
        let p = payload.get(..24)?;
        let u16at = |o: usize| u16::from_be_bytes([p[o], p[o + 1]]);
        let u32at = |o: usize| u32::from_be_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
        let xy = |o: usize| [u16at(o), u16at(o + 2)];
        Some(Mastering {
            primaries: [xy(8), xy(0), xy(4)],
            white: xy(12),
            max_luminance: u32at(16),
            min_luminance: u32at(20),
        })
    }

    /// From AV1 `metadata_hdr_mdcv` (AV1 5.8.4), converted to ST 2086 units.
    pub fn from_av1(primaries: [[u16; 2]; 3], white: [u16; 2], max: u32, min: u32) -> Mastering {
        let xy = |v: u16| ((u64::from(v) * 50_000 + (1 << 15)) >> 16) as u16;
        let lum = |v: u32, frac: u32| {
            ((u64::from(v) * 10_000 + (1 << (frac - 1))) >> frac).min(u64::from(u32::MAX)) as u32
        };
        let [r, g, b] = primaries;
        Mastering {
            primaries: [[xy(r[0]), xy(r[1])], [xy(g[0]), xy(g[1])], [xy(b[0]), xy(b[1])]],
            white: [xy(white[0]), xy(white[1])],
            max_luminance: lum(max, 8),
            min_luminance: lum(min, 14),
        }
    }
}

impl ContentLight {
    /// From an H.265 D.2.35 `content_light_level_info` payload, also `clli` and AV1
    /// `metadata_hdr_cll`.
    pub fn from_cta861(payload: &[u8]) -> Option<ContentLight> {
        let p = payload.get(..4)?;
        Some(ContentLight {
            max_cll: u16::from_be_bytes([p[0], p[1]]),
            max_fall: u16::from_be_bytes([p[2], p[3]]),
        })
    }
}

const fn pack4(w: [u16; 4]) -> i64 {
    ((w[0] as u64) << 48 | (w[1] as u64) << 32 | (w[2] as u64) << 16 | w[3] as u64) as i64
}

const fn unpack4(v: i64) -> [u16; 4] {
    let v = v as u64;
    [(v >> 48) as u16, (v >> 32) as u16, (v >> 16) as u16, v as u16]
}

impl HdrStatic {
    pub fn is_empty(&self) -> bool {
        *self == HdrStatic::default()
    }

    pub fn fields(&self) -> [(&'static str, Option<i64>); 4] {
        let m = &self.mastering;
        let [r, g, b] = m.primaries;
        let rg = [r[0], r[1], g[0], g[1]];
        let bw = [b[0], b[1], m.white[0], m.white[1]];
        let lum = (m.max_luminance as u64) << 32 | m.min_luminance as u64;
        let light = (self.light.max_cll as u64) << 16 | self.light.max_fall as u64;
        let stated = |v: u64| (v != 0).then_some(v as i64);
        [
            (FIELD_MASTERING_RG, stated(pack4(rg) as u64)),
            (FIELD_MASTERING_BW, stated(pack4(bw) as u64)),
            (FIELD_MASTERING_LUMINANCE, stated(lum)),
            (FIELD_CONTENT_LIGHT, stated(light)),
        ]
    }

    /// Take one field's value, `false` for an unknown name.
    pub fn set_field(&mut self, field: &str, v: i64) -> bool {
        let m = &mut self.mastering;
        match field {
            FIELD_MASTERING_RG => {
                let [rx, ry, gx, gy] = unpack4(v);
                (m.primaries[0], m.primaries[1]) = ([rx, ry], [gx, gy]);
            }
            FIELD_MASTERING_BW => {
                let [bx, by, wx, wy] = unpack4(v);
                (m.primaries[2], m.white) = ([bx, by], [wx, wy]);
            }
            FIELD_MASTERING_LUMINANCE => {
                (m.max_luminance, m.min_luminance) = ((v as u64 >> 32) as u32, v as u32);
            }
            FIELD_CONTENT_LIGHT => {
                self.light = ContentLight { max_cll: (v >> 16) as u16, max_fall: v as u16 };
            }
            _ => return false,
        }
        true
    }

    /// The mastering display peak in cd/m2, 0 when unstated.
    pub fn max_mastering_nits(&self) -> f32 {
        self.mastering.max_luminance as f32 / 10_000.0
    }

    /// Mastering and light level each from `self`, else from `under`.
    pub fn over(self, under: HdrStatic) -> HdrStatic {
        HdrStatic {
            mastering: if self.mastering != Mastering::default() {
                self.mastering
            } else {
                under.mastering
            },
            light: if self.light != ContentLight::default() { self.light } else { under.light },
        }
    }
}

pub type VuiSignal = (bool, Option<(u8, u8, u8)>);

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct StreamColor {
    pub matrix: Option<Matrix>,
    pub range: Option<Range>,
    pub transfer: Option<TransferFn>,
    pub primaries: Option<Primaries>,
    pub hdr: HdrStatic,
    pub chroma: Option<ChromaSite>,
    pub dovi: Option<DoviConfig>,
}

impl StreamColor {
    /// Bitstream colour (`self`) over container colour (`under`). Dolby Vision goes the other way.
    pub fn over(self, under: StreamColor) -> StreamColor {
        StreamColor {
            matrix: self.matrix.or(under.matrix),
            range: self.range.or(under.range),
            transfer: self.transfer.or(under.transfer),
            primaries: self.primaries.or(under.primaries),
            hdr: self.hdr.over(under.hdr),
            chroma: self.chroma.or(under.chroma),
            dovi: under.dovi.or(self.dovi),
        }
    }

    /// From an H.264 E.2.1 or H.265 E.3.1 VUI.
    pub fn from_vui(signal: Option<VuiSignal>, chroma_loc: Option<u32>) -> StreamColor {
        let mut c = StreamColor {
            chroma: chroma_loc.and_then(ChromaSite::from_h26x),
            ..StreamColor::default()
        };
        if let Some((full, desc)) = signal {
            c.range = Some(Range::from_flag(full));
            if let Some((p, t, m)) = desc {
                c.primaries = Primaries::from_h273(p);
                c.transfer = TransferFn::from_h273(t);
                c.matrix = Matrix::from_h273(m);
            }
        }
        c
    }

    pub fn is_complete(&self) -> bool {
        self.matrix.is_some()
            && self.range.is_some()
            && self.transfer.is_some()
            && self.primaries.is_some()
    }

    pub fn is_empty(&self) -> bool {
        *self == StreamColor::default()
    }

    /// The colorimetry with unstated fields defaulted for `height`.
    pub fn resolve(&self, height: u32) -> Colorimetry {
        let d = Colorimetry::default_for(height);
        Colorimetry {
            matrix: self.matrix.unwrap_or(d.matrix),
            range: self.range.unwrap_or(d.range),
            transfer: self.transfer.unwrap_or(d.transfer),
            primaries: self.primaries.unwrap_or(d.primaries),
        }
    }

    /// Write the stated fields into `out`, returning the count.
    pub fn write_fields(&self, out: &mut [(&'static str, ValueDesc)]) -> usize {
        let ids = [
            (FIELD_MATRIX, self.matrix.map(Matrix::caps_name)),
            (FIELD_RANGE, self.range.map(Range::caps_name)),
            (FIELD_TRANSFER, self.transfer.map(TransferFn::caps_name)),
            (FIELD_PRIMARIES, self.primaries.map(Primaries::caps_name)),
            (FIELD_CHROMA_SITE, self.chroma.map(ChromaSite::caps_name)),
        ];
        let ids = ids.into_iter().filter_map(|(f, v)| Some((f, ValueDesc::Id(v?))));
        let dovi = (FIELD_DOVI, self.dovi.map(DoviConfig::pack));
        let ints = self
            .hdr
            .fields()
            .into_iter()
            .chain([dovi])
            .filter_map(|(f, v)| Some((f, ValueDesc::Int(v?))));
        let mut n = 0;
        for (slot, field) in out.iter_mut().zip(ids.chain(ints)) {
            *slot = field;
            n += 1;
        }
        n
    }

    /// Read from a negotiated format.
    pub fn read(ctx: &Ctx, format: &FixedFormat) -> StreamColor {
        StreamColor::read_by(|f| ctx.field_id(f), |v| ctx.value_name(v), format)
    }

    pub fn read_vocabulary(vocabulary: &Vocabulary, format: &FixedFormat) -> StreamColor {
        StreamColor::read_by(|f| vocabulary.field_id(f), |v| vocabulary.value_name(v), format)
    }

    fn read_by<'a>(
        field_id: impl Fn(&str) -> Option<FieldId>,
        value_name: impl Fn(ValueId) -> Option<&'a str>,
        format: &FixedFormat,
    ) -> StreamColor {
        let mut c = StreamColor::default();
        let get = |field| format.get(field_id(field)?);
        let name = |field| match get(field)? {
            Value::Id(id) => value_name(id),
            _ => None,
        };
        c.matrix = name(FIELD_MATRIX).and_then(Matrix::from_caps_name);
        c.range = name(FIELD_RANGE).and_then(Range::from_caps_name);
        c.transfer = name(FIELD_TRANSFER).and_then(TransferFn::from_caps_name);
        c.primaries = name(FIELD_PRIMARIES).and_then(Primaries::from_caps_name);
        c.chroma = name(FIELD_CHROMA_SITE).and_then(ChromaSite::from_caps_name);
        for field in
            [FIELD_MASTERING_RG, FIELD_MASTERING_BW, FIELD_MASTERING_LUMINANCE, FIELD_CONTENT_LIGHT]
        {
            if let Some(Value::Int(v)) = get(field) {
                c.hdr.set_field(field, v);
            }
        }
        if let Some(Value::Int(v)) = get(FIELD_DOVI) {
            c.dovi = Some(DoviConfig::unpack(v));
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_names_round_trip() {
        for m in Matrix::ALL {
            assert_eq!(Matrix::from_caps_name(m.caps_name()), Some(m));
        }
        for r in Range::ALL {
            assert_eq!(Range::from_caps_name(r.caps_name()), Some(r));
        }
        for t in TransferFn::ALL {
            assert_eq!(TransferFn::from_caps_name(t.caps_name()), Some(t));
        }
        for p in Primaries::ALL {
            assert_eq!(Primaries::from_caps_name(p.caps_name()), Some(p));
        }
        for c in ChromaSite::ALL {
            assert_eq!(ChromaSite::from_caps_name(c.caps_name()), Some(c));
            assert_eq!(ChromaSite::from_h26x(u32::from(c.to_h26x())), Some(c));
        }
        assert_eq!(Matrix::from_caps_name("nope"), None);
    }

    #[test]
    fn every_code_point_round_trips() {
        for cp in 0..=255u8 {
            if let Some(m) = Matrix::from_h273(cp) {
                assert_eq!(m.to_h273(), cp);
            }
            if let Some(t) = TransferFn::from_h273(cp) {
                assert_eq!(t.to_h273(), cp);
            }
            if let Some(p) = Primaries::from_h273(cp) {
                assert_eq!(p.to_h273(), cp);
            }
        }
        assert_eq!(Matrix::ALL.len(), 16);
        assert_eq!(TransferFn::ALL.len(), 16);
        assert_eq!(Primaries::ALL.len(), 11);
        assert_eq!(
            (Matrix::ALL[0], TransferFn::ALL[0], Primaries::ALL[0]),
            (Matrix::Bt709, TransferFn::Bt709, Primaries::Bt709)
        );
        assert_eq!((Range::ALL[0], ChromaSite::ALL[0]), (Range::Limited, ChromaSite::Left));
    }

    #[test]
    fn h273_code_points_map_per_spec() {
        assert_eq!(Matrix::from_h273(1), Some(Matrix::Bt709));
        assert_eq!(Matrix::from_h273(5), Some(Matrix::Bt470Bg));
        assert_eq!(Matrix::from_h273(6), Some(Matrix::Bt601));
        assert_eq!(Matrix::from_h273(9), Some(Matrix::Bt2020));
        assert_eq!(Matrix::from_h273(10), Some(Matrix::Bt2020Cl));
        assert_eq!(Matrix::from_h273(14), Some(Matrix::ICtCp));
        assert_eq!(Matrix::from_h273(0), Some(Matrix::Identity));
        assert_eq!(Matrix::from_h273(2), None, "unspecified");
        assert_eq!(Matrix::from_h273(3), None, "reserved");
        assert_eq!(TransferFn::from_h273(16), Some(TransferFn::Pq));
        assert_eq!(TransferFn::from_h273(18), Some(TransferFn::Hlg));
        assert_eq!(TransferFn::from_h273(14), Some(TransferFn::Bt2020_10));
        assert_eq!(TransferFn::from_h273(13), Some(TransferFn::Srgb));
        assert_eq!(TransferFn::from_h273(2), None);
        assert_eq!(Primaries::from_h273(9), Some(Primaries::Bt2020));
        assert_eq!(Primaries::from_h273(11), Some(Primaries::DciP3));
        assert_eq!(Primaries::from_h273(12), Some(Primaries::DisplayP3));
        assert_eq!(Primaries::from_h273(22), Some(Primaries::Ebu3213));
        assert_eq!(Primaries::from_h273(13), None);
        assert_eq!(Range::from_mkv(1), Some(Range::Limited));
        assert_eq!(Range::from_mkv(2), Some(Range::Full));
        assert_eq!(Range::from_mkv(0), None);
    }

    #[test]
    fn classes_fold_onto_what_a_renderer_knows() {
        assert_eq!(TransferFn::Bt2020_10.class(), TransferFn::Bt709);
        assert_eq!(TransferFn::Smpte170.class(), TransferFn::Bt709);
        assert_eq!(TransferFn::Pq.class(), TransferFn::Pq);
        assert_eq!(Primaries::DisplayP3.class(), Primaries::DciP3);
        assert_eq!(Primaries::Bt470Bg.class(), Primaries::Bt601);
        assert_eq!(Matrix::Bt470Bg.class(), Some(Matrix::Bt601));
        assert_eq!(Matrix::ICtCp.class(), None);
    }

    #[test]
    fn chroma_sites_map_from_each_source() {
        assert_eq!(ChromaSite::from_mkv(1, 2), Some(ChromaSite::Left));
        assert_eq!(ChromaSite::from_mkv(1, 1), Some(ChromaSite::TopLeft));
        assert_eq!(ChromaSite::from_mkv(2, 2), Some(ChromaSite::Center));
        assert_eq!(ChromaSite::from_mkv(2, 1), Some(ChromaSite::Top));
        assert_eq!(ChromaSite::from_mkv(0, 1), None);
        for c in ChromaSite::ALL {
            if let Some((h, v)) = c.to_mkv() {
                assert_eq!(ChromaSite::from_mkv(h, v), Some(c));
            }
        }
        assert_eq!(ChromaSite::from_av1(1), Some(ChromaSite::Left));
        assert_eq!(ChromaSite::from_av1(2), Some(ChromaSite::TopLeft));
        assert_eq!(ChromaSite::from_av1(0), None);
        assert_eq!(ChromaSite::from_h26x(2), Some(ChromaSite::TopLeft));
        assert_eq!(ChromaSite::from_h26x(6), None);
    }

    #[test]
    fn unspecified_defaults_follow_resolution() {
        let hd = Colorimetry::default_for(1080);
        assert_eq!(hd.matrix, Matrix::Bt709);
        assert_eq!(hd.range, Range::Limited);
        let sd = Colorimetry::default_for(576);
        assert_eq!(sd.matrix, Matrix::Bt601);
        assert_eq!(sd.primaries, Primaries::Bt601);
        assert_eq!(Colorimetry::default_for(720).matrix, Matrix::Bt709);
    }

    #[test]
    fn av1_fixed_point_mastering_converts_to_st2086_units() {
        let m = Mastering::from_av1(
            [[46399, 19136], [11141, 52100], [8650, 3020]],
            [20493, 21561],
            1000 << 8,
            50 << 10,
        );
        assert_eq!(m.primaries[0][0], 35400);
        assert_eq!(m.max_luminance, 10_000_000, "1000 cd/m2");
        assert_eq!(m.min_luminance, 31_250);
    }

    #[test]
    fn precedence_is_the_bitstream_over_the_container_but_the_dovi_record() {
        let container = StreamColor {
            matrix: Some(Matrix::Bt709),
            primaries: Some(Primaries::Bt709),
            chroma: Some(ChromaSite::Left),
            dovi: Some(DoviConfig {
                profile: 8,
                level: 6,
                rpu: true,
                bl: true,
                compat_id: 1,
                ..DoviConfig::default()
            }),
            ..StreamColor::default()
        };
        let bitstream = StreamColor {
            matrix: Some(Matrix::Bt2020),
            chroma: Some(ChromaSite::TopLeft),
            dovi: Some(DoviConfig { profile: 8, rpu: true, bl: true, ..DoviConfig::default() }),
            ..StreamColor::default()
        };
        let c = bitstream.over(container);
        assert_eq!(c.matrix, Some(Matrix::Bt2020));
        assert_eq!(
            c.primaries,
            Some(Primaries::Bt709),
            "the container fills what the bitstream left"
        );
        assert_eq!(c.chroma, Some(ChromaSite::TopLeft));
        assert_eq!(c.dovi, container.dovi, "the record is the container's");
        let derived = bitstream.over(StreamColor::default());
        assert_eq!(
            derived.dovi, bitstream.dovi,
            "a derived record stands where the container has none"
        );
    }

    #[test]
    fn fields_round_trip_through_the_announcement_form() {
        let c = StreamColor {
            matrix: Some(Matrix::Bt2020),
            range: Some(Range::Limited),
            transfer: Some(TransferFn::Pq),
            primaries: Some(Primaries::Bt2020),
            hdr: HdrStatic {
                mastering: Mastering {
                    primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
                    white: [15635, 16450],
                    max_luminance: 10_000_000,
                    min_luminance: 50,
                },
                light: ContentLight { max_cll: 1000, max_fall: 400 },
            },
            chroma: Some(ChromaSite::TopLeft),
            dovi: Some(DoviConfig {
                major: 1,
                minor: 0,
                profile: 8,
                level: 6,
                rpu: true,
                el: false,
                bl: true,
                compat_id: 1,
                compression: 0,
            }),
        };
        let mut out = [("", ValueDesc::Int(0)); FIELDS.len()];
        let n = c.write_fields(&mut out);
        assert_eq!(n, 10);
        let mut back = StreamColor::default();
        for (field, v) in &out[..n] {
            match *v {
                ValueDesc::Id(name) => match *field {
                    FIELD_MATRIX => back.matrix = Matrix::from_caps_name(name),
                    FIELD_RANGE => back.range = Range::from_caps_name(name),
                    FIELD_TRANSFER => back.transfer = TransferFn::from_caps_name(name),
                    FIELD_PRIMARIES => back.primaries = Primaries::from_caps_name(name),
                    FIELD_CHROMA_SITE => back.chroma = ChromaSite::from_caps_name(name),
                    other => panic!("an unknown name field {other}"),
                },
                ValueDesc::Int(i) if *field == FIELD_DOVI => {
                    back.dovi = Some(DoviConfig::unpack(i))
                }
                ValueDesc::Int(i) => assert!(back.hdr.set_field(field, i)),
                ValueDesc::Rat(..) => panic!("no rational colour field"),
            }
        }
        assert_eq!(back, c);
        assert_eq!(
            StreamColor::default().write_fields(&mut out),
            0,
            "nothing stated, nothing written"
        );
    }
}
