use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Camera raw format identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CameraFormat {
    Arriraw,
    RedR3d,
    /// Sony RAW (X-OCN family). OP1a-MXF wrapped, sharing the .mxf extension and
    /// the SMPTE partition-pack key with ArriRaw, XAVC and DNxHR. Detected by
    /// matching Sony's private essence ULs in the MXF header (see
    /// `is_sony_raw_mxf`); the ULs are reverse-engineered (MediaInfo) and mark
    /// the Sony RAW family without distinguishing the X-OCN ST/LT/XT tiers.
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

impl CameraFormat {
    /// Human-readable name for logs and error messages.
    pub fn label(self) -> &'static str {
        match self {
            CameraFormat::Arriraw => "ARRIRAW",
            CameraFormat::RedR3d => "RED R3D",
            CameraFormat::SonyRaw => "Sony RAW (X-OCN family)",
            CameraFormat::CanonRaw => "Canon Cinema RAW Light",
            CameraFormat::BlackmagicBraw => "Blackmagic BRAW",
            CameraFormat::ProRes => "ProRes",
            CameraFormat::DnxHr => "DNxHR",
            CameraFormat::Unknown => "unknown",
        }
    }
}

/// Ingest options for camera media.
///
/// `colour_space`, `debayer_quality` and `gpu_device` describe RAW debayer intent
/// but stock ffmpeg cannot debayer camera RAW, so they are only meaningful once a
/// vendor decoder is wired in. Today ingest transcodes ffmpeg-decodable inputs
/// (ProRes, DNxHR) and rejects true RAW loudly; those fields are not applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestOptions {
    /// Camera card/media directory
    pub source: PathBuf,
    /// Destination for transcoded media
    pub output_dir: PathBuf,
    /// "dpx", "tiff", "exr", "prores"
    pub output_format: String,
    /// "ACES", "Rec.709", "P3", "LogC" (not applied, see struct note)
    pub colour_space: String,
    /// 1=fast, 3=high quality (not applied, see struct note)
    pub debayer_quality: u32,
    pub apply_lut: bool,
    pub lut_path: PathBuf,
    /// (not applied, see struct note)
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
            "mxf" => {
                if source.is_file() && is_sony_raw_mxf(source) {
                    CameraFormat::SonyRaw
                } else {
                    CameraFormat::DnxHr
                }
            }
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

// reverse-engineered Sony RAW / X-OCN essence ULs, from MediaInfo
// (MediaArea/MediaInfoLib Source/MediaInfo/Multiple/File_Mxf.cpp, "Sony RAW SQ").
// they sit under Sony's private node (06 0e 2b 34 ... 0e 06 ...), are not
// SMPTE-registered, and mark the Sony RAW family (X-OCN + older linear RAW)
// without distinguishing the X-OCN ST/LT/XT tiers. byte 7 is the UL registry
// version and varies between files, so it is wildcarded in both matchers.

/// PictureEssenceCoding value (File_Mxf.cpp Mxf_EssenceCompression, "Sony RAW SQ").
fn is_sony_raw_pict_coding(ul: &[u8]) -> bool {
    ul.len() >= 16
        && ul[..7] == [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01]
        && ul[8..15] == [0x0e, 0x06, 0x04, 0x01, 0x02, 0x04, 0x02]
}

/// EssenceContainer label (File_Mxf.cpp Mxf_EssenceContainer, "Sony RAW?").
fn is_sony_raw_container(ul: &[u8]) -> bool {
    ul.len() >= 16
        && ul[..7] == [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01]
        && ul[8..14] == [0x0e, 0x06, 0x0d, 0x03, 0x02, 0x01]
}

fn is_mxf_partition_key(k: &[u8]) -> bool {
    k.len() >= 16
        && k[..13]
            == [
                0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01,
            ]
        // header / body / footer partition
        && matches!(k[13], 0x02..=0x04)
}

/// BER length decode. returns (length, bytes_consumed).
fn read_ber_len(d: &[u8]) -> Option<(usize, usize)> {
    let first = *d.first()?;
    if first < 0x80 {
        return Some((first as usize, 1));
    }
    let n = (first & 0x7f) as usize;
    if n == 0 || n > 8 || d.len() < 1 + n {
        return None;
    }
    let mut len = 0usize;
    for &b in &d[1..1 + n] {
        len = (len << 8) | b as usize;
    }
    Some((len, 1 + n))
}

fn be_u32(d: &[u8]) -> u32 {
    u32::from_be_bytes([d[0], d[1], d[2], d[3]])
}

fn be_u64(d: &[u8]) -> u64 {
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// Detect the Sony RAW / X-OCN family in an OP1a-MXF file.
///
/// X-OCN and older Sony linear RAW share the .mxf extension and partition-pack
/// key with ArriRaw / XAVC / DNxHR, so extension alone can't tell them apart.
/// We parse the header partition pack's EssenceContainers batch and scan the
/// header-metadata region for Sony's private essence ULs (see the UL note
/// above). The scan is bounded to HeaderByteCount so essence data can't produce
/// a false positive. A match only yields a clearer detected-but-undecodable
/// error, since postkit rejects Sony RAW either way.
fn is_sony_raw_mxf(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    // partition pack + primer + header metadata sit at the top of the file;
    // 1 MiB covers the descriptor region of any real camera-raw MXF.
    let mut buf = vec![0u8; 1 << 20];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    buf.truncate(n);

    // first KLV must be a partition pack
    if !is_mxf_partition_key(&buf) {
        return false;
    }
    let Some((val_len, hdr)) = read_ber_len(&buf[16..]) else {
        return false;
    };
    let val_start = 16 + hdr;
    let val = &buf[val_start..(val_start + val_len).min(buf.len())];

    // partition-pack fixed fields, in order: MajorVersion(2) MinorVersion(2)
    // KAGSize(4) ThisPartition(8) PreviousPartition(8) FooterPartition(8)
    // HeaderByteCount(8) IndexByteCount(8) IndexSID(4) BodyOffset(8) BodySID(4)
    // OperationalPattern(16), then the EssenceContainers batch.
    let header_byte_count = val.get(32..40).map(be_u64).unwrap_or(0) as usize;
    const BATCH_OFF: usize = 80;
    if let Some(batch) = val.get(BATCH_OFF..)
        && batch.len() >= 8
    {
        let count = be_u32(batch) as usize;
        let item = be_u32(&batch[4..]) as usize;
        if item == 16 {
            for i in 0..count {
                let off = 8 + i * 16;
                if let Some(ul) = batch.get(off..off + 16)
                    && is_sony_raw_container(ul)
                {
                    return true;
                }
            }
        }
    }

    // scan the header-metadata region for the PictureEssenceCoding UL.
    let meta_start = val_start + val_len;
    let meta_end = if header_byte_count > 0 {
        (meta_start + header_byte_count).min(buf.len())
    } else {
        buf.len()
    };
    if let Some(meta) = buf.get(meta_start..meta_end) {
        return meta.windows(16).any(is_sony_raw_pict_coding);
    }
    false
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

/// True for camera RAW formats stock ffmpeg cannot decode. Decoding these needs
/// the vendor SDK (ARRI/RED/Blackmagic/Canon) or a dedicated debayer library.
pub fn is_raw_undecodable(format: CameraFormat) -> bool {
    matches!(
        format,
        CameraFormat::Arriraw
            | CameraFormat::RedR3d
            | CameraFormat::BlackmagicBraw
            | CameraFormat::CanonRaw
            | CameraFormat::SonyRaw
    )
}

/// Ingest/transcode camera media to standardized intermediate using ffmpeg.
///
/// Only ffmpeg-decodable inputs (ProRes, DNxHR) are transcoded. Camera RAW
/// (ARRIRAW, R3D, BRAW, Cinema RAW Light, X-OCN) is detected but rejected: stock
/// ffmpeg cannot decode it. See `is_raw_undecodable`.
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

    // Fail loud on RAW rather than handing an undecodable file to ffmpeg and
    // letting it die with a cryptic decoder error.
    if let Some(clip) = clips.iter().find(|c| is_raw_undecodable(c.format)) {
        tracing::error!(
            "Cannot ingest {}: {} is camera RAW that stock ffmpeg cannot decode; \
             a vendor SDK/decoder is required",
            clip.path.display(),
            clip.format.label()
        );
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

    #[test]
    fn raw_formats_are_flagged_undecodable() {
        for f in [
            CameraFormat::Arriraw,
            CameraFormat::RedR3d,
            CameraFormat::BlackmagicBraw,
            CameraFormat::CanonRaw,
            CameraFormat::SonyRaw,
        ] {
            assert!(is_raw_undecodable(f), "{f:?} should be undecodable");
        }
        for f in [CameraFormat::ProRes, CameraFormat::DnxHr] {
            assert!(!is_raw_undecodable(f), "{f:?} is ffmpeg-decodable");
        }
    }

    // Sony private ULs (byte 7 is a made-up registry version to exercise the
    // wildcard); JPEG2000 ULs stand in for a non-Sony .mxf.
    const SONY_CONTAINER: [u8; 16] = [
        0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x0a, 0x0e, 0x06, 0x0d, 0x03, 0x02, 0x01, 0x01,
        0x00,
    ];
    const SONY_PICT_CODING: [u8; 16] = [
        0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x0a, 0x0e, 0x06, 0x04, 0x01, 0x02, 0x04, 0x02,
        0x01,
    ];
    const J2K_CONTAINER: [u8; 16] = [
        0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x07, 0x0d, 0x01, 0x03, 0x01, 0x02, 0x0c, 0x01,
        0x00,
    ];
    const J2K_PICT_CODING: [u8; 16] = [
        0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x07, 0x04, 0x01, 0x02, 0x02, 0x03, 0x01, 0x01,
        0x00,
    ];

    // Minimal OP1a-MXF: header partition pack (with a one-entry EssenceContainers
    // batch) followed by a header-metadata blob carrying the picture-coding UL.
    fn build_mxf(container: &[u8; 16], pict_coding: &[u8; 16]) -> Vec<u8> {
        let mut meta = Vec::new();
        meta.extend_from_slice(&[0xAA; 8]);
        meta.extend_from_slice(pict_coding);
        meta.extend_from_slice(&[0xBB; 8]);

        let op1a: [u8; 16] = [
            0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x01,
            0x09, 0x00,
        ];
        let mut val = Vec::new();
        val.extend_from_slice(&1u16.to_be_bytes()); // MajorVersion
        val.extend_from_slice(&2u16.to_be_bytes()); // MinorVersion
        val.extend_from_slice(&0u32.to_be_bytes()); // KAGSize
        val.extend_from_slice(&0u64.to_be_bytes()); // ThisPartition
        val.extend_from_slice(&0u64.to_be_bytes()); // PreviousPartition
        val.extend_from_slice(&0u64.to_be_bytes()); // FooterPartition
        val.extend_from_slice(&(meta.len() as u64).to_be_bytes()); // HeaderByteCount
        val.extend_from_slice(&0u64.to_be_bytes()); // IndexByteCount
        val.extend_from_slice(&0u32.to_be_bytes()); // IndexSID
        val.extend_from_slice(&0u64.to_be_bytes()); // BodyOffset
        val.extend_from_slice(&0u32.to_be_bytes()); // BodySID
        val.extend_from_slice(&op1a);
        val.extend_from_slice(&1u32.to_be_bytes()); // batch count
        val.extend_from_slice(&16u32.to_be_bytes()); // batch item length
        val.extend_from_slice(container);
        assert!(val.len() < 0x80);

        let mut out = Vec::new();
        out.extend_from_slice(&[
            0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x02,
            0x04, 0x00,
        ]);
        out.push(val.len() as u8); // single-byte BER length
        out.extend_from_slice(&val);
        out.extend_from_slice(&meta);
        out
    }

    fn write_mxf(container: &[u8; 16], pict_coding: &[u8; 16]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("clip.mxf");
        std::fs::write(&p, build_mxf(container, pict_coding)).unwrap();
        (dir, p)
    }

    #[test]
    fn detect_sony_raw_mxf_via_essence_container() {
        let (_d, p) = write_mxf(&SONY_CONTAINER, &J2K_PICT_CODING);
        assert!(is_sony_raw_mxf(&p));
        assert_eq!(detect_format(&p), CameraFormat::SonyRaw);
    }

    #[test]
    fn detect_sony_raw_mxf_via_picture_essence_coding() {
        let (_d, p) = write_mxf(&J2K_CONTAINER, &SONY_PICT_CODING);
        assert!(is_sony_raw_mxf(&p));
        assert_eq!(detect_format(&p), CameraFormat::SonyRaw);
    }

    #[test]
    fn non_sony_mxf_is_not_sony_raw() {
        let (_d, p) = write_mxf(&J2K_CONTAINER, &J2K_PICT_CODING);
        assert!(!is_sony_raw_mxf(&p));
        // a non-Sony .mxf still resolves by extension to DNxHR, as before
        assert_eq!(detect_format(&p), CameraFormat::DnxHr);
    }

    #[test]
    fn ingest_rejects_raw_clips_loudly() {
        // a directory holding a .r3d makes scan_media report a RAW clip; ingest
        // must refuse rather than feed it to ffmpeg
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("A001.r3d"), b"not really r3d").unwrap();
        let out = tempfile::tempdir().unwrap();

        let opts = IngestOptions {
            source: src.path().to_path_buf(),
            output_dir: out.path().to_path_buf(),
            ..Default::default()
        };
        assert_eq!(ingest(&opts), -1);
    }
}
