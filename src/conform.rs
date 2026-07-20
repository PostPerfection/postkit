use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConformError {
    #[error("Failed to read timeline: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "AAF import not implemented (AAF is a binary OLE/Structured-Storage format; \
         export the edit as CMX 3600 EDL or FCP7 XML instead)"
    )]
    AafNotImplemented,
    #[error("Unsupported timeline format: {0}")]
    Unsupported(String),
    #[error("XML parse error: {0}")]
    Xml(String),
    #[error("No edit events found in timeline")]
    NoEvents,
}

/// Timeline edit decision format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimelineFormat {
    EdlCmx3600,
    Aaf,
    /// Final Cut Pro XML
    XmlFcp,
    /// FCP X XML
    XmlFcpx,
    /// DaVinci Resolve XML
    XmlResolve,
    /// OpenTimelineIO
    Otio,
    Unknown,
}

/// A single edit event in a timeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EditEvent {
    pub event_number: u32,
    pub reel_name: String,
    /// "V", "A1", "A2", etc.
    pub track_type: String,
    /// Source in frame number
    pub source_in: u32,
    pub source_out: u32,
    pub record_in: u32,
    pub record_out: u32,
    /// "CUT", "DISSOLVE"
    pub transition: String,
    pub comment: String,
}

/// Parsed timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub title: String,
    pub frame_rate: f64,
    pub format: TimelineFormat,
    pub events: Vec<EditEvent>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            title: String::new(),
            frame_rate: 24.0,
            format: TimelineFormat::Unknown,
            events: Vec::new(),
        }
    }
}

/// Conform options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformOptions {
    /// EDL/AAF/XML path
    pub timeline_file: PathBuf,
    /// Directory containing source reels
    pub media_dir: PathBuf,
    /// Assembled output
    pub output_dir: PathBuf,
    pub auto_detect_format: bool,
    pub force_format: TimelineFormat,
    pub frame_rate: f64,
}

impl Default for ConformOptions {
    fn default() -> Self {
        Self {
            timeline_file: PathBuf::new(),
            media_dir: PathBuf::new(),
            output_dir: PathBuf::new(),
            auto_detect_format: true,
            force_format: TimelineFormat::Unknown,
            frame_rate: 24.0,
        }
    }
}

/// Parse a timeline file. EDL (CMX 3600) and FCP7/Resolve XML (xmeml) are
/// parsed for real. AAF fails loud (binary format, out of scope). OTIO lives in
/// the otioz_import module.
pub fn parse_timeline(file: &Path) -> Result<Timeline, ConformError> {
    match detect_timeline_format(file) {
        TimelineFormat::EdlCmx3600 => Ok(parse_edl(file)),
        TimelineFormat::XmlFcp | TimelineFormat::XmlFcpx | TimelineFormat::XmlResolve => {
            parse_xml_timeline(file)
        }
        TimelineFormat::Aaf => Err(ConformError::AafNotImplemented),
        TimelineFormat::Otio => Err(ConformError::Unsupported(
            "OTIO: use the otioz_import module".to_string(),
        )),
        TimelineFormat::Unknown => Err(ConformError::Unsupported(
            "unrecognised timeline file".to_string(),
        )),
    }
}

/// Accumulator for one xmeml `<clipitem>` while streaming through the document.
#[derive(Default)]
struct ClipAccum {
    reel_name: String,
    file_name: String,
    track_type: String,
    source_in: u32,
    source_out: u32,
    rec_in: u32,
    rec_out: u32,
}

/// Parse an XML timeline. Handles the FCP7 / DaVinci Resolve XML interchange
/// (xmeml), whose integer frame counts map directly onto EditEvent. FCP X
/// fcpxml uses rational-time strings and is not supported.
fn parse_xml_timeline(file: &Path) -> Result<Timeline, ConformError> {
    let content = std::fs::read_to_string(file)?;
    if content.contains("<xmeml") {
        parse_xmeml(&content)
    } else if content.contains("<fcpxml") {
        Err(ConformError::Unsupported(
            "FCPXML (Final Cut Pro X) not supported; export as FCP7 XML (xmeml)".to_string(),
        ))
    } else {
        Err(ConformError::Xml(
            "not a recognised XML timeline (expected an <xmeml> root)".to_string(),
        ))
    }
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

fn set_frame(cur: Option<&mut ClipAccum>, val: &str, f: impl FnOnce(&mut ClipAccum, u32)) {
    if let (Some(c), Ok(v)) = (cur, val.parse::<u32>()) {
        f(c, v);
    }
}

fn parse_xmeml(content: &str) -> Result<Timeline, ConformError> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(content);
    let mut stack: Vec<String> = Vec::new();
    let mut timeline = Timeline {
        format: TimelineFormat::XmlFcp,
        frame_rate: 0.0,
        ..Default::default()
    };
    // Track whether the current clipitem sits under <video> or <audio>.
    let mut track_kind = "V".to_string();
    let mut cur: Option<ClipAccum> = None;
    let mut event_number: u32 = 0;

    loop {
        match reader
            .read_event()
            .map_err(|e| ConformError::Xml(e.to_string()))?
        {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "video" => track_kind = "V".to_string(),
                    "audio" => track_kind = "A".to_string(),
                    "clipitem" => {
                        cur = Some(ClipAccum {
                            track_type: track_kind.clone(),
                            ..Default::default()
                        });
                    }
                    _ => {}
                }
                stack.push(name);
            }
            Event::Text(e) => {
                let val = e
                    .unescape()
                    .map_err(|err| ConformError::Xml(err.to_string()))?
                    .trim()
                    .to_string();
                if !val.is_empty() {
                    let cur_tag = stack.last().map(String::as_str).unwrap_or("");
                    let parent = if stack.len() >= 2 {
                        stack[stack.len() - 2].as_str()
                    } else {
                        ""
                    };
                    match cur_tag {
                        "name" => {
                            if let Some(c) = cur.as_mut() {
                                if parent == "clipitem" {
                                    c.reel_name = val;
                                } else if parent == "file" {
                                    c.file_name = val;
                                }
                            } else if parent == "sequence" {
                                timeline.title = val;
                            }
                        }
                        // first timebase seen is the sequence rate
                        "timebase" => {
                            if timeline.frame_rate == 0.0
                                && let Ok(tb) = val.parse::<f64>()
                            {
                                timeline.frame_rate = tb;
                            }
                        }
                        "start" => set_frame(cur.as_mut(), &val, |c, v| c.rec_in = v),
                        "end" => set_frame(cur.as_mut(), &val, |c, v| c.rec_out = v),
                        "in" => set_frame(cur.as_mut(), &val, |c, v| c.source_in = v),
                        "out" => set_frame(cur.as_mut(), &val, |c, v| c.source_out = v),
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                let name = local_name(e.name().as_ref());
                if name == "clipitem"
                    && let Some(c) = cur.take()
                {
                    event_number += 1;
                    let reel_name = if c.reel_name.is_empty() {
                        c.file_name
                    } else {
                        c.reel_name
                    };
                    timeline.events.push(EditEvent {
                        event_number,
                        reel_name,
                        track_type: c.track_type,
                        source_in: c.source_in,
                        source_out: c.source_out,
                        record_in: c.rec_in,
                        record_out: c.rec_out,
                        transition: "CUT".to_string(),
                        comment: String::new(),
                    });
                }
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if timeline.frame_rate == 0.0 {
        timeline.frame_rate = 24.0;
    }
    if timeline.events.is_empty() {
        return Err(ConformError::NoEvents);
    }
    Ok(timeline)
}

/// Parse a CMX 3600 EDL file.
fn parse_edl(file: &Path) -> Timeline {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to read EDL file: {e}");
            return Timeline::default();
        }
    };

    let mut timeline = Timeline {
        format: TimelineFormat::EdlCmx3600,
        ..Default::default()
    };

    // CMX 3600 format:
    // TITLE: <title>
    // FCM: DROP FRAME / NON-DROP FRAME
    // 001  REEL001  V  C  01:00:00:00 01:00:05:00 01:00:00:00 01:00:05:00
    let event_re = regex::Regex::new(
        r"^\s*(\d+)\s+(\S+)\s+(\S+)\s+(\S+)\s+(\d{2}:\d{2}:\d{2}[:;]\d{2})\s+(\d{2}:\d{2}:\d{2}[:;]\d{2})\s+(\d{2}:\d{2}:\d{2}[:;]\d{2})\s+(\d{2}:\d{2}:\d{2}[:;]\d{2})"
    ).unwrap();

    let mut last_comment = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(title) = trimmed.strip_prefix("TITLE:") {
            timeline.title = title.trim().to_string();
            continue;
        }

        if trimmed.starts_with("FCM:") {
            if trimmed.contains("DROP") && !trimmed.contains("NON") {
                timeline.frame_rate = 29.97;
            }
            continue;
        }

        if trimmed.starts_with('*') || trimmed.starts_with(';') {
            last_comment = trimmed[1..].trim().to_string();
            continue;
        }

        if let Some(caps) = event_re.captures(trimmed) {
            let event = EditEvent {
                event_number: caps[1].parse().unwrap_or(0),
                reel_name: caps[2].to_string(),
                track_type: caps[3].to_string(),
                source_in: tc_to_frames(&caps[5], timeline.frame_rate as u32),
                source_out: tc_to_frames(&caps[6], timeline.frame_rate as u32),
                record_in: tc_to_frames(&caps[7], timeline.frame_rate as u32),
                record_out: tc_to_frames(&caps[8], timeline.frame_rate as u32),
                transition: caps[4].to_string(),
                comment: std::mem::take(&mut last_comment),
            };
            timeline.events.push(event);
        }
    }

    timeline
}

fn tc_to_frames(tc: &str, fps: u32) -> u32 {
    let fps = if fps == 0 { 24 } else { fps };
    let tc = tc.replace(';', ":");
    let parts: Vec<&str> = tc.split(':').collect();
    if parts.len() != 4 {
        return 0;
    }
    let h: u32 = parts[0].parse().unwrap_or(0);
    let m: u32 = parts[1].parse().unwrap_or(0);
    let s: u32 = parts[2].parse().unwrap_or(0);
    let f: u32 = parts[3].parse().unwrap_or(0);
    h * 3600 * fps + m * 60 * fps + s * fps + f
}

/// Detect timeline format from file extension/content.
pub fn detect_timeline_format(file: &Path) -> TimelineFormat {
    match file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("edl") => TimelineFormat::EdlCmx3600,
        Some("aaf") => TimelineFormat::Aaf,
        Some("otio") => TimelineFormat::Otio,
        Some("xml" | "fcpxml") => TimelineFormat::XmlFcpx,
        _ => TimelineFormat::Unknown,
    }
}

/// Conform/assemble media from a timeline into reel structure.
///
/// Creates symlinks or copies source media files into the output directory
/// organised by reel, based on the timeline edit decisions.
pub fn conform(opts: &ConformOptions) -> i32 {
    let timeline = match parse_timeline(&opts.timeline_file) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to parse timeline: {e}");
            return -1;
        }
    };

    if timeline.events.is_empty() {
        tracing::error!("No events found in timeline");
        return -1;
    }

    // Create output directory
    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        tracing::error!("Failed to create output directory: {e}");
        return -1;
    }

    // Check for missing reels
    let missing = find_missing_reels(&timeline, &opts.media_dir);
    if !missing.is_empty() {
        for m in &missing {
            tracing::warn!("Missing reel: {m}");
        }
    }

    // Write assembled timeline as JSON for downstream tools
    let manifest_path = opts.output_dir.join("conform_manifest.json");
    let json = serde_json::to_string_pretty(&timeline).unwrap_or_default();
    if let Err(e) = std::fs::write(&manifest_path, json) {
        tracing::error!("Failed to write manifest: {e}");
        return -1;
    }

    tracing::info!(
        "Conformed {} events to {}",
        timeline.events.len(),
        opts.output_dir.display()
    );
    0
}

/// Verify that all source reels referenced in timeline exist in media_dir.
pub fn find_missing_reels(timeline: &Timeline, media_dir: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    let mut checked = std::collections::HashSet::new();

    for event in &timeline.events {
        if event.reel_name == "BL" || event.reel_name == "AX" {
            continue; // black/aux
        }
        if !checked.insert(&event.reel_name) {
            continue; // already checked
        }

        // Check if any file matching the reel name exists
        let found = std::fs::read_dir(media_dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.contains(&event.reel_name))
            });

        if !found {
            missing.push(event.reel_name.clone());
        }
    }

    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_edl() {
        let dir = tempfile::tempdir().unwrap();
        let edl_path = dir.path().join("test.edl");
        std::fs::write(
            &edl_path,
            "TITLE: Test Edit\nFCM: NON-DROP FRAME\n\n001  REEL001  V  C        01:00:00:00 01:00:05:00 01:00:00:00 01:00:05:00\n002  REEL002  V  C        01:00:05:00 01:00:10:00 01:00:05:00 01:00:10:00\n",
        ).unwrap();
        let tl = parse_timeline(&edl_path).unwrap();
        assert_eq!(tl.title, "Test Edit");
        assert_eq!(tl.events.len(), 2);
        assert_eq!(tl.events[0].reel_name, "REEL001");
        assert_eq!(tl.events[1].event_number, 2);
    }

    #[test]
    fn test_tc_to_frames() {
        assert_eq!(tc_to_frames("01:00:00:00", 24), 86400);
        assert_eq!(tc_to_frames("00:00:01:00", 24), 24);
        assert_eq!(tc_to_frames("00:00:00:12", 24), 12);
    }

    #[test]
    fn test_parse_xmeml() {
        let dir = tempfile::tempdir().unwrap();
        let xml_path = dir.path().join("timeline.xml");
        std::fs::write(
            &xml_path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<xmeml version="5">
  <sequence>
    <name>My Cut</name>
    <rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
    <media>
      <video>
        <track>
          <clipitem>
            <name>REEL001</name>
            <rate><timebase>25</timebase></rate>
            <start>0</start>
            <end>120</end>
            <in>86400</in>
            <out>86520</out>
            <file id="f1"><name>REEL001.mov</name></file>
          </clipitem>
          <clipitem>
            <name>REEL002</name>
            <start>120</start>
            <end>240</end>
            <in>0</in>
            <out>120</out>
          </clipitem>
        </track>
      </video>
      <audio>
        <track>
          <clipitem>
            <name>A_MIX</name>
            <start>0</start>
            <end>240</end>
            <in>0</in>
            <out>240</out>
          </clipitem>
        </track>
      </audio>
    </media>
  </sequence>
</xmeml>"#,
        )
        .unwrap();

        let tl = parse_timeline(&xml_path).unwrap();
        assert_eq!(tl.format, TimelineFormat::XmlFcp);
        assert_eq!(tl.title, "My Cut");
        assert_eq!(tl.frame_rate, 25.0);
        assert_eq!(tl.events.len(), 3);

        assert_eq!(tl.events[0].event_number, 1);
        assert_eq!(tl.events[0].reel_name, "REEL001");
        assert_eq!(tl.events[0].track_type, "V");
        assert_eq!(tl.events[0].source_in, 86400);
        assert_eq!(tl.events[0].source_out, 86520);
        assert_eq!(tl.events[0].record_in, 0);
        assert_eq!(tl.events[0].record_out, 120);

        assert_eq!(tl.events[1].reel_name, "REEL002");
        assert_eq!(tl.events[2].reel_name, "A_MIX");
        assert_eq!(tl.events[2].track_type, "A");
    }

    #[test]
    fn test_parse_aaf_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let aaf_path = dir.path().join("edit.aaf");
        std::fs::write(&aaf_path, b"\xd0\xcf\x11\xe0garbage").unwrap();
        let err = parse_timeline(&aaf_path).unwrap_err();
        assert!(matches!(err, ConformError::AafNotImplemented));
    }

    #[test]
    fn test_parse_fcpxml_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proj.fcpxml");
        std::fs::write(
            &path,
            r#"<?xml version="1.0"?><fcpxml version="1.10"></fcpxml>"#,
        )
        .unwrap();
        assert!(matches!(
            parse_timeline(&path),
            Err(ConformError::Unsupported(_))
        ));
    }

    #[test]
    fn test_detect_format() {
        assert_eq!(
            detect_timeline_format(Path::new("test.edl")),
            TimelineFormat::EdlCmx3600
        );
        assert_eq!(
            detect_timeline_format(Path::new("test.aaf")),
            TimelineFormat::Aaf
        );
    }

    #[test]
    fn test_find_missing_reels() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("REEL001.mxf"), "").unwrap();
        let tl = Timeline {
            events: vec![
                EditEvent {
                    reel_name: "REEL001".into(),
                    ..Default::default()
                },
                EditEvent {
                    reel_name: "REEL002".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let missing = find_missing_reels(&tl, dir.path());
        assert_eq!(missing, vec!["REEL002"]);
    }
}
