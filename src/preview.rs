//! Media preview.
//!
//! Two paths live here.
//!
//! `PlaybackOptions` / [`play`] is a plain ffplay wrapper over a decodable
//! media file: no CPL selection, no MXF decode, no colour transform. It reads
//! the file's real frame rate and honours start/end/loop.
//!
//! The DCP-native path ([`render_dcp_frame`], [`play_dcp`]) resolves a DCP
//! directory / CPL / picture MXF to its picture essence, decrypts it in Rust
//! when it is encrypted (asdcplib-rs `AesDecContext`, key from a dcpwizard
//! `KEYS.json` or a raw hex key), decodes the JPEG 2000 codestream with ffmpeg
//! to raw X'Y'Z' code values, and colour-manages those to an sRGB monitor via
//! [`crate::colour::XyzToSrgb`] (the inverse of the DCDM encode). An optional
//! monitor ICC profile routes the transform through littleCMS instead (the
//! `icc` cargo feature). Encrypted essence with no key fails loud rather than
//! showing garbage.
//!
//! This is a correct decoded-and-colour-managed preview, not a real-time
//! projector-grade player: each frame decodes on the CPU and a range plays
//! back from a colour-managed intermediate, so speed is not the point.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Media preview options.
///
/// Playback is an ffplay wrapper over a decodable file, not a DCP/IMF-native
/// player: it cannot select a CPL, decode encrypted MXF, drive a GPU decoder or
/// apply a DCI display transform. `play` reads the file's real frame rate and
/// honours `start_frame`/`end_frame`/`loop_playback`, and errors when an option
/// it cannot fulfil (`cpl_uuid`, `gpu_device`, a non-sRGB `display_colourspace`)
/// is set, rather than ignoring it silently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackOptions {
    pub input: PathBuf,
    pub cpl_uuid: String,
    pub start_frame: u32,
    /// 0 = play to end
    pub end_frame: u32,
    pub loop_playback: bool,
    pub decode_to_display: bool,
    pub display_colourspace: String,
    pub gpu_device: i32,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            cpl_uuid: String::new(),
            start_frame: 0,
            end_frame: 0,
            loop_playback: false,
            decode_to_display: true,
            display_colourspace: "sRGB".to_string(),
            gpu_device: -1,
        }
    }
}

/// Frame metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameInfo {
    pub frame_number: u32,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
    pub codec: String,
}

/// Read the video frame rate via ffprobe, falling back to 24 fps when it can't
/// be determined.
pub fn read_frame_rate(input: &Path) -> f64 {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate",
            "-of",
            "default=nokey=1:noprint_wrappers=1",
        ])
        .arg(input)
        .output();

    let Ok(output) = output else {
        return 24.0;
    };
    let s = String::from_utf8_lossy(&output.stdout);
    let s = s.trim();
    let fps = if let Some((num, den)) = s.split_once('/') {
        let n: f64 = num.parse().unwrap_or(0.0);
        let d: f64 = den.parse().unwrap_or(0.0);
        if d > 0.0 { n / d } else { 0.0 }
    } else {
        s.parse().unwrap_or(0.0)
    };
    if fps > 0.0 { fps } else { 24.0 }
}

/// Extract a single frame as image (thumbnail/QC) using ffmpeg.
pub fn extract_frame(input: &Path, frame: u32, output_image: &Path) -> i32 {
    // Seek by the file's real frame rate, not a hardcoded 24 fps.
    let seconds = frame as f64 / read_frame_rate(input);

    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-ss")
        .arg(format!("{seconds:.3}"))
        .arg("-frames:v")
        .arg("1")
        .arg(output_image)
        .output();

    match output {
        Ok(o) if o.status.success() => 0,
        Ok(o) => {
            tracing::error!(
                "Frame extraction failed: {}",
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

/// Get frame metadata without full decode using ffprobe.
pub fn get_frame_info(input: &Path, frame: u32) -> FrameInfo {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-print_format", "json", "-show_streams"])
        .arg(input)
        .output();

    let Ok(output) = output else {
        return FrameInfo::default();
    };

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();

    let stream = json["streams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["codec_type"] == "video"));

    if let Some(s) = stream {
        FrameInfo {
            frame_number: frame,
            width: s["width"].as_u64().unwrap_or(0) as u32,
            height: s["height"].as_u64().unwrap_or(0) as u32,
            bitrate_kbps: s["bit_rate"]
                .as_str()
                .and_then(|b| b.parse::<u64>().ok())
                .map(|b| (b / 1000) as u32)
                .unwrap_or(0),
            codec: s["codec_name"].as_str().unwrap_or("").to_string(),
        }
    } else {
        FrameInfo {
            frame_number: frame,
            ..Default::default()
        }
    }
}

/// Start playback using ffplay (blocking).
///
/// Errors out (returns -1) on options the ffplay path cannot honour instead of
/// silently ignoring them. See `PlaybackOptions`.
pub fn play(opts: &PlaybackOptions) -> i32 {
    if !opts.cpl_uuid.is_empty() {
        tracing::error!("cpl_uuid is set but ffplay plays a file, not a CPL selection");
        return -1;
    }
    if opts.gpu_device >= 0 {
        tracing::error!("gpu_device is set but the ffplay path has no GPU decode");
        return -1;
    }
    if !opts.display_colourspace.eq_ignore_ascii_case("sRGB") {
        tracing::error!(
            "display_colourspace {:?} is unsupported; the ffplay path shows the file as-is",
            opts.display_colourspace
        );
        return -1;
    }

    let fps = read_frame_rate(&opts.input);
    let mut cmd = std::process::Command::new("ffplay");
    cmd.arg("-autoexit").arg(&opts.input);

    if opts.start_frame > 0 {
        let seconds = opts.start_frame as f64 / fps;
        cmd.arg("-ss").arg(format!("{seconds:.3}"));
    }

    // end_frame (0 = play to end) becomes a play duration from the start frame.
    if opts.end_frame > opts.start_frame {
        let seconds = (opts.end_frame - opts.start_frame) as f64 / fps;
        cmd.arg("-t").arg(format!("{seconds:.3}"));
    }

    if opts.loop_playback {
        cmd.arg("-loop").arg("0");
    }

    match cmd.status() {
        Ok(s) if s.success() => 0,
        Ok(_) => -1,
        Err(e) => {
            tracing::error!("Failed to run ffplay: {e}");
            -1
        }
    }
}

/// Render all frames to image sequence using ffmpeg.
pub fn render_to_sequence(input: &Path, output_dir: &Path, format: Option<&str>) -> i32 {
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        tracing::error!("Failed to create output directory: {e}");
        return -1;
    }

    let ext = format.unwrap_or("png");
    let output_pattern = output_dir.join(format!("frame_%06d.{ext}"));

    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg(&output_pattern)
        .output();

    match output {
        Ok(o) if o.status.success() => 0,
        Ok(o) => {
            tracing::error!("Render failed: {}", String::from_utf8_lossy(&o.stderr));
            -1
        }
        Err(e) => {
            tracing::error!("Failed to run ffmpeg: {e}");
            -1
        }
    }
}

// ─── DCP-native preview: resolve → decrypt → decode → colour-manage ────────

use crate::colour::{RenderingIntent, XyzToSrgb};
use asdcplib::crypto::AesDecContext;
use asdcplib::jp2k::MxfReader;
use std::io::Write as _;

/// Largest picture frame we read into. DCI caps a 4K frame at 500 Mbps / 24 fps
/// (~2.6 MB); 8 MB leaves comfortable headroom.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("could not resolve picture essence: {0}")]
    Resolve(String),
    #[error("picture essence is encrypted but no key was supplied ({0})")]
    MissingKey(String),
    #[error("invalid key: {0}")]
    BadKey(String),
    #[error("mxf error: {0}")]
    Mxf(String),
    #[error("j2k decode failed: {0}")]
    Decode(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// DCP picture preview options.
#[derive(Debug, Clone, Default)]
pub struct DcpPreviewOptions {
    /// DCP directory, CPL XML, or picture MXF.
    pub source: PathBuf,
    /// Raw 16-byte AES-128 content key for the picture essence, if encrypted.
    pub key: Option<[u8; 16]>,
    /// Optional monitor ICC profile. Requires the `icc` cargo feature; without
    /// it, a set profile is an error rather than being silently ignored.
    pub display_profile: Option<PathBuf>,
    pub intent: RenderingIntent,
    pub start_frame: u32,
    /// 0 = to end
    pub end_frame: u32,
    pub loop_playback: bool,
}

/// The resolved picture essence and its properties.
#[derive(Debug, Clone)]
pub struct ResolvedPicture {
    pub mxf: PathBuf,
    pub encrypted: bool,
    pub frame_count: u32,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// Parse a raw AES-128 content key from a 32-char hex string.
pub fn key_from_hex(s: &str) -> Result<[u8; 16], PreviewError> {
    let bytes = hex::decode(s.trim()).map_err(|e| PreviewError::BadKey(format!("not hex: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| PreviewError::BadKey("key must be 16 bytes (32 hex chars)".into()))
}

/// Load the picture content key from a dcpwizard `KEYS.json`.
///
/// Shape: `{ "cpl_id": "...", "keys": [ { "key_type": "Mdik", "key_id": "...",
/// "asset_uuid": "...", "content_key_hex": "..." }, ... ] }`. Picks the image
/// key (`Mdik`), matching `asset_uuid` when one is given.
pub fn key_from_keys_json(path: &Path, asset_uuid: Option<&str>) -> Result<[u8; 16], PreviewError> {
    let text = std::fs::read_to_string(path)?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| PreviewError::BadKey(format!("KEYS.json is not valid JSON: {e}")))?;
    let keys = json["keys"]
        .as_array()
        .ok_or_else(|| PreviewError::BadKey("KEYS.json has no `keys` array".into()))?;

    let pick = keys
        .iter()
        .find(|k| {
            k["key_type"].as_str() == Some("Mdik")
                && asset_uuid.is_none_or(|u| k["asset_uuid"].as_str() == Some(u))
        })
        .or_else(|| keys.iter().find(|k| k["key_type"].as_str() == Some("Mdik")))
        .ok_or_else(|| PreviewError::BadKey("no image key (Mdik) in KEYS.json".into()))?;

    let hex = pick["content_key_hex"]
        .as_str()
        .ok_or_else(|| PreviewError::BadKey("key entry has no content_key_hex".into()))?;
    key_from_hex(hex)
}

/// Resolve a DCP directory, CPL XML, or picture MXF to the picture essence.
pub fn resolve_picture(source: &Path) -> Result<ResolvedPicture, PreviewError> {
    let mxf = find_picture_mxf(source)?;
    let mxf_str = mxf.to_string_lossy().to_string();

    let mut reader = MxfReader::new();
    reader
        .open_read(&mxf_str)
        .map_err(|e| PreviewError::Mxf(format!("open {}: {e}", mxf.display())))?;
    let info = reader
        .writer_info()
        .map_err(|e| PreviewError::Mxf(format!("writer info: {e}")))?;
    let desc = reader
        .picture_descriptor()
        .map_err(|e| PreviewError::Mxf(format!("picture descriptor: {e}")))?;
    let _ = reader.close();

    let fps = if desc.edit_rate.denominator != 0 {
        desc.edit_rate.numerator as f64 / desc.edit_rate.denominator as f64
    } else {
        24.0
    };

    Ok(ResolvedPicture {
        mxf,
        encrypted: info.encrypted_essence,
        frame_count: desc.container_duration,
        width: desc.stored_width,
        height: desc.stored_height,
        fps,
    })
}

/// Find the picture MXF from a DCP dir, a CPL XML, or an MXF path.
fn find_picture_mxf(source: &Path) -> Result<PathBuf, PreviewError> {
    if source.is_file() {
        match source.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("mxf") => return Ok(source.to_path_buf()),
            _ => return resolve_via_cpl(source),
        }
    }
    if source.is_dir() {
        let cpl = find_cpl_in_dir(source)?;
        return resolve_via_cpl(&cpl);
    }
    Err(PreviewError::Resolve(format!(
        "{} is not a file or directory",
        source.display()
    )))
}

/// A DCP directory's first CPL: the ASSETMAP asset whose XML is a CPL.
fn find_cpl_in_dir(dir: &Path) -> Result<PathBuf, PreviewError> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
        {
            let head = read_head(&path, 4096);
            if head.contains("CompositionPlaylist") {
                return Ok(path);
            }
        }
    }
    Err(PreviewError::Resolve(format!(
        "no CPL (CompositionPlaylist XML) found in {}",
        dir.display()
    )))
}

/// Resolve a CPL's first MainPicture UUID to a file via the sibling ASSETMAP.
fn resolve_via_cpl(cpl: &Path) -> Result<PathBuf, PreviewError> {
    let text = std::fs::read_to_string(cpl)?;
    let picture_uuid = first_main_picture_id(&text)
        .ok_or_else(|| PreviewError::Resolve(format!("no MainPicture in {}", cpl.display())))?;

    let dir = cpl.parent().unwrap_or(Path::new("."));
    let assetmap = find_assetmap(dir)?;
    let map = parse_assetmap(&assetmap)?;
    let rel = map
        .get(&picture_uuid)
        .ok_or_else(|| PreviewError::Resolve(format!("asset {picture_uuid} not in ASSETMAP")))?;
    Ok(dir.join(rel))
}

fn find_assetmap(dir: &Path) -> Result<PathBuf, PreviewError> {
    for name in ["ASSETMAP.xml", "ASSETMAP"] {
        let p = dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(PreviewError::Resolve(format!(
        "no ASSETMAP in {}",
        dir.display()
    )))
}

/// Bare UUID of the first `<MainPicture>` (any namespace prefix) in a CPL.
fn first_main_picture_id(cpl: &str) -> Option<String> {
    let re = regex::Regex::new(
        r"(?s)<(?:\w+:)?MainPicture\b.*?<(?:\w+:)?Id>\s*(?:urn:uuid:)?([0-9a-fA-F-]{36})",
    )
    .ok()?;
    re.captures(cpl).map(|c| c[1].to_ascii_lowercase())
}

/// Map bare asset UUID → relative path from an ASSETMAP.
fn parse_assetmap(path: &Path) -> Result<std::collections::HashMap<String, String>, PreviewError> {
    let text = std::fs::read_to_string(path)?;
    let re = regex::Regex::new(
        r"(?s)<(?:\w+:)?Asset\b.*?<(?:\w+:)?Id>\s*(?:urn:uuid:)?([0-9a-fA-F-]{36})\s*</(?:\w+:)?Id>.*?<(?:\w+:)?Path>\s*([^<]+?)\s*</(?:\w+:)?Path>",
    )
    .map_err(|e| PreviewError::Resolve(format!("assetmap regex: {e}")))?;
    let mut map = std::collections::HashMap::new();
    for c in re.captures_iter(&text) {
        map.insert(c[1].to_ascii_lowercase(), c[2].to_string());
    }
    if map.is_empty() {
        return Err(PreviewError::Resolve(format!(
            "no assets parsed from {}",
            path.display()
        )));
    }
    Ok(map)
}

fn read_head(path: &Path, n: usize) -> String {
    use std::io::Read as _;
    let mut buf = vec![0u8; n];
    let read = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap_or(0);
    String::from_utf8_lossy(&buf[..read]).into_owned()
}

/// Build an AES decryption context for an encrypted essence, failing loud when
/// no key is available.
fn dec_context(
    resolved: &ResolvedPicture,
    key: Option<[u8; 16]>,
) -> Result<Option<AesDecContext>, PreviewError> {
    if !resolved.encrypted {
        return Ok(None);
    }
    let key = key.ok_or_else(|| PreviewError::MissingKey(resolved.mxf.display().to_string()))?;
    let mut ctx = AesDecContext::new();
    ctx.init_key(&key)
        .map_err(|e| PreviewError::BadKey(format!("init_key: {e}")))?;
    Ok(Some(ctx))
}

/// Read one picture frame's JPEG 2000 codestream, decrypting if a context is set.
fn read_j2c_frame(
    reader: &mut MxfReader,
    frame: u32,
    dec: Option<&mut AesDecContext>,
) -> Result<Vec<u8>, PreviewError> {
    let mut buf = vec![0u8; MAX_FRAME_BYTES];
    let size = reader
        .read_frame(frame, &mut buf, dec, None)
        .map_err(|e| PreviewError::Mxf(format!("read frame {frame}: {e}")))?;
    buf.truncate(size);
    Ok(buf)
}

/// Decode a JPEG 2000 codestream to raw `xyz12le` (12-bit X'Y'Z' code values)
/// via ffmpeg. Requesting `xyz12le` output keeps ffmpeg from applying its own
/// XYZ→RGB conversion, so we get the untouched component code values.
fn decode_j2c_to_xyz12le(j2c: Vec<u8>) -> Result<Vec<u8>, PreviewError> {
    let mut child = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "j2k_pipe",
            "-i",
            "pipe:0",
            "-pix_fmt",
            "xyz12le",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| PreviewError::Decode(format!("spawn ffmpeg: {e}")))?;

    // write stdin from a thread so a full stdout pipe cannot deadlock the write
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&j2c);
        drop(stdin);
    });

    let out = child
        .wait_with_output()
        .map_err(|e| PreviewError::Decode(format!("ffmpeg wait: {e}")))?;
    let _ = writer.join();

    if !out.status.success() {
        return Err(PreviewError::Decode(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    if out.stdout.is_empty() {
        return Err(PreviewError::Decode("ffmpeg produced no pixels".into()));
    }
    Ok(out.stdout)
}

/// A decoded, colour-managed frame as packed 8-bit RGB.
struct Rgb8Frame {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

/// One display transform, chosen from the options: built-in sRGB, or an ICC
/// monitor profile when one is set (and the `icc` feature is on).
enum Display {
    Srgb(XyzToSrgb),
    #[cfg(feature = "icc")]
    Icc(crate::colour::XyzToIcc),
}

impl Display {
    fn build(opts: &DcpPreviewOptions) -> Result<Self, PreviewError> {
        match &opts.display_profile {
            None => Ok(Display::Srgb(XyzToSrgb::new())),
            #[cfg(feature = "icc")]
            Some(p) => crate::colour::XyzToIcc::new(p, opts.intent)
                .map(Display::Icc)
                .map_err(PreviewError::Decode),
            #[cfg(not(feature = "icc"))]
            Some(_) => Err(PreviewError::Decode(
                "a display ICC profile was requested but postkit was built without the `icc` feature"
                    .into(),
            )),
        }
    }

    fn apply(&self, raw: &[u8], out: &mut Vec<u8>) {
        match self {
            Display::Srgb(t) => t.frame_xyz12le_to_srgb8(raw, out),
            #[cfg(feature = "icc")]
            Display::Icc(t) => t.frame_xyz12le_to_rgb8(raw, out),
        }
    }
}

/// Decode + colour-manage a single picture frame.
fn decode_dcp_frame(
    reader: &mut MxfReader,
    dec: Option<&mut AesDecContext>,
    frame: u32,
    display: &Display,
) -> Result<Rgb8Frame, PreviewError> {
    let j2c = read_j2c_frame(reader, frame, dec)?;
    let hdr = crate::j2k::parse_j2k_header(&j2c)
        .ok_or_else(|| PreviewError::Decode("decrypted data is not a J2K codestream".into()))?;
    let raw = decode_j2c_to_xyz12le(j2c)?;
    let mut data = Vec::new();
    display.apply(&raw, &mut data);
    Ok(Rgb8Frame {
        width: hdr.width,
        height: hdr.height,
        data,
    })
}

/// Decode a single DCP picture frame, colour-manage it, and write it to an
/// image file (format from the extension, encoded by ffmpeg).
pub fn render_dcp_frame(
    opts: &DcpPreviewOptions,
    frame: u32,
    out_image: &Path,
) -> Result<(), PreviewError> {
    let resolved = resolve_picture(&opts.source)?;
    let display = Display::build(opts)?;
    let mut dec = dec_context(&resolved, opts.key)?;

    let mut reader = MxfReader::new();
    reader
        .open_read(&resolved.mxf.to_string_lossy())
        .map_err(|e| PreviewError::Mxf(format!("open: {e}")))?;
    let img = decode_dcp_frame(&mut reader, dec.as_mut(), frame, &display)?;
    let _ = reader.close();

    write_rgb8_image(&img, out_image)
}

/// Encode a raw RGB frame to an image file via ffmpeg.
fn write_rgb8_image(img: &Rgb8Frame, out_image: &Path) -> Result<(), PreviewError> {
    let mut child = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-s",
            &format!("{}x{}", img.width, img.height),
            "-i",
            "pipe:0",
            "-frames:v",
            "1",
        ])
        .arg(out_image)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| PreviewError::Decode(format!("spawn ffmpeg: {e}")))?;

    let data = img.data.clone();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
        drop(stdin);
    });
    let out = child
        .wait_with_output()
        .map_err(|e| PreviewError::Decode(format!("ffmpeg wait: {e}")))?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(PreviewError::Decode(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(())
}

/// Decode a DCP picture range to a colour-managed intermediate and play it with
/// ffplay. Honours `start_frame`/`end_frame` (0 = to end) and `loop_playback`.
///
/// Not real-time: frames decode on the CPU into a lossless FFV1 intermediate
/// first, then ffplay plays that at the essence frame rate.
pub fn play_dcp(opts: &DcpPreviewOptions) -> Result<(), PreviewError> {
    let resolved = resolve_picture(&opts.source)?;
    let display = Display::build(opts)?;
    let mut dec = dec_context(&resolved, opts.key)?;

    let start = opts.start_frame;
    let end = if opts.end_frame == 0 || opts.end_frame > resolved.frame_count {
        resolved.frame_count
    } else {
        opts.end_frame
    };
    if end <= start {
        return Err(PreviewError::Decode(format!(
            "empty frame range {start}..{end}"
        )));
    }

    let tmp = std::env::temp_dir().join(format!("postkit-preview-{}.mkv", uuid::Uuid::new_v4()));

    let mut reader = MxfReader::new();
    reader
        .open_read(&resolved.mxf.to_string_lossy())
        .map_err(|e| PreviewError::Mxf(format!("open: {e}")))?;

    // decode the first frame to learn the dimensions, then start the encoder
    let first = decode_dcp_frame(&mut reader, dec.as_mut(), start, &display)?;
    let size = format!("{}x{}", first.width, first.height);

    let mut enc = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-s",
            &size,
            "-r",
            &format!("{}", resolved.fps),
            "-i",
            "pipe:0",
            "-c:v",
            "ffv1",
        ])
        .arg(&tmp)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| PreviewError::Decode(format!("spawn ffmpeg encoder: {e}")))?;
    let mut enc_stdin = enc.stdin.take().unwrap();

    let mut feed = |img: &Rgb8Frame| -> Result<(), PreviewError> {
        enc_stdin.write_all(&img.data)?;
        Ok(())
    };
    feed(&first)?;
    for frame in (start + 1)..end {
        let img = decode_dcp_frame(&mut reader, dec.as_mut(), frame, &display)?;
        feed(&img)?;
    }
    drop(enc_stdin);
    let _ = reader.close();

    let out = enc
        .wait_with_output()
        .map_err(|e| PreviewError::Decode(format!("encoder wait: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(PreviewError::Decode(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }

    let mut cmd = std::process::Command::new("ffplay");
    cmd.arg("-autoexit").arg("-hide_banner");
    if opts.loop_playback {
        cmd.arg("-loop").arg("0");
    }
    cmd.arg(&tmp);
    let status = cmd.status();
    let _ = std::fs::remove_file(&tmp);
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(PreviewError::Decode("ffplay exited non-zero".into())),
        Err(e) => Err(PreviewError::Decode(format!("run ffplay: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_rejects_options_ffplay_cannot_honour() {
        // each of these returns before ever spawning ffplay
        let with_cpl = PlaybackOptions {
            cpl_uuid: "urn:uuid:x".into(),
            ..Default::default()
        };
        assert_eq!(play(&with_cpl), -1);

        let with_gpu = PlaybackOptions {
            gpu_device: 0,
            ..Default::default()
        };
        assert_eq!(play(&with_gpu), -1);

        let with_cs = PlaybackOptions {
            display_colourspace: "P3".into(),
            ..Default::default()
        };
        assert_eq!(play(&with_cs), -1);
    }

    #[test]
    fn key_from_hex_parses_16_bytes() {
        let k = key_from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
        assert_eq!(k, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn key_from_hex_rejects_wrong_length() {
        assert!(key_from_hex("00010203").is_err());
        assert!(key_from_hex("zz").is_err());
    }

    #[test]
    fn key_from_keys_json_picks_the_image_key() {
        let dir = std::env::temp_dir().join(format!("postkit-keys-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("KEYS.json");
        std::fs::write(
            &path,
            r#"{"cpl_id":"c","keys":[
                {"key_type":"Mdak","key_id":"a","asset_uuid":"snd","content_key_hex":"ffffffffffffffffffffffffffffffff"},
                {"key_type":"Mdik","key_id":"b","asset_uuid":"pic","content_key_hex":"000102030405060708090a0b0c0d0e0f"}
            ]}"#,
        )
        .unwrap();
        // picks the image (Mdik) key, not the audio one
        let k = key_from_keys_json(&path, None).unwrap();
        assert_eq!(k[0], 0x00);
        assert_eq!(k[15], 0x0f);
        // and can match by asset uuid
        let k2 = key_from_keys_json(&path, Some("pic")).unwrap();
        assert_eq!(k2, k);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn assetmap_and_cpl_resolution() {
        // MainPicture id links the CPL to an ASSETMAP path
        let cpl = r#"<CompositionPlaylist xmlns="x"><Reel><AssetList>
            <MainPicture><Id>urn:uuid:11111111-1111-1111-1111-111111111111</Id></MainPicture>
            </AssetList></Reel></CompositionPlaylist>"#;
        assert_eq!(
            first_main_picture_id(cpl).as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );

        let dir = std::env::temp_dir().join(format!("postkit-am-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ASSETMAP.xml"),
            r#"<AssetMap><AssetList>
                <Asset><Id>urn:uuid:11111111-1111-1111-1111-111111111111</Id><ChunkList><Chunk><Path>video/pic.mxf</Path></Chunk></ChunkList></Asset>
            </AssetList></AssetMap>"#,
        )
        .unwrap();
        let map = parse_assetmap(&dir.join("ASSETMAP.xml")).unwrap();
        assert_eq!(
            map.get("11111111-1111-1111-1111-111111111111")
                .map(String::as_str),
            Some("video/pic.mxf")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
