# Changelog

## Unreleased

### Changed

- **`j2k::J2kProfile` carries the Rsiz values a codestream actually holds**: it
  mapped Rsiz 1 to `Dci2k` and 2 to `Dci4k`, which are not cinema profiles, named
  Rsiz 3 and 4 (the plain 2K and 4K digital cinema profiles, and what every DCP
  and grok itself writes) `CinemaS2k` and `CinemaS4k` as though they were the
  scalable ones, and called Rsiz 5, scalable 2K, `Broadcast`. The variants are now
  `Cinema2k`, `Cinema4k`, `CinemaScalable2k`, `CinemaScalable4k`,
  `CinemaLongTermStorage`, `Broadcast` for 0x0100 to 0x03ff and `Imf` for 0x0400
  to 0x09ff, matching ISO/IEC 15444-1 Amd 1 and grok's own `GRK_PROFILE_*`. The
  Part 2 extension bit no longer hides a profile. `validate_dci_header` follows,
  so it no longer wraps Rsiz 1 or 2 essence into a DCP as though it were DCI; the
  profiles it accepts are unchanged otherwise. imfwizard's `check_j2k_dci` is the
  one caller outside postkit and accepts the same two profiles it did before, but
  names the rejected one correctly now.
- **`extract_frame` decodes DCP picture essence with grok**: the thumbnail path
  both wizards call, `dcpwizard frame-extract` and imfwizard's alike, ran ffmpeg
  over whatever it was given, and its `-ss` sits after `-i`, so a late frame
  decoded every frame before it. A picture MXF whose codestream declares a DCI
  cinema profile now goes through the DCP-native path. On a 2K 125 Mb/s DCP of
  120 frames, frame 119 fell from 13.64 s to 0.24 s and frame 0 from 2.12 s to
  0.23 s, and the time no longer grows with the frame number. Encrypted essence
  is refused by name rather than handed to ffmpeg, which cannot decrypt it and
  renders the ciphertext as a picture, so extracting one still needs
  `render_dcp_frame` and a key. Everything else still goes through ffmpeg,
  including IMF App 2E track files, whose samples are RGB or YCbCr rather than
  X'Y'Z'. DCP essence comes out 8-bit sRGB where ffmpeg wrote 16-bit.
- **The DCP preview adapts no illuminant**: `XyzToSrgb` applied a Bradford
  adaptation from DCI white to D65 that the encode side never applied, so every
  DCP built from a Rec.709 or P3-D65 master previewed desaturated and faintly
  magenta. A DCDM stores absolute XYZ and does not record what its neutral is,
  and the encode leaves such a master's neutral at D65, as libdcp and
  DCP-o-matic do. The preview now renders that XYZ as measured, so it inverts
  the encode: Rec.709 in comes back as Rec.709 out within a code or two. A
  picture graded against DCI white renders green, which is what it measures.
  `XyzToIcc` adapts D65 to the D50 PCS for the same reason. Measured against the
  source frame of a 2K DCP, the preview went from 26.74 dB to 29.62 dB, ahead of
  ffmpeg's conversion on every channel where it had been behind on every one.
- `render_dcp_frame` writes a binary PPM itself rather than spawning ffmpeg for
  it, since a P6 file is the frame's own rgb24 bytes behind a short header. Every
  other format still goes through ffmpeg. The preview tests now need no external
  tool, which is what their docstring already claimed and what the CI runners,
  which carry no ffmpeg, required.
- **The DCP-native preview decodes with grok, not ffmpeg**: `render_dcp_frame`
  and `play_dcp` no longer pipe each codestream to an ffmpeg process. The frame's
  size comes from the decoder rather than from a second parse of the codestream
  header, and a codestream that is not one is refused by grok instead of by that
  parse. Decoding needs the `grok-ffi` feature now: without it `grok_decoder`
  refuses by name rather than falling back, because a decoder at a few frames a
  second reads as a hang. Both wizards already enable the feature, and dcpdoctor,
  which does not, never calls this path. The end-to-end preview test encodes its
  fixture in process with the same grok, so it needs no `opj_compress` and is no
  longer ignored, and it checks that a flat mid-grey field comes back flat and
  mid-range instead of checking that a file was written.
- `CompressParams.frame_rate: u16` is now `CompressParams.edit_rate:
  encode::FrameRate`. `encode_video_pipeline_resumable` converts blackdetect's
  and freezedetect's seconds to frame numbers with it, and the rounded rate put
  a 23.976 finding about a frame per thousand late: the end of a 25 second black
  head came back as frame 600, the first frame after it, instead of 599. grok is
  still given the whole rate, rounded from the rational in one place for both
  `cparams.framerate` and grk_compress's `-w`.

### Added

- `j2k::J2kProfile::is_dci_cinema`, whether a profile is a digital cinema one and
  so carries X'Y'Z' samples rather than the RGB or YCbCr an IMF or broadcast
  codestream holds. `extract_frame` routes on it.
- `grok_decoder`: in-process JPEG 2000 decoding through the grok FFI. Bytes in,
  planar samples out, so nothing is written to disk and no process is spawned.
  `decode(codestream, reduce)` discards `reduce` highest resolution levels and
  `DecodedFrame::to_xyz12le` packs the samples into the layout the colour
  transforms read. Measured on 2048x1080 frames at 125 Mb/s: 68 ms a frame at
  full resolution against ffmpeg's 302, 19 ms at `reduce` 1 and 5 ms at `reduce`
  2. grok returns 16-bit samples for a 12-bit codestream, not the 32-bit its
  struct name suggests, and rows carry a stride that need not equal the width, so
  both are read from the component rather than assumed.
- `picture_findings::detect_in_essence(essence, fps, frame_count)`, the same two
  detectors over finished picture essence rather than over a decode that is on
  its way to the encoder. ffmpeg reads the whole file, so the frame numbers are
  the essence's own, and a file it cannot decode comes back as an error naming
  the reason instead of as empty findings. Encrypted essence is the caller's to
  refuse, since ffmpeg cannot decrypt AS-DCP. The thresholds now live in one
  `detection_filters`, which the encode's split branch shares.
- `burnin::BurninOptions.video_codec` and `.video_crf`, passed as ffmpeg's
  `-c:v` and `-crf`, so a burnt copy names its encoder and quality instead of
  taking whatever ffmpeg guesses from the output file name.
  `video_codec: "libx264"` with `video_crf: Some(0)` writes a lossless copy. The
  command line is built by `ffmpeg_args`, which a test reads without encoding.
- `subtitle_formats::bidi::has_rtl`, which answers whether text holds a
  right-to-left run and so needs `to_visual`. The bidi algorithm decides, so
  every RTL script counts and not only the six code-point ranges dcpwizard's
  `--subtitle-rtl auto` used to test.
- `gui_job_queue`: the queue of builds a wizard GUI runs, with its on-disk
  record, held once instead of a copy in each wizard. `GuiJobQueue<C>` is
  generic over the wizard's job config, which implements `GuiJob` for the id,
  title and output_dir the queue reads and the serde bounds the file needs.
  Alongside it `StoredJob<C>`, `StoredJobState`, `LoadedJobs<C>`, `record`,
  `load`, `JobInfo`, `INTERRUPTED_MESSAGE`, and `jobs_path(environment_variable,
  data_dir)`, which takes both from the wizard because postkit does not know
  either wizard's name. No tauri dependency: the wizards keep their
  `#[tauri::command]` wrappers and call these methods.

- `quality_psnr` on `EncodeRunOptions`, `StreamEncodeOptions` and
  `CompressParams`: a PSNR target in dB that grok allocates layers by instead of
  the compression ratio. grok holds to `max_cs_size` under rate allocation but
  ignores it under quality allocation, so a frame whose quality-allocated
  codestream exceeds `codestream_byte_cap` is compressed again by rate at
  `raw_frame_bytes / cap`, inside the encoder thread, before the writer sees it.
  The writer's own size check is unchanged and still fails the run on anything
  over the cap. `encode_parallel` passes `-q` instead of `-r` to grk_compress.
  `stream_encode_subprocess` refuses a PSNR target the way it refuses a byte
  cap: its cinema profile hands grk_compress a frame rate rather than a layer
  allocation.

- `picture_findings::PictureFindings` on `encode::EncodeResult`,
  `pipeline::EncodeResult` and `grok_encoder::PipelineResult`: the black and
  frozen runs ffmpeg's `blackdetect` and `freezedetect` saw while the source
  decoded, as inclusive output frame numbers at the encode's `fps`. Every encode
  path that decodes through ffmpeg reports them, `encode_video_pipeline` and
  `encode_video_pipeline_resumable` included, which is the route dcpwizard's
  `create` takes. A pipeline handed frames by its caller reports nothing: the
  caller's own decode is where the filters run. Both filters sit on a `split`
  branch of the decode chain, because neither accepts rgb48be and putting them
  in the main chain makes ffmpeg round trip every frame through yuv444p16le on
  its way to the compressor. The thresholds are ffmpeg's own defaults: 2 seconds
  minimum for each filter and a 0.10 black pixel threshold. `freezedetect`
  prints no `freeze_end` for a run that reaches the last frame, so the decoded
  frame count closes that one. A J2K sequence and an image sequence
  `grk_compress` reads for itself also report nothing, since neither decodes
  through ffmpeg.
  `PictureFindings::describe` turns them into one log line per run, of the form
  `black picture from 00:00:00:00 to 00:00:02:23 (frames 0 to 71)`, timecoded at
  the encode's frame rate rounded to whole frames per second.

- `ColourSpace::LogC` transforms in `DcdmTransform` instead of being refused, so
  an ARRI LogC3 master encodes through the same per-frame path P3 and Rec.2020
  use. Code values decode with ARRI's LogC3 curve for EI 800 and matrix through
  ALEXA Wide Gamut RGB (D65) into X'Y'Z'. The curve reaches well past 1.0 scene
  linear and everything above that clips to white, the same clip libdcp's S-Log3
  path applies, so no tone mapping happens here. `create_dcdm` accepts `logc`,
  `logc3` and `arrilogc` as source space names. `Aces` and `AcesCg` are still
  refused.

- `probe::VideoInfo`, `upmix::Upmixer`, `subtitle_raster::BurnEffect`,
  `subtitle_raster::BurnStyleOverrides` and `subtitle_formats::Rgba` derive
  `Serialize` and `Deserialize`, so a consumer storing them no longer needs a
  `#[serde(remote)]` mirror. Field and variant names are unchanged, so the wire
  format matches what those mirrors wrote.

- `encode::FrameRange` with `EncodeRunOptions.frame_range` and
  `StreamEncodeOptions.frame_range`: encode one window of a source instead of
  all of it, so a wizard trimming five minutes out of a two hour source no
  longer compresses the two hours and relinks the codestreams it kept. The
  window counts output frames at the target `fps`, after the fps filter or the
  read-rate override, numbered from zero, and its codestreams are numbered from
  zero. A stream decode reaches it as `trim`/`setpts` right after the `fps`
  filter plus `-frames:v`, so ffmpeg stops at the window's end. An image
  sequence gets only the window's stills, in the concat list or handed to
  `grk_compress`. A window running past the source fails before ffmpeg starts,
  and a J2K sequence refuses one, since it is never encoded here.

- `grok_encoder::encode_video_pipeline_resumable` takes the same
  `Option<FrameRange>`, for the callers that encode through it rather than
  through the pipeline. The window is trimmed after the caller's `video_filter`,
  so a fade keeps the timing a full encode would have given it, and `-frames:v`
  stops the decode at the window's end. With a window, `total_frames` is the
  window's length, the codestreams are numbered from zero, and a `resume` counts
  inside the window. The window is checked against an ffprobe frame count, which
  only runs when a window is asked for.

- `picture_processing` grew the crop resolution both wizards each carried:
  `require_one_crop_decider`, `fill_crop` (with the quarter-turn aspect swap),
  `detect_crop` (auto-crop through a video or an image-sequence concat list,
  refusing an out-of-range threshold and an all-black source), and the
  `parse_rotation`/`parse_flip` spellings, which now accept the union of what
  the two wizards each accepted.

- `free_space::available_bytes`/`volume_bytes`: what is free on the filesystem
  holding a path, statvfs on unix and GetDiskFreeSpaceExW on windows. Moved up
  from dcpwizard; imfwizard read the same numbers through the fs4 crate.

- `fs::write_atomic`: temp file in the same directory, then rename, creating
  parents as needed. `package_edit` used a private rename-based copy of this and
  dcpwizard carried another.

- `certificate::cert_info_from_file`/`cert_info_from_pem`/`der_base64_to_pem`:
  parsing untrusted certificates into `CertInfo` with a real error instead of
  the empty-thumbprint default, moved up from dcpwizard's kdm store.

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

- A signed document carrying an XML comment now verifies with xmlsec1. The
  whole-document signer digested `URI=""` with comments kept, because that is
  what the `#WithComments` canonicalization it declares says, but XML-DSig
  dereferences `URI=""` and a barename `URI="#id"` to a node-set with the
  comments already removed, so xmlsec1 digests comment-free bytes whichever
  inclusive c14n the reference declares. Every CPL, PKL and KDM postkit signed
  with a comment anywhere in it failed `xmlsec1 --verify` with a digest
  mismatch. Both reference paths now canonicalize without comments, and the
  with-comments mode is left where it does apply, on SignedInfo. A comment added
  to a signed document no longer breaks the signature, which is what every other
  verifier already accepted.

- The `URI=""` digest covers the whole document node, not just the root element
  subtree: the processing instructions outside the root element are digested
  too, with the line feed C14N 1.0 puts after one that precedes the root element
  and before one that follows it. Editing an `<?xml-stylesheet?>` above the root
  used to leave the signature valid here while xmlsec1 rejected the document.
  `c14n_document_reference` is the single entry point the signer and the
  verifier both compute those bytes with.

- `encode_parallel` honours the job's compression ratio instead of a hardcoded
  10:1, so a plain still-sequence encode follows the bitrate setting like every
  other input, and it holds each finished frame to the `codestream_byte_cap` so
  an over-cap still fails at that frame rather than at the post-encode sweep.
  At 10:1 a 4K still sequence encoded near 760 Mb/s and every frame failed the
  cap at the end of the run.

- A `codestream_byte_cap` stops the encode at the first frame over it. The cap
  was a sweep of the finished J2K directory, so a bitrate set too high encoded
  every frame before the run failed: a 1443-frame job learned at the end that
  frame 279 was 1302215 bytes against the 1302083 byte cap. The in-process
  writer now sizes each codestream as it lands and fails there with the same
  refusal, which stops the encoder threads and the decode instead of letting
  them run out the sequence. `StreamEncodeOptions` carries the cap;
  `stream_encode_subprocess` refuses one, because grk_compress writes those
  codestreams itself. The sweep stays for the two inputs postkit never
  compresses, a J2K sequence and an image sequence handed straight to
  grk_compress.

- The in-process encoder no longer hangs when a frame fails. The queue's
  producer blocks while the queue is full, and every encoder thread stops on the
  first error, so nothing was left to drain it. Recording the error now closes
  the queue too, which is what releases the producer.

- `MpvRenderPlayer::render_opengl` no longer waits for each frame's display
  time. mpv's render call blocks until the frame is due, by default with 50ms
  of `video-timing-offset` headroom, and the app renders on its main thread, so
  playing video parked that thread for most of every frame period and the whole
  UI starved until playback paused. The render returns immediately now and
  `video-timing-offset` is zero, so mpv times frames for immediate display and
  the app's own vsync paces the presentation.

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

- `xmldsig` verification canonicalizes under the algorithm each signature
  declares. It always kept comments, so a validly signed document carrying an
  XML comment in the signed region, which the ISDCF reference DCPs and
  orca_wrapping both emit, failed its digest check and looked tampered with. The
  plain C14N URI omits comments, `#WithComments` keeps them, a `ds:Reference`
  declaring no c14n transform gets the no-comments default every verifier
  computes for it, and exclusive c14n is now an error naming the algorithm
  instead of a digest over the wrong bytes. `sign_enveloped` digests its
  references under that same default, so its output is unchanged unless a
  referenced element contains a comment; `sign_document_enveloped` still
  declares and uses `#WithComments`, where a comment added after signing breaks
  the signature.

### Changed

- `encode_parallel` takes the image files to compress rather than a directory to
  list, so the caller picks the frames and a window is fewer of them. Callers
  list the sequence with `find_source_frames` first.
- `encode_video_pipeline_resumable` takes a `frame_range` after `video_filter`.
  A caller encoding the whole source passes `None`.
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
