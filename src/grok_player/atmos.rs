use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use super::audio::{SoundComposition, SoundReel, reel_location};
use crate::iab::{self, AtmosReader, DIRECT_PATH_DELAY, FILTER_LENGTH, OutputLayout, Renderer};

// renders a frame ahead, the renderer returns each sample DIRECT_PATH_DELAY sample frames late
pub(super) struct AtmosSound {
    renderer: Renderer,
    layout: OutputLayout,
    reader: Option<(PathBuf, AtmosReader)>,
    // the format of the track's first frame with no elements, rendered in place of a frame that fails
    silent_frame: iab::Frame,
    // the composition frame of sound frame 0, which warnings name frames by
    first_frame: u64,
    // the frame the next render continues with, None after a seek
    next_rendered_frame: Option<u64>,
    // the rendered samples of the frame after the one last returned, past its first DIRECT_PATH_DELAY sample frames
    held: Vec<f32>,
    warned_errors: HashSet<String>,
}

pub(super) struct RenderedEditUnit {
    // interleaved in the renderer's channel order for its layout
    pub(super) samples: Vec<f32>,
    pub(super) warnings: Vec<String>,
}

impl AtmosSound {
    pub(super) fn open(
        composition: &SoundComposition,
        layout: OutputLayout,
    ) -> Result<Self, String> {
        let first = composition
            .atmos
            .first()
            .ok_or("the composition has no Atmos reel")?;
        let mut reader = AtmosReader::open(&first.path, first.key.as_ref().map(|key| key.bytes()))?;
        let first_frame = composition.first_frame;
        let frame = reader
            .read_frame(first.entry_edit_unit)
            .and_then(|bytes| iab::parse_frame(bytes).map_err(|error| error.to_string()))
            .map_err(|error| {
                format!(
                    "Atmos frame {first_frame} of {} sets the track's format and is unreadable: {error}",
                    first.path.display()
                )
            })?;
        let sample_count = frame.frame_rate.sample_count(frame.sample_rate);
        let picture_frame_samples = f64::from(frame.sample_rate.hertz()) / composition.fps;
        if (sample_count as f64 - picture_frame_samples).abs() >= 1.0 {
            return Err(format!(
                "{} holds {sample_count} samples a frame at {} Hz, which is not one frame of the {} fps picture",
                first.path.display(),
                frame.sample_rate.hertz(),
                composition.fps
            ));
        }
        let silent_frame = iab::Frame {
            beds: Vec::new(),
            objects: Vec::new(),
            dlc_audio: Vec::new(),
            pcm_audio: Vec::new(),
            authoring_tool_uris: Vec::new(),
            user_data: Vec::new(),
            ..frame
        };
        Ok(AtmosSound {
            renderer: Renderer::new(layout),
            layout,
            reader: Some((first.path.clone(), reader)),
            silent_frame,
            first_frame,
            next_rendered_frame: None,
            held: Vec::new(),
            warned_errors: HashSet::new(),
        })
    }

    pub(super) fn sample_rate(&self) -> u32 {
        self.silent_frame.sample_rate.hertz()
    }

    pub(super) fn layout(&self) -> OutputLayout {
        self.layout
    }

    pub(super) fn set_layout(&mut self, layout: OutputLayout) {
        if layout == self.layout {
            return;
        }
        self.renderer = Renderer::new(layout);
        self.layout = layout;
        self.restart();
    }

    pub(super) fn restart(&mut self) {
        self.next_rendered_frame = None;
    }

    // the samples of frame, from its first sample to its last
    pub(super) fn edit_unit(&mut self, reels: &[SoundReel], frame: u64) -> RenderedEditUnit {
        let mut warnings = Vec::new();
        if self.next_rendered_frame != Some(frame + 1) {
            self.start_at(reels, frame, &mut warnings);
        }
        let following = self.render_or_silence(reels, frame + 1, &mut warnings);
        let delay = self.delay_samples();
        let mut samples = std::mem::replace(&mut self.held, following[delay..].to_vec());
        samples.extend_from_slice(&following[..delay]);
        self.next_rendered_frame = Some(frame + 2);
        RenderedEditUnit { samples, warnings }
    }

    // the frames before it fill the renderer's history as continuous play would
    fn start_at(&mut self, reels: &[SoundReel], frame: u64, warnings: &mut Vec<String>) {
        self.renderer.reset();
        let sample_count = self
            .silent_frame
            .frame_rate
            .sample_count(self.silent_frame.sample_rate);
        let earlier_frames = (FILTER_LENGTH - 1).div_ceil(sample_count) as u64;
        for earlier in frame.saturating_sub(earlier_frames)..frame {
            if self.render(reels, earlier).is_err() {
                self.render_silence();
            }
        }
        let rendered = self.render_or_silence(reels, frame, warnings);
        self.held = rendered[self.delay_samples()..].to_vec();
    }

    fn delay_samples(&self) -> usize {
        DIRECT_PATH_DELAY * self.layout.channel_count()
    }

    fn render_or_silence(
        &mut self,
        reels: &[SoundReel],
        frame: u64,
        warnings: &mut Vec<String>,
    ) -> Vec<f32> {
        match self.render(reels, frame) {
            Ok(samples) => samples,
            Err(error) => {
                if self.warned_errors.insert(error.clone()) {
                    warnings.push(format!(
                        "Atmos frame {} plays as silence: {error}",
                        self.first_frame + frame
                    ));
                }
                self.render_silence()
            }
        }
    }

    fn render(&mut self, reels: &[SoundReel], frame: u64) -> Result<Vec<f32>, String> {
        let Some((reel, entry)) = reel_location(reels, frame) else {
            return Ok(self.render_silence());
        };
        let reader = self.reader_for(&reels[reel])?;
        let bytes = reader.read_frame(entry)?;
        let parsed = iab::parse_frame(bytes).map_err(|error| error.to_string())?;
        if parsed.sample_rate != self.silent_frame.sample_rate
            || parsed.frame_rate != self.silent_frame.frame_rate
        {
            return Err(
                "its sample rate or frame rate differs from the track's first frame".into(),
            );
        }
        let audio = iab::decode_audio(&parsed).map_err(|error| error.to_string())?;
        self.renderer
            .render(&parsed, &audio)
            .map_err(|error| error.to_string())
    }

    fn render_silence(&mut self) -> Vec<f32> {
        self.renderer
            .render(&self.silent_frame, &BTreeMap::new())
            .expect("a frame with no elements renders")
    }

    fn reader_for(&mut self, reel: &SoundReel) -> Result<&mut AtmosReader, String> {
        let open = self
            .reader
            .as_ref()
            .is_some_and(|(path, _)| path == &reel.path);
        if !open {
            self.reader = None;
            let reader = AtmosReader::open(&reel.path, reel.key.as_ref().map(|key| key.bytes()))?;
            self.reader = Some((reel.path.clone(), reader));
        }
        let (_, reader) = self.reader.as_mut().expect("the reel's reader is open");
        Ok(reader)
    }
}
