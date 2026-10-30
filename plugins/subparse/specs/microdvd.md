# MicroDVD (`.sub`) format reference

De-facto format; there is no authoritative published spec. The GStreamer C
parser is our reference:

- C reference: `gst-plugins-base/gst/subparse/gstsubparse.c`, `parse_mdvdsub`.
- Rust port: `crates/subparse-formats/src/formats/microdvd.rs`.
- Background: <https://en.wikipedia.org/wiki/MicroDVD> (format history / player).

## Line grammar

Each subtitle is exactly one line:

```
{START_FRAME}{END_FRAME}TEXT
```

`START_FRAME` / `END_FRAME` are **frame numbers** (unsigned integers), not times.
A line that does not match the `{u}{u}` prefix is skipped (lenient recovery).

`TEXT` may contain `|` as a hard line break and the inline style codes below.

## Timing (frame-based)

MicroDVD stores frames, so wall-clock timing depends on the video frame rate:

```
start_ns = start_frame                 * 1e9 * fps_den / fps_num   (truncated)
dur_ns   = (end_frame - start_frame)   * 1e9 * fps_den / fps_num   (truncated)
end_ns   = start_ns + dur_ns
```

Scaling truncates toward zero, matching `gst_util_uint64_scale`, and saturates at
`u64::MAX` where that function returns `G_MAXUINT64` (an absurd frame number must
not wrap into a small, plausible-looking timestamp).

### Frame rate

- Default when the caller supplies none: **`24000/1001`** (~23.976 fps), the
  upstream default (`gstsubparse.c` sets `fps_n = 24000; fps_d = 1001`). Exposed
  here as `ParseContext.fps` (`None` ⇒ default).
- A leading **`{1}{1}<fps>`** line overrides it in-band. Frame `1→1` is never a
  real cue, so this line emits nothing. `<fps>` accepts `.` or `,` as the decimal
  separator and is only honored when `0.001 ≤ fps ≤ 1000.0`.
  - The C runs `g_ascii_strtod` then `gst_util_double_to_fraction`. We instead
    take the **exact rational** of the decimal literal (reduced by gcd), which
    equals GLib's result for the terminating decimals real headers use
    (`25.000 → 25/1`, `12.500 → 25/2`) and avoids float round-trips.
  - The accepted syntax is `g_ascii_strtod`'s (C `strtod` in the C locale):
    optional whitespace, optional sign, digits with an optional fractional part,
    optional decimal exponent (`1e2` = 100 fps). Two things `strtod` also takes
    are left out on purpose: hexadecimal floats (`0x19p0`), which no header uses,
    and `inf`/`nan`, which the range check above rejects anyway.

## Inline style codes → Pango markup

Output is `text/x-raw, format=pango-markup`. Each visual line (split on `|`)
becomes a `<span ...>...</span>`. The text is GLib-`g_markup_escape_text`-escaped
(`&`,`<`,`>` → named entities, `'`→`&apos;`, `"`→`&quot;`, C0 controls → `&#xNN;`).

Per line, at its start, in this order (each checked **once**):

| code        | effect                              | span attribute            |
|-------------|-------------------------------------|---------------------------|
| `{y:i}`     | italic                              | `style="italic"`          |
| `{y:b}`     | bold                                | `weight="bold"`           |
| `{s:NN}`    | font size NN                        | `size="{NN*1000}"`        |
| leading `/` | italic                              | `style="italic"`          |

A **trailing `/`** on a chunk is dropped (stray end-italics marker). A `{s:NN}`
without a closing `}` makes the C abandon the line, so we emit no cue for it.
`{s:NN}` is read with `sscanf("{s:%u}")`, so whitespace before the digits is
skipped: `{s: 20}` is a size code too.

Escaping is `g_markup_escape_text`'s: the five XML metacharacters as named
references, and `&#xNN;` for the controls GLib escapes, which are `0x1..=0x8`,
`0xb..=0xc`, `0xe..=0x1f`, `0x7f..=0x84` and `0x86..=0x9f`. `0x85` is not one of
them.

Example: `{100}{200}/italics/|not italics` (at 25 fps) →
`<span style="italic">italics</span>` + newline + `<span>not italics</span>`,
spanning 4 s .. 8 s.

## Driver / end-of-input behavior

Only `\n`-terminated lines are parsed (a `\r` before the `\n` is dropped). The
element force-feeds a `"\n\n"` at EOS to flush a trailing record, but only for
SubRip, TMPlayer, MPL2, QTtext and WebVTT (`gst_sub_parse_sink_event`).
**MicroDVD is not on that list**, so a final line without its newline is lost.
We reproduce that: it is what a drop-in replacement owes, and LRC, DKS, SubViewer
and MPSub drop their tail for the same reason. `unterminated_final_line_is_dropped`
in `microdvd.rs` pins it.
