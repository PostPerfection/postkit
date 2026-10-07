use std::collections::VecDeque;
use std::sync::Arc;

use crate::audio_levels::{ChannelLevel, SILENCE_FLOOR_DBFS, dbfs_of_amplitude};

// positions count device sample frames since the seek, the clock the output's emitted count runs on
struct LevelBlock {
    start: u64,
    end: u64,
    peaks: Vec<f32>,
    mean_squares: Vec<f64>,
}

pub(super) struct ChannelMeasure {
    peaks: Vec<f32>,
    mean_squares: Vec<f64>,
}

impl ChannelMeasure {
    pub(super) fn of(interleaved: &[f32], channels: usize) -> Self {
        let mut peaks = vec![0.0f32; channels];
        let mut sums_of_squares = vec![0.0f64; channels];
        if channels == 0 {
            return ChannelMeasure {
                peaks,
                mean_squares: sums_of_squares,
            };
        }
        let mut sample_frames = 0usize;
        for frame in interleaved.chunks_exact(channels) {
            for (channel, sample) in frame.iter().enumerate() {
                peaks[channel] = peaks[channel].max(sample.abs());
                sums_of_squares[channel] += f64::from(*sample) * f64::from(*sample);
            }
            sample_frames += 1;
        }
        let divisor = sample_frames.max(1) as f64;
        ChannelMeasure {
            peaks,
            mean_squares: sums_of_squares.iter().map(|sum| sum / divisor).collect(),
        }
    }

    fn silent(channels: usize) -> Self {
        ChannelMeasure {
            peaks: vec![0.0; channels],
            mean_squares: vec![0.0; channels],
        }
    }
}

// what the feeder measured ahead of the device, read back at the position the device has played to
#[derive(Default)]
pub(super) struct LevelHistory {
    labels: Option<Arc<[String]>>,
    blocks: VecDeque<LevelBlock>,
    // the played position of the previous read, the peak covers what played since
    read_until: u64,
}

impl LevelHistory {
    pub(super) fn push(
        &mut self,
        labels: &Arc<[String]>,
        start: u64,
        end: u64,
        measure: ChannelMeasure,
        played: u64,
        window: u64,
    ) {
        // a reel with other channels starts the meter over
        if self.labels.as_deref() != Some(&**labels) {
            self.blocks.clear();
            self.labels = Some(Arc::clone(labels));
        }
        self.drop_before(played.saturating_sub(window));
        self.blocks.push_back(LevelBlock {
            start,
            end,
            peaks: measure.peaks,
            mean_squares: measure.mean_squares,
        });
    }

    // a stretch the composition has no sound for
    pub(super) fn push_silence(&mut self, start: u64, end: u64, played: u64, window: u64) {
        let Some(labels) = self.labels.clone() else {
            return;
        };
        let silence = ChannelMeasure::silent(labels.len());
        self.push(&labels, start, end, silence, played, window);
    }

    // the device buffer was cleared, what was queued behind `played` will never play
    pub(super) fn discard_unplayed(&mut self, played: u64) {
        self.blocks.retain(|block| block.start < played);
        self.read_until = self.read_until.min(played);
    }

    // the device count starts again from zero
    pub(super) fn restart(&mut self) {
        self.blocks.clear();
        self.read_until = 0;
    }

    pub(super) fn forget(&mut self) {
        self.restart();
        self.labels = None;
    }

    // None until a block names the channels, every channel at the floor when nothing is heard
    pub(super) fn read(
        &mut self,
        played: u64,
        window: u64,
        heard: bool,
    ) -> Option<Vec<ChannelLevel>> {
        let labels = self.labels.clone()?;
        let window_start = played.saturating_sub(window);
        let levels = match heard {
            true => self.played_levels(&labels, played, window_start),
            false => labels.iter().cloned().map(ChannelLevel::silent).collect(),
        };
        self.read_until = played;
        self.drop_before(window_start);
        Some(levels)
    }

    fn played_levels(
        &self,
        labels: &[String],
        played: u64,
        window_start: u64,
    ) -> Vec<ChannelLevel> {
        let mut peaks = vec![0.0f32; labels.len()];
        let mut weighted_squares = vec![0.0f64; labels.len()];
        let mut weighted_frames = 0u64;
        for block in self.blocks.iter().filter(|block| block.start < played) {
            if block.end > self.read_until {
                for (peak, block_peak) in peaks.iter_mut().zip(&block.peaks) {
                    *peak = peak.max(*block_peak);
                }
            }
            let overlap = block
                .end
                .min(played)
                .saturating_sub(block.start.max(window_start));
            for (sum, mean_square) in weighted_squares.iter_mut().zip(&block.mean_squares) {
                *sum += mean_square * overlap as f64;
            }
            weighted_frames += overlap;
        }
        labels
            .iter()
            .zip(peaks.iter().zip(&weighted_squares))
            .map(|(label, (peak, weighted_square))| {
                let rms_dbfs = match weighted_frames {
                    0 => SILENCE_FLOOR_DBFS,
                    frames => dbfs_of_amplitude((weighted_square / frames as f64).sqrt()),
                };
                ChannelLevel {
                    label: label.clone(),
                    peak_dbfs: dbfs_of_amplitude(f64::from(*peak)),
                    rms_dbfs,
                }
            })
            .collect()
    }

    fn drop_before(&mut self, position: u64) {
        while self
            .blocks
            .front()
            .is_some_and(|block| block.end <= position)
        {
            self.blocks.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: usize = 48_000;
    const TONE_HERTZ: f64 = 1_000.0;
    const WINDOW: u64 = 14_400;
    const HALF_SCALE_DBFS: f64 = -6.0206;
    // a sine's RMS is its peak less 3.01 dB
    const HALF_SCALE_SINE_RMS_DBFS: f64 = -9.0309;
    const TOLERANCE_DB: f64 = 0.01;

    // a sine on the first channel and silence on the second, interleaved
    fn half_scale_tone(sample_frames: usize) -> Vec<f32> {
        (0..sample_frames)
            .flat_map(|index| {
                let phase = 2.0 * std::f64::consts::PI * TONE_HERTZ * index as f64;
                [(0.5 * (phase / SAMPLE_RATE as f64).sin()) as f32, 0.0]
            })
            .collect()
    }

    fn labels() -> Arc<[String]> {
        Arc::from(vec!["L".to_string(), "R".to_string()])
    }

    fn history_with_tone(start: u64, sample_frames: usize) -> LevelHistory {
        let mut history = LevelHistory::default();
        let measure = ChannelMeasure::of(&half_scale_tone(sample_frames), 2);
        history.push(
            &labels(),
            start,
            start + sample_frames as u64,
            measure,
            0,
            WINDOW,
        );
        history
    }

    fn assert_db(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < TOLERANCE_DB,
            "{actual} dBFS, expected {expected}"
        );
    }

    #[test]
    fn a_half_scale_sine_reads_minus_six_peak_and_minus_nine_rms() {
        let mut history = history_with_tone(0, SAMPLE_RATE);

        let levels = history.read(SAMPLE_RATE as u64, WINDOW, true).unwrap();

        assert_eq!(levels[0].label, "L");
        assert_db(levels[0].peak_dbfs, HALF_SCALE_DBFS);
        assert_db(levels[0].rms_dbfs, HALF_SCALE_SINE_RMS_DBFS);
        assert_eq!(levels[1].label, "R");
        assert_eq!(levels[1].peak_dbfs, SILENCE_FLOOR_DBFS);
        assert_eq!(levels[1].rms_dbfs, SILENCE_FLOOR_DBFS);
    }

    #[test]
    fn the_peak_resets_on_read_and_the_rms_window_stays() {
        let mut history = history_with_tone(0, SAMPLE_RATE);
        history.read(SAMPLE_RATE as u64, WINDOW, true);

        let again = history.read(SAMPLE_RATE as u64, WINDOW, true).unwrap();

        assert_eq!(again[0].peak_dbfs, SILENCE_FLOOR_DBFS);
        assert_db(again[0].rms_dbfs, HALF_SCALE_SINE_RMS_DBFS);
    }

    #[test]
    fn sound_the_device_has_not_reached_reads_as_silence() {
        let mut history = history_with_tone(SAMPLE_RATE as u64, SAMPLE_RATE);

        let levels = history.read(SAMPLE_RATE as u64 / 2, WINDOW, true).unwrap();

        assert_eq!(levels[0].peak_dbfs, SILENCE_FLOOR_DBFS);
        assert_eq!(levels[0].rms_dbfs, SILENCE_FLOOR_DBFS);
    }

    #[test]
    fn the_rms_covers_the_window_before_the_played_position() {
        let mut history = history_with_tone(0, SAMPLE_RATE);
        let silence_end = 2 * SAMPLE_RATE as u64;
        history.push_silence(SAMPLE_RATE as u64, silence_end, 0, WINDOW);

        let levels = history.read(silence_end, WINDOW, true).unwrap();

        assert_eq!(levels[0].rms_dbfs, SILENCE_FLOOR_DBFS);
        // no read since the tone played, so its peak is still owed
        assert_db(levels[0].peak_dbfs, HALF_SCALE_DBFS);
    }

    #[test]
    fn nothing_is_heard_while_paused() {
        let mut history = history_with_tone(0, SAMPLE_RATE);

        let levels = history.read(SAMPLE_RATE as u64, WINDOW, false).unwrap();

        assert_eq!(levels[0].peak_dbfs, SILENCE_FLOOR_DBFS);
        assert_eq!(levels[0].rms_dbfs, SILENCE_FLOOR_DBFS);
    }

    #[test]
    fn no_channels_until_a_block_names_them() {
        assert!(LevelHistory::default().read(0, WINDOW, true).is_none());
    }
}
