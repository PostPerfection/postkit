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
    /// Reported by an application that does read AAF, since the timeline types
    /// it fills in are these.
    #[error("AAF import failed: {0}")]
    Aaf(String),
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
    /// fcpxml lane the clip came from: 0 is the primary storyline, positive is
    /// connected above it, negative below. Always 0 for EDL and xmeml.
    #[serde(default)]
    pub lane: i32,
}

/// Parsed timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub title: String,
    pub frame_rate: f64,
    pub format: TimelineFormat,
    pub events: Vec<EditEvent>,
    /// Timeline constructs that carry no source clip a reel plan could hold,
    /// one line each with the reason. Never silently dropped.
    #[serde(default)]
    pub skipped: Vec<String>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            title: String::new(),
            frame_rate: 24.0,
            format: TimelineFormat::Unknown,
            events: Vec::new(),
            skipped: Vec::new(),
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
                        ..Default::default()
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
/// A compound clip chain longer than this is a mistake or a reference cycle the
/// per-branch check cannot see.
const FCPXML_MAX_NESTING_DEPTH: usize = 16;

/// An `<asset>` declared in an fcpxml `<resources>` block.
#[derive(Default)]
struct FcpxmlAsset {
    name: String,
    start_seconds: f64,
    has_video: bool,
    has_audio: bool,
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
    (seconds * frame_rate).round().max(0.0) as u32
}

/// One fcpxml element with its attributes and children. Timeline content nests
/// (clips connected in lanes, compound clips holding a sequence of their own),
/// so the document is read into a tree and walked rather than streamed.
struct FcpxmlElement {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<FcpxmlElement>,
}

impl FcpxmlElement {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn flag(&self, name: &str) -> bool {
        self.attribute(name)
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    }

    fn seconds(&self, name: &str) -> Option<f64> {
        self.attribute(name).and_then(parse_fcpxml_seconds)
    }

    fn lane(&self) -> i32 {
        self.attribute("lane")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0)
    }

    fn child(&self, name: &str) -> Option<&FcpxmlElement> {
        self.children.iter().find(|child| child.name == name)
    }
}

fn fcpxml_element_head(element: &quick_xml::events::BytesStart) -> FcpxmlElement {
    FcpxmlElement {
        name: local_name(element.name().as_ref()),
        attributes: element
            .attributes()
            .flatten()
            .filter_map(|attribute| {
                let value = attribute.unescape_value().ok()?.into_owned();
                Some((local_name(attribute.key.as_ref()), value))
            })
            .collect(),
        children: Vec::new(),
    }
}

/// Read the whole document into a tree. Text nodes are dropped: fcpxml carries
/// everything this parser needs in attributes.
fn read_fcpxml_tree(content: &str) -> Result<FcpxmlElement, ConformError> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(content);
    let mut open = vec![FcpxmlElement {
        name: "fcpxml-document".to_string(),
        attributes: Vec::new(),
        children: Vec::new(),
    }];

    loop {
        match reader
            .read_event()
            .map_err(|e| ConformError::Xml(e.to_string()))?
        {
            Event::Start(element) => open.push(fcpxml_element_head(&element)),
            Event::Empty(element) => {
                let leaf = fcpxml_element_head(&element);
                match open.last_mut() {
                    Some(parent) => parent.children.push(leaf),
                    None => return Err(ConformError::Xml("element outside the root".to_string())),
                }
            }
            Event::End(_) => {
                let finished = open.pop();
                match (finished, open.last_mut()) {
                    (Some(finished), Some(parent)) => parent.children.push(finished),
                    _ => {
                        return Err(ConformError::Xml(
                            "closing tag with no matching open tag".to_string(),
                        ));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    match open.len() {
        1 => open
            .pop()
            .ok_or_else(|| ConformError::Xml("empty document".to_string())),
        remaining => Err(ConformError::Xml(format!(
            "{} unclosed element(s)",
            remaining - 1
        ))),
    }
}

/// The `<resources>` declarations a clip can point at: formats carry the frame
/// rate, assets the media name, media elements the sequence behind a compound
/// clip.
#[derive(Default)]
struct FcpxmlResources<'a> {
    frame_rate_by_format_id: std::collections::HashMap<String, f64>,
    asset_by_id: std::collections::HashMap<String, FcpxmlAsset>,
    media_by_id: std::collections::HashMap<String, &'a FcpxmlElement>,
}

fn collect_fcpxml_resources<'a>(element: &'a FcpxmlElement, into: &mut FcpxmlResources<'a>) {
    for child in &element.children {
        match child.name.as_str() {
            "format" => {
                if let (Some(id), Some(rate)) = (
                    child.attribute("id"),
                    child
                        .seconds("frameDuration")
                        .filter(|seconds| *seconds > 0.0)
                        .map(|seconds| 1.0 / seconds),
                ) {
                    into.frame_rate_by_format_id.insert(id.to_string(), rate);
                }
            }
            "asset" => {
                if let Some(id) = child.attribute("id") {
                    into.asset_by_id.insert(
                        id.to_string(),
                        FcpxmlAsset {
                            name: child.attribute("name").unwrap_or_default().to_string(),
                            start_seconds: child.seconds("start").unwrap_or(0.0),
                            has_video: child.flag("hasVideo"),
                            has_audio: child.flag("hasAudio"),
                        },
                    );
                }
            }
            "media" => {
                if let Some(id) = child.attribute("id") {
                    into.media_by_id.insert(id.to_string(), child);
                }
            }
            _ => {}
        }
        collect_fcpxml_resources(child, into);
    }
}

/// Depth-first search for the first `<name>` element outside `<resources>`, so
/// a compound clip's own sequence is never mistaken for the project's.
fn find_fcpxml_element<'a>(element: &'a FcpxmlElement, name: &str) -> Option<&'a FcpxmlElement> {
    for child in &element.children {
        if child.name == "resources" {
            continue;
        }
        if child.name == name {
            return Some(child);
        }
        if let Some(found) = find_fcpxml_element(child, name) {
            return Some(found);
        }
    }
    None
}

/// What a timeline element contributes to a flat reel plan.
enum FcpxmlElementKind {
    /// References an `<asset>`, so it becomes one edit event. May carry clips
    /// connected to it in lanes.
    AssetBacked,
    /// Holds its content in its children and contributes nothing itself.
    Container,
    /// `<ref-clip>`: stands in for the sequence of the `<media>` it references.
    CompoundReference,
    /// Empty time, but connected clips still hang off it.
    Gap,
    /// Nothing in a reel plan can stand in for it. The text is the reason.
    Unmappable(&'static str),
}

fn fcpxml_element_kind(name: &str) -> Option<FcpxmlElementKind> {
    use FcpxmlElementKind::*;
    Some(match name {
        "asset-clip" | "video" | "audio" => AssetBacked,
        "clip" | "sync-clip" | "spine" => Container,
        "ref-clip" => CompoundReference,
        "gap" => Gap,
        "title" | "caption" | "generator" => {
            Unmappable("generated in the editor, there is no source media to conform")
        }
        "mc-clip" => Unmappable(
            "multicam angle selection has no flat equivalent, flatten it in the editor and re-export",
        ),
        "audition" => Unmappable("audition alternatives, pick one in the editor and re-export"),
        "transition" => {
            Unmappable("a transition is not a source clip, conform assembles cuts only")
        }
        _ => return None,
    })
}

/// Where a container's own time base sits on the record timeline, and which
/// slice of that time base its parent actually shows.
#[derive(Clone, Copy)]
struct FcpxmlWindow {
    record_of_local_zero: f64,
    start: f64,
    end: f64,
    lane: i32,
}

/// A clip resolved to the record timeline, still in seconds because fcpxml
/// times are rate-independent and only the sequence rate turns them to frames.
struct FcpxmlResolvedClip {
    reel_name: String,
    track_type: &'static str,
    lane: i32,
    record_in_seconds: f64,
    source_in_seconds: f64,
    length_seconds: f64,
}

struct FcpxmlWalk<'a> {
    resources: &'a FcpxmlResources<'a>,
    clips: Vec<FcpxmlResolvedClip>,
    skipped: Vec<String>,
    /// Media ids being expanded right now, so a compound clip that contains
    /// itself is reported instead of recursing until the stack runs out.
    expanding: Vec<String>,
}

impl<'a> FcpxmlWalk<'a> {
    fn skip(&mut self, element: &FcpxmlElement, reason: &str) {
        let note = match element.attribute("name").filter(|n| !n.is_empty()) {
            Some(name) => format!("<{}> \"{name}\": {reason}", element.name),
            None => format!("<{}>: {reason}", element.name),
        };
        tracing::warn!("fcpxml: skipped {note}");
        self.skipped.push(note);
    }

    /// The clip's start inside the media it references. An absent `start` means
    /// the media's own beginning, which for an asset is its `start` and for a
    /// compound clip the inner sequence's `tcStart`.
    fn media_start(&self, element: &FcpxmlElement) -> f64 {
        if let Some(start) = element.seconds("start") {
            return start;
        }
        let Some(reference) = element.attribute("ref") else {
            return 0.0;
        };
        if let Some(asset) = self.resources.asset_by_id.get(reference) {
            return asset.start_seconds;
        }
        self.resources
            .media_by_id
            .get(reference)
            .and_then(|media| media.child("sequence"))
            .and_then(|sequence| sequence.seconds("tcStart"))
            .unwrap_or(0.0)
    }

    /// The part of `element` its parent actually shows, in the parent's time
    /// base. None when the parent trimmed all of it away.
    fn visible_range(&self, element: &FcpxmlElement, window: &FcpxmlWindow) -> Option<(f64, f64)> {
        let offset = element.seconds("offset").unwrap_or(0.0);
        let duration = element.seconds("duration").unwrap_or(f64::INFINITY);
        let start = offset.max(window.start);
        let end = (offset + duration).min(window.end);
        (end > start).then_some((start, end))
    }

    /// The time base of `element`'s own content, given the slice of it visible
    /// in the parent.
    fn inner_window(
        &self,
        element: &FcpxmlElement,
        window: &FcpxmlWindow,
        (visible_start, visible_end): (f64, f64),
        lane: i32,
    ) -> FcpxmlWindow {
        let offset = element.seconds("offset").unwrap_or(0.0);
        let start = self.media_start(element);
        let inner_start = start + (visible_start - offset);
        FcpxmlWindow {
            record_of_local_zero: window.record_of_local_zero + offset - start,
            start: inner_start,
            end: inner_start + (visible_end - visible_start),
            lane,
        }
    }

    fn walk_children(
        &mut self,
        container: &'a FcpxmlElement,
        window: FcpxmlWindow,
        depth: usize,
        connected_only: bool,
    ) {
        for child in &container.children {
            // inside a clip, a child without a lane is one of that clip's own
            // components (its audio role, its video), not a separate clip
            if connected_only && child.lane() == 0 {
                continue;
            }
            self.place(child, window, depth);
        }
    }

    fn place(&mut self, element: &'a FcpxmlElement, window: FcpxmlWindow, depth: usize) {
        let Some(kind) = fcpxml_element_kind(&element.name) else {
            return;
        };
        let Some(visible) = self.visible_range(element, &window) else {
            return;
        };
        if depth >= FCPXML_MAX_NESTING_DEPTH {
            self.skip(
                element,
                &format!("nested more than {FCPXML_MAX_NESTING_DEPTH} levels deep"),
            );
            return;
        }
        let lane = match element.lane() {
            0 => window.lane,
            lane => lane,
        };
        let inner = self.inner_window(element, &window, visible, lane);
        match kind {
            FcpxmlElementKind::AssetBacked => {
                // an asset-clip pointing at a <media> is a compound clip too
                if element
                    .attribute("ref")
                    .is_some_and(|reference| self.resources.media_by_id.contains_key(reference))
                {
                    self.expand_compound(element, inner, depth);
                } else {
                    self.emit(element, &window, visible, lane);
                }
                self.walk_children(element, inner, depth + 1, true);
            }
            FcpxmlElementKind::CompoundReference => {
                self.expand_compound(element, inner, depth);
                self.walk_children(element, inner, depth + 1, true);
            }
            FcpxmlElementKind::Container => self.walk_children(element, inner, depth + 1, false),
            FcpxmlElementKind::Gap => self.walk_children(element, inner, depth + 1, true),
            FcpxmlElementKind::Unmappable(reason) => self.skip(element, reason),
        }
    }

    /// Walk the spine of the `<media>` sequence `element` references, placing
    /// its clips on the record timeline as if they were spelled out here.
    fn expand_compound(&mut self, element: &'a FcpxmlElement, window: FcpxmlWindow, depth: usize) {
        let Some(reference) = element.attribute("ref") else {
            self.skip(element, "no ref attribute, nothing to expand");
            return;
        };
        let Some(media) = self.resources.media_by_id.get(reference).copied() else {
            self.skip(
                element,
                &format!("references \"{reference}\", which no <media> in <resources> declares"),
            );
            return;
        };
        if self.expanding.iter().any(|open| open == reference) {
            self.skip(
                element,
                &format!("compound clip \"{reference}\" contains itself"),
            );
            return;
        }
        let Some(sequence) = media.child("sequence") else {
            let reason = match media.child("multicam") {
                Some(_) => "multicam media, angle selection has no flat equivalent",
                None => "referenced media holds no sequence",
            };
            self.skip(element, reason);
            return;
        };
        let Some(spine) = sequence.child("spine") else {
            self.skip(element, "compound clip sequence has no spine");
            return;
        };
        self.expanding.push(reference.to_string());
        self.walk_children(spine, window, depth + 1, false);
        self.expanding.pop();
    }

    fn emit(
        &mut self,
        element: &FcpxmlElement,
        window: &FcpxmlWindow,
        (visible_start, visible_end): (f64, f64),
        lane: i32,
    ) {
        let reference = element.attribute("ref").unwrap_or_default();
        let asset = self.resources.asset_by_id.get(reference);
        let reel_name = [
            asset.map(|a| a.name.as_str()).unwrap_or_default(),
            element.attribute("name").unwrap_or_default(),
            reference,
        ]
        .into_iter()
        .find(|candidate| !candidate.is_empty())
        .unwrap_or(UNNAMED_REEL_NAME)
        .to_string();
        let track_type = match element.name.as_str() {
            "video" => "V",
            "audio" => "A",
            _ => match (element.attribute("srcEnable").unwrap_or_default(), asset) {
                ("audio", _) => "A",
                ("video", _) => "V",
                (_, Some(asset)) if asset.has_audio && !asset.has_video => "A",
                _ => "V",
            },
        };
        let length = visible_end - visible_start;
        let offset = element.seconds("offset").unwrap_or(0.0);
        self.clips.push(FcpxmlResolvedClip {
            reel_name,
            track_type,
            lane,
            record_in_seconds: window.record_of_local_zero + visible_start,
            source_in_seconds: self.media_start(element) + (visible_start - offset),
            length_seconds: if length.is_finite() { length } else { 0.0 },
        });
    }
}

fn parse_fcpxml(content: &str) -> Result<Timeline, ConformError> {
    let document = read_fcpxml_tree(content)?;
    let mut resources = FcpxmlResources::default();
    collect_fcpxml_resources(&document, &mut resources);

    let title = find_fcpxml_element(&document, "project")
        .and_then(|project| project.attribute("name"))
        .unwrap_or_default()
        .to_string();
    let Some(sequence) = find_fcpxml_element(&document, "sequence") else {
        return Err(ConformError::NoEvents);
    };
    let frame_rate = sequence
        .attribute("format")
        .and_then(|id| resources.frame_rate_by_format_id.get(id))
        .copied()
        .unwrap_or(FCPXML_FALLBACK_FRAME_RATE);

    let mut walk = FcpxmlWalk {
        resources: &resources,
        clips: Vec::new(),
        skipped: Vec::new(),
        expanding: Vec::new(),
    };
    if let Some(spine) = sequence.child("spine") {
        walk.walk_children(
            spine,
            FcpxmlWindow {
                record_of_local_zero: 0.0,
                start: 0.0,
                end: f64::INFINITY,
                lane: 0,
            },
            0,
            false,
        );
    }

    // spine order is not record order once lanes and compound clips are flattened
    walk.clips.sort_by(|a, b| {
        a.record_in_seconds
            .total_cmp(&b.record_in_seconds)
            .then(a.lane.cmp(&b.lane))
    });

    let events: Vec<EditEvent> = walk
        .clips
        .iter()
        .enumerate()
        .map(|(index, clip)| {
            let record_in = seconds_to_frames(clip.record_in_seconds, frame_rate);
            let source_in = seconds_to_frames(clip.source_in_seconds, frame_rate);
            let length = seconds_to_frames(clip.length_seconds, frame_rate);
            EditEvent {
                event_number: index as u32 + 1,
                reel_name: clip.reel_name.clone(),
                track_type: clip.track_type.to_string(),
                source_in,
                source_out: source_in + length,
                record_in,
                record_out: record_in + length,
                transition: "CUT".to_string(),
                comment: String::new(),
                lane: clip.lane,
            }
        })
        .collect();

    if events.is_empty() {
        return Err(ConformError::NoEvents);
    }

    Ok(Timeline {
        title,
        frame_rate,
        format: TimelineFormat::XmlFcpx,
        events,
        skipped: walk.skipped,
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
                lane: 0,
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

    /// Every clip in the lane fixtures below, as (reel, track, lane, record in,
    /// record out, source in, source out).
    fn clip_rows(timeline: &Timeline) -> Vec<(&str, &str, i32, u32, u32, u32, u32)> {
        timeline
            .events
            .iter()
            .map(|e| {
                (
                    e.reel_name.as_str(),
                    e.track_type.as_str(),
                    e.lane,
                    e.record_in,
                    e.record_out,
                    e.source_in,
                    e.source_out,
                )
            })
            .collect()
    }

    #[test]
    fn test_parse_fcpxml_connected_clips_in_lanes() {
        // 24 fps. MAIN runs the first 8s of the spine from 10s into its source,
        // with BROLL connected above it and MUSIC below. CARD is connected to a
        // gap, which FCP gives a start of 3600s, so its offset is 3601s.
        let (_dir, path) = write_timeline(
            "lanes.fcpxml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
  <resources>
    <format id="r1" frameDuration="1/24s" width="1920" height="1080"/>
    <asset id="r2" name="MAIN" start="0s" duration="600/24s" hasVideo="1" hasAudio="1" format="r1"/>
    <asset id="r3" name="BROLL" start="0s" duration="600/24s" hasVideo="1" format="r1"/>
    <asset id="r4" name="MUSIC" start="0s" duration="600/24s" hasAudio="1" audioSources="1"/>
    <asset id="r5" name="CARD" start="0s" duration="600/24s" hasVideo="1" format="r1"/>
  </resources>
  <project name="Lanes">
    <sequence format="r1" duration="288/24s" tcStart="0s" tcFormat="NDF">
      <spine>
        <asset-clip ref="r2" name="Main" offset="0s" start="10s" duration="8s">
          <asset-clip ref="r3" name="Broll" lane="1" offset="12s" duration="2s"/>
          <audio ref="r4" name="Music" lane="-1" offset="10s" duration="8s"/>
        </asset-clip>
        <gap name="Gap" offset="8s" start="3600s" duration="4s">
          <asset-clip ref="r5" name="Card" lane="1" offset="3601s" duration="2s"/>
        </gap>
      </spine>
    </sequence>
  </project>
</fcpxml>"#,
        );

        let tl = parse_timeline(&path).unwrap();
        assert!(tl.skipped.is_empty(), "unexpected skips: {:?}", tl.skipped);
        assert_eq!(
            clip_rows(&tl),
            vec![
                ("MUSIC", "A", -1, 0, 192, 0, 192),
                ("MAIN", "V", 0, 0, 192, 240, 432),
                ("BROLL", "V", 1, 48, 96, 0, 48),
                ("CARD", "V", 1, 216, 264, 0, 48),
            ]
        );
    }

    #[test]
    fn test_parse_fcpxml_compound_clip() {
        // 24 fps. The ref-clip shows the compound from 3s in for 4s, so it cuts
        // the tail off INSERT_A and the head off INSERT_B.
        let (_dir, path) = write_timeline(
            "compound.fcpxml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
  <resources>
    <format id="r1" frameDuration="1/24s" width="1920" height="1080"/>
    <asset id="r2" name="OPENING" start="0s" duration="600/24s" hasVideo="1" format="r1"/>
    <asset id="r3" name="INSERT_A" start="0s" duration="600/24s" hasVideo="1" format="r1"/>
    <asset id="r4" name="INSERT_B" start="0s" duration="600/24s" hasVideo="1" format="r1"/>
    <media id="r10" name="Montage">
      <sequence format="r1" duration="10s" tcStart="0s" tcFormat="NDF">
        <spine>
          <asset-clip ref="r3" name="A" offset="0s" duration="4s"/>
          <asset-clip ref="r4" name="B" offset="4s" start="2s" duration="6s"/>
        </spine>
      </sequence>
    </media>
  </resources>
  <project name="Compound">
    <sequence format="r1" duration="144/24s" tcStart="0s" tcFormat="NDF">
      <spine>
        <asset-clip ref="r2" name="Opening" offset="0s" duration="2s"/>
        <ref-clip ref="r10" name="Montage" offset="2s" start="3s" duration="4s"/>
      </spine>
    </sequence>
  </project>
</fcpxml>"#,
        );

        let tl = parse_timeline(&path).unwrap();
        assert!(tl.skipped.is_empty(), "unexpected skips: {:?}", tl.skipped);
        assert_eq!(tl.title, "Compound");
        assert_eq!(
            clip_rows(&tl),
            vec![
                ("OPENING", "V", 0, 0, 48, 0, 48),
                ("INSERT_A", "V", 0, 48, 72, 72, 96),
                ("INSERT_B", "V", 0, 72, 144, 48, 120),
            ]
        );
    }

    #[test]
    fn test_parse_fcpxml_nested_clips() {
        // A <clip> and a <sync-clip> hold their media in child elements rather
        // than referencing an asset themselves.
        let (_dir, path) = write_timeline(
            "nested.fcpxml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
  <resources>
    <format id="r1" frameDuration="1/24s" width="1920" height="1080"/>
    <asset id="r2" name="PICTURE" start="0s" duration="600/24s" hasVideo="1" format="r1"/>
    <asset id="r3" name="NARRATION" start="0s" duration="600/24s" hasAudio="1" audioSources="1"/>
    <asset id="r4" name="SYNC_PICTURE" start="0s" duration="600/24s" hasVideo="1" format="r1"/>
    <asset id="r5" name="SYNC_SOUND" start="0s" duration="600/24s" hasAudio="1" audioSources="1"/>
  </resources>
  <project name="Nested">
    <sequence format="r1" duration="168/24s" tcStart="0s" tcFormat="NDF">
      <spine>
        <clip name="Nested" offset="0s" start="0s" duration="4s">
          <video ref="r2" offset="0s" start="5s" duration="4s"/>
          <audio ref="r3" lane="-1" offset="0s" start="5s" duration="4s"/>
        </clip>
        <sync-clip name="Sync" offset="4s" start="0s" duration="3s">
          <asset-clip ref="r4" offset="0s" duration="3s"/>
          <audio ref="r5" lane="-1" offset="0s" duration="3s"/>
        </sync-clip>
      </spine>
    </sequence>
  </project>
</fcpxml>"#,
        );

        let tl = parse_timeline(&path).unwrap();
        assert!(tl.skipped.is_empty(), "unexpected skips: {:?}", tl.skipped);
        assert_eq!(
            clip_rows(&tl),
            vec![
                ("NARRATION", "A", -1, 0, 96, 120, 216),
                ("PICTURE", "V", 0, 0, 96, 120, 216),
                ("SYNC_SOUND", "A", -1, 96, 168, 0, 72),
                ("SYNC_PICTURE", "V", 0, 96, 168, 0, 72),
            ]
        );
    }

    #[test]
    fn test_parse_fcpxml_skips_name_the_construct() {
        let (_dir, path) = write_timeline(
            "skips.fcpxml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
  <resources>
    <format id="r1" frameDuration="1/24s" width="1920" height="1080"/>
    <asset id="r2" name="SHOT" start="0s" duration="600/24s" hasVideo="1" format="r1"/>
    <media id="r30" name="Loop">
      <sequence format="r1" duration="1s" tcStart="0s">
        <spine>
          <ref-clip ref="r30" name="Inner Loop" offset="0s" duration="1s"/>
        </spine>
      </sequence>
    </media>
  </resources>
  <project name="Skips">
    <sequence format="r1" duration="216/24s" tcStart="0s" tcFormat="NDF">
      <spine>
        <asset-clip ref="r2" name="Shot" offset="0s" duration="2s"/>
        <title name="Card" offset="2s" duration="1s"/>
        <transition name="Cross Dissolve" offset="2s" duration="1s"/>
        <mc-clip ref="r40" name="Multicam" offset="3s" duration="2s"/>
        <audition offset="5s" duration="2s">
          <asset-clip ref="r2" name="Take 1" offset="5s" duration="2s"/>
        </audition>
        <ref-clip ref="r99" name="Missing" offset="7s" duration="1s"/>
        <ref-clip ref="r30" name="Loop" offset="8s" duration="1s"/>
      </spine>
    </sequence>
  </project>
</fcpxml>"#,
        );

        let tl = parse_timeline(&path).unwrap();
        assert_eq!(clip_rows(&tl), vec![("SHOT", "V", 0, 0, 48, 0, 48)]);
        let skipped = tl.skipped.join("\n");
        for expected in [
            "<title> \"Card\": generated in the editor",
            "<transition> \"Cross Dissolve\": a transition is not a source clip",
            "<mc-clip> \"Multicam\": multicam angle selection",
            "<audition>: audition alternatives",
            "<ref-clip> \"Missing\": references \"r99\", which no <media>",
            "<ref-clip> \"Inner Loop\": compound clip \"r30\" contains itself",
        ] {
            assert!(
                skipped.contains(expected),
                "missing skip {expected:?} in:\n{skipped}"
            );
        }
        assert_eq!(tl.skipped.len(), 6, "{skipped}");
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
