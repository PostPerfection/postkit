use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// Concatenated trailer file (ratings card + leader + content).
    pub output_file: PathBuf,
}

const RATINGS_CARD_SECONDS: u32 = 5;
const DEFAULT_COUNTDOWN_SECONDS: u32 = 8;
const FALLBACK_FRAME_RATE: (u32, u32) = (24, 1);
const TITLE_HEIGHT_FRACTION: u32 = 15;
const RATING_HEIGHT_FRACTION: u32 = 30;
const RATING_GAP_HEIGHT_FRACTION: u32 = 54;
const COUNTDOWN_HEIGHT_FRACTION: u32 = 5;
const INTERMEDIATE_CRF: &str = "10";
const INTERMEDIATE_PIXEL_FORMAT: &str = "yuv444p10le";

pub fn package_trailer(opts: &TrailerOptions) -> TrailerResult {
    match build_trailer(opts) {
        Ok(output_file) => TrailerResult {
            success: true,
            error: String::new(),
            output_dir: opts.output_dir.clone(),
            output_file,
        },
        Err(error) => TrailerResult {
            success: false,
            error,
            ..Default::default()
        },
    }
}

fn build_trailer(opts: &TrailerOptions) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&opts.output_dir)
        .map_err(|e| format!("Failed to create output directory: {e}"))?;

    if !opts.content_dir.is_file() {
        return Err(format!(
            "content probe: {} is not a file",
            opts.content_dir.display()
        ));
    }

    let content = probe_video("content probe", &opts.content_dir)?;
    let (fps_num, fps_den) = resolve_frame_rate(opts, &content);

    let ratings_card = render_ratings_card(opts, &content, fps_num, fps_den)?;
    let leader = render_countdown_leader(opts, &content, fps_num, fps_den)?;

    let output_file = opts.output_dir.join("trailer_packaged.mp4");
    join_segments(
        &[&ratings_card, &leader, &opts.content_dir],
        &content,
        &output_file,
    )?;

    let card_frames = probe_video("ratings card probe", &ratings_card)?.frame_count;
    let leader_frames = probe_video("countdown leader probe", &leader)?.frame_count;
    let joined = probe_video("trailer join probe", &output_file)?;
    let expected = card_frames + leader_frames + content.frame_count;
    if joined.frame_count != expected {
        return Err(format!(
            "trailer join dropped frames: card {card_frames} + leader {leader_frames} + content {} = {expected}, joined file has {}",
            content.frame_count, joined.frame_count
        ));
    }

    Ok(output_file)
}

fn render_ratings_card(
    opts: &TrailerOptions,
    content: &VideoFormat,
    fps_num: u32,
    fps_den: u32,
) -> Result<PathBuf, String> {
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

    let title_size = content.height / TITLE_HEIGHT_FRACTION;
    let rating_size = content.height / RATING_HEIGHT_FRACTION;
    let rating_gap = content.height / RATING_GAP_HEIGHT_FRACTION;
    let drawtext = format!(
        "drawtext=text='{}':fontsize={title_size}:fontcolor=white:x=(w-text_w)/2:y=(h-text_h)/2,drawtext=text='{}':fontsize={rating_size}:fontcolor=white:x=(w-text_w)/2:y=(h+text_h)/2+{rating_gap}",
        opts.title.replace('\'', "\\'"),
        rating_text.replace('\'', "\\'"),
    );

    let ratings_card = opts.output_dir.join("ratings_card.mp4");
    let source = format!(
        "color=c={band_color}:s={}x{}:d={RATINGS_CARD_SECONDS}:r={fps_num}/{fps_den}",
        content.width, content.height
    );
    render_generated_clip("ratings card", &source, &drawtext, &ratings_card)?;
    Ok(ratings_card)
}

fn render_countdown_leader(
    opts: &TrailerOptions,
    content: &VideoFormat,
    fps_num: u32,
    fps_den: u32,
) -> Result<PathBuf, String> {
    let countdown = if opts.countdown_seconds > 0 {
        opts.countdown_seconds
    } else {
        DEFAULT_COUNTDOWN_SECONDS
    };
    let countdown_size = content.height / COUNTDOWN_HEIGHT_FRACTION;
    let countdown_filter = format!(
        "drawtext=text='%{{eif\\:({countdown}-t)\\:d}}':fontsize={countdown_size}:fontcolor=white:x=(w-text_w)/2:y=(h-text_h)/2"
    );

    let leader_file = opts.output_dir.join("leader.mp4");
    let source = format!(
        "color=c=black:s={}x{}:d={countdown}:r={fps_num}/{fps_den}",
        content.width, content.height
    );
    render_generated_clip("countdown leader", &source, &countdown_filter, &leader_file)?;
    Ok(leader_file)
}

fn render_generated_clip(
    step: &str,
    source: &str,
    filter: &str,
    output: &Path,
) -> Result<(), String> {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(source)
        .arg("-vf")
        .arg(filter)
        .arg("-c:v")
        .arg("libx264")
        .arg("-crf")
        .arg(INTERMEDIATE_CRF)
        .arg("-pix_fmt")
        .arg(INTERMEDIATE_PIXEL_FORMAT)
        .arg(output);
    run_ffmpeg(step, &mut cmd)
}

// a stream copy silently drops any segment the mp4 muxer will not take
fn join_segments(segments: &[&Path], content: &VideoFormat, output: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-y");
    for segment in segments {
        cmd.arg("-i").arg(segment);
    }

    let mut filter = String::new();
    for index in 0..segments.len() {
        filter.push_str(&format!(
            "[{index}:v]scale={}:{},setsar=1,format={INTERMEDIATE_PIXEL_FORMAT}[v{index}];",
            content.width, content.height
        ));
    }
    for index in 0..segments.len() {
        filter.push_str(&format!("[v{index}]"));
    }
    filter.push_str(&format!("concat=n={}:v=1:a=0[out]", segments.len()));

    cmd.arg("-filter_complex")
        .arg(&filter)
        .arg("-map")
        .arg("[out]")
        .arg("-c:v")
        .arg("libx264")
        .arg("-crf")
        .arg(INTERMEDIATE_CRF)
        .arg("-pix_fmt")
        .arg(INTERMEDIATE_PIXEL_FORMAT)
        // keeps the joined frame count the sum of the segments
        .arg("-fps_mode")
        .arg("passthrough")
        .arg(output);
    run_ffmpeg("trailer join", &mut cmd)
}

fn resolve_frame_rate(opts: &TrailerOptions, content: &VideoFormat) -> (u32, u32) {
    let requested = if opts.fps_num > 0 && opts.fps_den > 0 {
        Some((opts.fps_num, opts.fps_den))
    } else {
        None
    };

    let Some(probed) = content.frame_rate else {
        return requested.unwrap_or(FALLBACK_FRAME_RATE);
    };

    if let Some(requested) = requested
        && u64::from(probed.0) * u64::from(requested.1)
            != u64::from(probed.1) * u64::from(requested.0)
    {
        tracing::warn!(
            "trailer content runs at {}/{}, ignoring the requested {}/{}",
            probed.0,
            probed.1,
            requested.0,
            requested.1
        );
    }
    probed
}

struct VideoFormat {
    width: u32,
    height: u32,
    frame_rate: Option<(u32, u32)>,
    frame_count: u64,
}

fn probe_video(step: &str, path: &Path) -> Result<VideoFormat, String> {
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-count_frames")
        .arg("-show_entries")
        .arg("stream=width,height,r_frame_rate,nb_read_frames")
        .arg("-print_format")
        .arg("json")
        .arg(path);

    let output = cmd
        .output()
        .map_err(|e| format!("{step}: failed to run ffprobe: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{step} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("{step}: ffprobe output is not json: {e}"))?;
    let stream = parsed["streams"]
        .get(0)
        .ok_or_else(|| format!("{step}: no video stream in {}", path.display()))?;

    let width = stream["width"]
        .as_u64()
        .ok_or_else(|| format!("{step}: no width in {}", path.display()))? as u32;
    let height = stream["height"]
        .as_u64()
        .ok_or_else(|| format!("{step}: no height in {}", path.display()))? as u32;
    let frame_count: u64 = stream["nb_read_frames"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{step}: no frame count in {}", path.display()))?;
    if frame_count == 0 {
        return Err(format!("{step}: {} holds no frames", path.display()));
    }

    Ok(VideoFormat {
        width,
        height,
        frame_rate: stream["r_frame_rate"].as_str().and_then(parse_frame_rate),
        frame_count,
    })
}

fn parse_frame_rate(rate: &str) -> Option<(u32, u32)> {
    let (num, den) = rate.split_once('/')?;
    let num: u32 = num.parse().ok()?;
    let den: u32 = den.parse().ok()?;
    if num == 0 || den == 0 {
        return None;
    }
    Some((num, den))
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
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
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
        assert!(result.error.contains("content probe"), "{}", result.error);
    }
}
