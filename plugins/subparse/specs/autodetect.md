# autodetect format sniffer reference

Reference implementation: `gst_sub_parse_data_format_autodetect` and
`gst_sub_parse_data_format_autodetect_regex_once` in
`gst-plugins-base/gst/subparse/gstsubparseelement.c` (in the monorepo checkout).
The media-type mapping lives in `gst_sub_parse_type_find` in the same file.

There is no authoritative spec for format sniffing. The C `subparse`
element **is** the reference. Detection must be byte-for-byte order-compatible
with it. The same bytes must yield the same format, because downstream caps
negotiation keys off the detected media type.

Rust port: `crates/subparse-formats/src/autodetect.rs`, `pub fn detect(&str) ->
Option<Format>`. The C uses `GRegex` (PCRE) for four probes and
`sscanf`/`strstr`/`strncmp` for the rest. We hand-write the equivalent matching
(the crate is dependency-free, no regex crate).

## Input assumptions

`detect` receives the UTF-8 body `gst_sub_parse_gst_convert_to_utf8` produces,
which **strips a leading UTF-8 BOM**. `detect` strips a stray leading `U+FEFF`
too so the byte-level probes line up.

Line terminators are **not** normalized. Nothing strips `\r` before detection,
in the C or here, and the probes are written for that. The visible consequence:
in a CRLF `.lrc` file every line ends in `\r`, which fails the LRC per-line check
(it wants the line to end in `]`), so the file is not detected as LRC. That is C
parity, and parity is what matters here, since the detected media type is what
downstream negotiates on.

How much of the body is handed over is the caller's business, and the C has two
different answers:

- the **element** sniffs the first **35** bytes, once at least 6 bytes have
  arrived: `data = g_strndup (self->textbuf->str, 35)` in
  `gst_sub_parse_format_autodetect`.
- the **typefinder** peeks up to **128** bytes (`data_len = 128` in
  `gst_sub_parse_type_find`) and runs the same cascade on those.

`detect` itself sniffs whatever it is given, so the truncation belongs to the
element crate.

## Detection order

The C is a top-to-bottom `if` cascade. **The first match wins**. This order is
authoritative (note it is *not* the order the formats happen to be listed in the
`Format` enum):

| #  | Format    | C test                                        | Our probe           |
|----|-----------|-----------------------------------------------|---------------------|
| 1  | MicroDVD  | regex `^\{[0-9]+\}\{[0-9]+\}`                 | `matches_microdvd`  |
| 2  | SubRip    | regex (see below)                             | `matches_subrip`    |
| 3  | DKS       | regex `^\[[0-9]+:[0-9]+:[0-9]+\].*`           | `matches_dks`       |
| 4  | WebVTT    | regex `^(\xef\xbb\xbf)?WEBVTT[\xa\xd\x20\x9]` | `matches_webvtt`    |
| 5  | MPSub     | `strncmp(str, "FORMAT=TIME", 11) == 0`        | `matches_mpsub`     |
| 6  | SAMI      | `strstr("<SAMI>")` \|\| `strstr("<sami>")`    | `matches_sami`      |
| 7  | TMPlayer  | 5× `sscanf` (see below)                       | `matches_tmplayer`  |
| 8  | MPL2      | `sscanf(str, "[%u][%u]") == 2`                | `matches_mpl2`      |
| 9  | SubViewer | `strstr("[INFORMATION]")`                     | `matches_subviewer` |
| 10 | QTtext    | `strstr("{QTtext}")`                          | `matches_qttext`    |
| 11 | LRC       | `str[0] == '['` + per-line check              | `matches_lrc`       |
| -  | (none)    | `GST_SUB_PARSE_FORMAT_UNKNOWN`                | `None`              |

Ordering matters wherever probes overlap. In particular several formats start
with `[` (DKS (3), MPL2 (8), SubViewer (9) and LRC (11)), and LRC is the
deliberate last-resort catch-all for a `[`-prefixed body.

## Per-format signatures

- **MicroDVD**, `{`, ≥1 digits, `}`, `{`, ≥1 digits, `}` at offset 0. Probed
  *before* QTtext, so `{QTtext}` (no digits after `{`) correctly falls through
  rather than being mis-typed as MicroDVD.

- **SubRip**, the regex is
  `^[\s\n]*[\n]? {0,3}[ 0-9]{1,4}\s*(\x0d)?\x0a` (the index-line preamble)
  followed by a timestamp line
  ` ?[0-9]{1,2}: ?[0-9]{1,2}: ?[0-9]{1,2}[,.] {0,2}[0-9]{1,3}`
  ` +--> +`
  `[0-9]{1,2}: ?[0-9]{1,2}: ?[0-9]{1,2}[,.] {0,2}[0-9]{1,2}`.
  The preamble, anchored at 0, consists only of whitespace/digit bytes and ends
  at a mandatory `\n`. We enumerate each candidate `\n` and check that the prefix
  before it reduces to `whitespace* · <1..=4 space/digit> · whitespace*` (every
  digit falls inside the `[ 0-9]{1,4}` block, since the surrounding `\s*` can't
  match digits), then match the timestamp line right after. This reproduces the
  regex engine's greedy `\s*` + backtrack without an actual regex engine.
  The timestamp match is deterministic. Every variable-width run (` ?`,
  ` {0,2}`, ` +`, `[0-9]{1,n}`) is followed by a distinct-class byte, so greedy
  matching needs no backtracking. Note the loose spacing the regex allows:
  optional space after each `:`, up to two spaces after the `[,.]`, e.g.
  `0: 0:26, 26 --> 0: 0:28, 17`.

- **DKS**, `[`, digits, `:`, digits, `:`, digits, `]` at offset 0 (the regex's
  trailing `.*` is unconditional). Three digit groups distinguish it from MPL2's
  `[n][n]`.

- **WebVTT**, optional UTF-8 BOM, then literal `WEBVTT`, then **one** byte from
  `{\n, \r, space, tab}`. The trailing byte is required, so `WEBVTT` at EOF or
  `WEBVTTX` is *not* WebVTT.

- **MPSub**, body starts with the literal `FORMAT=TIME` (`strncmp` of 11 bytes,
  so anything may follow).

- **SAMI**, body *contains* `<SAMI>` or `<sami>` anywhere. Case-sensitive and
  only these two spellings (mirrors the C `strstr` pair, so mixed case is not
  detected).

- **TMPlayer**, five `sscanf` formats: `0:%02u:%02u:`, `0:%02u:%02u=`,
  `00:%02u:%02u:`, `00:%02u:%02u=`, `00:%02u:%02u,%u=`. `sscanf` returns the
  number of *assigned* conversions, so the trailing literal after the last `%u`
  (`:`, `=`) does **not** affect the `== 2` / `== 3` test. That collapses the
  five to three meaningful checks: `0:` u `:` u, `00:` u `:` u, and
  `00:` u `:` u `,` u. The last is already subsumed by the `00:` check, so its
  branch is effectively dead in the C too. `%u` skips leading whitespace and
  `%02u` caps at two digits, both reproduced in `scan_uint`. The literal `0`/`00`
  hour encodes the C's "first subtitle within the first hour" assumption.

- **MPL2**, `sscanf(str, "[%u][%u]") == 2`: `[`, uint, `]`, `[`, uint. The final
  `]` is after the second `%u`, so it is irrelevant to the count (`[123][456 x]`
  detects), and each `%u` takes an optional sign (`[+123][456]` detects). The
  MPL2 parser accepts both shapes as well.

  A note on that sign: C's `%u` is `strtoul`-based, so it accepts `+`/`-`
  *everywhere*, including in the TMPlayer and LRC probes above. Those two stay
  unsigned here, deliberately, so that each probe keeps agreeing with its own
  parser (detecting a format whose parser then finds nothing is the worse of the
  two failures). It makes them narrower than the C for a signed timestamp field,
  which no real file has.

- **SubViewer**, body *contains* `[INFORMATION]`.

- **QTtext**, body *contains* `{QTtext}`.

- **LRC**, body starts with `[`, and **every line except the last** is
  "LRC-good". The C loops `while (*ptr && *(ptr+1))` over a
  `g_strsplit(str, "\n")`, which visits all elements but the final one.
  `all_lines_good` is initialized `TRUE`, so a `[`-prefixed body with no
  newline (single element) already qualifies. LRC is the catch-all for any
  otherwise-unrecognized `[`-prefixed body. A line is LRC-good when it starts
  with an LRC timestamp, matched by `sscanf` `[%u:%02u.%02u]` or
  `[%u:%02u.%03u]` returning `== 3` (the closing `]` sits after the third `%u`,
  so a lyric may follow the timestamp), **or** when it is non-empty, ends with
  `]`, and contains a `:` (metadata lines like `[ar:Artist]`).

## Media-type mapping

From `gst_sub_parse_type_find`. `Format::media_type()` in `format.rs` already
encodes this:

| Format                             | caps                              |
|------------------------------------|-----------------------------------|
| MicroDVD, SubRip, MPSub, SubViewer | `application/x-subtitle`          |
| SAMI                               | `application/x-subtitle-sami`     |
| TMPlayer                           | `application/x-subtitle-tmplayer` |
| MPL2                               | `application/x-subtitle-mpl2`     |
| DKS                                | `application/x-subtitle-dks`      |
| QTtext                             | `application/x-subtitle-qttext`   |
| LRC                                | `application/x-subtitle-lrc`      |
| WebVTT                             | `application/x-subtitle-vtt`      |

(The typefinder registers itself at `GST_RANK_MARGINAL` for the extensions
`srt,sub,mpsub,mdvd,smi,txt,dks,vtt`.)
