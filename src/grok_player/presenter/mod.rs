mod gl;

use super::{ComposedFrame, RGBA_BYTES_PER_PIXEL};
pub(super) use gl::{GlPresenter, GlSurface};

// the software surface is rgb0, mpv's format, so the fourth byte stays zero
pub(super) const SOFTWARE_BYTES_PER_PIXEL: usize = 4;
const COLOUR_BYTES_PER_PIXEL: usize = 3;

pub const MINIMUM_BRIGHTNESS: f32 = 0.0;
pub const MAXIMUM_BRIGHTNESS: f32 = 4.0;
const DEFAULT_BRIGHTNESS: f32 = 1.0;
const MAXIMUM_MASK_FRACTION: f32 = 1.0;
const SAMPLE_VALUES: usize = 256;
const LARGEST_SAMPLE: f32 = 255.0;
const PIXEL_CENTRE: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PictureScaling {
    // the whole picture, bars where the aspect ratios differ
    #[default]
    Fit,
    // the whole surface, the picture's overflow cropped evenly
    Fill,
    // one picture pixel per surface pixel, cropped evenly when larger
    Native,
}

// black bands over the picture's edges, in fractions of the picture's width and height
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PictureMasks {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresentationSettings {
    // a gain on the displayed rgb
    pub brightness: f32,
    pub masks: PictureMasks,
    pub scaling: PictureScaling,
}

impl Default for PresentationSettings {
    fn default() -> Self {
        PresentationSettings {
            brightness: DEFAULT_BRIGHTNESS,
            masks: PictureMasks::default(),
            scaling: PictureScaling::default(),
        }
    }
}

impl PresentationSettings {
    pub(super) fn clamped(self) -> Self {
        let mask = |fraction: f32| fraction.clamp(0.0, MAXIMUM_MASK_FRACTION);
        PresentationSettings {
            brightness: self
                .brightness
                .clamp(MINIMUM_BRIGHTNESS, MAXIMUM_BRIGHTNESS),
            masks: PictureMasks {
                top: mask(self.masks.top),
                bottom: mask(self.masks.bottom),
                left: mask(self.masks.left),
                right: mask(self.masks.right),
            },
            scaling: self.scaling,
        }
    }

    // the edges of the unmasked picture as left, top, right, bottom in texture coordinates
    fn unmasked_area(&self) -> [f64; 4] {
        [
            f64::from(self.masks.left),
            f64::from(self.masks.top),
            1.0 - f64::from(self.masks.right),
            1.0 - f64::from(self.masks.bottom),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureRectangle {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

// a span of the picture in fractions of its width or height
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PictureSpan {
    pub start: f64,
    pub size: f64,
}

const WHOLE_PICTURE: PictureSpan = PictureSpan {
    start: 0.0,
    size: 1.0,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PicturePlacement {
    // the part of the surface the picture covers
    pub rectangle: PictureRectangle,
    // the part of the picture drawn into the rectangle
    pub horizontal: PictureSpan,
    pub vertical: PictureSpan,
}

struct AxisPlacement {
    offset: u32,
    shown: u32,
    span: PictureSpan,
}

fn place_axis(surface: u32, picture: u32, scale: f64) -> AxisPlacement {
    let scaled = f64::from(picture) * scale;
    let rounded = scaled.round() as u32;
    if rounded <= surface {
        let shown = rounded.max(1);
        return AxisPlacement {
            offset: (surface - shown) / 2,
            shown,
            span: WHOLE_PICTURE,
        };
    }
    let size = f64::from(surface) / scaled;
    AxisPlacement {
        offset: 0,
        shown: surface,
        span: PictureSpan {
            start: (1.0 - size) / 2.0,
            size,
        },
    }
}

// the gl path and the software path both draw into this
pub(super) fn picture_placement(
    surface_width: u32,
    surface_height: u32,
    picture_width: u32,
    picture_height: u32,
    scaling: PictureScaling,
) -> Option<PicturePlacement> {
    if surface_width == 0 || surface_height == 0 || picture_width == 0 || picture_height == 0 {
        return None;
    }
    let width_scale = f64::from(surface_width) / f64::from(picture_width);
    let height_scale = f64::from(surface_height) / f64::from(picture_height);
    let scale = match scaling {
        PictureScaling::Fit => width_scale.min(height_scale),
        PictureScaling::Fill => width_scale.max(height_scale),
        PictureScaling::Native => 1.0,
    };
    let horizontal = place_axis(surface_width, picture_width, scale);
    let vertical = place_axis(surface_height, picture_height, scale);
    Some(PicturePlacement {
        rectangle: PictureRectangle {
            x: horizontal.offset,
            y: vertical.offset,
            width: horizontal.shown,
            height: vertical.shown,
        },
        horizontal: horizontal.span,
        vertical: vertical.span,
    })
}

// the picture row or column a surface pixel samples, None under a mask, which covers each eye's edges
fn source_sample(
    position: usize,
    shown: u32,
    span: PictureSpan,
    picture: u32,
    eye_cells: u32,
    unmasked: (f64, f64),
) -> Option<usize> {
    let fraction = span.start + (position as f64 + PIXEL_CENTRE) / f64::from(shown) * span.size;
    let across_cells = fraction * f64::from(eye_cells);
    let eye_fraction = across_cells - across_cells.floor().min(f64::from(eye_cells - 1));
    if eye_fraction < unmasked.0 || eye_fraction > unmasked.1 {
        return None;
    }
    Some(((fraction * f64::from(picture)) as usize).min(picture as usize - 1))
}

fn brightness_table(brightness: f32) -> [u8; SAMPLE_VALUES] {
    std::array::from_fn(|sample| (sample as f32 * brightness).round().min(LARGEST_SAMPLE) as u8)
}

pub(super) fn draw_software(
    frame: &ComposedFrame,
    width: usize,
    height: usize,
    settings: &PresentationSettings,
    target: &mut [u8],
) -> Result<(), String> {
    let needed = width * height * SOFTWARE_BYTES_PER_PIXEL;
    if target.len() < needed {
        return Err(format!(
            "target buffer holds {} bytes, needs {needed}",
            target.len()
        ));
    }
    target[..needed].fill(0);
    let Some(placement) = picture_placement(
        width as u32,
        height as u32,
        frame.width,
        frame.height,
        settings.scaling,
    ) else {
        return Ok(());
    };
    let rectangle = placement.rectangle;
    let [unmasked_left, unmasked_top, unmasked_right, unmasked_bottom] = settings.unmasked_area();
    let (eyes_across, eyes_down) = frame.eyes.cells();
    let source_columns: Vec<Option<usize>> = (0..rectangle.width as usize)
        .map(|column| {
            source_sample(
                column,
                rectangle.width,
                placement.horizontal,
                frame.width,
                eyes_across,
                (unmasked_left, unmasked_right),
            )
        })
        .collect();
    let brightness = brightness_table(settings.brightness);

    for row in 0..rectangle.height as usize {
        let Some(source_row) = source_sample(
            row,
            rectangle.height,
            placement.vertical,
            frame.height,
            eyes_down,
            (unmasked_top, unmasked_bottom),
        ) else {
            continue;
        };
        for (column, source_column) in source_columns.iter().enumerate() {
            let Some(source_column) = source_column else {
                continue;
            };
            let source = (source_row * frame.width as usize + source_column) * RGBA_BYTES_PER_PIXEL;
            let at = ((rectangle.y as usize + row) * width + rectangle.x as usize + column)
                * SOFTWARE_BYTES_PER_PIXEL;
            let colour = &frame.data()[source..source + COLOUR_BYTES_PER_PIXEL];
            for (shown, sample) in target[at..at + COLOUR_BYTES_PER_PIXEL]
                .iter_mut()
                .zip(colour)
            {
                *shown = brightness[usize::from(*sample)];
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grok_player::ComposedPixels;
    use crate::grok_player::stereo::EyeArrangement;

    fn drawn_frame(sample: u8) -> ComposedFrame {
        ComposedFrame {
            width: 2,
            height: 2,
            pixels: ComposedPixels::Drawn(vec![sample; 2 * 2 * RGBA_BYTES_PER_PIXEL]),
            eyes: EyeArrangement::Single,
        }
    }

    fn fit_rectangle(
        surface_width: u32,
        surface_height: u32,
        picture_width: u32,
        picture_height: u32,
    ) -> PictureRectangle {
        let placement = picture_placement(
            surface_width,
            surface_height,
            picture_width,
            picture_height,
            PictureScaling::Fit,
        )
        .unwrap();
        assert_eq!(placement.horizontal, WHOLE_PICTURE);
        assert_eq!(placement.vertical, WHOLE_PICTURE);
        placement.rectangle
    }

    #[test]
    fn a_square_picture_gets_bars_either_side_of_a_wide_surface() {
        let rectangle = fit_rectangle(320, 180, 64, 64);
        assert_eq!(
            rectangle,
            PictureRectangle {
                x: 70,
                y: 0,
                width: 180,
                height: 180
            }
        );
    }

    #[test]
    fn a_wide_picture_gets_bars_above_and_below_a_square_surface() {
        let rectangle = fit_rectangle(200, 200, 100, 50);
        assert_eq!(
            rectangle,
            PictureRectangle {
                x: 0,
                y: 50,
                width: 200,
                height: 100
            }
        );
    }

    #[test]
    fn a_surface_with_no_area_has_no_picture_rectangle() {
        assert!(picture_placement(0, 180, 64, 64, PictureScaling::Fit).is_none());
        assert!(picture_placement(320, 180, 64, 0, PictureScaling::Fit).is_none());
    }

    #[test]
    fn a_frame_lands_centred_in_the_target_with_black_bars() {
        const SURFACE: usize = 8;
        let frame = drawn_frame(9);
        let mut target = vec![0xffu8; SURFACE * 4 * SOFTWARE_BYTES_PER_PIXEL];
        draw_software(
            &frame,
            SURFACE,
            4,
            &PresentationSettings::default(),
            &mut target,
        )
        .unwrap();
        let pixel = |x: usize, y: usize| target[(y * SURFACE + x) * SOFTWARE_BYTES_PER_PIXEL];
        assert_eq!(pixel(0, 0), 0, "left bar");
        assert_eq!(pixel(1, 0), 0, "left bar");
        assert_eq!(pixel(2, 0), 9, "picture");
        assert_eq!(pixel(5, 3), 9, "picture");
        assert_eq!(pixel(6, 3), 0, "right bar");
    }

    #[test]
    fn a_target_too_small_for_the_surface_is_refused() {
        let frame = drawn_frame(0);
        let mut target = vec![0u8; 4];
        assert!(
            draw_software(&frame, 8, 4, &PresentationSettings::default(), &mut target).is_err()
        );
    }

    fn grey_frame(width: u32, height: u32, samples: &[u8]) -> ComposedFrame {
        let data = samples
            .iter()
            .flat_map(|&sample| [sample, sample, sample, OPAQUE_SAMPLE])
            .collect();
        ComposedFrame {
            width,
            height,
            pixels: ComposedPixels::Drawn(data),
            eyes: EyeArrangement::Single,
        }
    }

    const OPAQUE_SAMPLE: u8 = 255;

    // the first sample of each surface pixel, row by row
    fn draw_grey(
        frame: &ComposedFrame,
        width: usize,
        height: usize,
        settings: PresentationSettings,
    ) -> Vec<u8> {
        let mut target = vec![0xffu8; width * height * SOFTWARE_BYTES_PER_PIXEL];
        draw_software(frame, width, height, &settings, &mut target).unwrap();
        target
            .as_chunks::<SOFTWARE_BYTES_PER_PIXEL>()
            .0
            .iter()
            .map(|pixel| {
                assert_eq!(pixel[0], pixel[1]);
                assert_eq!(pixel[0], pixel[2]);
                pixel[0]
            })
            .collect()
    }

    fn scaled(scaling: PictureScaling) -> PresentationSettings {
        PresentationSettings {
            scaling,
            ..PresentationSettings::default()
        }
    }

    #[test]
    fn fill_covers_the_surface_and_crops_the_overflow_evenly() {
        let placement = picture_placement(320, 180, 64, 64, PictureScaling::Fill).unwrap();
        assert_eq!(
            placement.rectangle,
            PictureRectangle {
                x: 0,
                y: 0,
                width: 320,
                height: 180
            }
        );
        assert_eq!(placement.horizontal, WHOLE_PICTURE);
        assert_eq!(
            placement.vertical,
            PictureSpan {
                start: 0.21875,
                size: 0.5625
            }
        );
    }

    #[test]
    fn native_centres_a_smaller_picture_pixel_for_pixel() {
        let placement = picture_placement(320, 180, 64, 64, PictureScaling::Native).unwrap();
        assert_eq!(
            placement.rectangle,
            PictureRectangle {
                x: 128,
                y: 58,
                width: 64,
                height: 64
            }
        );
        assert_eq!(placement.horizontal, WHOLE_PICTURE);
        assert_eq!(placement.vertical, WHOLE_PICTURE);
    }

    #[test]
    fn native_crops_a_larger_picture_to_its_centre() {
        let placement = picture_placement(100, 50, 200, 100, PictureScaling::Native).unwrap();
        assert_eq!(
            placement.rectangle,
            PictureRectangle {
                x: 0,
                y: 0,
                width: 100,
                height: 50
            }
        );
        let centre_half = PictureSpan {
            start: 0.25,
            size: 0.5,
        };
        assert_eq!(placement.horizontal, centre_half);
        assert_eq!(placement.vertical, centre_half);
    }

    #[test]
    fn settings_out_of_range_are_clamped() {
        let settings = PresentationSettings {
            brightness: 10.0,
            masks: PictureMasks {
                top: -0.5,
                bottom: 2.0,
                left: 0.25,
                right: 0.0,
            },
            scaling: PictureScaling::Fill,
        }
        .clamped();
        assert_eq!(settings.brightness, MAXIMUM_BRIGHTNESS);
        assert_eq!(
            settings.masks,
            PictureMasks {
                top: 0.0,
                bottom: 1.0,
                left: 0.25,
                right: 0.0,
            }
        );
        let darkened = PresentationSettings {
            brightness: -1.0,
            ..PresentationSettings::default()
        };
        assert_eq!(darkened.clamped().brightness, MINIMUM_BRIGHTNESS);
    }

    #[test]
    fn brightness_scales_each_sample_and_saturates() {
        let frame = grey_frame(2, 1, &[100, 200]);
        let brighter = PresentationSettings {
            brightness: 2.0,
            ..PresentationSettings::default()
        };
        assert_eq!(draw_grey(&frame, 2, 1, brighter), [200, 255]);
        let dimmer = PresentationSettings {
            brightness: 0.5,
            ..PresentationSettings::default()
        };
        assert_eq!(draw_grey(&frame, 2, 1, dimmer), [50, 100]);
    }

    #[test]
    fn masks_black_out_the_picture_edges() {
        let frame = grey_frame(4, 4, &[9; 16]);
        let settings = PresentationSettings {
            masks: PictureMasks {
                top: 0.5,
                bottom: 0.0,
                left: 0.25,
                right: 0.25,
            },
            ..PresentationSettings::default()
        };
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 0,
            0, 0, 0, 0,
            0, 9, 9, 0,
            0, 9, 9, 0,
        ];
        assert_eq!(draw_grey(&frame, 4, 4, settings), expected);
    }

    #[test]
    fn masks_cover_the_picture_not_the_bars() {
        let frame = grey_frame(2, 2, &[9; 4]);
        let settings = PresentationSettings {
            masks: PictureMasks {
                left: 0.5,
                ..PictureMasks::default()
            },
            ..PresentationSettings::default()
        };
        assert_eq!(draw_grey(&frame, 4, 2, settings), [0, 0, 9, 0, 0, 0, 9, 0]);
    }

    #[test]
    fn fill_draws_the_middle_of_a_wide_picture() {
        let frame = grey_frame(4, 1, &[10, 20, 30, 40]);
        assert_eq!(
            draw_grey(&frame, 2, 1, scaled(PictureScaling::Fill)),
            [20, 30]
        );
        assert_eq!(
            draw_grey(&frame, 2, 1, scaled(PictureScaling::Native)),
            [20, 30]
        );
        assert_eq!(
            draw_grey(&frame, 8, 2, scaled(PictureScaling::Fit)),
            [
                10, 10, 20, 20, 30, 30, 40, 40, 10, 10, 20, 20, 30, 30, 40, 40
            ]
        );
    }

    #[test]
    fn a_mask_on_a_cropped_picture_is_measured_on_the_whole_picture() {
        let frame = grey_frame(4, 1, &[10, 20, 30, 40]);
        let settings = PresentationSettings {
            masks: PictureMasks {
                left: 0.4,
                ..PictureMasks::default()
            },
            scaling: PictureScaling::Fill,
            ..PresentationSettings::default()
        };
        assert_eq!(draw_grey(&frame, 2, 1, settings), [0, 30]);
    }
}
