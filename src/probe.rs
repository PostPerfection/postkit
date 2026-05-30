use std::path::Path;

/// Video stream metadata from ffprobe.
#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub has_audio: bool,
    pub total_frames: u32,
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

    // Get total frame count
    let nb_output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok();
    let total_frames = nb_output
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);

    Some(VideoInfo {
        width,
        height,
        fps_num,
        fps_den,
        has_audio,
        total_frames,
    })
}

fn parse_frame_rate(s: &str) -> Option<(u32, u32)> {
    if let Some((num, den)) = s.split_once('/') {
        Some((num.parse().ok()?, den.parse().ok()?))
    } else {
        // Might be a plain number like "24"
        let fps: f64 = s.parse().ok()?;
        // Convert common rates
        let (num, den) = match fps as u32 {
            24 => (24, 1),
            25 => (25, 1),
            30 => (30, 1),
            48 => (48, 1),
            _ => ((fps * 1000.0) as u32, 1000),
        };
        Some((num, den))
    }
}
