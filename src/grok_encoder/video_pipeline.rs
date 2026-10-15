use super::{
    CompressParams, EncodePhase, EncodeProgress, PhaseClocks, PipelineResult,
    contiguous_encoded_frames, encode_pipeline_with_mxf_feed, gpu_active,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// High-performance video-to-J2K pipeline: decodes video with the FFmpeg
/// libraries and encodes each frame in-process via Grok FFI. No intermediate
/// files on disk.
///
/// Pipeline: decode and filter in process to raw frame buffers
///           → bounded queue → N encoder threads (1-thread Grok per frame)
///           → writer thread → .j2c files on disk
#[allow(clippy::too_many_arguments)]
pub fn encode_video_pipeline<P>(
    input_video: &Path,
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    width: u32,
    height: u32,
    cancel: &Arc<AtomicBool>,
    on_progress: P,
) -> PipelineResult
where
    P: FnMut(EncodeProgress),
{
    encode_video_pipeline_resumable(
        input_video,
        output_dir,
        params,
        total_frames,
        width,
        height,
        cancel,
        false,
        None,
        None,
        on_progress,
    )
}

/// The `-vf` chain for a decode that may be windowed. The caller's filters run
/// at source timing and the window is cut out of what they produced, so a fade
/// lands where a full encode would have put it and frame N of the window is the
/// frame a full encode would have written at `first_frame + N`.
fn window_filter_chain(
    video_filter: Option<&str>,
    frame_range: Option<crate::encode::FrameRange>,
) -> Option<String> {
    let Some(range) = frame_range else {
        return video_filter.map(str::to_string);
    };
    let mut chain: Vec<String> = video_filter.into_iter().map(str::to_string).collect();
    chain.extend(range.trim_filters());
    Some(chain.join(","))
}

/// Like [`encode_video_pipeline`], but when `resume` is true it skips frames
/// already encoded on disk (dom#344: an interrupted encode picks up where it
/// left off). The already-present contiguous prefix is decoded-and-discarded so
/// the decode stays frame-aligned, then encoding continues from the next index. The
/// last existing frame is always re-encoded in case it was truncated by the
/// interruption.
///
/// `frame_range` encodes one window of the source instead of all of it. With a
/// window, `total_frames` is the window's length rather than the source's, the
/// codestreams are numbered from zero, and `resume` counts inside the window.
#[allow(clippy::too_many_arguments)]
pub fn encode_video_pipeline_resumable<P>(
    input_video: &Path,
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    width: u32,
    height: u32,
    cancel: &Arc<AtomicBool>,
    resume: bool,
    video_filter: Option<&str>,
    frame_range: Option<crate::encode::FrameRange>,
    on_progress: P,
) -> PipelineResult
where
    P: FnMut(EncodeProgress),
{
    encode_video_pipeline_resumable_with_mxf_feed(
        input_video,
        output_dir,
        params,
        total_frames,
        width,
        height,
        &crate::encode::SourceColour::DisplayRgb,
        cancel,
        resume,
        video_filter,
        frame_range,
        None,
        on_progress,
        |_| {},
    )
}

/// Like [`encode_video_pipeline_resumable`], but the writer thread also hands
/// each codestream to `mxf_feed` once it is on disk, so a picture MXF can be
/// written while the encode runs. A resumed run hands the feed only the frames
/// it encodes, so a caller that resumes has no complete wrap to feed.
#[allow(clippy::too_many_arguments)]
pub fn encode_video_pipeline_resumable_with_mxf_feed<P>(
    input_video: &Path,
    output_dir: &Path,
    params: &CompressParams,
    total_frames: u64,
    width: u32,
    height: u32,
    // decides the decode's own colour step, the transform itself rides in params
    source_colour: &crate::encode::SourceColour,
    cancel: &Arc<AtomicBool>,
    resume: bool,
    // filter chain applied while decoding, for fades and the like. It must
    // not change the frame size or count: the reader copies fixed
    // width*height frames and the CPL already declares the count.
    video_filter: Option<&str>,
    frame_range: Option<crate::encode::FrameRange>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    mut on_progress: P,
    on_log: impl Fn(&str),
) -> PipelineResult
where
    P: FnMut(EncodeProgress),
{
    let failed = |error: String| PipelineResult {
        success: false,
        error,
        frames_encoded: 0,
        output_dir: output_dir.to_path_buf(),
        picture_findings: crate::picture_findings::PictureFindings::default(),
    };
    let probed = match crate::ffmpeg_decode::probe(input_video, crate::encode::DecodeSource::Video)
    {
        Ok(probed) => probed,
        Err(e) => return failed(e),
    };
    let source = probed.pixel_format;
    if let Some(range) = frame_range
        && let Err(e) = range.check_against_probe(probed.frame_count)
    {
        return failed(e);
    }

    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return failed(format!("Failed to create output directory: {e}"));
    }

    // resume: re-encode from the last-but-one existing frame (the last is
    // suspect: fs::write is not atomic, so an interrupt can truncate it).
    let start_frame = if resume {
        contiguous_encoded_frames(output_dir).saturating_sub(1)
    } else {
        0
    };
    // everything already done: nothing to encode.
    if resume && total_frames > 0 && start_frame >= total_frames {
        return PipelineResult {
            success: true,
            ..failed(String::new())
        };
    }

    let picture_filters = window_filter_chain(video_filter, frame_range).unwrap_or_default();
    let accelerator_active = gpu_active();
    let chain = match crate::encode::stream_encode::decode_chain(
        &crate::encode::stream_encode::DecodeChainInputs {
            decode_source: crate::encode::DecodeSource::Video,
            read_source_at: None,
            picture: crate::encode::PictureFilters::Given(&picture_filters),
            source_colour,
            source: &source,
            accelerator_active,
            quality_psnr: params.quality_psnr,
            postkit_prepares_the_frame: !params.source_preparation.is_empty(),
            detect_picture_findings: params.detect_picture_findings,
        },
        width,
        height,
        params,
        &on_log,
    ) {
        Ok(chain) => chain,
        Err(e) => return failed(e),
    };
    let input_format = chain.input_format;
    let frame_size = input_format.frame_bytes(width, height);
    let mut reader = match crate::encode::stream_encode::start_frame_decoder(
        &chain,
        input_video,
        frame_range,
        width,
        height,
    ) {
        Ok(reader) => reader,
        Err(e) => return failed(e),
    };

    // discard the already-encoded prefix so the decode stays frame-aligned.
    let mut frame_index: u64 = 0;
    if start_frame > 0 {
        if reader.skip(start_frame).is_err() {
            let _ = reader.finish(params.edit_rate.as_f64(), 0);
            return failed(format!(
                "resume: source has fewer than {start_frame} frames, cannot skip the encoded prefix"
            ));
        }
        frame_index = start_frame;
    }

    // progress total is the remaining frames so the pipeline's completion check
    // (done >= total) is reachable after a resume.
    let remaining_total = total_frames.saturating_sub(start_frame);
    let phase_clocks = Arc::new(PhaseClocks::default());
    let result = encode_pipeline_with_mxf_feed(
        output_dir,
        params,
        remaining_total,
        cancel,
        &phase_clocks,
        mxf_feed,
        None,
        |buffer_pool| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut buf = buffer_pool.take(frame_size);
            let read_start = std::time::Instant::now();
            let read = reader.read_into(&mut buf);
            phase_clocks.add(EncodePhase::DecoderWait, read_start.elapsed());
            if let crate::ffmpeg_decode::NextFrame::Ended = read {
                return None;
            }
            let index = frame_index;
            frame_index += 1;
            Some(input_format.raw_frame(buf, width, height, index))
        },
        &mut on_progress,
    );

    // the detection timestamps are on the decode's whole output, which after a
    // resume is longer than the frames this run encoded
    let decode = reader.finish(params.edit_rate.as_f64(), frame_index);

    let expected_frames = total_frames.saturating_sub(start_frame);
    let mut success = result.success;
    let mut error = result.error;
    if success
        && let Some(failure) = crate::encode::stream_encode::decode_failure(
            &decode,
            result.frames_encoded,
            expected_frames,
        )
    {
        success = false;
        error = failure;
    }
    PipelineResult {
        success,
        error,
        picture_findings: decode.findings,
        ..result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window is cut after the caller's filters, so a fade keeps the timing
    /// a full encode would have given it.
    #[test]
    fn a_frame_range_trims_the_decode_after_the_callers_filters() {
        let window = crate::encode::FrameRange {
            first_frame: 10,
            frame_count: 5,
        };

        assert_eq!(window_filter_chain(None, None), None);
        assert_eq!(
            window_filter_chain(Some("crop=1920:804:0:138"), None).as_deref(),
            Some("crop=1920:804:0:138"),
            "no window has to leave the caller's chain as it was"
        );
        assert_eq!(
            window_filter_chain(None, Some(window)).as_deref(),
            Some("trim=start_frame=10:end_frame=15,setpts=PTS-STARTPTS")
        );
        assert_eq!(
            window_filter_chain(Some("crop=1920:804:0:138,fade=out:96:24"), Some(window))
                .as_deref(),
            Some(
                "crop=1920:804:0:138,fade=out:96:24,\
                 trim=start_frame=10:end_frame=15,setpts=PTS-STARTPTS"
            )
        );
    }
}
