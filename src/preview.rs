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
//! `KEYS.json` or a raw hex key), decodes the JPEG 2000 codestream in process
//! with grok ([`crate::grok_decoder`]) to raw X'Y'Z' code values, and
//! colour-manages those to an sRGB monitor via
//! [`crate::colour::XyzToSrgb`] (the inverse of the DCDM encode). An optional
//! monitor ICC profile routes the transform through littleCMS instead (the
//! `icc` cargo feature). Encrypted essence with no key fails loud rather than
//! showing garbage.
//!
//! IMF App 2E picture ([`render_imf_frame`]) shares the resolve and decrypt
//! steps and then takes the RGB samples grok returns straight to 8-bit sRGB,
//! because the essence descriptor's ColorPrimaries and TransferCharacteristic
//! say Rec.709. Any other colour is refused by name.
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

/// Extract a single frame as an image (thumbnail/QC).
///
/// DCP picture essence goes through the grok decoder and the DCDM inverse
/// transform, which decodes a 2K frame in 68 ms against ffmpeg's 302 ms and does
/// not decode every earlier frame to reach a late one. IMF App 2E picture takes
/// the same decoder and the Rec.709 transform in [`render_imf_frame`].
/// Everything else, broadcast and unrestricted codestreams included, goes
/// through ffmpeg.
///
/// `key` is the picture essence's AES-128 content key, needed only for encrypted
/// essence. [`key_from_hex`] and [`picture_key_from_keys_json`] produce one.
pub fn extract_frame(input: &Path, frame: u32, output_image: &Path, key: Option<[u8; 16]>) -> i32 {
    let opts = DcpPreviewOptions {
        source: input.to_path_buf(),
        key,
        ..Default::default()
    };
    let rendered = match frame_route(input, frame, key) {
        FrameRoute::Dcp => render_dcp_frame(&opts, frame, output_image),
        FrameRoute::Imf => render_imf_frame(&opts, frame, output_image),
        FrameRoute::Ffmpeg => return extract_frame_with_ffmpeg(input, frame, output_image),
        FrameRoute::Refused(reason) => {
            tracing::error!("Frame extraction failed: {reason}");
            return -1;
        }
    };
    match rendered {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("Frame extraction failed: {e}");
            -1
        }
    }
}

/// What decodes an [`extract_frame`] input.
enum FrameRoute {
    /// DCP picture essence, decoded by grok and colour-managed from X'Y'Z'.
    Dcp,
    /// IMF App 2E picture essence, decoded by grok and shown as Rec.709 RGB.
    Imf,
    /// Anything else, decoded by ffmpeg.
    Ffmpeg,
    /// Refused, with the reason.
    Refused(String),
}

/// Pick the decoder for one frame of `input`.
///
/// Encrypted essence never goes to ffmpeg, which cannot decrypt it and would
/// render the ciphertext as a picture. Without a key it is refused; with one it
/// takes the grok path, and an encrypted codestream that is not DCI cinema is
/// refused outright because neither decoder can render it correctly.
fn frame_route(input: &Path, frame: u32, key: Option<[u8; 16]>) -> FrameRoute {
    if !cfg!(feature = "grok-ffi") || !is_jpeg2000_mxf(input) {
        if key.is_some() {
            return FrameRoute::Refused(format!(
                "a content key decrypts JPEG 2000 MXF essence, and {} is not that",
                input.display()
            ));
        }
        return FrameRoute::Ffmpeg;
    }
    let resolved = match resolve_picture(input) {
        Ok(resolved) => resolved,
        Err(e) => return FrameRoute::Refused(e.to_string()),
    };
    if resolved.encrypted && key.is_none() {
        return FrameRoute::Refused(format!(
            "{} is encrypted, so extracting a frame from it needs its content key",
            input.display()
        ));
    }
    if !resolved.encrypted && key.is_some() {
        tracing::warn!(
            "{} is not encrypted, so the content key goes unused",
            input.display()
        );
    }

    let j2c = match read_picture_codestream(&resolved, key, frame) {
        Ok(j2c) => j2c,
        Err(e) => return FrameRoute::Refused(e.to_string()),
    };
    let Some(header) = crate::j2k::parse_j2k_header(&j2c) else {
        return FrameRoute::Refused(format!(
            "frame {frame} of {} is not a JPEG 2000 codestream",
            resolved.mxf.display()
        ));
    };
    let profile = crate::j2k::J2kProfile::from(header.profile);
    if profile.is_dci_cinema() {
        return FrameRoute::Dcp;
    }
    if profile == crate::j2k::J2kProfile::Imf {
        return FrameRoute::Imf;
    }
    if resolved.encrypted {
        return FrameRoute::Refused(format!(
            "codestream profile {:#06x} is not DCI cinema, and ffmpeg cannot decrypt this essence",
            header.profile
        ));
    }
    tracing::debug!(
        "codestream profile {:#06x} is not DCI cinema, decoding with ffmpeg",
        header.profile
    );
    FrameRoute::Ffmpeg
}

/// Whether the input is mono JPEG 2000 picture essence in an MXF.
///
/// Stereoscopic essence needs asdcplib's stereo reader, which the DCP-native
/// path does not use.
fn is_jpeg2000_mxf(input: &Path) -> bool {
    matches!(
        asdcplib::essence_type(&input.to_string_lossy()),
        Ok(asdcplib::EssenceType::Jpeg2000 | asdcplib::EssenceType::As02Jpeg2000)
    )
}

fn extract_frame_with_ffmpeg(input: &Path, frame: u32, output_image: &Path) -> i32 {
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
use asdcplib::jp2k::{
    COLOR_PRIMARIES_BT709, COLOR_PRIMARIES_BT2020, COLOR_PRIMARIES_P3D65,
    TRANSFER_CHARACTERISTIC_BT709, TRANSFER_CHARACTERISTIC_BT2020, TRANSFER_CHARACTERISTIC_ST2084,
};
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
    #[error("no display transform: {0}")]
    Display(String),
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
    /// Bare UUID of the track file, the form a dcpwizard `KEYS.json` records.
    pub asset_uuid: String,
    pub encrypted: bool,
    pub frame_count: u32,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// AS-02 (IMF) wrapping rather than AS-DCP, which picks the reader.
    pub as02: bool,
    /// ColorPrimaries UL from the essence descriptor, `None` when it signals none.
    pub color_primaries: Option<[u8; 16]>,
    /// TransferCharacteristic UL from the essence descriptor.
    pub transfer_characteristic: Option<[u8; 16]>,
}

/// A JPEG 2000 picture reader, one variant per MXF flavour.
///
/// The AS-DCP reader opens an AS-02 file and then fails every `read_frame`, so
/// the flavour has to be settled before the first read.
enum PictureReader {
    AsDcp(asdcplib::jp2k::MxfReader),
    As02(asdcplib::as02::jp2k::MxfReader),
}

impl PictureReader {
    fn open(mxf: &Path, as02: bool) -> Result<Self, PreviewError> {
        let path = mxf.to_string_lossy().to_string();
        let mut reader = if as02 {
            PictureReader::As02(asdcplib::as02::jp2k::MxfReader::new())
        } else {
            PictureReader::AsDcp(asdcplib::jp2k::MxfReader::new())
        };
        let opened = match &mut reader {
            PictureReader::AsDcp(r) => r.open_read(&path),
            PictureReader::As02(r) => r.open_read(&path),
        };
        opened.map_err(|e| PreviewError::Mxf(format!("open {}: {e}", mxf.display())))?;
        Ok(reader)
    }

    fn writer_info(&mut self) -> Result<asdcplib::WriterInfo, PreviewError> {
        match self {
            PictureReader::AsDcp(r) => r.writer_info(),
            PictureReader::As02(r) => r.writer_info(),
        }
        .map_err(|e| PreviewError::Mxf(format!("writer info: {e}")))
    }

    fn picture_descriptor(&mut self) -> Result<asdcplib::jp2k::PictureDescriptor, PreviewError> {
        match self {
            PictureReader::AsDcp(r) => r.picture_descriptor(),
            PictureReader::As02(r) => r.picture_descriptor(),
        }
        .map_err(|e| PreviewError::Mxf(format!("picture descriptor: {e}")))
    }

    fn hdr_metadata(&mut self) -> Result<asdcplib::jp2k::HdrMetadata, PreviewError> {
        match self {
            PictureReader::AsDcp(r) => r.hdr_metadata(),
            PictureReader::As02(r) => r.hdr_metadata(),
        }
        .map_err(|e| PreviewError::Mxf(format!("hdr metadata: {e}")))
    }

    fn read_frame(
        &mut self,
        frame: u32,
        buf: &mut [u8],
        dec: Option<&mut AesDecContext>,
    ) -> Result<usize, PreviewError> {
        match self {
            PictureReader::AsDcp(r) => r.read_frame(frame, buf, dec, None),
            PictureReader::As02(r) => r.read_frame(frame, buf, dec, None),
        }
        .map_err(|e| PreviewError::Mxf(format!("read frame {frame}: {e}")))
    }

    fn close(&mut self) {
        let _ = match self {
            PictureReader::AsDcp(r) => r.close(),
            PictureReader::As02(r) => r.close(),
        };
    }
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

/// The picture content key from whichever source a caller was given: a raw hex
/// key, a dcpwizard `KEYS.json`, or neither.
///
/// `Ok(None)` means no key was asked for, which is what unencrypted essence
/// needs. The two sources are mutually exclusive, and the hex one wins if both
/// arrive.
pub fn resolve_picture_key(
    source: &Path,
    key_hex: Option<&str>,
    keys_json: Option<&Path>,
) -> Result<Option<[u8; 16]>, PreviewError> {
    match (key_hex, keys_json) {
        (Some(hex), _) => key_from_hex(hex).map(Some),
        (None, Some(path)) => picture_key_from_keys_json(source, path).map(Some),
        (None, None) => Ok(None),
    }
}

/// Load the content key for `source`'s picture essence from a dcpwizard
/// `KEYS.json`, matching the track file's own asset UUID.
///
/// [`key_from_keys_json`] alone takes the first image key in the file, which is
/// the wrong one when a package carries a picture asset per reel.
pub fn picture_key_from_keys_json(
    source: &Path,
    keys_json: &Path,
) -> Result<[u8; 16], PreviewError> {
    let resolved = resolve_picture(source)?;
    key_from_keys_json(keys_json, Some(&resolved.asset_uuid))
}

/// Resolve a DCP directory, CPL XML, or picture MXF to the picture essence.
pub fn resolve_picture(source: &Path) -> Result<ResolvedPicture, PreviewError> {
    let mxf = find_picture_mxf(source)?;
    let as02 = matches!(
        asdcplib::essence_type(&mxf.to_string_lossy()),
        Ok(asdcplib::EssenceType::As02Jpeg2000)
    );

    let mut reader = PictureReader::open(&mxf, as02)?;
    let info = reader.writer_info()?;
    let desc = reader.picture_descriptor()?;
    // a descriptor with no colour items is not an error, it reads as unsignalled
    let colour = reader.hdr_metadata().unwrap_or_default();
    reader.close();

    let fps = if desc.edit_rate.denominator != 0 {
        desc.edit_rate.numerator as f64 / desc.edit_rate.denominator as f64
    } else {
        24.0
    };

    Ok(ResolvedPicture {
        mxf,
        asset_uuid: uuid::Uuid::from_bytes(info.asset_uuid).to_string(),
        encrypted: info.encrypted_essence,
        frame_count: desc.container_duration,
        width: desc.stored_width,
        height: desc.stored_height,
        fps,
        as02,
        color_primaries: colour.color_primaries,
        transfer_characteristic: colour.transfer_characteristic,
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
    crate::assetmap::find(dir)
        .ok_or_else(|| PreviewError::Resolve(format!("no ASSETMAP in {}", dir.display())))
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
    let map = crate::assetmap::parse(path);
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
    reader: &mut PictureReader,
    frame: u32,
    dec: Option<&mut AesDecContext>,
) -> Result<Vec<u8>, PreviewError> {
    let mut buf = vec![0u8; MAX_FRAME_BYTES];
    let size = reader.read_frame(frame, &mut buf, dec)?;
    buf.truncate(size);
    Ok(buf)
}

/// One frame's codestream, decrypted when the essence is encrypted.
///
/// The routing in [`frame_route`] reads the profile from this rather than from
/// the raw essence, because a frame of encrypted essence read without its key is
/// ciphertext and carries no readable header.
fn read_picture_codestream(
    resolved: &ResolvedPicture,
    key: Option<[u8; 16]>,
    frame: u32,
) -> Result<Vec<u8>, PreviewError> {
    let mut dec = dec_context(resolved, key)?;
    let mut reader = PictureReader::open(&resolved.mxf, resolved.as02)?;
    let j2c = read_j2c_frame(&mut reader, frame, dec.as_mut())?;
    reader.close();
    Ok(j2c)
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
    reader: &mut PictureReader,
    dec: Option<&mut AesDecContext>,
    frame: u32,
    display: &Display,
) -> Result<Rgb8Frame, PreviewError> {
    let j2c = read_j2c_frame(reader, frame, dec)?;
    let decoded = crate::grok_decoder::decode(j2c, 0).map_err(PreviewError::Decode)?;
    let raw = decoded.to_xyz12le().map_err(PreviewError::Decode)?;
    let mut data = Vec::new();
    display.apply(&raw, &mut data);
    Ok(Rgb8Frame {
        width: decoded.width,
        height: decoded.height,
        data,
    })
}

/// Decode a single DCP picture frame, colour-manage it, and write it to an
/// image file, format from the extension.
pub fn render_dcp_frame(
    opts: &DcpPreviewOptions,
    frame: u32,
    out_image: &Path,
) -> Result<(), PreviewError> {
    let resolved = resolve_picture(&opts.source)?;
    let display = Display::build(opts)?;
    let mut dec = dec_context(&resolved, opts.key)?;

    let mut reader = PictureReader::open(&resolved.mxf, resolved.as02)?;
    let img = decode_dcp_frame(&mut reader, dec.as_mut(), frame, &display)?;
    reader.close();

    write_rgb8_image(&img, out_image)
}

/// Decode a single IMF App 2E picture frame and write it to an image file,
/// format from the extension.
///
/// The samples are RGB, and the essence descriptor's ColorPrimaries and
/// TransferCharacteristic say which RGB. Rec.709 shares its primaries and white
/// point with sRGB, so the 12-bit code values reach the screen with their low
/// bits dropped and nothing else. Any other signalled colour is refused by name.
pub fn render_imf_frame(
    opts: &DcpPreviewOptions,
    frame: u32,
    out_image: &Path,
) -> Result<(), PreviewError> {
    if opts.display_profile.is_some() {
        return Err(PreviewError::Display(
            "a display ICC profile was requested, and the IMF path has no ICC transform".into(),
        ));
    }
    let resolved = resolve_picture(&opts.source)?;
    check_rec709_colour(&resolved)?;
    let j2c = read_picture_codestream(&resolved, opts.key, frame)?;
    let decoded = crate::grok_decoder::decode(j2c, 0).map_err(PreviewError::Decode)?;
    let img = rec709_frame_to_srgb8(&decoded, &resolved.mxf)?;
    write_rgb8_image(&img, out_image)
}

/// Refuse any picture colour the pass-through transform would show wrong,
/// treating unsignalled colour as Rec.709: packages exist that signal nothing.
fn check_rec709_colour(resolved: &ResolvedPicture) -> Result<(), PreviewError> {
    const NO_TONE_MAPPING: &str = "the preview has no tone mapping for it yet";
    const REC709_ONLY: &str = "the preview has a display transform only for Rec.709";
    let file = resolved.mxf.display();

    match resolved.transfer_characteristic {
        None => tracing::warn!(
            "{file} signals no transfer characteristic, so the preview assumes Rec.709"
        ),
        Some(ul) if ul == TRANSFER_CHARACTERISTIC_BT709 => {}
        Some(ul) if ul == TRANSFER_CHARACTERISTIC_ST2084 => {
            return Err(PreviewError::Display(format!(
                "{file} signals the ST 2084 (PQ) transfer characteristic, and {NO_TONE_MAPPING}"
            )));
        }
        Some(ul) if ul == TRANSFER_CHARACTERISTIC_BT2020 => {
            return Err(PreviewError::Display(format!(
                "{file} signals the BT.2020 transfer characteristic, and {NO_TONE_MAPPING}"
            )));
        }
        Some(ul) => {
            return Err(PreviewError::Display(format!(
                "{file} signals the unrecognised transfer characteristic {ul:02x?}, and {REC709_ONLY}"
            )));
        }
    }

    match resolved.color_primaries {
        None => {
            tracing::warn!("{file} signals no colour primaries, so the preview assumes Rec.709")
        }
        Some(ul) if ul == COLOR_PRIMARIES_BT709 => {}
        Some(ul) if ul == COLOR_PRIMARIES_P3D65 => {
            return Err(PreviewError::Display(format!(
                "{file} signals P3-D65 colour primaries, and {NO_TONE_MAPPING}"
            )));
        }
        Some(ul) if ul == COLOR_PRIMARIES_BT2020 => {
            return Err(PreviewError::Display(format!(
                "{file} signals BT.2020 colour primaries, and {NO_TONE_MAPPING}"
            )));
        }
        Some(ul) => {
            return Err(PreviewError::Display(format!(
                "{file} signals the unrecognised colour primaries {ul:02x?}, and {REC709_ONLY}"
            )));
        }
    }
    Ok(())
}

/// Rec.709 RGB code values as packed 8-bit sRGB.
fn rec709_frame_to_srgb8(
    decoded: &crate::grok_decoder::DecodedFrame,
    mxf: &Path,
) -> Result<Rgb8Frame, PreviewError> {
    /// What an App 2E picture carries, and the only depth this shift is right for.
    const IMF_PRECISION_BITS: u8 = 12;
    const IMF_COMPONENT_COUNT: usize = 3;
    const TWELVE_TO_EIGHT_BIT_SHIFT: u32 = 4;
    const EIGHT_BIT_MAX: i32 = 255;

    if decoded.components.len() != IMF_COMPONENT_COUNT {
        return Err(PreviewError::Display(format!(
            "{} decodes to {} components, and the preview shows only 3-component RGB picture",
            mxf.display(),
            decoded.components.len()
        )));
    }
    if decoded.precision != IMF_PRECISION_BITS {
        return Err(PreviewError::Display(format!(
            "{} decodes at {} bits a sample, and the preview shows only {IMF_PRECISION_BITS}-bit \
             IMF picture",
            mxf.display(),
            decoded.precision
        )));
    }
    let samples = decoded.width as usize * decoded.height as usize;
    for (index, component) in decoded.components.iter().enumerate() {
        if component.len() != samples {
            return Err(PreviewError::Display(format!(
                "component {index} of {} holds {} samples, not the {samples} the frame is",
                mxf.display(),
                component.len()
            )));
        }
    }

    let mut data = Vec::with_capacity(samples * IMF_COMPONENT_COUNT);
    for sample in 0..samples {
        for component in &decoded.components {
            let value = component[sample] >> TWELVE_TO_EIGHT_BIT_SHIFT;
            data.push(value.clamp(0, EIGHT_BIT_MAX) as u8);
        }
    }
    Ok(Rgb8Frame {
        width: decoded.width,
        height: decoded.height,
        data,
    })
}

/// Write a raw RGB frame to an image file, format from the extension.
///
/// A binary PPM is the frame's own rgb24 bytes behind a short header, so it is
/// written here. Every other format goes through ffmpeg.
fn write_rgb8_image(img: &Rgb8Frame, out_image: &Path) -> Result<(), PreviewError> {
    if out_image
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("ppm"))
    {
        return write_ppm(img, out_image);
    }

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

/// Write the frame as a binary PPM (P6), 8 bits a channel.
fn write_ppm(img: &Rgb8Frame, out_image: &Path) -> Result<(), PreviewError> {
    let mut file = std::fs::File::create(out_image)?;
    write!(file, "P6\n{} {}\n255\n", img.width, img.height)?;
    file.write_all(&img.data)?;
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

    let mut reader = PictureReader::open(&resolved.mxf, resolved.as02)?;

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
    reader.close();

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
