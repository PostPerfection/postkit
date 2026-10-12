use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;

const SOURCE: &str = "in";
const SINK: &str = "out";

// samples sit in the low bits of a 16-bit word, so a depth comes back as it went in
const PLANAR_RGB_FORMATS: [(u8, Pixel, &str); 5] = [
    (9, Pixel::GBRP9LE, "gbrp9le"),
    (10, Pixel::GBRP10LE, "gbrp10le"),
    (12, Pixel::GBRP12LE, "gbrp12le"),
    (14, Pixel::GBRP14LE, "gbrp14le"),
    (16, Pixel::GBRP16LE, "gbrp16le"),
];

// ffmpeg's planar RGB stores green, blue, red
const PLANE_OF_COMPONENT: [usize; 3] = [2, 0, 1];

const BYTES_PER_SAMPLE: usize = 2;

pub struct PlanarRgbFilter {
    graph: ffmpeg::filter::Graph,
    pixel_format: Pixel,
    width: u32,
    height: u32,
    filters: String,
    frames_filtered: i64,
}

impl PlanarRgbFilter {
    pub fn new(width: u32, height: u32, precision: u8, filters: &[String]) -> Result<Self, String> {
        crate::ffmpeg_decode::ready()?;
        let (_, pixel_format, format_name) = PLANAR_RGB_FORMATS
            .into_iter()
            .find(|(depth, _, _)| *depth == precision)
            .ok_or_else(|| format!("{precision}-bit samples have no planar RGB filter format"))?;
        let filters = filters.join(",");
        let chain = if filters.is_empty() {
            format!("format=pix_fmts={format_name}")
        } else {
            format!("{filters},format=pix_fmts={format_name}")
        };
        let failed = |error: ffmpeg::Error| format!("the filters `{filters}` cannot run: {error}");

        let mut graph = ffmpeg::filter::Graph::new();
        let buffer = ffmpeg::filter::find("buffer").ok_or("libavfilter has no buffer source")?;
        let buffer_sink =
            ffmpeg::filter::find("buffersink").ok_or("libavfilter has no buffer sink")?;
        let source_args = format!(
            "video_size={width}x{height}:pix_fmt={format_name}:time_base=1/1:pixel_aspect=1/1"
        );
        graph.add(&buffer, SOURCE, &source_args).map_err(failed)?;
        graph.add(&buffer_sink, SINK, "").map_err(failed)?;
        graph
            .output(SOURCE, 0)
            .and_then(|parser| parser.input(SINK, 0))
            .and_then(|parser| parser.parse(&chain))
            .map_err(failed)?;
        graph.validate().map_err(failed)?;
        Ok(Self {
            graph,
            pixel_format,
            width,
            height,
            filters,
            frames_filtered: 0,
        })
    }

    pub fn filter(
        &mut self,
        components: &[Vec<i32>; 3],
    ) -> Result<(u32, u32, [Vec<i32>; 3]), String> {
        let samples = self.width as usize * self.height as usize;
        if components
            .iter()
            .any(|component| component.len() != samples)
        {
            return Err(format!(
                "the filter graph takes {}x{} frames",
                self.width, self.height
            ));
        }
        let mut input = ffmpeg::frame::Video::new(self.pixel_format, self.width, self.height);
        for (component, plane) in components.iter().zip(PLANE_OF_COMPONENT) {
            let stride = input.stride(plane);
            let data = input.data_mut(plane);
            for (row, samples) in component.chunks_exact(self.width as usize).enumerate() {
                let row_bytes = &mut data[row * stride..][..samples.len() * BYTES_PER_SAMPLE];
                let (words, _) = row_bytes.as_chunks_mut::<BYTES_PER_SAMPLE>();
                for (word, sample) in words.iter_mut().zip(samples) {
                    *word = (*sample as u16).to_le_bytes();
                }
            }
        }
        input.set_pts(Some(self.frames_filtered));
        self.frames_filtered += 1;

        let failed =
            |error: ffmpeg::Error| format!("the filters `{}` failed: {error}", self.filters);
        self.graph
            .get(SOURCE)
            .expect("added with the graph")
            .source()
            .add(&input)
            .map_err(failed)?;
        let mut output = ffmpeg::frame::Video::empty();
        self.graph
            .get(SINK)
            .expect("added with the graph")
            .sink()
            .frame(&mut output)
            .map_err(failed)?;

        let (width, height) = (output.width(), output.height());
        let components = PLANE_OF_COMPONENT.map(|plane| {
            let stride = output.stride(plane);
            let data = output.data(plane);
            let mut component = Vec::with_capacity(width as usize * height as usize);
            for row in 0..height as usize {
                let row_bytes = &data[row * stride..][..width as usize * BYTES_PER_SAMPLE];
                let (words, _) = row_bytes.as_chunks::<BYTES_PER_SAMPLE>();
                component.extend(
                    words
                        .iter()
                        .map(|word| i32::from(u16::from_le_bytes(*word))),
                );
            }
            component
        });
        Ok((width, height, components))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picture_processing::{Crop, Fit, PictureProcessing};

    const WIDTH: u32 = 64;
    const HEIGHT: u32 = 32;
    const PRECISION: u8 = 12;

    // every sample distinct per component, so a shifted or swapped plane shows
    fn pattern() -> [Vec<i32>; 3] {
        [0, 1, 2].map(|component| {
            (0..WIDTH * HEIGHT)
                .map(|index| ((index as i32 * 3 + component * 1000) % 4000) + 1)
                .collect()
        })
    }

    fn sample(components: &[Vec<i32>; 3], width: u32, component: usize, x: u32, y: u32) -> i32 {
        components[component][(y * width + x) as usize]
    }

    #[test]
    fn an_unscaled_crop_and_pad_bring_the_samples_back_exactly() {
        let crop = Crop {
            left: 4,
            right: 4,
            top: 2,
            bottom: 2,
        };
        let plan = PictureProcessing {
            crop,
            fit: Some(Fit {
                box_width: 56,
                box_height: 28,
                raster_width: WIDTH,
                raster_height: HEIGHT,
                placement: Default::default(),
            }),
            ..Default::default()
        }
        .plan(WIDTH, HEIGHT)
        .unwrap();
        assert!(
            !plan
                .filters
                .iter()
                .any(|filter| filter.starts_with("scale"))
        );
        let source = pattern();
        let mut filter = PlanarRgbFilter::new(WIDTH, HEIGHT, PRECISION, &plan.filters).unwrap();
        // the same graph twice, since a runner serves every frame of a track
        for _ in 0..2 {
            let (width, height, filtered) = filter.filter(&source).unwrap();
            assert_eq!((width, height), (WIDTH, HEIGHT));
            for component in 0..3 {
                for y in 0..HEIGHT {
                    for x in 0..WIDTH {
                        let inside = (plan.pad_left..plan.pad_left + 56).contains(&x)
                            && (plan.pad_top..plan.pad_top + 28).contains(&y);
                        let expected = if inside {
                            let source_x = x - plan.pad_left + crop.left;
                            let source_y = y - plan.pad_top + crop.top;
                            sample(&source, WIDTH, component, source_x, source_y)
                        } else {
                            0
                        };
                        assert_eq!(
                            sample(&filtered, WIDTH, component, x, y),
                            expected,
                            "component {component} at ({x},{y})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_scale_and_pad_keep_the_depth_and_leave_the_bars_at_zero() {
        const FLAT_CODE_VALUE: i32 = 2000;
        let plan = PictureProcessing {
            crop: Crop {
                left: 8,
                right: 8,
                top: 0,
                bottom: 0,
            },
            fit: Some(Fit {
                box_width: WIDTH,
                box_height: 16,
                raster_width: WIDTH,
                raster_height: HEIGHT,
                placement: Default::default(),
            }),
            ..Default::default()
        }
        .plan(WIDTH, HEIGHT)
        .unwrap();
        assert!(
            plan.filters
                .iter()
                .any(|filter| filter.starts_with("scale"))
        );
        assert!(plan.pad_left > 0);
        let flat = [0, 1, 2].map(|_| vec![FLAT_CODE_VALUE; (WIDTH * HEIGHT) as usize]);
        let (width, height, filtered) =
            PlanarRgbFilter::new(WIDTH, HEIGHT, PRECISION, &plan.filters)
                .unwrap()
                .filter(&flat)
                .unwrap();
        assert_eq!((width, height), (WIDTH, HEIGHT));
        for component in &filtered {
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let inside = (plan.pad_left..plan.pad_left + plan.scaled_width).contains(&x)
                        && (plan.pad_top..plan.pad_top + plan.scaled_height).contains(&y);
                    let expected = if inside { FLAT_CODE_VALUE } else { 0 };
                    assert_eq!(
                        component[(y * WIDTH + x) as usize],
                        expected,
                        "at ({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn a_frame_of_another_size_is_refused() {
        let mut filter = PlanarRgbFilter::new(WIDTH, HEIGHT, PRECISION, &[]).unwrap();
        let small = [0, 1, 2].map(|_| vec![0; 4]);
        assert!(filter.filter(&small).is_err());
    }
}
