use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
                    "tif" | "tiff" | "dpx" | "exr" | "bmp" => return InputType::ImageSequence,
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
            "tif" | "tiff" | "dpx" | "exr" | "bmp" => InputType::ImageSequence,
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
            .arg(format!("{}", opts.bitrate_mbps));

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

use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Options for streaming encode (video → J2K without intermediate files).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEncodeOptions {
    /// Input video file
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
    pub fps: u32,
    /// Path to compressor binary (auto-detected if empty)
    pub compressor_path: PathBuf,
    /// Library directory for LD_LIBRARY_PATH (if needed)
    pub lib_dir: Option<PathBuf>,
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
            fps: 24,
            compressor_path: PathBuf::new(),
            lib_dir: None,
        }
    }
}

/// Progress callback for streaming encode.
pub struct StreamProgress {
    pub frame: u64,
    pub total_frames: u64,
    pub fps: f64,
    pub elapsed_secs: f64,
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
    let dim_output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
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
        .args([
            "-v",
            "error",
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

/// Stream-encode a video file to J2K without intermediate files.
///
/// Pipes raw 16-bit RGB frames from ffmpeg directly to grk_compress stdin.
/// Calls `on_progress` periodically with current status.
/// Respects `cancel` flag to abort early.
/// Respects `pause` flag to pause between frames.
pub fn stream_encode<F>(
    opts: &StreamEncodeOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    mut on_progress: F,
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    let (width, height, total_frames) = probe_video(&opts.input);
    if width == 0 || height == 0 {
        return EncodeResult {
            success: false,
            error: "Could not determine video dimensions".to_string(),
            ..Default::default()
        };
    }

    let frame_size = (width as usize) * (height as usize) * 3 * 2; // 16-bit RGB

    let compressor = if opts.compressor_path.as_os_str().is_empty() {
        find_compressor().map(|(p, _)| p)
    } else {
        Some(opts.compressor_path.clone())
    };
    let Some(compressor) = compressor else {
        return EncodeResult {
            success: false,
            error: "grk_compress not found".to_string(),
            ..Default::default()
        };
    };

    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return EncodeResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    // Start ffmpeg
    let fps_filter = format!("fps={}", opts.fps);
    let mut ffmpeg = match std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&opts.input)
        .args([
            "-vf",
            &fps_filter,
            "-pix_fmt",
            "rgb48be",
            "-f",
            "rawvideo",
            "-an",
            "pipe:1",
        ])
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

    let mut frame_buf = vec![0u8; frame_size];
    let mut encoded: u64 = 0;
    let encode_start = std::time::Instant::now();
    let raw_fmt = format!("{},{},3,16,u", width, height);

    loop {
        if cancel.load(Ordering::Relaxed) {
            kill_child(&mut ffmpeg);
            return EncodeResult {
                success: false,
                error: "Cancelled".to_string(),
                frames_encoded: encoded,
                output_dir: opts.output_dir.clone(),
            };
        }
        while pause.load(Ordering::Relaxed) {
            if cancel.load(Ordering::Relaxed) {
                kill_child(&mut ffmpeg);
                return EncodeResult {
                    success: false,
                    error: "Cancelled".to_string(),
                    frames_encoded: encoded,
                    output_dir: opts.output_dir.clone(),
                };
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        match read_exact_or_eof(&mut ffmpeg_stdout, &mut frame_buf) {
            ReadResult::Ok => {}
            ReadResult::Eof => break,
            ReadResult::Err(e) => {
                kill_child(&mut ffmpeg);
                return EncodeResult {
                    success: false,
                    error: format!("Read error: {e}"),
                    frames_encoded: encoded,
                    output_dir: opts.output_dir.clone(),
                };
            }
        }

        let output_frame = opts.output_dir.join(format!("frame_{:08}.j2c", encoded));

        let mut cmd = std::process::Command::new(&compressor);
        if let Some(ref ld) = opts.lib_dir {
            cmd.env("LD_LIBRARY_PATH", ld);
        }
        cmd.args(["--in-fmt", "raw"])
            .arg("-F")
            .arg(&raw_fmt)
            .arg("-o")
            .arg(&output_frame)
            .arg("-r")
            .arg(format!("{}", opts.compression_ratio))
            .arg("-n")
            .arg(format!("{}", opts.num_resolutions))
            .arg("-b")
            .arg(format!("{},{}", opts.codeblock_size, opts.codeblock_size))
            .arg("-p")
            .arg(&opts.progression)
            .arg("--xyz")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut grk = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                kill_child(&mut ffmpeg);
                return EncodeResult {
                    success: false,
                    error: format!("Failed to start compressor: {e}"),
                    frames_encoded: encoded,
                    output_dir: opts.output_dir.clone(),
                };
            }
        };

        if let Some(mut stdin) = grk.stdin.take()
            && let Err(e) = stdin.write_all(&frame_buf)
        {
            kill_child(&mut ffmpeg);
            return EncodeResult {
                success: false,
                error: format!("Pipe error frame {encoded}: {e}"),
                frames_encoded: encoded,
                output_dir: opts.output_dir.clone(),
            };
        }

        let status = match grk.wait() {
            Ok(s) => s,
            Err(e) => {
                kill_child(&mut ffmpeg);
                return EncodeResult {
                    success: false,
                    error: format!("Compressor wait error: {e}"),
                    frames_encoded: encoded,
                    output_dir: opts.output_dir.clone(),
                };
            }
        };

        if !status.success() {
            let stderr_out = grk
                .stderr
                .take()
                .map(|mut s| {
                    let mut b = String::new();
                    let _ = s.read_to_string(&mut b);
                    b
                })
                .unwrap_or_default();
            kill_child(&mut ffmpeg);
            return EncodeResult {
                success: false,
                error: format!("Encode failed frame {encoded}: {stderr_out}"),
                frames_encoded: encoded,
                output_dir: opts.output_dir.clone(),
            };
        }

        encoded += 1;

        if encoded.is_multiple_of(5) || encoded == total_frames {
            let elapsed = encode_start.elapsed().as_secs_f64();
            on_progress(StreamProgress {
                frame: encoded,
                total_frames,
                fps: if elapsed > 0.0 {
                    encoded as f64 / elapsed
                } else {
                    0.0
                },
                elapsed_secs: elapsed,
            });
        }
    }

    let _ = ffmpeg.wait();

    EncodeResult {
        success: true,
        error: String::new(),
        frames_encoded: encoded,
        output_dir: opts.output_dir.clone(),
    }
}

enum ReadResult {
    Ok,
    Eof,
    Err(std::io::Error),
}

fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> ReadResult {
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
    mut on_progress: F,
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    use crate::grok_encoder::{self, CompressParams, RawFrame};

    let (width, height, total_frames) = probe_video(&opts.input);
    if width == 0 || height == 0 {
        return EncodeResult {
            success: false,
            error: "Could not determine video dimensions".to_string(),
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

    let frame_size = (width as usize) * (height as usize) * 3 * 2; // 16-bit RGB

    // Start ffmpeg: decode to raw 16-bit big-endian RGB
    let fps_filter = format!("fps={}", opts.fps);
    let mut ffmpeg = match std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&opts.input)
        .args([
            "-vf",
            &fps_filter,
            "-pix_fmt",
            "rgb48be",
            "-f",
            "rawvideo",
            "-an",
            "pipe:1",
        ])
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

    let params = CompressParams {
        compression_ratio: opts.compression_ratio,
        num_resolutions: opts.num_resolutions as u8,
        codeblock_size: opts.codeblock_size,
        frame_rate: opts.fps as u16,
        apply_xyz_transform: true,
        ..CompressParams::default()
    };

    grok_encoder::initialize(0);

    let mut frame_buf = vec![0u8; frame_size];
    let mut frame_index: u64 = 0;
    let encode_start = std::time::Instant::now();

    let result = grok_encoder::encode_pipeline(
        &opts.output_dir,
        &params,
        total_frames,
        cancel,
        || {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            match read_exact_or_eof(&mut ffmpeg_stdout, &mut frame_buf) {
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
            });
        },
    );

    kill_child(&mut ffmpeg);
    grok_encoder::deinitialize();

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

    let (width, height, total_frames) = probe_video(&opts.input);
    if width == 0 || height == 0 {
        return EncodeResult {
            success: false,
            error: "Could not determine video dimensions".to_string(),
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

    let frame_size = (width as usize) * (height as usize) * 3 * 2;

    // Start ffmpeg
    let fps_filter = format!("fps={}", opts.fps);
    let mut ffmpeg = match std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&opts.input)
        .args([
            "-vf",
            &fps_filter,
            "-pix_fmt",
            "rgb48be",
            "-f",
            "rawvideo",
            "-an",
            "pipe:1",
        ])
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
        frame_rate: opts.fps as u16,
        apply_xyz_transform: true,
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

/// Encode an image sequence using parallel single-threaded subprocesses.
///
/// Spawns up to `parallelism` grk_compress processes concurrently, each
/// processing one frame with `-H 1` (single thread). Returns when all
/// frames are encoded or an error occurs.
pub fn encode_parallel<F>(
    input_dir: &Path,
    output_dir: &Path,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    mut on_progress: F,
) -> EncodeResult
where
    F: FnMut(ParallelProgress),
{
    let frames = match find_source_frames(input_dir) {
        Ok(f) => f,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: format!("Failed to read input dir: {e}"),
                ..Default::default()
            };
        }
    };

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

    let frame_paths: Vec<_> = frames.iter().map(|f| f.to_path_buf()).collect();
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
                let frame_paths = &frame_paths;

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
                        if idx >= frame_paths.len() {
                            break;
                        }

                        let frame = &frame_paths[idx];
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
                                "10",
                                "-n",
                                "6",
                                "-b",
                                "32,32",
                                "-p",
                                "CPRL",
                                "-H",
                                "1",
                            ])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();

                        match result {
                            Ok(status) if status.success() => {
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
