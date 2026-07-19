use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Rating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RatingSystem {
    #[default]
    Mpaa,
    Bbfc,
    Fsk,
    Custom,
}

/// Trailer band colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TrailerBand {
    #[default]
    Green,
    Red,
    Yellow,
}

/// Trailer packaging options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrailerOptions {
    pub content_dir: PathBuf,
    pub audio_file: PathBuf,
    pub output_dir: PathBuf,
    pub title: String,
    pub rating: String,
    pub rating_system: RatingSystem,
    pub band: TrailerBand,
    pub countdown_seconds: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

/// Result of trailer packaging.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrailerResult {
    pub success: bool,
    pub error: String,
    pub output_dir: PathBuf,
    pub cpl_uuid: String,
}

/// Package a trailer (ratings card + leader + content).
///
/// Generates a countdown leader, ratings card TIFF, and assembles them with
/// the trailer content using ffmpeg into a complete trailer package.
pub fn package_trailer(opts: &TrailerOptions) -> TrailerResult {
    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return TrailerResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    let leader_dir = opts.output_dir.join("leader");
    if let Err(e) = std::fs::create_dir_all(&leader_dir) {
        return TrailerResult {
            success: false,
            error: format!("Failed to create leader directory: {e}"),
            ..Default::default()
        };
    }

    let fps = if opts.fps_num > 0 && opts.fps_den > 0 {
        opts.fps_num as f64 / opts.fps_den as f64
    } else {
        24.0
    };

    let countdown = if opts.countdown_seconds > 0 {
        opts.countdown_seconds
    } else {
        8
    };

    // Generate countdown leader using ffmpeg
    let band_color = match opts.band {
        TrailerBand::Green => "0x00FF00",
        TrailerBand::Red => "0xFF0000",
        TrailerBand::Yellow => "0xFFFF00",
    };

    let rating_text = if opts.rating.is_empty() {
        match opts.rating_system {
            RatingSystem::Mpaa => "G",
            RatingSystem::Bbfc => "U",
            RatingSystem::Fsk => "FSK 0",
            RatingSystem::Custom => "",
        }
    } else {
        &opts.rating
    };

    // Generate ratings card
    let ratings_card = opts.output_dir.join("ratings_card.png");
    let drawtext = format!(
        "drawtext=text='{}':fontsize=72:fontcolor=white:x=(w-text_w)/2:y=(h-text_h)/2,drawtext=text='{}':fontsize=36:fontcolor=white:x=(w-text_w)/2:y=(h+text_h)/2+20",
        opts.title.replace('\'', "\\'"),
        rating_text.replace('\'', "\\'"),
    );

    let mut rc_cmd = std::process::Command::new("ffmpeg");
    rc_cmd
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!("color=c={band_color}:s=1920x1080:d=1"))
        .arg("-vf")
        .arg(&drawtext)
        .arg("-frames:v")
        .arg("1")
        .arg(&ratings_card);

    if let Err(error) = run_ffmpeg("ratings card", &mut rc_cmd) {
        return TrailerResult {
            success: false,
            error,
            ..Default::default()
        };
    }

    // Generate countdown leader video
    let leader_file = opts.output_dir.join("leader.mp4");
    let countdown_filter = format!(
        "drawtext=text='%{{eif\\:({countdown}-t)\\:d}}':fontsize=200:fontcolor=white:x=(w-text_w)/2:y=(h-text_h)/2"
    );

    let mut leader_cmd = std::process::Command::new("ffmpeg");
    leader_cmd
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!("color=c=black:s=1920x1080:d={countdown}:r={fps}"))
        .arg("-vf")
        .arg(&countdown_filter)
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg(&leader_file);

    if let Err(error) = run_ffmpeg("countdown leader", &mut leader_cmd) {
        return TrailerResult {
            success: false,
            error,
            ..Default::default()
        };
    }

    // Create concat file list
    let concat_file = opts.output_dir.join("concat.txt");
    let mut concat_content = String::new();
    if leader_file.exists() {
        concat_content.push_str(&format!("file '{}'\n", leader_file.display()));
    }
    // Main content
    if opts.content_dir.is_file() {
        concat_content.push_str(&format!("file '{}'\n", opts.content_dir.display()));
    }

    if concat_content.is_empty() {
        return TrailerResult {
            success: false,
            error: format!(
                "Nothing to package: no leader was produced and content_dir {} is not a file",
                opts.content_dir.display()
            ),
            ..Default::default()
        };
    }

    if let Err(e) = std::fs::write(&concat_file, &concat_content) {
        return TrailerResult {
            success: false,
            error: format!("Failed to write concat file: {e}"),
            ..Default::default()
        };
    }

    let output_file = opts.output_dir.join("trailer_packaged.mp4");
    let mut concat_cmd = std::process::Command::new("ffmpeg");
    concat_cmd
        .arg("-y")
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(&concat_file)
        .arg("-c")
        .arg("copy")
        .arg(&output_file);

    if let Err(error) = run_ffmpeg("trailer concat", &mut concat_cmd) {
        return TrailerResult {
            success: false,
            error,
            ..Default::default()
        };
    }

    let cpl_uuid = uuid::Uuid::new_v4().to_string();

    TrailerResult {
        success: true,
        error: String::new(),
        output_dir: opts.output_dir.clone(),
        cpl_uuid,
    }
}

/// Run an ffmpeg step, turning a spawn failure or non-zero exit into an error.
fn run_ffmpeg(step: &str, cmd: &mut std::process::Command) -> Result<(), String> {
    match cmd.output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "{step} failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("{step}: failed to run ffmpeg: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ffmpeg_reports_missing_binary() {
        let mut cmd = std::process::Command::new("postkit-no-such-binary-exists");
        let err = run_ffmpeg("leader", &mut cmd).unwrap_err();
        assert!(err.contains("leader"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn run_ffmpeg_reports_non_zero_exit_with_stderr() {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("echo boom >&2; exit 1");
        let err = run_ffmpeg("concat", &mut cmd).unwrap_err();
        assert!(err.contains("concat"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn run_ffmpeg_accepts_clean_exit() {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("exit 0");
        assert!(run_ffmpeg("card", &mut cmd).is_ok());
    }

    #[test]
    fn package_trailer_does_not_report_success_on_ffmpeg_failure() {
        // the packaged output path is already a directory, so ffmpeg's final
        // concat cannot write it; this used to return success: true regardless
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(out.join("trailer_packaged.mp4")).unwrap();
        let content = dir.path().join("content.mp4");
        std::fs::write(&content, b"not a video file").unwrap();

        let result = package_trailer(&TrailerOptions {
            content_dir: content,
            output_dir: out,
            fps_num: 24,
            fps_den: 1,
            ..Default::default()
        });

        assert!(!result.success, "expected failure, got {result:?}");
        assert!(!result.error.is_empty());
    }
}
