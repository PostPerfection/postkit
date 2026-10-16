use std::collections::HashMap;
use std::path::PathBuf;

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::{FrameArea, HAlign, Rgba, StyledCue, StyledRun, SubtitleError, VAlign};

const MILLISECONDS_PER_SECOND: f64 = 1000.0;
const SECONDS_PER_MINUTE: f64 = 60.0;
const SECONDS_PER_HOUR: f64 = 3600.0;
const DEFAULT_FRAME_RATE: f64 = 30.0;
// px lengths are read against these when tt carries no tts:extent
const FALLBACK_ROOT_WIDTH_PIXELS: f64 = 1920.0;
const FALLBACK_ROOT_HEIGHT_PIXELS: f64 = 1080.0;
const FULL_PERCENT: f32 = 100.0;
const FULL_FRAME: FrameArea = FrameArea {
    left: 0.0,
    top: 0.0,
    width: FULL_PERCENT,
    height: FULL_PERCENT,
};
const TTML_ROOT: &str = "tt";
const PRESERVE: &str = "preserve";
const SEQUENTIAL: &str = "seq";
const RUBY_ANNOTATIONS: [&str; 3] = ["text", "textContainer", "delimiter"];

const NAMED_COLOURS: [(&str, [u8; 4]); 19] = [
    ("transparent", [0, 0, 0, 0]),
    ("black", [0, 0, 0, 255]),
    ("silver", [192, 192, 192, 255]),
    ("gray", [128, 128, 128, 255]),
    ("white", [255, 255, 255, 255]),
    ("maroon", [128, 0, 0, 255]),
    ("red", [255, 0, 0, 255]),
    ("purple", [128, 0, 128, 255]),
    ("fuchsia", [255, 0, 255, 255]),
    ("magenta", [255, 0, 255, 255]),
    ("green", [0, 128, 0, 255]),
    ("lime", [0, 255, 0, 255]),
    ("olive", [128, 128, 0, 255]),
    ("yellow", [255, 255, 0, 255]),
    ("navy", [0, 0, 128, 255]),
    ("blue", [0, 0, 255, 255]),
    ("teal", [0, 128, 128, 255]),
    ("aqua", [0, 255, 255, 255]),
    ("cyan", [0, 255, 255, 255]),
];

// an IMSC 1.x document, its cue times counted from the track file's start
#[derive(Debug, Clone, PartialEq)]
pub struct ImscDocument {
    pub language: Option<String>,
    pub cues: Vec<StyledCue>,
    // the resolver's error for each smpte:backgroundImage it could not find
    pub unresolved_images: Vec<String>,
}

// `image` finds the PNG an smpte:backgroundImage names
pub fn parse_imsc(
    xml: &str,
    image: impl Fn(&str) -> Result<PathBuf, SubtitleError>,
) -> Result<ImscDocument, SubtitleError> {
    let root = read_tree(xml)?;
    if root.name != TTML_ROOT {
        return Err(SubtitleError::Parse(format!(
            "the document root is {}, not a TTML tt",
            root.name
        )));
    }
    let time_base = TimeBase::of(&root)?;
    let head = root.child("head");
    let resolver = Resolver {
        styles: head
            .and_then(|head| head.child("styling"))
            .into_iter()
            .flat_map(|styling| styling.children_named("style"))
            .filter_map(|style| Some((style.attribute("id")?, style)))
            .collect(),
    };
    let root_extent = RootExtent::of(&root);
    let regions: Vec<&Element> = head
        .and_then(|head| head.child("layout"))
        .into_iter()
        .flat_map(|layout| layout.children_named("region"))
        .collect();
    let mut walk = Walk {
        resolver: &resolver,
        time_base,
        region_indices: regions
            .iter()
            .enumerate()
            .filter_map(|(index, region)| Some((region.attribute("id")?, index)))
            .collect(),
        placements: regions
            .iter()
            .map(|region| RegionPlacement::of(region, &resolver, root_extent))
            .collect(),
        paragraphs: Vec::new(),
        image: &image,
        image_cues: Vec::new(),
        unresolved_images: Vec::new(),
    };
    let preserve = root.attribute("space") == Some(PRESERVE);
    if let Some(body) = root.child("body") {
        let document = Interval {
            begin: 0.0,
            end: None,
        };
        if let Some(interval) = time_base.interval(body, document, 0.0)? {
            walk.block(body, interval, Style::default(), None, preserve)?;
        }
    }
    let mut cues = cues(&walk.paragraphs, &walk.placements);
    cues.append(&mut walk.image_cues);
    cues.sort_by_key(|cue| cue.start_ms);
    Ok(ImscDocument {
        language: root
            .attribute("lang")
            .filter(|language| !language.is_empty())
            .map(str::to_string),
        cues,
        unresolved_images: walk.unresolved_images,
    })
}

struct Element {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<Node>,
}

enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn children_named<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter_map(move |node| match node {
            Node::Element(element) if element.name == name => Some(element),
            _ => None,
        })
    }

    fn child(&self, name: &str) -> Option<&Element> {
        self.children_named(name).next()
    }
}

fn xml_error(error: impl std::fmt::Display) -> SubtitleError {
    SubtitleError::Xml(error.to_string())
}

// names lose their prefix and keep their case, a default or prefixed TTML namespace reads the same
fn read_tree(xml: &str) -> Result<Element, SubtitleError> {
    let mut reader = Reader::from_str(xml);
    let mut open: Vec<Element> = Vec::new();
    loop {
        let finished = match reader.read_event().map_err(xml_error)? {
            Event::Start(start) => {
                open.push(element(&start)?);
                None
            }
            Event::Empty(start) => Some(element(&start)?),
            Event::End(_) => open.pop(),
            Event::Text(text) => {
                if let Some(parent) = open.last_mut() {
                    let text = text.unescape().map_err(xml_error)?;
                    parent.children.push(Node::Text(text.into_owned()));
                }
                None
            }
            Event::CData(data) => {
                if let Some(parent) = open.last_mut() {
                    let text = String::from_utf8_lossy(&data.into_inner()).into_owned();
                    parent.children.push(Node::Text(text));
                }
                None
            }
            Event::Eof => {
                return Err(SubtitleError::Parse(
                    "the document ends before its root element closes".into(),
                ));
            }
            _ => None,
        };
        if let Some(finished) = finished {
            match open.last_mut() {
                Some(parent) => parent.children.push(Node::Element(finished)),
                None => return Ok(finished),
            }
        }
    }
}

fn element(start: &BytesStart) -> Result<Element, SubtitleError> {
    let attributes = start
        .attributes()
        .map(|attribute| {
            let attribute = attribute.map_err(xml_error)?;
            let name = String::from_utf8_lossy(attribute.key.local_name().as_ref()).into_owned();
            let value = attribute.unescape_value().map_err(xml_error)?.into_owned();
            Ok((name, value))
        })
        .collect::<Result<_, SubtitleError>>()?;
    Ok(Element {
        name: String::from_utf8_lossy(start.local_name().as_ref()).into_owned(),
        attributes,
        children: Vec::new(),
    })
}

#[derive(Clone, Copy)]
struct TimeBase {
    effective_frame_rate: f64,
    sub_frame_rate: f64,
    tick_rate: f64,
}

impl TimeBase {
    fn of(root: &Element) -> Result<TimeBase, SubtitleError> {
        let number = |name: &str| {
            root.attribute(name)
                .map(|text| {
                    text.trim().parse::<f64>().map_err(|_| {
                        SubtitleError::Parse(format!("ttp:{name} {text} is not a number"))
                    })
                })
                .transpose()
        };
        let frame_rate = number("frameRate")?;
        let multiplier = match root.attribute("frameRateMultiplier") {
            Some(text) => ratio(text).ok_or_else(|| {
                SubtitleError::Parse(format!("ttp:frameRateMultiplier {text} is not two numbers"))
            })?,
            None => 1.0,
        };
        let effective_frame_rate = frame_rate.unwrap_or(DEFAULT_FRAME_RATE) * multiplier;
        let sub_frame_rate = number("subFrameRate")?.unwrap_or(1.0);
        let tick_rate = match (number("tickRate")?, frame_rate) {
            (Some(tick_rate), _) => tick_rate,
            (None, Some(_)) => effective_frame_rate * sub_frame_rate,
            (None, None) => 1.0,
        };
        Ok(TimeBase {
            effective_frame_rate,
            sub_frame_rate,
            tick_rate,
        })
    }

    // begin and end count from `reference`, and the result never outlasts `parent`
    fn interval(
        &self,
        element: &Element,
        parent: Interval,
        reference: f64,
    ) -> Result<Option<Interval>, SubtitleError> {
        let offset = |name: &str| {
            element
                .attribute(name)
                .map(|text| self.seconds(text))
                .transpose()
        };
        let begin = reference + offset("begin")?.unwrap_or(0.0);
        let stated_end = [
            offset("end")?.map(|end| reference + end),
            offset("dur")?.map(|duration| begin + duration),
        ]
        .into_iter()
        .flatten()
        .reduce(f64::min);
        let end = match (stated_end, parent.end) {
            (Some(stated), Some(parent_end)) => Some(stated.min(parent_end)),
            (stated, parent_end) => stated.or(parent_end),
        };
        let begin = begin.max(parent.begin);
        if end.is_some_and(|end| end <= begin) {
            return Ok(None);
        }
        Ok(Some(Interval { begin, end }))
    }

    fn seconds(&self, text: &str) -> Result<f64, SubtitleError> {
        let text = text.trim();
        self.clock_time(text)
            .or_else(|| self.offset_time(text))
            .ok_or_else(|| SubtitleError::Parse(format!("{text} is not a TTML time expression")))
    }

    // HH:MM:SS, HH:MM:SS.fraction or HH:MM:SS:FF(.sub-frames)
    fn clock_time(&self, text: &str) -> Option<f64> {
        let fields: Vec<&str> = text.split(':').collect();
        if !(3..=4).contains(&fields.len()) {
            return None;
        }
        let whole = fields[0].parse::<u64>().ok()? as f64 * SECONDS_PER_HOUR
            + fields[1].parse::<u64>().ok()? as f64 * SECONDS_PER_MINUTE
            + decimal(fields[2])?;
        let Some(frame_field) = fields.get(3) else {
            return Some(whole);
        };
        let (frames, sub_frames) = match frame_field.split_once('.') {
            Some((frames, sub_frames)) => (frames, sub_frames.parse::<u64>().ok()? as f64),
            None => (*frame_field, 0.0),
        };
        let frames = frames.parse::<u64>().ok()? as f64 + sub_frames / self.sub_frame_rate;
        Some(whole + frames / self.effective_frame_rate)
    }

    fn offset_time(&self, text: &str) -> Option<f64> {
        let metric_start = text.find(|character: char| character.is_ascii_alphabetic())?;
        let (count, metric) = text.split_at(metric_start);
        let seconds_per_unit = match metric {
            "h" => SECONDS_PER_HOUR,
            "m" => SECONDS_PER_MINUTE,
            "s" => 1.0,
            "ms" => 1.0 / MILLISECONDS_PER_SECOND,
            "f" => 1.0 / self.effective_frame_rate,
            "t" => 1.0 / self.tick_rate,
            _ => return None,
        };
        Some(decimal(count)? * seconds_per_unit)
    }
}

// digits with an optional fraction, which f64 parsing alone would widen to inf and nan
fn decimal(text: &str) -> Option<f64> {
    let digits = text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.');
    (digits && !text.is_empty()).then(|| text.parse().ok())?
}

fn ratio(text: &str) -> Option<f64> {
    let mut parts = text.split_whitespace();
    let numerator: f64 = parts.next()?.parse().ok()?;
    let denominator: f64 = parts.next()?.parse().ok()?;
    (denominator > 0.0).then_some(numerator / denominator)
}

// seconds into the document, an end of None lasts as long as the document
#[derive(Clone, Copy)]
struct Interval {
    begin: f64,
    end: Option<f64>,
}

// a seq container starts each child where the previous one ended
struct ChildClock {
    sequential: bool,
    reference: Option<f64>,
}

impl ChildClock {
    fn of(parent: &Element, interval: Interval) -> ChildClock {
        ChildClock {
            sequential: parent.attribute("timeContainer") == Some(SEQUENTIAL),
            reference: Some(interval.begin),
        }
    }

    fn next(
        &mut self,
        child: &Element,
        parent: Interval,
        time_base: TimeBase,
    ) -> Result<Option<Interval>, SubtitleError> {
        let Some(reference) = self.reference else {
            return Ok(None);
        };
        let interval = time_base.interval(child, parent, reference)?;
        if self.sequential {
            self.reference = interval.and_then(|interval| interval.end);
        }
        Ok(interval)
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Style<'a> {
    italic: Option<bool>,
    bold: Option<bool>,
    underline: Option<bool>,
    color: Option<Rgba>,
    text_align: Option<HAlign>,
    display_align: Option<VAlign>,
    origin: Option<&'a str>,
    extent: Option<&'a str>,
}

impl<'a> Style<'a> {
    // what `self` leaves unset comes from `outer`
    fn over(self, outer: Style<'a>) -> Style<'a> {
        Style {
            italic: self.italic.or(outer.italic),
            bold: self.bold.or(outer.bold),
            underline: self.underline.or(outer.underline),
            color: self.color.or(outer.color),
            text_align: self.text_align.or(outer.text_align),
            display_align: self.display_align.or(outer.display_align),
            origin: self.origin.or(outer.origin),
            extent: self.extent.or(outer.extent),
        }
    }

    fn inline(element: &'a Element) -> Style<'a> {
        Style {
            italic: element
                .attribute("fontStyle")
                .and_then(|value| match value {
                    "italic" | "oblique" => Some(true),
                    "normal" => Some(false),
                    _ => None,
                }),
            bold: element
                .attribute("fontWeight")
                .and_then(|value| match value {
                    "bold" => Some(true),
                    "normal" => Some(false),
                    _ => None,
                }),
            underline: element.attribute("textDecoration").and_then(|value| {
                value
                    .split_whitespace()
                    .filter_map(|token| match token {
                        "underline" => Some(true),
                        "noUnderline" | "none" => Some(false),
                        _ => None,
                    })
                    .next_back()
            }),
            color: element.attribute("color").and_then(colour),
            text_align: element
                .attribute("textAlign")
                .and_then(|value| match value {
                    "left" | "start" => Some(HAlign::Left),
                    "center" => Some(HAlign::Center),
                    "right" | "end" => Some(HAlign::Right),
                    _ => None,
                }),
            display_align: element
                .attribute("displayAlign")
                .and_then(|value| match value {
                    "before" => Some(VAlign::Top),
                    "center" => Some(VAlign::Middle),
                    "after" => Some(VAlign::Bottom),
                    _ => None,
                }),
            origin: element.attribute("origin"),
            extent: element.attribute("extent"),
        }
    }

    fn run(&self, text: String) -> StyledRun {
        StyledRun {
            text,
            italic: self.italic.unwrap_or(false),
            bold: self.bold.unwrap_or(false),
            underline: self.underline.unwrap_or(false),
            color: self.color,
        }
    }
}

fn colour(text: &str) -> Option<Rgba> {
    let text = text.trim();
    if text.starts_with('#') {
        return Rgba::parse_hex(text).ok();
    }
    let functional = |prefix: &str| {
        let channels: Vec<u8> = text
            .strip_prefix(prefix)?
            .strip_suffix(')')?
            .split(',')
            .map(|channel| channel.trim().parse().ok())
            .collect::<Option<_>>()?;
        Some(channels)
    };
    let channels = match (functional("rgba("), functional("rgb(")) {
        (Some(channels), _) if channels.len() == 4 => channels,
        (_, Some(channels)) if channels.len() == 3 => [channels, vec![u8::MAX]].concat(),
        _ => NAMED_COLOURS
            .iter()
            .find(|(name, _)| *name == text)?
            .1
            .to_vec(),
    };
    Some(Rgba {
        r: channels[0],
        g: channels[1],
        b: channels[2],
        a: channels[3],
    })
}

struct Resolver<'a> {
    styles: HashMap<&'a str, &'a Element>,
}

impl<'a> Resolver<'a> {
    // the referenced styles in order, each overriding the last, then the element's own attributes
    fn specified(&self, element: &'a Element) -> Style<'a> {
        self.specified_through(element, &mut Vec::new())
    }

    fn specified_through(&self, element: &'a Element, expanding: &mut Vec<&'a str>) -> Style<'a> {
        let mut referenced = Style::default();
        for id in element
            .attribute("style")
            .unwrap_or_default()
            .split_whitespace()
        {
            let Some(style) = self.styles.get(id) else {
                continue;
            };
            if expanding.contains(&id) {
                continue;
            }
            expanding.push(id);
            referenced = self.specified_through(style, expanding).over(referenced);
            expanding.pop();
        }
        Style::inline(element).over(referenced)
    }
}

fn pixels(length: &str) -> Option<f64> {
    length.strip_suffix("px")?.parse().ok()
}

#[derive(Clone, Copy)]
struct RootExtent {
    width_pixels: f64,
    height_pixels: f64,
}

impl RootExtent {
    fn of(root: &Element) -> RootExtent {
        let extent: Vec<&str> = root
            .attribute("extent")
            .map(|extent| extent.split_whitespace().collect())
            .unwrap_or_default();
        let length = |index: usize| extent.get(index).and_then(|length| pixels(length));
        RootExtent {
            width_pixels: length(0).unwrap_or(FALLBACK_ROOT_WIDTH_PIXELS),
            height_pixels: length(1).unwrap_or(FALLBACK_ROOT_HEIGHT_PIXELS),
        }
    }
}

fn percent_of_root(length: &str, root_pixels: f64) -> Option<f32> {
    if let Some(percent) = length.strip_suffix('%') {
        return percent.parse().ok();
    }
    Some((pixels(length)? / root_pixels * f64::from(FULL_PERCENT)) as f32)
}

// the first of two lengths, as a percent of the root container's width
fn horizontal_percent(pair: &str, root: RootExtent) -> Option<f32> {
    percent_of_root(pair.split_whitespace().next()?, root.width_pixels)
}

// the second of two lengths, as a percent of the root container's height
fn vertical_percent(pair: &str, root: RootExtent) -> Option<f32> {
    percent_of_root(pair.split_whitespace().nth(1)?, root.height_pixels)
}

struct RegionPlacement<'a> {
    style: Style<'a>,
    valign: VAlign,
    // from the anchored edge, the way the raster reads it
    vposition: Option<f32>,
    // where a background image is drawn
    area: FrameArea,
}

impl<'a> RegionPlacement<'a> {
    fn of(region: &'a Element, resolver: &Resolver<'a>, root: RootExtent) -> RegionPlacement<'a> {
        let style = resolver.specified(region);
        // TTML's initial displayAlign is before
        let valign = style.display_align.unwrap_or(VAlign::Top);
        let top = style
            .origin
            .map_or(Some(0.0), |origin| vertical_percent(origin, root));
        let height = match style.extent {
            Some(extent) => vertical_percent(extent, root),
            None => top.map(|top| FULL_PERCENT - top),
        };
        let vposition = match valign {
            VAlign::Top => top,
            VAlign::Bottom => top
                .zip(height)
                .map(|(top, height)| (FULL_PERCENT - top - height).max(0.0)),
            // the raster centres a middle cue on the frame and reads no region centre
            VAlign::Middle => None,
        };
        let left = style
            .origin
            .and_then(|origin| horizontal_percent(origin, root))
            .unwrap_or(0.0);
        let area_top = top.unwrap_or(0.0);
        let area = FrameArea {
            left,
            top: area_top,
            width: style
                .extent
                .and_then(|extent| horizontal_percent(extent, root))
                .unwrap_or(FULL_PERCENT - left),
            height: height.unwrap_or(FULL_PERCENT - area_top),
        };
        RegionPlacement {
            style,
            valign,
            vposition,
            area,
        }
    }
}

enum Content<'a> {
    Text {
        text: &'a str,
        style: Style<'a>,
        preserve: bool,
    },
    Break,
}

struct Piece<'a> {
    begin: f64,
    end: f64,
    content: Content<'a>,
}

struct Paragraph<'a> {
    region: Option<usize>,
    begin: f64,
    end: f64,
    align: Option<HAlign>,
    pieces: Vec<Piece<'a>>,
}

struct Walk<'a, 'r> {
    resolver: &'r Resolver<'a>,
    time_base: TimeBase,
    region_indices: HashMap<&'a str, usize>,
    placements: Vec<RegionPlacement<'a>>,
    paragraphs: Vec<Paragraph<'a>>,
    image: &'r dyn Fn(&str) -> Result<PathBuf, SubtitleError>,
    image_cues: Vec<StyledCue>,
    unresolved_images: Vec<String>,
}

impl<'a> Walk<'a, '_> {
    fn block(
        &mut self,
        element: &'a Element,
        interval: Interval,
        inherited: Style<'a>,
        region: Option<&'a str>,
        preserve: bool,
    ) -> Result<(), SubtitleError> {
        let style = self.resolver.specified(element).over(inherited);
        let region = element.attribute("region").or(region);
        self.background_image(element, interval, region);
        let preserve = space_preserved(element, preserve);
        let mut clock = ChildClock::of(element, interval);
        for child in element.children.iter().filter_map(|node| match node {
            Node::Element(child) if child.name == "div" || child.name == "p" => Some(child),
            _ => None,
        }) {
            let Some(child_interval) = clock.next(child, interval, self.time_base)? else {
                continue;
            };
            if child.name == "div" {
                self.block(child, child_interval, style, region, preserve)?;
            } else {
                self.paragraph(child, child_interval, style, region, preserve)?;
            }
        }
        Ok(())
    }

    fn paragraph(
        &mut self,
        element: &'a Element,
        interval: Interval,
        inherited: Style<'a>,
        region: Option<&'a str>,
        preserve: bool,
    ) -> Result<(), SubtitleError> {
        let region = element.attribute("region").or(region);
        self.background_image(element, interval, region);
        let Some(end) = interval.end else {
            return Ok(());
        };
        let region_index = self.region_index(region);
        let region_style = region_index
            .map(|index| self.placements[index].style)
            .unwrap_or_default();
        let style = self
            .resolver
            .specified(element)
            .over(inherited)
            .over(region_style);
        let mut pieces = Vec::new();
        self.inline(
            element,
            interval,
            style,
            space_preserved(element, preserve),
            &mut pieces,
        )?;
        self.paragraphs.push(Paragraph {
            region: region_index,
            begin: interval.begin,
            end,
            align: style.text_align,
            pieces,
        });
        Ok(())
    }

    fn inline(
        &self,
        element: &'a Element,
        interval: Interval,
        style: Style<'a>,
        preserve: bool,
        pieces: &mut Vec<Piece<'a>>,
    ) -> Result<(), SubtitleError> {
        let mut clock = ChildClock::of(element, interval);
        // a paragraph's interval always ends, and every child is clipped to it
        let end = interval.end.unwrap_or(interval.begin);
        for node in &element.children {
            let child = match node {
                Node::Text(text) => {
                    pieces.push(Piece {
                        begin: interval.begin,
                        end,
                        content: Content::Text {
                            text,
                            style,
                            preserve,
                        },
                    });
                    continue;
                }
                Node::Element(child) => child,
            };
            let ruby_annotation = child
                .attribute("ruby")
                .is_some_and(|ruby| RUBY_ANNOTATIONS.contains(&ruby));
            if !(child.name == "span" || child.name == "br") || ruby_annotation {
                continue;
            }
            let Some(child_interval) = clock.next(child, interval, self.time_base)? else {
                continue;
            };
            if child.name == "br" {
                pieces.push(Piece {
                    begin: child_interval.begin,
                    end: child_interval.end.unwrap_or(end),
                    content: Content::Break,
                });
                continue;
            }
            self.inline(
                child,
                child_interval,
                self.resolver.specified(child).over(style),
                space_preserved(child, preserve),
                pieces,
            )?;
        }
        Ok(())
    }

    // a paragraph or image naming no region, or one the layout lacks, shows in the first region
    fn region_index(&self, region: Option<&str>) -> Option<usize> {
        region
            .and_then(|id| self.region_indices.get(id).copied())
            .or((!self.placements.is_empty()).then_some(0))
    }

    fn background_image(&mut self, element: &Element, interval: Interval, region: Option<&str>) {
        let (Some(reference), Some(end)) = (element.attribute("backgroundImage"), interval.end)
        else {
            return;
        };
        let (start_ms, end_ms) = (milliseconds(interval.begin), milliseconds(end));
        if end_ms <= start_ms {
            return;
        }
        let path = match (self.image)(reference) {
            Ok(path) => path,
            Err(error) => {
                self.unresolved_images.push(error.to_string());
                return;
            }
        };
        let area = self
            .region_index(region)
            .map_or(FULL_FRAME, |index| self.placements[index].area);
        self.image_cues.push(StyledCue {
            image: Some(path),
            image_area: Some(area),
            ..StyledCue::text(start_ms, end_ms, Vec::new())
        });
    }
}

fn space_preserved(element: &Element, inherited: bool) -> bool {
    element
        .attribute("space")
        .map_or(inherited, |space| space == PRESERVE)
}

// paragraphs active together in one region stack as lines of one cue, cut wherever any piece starts or stops
fn cues(paragraphs: &[Paragraph], placements: &[RegionPlacement]) -> Vec<StyledCue> {
    let mut regions: Vec<Option<usize>> = paragraphs
        .iter()
        .map(|paragraph| paragraph.region)
        .collect();
    regions.sort();
    regions.dedup();
    let mut cues = Vec::new();
    for region in regions {
        let mut in_region: Vec<(usize, &Paragraph)> = paragraphs
            .iter()
            .enumerate()
            .filter(|(_, paragraph)| paragraph.region == region)
            .collect();
        in_region.sort_by(|(_, first), (_, second)| first.begin.total_cmp(&second.begin));
        let placement = region.map(|index| &placements[index]);
        let mut overlapping: Vec<(usize, &Paragraph)> = Vec::new();
        let mut overlap_end = f64::NEG_INFINITY;
        for (index, paragraph) in in_region {
            if paragraph.begin >= overlap_end && !overlapping.is_empty() {
                cues.extend(overlap_cues(std::mem::take(&mut overlapping), placement));
            }
            overlap_end = overlap_end.max(paragraph.end);
            overlapping.push((index, paragraph));
        }
        cues.extend(overlap_cues(overlapping, placement));
    }
    cues.sort_by_key(|cue| cue.start_ms);
    cues
}

// `overlapping` pairs each paragraph with its place in the document
fn overlap_cues(
    mut overlapping: Vec<(usize, &Paragraph)>,
    placement: Option<&RegionPlacement>,
) -> Vec<StyledCue> {
    overlapping.sort_by_key(|(index, _)| *index);
    let paragraphs: Vec<&Paragraph> = overlapping
        .into_iter()
        .map(|(_, paragraph)| paragraph)
        .collect();
    let mut boundaries: Vec<f64> = paragraphs
        .iter()
        .flat_map(|paragraph| {
            [paragraph.begin, paragraph.end].into_iter().chain(
                paragraph
                    .pieces
                    .iter()
                    .flat_map(|piece| [piece.begin, piece.end]),
            )
        })
        .collect();
    boundaries.sort_by(f64::total_cmp);
    boundaries.dedup();
    let mut cues: Vec<StyledCue> = Vec::new();
    for window in boundaries.windows(2) {
        let (from, to) = (window[0], window[1]);
        let active: Vec<&Paragraph> = paragraphs
            .iter()
            .copied()
            .filter(|paragraph| paragraph.begin <= from && paragraph.end >= to)
            .collect();
        let mut runs: Vec<StyledRun> = Vec::new();
        for paragraph in &active {
            let lines = paragraph_runs(paragraph, from, to);
            if lines.is_empty() {
                continue;
            }
            if let Some(last) = runs.last_mut() {
                last.text.push('\n');
            }
            runs.extend(lines);
        }
        let (start_ms, end_ms) = (milliseconds(from), milliseconds(to));
        if runs.is_empty() || end_ms <= start_ms {
            continue;
        }
        let cue = StyledCue {
            start_ms,
            end_ms,
            runs,
            align: active.first().and_then(|paragraph| paragraph.align),
            valign: placement.map(|placement| placement.valign),
            vposition: placement.and_then(|placement| placement.vposition),
            image: None,
            image_area: None,
        };
        match cues.last_mut() {
            Some(previous) if continues(previous, &cue) => previous.end_ms = cue.end_ms,
            _ => cues.push(cue),
        }
    }
    cues
}

// a cut that changed nothing on screen
fn continues(previous: &StyledCue, next: &StyledCue) -> bool {
    previous.end_ms == next.start_ms
        && (
            &previous.runs,
            previous.align,
            previous.valign,
            previous.vposition,
        ) == (&next.runs, next.align, next.valign, next.vposition)
}

fn milliseconds(seconds: f64) -> u64 {
    (seconds * MILLISECONDS_PER_SECOND).round().max(0.0) as u64
}

// a line break ends the last run's text with '\n', as the DCP reader writes lines
fn paragraph_runs(paragraph: &Paragraph, from: f64, to: f64) -> Vec<StyledRun> {
    let mut lines = Lines::default();
    for piece in &paragraph.pieces {
        if piece.begin > from || piece.end < to {
            continue;
        }
        match &piece.content {
            Content::Text {
                text,
                style,
                preserve,
            } => lines.push_text(text, style, *preserve),
            Content::Break => lines.push_break(),
        }
    }
    lines.finish()
}

#[derive(Default)]
struct Lines {
    runs: Vec<StyledRun>,
    inside_line: bool,
    pending_space: bool,
}

impl Lines {
    // xml:space default turns every whitespace run into one space and drops it at line ends
    fn push_text(&mut self, text: &str, style: &Style, preserve: bool) {
        for character in text.chars() {
            if preserve {
                match character {
                    '\n' => self.push_break(),
                    '\r' => {}
                    _ => self.push_character(character, style),
                }
                continue;
            }
            if matches!(character, ' ' | '\t' | '\n' | '\r') {
                self.pending_space = self.inside_line;
                continue;
            }
            self.push_character(character, style);
        }
    }

    fn push_character(&mut self, character: char, style: &Style) {
        if self.pending_space
            && let Some(last) = self.runs.last_mut()
        {
            last.text.push(' ');
        }
        self.pending_space = false;
        self.inside_line = true;
        let run = style.run(String::new());
        match self.runs.last_mut() {
            Some(last) if same_style(last, &run) => last.text.push(character),
            _ => {
                let mut run = run;
                run.text.push(character);
                self.runs.push(run);
            }
        }
    }

    fn push_break(&mut self) {
        self.pending_space = false;
        self.inside_line = false;
        if let Some(last) = self.runs.last_mut() {
            last.text.push('\n');
        }
    }

    fn finish(mut self) -> Vec<StyledRun> {
        if let Some(last) = self.runs.last_mut() {
            let kept = last.text.trim_end_matches('\n').len();
            last.text.truncate(kept);
        }
        self.runs.retain(|run| !run.text.is_empty());
        self.runs
    }
}

fn same_style(first: &StyledRun, second: &StyledRun) -> bool {
    (first.italic, first.bold, first.underline, first.color)
        == (second.italic, second.bold, second.underline, second.color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const WHITE: Rgba = Rgba {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    fn document(parameters: &str, head: &str, body: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <tt xmlns=\"http://www.w3.org/ns/ttml\" xmlns:tts=\"http://www.w3.org/ns/ttml#styling\" \
             xmlns:ttp=\"http://www.w3.org/ns/ttml#parameter\" \
             xmlns:smpte=\"http://www.smpte-ra.org/schemas/2052-1/2010/smpte-tt\" xml:lang=\"en\" {parameters}>\
             <head>{head}</head><body>{body}</body></tt>"
        )
    }

    fn no_images(reference: &str) -> Result<PathBuf, SubtitleError> {
        Err(SubtitleError::MissingImage(PathBuf::from(reference)))
    }

    fn spans(document: &ImscDocument) -> Vec<(u64, u64, String)> {
        document
            .cues
            .iter()
            .map(|cue| (cue.start_ms, cue.end_ms, cue.plain_text()))
            .collect()
    }

    #[test]
    fn frame_clock_tick_and_offset_times_read_at_the_effective_frame_rate() {
        let xml = document(
            "ttp:frameRate=\"24\" ttp:frameRateMultiplier=\"1000 1001\"",
            "",
            "<div>\
             <p begin=\"216f\" end=\"00:00:10.5\">frames</p>\
             <p begin=\"00:00:12:06\" end=\"312t\">clock frames to default ticks</p>\
             <p begin=\"0.01h\" dur=\"1500ms\">hours</p>\
             <p begin=\"1m\" end=\"61.25s\">minutes</p>\
             </div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        // 216 frames at 24000/1001, 12 s plus 6 such frames, and ticks at that same rate
        assert_eq!(
            spans(&parsed),
            vec![
                (9009, 10500, "frames".into()),
                (12250, 13013, "clock frames to default ticks".into()),
                (36000, 37500, "hours".into()),
                (60000, 61250, "minutes".into()),
            ]
        );
        assert_eq!(parsed.language.as_deref(), Some("en"));
    }

    #[test]
    fn frames_default_to_thirty_a_second_and_ticks_to_one_without_a_frame_rate() {
        let xml = document(
            "",
            "",
            "<div><p begin=\"30f\" end=\"5t\">default rates</p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        assert_eq!(spans(&parsed), vec![(1000, 5000, "default rates".into())]);
    }

    #[test]
    fn a_stated_tick_rate_and_sub_frames_are_honoured() {
        let xml = document(
            "ttp:frameRate=\"25\" ttp:subFrameRate=\"2\" ttp:tickRate=\"10000000\"",
            "",
            "<div><p begin=\"00:00:01:05.1\" end=\"25000000t\">ticks</p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        assert_eq!(spans(&parsed), vec![(1220, 2500, "ticks".into())]);
    }

    #[test]
    fn a_child_counts_from_its_parent_begin_and_is_clipped_to_its_parent() {
        let xml = document(
            "",
            "",
            "<div begin=\"10s\"><p begin=\"1s\" end=\"5s\">Hello \
             <span begin=\"2s\" end=\"10s\">world</span></p></div>\
             <div><p begin=\"20s\">no end anywhere</p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        assert_eq!(
            spans(&parsed),
            vec![
                (11000, 13000, "Hello".into()),
                (13000, 15000, "Hello world".into()),
            ]
        );
    }

    #[test]
    fn a_sequential_container_starts_each_child_where_the_last_ended() {
        let xml = document(
            "",
            "",
            "<div timeContainer=\"seq\" begin=\"1s\"><p dur=\"2s\">first</p><p dur=\"3s\">second</p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        assert_eq!(
            spans(&parsed),
            vec![(1000, 3000, "first".into()), (3000, 6000, "second".into())]
        );
    }

    #[test]
    fn referenced_styles_chain_and_inline_attributes_override_them() {
        let xml = document(
            "",
            "<styling>\
             <style xml:id=\"lean\" tts:fontStyle=\"italic\" tts:color=\"yellow\"/>\
             <style xml:id=\"heavy\" style=\"lean\" tts:fontWeight=\"bold\" tts:textDecoration=\"underline\"/>\
             </styling>",
            "<div style=\"heavy\"><p begin=\"0s\" end=\"1s\" tts:color=\"rgb(0,255,0)\">green \
             <span tts:fontStyle=\"normal\" tts:color=\"#ff000080\">upright</span> \
             <span tts:textDecoration=\"noUnderline\" tts:color=\"rgba(1,2,3,4)\">plain</span></p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        let runs = &parsed.cues[0].runs;
        let texts: Vec<&str> = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(texts, ["green ", "upright ", "plain"]);
        let green = &runs[0];
        assert!(green.italic && green.bold && green.underline);
        assert_eq!(
            green.color,
            Some(Rgba {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            })
        );
        assert!(!runs[1].italic && runs[1].bold);
        assert_eq!(
            runs[1].color,
            Some(Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 128
            })
        );
        assert!(!runs[2].underline && runs[2].italic);
        assert_eq!(
            runs[2].color,
            Some(Rgba {
                r: 1,
                g: 2,
                b: 3,
                a: 4
            })
        );
    }

    #[test]
    fn a_region_sets_placement_alignment_and_the_outermost_style() {
        let xml = document(
            "tts:extent=\"1920px 1080px\"",
            "<styling><style xml:id=\"white\" tts:color=\"white\"/></styling>\
             <layout>\
             <region xml:id=\"bottom\" style=\"white\" tts:origin=\"10% 80%\" tts:extent=\"80% 10%\" \
             tts:displayAlign=\"after\" tts:textAlign=\"center\"/>\
             <region xml:id=\"top\" tts:origin=\"0px 108px\" tts:extent=\"1920px 216px\" \
             tts:displayAlign=\"before\" tts:textAlign=\"start\"/>\
             <region xml:id=\"middle\" tts:origin=\"10% 10%\" tts:extent=\"80% 80%\" \
             tts:displayAlign=\"center\" tts:textAlign=\"end\"/>\
             </layout>",
            "<div><p begin=\"0s\" end=\"1s\">first region</p></div>\
             <div region=\"top\"><p begin=\"2s\" end=\"3s\">from the div</p></div>\
             <div><p begin=\"4s\" end=\"5s\" region=\"middle\" tts:textAlign=\"left\">own region</p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        let [bottom, top, middle] = parsed.cues.as_slice() else {
            panic!("three cues: {:?}", parsed.cues);
        };
        assert_eq!(bottom.valign, Some(VAlign::Bottom));
        assert_eq!(bottom.vposition, Some(10.0));
        assert_eq!(bottom.align, Some(HAlign::Center));
        assert_eq!(bottom.runs[0].color, Some(WHITE));
        assert_eq!(top.valign, Some(VAlign::Top));
        assert_eq!(top.vposition, Some(10.0));
        assert_eq!(top.align, Some(HAlign::Left));
        assert_eq!(middle.valign, Some(VAlign::Middle));
        assert_eq!(middle.vposition, None);
        assert_eq!(middle.align, Some(HAlign::Left));
    }

    #[test]
    fn paragraphs_sharing_a_region_and_a_time_stack_as_lines() {
        let xml = document(
            "",
            "<layout><region xml:id=\"r\" tts:displayAlign=\"after\"/></layout>",
            "<div region=\"r\"><p begin=\"0s\" end=\"2s\">upper</p><p begin=\"1s\" end=\"2s\">lower</p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        assert_eq!(
            spans(&parsed),
            vec![
                (0, 1000, "upper".into()),
                (1000, 2000, "upper\nlower".into())
            ]
        );
    }

    #[test]
    fn whitespace_collapses_and_trims_at_line_ends_unless_preserved() {
        let xml = document(
            "",
            "",
            "<div><p begin=\"0s\" end=\"1s\">\n   Line \t  one   <br/>\n  <span>line</span>   two  <br/></p>\
             <p begin=\"2s\" end=\"3s\" xml:space=\"preserve\">  kept   as is</p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        assert_eq!(
            spans(&parsed),
            vec![
                (0, 1000, "Line one\nline two".into()),
                (2000, 3000, "  kept   as is".into()),
            ]
        );
    }

    #[test]
    fn ruby_annotations_and_unknown_styling_are_ignored() {
        let xml = document(
            "",
            "",
            "<div><p begin=\"0s\" end=\"1s\" tts:shear=\"16%\" tts:fontSize=\"80%\">\
             <span tts:ruby=\"container\"><span tts:ruby=\"base\">base</span>\
             <span tts:ruby=\"text\">annotation</span></span></p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        assert_eq!(spans(&parsed), vec![(0, 1000, "base".into())]);
    }

    #[test]
    fn a_background_image_is_a_cue_filling_its_region_against_the_root_extent() {
        let xml = document(
            "ttp:frameRate=\"24\" ttp:frameRateMultiplier=\"1000 1001\" tts:extent=\"3840px 2160px\"",
            "<layout>\
             <region xml:id=\"full\" tts:origin=\"0px 0px\" tts:extent=\"3840px 2160px\"/>\
             <region xml:id=\"cropped\" tts:origin=\"1152px 1728px\" tts:extent=\"1536px 108px\" \
             tts:luminanceGain=\"5.0\"/>\
             </layout>",
            "<div region=\"cropped\" begin=\"216f\" end=\"264f\" smpte:backgroundImage=\"urn:uuid:2\"/>\
             <div region=\"full\" begin=\"144f\" end=\"192f\" smpte:backgroundImage=\"urn:uuid:1\"/>\
             <div begin=\"288f\" end=\"336f\"><p smpte:backgroundImage=\"urn:uuid:3\"/></div>",
        );

        let parsed =
            parse_imsc(&xml, |reference| Ok(Path::new("/images").join(reference))).unwrap();

        let placed: Vec<(u64, u64, Option<&Path>, Option<FrameArea>)> = parsed
            .cues
            .iter()
            .map(|cue| {
                (
                    cue.start_ms,
                    cue.end_ms,
                    cue.image.as_deref(),
                    cue.image_area,
                )
            })
            .collect();
        // frames at 24000/1001, a region without one of its own takes the first
        assert_eq!(
            placed,
            vec![
                (
                    6006,
                    8008,
                    Some(Path::new("/images/urn:uuid:1")),
                    Some(FULL_FRAME)
                ),
                (
                    9009,
                    11011,
                    Some(Path::new("/images/urn:uuid:2")),
                    Some(FrameArea {
                        left: 30.0,
                        top: 80.0,
                        width: 40.0,
                        height: 5.0,
                    })
                ),
                (
                    12012,
                    14014,
                    Some(Path::new("/images/urn:uuid:3")),
                    Some(FULL_FRAME)
                ),
            ]
        );
        assert!(parsed.cues.iter().all(|cue| cue.runs.is_empty()));
        assert!(parsed.unresolved_images.is_empty());
    }

    #[test]
    fn a_region_without_an_extent_reaches_the_root_edge_and_no_layout_fills_the_frame() {
        let with_region = document(
            "tts:extent=\"1000px 500px\"",
            "<layout><region xml:id=\"r\" tts:origin=\"100px 50px\"/></layout>",
            "<div begin=\"0s\" end=\"1s\" smpte:backgroundImage=\"urn:uuid:1\"/>",
        );
        let without_layout = document(
            "",
            "",
            "<div begin=\"0s\" end=\"1s\" smpte:backgroundImage=\"urn:uuid:1\"/>",
        );
        let found = |reference: &str| Ok(PathBuf::from(reference));

        let with_region = parse_imsc(&with_region, found).unwrap();
        let without_layout = parse_imsc(&without_layout, found).unwrap();

        assert_eq!(
            with_region.cues[0].image_area,
            Some(FrameArea {
                left: 10.0,
                top: 10.0,
                width: 90.0,
                height: 90.0,
            })
        );
        assert_eq!(without_layout.cues[0].image_area, Some(FULL_FRAME));
    }

    #[test]
    fn an_unresolved_background_image_gives_no_cue_and_names_the_reference() {
        let xml = document(
            "tts:extent=\"3840px 2160px\"",
            "",
            "<div begin=\"6s\" end=\"8s\" smpte:backgroundImage=\"Event1.png\"/>\
             <div><p begin=\"1s\" end=\"2s\">text still shows</p></div>",
        );

        let parsed = parse_imsc(&xml, no_images).unwrap();

        assert_eq!(
            spans(&parsed),
            vec![(1000, 2000, "text still shows".into())]
        );
        let [unresolved] = parsed.unresolved_images.as_slice() else {
            panic!("one unresolved image: {:?}", parsed.unresolved_images);
        };
        assert!(unresolved.contains("Event1.png"), "{unresolved}");
    }

    #[test]
    fn a_bad_time_expression_is_an_error() {
        let xml = document("", "", "<div><p begin=\"soon\" end=\"1s\">x</p></div>");

        let error = parse_imsc(&xml, no_images).unwrap_err();

        assert!(error.to_string().contains("soon"), "{error}");
    }
}
