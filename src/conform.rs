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

/// Parse a timeline file. EDL (CMX 3600), FCP7/Resolve XML (xmeml) and FCP X
/// fcpxml are parsed for real. AAF fails loud (binary format, out of scope).
/// OTIO lives in the otioz_import module.
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
/// (xmeml), whose integer frame counts map directly onto EditEvent, and FCP X
/// fcpxml, whose rational-second times are converted to frames with the
/// sequence frame rate.
fn parse_xml_timeline(file: &Path) -> Result<Timeline, ConformError> {
    let content = std::fs::read_to_string(file)?;
    if content.contains("<xmeml") {
        parse_xmeml(&content)
    } else if content.contains("<fcpxml") {
        parse_fcpxml(&content)
    } else {
        Err(ConformError::Xml(
            "not a recognised XML timeline (expected an <xmeml> or <fcpxml> root)".to_string(),
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

const FCPXML_FALLBACK_FRAME_RATE: f64 = 24.0;
const UNNAMED_REEL_NAME: &str = "AX";

/// An `<asset>` declared in an fcpxml `<resources>` block.
#[derive(Default)]
struct FcpxmlAsset {
    name: String,
    has_video: bool,
    has_audio: bool,
}

/// A clip on the primary storyline, times kept as raw rational-second strings
/// until the sequence frame rate is known.
struct FcpxmlClip {
    element_name: String,
    asset_reference: String,
    clip_name: String,
    offset: String,
    duration: String,
    start: String,
    /// srcEnable: "all", "audio" or "video".
    enabled_sources: String,
}

/// Parse an fcpxml rational-second time ("0s", "5s", "1001/24000s") to seconds.
fn parse_fcpxml_seconds(value: &str) -> Option<f64> {
    let value = value.trim().strip_suffix('s')?;
    match value.split_once('/') {
        Some((numerator, denominator)) => {
            let numerator: f64 = numerator.trim().parse().ok()?;
            let denominator: f64 = denominator.trim().parse().ok()?;
            if denominator == 0.0 {
                return None;
            }
            Some(numerator / denominator)
        }
        None => value.parse().ok(),
    }
}

fn seconds_to_frames(seconds: f64, frame_rate: f64) -> u32 {
    (seconds * frame_rate).round() as u32
}

fn attribute(element: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    element.attributes().flatten().find_map(|a| {
        if local_name(a.key.as_ref()) != name {
            return None;
        }
        a.unescape_value().ok().map(|v| v.into_owned())
    })
}

fn attribute_flag(element: &quick_xml::events::BytesStart, name: &str) -> bool {
    attribute(element, name).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn parse_fcpxml(content: &str) -> Result<Timeline, ConformError> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    use std::collections::HashMap;

    let mut reader = Reader::from_str(content);
    let mut frame_rate_by_format_id: HashMap<String, f64> = HashMap::new();
    let mut asset_by_id: HashMap<String, FcpxmlAsset> = HashMap::new();
    let mut clips: Vec<FcpxmlClip> = Vec::new();
    let mut sequence_format_id = String::new();
    let mut title = String::new();
    let mut depth: usize = 0;
    let mut spine_depth: Option<usize> = None;
    let mut spine_seen = false;
    // compound clips are defined in <resources> with a spine of their own
    let mut resources_depth: Option<usize> = None;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| ConformError::Xml(e.to_string()))?;

        if let Event::Start(element) | Event::Empty(element) = &event {
            let name = local_name(element.name().as_ref());
            match name.as_str() {
                "format" => {
                    if let (Some(id), Some(rate)) = (
                        attribute(element, "id"),
                        attribute(element, "frameDuration")
                            .as_deref()
                            .and_then(parse_fcpxml_seconds)
                            .filter(|seconds| *seconds > 0.0)
                            .map(|seconds| 1.0 / seconds),
                    ) {
                        frame_rate_by_format_id.insert(id, rate);
                    }
                }
                "asset" => {
                    if let Some(id) = attribute(element, "id") {
                        asset_by_id.insert(
                            id,
                            FcpxmlAsset {
                                name: attribute(element, "name").unwrap_or_default(),
                                has_video: attribute_flag(element, "hasVideo"),
                                has_audio: attribute_flag(element, "hasAudio"),
                            },
                        );
                    }
                }
                "project" => {
                    if title.is_empty() {
                        title = attribute(element, "name").unwrap_or_default();
                    }
                }
                "resources" => {
                    if resources_depth.is_none() && matches!(&event, Event::Start(_)) {
                        resources_depth = Some(depth);
                    }
                }
                "sequence" => {
                    if resources_depth.is_none() && sequence_format_id.is_empty() {
                        sequence_format_id = attribute(element, "format").unwrap_or_default();
                    }
                }
                "spine" => {
                    if resources_depth.is_none() && !spine_seen && matches!(&event, Event::Start(_))
                    {
                        spine_seen = true;
                        spine_depth = Some(depth);
                    }
                }
                "asset-clip" | "video" | "audio"
                    if spine_depth.is_some_and(|spine| depth == spine + 1) =>
                {
                    clips.push(FcpxmlClip {
                        element_name: name,
                        asset_reference: attribute(element, "ref").unwrap_or_default(),
                        clip_name: attribute(element, "name").unwrap_or_default(),
                        offset: attribute(element, "offset").unwrap_or_default(),
                        duration: attribute(element, "duration").unwrap_or_default(),
                        start: attribute(element, "start").unwrap_or_default(),
                        enabled_sources: attribute(element, "srcEnable").unwrap_or_default(),
                    });
                }
                _ => {}
            }
        }

        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if spine_depth == Some(depth) {
                    spine_depth = None;
                }
                if resources_depth == Some(depth) {
                    resources_depth = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let frame_rate = frame_rate_by_format_id
        .get(&sequence_format_id)
        .copied()
        .unwrap_or(FCPXML_FALLBACK_FRAME_RATE);

    let events = clips
        .iter()
        .enumerate()
        .map(|(index, clip)| {
            let asset = asset_by_id.get(&clip.asset_reference);
            let seconds = |value: &str| parse_fcpxml_seconds(value).unwrap_or(0.0);
            let record_in = seconds_to_frames(seconds(&clip.offset), frame_rate);
            let source_in = seconds_to_frames(seconds(&clip.start), frame_rate);
            let length = seconds_to_frames(seconds(&clip.duration), frame_rate);
            let reel_name = [
                asset.map(|a| a.name.as_str()).unwrap_or_default(),
                clip.clip_name.as_str(),
                clip.asset_reference.as_str(),
            ]
            .into_iter()
            .find(|candidate| !candidate.is_empty())
            .unwrap_or(UNNAMED_REEL_NAME)
            .to_string();
            let track_type = match clip.element_name.as_str() {
                "video" => "V",
                "audio" => "A",
                _ => match (clip.enabled_sources.as_str(), asset) {
                    ("audio", _) => "A",
                    ("video", _) => "V",
                    (_, Some(a)) if a.has_audio && !a.has_video => "A",
                    _ => "V",
                },
            };
            EditEvent {
                event_number: index as u32 + 1,
                reel_name,
                track_type: track_type.to_string(),
                source_in,
                source_out: source_in + length,
                record_in,
                record_out: record_in + length,
                transition: "CUT".to_string(),
                comment: String::new(),
            }
        })
        .collect::<Vec<_>>();

    if events.is_empty() {
        return Err(ConformError::NoEvents);
    }

    Ok(Timeline {
        title,
        frame_rate,
        format: TimelineFormat::XmlFcpx,
        events,
    })
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

    fn write_timeline(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    #[test]
    fn test_parse_fcpxml_seconds() {
        assert_eq!(parse_fcpxml_seconds("0s"), Some(0.0));
        assert_eq!(parse_fcpxml_seconds("5s"), Some(5.0));
        assert_eq!(parse_fcpxml_seconds("100/2400s"), Some(1.0 / 24.0));
        let ntsc = parse_fcpxml_seconds("1001/24000s").unwrap();
        assert!((ntsc - 1001.0 / 24000.0).abs() < 1e-12);
        assert_eq!(parse_fcpxml_seconds("1/0s"), None);
        assert_eq!(parse_fcpxml_seconds("notatime"), None);
        assert_eq!(parse_fcpxml_seconds("12"), None);
        assert_eq!(parse_fcpxml_seconds(""), None);
    }

    #[test]
    fn test_parse_fcpxml() {
        // 24 fps sequence. Clip one sits at 1s for 5s, sourced from 150s in.
        // Clip two sits at 7s for 3s with no explicit source start.
        let (_dir, path) = write_timeline(
            "proj.fcpxml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
  <resources>
    <format id="r1" name="FFVideoFormat1080p24" frameDuration="100/2400s" width="1920" height="1080"/>
    <asset id="r2" name="REEL001" start="0s" duration="3600/24s" hasVideo="1" hasAudio="1" format="r1">
      <media-rep kind="original-media" src="file:///media/REEL001.mov"/>
    </asset>
    <asset id="r3" name="REEL002" start="0s" duration="1200/24s" hasVideo="1" format="r1"/>
    <media id="r4" name="Compound Clip">
      <sequence format="r1" duration="48/24s">
        <spine>
          <asset-clip ref="r3" offset="0s" name="Nested" duration="48/24s"/>
        </spine>
      </sequence>
    </media>
  </resources>
  <library>
    <event name="Test Event">
      <project name="My FCPX Cut">
        <sequence format="r1" duration="240/24s" tcStart="0s" tcFormat="NDF">
          <spine>
            <gap name="Gap" offset="0s" duration="24/24s"/>
            <asset-clip ref="r2" offset="24/24s" name="Clip One" start="3600/24s" duration="120/24s" format="r1"/>
            <title name="Card" offset="144/24s" duration="24/24s"/>
            <asset-clip ref="r3" offset="168/24s" name="Clip Two" duration="72/24s" format="r1"/>
          </spine>
        </sequence>
      </project>
    </event>
  </library>
</fcpxml>"#,
        );

        let tl = parse_timeline(&path).unwrap();
        assert_eq!(tl.format, TimelineFormat::XmlFcpx);
        assert_eq!(tl.title, "My FCPX Cut");
        assert_eq!(tl.frame_rate, 24.0);
        assert_eq!(tl.events.len(), 2);

        assert_eq!(tl.events[0].event_number, 1);
        assert_eq!(tl.events[0].reel_name, "REEL001");
        assert_eq!(tl.events[0].track_type, "V");
        assert_eq!(tl.events[0].transition, "CUT");
        assert_eq!(tl.events[0].record_in, 24);
        assert_eq!(tl.events[0].record_out, 144);
        assert_eq!(tl.events[0].source_in, 3600);
        assert_eq!(tl.events[0].source_out, 3720);

        assert_eq!(tl.events[1].event_number, 2);
        assert_eq!(tl.events[1].reel_name, "REEL002");
        assert_eq!(tl.events[1].record_in, 168);
        assert_eq!(tl.events[1].record_out, 240);
        assert_eq!(tl.events[1].source_in, 0);
        assert_eq!(tl.events[1].source_out, 72);
    }

    #[test]
    fn test_parse_fcpxml_track_types() {
        let (_dir, path) = write_timeline(
            "av.fcpxml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.11">
  <resources>
    <format id="r1" frameDuration="1001/24000s" width="1920" height="1080"/>
    <asset id="r2" name="PICTURE" hasVideo="1" videoSources="1" format="r1">
      <media-rep kind="original-media" src="file:///media/PICTURE.mov"/>
    </asset>
    <asset id="r3" name="DIALOGUE" hasAudio="1" audioSources="1" audioChannels="2" audioRate="48000">
      <media-rep kind="original-media" src="file:///media/DIALOGUE.wav"/>
    </asset>
    <asset id="r4" name="SYNC_TAKE" hasVideo="1" hasAudio="1" audioSources="1" format="r1">
      <media-rep kind="original-media" src="file:///media/SYNC_TAKE.mov"/>
    </asset>
  </resources>
  <project name="AV Cut">
    <sequence format="r1" duration="168168/24000s" tcStart="0s" tcFormat="NDF" audioLayout="stereo" audioRate="48k">
      <spine>
        <asset-clip ref="r2" offset="0s" name="Picture" duration="48048/24000s"/>
        <asset-clip ref="r3" offset="48048/24000s" name="Dialogue" duration="48048/24000s"/>
        <audio ref="r3" offset="96096/24000s" name="Room Tone" duration="24024/24000s" role="dialogue"/>
        <video ref="r2" offset="120120/24000s" name="Insert" duration="24024/24000s"/>
        <asset-clip ref="r4" offset="144144/24000s" name="Wild Track" duration="24024/24000s" srcEnable="audio"/>
      </spine>
    </sequence>
  </project>
</fcpxml>"#,
        );

        let tl = parse_timeline(&path).unwrap();
        assert_eq!(tl.title, "AV Cut");
        assert!((tl.frame_rate - 24000.0 / 1001.0).abs() < 1e-9);
        let track_types: Vec<&str> = tl.events.iter().map(|e| e.track_type.as_str()).collect();
        assert_eq!(track_types, vec!["V", "A", "A", "V", "A"]);
        assert_eq!(tl.events[2].reel_name, "DIALOGUE");
        assert_eq!(tl.events[2].record_in, 96);
        assert_eq!(tl.events[2].record_out, 120);
        assert_eq!(tl.events[4].reel_name, "SYNC_TAKE");
    }

    #[test]
    fn test_parse_fcpxml_resolve_export() {
        // FCPXML 1.8 shape: src on the asset itself, unreduced "0/1s" times.
        // 30 fps, so 3600s = 108000 frames and 301/30s = 301 frames.
        let (_dir, path) = write_timeline(
            "resolve.fcpxml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE fcpxml>
<fcpxml version="1.8">
  <resources>
    <format name="FFVideoFormat1080p30" frameDuration="1/30s" id="r0" height="1080" width="1920"/>
    <asset hasVideo="1" name="demo.mp4" hasAudio="1" audioSources="1" audioChannels="1" id="r1" duration="301/30s" start="0/1s" src="file:///media/V1-0001_demo.mov" format="r0"/>
    <asset hasVideo="1" name="demo.mp4" hasAudio="1" audioSources="1" audioChannels="1" id="r2" duration="39/1s" start="0/1s" src="file:///media/V1-0002_demo.mov" format="r0"/>
  </resources>
  <library>
    <event name="Timeline 1 (Resolve)">
      <project name="Timeline 1 (Resolve)">
        <sequence tcFormat="NDF" tcStart="3600/1s" duration="2641/30s" format="r0">
          <spine>
            <asset-clip name="demo.mp4" tcFormat="NDF" duration="301/30s" ref="r1" start="0/1s" enabled="1" offset="3600/1s" format="r0"/>
            <asset-clip name="demo.mp4" tcFormat="NDF" duration="39/1s" ref="r2" start="0/1s" enabled="1" offset="108301/30s" format="r0"/>
          </spine>
        </sequence>
      </project>
    </event>
  </library>
</fcpxml>"#,
        );

        let tl = parse_timeline(&path).unwrap();
        assert_eq!(tl.format, TimelineFormat::XmlFcpx);
        assert_eq!(tl.frame_rate, 30.0);
        assert_eq!(tl.events.len(), 2);
        assert_eq!(tl.events[0].record_in, 108000);
        assert_eq!(tl.events[0].record_out, 108301);
        assert_eq!(tl.events[0].source_in, 0);
        assert_eq!(tl.events[0].source_out, 301);
        assert_eq!(tl.events[1].record_in, 108301);
        assert_eq!(tl.events[1].record_out, 109471);
        assert_eq!(tl.events[1].reel_name, "demo.mp4");
    }

    #[test]
    fn test_parse_fcpxml_without_clips() {
        let (_dir, path) = write_timeline(
            "empty.fcpxml",
            r#"<?xml version="1.0"?><fcpxml version="1.10"></fcpxml>"#,
        );
        assert!(matches!(parse_timeline(&path), Err(ConformError::NoEvents)));
    }

    #[test]
    fn test_parse_unknown_xml_root() {
        let (_dir, path) = write_timeline(
            "other.xml",
            r#"<?xml version="1.0"?><timeline><clip/></timeline>"#,
        );
        assert!(matches!(parse_timeline(&path), Err(ConformError::Xml(_))));
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
