// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Chunk-boundary tests for [`SubtitleFormat::parse_incremental`].

// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use pf_subparse::{Cue, Format, ParseContext};

fn ctx() -> ParseContext {
    ParseContext::default()
}

struct Fixture {
    format: Format,
    name: &'static str,
    body: &'static str,
}

fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            format: Format::SubRip,
            name: "subrip",
            body: "1\n\
               00:00:01,000 --> 00:00:02,000\n\
               Café — naïve\n\
               Second line\n\
               \n\
               2\n\
               00:00:02,500 --> 00:00:04,000\n\
               <i>Two</i> & <b>bold\n\
               \n\
               not a cue id\n\
               3\n\
               00:00:0broken --> 00:00:09,000\n\
               \n\
               4\n\
               00:00:05,000 --> 00:00:06,000\n\
               Last cue, no blank line after it",
        },
        Fixture {
            format: Format::WebVtt,
            name: "webvtt",
            body: "WEBVTT\n\
               \n\
               NOTE this is a comment\n\
               \n\
               STYLE\n\
               ::cue(b) { color: peachpuff }\n\
               \n\
               STYLE\n\
               ::cue { background: rgba(0, 0, 0, 0.8) }\n\
               00:00:00.500 --> 00:00:00.900\n\
               Timing line ends the style block\n\
               \n\
               cue-id-1\n\
               00:00:01.000 --> 00:00:02.000\n\
               Héllo wörld\n\
               \n\
               00:00:03.000 --> 00:00:04.000 A:middle L:80%\n\
               <b>Two</b> <font>dropped</font>\n\
               \n\
               00:00:05.000 --> 00:00:06.000\n\
               No blank line before the next timing line\n\
               00:00:07.000 --> 00:00:08.000\n\
               Trailing cue with no blank line",
        },
        Fixture {
            format: Format::MicroDvd,
            name: "microdvd",
            body: "{1}{1}25.000\n\
               {25}{50}Café|second line\n\
               {100}{200}{y:i}{s:20}Two\n\
               this line is not microdvd at all\n\
               {300}{400}{s:20 broken\n\
               {500}{600}Trailing, unterminated",
        },
        Fixture {
            format: Format::Mpl2,
            name: "mpl2",
            body: "[123][456] This is the Earth|when the dinosaurs roamed\n\
               [1234][5678]a lush and fertile planét.\n\
               garbage\n\
               [bad]\n\
               [12345][27890] /Italic|Normal\n\
               [42345][47890]Trailing, unterminated",
        },
        Fixture {
            format: Format::SubViewer,
            name: "subviewer",
            body: "[INFORMATION]\n\
               [TITLE]xxxxxxxxxx\n\
               [AUTHOR]xxxxxxxx\n\
               [END INFORMATION]\n\
               [SUBTITLE]\n\
               [COLF]&HFFFFFF,[STYLE]bd,[SIZE]18,[FONT]Arial\n\
               00:00:41.00,00:00:44.40\n\
               The Age of Göds was closing.\n\
               Eternity had come to an end.\n\
               \n\
               00:00:55.00,00:00:58.40\n\
               THERE IS A PLACE[br]ON EARTH\n\
               \n\
               00:01:00.00,00:01:02.00\n\
               never terminated\n",
        },
        Fixture {
            format: Format::Sami,
            name: "sami",
            body: "<SAMI>\n\
               <HEAD><TITLE>Tëst</TITLE></HEAD>\n\
               <BODY>\n\
               <SYNC Start=1000><P>Hello wörld\n\
               <SYNC Start=2000><P>Second <i>italic</i> line\n\
               <SYNC Start=3000><P>Third with <font color=FF0000>colour</font>\n\
               <SYNC Start=4000><P>Fourth\n\
               </BODY>\n\
               </SAMI>\n",
        },
        Fixture {
            format: Format::TmPlayer,
            name: "tmplayer",
            body: "00:00:10,1=This is the Earth at a time\n\
               Yooboo wabahablablahuguug bogus line\n\
               00:00:10,2=when the dinosaurs roaméd...\n\
               00:00:13,1=\n\
               00:00:14,1=a lush and fertile planet.\n\
               00:00:16,1=\n\
               00:00:30,1=Trailing, unterminated",
        },
        Fixture {
            format: Format::MpSub,
            name: "mpsub",
            body: "FORMAT=TIME\n\
               \n\
               2.0 3.0\n\
               Hello wörld\n\
               \n\
               1.5 2.0\n\
               Second cue\n\
               Second line\n\
               \n\
               10\n\
               1.0 1.0\n\
               never terminated\n",
        },
        Fixture {
            format: Format::QtText,
            name: "qttext",
            body: "{QTtext}{font:Sans}{size:18}{textColor:65535,0,0}\n\
               [00:00:01.00]\n\
               Hello wörld\n\
               {bold}Bold line\n\
               [00:00:03.00]\n\
               {bad tag with no close\n\
               Second cue\n\
               [00:00:05.00]\n\
               Trailing block with no closing timestamp",
        },
        Fixture {
            format: Format::Lrc,
            name: "lrc",
            body: "[ti:Title]\n\
               [ar:Artist]\n\
               [00:01.00]first lyric\n\
               [00:02.34]sécond lyric\n\
               [00:05.00 no closing bracket\n\
               [123:04.05]late lyric\n\
               [00:09.00]never terminated",
        },
        Fixture {
            format: Format::Dks,
            name: "dks",
            body: "[00:00:07]THERE IS A PLACE ON EARTH[br]WHERE IT IS MÖRNING\n\
               [00:00:12]\n\
               [00:00:13]AND THE GREAT HERDS RUN FREE.\n\
               not a timestamp at all\n\
               [00:00:15]\n\
               [00:00:20]never terminated\n",
        },
        Fixture {
            format: Format::Ssa,
            name: "ssa",
            body: "[Script Info]\n\
               Title: Tëst\n\
               ScriptType: v4.00+\n\
               \n\
               [V4+ Styles]\n\
               Format: Name, Fontname, Fontsize\n\
               Style: Default,Arial,20\n\
               \n\
               [Events]\n\
               Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
               Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello {\\i1}wörld{\\i0}\n\
               Dialogue: 0,not-a-time,0:00:03.00,Default,,0,0,0,,dropped\n\
               Dialogue: 0,0:00:03.50,0:00:05.00,Default,,12,0,24,,{\\an8\\k25}Second\\Nline\n\
               Comment: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,ignored\n\
               Dialogue: 0,0:00:06.00,0:00:07.00,Default,,0,0,0,,Trailing, unterminated",
        },
    ]
}

fn crlf_body(body: &str) -> String {
    body.replace('\n', "\r\n")
}

fn longest_line(body: &str) -> usize {
    body.split('\n').map(str::len).max().unwrap_or(0)
}

fn parse_chunked(format: Format, body: &str, splits: &[usize]) -> Vec<Cue> {
    let mut parser = format.parser();
    let mut buf = String::new();
    let mut cues = Vec::new();

    let bound = longest_line(body);
    let mut prev = 0usize;
    let mut total_consumed = 0usize;

    let boundaries: Vec<usize> =
        splits.iter().copied().chain(std::iter::once(body.len())).collect();

    for (i, &end) in boundaries.iter().enumerate() {
        assert!(
            end >= prev && end <= body.len() && body.is_char_boundary(end),
            "bad test split {end} (prev {prev}, len {})",
            body.len()
        );
        buf.push_str(&body[prev..end]);
        prev = end;

        let at_eos = i + 1 == boundaries.len();
        let parsed = parser
            .parse_incremental(&buf, &ctx(), at_eos)
            .expect("parsers are infallible on valid UTF-8");

        assert!(
            parsed.consumed <= buf.len(),
            "consumed {} exceeds the {}-byte body",
            parsed.consumed,
            buf.len()
        );
        assert!(
            buf.is_char_boundary(parsed.consumed),
            "consumed {} lands inside a character (String::drain would panic)",
            parsed.consumed
        );

        cues.extend(parsed.cues);
        buf.drain(..parsed.consumed);
        total_consumed += parsed.consumed;

        assert!(
            buf.len() <= bound,
            "parser retained {} bytes, more than the longest line ({bound}); \
             the consumed offset is not advancing",
            buf.len()
        );

        if at_eos {
            assert!(buf.is_empty(), "at_eos left {} bytes unconsumed", buf.len());
        }
    }

    assert_eq!(
        total_consumed,
        body.len(),
        "the parser must consume every byte of the stream exactly once"
    );

    cues
}

fn parse_whole(format: Format, body: &str) -> Vec<Cue> {
    format.parser().parse(body, &ctx()).expect("parsers are infallible on valid UTF-8")
}

fn char_boundaries(body: &str) -> Vec<usize> {
    (0..=body.len()).filter(|&i| body.is_char_boundary(i)).collect()
}

#[test]
fn fixture_table_covers_every_format() {
    let have: Vec<Format> = fixtures().iter().map(|f| f.format).collect();
    for format in Format::ALL {
        assert!(
            have.contains(format),
            "{format:?} has no fixture in tests/incremental.rs, so none of the \
             chunk-boundary properties are being checked for it"
        );
    }
    assert_eq!(have.len(), Format::ALL.len());
}

#[test]
fn every_fixture_produces_cues() {
    for f in fixtures() {
        let cues = parse_whole(f.format, f.body);
        assert!(
            cues.len() >= 2,
            "{} fixture yields {} cues; it needs at least two for the \
             equivalence tests to be meaningful",
            f.name,
            cues.len()
        );
    }
}

#[test]
fn every_single_split_matches_whole() {
    for f in fixtures() {
        let expected = parse_whole(f.format, f.body);
        for k in char_boundaries(f.body) {
            let got = parse_chunked(f.format, f.body, &[k]);
            assert_eq!(
                got,
                expected,
                "{}: splitting after byte {k} changed the output\n  \
                 chunk 1 = {:?}\n  chunk 2 = {:?}",
                f.name,
                &f.body[..k],
                &f.body[k..]
            );
        }
    }
}

#[test]
fn every_single_split_matches_whole_with_crlf() {
    for f in fixtures() {
        let body = crlf_body(f.body);
        let expected = parse_whole(f.format, &body);
        for k in char_boundaries(&body) {
            let got = parse_chunked(f.format, &body, &[k]);
            assert_eq!(
                got, expected,
                "{}: CRLF body split after byte {k} changed the output",
                f.name
            );
        }
    }
}

fn cr_body(body: &str) -> String {
    body.replace('\n', "\r")
}

#[test]
fn cr_only_bodies_parse_like_lf_ones() {
    for f in fixtures() {
        let expected = parse_whole(f.format, f.body);
        assert_eq!(parse_whole(f.format, &cr_body(f.body)), expected, "{}: CR only", f.name);
    }
}

#[test]
fn every_single_split_matches_whole_with_cr() {
    for f in fixtures() {
        let body = cr_body(f.body);
        let expected = parse_whole(f.format, &body);
        for k in char_boundaries(&body) {
            let got = parse_chunked(f.format, &body, &[k]);
            assert_eq!(
                got, expected,
                "{}: CR body split after byte {k} changed the output",
                f.name
            );
        }
    }
}

#[test]
fn two_splits_match_whole() {
    for f in fixtures() {
        let expected = parse_whole(f.format, f.body);
        let bounds = char_boundaries(f.body);
        for (ai, &a) in bounds.iter().enumerate() {
            for &b in bounds.iter().skip(ai).step_by(7) {
                let got = parse_chunked(f.format, f.body, &[a, b]);
                assert_eq!(
                    got, expected,
                    "{}: splits after bytes {a} and {b} changed the output",
                    f.name
                );
            }
        }
    }
}

#[test]
fn one_char_at_a_time_matches_whole() {
    for f in fixtures() {
        let expected = parse_whole(f.format, f.body);
        let splits = char_boundaries(f.body);
        let got = parse_chunked(f.format, f.body, &splits);
        assert_eq!(got, expected, "{}: char-at-a-time changed the output", f.name);
    }
}

#[test]
fn single_chunk_matches_whole() {
    for f in fixtures() {
        let expected = parse_whole(f.format, f.body);
        let got = parse_chunked(f.format, f.body, &[]);
        assert_eq!(got, expected, "{}: single chunk changed the output", f.name);
    }
}

#[test]
fn empty_chunks_interleaved_match_whole() {
    for f in fixtures() {
        let expected = parse_whole(f.format, f.body);
        for k in char_boundaries(f.body) {
            let splits = [0, 0, k, k, k, f.body.len()];
            let got = parse_chunked(f.format, f.body, &splits);
            assert_eq!(
                got, expected,
                "{}: empty chunks around byte {k} changed the output",
                f.name
            );
        }
    }
}

#[test]
fn eos_on_empty_input_is_empty() {
    for format in Format::ALL {
        let mut parser = format.parser();
        let parsed = parser.parse_incremental("", &ctx(), true).unwrap();
        assert!(parsed.cues.is_empty(), "{format:?} emitted a cue for an empty stream");
        assert_eq!(parsed.consumed, 0);
        assert!(parse_whole(*format, "").is_empty());
    }
}

#[test]
fn empty_mid_stream_call_is_a_noop() {
    for format in Format::ALL {
        let mut parser = format.parser();
        for _ in 0..5 {
            let parsed = parser.parse_incremental("", &ctx(), false).unwrap();
            assert!(parsed.cues.is_empty(), "{format:?} emitted a cue from nothing");
            assert_eq!(parsed.consumed, 0);
        }
    }
}

#[test]
fn second_eos_call_emits_nothing() {
    for f in fixtures() {
        let mut parser = f.format.parser();
        let first = parser.parse_incremental(f.body, &ctx(), true).unwrap();
        assert_eq!(first.consumed, f.body.len());
        assert!(!first.cues.is_empty(), "{}: fixture emitted nothing", f.name);

        for round in 0..3 {
            let again = parser.parse_incremental("", &ctx(), true).unwrap();
            assert!(
                again.cues.is_empty(),
                "{}: EOS round {} re-emitted {} cue(s)",
                f.name,
                round + 2,
                again.cues.len()
            );
            assert_eq!(again.consumed, 0);
        }
    }
}

#[test]
fn unterminated_final_record_with_and_without_newline() {
    for f in fixtures() {
        for body in [f.body.trim_end_matches('\n').to_string(), {
            let mut b = f.body.trim_end_matches('\n').to_string();
            b.push('\n');
            b
        }] {
            let expected = parse_whole(f.format, &body);
            for k in char_boundaries(&body) {
                let got = parse_chunked(f.format, &body, &[k]);
                assert_eq!(
                    got, expected,
                    "{}: trailing-newline variant split at {k} disagreed",
                    f.name
                );
            }
        }
    }
}

#[test]
fn record_straddling_many_chunks() {
    for f in fixtures() {
        let body = inflate(f.format, f.body);
        let expected = parse_whole(f.format, &body);
        for chunk in [1usize, 2, 3, 7, 16, 64] {
            let splits: Vec<usize> = (0..body.len())
                .step_by(chunk)
                .map(|mut i| {
                    while i < body.len() && !body.is_char_boundary(i) {
                        i += 1;
                    }
                    i
                })
                .collect();
            let got = parse_chunked(f.format, &body, &splits);
            assert_eq!(
                got, expected,
                "{}: {chunk}-byte chunks over a long record disagreed",
                f.name
            );
        }
    }
}

fn inflate(_format: Format, body: &str) -> String {
    let pad = "x".repeat(300);
    let mut out = String::new();
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
        let plain = !line.is_empty()
            && !line.contains(':')
            && !line.contains('[')
            && !line.contains('{')
            && !line.contains('=')
            && !line.chars().all(|c| c.is_ascii_digit());
        if plain {
            out.push_str(&pad);
        }
    }
    out
}

#[test]
fn malformed_records_recover_identically_when_split() {
    for f in fixtures() {
        let body = with_garbage(f.body);
        let expected = parse_whole(f.format, &body);
        for k in char_boundaries(&body) {
            let got = parse_chunked(f.format, &body, &[k]);
            assert_eq!(
                got, expected,
                "{}: garbage-interleaved body split at {k} recovered differently",
                f.name
            );
        }
    }
}

fn with_garbage(body: &str) -> String {
    const JUNK: [&str; 4] = ["!!! not a subtitle line !!!", "00:00:!!broken", "[[[", "{{{unclosed"];
    let mut out = String::new();
    for (i, line) in body.split('\n').enumerate() {
        out.push_str(JUNK[i % JUNK.len()]);
        out.push('\n');
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[test]
fn pure_garbage_never_wedges_the_consumed_offset() {
    let garbage: String = (0..200)
        .map(|i| format!("garbage line {i} with some ünicode and <tags> and [brackets]\n"))
        .collect();

    for format in Format::ALL {
        let mut parser = format.parser();
        let mut buf = String::new();
        let mut total = 0usize;
        let bound = longest_line(&garbage);

        let boundaries: Vec<usize> = (0..=garbage.len())
            .step_by(7)
            .map(|mut i| {
                while i < garbage.len() && !garbage.is_char_boundary(i) {
                    i += 1;
                }
                i
            })
            .chain(std::iter::once(garbage.len()))
            .collect();

        let mut prev = 0;
        for &end in &boundaries {
            buf.push_str(&garbage[prev..end]);
            prev = end;
            let parsed = parser.parse_incremental(&buf, &ctx(), false).unwrap();
            buf.drain(..parsed.consumed);
            total += parsed.consumed;
            assert!(
                buf.len() <= bound,
                "{format:?} retained {} bytes of garbage (bound {bound}): \
                 the consumed offset stalled",
                buf.len()
            );
        }

        let parsed = parser.parse_incremental(&buf, &ctx(), true).unwrap();
        total += parsed.consumed;
        assert_eq!(total, garbage.len(), "{format:?} did not consume all of a pure-garbage stream");
    }
}

struct Rng(u64);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    fn splits(&mut self, body: &str, max_step: usize) -> Vec<usize> {
        let mut splits = Vec::new();
        let mut pos = 0usize;
        while pos < body.len() && splits.len() < 4 * body.len() + 16 {
            let step = self.next_u32() as usize % (max_step + 1);
            pos = (pos + step).min(body.len());
            while pos < body.len() && !body.is_char_boundary(pos) {
                pos += 1;
            }
            splits.push(pos);
        }
        splits
    }
}

fn big_body(body: &str) -> String {
    let mut s = String::new();
    for _ in 0..20 {
        s.push_str(body);
        if !s.ends_with('\n') {
            s.push('\n');
        }
    }
    s
}

#[test]
fn random_chunkings_match_whole() {
    const SEED: u64 = 0x5EED_1234_ABCD_9876;

    for f in fixtures() {
        let body = big_body(f.body);
        let expected = parse_whole(f.format, &body);
        let mut rng = Rng(SEED ^ f.name.len() as u64);

        for round in 0..12 {
            let max_step = [1usize, 2, 3, 5, 13, 64, 512][round % 7];
            let splits = rng.splits(&body, max_step);
            let got = parse_chunked(f.format, &body, &splits);
            assert_eq!(
                got, expected,
                "{}: random chunking round {round} (seed {SEED:#x}, max_step \
                 {max_step}) disagreed with the whole-body parse",
                f.name
            );
        }
    }
}

#[test]
fn named_adversarial_splits() {
    let cases: Vec<(Format, &str, &str, &str)> = vec![
        (
            Format::SubRip,
            "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n2\n00:00:03,000 --> 00:00:04,000\nTwo\n\n",
            " --> ",
            "inside the SubRip arrow",
        ),
        (
            Format::SubRip,
            "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n2\n00:00:03,000 --> 00:00:04,000\nTwo\n\n",
            "\n\n",
            "on the blank line between cues",
        ),
        (
            Format::SubRip,
            "\u{feff}1\n00:00:01,000 --> 00:00:02,000\nOne\n\n",
            "\u{feff}",
            "around the BOM",
        ),
        (
            Format::WebVtt,
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n",
            "WEBVTT",
            "inside the WEBVTT header",
        ),
        (
            Format::Ssa,
            "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, \
             MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,A\n\
             Dialogue: 0,0:00:04.00,0:00:05.00,Default,,0,0,0,,B\n",
            "Format: Layer, Start, End",
            "inside the SSA Format header",
        ),
        (
            Format::SubViewer,
            "[INFORMATION]\n[TITLE]t\n[END INFORMATION]\n00:00:01.00,00:00:02.00\nA\n\n\
             00:00:03.00,00:00:04.00\nB\n\n",
            "[INFORMATION]",
            "inside the SubViewer INFORMATION header",
        ),
        (
            Format::MpSub,
            "FORMAT=TIME\n2.0 3.0\nA\n\n1.0 2.0\nB\n\n",
            "FORMAT=TIME",
            "inside the MPSub FORMAT header",
        ),
        (
            Format::MicroDvd,
            "{1}{1}25.000\n{25}{50}A\n{100}{200}B\n",
            "{1}{1}25.000",
            "inside the MicroDVD fps header",
        ),
        (Format::Lrc, "[00:01.00]one\n[00:02.34]two\n", "[00:01.00]", "inside an LRC timestamp"),
        (
            Format::TmPlayer,
            "00:00:10,1=A\n00:00:10,2=B\n00:00:13,1=\n00:00:14,1=C\n00:00:16,1=\n",
            "00:00:13,1=",
            "inside a TMPlayer timestamp",
        ),
    ];

    for (format, body, marker, what) in cases {
        let expected = parse_whole(format, body);
        let mut found = 0;
        let mut from = 0;
        while let Some(rel) = body[from..].find(marker) {
            let at = from + rel;
            found += 1;
            for offset in 0..=marker.len() {
                let mut k = at + offset;
                while k < body.len() && !body.is_char_boundary(k) {
                    k += 1;
                }
                let got = parse_chunked(format, body, &[k]);
                assert_eq!(got, expected, "{format:?}: split {what} (byte {k}) changed the output");
            }
            from = at + marker.len();
        }
        assert!(found > 0, "{format:?}: marker {marker:?} not in the body");
    }
}

#[test]
fn split_inside_a_multi_digit_cue_number() {
    {
        let body = "12345\n00:00:01,000 --> 00:00:02,000\nOne\n\n\
                    12346\n00:00:03,000 --> 00:00:04,000\nTwo\n\n";
        let expected = parse_whole(Format::SubRip, body);
        assert_eq!(expected.len(), 2);
        for k in 0..=5 {
            assert_eq!(
                parse_chunked(Format::SubRip, body, &[k]),
                expected,
                "split inside the cue number at byte {k} changed the output"
            );
        }
    }
}
