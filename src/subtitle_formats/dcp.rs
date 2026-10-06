use std::path::PathBuf;

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::interop::{attr, local_name, parse_halign, parse_valign};
use super::{HAlign, Rgba, StyledCue, StyledRun, SubtitleError, VAlign};

// Interop times count editable units of 4 ms after the seconds
const INTEROP_EDITABLE_UNITS_PER_SECOND: f64 = 250.0;
const MILLISECONDS_PER_SECOND: f64 = 1000.0;
const SECONDS_PER_MINUTE: f64 = 60.0;
const SECONDS_PER_HOUR: f64 = 3600.0;
const SMPTE_ROOT: &str = "subtitlereel";
const ARGB_HEX_DIGITS: usize = 8;
const YES: &str = "yes";
const BOLD: &str = "bold";

// an Interop DCSubtitle or SMPTE ST 428-7 SubtitleReel, its cue times counted from the track file's start
#[derive(Debug, Clone, PartialEq)]
pub struct DcpSubtitleDocument {
    pub language: Option<String>,
    pub cues: Vec<StyledCue>,
}

#[derive(Clone, Copy, Default)]
struct Style {
    italic: bool,
    bold: bool,
    underline: bool,
    color: Option<Rgba>,
}

struct Line {
    runs: Vec<StyledRun>,
    valign: Option<VAlign>,
    halign: Option<HAlign>,
    vposition: Option<f32>,
}

#[derive(Clone, Copy, PartialEq)]
enum Inside {
    Nothing,
    Text,
    Image,
    Language,
    TimeCodeRate,
    StartTime,
}

// `image` finds the PNG an Image element names: a file beside an Interop XML, or a SMPTE resource's urn:uuid
pub fn parse_dcp_subtitle(
    xml: &str,
    image: impl Fn(&str) -> Result<PathBuf, SubtitleError>,
) -> Result<DcpSubtitleDocument, SubtitleError> {
    let mut reader = Reader::from_str(xml);
    let mut smpte = false;
    let mut language = None;
    let mut time_code_rate = None;
    let mut start_time_ms = 0;
    let mut styles = vec![Style::default()];
    let mut inside = Inside::Nothing;
    let mut text = String::new();
    let mut timing: Option<(String, String)> = None;
    let mut lines: Vec<Line> = Vec::new();
    let mut line: Option<Line> = None;
    let mut image_line: Option<Line> = None;
    let mut cues = Vec::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|error| SubtitleError::Xml(error.to_string()))?;
        match event {
            Event::Start(element) => {
                let name = local_name(element.name().as_ref());
                match name.as_str() {
                    SMPTE_ROOT => smpte = true,
                    "language" => inside = Inside::Language,
                    "timecoderate" => inside = Inside::TimeCodeRate,
                    "starttime" => inside = Inside::StartTime,
                    "font" => {
                        let style =
                            font_style(&element, *styles.last().unwrap_or(&Style::default()));
                        styles.push(style);
                    }
                    "subtitle" => {
                        timing = Some((
                            attr(&element, "timein").unwrap_or_default(),
                            attr(&element, "timeout").unwrap_or_default(),
                        ));
                        lines.clear();
                    }
                    "text" => {
                        inside = Inside::Text;
                        line = Some(placed_line(&element));
                    }
                    "image" => {
                        inside = Inside::Image;
                        image_line = Some(placed_line(&element));
                    }
                    _ => {}
                }
                text.clear();
            }
            Event::Text(content) => {
                let content = content
                    .unescape()
                    .map_err(|error| SubtitleError::Xml(error.to_string()))?;
                if inside == Inside::Text {
                    // indentation between nested elements is not part of the line
                    if content.trim().is_empty() && content.contains('\n') {
                        continue;
                    }
                    if let Some(line) = line.as_mut() {
                        let style = *styles.last().unwrap_or(&Style::default());
                        line.runs.push(StyledRun {
                            text: content.into_owned(),
                            italic: style.italic,
                            bold: style.bold,
                            underline: style.underline,
                            color: style.color,
                        });
                    }
                } else {
                    text.push_str(&content);
                }
            }
            Event::End(element) => {
                let name = local_name(element.name().as_ref());
                match name.as_str() {
                    "language" => language = Some(text.trim().to_string()),
                    "timecoderate" => time_code_rate = text.trim().parse::<f64>().ok(),
                    "starttime" => {
                        start_time_ms = time_ms(text.trim(), smpte, time_code_rate).unwrap_or(0)
                    }
                    "font" => {
                        styles.pop();
                    }
                    "text" => {
                        if let Some(line) = line.take() {
                            lines.push(line);
                        }
                    }
                    "image" => {
                        if let (Some(placed), Some((time_in, time_out))) =
                            (image_line.take(), timing.as_ref())
                        {
                            let (start_ms, end_ms) =
                                cue_span(time_in, time_out, smpte, time_code_rate, start_time_ms)?;
                            cues.push(StyledCue {
                                start_ms,
                                end_ms,
                                runs: Vec::new(),
                                align: placed.halign,
                                valign: placed.valign,
                                vposition: placed.vposition,
                                image: Some(image(text.trim())?),
                            });
                        }
                    }
                    "subtitle" => {
                        if let Some((time_in, time_out)) = timing.take()
                            && !lines.is_empty()
                        {
                            let (start_ms, end_ms) = cue_span(
                                &time_in,
                                &time_out,
                                smpte,
                                time_code_rate,
                                start_time_ms,
                            )?;
                            cues.push(text_cue(start_ms, end_ms, std::mem::take(&mut lines)));
                        }
                    }
                    _ => {}
                }
                inside = match name.as_str() {
                    "text" | "image" | "language" | "timecoderate" | "starttime" => Inside::Nothing,
                    _ if inside == Inside::Text => Inside::Text,
                    _ => inside,
                };
                if inside != Inside::Text {
                    text.clear();
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(DcpSubtitleDocument { language, cues })
}

fn font_style(element: &BytesStart, parent: Style) -> Style {
    let flag =
        |name: &str, on: &str| attr(element, name).map(|value| value.eq_ignore_ascii_case(on));
    Style {
        italic: flag("italic", YES).unwrap_or(parent.italic),
        bold: flag("weight", BOLD).unwrap_or(parent.bold),
        underline: flag("underlined", YES).unwrap_or(parent.underline),
        color: attr(element, "color")
            .and_then(|value| argb(&value))
            .or(parent.color),
    }
}

// a DCP writes colour as AARRGGBB
fn argb(text: &str) -> Option<Rgba> {
    if text.len() != ARGB_HEX_DIGITS || !text.is_ascii() {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&text[at..at + 2], 16).ok();
    Some(Rgba {
        a: channel(0)?,
        r: channel(2)?,
        g: channel(4)?,
        b: channel(6)?,
    })
}

fn placed_line(element: &BytesStart) -> Line {
    Line {
        runs: Vec::new(),
        valign: attr(element, "valign").and_then(|value| parse_valign(&value)),
        halign: attr(element, "halign").and_then(|value| parse_halign(&value)),
        vposition: attr(element, "vposition").and_then(|value| value.parse().ok()),
    }
}

// each Text element is one line, placed by its own vposition from the anchored edge
fn text_cue(start_ms: u64, end_ms: u64, mut lines: Vec<Line>) -> StyledCue {
    let valign = lines.first().and_then(|line| line.valign);
    let from_top = valign == Some(VAlign::Top);
    lines.sort_by(|first, second| {
        let order = first
            .vposition
            .unwrap_or_default()
            .total_cmp(&second.vposition.unwrap_or_default());
        if from_top { order } else { order.reverse() }
    });
    let vposition = lines
        .iter()
        .filter_map(|line| line.vposition)
        .min_by(f32::total_cmp);
    let align = lines.first().and_then(|line| line.halign);
    let line_count = lines.len();
    let mut runs = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        runs.extend(line.runs);
        if index + 1 < line_count
            && let Some(last) = runs.last_mut()
        {
            last.text.push('\n');
        }
    }
    StyledCue {
        start_ms,
        end_ms,
        runs,
        align,
        valign,
        vposition,
        image: None,
    }
}

fn cue_span(
    time_in: &str,
    time_out: &str,
    smpte: bool,
    time_code_rate: Option<f64>,
    start_time_ms: u64,
) -> Result<(u64, u64), SubtitleError> {
    let read = |text: &str| {
        time_ms(text, smpte, time_code_rate)
            .ok_or_else(|| SubtitleError::Parse(format!("{text} is not a subtitle time")))
    };
    Ok((
        read(time_in)?.saturating_sub(start_time_ms),
        read(time_out)?.saturating_sub(start_time_ms),
    ))
}

pub(super) fn interop_time_ms(text: &str) -> Option<u64> {
    time_ms(text, false, None)
}

// HH:MM:SS:FF at the TimeCodeRate for SMPTE, HH:MM:SS:TTT in 4 ms units for Interop, or HH:MM:SS.sss
fn time_ms(text: &str, smpte: bool, time_code_rate: Option<f64>) -> Option<u64> {
    let fields: Vec<&str> = text.split(':').collect();
    let whole = |index: usize| fields.get(index)?.parse::<f64>().ok();
    let seconds = match fields.len() {
        4 => {
            let units_per_second = if smpte {
                time_code_rate?
            } else {
                INTEROP_EDITABLE_UNITS_PER_SECOND
            };
            whole(0)? * SECONDS_PER_HOUR
                + whole(1)? * SECONDS_PER_MINUTE
                + whole(2)?
                + whole(3)? / units_per_second
        }
        3 => whole(0)? * SECONDS_PER_HOUR + whole(1)? * SECONDS_PER_MINUTE + whole(2)?,
        _ => return None,
    };
    Some((seconds * MILLISECONDS_PER_SECOND).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SMPTE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SubtitleReel xmlns="http://www.smpte-ra.org/schemas/428-7/2010/DCST">
  <Language>fr</Language>
  <EditRate>24 1</EditRate>
  <TimeCodeRate>24</TimeCodeRate>
  <StartTime>00:00:00:00</StartTime>
  <SubtitleList>
    <Font ID="theFont" Size="42" Color="FFFFFFFF">
      <Subtitle SpotNumber="1" TimeIn="00:00:01:12" TimeOut="00:00:03:00">
        <Text Valign="bottom" Vposition="16" Halign="center">Upper &amp; line</Text>
        <Text Valign="bottom" Vposition="8" Halign="center">Lower <Font Italic="yes" Color="FFFF0000">red</Font> end</Text>
      </Subtitle>
      <Subtitle SpotNumber="2" TimeIn="00:00:04:00" TimeOut="00:00:05:00">
        <Image Valign="top" Vposition="10" Halign="center">urn:uuid:aa000000-0000-4000-8000-000000000001</Image>
      </Subtitle>
    </Font>
  </SubtitleList>
</SubtitleReel>"#;

    const INTEROP: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DCSubtitle Version="1.0">
  <Language>English</Language>
  <Font Italic="no">
    <Subtitle SpotNumber="1" TimeIn="00:00:02:125" TimeOut="00:00:04:000">
      <Text VAlign="bottom" VPosition="10" HAlign="center">Interop line</Text>
    </Subtitle>
  </Font>
</DCSubtitle>"#;

    fn no_images(name: &str) -> Result<PathBuf, SubtitleError> {
        Err(SubtitleError::MissingImage(PathBuf::from(name)))
    }

    #[test]
    fn a_smpte_reel_reads_lines_in_screen_order_with_their_styles() {
        let document =
            parse_dcp_subtitle(SMPTE, |name| Ok(Path::new("/images").join(name))).unwrap();

        assert_eq!(document.language.as_deref(), Some("fr"));
        let [text, image] = document.cues.as_slice() else {
            panic!("two cues");
        };
        assert_eq!((text.start_ms, text.end_ms), (1500, 3000));
        assert_eq!(text.plain_text(), "Upper & line\nLower red end");
        assert_eq!(text.valign, Some(VAlign::Bottom));
        assert_eq!(text.vposition, Some(8.0));
        let red = text.runs.iter().find(|run| run.text == "red").unwrap();
        assert!(red.italic);
        assert_eq!(
            red.color,
            Some(Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            })
        );
        assert!(!text.runs[0].italic);
        assert_eq!((image.start_ms, image.end_ms), (4000, 5000));
        assert_eq!(
            image.image.as_deref(),
            Some(Path::new(
                "/images/urn:uuid:aa000000-0000-4000-8000-000000000001"
            ))
        );
        assert_eq!(image.valign, Some(VAlign::Top));
    }

    #[test]
    fn interop_times_count_four_millisecond_units() {
        let document = parse_dcp_subtitle(INTEROP, no_images).unwrap();

        let [cue] = document.cues.as_slice() else {
            panic!("one cue");
        };
        assert_eq!((cue.start_ms, cue.end_ms), (2500, 4000));
        assert_eq!(cue.plain_text(), "Interop line");
    }

    #[test]
    fn a_smpte_start_time_is_taken_off_every_cue() {
        let offset = SMPTE.replace(
            "<StartTime>00:00:00:00</StartTime>",
            "<StartTime>00:00:01:00</StartTime>",
        );

        let document = parse_dcp_subtitle(&offset, |name| Ok(PathBuf::from(name))).unwrap();

        assert_eq!(document.cues[0].start_ms, 500);
    }
}
