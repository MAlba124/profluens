# Profluens

A lightweight, general purpose data and multimedia streaming and processing graph framework, built for performance and predictable latency.

## Compared with GStreamer

The model is GStreamer's (elements, pads, negotiated formats, a clock, a bus), the machinery under it is not.

| | GStreamer | profluens core |
|---|---|---|
| Element | GObject class, per-pad chain and event functions, NULL/READY/PAUSED/PLAYING state machine | a Rust trait: `start`, `process(ctx, inputs)`, `event`, `stop`, with a static `ElementDesc` of pads, offers, properties, latency and scheduling hint |
| Data unit | one refcounted `GstBuffer` per push | a batch of buffers per `process()` call, from per-element pools, plus a per-call bump arena for scratch |
| Threads | a streaming thread per source, a new one at each `queue` | groups: each source and each `Active` element starts one, `Passive` single-input elements run inline in their upstream's group, groups talk over SPSC rings |
| Negotiation | caps queries and events link by link, renegotiated piecemeal | one whole-graph solve at start and at each commit (demand pass, fix pass, verify), transforms declare how output fields follow input, mid-stream changes are announced and resolved per link |
| Control | state changes and seek events sent through the pads | a command log (`Seek`, `SkipTo`, `SetRate`, `Pause`, `Stop`, `Splice`) every group applies in order, through `Control` from any thread |
| Live graph edits | pad probes and blocking by hand | `link`, `unlink`, `relink`, `remove`, then `commit`, spliced into the run as one change that rolls back if a group fails |
| Upstream messages | upstream events (QoS, seek, latency) | a closed `Feedback` set (QoS, bitrate), seeks go through the command log |
| Latency | `LATENCY` query when going to PLAYING | declared per element, summed per path, compensated at sinks, recomputed on each change under a `LatencyPolicy` |
| I/O | blocking calls on the element's thread | ops submitted to a `Reactor` (io_uring or epoll), completions delivered back to the element |
| Plugins | shared objects scanned at run time, `gst-launch` | Rust crates linked at build time, an opt-in `pf-registry` for by-name creation and launch-style lines |
| Testing | `GstHarness` per element | `pf-harness` per element, and `Pipeline::step`, a deterministic driver that runs a whole graph one pass at a time for simulation testing |
| Logging | `GST_DEBUG` to stderr | POD records on an SPSC ring drained off the hot path, `PROFLUENS_DEBUG` |

GStreamer is mature, cross-platform, has bindings for many languages and a very large plugin set. profluens is young, Linux only for now, Rust only, and covers the formats and codecs listed in its plugin crates.

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

FFmpeg is configured LGPL only: no `--enable-gpl`, `--enable-version3` or `--enable-nonfree`, and every external library off. It is linked statically, so a program that ships `pf-libav` must let its users relink against a modified FFmpeg (LGPL 2.1 section 6). Programs that ship the BSD libraries must carry their copyright notices. `pf-vaapi` and `pf-vpl` link libva and libvpl (both MIT) dynamically.

Reference material under `spec/` and `specs/` directories, and the WebVTT corpus in `plugins/subparse/tests/corpus`, keep their own licenses next to the files: BSD-3-Clause for the Monkey's Audio, WavPack, libmpc, Speex and web-platform-tests files, and the IETF Trust terms for the RFC texts.
