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

## Planned / not started

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

## Consumers still to switch (other repos, later phase)

- escape_xml: 5 private copies (dcpwizard cpl/subtitle/vf, imfwizard cpl/to_dcp)
  should switch to `postkit::packaging::escape_xml`.
- CPL/PKL/ASSETMAP: dcpwizard cpl.rs/pkl.rs/assetmap.rs and imfwizard
  cpl.rs/pkl.rs/assetmap.rs should map their configs onto the new
  `postkit::packaging` writers (DcpCpl / ImfCpl / PackingList / AssetMap /
  volindex_xml) and delete their hand-rolled XML.
- SRT: dcpwizard subtitle.rs and imfwizard subtitle_convert.rs should use
  `postkit::subtitle_retime::parse_srt` / `SrtCue`.

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
