# postkit

[![CI](https://github.com/PostPerfection/postkit/actions/workflows/ci.yml/badge.svg)](https://github.com/PostPerfection/postkit/actions/workflows/ci.yml)

Version 0.6 encodes JPEG 2000 via optional Grok FFI (`grok-ffi`). Tests without `grok-ffi` use the real disabled-feature path and do not require developer-local media.

Shared library for DCP Wizard, IMF Wizard, and DCP Doctor, common post-production functionality.

Written in Rust. MXF wrapping uses [asdcplib-rs](https://github.com/PostPerfection/asdcplib-rs) FFI bindings.

## Modules

| Module | Purpose |
|--------|---------|
| `accessibility` | Accessibility heuristic scan (CVAA, EAA, AODA, Ofcom); keyword-based, not a certified verdict |
| `burnin` | Subtitle/watermark burn-in |
| `certificate` | X.509 certificate generation and trust management |
| `colour` | Colour conversion via ffmpeg (Rec.709, P3, Rec.2020); wide-gamut/log spaces need a LUT. Also the DCI X'Y'Z' → sRGB display transform (`XyzToSrgb`, inverse of the DCDM encode) and an optional monitor-ICC path (`XyzToIcc`, `icc` feature) |
| `conform` | EDL, FCP7/Resolve XML (xmeml) and FCP X (fcpxml) timeline import and reel assembly (AAF not supported) |
| `cpl_annotation` | CPL annotation and revision metadata |
| `cpl_xml` | String-level CPL/OPL XML tag read/write helpers |
| `crossfade` | Equal-power PCM crossfade join (cos/sin gains, constant power) |
| `dashboard` | HTTP server exposing version and distribution analytics as JSON |
| `dcdm` | Digital Cinema Distribution Master creation and export; XYZ or P3-D65 mastering target |
| `dolby_vision` | Dolby Vision RPU, HDR10, HLG metadata handling |
| `edl_import` | EDL/FCP XML timeline parsing and import |
| `encode` | JPEG 2000 encoding pipeline (ffmpeg stream and TIFF sequence, through the linked grok) |
| `font_subset` | Subtitle font subsetting to a code-point set (640 KB embed limit) |
| `frame_compare` | Per-frame PSNR/SSIM and pooled VMAF via ffmpeg |
| `grok` | Grok codec detection and configuration |
| `grok_encoder` | Grok J2K encoder (FFI, multi-threaded, GPU-capable) |
| `hash` | SHA-1 / SHA-256 file hashing |
| `ingest` | Camera format detection (ARRI, RED, Canon Cinema RAW Light, BRAW, Sony RAW/X-OCN via MXF essence-UL parsing). Transcodes ffmpeg-decodable inputs (ProRes, DNxHR); camera RAW is rejected loudly (stock ffmpeg cannot decode it) |
| `j2k` | JPEG 2000 codestream parsing, DCI validation, MXF frame extraction and bitrate analysis |
| `job_queue` | Background job scheduling |
| `loudness` | Audio loudness measurement (EBU R128, Leq(m)) and gain adjustment |
| `mca` | SMPTE ST 377-4 Multi-Channel Audio label generation |
| `metadata_edit` | CPL/OPL metadata editor |
| `mid_side` | Mid-side stereo decode (M/S to L/R) in interleaved multi-channel buffers |
| `mpv` | mpv IPC player integration for preview |
| `mxf_wrap` | MXF track file wrapping (picture, stereoscopic 3D, audio, Atmos); optional AES-128 essence encryption, MCA labels, HDR/WCG metadata |
| `otioz_import` | OpenTimelineIO zip bundle import |
| `packaging` | Shared DCP/IMF CPL, PKL and ASSETMAP XML writers |
| `pipeline` | Full video-to-DCP streaming pipeline |
| `plugin` | Python plugin system with pre/post hooks |
| `preferences` | JSON preferences (XDG/AppData) |
| `preview` | Media preview and frame extraction. Plain-file ffplay path, plus a DCP-native path that resolves a DCP dir/CPL/MXF, decrypts encrypted picture essence in Rust (key from `KEYS.json` or hex), decodes J2K via ffmpeg and colour-manages X'Y'Z' → sRGB (or a monitor ICC). Not real-time: decodes to an intermediate first |
| `probe` | Media file probing (resolution, codec, duration) |
| `profiles` | Delivery profile presets |
| `prores` | ProRes detection and transcoding |
| `report` | HTML/JSON QC report generation |
| `rest_api` | Shared REST API server utilities |
| `shell_completion` | Bash/Zsh/Fish/PowerShell completion generation |
| `subtitle_formats` | Subtitle input parsers to a styled-cue model (ASS/SSA, FCPXML, Matroska, PAC, Interop PNG), RTL shaping, line-wrap |
| `subtitle_retime` | Subtitle timing conversion between framerates; standalone SRT parser |
| `timecode` | SMPTE timecode and frame-rate math (drop-frame aware) |
| `trailer` | Theatrical trailer packaging (ratings card + leader concatenated ahead of content) |
| `upmix` | Stereo to 5.1 upmix (DoM upmixer A/B, windowed-sinc FIR) |
| `version_tracker` | Content versioning database (delivery history) |
| `watch` | Watch folder automation |
| `watermark` | Burns a visible text mark into each frame (not forensic/invisible) |
| `wav_io` | WAV read/write to normalized interleaved f32 (shared DSP helper) |
| `webhook` | HTTP webhook notifications on job completion/failure |
| `xmldsig` | Enveloped XML-DSig sign/verify (SMPTE 430-3 profile) |

## Building

```bash
cargo build --release
cargo test
```

Cargo features: `grok-ffi` (J2K encoder), `async` (tokio), `icc`
(monitor-ICC display path in `preview`/`colour`, needs liblcms2). All off by
default.

`grok-ffi` links libgrokj2k (grok >= 20.4.3) found via pkg-config, so build and
install grok first (cmake, e.g. to `~/bin/grok`), then put its `lib/pkgconfig` on
`PKG_CONFIG_PATH` and its `lib` on `LD_LIBRARY_PATH`. CI does this in a cached
"Setup grok" step; see `.github/workflows/ci.yml`.

grok's accelerator plugin runs the wavelet and T1 on a device. grok looks for
`libgrokj2k_plugin` under `GRK_PLUGIN_PATH`, then in the working directory, then
next to the executable, and searches nowhere at all when `GRK_NO_PLUGIN` is set,
which keeps everything on the CPU. Call `grok_encoder::use_gpu` after
`grok_encoder::initialize` and every encode and decode in the process runs on
the device, `grok_encoder::use_cpu` sends them back. A reduced decode and a
tiled stream stay on the CPU either way. An encode then runs as one batch
through the plugin's pipeline: the encoder threads submit their frames and the
writer collects the code streams the plugin hands back on its own threads, so
the pipeline stays full instead of taking one frame at a time. The frame shape
comes from the first frame, and a shape or a set of parameters the plugin
declines puts the whole run back on the CPU. A decode still routes per call,
and an encode with a PSNR target stays on the CPU, because a frame over the
byte cap is compressed again by rate.

With the plugin on, ffmpeg decodes with `-hwaccel cuda` and the frames reach
the batch in the layout the plugin takes rather than the one postkit converts
itself. A yuv420p, yuv422p, yuv420p10le or yuv422p10le source goes to the pipe
as its own three planes and the device upsamples the chroma, converts YUV to
RGB and runs the X'Y'Z' transform. Every other source goes as packed 16-bit
RGB, little-endian when the plugin takes the interleaved buffer as it comes off
the pipe and big-endian when postkit deinterleaves it into grok's component
buffers. postkit asks the plugin which of those it takes before starting the
decoder, since the answer decides what ffmpeg writes. A subtitle burn,
postkit's own P3 or Rec.2020 transform, the HDR-to-DCI LUT, a filter that
changes the pixel format or the colour, and a PSNR target each keep a run on
packed RGB. An 8-bit YUV source on that pipe decodes through `format=gbrp16le`
first, because swscale converts 8-bit YUV straight to rgb48 at 8 bits and lands
about two codes of 255 off the exact colour.

`cargo test --features grok-gpu` runs the device round trip and needs a machine
with the plugin, CI has no GPU: `tests/grok_gpu.rs` for the round trip,
`tests/grok_gpu_yuv.rs` for the planar YUV source and
`tests/grok_gpu_rgb48le.rs` for the interleaved one. This needs grok v20.4.3 or
newer, which has `grk_plugin_set_enabled`.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
postkit = { git = "https://github.com/PostPerfection/postkit.git", tag = "v0.5.0" }
```

## License

AGPL-3.0-or-later. Copyright (C) 2026 Grok Image Compression Inc. See [LICENSE](LICENSE).
