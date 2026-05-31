# postkit

Shared library for DCP Wizard, IMF Wizard, and DCP Doctor — common post-production functionality.

Written in Rust. MXF wrapping uses [asdcplib-rs](https://github.com/PostPerfection/asdcplib-rs) FFI bindings.

## Modules

| Module | Purpose |
|--------|---------|
| `accessibility` | Accessibility compliance checking (CVAA, EAA, AODA, Ofcom) |
| `burnin` | Subtitle/watermark burn-in |
| `certificate` | X.509 certificate generation and trust management |
| `colour` | Colour space conversion (Rec.709, P3, XYZ) |
| `conform` | EDL/AAF/XML timeline import and reel assembly |
| `cpl_annotation` | CPL annotation and revision metadata |
| `dashboard` | Real-time job monitoring and analytics dashboard |
| `dcdm` | Digital Cinema Distribution Master creation and export |
| `dolby_vision` | Dolby Vision RPU, HDR10, HLG metadata handling |
| `edl_import` | EDL/FCP XML timeline parsing and import |
| `encode` | JPEG 2000 encoding pipeline (stream subprocess via grok) |
| `grok` | Grok codec detection and configuration |
| `grok_encoder` | Grok J2K encoder (FFI, multi-threaded, GPU-capable) |
| `hash` | SHA-1 / SHA-256 file hashing |
| `ingest` | Camera raw ingest and transcoding (ARRI, RED, Sony, Canon, BRAW) |
| `j2k` | JPEG 2000 codestream utilities |
| `job_queue` | Background job scheduling |
| `loudness` | Audio loudness measurement (EBU R128) |
| `mca` | SMPTE ST 377-4 Multi-Channel Audio label generation |
| `metadata_edit` | CPL/OPL metadata editor |
| `mpv` | mpv IPC player integration for preview |
| `mxf_wrap` | MXF track file wrapping (picture, audio, Atmos) |
| `openjpeg_encoder` | OpenJPEG J2K encoder (multi-instance, CPU-only, no GPU dependency) |
| `otioz_import` | OpenTimelineIO zip bundle import |
| `pipeline` | Full video-to-DCP streaming pipeline |
| `plugin` | Python plugin system with pre/post hooks |
| `preferences` | JSON preferences (XDG/AppData) |
| `preview` | Frame-accurate DCP/IMF playback and frame extraction |
| `probe` | Media file probing (resolution, codec, duration) |
| `profiles` | Delivery profile presets |
| `prores` | ProRes detection and transcoding |
| `report` | HTML/JSON QC report generation |
| `subtitle_retime` | Subtitle timing conversion between framerates |
| `trailer` | Theatrical trailer packaging (ratings cards, leaders) |
| `version_tracker` | Content versioning database (delivery history) |
| `watch` | Watch folder automation |
| `watermark` | Forensic watermarking (NexGuard, Civolution, internal) |
| `webhook` | HTTP webhook notifications on job completion/failure |

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
