//! Shared SMPTE timecode and frame-rate math.
//!
//! Single home for the timecode/framerate conversions that used to be copied
//! across edl_import, probe and subtitle_retime.

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
}
