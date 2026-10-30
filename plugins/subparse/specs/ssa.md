# SSA / ASS format reference

**SubStation Alpha (SSA v4)** and **Advanced SubStation Alpha (ASS, "v4+")** are
INI-style subtitle formats. There is **no single authoritative spec**. The
de-facto references are:

- Matroska subtitle technical notes (how SSA/ASS is carried in containers, and
  the field reordering demuxers apply): <https://www.matroska.org/technical/subtitles.html>
- **libass**, the de-facto reference *renderer/implementation*:
  <https://github.com/libass/libass>
- Aegisub's ASS documentation (the practical field/tag reference):
  <https://aegisub.org/docs/latest/ass_tags/>

C reference parser in this tree (what we match byte-for-byte for text):
`subprojects/gst-plugins-base/gst/subparse/gstssaparse.c`, the standalone
GStreamer `ssaparse` element. **It only extracts text**. It does *not* render
ASS styling. Our Rust port lives in
`crates/subparse-formats/src/formats/ssa.rs`.

## File structure (whole-file `.ass` / `.ssa`)

INI-like sections in `[Header]` form. The ones that matter here:

- `[Script Info]`, metadata (`Title:`, `ScriptType:`, `PlayResX/Y:`, ...). Also
  the section a demuxer looks for (`[Script Info]` header) to recognise SSA.
- `[V4 Styles]` / `[V4+ Styles]`, style definitions. **Ignored** by text
  extraction (it has its *own* `Format:` line, which we must not read as event
  columns).
- `[Events]`, the dialogue. A `Format:` line names the columns. Each
  `Dialogue:` line is a value per column. `Comment:` lines are non-rendering.

### Event columns

Standard order (SSA v4 uses `Marked`, ASS v4+ uses `Layer` for column 0):

```
Marked/Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
```

`Text` is **always the last column** and may itself contain commas, so a
Dialogue value is split into just enough fields that the Text field keeps its
commas. Our parser honours the `[Events]` `Format:` line to find the
`Start`/`End`/`Text` column indices (case-insensitive), defaulting to `1`/`2`/`9`
(the standard 10-column layout) when no `Format:` line is present. A `Format:`
line that declares no `Text` column makes every following `Dialogue:` line drop
(there is no text to emit).

### Timestamps

`H:MM:SS.cc`, hours (1+ digits), minutes and seconds (2 digits), and a
fractional part that is conventionally **centiseconds** (2 digits) but which we
parse at arbitrary precision (right-padded/truncated to nanoseconds).

## Text extraction (the `ssaparse` parity path)

This is what `gstssaparse.c` implements and what we reproduce exactly.

The GStreamer `ssaparse` element only handles **container-framed** streams. A
demuxer (e.g. matroskademux) delivers one Dialogue event per buffer, already
reordered to

```
ReadOrder, Layer, Style, Name, MarginL, MarginR, MarginV, Effect, Text
```

with Start/End carried as buffer timestamps, not in the payload. So the text is
reached by walking past **8 commas** (`gst_ssa_parse_push_line`), and a line
with fewer than 8 commas is dropped. This is `dialogue_to_pango_markup()`.

Given the raw Text field, the transform (`gst_ssa_parse_remove_override_codes`
then `g_markup_printf_escaped("%s", …)`) is `strip_to_pango_markup()`, applied in
order:

1. **Remove `{...}` override blocks.** Repeatedly find `{`, then the next `}`,
   and drop the whole `{...}` inclusive. A `}` appearing *before* a `{` is left
   alone (the C searches for `}` starting at the `{`). On an **unmatched `{`**
   (no following `}`), removal stops there, the remainder is kept verbatim, and
   (matching the C early `return`) **the escapes in step 2 are not applied**.
2. **Translate wrapping escapes** (only if step 1 didn't early-out):
   - `\N` and `\n` → `" \n"`, a **space then a newline**. The leading space is
     a quirk of the C (`t[0]=' '; t[1]='\n'`). We preserve it byte-for-byte.
   - `\h` → `"  "` (two spaces).
   - Any other `\x` is left verbatim (the backslash stays).
3. **Escape for Pango markup** (GLib `g_markup_escape_text` semantics):
   `&`→`&amp;`, `<`→`&lt;`, `>`→`&gt;`, `'`→`&apos;`, `"`→`&quot;` (the named
   references current GLib emits), and C0/C1 control chars (`0x01–08`,
   `0x0b–0c`, `0x0e–1f`, `0x7f–84`, `0x86–9f`) → `&#x<hex>;`. Tab (`0x09`) and
   the inserted newline (`0x0a`) pass through.

The element's output caps are `text/x-raw, format=pango-markup`, so
`Ssa::output_format()` returns `PangoMarkup` even though **no** Pango tags are
emitted. Styling override tags are discarded, not converted. (Full ASS styling
would require a real renderer such as libass, out of scope and matching upstream.)

## Styling (cue-ir only)

`text-format=cue-ir` gets the styling the text extraction throws away,
implemented in `crates/subparse-formats/src/ssastyle.rs` and applied by both
elements. The pango-markup output is untouched.

**Collection.** The whole-file parser feeds every line to an `SsaStyles`
collector on the side (`[Script Info]` `PlayResX/Y`, `[V4 Styles]` /
`[V4+ Styles]` with `Format:`-driven columns, v4's legacy alignment encoding
and `TertiaryColour` translated), and attaches an `SsaDialogue` (raw Text
field, style name, margin overrides) to every cue — the `Style`/`MarginL/R/V`
event columns are resolved from the `[Events]` `Format:` line like
`Start`/`End`/`Text`. In framed mode the `ssaparse` element parses the same
sections out of `codec_data` (the section the C keeps behind `FIXME: parse
initial section`) and reads the style/margin fields off each row.

**Mapping.** The dialogue's style becomes the IR base
(font/size/colors/bold/italic/underline/strikeout/scale/spacing, outline and
shadow, `BorderStyle=3` as a cue-background box in the outline colour) and
layout (numpad alignment → anchor + text align, margins). Override tags
become per-span styling: `\i \b \u \s` (empty arg = reset to style), `\fn
\fs \fsp \fscx \fscy`, `\c \1c \3c \4c`, `\alpha \1a \3a \4a` (inverted SSA
alpha converted), `\bord \shad \xshad \yshad`, `\an`/`\a` and `\pos`/`\move`
(first wins, like VSFilter), `\r`/`\rName`, karaoke `\k \K \kf \ko`
(cumulative centiseconds → `Span::reveal_ns`, absolute on the cue timeline),
and `\p` drawing mode (the path commands are dropped). `\t(...)` arguments
are consumed whole so animated tags never leak out statically; `\fad \fade
\clip \org \fr* \be \blur \q \fe \2c \2a` and unknown tags are consumed and
ignored.

**Units.** Positions, margins and font sizes are normalised out of `PlayRes`
space into frame percentages (`PlayRes` defaulting per the usual rules:
384x288 when absent, 4:3-derived when only one axis is given); font sizes use
`FontSize::FrameHeightPercent`. Outline widths, shadow offsets and letter
spacing land on the IR's point-denominated fields as `1px ≈ 1pt` — an
approximation that keeps proportions.

**Line breaks.** The IR gets clean line breaks for `\N`/`\n` and a no-break
space for `\h`; the C's `" \n"` / two-space quirks are preserved only in the
pango-markup text.

## Notes / deviations

- The C `ssaparse` element is per-line and relies on the container for timing.
  Our `Ssa::parse` additionally parses a **whole file body**. It scans sections,
  honours the `[Events]` `Format:` line, and emits timed `Cue`s. The per-line
  text transform is identical (shared `strip_to_pango_markup`), so the future
  `ssaparse` element can reuse `dialogue_to_pango_markup()` unchanged.
- Lenient recovery mirrors the C. Malformed Dialogue lines (bad time, too few
  fields) are skipped, not fatal.
