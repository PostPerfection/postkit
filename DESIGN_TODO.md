# Planned

- The preview shows only Rec.709 App 2E picture. `render_imf_frame` refuses
  ST 2084, HLG, BT.2020 and P3-D65 by name, which is most of what a real IMP
  carries: the display transform for them is a tone map plus a gamut conversion
  into sRGB, and neither exists in `colour` yet. 4:2:2 essence would also need
  chroma upsampling, which `grok_decoder` refuses by name today. The App 2E
  codestream in `tests/fixtures/imf4k_black_3840x2160.j2c` is P3D65 PQ per its
  CPL, so it pins the refusal, but it is a black leader frame and will prove
  nothing about a tone map. A frame with picture in it is wanted before that
  work starts. Same entry in imfwizard's DESIGN_TODO.
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
- GPU J2K encoding. The grok library itself has no GPU encode path: the
  pinned grok.h exposes acceleration only through a separately licensed
  accelerator plugin (`grk_plugin_load`, then `grk_plugin_init` with a device
  id, a licence key and an optional licence server), which speeds up T1 entropy
  coding and falls back to the CPU when it is absent. That plugin is what
  `grk_compress -G` and DCP-o-matic's `config grok-licence` drive. grokj2k-sys
  runs bindgen over the whole header with no allowlist, so `grk_plugin_load` and
  `grk_plugin_init` are already bound and nothing upstream is needed. What is
  left is a device and licence setting in both wizards passed into
  `grok_encoder`, and a machine with the plugin and a licence to prove the
  speed-up. Listed in both wizards' DESIGN_TODO as well.
- GPU J2K decode path. What CPU grok manages in process, measured on 2048x1080
  frames at 125 Mb/s: 68 ms a frame at full resolution (14.6 fps), 19 ms at
  `reduce` 1 and 5 ms at `reduce` 2. So 2K at 24 fps needs either two decode
  threads or a reduce, and 4K, four times the samples, is out of reach at full
  resolution however it is threaded. That is what the GPU path is for, and what
  the features gating on it wait for: SDI output, and the dcpdoctor/wizard player
  controls (loop dom#2700, speed dom#2917, markers dom#2893, waveform dom#3091,
  3D view modes dom#1974/dom#3165, A/V sync offset dom#3083). The accelerator
  plugin above is the same one that would serve this.
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
