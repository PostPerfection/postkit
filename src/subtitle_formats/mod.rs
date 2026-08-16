//! Subtitle input parsers and text utilities for the wizards.
//!
//! Parsers turn each source format into `StyledCue`s (timing, styled text runs,
//! optional alignment/position/image). `to_srt_cue` collapses a `StyledCue` back
//! to the plain `subtitle_retime::SrtCue` so existing consumers keep working.

use std::path::PathBuf;

use thiserror::Error;

use crate::subtitle_retime::SrtCue;

pub mod ass;
pub mod bidi;
pub mod fcpxml;
pub mod interop;
pub mod mks;
pub mod pac;
pub mod wrap;

#[derive(Debug, Error)]
pub enum SubtitleError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("XML error: {0}")]
    Xml(String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Unsupported PAC codepage {0}: only Latin (0) is implemented")]
    UnsupportedCodepage(u8),
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),
    #[error("No subtitle stream found in {0}")]
    NoSubtitleStream(PathBuf),
    #[error("Referenced image not found: {0}")]
    MissingImage(PathBuf),
    #[error("Not a PNG (bad magic): {0}")]
    BadPng(PathBuf),
}

/// Horizontal anchor for a cue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// Vertical anchor for a cue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

/// 8-bit RGBA colour (matches SMPTE subtitle FontColor ARGB channels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// Read a colour written as `RRGGBB` or `RRGGBBAA`, with an optional
    /// leading `#` and either case. Alpha is opaque when it is left off.
    pub fn parse_hex(text: &str) -> Result<Rgba, String> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        let bad = || format!("{text} is not a colour: write it as RRGGBB or RRGGBBAA");
        if !digits.is_ascii() {
            return Err(bad());
        }
        let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).map_err(|_| bad());
        let alpha = match digits.len() {
            RGB_HEX_DIGITS => u8::MAX,
            RGBA_HEX_DIGITS => channel(6)?,
            _ => return Err(bad()),
        };
        Ok(Rgba {
            r: channel(0)?,
            g: channel(2)?,
            b: channel(4)?,
            a: alpha,
        })
    }
}

/// Hex digits in a colour without an alpha channel.
const RGB_HEX_DIGITS: usize = 6;

/// Hex digits in a colour with an alpha channel.
const RGBA_HEX_DIGITS: usize = 8;

/// One run of text sharing a single style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledRun {
    pub text: String,
    pub italic: bool,
    pub bold: bool,
    pub underline: bool,
    pub color: Option<Rgba>,
}

impl StyledRun {
    /// A plain (unstyled) run.
    pub fn plain(text: impl Into<String>) -> Self {
        StyledRun {
            text: text.into(),
            italic: false,
            bold: false,
            underline: false,
            color: None,
        }
    }
}

/// A cue extending `SrtCue` with styling, layout hints, and bitmap support.
///
/// `start_ms`/`end_ms` use the same millisecond representation `parse_srt`
/// produces. Text lives in `runs`; run text may contain '\n' for line breaks.
#[derive(Debug, Clone, PartialEq)]
pub struct StyledCue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub runs: Vec<StyledRun>,
    pub align: Option<HAlign>,
    pub valign: Option<VAlign>,
    /// vertical position as a percent of screen height (0 = top).
    pub vposition: Option<f32>,
    /// bitmap-subtitle image path (Interop PNG subs), resolved and validated.
    pub image: Option<PathBuf>,
}

impl StyledCue {
    /// A text cue with default styling and no layout hints.
    pub fn text(start_ms: u64, end_ms: u64, runs: Vec<StyledRun>) -> Self {
        StyledCue {
            start_ms,
            end_ms,
            runs,
            align: None,
            valign: None,
            vposition: None,
            image: None,
        }
    }

    /// Concatenated plain text of every run (styling dropped).
    pub fn plain_text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// Lossless-to-plain conversion so consumers of `SrtCue` keep working.
    ///
    /// Styling, layout, and any bitmap reference are dropped; timing and text
    /// survive. Bitmap-only cues yield empty text.
    pub fn to_srt_cue(&self, index: u32) -> SrtCue {
        SrtCue {
            index,
            start_ms: self.start_ms,
            end_ms: self.end_ms,
            text: self.plain_text(),
        }
    }
}

/// Convert a slice of `StyledCue`s to plain `SrtCue`s (1-based index).
pub fn to_srt_cues(cues: &[StyledCue]) -> Vec<SrtCue> {
    cues.iter()
        .enumerate()
        .map(|(i, c)| c.to_srt_cue(i as u32 + 1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_srt_cue_flattens_runs_and_keeps_timing() {
        let cue = StyledCue::text(
            1000,
            4000,
            vec![
                StyledRun::plain("hello "),
                StyledRun {
                    text: "world".into(),
                    italic: true,
                    bold: false,
                    underline: false,
                    color: None,
                },
            ],
        );
        let srt = cue.to_srt_cue(1);
        assert_eq!(srt.index, 1);
        assert_eq!(srt.start_ms, 1000);
        assert_eq!(srt.end_ms, 4000);
        assert_eq!(srt.text, "hello world");
    }

    #[test]
    fn parse_hex_reads_six_and_eight_digit_colours_in_either_case() {
        assert_eq!(
            Rgba::parse_hex("ff8000").unwrap(),
            Rgba {
                r: 255,
                g: 128,
                b: 0,
                a: 255
            }
        );
        assert_eq!(
            Rgba::parse_hex("#FF800040").unwrap(),
            Rgba {
                r: 255,
                g: 128,
                b: 0,
                a: 64
            }
        );
        assert_eq!(
            Rgba::parse_hex("#aAbBcC").unwrap(),
            Rgba {
                r: 170,
                g: 187,
                b: 204,
                a: 255
            }
        );
    }

    #[test]
    fn parse_hex_refuses_a_bad_length_or_a_bad_digit() {
        for text in ["fff", "ff80001", "", "#", "ff80zz", "ffgg0011"] {
            let err = Rgba::parse_hex(text).unwrap_err();
            assert!(
                err.contains("RRGGBB") && err.contains("RRGGBBAA"),
                "{text} reported as: {err}"
            );
        }
    }

    #[test]
    fn to_srt_cues_numbers_from_one() {
        let cues = vec![
            StyledCue::text(0, 10, vec![StyledRun::plain("a")]),
            StyledCue::text(20, 30, vec![StyledRun::plain("b")]),
        ];
        let out = to_srt_cues(&cues);
        assert_eq!(out[0].index, 1);
        assert_eq!(out[1].index, 2);
    }
}
