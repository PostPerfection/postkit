//! ProRes detection and DCP packaging support.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProResError {
    #[error("Input file not found: {0}")]
    NotFound(PathBuf),
    #[error("ffmpeg not found — required for ProRes extraction")]
    FfmpegNotFound,
    #[error("ffprobe not found")]
    FfprobeNotFound,
    #[error("Failed to wrap ProRes essence to MXF")]
    WrapFailed,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// ProRes codec profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ProResProfile {
    Proxy,
    Lt,
    Standard,
    Hq,
    Xq,
}

impl ProResProfile {
    /// Four-character code for this profile.
    pub fn fourcc(self) -> &'static str {
        match self {
            Self::Proxy => "apco",
            Self::Lt => "apcs",
            Self::Standard => "apcn",
            Self::Hq => "apch",
            Self::Xq => "ap4h",
        }
    }
}

/// Options for creating a ProRes-sourced DCP.
#[derive(Debug, Clone)]
pub struct ProResDcpOptions {
    pub input_file: PathBuf,
    pub output_dir: PathBuf,
    pub title: String,
    pub issuer: String,
    pub profile: ProResProfile,
    pub fps_num: u32,
    pub fps_den: u32,
    pub sample_rate: u32,
    pub audio_bit_depth: u16,
}

impl Default for ProResDcpOptions {
    fn default() -> Self {
        Self {
            input_file: PathBuf::new(),
            output_dir: PathBuf::new(),
            title: String::new(),
            issuer: "DCP Wizard".to_string(),
            profile: ProResProfile::Hq,
            fps_num: 24,
            fps_den: 1,
            sample_rate: 48000,
            audio_bit_depth: 24,
        }
    }
}

/// Check if ffprobe is available.
pub fn has_ffprobe() -> bool {
    Command::new("which")
        .arg("ffprobe")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if ffmpeg is available.
pub fn has_ffmpeg() -> bool {
    Command::new("which")
        .arg("ffmpeg")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if a file contains ProRes video.
pub fn is_prores_file(file: &Path) -> Result<bool, ProResError> {
    if !has_ffprobe() {
        return Err(ProResError::FfprobeNotFound);
    }

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()?;

    let codec = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(codec == "prores")
}

/// Detect the ProRes profile of a file.
pub fn detect_prores_profile(file: &Path) -> Result<ProResProfile, ProResError> {
    if !has_ffprobe() {
        return Err(ProResError::FfprobeNotFound);
    }

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=profile",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()?;

    let profile_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(parse_profile(&profile_str))
}

fn parse_profile(s: &str) -> ProResProfile {
    if s.contains("Proxy") {
        ProResProfile::Proxy
    } else if s.contains("LT") {
        ProResProfile::Lt
    } else if s.contains("Standard") {
        ProResProfile::Standard
    } else if s.contains("XQ") || s.contains("4444 XQ") {
        ProResProfile::Xq
    } else {
        ProResProfile::Hq
    }
}

/// Build ffmpeg arguments for extracting ProRes video to MXF.
pub fn build_video_extract_args(input: &Path, output: &Path) -> Vec<String> {
    vec![
        "-y".to_string(),
        "-i".to_string(),
        input.display().to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-f".to_string(),
        "mxf".to_string(),
        output.display().to_string(),
    ]
}

/// Build ffmpeg arguments for extracting audio to PCM MXF.
pub fn build_audio_extract_args(
    input: &Path,
    output: &Path,
    bit_depth: u16,
    sample_rate: u32,
) -> Vec<String> {
    vec![
        "-y".to_string(),
        "-i".to_string(),
        input.display().to_string(),
        "-vn".to_string(),
        "-c:a".to_string(),
        format!("pcm_s{}le", bit_depth),
        "-ar".to_string(),
        sample_rate.to_string(),
        "-f".to_string(),
        "mxf".to_string(),
        output.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_fourcc() {
        assert_eq!(ProResProfile::Proxy.fourcc(), "apco");
        assert_eq!(ProResProfile::Lt.fourcc(), "apcs");
        assert_eq!(ProResProfile::Standard.fourcc(), "apcn");
        assert_eq!(ProResProfile::Hq.fourcc(), "apch");
        assert_eq!(ProResProfile::Xq.fourcc(), "ap4h");
    }

    #[test]
    fn test_parse_profile() {
        assert_eq!(parse_profile("Proxy"), ProResProfile::Proxy);
        assert_eq!(parse_profile("Apple ProRes LT"), ProResProfile::Lt);
        assert_eq!(parse_profile("Standard"), ProResProfile::Standard);
        assert_eq!(parse_profile("Apple ProRes 422 HQ"), ProResProfile::Hq);
        assert_eq!(parse_profile("Apple ProRes 4444 XQ"), ProResProfile::Xq);
        assert_eq!(parse_profile("unknown"), ProResProfile::Hq);
    }

    #[test]
    fn test_default_options() {
        let opts = ProResDcpOptions::default();
        assert_eq!(opts.profile, ProResProfile::Hq);
        assert_eq!(opts.fps_num, 24);
        assert_eq!(opts.fps_den, 1);
        assert_eq!(opts.sample_rate, 48000);
        assert_eq!(opts.audio_bit_depth, 24);
        assert_eq!(opts.issuer, "DCP Wizard");
    }

    #[test]
    fn test_build_video_extract_args() {
        let args = build_video_extract_args(
            Path::new("/input/movie.mov"),
            Path::new("/output/video.mxf"),
        );
        assert_eq!(args[0], "-y");
        assert_eq!(args[1], "-i");
        assert_eq!(args[2], "/input/movie.mov");
        assert_eq!(args[3], "-c:v");
        assert_eq!(args[4], "copy");
        assert_eq!(args[5], "-f");
        assert_eq!(args[6], "mxf");
        assert_eq!(args[7], "/output/video.mxf");
    }

    #[test]
    fn test_build_audio_extract_args() {
        let args = build_audio_extract_args(
            Path::new("/input/movie.mov"),
            Path::new("/output/audio.mxf"),
            24,
            48000,
        );
        assert!(args.contains(&"-vn".to_string()));
        assert!(args.contains(&"pcm_s24le".to_string()));
        assert!(args.contains(&"48000".to_string()));
        assert!(args.contains(&"mxf".to_string()));
    }

    #[test]
    fn test_build_audio_extract_args_16bit() {
        let args = build_audio_extract_args(Path::new("/in.mov"), Path::new("/out.mxf"), 16, 44100);
        assert!(args.contains(&"pcm_s16le".to_string()));
        assert!(args.contains(&"44100".to_string()));
    }
}
