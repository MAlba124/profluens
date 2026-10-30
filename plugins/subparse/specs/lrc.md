# LRC format reference

De-facto lyrics format; no single authoritative spec. The GStreamer C parser is
our reference (`parse_lrc` in
`gst-plugins-base/gst/subparse/gstsubparse.c`).

- Rust parser: `crates/subparse-formats/src/formats/lrc.rs`
- Output: `text/x-raw, format=utf8` (plain UTF-8, no markup).
- Background reading: <https://en.wikipedia.org/wiki/LRC_(file_format)>.

## Shape

One timed lyric per line:

```
[mm:ss.xx]lyric text
[mm:ss.xxx]another line
```

- A line must start with `[`.
- Time tag: `[%u:%02u.%03u]` or `[%u:%02u.%02u]`, minutes `:` seconds `.`
  fraction. The numeric match does **not** require the closing `]`, but a `]`
  must exist somewhere in the line (it delimits the text).
- Text: everything after the first `]`, verbatim.
- Each matching line emits one cue immediately (no buffering, no blank-line
  terminator).

## Quirks preserved from the C

- **Open-ended cues.** There is no end time. Upstream sets
  `duration = GST_CLOCK_TIME_NONE`, so the Rust cue has `end_ns = None`.
- **Fraction unit is decided by the byte offset of `]`.** In the canonical
  `[mm:ss.ff]` layout the `]` sits at index 9 for a two-digit fraction, which is
  read as centiseconds (`x10` ms). Any other offset is treated as milliseconds
  (`x1`). This is purely positional, so a one-digit minute (`[1:23.45]`, `]` at
  index 8) is treated as milliseconds even with a two-digit fraction.
- ID3-style metadata lines (`[ti:...]`, `[ar:...]`) and any line whose numeric
  fields do not parse are skipped.
- A line whose numbers parse but that has no `]` at all is skipped.

## Driver / end-of-input behavior

Only `\n`-terminated lines are parsed (`\r` before `\n` dropped). LRC is not
flushed on EOS, so the unterminated remainder after the last `\n` is ignored
(a final lyric line without a trailing newline is dropped).

## Cue mapping

`start_ns` = tag time. `end_ns = None`. The element decides the effective end
(typically the next cue's start).
