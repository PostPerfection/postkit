# Changelog

## Unreleased

### Added

- **`mxf_wrap::MxfTrackFile::hash_base64`**: the wrap's SHA-1 in the form a CPL
  or PKL declares, so a packager handed a wrapped picture can write its hash
  without reading the file a second time.
- **`grok_encoder::encode_video_pipeline_resumable_with_mxf_feed`**: the
  resumable video encode with the writer thread handing each codestream to an
  MXF feed as it lands, so a caller that encodes through it can write the
  picture MXF while the encode runs the way `pipeline::run_encode_and_wrap_picture`
  does, instead of reading the whole J2K directory back afterwards.
  `encode_video_pipeline_resumable` is that call with no feed.
- **`grok_encoder::use_gpu`, `use_cpu`, `gpu_active`, `accelerated_frames`**:
  grok's accelerator plugin, found through `GRK_PLUGIN_PATH`, the working
  directory or the executable's directory, encodes and decodes on the device
  after `use_gpu` (device 0, the plugin's first). Every compress and decompress
  in the process routes through it for the frames the plugin handles, a reduced
  decode or a tiled stream stays on the CPU, and `use_cpu` switches back. An
  encode runs as one batch through the plugin's pipeline rather than one frame
  at a time: the encoder threads submit into it and the writer thread collects
  the code streams the plugin hands back on its own threads. The first frame
  fixes the batch's shape, a shape or a set of parameters the plugin declines
  puts the run back on the CPU, and an encode with a PSNR target stays on the
  CPU because a frame over the byte cap is compressed again by rate. A decode
  still routes per call. `accelerated_frames` counts both the calls grok routed
  and the frames the batches got a code stream back for.
  `cargo test --features grok-gpu` runs the device round trip on a machine with
  the plugin, CI has no GPU. Needs a grok newer than v20.4.1, which has
  `grk_plugin_set_enabled`.
- **A source's own planar YUV reaches the device unconverted**:
  `probe::probe_pixel_format` reads the stream's pixel format, colour space and
  colour range, and `encode::choose_pipe_format` decides from those what ffmpeg
  writes to the pipe. With the accelerator on, a yuv420p, yuv422p, yuv420p10le
  or yuv422p10le source goes to the pipe as its own three planes and
  `grok_encoder::plugin_takes_frame` asks the plugin whether it takes that
  shape, by beginning a batch with the run's own compression parameters and
  ending it with no frame submitted. The plugin then upsamples the chroma,
  converts YUV to RGB and runs the X'Y'Z' transform on the device, and the code
  stream carries the 12 bits both the cinema profiles and App 2E are written
  at. A subtitle burn, postkit's own P3 or Rec.2020 transform, the HDR-to-DCI
  LUT, a filter chain that changes the pixel format or the colour, and a PSNR
  target each keep the run on packed RGB, since every one of them reads or
  rewrites the samples between the decoder and the compressor. ffmpeg decodes
  with `-hwaccel cuda` whenever the accelerator is on, whatever the pipe
  format. `EncodeResult.pipe_pixel_format` names what the pipe carried, and
  `tests/grok_gpu_yuv.rs` encodes a yuv420p clip through the plugin and decodes
  every frame back.
- **Packed RGB reaches the device interleaved**: `RawFrame::Packed` and
  `PipeFormat::PackedRgb` carry a `grok_encoder::SampleOrder`, so ffmpeg writes
  rgb48le or rgb48be and the run says which. `encode::pipe_format_for_run` runs
  `choose_pipe_format` and then asks the plugin about the source that survived
  it, planar YUV first and interleaved RGB after, so a run asks at most twice
  and only about a shape everything else already allows through. A little-endian
  frame reaches grok as the one buffer it came off the pipe in, as
  `GRK_SOURCE_RGB48LE`: the image's first component points at the buffer with
  the row pitch in 16-bit samples, the other two carry no data, and nothing is
  deinterleaved or copied on the host before submit. The plugin takes that
  source for a cinema code stream at 12 bits out of a 16-bit frame with the
  X'Y'Z' transform on, and declines a batch over a 16-bit source at 12 bits
  with the transform off, interleaved and planar alike, which leaves the IMF
  profile and the already-converted cinema colours on rgb48be with postkit
  deinterleaving into grok's component buffers. TIFF sequences take the same
  route: `TiffFrame::into_rgb48_frame` packs a still in either order and
  `encode_tiff_sequence_inprocess` asks the plugin once, at the first still's
  raster, before the loader threads start. `subtitle_raster::composite_rgb48`
  already read both orders and `colour::DcdmTransform::frame_rgb48_inplace` now
  writes both, so a burn and a source transform land the same either way.
  `tests/grok_gpu_rgb48le.rs` holds the device's interleaved encode of a video
  source and of a TIFF sequence to within 50 dB of the host's encode of the
  same frames, per component per frame.
- **`audio_mix_matrix::parse_named_audio_map`**: the `IN:OUT[@GAIN]` walk
  with a lane name taken wherever an output channel number is, which both
  wizards had a copy of. A `LaneVocabulary` holds the names each lane answers
  to, matched ignoring case, and the rule that turns the highest destination
  into an output channel count, so dcpwizard rounds a track up to 2, 6, 8 or
  16 channels and imfwizard writes as many lanes as the map named. Each
  wizard keeps only its own names.
- **`grok_decoder::decode_with_threads`**: `decode` with the grok thread count
  chosen by the caller, 0 for the shared pool and 1 for the calling thread. The
  4K fixture `cinema4k_grey_4096x2160.j2c` decodes on the pool, on one thread
  and at half size in the unit tests, which is the decode that crashed grok
  before v20.4.1 on arm64 macOS.
- **`hints`**: the advisory rules a job raises before an encode, which both
  wizards carried a copy of. `audio_level_hint`, `audio_language_hint` and
  `subtitle_hints` (a first cue before 4 seconds, a cue under 15 frames, a gap
  under 2 frames or an overlap, more than 3 lines, and a line over 52
  characters, replaced by the stronger hint at 79), over `Hint`, `AudioLevel`,
  `SubtitleCues` and `HintCue`. A format's own rule is a `CueRule` read over the
  same cue walk by `first_offence`, which is how dcpwizard's caption rules run.
  Rates are `f64`, so a composition at 23.976 counts frames at the rate it
  runs at.
- **`preflight`**: `check_burn_supported`, the four refusals both wizards made
  before drawing a burnt-in subtitle: the file is not there, it is also a
  timed-text track, the picture is a J2K directory, or the frames reach the
  encoder as X'Y'Z' already. `BurnTarget.frames_already_xyz` names the caller's
  own flags, which is all the two copies disagreed on.
- **`wav_io::channel_count`**: how many channels a WAV carries, without reading
  its samples. dcpwizard's `probe_channel_count` and imfwizard's
  `input_channels` were the same four lines and the same error.
- **`colour::parse_colour_space`**: the colour space a name written on a command
  line means, or None. It takes every spelling either wizard's own parser took,
  so `bt709`, `dcip3`, `dci-p3`, `ciexyz`, `2020`, `bt2020`, `ap0`, `ap1`,
  `alexa` and `arrilogc` all land where they look like they should, and the
  caller decides what an unknown name does.
- **TIFF sequences encode in process**: `encode::encode_tiff_sequence_inprocess`
  reads a sequence of 8, 12 or 16-bit TIFF stills on a small pool of loader
  threads (a quarter of the cores, at most four) into packed 16-bit frames and
  feeds the same encoder threads a decoded video uses, so a still sequence now
  gets the per-frame byte cap, a PSNR target under a cinema profile, the P3 and
  Rec.2020 transforms, a subtitle burn and the overlapped MXF wrap. The
  pipeline routes every TIFF sequence there unless a picture change needs
  ffmpeg's filters, and every other still format (DPX, EXR, BMP, PNG, JPEG)
  through ffmpeg's concat demuxer. A still that cannot be read fails the run by
  name. `TiffFrame::into_rgb48_frame` is the packing, and a 12-bit sample
  comes back exactly after the encoder shifts it down again.
- **`grok::write_tiff_rgb`**: an uncompressed RGB TIFF at 8, 12 or 16 bits a
  sample, the inverse of `load_tiff`, for an export that keeps the codestream's
  own precision. 12-bit rows are bit-packed and padded to a byte, and
  `load_tiff` now reads that padding, so an odd width round trips.
- **`DecodedFrame::interleaved_samples`**: the three decoded components pixel
  interleaved at the codestream's precision, what an image writer takes.
- **`encode::encode_loaded_frames`**: the loader pool behind the TIFF sequence
  encode, opened up to any caller that loads frames itself. `open_loader` runs
  once on each loader thread and returns what that thread loads a frame with,
  so a loader can hold its own MXF reader, and every frame gets the burn, the
  colour transform, the profile, the byte cap and the MXF feed a decoded video
  frame gets. `encode_tiff_sequence_inprocess` is now that with
  `grok::load_tiff` as the loader.
- **`encode::source_raster`**: the size of a container or of the first still of
  an image sequence, reading a TIFF itself since ffprobe cannot read a 12-bit
  one. Moved here from imfwizard so both wizards measure a source the same way.
- **`loudness::measure_true_peak_dbtp`**: the audio level hint's true peak,
  measured in process instead of by an ffmpeg loudnorm pass. Streams the WAV in
  one second blocks with one `Mode::TRUE_PEAK` meter per channel on its own
  thread, bit identical to a single meter over the whole file. On a 15 minute
  six channel 24 bit WAV: 1.0 s and 10 MB against loudnorm's 107.7 s and 177 MB,
  same -0.20 dBTP.

### Removed

- **Every grok command line path**: `encode::encode` (which passed a bitrate
  as grk_compress's ratio flag and declared no profile, so `create` refused its
  output), `encode_parallel`, `stream_encode_subprocess`,
  `grok_encoder::encode_pipeline_subprocess`, `grok::compress_file_subprocess`,
  `find_grk_compress`, `find_grk_decompress` and `grok_lib_path`, together with
  `StreamEncodeOptions.compressor_path` and `lib_dir`. Nothing in postkit
  spawns grk_compress or grk_decompress any more: encoding and decoding go
  through the linked library, so a build needs libgrokj2k and no grok binary.

### Fixed

- **The encode aimed at a compression ratio and missed the bitrate**: a caller
  with a bitrate divided the source raster's bytes by it, but the picture is
  padded to its container before grok sees it, and grok computes its own budget
  as `numcomps * prec * pixels / (ratio * 8)` on the image after its cinema
  transform. A 2048x872 source in a 2048x1080 container gave grok a budget of
  1,483,500 bytes for a 1,197,917 byte request, 24 percent over. Nothing bound
  the codestreams below the DCI cap either, because both allocation sites passed
  `max_bytes: 0`, so grok never got the cap as `max_cs_size` and a feature
  failed at frame 408 with "codestream ... is 1312856 bytes, over the 1302083
  byte per-frame cap". `CompressParams.target_codestream_bytes` is now the bytes
  per frame the allocation aims at, threaded from `EncodeRunOptions` and
  `StreamEncodeOptions`, and one `rate_allocation` turns it into the ratio grok
  needs from the image geometry grok will see and hands grok `min(target, cap)`
  as `max_cs_size`. Both the CPU compressor and the accelerator plugin's batch
  size their frames through it, so neither can drift. On the frame that failed,
  a 995,328 byte target lands at 995,262 bytes, the same size grok's own CLI
  writes in cinema mode at ratio 10, and a 1,197,917 byte target under a
  1,302,083 byte cap lands at 1,197,821. With no target the ratio is passed as
  it was, and the cap now reaches grok either way, so a codestream over it is
  the writer's guard catching a cap grok cannot meet rather than a bitrate
  nothing bounded.
- **The loudness passes could not read a packaged sound MXF**: `measure_loudness`,
  `measure_leq_m` and `measure_true_peak_dbtp` opened their input through hound,
  so a QC report handed a `sound_<uuid>.mxf` got "wav i/o: Ill-formed WAVE file:
  no RIFF tag found" instead of a measurement. The container is now detected by
  content: hound first, and a file with no RIFF tag is opened as a PCM MXF
  through `asdcplib::pcm::MxfReader`, one edit unit at a time, batched to about
  a second of audio per block. Anything that is neither still fails loud. On a
  15 minute six channel 24-bit track the MXF reads 2.20 s and 10.7 MB against
  the WAV's 2.15 s and 11.1 MB, and the same Leq(m) within 0.0002 dB. Encrypted
  essence is not decrypted, it fails with asdcplib's error.
- **Probing a video decoded every frame of it**: `probe::probe_video` counted
  frames with `ffprobe -count_frames`, a software decode of the whole file that
  took 181 s on an 888 s 2048x872 H.264 feature, as long as the GPU encode of
  the same film, with nothing logged while it ran. The count is now the video
  stream's own duration at its frame rate (1442 on a 60 s cut, 21312 on the
  feature, both exact), which ffprobe answers in about 20 ms. `-count_packets`
  is the fallback for a stream without a duration only, because an MP4 edit
  list trims packets it still counts: the same 60 s cut holds 1508 packets.
- **A DCI LUT path with a colon or a comma broke the decode**: `decode_filters`
  put the `.cube` path into the `lut3d` filter as it was, and ffmpeg's filter
  graph reads a colon as the end of an option and a comma as the end of a
  filter, so every Windows path with a drive letter failed with "Error applying
  option 'clut' to filter 'lut3d'". The path now goes through the same
  quoting and escaping the subtitle burn already uses for its file name.
- **Untransformed RGB frames were compressed without the component transform**:
  the grok image the encoder threads build for a planar or packed RGB frame was
  labelled sYCC, and grok switches MCT off for that colour space with a warning
  and writes the code stream without it, so every encode whose frames reach the
  compressor untransformed (an IMF `KeepRgb` encode, `AlreadyPq`, the DCI LUT
  and the P3 or Rec.2020 source transform) ignored `CompressParams.mct`. The
  cinema encode with grok's own X'Y'Z' transform was not affected, since that
  transform relabels the image. The image is now labelled sRGB, which is what
  the samples are, and MCT follows the parameters on every path.
- **Every 8-bit YUV source was compressed about two codes low**: swscale
  converts an 8-bit YUV pixel format straight to `rgb48be` at 8 bits, so the
  frames on the packed RGB pipe carried a level error of about two codes of
  255. A yuv420p frame of Y=126 Cb=Cr=128, whose exact limited range BT.601
  conversion is 128.08 of 255, came off the pipe at 0x7e7e per channel, 126
  times 257, and 0x4080c0, exactly 63.77, 128.38, 193.09, came off at 62, 127
  and 192 times 257. No `-sws_flags` value converts it any better.
  `encode::decode_filter_chain` now inserts `format=gbrp16le` ahead of every
  other filter when the pipe is packed RGB and `probe::probe_pixel_format` read
  one of the formats `encode::is_eight_bit_yuv_pixel_format` names, so
  everything downstream of the decoder runs at 16 bits, the packing to rgb48
  included, and those two colours arrive at 0x8016 and at 0x3fc6 0x8063 0xc119,
  each within 200 codes of 65535 of exact. The conversion has to go first
  because the HDR-to-DCI LUT takes RGB input and ffmpeg would otherwise convert
  the frame to 8-bit RGB for it, through the same 8-bit path. It costs about
  2.5 core milliseconds a frame at 2048x872, and the pipe format is still
  decided on the chain before the insertion, so a run that would have taken the
  source's own planes still takes them. A 10-bit YUV source already took
  swscale's high depth path, an RGB source has no matrix to apply, and the
  device converts the planar YUV it is handed itself, so none of those three
  changed. The one still on the old path is `still.rs`, which decodes a single
  still through its own ffmpeg call, so a JPEG still is still converted at 8
  bits. `tests/grok_gpu_yuv.rs` measures a device encode against a host encode
  of the same clip: the two 8-bit cases sat 45.2 dB apart and now clear the
  50 dB floor at 51.7 and 55.5 dB.
- **A wrap held the whole track file in memory**: the J2K, Atmos and
  stereoscopic wraps read every codestream into one `Vec<Vec<u8>>` before
  writing a frame, `wrap_pcm` read the whole WAV, and `compute_hash_and_size`
  read the finished MXF back with `fs::read` to hash it, so wrapping a feature
  needed tens of gigabytes and was killed instead of returning. Each of the
  four now reads one frame at a time inside its write loop, `parse_wav` takes a
  `Read + Seek` and skips past every chunk body instead of a slice of the whole
  file, and the hash runs over 1 MiB reads. `wrap_pcm` also cast the WAV data
  length to `u32` before dividing it into audio frames, so a data chunk past
  4 GiB wrapped round to a short frame count and the rest of the sound was
  dropped without an error: `WavFormat.data_len` is a `u64` and the division
  happens there. `tests/mxf_wrap_memory.rs` counts every allocation through a
  global allocator and holds each wrap to a quarter of the essence it writes.
- **No 4K frame could be compressed**: every DCP encode asked grok for the
  Cinema 2K profile whatever the raster, and grok refuses a frame past
  2048x1080 under it, so a 4K encode died at "Failed to initialize Grok
  compressor" and a 4K DCP was never buildable. A plain cinema profile now
  resolves to 2K or 4K by each frame's raster through `j2k::rsiz_for_raster`,
  and a frame past 4096x2160 is refused naming the raster. The wrap check that
  a 2K codestream fits 2048x1080 was already there, so nothing wrong could have
  shipped, nothing 4K could ship at all.
- **The DCDM review movie played at whatever rate ffmpeg picked**:
  `export_dcdm` now takes a `FrameRate` and passes it as an input option, so
  the movie plays at the rate the caller asks for. The concat list's frame
  durations never reached the output, which came out at the concat demuxer's
  default 25 whatever the DCDM was, and the earlier fix that wrote 24 into the
  list did not change that.

- **A producer that stopped short hung the encode**: `encode_pipeline` waited
  for `total_frames` codestreams, so a decoder that ended early or a loader
  that failed left the pipeline sleeping forever. It now waits for the frames
  the producer actually handed over.

- **Every encoder entry point carries the IMF profile**: `StreamEncodeOptions`
  learned the Rsiz and `KeepRgb` a release ago, but the pipeline, the parallel
  image-sequence encoder and the still hold all built their own compress
  parameters and left both at the cinema defaults. `EncodeRunOptions.rsiz`
  forwards into both `StreamEncodeOptions` the pipeline builds,
  `encode_parallel` takes the Rsiz, the frame rate and the source colour, runs
  grk_compress in its cinema mode for a cinema Rsiz and passes `-Z` for any
  other, with `--xyz` only for a colour that asks for it,
  and `StillHold.rsiz` sets `CompressParams.profile`. `encode_parallel` refuses
  an IMF Rsiz with the X'Y'Z' transform, as the in-process encoder already did,
  and the image sequence branch of `reject_unsupported_colour_path` now lets
  `KeepRgb` through, since grk_compress compresses those frames as they are.
  grk_compress writes each codestream at the precision of the file it read, so
  an IMF image sequence starts at 12 bits. `tests/imf_encode.rs` covers each
  entry point from the written codestreams only: the declared Rsiz, 12-bit
  components, and a red frame that decodes back to 4095,0,0 (4048,0,0 for the
  compressed hold), where an X'Y'Z' encode leaves all three components large.

- **The preview shows IMF App 2E picture**: `extract_frame` had no display path
  for RGB samples, so an App 2E frame went to ffmpeg and an encrypted one was
  refused outright. A codestream declaring an IMF profile now takes
  `render_imf_frame`: the same resolve, decrypt and grok decode the DCP path
  uses, and then the 12-bit RGB code values to 8-bit sRGB with their low four
  bits dropped, no matrix and no curve, because Rec.709 and sRGB share primaries
  and white point. Every read now picks its asdcplib reader from the essence
  type, AS-DCP or AS-02: the AS-DCP reader opens an AS-02 file and then fails
  every `read_frame`, which is what `frame-extract` on an IMP hit. The colour is
  read off the essence descriptor's ColorPrimaries and TransferCharacteristic,
  and anything the pass-through would show wrong is refused by name: ST 2084
  (PQ), the BT.2020 transfer, P3-D65 and BT.2020
  primaries, any UL the module does not know (HLG lands here), and a codestream
  that decodes at some depth other than 12 bits or with other than 3 components.
  A descriptor that signals no colour warns and is taken as Rec.709, since
  packages exist that signal nothing. Encrypted App 2E essence follows the same
  key rules as the DCP route. `tests/imf_preview.rs` wraps a pure red Rec.709
  frame AS-02 and asserts every pixel of the extracted PPM is 255,0,0, where the
  DCDM inverse would have given 255,0,70, and asserts the refusal on the same
  frame and on the real Netflix codestream wrapped as P3D65 PQ.

- **A conformance test that reads the App 2E track file back**:
  `tests/app2e_conformance.rs` encodes a pure red Rec.709 frame under an IMF
  Rsiz, wraps it AS-02, and then asserts only against the written MXF, so an
  encoder that agrees with itself cannot pass it. From the file it reads a
  12-bit RGB pixel layout with component max reference 4095 and min 0, an IMF 2K
  lossy PictureEssenceCoding label rather than the cinema 2K one a DCP encode
  would have produced, BT.709 primaries and BT.709 transfer, a sub-descriptor
  whose Rsiz is an IMF profile with three 12-bit components at 1:1, and a frame
  that decodes back to 4095,0,0 at the centre pixel, where an X'Y'Z' encode
  would leave all three components large. Two real codestreams come with it in
  `tests/fixtures`: a 64x64 DCI cinema frame at Rsiz 0x0003 and frame 0 of the
  picture track of Netflix Open Content's Sol Levante IMF at Rsiz 0x0536, which
  parses as 12-bit RGB, decodes at reduce 2, and wraps as the
  `IMFProfile_4K_Lossy_6_3` label the Netflix CPL carries. The three refusals
  below have a test each.

- **The J2K encoder can write IMF picture, not just DCI cinema**: every
  codestream it produced declared Rsiz 0x0003 and carried X'Y'Z' samples, which
  is why imfwizard shipped IMPs whose App 2E picture was cinema essence under an
  IMF label. `j2k` now composes an IMF Rsiz: `ImfProfile::for_raster` picks 2K,
  4K or 8K from the picture size, `imf_levels` reads the main level off the
  sample rate and the sub level off the bit rate, and `imf_rsiz` puts the three
  together, so 3840x2160 at 24 fps under 800 Mb/s comes out 0x0536, the Rsiz a
  real Netflix App 2E picture carries. A raster or a rate past the top level is
  refused rather than clamped, and a sub level of 2 or more raises the main level
  to two above it, which Table A.53 requires. Given an IMF Rsiz the in-process
  encoder shifts each sample down to 12 bits and declares that depth, because
  grok only reduces precision itself as part of the X'Y'Z' transform the cinema
  profiles run, and leaves the code block size, progression, layer count and
  wavelet to grok's own IMF parameters. An IMF profile with the X'Y'Z' transform
  still set is refused by name, as is one on the subprocess encoder, which hands
  grk_compress 16-bit frames it has no way to reduce. `SourceColour::KeepRgb`
  compresses display RGB untouched for picture whose descriptor names the colour,
  and `StreamEncodeOptions.rsiz` carries the profile into both encoders,
  defaulting to the cinema 2K they wrote before. Nothing in either wizard selects
  an IMF profile yet.

### Fixed

- **Image-sequence DCPs declared Rsiz 0x0000**: `encode_parallel` never handed
  grk_compress a cinema profile, because grok refused every still under 12 bits
  under one, so the codestreams carried no profile and `validate_dci_header`
  rejected them at the AS-DCP wrap. A cinema Rsiz now runs grk_compress in its
  cinema mode (`-w`/`-x` with the frame rate and, when a cap is set, the bit
  rate), and grok, from the release after v20.3.13, widens an 8-bit still to
  12-bit X'Y'Z' and keeps a ratio tighter than the DCI cap.
  `parallel_encode_honours_the_ratio_and_the_cap` reads a codestream back: Rsiz
  0x0003, three 12-bit components, and `validate_dci_header` passes.

### Changed

- **`loudness::measure_loudness` and `measure_leq_m` measure in process**: no
  ffmpeg, so the input is a WAV (PCM 8/16/24/32-bit integer or 32-bit float,
  whatever hound reads). `measure_loudness` makes one streamed pass that feeds
  a `Mode::TRUE_PEAK` meter per channel and one `Mode::I | Mode::LRA` meter
  over every channel, reading short-term loudness at R128's own 100 ms cadence
  as the mean of the meter's 100 ms energies, and returns the integrated
  loudness, the range, the true peak and the largest short-term window. On a
  15 minute six channel 24-bit WAV: 2.0 s and 21 MB against the two ffmpeg
  passes' 101.2 s and 177 MB, with integrated -17.64 LUFS against loudnorm's
  -17.68, LRA 24.06 against 24.00, true peak -0.20 dBTP against -0.20, and a
  short-term max of -9.39 LUFS against the ebur128 filter's -9.4.
  `measure_leq_m` streams the same reader.

- **Leq(m) sums the corrected channel powers**: `measure_leq_m` and a
  `LoudnessTarget::LeqM` gain plan weight each channel on its own, scale its
  energy by the channel's correction for its DCP channel index, and sum the
  channel energies. The corrections are leqm-nrt's, the ones DCP-o-matic passes
  it: L, R, C and LFE at 0 dB, Ls, Rs, Lc, Rc, BsL and BsR at -3 dB, and HI,
  VI, motion data, sync, sign language and the unused channel at -144 dB, so a
  narration or sync channel contributes nothing. A mono or stereo file is
  unaffected, both its channels sit at 0 dB. Multichannel numbers move: a
  15 minute 5.1 WAV reads 80.36 dB where the mean-of-channels downmix read
  70.19 dB and ffmpeg's `-ac 1`, which summed the layout at near unity, read
  84.00 dB. On one tone across a 5.1 layout the sum matches leqm-nrt to six
  decimals (6.991762 dB over the same tone in mono, against the
  10*log10(4 + 2*10^-0.3) the corrections predict). `measure_leq_m` and the
  `leqm=` gain target now weight a file the same way, so a QC report and an
  adjustment agree on what it measures. Each channel costs its own fft, so the
  same 15 minute track takes 2.15 s against the mono downmix's 0.85 s, at
  11 MB.

- **Leq(m) is calibrated to the ISO 21727 reference**: three things put the
  measurement 2.6 to 3.0 dB above leqm-nrt, the implementation DCP-o-matic
  reports Leq(m) from. The weighting was normalized at 1 kHz where the
  ISO 21727 M table is normalized at 2 kHz (5.6 dB); the reference offset was
  105.0 dB from full-scale RMS where the calibration derives 108.010299957 dB
  from 85 dB SPL at -20 dBFS through the reference tone's peak (3.0103 dB); and
  the weighting was the ITU-R 468 rational curve, which is convex between the
  points the ISO table samples and so runs 0.2 to 0.5 dB hotter over most of
  the band, worth another 0.19 dB on a broadband programme. The weighting is
  now the ISO 21727 table read straight between its points, and both constants
  are the reference's. Against leqm-nrt built at the sha DCP-o-matic pins:
  80.355616 dB against 80.325927 on a 15 minute 5.1 track, 92.832184 against
  92.750967 on six tones. leqm-nrt's own filter is a 64 point interpolation, so
  raising it to 1024 points moves it to 86.737542 dB on a 30 second cut where
  postkit reads 86.738372. Every Leq(m) number a package was measured at before
  this reads 2.6 to 3.0 dB high.

- **CI runs every test**: ffmpeg 8.1 is installed on all three runners, from
  the BtbN static build on Linux and Windows and from conda-forge on macOS,
  where Homebrew's is built without libass or freetype and the burn-in tests
  need both. grok is built on Windows as well, and `setup-libmpv` puts libmpv
  on every runner so the `libmpv` feature builds everywhere and its tests run
  on Linux. A `Check test tools` step fails the job when ffmpeg, ffprobe,
  grk_compress or grk_decompress is missing.
  The tests no longer return early when one of those is absent, so a missing
  tool is a failure instead of a pass. `postkit::grok::find_grk_compress` and
  the new `find_grk_decompress` scan PATH themselves rather than spawning
  `which`, which the Windows runner has no usable copy of.

- **The xmlsec1 tests run on the 1.2 and 1.3 series alike**: xmlsec 1.3 made
  the key search strict, so `--sign` stopped matching a `--privkey-pem` key
  against a template whose KeyInfo names a certificate, and `--verify` stopped
  building a chain out of the sibling X509Data elements a DCP signature puts
  the intermediate in. Every xmlsec1 call now goes through one
  `xmldsig::xmlsec1_cli`, which reads `xmlsec1 --help-all` once and adds
  `--lax-key-search` and `--verbose` where the tool has them, and takes the
  chain between the document's signer and the trusted root as an explicit
  `--untrusted-pem` list. Handing 1.3 a certificate that is not in the
  document's chain makes its key search give up, so the self-signed case passes
  none. Every assertion on an xmlsec1 or ffmpeg exit status now carries that
  run's stdout and stderr, which is what `--verbose` restores on 1.3.

- **No test skips over a missing tool or fixture**: `Check test tools` also
  demands xmllint and xmlsec1 on all three runners, which the Windows job gets
  from vcpkg's `libxml2[tools]` and `xmlsec[tools]` and macOS from brew, and
  the Linux job installs `fonts-dejavu-core`. The schemas the packaging tests
  validate against live in `tests/fixtures/xsd` now, so `POSTKIT_DCP_XSD_DIR`,
  `POSTKIT_IMF_PKL_XSD` and `IMFWIZARD_IMF_XSD_DIR` only redirect a test that
  already runs. The KDM schema test reads the DCP-o-matic fixtures unless
  `POSTKIT_SAMPLE_KDMS` names another directory, and the AssetUUID tests find
  asdcp-info in asdcplib-sys's own build output unless `POSTKIT_ASDCP_INFO`
  names one. The c14n cross-check against xmllint drops the carriage returns
  Windows adds instead of skipping there, and
  `required_extensions_fragment` binds the `ds` prefix that libdcp declares on
  the document root, which the extracted element otherwise used unbound. The
  font tests search each platform's font directories rather than
  `/usr/share/fonts` alone. `real_ecl_dcps_verify` reads the ECL set's signed
  CPLs and PKLs from `tests/fixtures/clairmeta-ecl`, so `POSTKIT_CLAIRMETA_DATA`
  only redirects it at a full ClairMeta_Data clone. One early return is left:
  an OpenSSL build that refuses RSA over SHA-1 cannot judge that document
  either way.

- **A picture wrap refuses a codestream the standard cannot carry**: `mxf_wrap`
  took any codestream for either standard, and the picture descriptor it opened
  asdcplib with left the JPEG2000 sub-descriptor zeroed, so a DCP's X'Y'Z'
  picture at Rsiz 0x0003 wrapped into an IMF track file and nothing in the stack
  said otherwise. That is what imfwizard shipped for two months. Three refusals
  now stand in both wrap paths, the batch `mxf_wrap` and the frame-at-a-time
  `IncrementalJ2kWrap`. A DCI cinema Rsiz into AS-02 is refused, because those
  samples are X'Y'Z' and an IMF reader will render them as RGB. An IMF Rsiz into
  AS-DCP is refused for the mirror reason. An AS-02 J2K wrap whose `hdr` is
  absent, or sets neither ColorPrimaries nor TransferCharacteristic, is refused,
  because ST 2067-21 signals the picture's colour on the RGBA essence descriptor
  and nothing else writes those two properties. Each profile refusal names the
  Rsiz it found. The descriptor now carries the real codestream header, parsed
  out of the first frame by asdcplib-rs `CodestreamHeader::parse`, so the
  sub-descriptor and the PictureEssenceCoding label the AS-02 writer derives from
  it describe the essence rather than zeroes. `rec709_sdr_picture_colour()` is
  the BT.709 primaries and transfer pair an SDR App 2E picture signals, for a
  caller with no mastering display to declare. imfwizard's `create` wraps X'Y'Z'
  cinema picture into AS-02 today, so it fails loud at the wrap until the slice
  that gives it an IMF encode lands.
- **`extract_frame` takes a content key, so encrypted DCP essence extracts**: it
  refused encrypted essence outright before, since it had nowhere to take a key.
  The signature gains `key: Option<[u8; 16]>` and both wizards' `frame-extract`
  gain `--key` (raw hex) and `--keys-json`. The profile that picks the decoder is
  now read from the decrypted codestream rather than the raw essence, because a
  frame read without its key is ciphertext and carries no readable header. A key
  handed to an input that is not JPEG 2000 MXF essence is refused rather than
  ignored, and one handed to unencrypted essence warns and goes unused. An
  encrypted codestream that is not DCI cinema is refused outright: grok would
  decode it but the DCDM transform does not fit its samples, and ffmpeg cannot
  decrypt it at all.
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
  `render_dcp_frame` and a key. Everything else still goes through ffmpeg. DCP
  essence comes out 8-bit sRGB where ffmpeg wrote 16-bit.
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

- `preview::resolve_picture_key(source, key_hex, keys_json)`, the content key
  from whichever source a caller was given, and
  `preview::picture_key_from_keys_json(source, keys_json)`, which matches the
  picture's own asset UUID rather than taking the first image key in the file, so
  it picks the right one for a package carrying a picture asset per reel.
  `ResolvedPicture` carries that `asset_uuid`.
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
