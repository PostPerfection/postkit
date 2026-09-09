//! Frame-by-frame video quality comparison (PSNR, SSIM, VMAF) via ffmpeg.
//!
//! `compare_frames` is the per-frame core; `compute_vmaf` and `compute_quality`
//! are the pooled/aggregate wrappers. ffmpeg stat output is parsed by
//! whitespace splitting, no regex.

use crate::filter_path::filter_option_path;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Per-frame quality metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameMetric {
    pub frame: u64,
    pub psnr_y: f64,
    pub psnr_u: f64,
    pub psnr_v: f64,
    pub psnr_avg: f64,
    pub ssim_y: f64,
    pub ssim_avg: f64,
}

/// Aggregate comparison result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompareResult {
    pub frames_compared: u64,
    pub avg_psnr: f64,
    pub min_psnr: f64,
    pub max_psnr: f64,
    pub avg_ssim: f64,
    pub min_ssim: f64,
    pub max_ssim: f64,
    pub per_frame: Vec<FrameMetric>,
}

/// Comparisons started so far, so two running at once do not write each other's
/// stats files. The pid alone does not cover two threads of one process.
static COMPARISONS_STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A stats file only this comparison writes and reads.
fn stats_log_path(metric: &str) -> PathBuf {
    let comparison = COMPARISONS_STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "imfwizard_{metric}_{}_{comparison}.log",
        std::process::id()
    ))
}

/// Compare two video files frame-by-frame using ffmpeg PSNR and SSIM filters.
pub fn compare_frames(reference: &Path, distorted: &Path) -> Result<CompareResult, String> {
    let psnr_log = stats_log_path("psnr");
    let ssim_log = stats_log_path("ssim");

    // Run ffmpeg with both PSNR and SSIM filters simultaneously
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(reference)
        .args(["-i"])
        .arg(distorted)
        .args([
            "-lavfi",
            // each input pad can only be consumed once, so split both before
            // feeding psnr and ssim; ssim's passthrough goes to a null sink and
            // psnr's is the graph output consumed by `-f null -`.
            &format!(
                "[0:v]split=2[r0][r1];[1:v]split=2[d0][d1];\
                 [r1][d1]ssim=stats_file={}[s];[s]nullsink;\
                 [r0][d0]psnr=stats_file={}",
                filter_option_path(&ssim_log),
                filter_option_path(&psnr_log)
            ),
            "-f",
            "null",
            "-",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        let last = stderr.lines().last().unwrap_or("no output");
        return Err(format!("ffmpeg comparison failed: {last}"));
    }

    // Parse PSNR log
    let psnr_data =
        std::fs::read_to_string(&psnr_log).map_err(|e| format!("Failed to read PSNR log: {e}"))?;
    let _ = std::fs::remove_file(&psnr_log);

    // Parse SSIM log
    let ssim_data =
        std::fs::read_to_string(&ssim_log).map_err(|e| format!("Failed to read SSIM log: {e}"))?;
    let _ = std::fs::remove_file(&ssim_log);

    let psnr_frames = parse_psnr_log(&psnr_data);
    let ssim_frames = parse_ssim_log(&ssim_data);

    let frame_count = psnr_frames.len().min(ssim_frames.len());
    if frame_count == 0 {
        return Err("No frames compared".to_string());
    }

    let mut result = CompareResult {
        frames_compared: frame_count as u64,
        min_psnr: f64::INFINITY,
        max_psnr: f64::NEG_INFINITY,
        min_ssim: f64::INFINITY,
        max_ssim: f64::NEG_INFINITY,
        ..Default::default()
    };

    let mut psnr_sum = 0.0;
    let mut ssim_sum = 0.0;

    for i in 0..frame_count {
        let (psnr_y, psnr_u, psnr_v, psnr_avg) = psnr_frames[i];
        let (ssim_y, ssim_avg) = ssim_frames[i];

        let metric = FrameMetric {
            frame: i as u64,
            psnr_y,
            psnr_u,
            psnr_v,
            psnr_avg,
            ssim_y,
            ssim_avg,
        };

        if psnr_avg < result.min_psnr {
            result.min_psnr = psnr_avg;
        }
        if psnr_avg > result.max_psnr {
            result.max_psnr = psnr_avg;
        }
        if ssim_avg < result.min_ssim {
            result.min_ssim = ssim_avg;
        }
        if ssim_avg > result.max_ssim {
            result.max_ssim = ssim_avg;
        }

        psnr_sum += psnr_avg;
        ssim_sum += ssim_avg;
        result.per_frame.push(metric);
    }

    result.avg_psnr = psnr_sum / frame_count as f64;
    result.avg_ssim = ssim_sum / frame_count as f64;

    Ok(result)
}

/// VMAF score (ffmpeg libvmaf pooled metrics).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VmafScore {
    pub frames: u64,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub harmonic_mean: f64,
}

/// True if the local ffmpeg exposes the libvmaf filter.
pub fn ffmpeg_has_libvmaf() -> bool {
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("libvmaf"))
        .unwrap_or(false)
}

/// Compute VMAF between two videos via ffmpeg's libvmaf filter.
///
/// libvmaf takes the distorted stream first and the reference second, so the
/// filter maps input 1 (distorted) then input 0 (reference). Errors clearly if
/// the local ffmpeg has no libvmaf.
pub fn compute_vmaf(reference: &Path, distorted: &Path) -> Result<VmafScore, String> {
    if !ffmpeg_has_libvmaf() {
        return Err(
            "ffmpeg has no libvmaf filter; install an ffmpeg built with --enable-libvmaf".into(),
        );
    }

    let log = std::env::temp_dir().join(format!("imfwizard_vmaf_{}.json", std::process::id()));
    let filter = format!(
        "[1:v][0:v]libvmaf=log_path={}:log_fmt=json",
        filter_option_path(&log)
    );
    let out = std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(reference)
        .arg("-i")
        .arg(distorted)
        .args(["-lavfi", &filter, "-f", "null", "-"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

    if !out.status.success() {
        let _ = std::fs::remove_file(&log);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr.lines().last().unwrap_or("ffmpeg vmaf failed");
        return Err(format!("ffmpeg vmaf failed: {last}"));
    }

    let data =
        std::fs::read_to_string(&log).map_err(|e| format!("Failed to read vmaf log: {e}"))?;
    let _ = std::fs::remove_file(&log);
    parse_vmaf_json(&data)
}

/// Parse libvmaf's JSON log into a pooled score.
fn parse_vmaf_json(data: &str) -> Result<VmafScore, String> {
    let v: serde_json::Value =
        serde_json::from_str(data).map_err(|e| format!("invalid vmaf json: {e}"))?;
    let pooled = v
        .get("pooled_metrics")
        .and_then(|p| p.get("vmaf"))
        .ok_or("vmaf json missing pooled_metrics.vmaf")?;
    let get = |k: &str| pooled.get(k).and_then(|x| x.as_f64());
    let mean = get("mean").ok_or("vmaf json missing mean")?;
    let frames = v
        .get("frames")
        .and_then(|f| f.as_array())
        .map(|a| a.len() as u64)
        .unwrap_or(0);
    Ok(VmafScore {
        frames,
        mean,
        min: get("min").unwrap_or(mean),
        max: get("max").unwrap_or(mean),
        harmonic_mean: get("harmonic_mean").unwrap_or(mean),
    })
}

/// Component keys ffmpeg writes per pixel format: luma for YUV video, and the
/// three primaries for RGB video, which is what DCP and IMF picture essence
/// decodes to.
const PSNR_COMPONENTS: [[&str; 3]; 2] = [
    ["psnr_y", "psnr_u", "psnr_v"],
    ["psnr_r", "psnr_g", "psnr_b"],
];
const SSIM_COMPONENTS: [&str; 2] = ["Y:", "R:"];

/// Parse ffmpeg PSNR stats file.
/// Format: n:1 mse_avg:0.00 mse_y:0.00 mse_u:0.00 mse_v:0.00 psnr_avg:inf psnr_y:inf psnr_u:inf psnr_v:inf
/// RGB:    n:1 mse_avg:0.00 mse_r:0.00 mse_g:0.00 mse_b:0.00 psnr_avg:inf psnr_r:inf psnr_g:inf psnr_b:inf
fn parse_psnr_log(data: &str) -> Vec<(f64, f64, f64, f64)> {
    data.lines()
        .filter_map(|line| {
            let get_val = |key: &str| -> Option<f64> {
                line.split_whitespace()
                    .find(|s| s.starts_with(key))
                    .and_then(|s| s.split(':').nth(1))
                    .and_then(|v| {
                        if v == "inf" {
                            Some(100.0)
                        } else {
                            v.parse().ok()
                        }
                    })
            };
            let psnr_avg = get_val("psnr_avg")?;
            let (first, second, third) = PSNR_COMPONENTS
                .iter()
                .find_map(|keys| Some((get_val(keys[0])?, get_val(keys[1])?, get_val(keys[2])?)))?;
            Some((first, second, third, psnr_avg))
        })
        .collect()
}

/// Parse ffmpeg SSIM stats file.
/// Format: n:1 Y:1.000000 (inf) U:1.000000 (inf) V:1.000000 (inf) All:1.000000 (inf)
/// RGB:    n:1 R:1.000000 G:1.000000 B:1.000000 All:1.000000 (inf)
fn parse_ssim_log(data: &str) -> Vec<(f64, f64)> {
    data.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let value = |key: &str| -> Option<f64> {
                parts
                    .iter()
                    .find(|s| s.starts_with(key))
                    .and_then(|s| s.strip_prefix(key))
                    .and_then(|v| v.parse::<f64>().ok())
            };
            let ssim_all = value("All:")?;
            let ssim_first = SSIM_COMPONENTS.iter().find_map(|key| value(key))?;
            Some((ssim_first, ssim_all))
        })
        .collect()
}

/// Pooled quality metrics from a reference-vs-distorted comparison.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QualityMetrics {
    pub success: bool,
    pub error: String,
    pub vmaf_score: f64,
    pub psnr_avg: f64,
    pub ssim: f64,
}

/// Which metrics to compute in `compute_quality`.
#[derive(Debug, Clone)]
pub struct QualityOptions {
    pub reference: PathBuf,
    pub distorted: PathBuf,
    pub compute_vmaf: bool,
    pub compute_psnr: bool,
    pub compute_ssim: bool,
}

/// Value after `key` on the same output run, e.g. `average:` or `VMAF score:`.
fn value_after(text: &str, key: &str) -> Option<f64> {
    let rest = text.split(key).nth(1)?.trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Compute pooled VMAF/PSNR/SSIM in one ffmpeg run. Distorted is input 0 so the
/// libvmaf filter (distorted first, reference second) is fed correctly.
pub fn compute_quality(opts: &QualityOptions) -> QualityMetrics {
    let mut result = QualityMetrics::default();

    let dist = opts.distorted.to_string_lossy().to_string();
    let reference = opts.reference.to_string_lossy().to_string();

    let mut parts = Vec::new();
    if opts.compute_vmaf {
        parts.push("libvmaf");
    }
    if opts.compute_psnr {
        parts.push("psnr");
    }
    if opts.compute_ssim {
        parts.push("ssim");
    }
    if parts.is_empty() {
        result.error = "No metrics selected".into();
        return result;
    }

    let filter = format!("[0:v][1:v]{}", parts.join(";[0:v][1:v]"));
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-i", &dist, "-i", &reference, "-lavfi", &filter, "-f", "null", "-",
        ])
        .output();

    let output = match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        }
        Err(e) => {
            result.error = format!("Failed to run ffmpeg: {e}");
            return result;
        }
    };

    if output.is_empty() {
        result.error = "ffmpeg quality analysis failed".into();
        return result;
    }

    if let Some(v) = value_after(&output, "VMAF score:") {
        result.vmaf_score = v;
    }
    if let Some(v) = value_after(&output, "average:") {
        result.psnr_avg = v;
    }
    if let Some(v) = value_after(&output, "All:") {
        result.ssim = v;
    }

    result.success = true;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_after_parses_metric_tokens() {
        assert_eq!(value_after("... average:42.50 ...", "average:"), Some(42.5));
        assert_eq!(
            value_after("VMAF score: 96.31\n", "VMAF score:"),
            Some(96.31)
        );
        assert_eq!(value_after("no key here", "average:"), None);
    }

    #[test]
    fn parse_psnr_and_ssim_logs() {
        let psnr = "n:1 mse_avg:0.00 psnr_avg:inf psnr_y:inf psnr_u:inf psnr_v:inf\n\
                    n:2 mse_avg:1.00 psnr_avg:48.13 psnr_y:50.00 psnr_u:45.00 psnr_v:44.00\n";
        let frames = parse_psnr_log(psnr);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].3, 100.0); // inf -> 100
        assert!((frames[1].3 - 48.13).abs() < 1e-9);
        assert!((frames[1].0 - 50.0).abs() < 1e-9);

        let ssim = "n:1 Y:1.000000 (inf) U:1.000000 (inf) V:1.000000 (inf) All:1.000000 (inf)\n\
                    n:2 Y:0.990000 U:0.990000 V:0.990000 All:0.985000\n";
        let sframes = parse_ssim_log(ssim);
        assert_eq!(sframes.len(), 2);
        assert!((sframes[1].1 - 0.985).abs() < 1e-9);
    }

    /// ffmpeg 8.1 stats for a yuv420p pair, pasted from a run.
    const YUV_PSNR_LOG: &str = "\
n:1 mse_avg:84.66 mse_y:65.39 mse_u:98.72 mse_v:147.66 psnr_avg:28.85 psnr_y:29.98 psnr_u:28.19 psnr_v:26.44
n:2 mse_avg:89.44 mse_y:67.33 mse_u:98.41 mse_v:168.93 psnr_avg:28.62 psnr_y:29.85 psnr_u:28.20 psnr_v:25.85
";
    const YUV_SSIM_LOG: &str = "\
n:1 Y:0.959888 U:0.946021 V:0.983675 All:0.961541 (14.150061)
n:2 Y:0.958191 U:0.946849 V:0.981837 All:0.960242 (14.005705)
";

    /// The same, for a gbrp pair. DCP and IMF picture essence decodes to RGB,
    /// so this is the naming every real package produces.
    const RGB_PSNR_LOG: &str = "\
n:1 mse_avg:64.04 mse_r:59.66 mse_g:64.36 mse_b:68.12 psnr_avg:30.07 psnr_r:30.37 psnr_g:30.04 psnr_b:29.80
n:2 mse_avg:63.76 mse_r:64.56 mse_g:65.57 mse_b:61.16 psnr_avg:30.09 psnr_r:30.03 psnr_g:29.96 psnr_b:30.27
";
    const RGB_SSIM_LOG: &str = "\
n:1 R:0.760857 G:0.731124 B:0.662389 All:0.718123 (5.499410)
n:2 R:0.763922 G:0.730375 B:0.672471 All:0.722256 (5.563553)
";

    #[test]
    fn rgb_stats_parse_the_way_luma_stats_do() {
        let rgb = parse_psnr_log(RGB_PSNR_LOG);
        let yuv = parse_psnr_log(YUV_PSNR_LOG);
        assert_eq!(rgb.len(), yuv.len(), "an RGB run reported no frames");
        assert!((rgb[0].3 - 30.07).abs() < 1e-9, "psnr_avg: {rgb:?}");
        assert!((rgb[0].0 - 30.37).abs() < 1e-9, "psnr_r: {rgb:?}");
        assert!((rgb[0].1 - 30.04).abs() < 1e-9, "psnr_g: {rgb:?}");
        assert!((rgb[0].2 - 29.80).abs() < 1e-9, "psnr_b: {rgb:?}");
        assert!((yuv[0].0 - 29.98).abs() < 1e-9, "psnr_y: {yuv:?}");

        let rgb = parse_ssim_log(RGB_SSIM_LOG);
        let yuv = parse_ssim_log(YUV_SSIM_LOG);
        assert_eq!(rgb.len(), yuv.len(), "an RGB run reported no frames");
        assert!((rgb[0].1 - 0.718123).abs() < 1e-9, "All: {rgb:?}");
        assert!((rgb[0].0 - 0.760857).abs() < 1e-9, "R: {rgb:?}");
        assert!((yuv[0].0 - 0.959888).abs() < 1e-9, "Y: {yuv:?}");
    }

    #[test]
    fn a_windows_stats_path_is_escaped_for_the_filter_graph() {
        assert_eq!(
            filter_option_path(Path::new("C:\\Users\\r\\Temp\\imfwizard_psnr_1_0.log")),
            "C\\\\:/Users/r/Temp/imfwizard_psnr_1_0.log"
        );
        assert_eq!(filter_option_path(Path::new("/tmp/x.log")), "/tmp/x.log");
    }

    // a linux path with a colon goes through the same two parsers a windows
    // drive letter does
    #[test]
    fn ffmpeg_writes_a_stats_file_on_a_path_with_a_colon() {
        let dir = tempfile::tempdir().unwrap();
        // windows forbids a colon in a name, its temp path has one in the drive
        let colon_dir = if cfg!(windows) {
            dir.path().to_path_buf()
        } else {
            let nested = dir.path().join("c:d");
            std::fs::create_dir(&nested).unwrap();
            nested
        };
        let stats = colon_dir.join("psnr.log");
        let escaped = filter_option_path(&stats);
        assert!(escaped.contains("\\\\:"), "no colon to prove: {escaped}");
        let run = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=s=64x64:r=1",
                "-f",
                "lavfi",
                "-i",
                "testsrc=s=64x64:r=1",
                "-frames:v",
                "1",
                "-lavfi",
                &format!("[0:v][1:v]psnr=stats_file={escaped}"),
                "-f",
                "null",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(stats.is_file(), "no stats file at {}", stats.display());
    }

    #[test]
    fn two_comparisons_do_not_share_a_stats_file() {
        assert_ne!(stats_log_path("psnr"), stats_log_path("psnr"));
        assert_ne!(stats_log_path("psnr"), stats_log_path("ssim"));
    }

    #[test]
    fn a_line_with_no_component_keys_is_no_frame() {
        assert!(parse_psnr_log("n:1 mse_avg:0.00 psnr_avg:inf\n").is_empty());
        assert!(parse_ssim_log("n:1 All:1.000000 (inf)\n").is_empty());
    }

    #[test]
    fn parse_vmaf_json_pools_score() {
        // shape of ffmpeg libvmaf log_fmt=json output (trimmed)
        let sample = r#"{
            "version": "2.3.1",
            "frames": [
                {"frameNum": 0, "metrics": {"vmaf": 92.5}},
                {"frameNum": 1, "metrics": {"vmaf": 96.5}}
            ],
            "pooled_metrics": {
                "vmaf": {"min": 92.5, "max": 96.5, "mean": 94.5, "harmonic_mean": 94.48}
            }
        }"#;
        let score = parse_vmaf_json(sample).unwrap();
        assert_eq!(score.frames, 2);
        assert!((score.mean - 94.5).abs() < 1e-9);
        assert!((score.min - 92.5).abs() < 1e-9);
        assert!((score.max - 96.5).abs() < 1e-9);
        assert!((score.harmonic_mean - 94.48).abs() < 1e-9);
    }

    #[test]
    fn parse_vmaf_json_missing_pool_errors() {
        assert!(parse_vmaf_json(r#"{"frames": []}"#).is_err());
    }
}
