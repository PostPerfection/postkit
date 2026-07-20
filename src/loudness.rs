use serde::{Deserialize, Serialize};
use std::path::Path;

/// EBU R128 loudness measurement result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoudnessResult {
    /// Integrated loudness in LUFS.
    pub integrated_lufs: f64,
    /// Loudness range in LU.
    pub range_lu: f64,
    /// True peak in dBTP.
    pub true_peak_dbtp: f64,
    /// Short-term loudness max in LUFS.
    pub short_term_max_lufs: f64,
    /// Whether measurement succeeded.
    pub success: bool,
    pub error: String,
}

/// Measure audio loudness per EBU R128 using ffmpeg.
pub fn measure_loudness(input: &Path) -> LoudnessResult {
    let output = match std::process::Command::new("ffmpeg")
        .args([
            "-i",
            &input.to_string_lossy(),
            "-af",
            "loudnorm=print_format=json",
            "-f",
            "null",
            "-",
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return LoudnessResult {
                success: false,
                error: format!("Failed to run ffmpeg: {e}"),
                ..Default::default()
            };
        }
    };

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Parse loudnorm JSON output from ffmpeg stderr
    if let Some(json_start) = stderr.rfind('{')
        && let Some(json_end) = stderr[json_start..].find('}')
    {
        let json_str = &stderr[json_start..json_start + json_end + 1];
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
            return LoudnessResult {
                integrated_lufs: val["input_i"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0),
                range_lu: val["input_lra"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0),
                true_peak_dbtp: val["input_tp"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0),
                // loudnorm's JSON has no short-term field; measure it separately.
                short_term_max_lufs: measure_short_term_max(input).unwrap_or(0.0),
                success: true,
                error: String::new(),
            };
        }
    }

    LoudnessResult {
        success: false,
        error: "Failed to parse loudnorm output from ffmpeg".to_string(),
        ..Default::default()
    }
}

/// Max short-term (3s window) loudness via ffmpeg's ebur128 filter.
fn measure_short_term_max(input: &Path) -> Option<f64> {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-i",
            &input.to_string_lossy(),
            "-af",
            "ebur128",
            "-f",
            "null",
            "-",
        ])
        .output()
        .ok()?;
    parse_short_term_max(&String::from_utf8_lossy(&output.stderr))
}

/// Largest `S:` (short-term LUFS) value in ebur128 stderr output.
///
/// ebur128 prints per-window lines like
/// `[Parsed_ebur128_0 @ ..] t: 3 M: -22.0 S: -19.8 I: -23.0 LUFS ...`; the
/// short-term max is the largest finite `S:` across them.
fn parse_short_term_max(stderr: &str) -> Option<f64> {
    let mut max: Option<f64> = None;
    for line in stderr.lines() {
        if let Some(pos) = line.find("S:") {
            let rest = line[pos + 2..].trim_start();
            let token = rest.split_whitespace().next().unwrap_or("");
            if let Ok(v) = token.parse::<f64>()
                && v.is_finite()
            {
                max = Some(max.map_or(v, |m: f64| m.max(v)));
            }
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_max_short_term_from_ebur128() {
        let stderr = "\
[Parsed_ebur128_0 @ 0x1] t: 1.0 M: -30.0 S: -25.0 I: -24.0 LUFS LRA: 2.0 LU
[Parsed_ebur128_0 @ 0x1] t: 2.0 M: -20.0 S: -18.5 I: -23.0 LUFS LRA: 3.0 LU
[Parsed_ebur128_0 @ 0x1] t: 3.0 M: -40.0 S: -inf I: -23.0 LUFS LRA: 3.0 LU
";
        let v = parse_short_term_max(stderr).unwrap();
        assert!((v - (-18.5)).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn short_term_none_without_measurements() {
        assert!(parse_short_term_max("no ebur128 lines here").is_none());
    }
}
