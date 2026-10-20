use super::{
    DecodeSource, EncodeResult, EncoderInputFormat, FrameRange, FrameRate, PACKED_RGB_PRECISION,
    PictureFilters, PlanarYuvPixelFormat, SIXTEEN_BIT_OVER_BLACK_FILTER, SourceColour,
    StreamEncodeOptions, StreamProgress, YuvFrameFormat, YuvMatrix, compress_params,
    packed_rgb_sample_order,
};
use crate::ffmpeg_decode::{FrameSource, NextFrame};
use crate::grok_encoder::SampleOrder;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

impl FrameRange {
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

/// The `-vf` items of one decode and the slots postkit's own filters go in.
struct DecodeFilters {
    /// the items in order, with no pixel format filter of postkit's own
    items: Vec<String>,
    /// where a pixel format filter goes for a frame that needs the geometry run
    /// in another format, `None` when the plan has no geometry
    geometry_format_position: Option<usize>,
}

impl DecodeFilters {
    /// The chain as ffmpeg reads it, with no pixel format filter of postkit's
    /// own.
    fn joined(&self) -> String {
        self.items.join(",")
    }

    /// The chain with a conversion to 16-bit RGB at `position`.
    fn with_format_filter_at(&self, position: usize, filter: &str) -> String {
        let mut items = self.items.clone();
        items.insert(position, filter.to_string());
        items.join(",")
    }
}

/// The ffmpeg filter chain for a stream decode: the caller's own chain, or the
/// picture plan with the output frame rate at the position the plan names and
/// the frame window right after that rate, plus the HDR-to-DCI LUT last when the
/// source needs one, so the LUT sees the finished picture.
fn decode_filters(picture: &PictureFilters, source_colour: &SourceColour) -> DecodeFilters {
    let mut filters = match picture {
        PictureFilters::Given(given) => DecodeFilters {
            items: if given.is_empty() {
                Vec::new()
            } else {
                vec![(*given).to_string()]
            },
            geometry_format_position: None,
        },
        PictureFilters::Planned {
            plan,
            fps,
            frame_range,
            extra_filter,
        } => planned_filters(plan, *fps, *frame_range, *extra_filter),
    };
    if let Some(lut) = source_colour.decode_lut() {
        filters.items.push(format!(
            "lut3d={}",
            crate::burnin::filter_argument(&lut.to_string_lossy())
        ));
    }
    filters
}

fn planned_filters(
    plan: &crate::picture_processing::PicturePlan,
    fps: FrameRate,
    frame_range: Option<FrameRange>,
    extra_filter: Option<&str>,
) -> DecodeFilters {
    let mut items = plan.filters.clone();
    items.insert(
        plan.fps_position,
        format!("fps={}", fps.ffmpeg_filter_value()),
    );
    let mut inserted = 1;
    // the window is counted in output frames, not source frames
    if let Some(range) = frame_range {
        let after_fps = plan.fps_position + 1;
        let trims = range.trim_filters();
        inserted += trims.len();
        items.splice(after_fps..after_fps, trims);
    }
    let geometry_position = plan.geometry_format_position + inserted;
    items.extend(extra_filter.map(str::to_string));
    DecodeFilters {
        items,
        geometry_format_position: plan.changes_geometry.then_some(geometry_position),
    }
}

/// The filter that converts an 8-bit YUV source to 16-bit planar RGB before
/// anything else reads it. swscale's own conversion from 8-bit YUV to rgb48
/// runs at 8 bits and leaves every sample about two codes of 255 off the exact
/// value.
const SIXTEEN_BIT_RGB_FILTER: &str = "format=gbrp16le";
const SIXTEEN_BIT_RGBA_FILTER: &str = "format=gbrap16le";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlphaChannel {
    Dropped,
    Kept,
}

impl AlphaChannel {
    // the encoded picture carries no alpha, so a bottom picture with alpha goes over black
    fn composites_over_black(self, source: &crate::probe::PixelFormatInfo) -> bool {
        self == AlphaChannel::Dropped && source.has_alpha()
    }

    fn sixteen_bit_filter(self, source: &crate::probe::PixelFormatInfo) -> &'static str {
        if self.composites_over_black(source) {
            return SIXTEEN_BIT_OVER_BLACK_FILTER;
        }
        match self {
            AlphaChannel::Dropped => SIXTEEN_BIT_RGB_FILTER,
            AlphaChannel::Kept => SIXTEEN_BIT_RGBA_FILTER,
        }
    }
}

/// ffmpeg filters that change the pixel format or the colour, so the decoded
/// picture is no longer the source's own planes. `format=` is how a caller's
/// own chain spells a conversion: postkit's own pixel format filters are added
/// after the encoder input format is decided. Geometry filters are not here:
/// cropping, scaling, padding, rotating, deinterlacing and denoising all keep the
/// pixel format they were given.
const COLOUR_CHANGING_FILTERS: [&str; 10] = [
    "format=",
    "lut3d",
    "haldclut",
    "colorspace",
    "zscale",
    "tonemap",
    "colorchannelmixer",
    "colorlevels",
    "curves",
    "eq=",
];

/// Whether a filter chain converts the picture away from the source's own
/// planes.
pub(crate) fn filters_change_the_colour(filters: &str) -> bool {
    COLOUR_CHANGING_FILTERS
        .iter()
        .any(|filter| filters.contains(filter))
}

/// ffprobe's `color_range` tag for samples that use the whole code value range.
const FULL_RANGE_TAG: &str = "pc";

/// Every 8-bit YUV pixel format ffmpeg decodes to, planar and semi-planar. A
/// format with a depth suffix takes swscale's high depth path and converts to
/// rgb48 exactly, and so does RGB of any depth.
const EIGHT_BIT_YUV_PIXEL_FORMATS: [&str; 19] = [
    "yuv410p", "yuv411p", "yuvj411p", "yuv420p", "yuvj420p", "yuv422p", "yuvj422p", "yuv440p",
    "yuvj440p", "yuv444p", "yuvj444p", "yuva420p", "yuva422p", "yuva444p", "nv12", "nv21", "nv16",
    "nv24", "nv42",
];

/// Whether ffprobe read the source as 8-bit YUV.
pub(crate) fn is_eight_bit_yuv_pixel_format(pix_fmt: &str) -> bool {
    EIGHT_BIT_YUV_PIXEL_FORMATS.contains(&pix_fmt)
}

impl EncoderInputFormat {
    // one decoded frame's bytes, handed to the compressor as they were read
    pub(crate) fn raw_frame(
        &self,
        data: Vec<u8>,
        width: u32,
        height: u32,
        index: u64,
    ) -> crate::grok_encoder::RawFrame {
        match *self {
            EncoderInputFormat::PackedRgb(order) => crate::grok_encoder::RawFrame::Packed {
                data,
                order,
                width,
                height,
                precision: PACKED_RGB_PRECISION,
                index,
            },
            EncoderInputFormat::PlanarYuv(format) => crate::grok_encoder::RawFrame::PlanarYuv {
                data,
                format,
                width,
                height,
                index,
            },
        }
    }
}

/// Everything [`choose_encoder_input_format`] reads, from the run's options and from
/// ffprobe.
pub(crate) struct FormatChoiceInputs<'a> {
    /// grok's accelerator plugin is switched on, so the encode runs as a batch
    pub accelerator_active: bool,
    pub quality_psnr: Option<f64>,
    /// postkit burns subtitles into the frame or converts its colour itself,
    /// and both need samples it can read
    pub postkit_prepares_the_frame: bool,
    pub source_colour: &'a SourceColour,
    /// the whole filter chain ffmpeg is given, detection branch included
    pub filters: &'a str,
    pub source: &'a crate::probe::PixelFormatInfo,
}

/// Whether the decode hands over the source's own planar YUV or the packed RGB
/// postkit converts itself.
///
/// Planar YUV only reaches grok's plugin untouched, so anything that reads or
/// rewrites the samples on the way keeps the run on packed RGB:
/// a subtitle burn, postkit's own colour transform, the decode LUT, and any
/// geometry work, which ffmpeg does in its own pixel format. A PSNR target
/// re-encodes a frame that overshoots the byte cap, which a batch cannot do, so
/// that run never reaches the plugin either.
pub(crate) fn choose_encoder_input_format(
    inputs: &FormatChoiceInputs,
    plugin_takes_planar_yuv: bool,
) -> EncoderInputFormat {
    let postkit_reads_the_samples = inputs.postkit_prepares_the_frame
        || matches!(
            inputs.source_colour,
            SourceColour::DisplayRgbIn(_)
                | SourceColour::KeepRgbFrom(_)
                | SourceColour::HdrDcdm { .. }
        )
        || inputs.source_colour.decode_lut().is_some();
    let Some(pixel_format) = PlanarYuvPixelFormat::from_ffmpeg_name(&inputs.source.pix_fmt) else {
        return EncoderInputFormat::PackedRgb(SampleOrder::Big);
    };
    if !inputs.accelerator_active
        || !plugin_takes_planar_yuv
        || inputs.quality_psnr.is_some()
        || postkit_reads_the_samples
        || filters_change_the_colour(inputs.filters)
    {
        return EncoderInputFormat::PackedRgb(SampleOrder::Big);
    }
    EncoderInputFormat::PlanarYuv(YuvFrameFormat {
        pixel_format,
        matrix: YuvMatrix::for_ffprobe_color_space(&inputs.source.color_space),
        full_range: inputs.source.color_range == FULL_RANGE_TAG,
    })
}

/// The pixel format one run's decode writes: what [`choose_encoder_input_format`] allows,
/// then grok's accelerator plugin asked about the frames that source would send
/// it.
///
/// The plugin is asked last, and only about a source everything else already
/// allows through, because each ask starts a batch. A run asks at most twice: a
/// declined YUV source falls back to packed RGB, which is asked about in turn.
pub(crate) fn encoder_input_format_for_run(
    inputs: &FormatChoiceInputs,
    width: u32,
    height: u32,
    params: &crate::grok_encoder::CompressParams,
) -> EncoderInputFormat {
    if let EncoderInputFormat::PlanarYuv(format) = choose_encoder_input_format(inputs, true) {
        let shape_only = crate::grok_encoder::RawFrame::PlanarYuv {
            data: Vec::new(),
            format,
            width,
            height,
            index: 0,
        };
        if crate::grok_encoder::plugin_takes_frame(&shape_only, params) {
            return EncoderInputFormat::PlanarYuv(format);
        }
    }
    EncoderInputFormat::PackedRgb(packed_rgb_sample_order(width, height, params))
}

/// The whole filter chain one decode runs: the picture filters with the
/// detectors split off them, and the pixel format filter a packed RGB frame
/// needs where it belongs.
///
/// An 8-bit YUV source converts at the head, which covers the geometry too, so
/// the geometry work, the decode LUT and the packing to rgb48 all run at 16
/// bits: swscale converts 8-bit YUV to rgb48 at 8 bits and leaves every sample
/// about two codes of 255 off, and `lut3d` and `haldclut` take RGB, which ffmpeg
/// would otherwise hand them at 8 bits. Any other source converts where the
/// geometry starts, because a subsampled source rounds a crop offset to the
/// chroma grid and mixes pad black with neighbouring chroma. A planar YUV frame
/// carries the source's own planes to grok's plugin, so it takes no pixel
/// format filter at all and the geometry runs on the planes.
fn decode_filter_chain(
    picture: &DecodeFilters,
    input_format: EncoderInputFormat,
    alpha: AlphaChannel,
    source: &crate::probe::PixelFormatInfo,
    source_colour: &SourceColour,
) -> String {
    let hdr = hdr_yuv_to_rgb_filter(source, source_colour, alpha);
    let filter = hdr.as_deref().unwrap_or(alpha.sixteen_bit_filter(source));
    match input_format {
        EncoderInputFormat::PlanarYuv(_) => picture.joined(),
        EncoderInputFormat::PackedRgb(_)
            if hdr.is_some()
                || alpha.composites_over_black(source)
                || is_eight_bit_yuv_pixel_format(&source.pix_fmt) =>
        {
            picture.with_format_filter_at(0, filter)
        }
        EncoderInputFormat::PackedRgb(_) => match picture.geometry_format_position {
            Some(position) => picture.with_format_filter_at(position, filter),
            None => picture.joined(),
        },
    }
}

// ffmpeg refuses an empty -vf
const PASSTHROUGH_FILTER: &str = "null";

fn passthrough_if_empty(picture_chain: String) -> String {
    if picture_chain.is_empty() {
        return PASSTHROUGH_FILTER.to_string();
    }
    picture_chain
}

fn with_optional_detection_branch(picture_chain: String, detect_picture_findings: bool) -> String {
    let picture_chain = passthrough_if_empty(picture_chain);
    if !detect_picture_findings {
        return picture_chain;
    }
    crate::picture_findings::detection_graph::with_detection_branch(&picture_chain)
}

const YUV_PIXEL_FORMAT_PREFIXES: [&str; 4] = ["yuv", "nv", "p0", "p2"];

fn is_yuv_pixel_format(pix_fmt: &str) -> bool {
    YUV_PIXEL_FORMAT_PREFIXES
        .iter()
        .any(|prefix| pix_fmt.starts_with(prefix))
}

// an hdr grade is never bt.601, so an untagged one takes its own matrix
fn hdr_yuv_matrix(hdr_source: crate::colour::HdrSource, color_space: &str) -> YuvMatrix {
    let tagged = YuvMatrix::for_ffprobe_color_space(color_space);
    if tagged != YuvMatrix::Bt601 {
        return tagged;
    }
    match hdr_source {
        crate::colour::HdrSource::Hdr10 | crate::colour::HdrSource::Hlg => YuvMatrix::Bt2020,
        crate::colour::HdrSource::PqP3D65 => YuvMatrix::Bt709,
    }
}

fn zscale_matrix_name(matrix: YuvMatrix) -> &'static str {
    match matrix {
        YuvMatrix::Bt601 => "170m",
        YuvMatrix::Bt709 => "709",
        YuvMatrix::Bt2020 => "2020_ncl",
    }
}

// zscale spells rgb output "gbr"
const ZSCALE_RGB_MATRIX: &str = "gbr";

// swscale infers the matrix, which lands a bt.2020 grade on bt.601 or bt.709
fn hdr_yuv_to_rgb_filter(
    source: &crate::probe::PixelFormatInfo,
    source_colour: &SourceColour,
    alpha: AlphaChannel,
) -> Option<String> {
    let hdr_source = source_colour.hdr_source()?;
    if !is_yuv_pixel_format(&source.pix_fmt) {
        return None;
    }
    let range = if source.color_range == FULL_RANGE_TAG {
        "full"
    } else {
        "limited"
    };
    let matrix = zscale_matrix_name(hdr_yuv_matrix(hdr_source, &source.color_space));
    Some(format!(
        "zscale=matrixin={matrix}:rangein={range}:matrix={ZSCALE_RGB_MATRIX}:range=full,{}",
        alpha.sixteen_bit_filter(source)
    ))
}

fn check_read_rate(
    decode_source: DecodeSource,
    read_source_at: Option<FrameRate>,
) -> Result<(), String> {
    match (decode_source, read_source_at) {
        (DecodeSource::ImageList, Some(rate)) => Err(format!(
            "a concat list of stills already holds each frame for one period, so it cannot also \
             be read at {} fps",
            rate.ffmpeg_filter_value()
        )),
        _ => Ok(()),
    }
}

// ─── the decode chain one run takes ───────────────────────────────────────────

/// Everything one decode's pixel format and filter chain are decided from.
pub(crate) struct DecodeChainInputs<'a> {
    pub decode_source: DecodeSource,
    pub read_source_at: Option<FrameRate>,
    pub picture: PictureFilters<'a>,
    pub source_colour: &'a SourceColour,
    pub source: &'a crate::probe::PixelFormatInfo,
    /// grok's accelerator plugin is switched on, so the encode runs as a batch
    pub accelerator_active: bool,
    pub quality_psnr: Option<f64>,
    /// postkit burns subtitles into the frame or converts its colour itself,
    /// and both need samples it can read
    pub postkit_prepares_the_frame: bool,
    pub detect_picture_findings: bool,
    pub alpha: AlphaChannel,
}

/// What one decode runs.
pub(crate) struct DecodeChain {
    pub input_format: EncoderInputFormat,
    pub alpha: AlphaChannel,
    // the chain without the detection branch, which the graph adds itself
    pub picture_filters: String,
    pub decode_source: DecodeSource,
    pub read_source_at: Option<FrameRate>,
    pub hardware_decode: bool,
    pub detect_picture_findings: bool,
}

/// Everything one stream decode runs: the source, the pixel format it writes
/// and the whole filter chain. Both the stream encode and
/// the resumable video encode decide here, so neither can drift from the other.
pub(crate) fn decode_chain(
    inputs: &DecodeChainInputs,
    width: u32,
    height: u32,
    params: &crate::grok_encoder::CompressParams,
    on_log: &dyn Fn(&str),
) -> Result<DecodeChain, String> {
    if let Some(lut) = inputs.source_colour.decode_lut() {
        if !lut.is_file() {
            return Err(format!("decode LUT not found: {}", lut.display()));
        }
        if std::fs::metadata(lut).is_ok_and(|metadata| metadata.len() == 0) {
            return Err(format!("the decode LUT {} is empty", lut.display()));
        }
    }
    check_read_rate(inputs.decode_source, inputs.read_source_at)?;
    let picture = decode_filters(&inputs.picture, inputs.source_colour);
    let input_format = encoder_input_format_for_run(
        // the input format is decided on the chain the caller asked for, so the
        // pixel format filter that chain gets afterwards cannot move the choice
        &FormatChoiceInputs {
            accelerator_active: inputs.accelerator_active,
            quality_psnr: inputs.quality_psnr,
            // a composite reads every layer's samples, so a layer with alpha is never planar YUV
            postkit_prepares_the_frame: inputs.postkit_prepares_the_frame
                || inputs.alpha == AlphaChannel::Kept,
            source_colour: inputs.source_colour,
            filters: &with_optional_detection_branch(
                picture.joined(),
                inputs.detect_picture_findings,
            ),
            source: inputs.source,
        },
        width,
        height,
        params,
    );
    let pixel_format = input_format.ffmpeg_pixel_format();
    tracing::info!(
        pixel_format,
        hardware_decode = inputs.accelerator_active,
        "decoding for the encoder"
    );
    on_log(&format!(
        "[ENCODE] decoding to pixel_format={pixel_format} hardware_decode={}",
        inputs.accelerator_active
    ));
    let picture_filters = passthrough_if_empty(decode_filter_chain(
        &picture,
        input_format,
        inputs.alpha,
        inputs.source,
        inputs.source_colour,
    ));
    Ok(DecodeChain {
        input_format,
        alpha: inputs.alpha,
        picture_filters,
        decode_source: inputs.decode_source,
        read_source_at: inputs.read_source_at,
        hardware_decode: inputs.accelerator_active,
        detect_picture_findings: inputs.detect_picture_findings,
    })
}

// ─── In-process stream encode (video → FFmpeg libraries → Grok FFI) ─────────

/// Stream-encode a video file to J2K using in-process Grok FFI.
///
/// Decodes the video in process with the FFmpeg libraries, then compresses
/// each frame in-process via the bounded-queue pipeline in `grok_encoder`.
pub fn stream_encode_inprocess<F>(
    opts: &StreamEncodeOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    on_progress: F,
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    stream_encode_inprocess_with_mxf_feed(opts, cancel, pause, None, on_progress, |_| {})
}

/// Like [`stream_encode_inprocess`], but each codestream also goes to `mxf_feed`
/// as it is written, so a picture MXF can be wrapped while the encode runs. See
/// [`crate::grok_encoder::encode_pipeline_with_mxf_feed`].
pub fn stream_encode_inprocess_with_mxf_feed<F>(
    opts: &StreamEncodeOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    on_progress: F,
    on_log: impl Fn(&str),
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    match decoded_frames(opts, &on_log) {
        Ok((params, frames)) => {
            encode_frames(opts, &params, frames, cancel, pause, mxf_feed, on_progress)
        }
        Err(error) => EncodeResult {
            success: false,
            error,
            ..Default::default()
        },
    }
}

pub(crate) struct FramesToEncode<S> {
    pub source: S,
    pub input_format: EncoderInputFormat,
    pub width: u32,
    pub height: u32,
    pub total_frames: u64,
}

fn decoded_frames(
    opts: &StreamEncodeOptions,
    on_log: &dyn Fn(&str),
) -> Result<
    (
        crate::grok_encoder::CompressParams,
        FramesToEncode<crate::ffmpeg_decode::FrameDecoder>,
    ),
    String,
> {
    let probed = crate::ffmpeg_decode::probe(&opts.input, opts.decode_source)?;
    let (source_width, source_height, source_frames) =
        (probed.width, probed.height, probed.frame_count);
    if source_width == 0 || source_height == 0 {
        return Err("Could not determine video dimensions".to_string());
    }
    if let Some(range) = opts.frame_range {
        range.check_against_probe(source_frames)?;
    }
    let total_frames = opts
        .frame_range
        .map_or(source_frames, |range| range.frame_count);
    let plan = opts.picture.plan(source_width, source_height)?;
    tracing::info!("picture: {}", plan.describe());
    let (width, height) = (plan.output_width, plan.output_height);

    std::fs::create_dir_all(&opts.output_dir)
        .map_err(|e| format!("Failed to create output directory: {e}"))?;

    let params = compress_params(opts)?;
    let chain = decode_chain(
        &DecodeChainInputs {
            decode_source: opts.decode_source,
            read_source_at: opts.read_source_at,
            picture: PictureFilters::Planned {
                plan: &plan,
                fps: opts.fps,
                frame_range: opts.frame_range,
                extra_filter: opts.extra_picture_filter.as_deref(),
            },
            source_colour: &opts.source_colour,
            source: &probed.pixel_format,
            accelerator_active: crate::grok_encoder::plugin_enabled_for(&params),
            quality_psnr: opts.quality_psnr,
            postkit_prepares_the_frame: opts.subtitle_burn.is_some() || opts.watermark.is_some(),
            detect_picture_findings: opts.detect_picture_findings,
            alpha: AlphaChannel::Dropped,
        },
        width,
        height,
        &params,
        on_log,
    )?;
    let source = start_frame_decoder(&chain, &opts.input, opts.frame_range, width, height)?;
    Ok((
        params,
        FramesToEncode {
            source,
            input_format: chain.input_format,
            width,
            height,
            total_frames,
        },
    ))
}

pub(crate) fn encode_frames<S, F>(
    opts: &StreamEncodeOptions,
    params: &crate::grok_encoder::CompressParams,
    frames: FramesToEncode<S>,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    mut on_progress: F,
) -> EncodeResult
where
    S: FrameSource,
    F: FnMut(StreamProgress),
{
    use crate::grok_encoder;

    let FramesToEncode {
        mut source,
        input_format,
        width,
        height,
        total_frames,
    } = frames;
    let frame_size = input_format.frame_bytes(width, height);

    grok_encoder::initialize(0);

    let mut frame_index: u64 = 0;
    let encode_start = std::time::Instant::now();
    let phase_clocks = Arc::new(grok_encoder::PhaseClocks::default());

    let result = grok_encoder::encode_pipeline_with_mxf_feed(
        &opts.output_dir,
        params,
        total_frames,
        cancel,
        &phase_clocks,
        mxf_feed,
        opts.codestream_byte_cap,
        |buffer_pool| {
            while pause.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut frame_buf = buffer_pool.take(frame_size);
            let read_start = std::time::Instant::now();
            let read = source.read_into(&mut frame_buf);
            phase_clocks.add(grok_encoder::EncodePhase::DecoderWait, read_start.elapsed());
            if let NextFrame::Ended = read {
                return None;
            }
            let index = frame_index;
            frame_index += 1;
            Some(input_format.raw_frame(frame_buf, width, height, index))
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

    let decode = source.finish(opts.fps.as_f64(), result.frames_encoded);

    let mut success = result.success;
    let mut error = result.error;
    if success && let Some(failure) = decode_failure(&decode, result.frames_encoded, total_frames) {
        success = false;
        error = failure;
    }

    EncodeResult {
        success,
        error,
        frames_encoded: result.frames_encoded,
        output_dir: opts.output_dir.clone(),
        picture_findings: decode.findings,
        encoder_input_pixel_format: Some(input_format.ffmpeg_pixel_format().to_string()),
    }
}

// ─── the decoder both stream decodes take their frames from ─────────────────

pub(crate) fn start_frame_decoder(
    chain: &DecodeChain,
    input: &Path,
    frame_range: Option<FrameRange>,
    width: u32,
    height: u32,
) -> Result<crate::ffmpeg_decode::FrameDecoder, String> {
    crate::ffmpeg_decode::FrameDecoder::start(&crate::ffmpeg_decode::DecodeRequest {
        input,
        decode_source: chain.decode_source,
        read_source_at: chain.read_source_at,
        hardware_decode: chain.hardware_decode,
        picture_filters: &chain.picture_filters,
        pixels: chain.decoded_pixels(),
        width,
        height,
        frame_limit: frame_range.map(|range| range.frame_count),
        detect_picture_findings: chain.detect_picture_findings,
    })
}

impl DecodeChain {
    fn decoded_pixels(&self) -> crate::ffmpeg_decode::DecodedPixels {
        use crate::ffmpeg_decode::DecodedPixels;
        match (self.alpha, self.input_format) {
            (AlphaChannel::Dropped, format) => DecodedPixels::ForEncoder(format),
            (AlphaChannel::Kept, EncoderInputFormat::PackedRgb(order)) => {
                DecodedPixels::PackedRgba(order)
            }
            (AlphaChannel::Kept, EncoderInputFormat::PlanarYuv(_)) => {
                unreachable!("decode_chain keeps a layer with alpha on packed RGB")
            }
        }
    }
}

pub(crate) fn decode_failure(
    decode: &crate::ffmpeg_decode::FinishedDecode,
    frames_encoded: u64,
    expected_frames: u64,
) -> Option<String> {
    if let Some(failure) = &decode.failure {
        return Some(failure.clone());
    }
    // a zero expected_frames is a probe that read no count, not an empty source
    (frames_encoded == 0 && expected_frames > 0)
        .then(|| "the decode produced no frames".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::tests::hdr10_source;
    use crate::encode::{PACKED_RGB_BYTES_PER_PIXEL, find_source_frames, write_image_concat_list};
    use std::path::PathBuf;

    /// The filters one picture plan decodes with.
    fn planned(
        fps: FrameRate,
        source_colour: &SourceColour,
        plan: &crate::picture_processing::PicturePlan,
        frame_range: Option<FrameRange>,
    ) -> DecodeFilters {
        decode_filters(
            &PictureFilters::Planned {
                plan,
                fps,
                frame_range,
                extra_filter: None,
            },
            source_colour,
        )
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
            planned(
                FrameRate::whole(24),
                &SourceColour::DisplayRgb,
                &plain,
                Some(window)
            )
            .joined(),
            "fps=24,trim=start_frame=7200:end_frame=7320,setpts=PTS-STARTPTS"
        );
        assert_eq!(
            planned(
                FrameRate::whole(24),
                &SourceColour::DisplayRgb,
                &plain,
                None
            )
            .joined(),
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
            planned(
                FrameRate::whole(24),
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                &processed,
                Some(window)
            )
            .joined(),
            "yadif,fps=24,trim=start_frame=7200:end_frame=7320,setpts=PTS-STARTPTS,hqdn3d,\
             lut3d=\\'/luts/hdr_to_dci.cube\\'"
        );
    }

    #[test]
    fn an_extra_picture_filter_runs_after_the_fit_and_before_the_lut() {
        let fitted = crate::picture_processing::PictureProcessing {
            fit: Some(crate::picture_processing::Fit {
                box_width: 1998,
                box_height: 1080,
                raster_width: 2048,
                raster_height: 1080,
                placement: crate::picture_processing::Placement::default(),
            }),
            ..crate::picture_processing::PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap();
        let without_fade = planned(
            FrameRate::whole(24),
            &SourceColour::DisplayRgb,
            &fitted,
            None,
        );
        let with_fade = decode_filters(
            &PictureFilters::Planned {
                plan: &fitted,
                fps: FrameRate::whole(24),
                frame_range: None,
                extra_filter: Some("fade=t=in:st=0:d=1"),
            },
            &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
        );
        assert!(
            without_fade.joined().contains("pad="),
            "{}",
            without_fade.joined()
        );
        assert_eq!(
            with_fade.joined(),
            format!(
                "{},fade=t=in:st=0:d=1,lut3d=\\'/luts/hdr_to_dci.cube\\'",
                without_fade.joined()
            )
        );
        assert_eq!(
            with_fade.geometry_format_position, without_fade.geometry_format_position,
            "the fade must not move the pixel format filter"
        );
    }

    #[test]
    fn an_image_list_cannot_also_be_read_at_a_rate() {
        assert!(check_read_rate(DecodeSource::Video, Some(FrameRate::whole(24))).is_ok());
        assert!(check_read_rate(DecodeSource::ImageList, None).is_ok());
        let refused =
            check_read_rate(DecodeSource::ImageList, Some(FrameRate::whole(24))).unwrap_err();
        assert!(refused.contains("concat list"), "{refused}");
    }

    #[test]
    fn the_lut_source_decodes_through_lut3d() {
        let plain = crate::picture_processing::PictureProcessing::default()
            .plan(1920, 1080)
            .unwrap();
        assert_eq!(
            planned(
                FrameRate::whole(24),
                &SourceColour::DisplayRgb,
                &plain,
                None
            )
            .joined(),
            "fps=24"
        );
        assert_eq!(
            planned(FrameRate::whole(25), &SourceColour::AlreadyPq, &plain, None).joined(),
            "fps=25"
        );
        assert_eq!(
            planned(
                FrameRate::whole(48),
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                &plain,
                None
            )
            .joined(),
            "fps=48,lut3d=\\'/luts/hdr_to_dci.cube\\'"
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
            planned(
                FrameRate::whole(24),
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                &plan,
                None
            )
            .joined(),
            "yadif,fps=24,hqdn3d,crop=1920:804:0:138,lut3d=\\'/luts/hdr_to_dci.cube\\'",
            "the pixel format filter is the encoder input format's, not the plan's"
        );
    }

    #[test]
    fn an_image_list_probes_the_pixel_format_of_its_stills() {
        let dir = tempfile::tempdir().unwrap();
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=s=64x48:d=1:r=2",
                "-pix_fmt",
                "yuvj420p",
            ])
            .arg(dir.path().join("still_%02d.jpg"))
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        let stills = find_source_frames(dir.path()).unwrap();
        let list = dir.path().join("stills.ffconcat");
        write_image_concat_list(&stills, FrameRate::whole(24), &list).unwrap();
        let probed = crate::ffmpeg_decode::probe(&list, DecodeSource::ImageList)
            .unwrap()
            .pixel_format;
        assert_eq!(probed.pix_fmt, "yuvj420p");
        assert_eq!(probed.color_range, "pc");
    }

    #[test]
    fn a_callers_own_chain_keeps_the_decode_lut_last() {
        let lut = SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube"));
        assert_eq!(
            decode_filters(&PictureFilters::Given("fade=t=in:st=0:d=1"), &lut).joined(),
            "fade=t=in:st=0:d=1,lut3d=\\'/luts/hdr_to_dci.cube\\'"
        );
        assert_eq!(
            decode_filters(&PictureFilters::Given(""), &lut).joined(),
            "lut3d=\\'/luts/hdr_to_dci.cube\\'"
        );
    }

    fn source_pixel_format(pix_fmt: &str) -> crate::probe::PixelFormatInfo {
        crate::probe::PixelFormatInfo {
            pix_fmt: pix_fmt.to_string(),
            ..Default::default()
        }
    }

    fn hdr10_pixel_format(pix_fmt: &str, color_range: &str) -> crate::probe::PixelFormatInfo {
        crate::probe::PixelFormatInfo {
            pix_fmt: pix_fmt.to_string(),
            color_space: "bt2020nc".to_string(),
            color_range: color_range.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn an_hdr_master_decodes_through_its_own_matrix_and_range() {
        let picture = decode_filters(&PictureFilters::Given(""), &SourceColour::DisplayRgb);
        let packed_rgb = EncoderInputFormat::PackedRgb(SampleOrder::Big);
        let hdr = hdr10_source();
        let chain = |source: &crate::probe::PixelFormatInfo| {
            decode_filter_chain(&picture, packed_rgb, AlphaChannel::Dropped, source, &hdr)
        };

        let studio = chain(&hdr10_pixel_format("yuv420p10le", "tv"));
        assert!(
            studio.starts_with(
                "zscale=matrixin=2020_ncl:rangein=limited:matrix=gbr:range=full,format=gbrp16le"
            ),
            "{studio}"
        );
        assert!(
            !studio.contains("tonemap")
                && !studio.contains("transfer")
                && !studio.contains("primaries"),
            "the transform reads the transfer function itself, so ffmpeg must leave it: {studio}"
        );
        assert!(
            chain(&hdr10_pixel_format("yuv422p10le", "pc")).contains("rangein=full"),
            "a full range master is not read as studio range"
        );
        assert!(
            chain(&source_pixel_format("yuv420p10le")).contains("matrixin=2020_ncl"),
            "an untagged HDR10 master is BT.2020, not the BT.601 an untagged SDR one is"
        );
        assert!(
            !chain(&hdr10_pixel_format("gbrp16le", "pc")).contains("zscale"),
            "an RGB master has no matrix to undo"
        );
        assert!(
            !decode_filter_chain(
                &picture,
                packed_rgb,
                AlphaChannel::Dropped,
                &hdr10_pixel_format("yuv420p10le", "tv"),
                &SourceColour::DisplayRgb
            )
            .contains("zscale"),
            "only an HDR master takes the explicit matrix"
        );
    }

    #[test]
    fn an_hdr_master_never_reaches_the_plugin_as_planar_yuv() {
        let source = hdr10_pixel_format("yuv420p10le", "tv");
        let hdr = hdr10_source();
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    accelerator_active: true,
                    quality_psnr: None,
                    postkit_prepares_the_frame: false,
                    source_colour: &hdr,
                    filters: "fps=24",
                    source: &source,
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "postkit reads every sample of an HDR master, so the planes cannot go to the plugin"
        );
    }

    #[test]
    fn the_yuv_pipe_needs_every_condition_at_once() {
        let source = source_pixel_format("yuv420p");
        let display_rgb = SourceColour::DisplayRgb;
        let accelerated = FormatChoiceInputs {
            accelerator_active: true,
            quality_psnr: None,
            postkit_prepares_the_frame: false,
            source_colour: &display_rgb,
            filters: "fps=24",
            source: &source,
        };
        assert!(
            matches!(
                choose_encoder_input_format(&accelerated, true),
                EncoderInputFormat::PlanarYuv(_)
            ),
            "an accelerated run over an untouched yuv420p source takes the planes"
        );

        assert_eq!(
            choose_encoder_input_format(&accelerated, false),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "a plugin that will not take the planes leaves the run on RGB"
        );
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    accelerator_active: false,
                    ..accelerated
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "nothing but the plugin reads the planes"
        );
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    quality_psnr: Some(50.0),
                    ..accelerated
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "a PSNR target keeps the run off the batch"
        );
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    postkit_prepares_the_frame: true,
                    ..accelerated
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "a burn needs samples postkit can write into"
        );
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    filters: "fps=24,format=gbrp16le,crop=1998:1080:0:0",
                    ..accelerated
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "a caller's own pixel format filter leaves nothing of the source's planes"
        );
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    filters: "fps=24,lut3d=\\'/luts/hdr_to_dci.cube\\'",
                    ..accelerated
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "a colour filter in the chain leaves nothing of the source's own planes"
        );
        assert!(
            matches!(
                choose_encoder_input_format(
                    &FormatChoiceInputs {
                        filters: "yadif,fps=24,crop=1998:1080:0:0,scale=w=1998:h=1080,\
                                  split=2[picture][detect];[detect]blackdetect,nullsink;\
                                  [picture]null",
                        ..accelerated
                    },
                    true
                ),
                EncoderInputFormat::PlanarYuv(_)
            ),
            "geometry and the detection branch keep the pixel format"
        );

        let wide_gamut = SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3);
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    source_colour: &wide_gamut,
                    ..accelerated
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "postkit converts this source itself and needs RGB"
        );
        let to_rec709 = SourceColour::KeepRgbFrom(crate::colour::ColourSpace::P3);
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    source_colour: &to_rec709,
                    ..accelerated
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "postkit converts this source to Rec.709 itself and needs RGB"
        );
        for lut in [
            SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
            SourceColour::KeepRgbAfterLut(PathBuf::from("/luts/to_rec709.cube")),
        ] {
            assert_eq!(
                choose_encoder_input_format(
                    &FormatChoiceInputs {
                        source_colour: &lut,
                        ..accelerated
                    },
                    true
                ),
                EncoderInputFormat::PackedRgb(SampleOrder::Big),
                "lut3d hands the encoder RGB"
            );
        }

        for already_transformed in [SourceColour::AlreadyPq, SourceColour::KeepRgb] {
            assert!(
                matches!(
                    choose_encoder_input_format(
                        &FormatChoiceInputs {
                            source_colour: &already_transformed,
                            ..accelerated
                        },
                        true
                    ),
                    EncoderInputFormat::PlanarYuv(_)
                ),
                "{already_transformed:?} leaves the frame alone, so the planes can go through"
            );
        }

        let rgb_source = source_pixel_format("gbrp12le");
        assert_eq!(
            choose_encoder_input_format(
                &FormatChoiceInputs {
                    source: &rgb_source,
                    ..accelerated
                },
                true
            ),
            EncoderInputFormat::PackedRgb(SampleOrder::Big),
            "a source that is not one of the four planar YUV formats stays on RGB"
        );
    }

    #[test]
    fn the_yuv_pipe_carries_the_sources_matrix_and_range() {
        let display_rgb = SourceColour::DisplayRgb;
        let source = crate::probe::PixelFormatInfo {
            pix_fmt: "yuv422p10le".to_string(),
            color_space: "bt2020nc".to_string(),
            color_range: "pc".to_string(),
            ..Default::default()
        };
        let chosen = choose_encoder_input_format(
            &FormatChoiceInputs {
                accelerator_active: true,
                quality_psnr: None,
                postkit_prepares_the_frame: false,
                source_colour: &display_rgb,
                filters: "fps=24",
                source: &source,
            },
            true,
        );
        assert_eq!(
            chosen,
            EncoderInputFormat::PlanarYuv(YuvFrameFormat {
                pixel_format: PlanarYuvPixelFormat::Yuv422p10le,
                matrix: YuvMatrix::Bt2020,
                full_range: true,
            })
        );
    }

    #[test]
    fn the_eight_bit_yuv_formats_are_the_ones_swscale_converts_at_eight_bits() {
        for pix_fmt in [
            "yuv420p", "yuvj420p", "yuv422p", "yuvj422p", "yuv444p", "yuvj444p", "yuv410p",
            "yuv411p", "yuv440p", "nv12", "nv21",
        ] {
            assert!(is_eight_bit_yuv_pixel_format(pix_fmt), "{pix_fmt}");
        }
        for pix_fmt in [
            "yuv420p10le",
            "yuv422p10le",
            "yuv422p12le",
            "yuv444p16le",
            "nv20le",
            "rgb24",
            "rgb48be",
            "gbrp",
            "gbrp16le",
            "gray",
            "gray10le",
            "pal8",
        ] {
            assert!(!is_eight_bit_yuv_pixel_format(pix_fmt), "{pix_fmt}");
        }
    }

    #[test]
    fn only_an_eight_bit_yuv_source_on_the_rgb_pipe_converts_to_sixteen_bits() {
        let untouched = "fps=24";
        let plain = crate::picture_processing::PictureProcessing::default()
            .plan(1920, 1080)
            .unwrap();
        let picture = planned(
            FrameRate::whole(24),
            &SourceColour::DisplayRgb,
            &plain,
            None,
        );
        let planar_yuv = EncoderInputFormat::PlanarYuv(YuvFrameFormat {
            pixel_format: PlanarYuvPixelFormat::Yuv420p,
            matrix: YuvMatrix::Bt601,
            full_range: false,
        });
        assert_eq!(
            decode_filter_chain(
                &picture,
                planar_yuv,
                AlphaChannel::Dropped,
                &source_pixel_format("yuv420p"),
                &SourceColour::DisplayRgb
            ),
            untouched,
            "the plugin converts the planes itself"
        );
        assert_eq!(
            decode_filter_chain(
                &picture,
                EncoderInputFormat::PackedRgb(SampleOrder::Big),
                AlphaChannel::Dropped,
                &source_pixel_format("yuv420p10le"),
                &SourceColour::DisplayRgb
            ),
            untouched,
            "a 10-bit source with no geometry already converts at full precision"
        );
        assert_eq!(
            decode_filter_chain(
                &picture,
                EncoderInputFormat::PackedRgb(SampleOrder::Big),
                AlphaChannel::Dropped,
                &source_pixel_format("gbrp"),
                &SourceColour::DisplayRgb
            ),
            untouched,
            "an RGB source has no matrix to apply"
        );
        assert_eq!(
            decode_filter_chain(
                &decode_filters(&PictureFilters::Given(""), &SourceColour::DisplayRgb),
                EncoderInputFormat::PackedRgb(SampleOrder::Little),
                AlphaChannel::Dropped,
                &source_pixel_format("yuv420p"),
                &SourceColour::DisplayRgb
            ),
            "format=gbrp16le",
            "a decode with no picture filters of its own still converts"
        );
    }

    #[test]
    fn the_detectors_run_only_when_asked() {
        assert_eq!(
            with_optional_detection_branch("fps=24".to_string(), false),
            "fps=24"
        );
        assert_eq!(with_optional_detection_branch(String::new(), false), "null");
        assert!(
            with_optional_detection_branch("fps=24".to_string(), true)
                .starts_with("fps=24,split=2[picture][detect];[detect]blackdetect")
        );
        assert!(
            with_optional_detection_branch(String::new(), true).starts_with("null,split=2"),
            "the branch needs a picture chain to split"
        );
    }

    /// A plan with every geometry step in it, which is what decides where the
    /// pixel format filter goes.
    fn geometry_plan(deinterlace: bool) -> crate::picture_processing::PicturePlan {
        crate::picture_processing::PictureProcessing {
            deinterlace,
            denoise: deinterlace,
            crop: crate::picture_processing::Crop::to_aspect(1920, 1080, 2048, 858),
            fit: Some(crate::picture_processing::Fit {
                box_width: 2048,
                box_height: 858,
                raster_width: 2048,
                raster_height: 1080,
                placement: crate::picture_processing::Placement::default(),
            }),
            ..crate::picture_processing::PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap()
    }

    #[test]
    fn a_layer_with_alpha_keeps_it_through_the_conversion_and_pads_see_through() {
        let plan = crate::picture_processing::PictureProcessing {
            fit: Some(crate::picture_processing::Fit {
                box_width: 2048,
                box_height: 858,
                raster_width: 2048,
                raster_height: 1080,
                placement: crate::picture_processing::Placement::default(),
            }),
            ..crate::picture_processing::PictureProcessing::default()
        }
        .plan_with_transparent_pad(1920, 1080)
        .unwrap();
        let picture = planned(FrameRate::whole(24), &SourceColour::DisplayRgb, &plan, None);
        let fit = "scale=w=1524:h=858:flags=lanczos,pad=w=2048:h=1080:x=262:y=110:color=black@0";
        for (pix_fmt, chain) in [
            ("yuva420p", format!("format=gbrap16le,fps=24,{fit}")),
            ("rgba", format!("fps=24,format=gbrap16le,{fit}")),
        ] {
            assert_eq!(
                decode_filter_chain(
                    &picture,
                    EncoderInputFormat::PackedRgb(SampleOrder::Big),
                    AlphaChannel::Kept,
                    &source_pixel_format(pix_fmt),
                    &SourceColour::DisplayRgb
                ),
                chain
            );
        }
    }

    #[test]
    fn a_bottom_picture_with_alpha_goes_over_black_at_the_head_and_an_opaque_one_does_not() {
        let plan = geometry_plan(false);
        let picture = planned(FrameRate::whole(24), &SourceColour::DisplayRgb, &plan, None);
        let chain = |source: &crate::probe::PixelFormatInfo, alpha, colour: &SourceColour| {
            decode_filter_chain(
                &picture,
                EncoderInputFormat::PackedRgb(SampleOrder::Big),
                alpha,
                source,
                colour,
            )
        };
        let display = SourceColour::DisplayRgb;
        let geometry = "fps=24,crop=1920:804:0:138,scale=w=2048:h=856:flags=lanczos,\
                        pad=w=2048:h=1080:x=0:y=112:color=black";
        for pix_fmt in ["rgba", "rgba64be", "yuva444p12le", "yuva420p", "pal8"] {
            assert_eq!(
                chain(
                    &source_pixel_format(pix_fmt),
                    AlphaChannel::Dropped,
                    &display
                ),
                format!("format=gbrap16le,premultiply=inplace=1,{geometry}"),
                "{pix_fmt}"
            );
        }
        assert!(
            !chain(&source_pixel_format("rgba"), AlphaChannel::Kept, &display)
                .contains("premultiply"),
            "an overlay keeps straight alpha for its own composite"
        );
        for pix_fmt in ["rgb24", "gbrp12le", "yuv420p", "yuv422p10le"] {
            assert!(
                !chain(
                    &source_pixel_format(pix_fmt),
                    AlphaChannel::Dropped,
                    &display
                )
                .contains("premultiply"),
                "{pix_fmt}"
            );
        }
        assert!(
            chain(
                &hdr10_pixel_format("yuva444p12le", "tv"),
                AlphaChannel::Dropped,
                &hdr10_source()
            )
            .starts_with(
                "zscale=matrixin=2020_ncl:rangein=limited:matrix=gbr:range=full,\
                 format=gbrap16le,premultiply=inplace=1,fps=24"
            ),
            "an hdr master leaves the matrix before it goes over black"
        );
    }

    /// One `format=gbrp16le` in each chain: the head conversion an 8-bit YUV
    /// source gets covers its geometry as well.
    #[test]
    fn the_packed_rgb_pipe_converts_where_the_geometry_starts() {
        let simple = geometry_plan(false);
        let picture = planned(
            FrameRate::whole(24),
            &SourceColour::DisplayRgb,
            &simple,
            None,
        );
        assert_eq!(
            decode_filter_chain(
                &picture,
                EncoderInputFormat::PackedRgb(SampleOrder::Big),
                AlphaChannel::Dropped,
                &source_pixel_format("yuv420p"),
                &SourceColour::DisplayRgb
            ),
            "format=gbrp16le,fps=24,crop=1920:804:0:138,scale=w=2048:h=856:flags=lanczos,\
                 pad=w=2048:h=1080:x=0:y=112:color=black"
        );
        assert_eq!(
            decode_filter_chain(
                &picture,
                EncoderInputFormat::PackedRgb(SampleOrder::Big),
                AlphaChannel::Dropped,
                &source_pixel_format("rgb24"),
                &SourceColour::DisplayRgb
            ),
            "fps=24,format=gbrp16le,crop=1920:804:0:138,scale=w=2048:h=856:flags=lanczos,\
                 pad=w=2048:h=1080:x=0:y=112:color=black"
        );

        let window = FrameRange {
            first_frame: 10,
            frame_count: 5,
        };
        let processed = geometry_plan(true);
        let picture = planned(
            FrameRate::whole(24),
            &SourceColour::DisplayRgb,
            &processed,
            Some(window),
        );
        assert_eq!(
            decode_filter_chain(
                &picture,
                EncoderInputFormat::PackedRgb(SampleOrder::Big),
                AlphaChannel::Dropped,
                &source_pixel_format("yuv420p"),
                &SourceColour::DisplayRgb
            ),
            "format=gbrp16le,yadif,fps=24,trim=start_frame=10:end_frame=15,\
                 setpts=PTS-STARTPTS,hqdn3d,crop=1920:804:0:138,\
                 scale=w=2048:h=856:flags=lanczos,pad=w=2048:h=1080:x=0:y=112:color=black"
        );
        assert_eq!(
            decode_filter_chain(
                &picture,
                EncoderInputFormat::PackedRgb(SampleOrder::Big),
                AlphaChannel::Dropped,
                &source_pixel_format("rgb24"),
                &SourceColour::DisplayRgb
            ),
            "yadif,fps=24,trim=start_frame=10:end_frame=15,setpts=PTS-STARTPTS,hqdn3d,\
                 format=gbrp16le,crop=1920:804:0:138,scale=w=2048:h=856:flags=lanczos,\
                 pad=w=2048:h=1080:x=0:y=112:color=black"
        );

        let planar_yuv = EncoderInputFormat::PlanarYuv(YuvFrameFormat {
            pixel_format: PlanarYuvPixelFormat::Yuv420p,
            matrix: YuvMatrix::Bt601,
            full_range: false,
        });
        let on_the_planes = decode_filter_chain(
            &picture,
            planar_yuv,
            AlphaChannel::Dropped,
            &source_pixel_format("yuv420p"),
            &SourceColour::DisplayRgb,
        );
        assert!(
            !on_the_planes.contains("format="),
            "the geometry runs on the source's own planes: {on_the_planes}"
        );
    }

    /// The BT.601 luma coefficients and the studio range an 8-bit YUV source's
    /// samples sit in: luma 16 to 235, chroma 128 either way by 112.
    const BT601_RED_COEFFICIENT: f64 = 0.299;

    const BT601_BLUE_COEFFICIENT: f64 = 0.114;

    const STUDIO_LUMA_FLOOR: f64 = 16.0;

    const STUDIO_LUMA_RANGE: f64 = 219.0;

    const STUDIO_CHROMA_MIDDLE: f64 = 128.0;

    const STUDIO_CHROMA_RANGE: f64 = 224.0;

    const EIGHT_BIT_PEAK: f64 = 255.0;

    const SIXTEEN_BIT_PEAK: f64 = 65535.0;

    /// The exact limited range BT.601 conversion of one YUV sample, as 16-bit
    /// RGB.
    fn exact_bt601_rgb(sample: [u8; 3]) -> [f64; 3] {
        let [luma_sample, blue_sample, red_sample] = sample.map(f64::from);
        let luma = (luma_sample - STUDIO_LUMA_FLOOR) * EIGHT_BIT_PEAK / STUDIO_LUMA_RANGE;
        let chroma_scale = 2.0 * EIGHT_BIT_PEAK / STUDIO_CHROMA_RANGE;
        let green_coefficient = 1.0 - BT601_RED_COEFFICIENT - BT601_BLUE_COEFFICIENT;
        let from_red =
            chroma_scale * (1.0 - BT601_RED_COEFFICIENT) * (red_sample - STUDIO_CHROMA_MIDDLE);
        let from_blue =
            chroma_scale * (1.0 - BT601_BLUE_COEFFICIENT) * (blue_sample - STUDIO_CHROMA_MIDDLE);
        let rgb = [
            luma + from_red,
            luma - from_red * BT601_RED_COEFFICIENT / green_coefficient
                - from_blue * BT601_BLUE_COEFFICIENT / green_coefficient,
            luma + from_blue,
        ];
        rgb.map(|value| value * SIXTEEN_BIT_PEAK / EIGHT_BIT_PEAK)
    }

    /// How far off the exact conversion a decoded sample may sit, in codes
    /// of 65535. swscale rounds the studio range expansion at 16 bits, which
    /// costs under 200 codes, while its 8-bit conversion of the same colour is
    /// 280 codes or more low.
    const EXACT_TOLERANCE_CODES: f64 = 200.0;

    const SOLID_COLOUR_SIZE: u32 = 64;

    fn run_ffmpeg(arguments: &[&str]) {
        let run = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "ffmpeg {}: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&run.stderr)
        );
    }

    /// A lossless one-frame clip of a single colour in 8-bit YUV, and the Y, Cb
    /// and Cr ffmpeg wrote into it.
    fn solid_colour_clip(dir: &Path, colour: &str) -> (PathBuf, [u8; 3]) {
        let clip = dir.join("colour.mkv");
        run_ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &format!("color=c={colour}:size={SOLID_COLOUR_SIZE}x{SOLID_COLOUR_SIZE}:rate=1"),
            "-frames:v",
            "1",
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
            &clip.to_string_lossy(),
        ]);
        let planes = dir.join("colour.yuv");
        run_ffmpeg(&[
            "-i",
            &clip.to_string_lossy(),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            &planes.to_string_lossy(),
        ]);
        let frame = std::fs::read(&planes).unwrap();
        let luma_samples = (SOLID_COLOUR_SIZE * SOLID_COLOUR_SIZE) as usize;
        (
            clip,
            [
                frame[0],
                frame[luma_samples],
                frame[luma_samples + luma_samples / 4],
            ],
        )
    }

    /// The middle pixel of the first frame the decode writes, which is inside
    /// the picture whether or not the chain pads it.
    fn middle_decoded_pixel(clip: &Path, filters: &str) -> [u16; 3] {
        let input_format = EncoderInputFormat::PackedRgb(SampleOrder::Big);
        let mut decoder =
            crate::ffmpeg_decode::FrameDecoder::start(&crate::ffmpeg_decode::DecodeRequest {
                input: clip,
                decode_source: DecodeSource::Video,
                read_source_at: None,
                hardware_decode: false,
                picture_filters: filters,
                pixels: crate::ffmpeg_decode::DecodedPixels::ForEncoder(input_format),
                width: SOLID_COLOUR_SIZE,
                height: SOLID_COLOUR_SIZE,
                frame_limit: None,
                detect_picture_findings: false,
            })
            .unwrap_or_else(|error| panic!("{filters}: {error}"));
        let mut frame = vec![0u8; input_format.frame_bytes(SOLID_COLOUR_SIZE, SOLID_COLOUR_SIZE)];
        assert!(
            matches!(
                decoder.read_into(&mut frame),
                crate::ffmpeg_decode::NextFrame::Copied
            ),
            "{filters}: the decode wrote no frame"
        );
        let finished = decoder.finish(1.0, 1);
        assert!(
            finished.failure.is_none(),
            "{filters}: {:?}",
            finished.failure
        );
        let middle = SOLID_COLOUR_SIZE as usize / 2;
        let offset = (middle * SOLID_COLOUR_SIZE as usize + middle) * PACKED_RGB_BYTES_PER_PIXEL;
        [0, 1, 2].map(|component| {
            let at = offset + component * 2;
            u16::from_be_bytes([frame[at], frame[at + 1]])
        })
    }

    #[test]
    fn an_eight_bit_yuv_source_reaches_the_pipe_at_the_exact_colour() {
        let dir = tempfile::tempdir().unwrap();
        let (clip, sample) = solid_colour_clip(dir.path(), "0x4080c0");
        let source = crate::probe::probe_pixel_format(&clip);
        assert_eq!(source.pix_fmt, "yuv420p");
        let exact = exact_bt601_rgb(sample);
        let packed_rgb = EncoderInputFormat::PackedRgb(SampleOrder::Big);

        let plain = crate::picture_processing::PictureProcessing::default()
            .plan(SOLID_COLOUR_SIZE, SOLID_COLOUR_SIZE)
            .unwrap();
        // what a subtitle burn to a DCI raster decodes through: postkit reads
        // the samples, so the frames are packed RGB and the geometry runs on them
        let burn = crate::picture_processing::PictureProcessing {
            fit: Some(crate::picture_processing::Fit {
                box_width: 48,
                box_height: 48,
                raster_width: SOLID_COLOUR_SIZE,
                raster_height: SOLID_COLOUR_SIZE,
                placement: crate::picture_processing::Placement::default(),
            }),
            ..crate::picture_processing::PictureProcessing::default()
        }
        .plan(SOLID_COLOUR_SIZE, SOLID_COLOUR_SIZE)
        .unwrap();
        let lut = dir.path().join("identity.cube");
        std::fs::write(
            &lut,
            "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
        )
        .unwrap();

        let chains = [
            (
                "plain",
                planned(
                    FrameRate::whole(24),
                    &SourceColour::DisplayRgb,
                    &plain,
                    None,
                ),
            ),
            (
                "burn",
                planned(FrameRate::whole(24), &SourceColour::DisplayRgb, &burn, None),
            ),
            (
                "lut",
                planned(
                    FrameRate::whole(24),
                    &SourceColour::DciLut(lut),
                    &plain,
                    None,
                ),
            ),
        ];
        for (name, picture_filters) in chains {
            let filters = decode_filter_chain(
                &picture_filters,
                packed_rgb,
                AlphaChannel::Dropped,
                &source,
                &SourceColour::DisplayRgb,
            );
            assert!(
                filters.starts_with("format=gbrp16le,"),
                "{name}: the conversion has to run before everything else: {filters}"
            );
            let pixel = middle_decoded_pixel(&clip, &filters);
            for (component, (measured, exact)) in pixel.iter().zip(exact).enumerate() {
                let error = f64::from(*measured) - exact;
                assert!(
                    error.abs() <= EXACT_TOLERANCE_CODES,
                    "{name}: component {component} decoded at {measured} against the \
                     exact {exact:.0}, {error:.0} codes out"
                );
            }
        }
    }
}
