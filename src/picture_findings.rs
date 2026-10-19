//! Black and frozen picture runs, reported by ffmpeg's blackdetect and
//! freezedetect while a stream encode decodes.
//!
//! Neither filter accepts the rgb48be the encoder reads, so they run on a
//! `split` branch of their own: put them in the main chain and ffmpeg quietly
//! round trips every frame through yuv444p16le on the way to the compressor.

use crate::encode::FrameRate;
use crate::timecode::Timecode;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::Path;

#[cfg(feature = "ffmpeg")]
pub(crate) mod detection_graph;

/// ffmpeg's blackdetect default: a black run shorter than this is not reported.
const BLACK_MINIMUM_DURATION_SECONDS: f64 = 2.0;
/// ffmpeg's blackdetect default: a pixel at or below this fraction of full
/// scale counts as black.
const BLACK_PIXEL_THRESHOLD: f64 = 0.10;
/// ffmpeg's freezedetect default: a frozen run shorter than this is not
/// reported.
const FREEZE_MINIMUM_DURATION_SECONDS: f64 = 2.0;

const BLACK_START: &str = "black_start:";
const BLACK_END: &str = "black_end:";
const FREEZE_START: &str = "lavfi.freezedetect.freeze_start:";
const FREEZE_END: &str = "lavfi.freezedetect.freeze_end:";

/// A run of output frames, both ends included.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameInterval {
    pub first_frame: u64,
    pub last_frame: u64,
}

impl FrameInterval {
    fn from_seconds(start: f64, end: f64, fps: f64, frame_count: u64) -> Self {
        let first_frame = (start * fps).round().max(0.0) as u64;
        let last_from_end = ((end * fps).round() as i64 - 1).max(0) as u64;
        let last_frame = match frame_count {
            0 => last_from_end,
            count => last_from_end.min(count - 1),
        };
        Self {
            first_frame,
            last_frame: last_frame.max(first_frame),
        }
    }

    fn describe(&self, condition: &str, fps: f64) -> String {
        let timecode_fps = fps.round() as u8;
        format!(
            "{condition} picture from {} to {} (frames {} to {})",
            Timecode::from_frames(self.first_frame, timecode_fps),
            Timecode::from_frames(self.last_frame, timecode_fps),
            self.first_frame,
            self.last_frame
        )
    }
}

/// The black and frozen runs one encode found.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PictureFindings {
    pub black: Vec<FrameInterval>,
    pub frozen: Vec<FrameInterval>,
}

impl PictureFindings {
    /// One log line per interval, black runs first.
    pub fn describe(&self, fps: f64) -> Vec<String> {
        let black = self.black.iter().map(|run| run.describe("black", fps));
        let frozen = self.frozen.iter().map(|run| run.describe("frozen", fps));
        black.chain(frozen).collect()
    }
}

/// The two detectors at the thresholds every pass uses.
fn detection_filters() -> String {
    format!(
        "blackdetect=black_min_duration={BLACK_MINIMUM_DURATION_SECONDS}:\
         pixel_black_th={BLACK_PIXEL_THRESHOLD},\
         freezedetect=duration={FREEZE_MINIMUM_DURATION_SECONDS}"
    )
}

/// Run the detectors over finished picture essence, for a package this process
/// did not encode.
///
/// ffmpeg decodes the whole file, so the frame numbers are the essence's own: a
/// reel the composition enters late still reports from its first frame. The
/// decode is ffmpeg's software J2K decoder, which manages a few frames a second
/// at DCP bitrates, so a feature takes hours. Encrypted essence is the caller's
/// to refuse, because ffmpeg cannot decrypt AS-DCP and reads it as garbage.
pub fn detect_in_essence(
    essence: &Path,
    fps: f64,
    frame_count: u64,
) -> Result<PictureFindings, String> {
    let mut ffmpeg = std::process::Command::new("ffmpeg");
    ffmpeg.args(["-hide_banner", "-nostats", "-i"]).arg(essence);
    let detection = detection_lines(ffmpeg, essence)?;
    Ok(parse_ffmpeg_stderr(&detection, fps, frame_count))
}

// a held image is frozen on purpose, so only its black runs count
pub fn detect_black_in_held_image(
    image: &Path,
    fps: FrameRate,
    hold_frames: u64,
) -> Result<Vec<FrameInterval>, String> {
    // every frame of a hold is the same, so one run just past the minimum answers for the whole hold
    let measured_frames =
        hold_frames.min((BLACK_MINIMUM_DURATION_SECONDS * fps.as_f64()).ceil() as u64 + 1);
    let mut ffmpeg = std::process::Command::new("ffmpeg");
    ffmpeg
        .args(["-hide_banner", "-nostats", "-loop", "1", "-framerate"])
        .arg(fps.ffmpeg_filter_value())
        .arg("-i")
        .arg(image)
        .arg("-frames:v")
        .arg(measured_frames.to_string());
    let detection = detection_lines(ffmpeg, image)?;
    let black = parse_ffmpeg_stderr(&detection, fps.as_f64(), measured_frames).black;
    if black.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![FrameInterval {
        first_frame: 0,
        last_frame: hold_frames - 1,
    }])
}

fn detection_lines(mut ffmpeg: std::process::Command, input: &Path) -> Result<Vec<String>, String> {
    let mut ffmpeg = ffmpeg
        .arg("-vf")
        .arg(detection_filters())
        .args(["-f", "null", "-"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run ffmpeg: {e}"))?;

    let stderr = ffmpeg.stderr.take().expect("stderr is piped");
    let mut detection = Vec::new();
    let mut last_other = String::new();
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        if is_detection_line(&line) {
            detection.push(line);
        } else if !line.trim().is_empty() {
            last_other = line;
        }
    }
    let status = ffmpeg
        .wait()
        .map_err(|e| format!("cannot wait for ffmpeg: {e}"))?;
    if !status.success() {
        return Err(match last_other.trim() {
            "" => format!("ffmpeg could not decode {}", input.display()),
            reason => reason.to_string(),
        });
    }
    Ok(detection)
}

fn is_detection_line(line: &str) -> bool {
    line.contains(BLACK_START) || line.contains(FREEZE_START) || line.contains(FREEZE_END)
}

fn tagged_seconds(line: &str, tag: &str) -> Option<f64> {
    let digits: String = line
        .split(tag)
        .nth(1)?
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse().ok()
}

/// Turn ffmpeg's detection lines into output frame numbers at `fps`.
///
/// `frame_count` is how many frames the decode produced. freezedetect prints no
/// `freeze_end` for a run that reaches the last frame, so that count is what
/// closes it, and a zero count leaves such a run out.
pub fn parse_ffmpeg_stderr(lines: &[String], fps: f64, frame_count: u64) -> PictureFindings {
    let mut findings = PictureFindings::default();
    let mut frozen_start: Option<f64> = None;
    for line in lines {
        if let (Some(start), Some(end)) = (
            tagged_seconds(line, BLACK_START),
            tagged_seconds(line, BLACK_END),
        ) {
            findings
                .black
                .push(FrameInterval::from_seconds(start, end, fps, frame_count));
        }
        if let Some(start) = tagged_seconds(line, FREEZE_START) {
            frozen_start = Some(start);
        }
        if let Some(end) = tagged_seconds(line, FREEZE_END)
            && let Some(start) = frozen_start.take()
        {
            findings
                .frozen
                .push(FrameInterval::from_seconds(start, end, fps, frame_count));
        }
    }
    if let Some(start) = frozen_start
        && frame_count > 0
    {
        let end = frame_count as f64 / fps;
        findings
            .frozen
            .push(FrameInterval::from_seconds(start, end, fps, frame_count));
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn a_black_clip_reports_black_and_frozen_over_finished_essence() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("black.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("color=c=black:size=320x240:rate=24:duration=3")
            .args(["-pix_fmt", "yuv420p", "-y"])
            .arg(&clip)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "fixture encode failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );

        let findings = detect_in_essence(&clip, 24.0, 72).unwrap();
        assert_eq!(findings.black.len(), 1, "{findings:?}");
        assert_eq!(findings.black[0].first_frame, 0, "{findings:?}");
        assert_eq!(findings.frozen.len(), 1, "{findings:?}");
        assert!(
            findings.black[0].last_frame >= 47,
            "a three second run at 24 fps covers most of 72 frames: {findings:?}"
        );
    }

    fn one_frame_png(directory: &Path, colour: &str) -> std::path::PathBuf {
        let image = directory.join(format!("{colour}.png"));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("color=c={colour}:size=320x240"))
            .args(["-frames:v", "1", "-pix_fmt", "rgb24", "-y"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "fixture encode failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        image
    }

    #[test]
    fn a_black_image_held_past_the_minimum_reports_black_over_the_whole_hold() {
        let dir = tempfile::tempdir().unwrap();
        let black = one_frame_png(dir.path(), "black");
        assert_eq!(
            detect_black_in_held_image(&black, FrameRate::whole(24), 72).unwrap(),
            vec![FrameInterval {
                first_frame: 0,
                last_frame: 71,
            }]
        );
        assert_eq!(
            detect_black_in_held_image(&black, FrameRate::whole(24), 14_400).unwrap(),
            vec![FrameInterval {
                first_frame: 0,
                last_frame: 14_399,
            }],
            "a ten minute hold is measured past the minimum and reported whole"
        );
        assert!(
            detect_black_in_held_image(&black, FrameRate::whole(24), 24)
                .unwrap()
                .is_empty(),
            "one second is under blackdetect's minimum"
        );
        let white = one_frame_png(dir.path(), "white");
        assert!(
            detect_black_in_held_image(&white, FrameRate::whole(24), 72)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn essence_ffmpeg_cannot_read_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let not_video = dir.path().join("notes.txt");
        std::fs::write(&not_video, "this is not picture essence").unwrap();
        let error = detect_in_essence(&not_video, 24.0, 24).unwrap_err();
        assert!(!error.trim().is_empty(), "the reason has to be named");
    }

    #[test]
    fn a_black_run_becomes_inclusive_frame_numbers() {
        let findings = parse_ffmpeg_stderr(
            &lines(&["[Parsed_blackdetect_0 @ 0x55] black_start:0 black_end:3 black_duration:3"]),
            24.0,
            216,
        );
        assert_eq!(
            findings.black,
            vec![FrameInterval {
                first_frame: 0,
                last_frame: 71,
            }]
        );
        assert!(findings.frozen.is_empty());
    }

    #[test]
    fn a_frozen_run_pairs_its_start_with_its_end() {
        let findings = parse_ffmpeg_stderr(
            &lines(&[
                "[Parsed_freezedetect_1 @ 0x55] lavfi.freezedetect.freeze_start: 0",
                "[Parsed_freezedetect_1 @ 0x55] lavfi.freezedetect.freeze_duration: 3",
                "[Parsed_freezedetect_1 @ 0x55] lavfi.freezedetect.freeze_end: 3",
            ]),
            24.0,
            216,
        );
        assert_eq!(
            findings.frozen,
            vec![FrameInterval {
                first_frame: 0,
                last_frame: 71,
            }]
        );
    }

    #[test]
    fn a_frozen_run_reaching_the_last_frame_is_closed_by_the_frame_count() {
        let findings = parse_ffmpeg_stderr(
            &lines(&["[Parsed_freezedetect_1 @ 0x55] lavfi.freezedetect.freeze_start: 6"]),
            24.0,
            216,
        );
        assert_eq!(
            findings.frozen,
            vec![FrameInterval {
                first_frame: 144,
                last_frame: 215,
            }]
        );
    }

    #[test]
    fn a_black_run_ending_at_the_last_frame_stays_inside_the_count() {
        let findings = parse_ffmpeg_stderr(
            &lines(&[
                "[Parsed_blackdetect_0 @ 0x55] black_start:3 black_end:5.958333 \
                 black_duration:2.958333",
            ]),
            24.0,
            144,
        );
        assert_eq!(
            findings.black,
            vec![FrameInterval {
                first_frame: 72,
                last_frame: 142,
            }]
        );
    }

    #[test]
    fn a_run_shorter_than_a_frame_still_holds_one() {
        let findings = parse_ffmpeg_stderr(
            &lines(&[
                "[Parsed_blackdetect_0 @ 0x55] black_start:1 black_end:1.01 black_duration:0.01",
            ]),
            24.0,
            216,
        );
        assert_eq!(
            findings.black,
            vec![FrameInterval {
                first_frame: 24,
                last_frame: 24,
            }]
        );
    }

    #[test]
    fn each_interval_describes_itself_with_timecodes_and_frame_numbers() {
        let findings = PictureFindings {
            black: vec![FrameInterval {
                first_frame: 0,
                last_frame: 71,
            }],
            frozen: vec![FrameInterval {
                first_frame: 144,
                last_frame: 215,
            }],
        };
        assert_eq!(
            findings.describe(24.0),
            vec![
                "black picture from 00:00:00:00 to 00:00:02:23 (frames 0 to 71)".to_string(),
                "frozen picture from 00:00:06:00 to 00:00:08:23 (frames 144 to 215)".to_string(),
            ]
        );
    }

    #[test]
    fn a_fractional_rate_timecodes_at_its_nearest_whole_rate() {
        let findings = PictureFindings {
            black: vec![FrameInterval {
                first_frame: 0,
                last_frame: 49,
            }],
            frozen: Vec::new(),
        };
        assert_eq!(
            findings.describe(24000.0 / 1001.0),
            vec!["black picture from 00:00:00:00 to 00:00:02:01 (frames 0 to 49)".to_string()]
        );
    }

    #[test]
    fn findings_with_nothing_in_them_describe_nothing() {
        assert!(PictureFindings::default().describe(24.0).is_empty());
    }

    #[test]
    fn lines_that_are_not_detections_are_ignored() {
        assert!(!is_detection_line(
            "[Parsed_freezedetect_1 @ 0x55] lavfi.freezedetect.freeze_duration: 3"
        ));
        assert!(!is_detection_line(
            "  Duration: 00:00:09.00, start: 0.000000"
        ));
        assert_eq!(
            parse_ffmpeg_stderr(&lines(&["frame=  216 fps=0.0 q=-0.0 Lsize=N/A"]), 24.0, 216),
            PictureFindings::default()
        );
    }

    #[test]
    fn a_fractional_rate_rounds_to_the_nearest_frame() {
        let findings = parse_ffmpeg_stderr(
            &lines(&[
                "[Parsed_blackdetect_0 @ 0x55] black_start:0 black_end:2.085417 \
                 black_duration:2.085417",
            ]),
            24000.0 / 1001.0,
            100,
        );
        assert_eq!(
            findings.black,
            vec![FrameInterval {
                first_frame: 0,
                last_frame: 49,
            }]
        );
    }
}
