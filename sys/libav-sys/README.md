# libav-sys

Static FFI to a subset of FFmpeg's libavcodec and libavutil, twenty decoders and the
native AAC encoder behind a flat C shim that pf owns. FFmpeg is the git submodule `ffmpeg/`, `build.rs` compiles the
files of those decoders out of it. `pf-libav` is the only user.

## FFmpeg

- Release **9.0.2**, tag `n9.0.2`, commit `946fcce07b6dcd0331c8cc609192aeff5e1924f8`,
  the submodule `ffmpeg/` from `https://git.ffmpeg.org/ffmpeg.git`, unpatched.
- Decoders `h264`, `hevc`, `vp8`, `vp9`, `mpeg2video`, `mpeg4` (with the `h263` it selects), `aac`, `ac3`, `eac3`, `mp3float`,
  `dca`, `truehd`, `mlp`, `vorbis`, `alac`, `flv`, `vp6`, `vp6f`, `vp6a`, `vp7`, parsers `h264`, `hevc`, `vp8`, `vp9`, `mpegvideo`, `aac`, `ac3`,
  `mpegaudio`, `dca`, `mlp`, the `vp9_superframe_split` bsf the
  vp9 decoder selects, and the encoder `aac` (AAC-LC, FFmpeg's own).
- MPEG-1 video and MPEG audio layers I and II need no decoder of their own. `mpeg2video` reads
  an MPEG-1 sequence header as MPEG-1 and `mp3float` decodes every layer, see `PROFLUENS-NOTES.md`.
- `build.rs` compiles only the `.c` and `.asm` files in `config/<target>/sources.txt`,
  the rest of the tree is never built. See `PROFLUENS-NOTES.md`.

## Submodule

`.gitmodules` marks it `shallow = true`, which only makes the first clone depth 1. The
pinned commit is not the tip of FFmpeg's default branch, and plain
`git submodule update --init` then fetches that commit with its whole history (about
220 MB). Fetch it at depth 1 instead (35 to 60 MB, git.ffmpeg.org and the GitHub mirror
both serve a commit by its sha):

```
git submodule update --init --depth 1
```

or `git clone --recurse-submodules --shallow-submodules`. A missing submodule fails the
build with that command in the message. A new FFmpeg release is a submodule bump, then
`cargo xtask libav-config <triple>` and `FFMPEG_RELEASE` in `build.rs`.

## License

The FFmpeg code is licensed under the GNU Lesser General Public License version 2.1 or
later (`ffmpeg/COPYING.LGPLv2.1`, `ffmpeg/LICENSE.md`). The build is LGPL only:
configure ran without `--enable-gpl`, `--enable-version3` or `--enable-nonfree` and with
every external library off (`config.h` states
`FFMPEG_LICENSE "LGPL version 2.1 or later"`, `CONFIG_GPL 0`, `CONFIG_NONFREE 0`). The
crate's own Rust, `build.rs`, `tools/` and `shim/` are MIT like the rest of the workspace.

The library is linked statically into the program that uses `pf-libav`. The LGPL then
asks that the program be distributable with what a user needs to relink it against a
modified FFmpeg (LGPL 2.1 section 6), which for this workspace is its source: rebuild
with a changed `ffmpeg/` submodule.

## Configure line

Run once per target, offline, out of tree, by `cargo xtask libav-config` on the submodule. The
outputs are committed under `config/<rust target triple>/` and `build.rs` never runs
configure.

```
configure --cc=clang --target-os=linux --arch=x86_64 --enable-pic \
  --disable-everything --disable-autodetect --disable-programs --disable-doc \
  --disable-avdevice --disable-avformat --disable-avfilter --disable-swscale \
  --disable-swresample --disable-network --disable-iconv --disable-debug \
  --disable-symver --disable-pixelutils \
  --enable-decoder=h264,hevc,vp8,vp9,aac,ac3,eac3,mp3float,dca,truehd,mlp,mpeg2video,mpeg4,vorbis,alac,flv,vp6,vp6f,vp6a,vp7 \
  --enable-parser=h264,hevc,vp8,vp9,aac,ac3,mpegaudio,dca,mlp,mpegvideo \
  --enable-encoder=aac
```

Threads stay compiled in (`HAVE_PTHREADS 1`). With `--disable-pthreads` FFmpeg's one-time
table setup is not thread safe and its configure says the library must not be used from
several threads, which a pipeline with two decoders on two groups does. The shim opens
every decoder and the encoder with `thread_count = 1` and `thread_type = 0`, so
libavcodec starts no thread of its own.

## Layout

```
config/x86_64-unknown-linux-gnu/   config.h, config_components.h, config.asm,
                                   config_components.asm, libavutil/avconfig.h,
                                   libavutil/ffversion.h, libavcodec/{codec,parser,bsf}_list.c,
                                   flags.txt (configure's CPPFLAGS, CFLAGS, nasm flags),
                                   sources.txt (every .c and .asm of the two libraries)
shim/pf_libav.{h,c}                the flat interface Rust binds
src/lib.rs                         hand-written FFI to the shim only
tools/objlist.py                   the file list, run by cargo xtask libav-config
ffmpeg/                            the FFmpeg submodule
```

`sources.txt` is committed rather than read from FFmpeg's Makefiles by `build.rs`. Those
spread one library's objects over nested includes, `ifdef` blocks, `$(if ...)` and the
`ffbuild/arch.mak` indirection, which takes a make evaluator (`tools/objlist.py`, run at
regen time), and with the list committed `build.rs` needs no Makefile at all.

## Adding a target

`cargo xtask libav-config <triple>` on (or with the compiler of) that target, after
adding its configure flags to `target_flags` in `xtask/src/libav.rs`. aarch64-linux-android and
aarch64-apple-darwin have a slot. `build.rs` assembles `.asm` with nasm and compiles
every `.c` listed, so an arm target's `.S` files need an assembler step added there.

## Build

The C is built at `-O3` with configure's flags, the x86 assembly with `nasm` (the devshell
has it), the two side by side. Cold, about 50 s on 12 threads.
