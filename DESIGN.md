# Design

Shared library crate for the PostPerfection suite. dcpdoctor, dcpwizard, and imfwizard depend on it by path; anything needed by more than one app lives here. Depends on asdcplib-rs (git) for MXF I/O. dcpdoctor-wasm deliberately does not use postkit (no-std-io rewrites in dcpdoctor-parse/-imf).

## Modules (44, all exported in lib.rs)

Packaging and formats:
- certificate: X.509 chains, KDM creation/rewrap, trust management. KDM content keys are caller-supplied (from the DCP's keys file) so the KDM unlocks the actual encrypted essence, falling back to a fresh MDIK when none are given
- xmldsig: enveloped XML-DSig sign/verify, SMPTE 430-3 profile (xmlsec1-cross-checked in tests)
- packaging: shared DCP/IMF CPL, PKL and ASSETMAP XML writers, plus the one public `escape_xml` and namespace constants. DcpCpl reels carry an optional picture/sound KeyId, emitted when the essence is encrypted
- mxf_wrap: J2K/PCM/timed-text/Atmos wrapping for DCP and AS-02 via asdcplib-rs, with optional AES-128 essence encryption at wrap time (J2K/PCM); the content key stays out of any serialized form and is redacted in Debug
- cpl_xml: string-level CPL/OPL XML tag helpers
- cpl_annotation, metadata_edit: CPL/OPL field read/write
- mca: ST 377-4 channel labels, soundfield presets, MCA XML
- j2k: J2K codestream header parsing, DCI bitrate analysis
- hash: SHA-1/SHA-256 file hashing
- dcdm: DCDM creation, correct Rec.709 to X'Y'Z' transform, 12/16-bit TIFF

Encoding:
- encode: J2K encode orchestration (detection, parallel encode, progress)
- grok / grok_encoder: grk_compress subprocess and in-process FFI pipeline (feature-gated)
- openjpeg_encoder: in-process OpenJPEG pipeline (feature-gated)
- pipeline: shared encode workflow used by the wizard GUIs
- prores: ProRes probe and extraction arg builders

Timeline and ingest:
- conform: CMX 3600 EDL and FCP7 xmeml import, reel assembly (AAF rejected loudly)
- edl_import, otioz_import: EDL/FCP XML and OTIO/OTIOZ parsing
- ingest: camera format detection (ARRI/RED/BRAW/CRM magic bytes); transcodes ffmpeg-decodable inputs (ProRes, DNxHR) and rejects true camera RAW loudly (stock ffmpeg cannot decode it)
- timecode: SMPTE timecode/framerate math (single home for it)
- subtitle_retime: TTML/SRT framerate retiming, plus a standalone SRT cue parser

Media tooling (ffmpeg/external-tool wrappers). These are honest about their limits:
- colour: Rec.709/P3/Rec.2020 via ffmpeg; XYZ/ACES/ACEScg/LogC require a LUT or the dcdm path, else rejected
- preview: ffplay/ffmpeg preview that reads the real frame rate and errors on options it can't fulfil (cpl_uuid, gpu, display colourspace); not a DCP-native player
- dolby_vision: HDR10 injection re-encodes with libx265 to write real SEI (copy cannot); RPU work delegates to dovi_tool
- loudness: EBU R128 via loudnorm, with short-term max from a second ebur128 pass
- trailer: ratings card and leader are concatenated ahead of the content; output is a media file, no CPL/DCP is produced
- watermark: visible drawtext mark only, not forensic
- burnin, probe, mpv

Workflow and infra:
- dashboard, version_tracker: sqlite version/delivery DBs
- job_queue: in-memory job store (caller drives execution)
- rest_api: minimal blocking HTTP server
- plugin, watch, webhook, preferences, profiles, report, accessibility, shell_completion

## Testing

199 lib tests plus MXF/DCDM integration tests, all passing: xmlsec1-verified signatures, real MXF roundtrips through asdcplib (including an encrypted-wrap roundtrip that proves the essence is no longer stored verbatim), DCDM reference-value colour tests, packaging XML writers, and the pure-function parsers (ebur128 short-term, x265 HDR10 params, SRT cues).
