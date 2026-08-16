//! Source picture processing for the encode pipeline: per-side crop, black
//! border detection, deinterlace, rotate, flip, denoise, and fitting the result
//! into a target raster.
//!
//! Every operation is planned as pure arithmetic first ([`PictureProcessing::plan`])
//! and only then spelled as ffmpeg filters, so the sizes a caller shows in a GUI
//! and the sizes the decode really produces come from one place. The plan also
//! carries the frame size the encoder has to slice out of ffmpeg's raw stream.
//!
//! Nothing here composites: a subtitle burn and the source colour transform run
//! on the decoded frame, after these filters, so they already see the processed
//! picture at its output size.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::encode::DecodeSource;

/// Planar 16-bit RGB for every geometry step. A chroma-subsampled source would
/// round a crop offset to the chroma grid and mix pad black with neighbouring
/// chroma, so the picture is unpacked to full-resolution components first.
const GEOMETRY_PIXEL_FORMAT: &str = "gbrp16le";

/// swscale algorithm for the fit scale.
const SCALE_ALGORITHM: &str = "lanczos";

/// Colour of the padding around a fitted picture.
const PAD_COLOUR: &str = "black";

/// ffmpeg filter that turns fields into progressive frames.
const DEINTERLACE_FILTER: &str = "yadif";

/// ffmpeg denoiser, at its own defaults.
const DENOISE_FILTER: &str = "hqdn3d";

/// Detected crop edges are a multiple of this, which is the finest cropdetect
/// offers that still keeps both dimensions even.
const CROPDETECT_ROUND: u32 = 2;

/// Recalculate the detected rectangle every frame, so a seeked single frame
/// reports its own content rather than an accumulated one.
const CROPDETECT_RESET: u32 = 1;

/// Take the first frame after each seek: cropdetect otherwise ignores two.
const CROPDETECT_SKIP: u32 = 0;

/// Round down to an even number, which every DCI raster and every subsampled
/// intermediate needs.
fn floor_to_even(value: u32) -> u32 {
    value & !1
}

/// Pixels removed from each side of the source, in source pixels and in the
/// source's own orientation, so a crop is expressed before any rotation or flip.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crop {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

impl Crop {
    /// Whether this crop removes nothing.
    pub fn is_none(&self) -> bool {
        self.left == 0 && self.right == 0 && self.top == 0 && self.bottom == 0
    }

    /// The centred crop that brings a source to the given aspect ratio, keeping
    /// both remaining dimensions even. This is the fill crop: content is cut
    /// away rather than padded, so the picture reaches the aspect full frame.
    pub fn to_aspect(
        source_width: u32,
        source_height: u32,
        aspect_width: u32,
        aspect_height: u32,
    ) -> Crop {
        // a zero source or a zero aspect has no crop to compute, and
        // `PictureProcessing::plan` is where a zero source fails
        if source_width == 0 || source_height == 0 || aspect_width == 0 || aspect_height == 0 {
            return Crop::default();
        }
        let source_ratio = source_width as f64 / source_height as f64;
        let target_ratio = aspect_width as f64 / aspect_height as f64;
        if source_ratio > target_ratio {
            let kept =
                floor_to_even((source_height as f64 * target_ratio) as u32).min(source_width);
            let total = source_width - kept;
            Crop {
                left: total / 2,
                right: total - total / 2,
                top: 0,
                bottom: 0,
            }
        } else {
            let kept =
                floor_to_even((source_width as f64 / target_ratio) as u32).min(source_height);
            let total = source_height - kept;
            Crop {
                left: 0,
                right: 0,
                top: total / 2,
                bottom: total - total / 2,
            }
        }
    }
}

/// Whole-quarter-turn rotation of the picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    None,
    Clockwise90,
    Half,
    CounterClockwise90,
}

impl Rotation {
    /// Size after the turn.
    fn applied_to(&self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Rotation::None | Rotation::Half => (width, height),
            Rotation::Clockwise90 | Rotation::CounterClockwise90 => (height, width),
        }
    }

    /// ffmpeg filter items for the turn. 180 degrees is two clockwise
    /// transposes, which transpose has no direction of its own for.
    fn filters(&self) -> Vec<String> {
        match self {
            Rotation::None => Vec::new(),
            Rotation::Clockwise90 => vec!["transpose=clock".to_string()],
            Rotation::Half => vec!["transpose=clock".to_string(), "transpose=clock".to_string()],
            Rotation::CounterClockwise90 => vec!["transpose=cclock".to_string()],
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Rotation::None => "none",
            Rotation::Clockwise90 => "clockwise 90",
            Rotation::Half => "180",
            Rotation::CounterClockwise90 => "counter-clockwise 90",
        }
    }
}

/// Fit the processed picture into a box and centre it on a raster.
///
/// The picture is scaled to the largest size that fits the box with its aspect
/// ratio kept, then centred on a raster of `raster_width` x `raster_height` with
/// black around it. Nothing is ever cropped here, and nothing ever grows past
/// the box. A source smaller than the box is scaled up to it, which is what a
/// DCI raster needs: the encoded picture has to be the raster the CPL declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fit {
    pub box_width: u32,
    pub box_height: u32,
    pub raster_width: u32,
    pub raster_height: u32,
}

impl Fit {
    /// Size the picture is scaled to before it is centred.
    fn scaled_size(&self, width: u32, height: u32) -> Result<(u32, u32), String> {
        if self.box_width == 0 || self.box_height == 0 {
            return Err(format!(
                "fit box is {}x{}, which holds no picture",
                self.box_width, self.box_height
            ));
        }
        if self.box_width > self.raster_width || self.box_height > self.raster_height {
            return Err(format!(
                "fit box {}x{} is larger than the {}x{} raster it has to sit on",
                self.box_width, self.box_height, self.raster_width, self.raster_height
            ));
        }
        let ratio =
            (self.box_width as f64 / width as f64).min(self.box_height as f64 / height as f64);
        let scaled_width = floor_to_even((width as f64 * ratio) as u32).min(self.box_width);
        let scaled_height = floor_to_even((height as f64 * ratio) as u32).min(self.box_height);
        if scaled_width == 0 || scaled_height == 0 {
            return Err(format!(
                "fitting {width}x{height} into {}x{} leaves a {scaled_width}x{scaled_height} picture",
                self.box_width, self.box_height
            ));
        }
        Ok((scaled_width, scaled_height))
    }
}

/// Everything done to the source picture before it is compressed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PictureProcessing {
    pub deinterlace: bool,
    pub denoise: bool,
    pub crop: Crop,
    pub rotation: Rotation,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
    pub fit: Option<Fit>,
}

impl PictureProcessing {
    /// Whether this leaves the picture exactly as decoded.
    pub fn is_identity(&self) -> bool {
        !self.deinterlace
            && !self.denoise
            && self.crop.is_none()
            && self.rotation == Rotation::None
            && !self.flip_horizontal
            && !self.flip_vertical
            && self.fit.is_none()
    }

    /// Work out every intermediate size and the ffmpeg filter chain for a source
    /// of the given size. Pure arithmetic: no ffmpeg is run.
    pub fn plan(&self, source_width: u32, source_height: u32) -> Result<PicturePlan, String> {
        if source_width == 0 || source_height == 0 {
            return Err(format!(
                "source raster is {source_width}x{source_height}, so there is no picture to process"
            ));
        }
        let horizontal_crop = self.crop.left.saturating_add(self.crop.right);
        let vertical_crop = self.crop.top.saturating_add(self.crop.bottom);
        if horizontal_crop >= source_width || vertical_crop >= source_height {
            return Err(format!(
                "crop {}/{}/{}/{} leaves nothing of a {source_width}x{source_height} source",
                self.crop.left, self.crop.right, self.crop.top, self.crop.bottom
            ));
        }
        let cropped_width = source_width - horizontal_crop;
        let cropped_height = source_height - vertical_crop;
        let (rotated_width, rotated_height) =
            self.rotation.applied_to(cropped_width, cropped_height);

        let (scaled_width, scaled_height, output_width, output_height) = match &self.fit {
            Some(fit) => {
                let (scaled_width, scaled_height) =
                    fit.scaled_size(rotated_width, rotated_height)?;
                (
                    scaled_width,
                    scaled_height,
                    fit.raster_width,
                    fit.raster_height,
                )
            }
            None => (rotated_width, rotated_height, rotated_width, rotated_height),
        };
        let pad_left = (output_width - scaled_width) / 2;
        let pad_top = (output_height - scaled_height) / 2;

        let scale_needed = (scaled_width, scaled_height) != (rotated_width, rotated_height);
        let pad_needed = (output_width, output_height) != (scaled_width, scaled_height);
        let geometry_needed = !self.crop.is_none()
            || self.rotation != Rotation::None
            || self.flip_horizontal
            || self.flip_vertical
            || scale_needed
            || pad_needed;

        let mut filters = Vec::new();
        if self.deinterlace {
            filters.push(DEINTERLACE_FILTER.to_string());
        }
        let fps_position = filters.len();
        if self.denoise {
            filters.push(DENOISE_FILTER.to_string());
        }
        if geometry_needed {
            filters.push(format!("format={GEOMETRY_PIXEL_FORMAT}"));
        }
        if !self.crop.is_none() {
            filters.push(format!(
                "crop={cropped_width}:{cropped_height}:{}:{}",
                self.crop.left, self.crop.top
            ));
        }
        filters.extend(self.rotation.filters());
        if self.flip_horizontal {
            filters.push("hflip".to_string());
        }
        if self.flip_vertical {
            filters.push("vflip".to_string());
        }
        if scale_needed {
            filters.push(format!(
                "scale=w={scaled_width}:h={scaled_height}:flags={SCALE_ALGORITHM}"
            ));
        }
        if pad_needed {
            filters.push(format!(
                "pad=w={output_width}:h={output_height}:x={pad_left}:y={pad_top}:color={PAD_COLOUR}"
            ));
        }

        Ok(PicturePlan {
            crop: self.crop,
            rotation: self.rotation,
            cropped_width,
            cropped_height,
            rotated_width,
            rotated_height,
            scaled_width,
            scaled_height,
            output_width,
            output_height,
            pad_left,
            pad_top,
            filters,
            fps_position,
        })
    }
}

/// The sizes and the ffmpeg filters one source size resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PicturePlan {
    pub crop: Crop,
    pub rotation: Rotation,
    pub cropped_width: u32,
    pub cropped_height: u32,
    pub rotated_width: u32,
    pub rotated_height: u32,
    pub scaled_width: u32,
    pub scaled_height: u32,
    /// Size of the frame ffmpeg emits, which is what the encoder slices and what
    /// the codestream declares.
    pub output_width: u32,
    pub output_height: u32,
    /// Where the scaled picture sits on the output raster.
    pub pad_left: u32,
    pub pad_top: u32,
    /// The `-vf` items in order, ready to join with ','.
    pub filters: Vec<String>,
    /// Where the frame rate filter belongs in `filters`: deinterlacing turns
    /// fields into frames, so it has to run before any rate conversion, and
    /// everything else runs after it.
    pub fps_position: usize,
}

impl PicturePlan {
    /// Whether the decode produces the source raster untouched.
    pub fn is_identity(&self) -> bool {
        self.filters.is_empty()
    }

    /// One line naming every step, for a log or a crop indicator.
    pub fn describe(&self) -> String {
        format!(
            "crop {}/{}/{}/{} to {}x{}, rotate {}, scale to {}x{}, pad to {}x{} at ({},{})",
            self.crop.left,
            self.crop.right,
            self.crop.top,
            self.crop.bottom,
            self.cropped_width,
            self.cropped_height,
            self.rotation.label(),
            self.scaled_width,
            self.scaled_height,
            self.output_width,
            self.output_height,
            self.pad_left,
            self.pad_top
        )
    }
}

/// Detect the black borders around the content of a source.
///
/// `black_threshold` is a fraction of full scale (0.1 is the usual default), and
/// `sample_count` frames spread evenly across the content are each seeked to and
/// measured on their own. The detected content rectangles are unioned, so a
/// frame that happens to be dark cannot crop away picture another frame has, and
/// the returned crop removes everything outside that union.
pub fn detect_black_borders(
    input: &Path,
    source: DecodeSource,
    black_threshold: f32,
    sample_count: u32,
) -> Result<Crop, String> {
    let (width, height, frame_count) = crate::encode::probe_decode_source(input, source);
    if width == 0 || height == 0 {
        return Err(format!(
            "cannot read the picture size of {}, so black borders cannot be detected",
            input.display()
        ));
    }
    let samples = sample_count.max(1).min(frame_count.max(1) as u32);
    let duration = probe_duration_seconds(input, source);

    let mut left = u32::MAX;
    let mut top = u32::MAX;
    let mut content_right = 0u32;
    let mut content_bottom = 0u32;
    let mut detections = 0u32;

    for index in 0..samples {
        // sample the middle of each equal slice of the content, so neither the
        // first nor the last frame decides the crop on its own
        let seek = duration.map(|seconds| seconds * (2 * index + 1) as f64 / (2 * samples) as f64);
        for (sample_width, sample_height, sample_x, sample_y) in
            cropdetect_sample(input, source, black_threshold, seek)?
        {
            left = left.min(sample_x);
            top = top.min(sample_y);
            content_right = content_right.max((sample_x + sample_width).min(width));
            content_bottom = content_bottom.max((sample_y + sample_height).min(height));
            detections += 1;
        }
    }

    if detections == 0 {
        return Err(format!(
            "cropdetect reported no crop rectangle for {}",
            input.display()
        ));
    }

    Ok(Crop {
        left: floor_to_even(left),
        right: floor_to_even(width.saturating_sub(content_right)),
        top: floor_to_even(top),
        bottom: floor_to_even(height.saturating_sub(content_bottom)),
    })
}

/// Run cropdetect over one seeked frame and return every rectangle it reported.
fn cropdetect_sample(
    input: &Path,
    source: DecodeSource,
    black_threshold: f32,
    seek: Option<f64>,
) -> Result<Vec<(u32, u32, u32, u32)>, String> {
    let filter = format!(
        "cropdetect=limit={black_threshold}:round={CROPDETECT_ROUND}:reset={CROPDETECT_RESET}:skip={CROPDETECT_SKIP}"
    );
    let mut command = std::process::Command::new("ffmpeg");
    command.arg("-y").arg("-hide_banner");
    if let Some(seconds) = seek {
        command.arg("-ss").arg(format!("{seconds}"));
    }
    let output = command
        .args(source.demuxer_args())
        .arg("-i")
        .arg(input)
        .args(["-frames:v", "1", "-vf", &filter, "-an", "-f", "null", "-"])
        .output()
        .map_err(|e| format!("cannot run ffmpeg for black border detection: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg failed to measure {}: {}",
            input.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(parse_crop_line)
        .collect())
}

/// Pull `crop=w:h:x:y` off one cropdetect log line.
fn parse_crop_line(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rectangle = line.rsplit_once("crop=")?.1;
    let mut values = rectangle.trim().split(':');
    let width = values.next()?.parse().ok()?;
    let height = values.next()?.parse().ok()?;
    let x = values.next()?.parse().ok()?;
    let y = values.next()?.trim().parse().ok()?;
    Some((width, height, x, y))
}

/// Content duration in seconds, for spreading the samples over it.
fn probe_duration_seconds(input: &Path, source: DecodeSource) -> Option<f64> {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "error"])
        .args(source.demuxer_args())
        .args(["-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(input)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|seconds| *seconds > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_processing_emits_no_filters() {
        let plan = PictureProcessing::default().plan(1920, 1080).unwrap();
        assert!(plan.filters.is_empty());
        assert!(plan.is_identity());
        assert_eq!((plan.output_width, plan.output_height), (1920, 1080));
        assert_eq!((plan.pad_left, plan.pad_top), (0, 0));
        assert_eq!(plan.fps_position, 0);
    }

    #[test]
    fn a_fill_crop_brings_the_source_to_the_aspect() {
        let crop = Crop::to_aspect(1920, 1080, 2048, 858);
        assert_eq!(
            crop,
            Crop {
                left: 0,
                right: 0,
                top: 138,
                bottom: 138
            }
        );
        let plan = PictureProcessing {
            crop,
            ..PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap();
        assert_eq!((plan.cropped_width, plan.cropped_height), (1920, 804));

        // a source wider than the target loses its sides instead
        let pillar = Crop::to_aspect(1920, 1080, 4, 3);
        assert_eq!(
            pillar,
            Crop {
                left: 240,
                right: 240,
                top: 0,
                bottom: 0
            }
        );
        assert!(Crop::to_aspect(1920, 1080, 16, 9).is_none());
    }

    #[test]
    fn a_cropped_source_scales_and_pads_onto_the_raster() {
        let processing = PictureProcessing {
            crop: Crop::to_aspect(1920, 1080, 2048, 858),
            fit: Some(Fit {
                box_width: 2048,
                box_height: 858,
                raster_width: 2048,
                raster_height: 1080,
            }),
            ..PictureProcessing::default()
        };
        let plan = processing.plan(1920, 1080).unwrap();

        assert_eq!((plan.cropped_width, plan.cropped_height), (1920, 804));
        assert_eq!((plan.scaled_width, plan.scaled_height), (2048, 856));
        assert_eq!((plan.output_width, plan.output_height), (2048, 1080));
        assert_eq!((plan.pad_left, plan.pad_top), (0, 112));
        assert_eq!(
            plan.filters,
            vec![
                "format=gbrp16le".to_string(),
                "crop=1920:804:0:138".to_string(),
                "scale=w=2048:h=856:flags=lanczos".to_string(),
                "pad=w=2048:h=1080:x=0:y=112:color=black".to_string(),
            ]
        );
        assert_eq!(
            plan.describe(),
            "crop 0/0/138/138 to 1920x804, rotate none, scale to 2048x856, pad to 2048x1080 at (0,112)"
        );
    }

    #[test]
    fn a_quarter_turn_swaps_the_dimensions() {
        let plan = PictureProcessing {
            rotation: Rotation::Clockwise90,
            ..PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap();
        assert_eq!((plan.output_width, plan.output_height), (1080, 1920));
        assert_eq!(plan.filters, vec!["format=gbrp16le", "transpose=clock"]);

        let half = PictureProcessing {
            rotation: Rotation::Half,
            ..PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap();
        assert_eq!((half.output_width, half.output_height), (1920, 1080));
        assert_eq!(
            half.filters,
            vec!["format=gbrp16le", "transpose=clock", "transpose=clock"]
        );
    }

    #[test]
    fn a_portrait_source_fits_the_raster_with_pillars() {
        let plan = PictureProcessing {
            rotation: Rotation::CounterClockwise90,
            fit: Some(Fit {
                box_width: 1998,
                box_height: 1080,
                raster_width: 1998,
                raster_height: 1080,
            }),
            ..PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap();
        assert_eq!((plan.rotated_width, plan.rotated_height), (1080, 1920));
        assert_eq!((plan.scaled_width, plan.scaled_height), (606, 1080));
        assert_eq!((plan.pad_left, plan.pad_top), (696, 0));
        assert!(
            plan.filters.iter().any(|f| f.starts_with("pad=")),
            "{:?}",
            plan.filters
        );
    }

    #[test]
    fn deinterlace_runs_before_the_frame_rate_and_denoise_after_it() {
        let plan = PictureProcessing {
            deinterlace: true,
            denoise: true,
            ..PictureProcessing::default()
        }
        .plan(720, 576)
        .unwrap();
        assert_eq!(plan.filters, vec!["yadif", "hqdn3d"]);
        assert_eq!(plan.fps_position, 1);
    }

    #[test]
    fn the_flips_come_after_the_turn() {
        let plan = PictureProcessing {
            flip_horizontal: true,
            flip_vertical: true,
            rotation: Rotation::Clockwise90,
            ..PictureProcessing::default()
        }
        .plan(640, 480)
        .unwrap();
        assert_eq!(
            plan.filters,
            vec!["format=gbrp16le", "transpose=clock", "hflip", "vflip"]
        );
    }

    #[test]
    fn an_impossible_plan_fails_loud() {
        let zero = PictureProcessing::default().plan(0, 1080).unwrap_err();
        assert!(zero.contains("0x1080"), "{zero}");

        let eaten = PictureProcessing {
            crop: Crop {
                left: 960,
                right: 960,
                top: 0,
                bottom: 0,
            },
            ..PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap_err();
        assert!(eaten.contains("leaves nothing"), "{eaten}");

        let overflowing_box = PictureProcessing {
            fit: Some(Fit {
                box_width: 4096,
                box_height: 2160,
                raster_width: 2048,
                raster_height: 1080,
            }),
            ..PictureProcessing::default()
        }
        .plan(1920, 1080)
        .unwrap_err();
        assert!(overflowing_box.contains("larger than"), "{overflowing_box}");
    }

    #[test]
    fn a_cropdetect_line_parses_to_its_rectangle() {
        let line = "[Parsed_cropdetect_0 @ 0x55] x1:0 x2:1919 y1:140 y2:939 w:1920 h:800 x:0 \
                    y:140 pts:0 t:0.000000 limit:0.100000 crop=1920:800:0:140";
        assert_eq!(parse_crop_line(line), Some((1920, 800, 0, 140)));
        assert_eq!(parse_crop_line("frame= 1 fps=0.0 q=-0.0"), None);
    }
}
