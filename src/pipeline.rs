//! GUI pipeline encode orchestration.
//!
//! Provides the shared encode workflow that both dcpwizard and imfwizard use.
//! The caller provides progress/log callbacks and handles packaging themselves.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::composite::{Composite, CompositeBase, CompositeLayer, CompositeOverlay, LayerFrames};
pub use crate::encode::FrameRange;
use crate::encode::{
    DecodeSource, FrameRate, ImageFormat, InputType, SourceColour, StreamEncodeOptions,
    StreamProgress, check_codestream_size, encode_tiff_sequence_inprocess,
    stream_encode_inprocess_with_mxf_feed,
};
use crate::mxf_unwrap::CANCELLED;
use crate::picture_processing::PictureProcessing;
use crate::timeline::{Clip, Timeline};

/// Progress information emitted during encode.
///
/// The four phase clocks carry [`StreamProgress`]'s breakdown of the time
/// inside the encode. They are zero for a stage that measures nothing, such as
/// a J2K sequence that is never encoded.
#[derive(Clone, Debug)]
pub struct PipelineProgress {
    pub stage: String,
    pub message: String,
    pub frame: u64,
    pub total_frames: u64,
    pub fps: f64,
    pub elapsed_secs: f64,
    pub percent: f64,
    /// Time the frame reader spent blocked on ffmpeg's pipe.
    pub decode_wait_secs: f64,
    /// Time spent burning subtitles and converting colour, summed over the
    /// encoder threads, so it can exceed `elapsed_secs`.
    pub prepare_secs: f64,
    /// Time spent compressing, summed over the encoder threads, so it can
    /// exceed `elapsed_secs`.
    pub encode_secs: f64,
    /// Time spent writing codestreams to disk.
    pub write_secs: f64,
}

impl PipelineProgress {
    /// One line naming where the encode time went, in the `4m10s` shape the
    /// wizards print their timings in.
    pub fn phase_breakdown(&self) -> String {
        format!(
            "decoder wait {}, frame prep {}, j2k {}, write {}",
            format_minutes_seconds(self.decode_wait_secs),
            format_minutes_seconds(self.prepare_secs),
            format_minutes_seconds(self.encode_secs),
            format_minutes_seconds(self.write_secs),
        )
    }
}

const SECONDS_PER_MINUTE: u64 = 60;

fn format_minutes_seconds(seconds: f64) -> String {
    let whole_seconds = seconds.max(0.0).round() as u64;
    let minutes = whole_seconds / SECONDS_PER_MINUTE;
    let remainder = whole_seconds % SECONDS_PER_MINUTE;
    if minutes == 0 {
        format!("{remainder}s")
    } else {
        format!("{minutes}m{remainder}s")
    }
}

/// Result of a successful encode run.
pub struct EncodeResult {
    /// Directory containing the J2K frames (or the original J2K input dir).
    pub j2k_dir: PathBuf,
    /// Number of frames encoded (0 if input was already J2K).
    pub frames_encoded: u64,
    /// Total elapsed time in seconds.
    pub elapsed_secs: f64,
    /// Black and frozen runs seen while the source decoded. Empty for a J2K
    /// sequence and for a TIFF sequence postkit read itself, since neither
    /// goes through ffmpeg.
    pub picture_findings: crate::picture_findings::PictureFindings,
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
    fps: FrameRate,
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
/// target bitrate convert it to a ratio first. `fps` sets the J2K edit rate.
#[allow(clippy::too_many_arguments)]
pub fn run_encode_with_ratio(
    video: &Path,
    output_dir: &Path,
    compression_ratio: f64,
    fps: FrameRate,
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
#[derive(Clone)]
pub struct EncodeRunOptions {
    /// J2K compression ratio (video input only), ignored under `target_codestream_bytes`.
    pub compression_ratio: f64,
    pub target_codestream_bytes: Option<u64>,
    /// A PSNR target in dB that grok allocates layers by instead of the
    /// compression ratio. `codestream_byte_cap` still holds: a frame the target
    /// cannot fit under the cap is compressed again by rate.
    pub quality_psnr: Option<f64>,
    /// J2K edit rate.
    pub fps: FrameRate,
    /// Read the source as if it ran at this rate, ignoring its own timestamps.
    /// This is how a 23.976 source becomes 24 fps by playing 0.1% faster: every
    /// frame reaches the encoder once, none is duplicated or dropped. Video
    /// only, and the sound needs the matching pull-up.
    pub read_source_at: Option<FrameRate>,
    /// Encode only this window of the source rather than all of it, so a caller
    /// keeping five minutes of a two hour source never compresses the rest.
    /// A J2K sequence is refused: it is not encoded here at all, so the caller
    /// links the codestreams it wants.
    pub frame_range: Option<FrameRange>,
    /// Colour the source frames carry, which decides whether the encoder runs
    /// the DCDM X'Y'Z' transform or leaves DCI PQ essence alone.
    pub source_colour: SourceColour,
    /// The Rsiz the codestreams declare: cinema 2K, cinema 4K, or an IMF profile
    /// with its levels from [`crate::j2k::imf_rsiz`].
    pub rsiz: u16,
    /// Per-codestream byte cap, e.g. the DCI HDR Addendum's raised cap. A frame
    /// over it fails the run. Anything postkit compresses itself is checked as
    /// each codestream is written, so the run stops on the first frame over the
    /// cap instead of encoding the rest first.
    pub codestream_byte_cap: Option<u64>,
    /// Subtitles burnt into every decoded frame. Every burn decodes through
    /// ffmpeg, image sequences included: [`reject_unsupported_burn`] names the
    /// inputs that cannot.
    pub subtitle_burn: Option<Arc<crate::subtitle_raster::SubtitleBurn>>,
    /// Crop, deinterlace, rotate, flip, denoise and raster fit applied while the
    /// source decodes. Anything but the identity takes an image sequence through
    /// ffmpeg, the way a burn does.
    pub picture: PictureProcessing,
    /// ffmpeg filters run after `picture` and the frame window, e.g. a fade.
    /// Like any processing, it takes an image sequence through ffmpeg.
    pub extra_picture_filter: Option<String>,
    /// Encoder threads the pipeline runs, 0 for one per available CPU
    pub encode_threads: u32,
    pub detect_picture_findings: bool,
}

impl Default for EncodeRunOptions {
    fn default() -> Self {
        Self {
            compression_ratio: 10.0,
            target_codestream_bytes: None,
            quality_psnr: None,
            fps: FrameRate::default(),
            read_source_at: None,
            frame_range: None,
            source_colour: SourceColour::DisplayRgb,
            rsiz: crate::encode::default_rsiz(),
            codestream_byte_cap: None,
            subtitle_burn: None,
            picture: PictureProcessing::default(),
            extra_picture_filter: None,
            encode_threads: 0,
            detect_picture_findings: false,
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
    let (encode, _) = run_encode_and_maybe_wrap(
        video,
        output_dir,
        options,
        None,
        FIRST_FRAME,
        cancel,
        pause,
        on_progress,
        on_log,
    )?;
    Ok(encode)
}

/// Encode one picture track and wrap it into an MXF at the same time.
///
/// The wrap runs on its own thread and takes each codestream from the encoder as
/// it is written, so it neither waits for the encode to finish nor reads the whole
/// J2K directory back into memory. The J2K directory is still written and still
/// left behind. A cancel or a failure of either side deletes the part-written MXF
/// and leaves the codestreams that finished, as an encode on its own does.
///
/// A J2K sequence is never encoded here, so it hands postkit no codestream to
/// feed the wrap: it is refused here and wraps with [`crate::mxf_wrap::mxf_wrap`]
/// instead. This is the 2D picture path only: a stereoscopic wrap interleaves
/// two eyes per frame and stays with [`crate::mxf_wrap::wrap_stereoscopic`],
/// and sound and Atmos never come through here at all.
#[allow(clippy::too_many_arguments)]
pub fn run_encode_and_wrap_picture(
    video: &Path,
    output_dir: &Path,
    options: &EncodeRunOptions,
    wrap: crate::mxf_wrap::IncrementalWrapOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<(EncodeResult, crate::mxf_wrap::MxfTrackFile), String> {
    let (encode, track) = run_encode_and_maybe_wrap(
        video,
        output_dir,
        options,
        Some(wrap),
        FIRST_FRAME,
        cancel,
        pause,
        on_progress,
        on_log,
    )?;
    Ok((encode, track.expect("a wrap was asked for")))
}

pub fn run_encode_timeline(
    timeline: &Timeline,
    output_dir: &Path,
    options: &EncodeRunOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<EncodeResult, String> {
    refuse_timeline_options(options)?;
    let stretches = timeline_stretches(timeline, options)?;
    let start_time = std::time::Instant::now();
    let total_frames = timeline.total_frames();
    let j2k_dir = output_dir.join("j2k");
    let mut picture_findings = crate::picture_findings::PictureFindings::default();
    let mut finished_phases = PhaseSeconds::default();
    for stretch in &stretches {
        let first_output_frame = stretch.frames.start;
        let frame_count = stretch.frames.end - stretch.frames.start;
        if cancel.load(Ordering::Relaxed) {
            on_log("=== CANCELLED ===");
            return Err(CANCELLED.to_string());
        }
        let stretch_phases = std::cell::Cell::new(PhaseSeconds::default());
        let report = |progress: &PipelineProgress| {
            stretch_phases.set(PhaseSeconds::of(progress));
            let phases = finished_phases.plus(stretch_phases.get());
            let frame = first_output_frame + progress.frame;
            on_progress(&PipelineProgress {
                stage: progress.stage.clone(),
                message: format!("Frame {frame}/{total_frames}"),
                frame,
                total_frames,
                fps: progress.fps,
                elapsed_secs: start_time.elapsed().as_secs_f64(),
                percent: (frame as f64 / total_frames as f64 * 100.0).min(99.0),
                decode_wait_secs: phases.decode_wait,
                prepare_secs: phases.prepare,
                encode_secs: phases.encode,
                write_secs: phases.write,
            });
        };
        let mut findings = encode_stretch(
            timeline,
            stretch,
            output_dir,
            options,
            StretchOutput::TimelineCodestreams,
            cancel,
            pause,
            report,
            &on_log,
        )?
        .picture_findings;
        let phases = stretch_phases.get();
        let frames_done = first_output_frame + frame_count;
        report(&PipelineProgress {
            stage: "encode".to_string(),
            message: String::new(),
            frame: frame_count,
            total_frames: frame_count,
            fps: frames_done as f64 / start_time.elapsed().as_secs_f64(),
            elapsed_secs: 0.0,
            percent: 0.0,
            decode_wait_secs: phases.decode_wait,
            prepare_secs: phases.prepare,
            encode_secs: phases.encode,
            write_secs: phases.write,
        });
        for interval in findings.black.iter_mut().chain(findings.frozen.iter_mut()) {
            interval.first_frame += first_output_frame;
            interval.last_frame += first_output_frame;
        }
        picture_findings.black.append(&mut findings.black);
        picture_findings.frozen.append(&mut findings.frozen);
        finished_phases = finished_phases.plus(stretch_phases.get());
    }
    on_log(&format!(
        "[TIMELINE] Done: {total_frames} frames from {} clips",
        timeline.clips.len()
    ));
    Ok(EncodeResult {
        j2k_dir,
        frames_encoded: total_frames,
        elapsed_secs: start_time.elapsed().as_secs_f64(),
        picture_findings,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn run_encode_timeline_stretch_and_wrap_picture(
    timeline: &Timeline,
    stretch: &TimelineStretch,
    output_dir: &Path,
    options: &EncodeRunOptions,
    wrap: crate::mxf_wrap::IncrementalWrapOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<(EncodeResult, crate::mxf_wrap::MxfTrackFile), String> {
    refuse_timeline_options(options)?;
    let start_time = std::time::Instant::now();
    let encoded = encode_stretch(
        timeline,
        stretch,
        output_dir,
        options,
        StretchOutput::TrackFile(wrap),
        cancel,
        pause,
        on_progress,
        on_log,
    )?;
    let encode = EncodeResult {
        j2k_dir: output_dir.join("j2k"),
        frames_encoded: encoded.frames_encoded,
        elapsed_secs: start_time.elapsed().as_secs_f64(),
        picture_findings: encoded.picture_findings,
    };
    Ok((encode, encoded.track.expect("a wrap was asked for")))
}

fn refuse_timeline_options(options: &EncodeRunOptions) -> Result<(), String> {
    if options.frame_range.is_some() {
        return Err(
            "a timeline's clips carry their own in and out points, so a timeline encode takes \
             no frame range"
                .to_string(),
        );
    }
    if let Some(filter) = &options.extra_picture_filter {
        return Err(format!(
            "the picture filter {filter} would start again on every clip, so a timeline encode \
             takes none"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StretchBase {
    Clip(usize),
    Black { width: u32, height: u32 },
}

// timeline frames whose clips stay the same throughout
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineStretch {
    pub frames: Range<u64>,
    pub base: StretchBase,
    // clip indices on layers above 0, bottom to top
    pub overlays: Vec<usize>,
}

enum StretchOutput {
    // into output_dir/j2k at the stretch's own timeline frames, beside the other stretches
    TimelineCodestreams,
    // into output_dir/j2k from frame 0, and into this track file as they are written
    TrackFile(crate::mxf_wrap::IncrementalWrapOptions),
}

impl StretchOutput {
    fn first_output_frame(&self, stretch_start: u64) -> u64 {
        match self {
            StretchOutput::TimelineCodestreams => stretch_start,
            StretchOutput::TrackFile(_) => FIRST_FRAME,
        }
    }

    fn wrap(self) -> Option<crate::mxf_wrap::IncrementalWrapOptions> {
        match self {
            StretchOutput::TimelineCodestreams => None,
            StretchOutput::TrackFile(wrap) => Some(wrap),
        }
    }
}

struct StretchEncoded {
    frames_encoded: u64,
    picture_findings: crate::picture_findings::PictureFindings,
    track: Option<crate::mxf_wrap::MxfTrackFile>,
}

#[allow(clippy::too_many_arguments)]
fn encode_stretch(
    timeline: &Timeline,
    stretch: &TimelineStretch,
    output_dir: &Path,
    options: &EncodeRunOptions,
    output: StretchOutput,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    report: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<StretchEncoded, String> {
    match (&stretch.base, stretch.overlays.is_empty()) {
        (StretchBase::Clip(index), true) => encode_base_clip(
            timeline,
            *index,
            &stretch.frames,
            output_dir,
            options,
            output,
            cancel,
            pause,
            report,
            &on_log,
        ),
        _ => encode_composite_stretch(
            timeline, stretch, output_dir, options, output, cancel, pause, report, &on_log,
        ),
    }
}

fn is_compressed(clip: &Clip) -> bool {
    matches!(
        crate::encode::detect_input_type(&clip.picture),
        InputType::J2kSequence | InputType::PictureMxf
    )
}

fn effective_source_colour(clip: &Clip, options: &EncodeRunOptions) -> SourceColour {
    if clip.source_colour == SourceColour::default() {
        return options.source_colour.clone();
    }
    clip.source_colour.clone()
}

fn effective_picture(clip: &Clip, options: &EncodeRunOptions) -> PictureProcessing {
    if clip.picture_processing.is_identity() {
        return options.picture.clone();
    }
    clip.picture_processing.clone()
}

// cut at every clip's start and end on every layer
pub fn timeline_stretches(
    timeline: &Timeline,
    options: &EncodeRunOptions,
) -> Result<Vec<TimelineStretch>, String> {
    timeline.check()?;
    let clips = &timeline.clips;
    if let Some((index, clip)) = clips
        .iter()
        .enumerate()
        .find(|(_, clip)| clip.layer > 0 && is_compressed(clip))
    {
        return Err(format!(
            "{} is on layer {} and is already JPEG 2000, so it cannot be composited",
            crate::timeline::clip_name(index, clip),
            clip.layer
        ));
    }
    let mut cuts: Vec<u64> = clips
        .iter()
        .flat_map(|clip| [clip.start, clip.end()])
        .chain([0])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .map(|pair| {
            let frames = pair[0]..pair[1];
            let covers = |clip: &Clip| clip.start <= frames.start && frames.end <= clip.end();
            let mut overlays: Vec<usize> = (0..clips.len())
                .filter(|&index| clips[index].layer > 0 && covers(&clips[index]))
                .collect();
            overlays.sort_by_key(|&index| clips[index].layer);
            let base = match clips
                .iter()
                .position(|clip| clip.layer == 0 && covers(clip))
            {
                Some(index) => StretchBase::Clip(index),
                None => black_base(&frames, options)?,
            };
            let stretch = TimelineStretch {
                frames,
                base,
                overlays,
            };
            check_composite(timeline, &stretch, options)?;
            Ok(stretch)
        })
        .collect()
}

fn black_base(frames: &Range<u64>, options: &EncodeRunOptions) -> Result<StretchBase, String> {
    let Some(fit) = options.picture.fit else {
        return Err(format!(
            "layer 0 is empty from frame {} to frame {}, and the black there has no raster: \
             the picture options need a fit",
            frames.start, frames.end
        ));
    };
    Ok(StretchBase::Black {
        width: fit.raster_width,
        height: fit.raster_height,
    })
}

fn check_composite(
    timeline: &Timeline,
    stretch: &TimelineStretch,
    options: &EncodeRunOptions,
) -> Result<(), String> {
    let clips = &timeline.clips;
    let name = |index: usize| crate::timeline::clip_name(index, &clips[index]);
    if let (StretchBase::Clip(base), Some(&first_overlay)) =
        (&stretch.base, stretch.overlays.first())
        && is_compressed(&clips[*base])
    {
        return Err(format!(
            "{} lies over {}, which is already JPEG 2000 and cannot be composited",
            name(first_overlay),
            name(*base)
        ));
    }
    let Some(reference) = colour_clip(stretch) else {
        return Ok(());
    };
    let reference_colour = effective_source_colour(&clips[reference], options);
    for &overlay in &stretch.overlays {
        let colour = effective_source_colour(&clips[overlay], options);
        if colour != reference_colour {
            return Err(format!(
                "{} has the source colour {colour:?} and {} under it has {reference_colour:?}: \
                 a composite takes one source colour",
                name(overlay),
                name(reference)
            ));
        }
    }
    Ok(())
}

// the clip whose source colour a stretch takes, none for a gap with nothing over it
fn colour_clip(stretch: &TimelineStretch) -> Option<usize> {
    match stretch.base {
        StretchBase::Clip(index) => Some(index),
        StretchBase::Black { .. } => stretch.overlays.first().copied(),
    }
}

fn clip_window(clip: &Clip, frames: &Range<u64>) -> FrameRange {
    FrameRange {
        first_frame: clip.in_frame + (frames.start - clip.start),
        frame_count: frames.end - frames.start,
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_base_clip(
    timeline: &Timeline,
    index: usize,
    frames: &Range<u64>,
    output_dir: &Path,
    options: &EncodeRunOptions,
    output: StretchOutput,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    report: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<StretchEncoded, String> {
    let clip = &timeline.clips[index];
    let name = crate::timeline::clip_name(index, clip);
    let window = clip_window(clip, frames);
    on_log(&format!(
        "[TIMELINE] {name}: frames {}..{} at timeline frame {}",
        window.first_frame,
        window.end_frame(),
        frames.start
    ));
    let input_type = crate::encode::detect_input_type(&clip.picture);
    let compressed = matches!(input_type, InputType::J2kSequence | InputType::PictureMxf);
    // the options' fit only sizes the black of a gap, a compressed clip has no frames to fit
    let picture = if compressed {
        clip.picture_processing.clone()
    } else {
        effective_picture(clip, options)
    };
    let clip_options = EncodeRunOptions {
        source_colour: effective_source_colour(clip, options),
        picture,
        ..options.clone()
    };
    if crate::still::is_still_image(&clip.picture) {
        return encode_held_still(
            index,
            &clip.picture,
            frames,
            timeline.fps,
            output_dir,
            &clip_options,
            output,
            cancel,
            pause,
            report,
            &on_log,
        )
        .map_err(|error| name_the_clip(&name, error));
    }
    if compressed && let StretchOutput::TimelineCodestreams = output {
        let scratch_dir = output_dir.join(format!("timeline_clip_{index}"));
        place_compressed_clip(
            clip,
            input_type,
            &scratch_dir,
            &output_dir.join("j2k"),
            frames.start,
            &clip_options,
            cancel,
            pause,
            report,
            &on_log,
        )
        .map_err(|error| name_the_clip(&name, error))?;
        return Ok(StretchEncoded {
            frames_encoded: frames.end - frames.start,
            picture_findings: crate::picture_findings::PictureFindings::default(),
            track: None,
        });
    }
    // a compressed clip goes whole, as it would through run_encode_and_wrap_picture
    let windowed = EncodeRunOptions {
        frame_range: (!compressed).then_some(window),
        ..clip_options
    };
    let first_output_frame = output.first_output_frame(frames.start);
    let (encode, track) = run_encode_and_maybe_wrap(
        &clip.picture,
        output_dir,
        &windowed,
        output.wrap(),
        first_output_frame,
        cancel,
        pause,
        report,
        &on_log,
    )
    .map_err(|error| name_the_clip(&name, error))?;
    Ok(StretchEncoded {
        frames_encoded: encode.frames_encoded,
        picture_findings: encode.picture_findings,
        track,
    })
}

// a directory of the one image takes the still through the encode an image sequence takes
#[allow(clippy::too_many_arguments)]
fn encode_held_still(
    index: usize,
    image: &Path,
    frames: &Range<u64>,
    fps: FrameRate,
    output_dir: &Path,
    options: &EncodeRunOptions,
    output: StretchOutput,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    report: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<StretchEncoded, String> {
    let frame_count = frames.end - frames.start;
    let first_output_frame = output.first_output_frame(frames.start);
    let output_frames = first_output_frame..first_output_frame + frame_count;
    let encoded =
        crate::still::distinct_frames(options.subtitle_burn.as_deref(), output_frames.clone());
    let image_dir = output_dir.join(format!("timeline_clip_{index}_image"));
    if image_dir.exists() {
        std::fs::remove_dir_all(&image_dir)
            .map_err(|e| format!("cannot clear {}: {e}", image_dir.display()))?;
    }
    std::fs::create_dir_all(&image_dir)
        .map_err(|e| format!("cannot create {}: {e}", image_dir.display()))?;
    link_or_copy(
        image,
        &image_dir.join(image.file_name().expect("a still image has a file name")),
    )?;
    let j2k_dir = output_dir.join("j2k");
    let mut wrap = output
        .wrap()
        .map(crate::mxf_wrap::IncrementalJ2kWrap::new)
        .transpose()?;
    let mut finished_phases = PhaseSeconds::default();
    for (at, &start) in encoded.iter().enumerate() {
        let end = encoded.get(at + 1).copied().unwrap_or(output_frames.end);
        let run_offset = start - first_output_frame;
        let run_length = end - start;
        let run_phases = std::cell::Cell::new(PhaseSeconds::default());
        run_encode_and_maybe_wrap(
            &image_dir,
            output_dir,
            options,
            None,
            start,
            cancel,
            pause,
            |progress: &PipelineProgress| {
                run_phases.set(PhaseSeconds::of(progress));
                let phases = finished_phases.plus(run_phases.get());
                // the one encoded frame stands for every frame of its run
                let frame = run_offset + progress.frame * run_length;
                report(&PipelineProgress {
                    message: format!("Frame {frame}/{frame_count}"),
                    frame,
                    total_frames: frame_count,
                    percent: (frame as f64 / frame_count as f64 * 100.0).min(99.0),
                    decode_wait_secs: phases.decode_wait,
                    prepare_secs: phases.prepare,
                    encode_secs: phases.encode,
                    write_secs: phases.write,
                    ..progress.clone()
                });
            },
            &on_log,
        )?;
        finished_phases = finished_phases.plus(run_phases.get());
        let codestream = j2k_dir.join(format!("frame_{start:08}.j2c"));
        for frame in start + 1..end {
            link_or_copy(&codestream, &j2k_dir.join(format!("frame_{frame:08}.j2c")))?;
        }
        if let Some(wrap) = wrap.as_mut() {
            let data = std::fs::read(&codestream)
                .map_err(|e| format!("cannot read {}: {e}", codestream.display()))?;
            for _ in start..end {
                if cancel.load(Ordering::Relaxed) {
                    return Err(CANCELLED.to_string());
                }
                wrap.write_frame(&data)?;
            }
        }
    }
    std::fs::remove_dir_all(&image_dir)
        .map_err(|e| format!("cannot remove {}: {e}", image_dir.display()))?;
    let track = wrap
        .map(crate::mxf_wrap::IncrementalJ2kWrap::finish)
        .transpose()?;
    if let Some(track) = &track {
        on_log(&format!(
            "[WRAP] {} frames into {}",
            track.duration,
            track.path.display()
        ));
    }
    let black = if options.detect_picture_findings {
        crate::picture_findings::detect_black_in_held_image(image, fps, frame_count)?
    } else {
        Vec::new()
    };
    Ok(StretchEncoded {
        frames_encoded: frame_count,
        picture_findings: crate::picture_findings::PictureFindings {
            black,
            frozen: Vec::new(),
        },
        track,
    })
}

fn link_or_copy(source: &Path, target: &Path) -> Result<(), String> {
    std::fs::hard_link(source, target)
        .or_else(|_| std::fs::copy(source, target).map(|_| ()))
        .map_err(|e| format!("cannot place {}: {e}", target.display()))
}

fn composite_layer(
    timeline: &Timeline,
    index: usize,
    frames: &Range<u64>,
    output_dir: &Path,
    options: &EncodeRunOptions,
) -> Result<CompositeLayer, String> {
    let clip = &timeline.clips[index];
    let name = crate::timeline::clip_name(index, clip);
    let window = clip_window(clip, frames);
    let picture = effective_picture(clip, options);
    let list = output_dir.join(format!("timeline_clip_{index}.ffconcat"));
    if crate::still::is_still_image(&clip.picture) {
        crate::encode::write_image_concat_list(
            std::slice::from_ref(&clip.picture),
            timeline.fps,
            &list,
        )?;
        return Ok(CompositeLayer {
            name,
            input: list,
            decode_source: DecodeSource::ImageList,
            frames: LayerFrames::Held,
            picture,
        });
    }
    match crate::encode::detect_input_type(&clip.picture) {
        InputType::Video => Ok(CompositeLayer {
            name,
            input: clip.picture.clone(),
            decode_source: DecodeSource::Video,
            frames: LayerFrames::Advancing {
                frame_range: Some(window),
            },
            picture,
        }),
        InputType::ImageSequence => {
            let images = crate::encode::find_source_frames(&clip.picture)
                .map_err(|e| format!("{name}: cannot list {}: {e}", clip.picture.display()))?;
            crate::encode::write_image_concat_list(
                window
                    .window_of(&images)
                    .map_err(|error| name_the_clip(&name, error))?,
                timeline.fps,
                &list,
            )?;
            Ok(CompositeLayer {
                name,
                input: list,
                decode_source: DecodeSource::ImageList,
                frames: LayerFrames::Advancing { frame_range: None },
                picture,
            })
        }
        other => unreachable!(
            "the timeline check refuses an unknown picture and the stretches a compressed \
             one, so {name} cannot be {other:?}"
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_composite_stretch(
    timeline: &Timeline,
    stretch: &TimelineStretch,
    output_dir: &Path,
    options: &EncodeRunOptions,
    output: StretchOutput,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    report: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<StretchEncoded, String> {
    let frames = &stretch.frames;
    let base = match stretch.base {
        StretchBase::Clip(index) => CompositeBase::Clip(composite_layer(
            timeline, index, frames, output_dir, options,
        )?),
        StretchBase::Black { width, height } => CompositeBase::Black { width, height },
    };
    let mut overlays = Vec::new();
    for &index in &stretch.overlays {
        overlays.push(CompositeOverlay {
            layer: composite_layer(timeline, index, frames, output_dir, options)?,
            opacity: timeline.clips[index].opacity,
        });
    }
    let layer_names: Vec<&str> = std::iter::once(match &base {
        CompositeBase::Clip(layer) => layer.name.as_str(),
        CompositeBase::Black { .. } => "black",
    })
    .chain(overlays.iter().map(|overlay| overlay.layer.name.as_str()))
    .collect();
    let stretch_name = format!("timeline frames {}..{}", frames.start, frames.end);
    on_log(&format!(
        "[TIMELINE] {stretch_name}: {}, bottom to top",
        layer_names.join(", ")
    ));
    let source_colour = colour_clip(stretch).map_or(options.source_colour.clone(), |index| {
        effective_source_colour(&timeline.clips[index], options)
    });
    if options.subtitle_burn.is_some() {
        // every layer decodes through ffmpeg the way a video does
        reject_unsupported_burn(InputType::Video, &source_colour)?;
    }
    let stream_options = StreamEncodeOptions {
        source_colour,
        ..stream_encode_options(
            options,
            &output_dir.join("j2k"),
            output.first_output_frame(frames.start),
        )
    };
    let composite = Composite {
        base,
        overlays,
        frame_count: frames.end - frames.start,
    };
    let mut overlapped_wrap = output
        .wrap()
        .map(crate::mxf_wrap::OverlappedJ2kWrap::start)
        .transpose()?;
    let result = crate::composite::encode_composite(
        &stream_options,
        &composite,
        cancel,
        pause,
        overlapped_wrap.as_ref().map(|wrap| wrap.sender()),
        |progress| report_stream_frame(progress, &report, &on_log),
        &on_log,
    );
    let (frames_encoded, picture_findings) =
        wrapped_stream_result(result, overlapped_wrap.as_mut())
            .map_err(|error| name_the_clip(&stretch_name, error))?;
    let track = finish_overlapped_wrap(overlapped_wrap, frames_encoded, &on_log)?;
    let base_layer = match &composite.base {
        CompositeBase::Clip(layer) => Some(layer),
        CompositeBase::Black { .. } => None,
    };
    let layers = base_layer
        .into_iter()
        .chain(composite.overlays.iter().map(|overlay| &overlay.layer));
    for layer in layers.filter(|layer| layer.decode_source == DecodeSource::ImageList) {
        std::fs::remove_file(&layer.input)
            .map_err(|e| format!("cannot remove {}: {e}", layer.input.display()))?;
    }
    Ok(StretchEncoded {
        frames_encoded,
        picture_findings,
        track,
    })
}

fn name_the_clip(name: &str, error: String) -> String {
    if error == CANCELLED {
        return error;
    }
    format!("{name}: {error}")
}

#[derive(Clone, Copy, Default)]
struct PhaseSeconds {
    decode_wait: f64,
    prepare: f64,
    encode: f64,
    write: f64,
}

impl PhaseSeconds {
    fn of(progress: &PipelineProgress) -> Self {
        Self {
            decode_wait: progress.decode_wait_secs,
            prepare: progress.prepare_secs,
            encode: progress.encode_secs,
            write: progress.write_secs,
        }
    }

    fn plus(self, other: Self) -> Self {
        Self {
            decode_wait: self.decode_wait + other.decode_wait,
            prepare: self.prepare + other.prepare,
            encode: self.encode + other.encode,
            write: self.write + other.write,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn place_compressed_clip(
    clip: &Clip,
    input_type: InputType,
    scratch_dir: &Path,
    j2k_dir: &Path,
    first_output_frame: u64,
    options: &EncodeRunOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<(), String> {
    let crate::timeline::SourceLength::Counted {
        frames: source_frames,
        ..
    } = crate::timeline::probe_source(&clip.picture)?
    else {
        unreachable!("a compressed clip is a codestream directory or a picture MXF, never a still")
    };
    if clip.in_frame != 0 || clip.out_frame != source_frames {
        return Err(format!(
            "it is already JPEG 2000 and cannot be trimmed: its in and out points have to be \
             0 and {source_frames}, the whole source"
        ));
    }
    let (encode, _) = run_encode_and_maybe_wrap(
        &clip.picture,
        scratch_dir,
        options,
        None,
        FIRST_FRAME,
        cancel,
        pause,
        on_progress,
        on_log,
    )?;
    std::fs::create_dir_all(j2k_dir)
        .map_err(|e| format!("cannot create {}: {e}", j2k_dir.display()))?;
    let unwrapped = input_type == InputType::PictureMxf;
    let source_dir = crate::encode::sequence_directory(&encode.j2k_dir);
    for (offset, codestream) in crate::timeline::j2k_codestreams(&source_dir)?
        .iter()
        .enumerate()
    {
        let target = j2k_dir.join(format!(
            "frame_{:08}.j2c",
            first_output_frame + offset as u64
        ));
        if unwrapped {
            std::fs::rename(codestream, &target)
                .map_err(|e| format!("cannot place {}: {e}", target.display()))?;
        } else {
            link_or_copy(codestream, &target)?;
        }
    }
    std::fs::remove_dir_all(scratch_dir)
        .map_err(|e| format!("cannot remove {}: {e}", scratch_dir.display()))
}

const FIRST_FRAME: u64 = 0;

#[allow(clippy::too_many_arguments)]
fn run_encode_and_maybe_wrap(
    video: &Path,
    output_dir: &Path,
    options: &EncodeRunOptions,
    wrap: Option<crate::mxf_wrap::IncrementalWrapOptions>,
    first_output_frame: u64,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: impl Fn(&PipelineProgress),
    on_log: impl Fn(&str),
) -> Result<(EncodeResult, Option<crate::mxf_wrap::MxfTrackFile>), String> {
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
    // tiff stills only reach ffmpeg for a picture change
    let sequence_needs_ffmpeg = !options.picture.is_identity()
        || options.extra_picture_filter.is_some()
        || sequence_frame_format != Some(ImageFormat::Tiff);
    let decodes_through_ffmpeg = match input_type {
        InputType::Video => true,
        InputType::ImageSequence => sequence_needs_ffmpeg,
        InputType::J2kSequence | InputType::PictureMxf | InputType::Unknown => false,
    };
    if options.read_source_at.is_some() && input_type != InputType::Video {
        return Err(format!(
            "a {input_type:?} input carries no timestamps to override, so it cannot be read at \
             another rate"
        ));
    }
    if options.frame_range.is_some() {
        reject_unsupported_frame_range(input_type)?;
    }
    reject_unsupported_colour_path(input_type, &options.source_colour, decodes_through_ffmpeg)?;
    if options.subtitle_burn.is_some() {
        reject_unsupported_burn(input_type, &options.source_colour)?;
    }
    if !options.picture.is_identity() {
        reject_unsupported_picture(input_type)?;
    }

    let mut overlapped_wrap = match wrap {
        Some(wrap) if matches!(input_type, InputType::J2kSequence | InputType::PictureMxf) => {
            return Err(format!(
                "a J2K sequence is never encoded here, so it hands postkit no codestream to \
                 wrap while it encodes: wrap {} from the sequence instead",
                wrap.output.display()
            ));
        }
        Some(wrap) => Some(crate::mxf_wrap::OverlappedJ2kWrap::start(wrap)?),
        None => None,
    };

    let j2k_dir = output_dir.join("j2k");
    let mut frames_encoded = 0u64;
    let mut picture_findings = crate::picture_findings::PictureFindings::default();

    // every in-process encode reports through these two
    let report_start = || {
        on_progress(&PipelineProgress {
            stage: "encode".to_string(),
            message: "Starting...".to_string(),
            frame: 0,
            total_frames: 0,
            fps: 0.0,
            elapsed_secs: 0.0,
            percent: 0.0,
            decode_wait_secs: 0.0,
            prepare_secs: 0.0,
            encode_secs: 0.0,
            write_secs: 0.0,
        });
    };
    let report_frame = |p: StreamProgress| report_stream_frame(p, &on_progress, &on_log);
    let mxf_feed = overlapped_wrap.as_ref().map(|wrap| wrap.sender());
    let mut finish = |result: crate::encode::EncodeResult| {
        wrapped_stream_result(result, overlapped_wrap.as_mut())
    };
    // a cloned feed would keep the wrap waiting after the encoder returns
    let run_stream = |opts: &StreamEncodeOptions, mxf_feed| {
        report_start();
        stream_encode_inprocess_with_mxf_feed(opts, cancel, pause, mxf_feed, report_frame, &on_log)
    };

    match input_type {
        InputType::Video => {
            (frames_encoded, picture_findings) = finish(run_stream(
                &StreamEncodeOptions {
                    input: video.to_path_buf(),
                    read_source_at: options.read_source_at,
                    frame_range: options.frame_range,
                    ..stream_encode_options(options, &j2k_dir, first_output_frame)
                },
                mxf_feed,
            ))?;
            on_log(&format!("[ENCODE] Done: {} frames", frames_encoded));
        }
        InputType::ImageSequence => {
            let input_dir = crate::encode::sequence_directory(video);
            let frames = crate::encode::find_source_frames(&input_dir)
                .map_err(|e| format!("cannot list {}: {e}", input_dir.display()))?;
            if frames.is_empty() {
                return Err(format!("no images in {}", input_dir.display()));
            }
            let frames = match options.frame_range {
                Some(range) => range.window_of(&frames)?.to_vec(),
                None => frames,
            };

            let compress = stream_encode_options(options, &j2k_dir, first_output_frame);
            if sequence_needs_ffmpeg {
                // a concat list holding each still for one frame period decodes
                // to the same rgb48be stream a video does
                let list = output_dir.join("frames.ffconcat");
                crate::encode::write_image_concat_list(&frames, options.fps, &list)?;
                on_log(&format!(
                    "[ENCODE] Taking {} images through ffmpeg",
                    frames.len()
                ));
                (frames_encoded, picture_findings) = finish(run_stream(
                    &StreamEncodeOptions {
                        input: list,
                        decode_source: crate::encode::DecodeSource::ImageList,
                        ..compress
                    },
                    mxf_feed,
                ))?;
            } else {
                on_log(&format!("[ENCODE] Reading {} tiff stills", frames.len()));
                report_start();
                (frames_encoded, picture_findings) = finish(encode_tiff_sequence_inprocess(
                    &frames,
                    &compress,
                    cancel,
                    pause,
                    mxf_feed,
                    report_frame,
                ))?;
            }
            on_log(&format!("[ENCODE] Done: {} frames", frames_encoded));
        }
        InputType::J2kSequence => {
            on_log("Input is already J2K, skipping encode");
        }
        InputType::PictureMxf => {
            report_start();
            let unwrap_start = std::time::Instant::now();
            let info = crate::mxf_unwrap::unwrap_picture_mxf(
                video,
                &j2k_dir,
                cancel,
                &mut |frame, total_frames| {
                    let elapsed_secs = unwrap_start.elapsed().as_secs_f64();
                    report_frame(StreamProgress {
                        frame,
                        total_frames,
                        fps: frame as f64 / elapsed_secs,
                        elapsed_secs,
                        decode_wait_secs: 0.0,
                        prepare_secs: 0.0,
                        encode_secs: 0.0,
                        write_secs: 0.0,
                    })
                },
            )
            .inspect_err(|error| {
                if error == CANCELLED {
                    on_log("=== CANCELLED ===");
                }
            })?;
            on_log(&format!(
                "[ENCODE] Unwrapped {} frames at {}/{} fps from {}, skipping encode",
                info.frames,
                info.edit_rate_num,
                info.edit_rate_den,
                video.display()
            ));
        }
        InputType::Unknown => {
            return Err(format!("Cannot determine input type: {}", video.display()));
        }
    }

    if cancel.load(Ordering::Relaxed) {
        on_log("=== CANCELLED ===");
        return Err(CANCELLED.to_string());
    }

    let final_j2k_dir = match input_type {
        InputType::J2kSequence => video.to_path_buf(),
        _ => j2k_dir,
    };

    // only J2K input is unchecked by the encoder
    if let Some(cap) = options.codestream_byte_cap
        && matches!(input_type, InputType::J2kSequence | InputType::PictureMxf)
    {
        check_codestream_dir(&final_j2k_dir, cap)?;
    }

    let track_file = finish_overlapped_wrap(overlapped_wrap.take(), frames_encoded, &on_log)?;

    let elapsed_secs = start_time.elapsed().as_secs_f64();

    Ok((
        EncodeResult {
            j2k_dir: final_j2k_dir,
            frames_encoded,
            elapsed_secs,
            picture_findings,
        },
        track_file,
    ))
}

fn wrapped_stream_result(
    result: crate::encode::EncodeResult,
    overlapped_wrap: Option<&mut crate::mxf_wrap::OverlappedJ2kWrap>,
) -> Result<(u64, crate::picture_findings::PictureFindings), String> {
    if !result.success {
        // the encoder only sees that the wrap stopped taking frames, so the
        // wrap's own error is the one worth reporting
        if let Some(wrap) = overlapped_wrap
            && let Some(error) = wrap.abandon()
        {
            return Err(error);
        }
        return Err(result.error);
    }
    Ok((result.frames_encoded, result.picture_findings))
}

// the wrap has every frame by now, so this only writes the footer and hashes
fn finish_overlapped_wrap(
    overlapped_wrap: Option<crate::mxf_wrap::OverlappedJ2kWrap>,
    frames_encoded: u64,
    on_log: &impl Fn(&str),
) -> Result<Option<crate::mxf_wrap::MxfTrackFile>, String> {
    let Some(wrap) = overlapped_wrap else {
        return Ok(None);
    };
    let track = wrap.finish(frames_encoded)?;
    on_log(&format!(
        "[WRAP] {} frames into {}",
        track.duration,
        track.path.display()
    ));
    Ok(Some(track))
}

fn stream_encode_options(
    options: &EncodeRunOptions,
    j2k_dir: &Path,
    first_output_frame: u64,
) -> StreamEncodeOptions {
    StreamEncodeOptions {
        output_dir: j2k_dir.to_path_buf(),
        compression_ratio: options.compression_ratio,
        target_codestream_bytes: options.target_codestream_bytes,
        quality_psnr: options.quality_psnr,
        num_resolutions: 6,
        codeblock_size: 32,
        progression: "CPRL".to_string(),
        fps: options.fps,
        source_colour: options.source_colour.clone(),
        rsiz: options.rsiz,
        subtitle_burn: options.subtitle_burn.clone(),
        picture: options.picture.clone(),
        extra_picture_filter: options.extra_picture_filter.clone(),
        codestream_byte_cap: options.codestream_byte_cap,
        encode_threads: options.encode_threads,
        detect_picture_findings: options.detect_picture_findings,
        first_output_frame,
        ..StreamEncodeOptions::default()
    }
}

fn report_stream_frame(
    p: StreamProgress,
    on_progress: &impl Fn(&PipelineProgress),
    on_log: &impl Fn(&str),
) {
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
        decode_wait_secs: p.decode_wait_secs,
        prepare_secs: p.prepare_secs,
        encode_secs: p.encode_secs,
        write_secs: p.write_secs,
    });
    on_log(&format!(
        "[ENCODE] frame={}/{} fps={:.1}",
        p.frame, p.total_frames, p.fps
    ));
}

/// Format of the first frame of an image sequence, which decides whether
/// postkit reads the stills itself or ffmpeg decodes them.
fn first_frame_format(input: &Path) -> Result<ImageFormat, String> {
    let first = crate::encode::first_source_frame(&crate::encode::sequence_directory(input))?;
    Ok(crate::encode::detect_image_format(&first))
}

/// Refuse a source colour the chosen input branch cannot honour: a decode LUT
/// runs inside ffmpeg's decode, so a TIFF sequence postkit reads itself cannot
/// take it, and nothing can be converted once the frames are compressed.
///
/// `decodes_through_ffmpeg` is what the sequence limit hangs on rather than the
/// input type: a sequence that a picture change routes through ffmpeg reaches
/// the LUT the way a video does.
fn reject_unsupported_colour_path(
    input_type: InputType,
    source_colour: &SourceColour,
    decodes_through_ffmpeg: bool,
) -> Result<(), String> {
    let already_compressed = matches!(input_type, InputType::J2kSequence | InputType::PictureMxf);
    if let Some(lut) = source_colour.decode_lut() {
        if input_type == InputType::ImageSequence && !decodes_through_ffmpeg {
            return Err(format!(
                "the 3D LUT {} runs inside ffmpeg's decode, and TIFF stills are read by \
                 postkit itself: encode from a video instead",
                lut.display()
            ));
        }
        if already_compressed {
            return Err(format!(
                "J2K input is already compressed, so the 3D LUT {} cannot be applied",
                lut.display()
            ));
        }
    }
    match source_colour {
        SourceColour::DisplayRgbIn(space) if already_compressed => Err(format!(
            "J2K input is already compressed, so a {space:?} source cannot be converted to \
             X'Y'Z' any more"
        )),
        SourceColour::KeepRgbFrom(space) if already_compressed => Err(format!(
            "J2K input is already compressed, so a {space:?} source cannot be converted to \
             Rec.709 RGB any more"
        )),
        SourceColour::HdrDcdm { source, .. } if already_compressed => Err(format!(
            "J2K input is already compressed, so a {source:?} master cannot be converted to \
             the DCI HDR Addendum's X\"Y\"Z\" any more"
        )),
        _ => Ok(()),
    }
}

/// Refuse a burn the encode cannot honour: subtitle bitmaps are display RGB, so
/// they only make sense on frames that have not been converted yet, and a J2K
/// sequence has no frames to composite onto at all.
fn reject_unsupported_burn(
    input_type: InputType,
    source_colour: &SourceColour,
) -> Result<(), String> {
    match (input_type, source_colour) {
        (InputType::J2kSequence | InputType::PictureMxf, _) => Err(
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
        (_, SourceColour::HdrDcdm { source, .. }) => Err(format!(
            "a {source:?} master reaches the burn as PQ-encoded HDR samples, so burnt-in text \
             would be drawn in the wrong colour space"
        )),
        _ => Ok(()),
    }
}

/// Refuse a frame range on an input the encode never compresses.
fn reject_unsupported_frame_range(input_type: InputType) -> Result<(), String> {
    match input_type {
        InputType::J2kSequence | InputType::PictureMxf => Err(
            "a J2K sequence is never encoded here, so a frame range has nothing to narrow: link \
             the codestreams you want instead"
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
        InputType::J2kSequence | InputType::PictureMxf => Err(
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
    fn the_phase_breakdown_reads_as_one_line_of_minutes_and_seconds() {
        let progress = PipelineProgress {
            stage: "encode".to_string(),
            message: "Frame 6000/6000".to_string(),
            frame: 6000,
            total_frames: 6000,
            fps: 24.0,
            elapsed_secs: 300.0,
            percent: 100.0,
            decode_wait_secs: 12.0,
            prepare_secs: 30.4,
            encode_secs: 250.0,
            write_secs: 7.6,
        };
        assert_eq!(
            progress.phase_breakdown(),
            "decoder wait 12s, frame prep 30s, j2k 4m10s, write 8s"
        );

        assert_eq!(format_minutes_seconds(0.0), "0s");
        assert_eq!(format_minutes_seconds(59.6), "1m0s");
        assert_eq!(format_minutes_seconds(3600.0), "60m0s");
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn the_video_encode_logs_the_pixel_format_it_decodes_to() {
        let directory = tempfile::tempdir().unwrap();
        let video = directory.path().join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=64x48:rate=24:duration=1")
            .args(["-frames:v", "2", "-pix_fmt", "yuv420p"])
            .arg(&video)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        let lines = std::sync::Mutex::new(Vec::new());
        run_encode_with_options(
            &video,
            &directory.path().join("out"),
            &EncodeRunOptions::default(),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
            |line| lines.lock().unwrap().push(line.to_string()),
        )
        .unwrap();
        let lines = lines.into_inner().unwrap();
        assert!(
            lines.iter().any(|line| {
                line.starts_with("[ENCODE] decoding to pixel_format=")
                    && line.ends_with(" hardware_decode=false")
            }),
            "{lines:#?}"
        );
    }

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
            "an image sequence burns on the frames postkit reads or ffmpeg decodes"
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
            (InputType::Video, hdr10_source(), "wrong colour space"),
        ] {
            let err = reject_unsupported_burn(input, &colour)
                .expect_err("this combination has to refuse a burn");
            assert!(err.contains(expected), "got: {err}");
        }
    }

    fn hdr10_source() -> SourceColour {
        SourceColour::HdrDcdm {
            source: crate::colour::HdrSource::Hdr10,
            source_peak_nits: crate::colour::HdrSource::DEFAULT_PEAK_NITS,
        }
    }

    #[test]
    fn compressed_input_refuses_an_hdr_master() {
        assert!(
            reject_unsupported_colour_path(InputType::Video, &hdr10_source(), THROUGH_FFMPEG)
                .is_ok()
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::ImageSequence,
                &hdr10_source(),
                READ_BY_POSTKIT
            )
            .is_ok(),
            "postkit reads the stills and converts them itself"
        );
        let compressed = reject_unsupported_colour_path(
            InputType::J2kSequence,
            &hdr10_source(),
            READ_BY_POSTKIT,
        )
        .unwrap_err();
        assert!(compressed.contains("already compressed"), "{compressed}");
    }

    /// The branch an input takes, as `reject_unsupported_colour_path` sees it.
    const THROUGH_FFMPEG: bool = true;
    const READ_BY_POSTKIT: bool = false;

    #[test]
    fn a_tiff_sequence_takes_every_colour_but_the_lut() {
        for colour in [
            SourceColour::DisplayRgb,
            SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3),
            SourceColour::AlreadyPq,
            SourceColour::KeepRgb,
            SourceColour::KeepRgbFrom(crate::colour::ColourSpace::P3),
        ] {
            assert!(
                reject_unsupported_colour_path(InputType::ImageSequence, &colour, READ_BY_POSTKIT)
                    .is_ok(),
                "{colour:?} is applied on the frames postkit reads"
            );
        }
        for lut in [
            SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
            SourceColour::KeepRgbAfterLut(PathBuf::from("/luts/hdr_to_dci.cube")),
        ] {
            let refused =
                reject_unsupported_colour_path(InputType::ImageSequence, &lut, READ_BY_POSTKIT)
                    .unwrap_err();
            assert!(refused.contains("hdr_to_dci.cube"), "{refused}");
            assert!(
                reject_unsupported_colour_path(InputType::ImageSequence, &lut, THROUGH_FFMPEG)
                    .is_ok(),
                "a picture change decodes the sequence through ffmpeg, where the LUT runs"
            );
        }
    }

    #[test]
    fn compressed_input_refuses_a_wide_gamut_source() {
        for p3 in [
            SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3),
            SourceColour::KeepRgbFrom(crate::colour::ColourSpace::P3),
        ] {
            assert!(reject_unsupported_colour_path(InputType::Video, &p3, THROUGH_FFMPEG).is_ok());
            let compressed =
                reject_unsupported_colour_path(InputType::J2kSequence, &p3, READ_BY_POSTKIT)
                    .unwrap_err();
            assert!(compressed.contains("already compressed"), "{compressed}");
        }
    }

    #[test]
    fn compressed_input_refuses_a_lut_but_takes_pq_frames() {
        assert!(
            reject_unsupported_colour_path(
                InputType::J2kSequence,
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                READ_BY_POSTKIT
            )
            .is_err()
        );
        assert!(
            reject_unsupported_colour_path(
                InputType::J2kSequence,
                &SourceColour::AlreadyPq,
                READ_BY_POSTKIT
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
    fn compressed_input_refuses_a_frame_range() {
        assert!(reject_unsupported_frame_range(InputType::Video).is_ok());
        assert!(reject_unsupported_frame_range(InputType::ImageSequence).is_ok());
        let compressed = reject_unsupported_frame_range(InputType::J2kSequence).unwrap_err();
        assert!(compressed.contains("never encoded here"), "{compressed}");
        assert!(compressed.contains("link the codestreams"), "{compressed}");
    }

    #[test]
    fn compressed_input_refuses_picture_processing() {
        assert!(reject_unsupported_picture(InputType::Video).is_ok());
        assert!(reject_unsupported_picture(InputType::ImageSequence).is_ok());
        let compressed = reject_unsupported_picture(InputType::J2kSequence).unwrap_err();
        assert!(compressed.contains("no frames to crop"), "{compressed}");
    }

    #[test]
    fn a_picture_mxf_is_refused_like_a_j2k_sequence() {
        let compressed = [InputType::J2kSequence, InputType::PictureMxf];
        let [sequence, mxf] = compressed.map(reject_unsupported_frame_range);
        assert_eq!(mxf, sequence);
        let [sequence, mxf] = compressed.map(reject_unsupported_picture);
        assert_eq!(mxf, sequence);
        let [sequence, mxf] =
            compressed.map(|input| reject_unsupported_burn(input, &SourceColour::DisplayRgb));
        assert_eq!(mxf, sequence);
        let colours = [
            hdr10_source(),
            SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3),
            SourceColour::KeepRgbFrom(crate::colour::ColourSpace::P3),
            SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
        ];
        for colour in colours {
            let [sequence, mxf] = compressed
                .map(|input| reject_unsupported_colour_path(input, &colour, READ_BY_POSTKIT));
            assert!(sequence.is_err(), "{colour:?}");
            assert_eq!(mxf, sequence, "{colour:?}");
        }
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
