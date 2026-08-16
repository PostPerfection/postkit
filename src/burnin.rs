use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Burn-in options for subtitle/watermark overlay.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BurninOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Subtitle file path (SRT, VTT, ASS). Subtitle XML is refused: ffmpeg has
    /// no reader for it.
    pub subtitle_file: Option<PathBuf>,
    /// Burn-in text (for watermark/slate)
    pub text: Option<String>,
    /// Font size in pixels. 0 means the default: 24 for burnt-in text, and for
    /// subtitles whatever the subtitle file and ffmpeg pick.
    pub font_size: u32,
    /// Font colour as hex RRGGBB (e.g. "FFFFFF"), optionally "#" or "0x"
    /// prefixed. Burnt-in text also takes any ffmpeg colour name and defaults to
    /// white; subtitles need the hex form and keep the subtitle file's own
    /// colour when this is empty.
    pub font_colour: String,
    /// Position: "top", "center" or "bottom"; anything else non-empty is
    /// bottom. Empty means bottom for burnt-in text and the subtitle file's own
    /// placement for subtitles.
    pub position: String,
}

/// Both SMPTE ST 428-7 and Interop subtitle XML, matched as substrings so a
/// namespace prefix on the root element still counts.
const SUBTITLE_XML_ROOTS: [&str; 2] = ["SubtitleReel", "DCSubtitle"];
const SUBTITLE_SNIFF_BYTES: u64 = 4096;
const DEFAULT_TEXT_FONT_SIZE: u32 = 24;
const DEFAULT_TEXT_COLOUR: &str = "white";

/// Burn subtitles or text into video frames using ffmpeg.
pub fn burnin(opts: &BurninOptions) -> std::io::Result<()> {
    let mut args = vec!["-i".to_string(), opts.input.to_string_lossy().to_string()];

    if let Some(ref sub) = opts.subtitle_file {
        args.push("-vf".to_string());
        args.push(subtitle_filter(opts, sub)?);
    } else if let Some(ref text) = opts.text {
        args.push("-vf".to_string());
        args.push(text_filter(opts, text));
    }

    args.push("-y".to_string());
    args.push(opts.output.to_string_lossy().to_string());

    let output = std::process::Command::new("ffmpeg").args(&args).output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::other(format!(
            "ffmpeg burn-in failed: {stderr}"
        )));
    }

    Ok(())
}

/// Build the `subtitles` filter, carrying the styling options as ASS style
/// overrides.
fn subtitle_filter(opts: &BurninOptions, subtitle: &Path) -> std::io::Result<String> {
    reject_subtitle_xml(subtitle)?;

    let mut style = Vec::new();
    if opts.font_size > 0 {
        style.push(format!("FontSize={}", opts.font_size));
    }
    if !opts.font_colour.is_empty() {
        let colour = ass_primary_colour(&opts.font_colour).ok_or_else(|| {
            std::io::Error::other(format!(
                "font colour {:?} is not RRGGBB hex, which is what a subtitle burn-in needs",
                opts.font_colour
            ))
        })?;
        style.push(format!("PrimaryColour={colour}"));
    }
    if !opts.position.is_empty() {
        style.push(format!("Alignment={}", ass_alignment(&opts.position)));
    }

    let mut filter = format!(
        "subtitles=f={}",
        filter_argument(&subtitle.to_string_lossy())
    );
    if !style.is_empty() {
        filter.push_str(&format!(
            ":force_style={}",
            filter_argument(&style.join(","))
        ));
    }
    Ok(filter)
}

/// Build the `drawtext` filter for a text watermark or slate.
fn text_filter(opts: &BurninOptions, text: &str) -> String {
    let font_size = if opts.font_size > 0 {
        opts.font_size
    } else {
        DEFAULT_TEXT_FONT_SIZE
    };
    let colour = if opts.font_colour.is_empty() {
        DEFAULT_TEXT_COLOUR
    } else {
        &opts.font_colour
    };
    let y_pos = match opts.position.as_str() {
        "top" => "10",
        "center" => "(h-text_h)/2",
        _ => "h-th-10",
    };
    format!(
        "drawtext=text={}:fontsize={font_size}:fontcolor={}:x=(w-text_w)/2:y={y_pos}",
        filter_argument(text),
        filter_argument(colour)
    )
}

/// Escape one option value for an ffmpeg filtergraph. Two levels: quote it for
/// the filter's own argument parser, then escape the characters that separate
/// filters in the graph description over the whole quoted token.
fn filter_argument(value: &str) -> String {
    let quoted = format!("'{}'", value.replace('\'', "'\\''"));
    let mut escaped = String::with_capacity(quoted.len());
    for c in quoted.chars() {
        if matches!(c, '\\' | '\'' | '[' | ']' | ',' | ';') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// ASS PrimaryColour is `&HAABBGGRR&`: alpha first, then the RGB triple
/// reversed. Alpha 00 is fully opaque.
fn ass_primary_colour(hex: &str) -> Option<String> {
    let digits = hex
        .trim_start_matches('#')
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (red, green, blue) = (&digits[0..2], &digits[2..4], &digits[4..6]);
    Some(format!("&H00{blue}{green}{red}&").to_uppercase())
}

/// force_style Alignment is the legacy SSA numbering, not the numpad one: 1-3
/// are the bottom row, +4 moves to the top, +8 to the middle.
fn ass_alignment(position: &str) -> &'static str {
    match position {
        "top" => "6",
        "center" => "10",
        _ => "2",
    }
}

/// ffmpeg reads no XML subtitle format, so a SMPTE or Interop subtitle asset
/// would die deep inside ffmpeg with an unhelpful message.
fn reject_subtitle_xml(subtitle: &Path) -> std::io::Result<()> {
    let mut head = Vec::new();
    std::fs::File::open(subtitle)
        .and_then(|f| f.take(SUBTITLE_SNIFF_BYTES).read_to_end(&mut head))
        .map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("cannot read subtitle {}: {e}", subtitle.display()),
            )
        })?;

    let head = String::from_utf8_lossy(&head);
    let is_subtitle_xml = SUBTITLE_XML_ROOTS.iter().any(|root| head.contains(root));
    let has_xml_extension = subtitle
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xml"));
    if !is_subtitle_xml && !has_xml_extension {
        return Ok(());
    }

    let described = if is_subtitle_xml {
        "SMPTE ST 428-7 (DCST) or Interop subtitle XML"
    } else {
        "XML subtitles"
    };
    Err(std::io::Error::other(format!(
        "cannot burn in {}: ffmpeg has no reader for {described}. Convert it to SRT or ASS first; dcpwizard's subtitle-extract writes SRT from a DCST asset.",
        subtitle.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn have_ffmpeg() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// A directory name carrying every character that ends a filter or a filter
    /// argument.
    fn awkward_dir(root: &Path) -> PathBuf {
        let dir = root.join("re:el 1, take 'two' [a]");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn srt(dir: &Path) -> PathBuf {
        let path = dir.join("cues.srt");
        std::fs::write(&path, "1\n00:00:00,000 --> 00:00:02,000\nhello\n\n").unwrap();
        path
    }

    fn one_frame_video(dir: &Path) -> PathBuf {
        let path = dir.join("in.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x240:rate=5:duration=1")
            .arg("-y")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "fixture encode failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        path
    }

    #[test]
    fn filter_argument_escapes_the_separators() {
        assert_eq!(filter_argument("plain.srt"), "\\'plain.srt\\'");
        assert_eq!(
            filter_argument("/a: b, c/x.srt"),
            "\\'/a: b\\, c/x.srt\\'",
            "a comma ends a filter, a colon ends an argument"
        );
        assert_eq!(filter_argument("it's"), "\\'it\\'\\\\\\'\\'s\\'");
        assert_eq!(filter_argument("a\\b"), "\\'a\\\\b\\'");
        assert_eq!(filter_argument("[a];b"), "\\'\\[a\\]\\;b\\'");
    }

    #[test]
    fn styling_options_reach_the_subtitle_filter() {
        let dir = TempDir::new().unwrap();
        let subtitle = srt(dir.path());
        let opts = BurninOptions {
            subtitle_file: Some(subtitle.clone()),
            font_size: 30,
            font_colour: "FF8000".into(),
            position: "top".into(),
            ..Default::default()
        };
        let filter = subtitle_filter(&opts, &subtitle).unwrap();
        assert!(
            filter.contains("FontSize=30"),
            "font size missing from {filter}"
        );
        assert!(
            filter.contains("PrimaryColour=&H000080FF&"),
            "colour missing or not AABBGGRR in {filter}"
        );
        assert!(
            filter.contains("Alignment=6"),
            "position missing from {filter}"
        );
    }

    #[test]
    fn unset_styling_leaves_the_subtitle_file_alone() {
        let dir = TempDir::new().unwrap();
        let subtitle = srt(dir.path());
        let opts = BurninOptions {
            subtitle_file: Some(subtitle.clone()),
            ..Default::default()
        };
        let filter = subtitle_filter(&opts, &subtitle).unwrap();
        assert!(
            !filter.contains("force_style"),
            "unset options forced a style: {filter}"
        );
    }

    #[test]
    fn a_font_colour_that_is_not_hex_fails_loud() {
        let dir = TempDir::new().unwrap();
        let subtitle = srt(dir.path());
        let opts = BurninOptions {
            subtitle_file: Some(subtitle.clone()),
            font_colour: "white".into(),
            ..Default::default()
        };
        let error = subtitle_filter(&opts, &subtitle).unwrap_err().to_string();
        assert!(error.contains("RRGGBB"), "unhelpful error: {error}");
    }

    #[test]
    fn hex_colours_take_a_prefix_and_any_case() {
        assert_eq!(ass_primary_colour("ffffff").unwrap(), "&H00FFFFFF&");
        assert_eq!(ass_primary_colour("#123456").unwrap(), "&H00563412&");
        assert_eq!(ass_primary_colour("0x123456").unwrap(), "&H00563412&");
        assert!(ass_primary_colour("12345").is_none());
        assert!(ass_primary_colour("12345g").is_none());
    }

    #[test]
    fn subtitle_xml_is_refused_with_the_remedy() {
        let dir = TempDir::new().unwrap();
        for (name, body) in [
            (
                "reel.xml",
                "<?xml version=\"1.0\"?>\n<dcst:SubtitleReel xmlns:dcst=\"http://www.smpte-ra.org/schemas/428-7/2010/DCST\"/>",
            ),
            ("interop.xml", "<DCSubtitle Version=\"1.0\"/>"),
            // no .xml extension, so only the sniff catches it
            ("reel.sub", "<SubtitleReel/>"),
            // not a subtitle format we know, but ffmpeg cannot read it either
            ("cues.xml", "<tt xmlns=\"http://www.w3.org/ns/ttml\"/>"),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            let error = reject_subtitle_xml(&path).unwrap_err().to_string();
            assert!(error.contains("SRT"), "{name}: unhelpful error: {error}");
        }
    }

    #[test]
    fn srt_and_ass_pass_the_xml_check() {
        let dir = TempDir::new().unwrap();
        assert!(reject_subtitle_xml(&srt(dir.path())).is_ok());
        let ass = dir.path().join("styled.ass");
        std::fs::write(&ass, "[Script Info]\nScriptType: v4.00+\n").unwrap();
        assert!(reject_subtitle_xml(&ass).is_ok());
    }

    #[test]
    fn a_missing_subtitle_names_itself() {
        let error = reject_subtitle_xml(Path::new("/nowhere/none.srt"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("none.srt"), "unhelpful error: {error}");
    }

    #[test]
    fn ffmpeg_burns_in_subtitles_from_an_awkward_path() {
        if !have_ffmpeg() {
            eprintln!("skipping burn-in test: ffmpeg not found");
            return;
        }
        let root = TempDir::new().unwrap();
        let dir = awkward_dir(root.path());
        let opts = BurninOptions {
            input: one_frame_video(root.path()),
            output: dir.join("out.mp4"),
            subtitle_file: Some(srt(&dir)),
            font_size: 30,
            font_colour: "00FF00".into(),
            position: "center".into(),
            ..Default::default()
        };
        burnin(&opts).unwrap();
        assert!(opts.output.exists());
    }

    #[test]
    fn ffmpeg_burns_in_text_holding_filter_separators() {
        if !have_ffmpeg() {
            eprintln!("skipping burn-in test: ffmpeg not found");
            return;
        }
        let root = TempDir::new().unwrap();
        let opts = BurninOptions {
            input: one_frame_video(root.path()),
            output: root.path().join("out.mp4"),
            text: Some("reel 1, take 'two': [final]".into()),
            font_size: 20,
            font_colour: "FFFF00".into(),
            position: "top".into(),
            ..Default::default()
        };
        burnin(&opts).unwrap();
        assert!(opts.output.exists());
    }
}
