# TMPlayer format reference

De-facto format; there is no authoritative published spec. Our reference is the
GStreamer C parser and the forum description it cites:

- C reference: `gst-plugins-base/gst/subparse/tmplayerparse.c`, `tmplayer_parse_line`.
- Rust port: `crates/subparse-formats/src/formats/tmplayer.rs`.
- Original description:
  <http://forum.doom9.org/archive/index.php/t-81059.html>.

## Line grammars

Output is `text/x-raw, format=utf8` (plain text: no markup, no escaping).
Timestamps are `HH:MM:SS` (hours 1 to 2 digits, minutes/seconds up to 2 digits).
Two shapes are recognized:

- **single line**: `HH:MM:SS<sep>TEXT`, where `<sep>` is `:` or `=`.
- **multiline**:   `HH:MM:SS,<N>=TEXT`, where `<N>` is a 1-based part index.

`|` inside `TEXT` is a hard line break (becomes `\n`). A line matching neither
shape (and not an empty line) is skipped (comments, blank-ish lines).

Both shapes are matched with `sscanf`, so each numeric field skips leading
whitespace: `00: 0:10:Hello` is a valid TMPlayer line (and autodetect, also
`sscanf`-based, agrees).

### Where the text starts (a C quirk worth knowing)

The C does not take the text from where its scan stopped. It searches for the
separator again:

- multiline: `strchr (line, '=')`, which can only be the separator just matched.
- single line: `strchr (line + 6, divc)`, i.e. the **first** separator at or
  after byte 6. With a one- or two-digit hour that is the separator just
  matched, but a longer hour field pushes the timestamp past byte 6, so
  `100:00:10:text` has the text `10:text`. And when the separator sits entirely
  before byte 6, as in `0:0:1:text`, the C finds none at all and treats the line
  as text-less (`text_start == NULL`), i.e. `0:0:1:text` contributes nothing.

The port reproduces both, since a drop-in replacement has to place the text
where the C places it.

## Timing (deduced, not stored)

A cue carries **only a start time**. Its duration is the gap to the *next*
timestamp. The parser therefore buffers text until the next timestamped line
(or an empty line / end-of-stream) closes the current cue:

```
duration = next_start - current_start
end_ns   = current_start + min(duration, MAX_DURATION)     // MAX_DURATION = 5 s
```

Key rules (all mirrored from the C):

- A timestamped line **with text** either starts a new cue or, for multiline
  parts `N > 1`, appends a `\n` + its text to the current cue.
- A timestamped line **without text** (e.g. `00:00:13:`) closes the current cue,
  giving it the duration up to that timestamp.
- The running start advances by the **unclamped** duration, so consecutive
  starts stay exact even when a reported duration was clamped to 5 s.
- **Duration clamp**: the element sets `max_duration = 5 * GST_SECOND` for
  TMPlayer, so any deduced duration longer than 5 s is capped in the output
  (only the displayed `end`; the internal start still advances by the full gap).
- **End of stream**: any text still buffered is flushed as a final,
  **open-ended** cue (`end_ns = None`). This mirrors the `"\n\n"` the element
  injects at EOS to force the last chunk out.

## Varieties (all supported)

`00:00:50:…` · `0:00:50:…` · `00:00:50=…` · `0:00:50=…` · `00:00:50,1=…` and the
"no empty lines" variant where each timestamp both closes the previous cue and
opens the next. See the C header comment for the canonical examples.
