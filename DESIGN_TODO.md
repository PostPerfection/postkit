# Planned

- GPU J2K decode path. Prerequisite for real-time preview and for the features that
  gate on it: SDI output, and the dcpdoctor/wizard player controls (loop dom#2700,
  speed dom#2917, markers dom#2893, waveform dom#3091, 3D view modes
  dom#1974/dom#3165, A/V sync offset dom#3083). CPU openjpeg/grok can't sustain 4K
  at frame rate.
- SDI output via Blackmagic DeckLink (easyDCP Player+ parity). A playback sink
  pushing decoded, colour-managed frames to an SDI board for reference monitoring.
  FFI to the DeckLink SDK (COM-style C++, likely a C shim) in a separate crate,
  mirroring asdcplib-sys: open DeckLinkOutput, schedule frames at the board clock,
  embed PCM from the sound MXF, reusing the preview decode + colour transform as the
  frame source. Needs genlock-accurate scheduling and the physical board to verify.
  Gates on GPU J2K decode.
- DTS:X. Would ride the generic DCData (ST 429-14) aux path, but the correct
  DataEssenceCoding UL could not be confirmed from asdcplib sources or SMPTE docs, so
  no `wrap_dcdata` was added rather than emit a wrong UL. Revisit once a confirmed UL
  exists.

## App-side dedup still pending (no further postkit work needed)

- hash: dcpdoctor-core/hash.rs can adapt onto `postkit::hash::hash_file`;
  dcpdoctor-wasm is pure-bytes and deliberately avoids postkit (postkit has no
  bytes-based hash).
- imfwizard to_dcp.rs: mechanical switch of the hand-rolled DCP CPL to
  `packaging::DcpCpl`. The escaper and per-reel ScreenAspectRatio blockers are gone.
- imfwizard cpl.rs inject_locale_list: obsolete. Set `ImfCpl.languages` and drop the
  string-splice helper (postkit emits the identical LocaleList block).

Cross-repo pins current at cleanup: dcpwizard a0c51aa, imfwizard 1afde88,
dcpdoctor 37540f3, dci-ctp 175c488. dcpwizard consumes the subtitle parsers via its
vendored postkit copy (dcpwizard-core/src/subtitle.rs). imfwizard and dcpdoctor pin
bumps for the uncommitted 07-23 dedup (j2k/bitrate/timecode/frame_compare) are
pending.

# Done

## 2026-07-23

Subtitle input parsers (b67e038, src/subtitle_formats/): a `StyledCue` model
(start/end ms matching `parse_srt`, `StyledRun`s with italic/bold/underline/colour,
optional HAlign/VAlign/vposition, optional bitmap image) plus `ass::parse_ass` (V4+
styles + inline `\i \b \u \an` tags, unsupported tags collected as warnings,
dom#1462), `fcpxml::parse_fcpxml` (caption/title, rational offset/duration timing,
per-run text-style-def styling, dom#2909), `mks::parse_mks` (Matroska via
ffprobe/ffmpeg extract to srt/ass then reparse, dom#3131), plus `pac`, `interop` PNG
bitmap subs, `bidi` RTL shaping, and `wrap` line-wrap. `to_srt_cue`/`to_srt_cues`
flatten back to `subtitle_retime::SrtCue` so existing callers are unaffected. Tests
per parser assert styling/timing; MKS skips when ffmpeg/ffprobe are absent.
imfwizard's subtitle-convert wiring (ass/fcpxml/mks to IMSC/TTML keeping
styling+placement) landed the same day.

Sony RAW / X-OCN detection (45d7a59, ingest.rs): now detected-but-undecodable, like
ARRIRAW/R3D/BRAW. `detect_format` opens each .mxf, reads the header partition pack's
EssenceContainers batch, and scans the header-metadata region (bounded by
HeaderByteCount) for Sony's private essence ULs -> `SonyRaw`; non-Sony .mxf still
resolves to `DnxHr` by extension. `is_raw_undecodable` rejects it loud with
`CameraFormat::label()` = "Sony RAW (X-OCN family)". The two ULs are
reverse-engineered by MediaInfo (MediaArea/MediaInfoLib File_Mxf.cpp):
PictureEssenceCoding `06 0e 2b 34 04 01 01 0x 0e 06 04 01 02 04 02 xx`
(Mxf_EssenceCompression:702-717, "Sony RAW SQ") and EssenceContainer
`06 0e 2b 34 04 01 01 0x 0e 06 0d 03 02 01 00 00` (Mxf_EssenceContainer:347-372).
Caveat: these ULs are NOT SMPTE-registered (bmx and ffmpeg have no Sony RAW essence
at all; the register has only Sony's private-use org node). They sit under Sony's
private node and mark the Sony RAW family (X-OCN + older linear RAW) without
distinguishing the X-OCN ST/LT/XT tiers, so the match is "Sony RAW family", not an
X-OCN-tier claim. Byte 7 (registry version) is wildcarded. Synthetic KLV fixtures
fire `SonyRaw` on a Sony container UL and on a Sony picture-coding UL, and stay
`DnxHr` on JPEG2000 ULs.

RGB->X'Y'Z' harmonization with grok (32838ea): postkit's matrix was aligned to the
grok/libdcp/DoM reference (gamma 2.2 display-referred linearization + the sRGB/D65
primaries matrix + 48/52.37 companding + 2.6 encode), replacing the old gamma-2.4
path. Both encode paths now agree: whether grok applies its internal XYZ transform
or postkit pre-transforms and feeds XYZ, BT.709 red lands on grok's published 12-bit
`[2817, 2183, 870]`. `colour::tests_xyz::red_matches_reference` asserts that
agreement (<=2 codes) plus an independent f64 reference;
`dcdm::rec709_mid_grey_uses_gamma_2_2` locks the same gamma-2.2 linearization in the
file pipeline.

P3-D65 DCDM target (ee1e70f, dcdm.rs): `DcdmOptions.target: DcdmTarget` (default
`Xyz`, byte-identical to before) adds a `P3D65` output. The XYZ->P3-D65 matrix is
derived from the P3 primaries + D65 white, cross-checked against the published SMPTE
RP 431-2 matrix within 1e-6, composed with the source->XYZ matrix so a source RGB
frame lands in linear P3-D65, encoded with 2.6 gamma. It is a mastering target (P3
primaries, D65 white), not a DCDM: source white maps to full-scale neutral, no
48/52.37 DCI companding. Exposes `--target p3-d65` for dcpwizard.

Packaging AnnotationText + KDM annotation (8bb2296): `packaging::PackingList` and
`packaging::AssetMap` gained optional `annotation: Option<String>`, emitted as
`<AnnotationText>` right after `<Id>` (the ST 429-8/-9 and IMF ST 2067-2/429-9
slot). `certificate::KdmConfig` gained `annotation: Option<String>` overriding the
derived `"<title> KDM for <recipient>"` ETM AnnotationText. All default None and are
byte-identical to before. Removes dcpwizard combine.rs's string-injection workaround
(dom#2027).

HDR/WCG MXF metadata (0813c64): `mxf_wrap::MxfWrapOptions` gained
`hdr: Option<asdcplib::jp2k::HdrMetadata>` (serde-skipped, like `encryption`); when
set on a J2K wrap, `wrap_j2k` calls `jp2k::MxfWriter::open_write_hdr` instead of
`open_write`, writing transfer/colour/ST-2086 onto the RGBA essence descriptor.
Needs the asdcplib pin at or past 6d7b8ca. The CPL side of HDR/WCG is deliberately
not synthesised here (see the 07-21 ImfCpl note).

Dedup landed (uncommitted, app pin bumps pending):
- j2k: `J2kHeader` gained per-component `bit_depths`, code-block width/height +
  exponents, `irreversible_transform`, `mct`, `guard_bits`, and
  `tlm_present`/`poc_present`/`tile_part_count`; `parse_j2k_header` fills them
  (additive). New MXF-extraction entry points `read_mxf_j2k_frame(path, frame)` and
  `parse_j2k_from_mxf(path, frame)` via the asdcplib jp2k reader (unencrypted).
  dcpdoctor switched: dropped `parse_cod_extras`, and `analyze_j2k_from_mxf` prefers
  `read_mxf_j2k_frame` with an ffprobe fallback for AS-02/OP1a essence the OP-Atom
  reader can't open.
- bitrate: `j2k::analyse_mxf_bitrate(path) -> MxfBitrateStats` reads frame sizes via
  the asdcplib picture descriptor + read_frame loop. dcpdoctor switched:
  `FrameBitrateStats` is a type alias and `analyze_picture_bitrate` delegates to it.
  The Note-producing `check_bitrate_compliance` stays app-side.
- timecode: `timecode::Timecode` (hours/minutes/seconds/frames/fps/drop_frame) with
  new/parse/to_frames/from_frames/Display, a superset of imfwizard-core's API (SMPTE
  drop-frame at 30/60 fps). imfwizard switched and deleted its local copy.
- frame_compare: per-frame `compare_frames(ref, dist)` returns PSNR (y/u/v/avg) +
  SSIM (y/avg), plus `VmafScore`/`ffmpeg_has_libvmaf`/`compute_vmaf` and the pooled
  `compute_quality` wrapper. ffmpeg stat output parsed by whitespace split, no regex.
  imfwizard and dcpdoctor both switched and deleted their local copies.
- Package diff: MOOT. imfwizard's imp_diff was a dead module (deleted) and dcpwizard
  has no dcp_diff; only dcpdoctor-core/diff.rs remains (single consumer), so nothing
  to dedup.

Also: stereo -> 5.1 upmix and equal-power crossfade join (d77ebff, dom#921/#1080,
dom#374); font subsetting to a code-point set for the 640 KB embed limit (b67e038,
dom#1023); mid-side WAV decode and a resumable encode pipeline (0d8daee);
caller-supplied resource ids in timed-text wrap (1f0811f).

## 2026-07-22

Imported KDM decryption (aba7c12, certificate.rs), the inverse of KDM generation.
`parse_kdm` reads a KDM's public metadata (format, CPL id, validity window,
KeyIds/types) without a key; `unwrap_kdm`/`unwrap_kdm_file` RSA-OAEP-decrypt every
EncryptedKey with the recipient private key, parse the plaintext block (138-byte
SMPTE / 134-byte Interop) and return the KeyId -> 16-byte AES key map. Key hygiene:
content keys are private, no derived Debug (manual redacting impl), and every key
plus the decrypted buffer is zeroed on drop (zeroize). A wrong key fails loud at the
OAEP unpad or the structure-id check, never returning garbage. Round-trip tests
(both formats), a wrong-key negative test, a Debug-redaction test. Unblocks
dcpdoctor's verify-encrypted-DCPs gap (dom#2971/dom#1957).

Leq(m) in loudness (aba7c12, dom#3092): `loudness::{leq_m_from_samples,
measure_leq_m, LeqMResult}`. dcpdoctor-core dropped its own CCIR 468 weighting +
level math (and the rustfft dep) and re-exports these.

Loudness gain adjustment (e3743a2): `loudness::{plan_gain, apply_gain}` for
measure-then-correct level trims.

## 2026-07-21

DCI JPEG 2000 validation before DCP wrapping (fb322e1/337adb5): `wrap_j2k` runs
`j2k::validate_dci_header` on every input frame, rejecting non-DCI RSIZ profiles
(2K = 1/3, 4K = 2/4), out-of-bounds dimensions, component count != 3, and non-12-bit
unsigned components. Gated to `MxfStandard::AsDcp`; AS-02 (IMF) only checks for a
non-empty image area, so imfwizard's non-DCI J2K is unaffected.

Grok multi-core encode fix + openjpeg removal (fd477a5): grok's compress scheduler
always parallelises T1 across the global TFSingleton pool (per-codec
`num_threads` is ignored on the compress path), so the old min(4) encoder-thread cap
left cores idle. encode_pipeline now forces the global pool to inline mode
(`grk_initialize(1)`) and runs one encoder thread per core, restoring the pool on
exit. 8-core grok/opj went 0.78x/0.79x -> 1.15x (2K) / 1.00x (4K). openjpeg then
removed: the `openjpeg` feature, openjpeg-sys dep, and openjpeg_encoder.rs are gone;
shared types already lived in grok_encoder. CI replaced the stale `async openjpeg`
steps with a grok-ffi build/test/clippy triple gated on a cached "Setup grok" step
(builds grok v20.3.6 from source; linux + macOS only, windows runs plain `async`).

ImfCpl CPL extensions (fd477a5): optional composition `languages` (ST 2067-3
LocaleList, replaces imfwizard's string-edited injection), optional
`essence_descriptors` (EssenceDescriptorList carrier) plus per-resource
`source_encoding`, for per-track audio MCA/soundfield + RFC 5646 language and image
color/HDR-WCG. postkit carries the descriptor body verbatim; the UL-coded MXF
descriptor internals come from asdcplib. All default to byte-identical output.
HDR/WCG CPL synthesis (ST 2067-21 RGBA descriptor color/mastering-display ULs) uses
the same carrier but is not emitted by postkit: those values can only be
lax-validated against the CPL XSD and belong to the MXF descriptor, so the CPL body
is left to the wizard/asdcplib.

Downstream DCP wrap + KDM additions (efd8151, for dcpwizard):
- HDR-to-DCI delivery policy: dcpwizard requires a supplied 3D LUT for detected HDR
  source video and calls `colour::convert_colour` with it. Generic FFmpeg tone
  mapping is opt-in and warns; postkit does not select it by default.
- Stereoscopic 3D (ST 429-10): `mxf_wrap::wrap_stereoscopic` wraps left/right J2K
  frame sequences into one stereoscopic picture MXF (both-eyes roundtrip test).
- MCA labels (SMPTE 377-4 / ST 429-12): PCM wrap gained
  `MxfWrapOptions.mca_config` (asdcp-wrap style string, e.g.
  "51(L,R,C,LFE,Ls,Rs),HI,VIN"; AS-DCP only, else errors).
  `mca::soundfield_to_mca_config` builds the string from the typed soundfields.
  Needed the asdcplib shim `pcm::MxfWriter::open_write_mca` +
  `pcm::MxfReader::mca_labels`.
- Atmos (ST 429-18): `wrap_atmos` fills the correct DataEssenceCoding UL. Added a
  container-structure roundtrip test (synthetic DCData). Essence-level conformance
  is unverified: needs real Atmos material.
- Interop KDM: `KdmConfig.format: KdmFormat` (Smpte default; Interop opt-in). Interop
  drops the 4-byte KeyType (138 -> 134), uses the digicine KDMRequiredExtensions
  namespace, and emits bare KeyId elements. Round-tripped + xmlsec1-verified. NOT
  validated against real legacy gear: validate before production use.

## 2026-07-20

Honest-API sweep (9c417b9): each overselling claim from 07-19 is now honest or fails
loud. ingest rejects undecodable camera RAW; hdr10-inject re-encodes with libx265 to
write real SEI; convert_colour rejects XYZ/ACES/ACEScg/LogC without a LUT; trailer
concatenates the ratings card and no longer fakes a cpl_uuid; preview reads the real
frame rate; loudness computes short_term_max via ebur128; accessibility is documented
as a heuristic; the watermark "forensic" wording is gone. The escape helper, DCP/IMF
PKL and ASSETMAP writers, DCP and IMF CPL writers, and a standalone SRT parser now
live in postkit.

Differential testing vs clairmeta ECL reference DCPs (a97336c) fixed two real bugs:
(1) xmldsig hardcoded SHA-256 for both the reference digest and the RSA signature, so
the 12 SHA-1-signed ECL DCPs were falsely rejected; verify now reads the declared
DigestMethod/SignatureMethod and dispatches (sha1/256/384/512, rsa-sha1/256/384/512),
failing loud on anything else. Signing stays SHA-256. (2) packaging writers emitted
schema-invalid XML: ASSETMAP omitted the required IssueDate/Issuer and DcpCpl put
ContentTitleText before IssueDate. Fixed to ST 429-7/8/9 element order with the
required IssueDate/Issuer/ContentVersion/empty RatingList; AssetMap orders its
metadata by namespace. Verified with xmllint against the SMPTE XSDs and 58 real
signed ECL CPL/PKL.

Colour-managed DCP preview (c017c05): preview resolves a DCP/CPL/MXF, decrypts
encrypted picture essence in Rust, decodes J2K via ffmpeg and applies the inverse
DCDM transform (X'Y'Z' -> sRGB, or a monitor ICC via the `icc` feature). Real-time
projector-grade playback stays out of scope (CPU decode, decode-to-intermediate
first); the GPU J2K decode path is the prerequisite (see Planned).
