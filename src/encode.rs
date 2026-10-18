use crate::grok_encoder::SampleOrder;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(feature = "ffmpeg")]
pub(crate) mod stream_encode;
#[cfg(feature = "ffmpeg")]
pub use stream_encode::{stream_encode_inprocess, stream_encode_inprocess_with_mxf_feed};

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
    /// The pixel format the decode handed the encoder, `None` for a run that
    /// decoded nothing through FFmpeg. A planar YUV name here is a run whose
    /// frames reached grok's accelerator plugin unconverted.
    #[serde(default)]
    pub encoder_input_pixel_format: Option<String>,
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
    PictureMxf,
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
            "mxf" if crate::mxf_unwrap::is_picture_mxf(path) => InputType::PictureMxf,
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

/// The directory holding an image sequence, whether the input names the
/// directory itself or one frame in it.
pub(crate) fn sequence_directory(input: &Path) -> PathBuf {
    if input.is_dir() {
        input.to_path_buf()
    } else {
        input.parent().unwrap_or(input).to_path_buf()
    }
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
        InputType::PictureMxf => {
            let info = crate::mxf_unwrap::probe_picture_mxf(picture)?;
            return Ok((info.width, info.height));
        }
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

// ─── Streaming encode (FFmpeg libraries → raw frames → in-process grok) ─────

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What colour the source frames carry when they reach the J2K compressor.
///
/// The compressor's own DCDM X'Y'Z' transform is applied if and only if this is
/// `DisplayRgb`, so essence that a caller later labels ST 2084 PQ or RGB can
/// never hold frames the encoder transformed itself.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
    // postkit converts these frames to the Rec.709 RGB an App 2E picture carries
    KeepRgbFrom(crate::colour::ColourSpace),
    // the LUT runs in ffmpeg's decode and its output is Rec.709 RGB
    KeepRgbAfterLut(PathBuf),
    // postkit writes the addendum's X"Y"Z" itself, so the compressor transform stays off
    HdrDcdm {
        source: crate::colour::HdrSource,
        // the grade's peak luminance, e.g. MaxCLL
        source_peak_nits: f32,
    },
}

impl SourceColour {
    /// Whether the compressor has to run the DCDM X'Y'Z' transform.
    pub fn applies_xyz_transform(&self) -> bool {
        matches!(self, SourceColour::DisplayRgb)
    }

    /// The transform postkit runs over each frame before compression, built once
    /// for the whole run. Errs for a space no 3x3 matrix reaches the output
    /// space from.
    pub fn frame_transform(
        &self,
    ) -> Result<Option<Arc<crate::colour::FrameColourTransform>>, String> {
        let transform = match self {
            SourceColour::DisplayRgbIn(space) => crate::colour::FrameColourTransform::ToXyz(
                crate::colour::DcdmTransform::to_xyz(*space)?,
            ),
            SourceColour::KeepRgbFrom(space) => crate::colour::FrameColourTransform::ToRec709(
                crate::colour::Rec709Transform::from_space(*space)?,
            ),
            SourceColour::HdrDcdm {
                source,
                source_peak_nits,
            } => crate::colour::FrameColourTransform::ToHdrXyz(Box::new(
                crate::colour::HdrDcdmTransform::new(*source, *source_peak_nits)?,
            )),
            SourceColour::DisplayRgb
            | SourceColour::DciLut(_)
            | SourceColour::AlreadyPq
            | SourceColour::KeepRgb
            | SourceColour::KeepRgbAfterLut(_) => return Ok(None),
        };
        Ok(Some(Arc::new(transform)))
    }

    pub fn decode_lut(&self) -> Option<&Path> {
        match self {
            SourceColour::DciLut(lut) | SourceColour::KeepRgbAfterLut(lut) => Some(lut),
            SourceColour::DisplayRgb
            | SourceColour::DisplayRgbIn(_)
            | SourceColour::AlreadyPq
            | SourceColour::KeepRgb
            | SourceColour::KeepRgbFrom(_)
            | SourceColour::HdrDcdm { .. } => None,
        }
    }

    pub fn hdr_source(&self) -> Option<crate::colour::HdrSource> {
        match self {
            SourceColour::HdrDcdm { source, .. } => Some(*source),
            SourceColour::DisplayRgb
            | SourceColour::DisplayRgbIn(_)
            | SourceColour::DciLut(_)
            | SourceColour::AlreadyPq
            | SourceColour::KeepRgb
            | SourceColour::KeepRgbFrom(_)
            | SourceColour::KeepRgbAfterLut(_) => None,
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
}

/// How one decode's picture filters are spelled.
pub enum PictureFilters<'a> {
    /// A picture plan, decoded at this frame rate over this window.
    Planned {
        plan: &'a crate::picture_processing::PicturePlan,
        fps: FrameRate,
        frame_range: Option<FrameRange>,
        /// ffmpeg filters run after the plan, on the fitted and windowed frame
        extra_filter: Option<&'a str>,
    },
    /// A caller's own chain, which changes neither the frame size nor the frame
    /// count.
    Given(&'a str),
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

// ─── the pixel format the decode hands the encoder ────────────────────────────

/// Packed 16-bit RGB: three components per pixel, six bytes. postkit
/// deinterleaves the big-endian layout itself and hands the little-endian one to
/// grok's accelerator plugin as it arrives.
const PACKED_RGB_BIG_ENDIAN_PIXEL_FORMAT: &str = "rgb48be";
const PACKED_RGB_LITTLE_ENDIAN_PIXEL_FORMAT: &str = "rgb48le";
pub(crate) const PACKED_RGB_BYTES_PER_PIXEL: usize = 6;
pub(crate) const PACKED_RGB_PRECISION: u8 = 16;

/// A planar YUV pixel format postkit passes to grok's accelerator plugin as it
/// comes off the decode. The name is ffprobe's and `-pix_fmt`'s.
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

    /// Bytes one sample takes in a decoded frame. A 10-bit sample arrives in a
    /// little-endian 16-bit container.
    pub fn bytes_per_sample(self) -> usize {
        if self.properties().bit_depth > 8 {
            2
        } else {
            1
        }
    }

    /// Where the three planes of one frame sit in the bytes the decode copies
    /// out.
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

/// Where each plane of one frame starts in the bytes the decode copies out, and
/// how big it is. The copy pads nothing, so each plane's row stride
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
    /// packed RGB path, so both encoder input formats give the same picture.
    pub fn for_ffprobe_color_space(color_space: &str) -> Self {
        match color_space {
            "bt709" => Self::Bt709,
            "bt2020nc" | "bt2020c" => Self::Bt2020,
            _ => Self::Bt601,
        }
    }
}

/// Everything about the decoded planar YUV frames that grok's plugin needs
/// and that is the same for every frame of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct YuvFrameFormat {
    pub pixel_format: PlanarYuvPixelFormat,
    pub matrix: YuvMatrix,
    /// whether the samples use the whole code value range rather than the
    /// studio range
    pub full_range: bool,
}

/// What the decode hands the encoder for a stream encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderInputFormat {
    /// Packed 16-bit RGB in that byte order. The compressor converts the
    /// colour either way: postkit deinterleaves the big-endian layout, and
    /// grok's accelerator plugin takes the little-endian one interleaved.
    PackedRgb(SampleOrder),
    /// The source's own planar YUV, handed to grok's accelerator plugin as it
    /// arrives: the plugin upsamples the chroma and converts the colour.
    PlanarYuv(YuvFrameFormat),
}

impl EncoderInputFormat {
    pub fn ffmpeg_pixel_format(&self) -> &'static str {
        match self {
            EncoderInputFormat::PackedRgb(SampleOrder::Big) => PACKED_RGB_BIG_ENDIAN_PIXEL_FORMAT,
            EncoderInputFormat::PackedRgb(SampleOrder::Little) => {
                PACKED_RGB_LITTLE_ENDIAN_PIXEL_FORMAT
            }
            EncoderInputFormat::PlanarYuv(format) => format.pixel_format.ffmpeg_name(),
        }
    }

    /// Bytes one decoded frame takes.
    pub fn frame_bytes(&self, width: u32, height: u32) -> usize {
        match self {
            EncoderInputFormat::PackedRgb(_) => {
                width as usize * height as usize * PACKED_RGB_BYTES_PER_PIXEL
            }
            EncoderInputFormat::PlanarYuv(format) => {
                format.pixel_format.plane_layout(width, height).frame_bytes
            }
        }
    }
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
    /// ffmpeg filters run after `picture` and the frame window, e.g. a fade,
    /// which sees the fitted frame and timestamps counted from the window start.
    #[serde(default)]
    pub extra_picture_filter: Option<String>,
    /// Subtitles burnt into each decoded frame before it is compressed. Not
    /// serialised: it carries a live font database, so a stored job names the
    /// subtitle file and rebuilds it.
    #[serde(skip)]
    pub subtitle_burn: Option<std::sync::Arc<crate::subtitle_raster::SubtitleBurn>>,
    /// A visible mark burnt over the subtitles on every frame. Not serialised
    /// for the same reason the subtitle burn is not: a stored job names the
    /// text and rebuilds it.
    #[serde(skip)]
    pub watermark: Option<std::sync::Arc<crate::subtitle_raster::SubtitleBurn>>,
    /// Per-codestream byte cap, e.g. the DCI HDR Addendum's raised cap. Each
    /// codestream is checked as it is written and the first one over the cap
    /// fails the encode there, so a bitrate set too high costs one frame rather
    /// than the whole sequence.
    #[serde(default)]
    pub codestream_byte_cap: Option<u64>,
    /// Encoder threads the pipeline runs, 0 for one per available CPU
    #[serde(default)]
    pub encode_threads: u32,
    #[serde(default)]
    pub detect_picture_findings: bool,
    #[serde(default)]
    pub first_output_frame: u64,
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
            extra_picture_filter: None,
            subtitle_burn: None,
            watermark: None,
            codestream_byte_cap: None,
            encode_threads: 0,
            detect_picture_findings: false,
            first_output_frame: 0,
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
    /// Time the frame reader spent blocked waiting for the next frame, on the
    /// decoder or on the TIFF loader threads.
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
    if source == DecodeSource::Video
        && let Some(info) = crate::probe::probe_video(input)
    {
        return (info.width, info.height, u64::from(info.total_frames));
    }

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
                crate::probe::displayed_raster(
                    input,
                    demuxer,
                    parts[0].parse().unwrap_or(0),
                    parts[1].parse().unwrap_or(0),
                )
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

/// The compressor settings a stream encode asks for, with the source's frame
/// transform built once for the whole run.
pub(crate) fn compress_params(
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
        encode_threads: opts.encode_threads,
        detect_picture_findings: opts.detect_picture_findings,
        first_output_frame: opts.first_output_frame,
        source_preparation: crate::grok_encoder::SourcePreparation {
            subtitle_burn: opts.subtitle_burn.clone(),
            watermark: opts.watermark.clone(),
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
/// and are not read: the loaded frames are the input. A source colour carrying
/// a decode LUT is refused, since that LUT runs inside ffmpeg's decode.
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
    if let Some(lut) = opts.source_colour.decode_lut() {
        return failure(format!(
            "the 3D LUT {} runs inside ffmpeg's decode, which these frames never pass \
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
            |_| {
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
        encoder_input_pixel_format: None,
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
    fn an_imf_source_converted_to_rec709_leaves_the_compressor_transform_off() {
        let rsiz = crate::j2k::imf_rsiz(
            crate::j2k::ImfProfile::Imf4k,
            crate::j2k::ImfLevels {
                main_level: 6,
                sub_level: 5,
            },
        );
        let params = compress_params(&StreamEncodeOptions {
            source_colour: SourceColour::KeepRgbFrom(crate::colour::ColourSpace::P3),
            rsiz,
            ..StreamEncodeOptions::default()
        })
        .unwrap();
        assert!(
            !params.apply_xyz_transform,
            "an App 2E picture is RGB, so grok must not convert it"
        );
        assert_eq!(params.profile, rsiz, "the IMF Rsiz reaches the codestreams");
        assert!(
            matches!(
                params.source_preparation.colour_transform.as_deref(),
                Some(crate::colour::FrameColourTransform::ToRec709(_))
            ),
            "postkit converts the frames to Rec.709 itself"
        );

        let kept = compress_params(&StreamEncodeOptions {
            source_colour: SourceColour::KeepRgb,
            rsiz,
            ..StreamEncodeOptions::default()
        })
        .unwrap();
        assert!(!kept.apply_xyz_transform);
        assert!(kept.source_preparation.colour_transform.is_none());
    }

    #[test]
    fn loaded_frames_refuse_a_lut_that_runs_in_the_decode() {
        let result = encode_loaded_frames(
            1,
            || Err::<FrameLoader, String>("no loader is opened".to_string()),
            &StreamEncodeOptions {
                source_colour: SourceColour::KeepRgbAfterLut(PathBuf::from("/luts/to_rec709.cube")),
                ..StreamEncodeOptions::default()
            },
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            None,
            |_| {},
        );
        assert!(!result.success);
        assert!(result.error.contains("to_rec709.cube"), "{}", result.error);
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
    fn only_a_jpeg_2000_picture_mxf_is_a_picture_mxf() {
        let dir = tempfile::tempdir().unwrap();
        let (picture, _) = crate::mxf_unwrap::tests::wrapped_picture_mxf(dir.path());
        assert_eq!(detect_input_type(&picture), InputType::PictureMxf);
        let upper_case = dir.path().join("UPPER_CASE.MXF");
        std::fs::copy(&picture, &upper_case).unwrap();
        assert_eq!(detect_input_type(&upper_case), InputType::PictureMxf);

        let mpeg2 = dir.path().join("mpeg2.mxf");
        let output = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=s=720x576:r=25"])
            .args(["-frames:v", "2", "-c:v", "mpeg2video"])
            .arg(&mpeg2)
            .output()
            .expect("this test needs the ffmpeg program on PATH to write an MPEG-2 MXF");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(detect_input_type(&mpeg2), InputType::Video);

        assert_eq!(
            detect_input_type(Path::new("/clips/a.mov")),
            InputType::Video
        );
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

    pub(super) fn hdr10_source() -> SourceColour {
        SourceColour::HdrDcdm {
            source: crate::colour::HdrSource::Hdr10,
            source_peak_nits: crate::colour::HdrSource::DEFAULT_PEAK_NITS,
        }
    }

    #[test]
    fn an_hdr_master_converts_to_the_addendum_xyz_before_compression() {
        let source = hdr10_source();
        assert!(
            !source.applies_xyz_transform(),
            "postkit writes the X\"Y\"Z\" itself, so the compressor must not"
        );
        assert!(source.decode_lut().is_none());
        assert!(
            matches!(
                source.frame_transform().unwrap().as_deref(),
                Some(crate::colour::FrameColourTransform::ToHdrXyz(_))
            ),
            "an HDR master carries the addendum transform"
        );
        assert!(
            SourceColour::HdrDcdm {
                source: crate::colour::HdrSource::Hlg,
                source_peak_nits: 0.0,
            }
            .frame_transform()
            .is_err(),
            "a peak below the volume floor has nothing to roll off from"
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
            EncoderInputFormat::PackedRgb(SampleOrder::Big).ffmpeg_pixel_format(),
            "rgb48be"
        );
        assert_eq!(
            EncoderInputFormat::PackedRgb(SampleOrder::Little).ffmpeg_pixel_format(),
            "rgb48le"
        );
        assert_eq!(
            EncoderInputFormat::PackedRgb(SampleOrder::Big).frame_bytes(64, 48),
            64 * 48 * 6
        );
        assert_eq!(
            EncoderInputFormat::PackedRgb(SampleOrder::Little).frame_bytes(64, 48),
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
            let input_format = EncoderInputFormat::PlanarYuv(YuvFrameFormat {
                pixel_format,
                matrix: YuvMatrix::Bt709,
                full_range: false,
            });
            assert_eq!(input_format.ffmpeg_pixel_format(), name);
            assert_eq!(
                input_format.frame_bytes(64, 48),
                pixel_format.plane_layout(64, 48).frame_bytes,
                "the producer's buffer is the layout's frame"
            );
        }
        assert_eq!(PlanarYuvPixelFormat::from_ffmpeg_name("rgb48be"), None);
        assert_eq!(PlanarYuvPixelFormat::Yuv420p.bit_depth(), 8);
        assert_eq!(PlanarYuvPixelFormat::Yuv422p10le.bit_depth(), 10);
    }
}
