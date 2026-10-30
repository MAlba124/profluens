# pf-mp4 — spec provenance and design notes

The ISO/IEC standards this crate is written against are **copyrighted and cannot be
vendored** (unlike the FLAC RFC 9639, the Matroska RFC 9559, or the Ogg RFC 3533 checked
into their crates' `spec/` directories). This file instead records the **exact editions**
each `§` citation in the source refers to, so a reviewer with access to the texts can
follow the code clause by clause, and so the citations do not silently drift between
standard revisions.

## Standards cited

- **ISO/IEC 14496-12** — *Information technology — Coding of audio-visual objects — Part
  12: ISO base media file format*. The box (atom) grammar: the `size`/`type` header, the
  `largesize`/`uuid` extensions (§4.2), FullBox version+flags (§4.2), and the container /
  leaf boxes the sample-table resolver reads. Citations in `src/boxes.rs` (e.g. `§8.7.4`
  for `stsc`) are to the **6th edition (2022)** clause numbering; the sample-table
  structure has been stable since the 2nd edition (2005), so the clause text is
  unambiguous even against an earlier printing. Sections used:
  - §4.2 Box / FullBox structure
  - §4.3 File Type Box (`ftyp`)
  - §8.1.1 Media Data Box (`mdat`)
  - §8.2.1/§8.2.2 Movie Box (`moov`) / Movie Header Box (`mvhd`)
  - §8.3.1/§8.3.2 Track Box (`trak`) / Track Header Box (`tkhd`)
  - §8.4.1–§8.4.4 Media Box (`mdia`) / Media Header (`mdhd`) / Handler (`hdlr`) /
    Media Information Box (`minf`)
  - §8.5.1/§8.5.2 Sample Table Box (`stbl`) / Sample Description Box (`stsd`)
  - §8.6.1.2 Decoding Time to Sample Box (`stts`)
  - §8.6.1.3 Composition Time to Sample Box (`ctts`) — v0 unsigned, v1 signed offsets
  - §8.6.2 Sync Sample Box (`stss`)
  - §8.6.5/§8.6.6 Edit Box (`edts`) / Edit List Box (`elst`)
  - §8.7.3.2/§8.7.3.3 Sample Size Box (`stsz`) / Compact Sample Size Box (`stz2`)
  - §8.7.4 Sample To Chunk Box (`stsc`)
  - §8.7.5 Chunk Offset Boxes (`stco` 32-bit, `co64` 64-bit)
  - §8.8.1/§8.8.4 Movie Extends Box (`mvex`) / Movie Fragment Box (`moof`) — read, see
    the Fragments section below.
  - §8.10.1 User Data Box (`udta`) / §8.11.1 Meta Box (`meta`) — the metadata path walked by
    `src/ilst.rs`. **`meta` is a FullBox**: four octets of version+flags precede its children.
  - §12.1.3 VisualSampleEntry, §12.2.3 AudioSampleEntry — the `stsd` entry layouts.

- **Apple, *QuickTime File Format Specification*, "Metadata" chapter** (the published
  developer documentation; freely available, not vendored because it is Apple's text). The
  iTunes metadata *item* scheme `src/ilst.rs` reads is Apple's, not ISO's, and has no clause
  numbers — citations in that file read "(QTFF Metadata)". Used for: the metadata item list
  (`ilst`) and the item-id-as-box-type convention; the item data atom (`data`) with its
  4-octet type indicator (type-set octet + 3-octet well-known type) and 4-octet locale
  indicator; the well-known types 0 (implicit), 1 (UTF-8), 13 (JPEG), 14 (PNG), 21 (BE signed
  integer); the `©`-prefixed (0xA9) legacy user-data ids `©nam`/`©ART`/`©alb`/`©day`/`©gen`/
  `©wrt`/`©cmt` plus `aART`, `trkn`, `disk`, `covr`, `gnre`; and the `----` freeform triple
  `mean` / `name` / `data` that carries ReplayGain. Also used, in `props_from_moov`, for the
  QuickTime **version-2 sound description** layout (an `f64` sample rate at octet 32 and a u32
  channel count at octet 40), which is how a `.mov` states a rate the ISO 16.16 field cannot
  hold. Clean-room: written from these standards only — no tag-library source was consulted.

- **ISO/IEC 14496-15** — *… Part 15: Carriage of network abstraction layer (NAL) unit
  structured video in the ISO base media file format*. The `AVCDecoderConfigurationRecord`
  (§5.3.3.1) and `HEVCDecoderConfigurationRecord` (§8.3.3.1) carried in the `avcC` / `hvcC`
  child of an `avc1` / `hvc1` sample entry. **This crate does not re-parse these records**:
  the identical record rides an MKV `CodecPrivate`, so `src/codec.rs` reuses `pf-mkv`'s
  already-tested `nal_head_from_config` (see "Reframer reuse" below). Edition: 2022.

- **RFC 6381** — *The 'Codecs' and 'Profiles' Parameters for "Bucket" Media Types*. The
  four-CC ⇒ codec family names in `src/codec.rs::family_for` follow the sample-entry
  four-CCs RFC 6381 §3.3 registers (`avc1`/`avc3`, `hvc1`/`hev1`, `vp09`, `av01`, `Opus`,
  `mp4a`). Not vendored (RFC text is freely available at rfc-editor.org).

## Reframer reuse (why pf-mp4 depends on pf-mkv)

`avc1`/`hvc1` sample entries store frames as **length-prefixed NAL units** and their
parameter sets in an `avcC`/`hvcC` configuration record — byte-for-byte the same record
Matroska carries in `CodecPrivate`, and the same length-prefixed → Annex B reframing MKV
Blocks need. Rather than copy `pf-mkv`'s tested `parse_avcc`/`parse_hvcc` +
`length_prefixed_to_annex_b`, pf-mp4 depends on pf-mkv and reuses
`pf_mkv::nal_head_from_config` + `pf_mkv::Reframer`. One implementation, tested once. This
is **not** a container dependency: the MP4 box grammar, sample-table resolution, and
streaming slicer are all hand-written in this crate; only the codec-record parser (a
14496-15 concern, not a 14496-12 one) is shared.

## The oracle: ffmpeg's mov demuxer

`tests/oracle.rs` resolves the committed fixtures with our `Mp4Reader` and dumps their
packets with a live `ffprobe`, and asserts the two agree on sample count, track, file
offset, per-sample size (== the mdat slice length), keyframe flag, and the pts/dts
arithmetic. A resolution bug (a wrong `stsc` fold, an off-by-one `stco`) shows up as a
disagreement against an independent reader. It skips where ffprobe is absent. No MP4
library is a dependency of any kind.

## Progressive vs fragmented

- **Progressive** files (a single `moov` + `mdat`, `moov` before or after `mdat`) are the
  v1 target. The `Mp4Demux` constructor is handed the **file head through `moov`** (the
  same constructor-supplied-discovery model `MkvDemux` uses — a mid-pipeline element gets
  no input during preroll and the scheduler freezes topology after preroll), so the sample
  tables are resolved up front regardless of where `mdat` sits.
- **Fragmented** files (`moov` carrying `mvex`, then `moof`+`mdat` fragments; §8.8)
  resolve to their tracks and no sample. The samples are read off each `moof` as the
  stream arrives (`src/frag.rs`, and the walk in `src/reader.rs`), so the same reader
  takes a fragmented file and an init segment followed by media segments.

## Fragments (§8.8, §8.16.3)

- **What is read.** `mvex` with `trex` (a track's defaults) and `mehd` (the duration with
  the fragments). `moof` with one `traf` per track, its `tfhd` (base offset, defaults,
  `default-base-is-moof`, `duration-is-empty`), its `tfdt` (the decode time of its first
  sample) and its `trun` boxes (data offset, first sample flags, and per sample a
  duration, a size, flags and a composition offset, unsigned in version 0 and signed
  from version 1). `sidx` and `mfra`/`tfra`/`mfro` for the seek index. `styp`, `emsg`,
  `prft`, `free` and whatever the reader does not know are stepped over (§4.2).
- **Offsets.** A `tfhd` base offset is an offset in the stream as it is fed. With
  `default-base-is-moof`, or with neither in a `moof`'s first `traf`, it is the first
  octet of the `moof`. With neither in a later `traf` it is where the data of the
  `traf` before ended (§8.8.7). A `trun` with no data offset goes on where the run
  before it ended (§8.8.8).
- **Times.** A sample's decode time is its `tfdt` plus the durations before it, and with
  no `tfdt` the track goes on where its last fragment ended. Of the edit list only the
  first edit's media time comes off, the one edit a fragmented movie writes. A sync sample is one whose
  flags leave `sample_is_non_sync_sample` clear (§8.8.3.1).
- **One timeline across segments.** A source of segments flags the first buffer of each
  (`BufferFlags::SEGMENT`). With a `pts` the segment starts a stretch of the timeline at
  that stream time, and its `dts`, when set, is the media time that lands there. With no
  `dts` the earliest decode time of the first fragment behind the mark lands there. A
  segment flagged with no `pts` goes on in the stretch it is in.
- **An init segment in the stream** (a `moov` behind the first) replaces the tracks when
  it describes as many, of the same kinds, and the pads send their codec head and their
  format again. One that repeats the last changes nothing. Any other is
  `Mp4Error::InitChanged`.
- **Untrusted input.** A `moof` is gathered whole, up to 16 MiB, and read only once its
  `mdat` is known. A sample has to lie inside that `mdat` and overlap no other, so a
  fragment holds no more samples than its `mdat` has octets, and no more than 2^20. A
  sample of no octets is not carried. A fragment that breaks a rule is cut where it does
  and said (`Mp4Reader::take_error`). A box whose size is smaller than its header loses
  the stream. Behind a source of segments the walk starts over at the next one, and a
  file fails the element.
- **Seeking.** `Tables::fragments` is the index, from a `sidx` in the head or from the
  `mfra` handed to `Mp4Reader::indexed`. `Tables::restart` names the `moof` of the last
  indexed fragment at or before what any track needs, and with no index the first
  fragment, so a file with neither is sought by reading it from its first fragment to
  the target, all of it through the source and none of it through a decoder. Behind
  the restart each track is let through by its stamps. Video passes from the last
  sync sample at or before the point and audio from its pre-roll ahead of it, both
  found in the first fragment that reaches past them.
- **Not read.** A `sidx` that refers to another `sidx` is stepped over. `saio`/`saiz`
  and `senc` (encrypted samples), `sbgp`/`sgpd` in a fragment, and more than one sample
  description per track. A group of pictures spread over several fragments, sought
  into behind its first, starts at the next sync sample.

## Edit lists (`elst`, §8.6.6)

The demuxer follows a progressive track's whole edit list (`src/edit.rs`). The list is read
in the movie timescale (`mvhd`) and put into the track's media timescale, a segment rounded
up so a sample that starts inside a segment rounded down into the coarse movie timescale
stays inside it, the starts summed in movie ticks so they do not drift.

- **Edits.** An edit at `media_rate` 1 presents the media `[media_time, media_time +
  duration)` from where the edits before it end on the presentation timeline. An empty edit
  (`media_time` -1) presents nothing for its duration, so a leading one starts the track
  later. A dwell (`media_rate` 0) presents nothing new for its duration, and a video sink
  keeps the picture shown before it. A segment duration of zero runs to the end of the
  media. The media ranges have to ascend. An edit at another rate, one that goes back in
  media time and those after either are not followed, nor any past the 1024th, and the
  track warns on the bus (`Mp4Error::EditList`).
- **Samples.** A sample whose presentation time (`dts + ctts`) lies in an edit is stamped
  where the edit puts it, its decode time moved by as much. Video keeps, for each edit,
  every sample from the sync sample at or before its first picture to its last picture in
  decode order. Those outside every edit are marked hidden (`Sample::hidden`) and leave
  flagged `BufferFlags::DECODE_ONLY`. They are stamped at the start of the next edit, or
  the end of the last, which is at or before the first picture shown behind them, so a
  decoder that hides by stamp (the software shell in `profluens-video` keeps the stamps of
  the units so flagged and hides the first picture out under each) hides those and no
  other. The VA-API decoders take the flag with the unit. An audio sample ahead of the
  first edit is the encoder's priming. It is kept as before, and the first edit's media
  time is the track's declared delay, which the decoder cuts. Every other sample outside
  every edit is dropped from the index. The seek index, `Tables::restart` and the sample
  a track restarts at all go by the presentation stamps.
- **Audio across edits.** An audio decoder stamps by counting samples on from its first,
  so the audio of two edits back to back plays back to back, and a gap or a dwell between
  two edits of an audio track is not heard as silence.
- **Duration.** A track lasts what its edit list spans where that differs from `mdhd` by
  more than one movie tick.
- **A list that presents nothing** (every edit past the media) is not followed. Only the
  first edit's media time comes off, as the reader did before it followed whole lists, and
  the track warns.

### The scanner's use of `elst` (`ilst::props_from_moov`)

Separate from the demuxer's shift above, the no-decode props path uses the edit list to answer
"how long does this file play". An MP4 states that three times — the sound track's `mdhd`
(media duration, fine timescale), its `elst` (presentation duration, coarse *movie* timescale),
and `mvhd` (the longest track's presentation duration) — and they disagree. Two guarded rules,
both measured against `ffprobe` over the 15 MP4/M4A files in the reference library:

* **The edit list wins over `mdhd` only when it differs by more than one movie tick.** A real
  priming trim (`media_time > 0`) moves the duration by tens of milliseconds. But the common
  `elst` is a single `media_time == 0` entry that trims nothing and merely restates `mdhd`
  rounded into the movie timescale — four of the 15 files are exactly that, and honouring them
  unconditionally puts the duration 0.17–0.87 ms *off* where `mdhd` alone is exact.
* **`mvhd` wins over the sound track only in a multi-track movie**, and again only past one
  movie tick. §8.2.2 defines `mvhd.duration` as the longest track's, so in a single-track file
  it *is* the sound track's and any excess is a stale header; in a movie with video it is the
  file's real playing time. This is what makes `ffmpeg_bar_test/output.mp4` report 427.734 s
  (`ffprobe`'s format duration is 427.733333) instead of the sound track's raw 427.745 s.

Empty edits (`media_time == -1`) contribute no duration in either rule: §8.6.6 makes them gaps,
and they present no media. Entry widths are handled at both versions (v0 32-bit, v1 64-bit).
The walk is hand-rolled rather than built on `boxes::parse_elst`, which returns a `Vec` — the
scanner path allocates nothing.

## Audio and text sample entries

- **`mp4a`** is AAC by its `esds` objectTypeIndication 0x40 or 0x66 to 0x68 with an
  AudioSpecificConfig (ISO/IEC 14496-1 section 7.2.6.6), and MPEG audio by 0x6B
  (ISO/IEC 11172-3) or 0x69 (ISO/IEC 13818-3), announced `mp3`, whose `esds` carries no
  DecoderSpecificInfo. A Layer II stream is announced `mp3` too, the MPEG audio decoders
  read the layer off each frame header.
- **`fLaC`** is FLAC in ISOBMFF (the encapsulation the xiph flac repository publishes).
  Its `dfLa` is a FullBox of version 0 holding the native metadata
  blocks, STREAMINFO first. The demuxer rebuilds `fLaC` and those blocks into a native
  stream head, the last block flagged as such, which `flacdec` starts on and which is
  Matroska's `A_FLAC` CodecPrivate. A sample is one FLAC frame.
- **`alac`** is announced `alac` with its ALACSpecificConfig kept, which no decoder here
  takes, so a player drops it by name.
- **`tx3g`** is 3GPP Timed Text (3GPP TS 26.245), what ffmpeg writes as
  `mov_text`. A sample is a 16-bit length and that much UTF-8 text, then style boxes,
  which are not read. It is announced `subtitle/srt` and the text goes out as the cue.
- **`wvtt`** is WebVTT in ISOBMFF (ISO/IEC 14496-30). A sample is a `vttc` per cue
  with its text in a `payl` box, its settings in `sttg` and its id in `iden`, which are not
  read, or a `vtte` for a gap. It is announced `subtitle/vtt` and each `payl` goes out as
  a cue.
- A cue lasts from its sample's time to the next sample of its track, the stretch the
  muxer gave it, the last one to the end of the track. A gap sample, a tx3g of length zero
  or a `vtte`, puts out nothing.
- **`stpp`** is TTML (ISO/IEC 14496-30), announced `subtitle/ttml`, which nothing
  here parses.

## Not yet (v1 scope)

- **Edits at a rate other than 1 or 0**, and edits that go back in media time (above).
- **Common encryption** (CENC). An `encv`/`enca` track is a `bytes` pad.
- **Codec-init via caps** — like `MkvDemux`, the head bytes are a constructor argument
  while codec-init negotiation is built; the sample entry's `avcC`/`hvcC` is emitted as an
  Annex B parameter-set head before the first frame instead.
- **What the muxer leaves out.** `src/writer.rs` and `src/mux.rs` write the box grammar
  this reader parses (2026-10-19). Not written: `colr`, `pasp`, a language, a faststart
  `moov`, `mfra` and `sidx`. The Dolby codecs have no sample entry here. `vp09` has one in
  `writer.rs` but `mp4mux` offers no VP9 family, so it is unreachable.
- **TTML** (`stpp`) is announced as `subtitle/ttml`, which no parser here reads.
- **ALAC** is announced as `alac`, which no decoder here takes.
