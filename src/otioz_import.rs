//! Import OTIOZ/OTIO (OpenTimelineIO) files and extract timeline information.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OtiozError {
    #[error("File not found: {0}")]
    NotFound(PathBuf),
    #[error("Expected .otioz or .otio file")]
    InvalidExtension,
    #[error("No content.otio found in bundle")]
    NoContent,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct OtiozClip {
    pub name: String,
    pub media_reference: String,
    pub start_time: f64,
    pub duration: f64,
    pub track_kind: String,
}

#[derive(Debug, Clone)]
pub struct OtiozImportOptions {
    pub input_file: PathBuf,
    pub output_dir: PathBuf,
    pub extract_media: bool,
    pub generate_cpl: bool,
    pub title: String,
    pub fps: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OtiozImportResult {
    pub clips: Vec<OtiozClip>,
    pub video_tracks: u32,
    pub audio_tracks: u32,
    pub subtitle_tracks: u32,
    pub extracted_dir: PathBuf,
    pub generated_cpl: PathBuf,
}

#[derive(Debug)]
struct ZipLocalHeader {
    filename: String,
    compressed_size: u32,
    #[allow(dead_code)]
    uncompressed_size: u32,
    data_offset: u64,
}

/// Import an OTIOZ or OTIO file and extract timeline clips.
pub fn import_otioz(opts: &OtiozImportOptions) -> Result<OtiozImportResult, OtiozError> {
    if !opts.input_file.exists() {
        return Err(OtiozError::NotFound(opts.input_file.clone()));
    }

    let ext = opts
        .input_file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext != "otioz" && ext != "otio" {
        return Err(OtiozError::InvalidExtension);
    }

    let otio_content = if ext == "otio" {
        fs::read_to_string(&opts.input_file)?
    } else {
        let entries = list_zip_entries(&opts.input_file)?;

        let content_entry = entries
            .iter()
            .find(|e| e.filename == "content.otio")
            .ok_or(OtiozError::NoContent)?;

        let mut file = fs::File::open(&opts.input_file)?;
        file.seek(SeekFrom::Start(content_entry.data_offset))?;
        let mut buf = vec![0u8; content_entry.compressed_size as usize];
        file.read_exact(&mut buf)?;

        if opts.extract_media && !opts.output_dir.as_os_str().is_empty() {
            let media_dir = opts.output_dir.join("media");
            fs::create_dir_all(&media_dir)?;

            for entry in &entries {
                if entry.filename.starts_with("media/") && entry.compressed_size > 0 {
                    let out_path = opts.output_dir.join(&entry.filename);
                    if let Some(parent) = out_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let mut f = fs::File::open(&opts.input_file)?;
                    f.seek(SeekFrom::Start(entry.data_offset))?;
                    let mut data = vec![0u8; entry.compressed_size as usize];
                    f.read_exact(&mut data)?;
                    fs::write(&out_path, &data)?;
                }
            }
        }

        String::from_utf8_lossy(&buf).to_string()
    };

    let clips = parse_otio_json(&otio_content);

    let mut video_tracks = 0u32;
    let mut audio_tracks = 0u32;
    let mut subtitle_tracks = 0u32;
    for clip in &clips {
        match clip.track_kind.as_str() {
            "Video" => video_tracks += 1,
            "Audio" => audio_tracks += 1,
            _ => subtitle_tracks += 1,
        }
    }

    let mut extracted_dir = PathBuf::new();
    if opts.extract_media && !opts.output_dir.as_os_str().is_empty() {
        extracted_dir = opts.output_dir.join("media");
    }

    let mut generated_cpl = PathBuf::new();
    if opts.generate_cpl && !opts.output_dir.as_os_str().is_empty() {
        generated_cpl = opts.output_dir.join("CPL_from_otio.xml");
        let title = if opts.title.is_empty() {
            "OTIOZ Import"
        } else {
            &opts.title
        };
        let cpl_content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CompositionPlaylist xmlns="http://www.smpte-ra.org/schemas/429-7/2006/CPL">
  <Id>urn:uuid:00000000-0000-0000-0000-000000000000</Id>
  <ContentTitleText>{title}</ContentTitleText>
  <EditRate>{} 1</EditRate>
  <!-- Imported from: {} -->
  <!-- {} clips imported -->
</CompositionPlaylist>
"#,
            opts.fps as u32,
            opts.input_file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            clips.len()
        );
        fs::write(&generated_cpl, cpl_content)?;
    }

    Ok(OtiozImportResult {
        clips,
        video_tracks,
        audio_tracks,
        subtitle_tracks,
        extracted_dir,
        generated_cpl,
    })
}

fn list_zip_entries(path: &Path) -> Result<Vec<ZipLocalHeader>, std::io::Error> {
    let mut entries = Vec::new();
    let mut file = fs::File::open(path)?;

    loop {
        let mut sig = [0u8; 4];
        if file.read_exact(&mut sig).is_err() {
            break;
        }
        if sig != [0x50, 0x4b, 0x03, 0x04] {
            break;
        }

        file.seek(SeekFrom::Current(22))?;

        let mut sizes = [0u8; 8];
        file.read_exact(&mut sizes)?;
        let comp_size = u32::from_le_bytes([sizes[0], sizes[1], sizes[2], sizes[3]]);
        let uncomp_size = u32::from_le_bytes([sizes[4], sizes[5], sizes[6], sizes[7]]);

        let mut lens = [0u8; 4];
        file.read_exact(&mut lens)?;
        let name_len = u16::from_le_bytes([lens[0], lens[1]]);
        let extra_len = u16::from_le_bytes([lens[2], lens[3]]);

        let mut name_buf = vec![0u8; name_len as usize];
        file.read_exact(&mut name_buf)?;
        let filename = String::from_utf8_lossy(&name_buf).to_string();

        file.seek(SeekFrom::Current(extra_len as i64))?;

        let data_offset = file.stream_position()?;

        file.seek(SeekFrom::Current(comp_size as i64))?;

        entries.push(ZipLocalHeader {
            filename,
            compressed_size: comp_size,
            uncompressed_size: uncomp_size,
            data_offset,
        });
    }

    Ok(entries)
}

fn parse_otio_json(json: &str) -> Vec<OtiozClip> {
    let mut clips = Vec::new();
    let clip_marker = "\"OTIO_SCHEMA\": \"Clip.";

    let mut pos = 0;
    while let Some(found) = json[pos..].find(clip_marker) {
        let abs_pos = pos + found;
        let block_end = (abs_pos + 2000).min(json.len());
        let block = &json[abs_pos..block_end];

        let name = extract_json_string(block, "name").unwrap_or_default();
        let duration = extract_json_number(block, "value").unwrap_or(0.0);
        let media_ref = extract_json_string(block, "target_url").unwrap_or_default();

        let preceding = &json[..abs_pos];
        let video_pos = preceding.rfind("\"kind\": \"Video\"");
        let audio_pos = preceding.rfind("\"kind\": \"Audio\"");

        let track_kind = match (video_pos, audio_pos) {
            (Some(v), Some(a)) if v > a => "Video",
            (Some(_), None) => "Video",
            _ => "Audio",
        }
        .to_string();

        clips.push(OtiozClip {
            name,
            media_reference: media_ref,
            start_time: 0.0,
            duration,
            track_kind,
        });

        pos = abs_pos + 10;
    }

    clips
}

fn extract_json_string(block: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\": \"");
    let start = block.find(&pattern)? + pattern.len();
    let end = block[start..].find('"')? + start;
    Some(block[start..end].to_string())
}

fn extract_json_number(block: &str, key: &str) -> Option<f64> {
    let pattern = format!("\"{key}\": ");
    let start = block.find(&pattern)? + pattern.len();
    let end = block[start..]
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(block[start..].len())
        + start;
    block[start..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_otio_json_video_clip() {
        let json = r#"{
            "OTIO_SCHEMA": "Timeline.1",
            "tracks": {
                "children": [{
                    "kind": "Video",
                    "children": [{
                        "OTIO_SCHEMA": "Clip.1",
                        "name": "shot_01",
                        "source_range": {
                            "duration": {
                                "OTIO_SCHEMA": "RationalTime.1",
                                "value": 48.0
                            }
                        },
                        "media_reference": {
                            "target_url": "media/shot_01.mxf"
                        }
                    }]
                }]
            }
        }"#;

        let clips = parse_otio_json(json);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "shot_01");
        assert_eq!(clips[0].duration, 48.0);
        assert_eq!(clips[0].media_reference, "media/shot_01.mxf");
        assert_eq!(clips[0].track_kind, "Video");
    }

    #[test]
    fn test_parse_otio_json_audio_clip() {
        let json = r#"{
            "OTIO_SCHEMA": "Timeline.1",
            "tracks": {
                "children": [{
                    "kind": "Audio",
                    "children": [{
                        "OTIO_SCHEMA": "Clip.2",
                        "name": "audio_mix",
                        "source_range": {
                            "duration": {
                                "OTIO_SCHEMA": "RationalTime.1",
                                "value": 120.0
                            }
                        },
                        "media_reference": {
                            "target_url": "media/audio.wav"
                        }
                    }]
                }]
            }
        }"#;

        let clips = parse_otio_json(json);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].track_kind, "Audio");
    }

    #[test]
    fn test_parse_otio_json_multiple_clips() {
        let json = r#"{
            "OTIO_SCHEMA": "Timeline.1",
            "tracks": { "children": [{
                "kind": "Video",
                "children": [
                    { "OTIO_SCHEMA": "Clip.1", "name": "clip_a", "source_range": { "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 24.0 } }, "media_reference": { "target_url": "a.mxf" } },
                    { "OTIO_SCHEMA": "Clip.1", "name": "clip_b", "source_range": { "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 48.0 } }, "media_reference": { "target_url": "b.mxf" } }
                ]
            }]}
        }"#;

        let clips = parse_otio_json(json);
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].name, "clip_a");
        assert_eq!(clips[1].name, "clip_b");
    }

    #[test]
    fn test_import_otio_plain_file() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("timeline.otio");

        let otio = r#"{
            "OTIO_SCHEMA": "Timeline.1",
            "tracks": { "children": [{
                "kind": "Video",
                "children": [
                    { "OTIO_SCHEMA": "Clip.1", "name": "shot1", "source_range": { "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 100.0 } }, "media_reference": { "target_url": "shot1.mxf" } }
                ]
            }]}
        }"#;
        fs::write(&input, otio).unwrap();

        let opts = OtiozImportOptions {
            input_file: input,
            output_dir: tmp.path().to_path_buf(),
            extract_media: false,
            generate_cpl: false,
            title: String::new(),
            fps: 24.0,
        };

        let result = import_otioz(&opts).unwrap();
        assert_eq!(result.clips.len(), 1);
        assert_eq!(result.video_tracks, 1);
        assert_eq!(result.audio_tracks, 0);
    }

    #[test]
    fn test_import_generates_cpl() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("test.otio");

        let otio = r#"{
            "OTIO_SCHEMA": "Timeline.1",
            "tracks": { "children": [{
                "kind": "Video",
                "children": [
                    { "OTIO_SCHEMA": "Clip.1", "name": "s1", "source_range": { "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": 50.0 } }, "media_reference": { "target_url": "s1.mxf" } }
                ]
            }]}
        }"#;
        fs::write(&input, otio).unwrap();

        let opts = OtiozImportOptions {
            input_file: input,
            output_dir: tmp.path().to_path_buf(),
            extract_media: false,
            generate_cpl: true,
            title: "My Timeline".to_string(),
            fps: 24.0,
        };

        let result = import_otioz(&opts).unwrap();
        assert!(result.generated_cpl.exists());
        let cpl = fs::read_to_string(&result.generated_cpl).unwrap();
        assert!(cpl.contains("My Timeline"));
        assert!(cpl.contains("24 1"));
    }

    #[test]
    fn test_invalid_extension() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("test.txt");
        fs::write(&input, "dummy").unwrap();

        let opts = OtiozImportOptions {
            input_file: input,
            output_dir: tmp.path().to_path_buf(),
            extract_media: false,
            generate_cpl: false,
            title: String::new(),
            fps: 24.0,
        };

        assert!(import_otioz(&opts).is_err());
    }

    #[test]
    fn test_missing_file() {
        let opts = OtiozImportOptions {
            input_file: PathBuf::from("/nonexistent.otioz"),
            output_dir: PathBuf::new(),
            extract_media: false,
            generate_cpl: false,
            title: String::new(),
            fps: 24.0,
        };

        assert!(import_otioz(&opts).is_err());
    }

    #[test]
    fn test_zip_parsing_invalid_file() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("bad.otioz");
        fs::write(&input, "not a zip file").unwrap();

        let opts = OtiozImportOptions {
            input_file: input,
            output_dir: tmp.path().to_path_buf(),
            extract_media: false,
            generate_cpl: false,
            title: String::new(),
            fps: 24.0,
        };

        assert!(import_otioz(&opts).is_err());
    }
}
