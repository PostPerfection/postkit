use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Dolby Vision profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DolbyVisionProfile {
    /// MEL (Minimum Enhancement Layer) — single-layer PQ
    Profile5,
    /// HLG backward compatible
    Profile8,
    /// SDR backward compatible (most common for cinema)
    #[default]
    Profile81,
    Unknown,
}

/// HDR metadata type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum HdrType {
    #[default]
    Sdr,
    /// Static PQ metadata (SMPTE ST 2086 + CTA 861.3)
    Hdr10,
    /// Dynamic PQ metadata (Samsung)
    Hdr10Plus,
    /// Dolby Vision RPU
    DolbyVision,
    /// Hybrid Log-Gamma
    Hlg,
    /// Academy Color Encoding System
    Aces,
}

/// Static HDR10 metadata (mastering display + content light level).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Hdr10Metadata {
    // Mastering display colour volume (SMPTE ST 2086)
    pub display_primaries_gx: u16,
    pub display_primaries_gy: u16,
    pub display_primaries_bx: u16,
    pub display_primaries_by: u16,
    pub display_primaries_rx: u16,
    pub display_primaries_ry: u16,
    pub white_point_x: u16,
    pub white_point_y: u16,
    /// cd/m² × 10000
    pub max_luminance: u32,
    /// cd/m² × 10000
    pub min_luminance: u32,
    // Content light level (CTA 861.3)
    /// MaxCLL
    pub max_cll: u16,
    /// MaxFALL
    pub max_fall: u16,
}

/// Dolby Vision RPU (Reference Processing Unit) options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DolbyVisionOptions {
    /// Source video/image sequence
    pub input: PathBuf,
    /// .bin RPU file or XML metadata
    pub rpu_file: PathBuf,
    pub profile: DolbyVisionProfile,
    pub output: PathBuf,
    /// Embed RPU in output MXF
    pub embed_rpu: bool,
}

/// HDR metadata injection options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HdrMetadataOptions {
    pub input: PathBuf,
    pub hdr_type: HdrType,
    pub hdr10: Hdr10Metadata,
    /// Dolby Vision metadata XML
    pub dolby_vision_xml: PathBuf,
    pub output: PathBuf,
}

/// Inject HDR10 static metadata into a video file using ffmpeg.
pub fn inject_hdr10_metadata(opts: &HdrMetadataOptions) -> i32 {
    let metadata_args = format!(
        "master-display=G({},{})B({},{})R({},{})WP({},{})L({},{}):max-cll={},{}",
        opts.hdr10.display_primaries_gx,
        opts.hdr10.display_primaries_gy,
        opts.hdr10.display_primaries_bx,
        opts.hdr10.display_primaries_by,
        opts.hdr10.display_primaries_rx,
        opts.hdr10.display_primaries_ry,
        opts.hdr10.white_point_x,
        opts.hdr10.white_point_y,
        opts.hdr10.max_luminance,
        opts.hdr10.min_luminance,
        opts.hdr10.max_cll,
        opts.hdr10.max_fall,
    );

    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&opts.input)
        .arg("-c")
        .arg("copy")
        .arg("-metadata:s:v")
        .arg("side_data_type=Mastering display metadata")
        .arg("-metadata:s:v")
        .arg(format!("hdr10={metadata_args}"))
        .arg(&opts.output)
        .output();

    match output {
        Ok(o) if o.status.success() => 0,
        Ok(o) => {
            tracing::error!("ffmpeg failed: {}", String::from_utf8_lossy(&o.stderr));
            -1
        }
        Err(e) => {
            tracing::error!("Failed to run ffmpeg: {e}");
            -1
        }
    }
}

/// Inject Dolby Vision RPU into a HEVC stream using dovi_tool.
pub fn inject_dolby_vision(opts: &DolbyVisionOptions) -> i32 {
    let output = std::process::Command::new("dovi_tool")
        .arg("inject-rpu")
        .arg("-i")
        .arg(&opts.input)
        .arg("--rpu-in")
        .arg(&opts.rpu_file)
        .arg("-o")
        .arg(&opts.output)
        .output();

    match output {
        Ok(o) if o.status.success() => 0,
        Ok(o) => {
            tracing::error!("dovi_tool failed: {}", String::from_utf8_lossy(&o.stderr));
            -1
        }
        Err(e) => {
            tracing::error!("Failed to run dovi_tool: {e}");
            -1
        }
    }
}

/// Detect HDR type from a video file using ffprobe.
pub fn detect_hdr_type(input: &Path) -> HdrType {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_frames",
            "-read_intervals",
            "%+#1",
        ])
        .arg(input)
        .output();

    let Ok(output) = output else {
        return HdrType::Sdr;
    };

    let json_str = String::from_utf8_lossy(&output.stdout);

    // Check for Dolby Vision RPU
    if json_str.contains("dovi") || json_str.contains("DOVI") {
        return HdrType::DolbyVision;
    }
    // Check for HDR10+
    if json_str.contains("hdr10plus") || json_str.contains("HDR10PLUS") {
        return HdrType::Hdr10Plus;
    }
    // Check for PQ/HDR10
    if json_str.contains("smpte2084") || json_str.contains("bt2020") {
        return HdrType::Hdr10;
    }
    // Check for HLG
    if json_str.contains("arib-std-b67") || json_str.contains("hlg") {
        return HdrType::Hlg;
    }

    HdrType::Sdr
}

/// Read HDR10 static metadata from a video file using ffprobe.
pub fn read_hdr10_metadata(input: &Path) -> Hdr10Metadata {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_frames",
            "-read_intervals",
            "%+#1",
            "-show_entries",
            "frame=side_data_list",
        ])
        .arg(input)
        .output();

    let Ok(output) = output else {
        return Hdr10Metadata::default();
    };

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();

    // Extract from side_data_list
    let side_data = json["frames"][0]["side_data_list"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut meta = Hdr10Metadata::default();

    for sd in &side_data {
        if sd["side_data_type"]
            .as_str()
            .is_some_and(|s| s.contains("Mastering"))
        {
            meta.max_luminance = sd["max_luminance"]
                .as_str()
                .and_then(|s| s.split('/').next()?.parse().ok())
                .unwrap_or(0);
            meta.min_luminance = sd["min_luminance"]
                .as_str()
                .and_then(|s| s.split('/').next()?.parse().ok())
                .unwrap_or(0);
        }
        if sd["side_data_type"]
            .as_str()
            .is_some_and(|s| s.contains("Content light"))
        {
            meta.max_cll = sd["max_content"].as_u64().unwrap_or(0) as u16;
            meta.max_fall = sd["max_average"].as_u64().unwrap_or(0) as u16;
        }
    }

    meta
}

/// Convert between HDR formats using ffmpeg tone mapping.
pub fn convert_hdr(input: &Path, target_type: HdrType, output: &Path) -> i32 {
    let filter = match target_type {
        HdrType::Sdr => {
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709:t=bt709:m=bt709,tonemap=tonemap=hable:desat=0,zscale=dither=ordered,format=yuv420p"
        }
        HdrType::Hlg => "zscale=t=arib-std-b67",
        HdrType::Hdr10 => "zscale=t=smpte2084:p=bt2020:m=bt2020nc",
        _ => {
            tracing::error!("Unsupported target HDR type: {target_type:?}");
            return -1;
        }
    };

    let result = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-vf")
        .arg(filter)
        .arg("-c:v")
        .arg("libx265")
        .arg("-c:a")
        .arg("copy")
        .arg(output)
        .output();

    match result {
        Ok(o) if o.status.success() => 0,
        Ok(o) => {
            tracing::error!(
                "ffmpeg HDR conversion failed: {}",
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

/// Dolby Vision RPU mode for dovi_tool operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DvMode {
    Mode0,
    Mode1,
    Mode2,
    Mode4,
    Mode5,
}

impl DvMode {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Mode0 => "0",
            Self::Mode1 => "1",
            Self::Mode2 => "2",
            Self::Mode4 => "4",
            Self::Mode5 => "5",
        }
    }
}

/// Extract Dolby Vision RPU using dovi_tool.
pub fn extract_rpu(input: &Path, output: &Path) -> Result<(), String> {
    let out = std::process::Command::new("dovi_tool")
        .arg("extract-rpu")
        .arg("-i")
        .arg(input)
        .arg("-o")
        .arg(output)
        .output()
        .map_err(|e| format!("Failed to run dovi_tool: {e}"))?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(())
}

/// Convert Dolby Vision mode using dovi_tool.
pub fn convert_dv_mode(input: &Path, output: &Path, mode: DvMode) -> Result<(), String> {
    let out = std::process::Command::new("dovi_tool")
        .arg("convert")
        .arg("--mode")
        .arg(mode.as_str())
        .arg("-i")
        .arg(input)
        .arg("-o")
        .arg(output)
        .output()
        .map_err(|e| format!("Failed to run dovi_tool: {e}"))?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(())
}

/// Inject RPU into HEVC stream using dovi_tool.
pub fn inject_rpu(hevc: &Path, rpu: &Path, output: &Path) -> Result<(), String> {
    let out = std::process::Command::new("dovi_tool")
        .arg("inject-rpu")
        .arg("-i")
        .arg(hevc)
        .arg("--rpu-in")
        .arg(rpu)
        .arg("-o")
        .arg(output)
        .output()
        .map_err(|e| format!("Failed to run dovi_tool: {e}"))?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(())
}
