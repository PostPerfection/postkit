mod gl;

use super::{ComposedFrame, RGBA_BYTES_PER_PIXEL};
pub(super) use gl::GlPresenter;

// the software surface is rgb0, mpv's format, so the fourth byte stays zero
pub(super) const SOFTWARE_BYTES_PER_PIXEL: usize = 4;
const COLOUR_BYTES_PER_PIXEL: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureRectangle {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

// the gl path and the software path both draw into this
pub(super) fn picture_rectangle(
    surface_width: u32,
    surface_height: u32,
    picture_width: u32,
    picture_height: u32,
) -> Option<PictureRectangle> {
    if surface_width == 0 || surface_height == 0 || picture_width == 0 || picture_height == 0 {
        return None;
    }
    let scale = f64::from(surface_width) / f64::from(picture_width);
    let scale = scale.min(f64::from(surface_height) / f64::from(picture_height));
    let width = ((f64::from(picture_width) * scale).round() as u32).clamp(1, surface_width);
    let height = ((f64::from(picture_height) * scale).round() as u32).clamp(1, surface_height);
    Some(PictureRectangle {
        x: (surface_width - width) / 2,
        y: (surface_height - height) / 2,
        width,
        height,
    })
}

pub(super) fn draw_software(
    frame: &ComposedFrame,
    width: usize,
    height: usize,
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
    let Some(rectangle) = picture_rectangle(width as u32, height as u32, frame.width, frame.height)
    else {
        return Ok(());
    };

    for row in 0..rectangle.height as usize {
        let source_row = row * frame.height as usize / rectangle.height as usize;
        let source_row = source_row.min(frame.height as usize - 1);
        for column in 0..rectangle.width as usize {
            let source_column = column * frame.width as usize / rectangle.width as usize;
            let source_column = source_column.min(frame.width as usize - 1);
            let source = (source_row * frame.width as usize + source_column) * RGBA_BYTES_PER_PIXEL;
            let at = ((rectangle.y as usize + row) * width + rectangle.x as usize + column)
                * SOFTWARE_BYTES_PER_PIXEL;
            target[at..at + COLOUR_BYTES_PER_PIXEL]
                .copy_from_slice(&frame.data()[source..source + COLOUR_BYTES_PER_PIXEL]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grok_player::ComposedPixels;

    fn drawn_frame(sample: u8) -> ComposedFrame {
        ComposedFrame {
            width: 2,
            height: 2,
            pixels: ComposedPixels::Drawn(vec![sample; 2 * 2 * RGBA_BYTES_PER_PIXEL]),
        }
    }

    #[test]
    fn a_square_picture_gets_bars_either_side_of_a_wide_surface() {
        let rectangle = picture_rectangle(320, 180, 64, 64).unwrap();
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
        let rectangle = picture_rectangle(200, 200, 100, 50).unwrap();
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
        assert!(picture_rectangle(0, 180, 64, 64).is_none());
        assert!(picture_rectangle(320, 180, 64, 0).is_none());
    }

    #[test]
    fn a_frame_lands_centred_in_the_target_with_black_bars() {
        const SURFACE: usize = 8;
        let frame = drawn_frame(9);
        let mut target = vec![0xffu8; SURFACE * 4 * SOFTWARE_BYTES_PER_PIXEL];
        draw_software(&frame, SURFACE, 4, &mut target).unwrap();
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
        assert!(draw_software(&frame, 8, 4, &mut target).is_err());
    }
}
