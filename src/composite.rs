use std::path::PathBuf;

use crate::encode::stream_encode::{
    AlphaChannel, DecodeChainInputs, FramesToEncode, decode_chain, encode_frames,
    start_frame_decoder,
};
use crate::encode::{
    DecodeSource, EncodeResult, EncoderInputFormat, FrameRange, PACKED_RGB_BYTES_PER_PIXEL,
    PictureFilters, StreamEncodeOptions, StreamProgress, compress_params, packed_rgb_sample_order,
};
use crate::ffmpeg_decode::{
    FinishedDecode, FrameDecoder, FrameSource, NextFrame, PACKED_RGBA_BYTES_PER_PIXEL,
};
use crate::grok_encoder::SampleOrder;
use crate::picture_findings::PictureFindings;
use crate::picture_processing::PictureProcessing;
use crate::subtitle_raster::{read_sample, source_over, write_sample};

const OPAQUE: u32 = u16::MAX as u32;
const ALPHA_SAMPLE: usize = 3;
const BYTES_PER_SAMPLE: usize = 2;

pub(crate) struct CompositeLayer {
    pub name: String,
    pub input: PathBuf,
    pub decode_source: DecodeSource,
    pub frames: LayerFrames,
    pub picture: PictureProcessing,
}

#[derive(Clone, Copy)]
pub(crate) enum LayerFrames {
    Advancing { frame_range: Option<FrameRange> },
    // a still, decoded once and repeated for every frame of the composite
    Held,
}

impl LayerFrames {
    fn frame_range(self) -> Option<FrameRange> {
        match self {
            LayerFrames::Advancing { frame_range } => frame_range,
            LayerFrames::Held => None,
        }
    }
}

pub(crate) enum CompositeBase {
    Clip(CompositeLayer),
    Black { width: u32, height: u32 },
}

pub(crate) struct CompositeOverlay {
    pub layer: CompositeLayer,
    pub opacity: f64,
}

// timeline frames whose layers stay the same throughout
pub(crate) struct Composite {
    pub base: CompositeBase,
    // bottom to top
    pub overlays: Vec<CompositeOverlay>,
    pub frame_count: u64,
}

// `frame` is packed rgb48 and `overlay` packed rgba64 with straight alpha
fn source_over_frame(frame: &mut [u8], overlay: &[u8], opacity: u16, order: SampleOrder) {
    let opacity = u32::from(opacity);
    let (pixels, _) = frame.as_chunks_mut::<PACKED_RGB_BYTES_PER_PIXEL>();
    let (overlay_pixels, _) = overlay.as_chunks::<PACKED_RGBA_BYTES_PER_PIXEL>();
    for (pixel, overlay_pixel) in pixels.iter_mut().zip(overlay_pixels) {
        let pixel_alpha = u32::from(read_sample(
            overlay_pixel,
            ALPHA_SAMPLE * BYTES_PER_SAMPLE,
            order,
        ));
        let alpha = (pixel_alpha * opacity + OPAQUE / 2) / OPAQUE;
        if alpha == 0 {
            continue;
        }
        for channel in 0..3 {
            let at = channel * BYTES_PER_SAMPLE;
            let mixed = source_over(
                u32::from(read_sample(overlay_pixel, at, order)),
                u32::from(read_sample(pixel, at, order)),
                alpha,
                OPAQUE,
            );
            write_sample(pixel, at, order, mixed as u16);
        }
    }
}

fn sixteen_bit_opacity(opacity: f64) -> u16 {
    (opacity * f64::from(u16::MAX)).round() as u16
}

enum BaseFrames {
    Decoded { name: String, decoder: FrameDecoder },
    Held(Vec<u8>),
    Black,
}

struct OverlayFrames {
    name: String,
    // None for a held still, whose one frame stays in `frame`
    decoder: Option<FrameDecoder>,
    opacity: u16,
    frame: Vec<u8>,
}

pub(crate) struct CompositeFrames {
    base: BaseFrames,
    overlays: Vec<OverlayFrames>,
    order: SampleOrder,
    frame_count: u64,
    frames_read: u64,
    failure: Option<String>,
}

fn ended_early(name: &str, frames_read: u64, frame_count: u64) -> String {
    format!("{name} ended after {frames_read} of the {frame_count} frames it is composited for")
}

impl FrameSource for CompositeFrames {
    fn read_into(&mut self, buffer: &mut [u8]) -> NextFrame {
        if self.frames_read == self.frame_count {
            return NextFrame::Ended;
        }
        match &mut self.base {
            BaseFrames::Decoded { name, decoder } => {
                if let NextFrame::Ended = decoder.read_into(buffer) {
                    self.failure = Some(ended_early(name, self.frames_read, self.frame_count));
                    return NextFrame::Ended;
                }
            }
            BaseFrames::Held(frame) => buffer.copy_from_slice(frame),
            BaseFrames::Black => buffer.fill(0),
        }
        for overlay in &mut self.overlays {
            if let Some(decoder) = &mut overlay.decoder
                && let NextFrame::Ended = decoder.read_into(&mut overlay.frame)
            {
                self.failure = Some(ended_early(
                    &overlay.name,
                    self.frames_read,
                    self.frame_count,
                ));
                return NextFrame::Ended;
            }
            source_over_frame(buffer, &overlay.frame, overlay.opacity, self.order);
        }
        self.frames_read += 1;
        NextFrame::Copied
    }

    fn finish(self, fps: f64, frame_count: u64) -> FinishedDecode {
        let base = match self.base {
            BaseFrames::Decoded { decoder, .. } => decoder.finish(fps, frame_count),
            BaseFrames::Held(_) | BaseFrames::Black => FinishedDecode {
                findings: PictureFindings::default(),
                failure: None,
            },
        };
        let overlay_failures: Vec<String> = self
            .overlays
            .into_iter()
            .filter_map(|overlay| overlay.decoder?.finish(fps, frame_count).failure)
            .collect();
        // a decoder's own failure says why it ended early
        let failure = base
            .failure
            .or(overlay_failures.into_iter().next())
            .or(self.failure);
        FinishedDecode {
            findings: base.findings,
            failure,
        }
    }
}

struct OpenedLayer {
    decoder: FrameDecoder,
    order: SampleOrder,
    width: u32,
    height: u32,
}

fn open_layer(
    layer: &CompositeLayer,
    alpha: AlphaChannel,
    opts: &StreamEncodeOptions,
    params: &crate::grok_encoder::CompressParams,
    on_log: &dyn Fn(&str),
) -> Result<OpenedLayer, String> {
    let probed = crate::ffmpeg_decode::probe(&layer.input, layer.decode_source)?;
    let frame_range = layer.frames.frame_range();
    if let Some(range) = frame_range {
        range.check_against_probe(probed.frame_count)?;
    }
    let plan = match alpha {
        AlphaChannel::Dropped => layer.picture.plan(probed.width, probed.height),
        AlphaChannel::Kept => layer
            .picture
            .plan_with_transparent_pad(probed.width, probed.height),
    }?;
    let (width, height) = (plan.output_width, plan.output_height);
    let chain = decode_chain(
        &DecodeChainInputs {
            decode_source: layer.decode_source,
            read_source_at: opts.read_source_at,
            picture: PictureFilters::Planned {
                plan: &plan,
                fps: opts.fps,
                frame_range,
                extra_filter: None,
            },
            source_colour: &opts.source_colour,
            source: &probed.pixel_format,
            accelerator_active: crate::grok_encoder::plugin_enabled_for(params),
            quality_psnr: opts.quality_psnr,
            postkit_prepares_the_frame: true,
            // the findings describe the base picture alone
            detect_picture_findings: opts.detect_picture_findings && alpha == AlphaChannel::Dropped,
            alpha,
        },
        width,
        height,
        params,
        on_log,
    )?;
    let EncoderInputFormat::PackedRgb(order) = chain.input_format else {
        unreachable!("postkit reads every composited sample, so no layer decodes to planar YUV")
    };
    let decoder = start_frame_decoder(&chain, &layer.input, frame_range, width, height)?;
    Ok(OpenedLayer {
        decoder,
        order,
        width,
        height,
    })
}

fn held_frame(opened: OpenedLayer, bytes_per_pixel: usize, fps: f64) -> Result<Vec<u8>, String> {
    let mut frame = vec![0; opened.width as usize * opened.height as usize * bytes_per_pixel];
    let mut decoder = opened.decoder;
    let read = decoder.read_into(&mut frame);
    if let Some(failure) = decoder.finish(fps, 1).failure {
        return Err(failure);
    }
    match read {
        NextFrame::Copied => Ok(frame),
        NextFrame::Ended => Err("the still decoded to no frame".to_string()),
    }
}

fn named(name: &str) -> impl Fn(String) -> String + '_ {
    move |error| format!("{name}: {error}")
}

pub(crate) fn composite_frames(
    composite: &Composite,
    opts: &StreamEncodeOptions,
    params: &crate::grok_encoder::CompressParams,
    on_log: &dyn Fn(&str),
) -> Result<FramesToEncode<CompositeFrames>, String> {
    let (base, order, width, height) = match &composite.base {
        CompositeBase::Clip(layer) => {
            let opened = open_layer(layer, AlphaChannel::Dropped, opts, params, on_log)
                .map_err(named(&layer.name))?;
            let (order, width, height) = (opened.order, opened.width, opened.height);
            let base = match layer.frames {
                LayerFrames::Advancing { .. } => BaseFrames::Decoded {
                    name: layer.name.clone(),
                    decoder: opened.decoder,
                },
                LayerFrames::Held => BaseFrames::Held(
                    held_frame(opened, PACKED_RGB_BYTES_PER_PIXEL, opts.fps.as_f64())
                        .map_err(named(&layer.name))?,
                ),
            };
            (base, order, width, height)
        }
        CompositeBase::Black { width, height } => {
            let order = packed_rgb_sample_order(*width, *height, params);
            (BaseFrames::Black, order, *width, *height)
        }
    };
    let mut overlays = Vec::new();
    for overlay in &composite.overlays {
        let layer = &overlay.layer;
        let opened = open_layer(layer, AlphaChannel::Kept, opts, params, on_log)
            .map_err(named(&layer.name))?;
        if (opened.width, opened.height) != (width, height) {
            return Err(format!(
                "{} fits to {}x{}, and the picture under it is {width}x{height}",
                layer.name, opened.width, opened.height
            ));
        }
        assert_eq!(
            opened.order, order,
            "one raster and one set of compression parameters take one sample order"
        );
        let (decoder, frame) = match layer.frames {
            LayerFrames::Advancing { .. } => (
                Some(opened.decoder),
                vec![0; width as usize * height as usize * PACKED_RGBA_BYTES_PER_PIXEL],
            ),
            LayerFrames::Held => (
                None,
                held_frame(opened, PACKED_RGBA_BYTES_PER_PIXEL, opts.fps.as_f64())
                    .map_err(named(&layer.name))?,
            ),
        };
        overlays.push(OverlayFrames {
            name: layer.name.clone(),
            decoder,
            opacity: sixteen_bit_opacity(overlay.opacity),
            frame,
        });
    }
    Ok(FramesToEncode {
        source: CompositeFrames {
            base,
            overlays,
            order,
            frame_count: composite.frame_count,
            frames_read: 0,
            failure: None,
        },
        input_format: EncoderInputFormat::PackedRgb(order),
        width,
        height,
        total_frames: composite.frame_count,
    })
}

// each layer carries its own input, window and picture, so those fields of `opts` are not read
pub(crate) fn encode_composite<F>(
    opts: &StreamEncodeOptions,
    composite: &Composite,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    pause: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    mxf_feed: Option<crate::mxf_wrap::J2kFrameSender>,
    on_progress: F,
    on_log: impl Fn(&str),
) -> EncodeResult
where
    F: FnMut(StreamProgress),
{
    let opened = std::fs::create_dir_all(&opts.output_dir)
        .map_err(|e| format!("cannot create {}: {e}", opts.output_dir.display()))
        .and_then(|()| {
            let params = compress_params(opts)?;
            let frames = composite_frames(composite, opts, &params, &on_log)?;
            Ok((params, frames))
        });
    match opened {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb48(pixels: &[[u16; 3]], order: SampleOrder) -> Vec<u8> {
        let mut frame = vec![0; pixels.len() * PACKED_RGB_BYTES_PER_PIXEL];
        for (index, pixel) in pixels.iter().enumerate() {
            for (channel, &sample) in pixel.iter().enumerate() {
                let at = index * PACKED_RGB_BYTES_PER_PIXEL + channel * BYTES_PER_SAMPLE;
                write_sample(&mut frame, at, order, sample);
            }
        }
        frame
    }

    fn rgba64(pixels: &[[u16; 4]], order: SampleOrder) -> Vec<u8> {
        let mut frame = vec![0; pixels.len() * PACKED_RGBA_BYTES_PER_PIXEL];
        for (index, pixel) in pixels.iter().enumerate() {
            for (channel, &sample) in pixel.iter().enumerate() {
                let at = index * PACKED_RGBA_BYTES_PER_PIXEL + channel * BYTES_PER_SAMPLE;
                write_sample(&mut frame, at, order, sample);
            }
        }
        frame
    }

    const BLUE: [u16; 3] = [0, 0, u16::MAX];
    const RED: u16 = u16::MAX;
    const HALF: u16 = 32768;

    #[test]
    fn an_overlay_covers_mixes_or_shows_the_pixel_under_it_by_its_alpha() {
        for order in [SampleOrder::Big, SampleOrder::Little] {
            let mut frame = rgb48(&[BLUE, BLUE, BLUE], order);
            let overlay = rgba64(
                &[[RED, 0, 0, u16::MAX], [RED, 0, 0, HALF], [RED, 0, 0, 0]],
                order,
            );
            source_over_frame(&mut frame, &overlay, sixteen_bit_opacity(1.0), order);
            assert_eq!(
                frame,
                rgb48(&[[RED, 0, 0], [HALF, 0, HALF - 1], BLUE], order),
                "opaque, half and transparent in {order:?} order"
            );
        }
    }

    #[test]
    fn opacity_scales_the_alpha_of_an_opaque_overlay() {
        let order = SampleOrder::Big;
        let mut frame = rgb48(&[BLUE], order);
        let overlay = rgba64(&[[RED, 0, 0, u16::MAX]], order);
        source_over_frame(&mut frame, &overlay, sixteen_bit_opacity(0.5), order);
        assert_eq!(frame, rgb48(&[[HALF, 0, HALF - 1]], order));
    }
}
