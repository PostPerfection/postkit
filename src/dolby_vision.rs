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

impl From<DvMode> for dolby_vision::rpu::ConversionMode {
    fn from(mode: DvMode) -> Self {
        match mode {
            DvMode::Mode0 => dolby_vision::rpu::ConversionMode::Lossless,
            DvMode::Mode1 => dolby_vision::rpu::ConversionMode::ToMel,
            DvMode::Mode2 => dolby_vision::rpu::ConversionMode::To81,
            DvMode::Mode4 => dolby_vision::rpu::ConversionMode::To81MappingPreserved,
            DvMode::Mode5 => dolby_vision::rpu::ConversionMode::To84,
        }
    }
}

/// Extract Dolby Vision RPU from HEVC stream using dovi_tool.
///
/// This requires full HEVC bitstream parsing, so it delegates to the dovi_tool CLI.
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

/// Convert Dolby Vision RPU mode natively using the dolby_vision crate.
///
/// Reads raw RPU binary data, converts each RPU to the target mode, writes the result.
/// For profile conversion on .bin RPU files (not full HEVC streams).
pub fn convert_dv_mode(input: &Path, output: &Path, mode: DvMode) -> Result<(), String> {
    use dolby_vision::rpu::dovi_rpu::DoviRpu;

    let data = std::fs::read(input).map_err(|e| format!("Failed to read RPU file: {e}"))?;

    // RPU .bin files contain concatenated RPU NALUs separated by start codes
    let rpus = parse_rpu_bin_file(&data)?;
    let conversion_mode: dolby_vision::rpu::ConversionMode = mode.into();

    let mut out_buf = Vec::new();
    for rpu_data in &rpus {
        let mut rpu =
            DoviRpu::parse_rpu(rpu_data).map_err(|e| format!("Failed to parse RPU: {e}"))?;
        rpu.convert_with_mode(conversion_mode)
            .map_err(|e| format!("Failed to convert RPU: {e}"))?;
        let converted = rpu
            .write_rpu()
            .map_err(|e| format!("Failed to write RPU: {e}"))?;
        // Write start code + NALU
        out_buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out_buf.extend_from_slice(&converted);
    }

    std::fs::write(output, &out_buf).map_err(|e| format!("Failed to write output: {e}"))?;

    tracing::info!(
        "Converted {} RPU(s) to mode {} → {}",
        rpus.len(),
        mode.as_str(),
        output.display()
    );
    Ok(())
}

/// Parse a single RPU NALU from raw bytes (no start code prefix).
pub fn parse_single_rpu(data: &[u8]) -> Result<dolby_vision::rpu::dovi_rpu::DoviRpu, String> {
    dolby_vision::rpu::dovi_rpu::DoviRpu::parse_rpu(data)
        .map_err(|e| format!("RPU parse error: {e}"))
}

/// Convert a single RPU in-memory to the target profile/mode.
pub fn convert_rpu(data: &[u8], mode: DvMode) -> Result<Vec<u8>, String> {
    use dolby_vision::rpu::dovi_rpu::DoviRpu;

    let mut rpu = DoviRpu::parse_rpu(data).map_err(|e| format!("RPU parse error: {e}"))?;
    let conversion_mode: dolby_vision::rpu::ConversionMode = mode.into();
    rpu.convert_with_mode(conversion_mode)
        .map_err(|e| format!("RPU conversion error: {e}"))?;
    rpu.write_rpu().map_err(|e| format!("RPU write error: {e}"))
}

/// Generate a default Dolby Vision profile 8.1 RPU.
pub fn generate_profile81_rpu() -> Result<Vec<u8>, String> {
    use dolby_vision::rpu::dovi_rpu::DoviRpu;
    use dolby_vision::rpu::generate::GenerateConfig;

    let config = GenerateConfig::default();
    let rpu = DoviRpu::profile81_config(&config)
        .map_err(|e| format!("Failed to generate profile 8.1 RPU: {e}"))?;
    rpu.write_rpu()
        .map_err(|e| format!("Failed to write RPU: {e}"))
}

/// Generate a default Dolby Vision profile 8.4 RPU.
pub fn generate_profile84_rpu() -> Result<Vec<u8>, String> {
    use dolby_vision::rpu::dovi_rpu::DoviRpu;
    use dolby_vision::rpu::generate::GenerateConfig;

    let config = GenerateConfig::default();
    let rpu = DoviRpu::profile84_config(&config)
        .map_err(|e| format!("Failed to generate profile 8.4 RPU: {e}"))?;
    rpu.write_rpu()
        .map_err(|e| format!("Failed to write RPU: {e}"))
}

/// Parse a .bin RPU file into individual RPU NALUs.
fn parse_rpu_bin_file(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut rpus = Vec::new();
    let mut i = 0;

    while i < data.len() {
        // Find start code (0x000001 or 0x00000001)
        let start = if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
            i + 4
        } else if i + 3 <= data.len() && data[i..i + 3] == [0, 0, 1] {
            i + 3
        } else {
            // If no start code at beginning, treat the whole thing as a single RPU
            if rpus.is_empty() {
                rpus.push(data.to_vec());
            }
            break;
        };

        // Find next start code
        let mut end = start;
        while end < data.len() {
            if end + 4 <= data.len() && data[end..end + 4] == [0, 0, 0, 1] {
                break;
            }
            if end + 3 <= data.len() && data[end..end + 3] == [0, 0, 1] {
                break;
            }
            end += 1;
        }

        if start < end {
            rpus.push(data[start..end].to_vec());
        }
        i = end;
    }

    if rpus.is_empty() {
        return Err("No RPU NALUs found in file".to_string());
    }
    Ok(rpus)
}

/// Inject RPU into HEVC stream using dovi_tool.
///
/// This requires full HEVC bitstream manipulation, so it delegates to the dovi_tool CLI.
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
