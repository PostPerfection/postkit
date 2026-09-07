use std::sync::Arc;

use super::{
    ComposedFrame, ComposedPixels, OPAQUE_ALPHA, OverlayRectangle, RGBA_BYTES_PER_PIXEL, Rgba8Frame,
};
use crate::subtitle_formats::StyledCue;
use crate::subtitle_raster::{
    BurnStyle, PositionedBitmap, SubtitleRasterizer, composite_rgb8, find_system_sans_font,
};

const OPAQUE: u32 = OPAQUE_ALPHA as u32;

pub(super) struct Layers<'a> {
    pub overlays: &'a [OverlayRectangle],
    // overlays are stated in full-resolution source pixels
    pub overlay_scale: f64,
    pub cue_tracks: &'a [&'a [StyledCue]],
    pub time_ms: u64,
}

pub(super) struct Compositor {
    rasterizer: Option<SubtitleRasterizer>,
    style: BurnStyle,
}

impl Compositor {
    pub fn new() -> Self {
        Compositor {
            rasterizer: None,
            style: BurnStyle::default(),
        }
    }

    // font discovery scans every system font directory
    pub fn prepare_subtitles(&mut self) -> Result<(), String> {
        if self.rasterizer.is_some() {
            return Ok(());
        }
        let font = find_system_sans_font();
        self.rasterizer =
            Some(SubtitleRasterizer::new(font.as_deref()).map_err(|reason| reason.to_string())?);
        Ok(())
    }

    pub fn compose(
        &mut self,
        picture: &Arc<Rgba8Frame>,
        layers: Layers<'_>,
    ) -> Result<ComposedFrame, String> {
        let expected = picture.width as usize * picture.height as usize * RGBA_BYTES_PER_PIXEL;
        if picture.data.len() != expected {
            return Err(format!(
                "a {}x{} frame carries {} bytes, not the {expected} it should",
                picture.width,
                picture.height,
                picture.data.len()
            ));
        }
        let bitmaps = self.raster_subtitles(
            picture.width,
            picture.height,
            layers.cue_tracks,
            layers.time_ms,
        );
        let has_overlays = layers.overlays.iter().any(|overlay| overlay.alpha != 0);
        let pixels = if !has_overlays && bitmaps.is_empty() {
            ComposedPixels::Picture(picture.clone())
        } else {
            let mut data = picture.data.clone();
            draw_overlays(
                &mut data,
                picture.width,
                picture.height,
                layers.overlays,
                layers.overlay_scale,
            );
            composite_rgb8(
                &mut data,
                picture.width,
                picture.height,
                RGBA_BYTES_PER_PIXEL,
                &bitmaps,
            );
            ComposedPixels::Drawn(data)
        };
        Ok(ComposedFrame {
            width: picture.width,
            height: picture.height,
            pixels,
        })
    }

    // every visible track's cues, in the order they are drawn
    fn raster_subtitles(
        &mut self,
        width: u32,
        height: u32,
        cue_tracks: &[&[StyledCue]],
        time_ms: u64,
    ) -> Vec<PositionedBitmap> {
        let mut drawn = Vec::new();
        let Some(rasterizer) = self.rasterizer.as_mut() else {
            return drawn;
        };
        for cues in cue_tracks {
            if cues.is_empty() {
                continue;
            }
            match rasterizer.render(cues, time_ms, width, height, &self.style) {
                Ok(mut bitmaps) => drawn.append(&mut bitmaps),
                Err(reason) => tracing::error!("subtitle raster at {time_ms} ms: {reason}"),
            }
        }
        drawn
    }
}

fn draw_overlays(
    data: &mut [u8],
    width: u32,
    height: u32,
    overlays: &[OverlayRectangle],
    scale: f64,
) {
    let stride = width as usize * RGBA_BYTES_PER_PIXEL;
    for overlay in overlays {
        if overlay.alpha == 0 {
            continue;
        }
        let left = scaled(overlay.x, scale).clamp(0, i64::from(width)) as usize;
        let top = scaled(overlay.y, scale).clamp(0, i64::from(height)) as usize;
        let right = scaled(overlay.x + overlay.width, scale).clamp(0, i64::from(width)) as usize;
        let bottom = scaled(overlay.y + overlay.height, scale).clamp(0, i64::from(height)) as usize;
        let alpha = u32::from(overlay.alpha);
        for row in top..bottom {
            for column in left..right {
                let at = row * stride + column * RGBA_BYTES_PER_PIXEL;
                for (channel, &source) in overlay.colour.iter().enumerate() {
                    let destination = u32::from(data[at + channel]);
                    let mixed =
                        (u32::from(source) * alpha + destination * (OPAQUE - alpha) + 127) / OPAQUE;
                    data[at + channel] = mixed.min(OPAQUE) as u8;
                }
            }
        }
    }
}

fn scaled(value: i64, scale: f64) -> i64 {
    (value as f64 * scale).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::preview::Rgb8Frame;

    const GREY: u8 = 100;

    fn grey(width: u32, height: u32) -> Arc<Rgba8Frame> {
        const RGB_BYTES_PER_PIXEL: usize = 3;
        Arc::new(Rgba8Frame::from_rgb8(&Rgb8Frame {
            width,
            height,
            data: vec![GREY; width as usize * height as usize * RGB_BYTES_PER_PIXEL],
        }))
    }

    fn pixel(data: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let at = (y * width + x) * RGBA_BYTES_PER_PIXEL;
        [data[at], data[at + 1], data[at + 2], data[at + 3]]
    }

    fn plain(overlays: &[OverlayRectangle], overlay_scale: f64) -> Layers<'_> {
        Layers {
            overlays,
            overlay_scale,
            cue_tracks: &[],
            time_ms: 0,
        }
    }

    #[test]
    fn an_overlay_lands_where_the_reduce_factor_puts_it() {
        let mut data = vec![0u8; 8 * 8 * RGBA_BYTES_PER_PIXEL];
        let overlay = OverlayRectangle {
            x: 8,
            y: 8,
            width: 4,
            height: 4,
            colour: [255, 0, 0],
            alpha: 255,
        };
        // a frame decoded at reduce 1 is half the source, so the rectangle halves
        draw_overlays(&mut data, 8, 8, &[overlay], 0.5);
        let colour = |x: usize, y: usize| {
            let [red, green, blue, _] = pixel(&data, 8, x, y);
            [red, green, blue]
        };
        assert_eq!(colour(4, 4), [255, 0, 0]);
        assert_eq!(colour(5, 5), [255, 0, 0]);
        assert_eq!(colour(6, 6), [0, 0, 0], "the rectangle ends at 6");
        assert_eq!(colour(3, 3), [0, 0, 0]);
    }

    #[test]
    fn a_half_alpha_overlay_mixes_with_what_is_under_it() {
        let mut data = grey(2, 2).data.clone();
        draw_overlays(
            &mut data,
            2,
            2,
            &[OverlayRectangle {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
                colour: [200, 200, 200],
                alpha: 128,
            }],
            1.0,
        );
        let mixed = ((200 * 128 + i32::from(GREY) * 127 + 127) / 255) as u8;
        assert_eq!(
            pixel(&data, 2, 0, 0),
            [mixed, mixed, mixed, OPAQUE_ALPHA],
            "the overlay mixes the colour bytes and leaves alpha opaque"
        );
    }

    #[test]
    fn an_overlay_composed_over_a_frame_draws_into_a_copy() {
        let picture = grey(2, 2);
        let overlays = [OverlayRectangle {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            colour: [255, 0, 0],
            alpha: 255,
        }];
        let composed = Compositor::new()
            .compose(&picture, plain(&overlays, 1.0))
            .unwrap();
        assert_ne!(composed.data().as_ptr(), picture.data.as_ptr());
        assert_eq!(
            pixel(composed.data(), 2, 0, 0),
            [255, 0, 0, OPAQUE_ALPHA],
            "the overlay is on the first pixel"
        );
        assert_eq!(
            pixel(composed.data(), 2, 1, 0),
            [GREY, GREY, GREY, OPAQUE_ALPHA],
            "the pixel beside it is untouched"
        );
    }

    #[test]
    fn a_cue_bitmap_lands_on_the_colour_bytes_of_a_four_byte_pixel() {
        let mut data = grey(2, 1).data.clone();
        let cue = PositionedBitmap {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
            pixels: vec![255, 0, 0, 255],
        };
        composite_rgb8(&mut data, 2, 1, RGBA_BYTES_PER_PIXEL, &[cue]);
        assert_eq!(pixel(&data, 2, 1, 0), [255, 0, 0, OPAQUE_ALPHA]);
        assert_eq!(pixel(&data, 2, 0, 0), [GREY, GREY, GREY, OPAQUE_ALPHA]);
    }

    #[test]
    fn composing_nothing_over_a_frame_hands_the_frame_back() {
        let picture = grey(4, 3);
        let composed = Compositor::new()
            .compose(&picture, plain(&[], 1.0))
            .unwrap();
        assert_eq!((composed.width, composed.height), (4, 3));
        assert_eq!(
            composed.data().as_ptr(),
            picture.data.as_ptr(),
            "the composed frame shares the decoded buffer"
        );
    }

    #[test]
    fn a_frame_whose_bytes_do_not_match_its_size_is_refused() {
        let short = Arc::new(Rgba8Frame {
            width: 4,
            height: 3,
            data: vec![0u8; 4],
        });
        assert!(Compositor::new().compose(&short, plain(&[], 1.0)).is_err());
    }
}
