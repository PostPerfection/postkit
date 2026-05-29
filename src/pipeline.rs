//! GUI pipeline encode orchestration.
//!
//! Provides the shared encode workflow that both dcpwizard and imfwizard use.
//! The caller provides progress/log callbacks and handles packaging themselves.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::encode::{
    InputType, ParallelProgress, StreamEncodeOptions, StreamProgress, encode_parallel,
    find_compressor, stream_encode,
};

/// Progress information emitted during encode.
#[derive(Clone, Debug)]
pub struct PipelineProgress {
    pub stage: String,
    pub message: String,
    pub frame: u64,
    pub total_frames: u64,
    pub fps: f64,
    pub elapsed_secs: f64,
    pub percent: f64,
}

/// Result of a successful encode run.
pub struct EncodeResult {
    /// Directory containing the J2K frames (or the original J2K input dir).
    pub j2k_dir: PathBuf,
    /// Number of frames encoded (0 if input was already J2K).
    pub frames_encoded: u64,
    /// Total elapsed time in seconds.
    pub elapsed_secs: f64,
}

/// Run the encode pipeline: detect input type, encode to J2K if needed.
///
/// - `video`: Input file or directory (video, image sequence, or J2K sequence)
/// - `output_dir`: Output directory (j2k subdir will be created here)
/// - `cancel`/`pause`: Shared atomics for job control
/// - `on_progress`: Called with progress updates (stage, frame counts, etc.)
/// - `on_log`: Called with log messages
///
/// Returns the path to the J2K directory and frame count on success.
pub fn run_encode(
    video: &Path,
    output_dir: &Path,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<EncodeResult, String> {
    if !video.exists() {
        return Err(format!("Input not found: {}", video.display()));
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create output directory: {e}"))?;

    let start_time = std::time::Instant::now();
    let input_type = crate::encode::detect_input_type(video);
    on_log(&format!("Input type: {:?}", input_type));

    let j2k_dir = output_dir.join("j2k");
    let mut frames_encoded = 0u64;

    match input_type {
        InputType::Video => {
            let (compressor_path, lib_dir) = find_compressor().ok_or("grk_compress not found")?;

            let opts = StreamEncodeOptions {
                input: video.to_path_buf(),
                output_dir: j2k_dir.clone(),
                compression_ratio: 10.0,
                num_resolutions: 6,
                codeblock_size: 32,
                progression: "CPRL".to_string(),
                fps: 24,
                compressor_path,
                lib_dir,
            };

            on_progress(&PipelineProgress {
                stage: "encode".to_string(),
                message: "Starting...".to_string(),
                frame: 0,
                total_frames: 0,
                fps: 0.0,
                elapsed_secs: 0.0,
                percent: 0.0,
            });

            let result = stream_encode(&opts, cancel, pause, |p: StreamProgress| {
                let percent = if p.total_frames > 0 {
                    (p.frame as f64 / p.total_frames as f64) * 100.0
                } else {
                    0.0
                };
                on_progress(&PipelineProgress {
                    stage: "encode".to_string(),
                    message: format!("Frame {}/{}", p.frame, p.total_frames),
                    frame: p.frame,
                    total_frames: p.total_frames,
                    fps: p.fps,
                    elapsed_secs: p.elapsed_secs,
                    percent: percent.min(99.0),
                });
                on_log(&format!(
                    "[ENCODE] frame={}/{} fps={:.1}",
                    p.frame, p.total_frames, p.fps
                ));
            });

            if !result.success {
                return Err(result.error);
            }
            frames_encoded = result.frames_encoded;
            on_log(&format!("[ENCODE] Done: {} frames", frames_encoded));
        }
        InputType::ImageSequence => {
            let input_dir = if video.is_dir() {
                video.to_path_buf()
            } else {
                video.parent().unwrap_or(video).to_path_buf()
            };

            on_progress(&PipelineProgress {
                stage: "encode".to_string(),
                message: "Encoding images...".to_string(),
                frame: 0,
                total_frames: 0,
                fps: 0.0,
                elapsed_secs: 0.0,
                percent: 0.0,
            });

            let result = encode_parallel(
                &input_dir,
                &j2k_dir,
                cancel,
                pause,
                |p: ParallelProgress| {
                    let percent = if p.total > 0 {
                        (p.done as f64 / p.total as f64) * 100.0
                    } else {
                        0.0
                    };
                    on_progress(&PipelineProgress {
                        stage: "encode".to_string(),
                        message: format!("Frame {}/{}", p.done, p.total),
                        frame: p.done,
                        total_frames: p.total,
                        fps: p.fps,
                        elapsed_secs: p.elapsed_secs,
                        percent: percent.min(99.0),
                    });
                },
            );

            if !result.success {
                return Err(result.error);
            }
            frames_encoded = result.frames_encoded;
            on_log(&format!("[ENCODE] Done: {} frames", frames_encoded));
        }
        InputType::J2kSequence => {
            on_log("Input is already J2K, skipping encode");
        }
        InputType::Unknown => {
            return Err(format!("Cannot determine input type: {}", video.display()));
        }
    }

    if cancel.load(Ordering::Relaxed) {
        on_log("=== CANCELLED ===");
        return Err("Cancelled".to_string());
    }

    let final_j2k_dir = match input_type {
        InputType::J2kSequence => video.to_path_buf(),
        _ => j2k_dir,
    };

    let elapsed_secs = start_time.elapsed().as_secs_f64();

    Ok(EncodeResult {
        j2k_dir: final_j2k_dir,
        frames_encoded,
        elapsed_secs,
    })
}
