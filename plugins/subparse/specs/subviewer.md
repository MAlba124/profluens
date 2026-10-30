# SubViewer format reference

De-facto format; there is no authoritative spec. The GStreamer C parser is our
reference (`parse_subviewer` in `gst-plugins-base/gst/subparse/gstsubparse.c`).

- Rust parser: `crates/subparse-formats/src/formats/subviewer.rs`
- Output: `text/x-raw, format=utf8` (plain UTF-8, no markup).
- Background reading: <http://www.doom9.org/index.html?/sub.htm> (SUB/SubViewer
  notes), <https://en.wikipedia.org/wiki/SubViewer>.

## Shape

An optional `[INFORMATION]` / `[SUBTITLE]` header block, then a run of cues:

```
00:00:41.00,00:00:44.40
The Age of Gods was closing.
Eternity had come to an end.
<blank line>
```

- Timing line: `HH:MM:SS.mmm,HH:MM:SS.mmm`, start `,` end. Parsed as
  `sscanf("%u:%u:%u.%u,%u:%u:%u.%u")`. All eight fields must be present.
- Text: one or more lines, joined with `\n`, terminated by a blank line.
- `[br]` in the text becomes a newline.
- Trailing newlines in the assembled text are stripped (but at least one
  character is always kept).

## Quirks preserved from the C

- **Fraction is a literal millisecond count, not a decimal fraction.** `44.40`
  is 44 s + 40 ms. `11.91` is 11 s + 91 ms. `x.5` is 5 ms. Each `%u` also reads
  an unbounded digit run, so `.400` would be 400 ms.
- Header/metadata lines (`[INFORMATION]`, `[TITLE]...`, `[COLF]...`, `[SUBTITLE]`,
  …) are not special-cased. They simply fail the timing `sscanf` and are
  skipped. `[DELAY]` is **not** applied.
- A cue with no text lines before the blank terminator is emitted with empty
  text.

## Driver / end-of-input behavior

Like the C `get_next_line`, only `\n`-terminated lines are parsed. A lone `\r`
before `\n` is dropped. SubViewer is **not** in the set of formats flushed on
EOS, so a final cue whose terminating blank line is missing (no trailing
newline) is never emitted. The Rust parser mirrors this, ignoring the
unterminated remainder after the last `\n`.

## Cue mapping

`start_ns` = start time. `end_ns = Some(start + (end - start))`. Segment
clipping in the C is an element-level concern and is not part of the pure
parser.
