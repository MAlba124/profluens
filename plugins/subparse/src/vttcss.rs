// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! WebVTT `STYLE` block support, a tolerant CSS parser and the `::cue` cascade onto [`crate::ir`].

// Cold path, once per document or cue.
#![allow(clippy::disallowed_methods)]

use crate::ir::{
    Color, FontSize, FontStyle, Names, Outline, RubyPosition, Shadow, StyleRec, size_step,
};

/// Rules a sheet keeps, every cue root and tag is matched against each one.
pub const MAX_RULES: usize = 512;

/// `:not(` nests this deep in a prelude before the selector is refused.
const MAX_NOT_DEPTH: usize = 8;

/// A parsed set of `::cue` rules in cascade order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stylesheet {
    rules: Vec<Rule>,
    /// The family and language names the rules refer to.
    names: Names,
    next_order: u32,
}

/// One selector with its parsed declarations.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// `None` is the argless `::cue` selector (matches the cue root only).
    pub compound: Option<Compound>,
    /// A delta, present fields overwrite. Its names live in the sheet's [`Names`].
    pub style: StyleRec,
    /// A `ruby-position` declaration, if present.
    pub ruby_position: Option<RubyPosition>,
    specificity: u32,
    order: u32,
}

/// A compound selector inside `::cue(...)`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Compound {
    /// Node type, `None` matches any node (`*` or omitted).
    pub element: Option<String>,
    /// `#id`, matched against the cue identifier (the root node carries it).
    pub id: Option<String>,
    /// `.class` parts, all must be present on the node.
    pub classes: Vec<String>,
    /// `[voice="..."]`, only `<v>` nodes carry a voice.
    pub voice: Option<String>,
    /// `:lang(...)` ranges, any one matching the node's language suffices.
    pub langs: Vec<String>,
    /// `:past` / `:future`, never matches a static IR.
    pub time_dependent: bool,
    /// A well-formed feature we cannot match (an attribute other than `voice`, an unknown
    /// pseudo-class).
    pub unsupported: bool,
}

/// The cue root (`element: None`, carrying the cue's `id`) or one inline tag with its classes,
/// voice and inherited language.
#[derive(Debug, Clone, Copy, Default)]
pub struct Node<'a> {
    pub element: Option<&'a str>,
    /// The node's classes as the tag wrote them, dot separated (`.a.b` or `a.b`).
    pub classes: &'a str,
    pub voice: Option<&'a str>,
    pub lang: Option<&'a str>,
    pub id: Option<&'a str>,
}

impl Stylesheet {
    /// Parse a stylesheet.
    pub fn parse(css: &str) -> Stylesheet {
        let mut sheet = Stylesheet::default();
        sheet.push_css(css);
        sheet
    }

    /// Append another `STYLE` block, keeping cascade order across blocks. Rules past
    /// [`MAX_RULES`] are dropped.
    pub fn push_css(&mut self, css: &str) {
        let before = self.rules.len();
        parse_rules(css, &mut self.rules, &mut self.names, &mut self.next_order);
        if self.rules.len() > before {
            self.rules.sort_by_key(|r| r.specificity);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rules in cascade order.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The names the rules' styles refer to.
    pub fn names(&self) -> &Names {
        &self.names
    }

    /// Apply every rule matching `node` in cascade order, interning names into `names`.
    pub fn apply(
        &self,
        node: &Node,
        names: &mut Names,
        style: &mut StyleRec,
        ruby_position: &mut RubyPosition,
    ) {
        for rule in &self.rules {
            if rule.matches(node) {
                style.merge_from(&rule.style, &self.names, names);
                if let Some(rp) = rule.ruby_position {
                    *ruby_position = rp;
                }
            }
        }
    }
}

impl Rule {
    fn matches(&self, node: &Node) -> bool {
        match &self.compound {
            // Argless `::cue` styles the root only, children inherit via the base style.
            None => node.element.is_none(),
            Some(c) => c.matches(node),
        }
    }
}

impl Compound {
    fn matches(&self, node: &Node) -> bool {
        if self.time_dependent || self.unsupported {
            return false;
        }
        if let Some(el) = self.element.as_deref()
            && node.element != Some(el)
        {
            return false;
        }
        if let Some(id) = self.id.as_deref()
            && node.id != Some(id)
        {
            return false;
        }
        if !self.classes.iter().all(|c| node.classes.split('.').any(|n| n == c)) {
            return false;
        }
        if let Some(v) = self.voice.as_deref()
            && node.voice != Some(v)
        {
            return false;
        }
        if !self.langs.is_empty() {
            let Some(lang) = node.lang else { return false };
            if !self.langs.iter().any(|r| lang_matches(r, lang)) {
                return false;
            }
        }
        true
    }
}

fn lang_matches(range: &str, lang: &str) -> bool {
    if range.is_empty() || lang.len() < range.len() {
        return false;
    }
    let (head, tail) = lang.split_at(range.len());
    head.eq_ignore_ascii_case(range) && (tail.is_empty() || tail.starts_with('-'))
}

fn parse_rules(css: &str, rules: &mut Vec<Rule>, names: &mut Names, next_order: &mut u32) {
    let css = strip_comments(css);
    let bytes = css.as_bytes();
    let mut pos = 0usize;
    // The cached next '{', still valid past a stray '}'. Re-searching for each was quadratic.
    let mut next_open: Option<Option<usize>> = None;

    while pos < bytes.len() && rules.len() < MAX_RULES {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        if bytes[pos] == b'@' {
            pos = skip_at_rule(&css, pos);
            next_open = None;
            continue;
        }
        // A stray top-level '}' before the '{' is a parse error, CSS discards up to and including
        // it.
        let open_at = match next_open {
            Some(hit) if hit.is_none_or(|o| o >= pos) => hit,
            _ => find_unquoted(&css, pos, b'{'),
        };
        next_open = Some(open_at);
        let open = match (open_at, find_unquoted(&css, pos, b'}')) {
            (Some(o), Some(c)) if c < o => {
                pos = c + 1;
                continue;
            }
            (None, Some(c)) => {
                pos = c + 1;
                continue;
            }
            (Some(o), _) => o,
            (None, None) => break,
        };
        let selectors = &css[pos..open];
        let close = matching_brace(&css, open);
        let block = &css[open + 1..close];
        pos = (close + 1).min(css.len());
        next_open = None;

        let (style, ruby_position) = parse_declarations(block, names);
        if style == StyleRec::default() && ruby_position.is_none() {
            continue; // nothing we can apply
        }
        let order = *next_order;
        *next_order += 1;
        for sel in split_top_level(selectors, ',') {
            if rules.len() >= MAX_RULES {
                break;
            }
            if let Some((compound, specificity)) = parse_selector(sel) {
                rules.push(Rule { compound, style, ruby_position, specificity, order });
            }
        }
    }
}

fn strip_comments(css: &str) -> String {
    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len());
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                } else if b == b'\\'
                    && let Some(c) = css[i..].chars().nth(1)
                {
                    out.push('\\');
                    out.push(c);
                    i += 1 + c.len_utf8();
                    continue;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    match css[i + 2..].find("*/") {
                        Some(rel) => {
                            i += 2 + rel + 2;
                            out.push(' ');
                            continue;
                        }
                        None => break,
                    }
                }
            }
        }
        let ch = css[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn skip_at_rule(css: &str, pos: usize) -> usize {
    let bytes = css.as_bytes();
    let mut i = pos;
    while i < bytes.len() {
        match bytes[i] {
            b';' => return i + 1,
            b'{' => return matching_brace(css, i) + 1,
            _ => i += 1,
        }
    }
    css.len()
}

fn find_unquoted(css: &str, from: usize, target: u8) -> Option<usize> {
    let bytes = css.as_bytes();
    let mut i = from;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                } else if b == b'\\' {
                    i += 1;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == target {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

fn matching_brace(css: &str, open: usize) -> usize {
    let bytes = css.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                } else if b == b'\\' {
                    i += 1;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    css.len()
}

fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                _ if b == sep as u8 && depth <= 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(&s[start..]);
    out
}

fn parse_selector(sel: &str) -> Option<(Option<Compound>, u32)> {
    let sel = sel.trim();
    let dc = sel.find("::")?;
    if !subject_matches_featureless(&sel[..dc]) {
        return None;
    }
    let rest = &sel[dc + 2..];
    let after = rest.get(..3).filter(|p| p.eq_ignore_ascii_case("cue"))?;
    let _ = after;
    let rest = rest[3..].trim();
    if rest.is_empty() {
        return Some((None, 0));
    }
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    let compound = parse_compound(inner)?;
    let spec = compound.specificity();
    Some((Some(compound), spec))
}

impl Compound {
    fn specificity(&self) -> u32 {
        let mut s = 0;
        if self.id.is_some() {
            s += 100;
        }
        s += 10 * self.classes.len() as u32;
        s += 10 * self.langs.len() as u32;
        if self.voice.is_some() {
            s += 10;
        }
        if self.time_dependent {
            s += 10;
        }
        if self.element.is_some() {
            s += 1;
        }
        s
    }
}

/// Whether the prelude before `::cue` matches the spec's featureless originating element.
fn subject_matches_featureless(subject: &str) -> bool {
    subject_matches_featureless_at(subject, 0)
}

/// `depth` counts the `:not(` levels above, past [`MAX_NOT_DEPTH`] the prelude is refused
/// rather than recursed into.
fn subject_matches_featureless_at(subject: &str, depth: usize) -> bool {
    // Remaining whitespace is a descendant combinator, and the featureless element has no
    // ancestors.
    if subject.is_empty() {
        return true;
    }
    if subject.contains(char::is_whitespace) || subject.contains(['>', '+', '~']) {
        return false;
    }
    featureless_compound(subject, depth)
}

fn featureless_compound(s: &str, depth: usize) -> bool {
    if depth > MAX_NOT_DEPTH {
        return false;
    }
    let mut rest = s;
    for p in ["*|*", "|*", "*"] {
        if let Some(r) = rest.strip_prefix(p) {
            rest = r;
            break;
        }
    }
    while !rest.is_empty() {
        if rest.len() < 5 || !rest.is_char_boundary(5) || !rest[..5].eq_ignore_ascii_case(":not(") {
            return false;
        }
        let bytes = rest.as_bytes();
        let mut parens = 1usize;
        let mut i = 5usize;
        while i < bytes.len() && parens > 0 {
            match bytes[i] {
                b'(' => parens += 1,
                b')' => parens -= 1,
                _ => {}
            }
            i += 1;
        }
        if parens != 0 {
            return false;
        }
        if subject_matches_featureless_at(rest[5..i - 1].trim(), depth + 1) {
            return false;
        }
        rest = &rest[i..];
    }
    true
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b >= 0x80
}

fn read_ident(s: &str, i: usize) -> (&str, usize) {
    let bytes = s.as_bytes();
    let mut j = i;
    while j < bytes.len() && is_ident_byte(bytes[j]) {
        j += 1;
    }
    (&s[i..j], j)
}

fn parse_compound(s: &str) -> Option<Compound> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut c = Compound::default();
    let mut i = 0usize;

    if bytes[0] == b'*' {
        i = 1;
    } else if is_ident_byte(bytes[0]) {
        let (name, j) = read_ident(s, 0);
        c.element = Some(name.to_ascii_lowercase());
        i = j;
    }

    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                let (name, j) = read_ident(s, i + 1);
                if name.is_empty() {
                    return None;
                }
                c.classes.push(name.to_owned());
                i = j;
            }
            b'#' => {
                let (name, j) = read_ident(s, i + 1);
                if name.is_empty() {
                    return None;
                }
                c.id = Some(name.to_owned());
                i = j;
            }
            b'[' => {
                let close = s[i..].find(']')? + i;
                parse_attr(&s[i + 1..close], &mut c);
                i = close + 1;
            }
            b':' => {
                // A second ':' would be a pseudo-element, invalid here.
                if bytes.get(i + 1) == Some(&b':') {
                    return None;
                }
                let (name, j) = read_ident(s, i + 1);
                i = j;
                match name.to_ascii_lowercase().as_str() {
                    "lang" => {
                        let rest = &s[i..];
                        let inner_end = rest.find(')')?;
                        let args = rest.strip_prefix('(')?[..inner_end - 1].trim();
                        for lang in args.split(',') {
                            let lang = lang.trim().trim_matches(['"', '\'']);
                            if !lang.is_empty() {
                                c.langs.push(lang.to_owned());
                            }
                        }
                        if c.langs.is_empty() {
                            return None;
                        }
                        i += inner_end + 1;
                    }
                    "past" | "future" => c.time_dependent = true,
                    "" => return None,
                    _ => c.unsupported = true,
                }
            }
            b if b.is_ascii_whitespace() => {
                // Combinators are invalid inside ::cue().
                return None;
            }
            _ => return None,
        }
    }
    Some(c)
}

/// Only `voice="..."` matches, other attributes mark the compound unsupported.
fn parse_attr(body: &str, c: &mut Compound) {
    let body = body.trim();
    match body.split_once('=') {
        Some((name, value)) if name.trim().eq_ignore_ascii_case("voice") => {
            let value = value.trim().trim_matches(['"', '\'']);
            c.voice = Some(value.to_owned());
        }
        _ => c.unsupported = true,
    }
}

fn parse_declarations(block: &str, names: &mut Names) -> (StyleRec, Option<RubyPosition>) {
    let mut style = StyleRec::default();
    let mut ruby = None;
    for decl in split_top_level(block, ';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let mut value = value.trim();
        if let Some(v) = strip_important(value) {
            value = v;
        }
        if value.is_empty() {
            continue;
        }
        apply_declaration(&name, value, &mut style, &mut ruby, names);
    }
    (style, ruby)
}

fn strip_important(value: &str) -> Option<&str> {
    let v = value.trim_end();
    let bang = v.rfind('!')?;
    v[bang + 1..].trim().eq_ignore_ascii_case("important").then(|| v[..bang].trim_end())
}

fn apply_declaration(
    name: &str,
    value: &str,
    style: &mut StyleRec,
    ruby: &mut Option<RubyPosition>,
    names: &mut Names,
) {
    match name {
        "color" => {
            if let Some(c) = parse_css_color(value) {
                style.set_foreground(Some(c));
            }
        }
        "background-color" => {
            if let Some(c) = parse_css_color(value) {
                style.set_background(Some(c));
            }
        }
        "background" => {
            if value.eq_ignore_ascii_case("none") {
                style.set_background(Some(Color::TRANSPARENT));
            } else if let Some(c) = split_tokens(value).iter().find_map(|t| parse_css_color(t)) {
                style.set_background(Some(c));
            }
        }
        "outline" => {
            let mut color = None;
            let mut width = None;
            for tok in split_tokens(value) {
                if let Some(c) = parse_css_color(tok) {
                    color = Some(c);
                } else if let Some(w) = parse_outline_width(tok) {
                    width = Some(w);
                }
                // Border styles are ignored, the IR outline is always solid.
            }
            if color.is_some() || width.is_some() {
                style.set_outline(Some(Outline {
                    color: color.unwrap_or(Color::BLACK),
                    width: width.unwrap_or(MEDIUM_OUTLINE_PT),
                }));
            }
        }
        "outline-color" => {
            if let Some(c) = parse_css_color(value) {
                let width = style.outline().map_or(MEDIUM_OUTLINE_PT, |o| o.width);
                style.set_outline(Some(Outline { color: c, width }));
            }
        }
        "outline-width" => {
            if let Some(w) = parse_outline_width(value) {
                let color = style.outline().map_or(Color::BLACK, |o| o.color);
                style.set_outline(Some(Outline { color, width: w }));
            }
        }
        "font-family" => {
            if let Some(fam) = clean_font_family(value) {
                style.set_family(Some(names.intern(&fam)));
            }
        }
        "font-size" => {
            if let Some(s) = parse_font_size(value) {
                style.set_font_size(Some(s));
            }
        }
        "font-style" => {
            style.set_font_style(match value.to_ascii_lowercase().as_str() {
                "normal" => Some(FontStyle::Normal),
                "italic" => Some(FontStyle::Italic),
                "oblique" => Some(FontStyle::Oblique),
                _ => style.font_style(),
            });
        }
        "font-weight" => {
            if let Some(w) = parse_font_weight(value) {
                style.set_font_weight(Some(w));
            }
        }
        "font" => parse_font_shorthand(value, style, names),
        "text-decoration" | "text-decoration-line" => {
            for tok in split_tokens(value) {
                match tok.to_ascii_lowercase().as_str() {
                    "none" => {
                        style.set_underline(Some(false));
                        style.set_strikethrough(Some(false));
                    }
                    "underline" => style.set_underline(Some(true)),
                    "line-through" => style.set_strikethrough(Some(true)),
                    _ => {}
                }
            }
        }
        "text-shadow" => {
            if value.eq_ignore_ascii_case("none") {
                return;
            }
            // The IR holds one shadow, take the first.
            if let Some(first) = split_top_level(value, ',').first()
                && let Some(shadow) = parse_shadow(first)
            {
                style.set_shadow(Some(shadow));
            }
        }
        "ruby-position" => {
            *ruby = match value.to_ascii_lowercase().as_str() {
                "over" => Some(RubyPosition::Over),
                "under" => Some(RubyPosition::Under),
                _ => *ruby,
            }
        }
        _ => {}
    }
}

const MEDIUM_OUTLINE_PT: f32 = 3.0 * 0.75;

fn split_tokens(value: &str) -> Vec<&str> {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        let mut depth = 0i32;
        while i < bytes.len() {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b if b.is_ascii_whitespace() && depth <= 0 => break,
                _ => {}
            }
            i += 1;
        }
        out.push(&value[start..i]);
    }
    out
}

fn parse_length_pt(tok: &str) -> Option<f32> {
    let t = tok.trim();
    if let Some(v) = t.strip_suffix("px") {
        v.trim().parse::<f32>().ok().map(|v| v * 0.75)
    } else if let Some(v) = t.strip_suffix("pt") {
        v.trim().parse::<f32>().ok()
    } else if t == "0" {
        Some(0.0)
    } else {
        None
    }
}

fn parse_outline_width(tok: &str) -> Option<f32> {
    match tok.trim().to_ascii_lowercase().as_str() {
        "thin" => Some(1.0 * 0.75),
        "medium" => Some(MEDIUM_OUTLINE_PT),
        "thick" => Some(5.0 * 0.75),
        t => parse_length_pt(t),
    }
}

fn parse_font_size(value: &str) -> Option<FontSize> {
    let v = value.trim().to_ascii_lowercase();
    let scale = size_step;
    Some(match v.as_str() {
        "xx-small" => scale(-3),
        "x-small" => scale(-2),
        "small" | "smaller" => scale(-1),
        "medium" => scale(0),
        "large" | "larger" => scale(1),
        "x-large" => scale(2),
        "xx-large" => scale(3),
        _ => {
            if let Some(pct) = v.strip_suffix('%') {
                FontSize::Scale(pct.trim().parse::<f32>().ok()? / 100.0)
            } else if let Some(em) = v.strip_suffix("em") {
                FontSize::Scale(em.trim().parse::<f32>().ok()?)
            } else {
                FontSize::Points(parse_length_pt(&v)?)
            }
        }
    })
}

fn parse_font_weight(value: &str) -> Option<u16> {
    match value.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(400),
        "bold" => Some(700),
        // `bolder`/`lighter` need the inherited weight, a delta cannot express that.
        v => v.parse::<u16>().ok().filter(|w| (1..=1000).contains(w)),
    }
}

fn clean_font_family(value: &str) -> Option<String> {
    let families: Vec<&str> = split_top_level(value, ',')
        .into_iter()
        .map(|f| f.trim().trim_matches(['"', '\'']).trim())
        .filter(|f| !f.is_empty())
        .collect();
    if families.is_empty() { None } else { Some(families.join(", ")) }
}

fn parse_font_shorthand(value: &str, style: &mut StyleRec, names: &mut Names) {
    let toks = split_tokens(value);
    let mut size_at = None;
    for (i, tok) in toks.iter().enumerate() {
        let head = tok.split('/').next().unwrap_or(tok);
        if parse_font_size(head).is_some() {
            size_at = Some((i, head));
            break;
        }
    }
    let Some((at, size_tok)) = size_at else {
        return;
    };
    let mut new = StyleRec::default();
    for tok in &toks[..at] {
        match tok.to_ascii_lowercase().as_str() {
            "italic" => new.set_font_style(Some(FontStyle::Italic)),
            "oblique" => new.set_font_style(Some(FontStyle::Oblique)),
            "normal" | "small-caps" => {}
            w => {
                if let Some(w) = parse_font_weight(w) {
                    new.set_font_weight(Some(w));
                }
            }
        }
    }
    new.set_font_size(parse_font_size(size_tok));
    if at + 1 < toks.len() {
        let family_start = toks[at + 1].as_ptr() as usize - value.as_ptr() as usize;
        new.set_family(clean_font_family(&value[family_start..]).map(|f| names.intern(&f)));
    }
    style.merge(&new);
}

fn parse_shadow(item: &str) -> Option<Shadow> {
    let mut lengths = Vec::new();
    let mut color = None;
    for tok in split_tokens(item) {
        if let Some(l) = parse_length_pt(tok) {
            if lengths.len() < 3 {
                lengths.push(l);
            }
        } else if let Some(c) = parse_css_color(tok) {
            color = Some(c);
        }
    }
    if lengths.len() < 2 {
        return None;
    }
    Some(Shadow {
        // A delta cannot reference `currentColor`, black like most cue shadows.
        color: color.unwrap_or(Color::BLACK),
        dx: lengths[0],
        dy: lengths[1],
        blur: lengths.get(2).copied().unwrap_or(0.0),
    })
}

/// Parse a CSS color.
pub fn parse_css_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if let Some(inner) = strip_func(s, "rgb").or_else(|| strip_func(s, "rgba")) {
        return parse_rgb_args(inner);
    }
    if let Some(inner) = strip_func(s, "hsl").or_else(|| strip_func(s, "hsla")) {
        return parse_hsl_args(inner);
    }
    if s.eq_ignore_ascii_case("transparent") {
        return Some(Color::TRANSPARENT);
    }
    crate::ir::named_color(s)
}

fn strip_func<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let rest = s.get(..name.len())?;
    if !rest.eq_ignore_ascii_case(name) {
        return None;
    }
    s[name.len()..].trim_start().strip_prefix('(')?.trim_end().strip_suffix(')')
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let nib = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).unwrap();
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
    Some(match hex.len() {
        3 => Color::rgb(nib(0) * 17, nib(1) * 17, nib(2) * 17),
        4 => Color::rgba(nib(0) * 17, nib(1) * 17, nib(2) * 17, nib(3) * 17),
        6 => Color::rgb(byte(0), byte(2), byte(4)),
        8 => Color::rgba(byte(0), byte(2), byte(4), byte(6)),
        _ => return None,
    })
}

fn color_args(inner: &str) -> (Vec<&str>, Option<&str>) {
    let (main, alpha) = match inner.split_once('/') {
        Some((m, a)) => (m, Some(a.trim())),
        None => (inner, None),
    };
    let args: Vec<&str> =
        main.split([',', ' ', '\t']).map(str::trim).filter(|a| !a.is_empty()).collect();
    if alpha.is_none() && args.len() == 4 {
        (args[..3].to_vec(), Some(args[3]))
    } else {
        (args, alpha)
    }
}

fn parse_alpha(a: &str) -> Option<u8> {
    let v = if let Some(p) = a.strip_suffix('%') {
        p.trim().parse::<f32>().ok()? / 100.0
    } else {
        a.parse::<f32>().ok()?
    };
    Some((v.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn parse_rgb_args(inner: &str) -> Option<Color> {
    let (args, alpha) = color_args(inner);
    if args.len() != 3 {
        return None;
    }
    let chan = |a: &str| -> Option<u8> {
        let v = if let Some(p) = a.strip_suffix('%') {
            p.trim().parse::<f32>().ok()? * 255.0 / 100.0
        } else {
            a.parse::<f32>().ok()?
        };
        Some(v.clamp(0.0, 255.0).round() as u8)
    };
    let (r, g, b) = (chan(args[0])?, chan(args[1])?, chan(args[2])?);
    let a = match alpha {
        Some(a) => parse_alpha(a)?,
        None => 0xff,
    };
    Some(Color::rgba(r, g, b, a))
}

fn parse_hsl_args(inner: &str) -> Option<Color> {
    let (args, alpha) = color_args(inner);
    if args.len() != 3 {
        return None;
    }
    let h = args[0].trim_end_matches("deg").parse::<f32>().ok()?.rem_euclid(360.0);
    let pct = |a: &str| -> Option<f32> {
        Some(a.strip_suffix('%')?.trim().parse::<f32>().ok()?.clamp(0.0, 100.0) / 100.0)
    };
    let (s, l) = (pct(args[1])?, pct(args[2])?);
    let a = match alpha {
        Some(a) => parse_alpha(a)?,
        None => 0xff,
    };

    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = match h {
        h if h < 60.0 => (c, x, 0.0),
        h if h < 120.0 => (x, c, 0.0),
        h if h < 180.0 => (0.0, c, x),
        h if h < 240.0 => (0.0, x, c),
        h if h < 300.0 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to8 = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(Color::rgba(to8(r1), to8(g1), to8(b1), a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_rule(css: &str) -> (Rule, Stylesheet) {
        let sheet = Stylesheet::parse(css);
        assert_eq!(sheet.rules().len(), 1, "css: {css:?}");
        (sheet.rules()[0].clone(), sheet)
    }

    fn family<'a>(sheet: &'a Stylesheet, r: &Rule) -> Option<&'a str> {
        r.style.family_ref().map(|f| sheet.names().get(f))
    }

    #[test]
    fn css_colors_parse() {
        assert_eq!(parse_css_color("red"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(parse_css_color("Papayawhip"), Some(Color::rgb(0xff, 0xef, 0xd5)));
        assert_eq!(parse_css_color("transparent"), Some(Color::TRANSPARENT));
        assert_eq!(parse_css_color("#f00"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(parse_css_color("#f008"), Some(Color::rgba(255, 0, 0, 0x88)));
        assert_eq!(parse_css_color("#12345678"), Some(Color::rgba(0x12, 0x34, 0x56, 0x78)));
        assert_eq!(parse_css_color("rgb(1, 2, 3)"), Some(Color::rgb(1, 2, 3)));
        assert_eq!(parse_css_color("rgb(1 2 3)"), Some(Color::rgb(1, 2, 3)));
        assert_eq!(parse_css_color("rgba(255, 0, 0, 0.5)"), Some(Color::rgba(255, 0, 0, 128)));
        assert_eq!(parse_css_color("rgb(100% 0% 0% / 50%)"), Some(Color::rgba(255, 0, 0, 128)));
        assert_eq!(parse_css_color("hsl(0, 100%, 50%)"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(parse_css_color("hsl(120, 100%, 25%)"), Some(Color::rgb(0, 128, 0)));
        assert_eq!(parse_css_color("hsl(240 100% 50%)"), Some(Color::rgb(0, 0, 255)));
        assert_eq!(parse_css_color("nonsense"), None);
        assert_eq!(parse_css_color("rgb(1,2)"), None);
    }

    #[test]
    fn argless_cue_selector() {
        let (r, _) = one_rule("::cue { color: yellow }");
        assert_eq!(r.compound, None);
        assert_eq!(r.style.foreground(), Some(Color::rgb(255, 255, 0)));
    }

    #[test]
    fn subject_matching_follows_the_featureless_element() {
        for sel in ["::cue", "*::cue", "*|*::cue", "|*::cue", ":not(video)::cue"] {
            assert_eq!(
                Stylesheet::parse(&format!("{sel} {{ color: red }}")).rules().len(),
                1,
                "{sel} must apply"
            );
        }
        for sel in [
            "video::cue",
            ":not(|*)::cue",
            "html|*::cue",
            "* ::cue",
            "* > *::cue",
            "* + *::cue",
            ".cls::cue",
        ] {
            assert!(
                Stylesheet::parse(&format!("{sel} {{ color: red }}")).is_empty(),
                "{sel} must not apply"
            );
        }
    }

    #[test]
    fn compound_parts_parse() {
        let (r, _) = one_rule("::cue(v.loud.blue[voice=\"Fred\"]:lang(en)) { color: red }");
        let c = r.compound.unwrap();
        assert_eq!(c.element.as_deref(), Some("v"));
        assert_eq!(c.classes, vec!["loud", "blue"]);
        assert_eq!(c.voice.as_deref(), Some("Fred"));
        assert_eq!(c.langs, vec!["en"]);
    }

    #[test]
    fn id_selector_and_specificity_order() {
        let sheet = Stylesheet::parse(
            "::cue(#intro) { color: red } ::cue(b) { color: blue } ::cue(.x) { color: lime }",
        );
        let specs: Vec<u32> = sheet.rules().iter().map(|r| r.specificity).collect();
        assert_eq!(specs, vec![1, 10, 100]);
    }

    #[test]
    fn cue_region_and_unknown_selectors_are_skipped() {
        assert!(Stylesheet::parse("::cue-region { color: red }").is_empty());
        assert!(Stylesheet::parse(".plain { color: red }").is_empty());
        assert!(Stylesheet::parse("::cue(b i) { color: red }").is_empty()); // combinator
    }

    #[test]
    fn selector_list_shares_declarations() {
        let sheet = Stylesheet::parse("::cue(b), ::cue(i) { color: red }");
        assert_eq!(sheet.rules().len(), 2);
    }

    #[test]
    fn past_future_never_match() {
        let sheet = Stylesheet::parse("::cue(:past) { color: red }");
        let mut style = StyleRec::default();
        let mut ruby = RubyPosition::Over;
        let node = Node { element: Some("c"), ..Node::default() };
        sheet.apply(&node, &mut Names::default(), &mut style, &mut ruby);
        assert_eq!(style.foreground(), None);
    }

    fn apply_to(css: &str, node: &Node) -> StyleRec {
        let sheet = Stylesheet::parse(css);
        let mut style = StyleRec::default();
        let mut ruby = RubyPosition::Over;
        sheet.apply(node, &mut Names::default(), &mut style, &mut ruby);
        style
    }

    #[test]
    fn type_and_class_matching() {
        let node = Node { element: Some("c"), classes: ".loud", ..Node::default() };
        assert_eq!(
            apply_to("::cue(c) { color: red }", &node).foreground(),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(
            apply_to("::cue(.loud) { color: red }", &node).foreground(),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(apply_to("::cue(b) { color: red }", &node).foreground(), None);
        assert_eq!(apply_to("::cue(.quiet) { color: red }", &node).foreground(), None);
        assert_eq!(
            apply_to("::cue(*) { color: red }", &node).foreground(),
            Some(Color::rgb(255, 0, 0))
        );
    }

    #[test]
    fn names_cross_into_the_cue_region() {
        let sheet = Stylesheet::parse("::cue(c) { font-family: Verdana }");
        let node = Node { element: Some("c"), ..Node::default() };
        let mut names = Names::default();
        let mut style = StyleRec::default();
        let mut ruby = RubyPosition::Over;
        sheet.apply(&node, &mut names, &mut style, &mut ruby);
        assert_eq!(style.family_ref().map(|f| names.get(f)), Some("Verdana"));
    }

    #[test]
    fn voice_matching() {
        let node = Node { element: Some("v"), voice: Some("Fred"), ..Node::default() };
        assert_eq!(
            apply_to("::cue(v[voice=\"Fred\"]) { color: red }", &node).foreground(),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(apply_to("::cue(v[voice=\"Bill\"]) { color: red }", &node).foreground(), None);
    }

    #[test]
    fn lang_matching_is_prefix_on_boundaries() {
        let node = Node { element: Some("lang"), lang: Some("en-US"), ..Node::default() };
        assert_eq!(
            apply_to("::cue(:lang(en)) { color: red }", &node).foreground(),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(
            apply_to("::cue(:lang(EN-us)) { color: red }", &node).foreground(),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(apply_to("::cue(:lang(e)) { color: red }", &node).foreground(), None);
    }

    #[test]
    fn root_id_matching() {
        let root = Node { id: Some("intro"), ..Node::default() };
        assert_eq!(
            apply_to("::cue(#intro) { color: red }", &root).foreground(),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(apply_to("::cue(#outro) { color: red }", &root).foreground(), None);
        assert_eq!(
            apply_to("::cue { color: red }", &root).foreground(),
            Some(Color::rgb(255, 0, 0))
        );
        let inner = Node { element: Some("b"), ..Node::default() };
        assert_eq!(apply_to("::cue { color: red }", &inner).foreground(), None);
    }

    #[test]
    fn cascade_specificity_beats_order() {
        let node = Node { element: Some("b"), ..Node::default() };
        let node = Node { classes: "x", ..node };
        let s = apply_to("::cue(.x) { color: lime } ::cue(b) { color: red }", &node);
        assert_eq!(s.foreground(), Some(Color::rgb(0, 255, 0)));
    }

    #[test]
    fn cascade_order_breaks_ties() {
        let node = Node { element: Some("b"), ..Node::default() };
        let s = apply_to("::cue(b) { color: lime } ::cue(b) { color: red }", &node);
        assert_eq!(s.foreground(), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn font_declarations() {
        let (r, sheet) = one_rule(
            "::cue { font-family: \"Comic Sans\", sans-serif; font-size: 150%; \
             font-style: italic; font-weight: bold }",
        );
        assert_eq!(family(&sheet, &r), Some("Comic Sans, sans-serif"));
        assert_eq!(r.style.font_size(), Some(FontSize::Scale(1.5)));
        assert_eq!(r.style.font_style(), Some(FontStyle::Italic));
        assert_eq!(r.style.font_weight(), Some(700));
    }

    #[test]
    fn font_shorthand() {
        let (r, sheet) = one_rule("::cue { font: italic bold 16px/1.4 Arial, sans-serif }");
        assert_eq!(r.style.font_style(), Some(FontStyle::Italic));
        assert_eq!(r.style.font_weight(), Some(700));
        assert_eq!(r.style.font_size(), Some(FontSize::Points(12.0)));
        assert_eq!(family(&sheet, &r), Some("Arial, sans-serif"));
    }

    #[test]
    fn font_size_units() {
        assert_eq!(parse_font_size("16px"), Some(FontSize::Points(12.0)));
        assert_eq!(parse_font_size("12pt"), Some(FontSize::Points(12.0)));
        assert_eq!(parse_font_size("1.5em"), Some(FontSize::Scale(1.5)));
        assert_eq!(parse_font_size("80%"), Some(FontSize::Scale(0.8)));
        assert_eq!(parse_font_size("xx-large"), Some(size_step(3)));
        assert_eq!(parse_font_size("16"), None);
    }

    #[test]
    fn decoration_shadow_outline_background() {
        let (r, _) = one_rule(
            "::cue(b) { text-decoration: underline line-through; \
             text-shadow: #000000 2px 2px 4px; outline: 2px solid yellow; \
             background: rgba(0, 0, 0, 0.8) }",
        );
        assert_eq!(r.style.underline(), Some(true));
        assert_eq!(r.style.strikethrough(), Some(true));
        assert_eq!(
            r.style.shadow(),
            Some(Shadow { color: Color::BLACK, dx: 1.5, dy: 1.5, blur: 3.0 })
        );
        assert_eq!(r.style.outline(), Some(Outline { color: Color::rgb(255, 255, 0), width: 1.5 }));
        assert_eq!(r.style.background(), Some(Color::rgba(0, 0, 0, 204)));
    }

    #[test]
    fn text_decoration_none_disables() {
        let (r, _) = one_rule("::cue(u) { text-decoration: none }");
        assert_eq!(r.style.underline(), Some(false));
        assert_eq!(r.style.strikethrough(), Some(false));
    }

    #[test]
    fn outline_longhands_merge() {
        let (r, _) = one_rule("::cue { outline-color: red; outline-width: 4px }");
        assert_eq!(r.style.outline(), Some(Outline { color: Color::rgb(255, 0, 0), width: 3.0 }));
    }

    #[test]
    fn ruby_position_declaration() {
        let (r, _) = one_rule("::cue(rt) { ruby-position: under }");
        assert_eq!(r.ruby_position, Some(RubyPosition::Under));
        assert!(r.style.is_plain());
    }

    #[test]
    fn important_is_stripped() {
        let (r, _) = one_rule("::cue { color: red !important }");
        assert_eq!(r.style.foreground(), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn comments_at_rules_and_junk_are_skipped() {
        let sheet = Stylesheet::parse(
            "/* header */ @import url(x.css); \
             @media screen { ::cue { color: blue } } \
             ::cue { /* inline */ color: red; not-a-prop: 4; } \
             garbage ; } ::cue(b) { color: lime }",
        );
        assert_eq!(sheet.rules().len(), 2);
        assert_eq!(sheet.rules()[0].style.foreground(), Some(Color::rgb(255, 0, 0)));
        assert_eq!(sheet.rules()[1].style.foreground(), Some(Color::rgb(0, 255, 0)));
    }

    #[test]
    fn declaration_only_rules_with_nothing_applicable_are_dropped() {
        assert!(Stylesheet::parse("::cue { white-space: pre; opacity: 0.5 }").is_empty());
    }

    #[test]
    fn unterminated_block_is_tolerated() {
        let sheet = Stylesheet::parse("::cue { color: red");
        assert_eq!(sheet.rules().len(), 1);
    }

    #[test]
    fn multiple_push_css_keeps_cascade_order() {
        let mut sheet = Stylesheet::parse("::cue(b) { color: red }");
        sheet.push_css("::cue(b) { color: lime }");
        let node = Node { element: Some("b"), ..Node::default() };
        let mut style = StyleRec::default();
        let mut ruby = RubyPosition::Over;
        sheet.apply(&node, &mut Names::default(), &mut style, &mut ruby);
        assert_eq!(style.foreground(), Some(Color::rgb(0, 255, 0)));
    }

    #[test]
    fn deeply_nested_not_in_a_prelude_is_cut_off_not_recursed() {
        let depth = 100_000;
        let css = format!("{}*{}::cue {{ color: red }}", ":not(".repeat(depth), ")".repeat(depth));
        assert!(Stylesheet::parse(&css).rules().len() <= 1);
        let css = "::cue { color: red } :not(:not(*))::cue { color: blue }";
        assert_eq!(Stylesheet::parse(css).rules().len(), 2);
    }

    #[test]
    fn rules_past_the_cap_are_dropped() {
        let css: String =
            (0..MAX_RULES + 100).map(|i| format!("::cue(.c{i}) {{ color: red }}\n")).collect();
        let mut sheet = Stylesheet::parse(&css);
        assert_eq!(sheet.rules().len(), MAX_RULES);
        sheet.push_css("::cue { color: blue }");
        assert_eq!(sheet.rules().len(), MAX_RULES);
    }
}
