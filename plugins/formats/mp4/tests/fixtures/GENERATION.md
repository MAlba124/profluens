# MP4 test fixture provenance

Three tiny committed MP4 fixtures, generated once with the system ffmpeg so the
oracle + end-to-end tests have deterministic, few-hundred-KB-max inputs. All are
**progressive** (single `moov` + `mdat`, `moov` first via `+faststart`); the video is
128x96, 15 frames at 15 fps, yuv420p (the 4:2:0 8-bit subset `pf-h264` decodes).

## `tiny_h264.mp4` (12.7 KB)

Baseline profile, **no B-frames** ⇒ **no `ctts`** box, so composition time == decode time
(pts == dts). A closed GOP of 15 with `stss` marking the single sync sample. This is the
primary end-to-end decode fixture: every sample decodes through `pf-h264` (Annex B, 4:2:0
8-bit) with no reorder caveat.

```sh
ffmpeg -v error -y -f lavfi -i "testsrc2=size=128x96:rate=15:duration=1" \
  -c:v libx264 -profile:v baseline -pix_fmt yuv420p -g 15 -bf 0 \
  -x264-params "keyint=15:min-keyint=15:scenecut=0" \
  -movflags +faststart tiny_h264.mp4
```

## `bframes_h264.mp4` (12.5 KB)

Main profile, **2 B-frames** ⇒ **`ctts` present**, so this exercises the composition-offset
arithmetic (`pts = dts + ctts`) and decode-order ≠ presentation-order. Used by the oracle
test for the `stts`/`ctts` cross-check; not required to decode end-to-end (the reorder
caveat in `pf-h264` is documented, so the oracle asserts pts/dts, not decoded planes).

```sh
ffmpeg -v error -y -f lavfi -i "testsrc2=size=128x96:rate=15:duration=1" \
  -c:v libx264 -profile:v main -pix_fmt yuv420p -g 15 -bf 2 \
  -x264-params "keyint=15:min-keyint=15:scenecut=0" \
  -movflags +faststart bframes_h264.mp4
```

## `tiny_av.mp4` (21.4 KB)

**Two tracks** — the same baseline no-B-frame video plus an AAC-LC stereo sine tone
(48 kHz, 48 audio access units), so track 1 is `avc1` (with `avcC`) and track 2 is
`mp4a` with an `esds`-carried AudioSpecificConfig. This is the multi-track remux
fixture: `mp4demux(passthrough) ! mkvmuxn` must carry both tracks' bytes bit-exact
(video length-prefixed NALs + record, audio raw AAC AUs + ASC — RFC 9559 §12 shapes).

```sh
ffmpeg -v error -y -f lavfi -i "testsrc2=size=128x96:rate=15:duration=1" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" \
  -c:v libx264 -profile:v baseline -pix_fmt yuv420p -g 15 -bf 0 \
  -x264-params "keyint=15:min-keyint=15:scenecut=0" \
  -c:a aac -b:a 64k -ac 2 -shortest -movflags +faststart tiny_av.mp4
```

## `mp4v_mpeg4.mp4`, `mp4v_mpeg2.mp4`, `mp4v_mpeg1.mp4` (31 KB in all)

`mp4v` sample entries told apart by the esds objectTypeIndication, 0x20 (MPEG-4 Visual),
0x61 (MPEG-2 Main) and 0x6A (MPEG-1). 128x96, 25 frames at 25 fps, a sync sample every
10, no B-frames. The MPEG-4 file carries its VOS and VOL headers only in the esds
DecoderSpecificInfo, none in band, so it decodes only if the demuxer hands that on. The
MPEG-1 and MPEG-2 esds have no DecoderSpecificInfo, the sequence header is in band.

```sh
for c in mpeg4 mpeg2video mpeg1video; do
  ffmpeg -v error -y -f lavfi -i "testsrc2=size=128x96:rate=25:duration=1" \
    -c:v $c -q:v 24 -g 10 -bf 0 -pix_fmt yuv420p -movflags +faststart -f mp4 mp4v_$c.mp4
done
mv mp4v_mpeg2video.mp4 mp4v_mpeg2.mp4; mv mp4v_mpeg1video.mp4 mp4v_mpeg1.mp4
```

## `frag/` (fragmented, 220 KB in all)

Made by one script, three seconds of the same 128x96 picture at 15 frames a second with
a group of pictures a second, so every file has three fragments of video.

```sh
video=(-f lavfi -i "testsrc2=size=128x96:rate=15:duration=3")
audio=(-f lavfi -i "sine=frequency=440:sample_rate=48000:duration=3")
x264=(-c:v libx264 -pix_fmt yuv420p -g 15 -x264-params "keyint=15:min-keyint=15:scenecut=0")
aac=(-c:a aac -b:a 48k -ac 2)

# frag_av.mp4: a fragment per group of pictures, nothing in moov, two B-frames, AAC
# beside it, absolute base offsets in each tfhd, and an mfra at the end.
ffmpeg -v error -y "${video[@]}" "${audio[@]}" "${x264[@]}" -profile:v main -bf 2 "${aac[@]}" -shortest \
  -movflags frag_keyframe+empty_moov frag_av.mp4

# frag_sidx.mp4: video alone, offsets from each moof, one sidx ahead of the fragments.
ffmpeg -v error -y "${video[@]}" "${x264[@]}" -profile:v baseline -bf 0 \
  -movflags frag_keyframe+empty_moov+default_base_moof+dash+global_sidx frag_sidx.mp4

# frag_hybrid.mp4: the first group of pictures in moov, the rest in fragments.
ffmpeg -v error -y "${video[@]}" "${x264[@]}" -profile:v baseline -bf 0 \
  -movflags frag_keyframe frag_hybrid.mp4

# dash_*: DASH, two video renditions (stream0 128x96, stream1 64x48) and one audio
# (stream2), an init segment and media segments of a second each, and the manifest.
ffmpeg -v error -y "${video[@]}" "${audio[@]}" -map 0:v -map 0:v -map 1:a \
  -c:v libx264 -pix_fmt yuv420p -g 15 -x264-params "keyint=15:min-keyint=15:scenecut=0" \
  -profile:v baseline -bf 0 -s:v:0 128x96 -s:v:1 64x48 "${aac[@]}" \
  -f dash -seg_duration 1 -use_template 1 -use_timeline 1 \
  -adaptation_sets "id=0,streams=v id=1,streams=a" manifest.mpd
```

`init-streamN.m4s` and `chunk-streamN-0000M.m4s` are kept as `dash_init-streamN.m4s`
and `dash_streamN-0000M.m4s`. What `ffprobe -show_packets` lists for the three files is
what `tests/fragmented.rs` holds the reader to, where there is an `ffprobe` to ask.

## System ffmpeg

```
ffmpeg version 8.1 (libx264), 8.1.2 for the `mp4v_*` files
```

The tests that consult a live ffmpeg as a second-opinion oracle **skip if ffmpeg/ffprobe
is absent** (they do not regenerate these committed fixtures — those are frozen).

## `fuzz/`

Inputs the `mp4_demux` fuzzer (fuzz/, `tools/dst-remote.sh fuzz`) failed on, minimized
with `-minimize_crash=1`, each a 16 octet control block and then the file
(fuzz/src/rig.rs). `tests/fuzzed.rs` replays them through the fuzzer's own rig.

- `pulled_nothing_wanted_*` are pulled, and no sample is wanted from the first ask on.
  The demuxer ended its asks with none out and the source never ran again to see it.
