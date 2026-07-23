//! Shared SMPTE timecode and frame-rate math.
//!
//! Single home for the timecode/framerate conversions that used to be copied
//! across edl_import, probe and subtitle_retime.

use serde::{Deserialize, Serialize};

/// Parse a frame rate string: "num/den" (e.g. "24000/1001") or a plain number
/// like "24" or "23.976". Returns (numerator, denominator).
pub fn parse_frame_rate(s: &str) -> Option<(u32, u32)> {
    if let Some((num, den)) = s.split_once('/') {
        Some((num.parse().ok()?, den.parse().ok()?))
    } else {
        // Might be a plain number like "24"
        let fps: f64 = s.parse().ok()?;
        // Convert common rates
        let (num, den) = match fps as u32 {
            24 => (24, 1),
            25 => (25, 1),
            30 => (30, 1),
            48 => (48, 1),
            _ => ((fps * 1000.0) as u32, 1000),
        };
        Some((num, den))
    }
}

/// Convert an "HH:MM:SS:FF" timecode to an absolute frame count.
pub fn timecode_to_frames(tc: &str, fps: f64) -> u32 {
    let parts: Vec<&str> = tc.split(':').collect();
    if parts.len() == 4 {
        let h: u32 = parts[0].parse().unwrap_or(0);
        let m: u32 = parts[1].parse().unwrap_or(0);
        let s: u32 = parts[2].parse().unwrap_or(0);
        let f: u32 = parts[3].parse().unwrap_or(0);
        ((h * 3600 + m * 60 + s) as f64 * fps) as u32 + f
    } else {
        0
    }
}

/// Parse an "HH:MM:SS:FF" (frame field) or "HH:MM:SS.mmm" (fractional) timecode
/// to seconds.
pub fn timecode_to_seconds(t: &str, fps: f64) -> f64 {
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

/// Format seconds as an "HH:MM:SS:FF" timecode.
pub fn seconds_to_timecode(seconds: f64, fps: f64) -> String {
    let h = (seconds / 3600.0) as u32;
    let rem = seconds - (h as f64) * 3600.0;
    let m = (rem / 60.0) as u32;
    let rem = rem - (m as f64) * 60.0;
    let s = rem as u32;
    let frac = rem - s as f64;
    let f = (frac * fps).round() as u32;
    format!("{h:02}:{m:02}:{s:02}:{f:02}")
}

/// SMPTE timecode with an integer frame rate. Complements the free functions
/// above for callers that want a structured value with drop-frame support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub fps: u8,
    pub drop_frame: bool,
}

impl Timecode {
    pub fn new(hours: u8, minutes: u8, seconds: u8, frames: u8, fps: u8) -> Self {
        Self {
            hours,
            minutes,
            seconds,
            frames,
            fps,
            drop_frame: false,
        }
    }

    /// Parse timecode from "HH:MM:SS:FF" or "HH:MM:SS;FF" (drop-frame).
    pub fn parse(s: &str, fps: u8) -> Result<Self, String> {
        let drop_frame = s.contains(';');
        let s = s.replace(';', ":");
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 4 {
            return Err(format!("Invalid timecode format: {s}"));
        }
        Ok(Self {
            hours: parts[0].parse().map_err(|_| "Invalid hours")?,
            minutes: parts[1].parse().map_err(|_| "Invalid minutes")?,
            seconds: parts[2].parse().map_err(|_| "Invalid seconds")?,
            frames: parts[3].parse().map_err(|_| "Invalid frames")?,
            fps,
            drop_frame,
        })
    }

    /// Total frame count, applying SMPTE drop-frame compensation at 30/60 fps.
    pub fn to_frames(&self) -> u64 {
        let fps = self.fps as u64;
        let total = (self.hours as u64) * 3600 * fps
            + (self.minutes as u64) * 60 * fps
            + (self.seconds as u64) * fps
            + (self.frames as u64);

        if self.drop_frame && (self.fps == 30 || self.fps == 60) {
            let d = if self.fps == 30 { 2u64 } else { 4u64 };
            let total_minutes = (self.hours as u64) * 60 + (self.minutes as u64);
            let dropped = d * (total_minutes - total_minutes / 10);
            total - dropped
        } else {
            total
        }
    }

    /// Convert a non-drop-frame frame count to a timecode.
    pub fn from_frames(mut frame_count: u64, fps: u8) -> Self {
        let f = fps as u64;
        let frames = (frame_count % f) as u8;
        frame_count /= f;
        let seconds = (frame_count % 60) as u8;
        frame_count /= 60;
        let minutes = (frame_count % 60) as u8;
        let hours = (frame_count / 60) as u8;
        Self {
            hours,
            minutes,
            seconds,
            frames,
            fps,
            drop_frame: false,
        }
    }
}

impl std::fmt::Display for Timecode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sep = if self.drop_frame { ';' } else { ':' };
        write!(
            f,
            "{:02}:{:02}:{:02}{sep}{:02}",
            self.hours, self.minutes, self.seconds, self.frames
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frame_rate() {
        assert_eq!(parse_frame_rate("24000/1001"), Some((24000, 1001)));
        assert_eq!(parse_frame_rate("24"), Some((24, 1)));
        assert_eq!(parse_frame_rate("25"), Some((25, 1)));
        assert_eq!(parse_frame_rate("23.976"), Some((23976, 1000)));
        assert_eq!(parse_frame_rate("notanumber"), None);
    }

    #[test]
    fn test_timecode_to_frames() {
        assert_eq!(timecode_to_frames("01:00:00:00", 24.0), 86400);
        assert_eq!(timecode_to_frames("00:00:01:00", 24.0), 24);
        assert_eq!(timecode_to_frames("00:00:01:12", 24.0), 36);
        assert_eq!(timecode_to_frames("00:00:00:01", 24.0), 1);
        assert_eq!(timecode_to_frames("bad", 24.0), 0);
    }

    #[test]
    fn test_timecode_to_seconds_frames() {
        let fps = 24.0;
        assert!((timecode_to_seconds("01:00:00:00", fps) - 3600.0).abs() < 0.001);
        assert!((timecode_to_seconds("00:01:00:12", fps) - 60.5).abs() < 0.001);
        assert!((timecode_to_seconds("00:00:01:00", fps) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_timecode_to_seconds_millis() {
        let fps = 24.0;
        assert!((timecode_to_seconds("00:00:01.500", fps) - 1.5).abs() < 0.001);
        assert!((timecode_to_seconds("01:30:00.000", fps) - 5400.0).abs() < 0.001);
    }

    #[test]
    fn test_seconds_to_timecode() {
        assert_eq!(seconds_to_timecode(3600.0, 24.0), "01:00:00:00");
        assert_eq!(seconds_to_timecode(60.5, 24.0), "00:01:00:12");
    }

    #[test]
    fn tc_parse() {
        let tc = Timecode::parse("01:02:03:04", 24).unwrap();
        assert_eq!(tc.hours, 1);
        assert_eq!(tc.minutes, 2);
        assert_eq!(tc.seconds, 3);
        assert_eq!(tc.frames, 4);
        assert!(Timecode::parse("01:02:03", 24).is_err());
    }

    #[test]
    fn tc_to_frames() {
        assert_eq!(Timecode::new(0, 0, 1, 0, 24).to_frames(), 24);
    }

    #[test]
    fn tc_from_frames() {
        let tc = Timecode::from_frames(48, 24);
        assert_eq!(tc.seconds, 2);
        assert_eq!(tc.frames, 0);
    }

    #[test]
    fn tc_display() {
        assert_eq!(Timecode::new(1, 2, 3, 4, 24).to_string(), "01:02:03:04");
    }

    #[test]
    fn tc_drop_frame_display() {
        let mut tc = Timecode::new(1, 2, 3, 4, 30);
        tc.drop_frame = true;
        assert_eq!(tc.to_string(), "01:02:03;04");
    }
}
