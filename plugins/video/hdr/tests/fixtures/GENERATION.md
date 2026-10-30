# HDR fixtures

Three frames of a 64x64 `testsrc2` at 24 frames a second, HDR10 signalled, made
once with ffmpeg 8.1.2 from `nix shell nixpkgs#ffmpeg` (x265, SVT-AV1, libvpx) and
committed. Shared by the tests of `pf-hdr`, the demuxers, the decoders and `pf-play`.

HDR10+ and Dolby Vision are not here: this x265 is built without HDR10+ (its
`dhdr10-info` writes nothing) and no free encoder writes an RPU, so those tests build
the SEI, the NAL units, the boxes and the elements from the spec layouts instead.

What each file states:

| file | container | bitstream |
|---|---|---|
| `hdr10_x265.hevc` (4.1 KB) | none | VUI BT.2020, PQ, BT.2020 ncl, limited, chroma loc 2; SEI 137 and 144 |
| `hdr10_x265_8bit.hevc` (4.1 KB) | none | the same signalling over 8-bit 4:2:0, which the software H.265 decoder takes |
| `hdr10_x264.h264` (3.7 KB) | none | VUI 9/16/9 limited, chroma loc 2; SEI 137 and 144 |
| `hdr10_x265.mkv` (7.3 KB) | `Colour`: matrix 9, range 1, siting 1/1, no transfer or primaries | as above |
| `hdr10_x265.mp4` (7.4 KB) | `colr` `nclx` 9/16/9 limited, no `mdcv` or `clli` | as above |
| `hdr10_svtav1.mkv` (2.4 KB) | `Colour`: 9/16/9, range 1 | sequence header 9/16/9 limited, metadata OBUs `HDR_MDCV` and `HDR_CLL` |
| `hdr10_vp9.webm` (3.0 KB) | `Colour`: matrix 9, range 1 | profile 2, `color_space` BT.2020, limited |

Output options alone (`$C`) reach the encoder's matrix and range and not its transfer
or primaries, so the x265 mkv and the VP9 WebM state only those two in `Colour`, the
case where the bitstream completes the container's colour. The mp4 and the AV1 file
set the frames' colour with `setparams` (`$VF`), which every field reaches.

```sh
X="colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:range=limited:chromaloc=2"
X="$X:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50)"
X="$X:max-cll=1000,400:keyint=3:bframes=0:log-level=error"
C="-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv"
SRC="-f lavfi -i testsrc2=size=64x64:rate=24:duration=0.125"
VF="-vf setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=tv"

ffmpeg -v error -y $SRC -c:v libx265 -pix_fmt yuv420p10le -preset ultrafast \
  -x265-params "$X" -frames:v 3 hdr10_x265.hevc
ffmpeg -v error -y $SRC -c:v libx265 -pix_fmt yuv420p -preset ultrafast \
  -x265-params "$X" -frames:v 3 hdr10_x265_8bit.hevc
ffmpeg -v error -y $SRC -c:v libx264 -pix_fmt yuv420p -preset ultrafast \
  -x264-params "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:range=tv:chromaloc=2:mastering-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):cll=1000,400:keyint=3:bframes=0" \
  -frames:v 3 hdr10_x264.h264
ffmpeg -v error -y $SRC -c:v libx265 -pix_fmt yuv420p10le -preset ultrafast \
  -x265-params "$X" $C -chroma_sample_location topleft -tag:v hvc1 \
  -frames:v 3 hdr10_x265.mkv
ffmpeg -v error -y $SRC $VF -c:v libx265 -pix_fmt yuv420p10le -preset ultrafast \
  -x265-params "$X" -chroma_sample_location topleft -tag:v hvc1 \
  -frames:v 3 hdr10_x265.mp4
ffmpeg -v error -y $SRC $VF -c:v libsvtav1 -pix_fmt yuv420p10le -preset 12 \
  -svtav1-params "mastering-display=G(0.265,0.690)B(0.150,0.060)R(0.680,0.320)WP(0.3127,0.3290)L(1000,0.005):content-light=1000,400:keyint=3" \
  -frames:v 3 hdr10_svtav1.mkv
ffmpeg -v error -y $SRC -c:v libvpx-vp9 -pix_fmt yuv420p10le -profile:v 2 \
  -deadline realtime -cpu-used 8 $C -frames:v 3 hdr10_vp9.webm
```
