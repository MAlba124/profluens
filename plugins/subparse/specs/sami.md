# SAMI format reference

**Media type:** `application/x-subtitle-sami` · **Output:** pango-markup

## Authoritative source

- Microsoft, *Understanding SAMI 1.0* (Synchronized Accessible Media
  Interchange). Archived documentation:
  <https://learn.microsoft.com/en-us/previous-versions/windows/desktop/dnacc/understanding-sami-1.0>

SAMI is an SGML/HTML-derived captioning format. The spec describes a strict
document model, but **real-world SAMI files diverge heavily** from it. Tags are
frequently unclosed, attributes unquoted, colours malformed, and markup nested
incorrectly. A conformant SGML parser is useless in practice, so the de-facto
reference for interoperability is the GStreamer implementation, not the spec.

## Reference implementation (what we match)

`gstreamer/subprojects/gst-plugins-base/gst/subparse/samiparse.c`, the
`GstSamiContext` state machine. Our parser is a close port. Match its output
byte-for-byte. Element unit tests: `tests/check/elements/subparse.c`
(`test_sami`, `test_sami_xml_entities`, `test_sami_html_entities`,
`test_sami_bad_entities`, `test_sami_comment`, `test_sami_self_contained_tags`).

## Document shape

```
<SAMI>
  <HEAD> ... <STYLE> ...css... </STYLE> </HEAD>   (ignored)
  <BODY>
    <SYNC Start=1000> <P Class=CC> caption text <br> more text
    <SYNC Start=2000> <P Class=CC> next caption
  </BODY>
</SAMI>
```

- **`<SYNC Start=ms>`** is the only timing primitive. `Start` is **milliseconds**
  (parsed like C `atoi`: leading integer, junk ignored, non-numeric → 0), scaled
  to nanoseconds. A cue runs from one `<SYNC>` start to the next. The final cue's
  end comes from `</BODY>`/`</SAMI>` and is left open (`end_ns = None`).
- Text is captured **only inside `<SYNC>`** (`in_sync`). HEAD, STYLE and comment
  content are skipped.
- Start times never go backwards: `time2 = max(new, previous)`.

## Autodetect

Sniffed by the presence of the literal `<SAMI>` or `<sami>` in the first ~35
bytes (`gstsubparseelement.c`). Lives in `autodetect.rs`, not here.

## Distilled parsing notes (this port)

Shape is **lex → parse**:

1. **Line feed.** The upstream element hands the parser one `\n`-terminated line
   at a time; SAMI is *not* in the EOS force-flush set, so an unterminated final
   line is dropped. We process only complete lines.
2. **Unescape (per line).** `unescape_string`:
   - `&nbsp`/`&nbsp;` → U+00A0 (case-insensitive, `;` optional).
   - XML entities `quot amp apos lt gt` → re-emitted canonical `&name;`
     (case-insensitive, `;` required) so they survive as pango markup.
   - HTML entities (the ~247-entry table, case-sensitive, `;` required) →
     UTF-8.
   - `&#dd;` / `&#xhh;` numeric refs → UTF-8 (`;` optional, only lowercase `x`
     starts hex). The value goes through `strtoul` into a `gunichar`, so it
     truncates to 32 bits, and a value past `ULONG_MAX` sets `ERANGE`: the C then
     passes the reference on **without consuming it**, so only the `&#`/`&#x`
     is dropped and the digits stay as literal text.
   - Any other `&` → `&amp;`.
   - Every run of ASCII whitespace collapses to a single space.
3. **Lex (tokenizer).** Split a line (plus any carried-over partial tag) into
   `<...>` tags and text runs. An unterminated tag is buffered across lines. Each
   text run is whitespace-stripped (`g_strstrip`) before emission, so **text
   abutting a tag loses its bordering spaces** (e.g. `a <i>b</i> c` →
   `a<i>b</i>c`). This is faithful to the C.
4. **Parse (state machine + tag stack).** A byte per open `<font>/<i>/<ruby>/<rt>`
   is pushed onto a stack. `<sync>` and close tags pop it, emitting the matching
   pango closers. Mappings:
   - `<i>` → `<i>` / `</i>`
   - `<font color=…>` → `<span foreground="…">`. Hashless 6-hex colours gain a
     `#`. The X11 names pango lacks (`aqua crimson fuchsia indigo lime olive
     silver teal`) are mapped to hex.
   - `<font face=…>` → `<span font_family="…">`
   - `<br>` → newline
   - `<ruby>`/`<rt>` → the annotation is collected into a small
     `<span size='xx-small' rise='-100'>…</span>` and prepended to the block.
   - `<p>` and unknown elements → ignored.

## Quirks replicated bug-for-bug

- **Whitespace-adjacent-to-tags loss** (item 3 above).
- **Unterminated-tag reprocessing:** deliberately *not* replicated. The C keeps
  the whole line buffer and rescans it next line, re-emitting the text before
  the tag each time (quadratic). Here the text before the tag is handed on
  once, only the tag waits for its `>`, and a tag past 64 KiB is dropped.
- **Crude attribute scan:** the C counts `=` signs and steps a *flat* array index
  by two, so tags with several attributes parse only the first few. We reproduce
  the same counting.
- **Malformed markup resets state:** a non-alphanumeric element/attribute name
  drops all pending content (context reset) and the line yields no cue.
- **Nesting guard:** a tag stack deeper than 64 aborts (DoS guard), resetting
  the context.
- **`GST_CLOCK_TIME_NONE` end** on the trailing cue → `Cue::end_ns == None`.

## Deliberate deviations

- **Popping a tag that is not open is a no-op.** `sami_context_pop_state` walks
  the tag stack from the top collecting `</i>`/`</span>` closers, and when the
  target turns out never to have been opened it discards them and leaves the
  stack alone. Its `<rt>` arm, however, appends to `rubybuf` *as it walks*, so
  that append survives the discarded walk. A stray `</font>` inside a `<ruby>`
  (`<ruby>base<rt>anno</font></ruby>`) therefore emits `</span>` twice for one
  opener, and pango rejects the whole cue as invalid markup, i.e. the cue is lost
  rather than merely mis-styled. We look the target up before mutating anything
  and return early when it is not open, which keeps the emitted markup balanced.
  Everything about the found case (which closers, in what order, and the
  truncation point) is unchanged, as is `CLEAR_TAG`.

## Not copied verbatim

The entity code-point/name table is transcribed from `samiparse.c` (data, not
prose). No SAMI spec text is reproduced.
