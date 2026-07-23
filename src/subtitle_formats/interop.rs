//! Interop DCSubtitle XML with PNG bitmap subs (dom#1376).
//!
//! Parses a DCSubtitle document whose <Subtitle> spots carry <Image> elements
//! naming PNG files. Returns cues with image paths resolved relative to the XML,
//! timing, and position. Each referenced PNG is validated to exist and start
//! with the PNG magic. This is input parsing for packaging, not rendering.

use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::{HAlign, StyledCue, SubtitleError, VAlign};
use crate::timecode::timecode_to_seconds;

const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Parse an Interop DCSubtitle XML file, resolving and validating PNG images.
///
/// `fps` interprets the last timecode field (frames) into seconds, matching the
/// rest of postkit's interop handling.
pub fn parse_interop_png(xml_path: &Path, fps: f64) -> Result<Vec<StyledCue>, SubtitleError> {
    let content = std::fs::read_to_string(xml_path)?;
    let base = xml_path.parent().unwrap_or_else(|| Path::new("."));
    let mut reader = Reader::from_str(&content);
    let mut cues = Vec::new();

    let mut cur_time: Option<(u64, u64)> = None;
    let mut img: Option<ImgAttrs> = None;
    let mut img_text = String::new();

    loop {
        match reader
            .read_event()
            .map_err(|e| SubtitleError::Xml(e.to_string()))?
        {
            Event::Start(e) | Event::Empty(e) => match local_name(e.name().as_ref()).as_str() {
                "subtitle" => {
                    let tin = attr(&e, "timein").map(|v| tc_ms(&v, fps));
                    let tout = attr(&e, "timeout").map(|v| tc_ms(&v, fps));
                    cur_time = Some((tin.unwrap_or(0), tout.unwrap_or(0)));
                }
                "image" => {
                    img = Some(ImgAttrs {
                        valign: attr(&e, "valign").and_then(|v| parse_valign(&v)),
                        halign: attr(&e, "halign").and_then(|v| parse_halign(&v)),
                        vposition: attr(&e, "vposition").and_then(|v| v.parse::<f32>().ok()),
                    });
                    img_text.clear();
                }
                _ => {}
            },
            Event::Text(e) => {
                if img.is_some() {
                    let t = e
                        .unescape()
                        .map_err(|err| SubtitleError::Xml(err.to_string()))?;
                    img_text.push_str(t.trim());
                }
            }
            Event::End(e) => {
                if local_name(e.name().as_ref()) == "image"
                    && let Some(attrs) = img.take()
                {
                    let (start, end) = cur_time.unwrap_or((0, 0));
                    let path = resolve_png(base, &img_text)?;
                    cues.push(StyledCue {
                        start_ms: start,
                        end_ms: end,
                        runs: Vec::new(),
                        align: attrs.halign,
                        valign: attrs.valign,
                        vposition: attrs.vposition,
                        image: Some(path),
                    });
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(cues)
}

struct ImgAttrs {
    valign: Option<VAlign>,
    halign: Option<HAlign>,
    vposition: Option<f32>,
}

fn resolve_png(base: &Path, name: &str) -> Result<PathBuf, SubtitleError> {
    let path = base.join(name);
    let mut f =
        std::fs::File::open(&path).map_err(|_| SubtitleError::MissingImage(path.clone()))?;
    use std::io::Read;
    let mut magic = [0u8; 8];
    if f.read_exact(&mut magic).is_err() || magic != PNG_MAGIC {
        return Err(SubtitleError::BadPng(path));
    }
    Ok(path)
}

fn tc_ms(tc: &str, fps: f64) -> u64 {
    (timecode_to_seconds(tc, fps) * 1000.0).round() as u64
}

fn parse_valign(s: &str) -> Option<VAlign> {
    match s.to_lowercase().as_str() {
        "top" => Some(VAlign::Top),
        "center" | "centre" => Some(VAlign::Middle),
        "bottom" => Some(VAlign::Bottom),
        _ => None,
    }
}

fn parse_halign(s: &str) -> Option<HAlign> {
    match s.to_lowercase().as_str() {
        "left" => Some(HAlign::Left),
        "center" | "centre" => Some(HAlign::Center),
        "right" => Some(HAlign::Right),
        _ => None,
    }
}

fn attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if local_name(a.key.as_ref()).to_lowercase() == name {
            Some(String::from_utf8_lossy(&a.value).into_owned())
        } else {
            None
        }
    })
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    let local = match s.rsplit_once(':') {
        Some((_, local)) => local,
        None => &s,
    };
    local.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_png(dir: &Path, name: &str) {
        let mut data = PNG_MAGIC.to_vec();
        data.extend_from_slice(&[0, 0, 0, 13]); // filler
        fs::write(dir.join(name), data).unwrap();
    }

    const XML: &str = r#"<?xml version="1.0"?>
<DCSubtitle Version="1.0">
  <Subtitle SpotNumber="1" TimeIn="00:00:01:00" TimeOut="00:00:05:12">
    <Image VAlign="bottom" HAlign="center" VPosition="8">sub0001.png</Image>
  </Subtitle>
</DCSubtitle>"#;

    #[test]
    fn interop_resolves_and_validates_png() {
        let dir = TempDir::new().unwrap();
        write_png(dir.path(), "sub0001.png");
        let xml = dir.path().join("subs.xml");
        fs::write(&xml, XML).unwrap();

        let cues = parse_interop_png(&xml, 24.0).unwrap();
        assert_eq!(cues.len(), 1);
        let c = &cues[0];
        assert_eq!(c.start_ms, 1000);
        assert_eq!(c.end_ms, 5500);
        assert_eq!(c.vposition, Some(8.0));
        assert_eq!(c.valign, Some(VAlign::Bottom));
        assert_eq!(c.align, Some(HAlign::Center));
        assert_eq!(c.image.as_ref().unwrap(), &dir.path().join("sub0001.png"));
    }

    #[test]
    fn interop_fails_on_missing_png() {
        let dir = TempDir::new().unwrap();
        let xml = dir.path().join("subs.xml");
        fs::write(&xml, XML).unwrap();
        assert!(matches!(
            parse_interop_png(&xml, 24.0),
            Err(SubtitleError::MissingImage(_))
        ));
    }

    #[test]
    fn interop_fails_on_non_png() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("sub0001.png"), b"not a png at all").unwrap();
        let xml = dir.path().join("subs.xml");
        fs::write(&xml, XML).unwrap();
        assert!(matches!(
            parse_interop_png(&xml, 24.0),
            Err(SubtitleError::BadPng(_))
        ));
    }
}
