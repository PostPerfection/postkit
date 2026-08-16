//! Rasterise [`StyledCue`]s to positioned RGBA bitmaps and alpha-blend them
//! onto a decoded picture frame.
//!
//! This is the burn-in path: instead of asking a decoder's subtitle filter to
//! draw text (which only reaches ffmpeg-decoded input and takes its styling
//! from the subtitle file rather than from our flags), cues are shaped and
//! rasterised here and composited onto the frame buffer, so every input shape
//! that decodes to pixels can carry burnt-in subtitles.
//!
//! Text is shaped by cosmic-text (harfrust shaping, unicode-bidi reordering,
//! per-codepoint fallback across the faces fontdb discovers) and rasterised by
//! swash. Bitmap cues (Interop PNG subs) skip shaping and are scaled to the
//! frame instead.
//!
//! Nothing here knows about the encoder, so a preview or an export can reuse
//! the same two steps.

use std::path::Path;
use std::sync::Arc;

use cosmic_text::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent,
    Weight, Wrap,
};

use crate::subtitle_formats::{HAlign, Rgba, StyledCue, StyledRun, SubtitleError, VAlign};

/// Text height as a fraction of frame height when the caller names none. 1/22nd
/// of the picture is about the DCI house style for a 2K subtitle.
pub const DEFAULT_FONT_SIZE_RATIO: f32 = 1.0 / 22.0;

/// Distance from the anchored edge as a fraction of frame height, used when the
/// cue carries no vposition.
const DEFAULT_MARGIN_RATIO: f32 = 0.08;

/// Line box height as a multiple of the text height.
const DEFAULT_LINE_HEIGHT_RATIO: f32 = 1.25;

/// Underline thickness as a fraction of the text height.
const UNDERLINE_THICKNESS_RATIO: f32 = 0.06;

/// How far below the baseline the underline sits, as a fraction of text height.
const UNDERLINE_OFFSET_RATIO: f32 = 0.13;

/// Outline thickness as a fraction of the text height. About 2px at the default
/// text height on a 2K frame.
pub const DEFAULT_OUTLINE_WIDTH_RATIO: f32 = 0.05;

/// How far down and right the shadow sits, as a fraction of the text height.
/// About 3px at the default text height on a 2K frame.
const SHADOW_OFFSET_RATIO: f32 = 0.06;

/// What is drawn under the text to lift it off the picture.
///
/// The names match the SMPTE subtitle `Effect` attribute, so a burnt track and
/// a packaged track can be asked for the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BurnEffect {
    None,
    Outline,
    #[default]
    Shadow,
}

/// How a cue is drawn when it does not say for itself.
#[derive(Debug, Clone)]
pub struct BurnStyle {
    /// Text height as a fraction of the frame height.
    pub font_size_ratio: f32,
    /// Line box height as a multiple of the text height.
    pub line_height_ratio: f32,
    /// Distance from the anchored edge as a fraction of the frame height, for
    /// cues with no vposition of their own.
    pub margin_ratio: f32,
    /// Colour for runs that carry none.
    pub default_colour: Rgba,
    /// What is drawn under the text: nothing, an outline, or a drop shadow.
    pub effect: BurnEffect,
    /// Colour of that outline or shadow.
    pub effect_colour: Rgba,
    /// Outline thickness as a fraction of the text height.
    pub outline_width_ratio: f32,
    /// Horizontal stretch of the rasterised text. 1.0 leaves it alone.
    pub x_scale: f32,
    /// Vertical stretch of the text, applied to the text height itself.
    pub y_scale: f32,
    /// How long a cue takes to ramp up from transparent at its start.
    pub fade_up_ms: u64,
    /// How long a cue takes to ramp down to transparent at its end.
    pub fade_down_ms: u64,
}

impl Default for BurnStyle {
    fn default() -> Self {
        BurnStyle {
            font_size_ratio: DEFAULT_FONT_SIZE_RATIO,
            line_height_ratio: DEFAULT_LINE_HEIGHT_RATIO,
            margin_ratio: DEFAULT_MARGIN_RATIO,
            default_colour: Rgba {
                r: 255,
                g: 255,
                b: 255,
                a: 255,
            },
            effect: BurnEffect::default(),
            effect_colour: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            outline_width_ratio: DEFAULT_OUTLINE_WIDTH_RATIO,
            x_scale: 1.0,
            y_scale: 1.0,
            fade_up_ms: 0,
            fade_down_ms: 0,
        }
    }
}

impl BurnStyle {
    fn validate(&self) -> Result<(), String> {
        for (name, scale) in [("x_scale", self.x_scale), ("y_scale", self.y_scale)] {
            if scale <= 0.0 || !scale.is_finite() {
                return Err(format!("a burn needs a positive {name}, got {scale}"));
            }
        }
        Ok(())
    }
}

/// Read an effect name, as the SMPTE `Effect` attribute spells it.
pub fn parse_burn_effect(text: &str) -> Result<BurnEffect, String> {
    match text.to_ascii_lowercase().as_str() {
        "none" => Ok(BurnEffect::None),
        "outline" => Ok(BurnEffect::Outline),
        "shadow" => Ok(BurnEffect::Shadow),
        _ => Err(format!(
            "{text} is not an effect: pick none, outline, or shadow"
        )),
    }
}

/// Percent of a whole, as the appearance flags take their sizes.
const PERCENT_DIVISOR: f32 = 100.0;

/// Text taller than the frame itself, which no caller can mean.
const MAX_FONT_SIZE_PERCENT: f32 = 100.0;

/// The appearance settings a caller named on the command line, each left
/// `None` when the flag was not given so the base style keeps its own value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BurnStyleOverrides {
    /// Text height as a percent of the frame height, so 4.5 means 0.045.
    pub font_size_percent: Option<f32>,
    pub colour: Option<Rgba>,
    pub effect: Option<BurnEffect>,
    pub effect_colour: Option<Rgba>,
    /// Outline thickness as a percent of the text height.
    pub outline_width_percent: Option<f32>,
    pub x_scale: Option<f32>,
    pub y_scale: Option<f32>,
    pub fade_up_ms: Option<u64>,
    pub fade_down_ms: Option<u64>,
}

impl BurnStyleOverrides {
    /// Lay every named value over `base` and check the result is drawable.
    pub fn apply(&self, base: BurnStyle) -> Result<BurnStyle, String> {
        let mut style = base;
        if let Some(percent) = self.font_size_percent {
            if !percent.is_finite() || percent <= 0.0 || percent > MAX_FONT_SIZE_PERCENT {
                return Err(format!(
                    "a font size is a percent of the frame height above 0 and up to {MAX_FONT_SIZE_PERCENT}, got {percent}"
                ));
            }
            style.font_size_ratio = percent / PERCENT_DIVISOR;
        }
        if let Some(percent) = self.outline_width_percent {
            if !percent.is_finite() || percent < 0.0 {
                return Err(format!(
                    "an outline width is a percent of the text height of 0 or more, got {percent}"
                ));
            }
            style.outline_width_ratio = percent / PERCENT_DIVISOR;
        }
        if let Some(colour) = self.colour {
            style.default_colour = colour;
        }
        if let Some(effect) = self.effect {
            style.effect = effect;
        }
        if let Some(colour) = self.effect_colour {
            style.effect_colour = colour;
        }
        if let Some(scale) = self.x_scale {
            style.x_scale = scale;
        }
        if let Some(scale) = self.y_scale {
            style.y_scale = scale;
        }
        if let Some(span) = self.fade_up_ms {
            style.fade_up_ms = span;
        }
        if let Some(span) = self.fade_down_ms {
            style.fade_down_ms = span;
        }
        style.validate()?;
        Ok(style)
    }
}

/// A rasterised cue: un-premultiplied RGBA8 pixels and the frame-space
/// top-left corner they belong at. `x`/`y` may be negative or run past the
/// frame; [`composite_rgb48`] clips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionedBitmap {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// RGBA8, four bytes per pixel, `width * height * 4` long.
    pub pixels: Vec<u8>,
}

impl PositionedBitmap {
    fn blank(x: i32, y: i32, width: u32, height: u32) -> Self {
        PositionedBitmap {
            x,
            y,
            width,
            height,
            pixels: vec![0u8; (width as usize) * (height as usize) * 4],
        }
    }

    /// Source-over one un-premultiplied RGBA pixel, no bounds check.
    fn blend(&mut self, px: u32, py: u32, colour: [u8; 4]) {
        let at = ((py as usize) * (self.width as usize) + px as usize) * 4;
        let source_alpha = colour[3] as u32;
        if source_alpha == 0 {
            return;
        }
        let keep = self.pixels[at + 3] as u32 * (255 - source_alpha) / 255;
        let out_alpha = source_alpha + keep;
        if out_alpha == 0 {
            self.pixels[at..at + 4].copy_from_slice(&[0; 4]);
            return;
        }
        for (channel, &source) in colour.iter().take(3).enumerate() {
            let mixed = source as u32 * source_alpha
                + self.pixels[at + channel] as u32 * keep
                + out_alpha / 2;
            self.pixels[at + channel] = (mixed / out_alpha) as u8;
        }
        self.pixels[at + 3] = out_alpha as u8;
    }
}

/// Byte order of the 16-bit samples in a packed rgb48 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleOrder {
    Big,
    Little,
}

/// Alpha-blend `bitmaps` onto a packed rgb48 frame, in order, clipping each to
/// the frame. `frame` is `width * height * 6` bytes of interleaved RGB with
/// 16-bit samples in `order`.
///
/// The source is 8-bit un-premultiplied RGBA, so a channel is lifted to 16 bits
/// by `* 257` before mixing and the whole blend stays in integer math.
pub fn composite_rgb48(
    frame: &mut [u8],
    width: u32,
    height: u32,
    order: SampleOrder,
    bitmaps: &[PositionedBitmap],
) {
    let stride = width as usize * 6;
    for bitmap in bitmaps {
        for row in 0..bitmap.height as i32 {
            let frame_y = bitmap.y + row;
            if frame_y < 0 || frame_y >= height as i32 {
                continue;
            }
            for column in 0..bitmap.width as i32 {
                let frame_x = bitmap.x + column;
                if frame_x < 0 || frame_x >= width as i32 {
                    continue;
                }
                let source = ((row as usize) * (bitmap.width as usize) + column as usize) * 4;
                let alpha = bitmap.pixels[source + 3] as u32;
                if alpha == 0 {
                    continue;
                }
                let at = frame_y as usize * stride + frame_x as usize * 6;
                for channel in 0..3 {
                    let sample = at + channel * 2;
                    let destination = match order {
                        SampleOrder::Big => u16::from_be_bytes([frame[sample], frame[sample + 1]]),
                        SampleOrder::Little => {
                            u16::from_le_bytes([frame[sample], frame[sample + 1]])
                        }
                    } as u32;
                    let source_sample = bitmap.pixels[source + channel] as u32 * 257;
                    let mixed = (source_sample * alpha + destination * (255 - alpha) + 127) / 255;
                    let bytes = (mixed.min(u16::MAX as u32) as u16).to_be_bytes();
                    match order {
                        SampleOrder::Big => {
                            frame[sample] = bytes[0];
                            frame[sample + 1] = bytes[1];
                        }
                        SampleOrder::Little => {
                            frame[sample] = bytes[1];
                            frame[sample + 1] = bytes[0];
                        }
                    }
                }
            }
        }
    }
}

/// A cue list ready to be burnt onto frames, addressed by frame number.
///
/// Holds the rasterizer behind a lock because the encoder threads share one
/// instance, and caches the last render: the active cue set changes a few times
/// a second at most, so nearly every frame reuses the bitmaps of the one before
/// it and the lock is held only long enough to clone an `Arc`.
pub struct SubtitleBurn {
    cues: Vec<StyledCue>,
    style: BurnStyle,
    /// Frames per second the cue timings are read against.
    fps: f64,
    state: std::sync::Mutex<BurnState>,
}

/// What a cached render belongs to: every active cue with the fade alpha it was
/// drawn at, plus the frame width and height.
///
/// The alpha is part of the key because it is baked into the rendered pixels, so
/// a cue mid-fade re-renders every frame while one at full strength keeps
/// hitting the cache.
type RenderKey = (Vec<(usize, u8)>, u32, u32);

struct BurnState {
    rasterizer: SubtitleRasterizer,
    rendered_for: Option<RenderKey>,
    rendered: Arc<Vec<PositionedBitmap>>,
}

impl std::fmt::Debug for SubtitleBurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubtitleBurn")
            .field("cues", &self.cues.len())
            .field("fps", &self.fps)
            .field("style", &self.style)
            .finish()
    }
}

impl SubtitleBurn {
    /// Prepare `cues` for burning at `fps`, drawing text with `font` (or the
    /// system faces when `None`).
    ///
    /// Refused when a text cue is present and no face is available at all,
    /// since that would silently encode a subtitle-free picture.
    pub fn new(
        cues: Vec<StyledCue>,
        font: Option<&Path>,
        style: BurnStyle,
        fps: f64,
    ) -> Result<Self, SubtitleError> {
        if fps <= 0.0 || !fps.is_finite() {
            return Err(SubtitleError::Parse(format!(
                "a burn needs a positive frame rate, got {fps}"
            )));
        }
        style.validate().map_err(SubtitleError::Parse)?;
        let rasterizer = SubtitleRasterizer::new(font)?;
        let has_text = cues.iter().any(|cue| cue.image.is_none());
        if has_text && !rasterizer.has_font() {
            return Err(SubtitleError::Parse(
                "no font found to burn subtitles with: install a system font or pass one".into(),
            ));
        }
        Ok(SubtitleBurn {
            cues,
            style,
            fps,
            state: std::sync::Mutex::new(BurnState {
                rasterizer,
                rendered_for: None,
                rendered: Arc::new(Vec::new()),
            }),
        })
    }

    /// Indices of the cues covering `frame_index`. Two frames with the same
    /// list burn to the same picture, which is what lets a held still encode
    /// once per cue change instead of once per frame.
    pub fn active_cues(&self, frame_index: u64) -> Vec<usize> {
        let time_ms = self.time_ms(frame_index);
        self.cues
            .iter()
            .enumerate()
            .filter(|(_, cue)| time_ms >= cue.start_ms && time_ms < cue.end_ms)
            .map(|(at, _)| at)
            .collect()
    }

    /// Burn the cues covering `frame_index` into a packed rgb48 frame.
    pub fn burn_rgb48(
        &self,
        frame: &mut [u8],
        width: u32,
        height: u32,
        order: SampleOrder,
        frame_index: u64,
    ) -> Result<(), SubtitleError> {
        let want = (width as usize) * (height as usize) * 6;
        if frame.len() != want {
            return Err(SubtitleError::Parse(format!(
                "a {width}x{height} rgb48 frame is {want} bytes, got {}",
                frame.len()
            )));
        }
        let time_ms = self.time_ms(frame_index);
        let key: Vec<(usize, u8)> = self
            .active_cues(frame_index)
            .into_iter()
            .map(|at| (at, fade_alpha(&self.cues[at], time_ms, &self.style)))
            .collect();
        let bitmaps = {
            let mut state = self.state.lock().expect("subtitle burn state");
            if state.rendered_for.as_ref() != Some(&(key.clone(), width, height)) {
                let due: Vec<StyledCue> =
                    key.iter().map(|(at, _)| self.cues[*at].clone()).collect();
                let rendered =
                    state
                        .rasterizer
                        .render(&due, time_ms, width, height, &self.style)?;
                state.rendered = Arc::new(rendered);
                state.rendered_for = Some((key, width, height));
            }
            Arc::clone(&state.rendered)
        };
        composite_rgb48(frame, width, height, order, &bitmaps);
        Ok(())
    }

    fn time_ms(&self, frame_index: u64) -> u64 {
        (frame_index as f64 * 1000.0 / self.fps).round() as u64
    }
}

/// Shaping and rasterisation state: the discovered font database and the glyph
/// raster cache, both reused across cues and frames.
///
/// [`FontSystem::new`] scans every system font directory, which costs tens to
/// hundreds of milliseconds, so build one and keep it.
pub struct SubtitleRasterizer {
    fonts: FontSystem,
    cache: SwashCache,
    /// Family name of the caller's font file, asked for as the primary family
    /// so the system faces only fill in codepoints it lacks. `None` asks for
    /// the generic sans-serif.
    primary_family: Option<String>,
}

impl std::fmt::Debug for SubtitleRasterizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubtitleRasterizer")
            .field("primary_family", &self.primary_family)
            .field("face_count", &self.fonts.db().len())
            .finish()
    }
}

impl SubtitleRasterizer {
    /// Discover the system fonts, and register `font` (a .ttf / .otf / .ttc)
    /// alongside them as the primary family when one is given.
    pub fn new(font: Option<&Path>) -> Result<Self, SubtitleError> {
        let mut fonts = FontSystem::new();
        let mut primary_family = None;
        if let Some(path) = font {
            let bytes = std::fs::read(path)?;
            let ids = fonts
                .db_mut()
                .load_font_source(cosmic_text::fontdb::Source::Binary(Arc::new(bytes)));
            primary_family = ids
                .first()
                .and_then(|id| fonts.db().face(*id))
                .and_then(|face| face.families.first())
                .map(|(name, _)| name.clone());
            if primary_family.is_none() {
                return Err(SubtitleError::Parse(format!(
                    "no usable font face in {}",
                    path.display()
                )));
            }
        }
        Ok(SubtitleRasterizer {
            fonts,
            cache: SwashCache::new(),
            primary_family,
        })
    }

    /// Whether any face at all is available. False means fontdb found no system
    /// font and the caller supplied none, so text cues would render empty.
    pub fn has_font(&self) -> bool {
        !self.fonts.db().is_empty()
    }

    /// Rasterise every cue covering `time_ms` for a `width` x `height` frame.
    ///
    /// Cues are returned in input order, so a later cue paints over an earlier
    /// one. A cue that shapes to nothing yields no bitmap.
    pub fn render(
        &mut self,
        cues: &[StyledCue],
        time_ms: u64,
        width: u32,
        height: u32,
        style: &BurnStyle,
    ) -> Result<Vec<PositionedBitmap>, SubtitleError> {
        let mut bitmaps = Vec::new();
        for cue in cues {
            if time_ms < cue.start_ms || time_ms >= cue.end_ms {
                continue;
            }
            let alpha = fade_alpha(cue, time_ms, style);
            if alpha == 0 {
                continue;
            }
            let drawn = match &cue.image {
                Some(path) => Some(render_bitmap_cue(path, cue, width, height, style)?),
                None => self.render_text_cue(cue, width, height, style),
            };
            let Some(mut bitmap) = drawn else {
                continue;
            };
            scale_alpha(&mut bitmap, alpha);
            bitmaps.push(bitmap);
        }
        Ok(bitmaps)
    }

    fn render_text_cue(
        &mut self,
        cue: &StyledCue,
        width: u32,
        height: u32,
        style: &BurnStyle,
    ) -> Option<PositionedBitmap> {
        let text = cue.plain_text();
        if text.trim().is_empty() {
            return None;
        }
        let font_size = (height as f32 * style.font_size_ratio * style.y_scale).max(1.0);
        let line_height = font_size * style.line_height_ratio;

        let family = self.primary_family.clone();
        let base = base_attrs(family.as_deref(), style);
        let spans: Vec<(&str, Attrs)> = cue
            .runs
            .iter()
            .filter(|run| !run.text.is_empty())
            .map(|run| (run.text.as_str(), run_attrs(&base, run, style)))
            .collect();
        if spans.is_empty() {
            return None;
        }

        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(font_size, line_height));
        // Cue lines are pre-broken by the subtitle format, so no wrapping: an
        // over-wide line is clipped at composite time rather than reflowed.
        buffer.set_wrap(&mut self.fonts, Wrap::None);
        buffer.set_size(&mut self.fonts, None, None);
        buffer.set_rich_text(&mut self.fonts, spans, &base, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);

        let underlines = underlined_byte_ranges(&cue.runs);
        let lines: Vec<LaidOutLine> = buffer
            .layout_runs()
            .map(|run| LaidOutLine {
                width: run.line_w,
                top: run.line_top,
                height: run.line_height,
                baseline: run.line_y - run.line_top,
                glyphs: run
                    .glyphs
                    .iter()
                    .map(|glyph| {
                        let physical = glyph.physical((0.0, 0.0), 1.0);
                        LaidOutGlyph {
                            key: physical.cache_key,
                            x: physical.x,
                            y: physical.y,
                            start: glyph.start,
                            left: glyph.x,
                            advance: glyph.w,
                            colour: glyph.color_opt,
                        }
                    })
                    .collect(),
            })
            .collect();
        if lines.is_empty() {
            return None;
        }

        let block_width = lines.iter().fold(0.0_f32, |widest, l| widest.max(l.width));
        let block_height = lines
            .iter()
            .fold(0.0_f32, |tallest, l| tallest.max(l.top + l.height));
        // Glyphs overhang their line box (descenders, accents), so pad by one
        // line height all round rather than clipping at the block edge.
        let pad = line_height.ceil() as i32;
        let bitmap_width = (block_width.ceil() as i32 + pad * 2).max(1) as u32;
        let bitmap_height = (block_height.ceil() as i32 + pad * 2).max(1) as u32;
        let (origin_x, origin_y) = anchor(
            cue,
            style,
            width,
            height,
            block_width * style.x_scale,
            block_height,
        );
        let mut bitmap = PositionedBitmap::blank(
            origin_x - (pad as f32 * style.x_scale).round() as i32,
            origin_y - pad,
            bitmap_width,
            bitmap_height,
        );

        let layout = TextLayout {
            lines,
            underlines,
            align: cue.align.unwrap_or(HAlign::Center),
            block_width,
            pad: pad as f32,
            font_size,
            default_colour: style.default_colour,
        };
        self.draw_effect(&mut bitmap, &layout, style);
        self.draw_text(&mut bitmap, &layout, None);
        Some(stretch_horizontally(bitmap, style.x_scale))
    }

    /// Draw the outline or shadow that sits under the text.
    ///
    /// The same glyphs are laid down again in the effect colour, then dilated or
    /// offset, so bidi order, font fallback and italics carry through without
    /// this pass knowing about any of them.
    fn draw_effect(
        &mut self,
        bitmap: &mut PositionedBitmap,
        layout: &TextLayout,
        style: &BurnStyle,
    ) {
        let colour = [
            style.effect_colour.r,
            style.effect_colour.g,
            style.effect_colour.b,
            style.effect_colour.a,
        ];
        let (dilation, offset) = match style.effect {
            BurnEffect::None => return,
            BurnEffect::Outline => (
                (layout.font_size * style.outline_width_ratio)
                    .round()
                    .max(1.0) as i32,
                0,
            ),
            BurnEffect::Shadow => (
                0,
                (layout.font_size * SHADOW_OFFSET_RATIO).round().max(1.0) as i32,
            ),
        };
        let mut layer = PositionedBitmap::blank(bitmap.x, bitmap.y, bitmap.width, bitmap.height);
        self.draw_text(&mut layer, layout, Some(colour));
        if dilation > 0 {
            layer = dilate(&layer, dilation, colour);
        }
        blend_layer(bitmap, &layer, offset);
    }

    /// Blit every glyph and underline bar of `layout` onto `bitmap`, in
    /// `force_colour` when the caller wants one flat colour.
    fn draw_text(
        &mut self,
        bitmap: &mut PositionedBitmap,
        layout: &TextLayout,
        force_colour: Option<[u8; 4]>,
    ) {
        for line in &layout.lines {
            let line_x = match layout.align {
                HAlign::Left => 0.0,
                HAlign::Center => (layout.block_width - line.width) / 2.0,
                HAlign::Right => layout.block_width - line.width,
            };
            let line_origin_x = layout.pad + line_x;
            let line_origin_y = layout.pad + line.top + line.baseline;
            for glyph in &line.glyphs {
                let colour = resolve_colour(glyph.colour, layout.default_colour, force_colour);
                self.blit_glyph(
                    bitmap,
                    glyph,
                    line_origin_x + glyph.x as f32,
                    line_origin_y + glyph.y as f32,
                    colour,
                    force_colour.is_some(),
                );
            }
            draw_underlines(
                bitmap,
                line,
                line_origin_x,
                line_origin_y,
                layout,
                force_colour,
            );
        }
    }

    fn blit_glyph(
        &mut self,
        bitmap: &mut PositionedBitmap,
        glyph: &LaidOutGlyph,
        origin_x: f32,
        origin_y: f32,
        colour: [u8; 4],
        ignore_glyph_colour: bool,
    ) {
        let Some(image) = self.cache.get_image(&mut self.fonts, glyph.key).as_ref() else {
            return;
        };
        let colour_bitmap = match image.content {
            SwashContent::Mask => false,
            SwashContent::Color => true,
            // Never asked for: the cache key carries no subpixel bin here.
            SwashContent::SubpixelMask => return,
        };
        let left = origin_x.round() as i32 + image.placement.left;
        let top = origin_y.round() as i32 - image.placement.top;
        let (glyph_width, glyph_height) = (image.placement.width, image.placement.height);
        let flat_colour = !colour_bitmap || ignore_glyph_colour;
        for row in 0..glyph_height as i32 {
            let py = top + row;
            if py < 0 || py >= bitmap.height as i32 {
                continue;
            }
            for column in 0..glyph_width as i32 {
                let px = left + column;
                if px < 0 || px >= bitmap.width as i32 {
                    continue;
                }
                let at = (row as usize) * (glyph_width as usize) + column as usize;
                let source = if flat_colour {
                    let coverage = if colour_bitmap {
                        image.data[at * 4 + 3]
                    } else {
                        image.data[at]
                    } as u32;
                    [
                        colour[0],
                        colour[1],
                        colour[2],
                        ((coverage * colour[3] as u32 + 127) / 255) as u8,
                    ]
                } else {
                    let rgba = &image.data[at * 4..at * 4 + 4];
                    [rgba[0], rgba[1], rgba[2], rgba[3]]
                };
                bitmap.blend(px as u32, py as u32, source);
            }
        }
    }
}

fn base_attrs<'a>(family: Option<&'a str>, style: &BurnStyle) -> Attrs<'a> {
    let colour = style.default_colour;
    let mut attrs = Attrs::new().color(Color::rgba(colour.r, colour.g, colour.b, colour.a));
    if let Some(name) = family {
        attrs = attrs.family(Family::Name(name));
    }
    attrs
}

fn run_attrs<'a>(base: &Attrs<'a>, run: &StyledRun, style: &BurnStyle) -> Attrs<'a> {
    let mut attrs = base.clone();
    if run.bold {
        attrs = attrs.weight(Weight::BOLD);
    }
    if run.italic {
        attrs = attrs.style(Style::Italic);
    }
    let colour = run.color.unwrap_or(style.default_colour);
    attrs.color(Color::rgba(colour.r, colour.g, colour.b, colour.a))
}

/// A shaped cue and everything the glyph pass needs to place it, so the text
/// and the effect under it are drawn from one description.
struct TextLayout {
    lines: Vec<LaidOutLine>,
    /// Byte ranges of the cue text that are underlined.
    underlines: Vec<(usize, usize)>,
    align: HAlign,
    /// Width of the widest line, which the shorter lines are aligned against.
    block_width: f32,
    /// Blank border around the text block, in pixels.
    pad: f32,
    font_size: f32,
    default_colour: Rgba,
}

/// The colour a glyph is drawn in: the caller's flat colour when it names one,
/// otherwise the run's own colour, otherwise the style default.
fn resolve_colour(run_colour: Option<Color>, default: Rgba, force: Option<[u8; 4]>) -> [u8; 4] {
    force.unwrap_or_else(|| {
        run_colour
            .map(|c| [c.r(), c.g(), c.b(), c.a()])
            .unwrap_or([default.r, default.g, default.b, default.a])
    })
}

/// One visual line of a shaped cue, in visual (post-bidi) order.
struct LaidOutLine {
    width: f32,
    top: f32,
    height: f32,
    /// Baseline offset from the top of the line box.
    baseline: f32,
    glyphs: Vec<LaidOutGlyph>,
}

struct LaidOutGlyph {
    key: cosmic_text::CacheKey,
    x: i32,
    y: i32,
    /// Byte offset of this glyph's cluster in the cue text, which is what ties
    /// a glyph back to the run that styled it.
    start: usize,
    /// Left edge of the glyph's cluster in the line, before bidi-independent
    /// rounding, used to place an underline.
    left: f32,
    advance: f32,
    colour: Option<Color>,
}

/// Byte ranges of `runs` whose text is underlined, as offsets into the
/// concatenated cue text.
fn underlined_byte_ranges(runs: &[StyledRun]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut at = 0usize;
    for run in runs {
        let end = at + run.text.len();
        if run.underline && end > at {
            ranges.push((at, end));
        }
        at = end;
    }
    ranges
}

/// Paint the underline bar under every glyph whose cluster falls in an
/// underlined run. cosmic-text carries no underline decoration, so the bar is
/// drawn per glyph and adjacent glyphs join into a continuous rule.
fn draw_underlines(
    bitmap: &mut PositionedBitmap,
    line: &LaidOutLine,
    line_origin_x: f32,
    baseline_y: f32,
    layout: &TextLayout,
    force_colour: Option<[u8; 4]>,
) {
    if layout.underlines.is_empty() {
        return;
    }
    let font_size = layout.font_size;
    let thickness = (font_size * UNDERLINE_THICKNESS_RATIO).round().max(1.0) as i32;
    let top = (baseline_y + font_size * UNDERLINE_OFFSET_RATIO).round() as i32;
    for glyph in &line.glyphs {
        if !layout
            .underlines
            .iter()
            .any(|(s, e)| glyph.start >= *s && glyph.start < *e)
        {
            continue;
        }
        let colour = resolve_colour(glyph.colour, layout.default_colour, force_colour);
        let start_x = (line_origin_x + glyph.left).round() as i32;
        let end_x = (line_origin_x + glyph.left + glyph.advance).round() as i32;
        for row in top..top + thickness {
            if row < 0 || row >= bitmap.height as i32 {
                continue;
            }
            for column in start_x..end_x {
                if column < 0 || column >= bitmap.width as i32 {
                    continue;
                }
                bitmap.blend(column as u32, row as u32, colour);
            }
        }
    }
}

/// Grow a flat-coloured layer's alpha by `radius` pixels in every direction, so
/// the text coverage becomes a band the text itself then sits inside.
fn dilate(layer: &PositionedBitmap, radius: i32, colour: [u8; 4]) -> PositionedBitmap {
    let mut grown = PositionedBitmap::blank(layer.x, layer.y, layer.width, layer.height);
    let spread: Vec<(i32, i32)> = (-radius..=radius)
        .flat_map(|row| (-radius..=radius).map(move |column| (column, row)))
        .filter(|(column, row)| column * column + row * row <= radius * radius)
        .collect();
    for row in 0..layer.height as i32 {
        for column in 0..layer.width as i32 {
            let from = ((row as usize) * (layer.width as usize) + column as usize) * 4;
            let alpha = layer.pixels[from + 3];
            if alpha == 0 {
                continue;
            }
            for (column_step, row_step) in &spread {
                let px = column + column_step;
                let py = row + row_step;
                if px < 0 || py < 0 || px >= layer.width as i32 || py >= layer.height as i32 {
                    continue;
                }
                let to = ((py as usize) * (layer.width as usize) + px as usize) * 4;
                if grown.pixels[to + 3] < alpha {
                    grown.pixels[to..to + 4]
                        .copy_from_slice(&[colour[0], colour[1], colour[2], alpha]);
                }
            }
        }
    }
    grown
}

/// Source-over `layer` onto `target` shifted `offset` pixels down and right,
/// clipping to the target.
fn blend_layer(target: &mut PositionedBitmap, layer: &PositionedBitmap, offset: i32) {
    for row in 0..layer.height as i32 {
        let py = row + offset;
        if py < 0 || py >= target.height as i32 {
            continue;
        }
        for column in 0..layer.width as i32 {
            let px = column + offset;
            if px < 0 || px >= target.width as i32 {
                continue;
            }
            let from = ((row as usize) * (layer.width as usize) + column as usize) * 4;
            let source = [
                layer.pixels[from],
                layer.pixels[from + 1],
                layer.pixels[from + 2],
                layer.pixels[from + 3],
            ];
            target.blend(px as u32, py as u32, source);
        }
    }
}

/// Resample a bitmap to `scale` times its width with a bilinear filter, keeping
/// its rows and its placement.
///
/// cosmic-text shapes at one size in both axes, so an anisotropic stretch has to
/// happen on the finished raster rather than in the shaper.
fn stretch_horizontally(bitmap: PositionedBitmap, scale: f32) -> PositionedBitmap {
    let target_width = ((bitmap.width as f32 * scale).round() as u32).max(1);
    if target_width == bitmap.width {
        return bitmap;
    }
    let mut stretched = PositionedBitmap::blank(bitmap.x, bitmap.y, target_width, bitmap.height);
    let step = bitmap.width as f32 / target_width as f32;
    for row in 0..bitmap.height as usize {
        let source_row = row * bitmap.width as usize;
        let target_row = row * target_width as usize;
        for column in 0..target_width as usize {
            let source = (column as f32 + 0.5) * step - 0.5;
            let fraction = source - source.floor();
            let left = (source.floor() as i32).clamp(0, bitmap.width as i32 - 1) as usize;
            let right = (left + 1).min(bitmap.width as usize - 1);
            let from_left = (source_row + left) * 4;
            let from_right = (source_row + right) * 4;
            // Mixed premultiplied, so the transparent side of an edge cannot
            // drag its colour into the visible pixel.
            let left_alpha = bitmap.pixels[from_left + 3] as f32 * (1.0 - fraction);
            let right_alpha = bitmap.pixels[from_right + 3] as f32 * fraction;
            let alpha = left_alpha + right_alpha;
            let to = (target_row + column) * 4;
            stretched.pixels[to + 3] = alpha.round().min(255.0) as u8;
            if alpha <= 0.0 {
                continue;
            }
            for channel in 0..3 {
                let mixed = bitmap.pixels[from_left + channel] as f32 * left_alpha
                    + bitmap.pixels[from_right + channel] as f32 * right_alpha;
                stretched.pixels[to + channel] = (mixed / alpha).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    stretched
}

/// Multiply every pixel's alpha by `alpha / 255`.
fn scale_alpha(bitmap: &mut PositionedBitmap, alpha: u8) {
    if alpha == u8::MAX {
        return;
    }
    for pixel in bitmap.pixels.chunks_exact_mut(4) {
        pixel[3] = ((pixel[3] as u32 * alpha as u32 + 127) / 255) as u8;
    }
}

/// How much of `cue` is visible at `time_ms`, as an 8-bit multiplier: it ramps
/// up over `fade_up_ms` from the cue's start and down over `fade_down_ms` to its
/// end. A cue shorter than the two ramps together takes whichever is lower, so
/// it never reaches full strength.
fn fade_alpha(cue: &StyledCue, time_ms: u64, style: &BurnStyle) -> u8 {
    let full = u8::MAX as u64;
    let up = match style.fade_up_ms {
        0 => full,
        span => time_ms.saturating_sub(cue.start_ms).min(span) * full / span,
    };
    let down = match style.fade_down_ms {
        0 => full,
        span => cue.end_ms.saturating_sub(time_ms).min(span) * full / span,
    };
    up.min(down) as u8
}

/// Decode a bitmap cue's PNG and place it. The image is scaled down to fit the
/// frame if it is larger, and otherwise used at its own size, matching how an
/// Interop bitmap sub is authored against a known picture size.
fn render_bitmap_cue(
    path: &Path,
    cue: &StyledCue,
    width: u32,
    height: u32,
    style: &BurnStyle,
) -> Result<PositionedBitmap, SubtitleError> {
    let (image_width, image_height, pixels) = decode_png_rgba(path)?;
    let scale = (width as f32 / image_width as f32)
        .min(height as f32 / image_height as f32)
        .min(1.0);
    let (scaled_width, scaled_height, scaled) = if scale >= 1.0 {
        (image_width, image_height, pixels)
    } else {
        let target_width = ((image_width as f32 * scale).round() as u32).max(1);
        let target_height = ((image_height as f32 * scale).round() as u32).max(1);
        (
            target_width,
            target_height,
            nearest_neighbour(
                &pixels,
                image_width,
                image_height,
                target_width,
                target_height,
            ),
        )
    };
    let (x, y) = anchor(
        cue,
        style,
        width,
        height,
        scaled_width as f32,
        scaled_height as f32,
    );
    Ok(PositionedBitmap {
        x,
        y,
        width: scaled_width,
        height: scaled_height,
        pixels: scaled,
    })
}

/// Decode a PNG to un-premultiplied RGBA8.
fn decode_png_rgba(path: &Path) -> Result<(u32, u32, Vec<u8>), SubtitleError> {
    let file = std::fs::File::open(path)?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|e| SubtitleError::Parse(format!("{}: {e}", path.display())))?;
    let mut buffer = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|e| SubtitleError::Parse(format!("{}: {e}", path.display())))?;
    buffer.truncate(info.buffer_size());
    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => buffer,
        (png::ColorType::Rgba, png::BitDepth::Sixteen) => {
            buffer.chunks_exact(2).map(|s| s[0]).collect()
        }
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => buffer
            .chunks_exact(2)
            .flat_map(|s| [s[0], s[0], s[0], s[1]])
            .collect(),
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Sixteen) => buffer
            .chunks_exact(4)
            .flat_map(|s| [s[0], s[0], s[0], s[2]])
            .collect(),
        (colour, depth) => {
            return Err(SubtitleError::Parse(format!(
                "{}: unsupported PNG {colour:?} at {depth:?} bits",
                path.display()
            )));
        }
    };
    Ok((info.width, info.height, rgba))
}

fn nearest_neighbour(
    pixels: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (target_width as usize) * (target_height as usize) * 4];
    for row in 0..target_height {
        let source_row = (row as u64 * source_height as u64 / target_height as u64)
            .min(source_height as u64 - 1) as usize;
        for column in 0..target_width {
            let source_column = (column as u64 * source_width as u64 / target_width as u64)
                .min(source_width as u64 - 1) as usize;
            let from = (source_row * source_width as usize + source_column) * 4;
            let to = ((row as usize) * (target_width as usize) + column as usize) * 4;
            out[to..to + 4].copy_from_slice(&pixels[from..from + 4]);
        }
    }
    out
}

/// Frame-space top-left corner of a `block_width` x `block_height` block, from
/// the cue's alignment and vertical position.
///
/// vposition is a percent of frame height measured from the anchored edge, the
/// way Interop DCSubtitle and PAC give it: from the top for a top anchor, from
/// the bottom for a bottom anchor, and as a centre offset for a middle anchor.
fn anchor(
    cue: &StyledCue,
    style: &BurnStyle,
    width: u32,
    height: u32,
    block_width: f32,
    block_height: f32,
) -> (i32, i32) {
    let frame_width = width as f32;
    let frame_height = height as f32;
    let margin = frame_height * style.margin_ratio;
    let inset = cue
        .vposition
        .map(|percent| frame_height * percent / 100.0)
        .unwrap_or(margin);
    let x = match cue.align.unwrap_or(HAlign::Center) {
        HAlign::Left => margin,
        HAlign::Center => (frame_width - block_width) / 2.0,
        HAlign::Right => frame_width - block_width - margin,
    };
    let y = match cue.valign.unwrap_or(VAlign::Bottom) {
        VAlign::Top => inset,
        VAlign::Middle => (frame_height - block_height) / 2.0 + inset - margin,
        VAlign::Bottom => frame_height - block_height - inset,
    };
    (x.round() as i32, y.round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle_formats::StyledRun;

    /// A rasterizer with a font, or `None` when this machine has none and the
    /// test should be skipped rather than failing on the environment.
    fn rasterizer() -> Option<SubtitleRasterizer> {
        let raster = SubtitleRasterizer::new(None).expect("font discovery");
        if raster.has_font() {
            Some(raster)
        } else {
            eprintln!("skipping: fontdb found no system font on this machine");
            None
        }
    }

    fn cue(text: &str) -> StyledCue {
        StyledCue::text(0, 1000, vec![StyledRun::plain(text)])
    }

    /// The default style with nothing drawn under the text.
    fn plain_style() -> BurnStyle {
        BurnStyle {
            effect: BurnEffect::None,
            ..BurnStyle::default()
        }
    }

    /// Total alpha in a bitmap, which is nonzero exactly when something drew.
    fn coverage(bitmap: &PositionedBitmap) -> u64 {
        bitmap.pixels.chunks_exact(4).map(|p| p[3] as u64).sum()
    }

    const WIDTH: u32 = 512;
    const HEIGHT: u32 = 256;

    #[test]
    fn a_text_cue_draws_pixels_only_while_it_is_on_screen() {
        let Some(mut raster) = rasterizer() else {
            return;
        };
        let cues = [cue("Hello")];
        let style = BurnStyle::default();
        let before = raster.render(&cues, 0, WIDTH, HEIGHT, &style).unwrap();
        assert_eq!(before.len(), 1, "the cue starts at 0 and must render");
        assert!(coverage(&before[0]) > 0, "shaped text drew nothing");
        let after = raster.render(&cues, 1000, WIDTH, HEIGHT, &style).unwrap();
        assert!(after.is_empty(), "the cue's end is exclusive");
    }

    #[test]
    fn alignment_moves_the_block_to_the_named_corner() {
        let Some(mut raster) = rasterizer() else {
            return;
        };
        let style = BurnStyle::default();
        let mut placed = Vec::new();
        for (halign, valign) in [
            (HAlign::Left, VAlign::Top),
            (HAlign::Center, VAlign::Middle),
            (HAlign::Right, VAlign::Bottom),
        ] {
            let mut c = cue("Hello");
            c.align = Some(halign);
            c.valign = Some(valign);
            let out = raster.render(&[c], 0, WIDTH, HEIGHT, &style).unwrap();
            assert_eq!(out.len(), 1);
            assert!(coverage(&out[0]) > 0, "{halign:?}/{valign:?} drew nothing");
            placed.push((out[0].x, out[0].y, out[0].width, out[0].height));
        }
        let (left_x, top_y, _, _) = placed[0];
        let (centre_x, middle_y, centre_w, centre_h) = placed[1];
        let (right_x, bottom_y, right_w, _) = placed[2];

        assert!(left_x < centre_x, "left must sit left of centre");
        assert!(centre_x < right_x, "right must sit right of centre");
        assert!(top_y < middle_y, "top must sit above middle");
        assert!(middle_y < bottom_y, "bottom must sit below middle");
        // The centred block's centre is the frame's centre, within a pixel of
        // rounding.
        let centre = centre_x + centre_w as i32 / 2;
        assert!(
            (centre - WIDTH as i32 / 2).abs() <= 2,
            "centred block centred at {centre}, frame centre {}",
            WIDTH / 2
        );
        assert!(
            (middle_y + centre_h as i32 / 2 - HEIGHT as i32 / 2).abs() <= 2,
            "middle block is not vertically centred"
        );
        assert!(
            right_x + right_w as i32 <= WIDTH as i32 + centre_w as i32,
            "right-aligned block ran off the frame"
        );
    }

    #[test]
    fn vposition_measures_from_the_anchored_edge() {
        let Some(mut raster) = rasterizer() else {
            return;
        };
        let style = BurnStyle::default();
        let mut top = cue("Hello");
        top.valign = Some(VAlign::Top);
        top.vposition = Some(10.0);
        let mut bottom = cue("Hello");
        bottom.valign = Some(VAlign::Bottom);
        bottom.vposition = Some(10.0);
        let top_out = raster.render(&[top], 0, WIDTH, HEIGHT, &style).unwrap();
        let bottom_out = raster.render(&[bottom], 0, WIDTH, HEIGHT, &style).unwrap();
        // 10% of 256 is 25.6 rows below the top edge.
        let expected_top = (HEIGHT as f32 * 0.1).round() as i32;
        let block_height = top_out[0].height as i32;
        assert!(
            (top_out[0].y + block_height - expected_top).abs() < block_height,
            "top-anchored block at y={} for a 10% inset",
            top_out[0].y
        );
        assert!(
            bottom_out[0].y > top_out[0].y,
            "a bottom anchor must sit lower than a top anchor at the same inset"
        );
    }

    #[test]
    fn bold_and_italic_change_what_is_drawn() {
        let Some(mut raster) = rasterizer() else {
            return;
        };
        let style = BurnStyle::default();
        let plain = raster
            .render(&[cue("Hamburgefonstiv")], 0, WIDTH, HEIGHT, &style)
            .unwrap();
        let bold_run = StyledRun {
            text: "Hamburgefonstiv".into(),
            bold: true,
            ..StyledRun::plain("")
        };
        let bold = raster
            .render(
                &[StyledCue::text(0, 1000, vec![bold_run])],
                0,
                WIDTH,
                HEIGHT,
                &style,
            )
            .unwrap();
        assert_ne!(
            plain[0].pixels, bold[0].pixels,
            "bold rendered the same pixels as plain"
        );
    }

    #[test]
    fn a_run_colour_overrides_the_default_colour() {
        let Some(mut raster) = rasterizer() else {
            return;
        };
        let red = StyledRun {
            text: "Hello".into(),
            color: Some(Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            }),
            ..StyledRun::plain("")
        };
        // No effect, so every solid pixel is text rather than the shadow the
        // default style draws under it.
        let out = raster
            .render(
                &[StyledCue::text(0, 1000, vec![red])],
                0,
                WIDTH,
                HEIGHT,
                &plain_style(),
            )
            .unwrap();
        let opaque: Vec<[u8; 4]> = out[0]
            .pixels
            .chunks_exact(4)
            .filter(|p| p[3] > 200)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect();
        assert!(!opaque.is_empty(), "no solid text pixels to check");
        assert!(
            opaque.iter().all(|p| p[0] > 200 && p[1] < 60 && p[2] < 60),
            "solid pixels are not red: {:?}",
            &opaque[..opaque.len().min(4)]
        );
    }

    #[test]
    fn underline_adds_coverage_below_the_text() {
        let Some(mut raster) = rasterizer() else {
            return;
        };
        let style = BurnStyle::default();
        let plain = raster
            .render(&[cue("Hello")], 0, WIDTH, HEIGHT, &style)
            .unwrap();
        let underlined_run = StyledRun {
            text: "Hello".into(),
            underline: true,
            ..StyledRun::plain("")
        };
        let underlined = raster
            .render(
                &[StyledCue::text(0, 1000, vec![underlined_run])],
                0,
                WIDTH,
                HEIGHT,
                &style,
            )
            .unwrap();
        assert!(
            coverage(&underlined[0]) > coverage(&plain[0]),
            "the underline bar added no coverage"
        );
    }

    #[test]
    fn a_bitmap_cue_composites_its_png() {
        let dir = tempfile::tempdir().unwrap();
        let png_path = dir.path().join("cue.png");
        write_solid_png(&png_path, 64, 32, [0, 255, 0, 255]);
        let mut cue = StyledCue::text(0, 1000, Vec::new());
        cue.image = Some(png_path);
        cue.valign = Some(VAlign::Bottom);
        let mut raster = SubtitleRasterizer::new(None).unwrap();
        let out = raster
            .render(&[cue], 500, WIDTH, HEIGHT, &BurnStyle::default())
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].width, out[0].height), (64, 32));
        assert_eq!(&out[0].pixels[..4], &[0, 255, 0, 255]);
        assert!(
            out[0].y + 32 <= HEIGHT as i32,
            "bottom-anchored bitmap ran off the frame"
        );
    }

    fn write_solid_png(path: &Path, width: u32, height: u32, rgba: [u8; 4]) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let data: Vec<u8> = rgba
            .iter()
            .copied()
            .cycle()
            .take((width * height * 4) as usize)
            .collect();
        writer.write_image_data(&data).unwrap();
    }

    #[test]
    fn compositing_half_alpha_white_mixes_the_frame_by_hand_computed_amounts() {
        // One 2x1 rgb48be frame at mid grey (0x4000), half-alpha white on the
        // left pixel only.
        let mut frame = vec![
            0x40, 0x00, 0x40, 0x00, 0x40, 0x00, 0x40, 0x00, 0x40, 0x00, 0x40, 0x00,
        ];
        let bitmap = PositionedBitmap {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            pixels: vec![255, 255, 255, 128],
        };
        composite_rgb48(&mut frame, 2, 1, SampleOrder::Big, &[bitmap]);
        // white lifted to 16 bits is 255*257 = 65535, so the mix is
        // (65535*128 + 16384*127 + 127) / 255 = 10469375 / 255 = 41056.
        assert_eq!(u16::from_be_bytes([frame[0], frame[1]]), 41056);
        assert_eq!(u16::from_be_bytes([frame[2], frame[3]]), 41056);
        assert_eq!(u16::from_be_bytes([frame[4], frame[5]]), 41056);
        // The right pixel is untouched.
        assert_eq!(&frame[6..], &[0x40, 0x00, 0x40, 0x00, 0x40, 0x00]);
    }

    #[test]
    fn compositing_respects_sample_order_and_clips_to_the_frame() {
        let mut big = vec![0u8; 6];
        let mut little = vec![0u8; 6];
        let bitmap = PositionedBitmap {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            pixels: vec![255, 0, 0, 255],
        };
        composite_rgb48(
            &mut big,
            1,
            1,
            SampleOrder::Big,
            std::slice::from_ref(&bitmap),
        );
        composite_rgb48(&mut little, 1, 1, SampleOrder::Little, &[bitmap]);
        assert_eq!(u16::from_be_bytes([big[0], big[1]]), 65535);
        assert_eq!(u16::from_le_bytes([little[0], little[1]]), 65535);
        assert_eq!(big[0..2], [0xff, 0xff]);

        // A bitmap straddling the top-left corner paints only its visible part.
        let mut frame = vec![0u8; 2 * 2 * 6];
        let straddle = PositionedBitmap {
            x: -1,
            y: -1,
            width: 2,
            height: 2,
            pixels: vec![255u8; 16],
        };
        composite_rgb48(&mut frame, 2, 2, SampleOrder::Big, &[straddle]);
        assert_eq!(u16::from_be_bytes([frame[0], frame[1]]), 65535);
        // Only pixel (0,0) is covered; the rest of the 2x2 frame stays black.
        assert!(
            frame[6..].iter().all(|&b| b == 0),
            "clipping painted outside"
        );
    }

    #[test]
    fn a_fade_ramps_alpha_over_the_span_it_is_given() {
        let cue = cue("Hello");
        let style = BurnStyle {
            fade_up_ms: 200,
            fade_down_ms: 400,
            ..BurnStyle::default()
        };
        assert_eq!(fade_alpha(&cue, 0, &style), 0, "a cue starts transparent");
        assert_eq!(fade_alpha(&cue, 100, &style), 127, "half way up the ramp");
        assert_eq!(fade_alpha(&cue, 200, &style), 255, "full at the ramp's end");
        assert_eq!(fade_alpha(&cue, 500, &style), 255, "full between the ramps");
        assert_eq!(fade_alpha(&cue, 600, &style), 255, "the down ramp starts");
        assert_eq!(fade_alpha(&cue, 800, &style), 127, "half way down the ramp");
        assert_eq!(fade_alpha(&cue, 999, &style), 0, "transparent at the end");
        assert_eq!(
            fade_alpha(&cue, 0, &BurnStyle::default()),
            255,
            "no ramps means full strength throughout"
        );
    }

    #[test]
    fn overlapping_ramps_on_a_short_cue_take_the_lower_one() {
        let short = StyledCue::text(0, 100, vec![StyledRun::plain("Hi")]);
        let style = BurnStyle {
            fade_up_ms: 100,
            fade_down_ms: 100,
            ..BurnStyle::default()
        };
        assert_eq!(fade_alpha(&short, 50, &style), 127);
        for time_ms in 0..100 {
            let alpha = fade_alpha(&short, time_ms, &style);
            assert!(
                alpha <= 127,
                "a cue as short as both ramps reached {alpha} at {time_ms}ms"
            );
        }
    }

    #[test]
    fn a_scale_that_is_not_positive_and_finite_is_refused() {
        for (field, style) in [
            (
                "x_scale",
                BurnStyle {
                    x_scale: 0.0,
                    ..BurnStyle::default()
                },
            ),
            (
                "x_scale",
                BurnStyle {
                    x_scale: f32::NAN,
                    ..BurnStyle::default()
                },
            ),
            (
                "y_scale",
                BurnStyle {
                    y_scale: -1.0,
                    ..BurnStyle::default()
                },
            ),
            (
                "y_scale",
                BurnStyle {
                    y_scale: f32::INFINITY,
                    ..BurnStyle::default()
                },
            ),
        ] {
            let err = SubtitleBurn::new(Vec::new(), None, style, 24.0).unwrap_err();
            assert!(
                err.to_string().contains(field),
                "{field} was accepted, or reported as something else: {err}"
            );
        }
    }

    #[test]
    fn the_scales_stretch_the_text_in_one_axis_each() {
        let Some(mut raster) = rasterizer() else {
            return;
        };
        let base = raster
            .render(&[cue("Hello")], 0, WIDTH, HEIGHT, &plain_style())
            .unwrap();
        let taller = raster
            .render(
                &[cue("Hello")],
                0,
                WIDTH,
                HEIGHT,
                &BurnStyle {
                    y_scale: 2.0,
                    ..plain_style()
                },
            )
            .unwrap();
        assert!(
            taller[0].height > base[0].height * 3 / 2,
            "y_scale 2.0 left a {}px block against {}px",
            taller[0].height,
            base[0].height
        );
        let wider = raster
            .render(
                &[cue("Hello")],
                0,
                WIDTH,
                HEIGHT,
                &BurnStyle {
                    x_scale: 2.0,
                    ..plain_style()
                },
            )
            .unwrap();
        assert_eq!(
            wider[0].height, base[0].height,
            "x_scale must not touch the height"
        );
        assert!(
            wider[0].width.abs_diff(base[0].width * 2) <= 1,
            "x_scale 2.0 left a {}px wide bitmap against {}px",
            wider[0].width,
            base[0].width
        );
    }

    #[test]
    fn an_effect_draws_effect_colour_pixels_the_plain_style_does_not() {
        let Some(mut raster) = rasterizer() else {
            return;
        };
        let green = Rgba {
            r: 0,
            g: 255,
            b: 0,
            a: 255,
        };
        let plain = raster
            .render(&[cue("Hello")], 0, WIDTH, HEIGHT, &plain_style())
            .unwrap();
        for effect in [BurnEffect::Outline, BurnEffect::Shadow] {
            let style = BurnStyle {
                effect,
                effect_colour: green,
                ..BurnStyle::default()
            };
            let out = raster
                .render(&[cue("Hello")], 0, WIDTH, HEIGHT, &style)
                .unwrap();
            assert_eq!(
                (out[0].width, out[0].height),
                (plain[0].width, plain[0].height),
                "{effect:?} changed the block size"
            );
            let solid_green = out[0]
                .pixels
                .chunks_exact(4)
                .filter(|p| p[3] > 200 && p[1] > 200 && p[0] < 60 && p[2] < 60)
                .count();
            assert!(solid_green > 0, "{effect:?} drew no effect-colour pixels");
            assert!(
                coverage(&out[0]) > coverage(&plain[0]),
                "{effect:?} added no coverage"
            );
        }
    }

    #[test]
    fn effect_names_parse_in_any_case_and_an_unknown_one_is_refused() {
        assert_eq!(parse_burn_effect("none").unwrap(), BurnEffect::None);
        assert_eq!(parse_burn_effect("Outline").unwrap(), BurnEffect::Outline);
        assert_eq!(parse_burn_effect("SHADOW").unwrap(), BurnEffect::Shadow);
        let err = parse_burn_effect("glow").unwrap_err();
        assert!(
            err.contains("none") && err.contains("outline") && err.contains("shadow"),
            "got: {err}"
        );
    }

    #[test]
    fn a_full_set_of_overrides_changes_every_field() {
        let red = Rgba {
            r: 255,
            g: 0,
            b: 0,
            a: 128,
        };
        let blue = Rgba {
            r: 0,
            g: 0,
            b: 255,
            a: 255,
        };
        let overrides = BurnStyleOverrides {
            font_size_percent: Some(9.0),
            colour: Some(red),
            effect: Some(BurnEffect::Outline),
            effect_colour: Some(blue),
            outline_width_percent: Some(12.5),
            x_scale: Some(1.5),
            y_scale: Some(0.5),
            fade_up_ms: Some(200),
            fade_down_ms: Some(300),
        };
        let style = overrides.apply(BurnStyle::default()).unwrap();
        assert_eq!(style.font_size_ratio, 0.09);
        assert_eq!(style.default_colour, red);
        assert_eq!(style.effect, BurnEffect::Outline);
        assert_eq!(style.effect_colour, blue);
        assert_eq!(style.outline_width_ratio, 0.125);
        assert_eq!(style.x_scale, 1.5);
        assert_eq!(style.y_scale, 0.5);
        assert_eq!(style.fade_up_ms, 200);
        assert_eq!(style.fade_down_ms, 300);
    }

    #[test]
    fn no_overrides_leaves_the_base_style_alone() {
        let base = BurnStyle::default();
        let style = BurnStyleOverrides::default().apply(base.clone()).unwrap();
        assert_eq!(style.font_size_ratio, base.font_size_ratio);
        assert_eq!(style.line_height_ratio, base.line_height_ratio);
        assert_eq!(style.margin_ratio, base.margin_ratio);
        assert_eq!(style.default_colour, base.default_colour);
        assert_eq!(style.effect, base.effect);
        assert_eq!(style.effect_colour, base.effect_colour);
        assert_eq!(style.outline_width_ratio, base.outline_width_ratio);
        assert_eq!(style.x_scale, base.x_scale);
        assert_eq!(style.y_scale, base.y_scale);
        assert_eq!(style.fade_up_ms, base.fade_up_ms);
        assert_eq!(style.fade_down_ms, base.fade_down_ms);
    }

    #[test]
    fn an_override_outside_its_range_is_refused() {
        for (wanted, overrides) in [
            (
                "font size",
                BurnStyleOverrides {
                    font_size_percent: Some(0.0),
                    ..BurnStyleOverrides::default()
                },
            ),
            (
                "font size",
                BurnStyleOverrides {
                    font_size_percent: Some(150.0),
                    ..BurnStyleOverrides::default()
                },
            ),
            (
                "x_scale",
                BurnStyleOverrides {
                    x_scale: Some(0.0),
                    ..BurnStyleOverrides::default()
                },
            ),
            (
                "outline width",
                BurnStyleOverrides {
                    outline_width_percent: Some(-1.0),
                    ..BurnStyleOverrides::default()
                },
            ),
        ] {
            let err = overrides.apply(BurnStyle::default()).unwrap_err();
            assert!(
                err.contains(wanted),
                "{overrides:?} was accepted, or reported as something else: {err}"
            );
        }
    }

    #[test]
    fn a_font_file_with_no_face_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-font.ttf");
        std::fs::write(&path, b"this is not a font").unwrap();
        let err = SubtitleRasterizer::new(Some(&path)).unwrap_err();
        assert!(
            err.to_string().contains("no usable font face"),
            "got: {err}"
        );
    }
}
