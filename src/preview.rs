use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Media preview options.
///
/// Playback is an ffplay wrapper over a decodable file, not a DCP/IMF-native
/// player: it cannot select a CPL, decode encrypted MXF, drive a GPU decoder or
/// apply a DCI display transform. `play` reads the file's real frame rate and
/// honours `start_frame`/`end_frame`/`loop_playback`, and errors when an option
/// it cannot fulfil (`cpl_uuid`, `gpu_device`, a non-sRGB `display_colourspace`)
/// is set, rather than ignoring it silently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackOptions {
    pub input: PathBuf,
    pub cpl_uuid: String,
    pub start_frame: u32,
    /// 0 = play to end
    pub end_frame: u32,
    pub loop_playback: bool,
    pub decode_to_display: bool,
    pub display_colourspace: String,
    pub gpu_device: i32,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            cpl_uuid: String::new(),
            start_frame: 0,
            end_frame: 0,
            loop_playback: false,
            decode_to_display: true,
            display_colourspace: "sRGB".to_string(),
            gpu_device: -1,
        }
    }
}

/// Frame metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameInfo {
    pub frame_number: u32,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
    pub codec: String,
}

/// Read the video frame rate via ffprobe, falling back to 24 fps when it can't
/// be determined.
pub fn read_frame_rate(input: &Path) -> f64 {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate",
            "-of",
            "default=nokey=1:noprint_wrappers=1",
        ])
        .arg(input)
        .output();

    let Ok(output) = output else {
        return 24.0;
    };
    let s = String::from_utf8_lossy(&output.stdout);
    let s = s.trim();
    let fps = if let Some((num, den)) = s.split_once('/') {
        let n: f64 = num.parse().unwrap_or(0.0);
        let d: f64 = den.parse().unwrap_or(0.0);
        if d > 0.0 { n / d } else { 0.0 }
    } else {
        s.parse().unwrap_or(0.0)
    };
    if fps > 0.0 { fps } else { 24.0 }
}

/// Extract a single frame as image (thumbnail/QC) using ffmpeg.
pub fn extract_frame(input: &Path, frame: u32, output_image: &Path) -> i32 {
    // Seek by the file's real frame rate, not a hardcoded 24 fps.
    let seconds = frame as f64 / read_frame_rate(input);

    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-ss")
        .arg(format!("{seconds:.3}"))
        .arg("-frames:v")
        .arg("1")
        .arg(output_image)
        .output();

    match output {
        Ok(o) if o.status.success() => 0,
        Ok(o) => {
            tracing::error!(
                "Frame extraction failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            -1
        }
        Err(e) => {
            tracing::error!("Failed to run ffmpeg: {e}");
            -1
        }
    }
}

/// Get frame metadata without full decode using ffprobe.
pub fn get_frame_info(input: &Path, frame: u32) -> FrameInfo {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-print_format", "json", "-show_streams"])
        .arg(input)
        .output();

    let Ok(output) = output else {
        return FrameInfo::default();
    };

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();

    let stream = json["streams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["codec_type"] == "video"));

    if let Some(s) = stream {
        FrameInfo {
            frame_number: frame,
            width: s["width"].as_u64().unwrap_or(0) as u32,
            height: s["height"].as_u64().unwrap_or(0) as u32,
            bitrate_kbps: s["bit_rate"]
                .as_str()
                .and_then(|b| b.parse::<u64>().ok())
                .map(|b| (b / 1000) as u32)
                .unwrap_or(0),
            codec: s["codec_name"].as_str().unwrap_or("").to_string(),
        }
    } else {
        FrameInfo {
            frame_number: frame,
            ..Default::default()
        }
    }
}

/// Start playback using ffplay (blocking).
///
/// Errors out (returns -1) on options the ffplay path cannot honour instead of
/// silently ignoring them. See `PlaybackOptions`.
pub fn play(opts: &PlaybackOptions) -> i32 {
    if !opts.cpl_uuid.is_empty() {
        tracing::error!("cpl_uuid is set but ffplay plays a file, not a CPL selection");
        return -1;
    }
    if opts.gpu_device >= 0 {
        tracing::error!("gpu_device is set but the ffplay path has no GPU decode");
        return -1;
    }
    if !opts.display_colourspace.eq_ignore_ascii_case("sRGB") {
        tracing::error!(
            "display_colourspace {:?} is unsupported; the ffplay path shows the file as-is",
            opts.display_colourspace
        );
        return -1;
    }

    let fps = read_frame_rate(&opts.input);
    let mut cmd = std::process::Command::new("ffplay");
    cmd.arg("-autoexit").arg(&opts.input);

    if opts.start_frame > 0 {
        let seconds = opts.start_frame as f64 / fps;
        cmd.arg("-ss").arg(format!("{seconds:.3}"));
    }

    // end_frame (0 = play to end) becomes a play duration from the start frame.
    if opts.end_frame > opts.start_frame {
        let seconds = (opts.end_frame - opts.start_frame) as f64 / fps;
        cmd.arg("-t").arg(format!("{seconds:.3}"));
    }

    if opts.loop_playback {
        cmd.arg("-loop").arg("0");
    }

    match cmd.status() {
        Ok(s) if s.success() => 0,
        Ok(_) => -1,
        Err(e) => {
            tracing::error!("Failed to run ffplay: {e}");
            -1
        }
    }
}

/// Render all frames to image sequence using ffmpeg.
pub fn render_to_sequence(input: &Path, output_dir: &Path, format: Option<&str>) -> i32 {
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        tracing::error!("Failed to create output directory: {e}");
        return -1;
    }

    let ext = format.unwrap_or("png");
    let output_pattern = output_dir.join(format!("frame_%06d.{ext}"));

    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg(&output_pattern)
        .output();

    match output {
        Ok(o) if o.status.success() => 0,
        Ok(o) => {
            tracing::error!("Render failed: {}", String::from_utf8_lossy(&o.stderr));
            -1
        }
        Err(e) => {
            tracing::error!("Failed to run ffmpeg: {e}");
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_rejects_options_ffplay_cannot_honour() {
        // each of these returns before ever spawning ffplay
        let with_cpl = PlaybackOptions {
            cpl_uuid: "urn:uuid:x".into(),
            ..Default::default()
        };
        assert_eq!(play(&with_cpl), -1);

        let with_gpu = PlaybackOptions {
            gpu_device: 0,
            ..Default::default()
        };
        assert_eq!(play(&with_gpu), -1);

        let with_cs = PlaybackOptions {
            display_colourspace: "P3".into(),
            ..Default::default()
        };
        assert_eq!(play(&with_cs), -1);
    }
}
