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

- Leq(m) (dom#3092): dcpdoctor-core still has its own copy of the CCIR 468
  weighting + level math; a later pass switches it to postkit::loudness.
- Player direction (dom#2700 loop, dom#2917 speed, dom#2893 markers, dom#3091
  waveform, dom#1974/dom#3165 3D view modes, dom#3083 A/V sync offset): all gate
  on the GPU J2K decode path already noted under the SDI/DeckLink item.

## Planned / not started

- DCI JPEG 2000 validation before DCP wrapping: `mxf_wrap::wrap_j2k` parses the
  codestream header but does not reject non-DCI `RSIZ` profiles. Validate the
  profile, dimensions, and other DCI-required codestream fields before writing
  an MXF so an IMF or Profile None J2K input cannot become an invalid DCP.

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
with xmllint against imf-cpl-20160411.xsd. HDR/WCG synthesis (ST 2067-21 RGBA
descriptor color/mastering-display ULs) uses the same carrier but is not emitted
by postkit: those values can only be lax-validated against the CPL XSD and belong
to the MXF descriptor, so they are left to the wizard/asdcplib.

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

- j2k: dcpdoctor-core/j2k.rs adds DCI validation + MXF extraction and a richer
  `J2kCodestreamInfo`; dcpdoctor-wasm/j2k.rs is pure-bytes with extra fields
  (bit_depths Vec, codeblock dims, marker flags) and parses J2K out of MXF.
  postkit::j2k::parse_j2k_header is a smaller pure-bytes header parser. To unify,
  postkit would need the extra fields and an MXF-extraction entry point.
- bitrate: dcpdoctor-core/bitrate.rs reads MXF descriptors via asdcplib;
  postkit::j2k::analyse_bitrate works on a slice of file paths. Different inputs.
- hash: dcpdoctor-core/hash.rs (sha1_base64/sha1_hex over a path) can adapt onto
  postkit::hash::hash_file cleanly; dcpdoctor-wasm/hash.rs is pure-bytes and
  postkit has no bytes-based hash. dcpdoctor-wasm deliberately avoids postkit.
- timecode: imfwizard-core/timecode.rs is a `Timecode` struct with methods;
  postkit::timecode is free functions. A switch needs a postkit Timecode type.
- frame_compare: dcpdoctor-core and imfwizard-core versions have incompatible
  APIs (CompareOptions/QualityMetrics + regex_lite vs a simple compare_frames
  returning per-frame PSNR/SSIM). Does not drop into one postkit module cleanly;
  left in the apps.
- Package diff: dcpwizard dcp_diff.rs vs imfwizard imp_diff.rs are ~75% similar;
  could become one postkit module, not yet attempted.
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
