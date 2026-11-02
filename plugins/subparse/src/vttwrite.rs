// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! WebVTT segments (RFC 8216bis section 3.1.4, W3C WebVTT) from `subtitle/cue-ir` cues, on a
//! segment grid the caller gives as starts and an end. A cue goes into every segment it shows
//! in, with its whole span, as the RFC asks, so a player that joins mid cue still shows it. A
//! segment is written once the stream is known to be past its end, by a cue that starts later
//! or a gap, plus `MARGIN` for a muxer that interleaves by stamps a B-frame put ahead.

use crate::ir::{Anchor, CueIr, FontStyle, LayoutRec, LinePosition, TextAlign};
use std::fmt::Write as _;
use std::io::Write as _;

/// How far past a segment's end the stream must be before it is written, ns.
pub const MARGIN: i64 = 1_000_000_000;
/// A cue that states no end shows this long, ns.
pub const OPEN_CUE: i64 = 4_000_000_000;
/// Cue times are the media timeline, as an fMP4 rendition's are, no MPEG-TS offset.
const HEADER: &[u8] = b"WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n";

/// One cue, its settings line suffix and its text, both as WebVTT.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cue {
    start: i64,
    end: i64,
    settings: String,
    text: String,
}

/// The cues of one track between `subparse` and the segments, and the next segment due.
#[derive(Debug, Default)]
pub struct Track {
    /// By start, those that may still show in a segment not written.
    cues: Vec<Cue>,
    pub next: u32,
    /// The stream holds no cue before this that has not come, ns.
    known: i64,
    /// Spare strings of cues dropped.
    spare: Vec<String>,
}

impl Track {
    /// Starts over at segment `from`, after a seek or a restart.
    pub fn reset(&mut self, from: u32) {
        for c in self.cues.drain(..) {
            self.spare.push(c.settings);
            self.spare.push(c.text);
        }
        self.next = from;
        self.known = i64::MIN;
    }

    fn string(&mut self) -> String {
        let mut s = self.spare.pop().unwrap_or_default();
        s.clear();
        s
    }

    /// A cue at `start` lasting `dur` ns, None for no stated length. An empty one is left out.
    pub fn cue(&mut self, start: i64, dur: Option<i64>, ir: &CueIr<'_>) {
        self.heard(start);
        let mut text = self.string();
        cue_text(ir, &mut text);
        if text.is_empty() {
            self.spare.push(text);
            return;
        }
        let mut settings = self.string();
        cue_settings(ir.layout(), &mut settings);
        let end = start + dur.filter(|&d| d > 0).unwrap_or(OPEN_CUE);
        let at = self.cues.partition_point(|c| c.start <= start);
        self.cues.insert(at, Cue { start, end, settings, text });
    }

    /// The stream holds nothing before `t` it has not put out.
    pub fn heard(&mut self, t: i64) {
        self.known = self.known.max(t);
    }

    /// Writes the next segment into `out` when it is whole, or at the end of the stream
    /// whatever is left, and returns it. Segment k spans `starts[k]` to the next start or
    /// `end`, ns.
    pub fn ready(
        &mut self,
        starts: &[i64],
        end: i64,
        ended: bool,
        out: &mut Vec<u8>,
    ) -> Option<u32> {
        let k = self.next;
        let from = *starts.get(k as usize)?;
        let to = starts.get(k as usize + 1).copied().unwrap_or(end);
        if !ended && self.known < to.saturating_add(MARGIN) {
            return None;
        }
        segment(out, self.cues.iter().filter(|c| c.start < to && c.end > from));
        self.next = k + 1;
        let spare = &mut self.spare;
        self.cues.retain_mut(|c| {
            if c.end > to {
                return true;
            }
            spare.push(std::mem::take(&mut c.settings));
            spare.push(std::mem::take(&mut c.text));
            false
        });
        Some(k)
    }
}

/// A WebVTT segment of `cues` into `out`.
fn segment<'a>(out: &mut Vec<u8>, cues: impl Iterator<Item = &'a Cue>) {
    out.clear();
    out.extend_from_slice(HEADER);
    for c in cues {
        stamp(out, c.start);
        out.extend_from_slice(b" --> ");
        stamp(out, c.end);
        out.extend_from_slice(c.settings.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(c.text.as_bytes());
        out.extend_from_slice(b"\n\n");
    }
}

/// `hh:mm:ss.ttt`, to the nearest millisecond.
fn stamp(out: &mut Vec<u8>, ns: i64) {
    let ms = (ns.max(0) + 500_000) / 1_000_000;
    let _ = write!(
        out,
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    );
}

/// The cue's text with italics, bold and underline as WebVTT tags, a line per line. An empty
/// line would end the cue and is left out.
pub fn cue_text(ir: &CueIr<'_>, out: &mut String) {
    let slanted = |f: Option<FontStyle>| matches!(f, Some(FontStyle::Italic | FontStyle::Oblique));
    let heavy = |w: Option<u16>| w.is_some_and(|w| w >= 600);
    let base = ir.base_style();
    for line in ir.lines() {
        let at = out.len();
        let mut any = false;
        for span in ir.line_spans(line) {
            let text = ir.span_text(span);
            if text.is_empty() {
                continue;
            }
            any |= !text.trim().is_empty();
            let st = ir.span_style(span);
            let i = slanted(st.font_style().or(base.font_style()));
            let b = heavy(st.font_weight().or(base.font_weight()));
            let u = st.underline().or(base.underline()).unwrap_or(false);
            let tags = [(i, "i"), (b, "b"), (u, "u")];
            for (on, t) in tags {
                if on {
                    let _ = write!(out, "<{t}>");
                }
            }
            escape(text, out);
            for (on, t) in tags.iter().rev() {
                if *on {
                    let _ = write!(out, "</{t}>");
                }
            }
        }
        if any {
            // a trailing space outside any tag, ASS rows often have one
            let n = out.trim_end_matches([' ', '\t']).len().max(at);
            out.truncate(n);
            out.push('\n');
        } else {
            out.truncate(at);
        }
    }
    let n = out.trim_end_matches('\n').len();
    out.truncate(n);
}

/// `&`, `<` and `>` as references, a line break inside a span as a space.
fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
}

/// The cue settings, each led by a space: line from the layout or its anchor's row, position,
/// size, and align from the layout or its anchor's column.
pub fn cue_settings(l: &LayoutRec, out: &mut String) {
    let (row, col) = match l.anchor() {
        Some(Anchor::TopLeft) => (Some(0), Some(-1)),
        Some(Anchor::TopCenter) => (Some(0), None),
        Some(Anchor::TopRight) => (Some(0), Some(1)),
        Some(Anchor::CenterLeft) => (Some(1), Some(-1)),
        Some(Anchor::Center) => (Some(1), None),
        Some(Anchor::CenterRight) => (Some(1), Some(1)),
        Some(Anchor::BottomLeft) => (None, Some(-1)),
        Some(Anchor::BottomRight) => (None, Some(1)),
        Some(Anchor::BottomCenter) | None => (None, None),
    };
    match (l.line(), row) {
        (Some(LinePosition::Percent(p)), _) => pct(out, " line:", p),
        (Some(LinePosition::Line(n)), _) => {
            let _ = write!(out, " line:{n}");
        }
        (None, Some(0)) => out.push_str(" line:0"),
        (None, Some(_)) => out.push_str(" line:50%"),
        (None, None) => {}
    }
    if let Some(p) = l.position() {
        pct(out, " position:", p);
    }
    if let Some(s) = l.size() {
        pct(out, " size:", s);
    }
    let align = match (l.align(), col) {
        (Some(TextAlign::Start), _) => Some("start"),
        (Some(TextAlign::Center), _) => Some("center"),
        (Some(TextAlign::End), _) => Some("end"),
        (Some(TextAlign::Left), _) | (None, Some(-1)) => Some("left"),
        (Some(TextAlign::Right), _) | (None, Some(1)) => Some("right"),
        (None, _) => None,
    };
    if let Some(a) = align {
        let _ = write!(out, " align:{a}");
    }
}

/// `key` and a percentage clamped to the frame, at most three decimals, no trailing zeros.
fn pct(out: &mut String, key: &str, p: f32) {
    out.push_str(key);
    let at = out.len();
    let _ = write!(out, "{:.3}", p.clamp(0.0, 100.0));
    let n = out[at..].trim_end_matches('0').trim_end_matches('.').len();
    out.truncate(at + n);
    out.push('%');
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::ir::{CueBuilder, StyleRec};

    const S: i64 = 1_000_000_000;
    const MS: i64 = 1_000_000;
    /// Segments of 2 s over 10 s.
    const GRID: [i64; 5] = [0, 2 * S, 4 * S, 6 * S, 8 * S];
    const END: i64 = 10 * S;

    fn build(f: impl FnOnce(&mut CueBuilder)) -> Vec<u8> {
        let mut b = CueBuilder::new();
        f(&mut b);
        b.to_vec()
    }

    fn plain(t: &str) -> Vec<u8> {
        build(|b| b.plain_text(t))
    }

    fn view(p: &[u8]) -> CueIr<'_> {
        CueIr::parse(p).expect("a builder payload validates")
    }

    fn text(p: &[u8]) -> String {
        let mut s = String::new();
        cue_text(&view(p), &mut s);
        s
    }

    fn header() -> &'static str {
        std::str::from_utf8(HEADER).expect("ascii")
    }

    #[test]
    fn styles_become_tags_and_markup_is_escaped() {
        let ir = build(|b| b.srt_text("<i>Hello</i> <b>a & b</b>\n\n1 < 2 --> 3 "));
        assert_eq!(text(&ir), "<i>Hello</i> <b>a &amp; b</b>\n1 &lt; 2 --&gt; 3");
        let ir = build(|b| {
            b.plain_text("all of it");
            b.base.set_font_style(Some(FontStyle::Italic));
        });
        assert_eq!(text(&ir), "<i>all of it</i>");
        let ir = build(|b| {
            let mut st = StyleRec::default();
            st.set_font_weight(Some(700));
            st.set_underline(Some(true));
            b.push_text("x");
            b.flush(&st, None, None);
            b.end_line();
            b.end_line();
        });
        assert_eq!(text(&ir), "<b><u>x</u></b>", "nested in order, no blank line");
        assert_eq!(text(&plain("  \n")), "");
    }

    #[test]
    fn an_anchor_or_the_layout_places_the_cue() {
        let mut s = String::new();
        let ir = build(|b| b.srt_text("{\\an8}on top"));
        cue_settings(view(&ir).layout(), &mut s);
        assert_eq!(s, " line:0 align:center");
        assert_eq!(text(&ir), "on top");
        let mut l = LayoutRec::default();
        l.set_line(Some(LinePosition::Percent(12.5)));
        l.set_position(Some(30.0));
        l.set_size(Some(140.0));
        l.set_anchor(Some(Anchor::BottomRight));
        s.clear();
        cue_settings(&l, &mut s);
        assert_eq!(s, " line:12.5% position:30% size:100% align:right");
        l.set_position(Some(0.0));
        l.set_size(Some(33.3333));
        s.clear();
        cue_settings(&l, &mut s);
        assert_eq!(s, " line:12.5% position:0% size:33.333% align:right");
        s.clear();
        cue_settings(&LayoutRec::default(), &mut s);
        assert_eq!(s, "");
    }

    #[test]
    fn stamps_are_hours_minutes_seconds_and_milliseconds() {
        let mut s = Vec::new();
        stamp(&mut s, 3_723_456_600_000 / 1000 * 1000);
        assert_eq!(s, b"01:02:03.457");
        s.clear();
        stamp(&mut s, -5);
        assert_eq!(s, b"00:00:00.000");
    }

    fn body(out: &[u8]) -> String {
        String::from_utf8(out.to_vec()).expect("utf8")
    }

    #[test]
    fn a_cue_shows_in_every_segment_it_spans() {
        let mut t = Track::default();
        t.reset(0);
        let mut out = Vec::new();
        t.cue(1500 * MS, Some(3 * S), &view(&plain("long")));
        assert_eq!(t.ready(&GRID, END, false, &mut out), None, "not past 2 s and the margin");
        t.heard(2 * S + MARGIN);
        assert_eq!(t.ready(&GRID, END, false, &mut out), Some(0));
        assert_eq!(body(&out), format!("{}00:00:01.500 --> 00:00:04.500\nlong\n\n", header()));
        t.cue(5 * S, None, &view(&plain("open")));
        t.heard(4 * S + MARGIN);
        assert_eq!(t.ready(&GRID, END, false, &mut out), Some(1));
        assert!(body(&out).contains("long"), "still showing in 1");
        assert_eq!(t.ready(&GRID, END, false, &mut out), None, "4 to 6 waits");
        assert_eq!(t.ready(&GRID, END, true, &mut out), Some(2), "the end lets it out");
        assert!(body(&out).contains("long"), "ends at 4.5 s, inside 2");
        assert!(body(&out).contains("00:00:05.000 --> 00:00:09.000\nopen"));
        assert_eq!(t.ready(&GRID, END, true, &mut out), Some(3));
        assert!(!body(&out).contains("long"), "gone by 3");
        assert_eq!(t.ready(&GRID, END, true, &mut out), Some(4));
        assert!(body(&out).contains("open"), "open to 9 s, inside 4");
        assert_eq!(t.ready(&GRID, END, true, &mut out), None, "past the last");
        t.reset(0);
        assert_eq!(t.ready(&GRID, END, true, &mut out), Some(0));
        assert_eq!(body(&out), header(), "a segment of no cue is a header");
    }

    #[test]
    fn cues_come_out_by_start_and_a_reset_starts_at_its_segment() {
        let mut t = Track::default();
        t.reset(3);
        t.cue(6500 * MS, Some(S), &view(&plain("b")));
        t.cue(5 * S, Some(3 * S), &view(&plain("a")));
        t.heard(i64::MAX / 2);
        let mut out = Vec::new();
        assert_eq!(t.ready(&GRID, END, false, &mut out), Some(3));
        let s = body(&out);
        assert!(s.find("\na\n").expect("a") < s.find("\nb\n").expect("b"), "{s}");
        t.reset(0);
        assert_eq!(t.ready(&GRID, END, false, &mut out), None, "nothing known after a reset");
        assert_eq!(t.next, 0);
    }
}
