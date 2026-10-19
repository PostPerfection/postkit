use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::composite::{Composite, composite_frames, encode_composite};
use crate::encode::{
    EncoderInputFormat, FrameRange, FrameRate, InputType, SourceColour, StreamEncodeOptions,
    StreamProgress, compress_params,
};
use crate::ffmpeg_decode::{FrameSource, NextFrame};
use crate::grok_encoder::SampleOrder;
use crate::mxf_unwrap::CANCELLED;
use crate::picture_processing::{Fit, PictureProcessing, Placement};
use crate::pipeline::{
    EncodeRunOptions, StretchBase, TimelineStretch, clip_window, effective_picture,
    effective_source_colour, refuse_timeline_options, stream_encode_options, stretch_composite,
    stretch_source_colour, timeline_stretches,
};
use crate::timeline::{Clip, RenderedStretch, Timeline, preview_renders_clip};

const PREVIEW_RASTER_MAX_WIDTH: u32 = 2048;

// the crop is in source pixels and stays as it is
pub fn preview_fit(fit: Fit) -> Fit {
    if fit.raster_width <= PREVIEW_RASTER_MAX_WIDTH {
        return fit;
    }
    Fit {
        box_width: fit.box_width / 2,
        box_height: fit.box_height / 2,
        raster_width: fit.raster_width / 2,
        raster_height: fit.raster_height / 2,
        placement: Placement {
            offset_x: fit.placement.offset_x / 2,
            offset_y: fit.placement.offset_y / 2,
            ..fit.placement
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewRenderProgress {
    pub frames_rendered: u64,
    // cached stretches are not counted
    pub total_frames: u64,
}

pub fn render_mpv_preview_stretches(
    timeline: &Timeline,
    options: &EncodeRunOptions,
    cache_dir: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: impl FnMut(PreviewRenderProgress),
) -> Result<Vec<RenderedStretch>, String> {
    render_preview_stretches(
        timeline,
        options,
        cache_dir,
        cancel,
        RenderedFormat::Mjpeg,
        on_progress,
    )
}

pub fn render_grok_preview_stretches(
    timeline: &Timeline,
    options: &EncodeRunOptions,
    cache_dir: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: impl FnMut(PreviewRenderProgress),
) -> Result<Vec<RenderedStretch>, String> {
    render_preview_stretches(
        timeline,
        options,
        cache_dir,
        cancel,
        RenderedFormat::Codestreams,
        on_progress,
    )
}

// changing it makes every earlier render unused
const RENDER_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize)]
enum RenderedFormat {
    Mjpeg,
    Codestreams,
}

impl RenderedFormat {
    fn file_name(self, key: &str) -> String {
        match self {
            RenderedFormat::Mjpeg => format!("{key}.mkv"),
            RenderedFormat::Codestreams => key.to_string(),
        }
    }
}

struct PlannedStretch {
    stretch: TimelineStretch,
    key: String,
    path: PathBuf,
}

fn render_preview_stretches(
    timeline: &Timeline,
    options: &EncodeRunOptions,
    cache_dir: &Path,
    cancel: &Arc<AtomicBool>,
    format: RenderedFormat,
    mut on_progress: impl FnMut(PreviewRenderProgress),
) -> Result<Vec<RenderedStretch>, String> {
    refuse_timeline_options(options)?;
    let options = preview_options(options);
    let timeline = preview_timeline(timeline);
    let planned: Vec<PlannedStretch> = timeline_stretches(&timeline, &options)?
        .into_iter()
        .filter(|stretch| stretch_is_rendered(&timeline, stretch))
        .map(|stretch| {
            let key = stretch_key(&timeline, &stretch, &options, format)?;
            let path = cache_dir.join(format.file_name(&key));
            Ok(PlannedStretch { stretch, key, path })
        })
        .collect::<Result<_, String>>()?;
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("cannot create {}: {e}", cache_dir.display()))?;
    let used: HashSet<&Path> = planned.iter().map(|plan| plan.path.as_path()).collect();
    delete_unused_renders(cache_dir, &used)?;

    // two stretches can share one key, such as one still placed twice
    let mut claimed = HashSet::new();
    let to_render: Vec<&PlannedStretch> = planned
        .iter()
        .filter(|plan| !plan.path.exists() && claimed.insert(plan.path.as_path()))
        .collect();
    let total_frames = to_render
        .iter()
        .map(|plan| plan.stretch.frames.end - plan.stretch.frames.start)
        .sum();
    let mut frames_before = 0;
    for plan in to_render {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }
        let mut on_frame = |frames_done: u64| {
            on_progress(PreviewRenderProgress {
                frames_rendered: frames_before + frames_done,
                total_frames,
            })
        };
        render_stretch(
            &timeline,
            plan,
            &options,
            cache_dir,
            cancel,
            format,
            &mut on_frame,
        )?;
        frames_before += plan.stretch.frames.end - plan.stretch.frames.start;
    }
    Ok(planned
        .into_iter()
        .map(|plan| RenderedStretch {
            frames: plan.stretch.frames,
            path: plan.path,
        })
        .collect())
}

fn preview_picture(picture: &PictureProcessing) -> PictureProcessing {
    PictureProcessing {
        fit: picture.fit.map(preview_fit),
        ..picture.clone()
    }
}

// the player shows the subtitles itself
fn preview_options(options: &EncodeRunOptions) -> EncodeRunOptions {
    EncodeRunOptions {
        picture: preview_picture(&options.picture),
        subtitle_burn: None,
        detect_picture_findings: false,
        ..options.clone()
    }
}

fn preview_timeline(timeline: &Timeline) -> Timeline {
    Timeline {
        clips: timeline
            .clips
            .iter()
            .map(|clip| Clip {
                picture_processing: preview_picture(&clip.picture_processing),
                ..clip.clone()
            })
            .collect(),
        ..timeline.clone()
    }
}

// bottom to top
fn stretch_clips(stretch: &TimelineStretch) -> impl Iterator<Item = usize> + '_ {
    let base = match stretch.base {
        StretchBase::Clip(index) => Some(index),
        StretchBase::Black { .. } => None,
    };
    base.into_iter().chain(stretch.overlays.iter().copied())
}

fn stretch_is_rendered(timeline: &Timeline, stretch: &TimelineStretch) -> bool {
    stretch_clips(stretch).any(|index| preview_renders_clip(&timeline.clips[index]))
}

#[derive(Serialize)]
struct StretchKey<'a> {
    render_format_version: u32,
    format: RenderedFormat,
    fps: FrameRate,
    frame_count: u64,
    black_raster: Option<(u32, u32)>,
    layers: Vec<LayerKey<'a>>,
    compression: Option<CompressionKey>,
}

#[derive(Serialize)]
struct LayerKey<'a> {
    picture: &'a Path,
    files: Vec<FileStamp>,
    first_frame: u64,
    layer: u32,
    opacity: f64,
    picture_processing: PictureProcessing,
    source_colour: SourceColour,
}

#[derive(Serialize)]
struct FileStamp {
    bytes: u64,
    modified_seconds: u64,
    modified_nanoseconds: u32,
}

#[derive(Serialize)]
struct CompressionKey {
    compression_ratio: f64,
    target_codestream_bytes: Option<u64>,
    quality_psnr: Option<f64>,
    rsiz: u16,
    codestream_byte_cap: Option<u64>,
}

fn stretch_key(
    timeline: &Timeline,
    stretch: &TimelineStretch,
    options: &EncodeRunOptions,
    format: RenderedFormat,
) -> Result<String, String> {
    let layers = stretch_clips(stretch)
        .map(|index| {
            let clip = &timeline.clips[index];
            let window = clip_window(clip, &stretch.frames);
            Ok(LayerKey {
                picture: &clip.picture,
                files: source_stamps(clip, window)?,
                first_frame: window.first_frame,
                layer: clip.layer,
                opacity: clip.opacity,
                picture_processing: effective_picture(clip, options),
                source_colour: effective_source_colour(clip, options),
            })
        })
        .collect::<Result<_, String>>()?;
    let black_raster = match stretch.base {
        StretchBase::Black { width, height } => Some((width, height)),
        StretchBase::Clip(_) => None,
    };
    let compression = match format {
        RenderedFormat::Mjpeg => None,
        RenderedFormat::Codestreams => Some(CompressionKey {
            compression_ratio: options.compression_ratio,
            target_codestream_bytes: options.target_codestream_bytes,
            quality_psnr: options.quality_psnr,
            rsiz: options.rsiz,
            codestream_byte_cap: options.codestream_byte_cap,
        }),
    };
    let key = StretchKey {
        render_format_version: RENDER_FORMAT_VERSION,
        format,
        fps: timeline.fps,
        frame_count: stretch.frames.end - stretch.frames.start,
        black_raster,
        layers,
        compression,
    };
    let serialised =
        serde_json::to_vec(&key).map_err(|e| format!("cannot serialise a preview key: {e}"))?;
    Ok(hex::encode(Sha256::digest(&serialised)))
}

fn source_stamps(clip: &Clip, window: FrameRange) -> Result<Vec<FileStamp>, String> {
    let sequence = matches!(
        crate::encode::detect_input_type(&clip.picture),
        InputType::ImageSequence
    );
    if crate::still::is_still_image(&clip.picture) || !sequence {
        return Ok(vec![file_stamp(&clip.picture)?]);
    }
    let images = crate::encode::find_source_frames(&clip.picture)
        .map_err(|e| format!("cannot list {}: {e}", clip.picture.display()))?;
    window
        .window_of(&images)?
        .iter()
        .map(|image| file_stamp(image))
        .collect()
}

fn file_stamp(path: &Path) -> Result<FileStamp, String> {
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let modified = metadata
        .modified()
        .map_err(|e| format!("cannot read when {} changed: {e}", path.display()))?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("{} changed before 1970: {e}", path.display()))?;
    Ok(FileStamp {
        bytes: metadata.len(),
        modified_seconds: modified.as_secs(),
        modified_nanoseconds: modified.subsec_nanos(),
    })
}

const KEY_HEX_DIGITS: usize = 64;

// a cache directory given by mistake keeps every file no render wrote
fn is_render_name(name: &str) -> bool {
    let key = name.split('.').next().unwrap_or_default();
    key.len() == KEY_HEX_DIGITS && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn delete_unused_renders(cache_dir: &Path, used: &HashSet<&Path>) -> Result<(), String> {
    let entries = std::fs::read_dir(cache_dir)
        .map_err(|e| format!("cannot list {}: {e}", cache_dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("cannot list {}: {e}", cache_dir.display()))?
            .path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !is_render_name(&name) || used.contains(path.as_path()) {
            continue;
        }
        remove_path(&path)?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<(), String> {
    let removed = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    removed.map_err(|e| format!("cannot remove {}: {e}", path.display()))
}

// a render that stops never leaves a file under the final name
fn render_stretch(
    timeline: &Timeline,
    plan: &PlannedStretch,
    options: &EncodeRunOptions,
    cache_dir: &Path,
    cancel: &Arc<AtomicBool>,
    format: RenderedFormat,
    on_frame: &mut dyn FnMut(u64),
) -> Result<(), String> {
    let work_dir = cache_dir.join(format!("{}.partial", plan.key));
    if work_dir.exists() {
        remove_path(&work_dir)?;
    }
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("cannot create {}: {e}", work_dir.display()))?;
    let rendered = match format {
        RenderedFormat::Mjpeg => write_mjpeg(
            timeline,
            &plan.stretch,
            options,
            &work_dir,
            cancel,
            on_frame,
        ),
        RenderedFormat::Codestreams => encode_codestreams(
            timeline,
            &plan.stretch,
            options,
            &work_dir,
            cancel,
            on_frame,
        ),
    }
    .and_then(|output| {
        std::fs::rename(&output, &plan.path)
            .map_err(|e| format!("cannot move {} into place: {e}", output.display()))
    });
    let removed = remove_path(&work_dir);
    rendered.and(removed)
}

fn stretch_options(
    timeline: &Timeline,
    stretch: &TimelineStretch,
    options: &EncodeRunOptions,
    j2k_dir: &Path,
) -> StreamEncodeOptions {
    StreamEncodeOptions {
        source_colour: stretch_source_colour(timeline, stretch, options),
        ..stream_encode_options(options, j2k_dir, 0)
    }
}

fn log_line(line: &str) {
    tracing::debug!("{line}");
}

fn stretch_name(stretch: &TimelineStretch) -> String {
    format!(
        "timeline frames {}..{}",
        stretch.frames.start, stretch.frames.end
    )
}

fn write_mjpeg(
    timeline: &Timeline,
    stretch: &TimelineStretch,
    options: &EncodeRunOptions,
    work_dir: &Path,
    cancel: &Arc<AtomicBool>,
    on_frame: &mut dyn FnMut(u64),
) -> Result<PathBuf, String> {
    let composite = stretch_composite(timeline, stretch, work_dir, options)?;
    let stream_options = stretch_options(timeline, stretch, options, &work_dir.join("j2k"));
    let params = compress_params(&stream_options)?;
    let frames = composite_frames(&composite, &stream_options, &params, &log_line)?;
    let EncoderInputFormat::PackedRgb(order) = frames.input_format else {
        unreachable!("a composite is always packed RGB")
    };
    let output = work_dir.join("stretch.mkv");
    let mut ffmpeg = start_mjpeg_writer(&output, frames.width, frames.height, order, timeline.fps)?;
    let mut buffer = vec![0; frames.input_format.frame_bytes(frames.width, frames.height)];
    let mut source = frames.source;
    let mut frames_written = 0;
    let written = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err(CANCELLED.to_string());
        }
        if let NextFrame::Ended = source.read_into(&mut buffer) {
            break Ok(());
        }
        let stdin = ffmpeg.stdin.as_mut().expect("the writer's stdin is piped");
        if let Err(e) = stdin.write_all(&buffer) {
            break Err(format!("cannot hand ffmpeg a frame: {e}"));
        }
        frames_written += 1;
        on_frame(frames_written);
    };
    let decoded = match source.finish(timeline.fps.as_f64(), frames_written).failure {
        Some(failure) => Err(failure),
        None => Ok(()),
    };
    if let Err(error) = written.and(decoded) {
        // the decode or write error already says what went wrong
        let _ = ffmpeg.kill();
        let _ = ffmpeg.wait();
        return Err(if error == CANCELLED {
            error
        } else {
            format!("{}: {error}", stretch_name(stretch))
        });
    }
    finish_mjpeg_writer(ffmpeg, &output)?;
    Ok(output)
}

// mpv and ffmpeg read every JPEG as BT.601 full range, whatever the container says
fn start_mjpeg_writer(
    output: &Path,
    width: u32,
    height: u32,
    order: SampleOrder,
    fps: FrameRate,
) -> Result<Child, String> {
    let pixel_format = match order {
        SampleOrder::Big => "rgb48be",
        SampleOrder::Little => "rgb48le",
    };
    Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            pixel_format,
        ])
        .args(["-s", &format!("{width}x{height}")])
        .args([
            "-framerate",
            &format!("{}/{}", fps.numerator, fps.denominator),
        ])
        .args(["-i", "-"])
        .args([
            "-vf",
            "scale=out_color_matrix=bt601:out_range=full,format=yuvj444p",
        ])
        .args(["-c:v", "mjpeg", "-q:v", "2"])
        .args(["-color_range", "pc", "-colorspace", "bt470bg"])
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run ffmpeg: {e}"))
}

fn finish_mjpeg_writer(mut ffmpeg: Child, output: &Path) -> Result<(), String> {
    drop(ffmpeg.stdin.take());
    let finished = ffmpeg
        .wait_with_output()
        .map_err(|e| format!("cannot wait for ffmpeg: {e}"))?;
    if !finished.status.success() {
        return Err(format!(
            "ffmpeg cannot write the preview {}: {}",
            output.display(),
            String::from_utf8_lossy(&finished.stderr).trim()
        ));
    }
    Ok(())
}

fn encode_codestreams(
    timeline: &Timeline,
    stretch: &TimelineStretch,
    options: &EncodeRunOptions,
    work_dir: &Path,
    cancel: &Arc<AtomicBool>,
    on_frame: &mut dyn FnMut(u64),
) -> Result<PathBuf, String> {
    let composite: Composite = stretch_composite(timeline, stretch, work_dir, options)?;
    let j2k_dir = work_dir.join("j2k");
    let stream_options = stretch_options(timeline, stretch, options, &j2k_dir);
    let never_paused = Arc::new(AtomicBool::new(false));
    let result = encode_composite(
        &stream_options,
        &composite,
        cancel,
        &never_paused,
        None,
        |progress: StreamProgress| on_frame(progress.frame),
        log_line,
    );
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.to_string());
    }
    if !result.success {
        return Err(format!("{}: {}", stretch_name(stretch), result.error));
    }
    Ok(j2k_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fit(raster_width: u32, raster_height: u32, offset_x: i32, offset_y: i32) -> Fit {
        Fit {
            box_width: raster_width,
            box_height: raster_height,
            raster_width,
            raster_height,
            placement: Placement {
                scale_percent: 90.0,
                offset_x,
                offset_y,
            },
        }
    }

    #[test]
    fn a_fit_wider_than_2048_halves_its_box_raster_and_offsets() {
        let scope_in_flat = Fit {
            box_width: 3996,
            box_height: 1672,
            ..fit(3996, 2160, 40, -20)
        };
        assert_eq!(
            preview_fit(scope_in_flat),
            Fit {
                box_width: 1998,
                box_height: 836,
                ..fit(1998, 1080, 20, -10)
            }
        );
        assert_eq!(preview_fit(fit(4096, 2160, 0, 0)), fit(2048, 1080, 0, 0));
        assert_eq!(preview_fit(fit(2048, 858, 7, -7)), fit(2048, 858, 7, -7));
    }

    #[test]
    fn an_odd_offset_halves_toward_the_centre() {
        assert_eq!(preview_fit(fit(4096, 1716, 3, -3)), fit(2048, 858, 1, -1));
        assert_eq!(preview_fit(fit(4096, 1716, 1, -1)), fit(2048, 858, 0, 0));
    }

    #[test]
    fn only_a_key_named_entry_counts_as_a_render() {
        let key = "0123456789abcdef".repeat(4);
        assert!(is_render_name(&key));
        assert!(is_render_name(&format!("{key}.mkv")));
        assert!(is_render_name(&format!("{key}.partial")));
        assert!(!is_render_name("sound.wav"));
        assert!(!is_render_name(&format!("{}.mkv", &key[1..])));
        assert!(!is_render_name(&format!("{}g.mkv", &key[1..])));
    }
}
