use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// EBU R128 loudness measurement result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoudnessResult {
    /// Integrated loudness in LUFS.
    pub integrated_lufs: f64,
    /// Loudness range in LU.
    pub range_lu: f64,
    /// True peak in dBTP.
    pub true_peak_dbtp: f64,
    /// Short-term loudness max in LUFS.
    pub short_term_max_lufs: f64,
    /// Whether measurement succeeded.
    pub success: bool,
    pub error: String,
}

/// Measure audio loudness per EBU R128 using ffmpeg.
pub fn measure_loudness(input: &Path) -> LoudnessResult {
    let output = match std::process::Command::new("ffmpeg")
        .args([
            "-i",
            &input.to_string_lossy(),
            "-af",
            "loudnorm=print_format=json",
            "-f",
            "null",
            "-",
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return LoudnessResult {
                success: false,
                error: format!("Failed to run ffmpeg: {e}"),
                ..Default::default()
            };
        }
    };

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Parse loudnorm JSON output from ffmpeg stderr
    if let Some(json_start) = stderr.rfind('{')
        && let Some(json_end) = stderr[json_start..].find('}')
    {
        let json_str = &stderr[json_start..json_start + json_end + 1];
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
            return LoudnessResult {
                integrated_lufs: val["input_i"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0),
                range_lu: val["input_lra"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0),
                true_peak_dbtp: val["input_tp"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0),
                // loudnorm's JSON has no short-term field; measure it separately.
                short_term_max_lufs: measure_short_term_max(input).unwrap_or(0.0),
                success: true,
                error: String::new(),
            };
        }
    }

    LoudnessResult {
        success: false,
        error: "Failed to parse loudnorm output from ffmpeg".to_string(),
        ..Default::default()
    }
}

/// Max short-term (3s window) loudness via ffmpeg's ebur128 filter.
fn measure_short_term_max(input: &Path) -> Option<f64> {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-i",
            &input.to_string_lossy(),
            "-af",
            "ebur128",
            "-f",
            "null",
            "-",
        ])
        .output()
        .ok()?;
    parse_short_term_max(&String::from_utf8_lossy(&output.stderr))
}

/// Largest `S:` (short-term LUFS) value in ebur128 stderr output.
///
/// ebur128 prints per-window lines like
/// `[Parsed_ebur128_0 @ ..] t: 3 M: -22.0 S: -19.8 I: -23.0 LUFS ...`; the
/// short-term max is the largest finite `S:` across them.
fn parse_short_term_max(stderr: &str) -> Option<f64> {
    let mut max: Option<f64> = None;
    for line in stderr.lines() {
        if let Some(pos) = line.find("S:") {
            let rest = line[pos + 2..].trim_start();
            let token = rest.split_whitespace().next().unwrap_or("");
            if let Ok(v) = token.parse::<f64>()
                && v.is_finite()
            {
                max = Some(max.map_or(v, |m: f64| m.max(v)));
            }
        }
    }
    max
}

// Leq(m) (ISO 21727) cinema loudness, CCIR 468 weighted.

/// Result of a Leq(m) (ISO 21727) measurement.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeqMResult {
    /// Leq(m) in dB, referenced to the SMPTE B-chain calibration.
    pub leq_m_db: f64,
    pub success: bool,
    pub error: String,
}

// fft block length; a power of two keeps rustfft fast. at 48 kHz this is ~0.68 s
// per block, far longer than the ccir 468 impulse response, so applying the
// weighting per block (circular convolution) has negligible boundary error.
const LEQ_BLOCK: usize = 32768;

// cinema B-chain calibration (SMPTE ST 202 / ISO 2969): pink noise at -20 dBFS
// RMS reproduces at 85 dB SPL, so a full-scale RMS signal maps to 105 dB. Leq(m)
// is reported on that scale.
const LEQ_M_REFERENCE_OFFSET_DB: f64 = 105.0;

/// ITU-R 468 (CCIR 468) weighting curve, linear magnitude, before normalization.
/// The published rational approximation is normalized to 0 dB at 2 kHz.
fn ccir468_raw(f: f64) -> f64 {
    let f2 = f * f;
    let h1 = -4.737_338_981_378_384e-24 * f2 * f2 * f2 + 2.043_828_333_606_125e-15 * f2 * f2
        - 1.363_894_795_463_638e-7 * f2
        + 1.0;
    let h2 = 1.306_612_257_412_824e-19 * f2 * f2 * f
        + -2.118_150_887_518_656e-11 * f2 * f
        + 5.559_488_023_498_642e-4 * f;
    1.246_332_637_532_143e-4 * f / (h1 * h1 + h2 * h2).sqrt()
}

// value of the raw curve at 1 kHz; dividing by it re-normalizes to 0 dB at 1 kHz,
// the reference Leq(m) uses (self-checked in a test).
const CCIR468_RAW_AT_1KHZ: f64 = 0.122_480_299_886_641_26;

/// CCIR 468 weighting, linear magnitude, normalized to 1.0 (0 dB) at 1 kHz.
fn ccir468_gain(f: f64) -> f64 {
    ccir468_raw(f) / CCIR468_RAW_AT_1KHZ
}

/// Sum of squares of the CCIR 468-weighted block (Parseval in the frequency
/// domain), i.e. the weighted energy contributed by `samples`. `samples` is
/// zero-padded to the fft length, so its true length still sets the sample count.
fn weighted_block_energy(fft: &dyn rustfft::Fft<f32>, samples: &[f32], sample_rate: u32) -> f64 {
    let n = LEQ_BLOCK;
    let mut buf = vec![Complex::<f32>::new(0.0, 0.0); n];
    for (dst, &s) in buf.iter_mut().zip(samples) {
        dst.re = s;
    }
    fft.process(&mut buf);
    let mut energy = 0.0f64;
    for (k, c) in buf.iter().enumerate() {
        // bin k above Nyquist mirrors the positive frequency n-k
        let bin = if k <= n / 2 { k } else { n - k };
        let freq = bin as f64 * sample_rate as f64 / n as f64;
        let w = ccir468_gain(freq);
        let mag2 = (c.re as f64) * (c.re as f64) + (c.im as f64) * (c.im as f64);
        energy += mag2 * w * w;
    }
    // Parseval: sum_n |x_n|^2 = (1/N) sum_k |X_k|^2
    energy / n as f64
}

/// Compute Leq(m) (ISO 21727) in dB from mono PCM samples in full-scale units
/// (-1.0..=1.0). The signal is CCIR 468-weighted and its equivalent continuous
/// level is referenced to the cinema B-chain calibration.
pub fn leq_m_from_samples(samples: &[f32], sample_rate: u32) -> f64 {
    if samples.is_empty() || sample_rate == 0 {
        return f64::NEG_INFINITY;
    }
    let fft = FftPlanner::<f32>::new().plan_fft_forward(LEQ_BLOCK);
    let mut total_energy = 0.0f64;
    for block in samples.chunks(LEQ_BLOCK) {
        total_energy += weighted_block_energy(fft.as_ref(), block, sample_rate);
    }
    let mean_square = total_energy / samples.len() as f64;
    10.0 * mean_square.log10() + LEQ_M_REFERENCE_OFFSET_DB
}

/// Measure Leq(m) (ISO 21727) of an audio file. Decodes to mono f32 PCM at
/// 48 kHz via ffmpeg and processes it in bounded blocks.
pub fn measure_leq_m(audio_file: &Path) -> LeqMResult {
    if !audio_file.exists() {
        return LeqMResult {
            error: "Audio file not found".into(),
            ..Default::default()
        };
    }

    const SR: u32 = 48000;
    let mut child = match Command::new("ffmpeg")
        .args([
            "-v",
            "quiet",
            "-i",
            &audio_file.to_string_lossy(),
            "-ac",
            "1",
            "-ar",
            "48000",
            "-f",
            "f32le",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return LeqMResult {
                error: format!("Failed to run ffmpeg: {e}"),
                ..Default::default()
            };
        }
    };

    let mut stdout = child.stdout.take().unwrap();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(LEQ_BLOCK);
    let mut total_energy = 0.0f64;
    let mut total_samples = 0u64;
    let mut block: Vec<f32> = Vec::with_capacity(LEQ_BLOCK);
    let mut byte_buf = [0u8; LEQ_BLOCK * 4];
    let mut carry: Vec<u8> = Vec::new();

    loop {
        let n = match stdout.read(&mut byte_buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = child.wait();
                return LeqMResult {
                    error: format!("Failed to read ffmpeg output: {e}"),
                    ..Default::default()
                };
            }
        };
        carry.extend_from_slice(&byte_buf[..n]);
        let whole = carry.len() / 4 * 4;
        for chunk in carry[..whole].chunks_exact(4) {
            block.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            if block.len() == LEQ_BLOCK {
                total_energy += weighted_block_energy(fft.as_ref(), &block, SR);
                total_samples += block.len() as u64;
                block.clear();
            }
        }
        carry.drain(..whole);
    }
    if !block.is_empty() {
        total_energy += weighted_block_energy(fft.as_ref(), &block, SR);
        total_samples += block.len() as u64;
    }
    let _ = child.wait();

    if total_samples == 0 {
        return LeqMResult {
            error: "ffmpeg decoded no audio samples".into(),
            ..Default::default()
        };
    }

    let mean_square = total_energy / total_samples as f64;
    LeqMResult {
        leq_m_db: 10.0 * mean_square.log10() + LEQ_M_REFERENCE_OFFSET_DB,
        success: true,
        error: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn parses_max_short_term_from_ebur128() {
        let stderr = "\
[Parsed_ebur128_0 @ 0x1] t: 1.0 M: -30.0 S: -25.0 I: -24.0 LUFS LRA: 2.0 LU
[Parsed_ebur128_0 @ 0x1] t: 2.0 M: -20.0 S: -18.5 I: -23.0 LUFS LRA: 3.0 LU
[Parsed_ebur128_0 @ 0x1] t: 3.0 M: -40.0 S: -inf I: -23.0 LUFS LRA: 3.0 LU
";
        let v = parse_short_term_max(stderr).unwrap();
        assert!((v - (-18.5)).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn short_term_none_without_measurements() {
        assert!(parse_short_term_max("no ebur128 lines here").is_none());
    }

    #[test]
    fn ccir468_is_unity_at_1khz() {
        // the hardcoded 1 kHz normalization constant matches the raw curve
        assert!((ccir468_raw(1000.0) - CCIR468_RAW_AT_1KHZ).abs() < 1e-9);
        // the curve is normalized to 0 dB (linear 1.0) at 1 kHz
        assert!(
            (ccir468_gain(1000.0) - 1.0).abs() < 1e-3,
            "{}",
            ccir468_gain(1000.0)
        );
        // and peaks near +12.2 dB around 6.3 kHz (defining point of ITU-R 468)
        let db_63k = 20.0 * ccir468_gain(6300.0).log10();
        assert!((db_63k - 12.2).abs() < 0.5, "6.3 kHz gain was {db_63k} dB");
    }

    // A full-scale (amplitude 1.0) sine at 1 kHz has RMS 1/sqrt(2), i.e. mean
    // square 0.5, so 10*log10(0.5) = -3.01 dBFS. CCIR 468 weighting is 0 dB at
    // 1 kHz, so the weighted level is unchanged. With the SMPTE B-chain reference
    // (-20 dBFS RMS = 85 dB SPL, i.e. +105 dB offset), Leq(m) = 105 - 3.01 =
    // 101.99 dB.
    #[test]
    fn full_scale_1khz_sine_matches_derived_leq_m() {
        let sr = 48000u32;
        let n = sr as usize; // 1 second
        let samples: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect();
        let leq = leq_m_from_samples(&samples, sr);
        assert!(
            (leq - 101.99).abs() < 0.3,
            "Leq(m) was {leq}, expected ~101.99 dB"
        );
    }

    // Halving the amplitude drops the level by 6.02 dB.
    #[test]
    fn half_amplitude_sine_is_6db_lower() {
        let sr = 48000u32;
        let n = sr as usize;
        let full: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect();
        let half: Vec<f32> = full.iter().map(|s| s * 0.5).collect();
        let d = leq_m_from_samples(&full, sr) - leq_m_from_samples(&half, sr);
        assert!(
            (d - 6.02).abs() < 0.1,
            "difference was {d} dB, expected ~6.02"
        );
    }
}
