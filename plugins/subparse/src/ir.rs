// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! The styled cue, `subtitle/cue-ir`. One definition serves the parsers, the wire and the
//! renderer: flat little-endian `#[repr(C)]` records behind a header, then one UTF-8 string
//! region. [`CueBuilder`] lays a cue out straight into a pool slot, [`CueIr`] validates a payload
//! once and reads it as typed slices.
//!
//! Layout, every section 8 byte aligned and in this order:
//!
//! ```text
//! CueHeader                 magic "CUIR", version, counts, section offsets, the LayoutRec
//! [LineRec; n_lines]        a run of the span table
//! [SpanRec; n_spans]        text, voice, classes, style, ruby, reveal time
//! [RubyRec; n_rubies]
//! [StrRef; n_classes]       WebVTT class names
//! [StyleRec; n_styles]      deduplicated, `base_style` indexes it
//! strings                   UTF-8, every StrRef is a byte range of it
//! ```

use crate::cue::{Cue, CueSettings, OutputFormat};
use crate::pod::{self, Pod};
use crate::vttcss::Stylesheet;

/// The family `subparse` emits and `suboverlay` consumes.
pub const CUE_IR_FAMILY: &str = "subtitle/cue-ir";
/// The 4-byte magic prefixing a payload.
pub const MAGIC: [u8; 4] = *b"CUIR";
/// Layout version.
pub const VERSION: u16 = 2;
/// An absent index.
pub const NONE: u32 = u32::MAX;
/// Alignment of the payload start and of every section.
pub const ALIGN: usize = 8;
/// Header length in bytes.
pub const HEADER_LEN: usize = std::mem::size_of::<CueHeader>();

/// An sRGB color with straight (non-premultiplied) alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const WHITE: Color = Color::rgb(0xff, 0xff, 0xff);
    pub const BLACK: Color = Color::rgb(0x00, 0x00, 0x00);
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 0xff }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// Packed `0xRRGGBBAA`, the wire form.
    pub const fn to_u32(self) -> u32 {
        (self.r as u32) << 24 | (self.g as u32) << 16 | (self.b as u32) << 8 | self.a as u32
    }

    pub const fn from_u32(v: u32) -> Color {
        Color::rgba((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// Parse a color the way `pango_color_parse` does.
    pub fn parse(s: &str) -> Option<Color> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            return Color::parse_hex(hex);
        }
        named_color(s)
    }

    fn parse_hex(hex: &str) -> Option<Color> {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        // 1..=4 digits per channel, as in pango.
        let per = match hex.len() {
            3 => 1,
            6 => 2,
            9 => 3,
            12 => 4,
            _ => return None,
        };
        let chan = |i: usize| -> u8 {
            let v = u32::from_str_radix(&hex[i * per..(i + 1) * per], 16).unwrap_or(0);
            // Top byte of the value left-aligned to 16 bits, as pango does.
            let max = (1u32 << (4 * per)) - 1;
            ((v * 255 + max / 2) / max) as u8
        };
        Some(Color::rgb(chan(0), chan(1), chan(2)))
    }
}

/// The CSS3 / X11 named colors `pango_color_parse` accepts, ASCII case-insensitive, sorted for
/// binary search.
pub const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

/// `a` (lowercase ASCII) against `b` folded to lowercase, no allocation.
fn cmp_ignore_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

pub(crate) fn named_color(name: &str) -> Option<Color> {
    NAMED_COLORS.binary_search_by(|(n, _)| cmp_ignore_case(n, name)).ok().map(|i| {
        let v = NAMED_COLORS[i].1;
        Color::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
}

impl FontStyle {
    const fn code(self) -> u8 {
        self as u8
    }
    const fn of(v: u8) -> Option<FontStyle> {
        Some(match v {
            0 => FontStyle::Normal,
            1 => FontStyle::Italic,
            2 => FontStyle::Oblique,
            _ => return None,
        })
    }
}

/// A font size, either absolute or relative to the renderer's base size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontSize {
    Points(f32),
    Scale(f32),
    FrameHeightPercent(f32),
}

/// The keyword size ladder, steps of 1.2 around `medium` (index 3). Literals, not `powi`, since
/// const-folded and runtime `powi` differ by an ULP.
pub const SIZE_LADDER: [f32; 8] = [0.5787037, 0.6944444, 0.8333333, 1.0, 1.2, 1.44, 1.728, 2.0736];

/// `steps` of 1.2 from `medium` as a [`FontSize::Scale`], clamped to the ladder.
pub fn size_step(steps: i32) -> FontSize {
    FontSize::Scale(SIZE_LADDER[(steps + 3).clamp(0, 7) as usize])
}

/// A text outline (SSA/ASS border, CC edge).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outline {
    pub color: Color,
    /// Stroke width in points.
    pub width: f32,
}

/// A drop shadow behind the text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub color: Color,
    /// Offset in points, positive right/down.
    pub dx: f32,
    pub dy: f32,
    /// Gaussian blur radius in points (`0.0` = hard shadow).
    pub blur: f32,
}

/// Where a ruby annotation is drawn relative to its base text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RubyPosition {
    #[default]
    Over,
    Under,
}

/// Block progression direction (WebVTT `vertical` setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritingMode {
    #[default]
    HorizontalTb,
    VerticalRl,
    VerticalLr,
}

impl WritingMode {
    const fn of(v: u8) -> Option<WritingMode> {
        Some(match v {
            0 => WritingMode::HorizontalTb,
            1 => WritingMode::VerticalRl,
            2 => WritingMode::VerticalLr,
            _ => return None,
        })
    }
}

/// Text alignment within the cue box (WebVTT `align`, SSA style alignment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Start,
    Center,
    End,
    Left,
    Right,
}

impl TextAlign {
    const fn of(v: u8) -> Option<TextAlign> {
        Some(match v {
            0 => TextAlign::Start,
            1 => TextAlign::Center,
            2 => TextAlign::End,
            3 => TextAlign::Left,
            4 => TextAlign::Right,
            _ => return None,
        })
    }
}

/// Which edge of the cue box the `line` position pins (WebVTT `line` align).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineAlign {
    Start,
    Center,
    End,
}

impl LineAlign {
    const fn of(v: u8) -> Option<LineAlign> {
        Some(match v {
            0 => LineAlign::Start,
            1 => LineAlign::Center,
            2 => LineAlign::End,
            _ => return None,
        })
    }
}

/// Alignment of the cue box around its `position` (WebVTT position align).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionAlign {
    Auto,
    LineLeft,
    Center,
    LineRight,
}

impl PositionAlign {
    const fn of(v: u8) -> Option<PositionAlign> {
        Some(match v {
            0 => PositionAlign::Auto,
            1 => PositionAlign::LineLeft,
            2 => PositionAlign::Center,
            3 => PositionAlign::LineRight,
            _ => return None,
        })
    }
}

/// The WebVTT `line` setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinePosition {
    Percent(f32),
    Line(i32),
}

/// A 9-way anchor (SSA/ASS numpad alignment, CEA-708 anchor points), numpad order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl Anchor {
    const fn of(v: u8) -> Option<Anchor> {
        Some(match v {
            0 => Anchor::TopLeft,
            1 => Anchor::TopCenter,
            2 => Anchor::TopRight,
            3 => Anchor::CenterLeft,
            4 => Anchor::Center,
            5 => Anchor::CenterRight,
            6 => Anchor::BottomLeft,
            7 => Anchor::BottomCenter,
            8 => Anchor::BottomRight,
            _ => return None,
        })
    }
}

/// Distances from the video frame edges the cue must keep clear, as percentages of the frame in
/// `[0, 100]` (SSA margins, normalised out of `PlayRes` space by the parser).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Margins {
    pub left: f32,
    pub right: f32,
    pub vertical: f32,
}

// ---------------------------------------------------------------------------------------------
// Wire records. `#[repr(C)]`, little endian, every padding byte a field.

/// A byte range of the string region.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StrRef {
    pub off: u32,
    pub len: u32,
}

impl StrRef {
    pub const EMPTY: StrRef = StrRef { off: 0, len: 0 };

    const fn end(self) -> Option<u32> {
        self.off.checked_add(self.len)
    }
}

/// A visual line, a run of the span table.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineRec {
    pub first_span: u32,
    pub span_count: u32,
}

/// `SpanRec::flags` bits.
pub const SPAN_VOICE: u32 = 1 << 0;
pub const SPAN_REVEAL: u32 = 1 << 1;
const SPAN_ALL: u32 = SPAN_VOICE | SPAN_REVEAL;

/// A run of text with one style.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpanRec {
    /// Karaoke reveal time on the cue's timeline, valid with `SPAN_REVEAL`.
    pub reveal_ns: u64,
    pub text: StrRef,
    /// The speaker of a WebVTT voice span, valid with `SPAN_VOICE`.
    pub voice: StrRef,
    /// A run of the class table.
    pub class_first: u32,
    pub class_count: u32,
    /// Index into the style table.
    pub style: u32,
    /// Index into the ruby table, or `NONE`.
    pub ruby: u32,
    pub flags: u32,
    pub pad: u32,
}

impl SpanRec {
    pub fn reveal_ns(&self) -> Option<u64> {
        (self.flags & SPAN_REVEAL != 0).then_some(self.reveal_ns)
    }
    pub fn has_voice(&self) -> bool {
        self.flags & SPAN_VOICE != 0
    }
    pub fn has_ruby(&self) -> bool {
        self.ruby != NONE
    }
}

/// A ruby annotation attached to a base span.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RubyRec {
    pub text: StrRef,
    /// 0 over, 1 under.
    pub position: u32,
    pub pad: u32,
}

impl RubyRec {
    pub fn position(&self) -> RubyPosition {
        if self.position == 1 { RubyPosition::Under } else { RubyPosition::Over }
    }
}

/// `StyleRec::present` bits, one per optional field.
pub const ST_FAMILY: u32 = 1 << 0;
pub const ST_SIZE: u32 = 1 << 1;
pub const ST_STYLE: u32 = 1 << 2;
pub const ST_WEIGHT: u32 = 1 << 3;
pub const ST_UNDERLINE: u32 = 1 << 4;
pub const ST_STRIKE: u32 = 1 << 5;
pub const ST_FG: u32 = 1 << 6;
pub const ST_BG: u32 = 1 << 7;
pub const ST_OUTLINE: u32 = 1 << 8;
pub const ST_SHADOW: u32 = 1 << 9;
pub const ST_SPACING: u32 = 1 << 10;
pub const ST_SHIFT: u32 = 1 << 11;
pub const ST_SCALE: u32 = 1 << 12;
pub const ST_LANGUAGE: u32 = 1 << 13;
const ST_ALL: u32 = (1 << 14) - 1;

/// Character styling for one span. A field reads as `None` where its `present` bit is clear,
/// then the span inherits the cue's base style and the renderer's defaults.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StyleRec {
    pub present: u32,
    /// CSS-style weight, 400 normal and 700 bold.
    pub font_weight: u16,
    /// 0 points, 1 scale, 2 frame-height percent.
    pub size_kind: u8,
    /// 0 normal, 1 italic, 2 oblique.
    pub font_style: u8,
    /// Font family name (a parley font stack entry).
    pub family: StrRef,
    /// BCP-47 language tag for shaping/line breaking (WebVTT `<lang>`, pango `lang`).
    pub language: StrRef,
    pub font_size: f32,
    /// Text (fill) color, packed.
    pub foreground: u32,
    /// Background box painted behind this span's glyphs, packed.
    pub background: u32,
    pub outline_color: u32,
    pub outline_width: f32,
    pub shadow_color: u32,
    pub shadow_dx: f32,
    pub shadow_dy: f32,
    pub shadow_blur: f32,
    /// Additional letter spacing in points (SSA `\fsp`, pango `letter_spacing`).
    pub letter_spacing: f32,
    /// Vertical baseline displacement in points, positive raised (pango `rise`, sub/superscript).
    pub baseline_shift: f32,
    /// Horizontal/vertical glyph scale factors (SSA `\fscx`/`\fscy`, `1.0` unscaled).
    pub scale_x: f32,
    pub scale_y: f32,
    /// 0 false, 1 true.
    pub underline: u8,
    pub strikethrough: u8,
    pub pad: [u8; 2],
}

macro_rules! opt_field {
    ($get:ident, $set:ident, $bit:ident, $t:ty, |$s:ident| $read:expr, |$w:ident, $v:ident| $write:block) => {
        pub fn $get(&self) -> Option<$t> {
            let $s = self;
            (self.present & $bit != 0).then(|| $read)
        }
        pub fn $set(&mut self, v: Option<$t>) {
            match v {
                Some($v) => {
                    self.present |= $bit;
                    let $w = self;
                    $write
                }
                None => self.present &= !$bit,
            }
        }
    };
}

impl StyleRec {
    /// Whether every field is absent (the span adds nothing over the base).
    pub fn is_plain(&self) -> bool {
        self.present == 0
    }

    pub fn has_family(&self) -> bool {
        self.present & ST_FAMILY != 0
    }
    pub fn has_language(&self) -> bool {
        self.present & ST_LANGUAGE != 0
    }
    pub fn family_ref(&self) -> Option<StrRef> {
        self.has_family().then_some(self.family)
    }
    pub fn language_ref(&self) -> Option<StrRef> {
        self.has_language().then_some(self.language)
    }
    pub fn set_family(&mut self, r: Option<StrRef>) {
        match r {
            Some(r) => (self.present, self.family) = (self.present | ST_FAMILY, r),
            None => self.present &= !ST_FAMILY,
        }
    }
    pub fn set_language(&mut self, r: Option<StrRef>) {
        match r {
            Some(r) => (self.present, self.language) = (self.present | ST_LANGUAGE, r),
            None => self.present &= !ST_LANGUAGE,
        }
    }

    opt_field!(
        font_size,
        set_font_size,
        ST_SIZE,
        FontSize,
        |s| match s.size_kind {
            0 => FontSize::Points(s.font_size),
            1 => FontSize::Scale(s.font_size),
            _ => FontSize::FrameHeightPercent(s.font_size),
        },
        |r, v| {
            (r.size_kind, r.font_size) = match v {
                FontSize::Points(p) => (0, p),
                FontSize::Scale(p) => (1, p),
                FontSize::FrameHeightPercent(p) => (2, p),
            };
        }
    );
    opt_field!(
        font_style,
        set_font_style,
        ST_STYLE,
        FontStyle,
        |s| FontStyle::of(s.font_style).unwrap_or_default(),
        |r, v| {
            r.font_style = v.code();
        }
    );
    opt_field!(font_weight, set_font_weight, ST_WEIGHT, u16, |s| s.font_weight, |r, v| {
        r.font_weight = v;
    });
    opt_field!(underline, set_underline, ST_UNDERLINE, bool, |s| s.underline != 0, |r, v| {
        r.underline = v as u8;
    });
    opt_field!(
        strikethrough,
        set_strikethrough,
        ST_STRIKE,
        bool,
        |s| s.strikethrough != 0,
        |r, v| {
            r.strikethrough = v as u8;
        }
    );
    opt_field!(
        foreground,
        set_foreground,
        ST_FG,
        Color,
        |s| Color::from_u32(s.foreground),
        |r, v| {
            r.foreground = v.to_u32();
        }
    );
    opt_field!(
        background,
        set_background,
        ST_BG,
        Color,
        |s| Color::from_u32(s.background),
        |r, v| {
            r.background = v.to_u32();
        }
    );
    opt_field!(
        outline,
        set_outline,
        ST_OUTLINE,
        Outline,
        |s| Outline { color: Color::from_u32(s.outline_color), width: s.outline_width },
        |r, v| {
            (r.outline_color, r.outline_width) = (v.color.to_u32(), v.width);
        }
    );
    opt_field!(
        shadow,
        set_shadow,
        ST_SHADOW,
        Shadow,
        |s| Shadow {
            color: Color::from_u32(s.shadow_color),
            dx: s.shadow_dx,
            dy: s.shadow_dy,
            blur: s.shadow_blur,
        },
        |r, v| {
            (r.shadow_color, r.shadow_dx, r.shadow_dy, r.shadow_blur) =
                (v.color.to_u32(), v.dx, v.dy, v.blur);
        }
    );
    opt_field!(
        letter_spacing,
        set_letter_spacing,
        ST_SPACING,
        f32,
        |s| s.letter_spacing,
        |r, v| {
            r.letter_spacing = v;
        }
    );
    opt_field!(baseline_shift, set_baseline_shift, ST_SHIFT, f32, |s| s.baseline_shift, |r, v| {
        r.baseline_shift = v;
    });
    opt_field!(scale, set_scale, ST_SCALE, (f32, f32), |s| (s.scale_x, s.scale_y), |r, v| {
        (r.scale_x, r.scale_y) = v;
    });

    /// The same style with every absent field zeroed. A non-finite field reads as absent, text
    /// like `size="NaN"` parses but never reaches the wire.
    pub fn canonical(&self) -> StyleRec {
        let mut c = StyleRec::default();
        c.merge(self);
        for (bit, v) in c.floats() {
            if !(v[0].is_finite() && v[1].is_finite()) {
                c.present &= !bit;
            }
        }
        let mut z = StyleRec::default();
        z.merge(&c);
        z
    }

    /// Each float-carrying field group with its present bit.
    fn floats(&self) -> [(u32, [f32; 2]); 7] {
        [
            (ST_SIZE, [self.font_size, 0.0]),
            (ST_OUTLINE, [self.outline_width, 0.0]),
            (ST_SHADOW, [self.shadow_dx, self.shadow_dy]),
            (ST_SHADOW, [self.shadow_blur, 0.0]),
            (ST_SPACING, [self.letter_spacing, 0.0]),
            (ST_SHIFT, [self.baseline_shift, 0.0]),
            (ST_SCALE, [self.scale_x, self.scale_y]),
        ]
    }

    /// Overwrite every field `src` has, its strings already in this record's region.
    pub fn merge(&mut self, src: &StyleRec) {
        macro_rules! take {
            ($($bit:ident => [$($f:ident),*]),* $(,)?) => {
                $(if src.present & $bit != 0 { $(self.$f = src.$f;)* self.present |= $bit; })*
            };
        }
        take!(
            ST_FAMILY => [family],
            ST_SIZE => [size_kind, font_size],
            ST_STYLE => [font_style],
            ST_WEIGHT => [font_weight],
            ST_UNDERLINE => [underline],
            ST_STRIKE => [strikethrough],
            ST_FG => [foreground],
            ST_BG => [background],
            ST_OUTLINE => [outline_color, outline_width],
            ST_SHADOW => [shadow_color, shadow_dx, shadow_dy, shadow_blur],
            ST_SPACING => [letter_spacing],
            ST_SHIFT => [baseline_shift],
            ST_SCALE => [scale_x, scale_y],
            ST_LANGUAGE => [language],
        );
    }

    /// [`StyleRec::merge`] across string regions, `src`'s names are re-interned into `into`.
    pub fn merge_from(&mut self, src: &StyleRec, from: &Names, into: &mut Names) {
        let mut copy = *src;
        if src.has_family() {
            copy.family = into.intern(from.get(src.family));
        }
        if src.has_language() {
            copy.language = into.intern(from.get(src.language));
        }
        self.merge(&copy);
    }

    fn validate(&self) -> Result<(), WireError> {
        if self.present & !ST_ALL != 0 {
            return Err(WireError::Enum("style present"));
        }
        let has = |bit| self.present & bit != 0;
        if has(ST_SIZE) && self.size_kind > 2 {
            return Err(WireError::Enum("font size kind"));
        }
        if has(ST_STYLE) && FontStyle::of(self.font_style).is_none() {
            return Err(WireError::Enum("font style"));
        }
        if (has(ST_UNDERLINE) && self.underline > 1) || (has(ST_STRIKE) && self.strikethrough > 1) {
            return Err(WireError::Enum("underline"));
        }
        // A present float is finite, the renderer never sees NaN or inf.
        let finite = self.floats();
        if finite.iter().any(|(bit, v)| has(*bit) && !(v[0].is_finite() && v[1].is_finite())) {
            return Err(WireError::Enum("style float"));
        }
        Ok(())
    }
}

/// `LayoutRec::present` bits.
pub const LY_LINE: u32 = 1 << 0;
pub const LY_LINE_ALIGN: u32 = 1 << 1;
pub const LY_POSITION: u32 = 1 << 2;
pub const LY_POSITION_ALIGN: u32 = 1 << 3;
pub const LY_SIZE: u32 = 1 << 4;
pub const LY_ALIGN: u32 = 1 << 5;
pub const LY_ANCHOR: u32 = 1 << 6;
pub const LY_ORIGIN: u32 = 1 << 7;
pub const LY_MARGINS: u32 = 1 << 8;
pub const LY_BACKGROUND: u32 = 1 << 9;
const LY_ALL: u32 = (1 << 10) - 1;

/// Cue-level placement.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutRec {
    pub present: u32,
    /// `f32` bits (percent) with `line_kind` 0, `i32` bits (line number) with 1.
    pub line_bits: u32,
    /// Where the cue box sits in the inline direction in percent (WebVTT `position`).
    pub position: f32,
    /// Extent of the cue box in the inline direction in percent (WebVTT `size`).
    pub size: f32,
    /// Explicit anchor-point position (SSA `\pos`), percent of the frame.
    pub origin_x: f32,
    pub origin_y: f32,
    pub margin_left: f32,
    pub margin_right: f32,
    pub margin_vertical: f32,
    /// Fill behind the whole cue box (SSA `BorderStyle=3`, CC window color), packed.
    pub background: u32,
    /// 0 horizontal-tb, 1 vertical-rl, 2 vertical-lr.
    pub writing_mode: u8,
    pub line_kind: u8,
    /// 0 start, 1 center, 2 end.
    pub line_align: u8,
    /// 0 auto, 1 line-left, 2 center, 3 line-right.
    pub position_align: u8,
    /// 0 start, 1 center, 2 end, 3 left, 4 right.
    pub align: u8,
    /// Numpad order, 0 top-left to 8 bottom-right.
    pub anchor: u8,
    pub pad: [u8; 2],
}

impl LayoutRec {
    pub fn writing_mode(&self) -> WritingMode {
        WritingMode::of(self.writing_mode).unwrap_or_default()
    }
    pub fn set_writing_mode(&mut self, v: WritingMode) {
        self.writing_mode = v as u8;
    }

    opt_field!(
        line,
        set_line,
        LY_LINE,
        LinePosition,
        |s| if s.line_kind == 1 {
            LinePosition::Line(s.line_bits as i32)
        } else {
            LinePosition::Percent(f32::from_bits(s.line_bits))
        },
        |r, v| {
            (r.line_kind, r.line_bits) = match v {
                LinePosition::Percent(p) => (0, p.to_bits()),
                LinePosition::Line(n) => (1, n as u32),
            };
        }
    );
    opt_field!(
        line_align,
        set_line_align,
        LY_LINE_ALIGN,
        LineAlign,
        |s| LineAlign::of(s.line_align).unwrap_or(LineAlign::Start),
        |r, v| {
            r.line_align = v as u8;
        }
    );
    opt_field!(position, set_position, LY_POSITION, f32, |s| s.position, |r, v| {
        r.position = v;
    });
    opt_field!(
        position_align,
        set_position_align,
        LY_POSITION_ALIGN,
        PositionAlign,
        |s| PositionAlign::of(s.position_align).unwrap_or(PositionAlign::Auto),
        |r, v| {
            r.position_align = v as u8;
        }
    );
    opt_field!(size, set_size, LY_SIZE, f32, |s| s.size, |r, v| {
        r.size = v;
    });
    opt_field!(
        align,
        set_align,
        LY_ALIGN,
        TextAlign,
        |s| TextAlign::of(s.align).unwrap_or(TextAlign::Start),
        |r, v| {
            r.align = v as u8;
        }
    );
    opt_field!(
        anchor,
        set_anchor,
        LY_ANCHOR,
        Anchor,
        |s| Anchor::of(s.anchor).unwrap_or(Anchor::BottomCenter),
        |r, v| {
            r.anchor = v as u8;
        }
    );
    opt_field!(origin, set_origin, LY_ORIGIN, (f32, f32), |s| (s.origin_x, s.origin_y), |r, v| {
        (r.origin_x, r.origin_y) = v;
    });
    opt_field!(
        margins,
        set_margins,
        LY_MARGINS,
        Margins,
        |s| Margins { left: s.margin_left, right: s.margin_right, vertical: s.margin_vertical },
        |r, v| {
            (r.margin_left, r.margin_right, r.margin_vertical) = (v.left, v.right, v.vertical);
        }
    );
    opt_field!(
        background,
        set_background,
        LY_BACKGROUND,
        Color,
        |s| Color::from_u32(s.background),
        |r, v| {
            r.background = v.to_u32();
        }
    );

    /// The same layout with every absent field zeroed. A non-finite field reads as absent.
    pub fn canonical(&self) -> LayoutRec {
        let finite = |v: Option<f32>| v.filter(|f| f.is_finite());
        let mut c = LayoutRec::default();
        c.set_writing_mode(self.writing_mode());
        c.set_line(self.line().filter(|l| match l {
            LinePosition::Percent(p) => p.is_finite(),
            LinePosition::Line(_) => true,
        }));
        c.set_line_align(self.line_align());
        c.set_position(finite(self.position()));
        c.set_position_align(self.position_align());
        c.set_size(finite(self.size()));
        c.set_align(self.align());
        c.set_anchor(self.anchor());
        c.set_origin(self.origin().filter(|(x, y)| x.is_finite() && y.is_finite()));
        c.set_margins(
            self.margins().filter(|m| [m.left, m.right, m.vertical].iter().all(|f| f.is_finite())),
        );
        c.set_background(self.background());
        c
    }

    fn validate(&self) -> Result<(), WireError> {
        if self.present & !LY_ALL != 0 {
            return Err(WireError::Enum("layout present"));
        }
        let has = |bit| self.present & bit != 0;
        if WritingMode::of(self.writing_mode).is_none() {
            return Err(WireError::Enum("writing mode"));
        }
        if has(LY_LINE) && self.line_kind > 1 {
            return Err(WireError::Enum("line kind"));
        }
        if has(LY_LINE_ALIGN) && LineAlign::of(self.line_align).is_none() {
            return Err(WireError::Enum("line align"));
        }
        if has(LY_POSITION_ALIGN) && PositionAlign::of(self.position_align).is_none() {
            return Err(WireError::Enum("position align"));
        }
        if has(LY_ALIGN) && TextAlign::of(self.align).is_none() {
            return Err(WireError::Enum("align"));
        }
        if has(LY_ANCHOR) && Anchor::of(self.anchor).is_none() {
            return Err(WireError::Enum("anchor"));
        }
        let line = if self.line_kind == 0 { f32::from_bits(self.line_bits) } else { 0.0 };
        let finite = [
            (LY_LINE, [line, 0.0]),
            (LY_POSITION, [self.position, 0.0]),
            (LY_SIZE, [self.size, 0.0]),
            (LY_ORIGIN, [self.origin_x, self.origin_y]),
            (LY_MARGINS, [self.margin_left, self.margin_right]),
            (LY_MARGINS, [self.margin_vertical, 0.0]),
        ];
        if finite.iter().any(|(bit, v)| has(*bit) && !(v[0].is_finite() && v[1].is_finite())) {
            return Err(WireError::Enum("layout float"));
        }
        Ok(())
    }
}

/// The payload header. Section offsets are from the payload start, 8 byte aligned.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CueHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub header_len: u16,
    pub total_len: u32,
    pub n_lines: u32,
    pub n_spans: u32,
    pub n_rubies: u32,
    pub n_classes: u32,
    pub n_styles: u32,
    pub off_lines: u32,
    pub off_spans: u32,
    pub off_rubies: u32,
    pub off_classes: u32,
    pub off_styles: u32,
    pub off_strings: u32,
    pub len_strings: u32,
    /// Index into the style table of what spans inherit (SSA style defaults).
    pub base_style: u32,
    pub layout: LayoutRec,
}

#[allow(unsafe_code)]
mod pod_impls {
    use super::*;
    // SAFETY: each is `#[repr(C)]`, built of integers, floats and `Pod` records, with every
    // padding byte declared, which the size assertions below pin.
    unsafe impl Pod for StrRef {}
    unsafe impl Pod for LineRec {}
    unsafe impl Pod for SpanRec {}
    unsafe impl Pod for RubyRec {}
    unsafe impl Pod for StyleRec {}
    unsafe impl Pod for LayoutRec {}
    unsafe impl Pod for CueHeader {}
}

const _: () = {
    use std::mem::{align_of, size_of};
    assert!(size_of::<StrRef>() == 8);
    assert!(size_of::<LineRec>() == 8);
    assert!(size_of::<SpanRec>() == 48);
    assert!(size_of::<RubyRec>() == 16);
    assert!(size_of::<StyleRec>() == 80);
    assert!(size_of::<LayoutRec>() == 48);
    assert!(size_of::<CueHeader>() == 112);
    assert!(align_of::<SpanRec>() == ALIGN);
    assert!(align_of::<CueHeader>() <= ALIGN);
    assert!(HEADER_LEN.is_multiple_of(ALIGN));
};

/// A growing UTF-8 region of interned names, equal strings share one [`StrRef`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    buf: Vec<u8>,
    /// Open addressing over the interned refs, a power of two long, `len` 0 is a free slot.
    index: Vec<StrRef>,
    count: usize,
}

fn hash_bytes(s: &[u8]) -> u64 {
    // FNV-1a.
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in s {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl Names {
    pub fn clear(&mut self) {
        self.buf.clear();
        self.index.fill(StrRef::EMPTY);
        self.count = 0;
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// `s`, appended unless an equal name was interned before. One hash and one probe run per
    /// call, so a cue of many names costs its length, not its square.
    pub fn intern(&mut self, s: &str) -> StrRef {
        if s.is_empty() {
            return StrRef::EMPTY;
        }
        if (self.count + 1) * 2 > self.index.len() {
            self.grow();
        }
        let mask = self.index.len() - 1;
        let mut i = hash_bytes(s.as_bytes()) as usize & mask;
        loop {
            let r = self.index[i];
            if r.len == 0 {
                break;
            }
            if self.get(r).as_bytes() == s.as_bytes() {
                return r;
            }
            i = (i + 1) & mask;
        }
        let off = self.buf.len() as u32;
        // Bounded scratch, the region grows to the longest cue seen and is reused.
        #[allow(clippy::disallowed_methods)]
        self.buf.extend_from_slice(s.as_bytes());
        let r = StrRef { off, len: s.len() as u32 };
        self.index[i] = r;
        self.count += 1;
        r
    }

    /// Double the index, keeping the load under a half.
    // Grows to the most names one cue held and is reused.
    #[allow(clippy::disallowed_methods)]
    fn grow(&mut self) {
        let n = (self.index.len() * 2).max(16);
        let old = std::mem::replace(&mut self.index, vec![StrRef::EMPTY; n]);
        let mask = n - 1;
        for r in old.into_iter().filter(|r| r.len > 0) {
            let mut i = hash_bytes(self.get(r).as_bytes()) as usize & mask;
            while self.index[i].len > 0 {
                i = (i + 1) & mask;
            }
            self.index[i] = r;
        }
    }

    /// The string behind a ref this region handed out.
    pub fn get(&self, r: StrRef) -> &str {
        let bytes = self.buf.get(r.off as usize..(r.off as usize).saturating_add(r.len as usize));
        bytes.and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("")
    }
}

/// Why a cue could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    DoesNotFit { capacity: usize },
    TooMany,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::DoesNotFit { capacity } => {
                write!(f, "cue does not fit a {capacity} byte slot")
            }
            EncodeError::TooMany => f.write_str("cue has more parts than the layout counts"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Why a payload was refused by [`CueIr::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    Short { have: usize },
    Misaligned,
    Magic,
    Version(u16),
    Length { stated: u32, have: usize },
    Section(&'static str),
    Strings,
    StringRef(&'static str),
    Index(&'static str),
    Enum(&'static str),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Short { have } => write!(f, "cue-ir payload of {have} bytes, no header"),
            WireError::Misaligned => write!(f, "cue-ir payload not {ALIGN} byte aligned"),
            WireError::Magic => f.write_str("cue-ir magic missing"),
            WireError::Version(v) => write!(f, "cue-ir version {v}, want {VERSION}"),
            WireError::Length { stated, have } => {
                write!(f, "cue-ir states {stated} bytes, buffer has {have}")
            }
            WireError::Section(what) => write!(f, "cue-ir {what} table out of bounds"),
            WireError::Strings => f.write_str("cue-ir string region is not UTF-8"),
            WireError::StringRef(what) => write!(f, "cue-ir {what} string reference invalid"),
            WireError::Index(what) => write!(f, "cue-ir {what} index out of range"),
            WireError::Enum(what) => write!(f, "cue-ir {what} names no variant"),
        }
    }
}

impl std::error::Error for WireError {}

// ---------------------------------------------------------------------------------------------
// Builder.

/// One markup nesting level the parser can return to.
#[derive(Clone, Copy, Default)]
struct Frame {
    /// The tag's name as a byte range of the input.
    name: (u32, u32),
    ctx: MarkupCtx,
}

/// Inherited markup state, plain data so a frame is a copy.
#[derive(Clone, Copy, Default)]
struct MarkupCtx {
    style: StyleRec,
    voice: StrRef,
    has_voice: bool,
    in_ruby: bool,
    in_rt: bool,
    /// The inherited `<lang>` and `<v>` annotations as byte ranges of the input, `len` 0 for
    /// none, what the stylesheet matches against.
    lang_in: (u32, u32),
    voice_in: (u32, u32),
    /// Classes on the stack at this depth.
    classes: u32,
    ruby_position: RubyPosition,
}

/// The bytes a builder lays out before it refuses the cue, the element's slot. A 300-syllable
/// karaoke line sharing one style fits.
pub const CUE_LIMIT: usize = 16 * 1024;

/// Open markup tags past this depth are dropped, the C's SAMI guard.
pub const MAX_MARKUP_DEPTH: usize = 64;

/// Classes one span can carry, deeper stacks are not copied per span.
pub const MAX_CLASSES: usize = 64;

/// Lays one cue out record by record, then writes it into a slot. Every table is bounded
/// scratch owned by the element, reused across cues, so steady state allocates nothing. A cue
/// past `limit` bytes stops growing at once and is refused by [`CueBuilder::finish`], so the
/// work on a hostile cue is bounded by the slot, not the input.
pub struct CueBuilder {
    pub layout: LayoutRec,
    pub base: StyleRec,
    lines: Vec<LineRec>,
    spans: Vec<SpanRec>,
    rubies: Vec<RubyRec>,
    classes: Vec<StrRef>,
    styles: Vec<StyleRec>,
    /// Span and ruby texts, in order.
    text: Vec<u8>,
    /// Interned voices, classes, families and languages, appended behind `text` on the wire.
    names: Names,
    line_first: u32,
    limit: usize,
    overflow: bool,
    // Parser scratch.
    cur_text: Vec<u8>,
    rt_text: Vec<u8>,
    class_stack: Vec<StrRef>,
    frames: Vec<Frame>,
}

impl Default for CueBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// Scratch tables grow to the largest cue seen and are reused, no steady-state allocation.
#[allow(clippy::disallowed_methods)]
impl CueBuilder {
    pub fn new() -> CueBuilder {
        CueBuilder {
            layout: LayoutRec::default(),
            base: StyleRec::default(),
            lines: Vec::new(),
            spans: Vec::new(),
            rubies: Vec::new(),
            classes: Vec::new(),
            styles: Vec::new(),
            text: Vec::new(),
            names: Names::default(),
            line_first: 0,
            limit: CUE_LIMIT,
            overflow: false,
            cur_text: Vec::new(),
            rt_text: Vec::new(),
            class_stack: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// The bytes a cue may lay out, [`CUE_LIMIT`] unless set.
    pub fn set_limit(&mut self, bytes: usize) {
        self.limit = bytes;
    }

    /// Whether the cue under construction passed the limit and will be refused.
    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Forget the current cue, keeping the scratch capacity.
    pub fn reset(&mut self) {
        self.layout = LayoutRec::default();
        self.base = StyleRec::default();
        self.lines.clear();
        self.spans.clear();
        self.rubies.clear();
        self.classes.clear();
        self.styles.clear();
        self.text.clear();
        self.names.clear();
        self.line_first = 0;
        self.overflow = false;
        self.cur_text.clear();
        self.rt_text.clear();
        self.class_stack.clear();
        self.frames.clear();
    }

    /// An upper bound on the wire length so far, counted from table lengths alone.
    fn laid_out(&self) -> usize {
        use std::mem::size_of;
        HEADER_LEN
            + self.lines.len() * size_of::<LineRec>()
            + self.spans.len() * size_of::<SpanRec>()
            + self.rubies.len() * size_of::<RubyRec>()
            + self.classes.len() * size_of::<StrRef>()
            + (self.styles.len() + 1) * size_of::<StyleRec>()
            + self.text.len()
            + self.names.len()
            + self.cur_text.len()
            + self.rt_text.len()
    }

    /// Whether `extra` more bytes pass the limit, latching the overflow.
    fn over(&mut self, extra: usize) -> bool {
        if !self.overflow && self.laid_out().saturating_add(extra) > self.limit {
            self.overflow = true;
        }
        self.overflow
    }

    pub fn names(&mut self) -> &mut Names {
        &mut self.names
    }

    /// Intern a voice, class, family or language.
    pub fn intern(&mut self, s: &str) -> StrRef {
        if self.over(s.len()) {
            return StrRef::EMPTY;
        }
        self.names.intern(s)
    }

    /// Text of the span being collected.
    pub fn push_text(&mut self, s: &str) {
        if self.over(s.len()) {
            return;
        }
        self.cur_text.extend_from_slice(s.as_bytes());
    }

    pub fn push_char(&mut self, c: char) {
        let mut b = [0u8; 4];
        self.push_text(c.encode_utf8(&mut b));
    }

    /// Text of the ruby annotation being collected.
    fn push_rt_text(&mut self, s: &str) {
        if self.over(s.len()) {
            return;
        }
        self.rt_text.extend_from_slice(s.as_bytes());
    }

    pub fn push_class(&mut self, class: StrRef) {
        if self.class_stack.len() >= MAX_CLASSES {
            return;
        }
        self.class_stack.push(class);
    }

    pub fn class_depth(&self) -> u32 {
        self.class_stack.len() as u32
    }

    pub fn truncate_classes(&mut self, depth: u32) {
        self.class_stack.truncate(depth as usize);
    }

    /// The index of `style` in the deduplicated table. Absent fields are zeroed first, so two
    /// styles that read the same share one record, and bytes decide, so a NaN dedups too.
    pub fn style_id(&mut self, style: &StyleRec) -> u32 {
        let style = style.canonical();
        // The last span's style first, a run of same-styled flushes never scans the table.
        if let Some(last) = self.spans.last()
            && self.styles.get(last.style as usize).is_some_and(|s| same_bytes(s, &style))
        {
            return last.style;
        }
        if let Some(at) = self.styles.iter().position(|s| same_bytes(s, &style)) {
            return at as u32;
        }
        self.styles.push(style);
        (self.styles.len() - 1) as u32
    }

    /// Close the span being collected. Text that continues the last span of the open line with
    /// the same styling extends it instead.
    pub fn flush(&mut self, style: &StyleRec, voice: Option<StrRef>, reveal_ns: Option<u64>) {
        if self.cur_text.is_empty() {
            return;
        }
        // A span, its classes and one style at most.
        let grows = std::mem::size_of::<SpanRec>()
            + self.class_stack.len() * std::mem::size_of::<StrRef>()
            + std::mem::size_of::<StyleRec>();
        if self.over(grows) {
            self.cur_text.clear();
            return;
        }
        let sid = self.style_id(style);
        let flags = voice.map_or(0, |_| SPAN_VOICE) | reveal_ns.map_or(0, |_| SPAN_REVEAL);
        let text_end = self.text.len() as u32;
        let in_line = self.spans.len() as u32 > self.line_first;
        if let Some(last) = self.spans.last_mut().filter(|_| in_line)
            && last.style == sid
            && last.flags == flags
            && (voice.is_none() || Some(last.voice) == voice)
            && last.reveal_ns == reveal_ns.unwrap_or(0)
            && last.ruby == NONE
            && last.text.off + last.text.len == text_end
            && self.classes[last.class_first as usize..][..last.class_count as usize]
                == self.class_stack[..]
        {
            last.text.len += self.cur_text.len() as u32;
            self.text.extend_from_slice(&self.cur_text);
            self.cur_text.clear();
            return;
        }
        let off = self.text.len() as u32;
        let len = self.cur_text.len() as u32;
        self.text.extend_from_slice(&self.cur_text);
        self.cur_text.clear();
        let class_first = self.classes.len() as u32;
        self.classes.extend_from_slice(&self.class_stack);
        self.spans.push(SpanRec {
            reveal_ns: reveal_ns.unwrap_or(0),
            text: StrRef { off, len },
            voice: voice.unwrap_or(StrRef::EMPTY),
            class_first,
            class_count: self.class_stack.len() as u32,
            style: sid,
            ruby: NONE,
            flags,
            pad: 0,
        });
    }

    /// Attach the collected ruby text to the last span of the open line, or to a new empty one.
    /// An empty annotation attaches nothing, like the markup it came from.
    fn attach_ruby(&mut self, position: RubyPosition) {
        if !self.rt_text.is_empty() {
            self.attach_ruby_always(position);
        }
    }

    fn attach_ruby_always(&mut self, position: RubyPosition) {
        let grows = std::mem::size_of::<RubyRec>() + std::mem::size_of::<SpanRec>();
        if self.over(grows) {
            self.rt_text.clear();
            return;
        }
        let off = self.text.len() as u32;
        let len = self.rt_text.len() as u32;
        self.text.extend_from_slice(&self.rt_text);
        self.rt_text.clear();
        let ruby = self.rubies.len() as u32;
        self.rubies.push(RubyRec { text: StrRef { off, len }, position: position as u32, pad: 0 });
        let in_line = self.spans.len() as u32 > self.line_first;
        if let Some(last) = self.spans.last_mut().filter(|_| in_line) {
            last.ruby = ruby;
        } else {
            let style = self.style_id(&StyleRec::default());
            let class_first = self.classes.len() as u32;
            self.spans.push(SpanRec { ruby, style, class_first, ..SpanRec::default() });
        }
    }

    /// End the open line, empty or not.
    pub fn end_line(&mut self) {
        if self.over(std::mem::size_of::<LineRec>()) {
            return;
        }
        let n = self.spans.len() as u32;
        self.lines.push(LineRec { first_span: self.line_first, span_count: n - self.line_first });
        self.line_first = n;
    }

    pub fn open_line_is_empty(&self) -> bool {
        self.spans.len() as u32 == self.line_first
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// End the open line unless it is empty and another line exists already.
    pub fn end_last_line(&mut self) {
        if !self.open_line_is_empty() || self.lines.is_empty() {
            self.end_line();
        }
    }

    /// The payload length [`CueBuilder::finish`] writes.
    pub fn wire_len(&self) -> usize {
        self.sections().5
    }

    /// Section offsets (lines, spans, rubies, classes, styles, strings) and the total.
    fn sections(&self) -> (usize, usize, usize, usize, usize, usize) {
        let off_lines = HEADER_LEN;
        let off_spans = off_lines + self.lines.len() * std::mem::size_of::<LineRec>();
        let off_rubies = off_spans + self.spans.len() * std::mem::size_of::<SpanRec>();
        let off_classes = off_rubies + self.rubies.len() * std::mem::size_of::<RubyRec>();
        let off_styles = off_classes + self.classes.len() * std::mem::size_of::<StrRef>();
        let base = self.base.canonical();
        let base_known = self.styles.iter().any(|s| same_bytes(s, &base));
        let n_styles = self.styles.len() + usize::from(!base_known);
        let off_strings = off_styles + n_styles * std::mem::size_of::<StyleRec>();
        let total = off_strings + self.text.len() + self.names.len();
        (off_lines, off_spans, off_rubies, off_classes, off_styles, total)
    }

    /// Write the cue into `out` and return the payload length. A cue that does not fit is refused
    /// whole. The builder keeps the cue, so a second call writes the same bytes.
    pub fn finish(&mut self, out: &mut [u8]) -> Result<usize, EncodeError> {
        if self.overflow {
            return Err(EncodeError::DoesNotFit { capacity: out.len().min(self.limit) });
        }
        let base = self.base;
        let base_style = self.style_id(&base);
        let (off_lines, off_spans, off_rubies, off_classes, off_styles, total) = self.sections();
        let off_strings = total - self.text.len() - self.names.len();
        if total > out.len() {
            return Err(EncodeError::DoesNotFit { capacity: out.len() });
        }
        let as_u32 = |n: usize| u32::try_from(n).map_err(|_| EncodeError::TooMany);
        as_u32(total)?;
        let tl = self.text.len() as u32;
        let header = CueHeader {
            magic: MAGIC,
            version: VERSION,
            header_len: HEADER_LEN as u16,
            total_len: total as u32,
            n_lines: self.lines.len() as u32,
            n_spans: self.spans.len() as u32,
            n_rubies: self.rubies.len() as u32,
            n_classes: self.classes.len() as u32,
            n_styles: self.styles.len() as u32,
            off_lines: off_lines as u32,
            off_spans: off_spans as u32,
            off_rubies: off_rubies as u32,
            off_classes: off_classes as u32,
            off_styles: off_styles as u32,
            off_strings: off_strings as u32,
            len_strings: (total - off_strings) as u32,
            base_style,
            layout: self.layout.canonical(),
        };
        out[..HEADER_LEN].copy_from_slice(pod::as_bytes(&header));
        out[off_lines..off_spans].copy_from_slice(pod::slice_as_bytes(&self.lines));
        // Names sit behind the texts on the wire, so their refs move up by the text length.
        let mut at = off_spans;
        for span in &self.spans {
            let mut s = *span;
            if s.flags & SPAN_VOICE != 0 {
                s.voice.off += tl;
            }
            at = put(out, at, &s);
        }
        out[off_rubies..off_classes].copy_from_slice(pod::slice_as_bytes(&self.rubies));
        at = off_classes;
        for class in &self.classes {
            at = put(out, at, &StrRef { off: class.off + tl, len: class.len });
        }
        at = off_styles;
        for style in &self.styles {
            let mut s = *style;
            if s.has_family() {
                s.family.off += tl;
            }
            if s.has_language() {
                s.language.off += tl;
            }
            at = put(out, at, &s);
        }
        out[off_strings..off_strings + self.text.len()].copy_from_slice(&self.text);
        out[off_strings + self.text.len()..total].copy_from_slice(self.names.as_bytes());
        Ok(total)
    }

    /// The payload on the heap, tests and tools.
    pub fn to_vec(&mut self) -> Vec<u8> {
        let mut v = vec![0u8; self.wire_len()];
        // `wire_len` sized it, the one refusal left is a count past u32.
        let n = self.finish(&mut v).unwrap_or(0);
        v.truncate(n);
        v
    }

    /// Copy a validated cue in, so a payload can be re-laid out. Absent fields come out zeroed.
    pub fn copy_from(&mut self, ir: &CueIr<'_>) {
        self.reset();
        self.layout = ir.layout().canonical();
        self.base = ir.base_style().canonical();
        if let Some(f) = ir.style_family(ir.base_style()) {
            self.base.family = self.intern(f);
        }
        if let Some(l) = ir.style_language(ir.base_style()) {
            self.base.language = self.intern(l);
        }
        for line in ir.lines() {
            for span in ir.line_spans(line) {
                let mut style = ir.span_style(span).canonical();
                if let Some(f) = ir.style_family(&style) {
                    style.family = self.intern(f);
                }
                if let Some(l) = ir.style_language(&style) {
                    style.language = self.intern(l);
                }
                for class in ir.span_classes(span) {
                    let r = self.intern(class);
                    self.push_class(r);
                }
                let voice = ir.span_voice(span).map(|v| self.intern(v));
                // Verbatim, a copy never merges what the source kept apart.
                let off = self.text.len() as u32;
                self.text.extend_from_slice(ir.span_text(span).as_bytes());
                let class_first = self.classes.len() as u32;
                self.classes.extend_from_slice(&self.class_stack);
                let reveal_ns = span.reveal_ns();
                let flags = voice.map_or(0, |_| SPAN_VOICE) | reveal_ns.map_or(0, |_| SPAN_REVEAL);
                let sid = self.style_id(&style);
                self.spans.push(SpanRec {
                    reveal_ns: reveal_ns.unwrap_or(0),
                    text: StrRef { off, len: span.text.len },
                    voice: voice.unwrap_or(StrRef::EMPTY),
                    class_first,
                    class_count: self.class_stack.len() as u32,
                    style: sid,
                    ruby: NONE,
                    flags,
                    pad: 0,
                });
                if let Some(r) = ir.span_ruby(span) {
                    // Kept even when empty, a payload from outside may carry one.
                    self.rt_text.extend_from_slice(ir.ruby_text(r).as_bytes());
                    self.attach_ruby_always(r.position());
                }
                self.truncate_classes(0);
            }
            self.end_line();
        }
    }

    /// Plain UTF-8 text (no markup) as an unstyled cue.
    pub fn plain_text(&mut self, text: &str) {
        self.reset();
        for l in text.split('\n') {
            let l = l.strip_suffix('\r').unwrap_or(l);
            self.push_text(l);
            self.flush(&StyleRec::default(), None, None);
            self.end_line();
        }
    }

    /// The pango-markup subset the format parsers emit, with an optional WebVTT [`Stylesheet`]
    /// and the cue identifier its `::cue(#id)` selectors match.
    pub fn pango_markup(&mut self, markup: &str, sheet: Option<&Stylesheet>, cue_id: Option<&str>) {
        self.reset();
        markup::parse(self, markup, sheet, cue_id, false);
    }

    /// Raw SubRip cue text ([`Cue::raw_text`], before the C's lossy markup transform).
    pub fn srt_text(&mut self, text: &str) {
        self.reset();
        markup::parse(self, text, None, None, true);
    }

    /// One [`Cue`] in the flavour `output` names, its WebVTT [`CueSettings`] folded into the
    /// layout.
    pub fn cue(&mut self, cue: &Cue, output: OutputFormat, sheet: Option<&Stylesheet>) {
        let text = cue.text.trim_end_matches(['\n', '\r']);
        match (cue.raw_text.as_deref(), output) {
            // The source text keeps styling the pango transform lost (SubRip's <font>).
            (Some(raw), _) => self.srt_text(raw.trim_end_matches(['\n', '\r'])),
            (None, OutputFormat::Utf8) => self.plain_text(text),
            (None, OutputFormat::PangoMarkup) => self.pango_markup(text, sheet, cue.id.as_deref()),
        }
        apply_settings(&mut self.layout, &cue.settings);
    }
}

fn same_bytes<T: Pod>(a: &T, b: &T) -> bool {
    pod::as_bytes(a) == pod::as_bytes(b)
}

fn put<T: Pod>(out: &mut [u8], at: usize, rec: &T) -> usize {
    let bytes = pod::as_bytes(rec);
    out[at..at + bytes.len()].copy_from_slice(bytes);
    at + bytes.len()
}

fn apply_settings(layout: &mut LayoutRec, s: &CueSettings) {
    if let Some(v) = s.line_position {
        layout.set_line(Some(LinePosition::Percent(v as f32)));
    } else if let Some(n) = s.line_number {
        layout.set_line(Some(LinePosition::Line(n)));
    }
    if let Some(a) = s.line_align.as_deref() {
        layout.set_line_align(match a {
            "start" => Some(LineAlign::Start),
            "center" => Some(LineAlign::Center),
            "end" => Some(LineAlign::End),
            _ => layout.line_align(),
        });
    }
    if let Some(v) = s.text_position {
        layout.set_position(Some(v as f32));
    }
    if let Some(a) = s.position_align.as_deref() {
        layout.set_position_align(match a {
            "line-left" => Some(PositionAlign::LineLeft),
            "center" => Some(PositionAlign::Center),
            "line-right" => Some(PositionAlign::LineRight),
            "auto" => Some(PositionAlign::Auto),
            _ => layout.position_align(),
        });
    }
    if let Some(v) = s.text_size {
        layout.set_size(Some(v as f32));
    }
    if let Some(v) = s.vertical.as_deref() {
        layout.set_writing_mode(match v {
            "vertical" | "rl" => WritingMode::VerticalRl,
            "vertical-lr" | "lr" => WritingMode::VerticalLr,
            _ => WritingMode::HorizontalTb,
        });
    }
    if let Some(a) = s.alignment.as_deref() {
        layout.set_align(match a {
            "start" => Some(TextAlign::Start),
            "middle" | "center" => Some(TextAlign::Center),
            "end" => Some(TextAlign::End),
            "left" => Some(TextAlign::Left),
            "right" => Some(TextAlign::Right),
            _ => layout.align(),
        });
    }
}

// ---------------------------------------------------------------------------------------------
// View.

/// A validated, borrowed `subtitle/cue-ir` payload. [`CueIr::parse`] checks the header, every
/// section bound and alignment, every index, string range and enum byte, and the string region's
/// UTF-8 once, so the accessors index typed slices and cannot fail.
#[derive(Clone, Copy)]
pub struct CueIr<'a> {
    h: &'a CueHeader,
    lines: &'a [LineRec],
    spans: &'a [SpanRec],
    rubies: &'a [RubyRec],
    classes: &'a [StrRef],
    styles: &'a [StyleRec],
    strings: &'a str,
}

impl<'a> CueIr<'a> {
    pub fn parse(p: &'a [u8]) -> Result<CueIr<'a>, WireError> {
        if p.len() < HEADER_LEN {
            return Err(WireError::Short { have: p.len() });
        }
        if !p.as_ptr().addr().is_multiple_of(ALIGN) {
            return Err(WireError::Misaligned);
        }
        if p[..4] != MAGIC {
            return Err(WireError::Magic);
        }
        let h: &CueHeader = pod::cast_ref(p).ok_or(WireError::Misaligned)?;
        if h.version != VERSION {
            return Err(WireError::Version(h.version));
        }
        if h.header_len as usize != HEADER_LEN {
            return Err(WireError::Section("header"));
        }
        if h.total_len as usize != p.len() {
            return Err(WireError::Length { stated: h.total_len, have: p.len() });
        }
        fn table<'b, R: Pod>(
            p: &'b [u8],
            off: u32,
            n: u32,
            what: &'static str,
        ) -> Result<&'b [R], WireError> {
            let size = std::mem::size_of::<R>();
            let len = (n as usize).checked_mul(size).ok_or(WireError::Section(what))?;
            let start = off as usize;
            let end = start.checked_add(len).ok_or(WireError::Section(what))?;
            if start < HEADER_LEN || end > p.len() || !start.is_multiple_of(ALIGN) {
                return Err(WireError::Section(what));
            }
            pod::cast_slice(&p[start..end]).ok_or(WireError::Section(what))
        }
        let lines = table::<LineRec>(p, h.off_lines, h.n_lines, "lines")?;
        let spans = table::<SpanRec>(p, h.off_spans, h.n_spans, "spans")?;
        let rubies = table::<RubyRec>(p, h.off_rubies, h.n_rubies, "rubies")?;
        let classes = table::<StrRef>(p, h.off_classes, h.n_classes, "classes")?;
        let styles = table::<StyleRec>(p, h.off_styles, h.n_styles, "styles")?;
        let strings = table::<u8>(p, h.off_strings, h.len_strings, "strings")?;
        let strings = std::str::from_utf8(strings).map_err(|_| WireError::Strings)?;
        if styles.is_empty() || h.base_style as usize >= styles.len() {
            return Err(WireError::Index("base style"));
        }
        let sref = |r: StrRef, what: &'static str| -> Result<(), WireError> {
            let end = r.end().ok_or(WireError::StringRef(what))?;
            if end > h.len_strings
                || !strings.is_char_boundary(r.off as usize)
                || !strings.is_char_boundary(end as usize)
            {
                return Err(WireError::StringRef(what));
            }
            Ok(())
        };
        h.layout.validate()?;
        for l in lines {
            let end = l.first_span.checked_add(l.span_count).ok_or(WireError::Index("line"))?;
            if end as usize > spans.len() {
                return Err(WireError::Index("line"));
            }
        }
        for s in spans {
            sref(s.text, "span text")?;
            if s.flags & !SPAN_ALL != 0 {
                return Err(WireError::Enum("span flags"));
            }
            if s.flags & SPAN_VOICE != 0 {
                sref(s.voice, "voice")?;
            }
            let end =
                s.class_first.checked_add(s.class_count).ok_or(WireError::Index("classes"))?;
            if end as usize > classes.len() {
                return Err(WireError::Index("classes"));
            }
            if s.style as usize >= styles.len() {
                return Err(WireError::Index("style"));
            }
            if s.ruby != NONE && s.ruby as usize >= rubies.len() {
                return Err(WireError::Index("ruby"));
            }
        }
        for r in rubies {
            sref(r.text, "ruby text")?;
            if r.position > 1 {
                return Err(WireError::Enum("ruby position"));
            }
        }
        for c in classes {
            sref(*c, "class")?;
        }
        for s in styles {
            s.validate()?;
            if s.has_family() {
                sref(s.family, "font family")?;
            }
            if s.has_language() {
                sref(s.language, "language")?;
            }
        }
        Ok(CueIr { h, lines, spans, rubies, classes, styles, strings })
    }

    pub fn header(&self) -> &'a CueHeader {
        self.h
    }
    pub fn layout(&self) -> &'a LayoutRec {
        &self.h.layout
    }
    pub fn base_style(&self) -> &'a StyleRec {
        let styles = self.styles;
        &styles[self.h.base_style as usize]
    }
    pub fn lines(&self) -> &'a [LineRec] {
        self.lines
    }
    pub fn spans(&self) -> &'a [SpanRec] {
        self.spans
    }
    pub fn rubies(&self) -> &'a [RubyRec] {
        self.rubies
    }
    pub fn styles(&self) -> &'a [StyleRec] {
        self.styles
    }
    pub fn strings(&self) -> &'a str {
        self.strings
    }
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
    pub fn span_count(&self) -> usize {
        self.spans.len()
    }
    pub fn style_count(&self) -> usize {
        self.styles.len()
    }
    /// An upper bound on the plain text of every line joined by newlines.
    pub fn text_len(&self) -> usize {
        self.strings.len() + self.lines.len()
    }

    /// The string behind a validated ref, `""` for one from another payload.
    pub fn str(&self, r: StrRef) -> &'a str {
        let end = (r.off as usize).saturating_add(r.len as usize);
        self.strings.get(r.off as usize..end).unwrap_or("")
    }
    pub fn line_spans(&self, l: &LineRec) -> &'a [SpanRec] {
        let spans = self.spans;
        &spans[l.first_span as usize..][..l.span_count as usize]
    }
    pub fn span_text(&self, s: &SpanRec) -> &'a str {
        self.str(s.text)
    }
    pub fn span_voice(&self, s: &SpanRec) -> Option<&'a str> {
        s.has_voice().then(|| self.str(s.voice))
    }
    pub fn span_classes(&self, s: &SpanRec) -> impl Iterator<Item = &'a str> + 'a {
        let (strings, classes) = (self.strings, self.classes);
        classes[s.class_first as usize..][..s.class_count as usize].iter().map(move |c| {
            let end = (c.off as usize).saturating_add(c.len as usize);
            strings.get(c.off as usize..end).unwrap_or("")
        })
    }
    pub fn span_class_count(&self, s: &SpanRec) -> usize {
        s.class_count as usize
    }
    pub fn span_ruby(&self, s: &SpanRec) -> Option<&'a RubyRec> {
        let rubies = self.rubies;
        s.has_ruby().then(|| &rubies[s.ruby as usize])
    }
    pub fn span_style(&self, s: &SpanRec) -> &'a StyleRec {
        let styles = self.styles;
        &styles[s.style as usize]
    }
    pub fn ruby_text(&self, r: &RubyRec) -> &'a str {
        self.str(r.text)
    }
    pub fn style_family(&self, s: &StyleRec) -> Option<&'a str> {
        s.has_family().then(|| self.str(s.family))
    }
    pub fn style_language(&self, s: &StyleRec) -> Option<&'a str> {
        s.has_language().then(|| self.str(s.language))
    }

    /// The plain text of line `i`, span by span, what a renderer without styling draws. Empty
    /// past the last line.
    pub fn line_text(&self, i: usize) -> impl Iterator<Item = &'a str> + 'a {
        let v = *self;
        let spans = self.lines.get(i).map_or(&[][..], |l| self.line_spans(l));
        spans.iter().map(move |s| v.span_text(s))
    }

    /// Whether two payloads read the same, record for record, however they were laid out.
    /// Allocates nothing.
    pub fn same_as(&self, other: &CueIr<'_>) -> bool {
        // String refs differ between layouts, the strings are compared below.
        let refless = |s: &StyleRec| {
            let mut c = s.canonical();
            (c.family, c.language) = (StrRef::EMPTY, StrRef::EMPTY);
            c
        };
        let style_eq = |a: &StyleRec, b: &StyleRec| {
            refless(a) == refless(b)
                && self.style_family(a) == other.style_family(b)
                && self.style_language(a) == other.style_language(b)
        };
        if self.layout().canonical() != other.layout().canonical()
            || !style_eq(self.base_style(), other.base_style())
            || self.lines.len() != other.lines.len()
        {
            return false;
        }
        for (la, lb) in self.lines.iter().zip(other.lines) {
            let (sa, sb) = (self.line_spans(la), other.line_spans(lb));
            if sa.len() != sb.len() {
                return false;
            }
            for (a, b) in sa.iter().zip(sb) {
                let ruby_eq = match (self.span_ruby(a), other.span_ruby(b)) {
                    (None, None) => true,
                    (Some(ra), Some(rb)) => {
                        ra.position() == rb.position() && self.ruby_text(ra) == other.ruby_text(rb)
                    }
                    _ => false,
                };
                if self.span_text(a) != other.span_text(b)
                    || self.span_voice(a) != other.span_voice(b)
                    || a.reveal_ns() != b.reveal_ns()
                    || !self.span_classes(a).eq(other.span_classes(b))
                    || !ruby_eq
                    || !style_eq(self.span_style(a), other.span_style(b))
                {
                    return false;
                }
            }
        }
        true
    }

    /// Everything a renderer reads, one line per record, for comparing two layouts of one cue.
    /// Tests and tools.
    #[allow(clippy::disallowed_methods)]
    pub fn dump(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let style = |out: &mut String, s: &StyleRec| {
            let _ = write!(
                out,
                "{:?} family={:?} lang={:?}",
                s.canonical(),
                self.style_family(s),
                self.style_language(s)
            );
        };
        let _ = writeln!(out, "layout {:?}", self.layout().canonical());
        out.push_str("base ");
        style(&mut out, self.base_style());
        out.push('\n');
        for line in self.lines() {
            out.push_str("line\n");
            for span in self.line_spans(line) {
                let _ = write!(
                    out,
                    "  span {:?} voice={:?} reveal={:?} classes=[",
                    self.span_text(span),
                    self.span_voice(span),
                    span.reveal_ns()
                );
                for c in self.span_classes(span) {
                    let _ = write!(out, "{c:?},");
                }
                out.push_str("] ruby=");
                match self.span_ruby(span) {
                    Some(r) => {
                        let _ = write!(out, "{:?}/{:?}", self.ruby_text(r), r.position());
                    }
                    None => out.push_str("none"),
                }
                out.push_str(" style=");
                style(&mut out, self.span_style(span));
                out.push('\n');
            }
        }
        out
    }

    /// Span texts concatenated, lines joined with `\n`, ruby excluded. Tests and tools.
    #[allow(clippy::disallowed_methods)]
    pub fn plain_text(&self) -> String {
        let mut out = String::with_capacity(self.text_len());
        for i in 0..self.lines.len() {
            if i != 0 {
                out.push('\n');
            }
            out.extend(self.line_text(i));
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Markup.

mod markup {
    use super::*;
    use crate::vttcss::Node;

    struct Parser<'a> {
        b: &'a mut CueBuilder,
        input: &'a str,
        sheet: Option<&'a Stylesheet>,
        ctx: MarkupCtx,
        /// `{\anN}`, the first one wins like the SSA renderers.
        an_set: bool,
        reveal_ns: Option<u64>,
        raw: bool,
    }

    /// Pango markup, or with `raw` SubRip text where only letter-initiated `<...>` are tags,
    /// like the C's escape/unescape pipeline.
    pub(super) fn parse(
        b: &mut CueBuilder,
        input: &str,
        sheet: Option<&Stylesheet>,
        cue_id: Option<&str>,
        raw: bool,
    ) {
        let sheet = sheet.filter(|s| !s.is_empty());
        let mut root_ruby = RubyPosition::default();
        if let Some(sheet) = sheet {
            let root = Node { id: cue_id, ..Node::default() };
            let mut base = StyleRec::default();
            sheet.apply(&root, &mut b.names, &mut base, &mut root_ruby);
            b.base = base;
        }
        let mut p = Parser {
            b,
            input,
            sheet,
            ctx: MarkupCtx { ruby_position: root_ruby, ..MarkupCtx::default() },
            an_set: false,
            reveal_ns: None,
            raw,
        };
        p.run();
        p.finish();
    }

    /// Whitelisted names may follow spaces (the C's unescape skips them), others need a letter
    /// after the optional `/`.
    fn is_srt_tag_start(rest: &str) -> bool {
        let body = rest.strip_prefix('/').unwrap_or(rest);
        if body.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return true;
        }
        let name = tag_name(body.trim_start_matches([' ', '\t']));
        ["i", "b", "u", "s", "font"].iter().any(|t| name.eq_ignore_ascii_case(t))
    }

    /// An escaped WebVTT inline timestamp `&lt;...&gt;`. The C's unhandled-tag removal spares
    /// these.
    fn parse_timestamp_ref(input: &str) -> Option<(u64, usize)> {
        let rest = input.strip_prefix("&lt;")?;
        // Timestamps are short, so only a window is searched, a distant "&gt;" is ordinary text
        // and a cue of many `&lt;` costs its length.
        let window = &rest.as_bytes()[..rest.len().min(24 + 4)];
        let end = window.windows(4).position(|w| w == b"&gt;")?;
        let ns = parse_vtt_timestamp(&rest[..end])?;
        Some((ns, 4 + end + 4))
    }

    /// `[HH:]MM:SS.mmm`, lenient. The fraction is required, `<12:30>` is far more likely prose.
    fn parse_vtt_timestamp(s: &str) -> Option<u64> {
        fn dec(s: &str) -> Option<u64> {
            (!s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse().unwrap_or(0))
        }
        let (h, rest) = match s.matches(':').count() {
            2 => {
                let (h, rest) = s.split_once(':')?;
                (dec(h)?, rest)
            }
            1 => (0, s),
            _ => return None,
        };
        let (m, sec_frac) = rest.split_once(':')?;
        let m = dec(m)?;
        let (sec, frac) = sec_frac.split_once(['.', ','])?;
        let sec = dec(sec)?;
        if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut frac_ns = 0u64;
        for i in 0..9 {
            frac_ns = frac_ns * 10 + frac.as_bytes().get(i).map_or(0, |b| (b - b'0') as u64);
        }
        // An absurd timestamp is prose. Saturating would yield u64::MAX, GST_CLOCK_TIME_NONE.
        let ns = h
            .checked_mul(3600)?
            .checked_add(m.checked_mul(60)?)?
            .checked_add(sec)?
            .checked_mul(1_000_000_000)?
            .checked_add(frac_ns)?;
        (ns < u64::MAX).then_some(ns)
    }

    fn decode_entity(input: &str) -> (Option<char>, usize) {
        // Entity names are short, so only a window is searched for the ';'.
        let window = &input.as_bytes()[1..input.len().min(1 + 13)];
        let semi = match window.iter().position(|&b| b == b';') {
            Some(rel) if rel <= 12 => 1 + rel,
            _ => return (None, 1),
        };
        let name = &input[1..semi];
        let ch = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = name.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match ch {
            Some(c) => (Some(c), semi + 1),
            None => (None, 1),
        }
    }

    impl<'a> Parser<'a> {
        fn run(&mut self) {
            let input = self.input;
            let bytes = input.as_bytes();
            let raw = self.raw;
            let mut pos = 0;
            while pos < input.len() {
                match bytes[pos] {
                    b'<' if raw && !is_srt_tag_start(&input[pos + 1..]) => {
                        // Not a tag by the C's reckoning (`<3`, `< x>`), one literal '<'.
                        self.push_char('<');
                        pos += 1;
                    }
                    b'<' => match input[pos + 1..].find('>') {
                        Some(rel) => {
                            self.handle_tag(pos + 1, pos + 1 + rel);
                            pos += rel + 2;
                        }
                        None => {
                            // No closing '>', the rest is text, like the C's GMarkup recovery.
                            self.text(&input[pos..]);
                            break;
                        }
                    },
                    b'&' if !raw => {
                        // A WebVTT inline timestamp (kept escaped by the C) sets the reveal time.
                        if let Some((ns, used)) = parse_timestamp_ref(&input[pos..]) {
                            self.timestamp(ns);
                            pos += used;
                        } else {
                            let (ch, used) = decode_entity(&input[pos..]);
                            self.push_char(ch.unwrap_or('&'));
                            pos += used;
                        }
                    }
                    b'{' if raw => {
                        if bytes.get(pos + 1) != Some(&b'\\') {
                            // This arm must consume a plain brace, the text arm stops at '{'.
                            self.push_char('{');
                            pos += 1;
                        } else {
                            match input[pos + 1..].find('}') {
                                Some(rel) => {
                                    self.ssa_block(&input[pos + 1..pos + 1 + rel]);
                                    pos += rel + 2;
                                }
                                None => {
                                    // Unclosed block stays literal, like ssaparse's unmatched '{'.
                                    self.text(&input[pos..]);
                                    break;
                                }
                            }
                        }
                    }
                    b'\n' => {
                        self.end_line();
                        pos += 1;
                    }
                    _ => {
                        let stops: &[char] =
                            if raw { &['<', '{', '\n'] } else { &['<', '&', '\n'] };
                        let end =
                            input[pos..].find(stops).map(|rel| pos + rel).unwrap_or(input.len());
                        self.text(&input[pos..end]);
                        pos = end;
                    }
                }
            }
        }

        fn text(&mut self, s: &str) {
            if self.ctx.in_rt {
                self.b.push_rt_text(s);
            } else {
                self.b.push_text(s);
            }
        }

        fn push_char(&mut self, c: char) {
            let mut buf = [0u8; 4];
            self.text(c.encode_utf8(&mut buf));
        }

        fn flush(&mut self) {
            let voice = self.ctx.has_voice.then_some(self.ctx.voice);
            self.b.flush(&self.ctx.style, voice, self.reveal_ns);
        }

        fn timestamp(&mut self, ns: u64) {
            self.flush();
            self.reveal_ns = Some(ns);
        }

        fn end_line(&mut self) {
            self.flush();
            self.b.end_line();
        }

        fn finish(&mut self) {
            while let Some(frame) = self.b.frames.pop() {
                self.close_frame(frame);
            }
            self.flush();
            self.b.end_last_line();
        }

        /// A `{\...}` block in raw SRT. `\anN`/`\aN` set the anchor (first wins), the rest is
        /// stripped.
        fn ssa_block(&mut self, block: &str) {
            for seg in block.split('\\').skip(1) {
                let seg = seg.trim();
                let numpad = if let Some(n) = seg.strip_prefix("an") {
                    n.parse::<i32>().ok()
                } else if let Some(n) = seg.strip_prefix('a') {
                    n.parse::<i32>().ok().and_then(crate::ssastyle::legacy_to_numpad)
                } else {
                    None
                };
                if !self.an_set
                    && let Some((anchor, align)) = numpad.and_then(crate::ssastyle::numpad_anchor)
                {
                    self.b.layout.set_anchor(Some(anchor));
                    self.b.layout.set_align(Some(align));
                    self.an_set = true;
                }
            }
        }

        fn handle_tag(&mut self, start: usize, end: usize) {
            let tag = self.input[start..end].trim();
            if let Some(rest) = tag.strip_prefix('/') {
                let name = tag_name(rest.trim_start());
                self.close_tag(name);
            } else {
                let at = start + (tag.as_ptr() as usize - self.input[start..].as_ptr() as usize);
                self.open_tag(at, tag);
            }
        }

        fn open_tag(&mut self, at: usize, tag: &'a str) {
            let name = tag_name(tag);
            // Past the depth cap a tag is dropped, so a closer scans a bounded stack and a
            // span copies a bounded class stack.
            if name.is_empty() || self.b.frames.len() >= MAX_MARKUP_DEPTH {
                return;
            }
            let rest = &tag[name.len()..];
            self.flush();
            let saved = self.ctx;
            let depth = self.b.class_depth();

            let vtt_node = ["i", "b", "u", "c", "v", "ruby", "rt", "lang"]
                .iter()
                .any(|t| name.eq_ignore_ascii_case(t));
            let (own_classes, annotation) =
                if vtt_node { split_tag_rest(rest) } else { ("", rest.trim()) };

            let lower = |t: &str| name.eq_ignore_ascii_case(t);
            if lower("i") {
                self.ctx.style.set_font_style(Some(FontStyle::Italic));
            } else if lower("b") {
                self.ctx.style.set_font_weight(Some(700));
            } else if lower("u") {
                self.ctx.style.set_underline(Some(true));
            } else if lower("s") {
                self.ctx.style.set_strikethrough(Some(true));
            } else if lower("big") {
                self.ctx.style.set_font_size(Some(size_step(1)));
            } else if lower("small") {
                self.ctx.style.set_font_size(Some(size_step(-1)));
            } else if lower("tt") {
                let f = self.b.intern("monospace");
                self.ctx.style.set_family(Some(f));
            } else if lower("sub") {
                self.ctx.style.set_baseline_shift(Some(-3.0));
            } else if lower("sup") {
                self.ctx.style.set_baseline_shift(Some(3.0));
            } else if lower("span") {
                self.apply_span_attrs(rest);
            } else if lower("font") {
                self.apply_font_attrs(rest);
            } else if lower("v") {
                if !annotation.is_empty() {
                    self.ctx.voice = self.b.intern(annotation);
                    self.ctx.has_voice = true;
                    self.ctx.voice_in = range_of(self.input, annotation);
                }
            } else if lower("lang") {
                if !annotation.is_empty() {
                    let l = self.b.intern(annotation);
                    self.ctx.style.set_language(Some(l));
                    self.ctx.lang_in = range_of(self.input, annotation);
                }
            } else if lower("ruby") {
                self.ctx.in_ruby = true;
            } else if lower("rt") && self.ctx.in_ruby {
                self.ctx.in_rt = true;
            }

            // Standard color classes style any tag, like browsers' `::cue(.white)` UA rules.
            for class in own_classes.split('.').filter(|c| !c.is_empty()) {
                if let Some(bg) = class.strip_prefix("bg_") {
                    if let Some(c) = vtt_class_color(bg) {
                        self.ctx.style.set_background(Some(c));
                    }
                } else if let Some(c) = vtt_class_color(class) {
                    self.ctx.style.set_foreground(Some(c));
                }
                let r = self.b.intern(class);
                self.b.push_class(r);
            }

            if vtt_node && let Some(sheet) = self.sheet {
                let slice = |(off, len): (u32, u32)| {
                    (len > 0).then(|| &self.input[off as usize..(off + len) as usize])
                };
                // A `<v>` node matches on its voice, the annotation or an inherited one.
                let voice = if lower("v") { slice(self.ctx.voice_in) } else { None };
                let lang = slice(self.ctx.lang_in);
                let node =
                    Node { element: Some(name), classes: own_classes, voice, lang, id: None };
                sheet.apply(
                    &node,
                    &mut self.b.names,
                    &mut self.ctx.style,
                    &mut self.ctx.ruby_position,
                );
            }

            // The context carries the class depth its close returns to.
            self.ctx.classes = depth;
            self.b.frames.push(Frame { name: (at as u32, name.len() as u32), ctx: saved });
        }

        fn frame_name(&self, f: &Frame) -> &'a str {
            &self.input[f.name.0 as usize..(f.name.0 + f.name.1) as usize]
        }

        fn close_tag(&mut self, name: &str) {
            // Close up to the innermost open tag of this name, a stray closer does nothing.
            let Some(at) =
                self.b.frames.iter().rposition(|f| self.frame_name(f).eq_ignore_ascii_case(name))
            else {
                return;
            };
            self.flush();
            while self.b.frames.len() > at {
                if let Some(frame) = self.b.frames.pop() {
                    self.close_frame(frame);
                }
            }
        }

        fn close_frame(&mut self, frame: Frame) {
            self.flush();
            if self.frame_name(&frame).eq_ignore_ascii_case("rt") && self.ctx.in_rt {
                self.b.attach_ruby(self.ctx.ruby_position);
            }
            self.b.truncate_classes(self.ctx.classes);
            self.ctx = frame.ctx;
        }

        fn apply_span_attrs(&mut self, attrs: &'a str) {
            for (key, value) in AttrIter::new(attrs) {
                let style = &mut self.ctx.style;
                let key_is = |k: &str| key.eq_ignore_ascii_case(k);
                let val_is = |v: &str| value.eq_ignore_ascii_case(v);
                if key_is("style") {
                    style.set_font_style(if val_is("italic") {
                        Some(FontStyle::Italic)
                    } else if val_is("oblique") {
                        Some(FontStyle::Oblique)
                    } else if val_is("normal") {
                        Some(FontStyle::Normal)
                    } else {
                        style.font_style()
                    });
                } else if key_is("weight") {
                    let named = [
                        ("thin", 100),
                        ("ultralight", 200),
                        ("light", 300),
                        ("semilight", 350),
                        ("book", 380),
                        ("normal", 400),
                        ("medium", 500),
                        ("semibold", 600),
                        ("bold", 700),
                        ("ultrabold", 800),
                        ("heavy", 900),
                        ("ultraheavy", 1000),
                    ];
                    let w = named.iter().find(|(n, _)| val_is(n)).map(|(_, w)| *w);
                    style.set_font_weight(
                        w.or_else(|| value.parse::<u16>().ok()).or(style.font_weight()),
                    );
                } else if key_is("size") {
                    if let Some(size) = parse_size(value) {
                        style.set_font_size(Some(size));
                    }
                } else if key_is("foreground") || key_is("fgcolor") || key_is("color") {
                    if let Some(c) = Color::parse(value) {
                        style.set_foreground(Some(c));
                    }
                } else if key_is("background") || key_is("bgcolor") {
                    if let Some(c) = Color::parse(value) {
                        style.set_background(Some(c));
                    }
                } else if key_is("font_family") || key_is("face") {
                    let f = self.b.intern(value);
                    self.ctx.style.set_family(Some(f));
                } else if key_is("font") || key_is("font_desc") {
                    let (family, size) = parse_font_desc(value);
                    if let Some(f) = family {
                        let f = self.b.intern(f);
                        self.ctx.style.set_family(Some(f));
                    }
                    if let Some(s) = size {
                        self.ctx.style.set_font_size(Some(FontSize::Points(s)));
                    }
                } else if key_is("underline") {
                    style.set_underline(if val_is("none") {
                        Some(false)
                    } else if ["single", "double", "low", "error"].iter().any(|v| val_is(v)) {
                        Some(true)
                    } else {
                        style.underline()
                    });
                } else if key_is("strikethrough") {
                    style.set_strikethrough(if val_is("true") {
                        Some(true)
                    } else if val_is("false") {
                        Some(false)
                    } else {
                        style.strikethrough()
                    });
                } else if key_is("rise") {
                    // 1024ths of a point, like pango.
                    if let Ok(v) = value.parse::<f32>() {
                        style.set_baseline_shift(Some(v / 1024.0));
                    }
                } else if key_is("letter_spacing") {
                    if let Ok(v) = value.parse::<f32>() {
                        style.set_letter_spacing(Some(v / 1024.0));
                    }
                } else if key_is("lang") {
                    let l = self.b.intern(value);
                    self.ctx.style.set_language(Some(l));
                    self.ctx.lang_in = range_of(self.input, value);
                }
            }
        }

        /// `<font color face size>` from SubRip files, which the C deletes.
        fn apply_font_attrs(&mut self, attrs: &'a str) {
            for (key, value) in AttrIter::new(attrs) {
                if key.eq_ignore_ascii_case("color") {
                    if let Some(c) = Color::parse(value) {
                        self.ctx.style.set_foreground(Some(c));
                    }
                } else if key.eq_ignore_ascii_case("face") {
                    let f = self.b.intern(value);
                    self.ctx.style.set_family(Some(f));
                } else if key.eq_ignore_ascii_case("size")
                    && let Some(s) = html_font_size(value)
                {
                    self.ctx.style.set_font_size(Some(s));
                }
            }
        }
    }

    /// `s`, a slice of `input`, as a byte range of it.
    fn range_of(input: &str, s: &str) -> (u32, u32) {
        let off = s.as_ptr() as usize - input.as_ptr() as usize;
        (off as u32, s.len() as u32)
    }

    /// Legacy HTML `<font size>`, 1..7 or +n/-n relative to 3. Saturating, the value is unbounded.
    fn html_font_size(value: &str) -> Option<FontSize> {
        let v = value.trim();
        let n = v.parse::<i64>().ok()?;
        let abs = if v.starts_with(['+', '-']) { 3i64.saturating_add(n) } else { n };
        Some(size_step(abs.clamp(1, 7) as i32 - 3))
    }

    /// The `.a.b` class list (with its dots) and the annotation after it.
    fn split_tag_rest(rest: &str) -> (&str, &str) {
        let rest = rest.trim();
        if !rest.starts_with('.') {
            return ("", rest);
        }
        match rest.find([' ', '\t']) {
            Some(at) => (&rest[..at], rest[at + 1..].trim()),
            None => (rest, ""),
        }
    }

    /// The alphanumeric prefix, compared case-insensitively by the callers.
    fn tag_name(tag: &str) -> &str {
        let end = tag.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(tag.len());
        &tag[..end]
    }

    /// The WebVTT default color classes, as browsers style `::cue(.<name>)`.
    fn vtt_class_color(name: &str) -> Option<Color> {
        Some(match name {
            "white" => Color::rgb(0xff, 0xff, 0xff),
            "lime" => Color::rgb(0x00, 0xff, 0x00),
            "cyan" => Color::rgb(0x00, 0xff, 0xff),
            "red" => Color::rgb(0xff, 0x00, 0x00),
            "yellow" => Color::rgb(0xff, 0xff, 0x00),
            "magenta" => Color::rgb(0xff, 0x00, 0xff),
            "blue" => Color::rgb(0x00, 0x00, 0xff),
            "black" => Color::rgb(0x00, 0x00, 0x00),
            _ => return None,
        })
    }

    fn parse_size(value: &str) -> Option<FontSize> {
        let v = value.trim();
        let is = |k: &str| v.eq_ignore_ascii_case(k);
        let scale = size_step;
        Some(if is("xx-small") {
            scale(-3)
        } else if is("x-small") {
            scale(-2)
        } else if is("small") || is("smaller") {
            scale(-1)
        } else if is("medium") {
            scale(0)
        } else if is("large") || is("larger") {
            scale(1)
        } else if is("x-large") {
            scale(2)
        } else if is("xx-large") {
            scale(3)
        } else if let Some(pt) = v.strip_suffix("pt").or_else(|| v.strip_suffix("PT")) {
            FontSize::Points(pt.trim().parse::<f32>().ok()?)
        } else {
            FontSize::Points(v.parse::<f32>().ok()? / 1024.0)
        })
    }

    fn parse_font_desc(value: &str) -> (Option<&str>, Option<f32>) {
        let value = value.trim();
        if value.is_empty() {
            return (None, None);
        }
        let (family, size) = match value.rsplit_once(char::is_whitespace) {
            Some((head, tail)) => match tail.parse::<f32>() {
                Ok(s) => (head.trim(), Some(s)),
                Err(_) => (value, None),
            },
            None => match value.parse::<f32>() {
                Ok(s) => ("", Some(s)),
                Err(_) => (value, None),
            },
        };
        ((!family.is_empty()).then_some(family), size)
    }

    struct AttrIter<'a> {
        rest: &'a str,
    }

    impl<'a> AttrIter<'a> {
        fn new(rest: &'a str) -> Self {
            AttrIter { rest }
        }
    }

    impl<'a> Iterator for AttrIter<'a> {
        type Item = (&'a str, &'a str);

        fn next(&mut self) -> Option<(&'a str, &'a str)> {
            loop {
                let s = self.rest.trim_start();
                if s.is_empty() {
                    self.rest = s;
                    return None;
                }
                let eq = s.find('=')?;
                let key = s[..eq].trim();
                let after = s[eq + 1..].trim_start();
                let quote = after.chars().next()?;
                if quote != '"' && quote != '\'' {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    let value = &after[..end];
                    self.rest = &after[end..];
                    if key.is_empty() || value.is_empty() {
                        continue;
                    }
                    return Some((key, value));
                }
                let close = after[1..].find(quote)?;
                let value = &after[1..1 + close];
                self.rest = &after[1 + close + 1..];
                if key.is_empty() {
                    continue;
                }
                return Some((key, value));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
pub(crate) mod testutil {
    use super::*;

    /// A cue on the heap with its view, for assertions.
    pub struct Owned(pub Vec<u8>);

    impl Owned {
        pub fn view(&self) -> CueIr<'_> {
            CueIr::parse(&self.0).expect("a builder payload validates")
        }
        pub fn plain_text(&self) -> String {
            self.view().plain_text()
        }
    }

    pub fn build(f: impl FnOnce(&mut CueBuilder)) -> Owned {
        let mut b = CueBuilder::new();
        f(&mut b);
        Owned(b.to_vec())
    }

    pub fn srt(text: &str) -> Owned {
        build(|b| b.srt_text(text))
    }

    pub fn markup(text: &str) -> Owned {
        build(|b| b.pango_markup(text, None, None))
    }

    pub fn styled(markup: &str, css: &str) -> Owned {
        let sheet = Stylesheet::parse(css);
        build(|b| b.pango_markup(markup, Some(&sheet), None))
    }

    pub fn spans_of<'a>(v: &CueIr<'a>) -> Vec<&'a SpanRec> {
        v.lines().iter().flat_map(|l| v.line_spans(l).iter()).collect()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn an_enormous_relative_font_size_does_not_overflow() {
        let top = size_step(4);
        for value in
            ["+2147483645", "+2147483646", "+2147483647", "+2147483648", "+9223372036854775807"]
        {
            let o = srt(&format!("<font size={value}>x</font>"));
            let v = o.view();
            let size = spans_of(&v).first().and_then(|s| v.span_style(s).font_size());
            assert_eq!(size, Some(top), "{value} did not saturate to the top rung");
        }

        let bottom = size_step(-2);
        for value in ["-2147483647", "-2147483648", "-9223372036854775808"] {
            let o = srt(&format!("<font size={value}>x</font>"));
            let v = o.view();
            let size = spans_of(&v).first().and_then(|s| v.span_style(s).font_size());
            assert_eq!(size, Some(bottom), "{value} did not saturate to the bottom rung");
        }

        let o = srt("<font size=enormous>x</font>");
        let v = o.view();
        let size = spans_of(&v).first().and_then(|s| v.span_style(s).font_size());
        assert_eq!(size, None, "a non-numeric size must be ignored");
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(Color::parse("#ff0000"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::parse("#f00"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::parse("#123456"), Some(Color::rgb(0x12, 0x34, 0x56)));
        assert_eq!(Color::parse("#ffffffffffff"), Some(Color::rgb(255, 255, 255)));
        assert_eq!(Color::parse("#12345"), None);
        assert_eq!(Color::parse("#nothex"), None);
    }

    #[test]
    fn named_color_table_is_sorted() {
        for pair in NAMED_COLORS.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "NAMED_COLORS out of order: {:?} before {:?}",
                pair[0].0,
                pair[1].0
            );
        }
    }

    #[test]
    fn parses_named_colors_case_insensitively() {
        assert_eq!(Color::parse("red"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::parse("Yellow"), Some(Color::rgb(255, 255, 0)));
        assert_eq!(Color::parse("LightSlateGrey"), Some(Color::rgb(119, 136, 153)));
        assert_eq!(Color::parse("notacolor"), None);
        assert_eq!(Color::parse("YELLOWGREEN"), Some(Color::rgb(0x9a, 0xcd, 0x32)));
    }

    #[test]
    fn plain_text_round_trips() {
        let o = build(|b| b.plain_text("One\nTwo"));
        let v = o.view();
        assert_eq!(v.line_count(), 2);
        assert_eq!(v.plain_text(), "One\nTwo");
        assert!(v.span_style(&v.line_spans(&v.lines()[0])[0]).is_plain());
    }

    #[test]
    fn plain_text_keeps_empty_lines() {
        let o = build(|b| b.plain_text("a\n\nb"));
        let v = o.view();
        assert_eq!(v.line_count(), 3);
        assert!(v.line_spans(&v.lines()[1]).is_empty());
        assert_eq!(v.plain_text(), "a\n\nb");
    }

    #[test]
    fn simple_tags_map_to_styles() {
        let o = markup("<i>it</i><b>bo</b><u>un</u>");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 3);
        assert_eq!(v.span_style(spans[0]).font_style(), Some(FontStyle::Italic));
        assert_eq!(v.span_style(spans[1]).font_weight(), Some(700));
        assert_eq!(v.span_style(spans[2]).underline(), Some(true));
        assert_eq!(v.plain_text(), "itboun");
    }

    #[test]
    fn nested_tags_compose() {
        let o = markup("<b><i>x</i>y</b>");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 2);
        assert_eq!(v.span_style(spans[0]).font_weight(), Some(700));
        assert_eq!(v.span_style(spans[0]).font_style(), Some(FontStyle::Italic));
        assert_eq!(v.span_style(spans[1]).font_weight(), Some(700));
        assert_eq!(v.span_style(spans[1]).font_style(), None);
    }

    #[test]
    fn unclosed_tags_close_at_the_end() {
        let o = markup("<i>Seven");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 1);
        assert_eq!(v.span_style(spans[0]).font_style(), Some(FontStyle::Italic));
    }

    #[test]
    fn stray_closers_are_ignored() {
        let o = markup("</b>text</i>");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 1);
        assert!(v.span_style(spans[0]).is_plain());
        assert_eq!(v.span_text(spans[0]), "text");
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(markup("Rock &amp; Roll &lt;5 &#x41;&#66;").plain_text(), "Rock & Roll <5 AB");
    }

    #[test]
    fn adjacent_identical_styling_merges() {
        let o = markup("a<unknowntag>b</unknowntag>c");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 1);
        assert_eq!(v.span_text(spans[0]), "abc");
        // An empty styled element between equal runs does not split them either.
        let o = markup("a<v Fred></v>b<lang en></lang>c<c.x></c>d");
        let v = o.view();
        assert_eq!(spans_of(&v).len(), 1);
        assert_eq!(v.plain_text(), "abcd");
    }

    #[test]
    fn newlines_split_lines_across_open_tags() {
        let o = markup("<i>a\nb</i>");
        let v = o.view();
        assert_eq!(v.line_count(), 2);
        for l in v.lines() {
            assert_eq!(v.span_style(&v.line_spans(l)[0]).font_style(), Some(FontStyle::Italic));
        }
        assert_eq!(v.plain_text(), "a\nb");
    }

    #[test]
    fn microdvd_span_attrs() {
        let o = markup("<span style=\"italic\" weight=\"bold\" size=\"20000\">Hi</span>");
        let v = o.view();
        let s = v.span_style(spans_of(&v)[0]);
        assert_eq!(s.font_style(), Some(FontStyle::Italic));
        assert_eq!(s.font_weight(), Some(700));
        assert_eq!(s.font_size(), Some(FontSize::Points(20000.0 / 1024.0)));
    }

    #[test]
    fn sami_foreground_and_family() {
        let o = markup("<span foreground=\"#00ffff\" font_family=\"Arial\">Hi</span>");
        let v = o.view();
        let s = v.span_style(spans_of(&v)[0]);
        assert_eq!(s.foreground(), Some(Color::rgb(0, 255, 255)));
        assert_eq!(v.style_family(s), Some("Arial"));
    }

    #[test]
    fn sami_named_foreground() {
        let o = markup("<span foreground=\"red\">Red</span>");
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).foreground(), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn sami_ruby_shape_maps_rise_and_size() {
        let o = markup("<span size='xx-small' rise='-100'> anno </span>\nbase");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_text(spans[0]), " anno ");
        assert_eq!(v.span_style(spans[0]).font_size(), Some(size_step(-3)));
        assert_eq!(v.span_style(spans[0]).baseline_shift(), Some(-100.0 / 1024.0));
        assert_eq!(v.span_text(spans[1]), "base");
    }

    #[test]
    fn qttext_font_desc_and_colors() {
        let o = markup(
            "<span font='Arial 20' bgcolor='#000000' color='#FFFF00' weight='bold'>All</span>",
        );
        let v = o.view();
        let s = v.span_style(spans_of(&v)[0]);
        assert_eq!(v.style_family(s), Some("Arial"));
        assert_eq!(s.font_size(), Some(FontSize::Points(20.0)));
        assert_eq!(s.background(), Some(Color::rgb(0, 0, 0)));
        assert_eq!(s.foreground(), Some(Color::rgb(255, 255, 0)));
        assert_eq!(s.font_weight(), Some(700));
    }

    #[test]
    fn qttext_bare_numeric_font_is_a_size() {
        let o = markup("<span font='12'>X</span>");
        let v = o.view();
        let s = v.span_style(spans_of(&v)[0]);
        assert_eq!(v.style_family(s), None);
        assert_eq!(s.font_size(), Some(FontSize::Points(12.0)));
    }

    #[test]
    fn vtt_voice_is_attributed() {
        let o = markup("<v Fred>Hi there");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_voice(spans[0]), Some("Fred"));
        assert_eq!(v.span_text(spans[0]), "Hi there");
    }

    #[test]
    fn vtt_color_classes_style_and_record() {
        let o = markup("<c.yellow.bg_blue>warn</c>done");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_style(spans[0]).foreground(), Some(Color::rgb(255, 255, 0)));
        assert_eq!(v.span_style(spans[0]).background(), Some(Color::rgb(0, 0, 255)));
        assert_eq!(v.span_classes(spans[0]).collect::<Vec<_>>(), ["yellow", "bg_blue"]);
        assert!(v.span_style(spans[1]).is_plain());
        assert_eq!(v.span_class_count(spans[1]), 0);
    }

    #[test]
    fn vtt_ruby_annotation_attaches_to_base() {
        let o = markup("<ruby>base<rt>anno</rt></ruby>tail");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_text(spans[0]), "base");
        let r = v.span_ruby(spans[0]).expect("a ruby");
        assert_eq!((v.ruby_text(r), r.position()), ("anno", RubyPosition::Over));
        assert_eq!(v.span_text(spans[1]), "tail");
        assert!(v.span_ruby(spans[1]).is_none());
        assert_eq!(v.plain_text(), "basetail");
    }

    #[test]
    fn vtt_unclosed_ruby_still_attaches() {
        let o = markup("<ruby>base<rt>anno");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_text(spans[0]), "base");
        assert_eq!(v.span_ruby(spans[0]).map(|r| v.ruby_text(r)), Some("anno"));
    }

    #[test]
    fn a_ruby_without_a_base_gets_an_empty_span() {
        let o = markup("<ruby><rt>anno</rt></ruby>");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 1);
        assert_eq!(v.span_text(spans[0]), "");
        assert_eq!(v.span_ruby(spans[0]).map(|r| v.ruby_text(r)), Some("anno"));
    }

    #[test]
    fn srt_font_color_tags() {
        let o = srt("<font color=\"#ff0000\">red</font> and <font color=lime>green</font>");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_style(spans[0]).foreground(), Some(Color::rgb(255, 0, 0)));
        assert_eq!(v.span_text(spans[1]), " and ");
        assert!(v.span_style(spans[1]).is_plain());
        assert_eq!(v.span_style(spans[2]).foreground(), Some(Color::rgb(0, 255, 0)));
        assert_eq!(v.plain_text(), "red and green");
    }

    #[test]
    fn srt_font_face_and_size() {
        let o = srt("<font face=\"Comic Sans\" size=\"5\">x</font>");
        let v = o.view();
        let s = v.span_style(spans_of(&v)[0]);
        assert_eq!(v.style_family(s), Some("Comic Sans"));
        assert_eq!(s.font_size(), Some(size_step(2)));
        let o = srt("<font size=+1>x</font>");
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).font_size(), Some(size_step(1)));
    }

    #[test]
    fn srt_simple_tags_and_case_insensitivity() {
        let o = srt("<I>it</I><B>bo</B><u>un</u>");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_style(spans[0]).font_style(), Some(FontStyle::Italic));
        assert_eq!(v.span_style(spans[1]).font_weight(), Some(700));
        assert_eq!(v.span_style(spans[2]).underline(), Some(true));
    }

    #[test]
    fn srt_unclosed_font_closes_at_end() {
        let o = srt("<font color=\"cyan\">all of it");
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).foreground(), Some(Color::rgb(0, 255, 255)));
    }

    #[test]
    fn srt_raw_text_is_not_markup() {
        assert_eq!(srt("5 &lt; 6 & <3 love").plain_text(), "5 &lt; 6 & <3 love");
        assert_eq!(srt("a <3> b").plain_text(), "a <3> b");
        // Not whitelisted, literal like the C.
        assert_eq!(srt("a < x> b").plain_text(), "a < x> b");
        // Whitelisted despite the space, the C unescapes it too.
        let o = srt("< i>x</i>");
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).font_style(), Some(FontStyle::Italic));
    }

    #[test]
    fn srt_unknown_letter_tags_drop_keeping_content() {
        // What the C's remove_unhandled_tags does to the visible text.
        assert_eq!(srt("a <blink>b</blink> c").plain_text(), "a b c");
    }

    #[test]
    fn srt_an_blocks_set_the_anchor() {
        let o = srt("{\\an8}on top");
        let v = o.view();
        assert_eq!(v.layout().anchor(), Some(Anchor::TopCenter));
        assert_eq!(v.layout().align(), Some(TextAlign::Center));
        assert_eq!(v.plain_text(), "on top");
        let o = srt("{\\a6}{\\an1}x");
        assert_eq!(o.view().layout().anchor(), Some(Anchor::TopCenter));
        assert_eq!(srt("{\\pos(1,2)}a {not a tag} b").plain_text(), "a {not a tag} b");
        assert_eq!(srt("a {\\an8").plain_text(), "a {\\an8");
    }

    #[test]
    fn inline_timestamps_become_reveal_times() {
        let o = markup("One... &lt;00:00:00,200&gt;Two... &lt;00:00:00,500&gt;Three...");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 3);
        assert_eq!((v.span_text(spans[0]), spans[0].reveal_ns()), ("One... ", None));
        assert_eq!((v.span_text(spans[1]), spans[1].reveal_ns()), ("Two... ", Some(200_000_000)));
        assert_eq!((v.span_text(spans[2]), spans[2].reveal_ns()), ("Three...", Some(500_000_000)));
        assert_eq!(v.plain_text(), "One... Two... Three...");
    }

    #[test]
    fn inline_timestamp_variants() {
        let o = markup("&lt;01:02:03.004&gt;x");
        let v = o.view();
        assert_eq!(
            spans_of(&v)[0].reveal_ns(),
            Some((3600 + 2 * 60 + 3) * 1_000_000_000 + 4_000_000)
        );
        let o = markup("&lt;00:01.5&gt;x");
        assert_eq!(spans_of(&o.view())[0].reveal_ns(), Some(1_500_000_000));
    }

    #[test]
    fn inline_timestamps_keep_the_surrounding_styling() {
        let o = markup("<c.yellow>la&lt;00:00:01.000&gt;la</c>");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].reveal_ns(), None);
        assert_eq!(spans[1].reveal_ns(), Some(1_000_000_000));
        for s in spans {
            assert_eq!(v.span_style(s).foreground(), Some(Color::rgb(255, 255, 0)));
            assert_eq!(v.span_classes(s).collect::<Vec<_>>(), ["yellow"]);
        }
    }

    #[test]
    fn absurd_inline_timestamps_stay_literal_text() {
        let o = markup("&lt;999999999:00:00.0&gt;x");
        let v = o.view();
        assert_eq!(spans_of(&v)[0].reveal_ns(), None);
        assert_eq!(v.plain_text(), "<999999999:00:00.0>x");
    }

    #[test]
    fn non_timestamps_stay_literal_text() {
        assert_eq!(markup("see you &lt;12:30&gt;").plain_text(), "see you <12:30>");
        assert_eq!(markup("&lt;123&gt;").plain_text(), "<123>");
        assert_eq!(markup("a &lt; b &gt; c").plain_text(), "a < b > c");
    }

    #[test]
    fn cue_prefers_raw_text() {
        let mut cue = Cue::new(0, Some(1), "colored");
        cue.raw_text = Some("<font color=\"red\">colored</font>".to_owned());
        let o = build(|b| b.cue(&cue, OutputFormat::PangoMarkup, None));
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).foreground(), Some(Color::rgb(255, 0, 0)));
        assert_eq!(v.plain_text(), "colored");
    }

    #[test]
    fn argless_cue_rule_becomes_base_style() {
        let o = styled("plain <b>bold</b>", "::cue { color: yellow; background: black }");
        let v = o.view();
        assert_eq!(v.base_style().foreground(), Some(Color::rgb(255, 255, 0)));
        assert_eq!(v.base_style().background(), Some(Color::rgb(0, 0, 0)));
        assert_eq!(v.span_style(spans_of(&v)[0]).foreground(), None);
    }

    #[test]
    fn type_rule_styles_matching_tags() {
        let o = styled("a<b>bee</b><i>eye</i>", "::cue(b) { color: red }");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_style(spans[0]).foreground(), None);
        assert_eq!(v.span_style(spans[1]).foreground(), Some(Color::rgb(255, 0, 0)));
        assert_eq!(v.span_style(spans[1]).font_weight(), Some(700));
        assert_eq!(v.span_style(spans[2]).foreground(), None);
    }

    #[test]
    fn author_css_overrides_tag_styling() {
        let o = styled("<i>x</i>", "::cue(i) { font-style: normal }");
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).font_style(), Some(FontStyle::Normal));
    }

    #[test]
    fn author_css_overrides_default_color_classes() {
        let o = styled("<c.yellow>x</c>", "::cue(.yellow) { color: #123456 }");
        let v = o.view();
        let span = spans_of(&v)[0];
        assert_eq!(v.span_style(span).foreground(), Some(Color::rgb(0x12, 0x34, 0x56)));
        assert_eq!(v.span_classes(span).collect::<Vec<_>>(), ["yellow"]);
    }

    #[test]
    fn voice_rule_matches_annotation() {
        let o = styled("<v Fred>hi</v><v Wilma>ho</v>", "::cue(v[voice=\"Fred\"]) { color: red }");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_style(spans[0]).foreground(), Some(Color::rgb(255, 0, 0)));
        assert_eq!(v.span_style(spans[1]).foreground(), None);
    }

    #[test]
    fn classes_and_annotation_split_on_any_tag() {
        let o = styled("<v.loud Fred>hi", "::cue(.loud) { color: red }");
        let v = o.view();
        let span = spans_of(&v)[0];
        assert_eq!(v.span_voice(span), Some("Fred"));
        assert_eq!(v.span_classes(span).collect::<Vec<_>>(), ["loud"]);
        assert_eq!(v.span_style(span).foreground(), Some(Color::rgb(255, 0, 0)));
        let o = markup("<b.loud>x</b>");
        let v = o.view();
        assert_eq!(v.span_classes(spans_of(&v)[0]).collect::<Vec<_>>(), ["loud"]);
    }

    #[test]
    fn descendant_tag_styling_beats_inherited_author_value() {
        let o = styled("<v Fred><i>x</i></v>", "::cue(v) { font-style: normal }");
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).font_style(), Some(FontStyle::Italic));
    }

    #[test]
    fn deeper_author_rule_beats_shallower_one() {
        let o = styled("<v Fred><c>x</c></v>", "::cue(v) { color: blue } ::cue(c) { color: red }");
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).foreground(), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn id_rule_matches_cue_identifier() {
        let sheet = Stylesheet::parse("::cue(#intro) { color: red }");
        let o = build(|b| b.pango_markup("x", Some(&sheet), Some("intro")));
        assert_eq!(o.view().base_style().foreground(), Some(Color::rgb(255, 0, 0)));
        let o = build(|b| b.pango_markup("x", Some(&sheet), Some("outro")));
        assert_eq!(o.view().base_style().foreground(), None);
    }

    #[test]
    fn lang_rule_matches_lang_tags() {
        let o = styled(
            "<lang en-GB>tea</lang><lang fr>caf\u{e9}</lang>",
            "::cue(:lang(en)) { color: red }",
        );
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(v.span_style(spans[0]).foreground(), Some(Color::rgb(255, 0, 0)));
        assert_eq!(v.style_language(v.span_style(spans[0])), Some("en-GB"));
        assert_eq!(v.span_style(spans[1]).foreground(), None);
        // The language is inherited by nested nodes.
        let o = styled("<lang en><i>x</i></lang>", "::cue(:lang(en)) { color: red }");
        let v = o.view();
        assert_eq!(v.span_style(spans_of(&v)[0]).foreground(), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn ruby_position_rule_moves_annotation() {
        let o = styled("<ruby>base<rt>anno</rt></ruby>", "::cue(rt) { ruby-position: under }");
        let v = o.view();
        let r = v.span_ruby(spans_of(&v)[0]).expect("a ruby");
        assert_eq!((v.ruby_text(r), r.position()), ("anno", RubyPosition::Under));
    }

    #[test]
    fn cue_feeds_id_and_sheet() {
        let sheet = Stylesheet::parse("::cue(#greeting) { color: lime }");
        let mut cue = Cue::new(0, Some(1), "hello");
        cue.id = Some("greeting".to_owned());
        let o = build(|b| b.cue(&cue, OutputFormat::PangoMarkup, Some(&sheet)));
        assert_eq!(o.view().base_style().foreground(), Some(Color::rgb(0, 255, 0)));
    }

    #[test]
    fn empty_sheet_changes_nothing() {
        let sheet = Stylesheet::parse("");
        let with = build(|b| b.pango_markup("<i>x</i>", Some(&sheet), None));
        let without = markup("<i>x</i>");
        assert_eq!(with.0, without.0);
    }

    #[test]
    fn cue_settings_fold_into_layout() {
        let mut cue = Cue::new(0, Some(1), "x");
        cue.settings = CueSettings {
            line_position: Some(10),
            text_position: Some(50),
            text_size: Some(35),
            vertical: Some("vertical-lr".to_owned()),
            alignment: Some("middle".to_owned()),
            ..CueSettings::default()
        };
        let o = build(|b| b.cue(&cue, OutputFormat::PangoMarkup, None));
        let l = *o.view().layout();
        assert_eq!(l.line(), Some(LinePosition::Percent(10.0)));
        assert_eq!(l.position(), Some(50.0));
        assert_eq!(l.size(), Some(35.0));
        assert_eq!(l.writing_mode(), WritingMode::VerticalLr);
        assert_eq!(l.align(), Some(TextAlign::Center));
    }

    #[test]
    fn modern_cue_settings_fold_into_layout() {
        let mut cue = Cue::new(0, Some(1), "x");
        cue.settings = CueSettings {
            line_number: Some(-1),
            line_align: Some("end".to_owned()),
            text_position: Some(50),
            position_align: Some("line-left".to_owned()),
            vertical: Some("rl".to_owned()),
            alignment: Some("center".to_owned()),
            ..CueSettings::default()
        };
        let o = build(|b| b.cue(&cue, OutputFormat::PangoMarkup, None));
        let l = *o.view().layout();
        assert_eq!(l.line(), Some(LinePosition::Line(-1)));
        assert_eq!(l.line_align(), Some(LineAlign::End));
        assert_eq!(l.position(), Some(50.0));
        assert_eq!(l.position_align(), Some(PositionAlign::LineLeft));
        assert_eq!(l.writing_mode(), WritingMode::VerticalRl);
        assert_eq!(l.align(), Some(TextAlign::Center));
    }

    #[test]
    fn cue_trims_trailing_newlines() {
        let cue = Cue::new(0, Some(1), "One\n\n");
        let o = build(|b| b.cue(&cue, OutputFormat::Utf8, None));
        assert_eq!(o.view().line_count(), 1);
        assert_eq!(o.plain_text(), "One");
    }

    #[test]
    fn a_cleared_field_leaves_no_trace_in_the_style_table() {
        let d = crate::ssastyle::SsaEvent {
            raw_text: "{\\bord4}a{\\bord0}b{\\k0}c{\\k0}d",
            style: "Default",
            margin_l: 0,
            margin_r: 0,
            margin_v: 0,
        };
        let o = build(|b| crate::ssastyle::dialogue_into(&d, &Default::default(), 0, b));
        let v = o.view();
        let texts: Vec<&str> = spans_of(&v).iter().map(|s| v.span_text(s)).collect();
        assert_eq!(texts, ["a", "b", "cd"], "a cleared outline reads as the base style");
        assert!(v.styles().iter().all(|s| *s == s.canonical()));
    }

    #[test]
    fn a_copy_keeps_an_empty_ruby_and_an_empty_base_span() {
        let o = markup("<ruby><rt>x</rt></ruby>");
        let v = o.view();
        let r = v.span_ruby(spans_of(&v)[0]).expect("a ruby");
        let at = v.header().off_rubies as usize + 4;
        let mut bytes = o.0.clone();
        bytes[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(r.text.len, 1);
        let v = CueIr::parse(&bytes).expect("an empty ruby is valid");
        let mut b = CueBuilder::new();
        b.copy_from(&v);
        let again = b.to_vec();
        let back = CueIr::parse(&again).expect("the copy validates");
        assert!(back.same_as(&v));
        assert_eq!(back.span_ruby(spans_of(&back)[0]).map(|r| back.ruby_text(r)), Some(""));
    }

    #[test]
    fn markup_plain_text_matches_strip_semantics() {
        assert_eq!(markup("<i>Six</i>").plain_text(), "Six");
        assert_eq!(markup("gave <i>Rock &amp; Roll</i> to").plain_text(), "gave Rock & Roll to");
        assert_eq!(
            markup("a &lt; b &unknown; &#177;").plain_text(),
            format!("a < b &unknown; {}", char::from_u32(177).unwrap())
        );
    }

    #[test]
    fn names_share_equal_strings_and_keep_distinct_ones_apart() {
        let mut names = Names::default();
        let a = names.intern("Arial");
        let b = names.intern("Verdana");
        assert_eq!(names.intern("Arial"), a);
        assert_ne!(a, b);
        assert_eq!((names.get(a), names.get(b)), ("Arial", "Verdana"));
        let refs: Vec<_> = (0..5_000).map(|i| names.intern(&format!("f{i}"))).collect();
        for (i, r) in refs.iter().enumerate() {
            assert_eq!(names.get(*r), format!("f{i}"));
            assert_eq!(names.intern(&format!("f{i}")), *r);
        }
        names.clear();
        assert!(names.is_empty());
        assert_eq!(names.intern("Verdana").off, 0);
    }

    #[test]
    fn a_cue_past_the_limit_stops_growing_and_is_refused() {
        let mut b = CueBuilder::new();
        let mut text = String::new();
        for i in 0..200_000u32 {
            text.push_str(&format!("<font color=#{:06x}>a</font>", i & 0xff_ffff));
        }
        b.srt_text(&text);
        assert!(b.overflowed());
        assert!(b.wire_len() <= CUE_LIMIT + 128, "{}", b.wire_len());
        let mut slot = vec![0u8; CUE_LIMIT];
        assert_eq!(b.finish(&mut slot), Err(EncodeError::DoesNotFit { capacity: CUE_LIMIT }));
        b.srt_text("fits again");
        assert!(!b.overflowed());
        assert!(b.finish(&mut slot).is_ok());
    }

    #[test]
    fn markup_deeper_than_the_cap_is_dropped_and_closers_stay_cheap() {
        let deep = format!("{}x{}", "<c.a>".repeat(10_000), "</q>".repeat(10_000));
        let o = markup(&deep);
        let v = o.view();
        assert_eq!(v.plain_text(), "x");
        assert_eq!(v.span_class_count(spans_of(&v)[0]), MAX_MARKUP_DEPTH);
    }

    #[test]
    fn non_finite_sizes_and_shifts_never_reach_the_wire() {
        let o = srt("<span size=\"NaN\" rise=\"inf\" letter_spacing=\"-inf\">x</span>");
        let v = o.view();
        let s = v.span_style(spans_of(&v)[0]);
        assert_eq!((s.font_size(), s.baseline_shift(), s.letter_spacing()), (None, None, None));
        let mut bad = StyleRec::default();
        bad.set_font_size(Some(FontSize::Points(f32::NAN)));
        assert!(bad.canonical().font_size().is_none());
        assert!(bad.validate().is_err());
        let mut layout = LayoutRec::default();
        layout.set_origin(Some((f32::INFINITY, 1.0)));
        assert!(layout.canonical().origin().is_none());
        assert!(layout.validate().is_err());
    }

    #[test]
    fn view_helpers_take_refs_and_indices_from_elsewhere_without_panicking() {
        let o = srt("one\ntwo");
        let v = o.view();
        assert_eq!(v.str(StrRef { off: u32::MAX, len: u32::MAX }), "");
        assert_eq!(v.line_text(99).count(), 0);
    }
}
