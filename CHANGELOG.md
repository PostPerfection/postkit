# Changelog

## Unreleased

### Added

- `colour::DcdmTransform`: the DCDM encode transform for one source colour
  space, built once and applied per frame (`pixel`, `frame_rgb48le`,
  `frame_rgb48be_inplace`). It covers Rec.709, P3, Rec.2020 and X'Y'Z', and
  refuses ACES, ACEScg and LogC, which need a LUT rather than a matrix.
  `create_dcdm` and `rgb_to_xyz_inplace` both run through it, so the file
  pipeline and the in-memory path are one transform.
- `subtitle_raster::find_system_sans_font`: a sans-serif font file off the same
  fontdb scan the burn path shapes with, preferring Liberation Sans, then DejaVu
  Sans, then whatever the generic sans-serif resolves to. Only a whole font file
  on disk qualifies, because the subsetter reads the first face of the bytes it
  is handed. A packager uses it to embed a font when the caller named none.
- `subtitle_raster`: a text rasteriser and frame compositor for burnt-in
  subtitles. `SubtitleRasterizer` shapes `StyledCue`s with cosmic-text (system
  fonts through fontdb, or a supplied .ttf/.otf) and rasterises them to
  positioned RGBA bitmaps, honouring per-run bold/italic/underline/colour and
  per-cue align/valign/vposition; bitmap cues composite their PNG directly.
  `composite_rgb48` alpha-blends those onto a packed rgb48 frame. Neither knows
  about the encoder, so a preview can reuse both.
- `SubtitleBurn` and `CompressParams.source_preparation`: subtitles composited
  into every decoded frame during the encode, so burning costs no second
  generation. The burn always lands before any colour conversion, this crate's
  own or the compressor's, because text is authored in display RGB.
  `SourcePreparation` replaces `CompressParams.source_transform` and carries
  both per-frame steps, so the order cannot be asked for the other way round.
  `SubtitleBurn::active_cues` names the cues on a frame, which is how a held
  still encodes once per cue change instead of once per frame.
- An image sequence reaches the per-frame encode path through ffmpeg's concat
  demuxer when a burn is set (`DecodeSource::ImageList`,
  `write_image_concat_list`), so TIFF, DPX, EXR and PNG sequences burn the same
  way a video does. Without a burn they still go straight to `grk_compress`.
- `SourceColour::DisplayRgbIn(space)` and `CompressParams.source_preparation`: a
  P3 or Rec.2020 video source is converted to X'Y'Z' on the encoder threads,
  with grok's own transform off, so those sources encode correctly instead of
  being refused. Image sequences, J2K input and the subprocess encoder refuse
  the variant, naming why.
- `encode_video_pipeline_resumable` takes an optional ffmpeg `-vf` chain, so a
  caller can fade the picture while it decodes rather than in a second pass.
- `DcpCpl::annotation_text` and per-reel `picture_hash` / `sound_hash`, so a
  generated CPL can carry the `AnnotationText` Bv2.1 requires and the `<Hash>`
  some servers check instead of the PKL's. Both are schema-validated in place.
- Forensic marking flags: `KdmConfig` and `RewrapConfig` can disable picture
  marking, audio marking, or audio marking above a given channel, writing the
  ST 430-1 `ForensicMarkFlagList`. Marking stays on by default, and the element
  is absent then.
- `classify_kdm_window` reports whether a KDM validity window sits within,
  overlaps or falls outside a recipient certificate's own validity.
- The KDM vocabulary is public, so a command line no longer keeps its own copy:
  `KdmFormulation::ALL`, `lists_supplied_devices` and `device_list_counterpart`;
  `FromStr`, `Display` and `ALL` on `KdmFormat`; `forensic_mark_flag_uris`,
  which renders a marking pair to the ST 430-1 Annex C URIs a KDM carries; and
  the `ContentAuthenticator`, `CertificateThumbprint`, `ForensicMarkFlagList`
  and `ForensicMarkFlag` element names.
- `wav_io::read_interleaved_exact` and `write_interleaved_exact`, which carry
  interleaved samples in the file's own type through the new `Samples` enum, so a
  read and write back is byte-identical at every depth. The normalised f32 pair
  cannot be: 32 significant bits do not fit an f32 mantissa, so 32-bit int PCM
  loses its low bits through any read-modify-write. Anything that only moves
  samples around, an audio delay or a trim, wants the exact pair.

### Changed

- The DCDM Rec.709 matrix is the sRGB/D65 one grok and libdcp use, where
  `create_dcdm` had a BT.709-derived variant differing in the fourth decimal.
  Blue moves by up to 5 code values at 16 bits, under one at 12.
- `rgb_to_xyz_inplace` gamma-encodes each value directly instead of through a
  16-bit quantised output table, which was coarse near black.
- Burn-in applies `font_size`, `font_colour` and `position` to subtitles, as ASS
  style overrides on the `subtitles` filter. The three options reached the text
  watermark path only, so `--font-size` did nothing on the path the command is
  named after. Each one is left to the subtitle file when it is zero or empty,
  and `font_colour` must be RRGGBB hex there.
- Burn-in escapes what it puts in an ffmpeg filtergraph, so a subtitle path or a
  watermark text holding `:`, `,`, `'`, `[`, `]` or `\` no longer breaks ffmpeg's
  filter parser or silently truncates.
- Burn-in refuses a subtitle XML input before it runs ffmpeg, naming SRT as the
  remedy. ffmpeg has no reader for SMPTE ST 428-7 (DCST) or Interop subtitle XML,
  so it died inside ffmpeg with a message about the filter chain.
- Timed text and Atmos wraps honour `MxfWrapOptions.encryption`, so a DCP built
  with encryption no longer ships cleartext subtitles and Atmos beside encrypted
  picture and sound. Both carry the caller's KeyId and an HMAC, as picture and
  sound already did.
- `KdmFormat` and `KdmFormulation` parse case-insensitively, so a command line
  may spell them `SMPTE` or `DCI-Any`. An empty value is still an error.
- `MxfEncryption` zeroes its content key on drop, as the keys recovered from a
  KDM already did, so wrap keys do not linger in freed memory.
- Generated certificates are DCI-conformant: every DN value is a
  PrintableString, every tier carries a dnQualifier holding the base64 SHA-1 of
  its public key, the leaf adds `keyEncipherment`, basicConstraints path lengths
  are 3 and 2, both key identifiers are written, and `generate_chain` puts the
  ST 430-2 role token in each CommonName. Regenerate any chain postkit produced
  earlier.
- `build_kdm` and `rewrap_dkdm` refuse a validity window the recipient
  certificate could never open, and one the signer chain does not fully cover.
- One certificate thumbprint everywhere: `CertInfo.thumbprint` and
  `TrustedDevice.thumbprint` carry the base64 ST 430-2 value a KDM lists, and
  existing trusted-device stores migrate themselves on first use.
- `KdmConfig.formulation` is a typed `KdmFormulation` that emits
  `ContentAuthenticator` for the dci formulations and rejects a device list its
  formulation would discard, instead of being read by nothing.
- Every tier of a generated chain is valid for 10 years, staggered by a day per
  tier as libdcp does so a parent outlives the child it issued, and any longer
  validity is refused: DCP-o-matic rejects a signer certificate spanning more
  than 15. Regenerate any chain postkit produced earlier.
- `X509SubjectName` and `X509IssuerName` are written in RFC 4514 order, most
  specific RDN first, which is what libdcp writes and what a projector matches a
  KDM recipient against. Reissue any KDM postkit produced earlier.

### Subtitle MXFs written before this release reused the file id as the ResourceID

Rebuild any package carrying a subtitle or closed-caption track. The timed-text
wrap wrote the track file's own asset id as the descriptor AssetID, which
asdcplib writes as the ResourceID of the timed-text resource. ST 429-5 wants
that to be the id the document declares, and wants the track file, the document
and the resource to be three different things, so libdcp and dcpdoctor both
report the file id as reused. The wrap now reads the id out of the XML (DCST
`<Id>`, Interop `SubtitleID` as an element or a root attribute) and refuses a
document that declares none.

### Subtitle MXFs written before this release hid their fonts and images

Rebuild any package whose subtitles embed a font or an image. The timed-text
wrap wrote each resource into the file but never declared it in the header, and
a reader can only enumerate what the header declares, so the resource count came
back zero and reading one by its id failed. A player therefore could not find
the font a DCST asks for by `urn:uuid`, which defeated font subsetting entirely.
Subtitle MXFs with no embedded resource are byte-for-byte what they were.

AS-02 (IMF) timed text has the same defect and no fix available here, because
asdcplib exposes no AS-02 entry point that declares the resource list. That wrap
now refuses fonts and images rather than embedding one nothing can read.

## 0.6.0 - 2026-08-13

### Generated signer certificates were missing two required extensions

Regenerate any chain postkit produced earlier. Every leaf certificate came out
with no Basic Constraints and no Key Usage extension, which ST 430-2 requires,
so validators reject a package signed with one. rcgen writes no extensions at
all for `IsCa::NoCa`, and the leaf was the only certificate using it, so root
and intermediate were always correct and only the signer was affected.

### KDMs written before this release were schema-invalid

Regenerate any KDM postkit produced earlier. Three defects broke it against the
ST 430-1 schema, and each one on its own is enough for a conformant consumer to
reject the message.

- No `AuthorizedDeviceInfo` at all. ST 430-1 Annex B declares it with no
  `minOccurs`, so it is required, and postkit never wrote one.
- `Recipient/X509IssuerSerial` children were written unprefixed. The element is
  typed `ds:X509IssuerSerialType`, so `X509IssuerName` and `X509SerialNumber`
  belong in the xmldsig namespace.
- The ETM `Signer` carried a third `X509SubjectName` child. ST 430-3 types it as
  `ds:X509IssuerSerialType`, which permits issuer and serial only.

The `Recipient`'s own `X509SubjectName` was always correct and is unchanged. It
is a sibling of `X509IssuerSerial`, not a child of it.

### Certificates generated before this release fail DCI CTP 2.1.4

Regenerate them too. `generate_certificate` never set a serial number, so rcgen
fell back to 20 bytes of a public key hash. ST 430-2 5.2 requires an unsigned
integer of 64 bits or less, and CTP 2.1.4 fails anything larger. Serials are now
random 63-bit values.

### Added

- `KdmConfig.device_cert_files` and `RewrapConfig.device_cert_files` restrict a
  KDM to named playback devices by certificate thumbprint. Empty, the default,
  emits the DCI assume-trust thumbprint alone.
- `AccessibilityTrack::VisuallyImpairedText` for the ST 2067-2
  `VisuallyImpairedTextSequence`, separate from `AudioDescription` because it is
  text a renderer speaks rather than a narration channel carried as audio.

### Changed

- `AccessibilityTrack` is `#[non_exhaustive]`, so matches in other crates need a
  wildcard arm.
- `dci_max_bitrate_mbps(width)` is replaced by the constant
  `DCI_MAX_BITRATE_MBPS`, and the 4K limit drops from 500 Mb/s to 250. DCSS 4.3.3
  caps a 4K frame at the same 1,302,083 bytes as 24 fps 2K, and the 500 has no
  source in DCI, ST 429-4 or ST 429-2. A 4K package between 250 and 500 Mb/s that
  passed before is now reported over the limit.
- `analyse_bitrate` lost its `width` parameter, which fed only that branch.
