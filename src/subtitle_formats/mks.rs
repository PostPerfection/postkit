//! MKS / Matroska subtitle extraction (dom#3131).
//!
//! No EBML parser: ffprobe lists the subtitle streams and ffmpeg extracts the
//! chosen one to SRT or ASS, which the existing parsers then read. Fails loud
//! when ffmpeg/ffprobe is missing or the container has no subtitle stream.

use std::path::{Path, PathBuf};
use std::process::Command;

use uuid::Uuid;

use super::{StyledCue, StyledRun, SubtitleError, ass};
use crate::subtitle_retime::parse_srt;

/// A subtitle stream in the container (subtitle-relative index + codec name).
#[derive(Debug, Clone)]
pub struct SubStream {
    pub index: usize,
    pub codec: String,
}

/// List subtitle streams in a Matroska/MKS container via ffprobe.
pub fn list_subtitle_streams(path: &Path) -> Result<Vec<SubStream>, SubtitleError> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "s",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .map_err(|e| SubtitleError::Ffmpeg(format!("ffprobe not runnable: {e}")))?;
    if !out.status.success() {
        return Err(SubtitleError::Ffmpeg(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    let streams: Vec<SubStream> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| SubStream {
            index: i,
            codec: l.trim().to_string(),
        })
        .collect();
    if streams.is_empty() {
        return Err(SubtitleError::NoSubtitleStream(path.to_path_buf()));
    }
    Ok(streams)
}

/// Extract and parse a subtitle stream from a Matroska/MKS container.
///
/// `stream_index` selects the subtitle-relative stream (0-based); `None` picks
/// the first. ASS/SSA streams keep styling; text streams come back as plain runs.
pub fn parse_mks(
    path: &Path,
    stream_index: Option<usize>,
) -> Result<Vec<StyledCue>, SubtitleError> {
    let streams = list_subtitle_streams(path)?;
    let idx = stream_index.unwrap_or(0);
    let stream = streams
        .iter()
        .find(|s| s.index == idx)
        .ok_or_else(|| SubtitleError::NoSubtitleStream(path.to_path_buf()))?;

    let is_ass = matches!(stream.codec.as_str(), "ass" | "ssa");
    let ext = if is_ass { "ass" } else { "srt" };
    let tmp: PathBuf = std::env::temp_dir().join(format!("postkit-mks-{}.{ext}", Uuid::new_v4()));

    let codec_arg = if is_ass { "copy" } else { "srt" };
    let out = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(path)
        .arg("-map")
        .arg(format!("0:s:{idx}"))
        .arg("-c:s")
        .arg(codec_arg)
        .arg(&tmp)
        .output()
        .map_err(|e| SubtitleError::Ffmpeg(format!("ffmpeg not runnable: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(SubtitleError::Ffmpeg(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }

    let content = std::fs::read_to_string(&tmp)?;
    let _ = std::fs::remove_file(&tmp);

    if is_ass {
        Ok(ass::parse_ass(&content)?.cues)
    } else {
        Ok(parse_srt(&content)
            .into_iter()
            .map(|c| StyledCue::text(c.start_ms, c.end_ms, vec![StyledRun::plain(c.text)]))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn have(bin: &str) -> bool {
        Command::new(bin)
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn mks_extracts_and_parses_srt_stream() {
        if !have("ffmpeg") || !have("ffprobe") {
            eprintln!("skipping mks test: ffmpeg/ffprobe not found");
            return;
        }
        let dir = TempDir::new().unwrap();
        let srt = dir.path().join("in.srt");
        std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:03,000\nHello mks\n").unwrap();
        let mkv = dir.path().join("out.mkv");
        let mux = Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(&srt)
            .args(["-c:s", "srt"])
            .arg(&mkv)
            .output()
            .unwrap();
        assert!(
            mux.status.success(),
            "mux failed: {}",
            String::from_utf8_lossy(&mux.stderr)
        );

        let cues = parse_mks(&mkv, None).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].plain_text(), "Hello mks");
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].end_ms, 3000);
    }

    #[test]
    fn mks_fails_loud_without_subtitle_stream() {
        if !have("ffmpeg") || !have("ffprobe") {
            eprintln!("skipping mks test: ffmpeg/ffprobe not found");
            return;
        }
        let dir = TempDir::new().unwrap();
        let mkv = dir.path().join("audio.mkv");
        let make = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo",
                "-t",
                "0.2",
            ])
            .arg(&mkv)
            .output()
            .unwrap();
        assert!(
            make.status.success(),
            "gen failed: {}",
            String::from_utf8_lossy(&make.stderr)
        );
        assert!(matches!(
            parse_mks(&mkv, None),
            Err(SubtitleError::NoSubtitleStream(_))
        ));
    }
}
