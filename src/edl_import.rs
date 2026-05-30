//! Import Edit Decision Lists (EDL) from CMX 3600 and FCP XML formats.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EdlError {
    #[error("File not found: {0}")]
    NotFound(PathBuf),
    #[error("Cannot open file: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Format not supported: {0}")]
    UnsupportedFormat(String),
    #[error("No clips found")]
    NoClips,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EdlFormat {
    CmxEdl,
    Aaf,
    FcpXml,
    Otio,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditEvent {
    pub index: u32,
    pub reel_name: String,
    pub src_in: u32,
    pub src_out: u32,
    pub rec_in: u32,
    pub rec_out: u32,
    pub track_type: String,
    pub transition: String,
    pub source_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct EdlParseOptions {
    pub input_file: PathBuf,
    pub format: EdlFormat,
    pub fps_num: u32,
    pub fps_den: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdlParseResult {
    pub events: Vec<EditEvent>,
    pub title: String,
    pub fps: f64,
    pub total_frames: u32,
}

/// Auto-detect EDL format from file extension.
pub fn detect_edl_format(path: &Path) -> EdlFormat {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "edl" => EdlFormat::CmxEdl,
        "aaf" => EdlFormat::Aaf,
        "xml" | "fcpxml" => EdlFormat::FcpXml,
        "otio" => EdlFormat::Otio,
        _ => EdlFormat::CmxEdl,
    }
}

/// Parse an EDL file into a list of edit events.
pub fn parse_edl(opts: &EdlParseOptions) -> Result<EdlParseResult, EdlError> {
    if !opts.input_file.exists() {
        return Err(EdlError::NotFound(opts.input_file.clone()));
    }

    let fps = f64::from(opts.fps_num) / f64::from(opts.fps_den);

    match opts.format {
        EdlFormat::CmxEdl => parse_cmx_edl(&opts.input_file, fps),
        EdlFormat::FcpXml => parse_fcp_xml(&opts.input_file, fps),
        EdlFormat::Aaf => Err(EdlError::UnsupportedFormat("AAF".to_string())),
        EdlFormat::Otio => Err(EdlError::UnsupportedFormat("OTIO".to_string())),
    }
}

fn timecode_to_frames(tc: &str, fps: f64) -> u32 {
    let parts: Vec<&str> = tc.split(':').collect();
    if parts.len() == 4 {
        let h: u32 = parts[0].parse().unwrap_or(0);
        let m: u32 = parts[1].parse().unwrap_or(0);
        let s: u32 = parts[2].parse().unwrap_or(0);
        let f: u32 = parts[3].parse().unwrap_or(0);
        ((h * 3600 + m * 60 + s) as f64 * fps) as u32 + f
    } else {
        0
    }
}

fn parse_cmx_edl(file: &Path, fps: f64) -> Result<EdlParseResult, EdlError> {
    let content = fs::read_to_string(file)?;
    let mut lines = content.lines();

    let mut title = String::new();
    if let Some(first_line) = lines.next()
        && let Some(pos) = first_line.find("TITLE:")
    {
        title = first_line[pos + 6..].trim().to_string();
    }

    let mut events = Vec::new();

    for line in lines {
        if let Some(event) = parse_cmx_event_line(line, fps) {
            events.push(event);
        }
    }

    let total_frames = events.last().map(|e| e.rec_out).unwrap_or(0);

    Ok(EdlParseResult {
        events,
        title,
        fps,
        total_frames,
    })
}

fn parse_cmx_event_line(line: &str, fps: f64) -> Option<EditEvent> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 8 {
        return None;
    }

    let index: u32 = parts[0].parse().ok()?;

    let tc_pattern = |s: &str| -> bool { s.len() == 11 && s.chars().nth(2) == Some(':') };
    if !tc_pattern(parts[4])
        || !tc_pattern(parts[5])
        || !tc_pattern(parts[6])
        || !tc_pattern(parts[7])
    {
        return None;
    }

    Some(EditEvent {
        index,
        reel_name: parts[1].to_string(),
        track_type: parts[2].to_string(),
        transition: parts[3].to_string(),
        src_in: timecode_to_frames(parts[4], fps),
        src_out: timecode_to_frames(parts[5], fps),
        rec_in: timecode_to_frames(parts[6], fps),
        rec_out: timecode_to_frames(parts[7], fps),
        source_file: PathBuf::new(),
    })
}

fn parse_fcp_xml(file: &Path, fps: f64) -> Result<EdlParseResult, EdlError> {
    let content = fs::read_to_string(file)?;

    let title = extract_tag_value(&content, "name").unwrap_or_default();

    let mut events = Vec::new();
    let mut rec_pos: u32 = 0;
    let mut idx: u32 = 1;

    for clip_block in content.split("<clip").skip(1) {
        let Some(end_pos) = clip_block.find("</clip>") else {
            continue;
        };
        let block = &clip_block[..end_pos];

        let name = extract_tag_value(block, "name").unwrap_or_default();
        let start: u32 = extract_tag_value(block, "start")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let end: u32 = extract_tag_value(block, "end")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let duration = end.saturating_sub(start);
        events.push(EditEvent {
            index: idx,
            reel_name: name,
            src_in: start,
            src_out: end,
            rec_in: rec_pos,
            rec_out: rec_pos + duration,
            track_type: "V".to_string(),
            transition: "Cut".to_string(),
            source_file: PathBuf::new(),
        });
        rec_pos += duration;
        idx += 1;
    }

    if events.is_empty() {
        return Err(EdlError::NoClips);
    }

    Ok(EdlParseResult {
        events,
        title,
        fps,
        total_frames: rec_pos,
    })
}

fn extract_tag_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_detect_edl_format() {
        assert_eq!(detect_edl_format(Path::new("test.edl")), EdlFormat::CmxEdl);
        assert_eq!(detect_edl_format(Path::new("test.EDL")), EdlFormat::CmxEdl);
        assert_eq!(
            detect_edl_format(Path::new("test.fcpxml")),
            EdlFormat::FcpXml
        );
        assert_eq!(detect_edl_format(Path::new("test.xml")), EdlFormat::FcpXml);
        assert_eq!(detect_edl_format(Path::new("test.aaf")), EdlFormat::Aaf);
        assert_eq!(detect_edl_format(Path::new("test.otio")), EdlFormat::Otio);
        assert_eq!(detect_edl_format(Path::new("test.txt")), EdlFormat::CmxEdl);
    }

    #[test]
    fn test_timecode_to_frames() {
        assert_eq!(timecode_to_frames("01:00:00:00", 24.0), 86400);
        assert_eq!(timecode_to_frames("00:00:01:00", 24.0), 24);
        assert_eq!(timecode_to_frames("00:00:01:12", 24.0), 36);
        assert_eq!(timecode_to_frames("00:00:00:01", 24.0), 1);
    }

    #[test]
    fn test_parse_cmx_edl() {
        let tmp = TempDir::new().unwrap();
        let edl_file = tmp.path().join("test.edl");

        let content = "TITLE: My Project\n\
            001  REEL1    V     C        01:00:00:00 01:00:05:00 00:00:00:00 00:00:05:00\n\
            002  REEL1    V     C        01:00:05:00 01:00:10:00 00:00:05:00 00:00:10:00\n";

        fs::write(&edl_file, content).unwrap();

        let opts = EdlParseOptions {
            input_file: edl_file,
            format: EdlFormat::CmxEdl,
            fps_num: 24,
            fps_den: 1,
        };

        let result = parse_edl(&opts).unwrap();
        assert_eq!(result.title, "My Project");
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].index, 1);
        assert_eq!(result.events[0].reel_name, "REEL1");
        assert_eq!(result.events[0].src_in, 86400);
        assert_eq!(result.events[0].rec_in, 0);
        assert_eq!(result.events[1].rec_out, 240);
        assert_eq!(result.total_frames, 240);
    }

    #[test]
    fn test_parse_fcp_xml() {
        let tmp = TempDir::new().unwrap();
        let xml_file = tmp.path().join("test.fcpxml");

        let content = r#"<?xml version="1.0"?>
<fcpxml>
  <project>
    <name>Test Project</name>
    <sequence>
      <clip>
        <name>Shot_01</name>
        <start>0</start>
        <end>120</end>
      </clip>
      <clip>
        <name>Shot_02</name>
        <start>0</start>
        <end>240</end>
      </clip>
    </sequence>
  </project>
</fcpxml>"#;
        fs::write(&xml_file, content).unwrap();

        let opts = EdlParseOptions {
            input_file: xml_file,
            format: EdlFormat::FcpXml,
            fps_num: 24,
            fps_den: 1,
        };

        let result = parse_edl(&opts).unwrap();
        assert_eq!(result.title, "Test Project");
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].reel_name, "Shot_01");
        assert_eq!(result.events[0].src_in, 0);
        assert_eq!(result.events[0].src_out, 120);
        assert_eq!(result.events[0].rec_in, 0);
        assert_eq!(result.events[0].rec_out, 120);
        assert_eq!(result.events[1].rec_in, 120);
        assert_eq!(result.events[1].rec_out, 360);
        assert_eq!(result.total_frames, 360);
    }

    #[test]
    fn test_unsupported_format() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("test.aaf");
        fs::write(&file, "dummy").unwrap();

        let opts = EdlParseOptions {
            input_file: file,
            format: EdlFormat::Aaf,
            fps_num: 24,
            fps_den: 1,
        };

        let result = parse_edl(&opts);
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_file() {
        let opts = EdlParseOptions {
            input_file: PathBuf::from("/nonexistent/file.edl"),
            format: EdlFormat::CmxEdl,
            fps_num: 24,
            fps_den: 1,
        };

        assert!(parse_edl(&opts).is_err());
    }

    #[test]
    fn test_empty_fcp_xml() {
        let tmp = TempDir::new().unwrap();
        let xml_file = tmp.path().join("empty.fcpxml");
        fs::write(&xml_file, "<fcpxml><name>Empty</name></fcpxml>").unwrap();

        let opts = EdlParseOptions {
            input_file: xml_file,
            format: EdlFormat::FcpXml,
            fps_num: 24,
            fps_den: 1,
        };

        let result = parse_edl(&opts);
        assert!(result.is_err());
    }

    #[test]
    fn test_cmx_event_parsing_details() {
        let line = "003  BL       V     D     01:00:10:00 01:00:15:12 00:00:10:00 00:00:15:12";
        let event = parse_cmx_event_line(line, 24.0).unwrap();
        assert_eq!(event.index, 3);
        assert_eq!(event.reel_name, "BL");
        assert_eq!(event.track_type, "V");
        assert_eq!(event.transition, "D");
        assert_eq!(event.src_out, timecode_to_frames("01:00:15:12", 24.0));
    }
}
