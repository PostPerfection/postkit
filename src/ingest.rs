use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Camera raw format identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CameraFormat {
    Arriraw,
    RedR3d,
    /// Sony X-OCN / Sony RAW. Not auto-detected: these are OP1a-MXF wrapped and
    /// share the .mxf extension and the SMPTE partition-pack key with ArriRaw,
    /// XAVC and DNxHR. Telling them apart needs MXF header-metadata parsing
    /// (essence descriptor / format id), which is out of scope here. Documented
    /// gap, not a shallow signature.
    SonyRaw,
    /// Canon Cinema RAW Light (.crm), detected by extension or by the ISOBMFF
    /// `ftyp` brand `crx ` plus a `CNCV` box starting with "CanonCRM".
    CanonRaw,
    BlackmagicBraw,
    ProRes,
    DnxHr,
    #[default]
    Unknown,
}

/// Ingest options for camera media.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestOptions {
    /// Camera card/media directory
    pub source: PathBuf,
    /// Destination for transcoded media
    pub output_dir: PathBuf,
    /// "dpx", "tiff", "exr", "prores"
    pub output_format: String,
    /// "ACES", "Rec.709", "P3", "LogC"
    pub colour_space: String,
    /// 1=fast, 3=high quality
    pub debayer_quality: u32,
    pub apply_lut: bool,
    pub lut_path: PathBuf,
    pub gpu_device: i32,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            source: PathBuf::new(),
            output_dir: PathBuf::new(),
            output_format: "dpx".to_string(),
            colour_space: "ACES".to_string(),
            debayer_quality: 3,
            apply_lut: false,
            lut_path: PathBuf::new(),
            gpu_device: -1,
        }
    }
}

/// Detected camera clip metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClipInfo {
    pub path: PathBuf,
    pub format: CameraFormat,
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
    pub frame_count: u32,
    pub codec: String,
    pub colour_space: String,
    pub camera_model: String,
    pub reel_name: String,
}

/// Detect camera format from directory/file extension.
pub fn detect_format(source: &Path) -> CameraFormat {
    // Check extension first (works for both existing and non-existing paths)
    if let Some(ext) = source
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
    {
        let fmt = match ext.as_str() {
            "ari" => CameraFormat::Arriraw,
            "r3d" => CameraFormat::RedR3d,
            "braw" => CameraFormat::BlackmagicBraw,
            "crm" => CameraFormat::CanonRaw,
            "mxf" => CameraFormat::DnxHr,
            "mov" => {
                if source.is_file() {
                    probe_mov_codec(source)
                } else {
                    CameraFormat::ProRes
                }
            }
            _ => CameraFormat::Unknown,
        };
        if fmt != CameraFormat::Unknown {
            return fmt;
        }
    }

    // Content sniff for a single file whose extension did not identify it.
    if source.is_file() && is_cinema_raw_light(source) {
        return CameraFormat::CanonRaw;
    }

    // Check directory for characteristic files
    if source.is_dir() {
        if has_files_with_ext(source, "ari") {
            return CameraFormat::Arriraw;
        }
        if has_files_with_ext(source, "r3d") {
            return CameraFormat::RedR3d;
        }
        if has_files_with_ext(source, "braw") {
            return CameraFormat::BlackmagicBraw;
        }
        if has_files_with_ext(source, "crm") {
            return CameraFormat::CanonRaw;
        }
    }

    CameraFormat::Unknown
}

/// Canon Cinema RAW Light (.crm) magic sniff.
///
/// CRM is an ISOBMFF container whose first box is `ftyp` with major brand
/// `crx ` (shared with Canon CR3 stills). The `CNCV` box value disambiguates
/// them: CRM starts with "CanonCRM", CR3 with "CanonCR3". We read a small
/// header window and require both the `crx ` brand and the "CanonCRM" marker.
fn is_cinema_raw_light(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 512];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    let head = &buf[..n];
    let is_crx = head.len() >= 12 && &head[4..8] == b"ftyp" && &head[8..12] == b"crx ";
    is_crx && head.windows(8).any(|w| w == b"CanonCRM")
}

fn has_files_with_ext(dir: &Path, ext: &str) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case(ext))
        })
}

fn probe_mov_codec(path: &Path) -> CameraFormat {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-print_format", "json", "-show_streams"])
        .arg(path)
        .output();

    let Ok(output) = output else {
        return CameraFormat::Unknown;
    };

    let json_str = String::from_utf8_lossy(&output.stdout);
    if json_str.contains("prores") {
        CameraFormat::ProRes
    } else if json_str.contains("dnxh") {
        CameraFormat::DnxHr
    } else {
        CameraFormat::Unknown
    }
}

/// Scan a camera card and return clip info using ffprobe.
pub fn scan_media(source: &Path) -> Vec<ClipInfo> {
    let mut clips = Vec::new();

    let entries: Vec<PathBuf> = if source.is_file() {
        vec![source.to_path_buf()]
    } else {
        std::fs::read_dir(source)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect()
    };

    for path in entries {
        let format = detect_format(&path);
        if format == CameraFormat::Unknown {
            continue;
        }

        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "quiet",
                "-print_format",
                "json",
                "-show_streams",
                "-show_format",
            ])
            .arg(&path)
            .output();

        let clip = if let Ok(output) = output {
            let json: serde_json::Value =
                serde_json::from_slice(&output.stdout).unwrap_or_default();

            let stream = json["streams"]
                .as_array()
                .and_then(|s| s.iter().find(|s| s["codec_type"] == "video"));

            let (width, height, fps, codec) = if let Some(s) = stream {
                let w = s["width"].as_u64().unwrap_or(0) as u32;
                let h = s["height"].as_u64().unwrap_or(0) as u32;
                let fps_str = s["r_frame_rate"].as_str().unwrap_or("24/1");
                let fps = parse_fraction(fps_str);
                let codec = s["codec_name"].as_str().unwrap_or("").to_string();
                (w, h, fps, codec)
            } else {
                (0, 0, 0.0, String::new())
            };

            let nb_frames = json["streams"]
                .as_array()
                .and_then(|s| s.first())
                .and_then(|s| s["nb_frames"].as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u32);

            ClipInfo {
                path: path.clone(),
                format,
                width,
                height,
                frame_rate: fps,
                frame_count: nb_frames,
                codec,
                reel_name: path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string(),
                ..Default::default()
            }
        } else {
            ClipInfo {
                path: path.clone(),
                format,
                ..Default::default()
            }
        };

        clips.push(clip);
    }

    clips
}

fn parse_fraction(s: &str) -> f64 {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2 {
        let num: f64 = parts[0].parse().unwrap_or(0.0);
        let den: f64 = parts[1].parse().unwrap_or(1.0);
        if den > 0.0 { num / den } else { 0.0 }
    } else {
        s.parse().unwrap_or(0.0)
    }
}

/// Ingest/transcode camera media to standardized intermediate using ffmpeg.
pub fn ingest(opts: &IngestOptions) -> i32 {
    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        tracing::error!("Failed to create output directory: {e}");
        return -1;
    }

    let clips = scan_media(&opts.source);
    if clips.is_empty() {
        tracing::error!("No media clips found in {}", opts.source.display());
        return -1;
    }

    let output_ext = match opts.output_format.as_str() {
        "dpx" => "dpx",
        "tiff" | "tif" => "tif",
        "exr" => "exr",
        "png" => "png",
        "prores" => "mov",
        _ => "dpx",
    };

    for clip in &clips {
        let clip_out_dir = opts.output_dir.join(&clip.reel_name);
        if let Err(e) = std::fs::create_dir_all(&clip_out_dir) {
            tracing::error!("Failed to create clip output dir: {e}");
            return -1;
        }

        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.arg("-y").arg("-i").arg(&clip.path);

        if opts.apply_lut && opts.lut_path.exists() {
            cmd.arg("-vf")
                .arg(format!("lut3d={}", opts.lut_path.display()));
        }

        if output_ext == "mov" {
            cmd.arg("-c:v")
                .arg("prores_ks")
                .arg("-profile:v")
                .arg("4444");
            cmd.arg(clip_out_dir.join(format!("{}.mov", clip.reel_name)));
        } else {
            cmd.arg(clip_out_dir.join(format!("%06d.{output_ext}")));
        }

        let output = cmd.output();
        match output {
            Ok(o) if o.status.success() => {
                tracing::info!("Ingested {} → {}", clip.reel_name, clip_out_dir.display());
            }
            Ok(o) => {
                tracing::error!(
                    "Failed to ingest {}: {}",
                    clip.reel_name,
                    String::from_utf8_lossy(&o.stderr)
                );
                return -1;
            }
            Err(e) => {
                tracing::error!("Failed to run ffmpeg: {e}");
                return -1;
            }
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_format_file() {
        assert_eq!(detect_format(Path::new("clip.ari")), CameraFormat::Arriraw);
        assert_eq!(detect_format(Path::new("clip.r3d")), CameraFormat::RedR3d);
        assert_eq!(
            detect_format(Path::new("clip.braw")),
            CameraFormat::BlackmagicBraw
        );
        assert_eq!(detect_format(Path::new("clip.txt")), CameraFormat::Unknown);
    }

    #[test]
    fn test_detect_canon_crm_extension() {
        assert_eq!(
            detect_format(Path::new("A001C001.crm")),
            CameraFormat::CanonRaw
        );
        assert_eq!(
            detect_format(Path::new("A001C001.CRM")),
            CameraFormat::CanonRaw
        );
    }

    fn crx_header(cncv: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        // ftyp box: size, "ftyp", major brand "crx ", minor version, compat brands
        data.extend_from_slice(&[0, 0, 0, 0x14]);
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(b"crx ");
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(b"crx ");
        // CNCV box: size, "CNCV", codec-version string
        data.extend_from_slice(&[0, 0, 0, 0x2c]);
        data.extend_from_slice(b"CNCV");
        data.extend_from_slice(cncv);
        data
    }

    #[test]
    fn test_detect_canon_crm_magic_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("clip.bin");
        std::fs::write(&p, crx_header(b"CanonCRM0001/02.09.00/00.00.00")).unwrap();
        assert_eq!(detect_format(&p), CameraFormat::CanonRaw);
    }

    #[test]
    fn test_cr3_stills_not_detected_as_canon_raw() {
        // Same crx container brand, but CR3 stills, not Cinema RAW Light.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("photo.bin");
        std::fs::write(&p, crx_header(b"CanonCR3_001/00.09.00/00.00.00")).unwrap();
        assert_eq!(detect_format(&p), CameraFormat::Unknown);
    }

    #[test]
    fn test_parse_fraction() {
        assert!((parse_fraction("24000/1001") - 23.976).abs() < 0.01);
        assert!((parse_fraction("25/1") - 25.0).abs() < 0.01);
    }
}
