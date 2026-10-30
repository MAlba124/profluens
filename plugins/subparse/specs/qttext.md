# QTtext (QuickTime Text) format reference

## Authoritative sources

QTtext is Apple's legacy QuickTime text-track sidecar format. Apple's original
tutorials are the nearest thing to a spec. The pages have been retired, so cite
archived copies:

- QuickTime "Text Tracks" tutorial (legacy):
  `http://www.apple.com/quicktime/tutorials/texttracks.html`
  (archived: `https://web.archive.org/web/*/apple.com/quicktime/tutorials/texttracks.html`)
- QuickTime "Text Descriptors" tutorial (legacy):
  `http://www.apple.com/quicktime/tutorials/textdescriptors.html`
  (archived: `https://web.archive.org/web/*/apple.com/quicktime/tutorials/textdescriptors.html`)

There is no formal grammar. The de-facto reference for the subset GStreamer
understands is the C parser:

- `gst-plugins-base/gst/subparse/qttextparse.c` has `parse_qttext`,
  `qttext_parse_tag`, `qttext_parse_timestamp`, and the `GstQTTextContext` state.
- `gst-plugins-base/gst/subparse/gstsubparse.c` is the line-feeding driver
  (`handle_buffer`, `get_next_line`) and the EOS `"\n\n"` flush.
- Detection: `gstsubparseelement.c` selects QTtext when the buffer contains the
  literal `{QTtext}`.

No copyrighted spec text is reproduced here, only our own distilled notes.

## Distilled grammar (the subset we parse)

A body is a sequence of `\n`-separated lines. Each line is scanned left to right.
The first meaningful character decides its kind:

- `{` starts a **descriptor** `{name[:value]}`. Multiple descriptors may appear
  on one line and may be followed by text. An unterminated `{...}` (no `}`)
  aborts the rest of that line.
- `[` starts a **timestamp** `[HH:MM:SS.dec]`. It closes any pending text as a
  cue and is assumed to be the only thing on its line.
- space / tab before any text are skipped (leading indentation is dropped).
- anything else is **text**: the remainder of the line, verbatim.

### Descriptors

| descriptor                    | effect                                                                |
|-------------------------------|-----------------------------------------------------------------------|
| `{QTtext}`                    | no-op marker (also the detection token)                               |
| `{font:NAME}`                 | set font family; enables markup                                       |
| `{size:N}`                    | set font size (N=0 or missing → 12); enables markup                   |
| `{textColor:r,g,b}`           | foreground; channels 0..65535, divided by 256 for Pango               |
| `{backColor:r,g,b}`           | background; a parse failure *clears* the background                   |
| `{plain}`/`{bold}`/`{italic}` | mutually exclusive style (never bold+italic together); enables markup |
| `{timescale:N}`               | ticks per second for the decimal field (N=0 or missing → 1000)        |
| `{timestamps:relative}`       | switch to relative timing (see quirk below)                           |

Name matching is a case-sensitive prefix match (as upstream's `strncmp`).

### Timestamps and timing

`[HH:MM:SS.dec]` parses like C's `sscanf("[%u:%u:%u.%u]")`:

- the decimal part is optional (`[HH:MM:SS]` → dec = 0);
- `dec` is scaled by `GST_SECOND / timescale` (truncating), so with the default
  timescale 1000, `.500` = 0.5 s but `.5` = 0.005 s;
- a malformed timestamp yields 0.

Text accumulates until the **next** timestamp flushes it. A cue therefore spans
`[previous_ts, this_ts)`:

- **absolute** (default): `duration = this_ts - previous_ts`, cue starts at
  `previous_ts`, and the clock jumps to `this_ts`;
- **relative**: `duration = this_ts` (the raw value is a delta), and the clock
  advances by `this_ts`.

A timestamp equal to 0 (bad, or literally `[00:00:00.00]`) still flushes pending
text but does **not** advance the clock.

**Deliberate divergence: a timestamp that goes backwards.** In absolute mode the
duration is `this_ts - previous_ts`, computed unsigned, so a timestamp *earlier*
than the pending block's start underflows in the C into a duration of some 584
years. The 0 a malformed timestamp reports lands in exactly that case whenever
the clock has moved. Both readings are garbage, but an end before the start is
not something a `Cue` may carry, so we emit an **open-ended** cue (`end_ns =
None`) there: unbounded in practice, like the C's, and ordered. See
`malformed_timestamp_after_text_yields_an_open_ended_cue` in the unit tests.

Because flushing only happens on the next timestamp, a trailing text block with
no following timestamp is never emitted. Upstream's EOS handler pushes `"\n\n"`,
but empty lines are no-ops for QTtext, so this does not rescue the last block.
Well-formed files end with a closing timestamp.

### Output

Output caps are `text/x-raw, format=pango-markup`. When any markup descriptor has
been seen (`need_markup`), every subsequent text line is wrapped in a
`<span ...>` opened from the current context, in attribute order:
`font` (always, `'NAME SIZE'` or `'SIZE'`), `bgcolor`, `color`, `weight='bold'`,
`style='italic'`. A previously open span is closed (`</span>`) before the next is
opened and when the cue is flushed. Multi-line cues thus repeat the span per
line, with the joining `\n` placed inside the closing span, e.g.
`<span ...>A\n</span><span ...>B</span>`. Cue text itself is appended verbatim
(no markup escaping), matching upstream.

## Known upstream quirks (reproduced for parity)

- **`{timestamps:absolute}` is treated as relative.** Upstream `string_match`
  compares a `strstr` result pointer against an `upto` bound. When "relative" is
  not found before the closing brace the `strstr` returns NULL (which sorts
  below `upto`), so the function reports a match and selects relative timing.
  Only an explicit `relative` (or the word appearing after the `}`) yields the
  intended result. Our port reproduces this behavior. See
  `timestamps_absolute_is_parsed_as_relative_quirk` in the unit tests.
- **`{fontsize:...}` matches the `font` prefix** (prefix matching), and other
  such prefix collisions follow from the same rule.

## Port notes

- Pure `std`, `#![forbid(unsafe_code)]` (crate-level). Timing is in nanoseconds.
- Byte-oriented lexer mirroring the C pointer arithmetic; slices are taken at
  ASCII delimiter boundaries so the (guaranteed valid) UTF-8 body stays valid.
- Unsigned arithmetic uses wrapping/checked ops to match C's overflow semantics
  without panicking. Timestamp overflow maps to `GST_CLOCK_TIME_NONE`.
