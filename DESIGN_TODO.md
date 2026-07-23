# Planned

The 2026-07-20 overselling items are fixed (each claim is now honest or fails
loud): ingest rejects undecodable camera RAW, hdr10-inject re-encodes with
libx265 to write real SEI, convert_colour rejects XYZ/ACES/ACEScg/LogC without a
LUT, trailer concatenates the ratings card and no longer fakes a cpl_uuid,
preview reads the real frame rate and errors on options it can't honour, loudness
computes short_term_max via ebur128, accessibility is documented as a heuristic,
shell_completion's docstring no longer claims clap_complete, and the watermark
"forensic" wording is gone from the docs. The escape helper, DCP/IMF PKL and
ASSETMAP writers, DCP and IMF CPL writers, and a standalone SRT parser now live
in postkit.

Differential testing against clairmeta (its ECL reference DCPs) fixed two real
bugs 2026-07-20: (1) xmldsig hardcoded SHA-256 for both the reference digest and
the RSA signature, so the 12 SHA-1-signed ECL DCPs were falsely rejected; verify
now reads the declared DigestMethod/SignatureMethod and dispatches (sha1/256/384/
512, rsa-sha1/256/384/512), failing loud on anything else. Signing stays SHA-256.
(2) packaging writers emitted schema-invalid XML: ASSETMAP omitted the required
IssueDate/Issuer and DcpCpl put ContentTitleText before IssueDate. Fixed to ST
429-7/8/9 element order with the required IssueDate/Issuer/ContentVersion/empty
RatingList, and AssetMap now orders its metadata by namespace. Verified with
xmllint against the SMPTE XSDs and against 58 real signed ECL CPL/PKL.

## DoM tracker gaps (2026-07-22)

Library-level capabilities behind feature requests in the DCP-o-matic Mantis
tracker (dom#N = https://dcpomatic.com/bugs/view.php?id=N); the wizards and
dcpdoctor expose them (see their DESIGN_TODOs, same date).

- Leq(m) (dom#3092): DONE 2026-07-23. dcpdoctor-core dropped its CCIR 468
  weighting + level math (and the rustfft dep) and re-exports
  `postkit::loudness::{leq_m_from_samples, measure_leq_m, LeqMResult}`.
- Player direction (dom#2700 loop, dom#2917 speed, dom#2893 markers, dom#3091
  waveform, dom#1974/dom#3165 3D view modes, dom#3083 A/V sync offset): all gate
  on the GPU J2K decode path already noted under the SDI/DeckLink item.

## Planned / not started

(nothing open)

Subtitle input parsers landed 2026-07-23 (b67e038, src/subtitle_formats/): a
`StyledCue` model (start/end ms matching `parse_srt`, `StyledRun`s with
italic/bold/underline/colour, optional HAlign/VAlign/vposition, optional bitmap
image) plus parsers `ass::parse_ass` (V4+ styles + inline \i \b \u \an override
tags, unsupported tags collected as warnings, dom#1462), `fcpxml::parse_fcpxml`
(caption/title, rational offset/duration timing, per-run text-style-def styling,
dom#2909), and `mks::parse_mks` (Matroska via ffprobe/ffmpeg extract to srt/ass
then reparse, dom#3131), alongside `pac`, `interop` PNG bitmap subs, `bidi` RTL
shaping, and `wrap` line-wrap. `to_srt_cue`/`to_srt_cues` flatten back to
`subtitle_retime::SrtCue` so existing callers are unaffected. dcpwizard already
consumes these via its vendored postkit copy (dcpwizard-core/src/subtitle.rs);
imfwizard's subtitle-convert wiring landed 2026-07-23 (its DESIGN_TODO; pin bump
pending): ass/fcpxml/mks now convert to IMSC/TTML keeping styling+placement.
Tests per parser assert styling/timing; MKS skips when ffmpeg/ffprobe are absent.

Sony RAW / X-OCN detection landed 2026-07-23 (ingest.rs): now detected-but-
undecodable, like ARRIRAW/R3D/BRAW. `detect_format` opens each .mxf, reads the
header partition pack's EssenceContainers batch, and scans the header-metadata
region (bounded by HeaderByteCount) for Sony's private essence ULs -> `SonyRaw`;
non-Sony .mxf still resolves to `DnxHr` by extension. `is_raw_undecodable` already
covered `SonyRaw`, so ingest rejects it loud. Error/logs now use
`CameraFormat::label()` -> "Sony RAW (X-OCN family)".

The two ULs are reverse-engineered by MediaInfo (MediaArea/MediaInfoLib
Source/MediaInfo/Multiple/File_Mxf.cpp): PictureEssenceCoding
`06 0e 2b 34 04 01 01 0x 0e 06 04 01 02 04 02 xx` (Mxf_EssenceCompression:702-717,
"Sony RAW SQ") and EssenceContainer `06 0e 2b 34 04 01 01 0x 0e 06 0d 03 02 01 00
00` (Mxf_EssenceContainer:347-372; label def File_Mxf_Automated.h:5066,10047).
Caveats recorded in the code: bmx (bbc/bmx `EssenceType`/mxf_helper) and ffmpeg
(mxf.c/mxfdec.c) have no Sony RAW essence at all; the SMPTE register has only Sony's
private-use org node, so these ULs are NOT SMPTE-registered. They sit under Sony's
private node and mark the Sony RAW family (X-OCN + older linear RAW) without
distinguishing the X-OCN ST/LT/XT tiers, so the match is "Sony RAW family", not an
X-OCN-tier claim. Byte 7 (registry version) is wildcarded. Tests: synthetic KLV
fixtures fire `SonyRaw` on a Sony container UL, on a Sony picture-coding UL, and
stay `DnxHr` on JPEG2000 ULs. Not used: asdcplib's readers only open
JP2K/PCM/TimedText essence, so they can't probe a Sony RAW MXF.

P3-D65 DCDM target landed 2026-07-23 (dcdm.rs): `DcdmOptions.target: DcdmTarget`
(default `Xyz`, byte-identical to before) adds a `P3D65` output. The XYZ->P3-D65
matrix is derived from the P3 primaries + D65 white (`rgb_to_xyz_matrix`/`invert3`,
cross-checked against the published SMPTE RP 431-2 matrix within 1e-6), composed
with the source->XYZ matrix so a source RGB frame lands in linear P3-D65, encoded
with 2.6 gamma. It is a mastering target (P3 primaries, D65 white), not a DCDM:
source white maps to full-scale neutral, no 48/52.37 DCI companding. Exposes
`--target p3-d65` for dcpwizard (mechanical routing alongside the existing `xyz`
DCDM path). Tests: matrix vs published, rec709 white -> full-scale neutral, rec709
red vs f64 reference, and P3-D65 differs from the XYZ target.

RGB->X'Y'Z' harmonization with grok landed 2026-07-23 (32838ea): postkit's matrix
was aligned to the grok/libdcp/DoM reference (gamma 2.2 display-referred
linearization + the sRGB/D65 primaries matrix + 48/52.37 companding + 2.6 encode),
replacing the old gamma-2.4 path. Both encode paths now agree: whether grok applies
its internal XYZ transform (`apply_xyz_transform`) or postkit pre-transforms and
feeds XYZ, BT.709 red lands on grok's published 12-bit `[2817, 2183, 870]`.
`colour::tests_xyz::red_matches_reference` asserts that agreement (<=2 codes) plus
an independent f64 reference; `dcdm::rec709_mid_grey_uses_gamma_2_2` locks the same
gamma-2.2 linearization in the file pipeline.

Packaging AnnotationText + KDM annotation landed 2026-07-23:
`packaging::PackingList` and `packaging::AssetMap` gained an optional
`annotation: Option<String>`, emitted as `<AnnotationText>` right after `<Id>`
(the schema slot in ST 429-8/-9 and the shared IMF ST 2067-2 PKL / 429-9 AM).
`certificate::KdmConfig` gained `annotation: Option<String>` overriding the
derived `"<title> KDM for <recipient>"` ETM AnnotationText. All three default
None and are byte-identical to before (existing writer/XSD/xmlsec1 tests plus new
None-vs-Some diff tests prove it). This removes dcpwizard combine.rs's
string-injection workaround (dom#2027). Extended the gated DCP-XSD test with
annotated PKL/ASSETMAP and the KDM round-trip with an annotation override.

DCI JPEG 2000 validation before DCP wrapping (done 2026-07-21, fb322e1/337adb5):
`mxf_wrap::wrap_j2k` runs `j2k::validate_dci_header` on every input frame before
writing, rejecting non-DCI RSIZ profiles (2K = 1/3, 4K = 2/4 per dcpdoctor's
convention), out-of-bounds dimensions, component count != 3, and non-12-bit
unsigned components. Gated to `MxfStandard::AsDcp`; AS-02 (IMF) only checks for a
non-empty image area, so imfwizard's non-DCI J2K is unaffected. Tests: a
conformant profile-3 codestream wraps, a Profile-None codestream is rejected.

Imported KDM decryption landed 2026-07-22 (certificate.rs): the inverse of the
KDM generation path. `parse_kdm` reads a KDM's public metadata (format, CPL id,
validity window, KeyIds/types) without a key; `unwrap_kdm`/`unwrap_kdm_file`
RSA-OAEP-decrypt every EncryptedKey with the recipient private key, parse the
plaintext block (138-byte SMPTE / 134-byte Interop) and return the KeyId ->
16-byte AES key map as an `UnwrappedKdm`. Key hygiene: content keys are private
(read via `content_key`), no derived Debug (manual redacting impl), and every
key plus the decrypted key-block buffer is zeroed on drop (zeroize, already in
the tree via rsa). A wrong recipient key fails loud at the OAEP unpad or the
structure-id check, never returning garbage. The XML parser is now shared with
re-wrap, and the block parser handles both formats. Round-trip tests (both
formats, caller-chosen key bytes asserted) plus a wrong-key negative test and a
Debug-redaction test. Unblocks dcpdoctor's verify-encrypted-DCPs gap
(dom#2971/dom#1957) and dcpwizard's "Encrypted DCP derivatives".

Grok multi-core encode fixed 2026-07-21: grok's compress scheduler always
parallelises T1 across the global TFSingleton pool (per-codec cparams.num_threads
is ignored on the compress path), so the old min(4) encoder-thread cap left cores
idle and lost to openjpeg at 8 cores. encode_pipeline now forces the global pool
to inline mode (grk_initialize(1) => each grk_compress runs its whole taskflow on
the calling thread) and runs one encoder thread per core, restoring the pool on
exit. 8-core grok/opj went 0.78x/0.79x -> 1.15x (2K) / 1.00x (4K). openjpeg then
removed: the `openjpeg` feature, openjpeg-sys dep, and src/openjpeg_encoder.rs are
gone; shared types (BoundedQueue, CompressParams, RawFrame, ...) already lived in
grok_encoder. bench_j2k is now a grok-only rerunnable perf check. ci.yml follows:
the stale `async openjpeg` feature steps are replaced by a grok-ffi build/test/clippy
triple, gated on a cached "Setup grok" step that builds grok v20.3.6 from source
(cmake) and exports PKG_CONFIG_PATH/LD_LIBRARY_PATH. linux + macOS only; windows
runs the plain `async` passes since the grok source build there is not wired up.

ImfCpl gained optional composition `languages` (ST 2067-3 LocaleList, replaces
imfwizard's string-edited injection), optional `essence_descriptors`
(EssenceDescriptorList carrier) plus per-resource `source_encoding`, for per-track
audio MCA/soundfield + RFC 5646 language and image color/HDR-WCG. postkit carries
the descriptor body verbatim; the UL-coded MXF descriptor internals come from
asdcplib, not synthesised here. All default to byte-identical output. Validated
with xmllint against imf-cpl-20160411.xsd. HDR/WCG CPL synthesis (ST 2067-21 RGBA
descriptor color/mastering-display ULs) uses the same carrier but is not emitted
by postkit: those values can only be lax-validated against the CPL XSD and belong
to the MXF descriptor, so the CPL body is left to the wizard/asdcplib.

The MXF side of HDR/WCG is wired here (2026-07-23): `mxf_wrap::MxfWrapOptions`
gained an `hdr: Option<asdcplib::jp2k::HdrMetadata>` field (serde-skipped, like
`encryption`); when set on a J2K wrap, `wrap_j2k` calls
`jp2k::MxfWriter::open_write_hdr` instead of `open_write`, writing the
transfer/colour/ST-2086 values onto the RGBA essence descriptor. Needs the
asdcplib pin at or past the HDR commit (6d7b8ca).

- Colour-managed DCP preview landed: preview now resolves a DCP/CPL/MXF,
  decrypts encrypted picture essence in Rust, decodes J2K via ffmpeg and applies
  the inverse DCDM transform (X'Y'Z' → sRGB, or a monitor ICC via the `icc`
  feature). Real-time projector-grade playback is still out of scope (CPU decode,
  decode-to-intermediate first). A GPU J2K decode path is the prerequisite for
  real-time.
- SDI output via Blackmagic DeckLink (future, not started). A playback sink that
  pushes decoded, colour-managed frames to an SDI board for reference monitoring
  (easyDCP Player+ parity). Plumbing mirrors asdcplib-sys: FFI to the DeckLink SDK
  (COM-style C++ API, likely a C shim) in a separate crate, open DeckLinkOutput,
  schedule frames at the board clock, embed PCM from the sound MXF. Reuses this
  preview pipeline's decode + colour transform as the frame source. Blocker:
  real-time J2K at frame rate needs GPU decode (CPU openjpeg/grok won't sustain
  4K), so SDI gates on a GPU J2K decode path; also needs genlock-accurate
  scheduling and the physical board to verify. Effort: medium for the SDI
  plumbing (2K), large once GPU decode + 4K + genlock are required.

## Dedup that is NOT a straight switch (needs postkit API work first)

Audited 2026-07-20: these app copies are not drop-in supersets of the postkit
modules, so they can't just switch. Extract only after extending the postkit API.

- j2k: DONE 2026-07-23 (uncommitted, app pin bump pending). `J2kHeader` gained the
  richer fields both apps carried: per-component `bit_depths`, code-block
  width/height + exponents, `irreversible_transform`, `mct`, `guard_bits`, and the
  `tlm_present`/`poc_present`/`tile_part_count` marker/tile info. `parse_j2k_header`
  fills them all (existing callers unaffected, fields are additive). Added the
  MXF-extraction entry points `read_mxf_j2k_frame(path, frame)` and
  `parse_j2k_from_mxf(path, frame)` via the asdcplib jp2k reader (unencrypted).
  dcpdoctor-core/j2k.rs can drop its `parse_cod_extras`; dcpdoctor-wasm stays
  pure-bytes by choice. Tests build full SOC/SIZ/COD/QCD/TLM + tile-part streams
  and assert every new field for 2K (3 parts) and 4K (6 parts).
  dcpdoctor switched 2026-07-23: dropped `parse_cod_extras`, `J2kCodestreamInfo`
  built off `parse_j2k_header`, and `analyze_j2k_from_mxf` prefers
  `read_mxf_j2k_frame` (real codestream) with an ffprobe fallback for AS-02/OP1a
  essence the OP-Atom reader can't open (noted in dcpdoctor's DESIGN_TODO).
- bitrate: DONE 2026-07-23 (uncommitted, pin bump pending). Added
  `j2k::analyse_mxf_bitrate(path) -> MxfBitrateStats` reading frame sizes via the
  asdcplib picture descriptor + read_frame loop, mirroring dcpdoctor-core
  bitrate.rs's `FrameBitrateStats` fields (valid/error/frame_count/width/height/
  frame_rate/total+min+max bytes/max_frame_index/avg+min+max mbps). The
  Note-producing `check_bitrate_compliance` stays app-side (uses dcpdoctor's
  Note/Code). The path-slice `analyse_bitrate` is unchanged. dcpdoctor switched
  2026-07-23: `FrameBitrateStats` is a type alias for `MxfBitrateStats` and
  `analyze_picture_bitrate` delegates to `analyse_mxf_bitrate`.
- hash: dcpdoctor-core/hash.rs (sha1_base64/sha1_hex over a path) can adapt onto
  postkit::hash::hash_file cleanly; dcpdoctor-wasm/hash.rs is pure-bytes and
  postkit has no bytes-based hash. dcpdoctor-wasm deliberately avoids postkit.
- timecode: DONE 2026-07-23 (uncommitted, pin bump pending). Added a
  `timecode::Timecode` struct (hours/minutes/seconds/frames/fps/drop_frame) with
  `new`/`parse`/`to_frames`/`from_frames`/Display, a superset of imfwizard-core's
  API (SMPTE drop-frame compensation at 30/60 fps). Ported imfwizard's tests.
  imfwizard switched to it and deleted its local copy 2026-07-23.
- frame_compare: DONE 2026-07-23 (uncommitted, pin bump pending). New
  `postkit::frame_compare` module. Per-frame core `compare_frames(ref, dist)` returns
  per-frame PSNR (y/u/v/avg) + SSIM (y/avg) (imfwizard drop-in), plus `VmafScore`/
  `ffmpeg_has_libvmaf`/`compute_vmaf` and the pooled `QualityMetrics`/`QualityOptions`/
  `compute_quality` wrapper (dcpdoctor). ffmpeg stat output is parsed by whitespace
  split, no regex (the regex was only ffmpeg-output parsing, not an app concern).
  dcpdoctor's threshold scoring (CompareOptions/FrameDiff `significant`) stays a thin
  app wrapper over `compare_frames`'s per_frame + a threshold. Tests cover the log
  and token parsers with sample ffmpeg output (no ffmpeg needed).
  imfwizard switched to it and deleted its local copy 2026-07-23. dcpdoctor
  switched 2026-07-23: `compare_files` is now a thin threshold wrapper over
  `compare_frames`/`compute_vmaf`; its unused `QualityMetrics`/`compute_quality`
  copies were deleted.
- Package diff: MOOT (verified 2026-07-23). The premise was stale: imfwizard's
  imp_diff was deleted as a dead module (zero callers) and dcpwizard has no dcp_diff.
  The only remaining diff is dcpdoctor-core/diff.rs (DCP-only, single consumer, built
  on dcpdoctor's own DCP model + hash). With one consumer there is nothing to dedup,
  so no postkit module was added.
- imfwizard to_dcp.rs DCP CPL/PKL/ASSETMAP writers: only the escaper switched to
  postkit. DcpCplReel now carries per-reel picture_width/height and emits a
  per-asset ScreenAspectRatio (SMPTE Rational, Interop decimal), so that blocker
  is gone; the remaining work is the mechanical switch of to_dcp's hand-rolled
  CPL to postkit::packaging::DcpCpl.
- imfwizard cpl.rs inject_locale_list: obsolete. Set ImfCpl.languages and drop the
  string-splice helper (postkit emits the identical LocaleList block).

Downstream DCP wrap + KDM additions 2026-07-21 (for dcpwizard):
- HDR-to-DCI delivery policy: dcpwizard requires a supplied 3D LUT for detected HDR
  source video and calls `colour::convert_colour` with that LUT. Generic FFmpeg
  tone mapping is opt-in and warns; postkit does not select it by default.
- Stereoscopic 3D (ST 429-10): `mxf_wrap::wrap_stereoscopic(&StereoscopicWrapOptions)`
  wraps left/right J2K frame sequences into one stereoscopic picture MXF. The
  asdcplib-rs JP2K MXFS writer/reader binding already existed; this is the postkit
  entry plus a both-eyes roundtrip test.
- MCA labels (SMPTE 377-4 / ST 429-12): PCM wrap gained `MxfWrapOptions.mca_config`
  (asdcp-wrap style string, e.g. "51(L,R,C,LFE,Ls,Rs),HI,VIN"; AS-DCP only, else
  errors). `mca::soundfield_to_mca_config(&McaSoundfield)` builds the string from
  the existing typed soundfields (HI/VI-N emitted as standalone channels). Needed a
  new asdcplib-rs shim: `pcm::MxfWriter::open_write_mca` +
  `pcm::MxfReader::mca_labels`. Roundtrip test reads back 6 channel labels, 1
  soundfield group, and the MCA ChannelAssignment UL.
- Atmos (ST 429-18): `wrap_atmos` (EssenceType::Atmos) already existed and fills
  the correct DataEssenceCoding UL (asdcplib overwrites the zero placeholder). Added
  a container-structure roundtrip test using a synthetic DCData payload. Real Atmos
  essence cannot be synthesised, so essence-level conformance is unverified: needs
  real Atmos material.
- DTS:X: NOT implemented. It would ride the generic DCData (ST 429-14) aux path, but
  the correct DataEssenceCoding UL could not be confirmed from asdcplib sources or
  SMPTE docs, so no wrap_dcdata was added rather than emit a wrong UL. Revisit once a
  confirmed UL exists.
- Interop KDM: `certificate::KdmConfig.format: KdmFormat` (Smpte default,
  byte-identical to before; Interop is opt-in). Interop drops the 4-byte KeyType from
  the key block (138 -> 134), uses the digicine KDMRequiredExtensions namespace, and
  emits bare KeyId elements without TypedKeyId. Test round-trips a 134-byte block
  (RSA-OAEP decrypt, every field asserted) and xmlsec1-verifies the signature.
  NOT validated against real legacy gear: validate before production use.
