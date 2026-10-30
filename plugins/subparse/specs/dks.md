# DKS format reference

De-facto format; no authoritative spec. The GStreamer C parser is our reference
(`parse_dks` in `gst-plugins-base/gst/subparse/gstsubparse.c`).

- Rust parser: `crates/subparse-formats/src/formats/dks.rs`
- Output: `text/x-raw, format=utf8` (plain UTF-8, no markup).
- Background reading: the format is only documented by the `subparse`
  implementation itself. Treat the C parser as the spec.

## Shape

A two-line-per-cue format:

```
[00:00:07]THERE IS A PLACE ON EARTH WHERE IT[br]IS STILL THE MORNING OF LIFE...
[00:00:12]
```

- Start line: `[HH:MM:SS]<text>`, timestamp then payload.
- End line: `[HH:MM:SS]` (usually blank after the `]`), which supplies the end
  time and flushes the cue.
- `[br]` in the payload becomes a newline (`unescape_newlines_br`).

Time tags are parsed as `sscanf("[%u:%u:%u]")`. Each field is an unbounded digit
run, and the closing `]` is not required for the numeric match. The payload is
everything after the first `]`.

## State machine (from the C)

- **State 0 (expect start):** on a valid timestamp, set the start time and take
  the text after `]`. Only if that text is **non-empty** does it open a cue and
  move to state 1. A bare `[HH:MM:SS]` merely updates the pending start time.
- **State 1 (expect end):** the next valid timestamp sets the end time and emits
  the cue (`start`, `end`). A line that is not a timestamp is dropped (the C
  logs a warning and stays in state 1).

## Quirks preserved from the C

- Only the start line contributes text. Multi-line cues rely on `[br]`.
- No trailing-newline stripping is applied to DKS text (unlike SubViewer).
- Payloads shorter than four bytes are returned unchanged (the `[br]` scan's
  length guard), which cannot contain `[br]` anyway.

## Driver / end-of-input behavior

Only `\n`-terminated lines are parsed (`\r` before `\n` dropped). DKS is not
flushed on EOS, so a pending cue whose end-time line is missing (no trailing
newline) is never emitted. The Rust parser ignores the unterminated remainder.

## Cue mapping

`start_ns` = start tag. `end_ns = Some(start + (end - start))`, with a saturating
subtraction so an end-before-start line cannot panic (it clamps the duration to
zero). Segment clipping is an element-level concern.
