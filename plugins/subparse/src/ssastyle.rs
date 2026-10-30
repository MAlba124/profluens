// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! SSA/ASS styling for the `cue-ir` output path, the `[V4(+) Styles]` registry and the `{\...}`
//! override-tag parser.

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;

use crate::ir::{
    Anchor, Color, CueBuilder, FontSize, FontStyle, LayoutRec, Margins, Names, Outline, Shadow,
    StyleRec, TextAlign,
};

/// `Style:` lines kept per script, the rest are ignored.
pub const MAX_STYLES: usize = 4096;

/// `Format:` columns read, v4+ declares 23.
const MAX_COLUMNS: usize = 64;

/// What one `Dialogue:` event carries beyond its stripped text, attached to
/// [`crate::cue::Cue::ssa`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SsaDialogue {
    /// The Text field verbatim, `{...}` override blocks intact.
    pub raw_text: String,
    /// The name of a `[V4(+) Styles]` definition.
    pub style: String,
    /// Margin overrides in `PlayRes` pixels, `0` means the style's.
    pub margin_l: u32,
    pub margin_r: u32,
    pub margin_v: u32,
}

/// One `Style:` definition.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Style {
    pub font_family: Option<String>,
    /// Font size in `PlayRes` pixels.
    pub font_size: Option<f32>,
    /// PrimaryColour, the text fill.
    pub primary: Option<Color>,
    /// OutlineColour (v4+) or TertiaryColour (v4), the border stroke.
    pub outline_color: Option<Color>,
    /// BackColour, the shadow color. VSFilter fills the `BorderStyle=3` box with OutlineColour.
    pub back_color: Option<Color>,
    /// CSS-style weight (SSA `Bold` is a boolean, `-1` = bold).
    pub font_weight: Option<u16>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strikeout: Option<bool>,
    /// ScaleX/ScaleY, percent (100 = unscaled).
    pub scale_x: Option<f32>,
    pub scale_y: Option<f32>,
    /// Extra letter spacing, `PlayRes` pixels.
    pub spacing: Option<f32>,
    /// 1 = outline + shadow, 3 = opaque box.
    pub border_style: Option<i32>,
    /// Outline width, `PlayRes` pixels.
    pub outline_w: Option<f32>,
    /// Shadow depth (dx = dy), `PlayRes` pixels.
    pub shadow_d: Option<f32>,
    /// Raw Alignment column (numpad in v4+, legacy encoding in v4).
    pub alignment: Option<i32>,
    /// Margins, `PlayRes` pixels.
    pub margin_l: Option<f32>,
    pub margin_r: Option<f32>,
    pub margin_v: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    Other,
    ScriptInfo,
    Styles {
        v4plus: bool,
    },
}

/// `PlayRes` and the style definitions of one script.
#[derive(Debug, Clone, Default)]
pub struct SsaStyles {
    pub play_res_x: Option<f32>,
    pub play_res_y: Option<f32>,
    styles: Vec<(String, Style)>,
    /// Name to index with leading `*`s stripped, as written and ASCII-lowercased, the first
    /// definition of a name wins.
    exact: HashMap<String, u32>,
    folded: HashMap<String, u32>,
    v4plus: bool,
    section: Section,
    format: Vec<String>,
}

const V4_FORMAT: &[&str] = &[
    "name",
    "fontname",
    "fontsize",
    "primarycolour",
    "secondarycolour",
    "tertiarycolour",
    "backcolour",
    "bold",
    "italic",
    "borderstyle",
    "outline",
    "shadow",
    "alignment",
    "marginl",
    "marginr",
    "marginv",
    "alphalevel",
    "encoding",
];

const V4PLUS_FORMAT: &[&str] = &[
    "name",
    "fontname",
    "fontsize",
    "primarycolour",
    "secondarycolour",
    "outlinecolour",
    "backcolour",
    "bold",
    "italic",
    "underline",
    "strikeout",
    "scalex",
    "scaley",
    "spacing",
    "angle",
    "borderstyle",
    "outline",
    "shadow",
    "alignment",
    "marginl",
    "marginr",
    "marginv",
    "encoding",
];

impl SsaStyles {
    /// Parse a whole header/init section (the `codec_data` a demuxer hands `ssaparse`).
    pub fn parse(text: &str) -> SsaStyles {
        let mut s = SsaStyles::default();
        for line in text.lines() {
            s.feed_line(line.strip_suffix('\r').unwrap_or(line));
        }
        s
    }

    /// Whether anything usable was collected.
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty() && self.play_res_x.is_none() && self.play_res_y.is_none()
    }

    /// The styles in declaration order.
    pub fn styles(&self) -> impl Iterator<Item = (&str, &Style)> {
        self.styles.iter().map(|(n, s)| (n.as_str(), s))
    }

    /// Feed one script line.
    pub fn feed_line(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('[') {
            let name = match rest.split_once(']') {
                Some((name, _)) => name,
                None => rest.trim_end(),
            };
            self.section = if name.eq_ignore_ascii_case("script info") {
                Section::ScriptInfo
            } else if name.eq_ignore_ascii_case("v4 styles") {
                self.enter_styles(false);
                Section::Styles { v4plus: false }
            } else if name.eq_ignore_ascii_case("v4+ styles")
                || name.eq_ignore_ascii_case("v4++ styles")
            {
                self.enter_styles(true);
                Section::Styles { v4plus: true }
            } else {
                Section::Other
            };
            return;
        }

        let Some((keyword, value)) = split_keyword(trimmed) else {
            return;
        };
        match self.section {
            Section::ScriptInfo => {
                if keyword.eq_ignore_ascii_case("PlayResX") {
                    self.play_res_x = value.trim().parse::<f32>().ok().filter(|v| *v > 0.0);
                } else if keyword.eq_ignore_ascii_case("PlayResY") {
                    self.play_res_y = value.trim().parse::<f32>().ok().filter(|v| *v > 0.0);
                }
            }
            Section::Styles { v4plus } => {
                if keyword.eq_ignore_ascii_case("Format") {
                    // Past the columns a style has, the rest of a hostile line is ignored.
                    self.format = value
                        .split(',')
                        .take(MAX_COLUMNS)
                        .map(|n| n.trim().to_ascii_lowercase())
                        .collect();
                } else if keyword.eq_ignore_ascii_case("Style") {
                    self.push_style(value, v4plus);
                }
            }
            Section::Other => {}
        }
    }

    fn enter_styles(&mut self, v4plus: bool) {
        self.v4plus = v4plus;
        let default = if v4plus { V4PLUS_FORMAT } else { V4_FORMAT };
        self.format = default.iter().map(|s| (*s).to_owned()).collect();
    }

    fn push_style(&mut self, value: &str, v4plus: bool) {
        let mut name = String::new();
        let mut style = Style::default();
        for (col, field) in self.format.iter().zip(value.split(',')) {
            let field = field.trim();
            if field.is_empty() {
                continue;
            }
            match col.as_str() {
                "name" => name = field.to_owned(),
                "fontname" => style.font_family = Some(field.to_owned()),
                "fontsize" => style.font_size = field.parse().ok(),
                "primarycolour" => style.primary = parse_ssa_color(field),
                "outlinecolour" | "tertiarycolour" => style.outline_color = parse_ssa_color(field),
                "backcolour" => style.back_color = parse_ssa_color(field),
                "bold" => style.font_weight = parse_bold(field),
                "italic" => style.italic = parse_ssa_bool(field),
                "underline" => style.underline = parse_ssa_bool(field),
                "strikeout" => style.strikeout = parse_ssa_bool(field),
                "scalex" => style.scale_x = field.parse().ok(),
                "scaley" => style.scale_y = field.parse().ok(),
                "spacing" => style.spacing = field.parse().ok(),
                "borderstyle" => style.border_style = field.parse().ok(),
                "outline" => style.outline_w = field.parse().ok(),
                "shadow" => style.shadow_d = field.parse().ok(),
                "alignment" => {
                    style.alignment = field
                        .parse::<i32>()
                        .ok()
                        .and_then(|a| if v4plus { Some(a) } else { legacy_to_numpad(a) })
                }
                "marginl" => style.margin_l = field.parse().ok(),
                "marginr" => style.margin_r = field.parse().ok(),
                "marginv" => style.margin_v = field.parse().ok(),
                _ => {}
            }
        }
        if name.is_empty() || self.styles.len() >= MAX_STYLES {
            return;
        }
        let at = self.styles.len() as u32;
        let key = name.trim_start_matches('*');
        self.exact.entry(key.to_owned()).or_insert(at);
        self.folded.entry(key.to_ascii_lowercase()).or_insert(at);
        self.styles.push((name, style));
    }

    /// Look up a style by name, exact then ASCII case-insensitive, leading `*`s stripped
    /// (VSFilter).
    pub fn lookup(&self, name: &str) -> Option<&Style> {
        let wanted = name.trim_start_matches('*');
        let folded_at = |key: &str| self.folded.get(key).copied();
        let at = match self.exact.get(wanted).copied() {
            Some(at) => at,
            None if self.folded.is_empty() => return None,
            None => {
                // Folded on the stack for the usual short name, so a miss allocates nothing.
                let mut small = [0u8; 64];
                match small.get_mut(..wanted.len()) {
                    Some(buf) => {
                        buf.copy_from_slice(wanted.as_bytes());
                        buf.make_ascii_lowercase();
                        folded_at(std::str::from_utf8(buf).ok()?)?
                    }
                    None => folded_at(&wanted.to_ascii_lowercase())?,
                }
            }
        };
        self.styles.get(at as usize).map(|(_, s)| s)
    }

    /// The effective script resolution, declared, derived 4:3 from one axis, or the 384x288
    /// default.
    pub fn play_res(&self) -> (f32, f32) {
        match (self.play_res_x, self.play_res_y) {
            (Some(x), Some(y)) => (x, y),
            (Some(x), None) => (x, x * 3.0 / 4.0),
            (None, Some(y)) => (y * 4.0 / 3.0, y),
            (None, None) => (384.0, 288.0),
        }
    }
}

fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(':')?;
    Some((line[..idx].trim(), line[idx + 1..].trim_start_matches(' ')))
}

/// Parse an SSA/ASS color, `&HAABBGGRR&` hex or a decimal integer.
pub fn parse_ssa_color(s: &str) -> Option<Color> {
    let v = parse_ssa_scalar(s)?;
    Some(Color {
        r: (v & 0xff) as u8,
        g: ((v >> 8) & 0xff) as u8,
        b: ((v >> 16) & 0xff) as u8,
        a: 255 - ((v >> 24) & 0xff) as u8,
    })
}

fn parse_ssa_alpha(s: &str) -> Option<u8> {
    let v = parse_ssa_scalar(s)?;
    Some(255 - (v & 0xff) as u8)
}

/// The `&H...&` or decimal scalar, wrapping to 32 bits like the C parsers.
fn parse_ssa_scalar(s: &str) -> Option<u32> {
    let t = s.trim().trim_end_matches('&');
    if let Some(hex) = t
        .strip_prefix("&H")
        .or_else(|| t.strip_prefix("&h"))
        .or_else(|| t.strip_prefix("H"))
        .or_else(|| t.strip_prefix("h"))
    {
        return u64::from_str_radix(hex, 16).ok().map(|v| v as u32);
    }
    t.parse::<i64>().ok().map(|v| v as u32)
}

fn parse_ssa_bool(s: &str) -> Option<bool> {
    s.parse::<i32>().ok().map(|v| v != 0)
}

fn parse_bold(s: &str) -> Option<u16> {
    let v = s.parse::<i32>().ok()?;
    Some(match v {
        0 => 400,
        -1 | 1 => 700,
        100..=1000 => v as u16,
        _ => 700,
    })
}

pub(crate) fn numpad_anchor(n: i32) -> Option<(Anchor, TextAlign)> {
    Some(match n {
        1 => (Anchor::BottomLeft, TextAlign::Left),
        2 => (Anchor::BottomCenter, TextAlign::Center),
        3 => (Anchor::BottomRight, TextAlign::Right),
        4 => (Anchor::CenterLeft, TextAlign::Left),
        5 => (Anchor::Center, TextAlign::Center),
        6 => (Anchor::CenterRight, TextAlign::Right),
        7 => (Anchor::TopLeft, TextAlign::Left),
        8 => (Anchor::TopCenter, TextAlign::Center),
        9 => (Anchor::TopRight, TextAlign::Right),
        _ => return None,
    })
}

/// Legacy alignment (SSA v4 `Alignment`, `\a`) to numpad. 1-3 bottom, +4 top, +8 middle.
pub(crate) fn legacy_to_numpad(a: i32) -> Option<i32> {
    Some(match a {
        1..=3 => a,
        5..=7 => a + 2,
        9..=11 => a - 5,
        _ => return None,
    })
}

/// VSFilter's default `Outline`, for a tag that needs one when nothing set it.
const DEFAULT_OUTLINE_W: f32 = 2.0;

/// One event's extras borrowed from its row, what [`dialogue_into`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SsaEvent<'a> {
    /// The Text field verbatim, `{...}` override blocks intact.
    pub raw_text: &'a str,
    /// The name of a `[V4(+) Styles]` definition.
    pub style: &'a str,
    /// Margin overrides in `PlayRes` pixels, `0` means the style's.
    pub margin_l: u32,
    pub margin_r: u32,
    pub margin_v: u32,
}

impl SsaDialogue {
    pub fn event(&self) -> SsaEvent<'_> {
        SsaEvent {
            raw_text: &self.raw_text,
            style: &self.style,
            margin_l: self.margin_l,
            margin_r: self.margin_r,
            margin_v: self.margin_v,
        }
    }
}

fn style_span(style: &Style, res_y: f32, names: &mut Names) -> StyleRec {
    let mut s = StyleRec::default();
    s.set_family(style.font_family.as_deref().map(|f| names.intern(f)));
    s.set_font_style(style.italic.map(|i| if i { FontStyle::Italic } else { FontStyle::Normal }));
    s.set_font_weight(style.font_weight);
    s.set_underline(style.underline);
    s.set_strikethrough(style.strikeout);
    s.set_foreground(style.primary);
    s.set_letter_spacing(style.spacing.filter(|v| *v != 0.0));
    if let Some(size) = style.font_size.filter(|v| *v > 0.0) {
        s.set_font_size(Some(FontSize::FrameHeightPercent(size / res_y * 100.0)));
    }
    // BorderStyle 3 draws a cue-level opaque box instead of an outline.
    if style.border_style != Some(3) {
        let width = style.outline_w.unwrap_or(DEFAULT_OUTLINE_W);
        if width > 0.0 && (style.outline_color.is_some() || style.outline_w.is_some()) {
            s.set_outline(Some(Outline {
                color: style.outline_color.unwrap_or(Color::BLACK),
                width,
            }));
        }
        if let Some(d) = style.shadow_d.filter(|d| *d > 0.0) {
            s.set_shadow(Some(Shadow {
                color: style.back_color.unwrap_or(Color::rgba(0, 0, 0, 128)),
                dx: d,
                dy: d,
                blur: 0.0,
            }));
        }
    }
    match (style.scale_x, style.scale_y) {
        (None, None) => {}
        (sx, sy) => {
            let (sx, sy) = (sx.unwrap_or(100.0) / 100.0, sy.unwrap_or(100.0) / 100.0);
            if (sx, sy) != (1.0, 1.0) {
                s.set_scale(Some((sx, sy)));
            }
        }
    }
    s
}

fn style_layout(style: &Style, d: &SsaEvent<'_>, res: (f32, f32)) -> LayoutRec {
    let mut layout = LayoutRec::default();
    if let Some((anchor, align)) = style.alignment.and_then(numpad_anchor) {
        layout.set_anchor(Some(anchor));
        layout.set_align(Some(align));
    }

    let margin = |over: u32, style_m: Option<f32>| -> Option<f32> {
        if over > 0 { Some(over as f32) } else { style_m.filter(|m| *m > 0.0) }
    };
    let (res_x, res_y) = res;
    let l = margin(d.margin_l, style.margin_l).map(|m| m / res_x * 100.0);
    let r = margin(d.margin_r, style.margin_r).map(|m| m / res_x * 100.0);
    let v = margin(d.margin_v, style.margin_v).map(|m| m / res_y * 100.0);
    if l.is_some() || r.is_some() || v.is_some() {
        layout.set_margins(Some(Margins {
            left: l.unwrap_or(0.0),
            right: r.unwrap_or(0.0),
            vertical: v.unwrap_or(0.0),
        }));
    }

    // The box uses the outline color, as VSFilter does. BackColour is the shadow's.
    if style.border_style == Some(3) {
        layout.set_background(Some(style.outline_color.unwrap_or(Color::rgba(0, 0, 0, 128))));
    }
    layout
}

/// Build the styled cue for one event into `b`.
pub fn dialogue_into(d: &SsaEvent<'_>, styles: &SsaStyles, start_ns: u64, b: &mut CueBuilder) {
    b.reset();
    let res = styles.play_res();
    let style = styles.lookup(d.style);
    let fallback = Style::default();
    b.base = match style {
        Some(s) => style_span(s, res.1, b.names()),
        None => StyleRec::default(),
    };
    b.layout = style_layout(style.unwrap_or(&fallback), d, res);

    let mut p = TextParser {
        b,
        styles,
        res,
        cur_style: StyleRec::default(),
        reveal_ns: None,
        karaoke_ns: start_ns,
        an_set: false,
        pos_set: false,
        drawing: false,
    };
    p.run(d.raw_text);
    p.finish();
}

struct TextParser<'a> {
    b: &'a mut CueBuilder,
    styles: &'a SsaStyles,
    res: (f32, f32),
    cur_style: StyleRec,
    reveal_ns: Option<u64>,
    karaoke_ns: u64,
    an_set: bool,
    pos_set: bool,
    drawing: bool,
}

impl TextParser<'_> {
    fn run(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => match text[i + 1..].find('}') {
                    Some(rel) => {
                        let block = &text[i + 1..i + 1 + rel];
                        self.override_block(block);
                        i += rel + 2;
                    }
                    None => {
                        // Unmatched '{', the C keeps the rest verbatim, escapes untranslated.
                        self.text(&text[i..]);
                        return;
                    }
                },
                // Escape a whole character, not a byte. No `i + 1 < len` guard, a guarded arm left
                // a trailing backslash spinning on `Some(0)`.
                b'\\' => match text[i + 1..].chars().next() {
                    None => {
                        self.text(&text[i..]);
                        return;
                    }
                    Some(escaped) => {
                        let end = i + 1 + escaped.len_utf8();
                        match escaped {
                            'N' | 'n' => self.end_line(),
                            'h' => self.text("\u{a0}"),
                            _ => self.text(&text[i..end]),
                        }
                        i = end;
                    }
                },
                _ => {
                    let end = text[i..].find(['{', '\\']).map(|rel| i + rel).unwrap_or(text.len());
                    // Both delimiters have their own arm, so `Some(0)` here would spin.
                    debug_assert!(end > i, "the scanner must advance: {text:?} at {i}");
                    self.text(&text[i..end]);
                    i = end;
                }
            }
        }
    }

    fn text(&mut self, s: &str) {
        if !self.drawing {
            self.b.push_text(s);
        }
    }

    fn flush(&mut self) {
        self.b.flush(&self.cur_style, None, self.reveal_ns);
    }

    fn end_line(&mut self) {
        self.flush();
        self.b.end_line();
    }

    fn finish(&mut self) {
        self.flush();
        self.b.end_last_line();
    }

    fn override_block(&mut self, block: &str) {
        self.flush();
        let bytes = block.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'\\' {
                i += 1;
                continue;
            }
            i += 1;
            let (name, arg, next) = read_tag(block, i);
            if name.is_empty() {
                continue;
            }
            self.apply_tag(name, arg);
            i = next;
        }
    }

    fn apply_tag(&mut self, name: &str, arg: &str) {
        let arg = arg.trim();
        let s = &mut self.cur_style;
        match name {
            "i" => s.set_font_style(
                flag(arg).map(|on| if on { FontStyle::Italic } else { FontStyle::Normal }),
            ),
            "b" => s.set_font_weight(if arg.is_empty() { None } else { parse_bold(arg) }),
            "u" => s.set_underline(flag(arg)),
            "s" => s.set_strikethrough(flag(arg)),
            "fn" => {
                let family = (!arg.is_empty()).then(|| self.b.intern(arg));
                self.cur_style.set_family(family);
            }
            "fs" => {
                // Relative `\fs+n`/`\fs-n` need the inherited size, so only absolute applies.
                s.set_font_size(match arg.parse::<f32>() {
                    Ok(v) if v > 0.0 && !arg.starts_with(['+', '-']) => {
                        Some(FontSize::FrameHeightPercent(v / self.res.1 * 100.0))
                    }
                    _ => None,
                });
            }
            "fsp" => s.set_letter_spacing(arg.parse().ok()),
            "fscx" => {
                let (_, y) = current_scale(s);
                s.set_scale(arg.parse::<f32>().ok().map(|x| (x / 100.0, y)));
            }
            "fscy" => {
                let (x, _) = current_scale(s);
                s.set_scale(arg.parse::<f32>().ok().map(|y| (x, y / 100.0)));
            }
            "c" | "1c" => s.set_foreground(parse_ssa_color(arg)),
            "3c" => {
                let width = s.outline().map_or(DEFAULT_OUTLINE_W, |o| o.width);
                s.set_outline(parse_ssa_color(arg).map(|color| Outline { color, width }));
            }
            "4c" => {
                if let Some(color) = parse_ssa_color(arg) {
                    let sh = s.shadow().unwrap_or(Shadow { color, dx: 0.0, dy: 0.0, blur: 0.0 });
                    s.set_shadow(Some(Shadow { color, ..sh }));
                }
            }
            "alpha" => {
                if let Some(a) = parse_ssa_alpha(arg) {
                    s.set_foreground(Some(with_alpha(s.foreground(), a)));
                    if let Some(mut o) = s.outline() {
                        o.color.a = a;
                        s.set_outline(Some(o));
                    }
                    if let Some(mut sh) = s.shadow() {
                        sh.color.a = a;
                        s.set_shadow(Some(sh));
                    }
                }
            }
            "1a" => {
                if let Some(a) = parse_ssa_alpha(arg) {
                    s.set_foreground(Some(with_alpha(s.foreground(), a)));
                }
            }
            "3a" => {
                if let (Some(a), Some(mut o)) = (parse_ssa_alpha(arg), s.outline()) {
                    o.color.a = a;
                    s.set_outline(Some(o));
                }
            }
            "4a" => {
                if let (Some(a), Some(mut sh)) = (parse_ssa_alpha(arg), s.shadow()) {
                    sh.color.a = a;
                    s.set_shadow(Some(sh));
                }
            }
            "bord" => {
                if let Ok(w) = arg.parse::<f32>() {
                    let color = s.outline().map_or(Color::BLACK, |o| o.color);
                    s.set_outline((w > 0.0).then_some(Outline { color, width: w }));
                }
            }
            "shad" => {
                if let Ok(v) = arg.parse::<f32>() {
                    let prev = s.shadow().unwrap_or(DEFAULT_SHADOW);
                    s.set_shadow((v > 0.0).then_some(Shadow { dx: v, dy: v, ..prev }));
                }
            }
            "xshad" | "yshad" => {
                if let Ok(v) = arg.parse::<f32>() {
                    let mut sh = s.shadow().unwrap_or(DEFAULT_SHADOW);
                    if name == "xshad" {
                        sh.dx = v;
                    } else {
                        sh.dy = v;
                    }
                    s.set_shadow(Some(sh));
                }
            }
            "an" | "a" => {
                if !self.an_set
                    && let Ok(v) = arg.parse::<i32>()
                {
                    let numpad = if name == "an" { Some(v) } else { legacy_to_numpad(v) };
                    if let Some((anchor, align)) = numpad.and_then(numpad_anchor) {
                        self.b.layout.set_anchor(Some(anchor));
                        self.b.layout.set_align(Some(align));
                        self.an_set = true;
                    }
                }
            }
            "pos" | "move" => {
                if !self.pos_set
                    && let Some((x, y)) = parse_xy(arg)
                {
                    let (rx, ry) = self.res;
                    self.b.layout.set_origin(Some((x / rx * 100.0, y / ry * 100.0)));
                    self.pos_set = true;
                }
            }
            "r" => {
                let named = (!arg.is_empty()).then(|| self.styles.lookup(arg)).flatten();
                self.cur_style = match named {
                    Some(style) => style_span(style, self.res.1, self.b.names()),
                    None => StyleRec::default(),
                };
            }
            "k" | "K" | "kf" | "ko" => {
                if let Ok(cs) = arg.parse::<f32>() {
                    // The following syllable highlights when the clock reaches the sum of every
                    // earlier duration.
                    self.reveal_ns = Some(self.karaoke_ns);
                    let ns = (cs.max(0.0) * 10_000_000.0) as u64;
                    self.karaoke_ns = self.karaoke_ns.saturating_add(ns);
                }
            }
            "p" => {
                self.drawing = arg.parse::<f32>().map(|v| v > 0.0).unwrap_or(false);
            }
            _ => {}
        }
    }
}

const DEFAULT_SHADOW: Shadow =
    Shadow { color: Color::rgba(0, 0, 0, 128), dx: 0.0, dy: 0.0, blur: 0.0 };

fn current_scale(s: &StyleRec) -> (f32, f32) {
    s.scale().unwrap_or((1.0, 1.0))
}

/// `color` with alpha `a`. Alpha on an unknown inherited fill assumes the customary white.
fn with_alpha(color: Option<Color>, a: u8) -> Color {
    match color {
        Some(c) => Color { a, ..c },
        None => Color::rgba(0xff, 0xff, 0xff, a),
    }
}

fn flag(arg: &str) -> Option<bool> {
    match arg {
        "" => None,
        "0" => Some(false),
        _ => Some(true),
    }
}

fn parse_xy(arg: &str) -> Option<(f32, f32)> {
    let inner = arg.trim().strip_prefix('(')?;
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    let mut it = inner.split(',');
    let x = it.next()?.trim().parse().ok()?;
    let y = it.next()?.trim().parse().ok()?;
    Some((x, y))
}

/// The known override tag names, longest first so `\shad` is never read as `\s` + junk.
const TAG_NAMES: &[&str] = &[
    "alpha", "iclip", "xshad", "yshad", "xbord", "ybord", "shad", "fscx", "fscy", "bord", "blur",
    "clip", "fade", "move", "fad", "fsp", "org", "pbo", "pos", "frx", "fry", "frz", "1c", "2c",
    "3c", "4c", "1a", "2a", "3a", "4a", "an", "be", "fe", "fn", "fr", "fs", "kf", "ko", "a", "b",
    "c", "i", "k", "K", "p", "q", "r", "s", "t", "u",
];

/// Read one `\tag<arg>` past the backslash at `i`. A parenthesised argument runs to its matching
/// `)`, so tags inside `\t(...)` do not leak.
fn read_tag(block: &str, i: usize) -> (&str, &str, usize) {
    let rest = &block[i..];
    let name = TAG_NAMES.iter().find(|n| rest.starts_with(**n)).copied().unwrap_or("");
    if name.is_empty() {
        return ("", "", i);
    }
    let mut j = i + name.len();
    let bytes = block.as_bytes();
    let mut depth = 0i32;
    while j < bytes.len() {
        match bytes[j] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'\\' if depth <= 0 => break,
            _ => {}
        }
        j += 1;
    }
    (name, &block[i + name.len()..j], j)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::testutil::{Owned, build, spans_of};
    use crate::ir::{CueIr, SpanRec};

    const S: u64 = 1_000_000_000;

    fn registry(style_line: &str) -> SsaStyles {
        SsaStyles::parse(&format!(
            "[Script Info]\nPlayResX: 384\nPlayResY: 288\n\n\
             [V4+ Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
             Style: {style_line}\n"
        ))
    }

    fn default_registry() -> SsaStyles {
        registry(
            "Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,1,2,10,10,10,1",
        )
    }

    fn ir_for(text: &str) -> Owned {
        let d =
            SsaEvent { raw_text: text, style: "Default", margin_l: 0, margin_r: 0, margin_v: 0 };
        build(|b| dialogue_into(&d, &default_registry(), 0, b))
    }

    fn with_margins(text: &str, reg: &SsaStyles, margin_v: u32) -> Owned {
        let d = SsaEvent { raw_text: text, style: "Default", margin_l: 0, margin_r: 0, margin_v };
        build(|b| dialogue_into(&d, reg, 0, b))
    }

    fn st<'a>(v: &CueIr<'a>, s: &SpanRec) -> &'a StyleRec {
        v.span_style(s)
    }

    #[test]
    fn ssa_colors_parse() {
        assert_eq!(parse_ssa_color("&HFF0000&"), Some(Color::rgb(0, 0, 255)));
        assert_eq!(parse_ssa_color("&H0000FF"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(parse_ssa_color("&H00FFFFFF"), Some(Color::rgb(255, 255, 255)));
        assert_eq!(parse_ssa_color("&H800000FF"), Some(Color::rgba(255, 0, 0, 127)));
        assert_eq!(parse_ssa_color("65535"), Some(Color::rgb(255, 255, 0)));
        assert_eq!(parse_ssa_color("-2147483640"), Some(Color::rgba(8, 0, 0, 255 - 0x80)));
        assert_eq!(parse_ssa_color("nope"), None);
    }

    #[test]
    fn v4plus_style_fields_parse() {
        let reg = registry(
            "Default,DejaVu Sans,24,&H0000FFFF,&H000000FF,&H00FF0000,&H80000000,-1,-1,-1,-1,120,80,1.5,0,1,3,2,8,12,14,16,1",
        );
        let style = reg.lookup("Default").unwrap();
        assert_eq!(style.font_family.as_deref(), Some("DejaVu Sans"));
        assert_eq!(style.font_size, Some(24.0));
        assert_eq!(style.primary, Some(Color::rgb(255, 255, 0)));
        assert_eq!(style.outline_color, Some(Color::rgb(0, 0, 255)));
        assert_eq!(style.back_color, Some(Color::rgba(0, 0, 0, 127)));
        assert_eq!(style.font_weight, Some(700));
        assert_eq!(style.italic, Some(true));
        assert_eq!(style.underline, Some(true));
        assert_eq!(style.strikeout, Some(true));
        assert_eq!(style.scale_x, Some(120.0));
        assert_eq!(style.scale_y, Some(80.0));
        assert_eq!(style.spacing, Some(1.5));
        assert_eq!(style.border_style, Some(1));
        assert_eq!(style.outline_w, Some(3.0));
        assert_eq!(style.shadow_d, Some(2.0));
        assert_eq!(style.alignment, Some(8));
        assert_eq!(style.margin_l, Some(12.0));
    }

    #[test]
    fn v4_style_uses_legacy_alignment_and_tertiary() {
        let reg = SsaStyles::parse(
            "[V4 Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\n\
             Style: Default,Arial,18,16777215,65535,255,0,-1,0,1,2,1,6,10,10,10,0,1\n",
        );
        let style = reg.lookup("Default").unwrap();
        assert_eq!(style.alignment, Some(8));
        assert_eq!(style.outline_color, Some(Color::rgb(255, 0, 0)));
        assert_eq!(style.font_weight, Some(700));
    }

    #[test]
    fn style_lookup_is_lenient() {
        let reg = default_registry();
        assert!(reg.lookup("Default").is_some());
        assert!(reg.lookup("default").is_some());
        assert!(reg.lookup("*Default").is_some());
        assert!(reg.lookup("Missing").is_none());
    }

    #[test]
    fn play_res_defaults_and_derivation() {
        assert_eq!(SsaStyles::parse("").play_res(), (384.0, 288.0));
        assert_eq!(SsaStyles::parse("[Script Info]\nPlayResY: 480\n").play_res(), (640.0, 480.0));
        assert_eq!(
            SsaStyles::parse("[Script Info]\nPlayResX: 1280\nPlayResY: 720\n").play_res(),
            (1280.0, 720.0)
        );
    }

    #[test]
    fn style_becomes_base_and_layout() {
        let o = ir_for("Hi");
        let v = o.view();
        let base = v.base_style();
        assert_eq!(base.font_size(), Some(FontSize::FrameHeightPercent(20.0 / 288.0 * 100.0)));
        assert_eq!(v.style_family(base), Some("Arial"));
        assert_eq!(base.foreground(), Some(Color::WHITE));
        assert_eq!(base.outline(), Some(Outline { color: Color::BLACK, width: 2.0 }));
        assert_eq!(base.shadow().map(|s| (s.dx, s.dy)), Some((1.0, 1.0)));
        assert_eq!(v.layout().anchor(), Some(Anchor::BottomCenter));
        assert_eq!(v.layout().align(), Some(TextAlign::Center));
        let m = v.layout().margins().unwrap();
        assert!((m.left - 10.0 / 384.0 * 100.0).abs() < 1e-4);
        assert!((m.vertical - 10.0 / 288.0 * 100.0).abs() < 1e-4);
        assert_eq!(v.plain_text(), "Hi");
    }

    #[test]
    fn dialogue_margins_override_style() {
        let o = with_margins("x", &default_registry(), 72);
        assert_eq!(o.view().layout().margins().unwrap().vertical, 25.0); // 72/288
    }

    #[test]
    fn border_style_3_becomes_cue_background() {
        let reg = registry(
            "Default,Arial,20,&H00FFFFFF,&H000000FF,&H00404040,&H00000000,0,0,0,0,100,100,0,0,3,2,0,2,0,0,0,1",
        );
        let o = with_margins("x", &reg, 0);
        let v = o.view();
        assert_eq!(v.layout().background(), Some(Color::rgb(0x40, 0x40, 0x40)));
        assert_eq!(v.base_style().outline(), None, "no stroke in box mode");
    }

    #[test]
    fn unknown_style_leaves_renderer_defaults() {
        let d = SsaEvent { raw_text: "x", style: "Nope", margin_l: 0, margin_r: 0, margin_v: 0 };
        let o = build(|b| dialogue_into(&d, &default_registry(), 0, b));
        assert!(o.view().base_style().is_plain());
        assert_eq!(o.plain_text(), "x");
    }

    #[test]
    fn basic_flags_and_reset() {
        let o = ir_for("{\\i1}it{\\i0}no{\\b1\\u1}bu{\\r}plain");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 4);
        assert_eq!(st(&v, spans[0]).font_style(), Some(FontStyle::Italic));
        assert_eq!(st(&v, spans[1]).font_style(), Some(FontStyle::Normal));
        assert_eq!(st(&v, spans[2]).font_weight(), Some(700));
        assert_eq!(st(&v, spans[2]).underline(), Some(true));
        assert!(st(&v, spans[3]).is_plain());
        assert_eq!(v.plain_text(), "itnobuplain");
    }

    #[test]
    fn empty_flag_resets_to_base() {
        let o = ir_for("{\\i1}a{\\i}b");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(st(&v, spans[0]).font_style(), Some(FontStyle::Italic));
        assert_eq!(st(&v, spans[1]).font_style(), None);
    }

    #[test]
    fn font_and_size_and_spacing() {
        let o = ir_for("{\\fnComic Sans\\fs36\\fsp2}x");
        let v = o.view();
        let s = st(&v, spans_of(&v)[0]);
        assert_eq!(v.style_family(s), Some("Comic Sans"));
        assert_eq!(s.font_size(), Some(FontSize::FrameHeightPercent(12.5))); // 36/288
        assert_eq!(s.letter_spacing(), Some(2.0));
    }

    #[test]
    fn colors_and_alpha() {
        let o = ir_for("{\\c&H0000FF&}red{\\1a&H80&}faded");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(st(&v, spans[0]).foreground(), Some(Color::rgb(255, 0, 0)));
        assert_eq!(st(&v, spans[1]).foreground(), Some(Color::rgba(255, 0, 0, 127)));
    }

    #[test]
    fn outline_and_shadow_overrides() {
        let o = ir_for("{\\bord4\\3c&HFF0000&\\shad3\\4c&H00FF00&}x{\\bord0}y");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(
            st(&v, spans[0]).outline(),
            Some(Outline { color: Color::rgb(0, 0, 255), width: 4.0 })
        );
        let sh = st(&v, spans[0]).shadow().unwrap();
        assert_eq!((sh.dx, sh.dy), (3.0, 3.0));
        assert_eq!(sh.color, Color::rgb(0, 255, 0));
        assert_eq!(st(&v, spans[1]).outline(), None);
    }

    #[test]
    fn scale_overrides() {
        let o = ir_for("{\\fscx200}wide{\\fscy50}also-flat");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(st(&v, spans[0]).scale(), Some((2.0, 1.0)));
        assert_eq!(st(&v, spans[1]).scale(), Some((2.0, 0.5)));
    }

    #[test]
    fn alignment_and_position() {
        let o = ir_for("{\\an8\\pos(192,36)}top");
        let l = *o.view().layout();
        assert_eq!(l.anchor(), Some(Anchor::TopCenter));
        assert_eq!(l.align(), Some(TextAlign::Center));
        assert_eq!(l.origin(), Some((50.0, 12.5)));
    }

    #[test]
    fn first_alignment_and_position_win() {
        let o = ir_for("{\\an8}x{\\an1}y{\\pos(0,0)}z");
        let l = *o.view().layout();
        assert_eq!(l.anchor(), Some(Anchor::TopCenter));
        assert_eq!(l.origin(), Some((0.0, 0.0)));
        let o = ir_for("{\\pos(192,144)}x{\\pos(0,0)}y");
        assert_eq!(o.view().layout().origin(), Some((50.0, 50.0)));
    }

    #[test]
    fn legacy_alignment_tag() {
        assert_eq!(ir_for("{\\a6}top").view().layout().anchor(), Some(Anchor::TopCenter));
    }

    #[test]
    fn move_takes_start_point() {
        assert_eq!(ir_for("{\\move(96,72,192,144)}x").view().layout().origin(), Some((25.0, 25.0)));
    }

    #[test]
    fn reset_to_named_style() {
        let mut reg = default_registry();
        reg.feed_line("[V4+ Styles]");
        reg.feed_line("Format: Name, PrimaryColour, Italic");
        reg.feed_line("Style: Alt,&H0000FF&,-1");
        let o = with_margins("a{\\rAlt}b", &reg, 0);
        let v = o.view();
        let spans = spans_of(&v);
        assert!(st(&v, spans[0]).is_plain());
        assert_eq!(st(&v, spans[1]).foreground(), Some(Color::rgb(255, 0, 0)));
        assert_eq!(st(&v, spans[1]).font_style(), Some(FontStyle::Italic));
    }

    #[test]
    fn line_breaks_and_hard_space() {
        let o = ir_for("one\\Ntwo\\nthree\\hx");
        assert_eq!(o.view().line_count(), 3);
        assert_eq!(o.plain_text(), "one\ntwo\nthree\u{a0}x");
    }

    #[test]
    fn a_trailing_backslash_terminates_instead_of_spinning() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(ir_for("Hello\\").plain_text());
        });

        let text = finished
            .recv_timeout(Duration::from_secs(2))
            .expect("the scanner must terminate on a trailing backslash");
        assert_eq!(text, "Hello\\", "a lone trailing backslash stays literal");
    }

    #[test]
    fn a_backslash_before_a_multibyte_character_does_not_panic() {
        for text in ["a\\\u{e9}b", "a\\\u{4e00}b", "\\\u{439}"] {
            let rendered = ir_for(text).plain_text();
            assert_eq!(rendered, text, "an unknown escape stays literal, character and all");
        }
    }

    #[test]
    fn no_arrangement_of_the_delimiters_spins_the_scanner() {
        use std::sync::mpsc;
        use std::time::Duration;

        let shapes = [
            "\\",
            "\\\\",
            "\\\\\\",
            "{",
            "{{",
            "}",
            "{}",
            "\\{",
            "{\\",
            "{\\}",
            "\\{}",
            "a\\",
            "a{",
            "{a",
            "\\a\\",
            "{a\\",
            "\\{a",
            "{\\a",
            "\\}a",
            "{a}\\",
            "{\\an8}x\\",
            "{\\b1}{\\i1}\\",
            "\u{e9}\\",
            "\\\u{e9}",
            "{\u{4e00}\\",
        ];
        for shape in shapes {
            let (done, finished) = mpsc::channel();
            let owned = shape.to_owned();
            std::thread::spawn(move || {
                let _ = done.send(ir_for(&owned).plain_text());
            });
            finished
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("the scanner did not terminate on {shape:?}"));
        }
    }

    #[test]
    fn other_backslash_escapes_stay_literal() {
        assert_eq!(ir_for("a\\db").plain_text(), "a\\db");
    }

    #[test]
    fn drawing_commands_are_dropped() {
        assert_eq!(ir_for("{\\p1}m 0 0 l 100 0 100 100{\\p0}after").plain_text(), "after");
    }

    #[test]
    fn unmatched_brace_keeps_remainder() {
        assert_eq!(ir_for("ok{\\i1 oops").plain_text(), "ok{\\i1 oops");
    }

    #[test]
    fn karaoke_accumulates_reveal_times() {
        let d = SsaEvent {
            raw_text: "{\\k50}la{\\k100}lala{\\kf25}end",
            style: "Default",
            margin_l: 0,
            margin_r: 0,
            margin_v: 0,
        };
        let o = build(|b| dialogue_into(&d, &default_registry(), 10 * S, b));
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].reveal_ns(), Some(10 * S));
        assert_eq!(spans[1].reveal_ns(), Some(10 * S + 500_000_000));
        assert_eq!(spans[2].reveal_ns(), Some(10 * S + 1_500_000_000));
        assert_eq!(v.plain_text(), "lalalaend");
    }

    #[test]
    fn transform_args_do_not_leak() {
        let o = ir_for("{\\t(0,500,\\c&H0000FF&)}steady");
        let v = o.view();
        assert_eq!(st(&v, spans_of(&v)[0]).foreground(), None);
        let o = ir_for("{\\t(0,500,\\c&H0000FF&)\\b1}bold");
        let v = o.view();
        assert_eq!(st(&v, spans_of(&v)[0]).font_weight(), Some(700));
        assert_eq!(st(&v, spans_of(&v)[0]).foreground(), None);
    }

    #[test]
    fn shad_is_not_read_as_s() {
        let o = ir_for("{\\shad2}x");
        let v = o.view();
        let s = st(&v, spans_of(&v)[0]);
        assert_eq!(s.strikethrough(), None);
        assert_eq!(s.shadow().map(|sh| sh.dx), Some(2.0));
    }

    #[test]
    fn adjacent_identical_spans_merge() {
        let o = ir_for("a{\\i1}{\\i1}b{comment}c");
        let v = o.view();
        let spans = spans_of(&v);
        assert_eq!(spans.len(), 2);
        assert_eq!(v.span_text(spans[0]), "a");
        assert_eq!(v.span_text(spans[1]), "bc");
    }

    #[test]
    fn styles_past_the_cap_are_ignored_and_lookups_stay_cheap() {
        let mut header = String::from("[V4+ Styles]\nFormat: Name, Fontname\n");
        for i in 0..MAX_STYLES + 10 {
            header.push_str(&format!("Style: s{i},Arial\n"));
        }
        let reg = SsaStyles::parse(&header);
        assert_eq!(reg.styles().count(), MAX_STYLES);
        assert!(reg.lookup("s0").is_some());
        assert!(reg.lookup("S4095").is_some());
        assert!(reg.lookup("s4096").is_none());
        let text = format!("{{{}}}x", "\\rmissing\\rS4095".repeat(100_000));
        let d = SsaEvent { raw_text: &text, style: "Default", margin_l: 0, margin_r: 0, margin_v: 0 };
        let o = build(|b| dialogue_into(&d, &reg, 0, b));
        assert_eq!(o.plain_text(), "x");
    }

    #[test]
    fn lookup_prefers_exact_case_over_an_earlier_folded_match() {
        let reg = SsaStyles::parse(
            "[V4+ Styles]\nFormat: Name, Fontname\nStyle: default,A\nStyle: Default,B\nStyle: *Default,C\n",
        );
        let family = |name: &str| reg.lookup(name).and_then(|s| s.font_family.clone());
        assert_eq!(family("Default").as_deref(), Some("B"));
        assert_eq!(family("DEFAULT").as_deref(), Some("A"));
        assert_eq!(family("**default").as_deref(), Some("A"));
        assert_eq!(family("other"), None);
    }
}
