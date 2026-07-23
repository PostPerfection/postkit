# Design

Shared library crate for the PostPerfection suite. dcpdoctor, dcpwizard, and imfwizard depend on it by path; anything needed by more than one app lives here. Depends on asdcplib-rs (git) for MXF I/O. dcpdoctor-wasm deliberately does not use postkit (no-std-io rewrites in dcpdoctor-parse/-imf).

## Modules (50 in lib.rs)

Packaging and formats:
- certificate: X.509 chains, KDM creation/rewrap, trust management. KDM content keys are caller-supplied (from the DCP's keys file) so the KDM unlocks the actual encrypted essence, falling back to a fresh MDIK when none are given
- xmldsig: enveloped XML-DSig sign/verify, SMPTE 430-3 profile (xmlsec1-cross-checked in tests). Signing is SHA-256; verification reads the declared DigestMethod/SignatureMethod and dispatches on them (sha1/256/384/512, rsa-sha1/256/384/512), so real SHA-1-signed Interop DCPs verify. Unknown algorithms fail loud
- packaging: shared DCP/IMF CPL, PKL and ASSETMAP XML writers, plus the one public `escape_xml` and namespace constants. Output is SMPTE-XSD-valid: writers emit elements in ST 429-7/8/9 order and include the required IssueDate/Issuer/ContentVersion/RatingList; AssetMap orders its metadata by namespace (SMPTE vs Interop). DcpCpl reels carry an optional picture/sound KeyId, emitted when the essence is encrypted
- mxf_wrap: J2K/PCM/timed-text/Atmos wrapping for DCP and AS-02 via asdcplib-rs, with optional AES-128 essence encryption at wrap time (J2K/PCM); the content key stays out of any serialized form and is redacted in Debug
- cpl_xml: string-level CPL/OPL XML tag helpers
- cpl_annotation, metadata_edit: CPL/OPL field read/write
- mca: ST 377-4 channel labels, soundfield presets, MCA XML
- j2k: J2K codestream header parsing, DCI bitrate analysis
- hash: SHA-1/SHA-256 file hashing
- dcdm: DCDM creation; Rec.709 to X'Y'Z' via gamma 2.2 + 48/52.37 companding + 2.6 gamma (matching colour::rgb_to_xyz_inplace and libdcp/DoM/grok), 12/16-bit TIFF

Encoding:
- encode: J2K encode orchestration (detection, parallel encode, progress)
- grok / grok_encoder: grk_compress subprocess and in-process FFI pipeline (feature-gated)
- grok_encoder: in-process Grok J2K encoder (FFI, feature-gated behind `grok-ffi`)
- pipeline: shared encode workflow used by the wizard GUIs
- prores: ProRes probe and extraction arg builders

Timeline and ingest:
- conform: CMX 3600 EDL and FCP7 xmeml import, reel assembly (AAF rejected loudly)
- edl_import, otioz_import: EDL/FCP XML and OTIO/OTIOZ parsing
- ingest: camera format detection (ARRI/RED/BRAW/CRM magic bytes); transcodes ffmpeg-decodable inputs (ProRes, DNxHR) and rejects true camera RAW loudly (stock ffmpeg cannot decode it)
- timecode: SMPTE timecode/framerate math (single home for it)
- subtitle_retime: TTML/SRT framerate retiming, plus a standalone SRT cue parser
- subtitle_formats: input parsers that all yield a common `StyledCue` (SRT-style ms timing, styled text runs with italic/bold/underline/colour, optional horizontal align / vertical anchor / vertical position percent, optional bitmap image path), with `to_srt_cue` for the lossless-to-plain `SrtCue` path. Covers ASS/SSA (per-style + inline `\i \b \u \an`, unsupported override tags returned as warnings rather than dropped), PAC (Latin codepage, byte map and timecode decode ported from Subtitle Edit's Pac.cs; non-Latin codepages fail loud), MKS/Matroska (ffprobe lists streams and ffmpeg extracts the chosen SRT/ASS one, no EBML parser; fails loud on missing ffmpeg or no subtitle stream), FCPXML caption/title with exact rational offset/duration timing, and Interop DCSubtitle XML that references PNG bitmap subs (each PNG resolved relative to the XML and validated for existence + magic). Submodules: `bidi` (a pure logical-to-visual reorder for RTL subtitles, Arabic presentation-form shaping via ar-reshaper then Unicode bidi reorder per line) and `wrap` (max-column line wrapping that breaks on whitespace, keeps an overlong word whole, and preserves styled-run boundaries; plain-string and `StyledCue` forms)
- font_subset: TTF/OTF glyph subsetting to a code-point set via allsorts (`subset_font(bytes, chars)`), emitting a usable standalone OpenType font that keeps its cmap (Minimal profile + Unicode cmap), so an embedded subtitle font fits the SMPTE/interop 640 KB limit. Chars resolve to glyph ids through the font's own cmap

Media tooling (ffmpeg/external-tool wrappers). These are honest about their limits:
- colour: Rec.709/P3/Rec.2020 via ffmpeg; XYZ/ACES/ACEScg/LogC require a LUT or the dcdm path, else rejected. DCPwizard uses this LUT path for required HDR-to-DCI transforms; generic HDR tone mapping stays an explicit caller choice. `rgb_to_xyz_inplace` is the standardized display-referred Rec.709 RGB to DCI X'Y'Z' encode (16-bit rgb48be in place): gamma 2.2 linearize, Rec.709/D65 matrix pre-multiplied by the DCI companding coefficient 48/52.37, then 2.6 gamma. This matches libdcp's `rec709_to_xyz` + `rgb_xyz.cc` (DoM parity) and grok's `applyXYZTransform`, so the encode path (grok FFI) and pad-colour frames agree. Also the display transform `XyzToSrgb`: the inverse of the DCDM encode (code/4095 → ^2.6 → Y-scale to DCI ref white → Bradford DCI→D65 → XYZ→sRGB → sRGB OETF), built once as LUTs + a 3x3 matrix. `XyzToIcc` (`icc` feature) routes X'Y'Z' → a monitor ICC through littleCMS with a selectable intent
- preview: two paths. Plain files still go through ffplay (real frame rate, errors on options it can't fulfil). The DCP-native path resolves a DCP dir/CPL/MXF to the picture essence, decrypts it in Rust when encrypted (asdcplib-rs AesDecContext; key from a dcpwizard KEYS.json image key or a raw hex key; encrypted-with-no-key fails loud), decodes the J2K with ffmpeg to raw X'Y'Z' codes and colour-manages them to sRGB or a monitor ICC. `render_dcp_frame` writes one colour-correct still; `play_dcp` decodes a range into a lossless intermediate and plays it. Not real-time (CPU decode, decode-to-intermediate first)
- dolby_vision: HDR10 injection re-encodes with libx265 to write real SEI (copy cannot); RPU work delegates to dovi_tool
- loudness: EBU R128 via loudnorm, with short-term max from a second ebur128 pass
- trailer: ratings card and leader are concatenated ahead of the content; output is a media file, no CPL/DCP is produced
- watermark: visible drawtext mark only, not forensic
- burnin, probe, mpv

Audio DSP (pure PCM, no ffmpeg):
- mid_side: M/S to L/R decode in place (L=M+S, R=M-S with DoM's /2 normalization), leaving every other channel byte-identical
- upmix: stereo to 5.1, mirroring DoM's two upmixers (A splits by band through blackman-windowed sinc FIR filters, B is passthrough L/R with a summed centre, lowpassed LFE, and a 20 ms-delayed L-R to both surrounds). Interleaved f32 or stereo-WAV in, 6-channel DCP order L,R,C,LFE,Ls,Rs out
- crossfade: equal-power (cos/sin) join of two PCM sources by frame or by WAV file, failing loud on sample-rate/bit-depth/format/channel mismatch or an overlap longer than either source

Workflow and infra:
- dashboard, version_tracker: sqlite version/delivery DBs
- job_queue: in-memory job store (caller drives execution)
- rest_api: minimal blocking HTTP server
- plugin, watch, webhook, preferences, profiles, report, accessibility, shell_completion

## Testing

294 lib tests plus MXF/DCDM/preview integration tests, all passing: xmlsec1-verified signatures, SHA-1 and SHA-256 enveloped verify (with tamper rejection) plus a gated cross-check against real signed ClairMeta ECL DCPs, a gated xmllint check that generated ASSETMAP/PKL/CPL pass the SMPTE XSDs, real MXF roundtrips through asdcplib (including an encrypted-wrap roundtrip that proves the essence is no longer stored verbatim), DCDM reference-value colour tests, the inverse display transform (DCI white → sRGB white, black → black, neutral mid grey, monotonic), the encrypted-no-key fail-loud path against a real encrypted MXF, packaging XML writers, and the pure-function parsers (ebur128 short-term, x265 HDR10 params, SRT cues). A gated integration test decrypts + decodes + colour-manages a real J2K frame end to end (`cargo test -- --ignored`), and an `icc`-feature test runs a patch through littleCMS.
