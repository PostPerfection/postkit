//! ASS/SSA subtitle parser (dom#1462).
//!
//! Reads [V4+ Styles] and [Events], maps per-style italic/bold/underline/
//! alignment and inline override tags \i \b \u \an. Override tags we do not
//! model are collected as warnings, never dropped silently and never fatal.

use super::{HAlign, Rgba, StyledCue, StyledRun, SubtitleError, VAlign};

/// A parsed ASS/SSA file: cues plus warnings for unsupported override tags.
#[derive(Debug, Clone)]
pub struct AssParsed {
    pub cues: Vec<StyledCue>,
    /// distinct unsupported override tags encountered (e.g. "\pos", "\c").
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Style {
    italic: bool,
    bold: bool,
    underline: bool,
    color: Option<Rgba>,
    align: Option<HAlign>,
    valign: Option<VAlign>,
}

/// Parse ASS/SSA text into styled cues.
pub fn parse_ass(content: &str) -> Result<AssParsed, SubtitleError> {
    let mut section = String::new();
    let mut style_format: Vec<String> = Vec::new();
    let mut event_format: Vec<String> = Vec::new();
    let mut styles: std::collections::HashMap<String, Style> = std::collections::HashMap::new();
    let mut cues = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_lowercase();
        let value = value.trim();

        match section.as_str() {
            "v4+ styles" | "v4 styles" | "v4+styles" => match key.as_str() {
                "format" => style_format = split_fields(value),
                "style" => {
                    if let Some((name, style)) = parse_style(&style_format, value) {
                        styles.insert(name, style);
                    }
                }
                _ => {}
            },
            "events" => match key.as_str() {
                "format" => event_format = split_fields(value),
                "dialogue" => {
                    if let Some(cue) = parse_dialogue(&event_format, value, &styles, &mut warnings)
                    {
                        cues.push(cue);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    if cues.is_empty() && styles.is_empty() {
        return Err(SubtitleError::Parse(
            "no [Events] or [V4+ Styles] found; not an ASS/SSA file".into(),
        ));
    }

    Ok(AssParsed { cues, warnings })
}

fn split_fields(value: &str) -> Vec<String> {
    value.split(',').map(|s| s.trim().to_lowercase()).collect()
}

fn field_index(format: &[String], name: &str) -> Option<usize> {
    format.iter().position(|f| f == name)
}

fn parse_style(format: &[String], value: &str) -> Option<(String, Style)> {
    let fields: Vec<&str> = value.split(',').map(str::trim).collect();
    let get = |name: &str| -> Option<&str> {
        field_index(format, name).and_then(|i| fields.get(i).copied())
    };
    let name = get("name")?.to_string();
    let ass_bool = |s: Option<&str>| {
        s.map(|v| v.trim_start_matches('-') != "0" && !v.is_empty() && v != "0")
            .unwrap_or(false)
    };
    let mut style = Style {
        italic: ass_bool(get("italic")),
        bold: ass_bool(get("bold")),
        underline: ass_bool(get("underline")),
        color: get("primarycolour").and_then(parse_ass_color),
        ..Default::default()
    };
    if let Some(a) = get("alignment").and_then(|v| v.parse::<u8>().ok()) {
        let (h, v) = alignment_an(a);
        style.align = h;
        style.valign = v;
    }
    Some((name, style))
}

/// Parse an ASS colour literal &HAABBGGRR (alpha 00 = opaque).
fn parse_ass_color(s: &str) -> Option<Rgba> {
    let hex = s
        .trim()
        .trim_start_matches('&')
        .trim_start_matches(['h', 'H'])
        .trim_end_matches('&');
    let v = u32::from_str_radix(hex, 16).ok()?;
    let (a, b, g, r) = ((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8);
    Some(Rgba {
        r,
        g,
        b,
        a: 255u8.wrapping_sub(a),
    })
}

/// Map an \an numpad alignment (1-9) to horizontal + vertical anchors.
fn alignment_an(a: u8) -> (Option<HAlign>, Option<VAlign>) {
    if !(1..=9).contains(&a) {
        return (None, None);
    }
    let h = match a % 3 {
        1 => HAlign::Left,
        0 => HAlign::Right,
        _ => HAlign::Center,
    };
    let v = match (a - 1) / 3 {
        0 => VAlign::Bottom,
        1 => VAlign::Middle,
        _ => VAlign::Top,
    };
    (Some(h), Some(v))
}

fn parse_dialogue(
    format: &[String],
    value: &str,
    styles: &std::collections::HashMap<String, Style>,
    warnings: &mut Vec<String>,
) -> Option<StyledCue> {
    let start_i = field_index(format, "start")?;
    let end_i = field_index(format, "end")?;
    let text_i = field_index(format, "text")?;
    let style_i = field_index(format, "style");

    // text is the last field and may contain commas, so split with a cap
    let n = format.len();
    let parts: Vec<&str> = value.splitn(n, ',').collect();
    if parts.len() < n {
        return None;
    }
    let start_ms = parse_ass_time(parts[start_i].trim())?;
    let end_ms = parse_ass_time(parts[end_i].trim())?;
    let base = style_i
        .and_then(|i| parts.get(i))
        .and_then(|name| styles.get(name.trim()))
        .cloned()
        .unwrap_or_default();

    let (runs, align, valign) = parse_text(parts[text_i], &base, warnings);
    Some(StyledCue {
        start_ms,
        end_ms,
        runs,
        align: align.or(base.align),
        valign: valign.or(base.valign),
        vposition: None,
        image: None,
    })
}

/// Parse ASS time "H:MM:SS.cc" to milliseconds.
fn parse_ass_time(t: &str) -> Option<u64> {
    let mut it = t.split(':');
    let h: u64 = it.next()?.trim().parse().ok()?;
    let m: u64 = it.next()?.trim().parse().ok()?;
    let sec = it.next()?.trim();
    if it.next().is_some() {
        return None;
    }
    let (s, cs) = sec.split_once('.').unwrap_or((sec, "0"));
    let s: u64 = s.parse().ok()?;
    // centiseconds, pad/truncate to hundredths
    let cs: u64 = format!("{cs:0<2}")[..2].parse().ok()?;
    Some(((h * 3600 + m * 60 + s) * 1000) + cs * 10)
}

/// Split dialogue text into styled runs, applying inline overrides.
fn parse_text(
    text: &str,
    base: &Style,
    warnings: &mut Vec<String>,
) -> (Vec<StyledRun>, Option<HAlign>, Option<VAlign>) {
    let mut italic = base.italic;
    let mut bold = base.bold;
    let mut underline = base.underline;
    let color = base.color;
    let mut align = None;
    let mut valign = None;

    let mut runs: Vec<StyledRun> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    let flush =
        |cur: &mut String, runs: &mut Vec<StyledRun>, italic: bool, bold: bool, underline: bool| {
            if !cur.is_empty() {
                runs.push(StyledRun {
                    text: std::mem::take(cur),
                    italic,
                    bold,
                    underline,
                    color,
                });
            }
        };

    while i < chars.len() {
        match chars[i] {
            '{' => {
                // override block; find closing brace
                let close = chars[i..].iter().position(|&c| c == '}').map(|p| i + p);
                let Some(close) = close else {
                    // no closing brace: treat rest as literal
                    cur.extend(&chars[i..]);
                    break;
                };
                // flush text collected so far under the style in effect before this block
                flush(&mut cur, &mut runs, italic, bold, underline);
                let block: String = chars[i + 1..close].iter().collect();
                apply_overrides(
                    &block,
                    &mut italic,
                    &mut bold,
                    &mut underline,
                    &mut align,
                    &mut valign,
                    warnings,
                );
                i = close + 1;
            }
            '\\' if i + 1 < chars.len() && matches!(chars[i + 1], 'N' | 'n') => {
                cur.push('\n');
                i += 2;
            }
            '\\' if i + 1 < chars.len() && chars[i + 1] == 'h' => {
                cur.push(' ');
                i += 2;
            }
            c => {
                cur.push(c);
                i += 1;
            }
        }
    }
    flush(&mut cur, &mut runs, italic, bold, underline);
    if runs.is_empty() {
        runs.push(StyledRun {
            text: String::new(),
            italic: base.italic,
            bold: base.bold,
            underline: base.underline,
            color,
        });
    }
    (runs, align, valign)
}

/// Apply the tags inside one override block, mutating the running style.
fn apply_overrides(
    block: &str,
    italic: &mut bool,
    bold: &mut bool,
    underline: &mut bool,
    align: &mut Option<HAlign>,
    valign: &mut Option<VAlign>,
    warnings: &mut Vec<String>,
) {
    // tags are backslash-delimited; iterate each \tag token
    for tok in block.split('\\').skip(1) {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        if let Some(rest) = tok.strip_prefix("an")
            && let Ok(a) = rest.parse::<u8>()
        {
            let (h, v) = alignment_an(a);
            *align = h;
            *valign = v;
            continue;
        }
        if let Some(rest) = tok.strip_prefix('i')
            && (rest == "0" || rest == "1")
        {
            *italic = rest == "1";
            continue;
        }
        if let Some(rest) = tok.strip_prefix('b') {
            // \b1/\b0 toggle bold; \b<weight> also sets bold (>0)
            if let Ok(w) = rest.parse::<u32>() {
                *bold = w > 0;
                continue;
            }
        }
        if let Some(rest) = tok.strip_prefix('u')
            && (rest == "0" || rest == "1")
        {
            *underline = rest == "1";
            continue;
        }
        // unsupported: record the tag name (letters up to first digit/paren)
        let name: String = tok
            .chars()
            .take_while(|c| c.is_alphabetic() || *c == '&' || *c == '*')
            .collect();
        let warn = format!("\\{}", if name.is_empty() { tok } else { &name });
        if !warnings.contains(&warn) {
            warnings.push(warn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Script Info]
Title: test

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, Bold, Italic, Underline, Alignment
Style: Default,Arial,40,&H00FF0000,0,-1,0,2
Style: Top,Arial,40,&H00FFFFFF,0,0,0,8

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,plain {\\b1}bold{\\b0} again
Dialogue: 0,0:00:04.00,0:00:05.00,Top,,0,0,0,,{\\an9}{\\pos(10,10)}corner\\Nsecond
";

    #[test]
    fn ass_parses_styles_and_inline_overrides() {
        let parsed = parse_ass(SAMPLE).unwrap();
        assert_eq!(parsed.cues.len(), 2);

        let c0 = &parsed.cues[0];
        assert_eq!(c0.start_ms, 1000);
        assert_eq!(c0.end_ms, 3500);
        // base style Default is italic (-1); the middle run also carries bold
        assert!(c0.runs.iter().all(|r| r.italic));
        assert!(c0.runs.iter().any(|r| r.bold && r.text == "bold"));
        assert_eq!(c0.plain_text(), "plain bold again");

        let c1 = &parsed.cues[1];
        assert_eq!(c1.align, Some(HAlign::Right));
        assert_eq!(c1.valign, Some(VAlign::Top));
        assert_eq!(c1.plain_text(), "corner\nsecond");
    }

    #[test]
    fn ass_collects_unsupported_tags_as_warnings() {
        let parsed = parse_ass(SAMPLE).unwrap();
        assert!(parsed.warnings.contains(&"\\pos".to_string()));
    }

    #[test]
    fn ass_rejects_non_ass_input() {
        let err = parse_ass("just some random text\nno sections here");
        assert!(matches!(err, Err(SubtitleError::Parse(_))));
    }

    #[test]
    fn ass_time_parses_centiseconds() {
        assert_eq!(parse_ass_time("0:00:01.00"), Some(1000));
        assert_eq!(parse_ass_time("1:02:03.45"), Some(3723450));
    }
}
