# SubRip (`.srt`) format reference

## Authoritative source

There is **no formal SubRip specification**. SubRip is a de-facto format defined
by what players accept and, for GStreamer, by what the upstream `subparse`
plugin produces. The reference implementation is therefore:

- `gst-plugins-base/gst/subparse/gstsubparse.c`, `parse_subrip` and its helpers
  (`parse_subrip_time`, `subrip_unescape_formatting`,
  `subrip_remove_unhandled_tags`, `strip_trailing_newlines`,
  `subrip_fix_up_markup`).

Our parser (`crates/subparse-formats/src/formats/subrip.rs`) is a port of that
code. Where behavior is ambiguous, the C wins (validated by unit tests mirroring
`gst-plugins-base/tests/check/elements/subparse.c`).

## Structure

A file is a sequence of cues separated by blank lines. Each cue is:

```
<id>
<start> --> <end>
<text line 1>
<text line 2 ...>
<blank line>
```

- **id**, an integer. The value is not used. It only marks the start of a cue.
  Chunk numbering need not start at 1 and gaps are ignored.
- **timestamps**, see below.
- **text**, one or more lines, joined with `\n`. May contain light inline
  markup (`<i> <b> <u>`). Terminated by a blank line (or end of input).

Lines use `\n`. A trailing `\r` (CRLF input) is stripped per line.

## Timestamps

Format `HH:MM:SS,mmm` (the reference also accepts `MM:SS,mmm`, treating hours as
0, a WebVTT convention that `parse_subrip_time` shares). Tolerances, all
mirrored from the C:

- `.` is accepted in place of `,` for the sub-second separator.
- Spaces inside the timestamp are treated as `0`
  (e.g. ` 0: 0:26, 26` → `0:00:26,026`).
- Leading spaces are skipped. Trailing whitespace is trimmed.
- The sub-second field is normalized to exactly **3 digits** by right-padding
  with `0` (so `,5` = 500 ms, `,05` = 50 ms, `,500` = 500 ms) or truncating to 3.
- A `,` (or `.`) **must** be present, and must occur within the first 11 bytes
  of the munged string (`sizeof("hhh:mm:ss,")`), else the timestamp is rejected.

### Field parsing (`sscanf "%u:%u:%u,%u"`)

The C reads the munged string with one `sscanf`, falling back to `"%u:%u,%u"`
(hours = 0). That gives each field these semantics, all mirrored:

- `%u` **skips leading whitespace**, so a tab survives where a space would have
  become a `0` (`\t00:00:01,000` and `00:\t0:01,000` both parse).
- `%u` then defers to `strtoul`: an optional sign, then a decimal run. `+1` is
  `1`, and `-1` wraps into the unsigned field.
- Between fields the format has a literal (`:` or `,`), so a field must be
  **fully** consumed. `00:00:01x,000` is **rejected** (the `x` is matched
  against the `,`) and the cue is dropped.
- The sub-second field is the last conversion, so nothing has to follow it and
  junk after its digits is simply not read. This is what keeps a short fraction
  followed by WebVTT cue settings alive: `,5 A:start` munges to `,50A:start`,
  truncates to `50A`, and yields 50 ms.
- Fields land in the C's `guint`. Overflow past `ULONG_MAX` yields `ULONG_MAX`
  (`ERANGE`) and the store truncates to 32 bits.

Result is nanoseconds: `((h*3600 + m*60 + s) * 1e9) + ms * 1e6`, where the
seconds sum is computed in **32 bits** like the C's `guint` arithmetic. An
absurd field therefore wraps instead of saturating into the far future
(`99999999:00,000` is 1705032644 s, `71582789:00,000` is 44 s). This matters
because a far-future cue would suppress every cue after it via the guard below.

The time line must contain the literal `" --> "` (space-dash-dash-dash-space).
The start is parsed from the whole line (truncated at the first `-->`). The end
comes from the remainder after `" --> "`.

### Ordering guard

The reference keeps the previous cue's end time and rejects a time line whose
end is **before** it (`prev_end <= ts_end`), dropping that cue. Initialized to 0,
so the first cue always passes. (In the element the value is `start_time`, which
each push advances by the cue's duration, i.e. back to the parsed end.)

A **reversed** time line (`00:00:05,000 --> 00:00:02,000`) passes that guard and
leaves the C with an underflowed duration. We emit the cue with its end clamped
up to its start, honouring the `end_ns >= start_ns` invariant `Cue` documents,
and keep the **parsed** end for the guard so the following cue is still accepted
exactly as upstream.

## Text → Pango markup

`output_format()` is `PangoMarkup`. The reference pipeline, applied to the
joined text buffer, is (in order):

1. **Escape** everything with `g_markup_escape_text`
   (`& < > ' "` → `&amp; &lt; &gt; &apos; &quot;`, the named references current
   GLib emits, plus stray control characters → `&#xNN;`).
2. **Un-escape a whitelist** (`subrip_unescape_formatting`): turn escaped
   `&lt;tag&gt;` back into real `<tag>` for **`i`, `b`, `u` only**
   (case-sensitive, exact name match). SubRip does **not** allow tag
   attributes, so any attributes (e.g. `<b.loud>` → `<b>`) are dropped.
   Non-whitelisted tags stay escaped.
3. **Drop unknown tags** (`subrip_remove_unhandled_tags`): remove any leftover
   escaped `&lt;...&gt;` whose name begins with an ASCII letter
   (e.g. `<font ...>`, `<v>`, `<ruby>`). A `&lt;` not followed by a letter is
   left intact (so a literal `<5` survives as `&lt;5`). The C looks for the
   closing `&gt;` from every escaped `<`, i.e. it rescans to the end of the
   buffer per `&lt;`. We scan once with a forward-only cursor instead, since a
   cue's length is bounded by nothing (see `tests/tag_scan_linearity.rs`).
4. **Strip trailing newlines** (`strip_trailing_newlines`), keeping ≥ 1 byte.
5. **Balance** (`subrip_fix_up_markup`): append missing closing tags (innermost
   first) and remove stray closing tags that were never opened.

Consequences worth noting (all covered by tests):

- `<i>Seven` → `<i>Seven</i>`; `<b><i>Eight` → `<b><i>Eight</i></b>`.
- `</b>` alone → `` (empty); `<i>xyz</b>` → `<i>xyz</i>`.
- `gave <i>Rock & Roll</i> to` → `gave <i>Rock &amp; Roll</i> to`.
- `<i>italics</ i>` → `<i>italics</i>` (whitespace inside a closing tag is
  tolerated). `<i>italics</ x>` → `<i>italics&lt;/ x&gt;</i>`.
- Apostrophes/quotes in plain text are escaped (`don't` → `don&apos;t`).

## Leniency / recovery

Malformed cues are skipped and parsing continues. A non-numeric line where an id
is expected, or a bad time line, drops the state machine back to "expect id".
The last cue is emitted even without a trailing blank line (the element injects
`"\n\n"` at EOS, and the parser appends one synthetic blank line to the same
effect).

## Styling (cue-ir only)

The pipeline above is lossy: only `<i>/<b>/<u>` survive it, so `<font
color="...">` — the one styling tag real SRT files actually use — is deleted
from the pango output, and `{\an8}`-style override blocks (an SSA-ism every
player honours) are shown literally. For `text-format=cue-ir` the parser
keeps the cue's **source text** on `Cue.raw_text`, and
`CueIr::from_srt_text` re-parses it into styled spans:

- `<i>/<b>/<u>/<s>` and `<font color|face|size>` (attribute values quoted or
  bare; colors named or `#hex`; sizes the legacy HTML `1..7` ladder, `+n/-n`
  relative). Matching is lenient — case-insensitive names, unclosed tags
  close at the cue's end, stray closers ignored.
- Tag-vs-literal classification mirrors the C's visible-text behaviour:
  unknown letter-initiated tags are dropped with their content kept;
  `<3`, `< junk>` and other non-tags stay literal. Entities are **not**
  decoded (this is source text, not markup), matching what the C's
  escape-then-unescape round trip displays.
- `{\anN}` / `{\aN}` blocks set the cue anchor (first wins); other `{\...}`
  blocks are stripped from the IR text (the pango output keeps them
  literally, like the C).

## Not handled here

Charset detection, BOM stripping, and buffering/seeking live in the
`gst-subparse` **element**, not in this pure parser. (This parser does defensively
drop a single leading UTF-8 BOM so it is usable standalone.)
