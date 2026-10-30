# MPSub format reference

De-facto format (MPlayer subtitles); no authoritative spec. The GStreamer C
parser is our reference (`parse_mpsub` in
`gst-plugins-base/gst/subparse/gstsubparse.c`).

- Rust parser: `crates/subparse-formats/src/formats/mpsub.rs`
- Output: `text/x-raw, format=utf8` (plain UTF-8, no markup).
- Background reading: MPlayer `sub` / MPsub documentation, e.g.
  <https://www.mplayerhq.hu/DOCS/HTML/en/subosd.html>.

## Shape

A header (at least `FORMAT=TIME`) followed by relatively-timed cues:

```
FORMAT=TIME

2.0 3.0
Hello

1.5 2.0
World
```

- Timing line: two floats, `<offset> <duration>` in seconds
  (`sscanf("%f %f")`). Both must parse.
- Text: one or more lines joined with `\n`, terminated by a blank line.

## Relative timing

Times accumulate. The offset is measured from the **end of the previous cue**,
which `parse_mpsub` accounts for when it opens a cue (gstsubparse.c:1344):

```
start += previous_duration + offset
duration = this_duration
```

Starting from `start = 0`, `duration = 0`.

### The duration is counted twice (upstream quirk, reproduced)

After the element pushes a finished cue it advances the same running start by
the same duration once more (gstsubparse.c:1848, `/* move this forward (the
tmplayer parser needs this) */`). Nothing resets it in between, so the next
timing line adds that duration on top of it again:

```
start = previous_start + previous_duration          (the driver, after the push)
start += previous_duration + offset                 (the parser, next cue)
```

Every cue after the first therefore starts one previous-duration late, and the
error accumulates over the file. `2.0 3.0` then `1.0 2.0` then `1.0 2.0` gives
cue starts of 2 s, 9 s and 14 s, where the format's own reading (offset from the
previous end, counted once) would give 2 s, 6 s and 9 s.

**We reproduce the C.** This crate is a drop-in replacement for the C element,
so the same bytes must produce the same timestamps, down to the `f32` rounding
below. And the C parser is the only reference this de-facto format has here:
"fixing" the arithmetic would silently retime every multi-cue MPSub file
relative to `subparse`. `c_parity_duration_is_counted_twice_per_cue` in
`mpsub.rs` pins it.

(The `f32` rounding makes the exact figures 2 s, 8.999999488 s and
13.999998976 s.)

Only `FORMAT=TIME` is handled here (autodetect keys on the `FORMAT=TIME`
prefix). Other `FORMAT=` units are not supported by the C and are treated as
header noise.

## Quirks preserved from the C

- **`float` arithmetic.** Upstream keeps the running time as C `float`, so the
  Rust parser does the accumulation in `f32` to reproduce the same rounding.
  (`GST_SECOND` = 1e9 is exactly representable as `f32`.)
- **No `[br]` unescaping** (unlike SubViewer/DKS).
- **No trailing-newline stripping.** The blank terminator appends a `\n` to the
  buffer that is never removed, so a cue's text ends with `\n` (e.g. `"Hello\n"`).
- A line with only one float is not a timing line (sscanf returns 1). It is
  skipped in the timing state.

## Driver / end-of-input behavior

Only `\n`-terminated lines are parsed (`\r` before `\n` dropped). MPSub is not
flushed on EOS, so a final cue lacking its terminating blank line is never
emitted. The Rust parser ignores the unterminated remainder after the last
`\n`.

## Cue mapping

`start_ns` = accumulated start. `end_ns = Some(start + duration)`. Segment
clipping is left to the element crate.
