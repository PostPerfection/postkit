use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::timecode::parse_frame_rate;

/// Video stream metadata from ffprobe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub has_audio: bool,
    pub total_frames: u32,
    /// ffprobe's `pix_fmt`, e.g. "yuv422p10le".
    pub pix_fmt: String,
    /// ffprobe's `color_space`, e.g. "bt709".
    pub color_space: String,
    /// ffprobe's `color_range`, "pc" for full range and "tv" for studio range.
    pub color_range: String,
}

/// What ffprobe prints for a field the stream carries no tag for.
const UNTAGGED: &str = "unknown";

/// The pixel format and colour tags of a video stream, spelled the way ffprobe
/// prints them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixelFormatInfo {
    pub pix_fmt: String,
    pub color_space: String,
    pub color_range: String,
}

impl Default for PixelFormatInfo {
    fn default() -> Self {
        Self {
            pix_fmt: UNTAGGED.to_string(),
            color_space: UNTAGGED.to_string(),
            color_range: UNTAGGED.to_string(),
        }
    }
}

/// Read the first video stream's pixel format and colour tags. A file ffprobe
/// cannot read comes back with every field untagged.
pub fn probe_pixel_format(path: &Path) -> PixelFormatInfo {
    let mut info = PixelFormatInfo::default();
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=pix_fmt,color_space,color_range",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output();
    let Ok(output) = output else {
        return info;
    };
    if !output.status.success() {
        return info;
    }
    // ffprobe prints these in its own order rather than the order they were
    // asked for, so each line is read by its key
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let field = match key.trim() {
            "pix_fmt" => &mut info.pix_fmt,
            "color_space" => &mut info.color_space,
            "color_range" => &mut info.color_range,
            _ => continue,
        };
        *field = value.trim().to_string();
    }
    info
}

/// Probe a video file using ffprobe to extract resolution and frame rate.
pub fn probe_video(path: &Path) -> Option<VideoInfo> {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate,width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    // Format: "width,height,num/den"
    let parts: Vec<&str> = text.split(',').collect();
    if parts.len() < 3 {
        return None;
    }

    let width: u32 = parts[0].parse().ok()?;
    let height: u32 = parts[1].parse().ok()?;
    let (fps_num, fps_den) = parse_frame_rate(parts[2])?;

    // Check for audio stream
    let audio_output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok();

    let has_audio = audio_output
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);

    let total_frames = frame_count(path, fps_num, fps_den);

    let pixel_format = probe_pixel_format(path);

    Some(VideoInfo {
        width,
        height,
        fps_num,
        fps_den,
        has_audio,
        total_frames,
        pix_fmt: pixel_format.pix_fmt,
        color_space: pixel_format.color_space,
        color_range: pixel_format.color_range,
    })
}

/// How many frames the video stream holds. ffprobe's `-count_frames` decodes
/// the whole file and takes as long as an encode of it, and `-count_packets`
/// counts packets an edit list trims away (an MP4 cut with `-t` carries 1508
/// packets for 1442 frames), so the count is the stream's own duration at its
/// frame rate, with the packet count as the fallback for a stream that
/// reports no duration.
fn frame_count(path: &Path, fps_num: u32, fps_den: u32) -> u32 {
    let duration =
        ffprobe_video_field(path, "duration", &[]).and_then(|value| value.parse::<f64>().ok());
    if let Some(duration) = duration {
        return frames_in(duration, fps_num, fps_den);
    }
    ffprobe_video_field(path, "nb_read_packets", &["-count_packets"])
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Whole frames in `duration` seconds at `fps_num / fps_den`, rounded, since
/// a container duration carries the frame count with a timescale rounding.
fn frames_in(duration: f64, fps_num: u32, fps_den: u32) -> u32 {
    if fps_num == 0 || fps_den == 0 {
        return 0;
    }
    (duration * fps_num as f64 / fps_den as f64).round() as u32
}

/// One field of the first video stream, `None` when ffprobe prints nothing
/// usable for it (`N/A` included).
fn ffprobe_video_field(path: &Path, field: &str, extra_args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-select_streams", "v:0"])
        .args(extra_args)
        .args([
            "-show_entries",
            &format!("stream={field}"),
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "N/A" {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clip ffmpeg encodes at `pixel_format`, tagged with `colour_args`.
    fn clip(
        dir: &Path,
        name: &str,
        pixel_format: &str,
        colour_args: &[&str],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=64x48:rate=24:duration=1")
            .args([
                "-frames:v",
                "4",
                "-c:v",
                "libx264",
                "-pix_fmt",
                pixel_format,
            ])
            .args(colour_args)
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "fixture encode failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        path
    }

    #[test]
    fn an_untagged_source_reports_its_pixel_format_and_unknown_colour() {
        let dir = tempfile::tempdir().unwrap();
        let info = probe_pixel_format(&clip(dir.path(), "untagged.mp4", "yuv420p", &[]));
        assert_eq!(info.pix_fmt, "yuv420p");
        assert_eq!(info.color_space, "unknown");
        assert_eq!(info.color_range, "unknown");
    }

    #[test]
    fn a_tagged_source_reports_the_tags_ffprobe_prints() {
        let dir = tempfile::tempdir().unwrap();
        let tagged = clip(
            dir.path(),
            "tagged.mp4",
            "yuv422p10le",
            &["-colorspace", "bt709", "-color_range", "pc"],
        );
        let info = probe_pixel_format(&tagged);
        assert_eq!(info.pix_fmt, "yuv422p10le");
        assert_eq!(info.color_space, "bt709");
        assert_eq!(info.color_range, "pc");
    }

    #[test]
    fn a_file_ffprobe_cannot_read_is_untagged() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_video = dir.path().join("notes.txt");
        std::fs::write(&not_a_video, "no video here").unwrap();
        assert_eq!(probe_pixel_format(&not_a_video), PixelFormatInfo::default());
    }

    #[test]
    fn the_video_probe_carries_the_pixel_format_fields() {
        let dir = tempfile::tempdir().unwrap();
        let info = probe_video(&clip(dir.path(), "probe.mp4", "yuv420p", &[])).unwrap();
        assert_eq!((info.width, info.height), (64, 48));
        assert_eq!(info.pix_fmt, "yuv420p");
        assert_eq!(info.color_space, "unknown");
        assert_eq!(info.color_range, "unknown");
    }

    #[test]
    fn the_frame_count_is_the_duration_at_the_frame_rate() {
        assert_eq!(frames_in(60.083008, 24, 1), 1442);
        assert_eq!(frames_in(888.0, 24, 1), 21312);
        assert_eq!(frames_in(60.0, 24000, 1001), 1439);
        assert_eq!(frames_in(5.0, 0, 1), 0);
    }

    #[test]
    fn the_video_probe_counts_the_frames_a_short_clip_holds() {
        let dir = tempfile::tempdir().unwrap();
        let info = probe_video(&clip(dir.path(), "count.mp4", "yuv420p", &[])).unwrap();
        assert_eq!(info.total_frames, 4);
    }
}
