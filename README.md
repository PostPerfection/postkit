# postkit

Shared library for DCP Wizard, IMF Wizard, and DCP Doctor — common post-production functionality.

Written in Rust. MXF wrapping uses [asdcplib-rs](https://github.com/PostPerfection/asdcplib-rs) FFI bindings.

## Modules

| Module | Purpose |
|--------|---------|
| `encode` | JPEG 2000 encoding pipeline (stream subprocess via grok) |
| `grok_encoder` | Grok J2K encoder (FFI, multi-threaded, GPU-capable) |
| `openjpeg_encoder` | OpenJPEG J2K encoder (multi-instance, CPU-only, no GPU dependency) |
| `transcode` | Video format conversion (via ffmpeg) |
| `hash` | SHA-1 / SHA-256 file hashing |
| `mxf_wrap` | MXF track file wrapping |
| `colour` | Colour space conversion (Rec.709, P3, XYZ) |
| `loudness` | Audio loudness measurement (EBU R128) |
| `atmos` | Dolby Atmos IAB packaging |
| `job_queue` | Background job scheduling |
| `preferences` | JSON preferences (XDG/AppData) |
| `rest_api` | HTTP REST API server |
| `profiles` | Delivery profile presets |
| `burnin` | Subtitle/watermark burn-in |
| `report` | HTML/JSON QC report generation |
| `watch` | Watch folder automation |
| `shell_completion` | Bash/Zsh/Fish completion |
| `portable` | Portable/USB deployment |
| `preview` | Frame-accurate DCP/IMF playback and frame extraction |
| `ingest` | Camera raw ingest (ARRI, RED, Sony, Canon, BRAW) |
| `conform` | EDL/AAF/XML timeline import and reel assembly |
| `metadata_edit` | CPL/OPL metadata editor |
| `certificate` | X.509 certificate generation and trust management |
| `dolby_vision` | Dolby Vision RPU, HDR10, HLG metadata handling |
| `dashboard` | Real-time job monitoring and analytics dashboard |
| `watermark` | Forensic watermarking (NexGuard, Civolution, internal) |
| `dcdm` | Digital Cinema Distribution Master creation and export |
| `version_tracker` | Content versioning database (delivery history) |
| `trailer` | Theatrical trailer packaging (ratings cards, leaders) |
| `accessibility` | Accessibility compliance checking (CVAA, EAA, AODA, Ofcom) |

## Building

```bash
cargo build --release
cargo test
```

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
postkit = { git = "https://github.com/PostPerfection/postkit.git", branch = "master" }
```

## License

GPL-3.0
