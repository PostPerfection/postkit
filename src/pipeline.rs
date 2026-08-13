//! GUI pipeline encode orchestration.
//!
//! Provides the shared encode workflow that both dcpwizard and imfwizard use.
//! The caller provides progress/log callbacks and handles packaging themselves.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::encode::{
    InputType, ParallelProgress, SourceColour, StreamEncodeOptions, StreamProgress,
    check_codestream_size, encode_parallel, stream_encode_inprocess,
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
    fps: u32,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<EncodeResult, String> {
    run_encode_with_ratio(
        video,
        output_dir,
        10.0,
        fps,
        cancel,
        pause,
        on_progress,
        on_log,
    )
}

/// Same as `run_encode` but with a caller-chosen J2K compression ratio. Only the
/// video branch honours it; image/J2K sequences ignore it. Callers that expose a
/// target bitrate convert it to a ratio first. `fps` sets the J2K edit rate
/// (0 falls back to 24).
#[allow(clippy::too_many_arguments)]
pub fn run_encode_with_ratio(
    video: &Path,
    output_dir: &Path,
    compression_ratio: f64,
    fps: u32,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<EncodeResult, String> {
    run_encode_with_options(
        video,
        output_dir,
        &EncodeRunOptions {
            compression_ratio,
            fps,
            ..EncodeRunOptions::default()
        },
        cancel,
        pause,
        on_progress,
        on_log,
    )
}

/// Encode settings for one pipeline run.
pub struct EncodeRunOptions {
    /// J2K compression ratio (video input only).
    pub compression_ratio: f64,
    /// J2K edit rate (0 falls back to 24).
    pub fps: u32,
    /// Colour the source frames carry, which decides whether the encoder runs
    /// the DCDM X'Y'Z' transform or leaves DCI PQ essence alone.
    pub source_colour: SourceColour,
    /// Per-codestream byte cap, e.g. the DCI HDR Addendum's raised cap. A frame
    /// over it fails the run.
    pub codestream_byte_cap: Option<u64>,
}

impl Default for EncodeRunOptions {
    fn default() -> Self {
        Self {
            compression_ratio: 10.0,
            fps: 24,
            source_colour: SourceColour::DisplayRgb,
            codestream_byte_cap: None,
        }
    }
}

/// Run the encode pipeline with the full option set, including the HDR source
/// colour path and a per-codestream byte cap.
pub fn run_encode_with_options(
    video: &Path,
    output_dir: &Path,
    options: &EncodeRunOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<EncodeResult, String> {
    let compression_ratio = options.compression_ratio;
    let fps = if options.fps == 0 { 24 } else { options.fps };
    if !video.exists() {
        return Err(format!("Input not found: {}", video.display()));
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create output directory: {e}"))?;

    let start_time = std::time::Instant::now();
    let input_type = crate::encode::detect_input_type(video);
    on_log(&format!("Input type: {:?}", input_type));
    reject_unsupported_colour_path(input_type, &options.source_colour)?;

    let j2k_dir = output_dir.join("j2k");
    let mut frames_encoded = 0u64;

    match input_type {
        InputType::Video => {
            let opts = StreamEncodeOptions {
                input: video.to_path_buf(),
                output_dir: j2k_dir.clone(),
                compression_ratio,
                num_resolutions: 6,
                codeblock_size: 32,
                progression: "CPRL".to_string(),
                fps,
                source_colour: options.source_colour.clone(),
                ..StreamEncodeOptions::default()
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

            let result = stream_encode_inprocess(&opts, cancel, pause, |p: StreamProgress| {
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

    if let Some(cap) = options.codestream_byte_cap {
        check_codestream_dir(&final_j2k_dir, cap)?;
    }

    let elapsed_secs = start_time.elapsed().as_secs_f64();

    Ok(EncodeResult {
        j2k_dir: final_j2k_dir,
        frames_encoded,
        elapsed_secs,
    })
}

/// Refuse a source colour the chosen input branch cannot honour: the image
/// sequence encoder always applies the DCDM X'Y'Z' transform, and a LUT cannot
/// be run over frames that are already compressed.
fn reject_unsupported_colour_path(
    input_type: InputType,
    source_colour: &SourceColour,
) -> Result<(), String> {
    match (input_type, source_colour) {
        (InputType::ImageSequence, colour) if !colour.applies_xyz_transform() => Err(format!(
            "image sequences are always encoded through the DCDM X'Y'Z' transform, so {colour:?} would be mislabelled"
        )),
        (InputType::J2kSequence, SourceColour::DciLut(lut)) => Err(format!(
            "J2K input is already compressed, so the HDR-to-DCI LUT {} cannot be applied",
            lut.display()
        )),
        _ => Ok(()),
    }
}

/// Hold every codestream in a directory under the per-frame byte cap.
fn check_codestream_dir(dir: &Path, cap: u64) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            check_codestream_size(&path, cap)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_sequences_refuse_an_untransformed_source() {
        assert!(
            reject_unsupported_colour_path(InputType::ImageSequence, &SourceColour::DisplayRgb)
                .is_ok()
        );
        assert!(
            reject_unsupported_colour_path(InputType::ImageSequence, &SourceColour::AlreadyPq)
                .is_err()
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::ImageSequence,
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
            )
            .is_err()
        );
    }

    #[test]
    fn compressed_input_refuses_a_lut_but_takes_pq_frames() {
        assert!(
            reject_unsupported_colour_path(
                InputType::J2kSequence,
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
            )
            .is_err()
        );
        assert!(
            reject_unsupported_colour_path(InputType::J2kSequence, &SourceColour::AlreadyPq)
                .is_ok()
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::Video,
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
            )
            .is_ok()
        );
    }

    #[test]
    fn a_pre_encoded_frame_over_the_cap_fails_the_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("frame_00000000.j2c"), vec![0u8; 512]).unwrap();
        std::fs::write(dir.path().join("frame_00000001.j2c"), vec![0u8; 4096]).unwrap();

        assert!(check_codestream_dir(dir.path(), 4096).is_ok());
        let error = check_codestream_dir(dir.path(), 1024).unwrap_err();
        assert!(error.contains("frame_00000001.j2c"), "{error}");
    }
}
