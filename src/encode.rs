use crate::grok_encoder::SampleOrder;
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

/// Result of encoding operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EncodeResult {
    pub success: bool,
    pub error: String,
    pub frames_encoded: u64,
    pub output_dir: PathBuf,
    /// Black and frozen runs blackdetect and freezedetect saw during the
    /// decode. Empty for a TIFF sequence, which postkit reads itself, since
    /// nothing decoded through ffmpeg there.
    #[serde(default)]
    pub picture_findings: crate::picture_findings::PictureFindings,
    /// The pixel format ffmpeg wrote to the pipe, `None` for a run that decoded
    /// no pipe at all. A planar YUV name here is a run whose frames reached
    /// grok's accelerator plugin unconverted.
    #[serde(default)]
    pub pipe_pixel_format: Option<String>,
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

/// The first still of an image sequence, the frame that decides its format and
/// its raster.
pub(crate) fn first_source_frame(directory: &Path) -> Result<PathBuf, String> {
    let frames = find_source_frames(directory)
        .map_err(|e| format!("cannot list {}: {e}", directory.display()))?;
    frames
        .into_iter()
        .next()
        .ok_or_else(|| format!("no images in {}", directory.display()))
}

/// The picture's size: a container's own raster, or the first frame of an
/// image sequence. A TIFF is read by postkit itself, since ffprobe cannot read
/// a 12-bit one.
pub fn source_raster(picture: &Path) -> Result<(u32, u32), String> {
    let measured = match detect_input_type(picture) {
        InputType::ImageSequence if picture.is_dir() => first_source_frame(picture)?,
        _ => picture.to_path_buf(),
    };
    if detect_image_format(&measured) == ImageFormat::Tiff {
        let frame = crate::grok::load_tiff(&measured)?;
        return Ok((frame.width, frame.height));
    }
    let info = crate::probe::probe_video(&measured)
        .ok_or_else(|| format!("cannot read the picture size of {}", measured.display()))?;
    Ok((info.width, info.height))
}

// ─── Streaming encode (ffmpeg → raw pipe → in-process grok) ────────────────

use std::io::Read;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What colour the source frames carry when they reach the J2K compressor.
///
/// The compressor's own DCDM X'Y'Z' transform is applied if and only if this is
/// `DisplayRgb`, so essence that a caller later labels ST 2084 PQ or RGB can
/// never hold frames the encoder transformed itself.
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
    /// Display RGB compressed as it is, for IMF picture whose descriptor names
    /// the colour the samples are already in.
    KeepRgb,
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
        filters.push(format!(
            "lut3d={}",
            crate::burnin::filter_argument(&lut.to_string_lossy())
        ));
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

// ─── the pixel format ffmpeg writes to the pipe ───────────────────────────────

/// Decode on the GPU and bring the frames back to system memory. Without
/// `-hwaccel_output_format` ffmpeg downloads them itself, and a codec the device
/// cannot decode falls back to software decoding with no error.
pub(crate) const HARDWARE_DECODE_ARGS: [&str; 2] = ["-hwaccel", "cuda"];

/// Packed 16-bit RGB: three components per pixel, six bytes. postkit
/// deinterleaves the big-endian layout itself and hands the little-endian one to
/// grok's accelerator plugin as it arrives.
const PACKED_RGB_BIG_ENDIAN_PIXEL_FORMAT: &str = "rgb48be";
const PACKED_RGB_LITTLE_ENDIAN_PIXEL_FORMAT: &str = "rgb48le";
const PACKED_RGB_BYTES_PER_PIXEL: usize = 6;
pub(crate) const PACKED_RGB_PRECISION: u8 = 16;

/// The filter that converts an 8-bit YUV source to 16-bit planar RGB before
/// anything else reads it. swscale's own conversion from 8-bit YUV to rgb48
/// runs at 8 bits and leaves every sample about two codes of 255 off the exact
/// value.
const SIXTEEN_BIT_RGB_FILTER: &str = "format=gbrp16le";

/// ffmpeg filters that change the pixel format or the colour, so the picture on
/// the pipe is no longer the source's own planes. `format=` is what
/// [`crate::picture_processing`] inserts for any geometry work. Geometry
/// filters are not here: cropping, scaling, padding, rotating, deinterlacing
/// and denoising all keep the pixel format they were given.
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

/// A planar YUV pixel format postkit passes to grok's accelerator plugin as it
/// comes off the pipe. The name is ffprobe's and `-pix_fmt`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanarYuvPixelFormat {
    Yuv420p,
    Yuv422p,
    Yuv420p10le,
    Yuv422p10le,
}

/// What one planar YUV pixel format is made of.
struct PlanarYuvProperties {
    ffmpeg_name: &'static str,
    /// bits one sample carries, 8 or 10
    bit_depth: u8,
    /// whether the chroma planes have half the rows as well as half the columns
    chroma_is_half_height: bool,
}

impl PlanarYuvPixelFormat {
    const ALL: [Self; 4] = [
        Self::Yuv420p,
        Self::Yuv422p,
        Self::Yuv420p10le,
        Self::Yuv422p10le,
    ];

    fn properties(self) -> PlanarYuvProperties {
        match self {
            Self::Yuv420p => PlanarYuvProperties {
                ffmpeg_name: "yuv420p",
                bit_depth: 8,
                chroma_is_half_height: true,
            },
            Self::Yuv422p => PlanarYuvProperties {
                ffmpeg_name: "yuv422p",
                bit_depth: 8,
                chroma_is_half_height: false,
            },
            Self::Yuv420p10le => PlanarYuvProperties {
                ffmpeg_name: "yuv420p10le",
                bit_depth: 10,
                chroma_is_half_height: true,
            },
            Self::Yuv422p10le => PlanarYuvProperties {
                ffmpeg_name: "yuv422p10le",
                bit_depth: 10,
                chroma_is_half_height: false,
            },
        }
    }

    /// The format of that name, or `None` for every other pixel format,
    /// including the ones postkit reads itself.
    pub fn from_ffmpeg_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|format| format.properties().ffmpeg_name == name)
    }

    pub fn ffmpeg_name(self) -> &'static str {
        self.properties().ffmpeg_name
    }

    /// Bits one sample carries, 8 or 10.
    pub fn bit_depth(self) -> u8 {
        self.properties().bit_depth
    }

    /// Whether the chroma planes have half the rows as well as half the
    /// columns, which is what separates 4:2:0 from 4:2:2.
    pub fn chroma_is_half_height(self) -> bool {
        self.properties().chroma_is_half_height
    }

    /// Bytes one sample takes on the pipe. A 10-bit sample arrives in a
    /// little-endian 16-bit container.
    pub fn bytes_per_sample(self) -> usize {
        if self.properties().bit_depth > 8 {
            2
        } else {
            1
        }
    }

    /// Where the three planes of one frame sit in the bytes ffmpeg writes.
    pub fn plane_layout(self, width: u32, height: u32) -> YuvPlaneLayout {
        let chroma_width = width.div_ceil(2);
        let chroma_height = if self.properties().chroma_is_half_height {
            height.div_ceil(2)
        } else {
            height
        };
        let bytes_per_sample = self.bytes_per_sample();
        let luma_bytes = width as usize * height as usize * bytes_per_sample;
        let chroma_bytes = chroma_width as usize * chroma_height as usize * bytes_per_sample;
        YuvPlaneLayout {
            luma_width: width,
            luma_height: height,
            chroma_width,
            chroma_height,
            bytes_per_sample,
            luma_offset: 0,
            blue_chroma_offset: luma_bytes,
            red_chroma_offset: luma_bytes + chroma_bytes,
            frame_bytes: luma_bytes + 2 * chroma_bytes,
        }
    }
}

/// Where each plane of one frame starts in the bytes ffmpeg's rawvideo muxer
/// writes, and how big it is. rawvideo pads nothing, so each plane's row stride
/// in samples is its own width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YuvPlaneLayout {
    pub luma_width: u32,
    pub luma_height: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub bytes_per_sample: usize,
    pub luma_offset: usize,
    pub blue_chroma_offset: usize,
    pub red_chroma_offset: usize,
    pub frame_bytes: usize,
}

/// The matrix a YUV source is converted to RGB with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl YuvMatrix {
    /// The matrix for what ffprobe reported as the stream's colour space. An
    /// untagged stream is BT.601, which is what swscale converts one as on the
    /// packed RGB path, so both pipe formats give the same picture.
    pub fn for_ffprobe_color_space(color_space: &str) -> Self {
        match color_space {
            "bt709" => Self::Bt709,
            "bt2020nc" | "bt2020c" => Self::Bt2020,
            _ => Self::Bt601,
        }
    }
}

/// Everything about the planar YUV frames on the pipe that grok's plugin needs
/// and that is the same for every frame of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct YuvFrameFormat {
    pub pixel_format: PlanarYuvPixelFormat,
    pub matrix: YuvMatrix,
    /// whether the samples use the whole code value range rather than the
    /// studio range
    pub full_range: bool,
}

/// What ffmpeg writes to the pipe for a stream encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeFormat {
    /// Packed 16-bit RGB in that byte order. The compressor converts the
    /// colour either way: postkit deinterleaves the big-endian layout, and
    /// grok's accelerator plugin takes the little-endian one interleaved.
    PackedRgb(SampleOrder),
    /// The source's own planar YUV, handed to grok's accelerator plugin as it
    /// arrives: the plugin upsamples the chroma and converts the colour.
    PlanarYuv(YuvFrameFormat),
}

impl PipeFormat {
    pub fn ffmpeg_pixel_format(&self) -> &'static str {
        match self {
            PipeFormat::PackedRgb(SampleOrder::Big) => PACKED_RGB_BIG_ENDIAN_PIXEL_FORMAT,
            PipeFormat::PackedRgb(SampleOrder::Little) => PACKED_RGB_LITTLE_ENDIAN_PIXEL_FORMAT,
            PipeFormat::PlanarYuv(format) => format.pixel_format.ffmpeg_name(),
        }
    }

    /// Bytes one frame takes on the pipe.
    pub fn frame_bytes(&self, width: u32, height: u32) -> usize {
        match self {
            PipeFormat::PackedRgb(_) => {
                width as usize * height as usize * PACKED_RGB_BYTES_PER_PIXEL
            }
            PipeFormat::PlanarYuv(format) => {
                format.pixel_format.plane_layout(width, height).frame_bytes
            }
        }
    }
}

/// Everything [`choose_pipe_format`] reads, from the run's options and from
/// ffprobe.
pub(crate) struct PipeFormatInputs<'a> {
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

/// Whether the decode writes the source's own planar YUV to the pipe or the
/// packed RGB postkit converts itself.
///
/// Planar YUV only reaches grok's plugin untouched, so anything that reads or
/// rewrites the samples on the way keeps the run on packed RGB:
/// a subtitle burn, postkit's own colour transform, the decode LUT, and any
/// geometry work, which ffmpeg does in its own pixel format. A PSNR target
/// re-encodes a frame that overshoots the byte cap, which a batch cannot do, so
/// that run never reaches the plugin either.
pub(crate) fn choose_pipe_format(
    inputs: &PipeFormatInputs,
    plugin_takes_planar_yuv: bool,
) -> PipeFormat {
    let postkit_reads_the_samples = inputs.postkit_prepares_the_frame
        || matches!(
            inputs.source_colour,
            SourceColour::DisplayRgbIn(_) | SourceColour::DciLut(_)
        );
    let Some(pixel_format) = PlanarYuvPixelFormat::from_ffmpeg_name(&inputs.source.pix_fmt) else {
        return PipeFormat::PackedRgb(SampleOrder::Big);
    };
    if !inputs.accelerator_active
        || !plugin_takes_planar_yuv
        || inputs.quality_psnr.is_some()
        || postkit_reads_the_samples
        || filters_change_the_colour(inputs.filters)
    {
        return PipeFormat::PackedRgb(SampleOrder::Big);
    }
    PipeFormat::PlanarYuv(YuvFrameFormat {
        pixel_format,
        matrix: YuvMatrix::for_ffprobe_color_space(&inputs.source.color_space),
        full_range: inputs.source.color_range == FULL_RANGE_TAG,
    })
}

/// rgb48le when grok's accelerator plugin takes an interleaved 16-bit RGB batch
/// at this frame shape and these compression parameters, and rgb48be otherwise,
/// which postkit deinterleaves itself.
pub(crate) fn packed_rgb_sample_order(
    width: u32,
    height: u32,
    params: &crate::grok_encoder::CompressParams,
) -> SampleOrder {
    let shape_only = crate::grok_encoder::RawFrame::Packed {
        data: Vec::new(),
        order: SampleOrder::Little,
        width,
        height,
        precision: PACKED_RGB_PRECISION,
        index: 0,
    };
    if crate::grok_encoder::plugin_takes_frame(&shape_only, params) {
        SampleOrder::Little
    } else {
        SampleOrder::Big
    }
}

/// The pixel format one run's decode writes: what [`choose_pipe_format`] allows,
/// then grok's accelerator plugin asked about the frames that source would send
/// it.
///
/// The plugin is asked last, and only about a source everything else already
/// allows through, because each ask starts a batch. A run asks at most twice: a
/// declined YUV source falls back to packed RGB, which is asked about in turn.
pub(crate) fn pipe_format_for_run(
    inputs: &PipeFormatInputs,
    width: u32,
    height: u32,
    params: &crate::grok_encoder::CompressParams,
) -> PipeFormat {
    if let PipeFormat::PlanarYuv(format) = choose_pipe_format(inputs, true) {
        let shape_only = crate::grok_encoder::RawFrame::PlanarYuv {
            data: Vec::new(),
            format,
            width,
            height,
            index: 0,
        };
        if crate::grok_encoder::plugin_takes_frame(&shape_only, params) {
            return PipeFormat::PlanarYuv(format);
        }
    }
    PipeFormat::PackedRgb(packed_rgb_sample_order(width, height, params))
}

/// The whole filter chain one decode runs: the picture filters with the
/// detectors split off them, and for an 8-bit YUV source going to a packed RGB
/// pipe the 16-bit conversion ahead of everything.
///
/// The conversion goes first so the geometry work, the decode LUT and the
/// packing to rgb48 all run at 16 bits. `lut3d` and `haldclut` take RGB, and
/// ffmpeg would otherwise convert to 8-bit RGB for them.
pub(crate) fn decode_filter_chain(
    picture_filters: &str,
    pipe_format: PipeFormat,
    source_pix_fmt: &str,
) -> String {
    let convert_to_sixteen_bits = matches!(pipe_format, PipeFormat::PackedRgb(_))
        && is_eight_bit_yuv_pixel_format(source_pix_fmt);
    if !convert_to_sixteen_bits {
        return crate::picture_findings::with_detection_branch(picture_filters);
    }
    let mut converted = SIXTEEN_BIT_RGB_FILTER.to_string();
    if !picture_filters.is_empty() {
        converted.push(',');
        converted.push_str(picture_filters);
    }
    crate::picture_findings::with_detection_branch(&converted)
}

/// Every ffmpeg argument that goes before `-i` for a stream decode: what the
/// demuxer needs, the hardware decoder when the accelerator is running, plus the
/// input rate when the caller reads the source at a rate other than its own.
pub(crate) fn decode_input_args(
    decode_source: DecodeSource,
    read_source_at: Option<FrameRate>,
    hardware_decode: bool,
) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = decode_source
        .demuxer_args()
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
    if hardware_decode {
        args.extend(HARDWARE_DECODE_ARGS.iter().map(|arg| (*arg).to_string()));
    }
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

/// Every ffmpeg argument after `-i` for a stream decode: the filter chain, the
/// pixel format the frames reach postkit in and the raw output on stdout, plus a
/// frame limit for a window so ffmpeg stops at the window's end instead of
/// decoding the rest of the source.
pub(crate) fn decode_output_args(
    filters: &str,
    pipe_format: PipeFormat,
    frame_range: Option<FrameRange>,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-vf",
        filters,
        "-pix_fmt",
        pipe_format.ffmpeg_pixel_format(),
        "-f",
        "rawvideo",
        "-an",
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
    /// Target compression ratio (e.g. 10 for 10:1), ignored under `target_codestream_bytes`.
    pub compression_ratio: f64,
    #[serde(default)]
    pub target_codestream_bytes: Option<u64>,
    /// A PSNR target in dB that grok allocates layers by instead of the
    /// compression ratio. `codestream_byte_cap` still holds: a frame the target
    /// cannot fit under the cap is compressed again by rate.
    #[serde(default)]
    pub quality_psnr: Option<f64>,
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
    /// Colour the decoded frames carry, which decides the encoder transform.
    #[serde(default)]
    pub source_colour: SourceColour,
    /// The Rsiz the codestreams declare: cinema 2K, cinema 4K, or an IMF
    /// profile with its levels from [`crate::j2k::imf_rsiz`].
    #[serde(default = "default_rsiz")]
    pub rsiz: u16,
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
    /// than the whole sequence.
    #[serde(default)]
    pub codestream_byte_cap: Option<u64>,
}

/// The cinema profile a DCP picture declares, written as 2K or 4K by each
/// frame's raster.
pub fn default_rsiz() -> u16 {
    crate::grok_encoder::CompressParams::default().profile
}

impl Default for StreamEncodeOptions {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            output_dir: PathBuf::new(),
            compression_ratio: 10.0,
            target_codestream_bytes: None,
            quality_psnr: None,
            num_resolutions: 6,
            codeblock_size: 32,
            progression: "CPRL".to_string(),
            fps: FrameRate::default(),
            read_source_at: None,
            frame_range: None,
            source_colour: SourceColour::DisplayRgb,
            rsiz: default_rsiz(),
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
    /// Time the frame reader spent blocked waiting for the next frame, on
    /// ffmpeg's pipe or on the TIFF loader threads.
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

// ─── In-process stream encode (video → ffmpeg pipe → Grok FFI) ─────────────

/// Stream-encode a video file to J2K using in-process Grok FFI.
///
/// Uses ffmpeg to decode the video to raw 16-bit RGB frames, then compresses
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
    use crate::grok_encoder::{self, RawFrame};

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

    let params = match compress_params(opts) {
        Ok(params) => params,
        Err(e) => {
            return EncodeResult {
                success: false,
                error: e,
                ..Default::default()
            };
        }
    };

    let picture_filters = decode_filters(opts.fps, &opts.source_colour, &plan, opts.frame_range);
    let source = crate::probe::probe_pixel_format(&opts.input);
    let accelerator_active = grok_encoder::gpu_active();
    // the pipe format is decided on the chain the caller asked for, so the
    // conversion decode_filter_chain inserts cannot move the decision
    let pipe_format = pipe_format_for_run(
        &PipeFormatInputs {
            accelerator_active,
            quality_psnr: opts.quality_psnr,
            postkit_prepares_the_frame: opts.subtitle_burn.is_some(),
            source_colour: &opts.source_colour,
            filters: &crate::picture_findings::with_detection_branch(&picture_filters),
            source: &source,
        },
        width,
        height,
        &params,
    );
    let filters = decode_filter_chain(&picture_filters, pipe_format, &source.pix_fmt);
    tracing::info!(
        pixel_format = pipe_format.ffmpeg_pixel_format(),
        hardware_decode = accelerator_active,
        "decoding to the pipe"
    );
    let frame_size = pipe_format.frame_bytes(width, height);
    let input_args =
        match decode_input_args(opts.decode_source, opts.read_source_at, accelerator_active) {
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
        // the progress line carries no newline, so the reader would hold the
        // whole run in one string
        .arg("-nostats")
        .args(&input_args)
        .arg("-i")
        .arg(&opts.input)
        .args(decode_output_args(&filters, pipe_format, opts.frame_range))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    let detection_reader = ffmpeg
        .stderr
        .take()
        .map(crate::picture_findings::read_detection_lines);

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

    grok_encoder::initialize(0);

    let mut frame_buf = vec![0u8; frame_size];
    let mut frame_index: u64 = 0;
    let mut decode_read_to_end = false;
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
                ReadResult::Eof => {
                    decode_read_to_end = true;
                    return None;
                }
                ReadResult::Err(_) => return None,
            }

            let idx = frame_index;
            frame_index += 1;

            // Pass the bytes on as they came off the pipe: the encoder threads
            // deinterleave big-endian packed RGB into grok's component buffers,
            // and the other two formats reach the plugin untouched
            Some(match pipe_format {
                PipeFormat::PackedRgb(order) => RawFrame::Packed {
                    data: frame_buf.clone(),
                    order,
                    width,
                    height,
                    precision: PACKED_RGB_PRECISION,
                    index: idx,
                },
                PipeFormat::PlanarYuv(format) => RawFrame::PlanarYuv {
                    data: frame_buf.clone(),
                    format,
                    width,
                    height,
                    index: idx,
                },
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

    let picture_findings = crate::picture_findings::finish_detection(
        &mut ffmpeg,
        detection_reader,
        decode_read_to_end,
        opts.fps.as_f64(),
        result.frames_encoded,
    );

    EncodeResult {
        success: result.success,
        error: result.error,
        frames_encoded: result.frames_encoded,
        output_dir: opts.output_dir.clone(),
        picture_findings,
        pipe_pixel_format: Some(pipe_format.ffmpeg_pixel_format().to_string()),
    }
}

/// The compressor settings a stream encode asks for, with the source's frame
/// transform built once for the whole run.
fn compress_params(
    opts: &StreamEncodeOptions,
) -> Result<crate::grok_encoder::CompressParams, String> {
    let colour_transform = opts.source_colour.frame_transform()?;
    Ok(crate::grok_encoder::CompressParams {
        compression_ratio: opts.compression_ratio,
        target_codestream_bytes: opts.target_codestream_bytes,
        quality_psnr: opts.quality_psnr,
        codestream_byte_cap: opts.codestream_byte_cap,
        num_resolutions: opts.num_resolutions as u8,
        codeblock_size: opts.codeblock_size,
        // grok only sizes the per-frame byte budget from this, so the whole rate is enough
        edit_rate: opts.fps,
        profile: opts.rsiz,
        apply_xyz_transform: opts.source_colour.applies_xyz_transform(),
        source_preparation: crate::grok_encoder::SourcePreparation {
            subtitle_burn: opts.subtitle_burn.clone(),
            colour_transform,
        },
        ..crate::grok_encoder::CompressParams::default()
    })
}

// ─── TIFF sequence encode (loader threads → in-process grok) ──────────────

/// The most loader threads an in-process encode gets. Loading a frame costs a
/// fraction of compressing it, so a few loaders keep every encoder thread fed
/// while the encoders take every core.
const LOADER_THREADS_MAX: usize = 4;
/// Cores each loader thread is worth.
const CORES_PER_LOADER: usize = 4;

fn loader_thread_count() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(CORES_PER_LOADER);
    (cores / CORES_PER_LOADER).clamp(1, LOADER_THREADS_MAX)
}

/// Loads the frame at an index, on the loader thread that opened it.
pub type FrameLoader<'a> =
    Box<dyn FnMut(u64) -> Result<crate::grok_encoder::RawFrame, String> + 'a>;

/// Encode frames a caller loads itself through the in-process grok pipeline.
///
/// `open_loader` runs once on each of a small pool of loader threads and gives
/// back what that thread loads frames with, so a loader can hold a reader of
/// its own. The loaders run ahead of the encoder threads, which take every
/// core. Each frame gets the burn, the colour transform, the profile and the
/// byte cap a decoded video frame does, and its codestream reaches `mxf_feed`
/// the same way, so the frames can wrap as they encode. The codestreams are
/// numbered from zero by the index the frame was loaded at.
///
/// `opts.input`, `read_source_at`, `picture` and `decode_source` are ffmpeg's
/// and are not read: the loaded frames are the input. A `DciLut` source is
/// refused, since that LUT runs inside ffmpeg's decode.
pub fn encode_loaded_frames<'a, O, F>(
    frame_count: u64,
    open_loader: O,
    opts: &StreamEncodeOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    mut on_progress: F,
) -> EncodeResult
where
    O: Fn() -> Result<FrameLoader<'a>, String> + Sync,
    F: FnMut(StreamProgress),
{
    use crate::grok_encoder::{self, EncodePhase, RawFrame};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    let failure = |error: String| EncodeResult {
        success: false,
        error,
        output_dir: opts.output_dir.clone(),
        ..Default::default()
    };
    if frame_count == 0 {
        return failure("No frames to encode".to_string());
    }
    if let SourceColour::DciLut(lut) = &opts.source_colour {
        return failure(format!(
            "the HDR-to-DCI LUT {} runs inside ffmpeg's decode, which these frames never pass \
             through: encode from a video instead",
            lut.display()
        ));
    }
    let params = match compress_params(opts) {
        Ok(params) => params,
        Err(e) => return failure(e),
    };
    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return failure(format!("Failed to create output directory: {e}"));
    }

    grok_encoder::initialize(0);

    let encode_start = std::time::Instant::now();
    let phase_clocks = Arc::new(grok_encoder::PhaseClocks::default());
    let loader_count = loader_thread_count();
    let next_frame = AtomicU64::new(0);
    let load_error: Mutex<Option<String>> = Mutex::new(None);
    let record_load_error = |error: String| {
        let mut slot = load_error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(error);
        }
    };
    let (loaded_tx, loaded_rx) = std::sync::mpsc::sync_channel::<RawFrame>(loader_count);

    let result = std::thread::scope(|scope| {
        for _ in 0..loader_count {
            let loaded_tx = loaded_tx.clone();
            let next_frame = &next_frame;
            let load_error = &load_error;
            let record_load_error = &record_load_error;
            let open_loader = &open_loader;
            scope.spawn(move || {
                let mut load_frame = match open_loader() {
                    Ok(loader) => loader,
                    Err(e) => return record_load_error(e),
                };
                loop {
                    if cancel.load(Ordering::Relaxed) || load_error.lock().unwrap().is_some() {
                        break;
                    }
                    let index = next_frame.fetch_add(1, Ordering::Relaxed);
                    if index >= frame_count {
                        break;
                    }
                    match load_frame(index) {
                        Ok(frame) => {
                            if loaded_tx.send(frame).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            record_load_error(e);
                            break;
                        }
                    }
                }
            });
        }
        // the loaders hold the only other senders
        drop(loaded_tx);

        // a loader blocked on a full channel exits when the receiver drops
        grok_encoder::encode_pipeline_with_mxf_feed(
            &opts.output_dir,
            &params,
            frame_count,
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
                let wait_start = std::time::Instant::now();
                let frame = loaded_rx.recv().ok();
                phase_clocks.add(EncodePhase::DecoderWait, wait_start.elapsed());
                frame
            },
            |progress| {
                on_progress(StreamProgress {
                    frame: progress.frames_encoded,
                    total_frames: progress.total_frames,
                    fps: progress.fps,
                    elapsed_secs: encode_start.elapsed().as_secs_f64(),
                    decode_wait_secs: progress.decode_wait_secs,
                    prepare_secs: progress.prepare_secs,
                    encode_secs: progress.encode_secs,
                    write_secs: progress.write_secs,
                });
            },
        )
    });

    let load_error = load_error.into_inner().unwrap();
    let (success, error) = match (result.success, load_error) {
        (false, _) => (false, result.error),
        (true, Some(load_error)) => (false, load_error),
        (true, None) => (true, String::new()),
    };
    EncodeResult {
        success,
        error,
        frames_encoded: result.frames_encoded,
        output_dir: opts.output_dir.clone(),
        picture_findings: crate::picture_findings::PictureFindings::default(),
        pipe_pixel_format: None,
    }
}

/// rgb48le when grok's accelerator plugin takes an interleaved batch at the
/// first still's frame shape, and rgb48be otherwise. Asked once, before the
/// loaders start, because every frame of a batch has the shape its begin call
/// declared.
///
/// The run itself reports a bad parameter set or an unreadable first still, so
/// either only decides the byte order here.
fn tiff_sequence_sample_order(first: &Path, opts: &StreamEncodeOptions) -> SampleOrder {
    if !crate::grok_encoder::gpu_active() {
        return SampleOrder::Big;
    }
    let Ok(params) = compress_params(opts) else {
        return SampleOrder::Big;
    };
    let Ok(still) = crate::grok::load_tiff(first) else {
        return SampleOrder::Big;
    };
    packed_rgb_sample_order(still.width, still.height, &params)
}

/// Encode a TIFF sequence through [`encode_loaded_frames`]: each loader thread
/// reads the stills through `grok::load_tiff` into packed 16-bit RGB, so the
/// burn, the colour transform, the profile, the byte cap and the MXF feed all
/// apply to stills exactly as to decoded video. A still that cannot be read
/// fails the run by name.
pub fn encode_tiff_sequence_inprocess<F>(
    frames: &[PathBuf],
    opts: &StreamEncodeOptions,
    cancel: &Arc<AtomicBool>,
    pause: &Arc<AtomicBool>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    on_progress: F,
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    if frames.is_empty() {
        return EncodeResult {
            success: false,
            error: "No source image files found".to_string(),
            output_dir: opts.output_dir.clone(),
            ..Default::default()
        };
    }
    let order = tiff_sequence_sample_order(&frames[0], opts);
    let open_loader = || -> Result<FrameLoader<'_>, String> {
        Ok(Box::new(move |index: u64| {
            let path = &frames[index as usize];
            let tiff = crate::grok::load_tiff(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            Ok(tiff.into_rgb48_frame(index, order))
        }))
    };
    encode_loaded_frames(
        frames.len() as u64,
        open_loader,
        opts,
        cancel,
        pause,
        mxf_feed,
        on_progress,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "grok-ffi")]
    fn write_noise_tiff(path: &Path, seed: u32) {
        let (w, h) = (128u32, 128u32);
        let mut state = seed;
        let data: Vec<u8> = (0..(w * h * 3))
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        let file = std::fs::File::create(path).unwrap();
        tiff::encoder::TiffEncoder::new(std::io::BufWriter::new(file))
            .unwrap()
            .write_image::<tiff::encoder::colortype::RGB8>(w, h, &data)
            .unwrap();
    }

    #[cfg(feature = "grok-ffi")]
    fn encode_tiffs(
        frames: &[PathBuf],
        output: &Path,
        ratio: f64,
        cap: Option<u64>,
    ) -> EncodeResult {
        encode_tiff_sequence_inprocess(
            frames,
            &StreamEncodeOptions {
                output_dir: output.to_path_buf(),
                compression_ratio: ratio,
                codestream_byte_cap: cap,
                ..StreamEncodeOptions::default()
            },
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            None,
            |_| {},
        )
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_tiff_sequence_honours_the_ratio_and_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in");
        std::fs::create_dir(&input).unwrap();
        for i in 0..2u32 {
            write_noise_tiff(&input.join(format!("frame_{i:03}.tif")), 7 + i);
        }
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
        let result = encode_tiffs(&frames, &loose_dir, 10.0, None);
        assert!(result.success, "{}", result.error);
        let codestream = std::fs::read(loose_dir.join("frame_00000000.j2c")).unwrap();
        let header = crate::j2k::parse_j2k_header(&codestream).expect("codestream header");
        assert_eq!(
            header.profile, 0x0003,
            "a DCP image sequence has to declare the cinema 2K Rsiz"
        );
        assert_eq!(
            header.bit_depths,
            vec![12, 12, 12],
            "8-bit stills are widened to the 12 bits cinema requires"
        );
        crate::j2k::validate_dci_header(&header).expect("the AS-DCP wrap accepts the codestream");

        let tight_dir = dir.path().join("r40");
        let result = encode_tiffs(&frames, &tight_dir, 40.0, None);
        assert!(result.success, "{}", result.error);
        let loose = sizes(&loose_dir);
        let tight = sizes(&tight_dir);
        assert_eq!(loose.len(), 2);
        assert_eq!(tight.len(), 2);
        assert!(
            tight[1] < loose[0],
            "noise at 40:1 must be smaller than at 10:1, got {tight:?} vs {loose:?}"
        );

        let result = encode_tiffs(&frames, &dir.path().join("capped"), 10.0, Some(64));
        assert!(!result.success, "a 64 byte cap cannot hold a codestream");
        assert!(
            result.error.contains("per-frame cap"),
            "got: {}",
            result.error
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_tiff_sequence_compresses_only_the_frames_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in");
        std::fs::create_dir(&input).unwrap();
        for i in 0..6u32 {
            write_noise_tiff(&input.join(format!("frame_{i:03}.tif")), 11 + i);
        }
        let frames = find_source_frames(&input).unwrap();
        let window = FrameRange {
            first_frame: 2,
            frame_count: 3,
        }
        .window_of(&frames)
        .unwrap();

        let output = dir.path().join("out");
        let result = encode_tiffs(window, &output, 10.0, None);
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
            vec![
                "frame_00000000.j2c",
                "frame_00000001.j2c",
                "frame_00000002.j2c"
            ],
            "only the window's stills may be compressed, numbered from zero"
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn an_unreadable_still_fails_the_run_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in");
        std::fs::create_dir(&input).unwrap();
        write_noise_tiff(&input.join("frame_000.tif"), 3);
        let broken = input.join("frame_001.tif");
        std::fs::write(&broken, b"not really a tiff").unwrap();
        write_noise_tiff(&input.join("frame_002.tif"), 5);

        let frames = find_source_frames(&input).unwrap();
        let result = encode_tiffs(&frames, &dir.path().join("out"), 10.0, None);
        assert!(
            !result.success,
            "a still that cannot be read has to fail the run"
        );
        assert!(
            result.error.contains("frame_001.tif"),
            "the failure has to name the still: {}",
            result.error
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn the_lut_source_needs_ffmpeg_so_a_tiff_sequence_refuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let result = encode_tiff_sequence_inprocess(
            &[dir.path().join("frame_000.tif")],
            &StreamEncodeOptions {
                output_dir: dir.path().join("out"),
                source_colour: SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                ..StreamEncodeOptions::default()
            },
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            None,
            |_| {},
        );
        assert!(!result.success);
        assert!(result.error.contains("hdr_to_dci.cube"), "{}", result.error);
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
             lut3d=\\'/luts/hdr_to_dci.cube\\'"
        );
    }

    #[test]
    fn a_frame_window_stops_ffmpeg_at_its_end() {
        assert_eq!(
            decode_output_args("fps=24", PipeFormat::PackedRgb(SampleOrder::Big), None),
            vec![
                "-vf", "fps=24", "-pix_fmt", "rgb48be", "-f", "rawvideo", "-an", "pipe:1"
            ]
        );
        assert_eq!(
            decode_output_args(
                "fps=24",
                PipeFormat::PackedRgb(SampleOrder::Big),
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
            decode_input_args(DecodeSource::Video, None, false).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            decode_input_args(DecodeSource::Video, Some(FrameRate::whole(24)), false).unwrap(),
            vec!["-r", "24"]
        );
        assert_eq!(
            decode_input_args(DecodeSource::ImageList, None, false).unwrap(),
            vec!["-f", "concat", "-safe", "0"]
        );
        let refused = decode_input_args(DecodeSource::ImageList, Some(FrameRate::whole(24)), false)
            .unwrap_err();
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
            decode_filters(
                FrameRate::whole(24),
                &SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube")),
                &plan,
                None
            ),
            "yadif,fps=24,hqdn3d,format=gbrp16le,crop=1920:804:0:138,lut3d=\\'/luts/hdr_to_dci.cube\\'"
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

    fn source_pixel_format(pix_fmt: &str) -> crate::probe::PixelFormatInfo {
        crate::probe::PixelFormatInfo {
            pix_fmt: pix_fmt.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_yuv_pipe_needs_every_condition_at_once() {
        let source = source_pixel_format("yuv420p");
        let display_rgb = SourceColour::DisplayRgb;
        let accelerated = PipeFormatInputs {
            accelerator_active: true,
            quality_psnr: None,
            postkit_prepares_the_frame: false,
            source_colour: &display_rgb,
            filters: "fps=24",
            source: &source,
        };
        assert!(
            matches!(
                choose_pipe_format(&accelerated, true),
                PipeFormat::PlanarYuv(_)
            ),
            "an accelerated run over an untouched yuv420p source takes the planes"
        );

        assert_eq!(
            choose_pipe_format(&accelerated, false),
            PipeFormat::PackedRgb(SampleOrder::Big),
            "a plugin that will not take the planes leaves the run on RGB"
        );
        assert_eq!(
            choose_pipe_format(
                &PipeFormatInputs {
                    accelerator_active: false,
                    ..accelerated
                },
                true
            ),
            PipeFormat::PackedRgb(SampleOrder::Big),
            "nothing but the plugin reads the planes"
        );
        assert_eq!(
            choose_pipe_format(
                &PipeFormatInputs {
                    quality_psnr: Some(50.0),
                    ..accelerated
                },
                true
            ),
            PipeFormat::PackedRgb(SampleOrder::Big),
            "a PSNR target keeps the run off the batch"
        );
        assert_eq!(
            choose_pipe_format(
                &PipeFormatInputs {
                    postkit_prepares_the_frame: true,
                    ..accelerated
                },
                true
            ),
            PipeFormat::PackedRgb(SampleOrder::Big),
            "a burn needs samples postkit can write into"
        );
        assert_eq!(
            choose_pipe_format(
                &PipeFormatInputs {
                    filters: "fps=24,format=gbrp16le,crop=1998:1080:0:0",
                    ..accelerated
                },
                true
            ),
            PipeFormat::PackedRgb(SampleOrder::Big),
            "geometry work makes ffmpeg convert the picture itself"
        );
        assert_eq!(
            choose_pipe_format(
                &PipeFormatInputs {
                    filters: "fps=24,lut3d=\\'/luts/hdr_to_dci.cube\\'",
                    ..accelerated
                },
                true
            ),
            PipeFormat::PackedRgb(SampleOrder::Big),
            "a colour filter in the chain leaves nothing of the source's own planes"
        );
        assert!(
            matches!(
                choose_pipe_format(
                    &PipeFormatInputs {
                        filters: "yadif,fps=24,crop=1998:1080:0:0,scale=w=1998:h=1080,\
                                  split=2[picture][detect];[detect]blackdetect,nullsink;\
                                  [picture]null",
                        ..accelerated
                    },
                    true
                ),
                PipeFormat::PlanarYuv(_)
            ),
            "geometry and the detection branch keep the pixel format"
        );

        let wide_gamut = SourceColour::DisplayRgbIn(crate::colour::ColourSpace::P3);
        assert_eq!(
            choose_pipe_format(
                &PipeFormatInputs {
                    source_colour: &wide_gamut,
                    ..accelerated
                },
                true
            ),
            PipeFormat::PackedRgb(SampleOrder::Big),
            "postkit converts this source itself and needs RGB"
        );
        let lut = SourceColour::DciLut(PathBuf::from("/luts/hdr_to_dci.cube"));
        assert_eq!(
            choose_pipe_format(
                &PipeFormatInputs {
                    source_colour: &lut,
                    ..accelerated
                },
                true
            ),
            PipeFormat::PackedRgb(SampleOrder::Big),
            "lut3d puts RGB on the pipe"
        );

        for already_transformed in [SourceColour::AlreadyPq, SourceColour::KeepRgb] {
            assert!(
                matches!(
                    choose_pipe_format(
                        &PipeFormatInputs {
                            source_colour: &already_transformed,
                            ..accelerated
                        },
                        true
                    ),
                    PipeFormat::PlanarYuv(_)
                ),
                "{already_transformed:?} leaves the frame alone, so the planes can go through"
            );
        }

        let rgb_source = source_pixel_format("gbrp12le");
        assert_eq!(
            choose_pipe_format(
                &PipeFormatInputs {
                    source: &rgb_source,
                    ..accelerated
                },
                true
            ),
            PipeFormat::PackedRgb(SampleOrder::Big),
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
        };
        let chosen = choose_pipe_format(
            &PipeFormatInputs {
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
            PipeFormat::PlanarYuv(YuvFrameFormat {
                pixel_format: PlanarYuvPixelFormat::Yuv422p10le,
                matrix: YuvMatrix::Bt2020,
                full_range: true,
            })
        );
    }

    #[test]
    fn an_untagged_stream_converts_as_bt601() {
        assert_eq!(
            YuvMatrix::for_ffprobe_color_space("bt709"),
            YuvMatrix::Bt709
        );
        for name in ["bt2020nc", "bt2020c"] {
            assert_eq!(YuvMatrix::for_ffprobe_color_space(name), YuvMatrix::Bt2020);
        }
        for name in ["unknown", "smpte170m", "bt470bg", ""] {
            assert_eq!(
                YuvMatrix::for_ffprobe_color_space(name),
                YuvMatrix::Bt601,
                "{name} has to convert the way swscale converts it on the RGB path"
            );
        }
    }

    #[test]
    fn the_planes_of_one_frame_are_where_the_layout_says() {
        let layout = PlanarYuvPixelFormat::Yuv420p.plane_layout(64, 48);
        assert_eq!(
            layout,
            YuvPlaneLayout {
                luma_width: 64,
                luma_height: 48,
                chroma_width: 32,
                chroma_height: 24,
                bytes_per_sample: 1,
                luma_offset: 0,
                blue_chroma_offset: 64 * 48,
                red_chroma_offset: 64 * 48 + 32 * 24,
                frame_bytes: 64 * 48 + 2 * 32 * 24,
            }
        );

        let layout = PlanarYuvPixelFormat::Yuv422p.plane_layout(64, 48);
        assert_eq!(layout.chroma_height, 48, "4:2:2 keeps every chroma row");
        assert_eq!(layout.frame_bytes, 64 * 48 + 2 * 32 * 48);

        let layout = PlanarYuvPixelFormat::Yuv420p10le.plane_layout(64, 48);
        assert_eq!(layout.bytes_per_sample, 2, "10 bits arrive in two bytes");
        assert_eq!(layout.blue_chroma_offset, 64 * 48 * 2);
        assert_eq!(layout.frame_bytes, 2 * (64 * 48 + 2 * 32 * 24));

        let layout = PlanarYuvPixelFormat::Yuv422p10le.plane_layout(64, 48);
        assert_eq!(layout.frame_bytes, 2 * (64 * 48 + 2 * 32 * 48));

        let odd = PlanarYuvPixelFormat::Yuv420p.plane_layout(65, 49);
        assert_eq!(
            (odd.chroma_width, odd.chroma_height),
            (33, 25),
            "an odd raster rounds the chroma plane up"
        );
        assert_eq!(odd.frame_bytes, 65 * 49 + 2 * 33 * 25);
        assert_eq!(odd.red_chroma_offset, 65 * 49 + 33 * 25);
    }

    #[test]
    fn a_pipe_format_names_its_pixel_format_and_sizes_its_frame() {
        assert_eq!(
            PipeFormat::PackedRgb(SampleOrder::Big).ffmpeg_pixel_format(),
            "rgb48be"
        );
        assert_eq!(
            PipeFormat::PackedRgb(SampleOrder::Little).ffmpeg_pixel_format(),
            "rgb48le"
        );
        assert_eq!(
            PipeFormat::PackedRgb(SampleOrder::Big).frame_bytes(64, 48),
            64 * 48 * 6
        );
        assert_eq!(
            PipeFormat::PackedRgb(SampleOrder::Little).frame_bytes(64, 48),
            64 * 48 * 6
        );
        for (pixel_format, name) in [
            (PlanarYuvPixelFormat::Yuv420p, "yuv420p"),
            (PlanarYuvPixelFormat::Yuv422p, "yuv422p"),
            (PlanarYuvPixelFormat::Yuv420p10le, "yuv420p10le"),
            (PlanarYuvPixelFormat::Yuv422p10le, "yuv422p10le"),
        ] {
            assert_eq!(pixel_format.ffmpeg_name(), name);
            assert_eq!(
                PlanarYuvPixelFormat::from_ffmpeg_name(name),
                Some(pixel_format)
            );
            let pipe = PipeFormat::PlanarYuv(YuvFrameFormat {
                pixel_format,
                matrix: YuvMatrix::Bt709,
                full_range: false,
            });
            assert_eq!(pipe.ffmpeg_pixel_format(), name);
            assert_eq!(
                pipe.frame_bytes(64, 48),
                pixel_format.plane_layout(64, 48).frame_bytes,
                "the producer's buffer is the layout's frame"
            );
        }
        assert_eq!(PlanarYuvPixelFormat::from_ffmpeg_name("rgb48be"), None);
        assert_eq!(PlanarYuvPixelFormat::Yuv420p.bit_depth(), 8);
        assert_eq!(PlanarYuvPixelFormat::Yuv422p10le.bit_depth(), 10);
    }

    #[test]
    fn a_yuv_pipe_reaches_ffmpeg_as_its_own_pixel_format() {
        let pipe = PipeFormat::PlanarYuv(YuvFrameFormat {
            pixel_format: PlanarYuvPixelFormat::Yuv420p10le,
            matrix: YuvMatrix::Bt709,
            full_range: false,
        });
        assert_eq!(
            decode_output_args("fps=24", pipe, None),
            vec![
                "-vf",
                "fps=24",
                "-pix_fmt",
                "yuv420p10le",
                "-f",
                "rawvideo",
                "-an",
                "pipe:1"
            ]
        );
    }

    #[test]
    fn an_accelerated_decode_asks_for_the_hardware_decoder() {
        assert_eq!(
            decode_input_args(DecodeSource::Video, None, true).unwrap(),
            vec!["-hwaccel", "cuda"]
        );
        assert_eq!(
            decode_input_args(DecodeSource::ImageList, None, true).unwrap(),
            vec!["-f", "concat", "-safe", "0", "-hwaccel", "cuda"]
        );
        assert_eq!(
            decode_input_args(DecodeSource::Video, Some(FrameRate::whole(24)), true).unwrap(),
            vec!["-hwaccel", "cuda", "-r", "24"],
            "the hardware decoder goes before -i with the rest of the input arguments"
        );
        assert_eq!(
            decode_input_args(DecodeSource::Video, None, false).unwrap(),
            Vec::<String>::new(),
            "the CPU path's arguments do not change"
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
        let untouched = crate::picture_findings::with_detection_branch("fps=24");
        let planar_yuv = PipeFormat::PlanarYuv(YuvFrameFormat {
            pixel_format: PlanarYuvPixelFormat::Yuv420p,
            matrix: YuvMatrix::Bt601,
            full_range: false,
        });
        assert_eq!(
            decode_filter_chain("fps=24", planar_yuv, "yuv420p"),
            untouched,
            "the plugin converts the planes itself"
        );
        assert_eq!(
            decode_filter_chain(
                "fps=24",
                PipeFormat::PackedRgb(SampleOrder::Big),
                "yuv420p10le"
            ),
            untouched,
            "a 10-bit source already converts at full precision"
        );
        assert_eq!(
            decode_filter_chain("fps=24", PipeFormat::PackedRgb(SampleOrder::Big), "gbrp"),
            untouched,
            "an RGB source has no matrix to apply"
        );
        assert_eq!(
            decode_filter_chain("", PipeFormat::PackedRgb(SampleOrder::Little), "yuv420p"),
            crate::picture_findings::with_detection_branch("format=gbrp16le"),
            "a decode with no picture filters of its own still converts"
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

    /// How far off the exact conversion a sample on the pipe may sit, in codes
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

    /// The middle pixel of the first frame the decode writes to the pipe, which
    /// is inside the picture whether or not the chain pads it.
    fn middle_pixel_on_the_pipe(clip: &Path, filters: &str) -> [u16; 3] {
        let pipe_format = PipeFormat::PackedRgb(SampleOrder::Big);
        let run = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(decode_input_args(DecodeSource::Video, None, false).unwrap())
            .arg("-i")
            .arg(clip)
            .args(decode_output_args(filters, pipe_format, None))
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{filters}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let frame_bytes = pipe_format.frame_bytes(SOLID_COLOUR_SIZE, SOLID_COLOUR_SIZE);
        assert!(
            run.stdout.len() >= frame_bytes,
            "{filters}: the pipe carried {} bytes of a {frame_bytes} byte frame",
            run.stdout.len()
        );
        let middle = SOLID_COLOUR_SIZE as usize / 2;
        let offset = (middle * SOLID_COLOUR_SIZE as usize + middle) * PACKED_RGB_BYTES_PER_PIXEL;
        [0, 1, 2].map(|component| {
            let at = offset + component * 2;
            u16::from_be_bytes([run.stdout[at], run.stdout[at + 1]])
        })
    }

    #[test]
    fn an_eight_bit_yuv_source_reaches_the_pipe_at_the_exact_colour() {
        let dir = tempfile::tempdir().unwrap();
        let (clip, sample) = solid_colour_clip(dir.path(), "0x4080c0");
        let source = crate::probe::probe_pixel_format(&clip);
        assert_eq!(source.pix_fmt, "yuv420p");
        let exact = exact_bt601_rgb(sample);
        let packed_rgb = PipeFormat::PackedRgb(SampleOrder::Big);

        let plain = crate::picture_processing::PictureProcessing::default()
            .plan(SOLID_COLOUR_SIZE, SOLID_COLOUR_SIZE)
            .unwrap();
        // what a subtitle burn to a DCI raster decodes through: postkit reads
        // the samples, so the pipe is packed RGB and the geometry runs on it
        let burn = crate::picture_processing::PictureProcessing {
            fit: Some(crate::picture_processing::Fit {
                box_width: 48,
                box_height: 48,
                raster_width: SOLID_COLOUR_SIZE,
                raster_height: SOLID_COLOUR_SIZE,
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
                decode_filters(
                    FrameRate::whole(24),
                    &SourceColour::DisplayRgb,
                    &plain,
                    None,
                ),
            ),
            (
                "burn",
                decode_filters(FrameRate::whole(24), &SourceColour::DisplayRgb, &burn, None),
            ),
            (
                "lut",
                decode_filters(
                    FrameRate::whole(24),
                    &SourceColour::DciLut(lut),
                    &plain,
                    None,
                ),
            ),
        ];
        for (name, picture_filters) in chains {
            let filters = decode_filter_chain(&picture_filters, packed_rgb, &source.pix_fmt);
            assert!(
                filters.starts_with("format=gbrp16le,"),
                "{name}: the conversion has to run before everything else: {filters}"
            );
            let pixel = middle_pixel_on_the_pipe(&clip, &filters);
            for (component, (measured, exact)) in pixel.iter().zip(exact).enumerate() {
                let error = f64::from(*measured) - exact;
                assert!(
                    error.abs() <= EXACT_TOLERANCE_CODES,
                    "{name}: component {component} came off the pipe at {measured} against the \
                     exact {exact:.0}, {error:.0} codes out"
                );
            }
        }
    }
}
