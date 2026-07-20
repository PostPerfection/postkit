use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Visible watermark options.
///
/// This burns a plainly visible text mark into each frame with ffmpeg drawtext.
/// It is NOT invisible/forensic watermarking and carries no recoverable payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatermarkOptions {
    pub operator_id: String,
    pub session_id: String,
    pub strength: f32,
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
}

/// Watermark operation result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatermarkResult {
    pub success: bool,
    pub error: String,
    pub frames_processed: u64,
    pub payload_hash: String,
}

/// Burn a visible text mark (first 8 hex chars of the operator/session hash plus
/// the session id) into each frame with ffmpeg drawtext. Plainly visible, not
/// forensic.
pub fn embed_watermark(opts: &WatermarkOptions) -> WatermarkResult {
    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return WatermarkResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    // Payload hash from operator + session IDs, shown in the visible mark.
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(opts.operator_id.as_bytes());
    hasher.update(opts.session_id.as_bytes());
    let payload_hash = hex::encode(hasher.finalize());

    let frames: Vec<PathBuf> = std::fs::read_dir(&opts.input_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "tif" | "tiff" | "dpx" | "exr" | "png" | "jpg"))
        })
        .collect();

    if frames.is_empty() {
        return WatermarkResult {
            success: false,
            error: "No image frames found in input directory".into(),
            ..Default::default()
        };
    }

    let strength = if opts.strength > 0.0 {
        (opts.strength * 255.0) as u8
    } else {
        8 // very faint by default
    };

    let ext = frames
        .first()
        .and_then(|f| f.extension())
        .and_then(|e| e.to_str())
        .unwrap_or("tif");

    let input_pattern = opts.input_dir.join(format!("*.{ext}"));
    let output_pattern = opts.output_dir.join(format!("%06d.{ext}"));

    let watermark_text = format!("{}:{}", &payload_hash[..8], opts.session_id);
    let filter = format!(
        "drawtext=text='{}':fontsize=10:fontcolor=white@0.{:02}:x=10:y=h-20",
        watermark_text.replace('\'', "\\'"),
        strength.min(99)
    );

    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-pattern_type")
        .arg("glob")
        .arg("-i")
        .arg(input_pattern.to_string_lossy().as_ref())
        .arg("-vf")
        .arg(&filter)
        .arg(&output_pattern)
        .output();

    match output {
        Ok(o) if o.status.success() => WatermarkResult {
            success: true,
            error: String::new(),
            frames_processed: frames.len() as u64,
            payload_hash,
        },
        Ok(o) => WatermarkResult {
            success: false,
            error: String::from_utf8_lossy(&o.stderr).into_owned(),
            ..Default::default()
        },
        Err(e) => WatermarkResult {
            success: false,
            error: format!("Failed to run ffmpeg: {e}"),
            ..Default::default()
        },
    }
}
