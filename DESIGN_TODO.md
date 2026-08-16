# Planned

- Embedded playback via the libmpv render API, for both wizard GUIs. Landed,
  manual verification remaining. Native Wayland has no foreign-window reparenting, so the spawned
  mpv preview can never sit inside the tauri window: mpv --wid is X11/Windows
  only. The route that works is linking libmpv (mpv-libs-devel, pkg-config "mpv",
  2.5.0 on the dev machine) and drawing through mpv_render_context into a
  GtkGLArea placed next to the webview in the tauri gtk window; the GL area must
  use the exact gtk crate versions tao links. The engine half lives in postkit
  (replacing the process-spawning side of the mpv module); each wizard hosts the
  GL area in its own tauri glue, mirroring how preview_server.rs is duplicated.
  State: landed on master and in both wizards, which build with the feature on
  in CI. Both embeds run live on Fedora/Wayland with good performance,
  MPV_RENDER_PARAM_FLIP_Y set (GtkGLArea's FBO is y-flipped, without it video
  renders upside down). Engine: postkit src/mpv_render behind the default-off
  libmpv feature. As of a65ef22 that module builds on every platform, not just
  linux: it is gated on the feature alone, and build.rs links it through
  pkg-config on unix (linux and macos both, homebrew's mpv ships mpv.pc) and
  through an MPV_LIB_DIR search path on windows. Windows needs an import library
  in that directory, mpv.lib for msvc (gendef plus lib.exe over libmpv-2.dll) or
  libmpv.dll.a for mingw. The module type-checks for windows-msvc, windows-gnu
  and aarch64-apple-darwin, but nothing about linking or running off linux is
  verified. Host glue moves out of each wizard into a crate in guikit, see the
  guikit phase 2 entry in the wizards' DESIGN_TODO.
  imfwizard no longer crashes at launch: tauri-runtime-wry
  undecorated_resizing.rs unwraps the window child and finds the embed's
  GtkOverlay, so imfwizard builds its window and webview in code with
  tauri.linux.conf.json declaring no windows, keeping wry off that path
  (dcpwizard's window config dodges it already). Evidence recorded:
  hwdec-current vaapi (needs MPV_RENDER_PARAM_WL_DISPLAY or decode silently
  falls back), zero dropped frames, less CPU than the floating window; libmpv
  requires LC_NUMERIC "C" before mpv_create. Remaining, untested by hand:
  closing the preview panel (shrinks the GL area to 1x1, the render loop must
  keep answering), and no automated orientation check exists (framebuffer
  readback returns black), so eyeball after any render change. Remaining work
  proper: none on the engine side. The macos and windows host surfaces landed
  in guikit on 2026-08-13 (NSOpenGLView layered over the WKWebView, WS_CHILD
  window with wgl over the WebView2 child), both wizards pin them and their CI
  compiles all three platforms. Neither host has run on real hardware, so a
  hand pass there is the last step. When that passes, fold into DESIGN.md and
  delete this entry.
  Hardware acceleration reality: rendering and colour transforms run as GPU
  shaders, hwdec (VAAPI over EGL) accelerates H.264/HEVC/AV1 sources. J2K stays
  CPU-decoded: no GPU has fixed-function J2K, that is what the GPU decode item
  below is for, and it would hand frames to this same GL surface.
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
- P-HFR gets no separate bitrate limit. `DCI_MAX_BITRATE_MBPS` is the flat DCI
  figure, which is the only one with normative text behind it: DCSS 4.3.3 states
  byte caps per frame and every one works out to 250 Mb/s. Two other numbers are
  in circulation for high frame rates and neither is normative. ISDCF's P-HFR
  paper (v005, 2012) sets 500 Mb/s for the total codestream of 2K stereoscopic
  HFR, keyed on a P-HFR-2K picture essence label, and calls itself a proposal for
  experimental use. asdcplib applies 400 to that same label with no source cited,
  which is stricter than the document defining the label. Applying either would
  mean reading the essence coding UL, which nothing here does. Settle which
  number is right before adding that.
- Picture processing flags in the apps. postkit does the work in
  `picture_processing`, and the callers still have to expose it: dcpwizard's
  `create --crop-*/--auto-crop/--fill-crop/--deinterlace/--rotate/--flip/--denoise`,
  a GUI crop indicator over `PicturePlan::describe`, and dcpwizard-core's
  `check_encode_raster`, which refuses a source raster that is not the encode
  raster rather than building a `Fit` for it. Deliberately out of postkit:
  arbitrary rotation angles, anamorphic desqueeze, field-rate deinterlacing
  (yadif emits one frame per frame here), and border detection during the
  encode, which stays a separate pass a caller runs first because it seeks and
  decodes frames of its own.

# Done

## 2026-08-16

Per-frame source colour transform (colour.rs, grok_encoder.rs, encode.rs): the
DCDM maths that lived privately in dcdm.rs is now `colour::DcdmTransform`, built
once from a `ColourSpace` and applied per frame as `pixel`, `frame_rgb48le` or
`frame_rgb48be_inplace`. `create_dcdm` composes it with the P3-D65 target and
keeps its name parsing; `rgb_to_xyz_inplace` is its Rec.709 one-shot form. A
dcdm_smoke test compares a written DCDM frame against the public transform for
rec709, p3 and rec2020 and requires equality, so the file pipeline and the
encoder cannot drift. Harmonizing the two dropped dcdm's BT.709-derived matrix
for the sRGB/D65 one grok and libdcp use.

The encoder reaches it through `SourceColour::DisplayRgbIn(space)`, which sets
`CompressParams.source_transform` and leaves grok's `apply_xyz_transform` off.
The transform runs in `encoder_thread_fn`, not in the frame producer: a 2K frame
costs ~120 ms of `powf` single-threaded, which would have capped the pipeline at
about 8 fps, and the encoder threads absorb it in parallel instead. Setting both
transforms fails the frame loudly rather than converting twice. The variant only
works where postkit owns the frames: the image-sequence encoder hands each file
to grk_compress, which converts Rec.709 only, and J2K input is already
compressed, so `reject_unsupported_colour_path` refuses both, and the subprocess
encoder refuses it too. Verified end to end in dcpwizard: a solid P3 frame
encoded through the route and decoded with grk_decompress lands within one code
value of an independent f64 computation over the RP 431-2 P3 matrix
([65535,0,0] -> [2901, 2171, 0], where Rec.709 red is [2817, 2183, 870]).

## 2026-08-15

One thumbprint (src/certificate.rs): the whole-certificate hex SHA-1 is gone.
`CertInfo.thumbprint` and `TrustedDevice.thumbprint` now hold the base64 ST 430-2
thumbprint computed through the same `cert_thumbprint` the KDM writer uses, so a
displayed value and a KDM value can no longer differ. An empty thumbprint stays
the "not a valid certificate" sentinel `read_certificate` returns on any parse
failure. The trusted-device store keeps the same digest in two spellings, base64
in the record and hex as the file stem, because base64 contains `/`; both come
from `thumbprint_base64` and `thumbprint_stem` over one digest. Old stores are
migrated in place on every access by recomputing thumbprint and stem from the
stored PEM, which is idempotent and needs no schema marker; a record with no
certificate beside it cannot be recomputed and is left alone. Migration never
fails the operation that triggered it. `remove_trusted_device` takes the base64
thumbprint and finds the files by reading the records, not by rebuilding a name.

`KdmConfig.formulation` is the typed `KdmFormulation` (ISDCF spellings via
`FromStr` and serde), and it now does something: it decides whether
ContentAuthenticator is emitted (present for `dci-any` and `dci-specific`), which
is the signer leaf's base64 ST 430-2 thumbprint, placed between ContentTitleText
and ContentKeysNotValidBefore for SMPTE and Interop alike. libdcp calls that
element approximate, since strictly it names a CPL signer certificate, the same
certificate only when one entity signs both. A formulation that contradicts
`device_cert_files` is an error naming the formulation to use instead, in
`build_kdm` and `rewrap_dkdm` both: libdcp falls back silently in both
directions, and DCP-o-matic accepting `-T` under a formulation that discards it
is exactly the trap this refuses to copy. Four DCP-o-matic 2.18.39 KDMs, one per
formulation, are vendored under tests/fixtures/ as the cross-implementation
oracle for both the thumbprint values and the presence table.

## 2026-08-12

MCA-labelled accessibility tracks (src/accessibility.rs): the probe settles
AudioDescription, HearingImpaired and SignLanguage from the sound MXF's ST 377-4
MCA tag symbols (`chVIN`, `chHI`, `SLVS`) via
`pcm::MxfReader::mca_label_subdescriptors`, which needed the asdcplib pin at
4f137a0. The three-state contract is unchanged: a tag symbol read means Present,
labels read without the relevant symbol means Absent, and a sound file that
cannot be resolved, opened or labelled leaves the track Undeterminable.

A MainSoundConfiguration still wins for the HI and VIN channels, but the labels
are read even when one is present, because SLVS has no configuration token and
the labels are the only thing that can rule a sign-language channel out. Verified
against a real dcpwizard `--sign-language-video` package with the ISDCF
ExtensionMetadata stripped, which reports Present off the SLVS label alone.

ISDCF Doc 13 §5.2 requires the SLVS tag symbol on the MCA Audio Channel Label
Subdescriptor and §5.1 only recommends the ExtensionMetadata, so the MCA label is
the normative declaration and the CPL extension is the optional hint. Doc 13
defines no MainSoundConfiguration token for sign language, so the probe does not
look for one. dcpdoctor's `SOUND_CHANNEL_LABELS` accepts `SLVS` and `Sign` in a
configuration slot, but that list mirrors DCP-o-matic rather than Doc 13.

Sound files resolve through the ASSETMAP (new src/assetmap.rs, shared with
preview) rather than by reading an id out of a filename, which only holds for
packages some tools build.

`AccessibilityTrack::VisuallyImpairedText` (src/accessibility.rs): the ST 2067-2
VisuallyImpairedTextSequence is now its own track rather than a footnote on the
AudioDescription evidence. Element name confirmed from the published
ST 2067-2:2020 section 6.3.3 Table 14, with Annex B.3 defining the content kind.
It is deliberately not AudioDescription: this is timed text a renderer speaks
aloud, while AudioDescription is a narration channel already carried as audio, so
a package can have either without the other and merging them would report a track
that is not there.

Only an ST 2067-3 SequenceList can declare it, so the three-state contract reads:
sequence present means Present, a SequenceList read without one means Absent, and
a DCP composition leaves it Undeterminable however completely it describes its
reels. `AccessibilityTrack` also gained `#[non_exhaustive]`, so downstream crates
need a wildcard arm and future track types are additive. Matches inside postkit
stay exhaustive on purpose, so a new variant breaks the build here instead of
falling through a catch-all.

KDM Trusted Device List (src/certificate.rs): every KDM now carries
AuthorizedDeviceInfo, SMPTE and Interop alike. It is required by ST 430-1 Annex B
with no minOccurs, so the KDMs postkit wrote before this were schema-invalid
rather than merely incomplete. Its position in KDMRequiredExtensions is fixed
between ContentKeysNotValidAfter and KeyIdList.

`KdmConfig.device_cert_files` and `RewrapConfig.device_cert_files` choose the
contents. Empty emits the DCI DCSS 9.4.3.5 assume-trust thumbprint
(`2jmj7l5rSw0yVb/vlWAYkK/YBwk=`, base64 SHA-1 of the empty string) alone, and the
exclusivity is normative: listing any real thumbprint beside it disables
assume-trust entirely, so it is one or the other. An empty DeviceList is not an
option, CertificateThumbprint is minOccurs="1". The recipient's own certificate is
deliberately excluded, per ISDCF Doc 5 deprecating the formulation that carried
it. The ISDCF formulations are combinations of ContentAuthenticator presence and
device list contents, typed as `KdmFormulation` on 2026-08-15 (below).

CertificateThumbprint is SHA-1 over the complete DER TBSCertificate including the
SEQUENCE tag and length, the same value the 138-byte key block already carried.
ST 430-2 5.4 says to exclude the tag and length, but libdcp hashes `i2d_re_X509_tbs`
output, which includes them, and its KDMs work in the field. Settled by computing
both readings for a generated certificate and cross-checking against openssl
asn1parse, which put the TBSCertificate at 535 bytes with a 4-byte header.

Two older KDM defects surfaced once the schema would compile and are fixed in the
same change. `Recipient/X509IssuerSerial` children were emitted unprefixed when
`ds:X509IssuerSerialType` puts them in the xmldsig namespace, and the ETM `Signer`
carried a third `X509SubjectName` child that type does not allow. Three real
Doremi-signed KDMs settled both, and they also declare `xmlns:ds` locally on
`Signer` and `X509IssuerSerial` rather than relying on the root, which is what
keeps those elements valid when read on their own. The `Recipient`'s own
`X509SubjectName` is a sibling of `X509IssuerSerial` and was always right.

Certificate serials were the third defect. `generate_certificate` never set one,
so rcgen fell back to 20 bytes of a SHA-256 over the public key. ST 430-2 5.2
requires an unsigned integer of 64 bits or less and DCI CTP 2.1.4 fails anything
larger, so serials are now random 63-bit values, minimally DER encoded. 63 rather
than 64 keeps the value positive without the leading zero byte an ASN.1 INTEGER
would otherwise need.

The ST 430-1 schema and its three imports are vendored in schemas/ so
`kdm_required_extensions_pass_the_st_430_1_xsd` runs offline rather than behind an
env var. `real_kdms_pass_the_same_schema` puts real KDMs through the same
extraction and schema, gated on POSTKIT_SAMPLE_KDMS since the samples live outside
the repo. The vendored ETM schema needed one repair: the published transcription
wraps the UUID pattern across a line break inside a character class, which libxml2
reads as a literal space and rejects.

## App-side dedup: all landed

hash (dcpdoctor hash.rs adapts `postkit::hash::hash_file`), imfwizard to_dcp.rs
(`packaging::DcpCpl`), and imfwizard inject_locale_list (`ImfCpl.languages`) are
done in the app repos. All three apps pin postkit at or past the 07-23 dedup.

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
