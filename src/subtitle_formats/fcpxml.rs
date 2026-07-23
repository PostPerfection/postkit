//! FCPXML caption/title parser (dom#2909).
//!
//! Reads <caption> and <title> elements: rational offset/duration timing
//! ("3600/2500s", "240240/24000s", "0s") and per-run styling from
//! <text-style-def>/<text-style>. Offsets are taken as timeline-relative cue
//! positions; nested clip offset composition is not resolved.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::{Rgba, StyledCue, StyledRun, SubtitleError};

#[derive(Debug, Clone, Default)]
struct RunStyle {
    italic: bool,
    bold: bool,
    underline: bool,
    color: Option<Rgba>,
}

struct CueAcc {
    start_ms: u64,
    dur_ms: u64,
    runs_raw: Vec<(Option<String>, String)>,
    defs: HashMap<String, RunStyle>,
}

/// Parse FCPXML text into styled cues (captions and titles).
pub fn parse_fcpxml(content: &str) -> Result<Vec<StyledCue>, SubtitleError> {
    if !content.contains("<fcpxml") {
        return Err(SubtitleError::Parse("not an fcpxml document".into()));
    }
    let mut reader = Reader::from_str(content);
    let mut cues = Vec::new();

    let mut cur: Option<CueAcc> = None;
    let mut cur_ref: Option<String> = None; // ref id of the open run
    let mut cur_ref_text = String::new();
    let mut cur_def_id: Option<String> = None; // id of the open text-style-def

    loop {
        match reader
            .read_event()
            .map_err(|e| SubtitleError::Xml(e.to_string()))?
        {
            Event::Start(e) | Event::Empty(e) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "caption" | "title" => {
                        let offset = attr(&e, "offset").and_then(|v| parse_rational_time(&v));
                        let dur = attr(&e, "duration").and_then(|v| parse_rational_time(&v));
                        cur = Some(CueAcc {
                            start_ms: offset.unwrap_or(0),
                            dur_ms: dur.unwrap_or(0),
                            runs_raw: Vec::new(),
                            defs: HashMap::new(),
                        });
                    }
                    "text-style-def" => {
                        cur_def_id = attr(&e, "id");
                    }
                    "text-style" => {
                        if let Some(r) = attr(&e, "ref") {
                            // a run reference inside <text>
                            cur_ref = Some(r);
                            cur_ref_text.clear();
                        } else if let (Some(cue), Some(def_id)) = (cur.as_mut(), cur_def_id.as_ref()) {
                            // the style body inside a text-style-def
                            cue.defs.insert(def_id.clone(), parse_run_style(&e));
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(e) => {
                if cur_ref.is_some() {
                    let t = e
                        .unescape()
                        .map_err(|err| SubtitleError::Xml(err.to_string()))?;
                    cur_ref_text.push_str(&t);
                }
            }
            Event::End(e) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "text-style" => {
                        if cur_ref.is_some()
                            && let Some(cue) = cur.as_mut()
                        {
                            cue.runs_raw
                                .push((cur_ref.take(), std::mem::take(&mut cur_ref_text)));
                        }
                    }
                    "text-style-def" => cur_def_id = None,
                    "caption" | "title" => {
                        if let Some(cue) = cur.take() {
                            cues.push(build_cue(cue));
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(cues)
}

fn build_cue(acc: CueAcc) -> StyledCue {
    let mut runs: Vec<StyledRun> = acc
        .runs_raw
        .into_iter()
        .map(|(r, text)| {
            let s = r
                .as_ref()
                .and_then(|id| acc.defs.get(id))
                .cloned()
                .unwrap_or_default();
            StyledRun {
                text,
                italic: s.italic,
                bold: s.bold,
                underline: s.underline,
                color: s.color,
            }
        })
        .collect();
    if runs.is_empty() {
        runs.push(StyledRun::plain(String::new()));
    }
    StyledCue::text(acc.start_ms, acc.start_ms + acc.dur_ms, runs)
}

fn parse_run_style(e: &quick_xml::events::BytesStart) -> RunStyle {
    let flag = |name: &str| attr(e, name).map(|v| v == "1").unwrap_or(false);
    RunStyle {
        italic: flag("italic"),
        bold: flag("bold"),
        underline: flag("underline"),
        color: attr(e, "fontColor").and_then(|v| parse_fcp_color(&v)),
    }
}

/// Parse an FCPXML fontColor "r g b a" (floats 0..1) to Rgba.
fn parse_fcp_color(s: &str) -> Option<Rgba> {
    let vals: Vec<f32> = s.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    if vals.len() < 3 {
        return None;
    }
    let to_u8 = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(Rgba {
        r: to_u8(vals[0]),
        g: to_u8(vals[1]),
        b: to_u8(vals[2]),
        a: vals.get(3).map(|&a| to_u8(a)).unwrap_or(255),
    })
}

/// Parse an FCPXML rational time ("3600/2500s", "240240/24000s", "0s") to ms.
pub fn parse_rational_time(s: &str) -> Option<u64> {
    let s = s.trim().strip_suffix('s')?;
    let secs = if let Some((num, den)) = s.split_once('/') {
        let num: f64 = num.parse().ok()?;
        let den: f64 = den.parse().ok()?;
        if den == 0.0 {
            return None;
        }
        num / den
    } else {
        s.parse::<f64>().ok()?
    };
    Some((secs * 1000.0).round() as u64)
}

fn attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if local_name(a.key.as_ref()) == name {
            Some(String::from_utf8_lossy(&a.value).into_owned())
        } else {
            None
        }
    })
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<fcpxml version="1.10">
 <library><event><project><sequence><spine>
   <caption offset="3600/2500s" duration="2500/2500s" role="captions">
     <text><text-style ref="ts1">Hello world</text-style></text>
     <text-style-def id="ts1"><text-style italic="1" bold="0"/></text-style-def>
   </caption>
   <title offset="10s" duration="5s">
     <text><text-style ref="ts2">A title</text-style></text>
     <text-style-def id="ts2"><text-style fontColor="1 0 0 1"/></text-style-def>
   </title>
 </spine></sequence></project></event></library>
</fcpxml>"#;

    #[test]
    fn rational_time_exact() {
        assert_eq!(parse_rational_time("3600/2500s"), Some(1440));
        assert_eq!(parse_rational_time("0s"), Some(0));
        assert_eq!(parse_rational_time("10s"), Some(10000));
        assert_eq!(parse_rational_time("240240/24000s"), Some(10010));
    }

    #[test]
    fn fcpxml_parses_caption_and_title() {
        let cues = parse_fcpxml(SAMPLE).unwrap();
        assert_eq!(cues.len(), 2);

        let c0 = &cues[0];
        assert_eq!(c0.start_ms, 1440);
        assert_eq!(c0.end_ms, 2440);
        assert_eq!(c0.plain_text(), "Hello world");
        assert!(c0.runs[0].italic);

        let c1 = &cues[1];
        assert_eq!(c1.start_ms, 10000);
        assert_eq!(c1.end_ms, 15000);
        assert_eq!(c1.plain_text(), "A title");
        assert_eq!(c1.runs[0].color, Some(Rgba { r: 255, g: 0, b: 0, a: 255 }));
    }

    #[test]
    fn fcpxml_rejects_non_fcpxml() {
        assert!(matches!(
            parse_fcpxml("<xmeml><foo/></xmeml>"),
            Err(SubtitleError::Parse(_))
        ));
    }
}
