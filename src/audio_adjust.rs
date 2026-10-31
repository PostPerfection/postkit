use crate::loudness::{DEFAULT_TRUE_PEAK_CEILING_DBTP, LoudnessTarget};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What to do to the audio, in the order ffmpeg applies it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioAdjust {
    /// Gain in dB, positive or negative.
    pub gain_db: Option<f64>,
    /// Fade up from silence over this many seconds from the start.
    pub fade_in_seconds: Option<f64>,
    /// Fade down to silence over this many seconds at the end.
    pub fade_out_seconds: Option<f64>,
}

impl AudioAdjust {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

// the level step and the fades run on the kept sound
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoundFinish {
    pub adjust: AudioAdjust,
    pub loudness_target: Option<LoudnessTarget>,
    pub true_peak_ceiling_dbtp: f64,
}

impl SoundFinish {
    // the target in the leqm=85 or lufs=-23 spelling, the ceiling at its default when not given
    pub fn from_typed(
        adjust: AudioAdjust,
        loudness_target: Option<&str>,
        true_peak_ceiling_dbtp: Option<f64>,
    ) -> Result<Self, String> {
        Ok(SoundFinish {
            adjust,
            loudness_target: loudness_target
                .map(crate::loudness::parse_loudness_target)
                .transpose()?,
            true_peak_ceiling_dbtp: true_peak_ceiling_dbtp
                .unwrap_or(DEFAULT_TRUE_PEAK_CEILING_DBTP),
        })
    }
}

impl Default for SoundFinish {
    fn default() -> Self {
        SoundFinish {
            adjust: AudioAdjust::default(),
            loudness_target: None,
            true_peak_ceiling_dbtp: DEFAULT_TRUE_PEAK_CEILING_DBTP,
        }
    }
}

/// Build the ffmpeg filter chain. `duration_seconds` positions the fade-out,
/// which ffmpeg needs as a start time rather than an offset from the end.
fn filter_chain(adjust: &AudioAdjust, duration_seconds: f64) -> Result<String, String> {
    let mut filters = Vec::new();
    if let Some(gain) = adjust.gain_db {
        if !gain.is_finite() {
            return Err(format!("audio gain {gain} is not a number"));
        }
        filters.push(format!("volume={gain}dB"));
    }
    if let Some(seconds) = adjust.fade_in_seconds {
        check_fade("audio fade-in", seconds, duration_seconds)?;
        filters.push(format!("afade=t=in:st=0:d={seconds}"));
    }
    if let Some(seconds) = adjust.fade_out_seconds {
        check_fade("audio fade-out", seconds, duration_seconds)?;
        // ffmpeg wants the moment the fade starts, not its length from the end
        let start = duration_seconds - seconds;
        filters.push(format!("afade=t=out:st={start}:d={seconds}"));
    }
    Ok(filters.join(","))
}

pub fn check_fade(name: &str, seconds: f64, duration_seconds: f64) -> Result<(), String> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!("{name} of {seconds}s must be greater than zero"));
    }
    if seconds > duration_seconds {
        return Err(format!(
            "{name} of {seconds}s is longer than the {duration_seconds}s it fades"
        ));
    }
    Ok(())
}

/// Running time of a WAV, read from its header.
pub fn duration_seconds(path: &Path) -> Result<f64, String> {
    let layout = crate::wav_io::WavLayout::read(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let rate = layout.spec.sample_rate;
    if rate == 0 {
        return Err(format!("{} declares a sample rate of zero", path.display()));
    }
    Ok(layout.frames() as f64 / rate as f64)
}

/// Apply `adjust` to `input`, writing `output`. Returns the path actually to
/// use, which is `input` unchanged when there is nothing to do.
pub fn apply(
    input: &Path,
    output: &Path,
    adjust: &AudioAdjust,
    duration_seconds: f64,
) -> Result<PathBuf, String> {
    if adjust.is_empty() {
        return Ok(input.to_path_buf());
    }
    let chain = filter_chain(adjust, duration_seconds)?;
    if chain.is_empty() {
        return Ok(input.to_path_buf());
    }

    let status = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-af")
        .arg(&chain)
        // the wrap wants the same PCM it would have had, only quieter
        .args(["-c:a", "pcm_s24le", "-rf64", "auto"])
        .arg(output)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("could not run ffmpeg to adjust the audio: {e}"))?;
    if !status.success() {
        return Err(format!(
            "ffmpeg failed to apply the audio filters '{chain}'"
        ));
    }
    Ok(output.to_path_buf())
}

// how the level of the kept sound is set
#[derive(Debug, Clone, Copy)]
pub enum LevelStep {
    // a typed gain sets the level, and a loudness target beside it is only a reference
    Gain {
        gain_db: f64,
        unapplied_target: Option<LoudnessTarget>,
    },
    Normalise(LoudnessTarget),
    Unchanged,
}

pub fn level_step(gain_db: Option<f64>, target: Option<LoudnessTarget>) -> LevelStep {
    match (gain_db, target) {
        (Some(gain_db), unapplied_target) => LevelStep::Gain {
            gain_db,
            unapplied_target,
        },
        (None, Some(target)) => LevelStep::Normalise(target),
        (None, None) => LevelStep::Unchanged,
    }
}

// runs on the trimmed sound, so a loudness target measures what ships
pub fn finish_kept_sound(
    input: &Path,
    work_dir: &Path,
    finish: &SoundFinish,
    log: impl Fn(&str),
) -> Result<PathBuf, String> {
    let mut path = input.to_path_buf();
    let gain_db = match level_step(finish.adjust.gain_db, finish.loudness_target) {
        LevelStep::Gain {
            gain_db,
            unapplied_target,
        } => {
            if let Some(target) = unapplied_target {
                log(&format!(
                    "Loudness target {} not applied: the gain sets the level",
                    crate::loudness::loudness_target_spec(target)
                ));
            }
            Some(gain_db)
        }
        LevelStep::Normalise(target) => {
            create_work_dir(work_dir)?;
            let out = work_dir.join("loudness.wav");
            let plan = crate::loudness::adjust_loudness(
                &path,
                &out,
                target,
                finish.true_peak_ceiling_dbtp,
            )
            .map_err(|e| e.to_string())?;
            log(&format!(
                "Loudness {:.1} -> {:.1} dB (gain {:+.2} dB, peak {:.2} dBTP)",
                plan.measured_db, plan.target_db, plan.gain_db, plan.resulting_true_peak_dbtp
            ));
            path = out;
            None
        }
        LevelStep::Unchanged => None,
    };
    let adjust = AudioAdjust {
        gain_db,
        ..finish.adjust.clone()
    };
    if adjust.is_empty() {
        return Ok(path);
    }
    create_work_dir(work_dir)?;
    let adjusted = apply(
        &path,
        &work_dir.join("adjusted.wav"),
        &adjust,
        duration_seconds(&path)?,
    )?;
    log("Applied gain/fades");
    Ok(adjusted)
}

fn create_work_dir(work_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(work_dir)
        .map_err(|e| format!("cannot create {}: {e}", work_dir.display()))
}
#[cfg(test)]
mod tests {
    use super::*;

    const DURATION: f64 = 10.0;

    #[test]
    fn nothing_to_do_leaves_the_input_alone() {
        let adjust = AudioAdjust::default();
        assert!(adjust.is_empty());
        let input = Path::new("in.wav");
        assert_eq!(
            apply(input, Path::new("out.wav"), &adjust, DURATION).unwrap(),
            input.to_path_buf(),
            "an empty adjustment must not spawn ffmpeg or rewrite the audio"
        );
    }

    #[test]
    fn gain_and_both_fades_build_one_chain_in_order() {
        let chain = filter_chain(
            &AudioAdjust {
                gain_db: Some(-3.5),
                fade_in_seconds: Some(2.0),
                fade_out_seconds: Some(4.0),
            },
            DURATION,
        )
        .unwrap();
        assert_eq!(
            chain,
            "volume=-3.5dB,afade=t=in:st=0:d=2,afade=t=out:st=6:d=4"
        );
    }

    // the fade-out is positioned from the end, so its start moves with the length
    #[test]
    fn the_fade_out_starts_its_own_length_before_the_end() {
        for (duration, fade, start) in [(10.0, 4.0, 6.0), (100.0, 4.0, 96.0), (5.0, 5.0, 0.0)] {
            let chain = filter_chain(
                &AudioAdjust {
                    fade_out_seconds: Some(fade),
                    ..Default::default()
                },
                duration,
            )
            .unwrap();
            assert_eq!(chain, format!("afade=t=out:st={start}:d={fade}"));
        }
    }

    #[test]
    fn a_fade_longer_than_the_audio_is_refused() {
        for adjust in [
            AudioAdjust {
                fade_in_seconds: Some(DURATION + 1.0),
                ..Default::default()
            },
            AudioAdjust {
                fade_out_seconds: Some(DURATION + 1.0),
                ..Default::default()
            },
        ] {
            let err = filter_chain(&adjust, DURATION).unwrap_err();
            assert!(err.contains("longer than"), "got: {err}");
        }
    }

    #[test]
    fn a_zero_or_negative_fade_is_refused() {
        for seconds in [0.0, -1.0] {
            let err = filter_chain(
                &AudioAdjust {
                    fade_in_seconds: Some(seconds),
                    ..Default::default()
                },
                DURATION,
            )
            .unwrap_err();
            assert!(err.contains("greater than zero"), "got: {err}");
        }
    }

    #[test]
    fn a_typed_gain_sets_the_level_and_leaves_the_target_unapplied() {
        let target = LoudnessTarget::IntegratedLufs(-20.0);
        assert!(matches!(
            level_step(Some(-3.0), Some(target)),
            LevelStep::Gain {
                gain_db,
                unapplied_target: Some(LoudnessTarget::IntegratedLufs(lufs)),
            } if gain_db == -3.0 && lufs == -20.0
        ));
        assert!(matches!(
            level_step(Some(0.0), None),
            LevelStep::Gain {
                unapplied_target: None,
                ..
            }
        ));
        assert!(matches!(
            level_step(None, Some(target)),
            LevelStep::Normalise(LoudnessTarget::IntegratedLufs(lufs)) if lufs == -20.0
        ));
        assert!(matches!(level_step(None, None), LevelStep::Unchanged));
    }

    #[test]
    fn a_gain_beside_a_target_is_applied_alone_and_the_target_logged() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.wav");
        write_silence(&input, 48_000);
        let lines = std::cell::RefCell::new(Vec::new());
        let finished = finish_kept_sound(
            &input,
            &dir.path().join("work"),
            &SoundFinish {
                adjust: AudioAdjust {
                    gain_db: Some(-6.0),
                    ..Default::default()
                },
                loudness_target: Some(LoudnessTarget::LeqM(85.0)),
                ..Default::default()
            },
            |line| lines.borrow_mut().push(line.to_string()),
        )
        .unwrap();
        assert_eq!(finished, dir.path().join("work").join("adjusted.wav"));
        assert_eq!(
            lines.into_inner(),
            vec![
                "Loudness target leqm=85 not applied: the gain sets the level",
                "Applied gain/fades",
            ]
        );
        assert!(!dir.path().join("work").join("loudness.wav").exists());
    }

    fn write_silence(path: &Path, frames: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for _ in 0..frames {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
    }

    // a gain of zero is a real request (it is what --audio-gain 0 means), so it
    // must build a chain rather than being treated as absent
    #[test]
    fn a_zero_gain_still_builds_a_filter() {
        let chain = filter_chain(
            &AudioAdjust {
                gain_db: Some(0.0),
                ..Default::default()
            },
            DURATION,
        )
        .unwrap();
        assert_eq!(chain, "volume=0dB");
    }
}
