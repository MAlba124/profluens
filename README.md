# Profluens

A lightweight, general purpose data and multimedia streaming and processing graph framework, built for performance and predictable latency.

## Building

```sh
git submodule update --init --depth 1
nix develop
cargo check --workspace --all-targets
cargo nextest run --cargo-profile fasttest
```

`libdav1d-sys` (behind `pf-av1`) builds dav1d 1.5.4, `libav-sys` (behind `pf-libav`) the FFmpeg 9.0.2 decoders it uses, `libflac-sys` (behind `pf-libflac`) libFLAC and `libopus-sys` (behind `pf-opus`) libopus 1.6.1, each from its git submodule under `sys/`, statically linked. They need `meson`, `ninja`, a C compiler and `nasm` on the path, which the `nix develop` shell has.

## License

The profluens code is MIT, see [LICENSE](LICENSE). Some crates build or carry third-party code under its own license:

| crate | third-party code | license | text |
|---|---|---|---|
| `libav-sys` | FFmpeg 9.0.2, `sys/libav-sys/ffmpeg` and the configure output in `sys/libav-sys/config` | LGPL-2.1-or-later | `ffmpeg/COPYING.LGPLv2.1` |
| `libdav1d-sys` | dav1d 1.5.4, `sys/libdav1d-sys/dav1d` | BSD-2-Clause | `dav1d/COPYING` |
| `libflac-sys` | libFLAC, `sys/libflac-sys/flac` (the library only, not the tools) | BSD-3-Clause | `flac/COPYING.Xiph` |
| `libopus-sys` | libopus 1.6.1, `sys/libopus-sys/opus` | BSD-3-Clause | `opus/COPYING` |
| `pf-text` | glyph atlas made from JetBrains Mono | OFL-1.1 | `plugins/text/src/FONT_LICENSE.txt` |

FFmpeg is configured LGPL only: no `--enable-gpl`, `--enable-version3` or `--enable-nonfree`, and every external library off. It is linked statically, so a program that ships `pf-libav` must let its users relink against a modified FFmpeg (LGPL 2.1 section 6). Programs that ship the BSD libraries must carry their copyright notices. `pf-vaapi` links libva (MIT) dynamically.

Reference material under `spec/` and `specs/` directories, and the WebVTT corpus in `plugins/subparse/tests/corpus`, keep their own licenses next to the files: BSD-3-Clause for the Monkey's Audio, WavPack, libmpc, Speex and web-platform-tests files, and the IETF Trust terms for the RFC texts.
