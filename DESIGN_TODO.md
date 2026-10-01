# Planned

- In-process decode for every video encode (decided 2026-10-01). The ffmpeg
  pipe caps a real 4K encode at 50 fps on hercules (RTX 2080 Ti, Threadripper
  3960X) where frames fed from memory reach 110 fps on the device. ffmpeg's one
  muxer thread writes each 35 MB padded frame in 32 KB pieces: a bare reader gets
  74 fps, 99 without the pad, and pipe size or `-avioflags direct` change nothing.
  Decisions:
  - CPU and GPU encodes both decode in process through ffmpeg-next 9 (ffmpeg-sys-next
    for gaps). The pipe code is deleted, with no fallback.
  - FFmpeg 8.1.3, LGPL, built by us, because the closed GPU plugin shares the
    process. FFmpeg 9 is a later bump on its own.
  - Only the picture decode, its filter chain and the probing behind them move.
    Audio demux and every other feature keep running the user's ffmpeg program,
    several of them need x264, x265 or GPL-only filters.
  - Denoise switches from hqdn3d, which needs a GPL build, to atadenoise. Both run
    about 28 fps on 4K on hercules, so a denoised encode stays CPU bound. A device
    denoiser of our own, written from scratch, is a later slice.
  - The pad filter stays in the in-process graph. Padding on the device is a later
    measured optimisation.
  - Gate before the pipe code goes: byte-identical codestreams against the pipe on
    the encode fixtures (CPU and GPU, every pipe format and filter except denoise),
    every suite green, and at least 99 fps on hercules for the 60 s Toms clip.
  - The LGPL FFmpeg and libmpv come from one recipe in a public
    PostPerfection/ffmpeg-mpv-builds repo: source builds on all three platforms, a
    link closure licence check in its CI, pinned releases carrying the source
    tarballs and one third-party licence file per platform, installed by its own
    setup action. Every package bundles them, the deb and rpm drop the distro
    libmpv dependency, and setup-libmpv is archived after the switch.
  - Linux is one build on ubuntu-24.04 shared by the deb, rpm and AppImage. It
    bundles libplacebo too, because its soname is its API version (.351 on Fedora
    43, .360 on 44, .338 on Ubuntu 24.04), and links only system libraries whose
    sonames stay put. Windows and macOS take the rest of the closure from MSYS2
    and Homebrew.
  - Order: the build repo and in-process decode start together, decode pushes
    once the build repo has a release, the installer switch follows the build repo.
  - When the wizards turn `ffmpeg-decode` on, every binary and test crate that
    links postkit emits `FFMPEG_DIR/lib` as a link search path from its own build
    script (one shared helper). Cargo sorts dependency link paths by package
    name after the crate's own, every pkg-config crate emits `/usr/lib64`, and
    `alsa-sys` sorts before `ffmpeg-sys-next` and `postkit`, so on a box with a
    distro FFmpeg the wizard binary links the distro `libavcodec` (seen on Fedora
    43: the GUI carried both `libavcodec.so.61` and `.so.62`). postkit's own guard
    in build.rs only covers postkit's own test binaries. Verified 2026-10-01: the
    GUI's build script emitting the path links only the `FFMPEG_DIR` libraries.

- Stereoscopic JPEG 2000 stays on libmpv. `GrokPlayer::accepts` returns false for
  `EssenceType::Jpeg2000Stereo` and `load` refuses it by name, because the mono
  AS-DCP reader cannot read it. Needs asdcplib's stereo reader.
- Verify the non-blocking render live (2026-08-17). `render_opengl` now passes
  `MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME = 0` with `video-timing-offset` 0,
  because the default wait parked the app's main thread for most of each frame
  period and the whole wizard UI starved during playback (frozen play icon,
  playhead and timecode until paused). Headless GL probes pass; what remains is
  a hand pass in a wizard: transport controls track live during playback, A/V
  sync and smoothness are unchanged.

- Extract the wizards' progress event into postkit (2026-08-17, proposed, not
  accepted). Both wizards emit the same `PipelineProgress` from their src-tauri
  glue: job_id, stage, message, frame, total_frames, fps, elapsed_secs, percent.
  Same shape as the gui_job_queue move: the event type defaults into postkit,
  the wizards keep the tauri emit calls.

- Embedded playback hand pass. The libmpv render engine (src/mpv_render, libmpv
  feature) and the three guikit hosts are in both wizards and CI compiles all
  three platforms, but neither the macos nor the windows host has run on real
  hardware. Untested by hand on linux too: closing the preview panel (the GL
  area shrinks to 1x1 and the render loop must keep answering), and no
  automated orientation check exists because framebuffer readback returns
  black, so eyeball after any render change. Off linux nothing about linking
  (mpv.lib or libmpv.dll.a in MPV_LIB_DIR on windows, homebrew's mpv.pc on
  macos) or running is verified.
- Player controls the wizards lack, easyDCP Player parity: loop (dom#2700),
  markers (dom#2893), waveform (dom#3091), 3D view modes
  (dom#1974, dom#3165), A/V sync offset (dom#3083). They waited on real-time 4K
  decode, which the device backend now gives.
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
