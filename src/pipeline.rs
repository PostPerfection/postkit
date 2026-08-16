//! GUI pipeline encode orchestration.
//!
//! Provides the shared encode workflow that both dcpwizard and imfwizard use.
//! The caller provides progress/log callbacks and handles packaging themselves.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::encode::{
    ImageFormat, InputType, ParallelProgress, SourceColour, StreamEncodeOptions, StreamProgress,
    check_codestream_size, encode_parallel, stream_encode_inprocess,
};
use crate::picture_processing::PictureProcessing;

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
    /// Subtitles burnt into every decoded frame. Every burn decodes through
    /// ffmpeg, image sequences included: [`reject_unsupported_burn`] names the
    /// inputs that cannot.
    pub subtitle_burn: Option<Arc<crate::subtitle_raster::SubtitleBurn>>,
    /// Crop, deinterlace, rotate, flip, denoise and raster fit applied while the
    /// source decodes. Anything but the identity takes an image sequence through
    /// ffmpeg, the way a burn does.
    pub picture: PictureProcessing,
}

impl Default for EncodeRunOptions {
    fn default() -> Self {
        Self {
            compression_ratio: 10.0,
            fps: 24,
            source_colour: SourceColour::DisplayRgb,
            codestream_byte_cap: None,
            subtitle_burn: None,
            picture: PictureProcessing::default(),
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
    let sequence_frame_format = match input_type {
        InputType::ImageSequence => Some(first_frame_format(video)?),
        _ => None,
    };
    // an image sequence only reaches ffmpeg when something has to happen to each
    // frame, and the branch it takes decides which colour paths are open. jpeg
    // and png frames always go that way, because grk_compress reads them only if
    // grok was built with those loaders and ffmpeg always does
    let sequence_needs_ffmpeg = options.subtitle_burn.is_some()
        || !options.picture.is_identity()
        || matches!(
            sequence_frame_format,
            Some(ImageFormat::Jpeg | ImageFormat::Png)
        );
    let decodes_through_ffmpeg = match input_type {
        InputType::Video => true,
        InputType::ImageSequence => sequence_needs_ffmpeg,
        InputType::J2kSequence | InputType::Unknown => false,
    };
    reject_unsupported_colour_path(input_type, &options.source_colour, decodes_through_ffmpeg)?;
    if options.subtitle_burn.is_some() {
        reject_unsupported_burn(input_type, &options.source_colour)?;
    }
    if !options.picture.is_identity() {
        reject_unsupported_picture(input_type)?;
    }

    let j2k_dir = output_dir.join("j2k");
    let mut frames_encoded = 0u64;

    // Everything ffmpeg decodes goes through one code path, so a subtitle burn
    // has a single place to hook into. `run_stream` is that path; the arms below
    // only decide what ffmpeg opens.
    let run_stream = |opts: &StreamEncodeOptions| -> Result<u64, String> {
        on_progress(&PipelineProgress {
            stage: "encode".to_string(),
            message: "Starting...".to_string(),
            frame: 0,
            total_frames: 0,
            fps: 0.0,
            elapsed_secs: 0.0,
            percent: 0.0,
        });
        let result = stream_encode_inprocess(opts, cancel, pause, |p: StreamProgress| {
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
        Ok(result.frames_encoded)
    };

    match input_type {
        InputType::Video => {
            frames_encoded = run_stream(&StreamEncodeOptions {
                input: video.to_path_buf(),
                output_dir: j2k_dir.clone(),
                compression_ratio,
                num_resolutions: 6,
                codeblock_size: 32,
                progression: "CPRL".to_string(),
                fps,
                source_colour: options.source_colour.clone(),
                subtitle_burn: options.subtitle_burn.clone(),
                picture: options.picture.clone(),
                ..StreamEncodeOptions::default()
            })?;
            on_log(&format!("[ENCODE] Done: {} frames", frames_encoded));
        }
        InputType::ImageSequence => {
            let input_dir = sequence_directory(video);

            if sequence_needs_ffmpeg {
                // grk_compress reads the stills itself and never shows postkit a
                // frame buffer, so a burn or a picture change takes the sequence
                // through ffmpeg instead: a concat list holding each still for
                // one frame period decodes to the same rgb48be stream a video
                // does.
                let frames = crate::encode::find_source_frames(&input_dir)
                    .map_err(|e| format!("cannot list {}: {e}", input_dir.display()))?;
                if frames.is_empty() {
                    return Err(format!("no images in {}", input_dir.display()));
                }
                let list = output_dir.join("frames.ffconcat");
                crate::encode::write_image_concat_list(&frames, fps, &list)?;
                on_log(&format!(
                    "[ENCODE] Taking {} images through ffmpeg",
                    frames.len()
                ));
                frames_encoded = run_stream(&StreamEncodeOptions {
                    input: list,
                    output_dir: j2k_dir.clone(),
                    compression_ratio,
                    num_resolutions: 6,
                    codeblock_size: 32,
                    progression: "CPRL".to_string(),
                    fps,
                    source_colour: options.source_colour.clone(),
                    subtitle_burn: options.subtitle_burn.clone(),
                    picture: options.picture.clone(),
                    decode_source: crate::encode::DecodeSource::ImageList,
                    ..StreamEncodeOptions::default()
                })?;
                on_log(&format!("[ENCODE] Done: {} frames", frames_encoded));
            } else {
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

/// The directory holding an image sequence, whether the input names the
/// directory itself or one frame in it.
fn sequence_directory(input: &Path) -> PathBuf {
    if input.is_dir() {
        input.to_path_buf()
    } else {
        input.parent().unwrap_or(input).to_path_buf()
    }
}

/// Format of the first frame of an image sequence, which decides whether the
/// frames can be handed to grk_compress as they are.
fn first_frame_format(input: &Path) -> Result<ImageFormat, String> {
    let directory = sequence_directory(input);
    let frames = crate::encode::find_source_frames(&directory)
        .map_err(|e| format!("cannot list {}: {e}", directory.display()))?;
    let first = frames
        .first()
        .ok_or_else(|| format!("no images in {}", directory.display()))?;
    Ok(crate::encode::detect_image_format(first))
}

/// Refuse a source colour the chosen input branch cannot honour: the image
/// sequence encoder hands each file to grk_compress, which only converts
/// Rec.709, and nothing can be converted once the frames are compressed.
///
/// `decodes_through_ffmpeg` is what the sequence limits hang on rather than the
/// input type: a sequence that a burn or a picture change routes through ffmpeg
/// reaches the same per-frame hooks a video does, so those limits fall away.
fn reject_unsupported_colour_path(
    input_type: InputType,
    source_colour: &SourceColour,
    decodes_through_ffmpeg: bool,
) -> Result<(), String> {
    match (input_type, source_colour) {
        (InputType::ImageSequence, SourceColour::DisplayRgbIn(space))
            if !decodes_through_ffmpeg =>
        {
            Err(format!(
                "image sequences are compressed straight from file by grk_compress, which only \
                 converts Rec.709: convert a {space:?} sequence to X'Y'Z' first, or encode from \
                 a video"
            ))
        }
        (InputType::ImageSequence, colour)
            if !decodes_through_ffmpeg && !colour.applies_xyz_transform() =>
        {
            Err(format!(
                "image sequences are always encoded through the DCDM X'Y'Z' transform, so {colour:?} would be mislabelled"
            ))
        }
        (InputType::J2kSequence, SourceColour::DciLut(lut)) => Err(format!(
            "J2K input is already compressed, so the HDR-to-DCI LUT {} cannot be applied",
            lut.display()
        )),
        (InputType::J2kSequence, SourceColour::DisplayRgbIn(space)) => Err(format!(
            "J2K input is already compressed, so a {space:?} source cannot be converted to \
             X'Y'Z' any more"
        )),
        _ => Ok(()),
    }
}

/// Refuse a burn the encode cannot honour: subtitle bitmaps are display RGB, so
/// they only make sense on frames that have not been converted yet, and only
/// the video path decodes frames postkit can composite onto at all.
fn reject_unsupported_burn(
    input_type: InputType,
    source_colour: &SourceColour,
) -> Result<(), String> {
    match (input_type, source_colour) {
        (InputType::J2kSequence, _) => Err(
            "J2K input is already compressed, so there are no frames to burn subtitles onto"
                .to_string(),
        ),
        (_, SourceColour::DciLut(lut)) => Err(format!(
            "the HDR-to-DCI LUT {} converts frames to X'Y'Z' during decode, so burnt-in text \
             would be drawn in the wrong colour space",
            lut.display()
        )),
        (_, SourceColour::AlreadyPq) => Err(
            "an X'Y'Z' PQ source is already in the projector's colour space, so burnt-in text \
             would be drawn in the wrong one"
                .to_string(),
        ),
        _ => Ok(()),
    }
}

/// Refuse picture processing the encode cannot honour: cropping, scaling and
/// the rest happen while ffmpeg decodes, so an input that never decodes has no
/// picture to process.
fn reject_unsupported_picture(input_type: InputType) -> Result<(), String> {
    match input_type {
        InputType::J2kSequence => Err(
            "J2K input is already compressed, so there are no frames to crop, rotate or fit"
                .to_string(),
        ),
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
    fn a_burn_is_refused_wherever_the_frames_are_not_display_rgb() {
        assert!(reject_unsupported_burn(InputType::Video, &SourceColour::DisplayRgb).is_ok());
        assert!(
            reject_unsupported_burn(
                InputType::Video,
                &SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3),
            )
            .is_ok(),
            "P3 burns in display RGB and is converted afterwards"
        );
        assert!(
            reject_unsupported_burn(InputType::ImageSequence, &SourceColour::DisplayRgb).is_ok(),
            "an image sequence burns through the concat demuxer"
        );
        for (input, colour, expected) in [
            (
                InputType::J2kSequence,
                SourceColour::DisplayRgb,
                "already compressed",
            ),
            (
                InputType::Video,
                SourceColour::DciLut(PathBuf::from("hdr.cube")),
                "wrong colour space",
            ),
            (InputType::Video, SourceColour::AlreadyPq, "wrong one"),
        ] {
            let err = reject_unsupported_burn(input, &colour)
                .expect_err("this combination has to refuse a burn");
            assert!(err.contains(expected), "got: {err}");
        }
    }

    /// The branch an input takes, as `reject_unsupported_colour_path` sees it.
    const THROUGH_FFMPEG: bool = true;
    const STRAIGHT_FROM_FILE: bool = false;

    #[test]
    fn image_sequences_refuse_an_untransformed_source() {
        assert!(
            reject_unsupported_colour_path(
                InputType::ImageSequence,
                &SourceColour::DisplayRgb,
                STRAIGHT_FROM_FILE
            )
            .is_ok()
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::ImageSequence,
                &SourceColour::AlreadyPq,
                STRAIGHT_FROM_FILE
            )
            .is_err()
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::ImageSequence,
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                STRAIGHT_FROM_FILE
            )
            .is_err()
        );
    }

    #[test]
    fn a_streamed_image_sequence_takes_the_colours_a_video_does() {
        let p3 = SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3);
        assert!(
            reject_unsupported_colour_path(InputType::ImageSequence, &p3, THROUGH_FFMPEG).is_ok(),
            "a burn or a picture change decodes the sequence through ffmpeg, where the frame \
             transform runs"
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::ImageSequence,
                &SourceColour::AlreadyPq,
                THROUGH_FFMPEG
            )
            .is_ok()
        );
    }

    #[test]
    fn a_wide_gamut_source_encodes_from_video_only() {
        let p3 = SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3);
        assert!(reject_unsupported_colour_path(InputType::Video, &p3, THROUGH_FFMPEG).is_ok());
        let images =
            reject_unsupported_colour_path(InputType::ImageSequence, &p3, STRAIGHT_FROM_FILE)
                .unwrap_err();
        assert!(images.contains("P3"), "{images}");
        let compressed =
            reject_unsupported_colour_path(InputType::J2kSequence, &p3, STRAIGHT_FROM_FILE)
                .unwrap_err();
        assert!(compressed.contains("already compressed"), "{compressed}");
    }

    #[test]
    fn compressed_input_refuses_a_lut_but_takes_pq_frames() {
        assert!(
            reject_unsupported_colour_path(
                InputType::J2kSequence,
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                STRAIGHT_FROM_FILE
            )
            .is_err()
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::J2kSequence,
                &SourceColour::AlreadyPq,
                STRAIGHT_FROM_FILE
            )
            .is_ok()
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::Video,
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                THROUGH_FFMPEG
            )
            .is_ok()
        );
    }

    #[test]
    fn compressed_input_refuses_picture_processing() {
        assert!(reject_unsupported_picture(InputType::Video).is_ok());
        assert!(reject_unsupported_picture(InputType::ImageSequence).is_ok());
        let compressed = reject_unsupported_picture(InputType::J2kSequence).unwrap_err();
        assert!(compressed.contains("no frames to crop"), "{compressed}");
    }

    #[test]
    fn the_routing_reads_the_format_of_the_first_frame() {
        let dir = tempfile::tempdir().unwrap();
        let frame = dir.path().join("shot_0001.jpg");
        std::fs::write(&frame, b"not really a jpeg").unwrap();

        assert_eq!(first_frame_format(dir.path()).unwrap(), ImageFormat::Jpeg);
        assert_eq!(
            first_frame_format(&frame).unwrap(),
            ImageFormat::Jpeg,
            "naming one frame has to find the sequence it belongs to"
        );

        std::fs::write(dir.path().join("shot_0000.tiff"), b"not really a tiff").unwrap();
        assert_eq!(
            first_frame_format(dir.path()).unwrap(),
            ImageFormat::Tiff,
            "the first frame by name decides"
        );

        let empty = tempfile::tempdir().unwrap();
        assert!(first_frame_format(empty.path()).is_err());
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
