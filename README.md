# postkit

[![CI](https://github.com/PostPerfection/postkit/actions/workflows/ci.yml/badge.svg)](https://github.com/PostPerfection/postkit/actions/workflows/ci.yml)

Version 0.5 encodes JPEG 2000 via optional Grok FFI (`grok-ffi`). Tests without `grok-ffi` use the real disabled-feature path and do not require developer-local media.

Shared library for DCP Wizard, IMF Wizard, and DCP Doctor, common post-production functionality.

Written in Rust. MXF wrapping uses [asdcplib-rs](https://github.com/PostPerfection/asdcplib-rs) FFI bindings.

## Modules

| Module | Purpose |
|--------|---------|
| `accessibility` | Accessibility heuristic scan (CVAA, EAA, AODA, Ofcom); keyword-based, not a certified verdict |
| `burnin` | Subtitle/watermark burn-in |
| `certificate` | X.509 certificate generation and trust management |
| `colour` | Colour conversion via ffmpeg (Rec.709, P3, Rec.2020); wide-gamut/log spaces need a LUT. Also the DCI X'Y'Z' → sRGB display transform (`XyzToSrgb`, inverse of the DCDM encode) and an optional monitor-ICC path (`XyzToIcc`, `icc` feature) |
| `conform` | EDL and FCP7/Resolve XML (xmeml) timeline import and reel assembly (AAF not supported) |
| `cpl_annotation` | CPL annotation and revision metadata |
| `cpl_xml` | String-level CPL/OPL XML tag read/write helpers |
| `crossfade` | Equal-power PCM crossfade join (cos/sin gains, constant power) |
| `dashboard` | HTTP server exposing version and distribution analytics as JSON |
| `dcdm` | Digital Cinema Distribution Master creation and export; XYZ or P3-D65 mastering target |
| `dolby_vision` | Dolby Vision RPU, HDR10, HLG metadata handling |
| `edl_import` | EDL/FCP XML timeline parsing and import |
| `encode` | JPEG 2000 encoding pipeline (stream subprocess via grok) |
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

`grok-ffi` links libgrokj2k (grok >= 12) found via pkg-config, so build and
install grok first (cmake, e.g. to `~/bin/grok`), then put its `lib/pkgconfig` on
`PKG_CONFIG_PATH` and its `lib` on `LD_LIBRARY_PATH`. CI does this in a cached
"Setup grok" step; see `.github/workflows/ci.yml`.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
postkit = { git = "https://github.com/PostPerfection/postkit.git", tag = "v0.5.0" }
```

## License

GPL-3.0
