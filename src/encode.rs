use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A frame rate as an exact fraction, so 24000/1001 reaches ffmpeg as itself
/// rather than as 24.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRate {
    pub numerator: u32,
    pub denominator: u32,
}

impl FrameRate {
    /// Panics on a zero numerator or denominator: no source carries one, so a
    /// caller passing one has a bug rather than an unusual rate.
    pub fn new(numerator: u32, denominator: u32) -> Self {
        assert!(
            numerator > 0 && denominator > 0,
            "frame rate {numerator}/{denominator} needs a non-zero numerator and denominator"
        );
        Self {
            numerator,
            denominator,
        }
    }

    /// A whole-number rate such as 24 or 25.
    pub fn whole(fps: u32) -> Self {
        Self::new(fps, 1)
    }

    pub fn as_f64(&self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }

    /// The rate as ffmpeg's `fps` filter takes it, `24000/1001` or `24`.
    pub fn ffmpeg_filter_value(&self) -> String {
        if self.denominator == 1 {
            return self.numerator.to_string();
        }
        format!("{}/{}", self.numerator, self.denominator)
    }

    /// One frame period in seconds, as an ffmpeg concat list writes it. The
    /// concat demuxer rejects a fraction, so this is decimal seconds.
    pub fn frame_duration_seconds(&self) -> String {
        format!(
            "{:.9}",
            f64::from(self.denominator) / f64::from(self.numerator)
        )
    }
}

impl Default for FrameRate {
    fn default() -> Self {
        Self::whole(24)
    }
}

/// JPEG 2000 encoding options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodeOptions {
    /// Source image sequence directory (DPX/TIFF/EXR/PNG/BMP)
    pub input_dir: PathBuf,
    /// Output J2K codestream directory
    pub output_dir: PathBuf,
    /// Target bitrate in Mbps (e.g. 250.0 for DCI 2K)
    pub bitrate_mbps: f64,
    /// Resolution: "2K" or "4K"
    pub resolution: String,
    /// Frame rate numerator
    pub fps_num: u32,
    /// Frame rate denominator
    pub fps_den: u32,
    /// Number of quality layers
    pub num_layers: u32,
    /// Progression order: "CPRL", "LRCP", "RLCP"
    pub progression: String,
    /// Number of decomposition levels
    pub num_resolutions: u32,
    /// Code block size (usually 32 or 64)
    pub codeblock_size: u32,
    /// Path to external grok compressor binary (grk_compress)
    pub compressor_path: PathBuf,
    /// GPU device index (-1 for CPU)
    pub gpu_device: i32,
    /// Number of parallel encoding threads
    pub num_threads: u32,
    /// Library directory for LD_LIBRARY_PATH (if needed)
    #[serde(default)]
    pub lib_dir: Option<PathBuf>,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            input_dir: PathBuf::new(),
            output_dir: PathBuf::new(),
            bitrate_mbps: 250.0,
            resolution: "2K".to_string(),
            fps_num: 24,
            fps_den: 1,
            num_layers: 1,
            progression: "CPRL".to_string(),
            num_resolutions: 6,
            codeblock_size: 32,
            compressor_path: PathBuf::new(),
            gpu_device: -1,
            num_threads: 0, // auto-detect
            lib_dir: None,
        }
    }
}

/// Result of encoding operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EncodeResult {
    pub success: bool,
    pub error: String,
    pub frames_encoded: u64,
    pub output_dir: PathBuf,
}

/// Image format detected from file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Dpx,
    Tiff,
    Exr,
    Png,
    Bmp,
    Jpeg,
    Unknown,
}

/// Input type detection for pipeline routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputType {
    Video,
    ImageSequence,
    J2kSequence,
    Unknown,
}

/// Detect input type from a file or directory path.
pub fn detect_input_type(path: &Path) -> InputType {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let ext = entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_lowercase())
                    .unwrap_or_default();
                match ext.as_str() {
                    "j2c" | "j2k" => return InputType::J2kSequence,
                    "tif" | "tiff" | "dpx" | "exr" | "bmp" | "jpg" | "jpeg" | "png" => {
                        return InputType::ImageSequence;
                    }
                    _ => {}
                }
            }
        }
        InputType::Unknown
    } else {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "mp4" | "mkv" | "mov" | "avi" | "mxf" | "webm" | "ts" | "m2ts" => InputType::Video,
            "tif" | "tiff" | "dpx" | "exr" | "bmp" | "jpg" | "jpeg" | "png" => {
                InputType::ImageSequence
            }
            "j2c" | "j2k" => InputType::J2kSequence,
            _ => InputType::Unknown,
        }
    }
}

/// Detect image format from file extension.
pub fn detect_image_format(path: &Path) -> ImageFormat {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("dpx") => ImageFormat::Dpx,
        Some("tif" | "tiff") => ImageFormat::Tiff,
        Some("exr") => ImageFormat::Exr,
        Some("png") => ImageFormat::Png,
        Some("bmp") => ImageFormat::Bmp,
        Some("jpg" | "jpeg") => ImageFormat::Jpeg,
        _ => ImageFormat::Unknown,
    }
}

/// Find source image files in a directory, sorted by name.
pub fn find_source_frames(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut frames: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && detect_image_format(p) != ImageFormat::Unknown)
        .collect();
    frames.sort();
    Ok(frames)
}

/// Encode a sequence of images to JPEG 2000 using an external compressor.
///
/// This spawns the compressor binary (e.g. `grk_compress`) for each frame.
/// For GPU-accelerated encoding, set `opts.gpu_device` to the device index.
pub fn encode(opts: &EncodeOptions) -> EncodeResult {
    let compressor = if opts.compressor_path.as_os_str().is_empty() {
        // Try to find grk_compress in PATH
        which_compressor()
    } else {
        Some(opts.compressor_path.clone())
    };

    let Some(compressor) = compressor else {
        return EncodeResult {
            success: false,
            error: "grk_compress not found in PATH and no compressor_path specified".to_string(),
            ..Default::default()
        };
    };

    let frames = match find_source_frames(&opts.input_dir) {
        Ok(f) => f,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: format!("Failed to read input directory: {e}"),
                ..Default::default()
            };
        }
    };

    if frames.is_empty() {
        return EncodeResult {
            success: false,
            error: "No source image files found in input directory".to_string(),
            ..Default::default()
        };
    }

    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return EncodeResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    let mut encoded = 0u64;
    for frame in &frames {
        let stem = frame.file_stem().unwrap_or_default();
        let output = opts
            .output_dir
            .join(format!("{}.j2c", stem.to_string_lossy()));

        let mut cmd = std::process::Command::new(&compressor);
        if let Some(ref ld) = opts.lib_dir {
            cmd.env("LD_LIBRARY_PATH", ld);
        }
        cmd.arg("-i")
            .arg(frame)
            .arg("-o")
            .arg(&output)
            .arg("-r")
            .arg(format!("{}", opts.bitrate_mbps))
            .arg("-X");

        if opts.gpu_device >= 0 {
            cmd.arg("-G").arg(format!("{}", opts.gpu_device));
        }
        if opts.num_threads > 0 {
            cmd.arg("-t").arg(format!("{}", opts.num_threads));
        }

        match cmd.output() {
            Ok(out) if out.status.success() => {
                encoded += 1;
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::error!("Encode failed for {:?}: {}", frame, stderr);
                return EncodeResult {
                    success: false,
                    error: format!("Encode failed at frame {}: {}", encoded, stderr),
                    frames_encoded: encoded,
                    output_dir: opts.output_dir.clone(),
                };
            }
            Err(e) => {
                return EncodeResult {
                    success: false,
                    error: format!("Failed to spawn compressor: {e}"),
                    frames_encoded: encoded,
                    output_dir: opts.output_dir.clone(),
                };
            }
        }
    }

    EncodeResult {
        success: true,
        error: String::new(),
        frames_encoded: encoded,
        output_dir: opts.output_dir.clone(),
    }
}

/// Try to find `grk_compress` in PATH.
fn which_compressor() -> Option<PathBuf> {
    std::process::Command::new("which")
        .arg("grk_compress")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(PathBuf::from(s))
            }
        })
}

// ─── Streaming encode (ffmpeg → raw pipe → grk_compress) ──────────────────

use std::io::Read;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What colour the source frames carry when they reach the J2K compressor.
///
/// The compressor's own DCDM X'Y'Z' transform is applied if and only if this is
/// `DisplayRgb`, so essence that a caller later labels ST 2084 PQ can never hold
/// frames the encoder transformed itself.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SourceColour {
    /// Display RGB. The compressor runs the DCDM X'Y'Z' transform.
    #[default]
    DisplayRgb,
    /// Display RGB in a space the compressor's own transform does not model
    /// (P3, Rec.2020): postkit converts every frame with that space's matrix
    /// before compression, and the compressor transform stays off.
    DisplayRgbIn(crate::colour::ColourSpace),
    /// HDR source converted to DCI X'Y'Z' by this 3D LUT during decode, so the
    /// compressor applies no further transform.
    DciLut(PathBuf),
    /// Source is already DCI X'Y'Z' with the ST 2084 PQ transfer function, and
    /// is compressed untransformed.
    AlreadyPq,
}

impl SourceColour {
    /// Whether the compressor has to run the DCDM X'Y'Z' transform.
    pub fn applies_xyz_transform(&self) -> bool {
        matches!(self, SourceColour::DisplayRgb)
    }

    /// The transform postkit runs over each frame before compression, built once
    /// for the whole run. Errs for a space no 3x3 matrix reaches X'Y'Z' from.
    pub fn frame_transform(&self) -> Result<Option<Arc<crate::colour::DcdmTransform>>, String> {
        match self {
            SourceColour::DisplayRgbIn(space) => Ok(Some(Arc::new(
                crate::colour::DcdmTransform::to_xyz(*space)?,
            ))),
            _ => Ok(None),
        }
    }
}

/// The window of a source to encode, so a caller keeping five minutes of a two
/// hour source encodes those five minutes and nothing else.
///
/// Frames are counted in output frames at the target `fps`, after the fps filter
/// or the read-rate override, and numbered from zero: frame N of the window is
/// the codestream a full encode would have written as frame
/// `first_frame + N`. The window's own codestreams are numbered from zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRange {
    pub first_frame: u64,
    pub frame_count: u64,
}

impl FrameRange {
    /// One past the last output frame the window covers.
    pub fn end_frame(&self) -> u64 {
        self.first_frame.saturating_add(self.frame_count)
    }

    fn longer_than(&self, source_frames: u64) -> String {
        format!(
            "frames {}..{} were asked for, but the source is {source_frames} frames long",
            self.first_frame,
            self.end_frame()
        )
    }

    /// Refuse a window the source cannot fill. A zero `source_frames` is a probe
    /// that read no count rather than an empty source, so it is not checked.
    pub fn check_against_probe(&self, source_frames: u64) -> Result<(), String> {
        if source_frames == 0 || self.end_frame() <= source_frames {
            return Ok(());
        }
        Err(self.longer_than(source_frames))
    }

    /// The window's frames of a source list, refusing a window the list cannot
    /// fill.
    pub fn window_of<'a, T>(&self, frames: &'a [T]) -> Result<&'a [T], String> {
        let available = frames.len() as u64;
        if self.end_frame() > available {
            return Err(self.longer_than(available));
        }
        Ok(&frames[self.first_frame as usize..self.end_frame() as usize])
    }

    /// The ffmpeg output option that stops the decode at the window's end
    /// instead of running the source out.
    pub(crate) fn frame_limit_args(&self) -> [String; 2] {
        ["-frames:v".to_string(), self.frame_count.to_string()]
    }

    /// The ffmpeg filters that drop everything outside the window and restamp
    /// the kept frames from zero.
    pub(crate) fn trim_filters(&self) -> [String; 2] {
        [
            format!(
                "trim=start_frame={}:end_frame={}",
                self.first_frame,
                self.end_frame()
            ),
            "setpts=PTS-STARTPTS".to_string(),
        ]
    }
}

/// The ffmpeg filter chain for a stream decode: the picture plan, the output
/// frame rate at the position the plan names, the frame window right after that
/// rate, plus the HDR-to-DCI LUT last when the source needs one, so the LUT sees
/// the finished picture.
pub(crate) fn decode_filters(
    fps: FrameRate,
    source_colour: &SourceColour,
    plan: &crate::picture_processing::PicturePlan,
    frame_range: Option<FrameRange>,
) -> String {
    let mut filters = plan.filters.clone();
    filters.insert(
        plan.fps_position,
        format!("fps={}", fps.ffmpeg_filter_value()),
    );
    // the window is counted in output frames, not source frames
    if let Some(range) = frame_range {
        let after_fps = plan.fps_position + 1;
        filters.splice(after_fps..after_fps, range.trim_filters());
    }
    if let SourceColour::DciLut(lut) = source_colour {
        filters.push(format!("lut3d={}", lut.display()));
    }
    filters.join(",")
}

/// Reject a written codestream that exceeds the per-frame byte cap (DCI caps
/// each codestream, and the HDR Addendum raises the cap rather than removing it).
pub(crate) fn check_codestream_size(frame: &Path, cap: u64) -> Result<(), String> {
    let size = std::fs::metadata(frame)
        .map_err(|e| format!("cannot size {}: {e}", frame.display()))?
        .len();
    if size > cap {
        return Err(format!(
            "codestream {} is {size} bytes, over the {cap} byte per-frame cap: lower the bitrate",
            frame.display()
        ));
    }
    Ok(())
}

/// What ffmpeg opens for a stream encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DecodeSource {
    /// A container ffmpeg opens directly.
    #[default]
    Video,
    /// A concat list naming the stills of an image sequence, one frame each.
    /// This is how a sequence reaches the same per-frame path a video does,
    /// which is what a subtitle burn needs.
    ImageList,
}

impl DecodeSource {
    /// Demuxer arguments that go before `-i`.
    pub(crate) fn demuxer_args(&self) -> &'static [&'static str] {
        match self {
            DecodeSource::Video => &[],
            DecodeSource::ImageList => &["-f", "concat", "-safe", "0"],
        }
    }
}

/// Every ffmpeg argument that goes before `-i` for a stream decode: what the
/// demuxer needs, plus the input rate when the caller reads the source at a rate
/// other than its own.
pub(crate) fn decode_input_args(
    decode_source: DecodeSource,
    read_source_at: Option<FrameRate>,
) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = decode_source
        .demuxer_args()
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
    let Some(rate) = read_source_at else {
        return Ok(args);
    };
    if decode_source == DecodeSource::ImageList {
        return Err(format!(
            "a concat list of stills already holds each frame for one period, so it cannot also \
             be read at {} fps",
            rate.ffmpeg_filter_value()
        ));
    }
    args.push("-r".to_string());
    args.push(rate.ffmpeg_filter_value());
    Ok(args)
}

/// Every ffmpeg argument after `-i` for a stream decode: the filter chain and
/// the raw output on stdout, plus a frame limit for a window so ffmpeg stops at
/// the window's end instead of decoding the rest of the source.
pub(crate) fn decode_output_args(filters: &str, frame_range: Option<FrameRange>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-vf", filters, "-pix_fmt", "rgb48be", "-f", "rawvideo", "-an",
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect();
    if let Some(range) = frame_range {
        args.extend(range.frame_limit_args());
    }
    args.push("pipe:1".to_string());
    args
}

/// Write an ffmpeg concat list holding every frame of an image sequence for one
/// frame period, in the order [`find_source_frames`] returns them.
///
/// Absolute paths, so the list can live anywhere; single quotes inside a path
/// are escaped the way the concat demuxer reads them.
pub fn write_image_concat_list(
    frames: &[PathBuf],
    fps: FrameRate,
    list_path: &Path,
) -> Result<(), String> {
    let duration = fps.frame_duration_seconds();
    let mut list = String::from("ffconcat version 1.0\n");
    for frame in frames {
        let absolute = frame
            .canonicalize()
            .map_err(|e| format!("cannot resolve {}: {e}", frame.display()))?;
        let quoted = absolute.to_string_lossy().replace('\'', r"'\''");
        list.push_str(&format!("file '{quoted}'\nduration {duration}\n"));
    }
    std::fs::write(list_path, list)
        .map_err(|e| format!("cannot write {}: {e}", list_path.display()))
}

/// Options for streaming encode (video → J2K without intermediate files).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEncodeOptions {
    /// Input video file, or the concat list when `decode_source` names one.
    pub input: PathBuf,
    /// Output J2K directory
    pub output_dir: PathBuf,
    /// Target compression ratio (e.g. 10 for 10:1)
    pub compression_ratio: f64,
    /// Number of decomposition levels
    pub num_resolutions: u32,
    /// Code block size
    pub codeblock_size: u32,
    /// Progression order
    pub progression: String,
    /// Target frame rate for output (ffmpeg fps filter)
    pub fps: FrameRate,
    /// Read the source as if it ran at this rate, ignoring its own timestamps.
    /// This is how a 23.976 source becomes 24 fps by playing 0.1% faster: every
    /// frame reaches the encoder once, none is duplicated or dropped. Video
    /// only, and the sound needs the matching pull-up.
    #[serde(default)]
    pub read_source_at: Option<FrameRate>,
    /// Encode only this window of the source instead of all of it. The frames
    /// before it are still decoded and thrown away, which costs far less than
    /// compressing them.
    #[serde(default)]
    pub frame_range: Option<FrameRange>,
    /// Path to compressor binary (auto-detected if empty)
    pub compressor_path: PathBuf,
    /// Library directory for LD_LIBRARY_PATH (if needed)
    pub lib_dir: Option<PathBuf>,
    /// Colour the decoded frames carry, which decides the encoder transform.
    #[serde(default)]
    pub source_colour: SourceColour,
    /// Whether `input` is a container or a concat list of stills.
    #[serde(default)]
    pub decode_source: DecodeSource,
    /// Crop, deinterlace, rotate, flip, denoise and raster fit applied during
    /// the decode, which decides the size of the frames the encoder receives.
    #[serde(default)]
    pub picture: crate::picture_processing::PictureProcessing,
    /// Subtitles burnt into each decoded frame before it is compressed. Not
    /// serialised: it carries a live font database, so a stored job names the
    /// subtitle file and rebuilds it.
    #[serde(skip)]
    pub subtitle_burn: Option<std::sync::Arc<crate::subtitle_raster::SubtitleBurn>>,
    /// Per-codestream byte cap, e.g. the DCI HDR Addendum's raised cap. Each
    /// codestream is checked as it is written and the first one over the cap
    /// fails the encode there, so a bitrate set too high costs one frame rather
    /// than the whole sequence. Only the in-process encoder can hold to it;
    /// [`stream_encode_subprocess`] refuses a cap instead of ignoring it.
    #[serde(default)]
    pub codestream_byte_cap: Option<u64>,
}

impl Default for StreamEncodeOptions {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            output_dir: PathBuf::new(),
            compression_ratio: 10.0,
            num_resolutions: 6,
            codeblock_size: 32,
            progression: "CPRL".to_string(),
            fps: FrameRate::default(),
            read_source_at: None,
            frame_range: None,
            compressor_path: PathBuf::new(),
            lib_dir: None,
            source_colour: SourceColour::DisplayRgb,
            decode_source: DecodeSource::Video,
            picture: crate::picture_processing::PictureProcessing::default(),
            subtitle_burn: None,
            codestream_byte_cap: None,
        }
    }
}

/// Progress callback for streaming encode.
///
/// The four phase clocks say where the time inside the encode went. Each is
/// cumulative over the whole run so far, and a phase nothing measures on the
/// chosen path stays at zero.
pub struct StreamProgress {
    pub frame: u64,
    pub total_frames: u64,
    pub fps: f64,
    pub elapsed_secs: f64,
    /// Time the frame reader spent blocked on ffmpeg's pipe.
    pub decode_wait_secs: f64,
    /// Time spent burning subtitles and converting colour, summed over the
    /// encoder threads, so it can exceed `elapsed_secs`. Zero on the subprocess
    /// path, which refuses both.
    pub prepare_secs: f64,
    /// Time spent compressing, summed over the encoder threads, so it can
    /// exceed `elapsed_secs`. On the subprocess path this is the whole
    /// grk_compress run, which also covers the child's own read and write.
    pub encode_secs: f64,
    /// Time spent writing codestreams to disk. Zero on the subprocess path,
    /// where grk_compress writes them inside the child.
    pub write_secs: f64,
}

/// Find the grk_compress binary, checking common locations.
pub fn find_compressor() -> Option<(PathBuf, Option<PathBuf>)> {
    // Check $HOME/bin/grok/bin first
    if let Ok(home) = std::env::var("HOME") {
        let grk = PathBuf::from(&home).join("bin/grok/bin/grk_compress");
        if grk.exists() {
            let lib_dir = PathBuf::from(&home).join("bin/grok/lib64");
            return Some((grk, Some(lib_dir)));
        }
    }
    // Check PATH
    if let Some(p) = which_compressor() {
        return Some((p, None));
    }
    None
}

/// Probe a video file for dimensions and frame count.
pub fn probe_video(input: &Path) -> (u32, u32, u64) {
    probe_decode_source(input, DecodeSource::Video)
}

/// Probe whatever ffmpeg will decode for dimensions and frame count.
pub fn probe_decode_source(input: &Path, source: DecodeSource) -> (u32, u32, u64) {
    let demuxer = source.demuxer_args();
    let dim_output = std::process::Command::new("ffprobe")
        .args(["-v", "error"])
        .args(demuxer)
        .args([
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
        ])
        .arg(input)
        .output();

    let (width, height) = match dim_output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let parts: Vec<&str> = s.split('x').collect();
            if parts.len() == 2 {
                (parts[0].parse().unwrap_or(0), parts[1].parse().unwrap_or(0))
            } else {
                (0, 0)
            }
        }
        _ => (0, 0),
    };

    let count_output = std::process::Command::new("ffprobe")
        .args(["-v", "error"])
        .args(demuxer)
        .args([
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .output();

    let frame_count = match count_output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        _ => 0,
    };

    (width, height, frame_count)
}

pub(crate) enum ReadResult {
    Ok,
    Eof,
    Err(std::io::Error),
}

pub(crate) fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> ReadResult {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Result::Ok(0) => {
                return if filled == 0 {
                    ReadResult::Eof
                } else {
                    ReadResult::Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "partial frame",
                    ))
                };
            }
            Result::Ok(n) => filled += n,
            Err(e) => return ReadResult::Err(e),
        }
    }
    ReadResult::Ok
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ─── In-process stream encode (video → ffmpeg pipe → Grok FFI) ─────────────

/// Stream-encode a video file to J2K using in-process Grok FFI.
///
/// Uses ffmpeg to decode the video to raw 16-bit RGB frames, then compresses
/// each frame in-process via the bounded-queue pipeline in `grok_encoder`.
/// This eliminates per-frame subprocess overhead.
pub fn stream_encode_inprocess<F>(
    opts: &StreamEncodeOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: F,
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    stream_encode_inprocess_with_mxf_feed(opts, cancel, pause, None, on_progress)
}

/// Like [`stream_encode_inprocess`], but each codestream also goes to `mxf_feed`
/// as it is written, so a picture MXF can be wrapped while the encode runs. See
/// [`crate::grok_encoder::encode_pipeline_with_mxf_feed`].
pub fn stream_encode_inprocess_with_mxf_feed<F>(
    opts: &StreamEncodeOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    mut on_progress: F,
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    use crate::grok_encoder::{self, CompressParams, RawFrame};

    if let SourceColour::DciLut(lut) = &opts.source_colour
        && !lut.is_file()
    {
        return EncodeResult {
            success: false,
            error: format!("HDR-to-DCI LUT not found: {}", lut.display()),
            ..Default::default()
        };
    }

    let (source_width, source_height, source_frames) =
        probe_decode_source(&opts.input, opts.decode_source);
    if source_width == 0 || source_height == 0 {
        return EncodeResult {
            success: false,
            error: "Could not determine video dimensions".to_string(),
            ..Default::default()
        };
    }
    if let Some(range) = opts.frame_range
        && let Err(e) = range.check_against_probe(source_frames)
    {
        return EncodeResult {
            success: false,
            error: e,
            ..Default::default()
        };
    }
    let total_frames = opts
        .frame_range
        .map_or(source_frames, |range| range.frame_count);
    let plan = match opts.picture.plan(source_width, source_height) {
        Ok(plan) => plan,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: e,
                ..Default::default()
            };
        }
    };
    tracing::info!("picture: {}", plan.describe());
    let (width, height) = (plan.output_width, plan.output_height);

    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return EncodeResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    let frame_size = (width as usize) * (height as usize) * 3 * 2; // 16-bit RGB

    // Start ffmpeg: decode to raw 16-bit big-endian RGB
    let filters = decode_filters(opts.fps, &opts.source_colour, &plan, opts.frame_range);
    let input_args = match decode_input_args(opts.decode_source, opts.read_source_at) {
        Ok(args) => args,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: e,
                ..Default::default()
            };
        }
    };
    tracing::debug!(
        "ffmpeg -y {} -i {} -vf {filters}",
        input_args.join(" "),
        opts.input.display()
    );
    let mut ffmpeg = match std::process::Command::new("ffmpeg")
        .arg("-y")
        .args(&input_args)
        .arg("-i")
        .arg(&opts.input)
        .args(decode_output_args(&filters, opts.frame_range))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: format!("Failed to start ffmpeg: {e}"),
                ..Default::default()
            };
        }
    };

    let mut ffmpeg_stdout = match ffmpeg.stdout.take() {
        Some(s) => s,
        None => {
            return EncodeResult {
                success: false,
                error: "Failed to capture ffmpeg stdout".to_string(),
                ..Default::default()
            };
        }
    };

    let colour_transform = match opts.source_colour.frame_transform() {
        Ok(t) => t,
        Err(e) => {
            kill_child(&mut ffmpeg);
            return EncodeResult {
                success: false,
                error: e,
                ..Default::default()
            };
        }
    };

    let params = CompressParams {
        compression_ratio: opts.compression_ratio,
        num_resolutions: opts.num_resolutions as u8,
        codeblock_size: opts.codeblock_size,
        // grok only sizes the per-frame byte budget from this, so the whole rate is enough
        frame_rate: opts.fps.as_f64().round() as u16,
        apply_xyz_transform: opts.source_colour.applies_xyz_transform(),
        source_preparation: grok_encoder::SourcePreparation {
            subtitle_burn: opts.subtitle_burn.clone(),
            colour_transform,
        },
        ..CompressParams::default()
    };

    grok_encoder::initialize(0);

    let mut frame_buf = vec![0u8; frame_size];
    let mut frame_index: u64 = 0;
    let encode_start = std::time::Instant::now();
    let phase_clocks = Arc::new(grok_encoder::PhaseClocks::default());

    let result = grok_encoder::encode_pipeline_with_mxf_feed(
        &opts.output_dir,
        &params,
        total_frames,
        cancel,
        &phase_clocks,
        mxf_feed,
        opts.codestream_byte_cap,
        || {
            while pause.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let read_start = std::time::Instant::now();
            let read = read_exact_or_eof(&mut ffmpeg_stdout, &mut frame_buf);
            phase_clocks.add(grok_encoder::EncodePhase::DecoderWait, read_start.elapsed());
            match read {
                ReadResult::Ok => {}
                ReadResult::Eof => return None,
                ReadResult::Err(_) => return None,
            }

            let idx = frame_index;
            frame_index += 1;

            // Pass packed bytes directly — encoder threads will deinterleave
            // into Grok's component buffers (avoids 21MB intermediate alloc)
            Some(RawFrame::Packed {
                data: frame_buf.clone(),
                width,
                height,
                precision: 16,
                index: idx,
            })
        },
        |progress| {
            let elapsed = encode_start.elapsed().as_secs_f64();
            on_progress(StreamProgress {
                frame: progress.frames_encoded,
                total_frames: progress.total_frames,
                fps: progress.fps,
                elapsed_secs: elapsed,
                decode_wait_secs: progress.decode_wait_secs,
                prepare_secs: progress.prepare_secs,
                encode_secs: progress.encode_secs,
                write_secs: progress.write_secs,
            });
        },
    );

    kill_child(&mut ffmpeg);

    EncodeResult {
        success: result.success,
        error: result.error,
        frames_encoded: result.frames_encoded,
        output_dir: opts.output_dir.clone(),
    }
}

/// Stream-encode using subprocess pool (ffmpeg → raw frames → grk_compress subprocesses).
///
/// This achieves higher throughput than the FFI path because each subprocess
/// gets its own independent Grok thread pool. Temporary frames are written to
/// /dev/shm (ramdisk) to avoid disk I/O bottleneck.
pub fn stream_encode_subprocess<F>(
    opts: &StreamEncodeOptions,
    cancel: &Arc<AtomicBool>,
    mut on_progress: F,
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    use crate::grok_encoder;

    if let SourceColour::DciLut(lut) = &opts.source_colour
        && !lut.is_file()
    {
        return EncodeResult {
            success: false,
            error: format!("HDR-to-DCI LUT not found: {}", lut.display()),
            ..Default::default()
        };
    }
    if let SourceColour::DisplayRgbIn(space) = &opts.source_colour {
        return EncodeResult {
            success: false,
            error: format!(
                "a {space:?} source needs the in-process encoder: the subprocess pool hands \
                 raw frames to grk_compress, which only converts Rec.709"
            ),
            ..Default::default()
        };
    }
    if opts.codestream_byte_cap.is_some() {
        return EncodeResult {
            success: false,
            error: "a per-frame byte cap needs the in-process encoder: here grk_compress \
                    writes each codestream itself, so postkit never has one to size"
                .to_string(),
            ..Default::default()
        };
    }

    let (source_width, source_height, source_frames) =
        probe_decode_source(&opts.input, opts.decode_source);
    if source_width == 0 || source_height == 0 {
        return EncodeResult {
            success: false,
            error: "Could not determine video dimensions".to_string(),
            ..Default::default()
        };
    }
    if let Some(range) = opts.frame_range
        && let Err(e) = range.check_against_probe(source_frames)
    {
        return EncodeResult {
            success: false,
            error: e,
            ..Default::default()
        };
    }
    let total_frames = opts
        .frame_range
        .map_or(source_frames, |range| range.frame_count);
    let plan = match opts.picture.plan(source_width, source_height) {
        Ok(plan) => plan,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: e,
                ..Default::default()
            };
        }
    };
    tracing::info!("picture: {}", plan.describe());
    let (width, height) = (plan.output_width, plan.output_height);

    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return EncodeResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    let frame_size = (width as usize) * (height as usize) * 3 * 2;

    // Start ffmpeg
    let filters = decode_filters(opts.fps, &opts.source_colour, &plan, opts.frame_range);
    let input_args = match decode_input_args(opts.decode_source, opts.read_source_at) {
        Ok(args) => args,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: e,
                ..Default::default()
            };
        }
    };
    tracing::debug!(
        "ffmpeg -y {} -i {} -vf {filters}",
        input_args.join(" "),
        opts.input.display()
    );
    let mut ffmpeg = match std::process::Command::new("ffmpeg")
        .arg("-y")
        .args(&input_args)
        .arg("-i")
        .arg(&opts.input)
        .args(decode_output_args(&filters, opts.frame_range))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: format!("Failed to launch ffmpeg: {e}"),
                ..Default::default()
            };
        }
    };

    let mut ffmpeg_stdout = match ffmpeg.stdout.take() {
        Some(s) => s,
        None => {
            return EncodeResult {
                success: false,
                error: "Failed to capture ffmpeg stdout".to_string(),
                ..Default::default()
            };
        }
    };

    let params = grok_encoder::CompressParams {
        compression_ratio: opts.compression_ratio,
        num_resolutions: opts.num_resolutions as u8,
        codeblock_size: opts.codeblock_size,
        // grok only sizes the per-frame byte budget from this, so the whole rate is enough
        frame_rate: opts.fps.as_f64().round() as u16,
        apply_xyz_transform: opts.source_colour.applies_xyz_transform(),
        ..grok_encoder::CompressParams::default()
    };

    let grk_bin = if opts.compressor_path.as_os_str().is_empty() {
        PathBuf::from("grk_compress")
    } else {
        opts.compressor_path.clone()
    };

    let encode_start = std::time::Instant::now();

    let result = grok_encoder::encode_pipeline_subprocess(
        &opts.output_dir,
        &params,
        &grk_bin,
        total_frames,
        width,
        height,
        frame_size,
        &mut ffmpeg_stdout,
        cancel,
        |progress| {
            let elapsed = encode_start.elapsed().as_secs_f64();
            on_progress(StreamProgress {
                frame: progress.frames_encoded,
                total_frames: progress.total_frames,
                fps: progress.fps,
                elapsed_secs: elapsed,
                decode_wait_secs: progress.decode_wait_secs,
                prepare_secs: progress.prepare_secs,
                encode_secs: progress.encode_secs,
                write_secs: progress.write_secs,
            });
        },
    );

    kill_child(&mut ffmpeg);

    EncodeResult {
        success: result.success,
        error: result.error,
        frames_encoded: result.frames_encoded,
        output_dir: opts.output_dir.clone(),
    }
}

// ─── Parallel encode (image sequence → parallel grk_compress subprocesses) ─

/// Progress callback for parallel encode.
pub struct ParallelProgress {
    pub done: u64,
    pub total: u64,
    pub fps: f64,
    pub elapsed_secs: f64,
}

/// Encode the given image files using parallel single-threaded subprocesses.
///
/// Spawns up to `parallelism` grk_compress processes concurrently, each
/// processing one frame with `-H 1` (single thread). Returns when all
/// frames are encoded or an error occurs. Each finished frame is held to
/// `codestream_byte_cap` so a run fails at the frame that breaks it.
///
/// The caller lists the sequence with [`find_source_frames`] and passes the
/// frames it wants, so encoding a window is passing fewer of them.
pub fn encode_parallel<F>(
    frames: &[PathBuf],
    output_dir: &Path,
    compression_ratio: f64,
    codestream_byte_cap: Option<u64>,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    mut on_progress: F,
) -> EncodeResult
where
    F: FnMut(ParallelProgress),
{
    if frames.is_empty() {
        return EncodeResult {
            success: false,
            error: "No source image files found".to_string(),
            ..Default::default()
        };
    }

    let total = frames.len() as u64;

    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return EncodeResult {
            success: false,
            error: format!("Failed to create output dir: {e}"),
            ..Default::default()
        };
    }

    let grk_bin = match crate::grok::find_grk_compress() {
        Some(p) => p,
        None => {
            return EncodeResult {
                success: false,
                error: "Cannot find grk_compress binary".to_string(),
                ..Default::default()
            };
        }
    };
    let lib_path = crate::grok::grok_lib_path();

    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(16);

    let encode_start = std::time::Instant::now();
    let done_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let error_flag = Arc::new(AtomicBool::new(false));
    let first_error = Arc::new(std::sync::Mutex::new(String::new()));

    let work_idx = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    std::thread::scope(|s| {
        let _workers: Vec<_> = (0..parallelism)
            .map(|_| {
                let work_idx = work_idx.clone();
                let done_count = done_count.clone();
                let error_flag = error_flag.clone();
                let first_error = first_error.clone();
                let grk_bin = &grk_bin;
                let lib_path = &lib_path;

                s.spawn(move || {
                    loop {
                        if cancel.load(Ordering::Relaxed) || error_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        while pause.load(Ordering::Relaxed) {
                            if cancel.load(Ordering::Relaxed) {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }

                        let idx = work_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= frames.len() {
                            break;
                        }

                        let frame = &frames[idx];
                        let stem = frame
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("frame");
                        let out_file = output_dir.join(format!("{stem}.j2k"));

                        let result = std::process::Command::new(grk_bin)
                            .env("LD_LIBRARY_PATH", lib_path)
                            .args([
                                "-i",
                                &frame.to_string_lossy(),
                                "-o",
                                &out_file.to_string_lossy(),
                                "--xyz",
                                "-r",
                                &format!("{compression_ratio}"),
                                "-n",
                                "6",
                                "-b",
                                "32,32",
                                "-p",
                                "CPRL",
                                "-H",
                                "1",
                                "-X",
                            ])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();

                        match result {
                            Ok(status) if status.success() => {
                                if let Some(cap) = codestream_byte_cap
                                    && let Err(e) = check_codestream_size(&out_file, cap)
                                {
                                    error_flag.store(true, Ordering::Relaxed);
                                    let mut err = first_error.lock().unwrap();
                                    if err.is_empty() {
                                        *err = e;
                                    }
                                    continue;
                                }
                                done_count.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(status) => {
                                error_flag.store(true, Ordering::Relaxed);
                                let mut err = first_error.lock().unwrap();
                                if err.is_empty() {
                                    *err = format!(
                                        "grk_compress failed on {}: {}",
                                        frame.display(),
                                        status
                                    );
                                }
                            }
                            Err(e) => {
                                error_flag.store(true, Ordering::Relaxed);
                                let mut err = first_error.lock().unwrap();
                                if err.is_empty() {
                                    *err = format!(
                                        "Failed to spawn grk_compress for {}: {}",
                                        frame.display(),
                                        e
                                    );
                                }
                            }
                        }
                    }
                })
            })
            .collect();

        // Monitor progress
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));

            let done = done_count.load(Ordering::Relaxed);
            let elapsed = encode_start.elapsed().as_secs_f64();
            let fps = if elapsed > 0.0 {
                done as f64 / elapsed
            } else {
                0.0
            };

            on_progress(ParallelProgress {
                done,
                total,
                fps,
                elapsed_secs: elapsed,
            });

            if done >= total || error_flag.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed)
            {
                break;
            }
        }

        // Scoped threads join automatically here
    });

    if cancel.load(Ordering::Relaxed) {
        return EncodeResult {
            success: false,
            error: "Cancelled".to_string(),
            frames_encoded: done_count.load(Ordering::Relaxed),
            output_dir: output_dir.to_path_buf(),
        };
    }

    let err_msg = first_error.lock().unwrap();
    if !err_msg.is_empty() {
        return EncodeResult {
            success: false,
            error: err_msg.clone(),
            frames_encoded: done_count.load(Ordering::Relaxed),
            output_dir: output_dir.to_path_buf(),
        };
    }

    EncodeResult {
        success: true,
        error: String::new(),
        frames_encoded: total,
        output_dir: output_dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_noise_png(path: &Path, seed: u32) {
        let (w, h) = (128u32, 128u32);
        let mut state = seed;
        let data: Vec<u8> = (0..(w * h * 3))
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&data)
            .unwrap();
    }

    #[test]
    fn parallel_encode_honours_the_ratio_and_the_cap() {
        if crate::grok::find_grk_compress().is_none() {
            eprintln!("skipping: grk_compress not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in");
        std::fs::create_dir(&input).unwrap();
        for i in 0..2u32 {
            write_noise_png(&input.join(format!("frame_{i:03}.png")), 7 + i);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
        let sizes = |out: &Path| -> Vec<u64> {
            let mut v: Vec<u64> = std::fs::read_dir(out)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.metadata().unwrap().len())
                .collect();
            v.sort();
            v
        };

        let frames = find_source_frames(&input).unwrap();
        let loose_dir = dir.path().join("r10");
        let result = encode_parallel(&frames, &loose_dir, 10.0, None, &cancel, &pause, |_| {});
        assert!(result.success, "{}", result.error);
        let tight_dir = dir.path().join("r40");
        let result = encode_parallel(&frames, &tight_dir, 40.0, None, &cancel, &pause, |_| {});
        assert!(result.success, "{}", result.error);
        let loose = sizes(&loose_dir);
        let tight = sizes(&tight_dir);
        assert_eq!(loose.len(), 2);
        assert_eq!(tight.len(), 2);
        assert!(
            tight[1] < loose[0],
            "noise at 40:1 must be smaller than at 10:1, got {tight:?} vs {loose:?}"
        );

        let capped_dir = dir.path().join("capped");
        let result = encode_parallel(
            &frames,
            &capped_dir,
            10.0,
            Some(64),
            &cancel,
            &pause,
            |_| {},
        );
        assert!(!result.success, "a 64 byte cap cannot hold a codestream");
        assert!(
            result.error.contains("per-frame cap"),
            "got: {}",
            result.error
        );
    }

    #[test]
    fn parallel_encode_compresses_only_the_frames_it_is_given() {
        if crate::grok::find_grk_compress().is_none() {
            eprintln!("skipping: grk_compress not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in");
        std::fs::create_dir(&input).unwrap();
        for i in 0..6u32 {
            write_noise_png(&input.join(format!("frame_{i:03}.png")), 11 + i);
        }
        let frames = find_source_frames(&input).unwrap();
        let window = FrameRange {
            first_frame: 2,
            frame_count: 3,
        }
        .window_of(&frames)
        .unwrap();

        let output = dir.path().join("out");
        let result = encode_parallel(
            window,
            &output,
            10.0,
            None,
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert!(result.success, "{}", result.error);
        assert_eq!(result.frames_encoded, 3);

        let mut written: Vec<String> = std::fs::read_dir(&output)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        written.sort();
        assert_eq!(
            written,
            vec!["frame_002.j2k", "frame_003.j2k", "frame_004.j2k"],
            "only the window's stills may be compressed"
        );
    }

    #[test]
    fn a_frame_window_trims_after_the_fps_filter() {
        let plain = crate::picture_processing::PictureProcessing::default()
            .plan(1920, 1080)
            .unwrap();
        let window = FrameRange {
            first_frame: 7200,
            frame_count: 120,
        };
        assert_eq!(
            decode_filters(
                FrameRate::whole(24),
                &SourceColour::DisplayRgb,
                &plain,
                Some(window)
            ),
            "fps=24,trim=start_frame=7200:end_frame=7320,setpts=PTS-STARTPTS"
        );
        assert_eq!(
            decode_filters(
                FrameRate::whole(24),
                &SourceColour::DisplayRgb,
                &plain,
                None
            ),
            "fps=24",
            "no window has to leave the chain as it was"
        );

        // a deinterlace runs before the fps filter, so the window still has to
        // land after it and before the lut
        let processed = crate::picture_processing::PictureProcessing {
            deinterlace: true,
            denoise: true,
            ..crate::picture_processing::PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap();
        assert_eq!(
            decode_filters(
                FrameRate::whole(24),
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                &processed,
                Some(window)
            ),
            "yadif,fps=24,trim=start_frame=7200:end_frame=7320,setpts=PTS-STARTPTS,hqdn3d,\
             lut3d=/luts/hdr_to_dci.cube"
        );
    }

    #[test]
    fn a_frame_window_stops_ffmpeg_at_its_end() {
        assert_eq!(
            decode_output_args("fps=24", None),
            vec![
                "-vf", "fps=24", "-pix_fmt", "rgb48be", "-f", "rawvideo", "-an", "pipe:1"
            ]
        );
        assert_eq!(
            decode_output_args(
                "fps=24",
                Some(FrameRange {
                    first_frame: 10,
                    frame_count: 5,
                })
            ),
            vec![
                "-vf",
                "fps=24",
                "-pix_fmt",
                "rgb48be",
                "-f",
                "rawvideo",
                "-an",
                "-frames:v",
                "5",
                "pipe:1"
            ],
            "ffmpeg has to stop at the window instead of decoding to the end"
        );
    }

    #[test]
    fn a_window_past_the_end_of_the_source_is_refused() {
        let window = FrameRange {
            first_frame: 10,
            frame_count: 5,
        };
        assert_eq!(window.end_frame(), 15);
        assert!(window.check_against_probe(15).is_ok());
        assert!(window.check_against_probe(48).is_ok());
        assert!(
            window.check_against_probe(0).is_ok(),
            "a probe that read no count says nothing about the length"
        );
        let error = window.check_against_probe(14).unwrap_err();
        assert!(error.contains("10..15"), "{error}");
        assert!(error.contains("14 frames long"), "{error}");

        let frames: Vec<u64> = (0..15).collect();
        assert_eq!(window.window_of(&frames).unwrap(), &[10, 11, 12, 13, 14]);
        let error = window.window_of(&frames[..14]).unwrap_err();
        assert!(error.contains("14 frames long"), "{error}");
        assert!(
            window.window_of(&[] as &[u64]).is_err(),
            "an empty list cannot fill any window"
        );
    }

    #[test]
    fn a_concat_list_holds_only_the_windowed_frames() {
        let dir = tempfile::tempdir().unwrap();
        let frames: Vec<PathBuf> = (0..8)
            .map(|index| {
                let frame = dir.path().join(format!("frame_{index:03}.png"));
                std::fs::write(&frame, b"not a real png").unwrap();
                frame
            })
            .collect();
        let window = FrameRange {
            first_frame: 3,
            frame_count: 2,
        }
        .window_of(&frames)
        .unwrap();

        let list_path = dir.path().join("frames.ffconcat");
        write_image_concat_list(window, FrameRate::whole(24), &list_path).unwrap();
        let list = std::fs::read_to_string(&list_path).unwrap();
        assert_eq!(list.matches("file '").count(), 2, "{list}");
        assert!(list.contains("frame_003.png"), "{list}");
        assert!(list.contains("frame_004.png"), "{list}");
        for outside in ["frame_000.png", "frame_002.png", "frame_005.png"] {
            assert!(
                !list.contains(outside),
                "{outside} is outside the window\n{list}"
            );
        }
    }

    #[test]
    fn a_fractional_rate_keeps_its_fraction_and_a_whole_one_prints_bare() {
        assert_eq!(FrameRate::whole(24).ffmpeg_filter_value(), "24");
        assert_eq!(
            FrameRate::new(24000, 1001).ffmpeg_filter_value(),
            "24000/1001"
        );
        assert_eq!(FrameRate::default(), FrameRate::whole(24));
        assert_eq!(FrameRate::whole(25).as_f64(), 25.0);
        assert!((FrameRate::new(24000, 1001).as_f64() - 23.976_023_976).abs() < 1e-9);
    }

    #[test]
    #[should_panic(expected = "non-zero numerator and denominator")]
    fn a_zero_rate_is_refused() {
        FrameRate::new(0, 1);
    }

    #[test]
    fn a_concat_list_holds_each_still_for_the_exact_frame_period() {
        let dir = tempfile::tempdir().unwrap();
        let frames: Vec<PathBuf> = (0..2)
            .map(|index| {
                let frame = dir.path().join(format!("frame_{index}.png"));
                std::fs::write(&frame, b"not a real png").unwrap();
                frame
            })
            .collect();
        let list_path = dir.path().join("frames.ffconcat");
        write_image_concat_list(&frames, FrameRate::new(24000, 1001), &list_path).unwrap();
        let list = std::fs::read_to_string(&list_path).unwrap();
        assert_eq!(list.matches("duration 0.041708333").count(), 2, "{list}");

        write_image_concat_list(&frames, FrameRate::whole(24), &list_path).unwrap();
        let list = std::fs::read_to_string(&list_path).unwrap();
        assert_eq!(list.matches("duration 0.041666667").count(), 2, "{list}");
    }

    #[test]
    fn a_source_read_rate_reaches_ffmpeg_as_an_input_rate() {
        assert_eq!(
            decode_input_args(DecodeSource::Video, None).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            decode_input_args(DecodeSource::Video, Some(FrameRate::whole(24))).unwrap(),
            vec!["-r", "24"]
        );
        assert_eq!(
            decode_input_args(DecodeSource::ImageList, None).unwrap(),
            vec!["-f", "concat", "-safe", "0"]
        );
        let refused =
            decode_input_args(DecodeSource::ImageList, Some(FrameRate::whole(24))).unwrap_err();
        assert!(refused.contains("concat list"), "{refused}");
    }

    #[test]
    fn only_display_rgb_gets_the_xyz_transform() {
        assert!(SourceColour::DisplayRgb.applies_xyz_transform());
        assert!(!SourceColour::AlreadyPq.applies_xyz_transform());
        assert!(
            !SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")).applies_xyz_transform()
        );
        assert!(
            !SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3).applies_xyz_transform(),
            "postkit converts a P3 source itself, so the compressor must not"
        );
    }

    #[test]
    fn a_wide_gamut_source_carries_its_own_frame_transform() {
        for space in [
            crate::colour::ColourSpace::P3,
            crate::colour::ColourSpace::Rec2020,
        ] {
            assert!(
                SourceColour::DisplayRgbIn(space)
                    .frame_transform()
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            SourceColour::DisplayRgb
                .frame_transform()
                .unwrap()
                .is_none()
        );
        assert!(SourceColour::AlreadyPq.frame_transform().unwrap().is_none());
        assert!(
            SourceColour::DisplayRgbIn(crate::colour::ColourSpace::Aces)
                .frame_transform()
                .is_err(),
            "a scene-referred source has no matrix"
        );
    }

    #[test]
    fn the_lut_source_decodes_through_lut3d() {
        let plain = crate::picture_processing::PictureProcessing::default()
            .plan(1920, 1080)
            .unwrap();
        assert_eq!(
            decode_filters(
                FrameRate::whole(24),
                &SourceColour::DisplayRgb,
                &plain,
                None
            ),
            "fps=24"
        );
        assert_eq!(
            decode_filters(FrameRate::whole(25), &SourceColour::AlreadyPq, &plain, None),
            "fps=25"
        );
        assert_eq!(
            decode_filters(
                FrameRate::whole(48),
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                &plain,
                None
            ),
            "fps=48,lut3d=/luts/hdr_to_dci.cube"
        );
    }

    #[test]
    fn the_picture_plan_wraps_the_frame_rate_and_keeps_the_lut_last() {
        let plan = crate::picture_processing::PictureProcessing {
            deinterlace: true,
            denoise: true,
            crop: crate::picture_processing::Crop {
                left: 0,
                right: 0,
                top: 138,
                bottom: 138,
            },
            ..crate::picture_processing::PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap();
        assert_eq!(
            decode_filters(
                FrameRate::whole(24),
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                &plan,
                None
            ),
            "yadif,fps=24,hqdn3d,format=gbrp16le,crop=1920:804:0:138,lut3d=/luts/hdr_to_dci.cube"
        );
    }

    #[test]
    fn jpeg_stills_are_an_image_sequence() {
        assert_eq!(
            detect_input_type(Path::new("/frames/shot_0001.jpg")),
            InputType::ImageSequence
        );
        assert_eq!(
            detect_input_type(Path::new("/frames/shot_0001.png")),
            InputType::ImageSequence
        );
        assert_eq!(
            detect_input_type(Path::new("/frames/shot_0001.JPEG")),
            InputType::ImageSequence
        );
        assert_eq!(
            detect_image_format(Path::new("/frames/shot_0001.jpg")),
            ImageFormat::Jpeg
        );
        assert_eq!(
            detect_image_format(Path::new("/frames/shot_0001.Jpeg")),
            ImageFormat::Jpeg
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot_0001.jpeg"), b"not really a jpeg").unwrap();
        assert_eq!(detect_input_type(dir.path()), InputType::ImageSequence);
        assert_eq!(find_source_frames(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn a_frame_over_the_cap_fails() {
        let dir = tempfile::tempdir().unwrap();
        let frame = dir.path().join("frame_00000000.j2c");
        std::fs::write(&frame, vec![0u8; 2048]).unwrap();

        assert!(check_codestream_size(&frame, 2048).is_ok());
        let error = check_codestream_size(&frame, 2047).unwrap_err();
        assert!(error.contains("2048 bytes"), "{error}");
        assert!(check_codestream_size(&dir.path().join("missing.j2c"), 2048).is_err());
    }

    #[test]
    fn the_subprocess_encoder_refuses_a_byte_cap() {
        let result = stream_encode_subprocess(
            &StreamEncodeOptions {
                input: PathBuf::from("/nonexistent.mov"),
                codestream_byte_cap: Some(1_302_083),
                ..StreamEncodeOptions::default()
            },
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert!(!result.success);
        assert!(
            result.error.contains("needs the in-process encoder"),
            "{}",
            result.error
        );
    }
}
