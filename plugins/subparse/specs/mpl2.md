# MPL2 format reference

De-facto format; there is no authoritative published spec. Our reference is the
GStreamer C parser and the original MPlayer mailing-list description it cites:

- C reference: `gst-plugins-base/gst/subparse/mpl2parse.c`, `mpl2_parse_line`.
- Rust port: `crates/subparse-formats/src/formats/mpl2.rs`.
- Original description:
  <http://lists.mplayerhq.hu/pipermail/mplayer-users/2003-February/030222.html>.

## Line grammar

Each subtitle is one line:

```
[START][END]TEXT
```

`START` / `END` are integers in **deciseconds** (tenths of a second). The space
between `]` and `TEXT` is optional. Lines without a valid `[u][u]` prefix are
skipped. `TEXT` starts after the **second** `]` in the line.

The prefix test is `sscanf(line, "[%u][%u]") == 2`, and that count is what
decides, so the C is looser than the literal shape suggests:

- The `]` closing the **second** bracket comes after the last conversion, so its
  absence cannot change the count: `[123][456 x]y` is a record (12.3 s .. 45.6 s,
  text `y`, found by the two `strchr(line, ']')` steps, which are independent of
  the scan). The `]` closing the **first** bracket is a literal *between* the two
  conversions, so it is required: `[123 x][456]y` scans one number and is skipped.
- Each `%u` skips leading whitespace and takes an optional sign, so `[+123][456]`
  is a record (autodetect agrees, being the same `sscanf`). A negative value wraps
  into a garbage timestamp in the C, since it lands in a `gint` that is then
  multiplied out as unsigned. We read it as 0 instead, which keeps a cue's end at
  or after its start.

## Timing (deciseconds)

```
start_ns = START * (1e9 / 10)   = START * 100_000_000
end_ns   = END   * (1e9 / 10)   = END   * 100_000_000
```

## Text → Pango markup

Output is `text/x-raw, format=pango-markup`.

- `|` separates visual lines (becomes `\n`).
- A leading `/` on a line marks it italic, wrapping that line in `<i>...</i>`.
  (Only a *leading* `/` per line, with no trailing-`/` handling, unlike
  MicroDVD.)
- Leading spaces/tabs of each visual line are dropped before the `/` check.
- Text is GLib-`g_markup_escape_text`-escaped (`&`,`<`,`>` → named entities,
  `'`→`&apos;`, `"`→`&quot;`, and `&#xNN;` for the controls GLib escapes:
  `0x1..=0x8`, `0xb..=0xc`, `0xe..=0x1f`, `0x7f..=0x84`, `0x86..=0x9f`, so not
  `0x85`).
- The assembled markup is finally whitespace-stripped at both ends
  (`g_strstrip`), so a trailing space or a trailing empty `|` chunk disappears.

Examples:

| input                     | output                          |
|---------------------------|---------------------------------|
| `[123][456] a\|b`         | `a\nb` (12.3 s .. 45.6 s)       |
| `[1][2] /Italic\|Normal`  | `<i>Italic</i>\nNormal`         |
| `[1][2]/Italic\|/Italic`  | `<i>Italic</i>\n<i>Italic</i>`  |
