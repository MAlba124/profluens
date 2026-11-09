# profluens validation corpus

## Layout

| file             | what                                                        |
|------------------|-------------------------------------------------------------|
| `cargo xtask fixtures` | generate the corpus into `out/` with ffmpeg ()        |
| `out/`           | generated media                                             |

## Regenerating

```sh
cargo xtask fixtures                          # generate whatever is missing, then ffprobe-sanity-check
FORCE=1 cargo xtask fixtures                  # rebuild every fixture from scratch
FFMPEG=/path/to/ffmpeg cargo xtask fixtures   # FFPROBE likewise, else the ffprobe beside it
```

### The matrix

| fixture                     | intent                                             |
|-----------------------------|----------------------------------------------------|
| `mkv_h264_aac.mkv`          | H.264 + AAC in Matroska                             |
| `mkv_h265_aac.mkv`          | H.265 + AAC in Matroska                             |
| `webm_vp8_vorbis.webm`      | VP8 video + Vorbis audio in WebM                    |
| `webm_vp9.webm`             | VP9, video-only                                    |
| `mkv_av1.mkv`               | AV1, video-only (if an AV1 encoder exists)         |
| `mp4_h264_aac.mp4`          | H.264 + AAC, **+faststart** (moov before mdat)     |
| `mp4_h265.mp4`              | H.265, video-only, +faststart                      |
| `mp4_h264_nofaststart.mp4`  | **negative** — moov *after* mdat                   |
| `ogg_flac.oga`              | FLAC-in-Ogg                                         |
| `ogg_vorbis.ogg`            | Vorbis-in-Ogg                                       |
| `ogg_speex.spx`             | Speex-in-Ogg, 16 kHz wideband, 2 s                  |
| `ogg_theora_speex.ogv`      | Theora 160x120 at 15 fps + Speex in Ogg, 2 s        |
| `audio.mp3`                 | MP3, 44.1 kHz stereo CBR                            |
| `audio.flac`                | FLAC (native)                                       |
| `audio.wav`                 | WAV, s16le PCM                                      |
| `audio.aac`                 | raw ADTS AAC (no container)                         |
| `audio_f32.wav`             | WAV, f32le PCM                                      |
| `audio.ac3`                 | AC-3, stereo                                        |
| `audio_51.ac3`              | AC-3, 5.1                                           |
| `audio.eac3`                | E-AC-3                                              |
| `audio.dts`                 | DTS core, stereo                                    |
| `ts_h264_aac.ts`            | H.264 + AAC in MPEG-TS                              |
| `ts_h265_ac3.ts`            | H.265 + AC-3 in MPEG-TS                             |
| `ps_mpeg1_mp2.mpg`          | MPEG-1 video + MP2 in an MPEG-1 program stream      |
| `ps_mpeg2_ac3.vob`          | MPEG-2 video + AC-3 in DVD VOB packs                |

## Running the matrix

```sh
./run-matrix.sh /path/to/pfplay          # default (hardware) decode path
./run-matrix.sh --sw /path/to/pfplay     # software path (PF_NO_VAAPI=1)
```

The player CLI contract (frozen — `pfplay` is built to this spec):

```
pfplay [--no-window] [--no-audio] [--stats] [--max-secs N] FILE
```
