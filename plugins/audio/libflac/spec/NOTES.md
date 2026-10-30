# pf-libflac notes

The codec is libFLAC 1.5.0 (`sys/libflac-sys`). What this crate decides around it is here.
`rfc9639.txt` is the format (RFC 9639, December 2024), `ogg-flac-mapping.md` the Ogg
mapping `oggflacdeframe` follows. Both came over from the hand-written pf-flac, which this
crate replaced on 2026-10-26.

## Feeding libFLAC from pushed bytes

libFLAC's stream decoder pulls its input through a read callback and cannot stop inside
a frame. `StreamDecoder` keeps the pushed bytes, finds the frames itself and gives libFLAC
one frame at a time. Every decision is made on the bytes alone, never on how many had
arrived, so the output (frames, where they are placed, how the stream ends, the error
count) is the same however the input was cut, damaged or not. The fuzz target holds the
pieces, the whole and a resync equal on any input.

- An ID3v2 tag ahead of `fLaC` is skipped by its synchsafe size (10-byte header, 10 more
  with the footer flag), never parsed, `pf_tags::flacmeta::id3v2_len`. Its bytes are
  dropped as they come, whether pushed or pulled by window, so a large tag is not
  buffered. One tag only: what follows it must be `fLaC`. A size byte with its high bit
  set is no tag, so `BadMagic`. `audio_start` and frame bytes count the tag, they are file
  offsets. `flacmeta` skips the same tag, and the player's probe looks behind a tag for
  `fLaC` before calling the file MP3.
- The metadata chain is waited for whole (`pf_tags::flacmeta::audio_start`) before libFLAC
  sees a byte. The marker and the first block are checked first, as pf-flac did: no
  `fLaC` is `BadMagic`, a first block that is not a 34-byte STREAMINFO is `BadStreamInfo`.
  A chain past 64 MiB is `TooLarge`, whole or not. libFLAC is then given the chain and no
  more, a block a call, so a chain it reads otherwise than flacmeta (it wants more than the
  blocks hold) is `BadStreamInfo`. A STREAMINFO after the first is turned into PADDING in
  the bytes libFLAC gets: the first is the stream's, as flacmeta and pf-flac read it, where
  libFLAC would take the last. The frames start where flacmeta ends the chain, also when
  libFLAC found fault with a block and stopped reading metadata early.
- Frame headers are found here: a sync code `0xFFF8`/`0xFFF9`, a well formed coded number
  and the header's CRC-8 (RFC 9639 section 9.1). Too few bytes to tell is waited on, so a
  candidate is never judged on a partial header. Bytes skipped to reach a header are
  dropped, read once, and a run of them counts one error.
- On each header libFLAC is flushed and started (`FLAC__stream_decoder_flush`, STREAMINFO
  kept), to decode that frame alone. Running dry inside it aborts the call (the read
  callback), and the next call starts on the same header once more bytes are in. A
  truncated frame is backpressure, not an error, as before. Nothing is retried until the
  input grows.
- libFLAC's first error in a frame (bad header, CRC mismatch, samples out of range) ends
  the call: the read and write callbacks abort, so a frame libFLAC would find on its own
  in what it holds is not kept and its resync never runs. The attempt counts one error and
  the search goes on a byte past the sync code, as libFLAC's own resync would. So what
  libFLAC holds in its read buffer, which depended on how the bytes came, no longer
  matters, there is no seek callback and no call outside the public API.
- A frame that needs more than 2^24 - 1 bytes (STREAMINFO's largest frame size) is
  `TooLarge`, whether it ran dry past that or decoded, so the cap is met the same however
  the bytes came.
- Cost on valid streams: one CRC-8 over the header per frame on top of what libFLAC does.
  Valid streams decode bit for bit, frame for frame and place for place as before the
  change (diffed on the fuzz corpus, `fixtures/out/audio.flac` and libFLAC's test files,
  in pieces of 1 byte to whole). Damaged ones salvage the same samples on that corpus, the
  error counts now follow the rule above.
- Behind a seek, `seek_reset` flushes libFLAC and the search starts on the bytes pushed
  next. The header is never needed again. The landing counts as a skipped run.

## Frames

- libFLAC drops a frame whose CRC fails, or whose samples do not fit the bit depth, and
  the search finds the next header. pf-flac failed the element on a corrupt frame.
  Corruption now costs the frames it touches and the stream goes on, which is what a
  player wants.
- libFLAC writes silence for frames it finds missing, up to 5 s or 50 frames, but only
  against the frame it decoded before in the same run, and every frame is a run of its own
  here, so none is written. The frame count of a corrupt stream stays bounded by its
  bytes, as the corruption gate asserts, and the stamps count what was decoded, as before.
- A frame starts at the header it was started on. Its first sample is read off the frame
  header, as pf-flac did (RFC 9639 section 9.1.5): a variable block frame carries it, a
  fixed block frame's number times STREAMINFO's largest block, or the frame's own where
  that is 0. libFLAC multiplies by the smallest, and where STREAMINFO's block sizes differ
  reads the frame number as a sample number (a concession to an old variable block
  encoder). pf-flac's own two-pass encoder wrote such streams, its short last block in the
  minimum.
- STREAMINFO comes from libFLAC's metadata callback. The tags, the SEEKTABLE and the end of
  the header come from `pf_tags::flacmeta`, the reader the tag scanner uses, so the two can
  not disagree.
- The decoder writes interleaved `i32` into a buffer that trades places with the caller's,
  so a steady decode allocates nothing.
- `flacdec` puts a depth out in the narrowest of `s8`, `s16`, `s24` and `s32` that holds
  it, each sample shifted left to the container's full scale: 12 bits as `s16` shifted by
  4, 20 bits as `s24` by 4, 4 to 7 bits as `s8`, 25 to 31 bits as `s32`. The label and the
  bytes agree. pf-flac (and this crate before) said `s32` and wrote `ceil(bits / 8)` bytes.
  `StreamDecoder` and `FlacDecoder` keep the samples at their own depth.

## Encoder (`encoder` feature)

- libFLAC at `-5`, block 4608 for `FlacEncoder::new` (the subset's largest at 48 kHz and
  below, what pf-flac used), the declared block for `new_streaming`. A rate or depth
  outside the streamable subset retries with the subset off.
- libFLAC holds back a block and a sample, so frames come out late and the rest at
  `finish(&mut out)`, which also returns the final STREAMINFO body (MD5 filled in, unlike
  pf-flac). `encode_frames` / `finish_frames` hand each frame out alone, cut where libFLAC
  wrote it, for a container fixture.
- The header handed out is `fLaC` and STREAMINFO alone, last-block flag set. libFLAC's
  VORBIS_COMMENT naming itself is left out, so the header is 42 bytes as before.
- `flacenc` sends the held-back frames at end of stream.

## Kept as it was

- The `flacdec` element around the decoder: the format and tags said once, the stamps on
  the sample grid, the cut to the presentation point, `FRAMES_PER_BATCH`, backpressure, the
  restart behind a seek and the seek index (`index.rs`, `GROWN_EVERY`). Its output is
  byte for byte and stamp for stamp pf-flac's on every valid stream tried (the differential
  check of the switch), but for the depths that are not a whole number of bytes (Frames).
