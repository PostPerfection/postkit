//! Re-time subtitle files (TTML or SRT) from one framerate to another.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RetimeError {
    #[error("Input file not found: {0}")]
    NotFound(PathBuf),
    #[error("Invalid framerate")]
    InvalidFramerate,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct RetimeOptions {
    pub input_file: PathBuf,
    pub output_file: PathBuf,
    pub source_fps_num: u32,
    pub source_fps_den: u32,
    pub target_fps_num: u32,
    pub target_fps_den: u32,
    /// true = stretch timing proportionally, false = snap to nearest frame
    pub stretch: bool,
}

#[derive(Debug, Clone)]
pub struct RetimeResult {
    pub output_file: PathBuf,
    pub entries_processed: u32,
    pub time_shift_ms: f64,
}

/// Parse a TTML time code like "HH:MM:SS:FF" or "HH:MM:SS.mmm" to seconds.
fn parse_ttml_time(t: &str, fps: f64) -> f64 {
    let parts: Vec<&str> = t.split(':').collect();
    if parts.len() == 4 {
        let h: f64 = parts[0].parse().unwrap_or(0.0);
        let m: f64 = parts[1].parse().unwrap_or(0.0);
        let s: f64 = parts[2].parse().unwrap_or(0.0);
        let f: f64 = parts[3].parse().unwrap_or(0.0);
        return h * 3600.0 + m * 60.0 + s + f / fps;
    }

    if parts.len() == 3 {
        let h: f64 = parts[0].parse().unwrap_or(0.0);
        let m: f64 = parts[1].parse().unwrap_or(0.0);
        if let Some((sec, ms)) = parts[2].split_once('.') {
            let s: f64 = sec.parse().unwrap_or(0.0);
            let frac: f64 = ms.parse::<f64>().unwrap_or(0.0);
            let divisor = 10f64.powi(ms.len() as i32);
            return h * 3600.0 + m * 60.0 + s + frac / divisor;
        }
        let s: f64 = parts[2].parse().unwrap_or(0.0);
        return h * 3600.0 + m * 60.0 + s;
    }

    0.0
}

/// Format seconds as "HH:MM:SS:FF" TTML time code.
fn format_ttml_time(seconds: f64, fps: f64) -> String {
    let h = (seconds / 3600.0) as u32;
    let rem = seconds - (h as f64) * 3600.0;
    let m = (rem / 60.0) as u32;
    let rem = rem - (m as f64) * 60.0;
    let s = rem as u32;
    let frac = rem - s as f64;
    let f = (frac * fps).round() as u32;
    format!("{:02}:{:02}:{:02}:{:02}", h, m, s, f)
}

/// Format seconds as "HH:MM:SS,mmm" SRT time code.
fn format_srt_time(seconds: f64) -> String {
    let total = seconds.max(0.0);
    let h = (total / 3600.0) as u32;
    let rem = total - (h as f64) * 3600.0;
    let m = (rem / 60.0) as u32;
    let rem = rem - (m as f64) * 60.0;
    let s = rem as u32;
    let ms = ((rem - s as f64) * 1000.0).round() as u32;
    format!("{:02}:{:02}:{:02},{:03}", h, m, s, ms)
}

/// Parse an SRT time code "HH:MM:SS,mmm" to seconds.
fn parse_srt_time(t: &str) -> f64 {
    let (hms, ms_str) = t.split_once(',').unwrap_or((t, "0"));
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return 0.0;
    }
    let h: f64 = parts[0].parse().unwrap_or(0.0);
    let m: f64 = parts[1].parse().unwrap_or(0.0);
    let s: f64 = parts[2].parse().unwrap_or(0.0);
    let ms: f64 = ms_str.parse().unwrap_or(0.0);
    h * 3600.0 + m * 60.0 + s + ms / 1000.0
}

/// Re-time a subtitle file from one framerate to another.
pub fn retime_subtitles(opts: &RetimeOptions) -> Result<RetimeResult, RetimeError> {
    if !opts.input_file.exists() {
        return Err(RetimeError::NotFound(opts.input_file.clone()));
    }

    let src_fps = f64::from(opts.source_fps_num) / f64::from(opts.source_fps_den);
    let tgt_fps = f64::from(opts.target_fps_num) / f64::from(opts.target_fps_den);

    if src_fps <= 0.0 || tgt_fps <= 0.0 {
        return Err(RetimeError::InvalidFramerate);
    }

    let ratio = tgt_fps / src_fps;

    let content = fs::read_to_string(&opts.input_file)?;
    let is_ttml = content.contains("<tt");

    let (output, entries) = if is_ttml {
        retime_ttml(&content, src_fps, tgt_fps, ratio, opts.stretch)
    } else {
        retime_srt(&content, ratio, opts.stretch)
    };

    fs::write(&opts.output_file, &output)?;

    Ok(RetimeResult {
        output_file: opts.output_file.clone(),
        entries_processed: entries / 2,
        time_shift_ms: 0.0,
    })
}

fn retime_ttml(
    content: &str,
    src_fps: f64,
    tgt_fps: f64,
    ratio: f64,
    stretch: bool,
) -> (String, u32) {
    let mut output = String::with_capacity(content.len());
    let mut entries = 0u32;
    let mut remaining = content;

    while let Some(attr_pos) = find_time_attr(remaining) {
        let (prefix, attr_name, time_val, rest) = attr_pos;
        output.push_str(prefix);

        let t = parse_ttml_time(time_val, src_fps);
        let new_t = if stretch { t / ratio } else { t };
        let new_time = format_ttml_time(new_t, tgt_fps);

        let _ = write!(output, "{}=\"{}\"", attr_name, new_time);
        remaining = rest;
        entries += 1;
    }
    output.push_str(remaining);

    (output, entries)
}

fn find_time_attr(s: &str) -> Option<(&str, &str, &str, &str)> {
    let patterns = ["begin=\"", "end=\""];

    let mut best: Option<(usize, &str)> = None;
    for pat in &patterns {
        if let Some(pos) = s.find(pat) {
            match best {
                None => best = Some((pos, pat)),
                Some((best_pos, _)) if pos < best_pos => best = Some((pos, pat)),
                _ => {}
            }
        }
    }

    let (pos, pat) = best?;
    let attr_name = &pat[..pat.len() - 2];
    let val_start = pos + pat.len();
    let val_end = s[val_start..].find('"')? + val_start;
    let time_val = &s[val_start..val_end];

    if time_val.len() >= 8 && time_val.as_bytes()[2] == b':' {
        let prefix = &s[..pos];
        let rest = &s[val_end + 1..];
        Some((prefix, attr_name, time_val, rest))
    } else {
        let skip_to = val_end + 1;
        let after = &s[skip_to..];
        let (sub_prefix, sub_attr, sub_val, sub_rest) = find_time_attr(after)?;
        let full_prefix_end = skip_to + sub_prefix.len();
        Some((&s[..full_prefix_end], sub_attr, sub_val, sub_rest))
    }
}

fn retime_srt(content: &str, ratio: f64, stretch: bool) -> (String, u32) {
    let mut output = String::with_capacity(content.len());
    let mut entries = 0u32;
    let mut remaining = content;

    while let Some(pos) = find_srt_timecode(remaining) {
        let (prefix, start_tc, end_tc, rest) = pos;
        output.push_str(prefix);

        let start_secs = parse_srt_time(start_tc);
        let end_secs = parse_srt_time(end_tc);

        let new_start = if stretch {
            start_secs / ratio
        } else {
            start_secs
        };
        let new_end = if stretch { end_secs / ratio } else { end_secs };

        let _ = write!(
            output,
            "{} --> {}",
            format_srt_time(new_start),
            format_srt_time(new_end)
        );
        remaining = rest;
        entries += 2;
    }
    output.push_str(remaining);

    (output, entries)
}

fn find_srt_timecode(s: &str) -> Option<(&str, &str, &str, &str)> {
    let arrow = " --> ";
    let pos = s.find(arrow)?;

    if pos < 12 {
        let rest = &s[pos + arrow.len()..];
        let (sub_prefix, start, end, sub_rest) = find_srt_timecode(rest)?;
        let prefix_len = pos + arrow.len() + sub_prefix.len();
        return Some((&s[..prefix_len], start, end, sub_rest));
    }

    let start_tc = &s[pos - 12..pos];
    if !is_srt_tc(start_tc) {
        let rest = &s[pos + arrow.len()..];
        return find_srt_timecode(rest).map(|(sub_prefix, start, end, sub_rest)| {
            let prefix_len = pos + arrow.len() + sub_prefix.len();
            (&s[..prefix_len], start, end, sub_rest)
        });
    }

    let after_arrow = &s[pos + arrow.len()..];
    if after_arrow.len() < 12 {
        return None;
    }
    let end_tc = &after_arrow[..12];
    if !is_srt_tc(end_tc) {
        return None;
    }

    let prefix = &s[..pos - 12];
    let rest = &after_arrow[12..];
    Some((prefix, start_tc, end_tc, rest))
}

fn is_srt_tc(s: &str) -> bool {
    s.len() == 12 && s.as_bytes()[2] == b':' && s.as_bytes()[5] == b':' && s.as_bytes()[8] == b','
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_ttml_time_frames() {
        let fps = 24.0;
        assert!((parse_ttml_time("01:00:00:00", fps) - 3600.0).abs() < 0.001);
        assert!((parse_ttml_time("00:01:00:12", fps) - 60.5).abs() < 0.001);
        assert!((parse_ttml_time("00:00:01:00", fps) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_ttml_time_millis() {
        let fps = 24.0;
        assert!((parse_ttml_time("00:00:01.500", fps) - 1.5).abs() < 0.001);
        assert!((parse_ttml_time("01:30:00.000", fps) - 5400.0).abs() < 0.001);
    }

    #[test]
    fn test_format_ttml_time() {
        assert_eq!(format_ttml_time(3600.0, 24.0), "01:00:00:00");
        assert_eq!(format_ttml_time(60.5, 24.0), "00:01:00:12");
    }

    #[test]
    fn test_parse_srt_time() {
        assert!((parse_srt_time("01:00:00,000") - 3600.0).abs() < 0.001);
        assert!((parse_srt_time("00:01:30,500") - 90.5).abs() < 0.001);
    }

    #[test]
    fn test_format_srt_time() {
        assert_eq!(format_srt_time(3600.0), "01:00:00,000");
        assert_eq!(format_srt_time(90.5), "00:01:30,500");
    }

    #[test]
    fn test_retime_srt_stretch() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("test.srt");
        let output = tmp.path().join("out.srt");

        let srt_content =
            "1\n00:00:01,000 --> 00:00:02,000\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nWorld\n";
        fs::write(&input, srt_content).unwrap();

        let opts = RetimeOptions {
            input_file: input,
            output_file: output.clone(),
            source_fps_num: 24000,
            source_fps_den: 1001,
            target_fps_num: 24,
            target_fps_den: 1,
            stretch: true,
        };

        let result = retime_subtitles(&opts).unwrap();
        assert_eq!(result.entries_processed, 2);

        let out_content = fs::read_to_string(&output).unwrap();
        assert!(out_content.contains("-->"));
        assert!(out_content.contains("00:00:00,"));
    }

    #[test]
    fn test_retime_ttml() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("test.ttml");
        let output = tmp.path().join("out.ttml");

        let ttml_content = r#"<?xml version="1.0"?>
<tt xmlns="http://www.w3.org/ns/ttml">
  <body>
    <div>
      <p begin="00:00:01:00" end="00:00:02:00">Hello</p>
      <p begin="00:00:03:00" end="00:00:04:00">World</p>
    </div>
  </body>
</tt>"#;
        fs::write(&input, ttml_content).unwrap();

        let opts = RetimeOptions {
            input_file: input,
            output_file: output.clone(),
            source_fps_num: 24,
            source_fps_den: 1,
            target_fps_num: 25,
            target_fps_den: 1,
            stretch: true,
        };

        let result = retime_subtitles(&opts).unwrap();
        assert_eq!(result.entries_processed, 2);

        let out_content = fs::read_to_string(&output).unwrap();
        assert!(out_content.contains("begin="));
        assert!(out_content.contains("end="));
    }

    #[test]
    fn test_missing_input() {
        let opts = RetimeOptions {
            input_file: PathBuf::from("/nonexistent/file.srt"),
            output_file: PathBuf::from("/tmp/out.srt"),
            source_fps_num: 24,
            source_fps_den: 1,
            target_fps_num: 25,
            target_fps_den: 1,
            stretch: false,
        };

        assert!(retime_subtitles(&opts).is_err());
    }

    #[test]
    fn test_invalid_framerate() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("test.srt");
        fs::write(&input, "1\n00:00:01,000 --> 00:00:02,000\nHi\n").unwrap();

        let opts = RetimeOptions {
            input_file: input,
            output_file: tmp.path().join("out.srt"),
            source_fps_num: 0,
            source_fps_den: 1,
            target_fps_num: 24,
            target_fps_den: 1,
            stretch: false,
        };

        assert!(retime_subtitles(&opts).is_err());
    }

    #[test]
    fn test_no_stretch_passthrough() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("test.srt");
        let output = tmp.path().join("out.srt");

        let srt_content = "1\n00:00:01,000 --> 00:00:02,000\nHello\n";
        fs::write(&input, srt_content).unwrap();

        let opts = RetimeOptions {
            input_file: input,
            output_file: output.clone(),
            source_fps_num: 24,
            source_fps_den: 1,
            target_fps_num: 25,
            target_fps_den: 1,
            stretch: false,
        };

        let result = retime_subtitles(&opts).unwrap();
        assert_eq!(result.entries_processed, 1);

        let out_content = fs::read_to_string(&output).unwrap();
        assert!(out_content.contains("00:00:01,000 --> 00:00:02,000"));
    }
}
