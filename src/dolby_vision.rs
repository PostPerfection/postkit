use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::extension_metadata::blocks::{ExtMetadataBlock, ExtMetadataBlockLevel6};
use dolby_vision::rpu::vdr_dm_data::VdrDmData;
use hevc_parser::HevcParser;
use hevc_parser::hevc::NAL_UNSPEC62;
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

/// Build the libx265 params that embed HDR10 static metadata as SEI: mastering
/// display colour volume (ST 2086) plus MaxCLL/MaxFALL (CTA 861.3).
pub fn x265_hdr10_params(m: &Hdr10Metadata) -> String {
    format!(
        "hdr10=1:hdr10-opt=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:\
         master-display=G({},{})B({},{})R({},{})WP({},{})L({},{}):max-cll={},{}",
        m.display_primaries_gx,
        m.display_primaries_gy,
        m.display_primaries_bx,
        m.display_primaries_by,
        m.display_primaries_rx,
        m.display_primaries_ry,
        m.white_point_x,
        m.white_point_y,
        m.max_luminance,
        m.min_luminance,
        m.max_cll,
        m.max_fall,
    )
}

/// Embed HDR10 static metadata (mastering display + MaxCLL/MaxFALL) into a video.
///
/// HDR10 metadata lives in HEVC SEI, which cannot be added to an existing stream
/// with `-c copy`. So this re-encodes the video with libx265, writing the SEI via
/// x265-params; audio is copied. The output is a genuine HDR10 HEVC stream, not a
/// container tag that players ignore.
pub fn inject_hdr10_metadata(opts: &HdrMetadataOptions) -> i32 {
    let x265_params = x265_hdr10_params(&opts.hdr10);

    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&opts.input)
        .args(["-c:v", "libx265", "-pix_fmt", "yuv420p10le"])
        .arg("-x265-params")
        .arg(&x265_params)
        .args(["-c:a", "copy"])
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
    let data = std::fs::read(input).map_err(|e| format!("Failed to read RPU file: {e}"))?;

    // RPU .bin files contain concatenated RPU NALUs separated by start codes
    let rpus = parse_rpu_bin_file(&data)?;
    let conversion_mode: dolby_vision::rpu::ConversionMode = mode.into();

    let mut out_buf = Vec::new();
    for nalu in &rpus {
        let mut rpu =
            DoviRpu::parse_unspec62_nalu(nalu).map_err(|e| format!("Failed to parse RPU: {e}"))?;
        rpu.convert_with_mode(conversion_mode)
            .map_err(|e| format!("Failed to convert RPU: {e}"))?;
        out_buf.extend_from_slice(NAL_START_CODE);
        out_buf.extend_from_slice(&write_rpu_nalu(&rpu)?);
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

// a single RPU NALU, with or without its start code, escaped as it sits in a .bin
pub fn parse_single_rpu(data: &[u8]) -> Result<dolby_vision::rpu::dovi_rpu::DoviRpu, String> {
    dolby_vision::rpu::dovi_rpu::DoviRpu::parse_unspec62_nalu(data)
        .map_err(|e| format!("RPU parse error: {e}"))
}

/// Convert a single RPU in-memory to the target profile/mode.
pub fn convert_rpu(data: &[u8], mode: DvMode) -> Result<Vec<u8>, String> {
    let mut rpu = parse_single_rpu(data)?;
    let conversion_mode: dolby_vision::rpu::ConversionMode = mode.into();
    rpu.convert_with_mode(conversion_mode)
        .map_err(|e| format!("RPU conversion error: {e}"))?;
    write_rpu_nalu(&rpu)
}

// the two byte HEVC NAL header write_hevc_unspec62_nalu prepends, which a .bin does not carry
const HEVC_NAL_HEADER_BYTES: usize = 2;

// dovi_tool keeps the emulation prevention bytes in a .bin, so the RPU is written as a NALU
fn write_rpu_nalu(rpu: &DoviRpu) -> Result<Vec<u8>, String> {
    let nalu = rpu
        .write_hevc_unspec62_nalu()
        .map_err(|e| format!("Failed to write RPU: {e}"))?;
    Ok(nalu[HEVC_NAL_HEADER_BYTES..].to_vec())
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

pub const DOLBY_VISION_FIXTURE_FRAMES: usize = 6;

const FIXTURE_SOURCE_FILTER: &str = "color=c=gray:s=320x180:r=25";
const FIXTURE_PIXEL_FORMAT: &str = "yuv420p10le";
const FIXTURE_LEVEL_1_AVG_PQ: u16 = 1229;
const NAL_START_CODE: &[u8] = &[0, 0, 0, 1];
const NAL_START_CODE_PREFIX: &[u8] = &[0, 0, 1];
const LAST_VIDEO_CODING_NAL_TYPE: u8 = 21;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DolbyVisionFixtureProfile {
    Profile5,
    Profile81,
    Profile84,
}

impl From<DolbyVisionFixtureProfile> for dolby_vision::rpu::generate::GenerateProfile {
    fn from(profile: DolbyVisionFixtureProfile) -> Self {
        use dolby_vision::rpu::generate::GenerateProfile;
        match profile {
            DolbyVisionFixtureProfile::Profile5 => GenerateProfile::Profile5,
            DolbyVisionFixtureProfile::Profile81 => GenerateProfile::Profile81,
            DolbyVisionFixtureProfile::Profile84 => GenerateProfile::Profile84,
        }
    }
}

fn encode_plain_hevc(output: &Path) -> Result<(), String> {
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            FIXTURE_SOURCE_FILTER,
            "-frames:v",
        ])
        .arg(DOLBY_VISION_FIXTURE_FRAMES.to_string())
        .args([
            "-pix_fmt",
            FIXTURE_PIXEL_FORMAT,
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=none",
            "-f",
            "hevc",
        ])
        .arg(output)
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "ffmpeg failed to encode the Dolby Vision fixture base layer, which needs libx265: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn generate_rpu_nalus(
    profile: DolbyVisionFixtureProfile,
    level6: Option<ExtMetadataBlockLevel6>,
    level1_max_pq: Option<u16>,
) -> Result<Vec<Vec<u8>>, String> {
    use dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlockLevel1;
    use dolby_vision::rpu::generate::{GenerateConfig, VideoShot};
    use dolby_vision::rpu::vdr_dm_data::CmVersion;

    let metadata_blocks = level1_max_pq
        .map(|max_pq| {
            vec![ExtMetadataBlock::Level1(ExtMetadataBlockLevel1::new(
                0,
                max_pq,
                FIXTURE_LEVEL_1_AVG_PQ,
            ))]
        })
        .unwrap_or_default();

    let mut config = GenerateConfig {
        cm_version: CmVersion::V40,
        profile: profile.into(),
        length: DOLBY_VISION_FIXTURE_FRAMES,
        level6,
        shots: vec![VideoShot {
            start: 0,
            duration: DOLBY_VISION_FIXTURE_FRAMES,
            metadata_blocks,
            ..Default::default()
        }],
        ..Default::default()
    };
    config.fixup_l1();

    config
        .generate_rpu_list()
        .map_err(|e| format!("Failed to generate the Dolby Vision RPUs: {e}"))?
        .iter()
        .map(|rpu| {
            rpu.write_hevc_unspec62_nalu()
                .map_err(|e| format!("Failed to write a Dolby Vision RPU NALU: {e}"))
        })
        .collect()
}

fn nal_start_offsets(stream: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut index = 0;
    while index + NAL_START_CODE_PREFIX.len() <= stream.len() {
        if &stream[index..index + NAL_START_CODE_PREFIX.len()] == NAL_START_CODE_PREFIX {
            offsets.push(index);
            index += NAL_START_CODE_PREFIX.len();
        } else {
            index += 1;
        }
    }
    offsets
}

// the rpu is suffixed to the slices of its access unit, so it lands after each slice nal
fn insert_rpus_after_slices(stream: &[u8], rpus: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let offsets = nal_start_offsets(stream);
    let mut out = Vec::with_capacity(stream.len());
    let mut next_rpu = 0;

    for (index, offset) in offsets.iter().enumerate() {
        let end = offsets.get(index + 1).copied().unwrap_or(stream.len());
        out.extend_from_slice(&stream[*offset..end]);

        let nal_type = stream[offset + NAL_START_CODE_PREFIX.len()] >> 1;
        if nal_type <= LAST_VIDEO_CODING_NAL_TYPE {
            let Some(rpu) = rpus.get(next_rpu) else {
                return Err(format!(
                    "The fixture base layer holds more than the {} coded pictures the RPUs cover",
                    rpus.len()
                ));
            };
            out.extend_from_slice(NAL_START_CODE);
            out.extend_from_slice(rpu);
            next_rpu += 1;
        }
    }

    if next_rpu != rpus.len() {
        return Err(format!(
            "The fixture base layer holds {next_rpu} coded pictures for {} RPUs",
            rpus.len()
        ));
    }
    Ok(out)
}

pub fn write_dolby_vision_fixture(
    directory: &Path,
    name: &str,
    profile: DolbyVisionFixtureProfile,
    level6: Option<ExtMetadataBlockLevel6>,
    level1_max_pq: Option<u16>,
) -> Result<PathBuf, String> {
    let base_layer = directory.join("plain.hevc");
    encode_plain_hevc(&base_layer)?;
    let stream = std::fs::read(&base_layer)
        .map_err(|e| format!("Failed to read {}: {e}", base_layer.display()))?;

    let rpus = generate_rpu_nalus(profile, level6, level1_max_pq)?;
    let output = directory.join(name);
    std::fs::write(&output, insert_rpus_after_slices(&stream, &rpus)?)
        .map_err(|e| format!("Failed to write {}: {e}", output.display()))?;
    Ok(output)
}

// each NALU keeps its start code, which is what parse_unspec62_nalu trims
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
            rpus.push(data[i..end].to_vec());
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

const HEVC_CODEC_NAME: &str = "hevc";
const BYTES_PER_MIB: usize = 1024 * 1024;
const MAX_ANNEX_B_STREAM_MIB: usize = 512;
const DOLBY_VISION_LEVEL_1: u8 = 1;
const DOLBY_VISION_LEVEL_6: u8 = 6;
const DOLBY_VISION_PROFILE_5: u8 = 5;

// ST 2084 EOTF, the 12 bit code is the RPU's PQ signal
const PQ_CODE_MAX: f32 = 4095.0;
const PQ_M1: f32 = 2610.0 / 16384.0;
const PQ_M2: f32 = 2523.0 * 128.0 / 4096.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 * 32.0 / 4096.0;
const PQ_C3: f32 = 2392.0 * 32.0 / 4096.0;
const PQ_PEAK_NITS: f32 = 10000.0;

// level 6 holds the mastering display minimum in 0.0001 cd/m² and the maximum in cd/m²
const MASTERING_DISPLAY_MIN_LUMINANCE_STEP_NITS: f32 = 0.0001;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DolbyVisionSummary {
    pub profile: u8,
    pub frames: usize,
    pub shots: usize,
    pub max_content_light_level_nits: Option<f32>,
    pub max_frame_average_light_level_nits: Option<f32>,
    pub peak_luminance_nits: f32,
    pub mastering_display_max_nits: Option<f32>,
    pub mastering_display_min_nits: Option<f32>,
}

fn pq_code_to_nits(code: u16) -> f32 {
    let signal = f32::from(code) / PQ_CODE_MAX;
    let encoded = signal.powf(1.0 / PQ_M2);
    let numerator = (encoded - PQ_C1).max(0.0);
    let denominator = PQ_C2 - PQ_C3 * encoded;
    PQ_PEAK_NITS * (numerator / denominator).powf(1.0 / PQ_M1)
}

fn video_codec_name(path: &Path) -> Result<String, String> {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("Failed to run ffprobe: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn demux_annex_b(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let cap = MAX_ANNEX_B_STREAM_MIB * BYTES_PER_MIB;

    let mut child = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-bsf:v",
            "hevc_mp4toannexb",
            "-f",
            "hevc",
            "-",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

    let mut stdout = child.stdout.take().expect("ffmpeg stdout is piped");
    let mut stream = Vec::new();
    let read = stdout
        .by_ref()
        .take(cap as u64 + 1)
        .read_to_end(&mut stream);

    if read.is_err() || stream.len() > cap {
        let _ = child.kill();
        let _ = child.wait();
    }

    if let Err(e) = read {
        return Err(format!("Failed to read the ffmpeg output: {e}"));
    }
    if stream.len() > cap {
        return Err(format!(
            "The video stream of {} is larger than the {MAX_ANNEX_B_STREAM_MIB} MiB Dolby Vision read cap",
            path.display()
        ));
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for ffmpeg: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffmpeg failed to demux {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    Ok(stream)
}

fn parse_rpu_nalus(stream: &[u8]) -> Result<Vec<DoviRpu>, String> {
    let mut parser = HevcParser::default();
    let mut offsets = Vec::new();
    parser.get_offsets(stream, &mut offsets);

    let Some(last) = offsets.last().copied() else {
        return Ok(Vec::new());
    };

    let nals = parser
        .split_nals(stream, &offsets, last, false)
        .map_err(|e| format!("Failed to split the HEVC stream into NAL units: {e}"))?;

    nals.iter()
        .filter(|nal| nal.nal_type == NAL_UNSPEC62)
        .enumerate()
        .map(|(index, nal)| {
            DoviRpu::parse_unspec62_nalu(&stream[nal.start..nal.end])
                .map_err(|e| format!("Failed to parse the Dolby Vision RPU of frame {index}: {e}"))
        })
        .collect()
}

fn level6_block(dm_data: &[&VdrDmData]) -> Option<ExtMetadataBlockLevel6> {
    dm_data
        .iter()
        .find_map(|dm| match dm.get_block(DOLBY_VISION_LEVEL_6) {
            Some(ExtMetadataBlock::Level6(block)) => Some(block.clone()),
            _ => None,
        })
}

fn peak_level1_pq_code(dm_data: &[&VdrDmData]) -> Option<u16> {
    dm_data
        .iter()
        .filter_map(|dm| match dm.get_block(DOLBY_VISION_LEVEL_1) {
            Some(ExtMetadataBlock::Level1(block)) => Some(block.max_pq),
            _ => None,
        })
        .max()
}

fn light_level_nits(value: u16) -> Option<f32> {
    (value > 0).then(|| f32::from(value))
}

fn summarise_rpus(first: &DoviRpu, rpus: &[DoviRpu]) -> Result<DolbyVisionSummary, String> {
    let dm_data: Vec<&VdrDmData> = rpus
        .iter()
        .filter_map(|rpu| rpu.vdr_dm_data.as_ref())
        .collect();
    let Some(first_dm_data) = dm_data.first() else {
        return Err(format!(
            "The {} Dolby Vision RPUs carry no display management metadata",
            rpus.len()
        ));
    };

    let level6 = level6_block(&dm_data);
    let max_content_light_level_nits = level6
        .as_ref()
        .and_then(|block| light_level_nits(block.max_content_light_level));

    let peak_luminance_nits = match (max_content_light_level_nits, peak_level1_pq_code(&dm_data)) {
        (Some(nits), _) => nits,
        (None, Some(code)) => pq_code_to_nits(code),
        (None, None) => pq_code_to_nits(first_dm_data.source_max_pq),
    };

    Ok(DolbyVisionSummary {
        profile: first.dovi_profile,
        frames: rpus.len(),
        shots: dm_data
            .iter()
            .filter(|dm| dm.scene_refresh_flag > 0)
            .count(),
        max_content_light_level_nits,
        max_frame_average_light_level_nits: level6
            .as_ref()
            .and_then(|block| light_level_nits(block.max_frame_average_light_level)),
        peak_luminance_nits,
        mastering_display_max_nits: level6
            .as_ref()
            .map(|block| f32::from(block.max_display_mastering_luminance)),
        mastering_display_min_nits: level6.as_ref().map(|block| {
            f32::from(block.min_display_mastering_luminance)
                * MASTERING_DISPLAY_MIN_LUMINANCE_STEP_NITS
        }),
    })
}

pub fn read_dolby_vision(path: &Path) -> Result<Option<DolbyVisionSummary>, String> {
    if video_codec_name(path)? != HEVC_CODEC_NAME {
        return Ok(None);
    }

    let stream = demux_annex_b(path)?;
    let rpus = parse_rpu_nalus(&stream)?;

    let Some(first) = rpus.first() else {
        return Ok(None);
    };

    Ok(Some(summarise_rpus(first, &rpus)?))
}

// Dolby Vision Profiles and levels V1.2.92 table 3, the level a base layer fits
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DolbyVisionLevel {
    pub id: u8,
    pub name: &'static str,
    pub max_pixels_per_second: u64,
    pub main_tier_megabits_per_second: u32,
    pub high_tier_megabits_per_second: u32,
}

// table 3 gives a resolution times frame rate per level and says the product is
// the constant that bounds it
pub const DOLBY_VISION_LEVELS: [DolbyVisionLevel; 9] = [
    DolbyVisionLevel {
        id: 1,
        name: "hd24",
        max_pixels_per_second: 1280 * 720 * 24,
        main_tier_megabits_per_second: 20,
        high_tier_megabits_per_second: 50,
    },
    DolbyVisionLevel {
        id: 2,
        name: "hd30",
        max_pixels_per_second: 1280 * 720 * 30,
        main_tier_megabits_per_second: 20,
        high_tier_megabits_per_second: 50,
    },
    DolbyVisionLevel {
        id: 3,
        name: "fhd24",
        max_pixels_per_second: 1920 * 1080 * 24,
        main_tier_megabits_per_second: 20,
        high_tier_megabits_per_second: 70,
    },
    DolbyVisionLevel {
        id: 4,
        name: "fhd30",
        max_pixels_per_second: 1920 * 1080 * 30,
        main_tier_megabits_per_second: 20,
        high_tier_megabits_per_second: 70,
    },
    DolbyVisionLevel {
        id: 5,
        name: "fhd60",
        max_pixels_per_second: 1920 * 1080 * 60,
        main_tier_megabits_per_second: 20,
        high_tier_megabits_per_second: 70,
    },
    DolbyVisionLevel {
        id: 6,
        name: "uhd24",
        max_pixels_per_second: 3840 * 2160 * 24,
        main_tier_megabits_per_second: 25,
        high_tier_megabits_per_second: 130,
    },
    DolbyVisionLevel {
        id: 7,
        name: "uhd30",
        max_pixels_per_second: 3840 * 2160 * 30,
        main_tier_megabits_per_second: 25,
        high_tier_megabits_per_second: 130,
    },
    DolbyVisionLevel {
        id: 8,
        name: "uhd48",
        max_pixels_per_second: 3840 * 2160 * 48,
        main_tier_megabits_per_second: 40,
        high_tier_megabits_per_second: 130,
    },
    DolbyVisionLevel {
        id: 9,
        name: "uhd60",
        max_pixels_per_second: 3840 * 2160 * 60,
        main_tier_megabits_per_second: 40,
        high_tier_megabits_per_second: 130,
    },
];

// the lowest level that covers this raster and frame rate, None above uhd60
pub fn dolby_vision_level_for(
    width: u32,
    height: u32,
    frames_per_second: f64,
) -> Option<DolbyVisionLevel> {
    if width == 0 || height == 0 || frames_per_second <= 0.0 || frames_per_second.is_nan() {
        return None;
    }
    let pixels_per_second = (f64::from(width) * f64::from(height) * frames_per_second).ceil();
    DOLBY_VISION_LEVELS
        .into_iter()
        .find(|level| pixels_per_second <= level.max_pixels_per_second as f64)
}

// the base layer VUI as table 1 writes it: EOTF, primaries, matrix, range
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseLayerSignalling {
    pub transfer_characteristics: u8,
    pub colour_primaries: u8,
    pub matrix_coefficients: u8,
    pub full_range: bool,
}

// table 1 note: the cross-compatibility ID picking between these rows is not in
// the bitstream, so the base layer VUI is the only thing that says which it is
const IPT_BASE_LAYER: BaseLayerSignalling = BaseLayerSignalling {
    transfer_characteristics: 2,
    colour_primaries: 2,
    matrix_coefficients: 2,
    full_range: true,
};

const HDR10_BASE_LAYER: BaseLayerSignalling = BaseLayerSignalling {
    transfer_characteristics: 16,
    colour_primaries: 9,
    matrix_coefficients: 9,
    full_range: false,
};

const REC709_BASE_LAYER: BaseLayerSignalling = BaseLayerSignalling {
    transfer_characteristics: 1,
    colour_primaries: 1,
    matrix_coefficients: 1,
    full_range: false,
};

// cross-compatibility ID 4, HLG signalled as preferred_transfer_function 18
const HLG_BASE_LAYER: BaseLayerSignalling = BaseLayerSignalling {
    transfer_characteristics: 18,
    colour_primaries: 9,
    matrix_coefficients: 9,
    full_range: false,
};

// what table 1 allows a profile's base layer to carry, empty for a profile it
// does not list
pub fn allowed_base_layer_signalling(profile: u8) -> &'static [BaseLayerSignalling] {
    match profile {
        4 => &[REC709_BASE_LAYER],
        5 => &[IPT_BASE_LAYER],
        7 => &[HDR10_BASE_LAYER],
        8 => &[HDR10_BASE_LAYER, REC709_BASE_LAYER, HLG_BASE_LAYER],
        9 => &[REC709_BASE_LAYER],
        _ => &[],
    }
}

pub fn refuse_undecodable_dolby_vision(summary: &DolbyVisionSummary) -> Result<(), String> {
    if summary.profile == DOLBY_VISION_PROFILE_5 {
        return Err("Dolby Vision profile 5 carries IPT PQ c2 colour that only the RPU can turn back into RGB, export a profile 8.1 or an HDR10 master instead".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x265_params_carry_mastering_display_and_cll() {
        let m = Hdr10Metadata {
            display_primaries_rx: 34000,
            display_primaries_ry: 16000,
            display_primaries_gx: 13250,
            display_primaries_gy: 34500,
            display_primaries_bx: 7500,
            display_primaries_by: 3000,
            white_point_x: 15635,
            white_point_y: 16450,
            max_luminance: 10000000,
            min_luminance: 50,
            max_cll: 1000,
            max_fall: 400,
        };
        let p = x265_hdr10_params(&m);
        // real SEI signalling, not a copy-only container tag
        assert!(p.contains("hdr10=1"));
        assert!(p.contains("transfer=smpte2084"));
        assert!(p.contains(
            "master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50)"
        ));
        assert!(p.contains("max-cll=1000,400"));
    }

    #[test]
    fn pq_codes_convert_to_nits() {
        assert!((pq_code_to_nits(4095) - 10000.0).abs() < 0.5);
        // 2546 is the 12 bit PQ code for 299.6 cd/m²
        assert!((pq_code_to_nits(2546) - 299.6).abs() < 0.5);
    }

    #[test]
    fn profile_5_is_the_only_refused_profile() {
        let mut summary = DolbyVisionSummary {
            profile: 5,
            frames: 1,
            shots: 1,
            max_content_light_level_nits: None,
            max_frame_average_light_level_nits: None,
            peak_luminance_nits: 1000.0,
            mastering_display_max_nits: None,
            mastering_display_min_nits: None,
        };
        assert!(refuse_undecodable_dolby_vision(&summary).is_err());

        for profile in [4, 7, 8] {
            summary.profile = profile;
            assert!(refuse_undecodable_dolby_vision(&summary).is_ok());
        }
    }

    #[test]
    fn a_uhd24_master_fits_the_level_dolby_names_for_it() {
        let level = dolby_vision_level_for(3840, 2160, 24.0).expect("a level for uhd24");
        assert_eq!(level.name, "uhd24");
        assert_eq!(level.id, 6);
        assert_eq!(level.high_tier_megabits_per_second, 130);
    }

    // table 3 lists 1920x1080x25 under fhd30 rather than giving it a level
    #[test]
    fn a_1080p25_master_fits_fhd30() {
        let level = dolby_vision_level_for(1920, 1080, 25.0).expect("a level for 1080p25");
        assert_eq!(level.name, "fhd30");
    }

    #[test]
    fn a_master_past_uhd60_fits_no_dolby_level() {
        assert!(dolby_vision_level_for(3840, 2160, 120.0).is_none());
    }

    #[test]
    fn profile_8_takes_an_hdr10_base_layer_and_profile_5_does_not() {
        assert!(allowed_base_layer_signalling(8).contains(&HDR10_BASE_LAYER));
        assert!(!allowed_base_layer_signalling(5).contains(&HDR10_BASE_LAYER));
        assert_eq!(allowed_base_layer_signalling(5), &[IPT_BASE_LAYER]);
        assert!(allowed_base_layer_signalling(6).is_empty());
    }
}
