# Changelog

## Unreleased

### Added

- `still`: a single image held for a duration, encoded once and hard-linked into
  a directory of J2K frames, with a burnt-in subtitle breaking the repeat only
  where the cues change. Moved up from the two wizards, which had drifted apart:
  colour reaches it as what the encoder takes, `apply_xyz_transform` plus an
  optional `DcdmTransform`, so both wizards' routes fit. `is_still_image`,
  `STILL_EXTENSIONS` and `HELD_PICTURE_DIR` come with it.

- `pipeline::run_encode_and_wrap_picture`: encode a picture track and wrap its MXF
  at the same time, instead of wrapping once the encode is done. The encoder's
  writer thread hands each codestream to `mxf_wrap::OverlappedJ2kWrap` after it is
  on disk, a reorder buffer there releases them in index order, and asdcplib
  writes them as they come, so the wrap no longer waits for the last frame and no
  longer reads every codestream back into memory (a 2000-frame feature was ~2.6 GB
  of that). The J2K directory is written and left behind exactly as before, and
  the MXF carries the same essence the sequential wrap does, frame for frame. The
  pieces are usable on their own: `mxf_wrap::IncrementalJ2kWrap` takes one
  codestream at a time and `grok_encoder::encode_pipeline_with_mxf_feed` /
  `encode::stream_encode_inprocess_with_mxf_feed` are the existing encoders with
  the feed attached. A cancel or a failure of either side deletes the
  part-written MXF, which asdcplib cannot read without its footer, and leaves the
  codestreams that finished. Inputs that never hand postkit a codestream, a J2K
  sequence or an image sequence grk_compress reads for itself, are refused rather
  than wrapped empty; stereoscopic and sound wraps are untouched.

- `package_edit::edit_package`: retitle or re-annotate a package that is already
  written, a DCP or an IMP, without re-wrapping essence. It gives the CPL a new
  composition id, rewrites the title / annotation / content kind / issuer asked
  for, and repoints the PKL entry (id, new hash and size) and the ASSETMAP entry
  (id and Path) at the rewritten CPL. Essence files keep their asset ids and
  their bytes. The two formats differ only in a `CplVocabulary`: ST 429-7 names
  the title `ContentTitleText` and the annotation `AnnotationText` and may carry
  a ST 429-16 `meta:FullContentTitleText` alongside, ST 2067-3 names them
  `ContentTitle` and `Annotation` and has no metadata title. An annotation the
  CPL lacks is inserted where both standards place it, between the composition
  Id and IssueDate, so a reel's or a segment's own annotation is never mistaken
  for the composition's. `normalized_content_kind` resolves an ISDCF
  abbreviation (FTR, TLR, ...) for either format. An encrypted package is
  refused: a KDM authorises its keys for one named composition id, and a
  metadata edit mints a new one. A signed document is rewritten unsigned and
  named in `unsigned_documents`. Documents are found by root element rather than
  by file name, so an Interop `ASSETMAP` with no extension resolves. Setting a
  field the CPL does not carry is an error rather than a silent no-op.
- `tms` (feature `tms`, off by default): push a written package to a theatre
  management system over ftp or sftp, moved out of dcpwizard so imfwizard can
  deliver an IMP the same way. `TmsConfig` carries DCP-o-matic's
  `tms_protocol`/`tms_ip`/`tms_path`/`tms_user`/`tms_password` keys, deserializes
  from the app's config file, redacts the password in Debug, and `validate`
  refuses one naming no server or no login; `upload_package` puts every file under
  the package directory into `<path>/<package dir name>/`, creating each remote
  directory before the files that go in it and stopping at the first failure
  naming the file and the remote path it was going to. One `TmsTransport` trait
  (`ensure_dir`, `put_file`) covers both protocols, with an ssh2/libssh2
  implementation, a suppaftp one, and a fake the layout tests drive, so no test
  touches a network. sftp checks the host key against `~/.ssh/known_hosts` and
  refuses an unknown or changed key, printing the SHA256 fingerprint and the
  `ssh-keyscan` line that would record it; ftp warns that the login and the
  package cross the network in the clear. The feature is off by default because
  ssh2 links libssh2 and openssl, which dcpdoctor's wasm build cannot have.
  Reading the config file stays with the app: the path carries the app's name,
  and a `toml` dependency here would put winnow's `AsRef` impls in front of every
  crate that links postkit, which stops dcpdoctor's schema reader compiling.
- `xmldsig::strip_signature` drops a document's ds:Signature and the Signer
  beside it, whatever namespace prefix they carry, reporting whether the
  document was signed. Anything that rewrites a signed document needs it: a
  signature left over edited bytes reads as tampering.
- `cpl_xml::replace_tag`, `write_tag` that returns None when the element is
  absent, so a caller can tell a rewritten document from an untouched one.
- `composition_timeline::mpv_source`: a package directory to the one mpv source
  that plays its whole composition, `None` when nothing there resolves.
  `assetmap::parse_ordered` gives the same pairs as `parse` in document order,
  which is what makes the choice between several CPLs deterministic. It returns
  a `CompositionSource` carrying the `uri` and the composition `title`. Each
  reel plays only the span its CPL states: an `EntryPoint` or a `Duration`
  shorter than the file's `IntrinsicDuration` becomes the EDL segment's
  `,start,length` in seconds, converted with that segment's own `EditRate` (an
  IMF resource without one falls back to the composition's). A trimmed segment
  whose edit rate is missing plays whole rather than failing to resolve. Both
  `load_package_dir` callers force the title into mpv's `force-media-title`, and
  their `get_metadata` reads `media-title`, so the transport bar names the
  composition instead of the last reel's file. `load_file` and a package that
  resolves without a title clear the forced title.
- Encode progress carries where the time inside an encode went:
  `StreamProgress`, `PipelineProgress` and `grok_encoder::EncodeProgress` gain
  `decode_wait_secs`, `prepare_secs`, `encode_secs` and `write_secs`, all
  cumulative over the run. Preparation and compression are summed over the
  encoder threads, so those two can each exceed `elapsed_secs`.
  `PipelineProgress::phase_breakdown()` renders them as one line, `decoder wait
  12s, frame prep 30s, j2k 4m10s, write 8s`, in the shape the wizards print
  their `[TIMING]` lines in. The in-process encoder fills in all four;
  the subprocess encoder fills in the decoder wait and the whole grk_compress
  run, and leaves preparation and write at zero, since it prepares nothing and
  the child writes the codestream itself. `grok_encoder::encode_pipeline` takes
  the `PhaseClocks` its producer adds the decoder wait to.
- `StreamEncodeOptions.read_source_at` and `EncodeRunOptions.read_source_at`:
  read the source as if it ran at this rate, ignoring its own timestamps. It
  reaches ffmpeg as an input `-r` before `-i`, which regenerates constant-rate
  timestamps, so the `fps` filter of the same rate passes every frame through
  untouched. This is how a 23.976 source becomes a 24 fps DCP by playing 0.1%
  faster instead of having one frame in 1001 duplicated, and the sound needs the
  matching 1000/1001 pull-up. Video only: an image sequence carries its rate in
  the concat list and is refused.
- `MxfWrapOptions.timed_text_duration_frames`: the essence duration a timed-text
  wrap writes, in frames, for a caller that knows how long the asset has to be.
  A reel of a subtitled composition needs a subtitle asset spanning the whole
  reel even where no cue falls inside it, so a document with no cue timing now
  wraps when the duration is given. A cue ending past it is refused, with both
  frame counts in the message. Unset keeps the cue-derived duration.
- `colour::DcdmTransform`: the DCDM encode transform for one source colour
  space, built once and applied per frame (`pixel`, `frame_rgb48le`,
  `frame_rgb48be_inplace`). It covers Rec.709, P3, Rec.2020 and X'Y'Z', and
  refuses ACES, ACEScg and LogC, which need a LUT rather than a matrix.
  `create_dcdm` and `rgb_to_xyz_inplace` both run through it, so the file
  pipeline and the in-memory path are one transform.
- `subtitle_raster::find_system_sans_font`: a sans-serif font file off the same
  fontdb scan the burn path shapes with, preferring Liberation Sans, then DejaVu
  Sans, Arial, Helvetica Neue, Helvetica, Segoe UI, Noto Sans, then whatever the
  generic sans-serif resolves to, then any regular face calling itself sans, then
  any face at all. Only a whole font file
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

- `MpvRenderPlayer::set_osd_overlay(id, Option<OsdAssOverlay>)`: draw ASS over the
  video through mpv's `osd-overlay` command, or take that overlay away with None.
  `OsdAssOverlay` carries the events, one dialogue event per line, and the
  PlayResX / PlayResY the coordinates are in. ASS drawing commands make this a
  vector overlay, which is what QC guides (safe area, aspect masks, a crop
  rectangle) want: it is composited by the OSD renderer rather than by a video
  filter, so no frame passes through the CPU for it and a change is not a filter
  reconfiguration. mpv stretches the PlayRes canvas over the whole rendered
  surface, letterbox bars included, so a caller drawing onto the picture places it
  there itself from `osd-dimensions`.

### Fixed

- The in-process encoder's work queue is FIFO. It was a stack, so while the
  decoder kept it full the frame at the bottom was never picked up until the
  decode ended. The sequential wrap never noticed, codestream files are named by
  index, but an overlapped wrap holds every frame after the stuck one in its
  reorder buffer and fails once that passes the buffer's capacity.

- `metadata_edit::write_metadata` writes an IMF composition's annotation into
  `Annotation`, the element ST 2067-3 declares, instead of the ST 429-7
  `AnnotationText` it wrote for both formats. On an IMF CPL the old element is
  absent, so the annotation was silently dropped. Which element to write now
  comes out of `package_edit`'s `CplVocabulary`, so the two modules cannot
  disagree about it. DCP output is unchanged.

- Previewing a package plays the whole composition. `MpvPlayer::load_package_dir`
  and `MpvRenderPlayer::load_package_dir` picked one picture MXF by filename
  ("pic") or by size, so a three-reel DCP played as whichever single reel the
  guess landed on. Both now resolve ASSETMAP → CPL → each reel's MainPicture and
  hand mpv one `edl://` virtual timeline, which makes the reported duration the
  composition total and lets a seek cross a reel boundary. An IMF CPL resolves
  the same way through its MainImageSequence resource TrackFileIds. A package
  with one picture asset loads as the plain path it always did, and a package
  with no ASSETMAP, no CPL or no picture asset falls back to the old guess.

- `accessibility`: the closed-caption probe recognises the ST 429-12 `ClosedCaption` reel element (the name the schema declares, which libdcp and dcpwizard write) as well as the older `MainClosedCaption`.

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
- `MpvRenderPlayer::init_opengl` sets `vd-lavc-dr=no`, turning libavcodec's direct
  rendering off. With it on, the decoder's buffers are allocated on the thread
  holding the render context, which mpv documents as deadlocking against
  `MPV_RENDER_PARAM_ADVANCED_CONTROL` (DOCS/client-api-changes.rst): a caller that
  waits on the core from its render thread never comes back, and the app is frozen
  for good. The copy direct rendering saves is not worth that. The software render
  context never had it.

### Removed

- `j2k::analyse_mxf_bitrate` and `j2k::analyse_as02_mxf_bitrate`. Their only
  consumer, dcpdoctor's `analyze_picture_bitrate`, now measures the frames
  through its own key-aware reader, which also opens the essence these two could
  not. `MxfBitrateStats` stays: it is the shape dcpdoctor reports.

### Subtitle MXFs written before this release reused the file id as the ResourceID

Rebuild any package carrying a subtitle or closed-caption track. The timed-text
wrap wrote the track file's own asset id as the descriptor AssetID, which
asdcplib writes as the ResourceID of the timed-text resource. ST 429-5 wants
that to be the id the document declares, and wants the track file, the document
and the resource to be three different things, so libdcp and dcpdoctor both
report the file id as reused. The wrap now reads the id out of the XML (DCST
`<Id>`, Interop `SubtitleID` as an element or a root attribute) and refuses an
AS-DCP document that declares none. An AS-02 (IMF) wrap keeps the file id there
when the TTML declares no id, since IMSC has no such element.

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

Regenerate any KDM postkit produced earlier. Four defects broke it against the
ST 430-1 schema, and each one on its own is enough for a conformant consumer to
reject the message.

- No `NonCriticalExtensions`. ST 430-3 requires the element after
  `RequiredExtensions` even when empty, and dcpdoctor's schema pass flagged
  every KDM for it. A whole-message schema test now covers the envelope, not
  only the `KDMRequiredExtensions` fragment.

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
