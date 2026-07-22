use ebur128::{EbuR128, Mode};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
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

// Loudness adjustment (dom#1382): pure sample-domain gain to hit a target.

/// Which loudness quantity a gain adjustment targets.
#[derive(Debug, Clone, Copy)]
pub enum LoudnessTarget {
    /// EBU R128 integrated loudness, in LUFS (e.g. -20.0 for theatrical).
    IntegratedLufs(f64),
    /// ISO 21727 Leq(m), in dB.
    LeqM(f64),
}

/// Default true-peak ceiling, dBTP.
pub const DEFAULT_TRUE_PEAK_CEILING_DBTP: f64 = -1.0;

/// The numbers behind moving a measured level to a target with a linear gain,
/// and the resulting true peak (all before any file is written).
#[derive(Debug, Clone, Copy)]
pub struct GainPlan {
    pub measured_db: f64,
    pub target_db: f64,
    pub gain_db: f64,
    pub input_true_peak_dbtp: f64,
    pub resulting_true_peak_dbtp: f64,
    pub true_peak_ceiling_dbtp: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum AdjustError {
    #[error("wav i/o: {0}")]
    Wav(#[from] hound::Error),
    #[error("ebur128: {0}")]
    Ebur128(#[from] ebur128::Error),
    #[error("no audio samples to measure")]
    Empty,
    #[error("measured level is {0} dB; cannot compute a finite gain")]
    NonFiniteMeasurement(f64),
    #[error(
        "true-peak ceiling exceeded: a gain of {gain_db:.2} dB would raise true peak from \
         {input_true_peak_dbtp:.2} to {resulting_true_peak_dbtp:.2} dBTP, above the \
         {true_peak_ceiling_dbtp:.2} dBTP ceiling (only {headroom_db:.2} dB headroom); \
         pass a lower target"
    )]
    TruePeakExceeded {
        gain_db: f64,
        input_true_peak_dbtp: f64,
        resulting_true_peak_dbtp: f64,
        true_peak_ceiling_dbtp: f64,
        headroom_db: f64,
    },
}

// decoded PCM in its native form. gain is applied by scaling the raw values, so
// int stays int at the same bit depth and float stays float.
enum Pcm {
    Int(Vec<i32>),
    Float(Vec<f32>),
}

impl Pcm {
    fn frame_count(&self, channels: usize) -> usize {
        let len = match self {
            Pcm::Int(s) => s.len(),
            Pcm::Float(s) => s.len(),
        };
        len / channels.max(1)
    }

    // interleaved, normalized to full-scale (-1.0..=1.0), for loudness measurement.
    fn normalized(&self, bits_per_sample: u16) -> Vec<f32> {
        match self {
            Pcm::Int(s) => {
                let fs = (1i64 << (bits_per_sample - 1)) as f32;
                s.iter().map(|&v| v as f32 / fs).collect()
            }
            Pcm::Float(s) => s.clone(),
        }
    }
}

fn load_pcm(input: &Path) -> Result<(WavSpec, Pcm), AdjustError> {
    let reader = WavReader::open(input)?;
    let spec = reader.spec();
    let pcm = match spec.sample_format {
        SampleFormat::Int => Pcm::Int(reader.into_samples::<i32>().collect::<Result<_, _>>()?),
        SampleFormat::Float => Pcm::Float(reader.into_samples::<f32>().collect::<Result<_, _>>()?),
    };
    Ok((spec, pcm))
}

// mono downmix (mean of channels) of an interleaved buffer.
fn downmix_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

// measured level (per the target's metric) and the input true peak in dBTP.
fn measure(spec: &WavSpec, pcm: &Pcm, target: LoudnessTarget) -> Result<(f64, f64), AdjustError> {
    let channels = spec.channels as usize;
    if channels == 0 || pcm.frame_count(channels) == 0 {
        return Err(AdjustError::Empty);
    }
    let interleaved = pcm.normalized(spec.bits_per_sample);

    let mut meter = EbuR128::new(
        spec.channels as u32,
        spec.sample_rate,
        Mode::I | Mode::TRUE_PEAK,
    )?;
    meter.add_frames_f32(&interleaved)?;
    let mut peak_linear = 0.0f64;
    for ch in 0..spec.channels as u32 {
        peak_linear = peak_linear.max(meter.true_peak(ch)?);
    }
    let true_peak_dbtp = 20.0 * peak_linear.log10();

    let measured = match target {
        LoudnessTarget::IntegratedLufs(_) => meter.loudness_global()?,
        LoudnessTarget::LeqM(_) => {
            let mono = downmix_mono(&interleaved, channels);
            leq_m_from_samples(&mono, spec.sample_rate)
        }
    };
    Ok((measured, true_peak_dbtp))
}

/// Compute the gain needed to move `input` to `target` and check it against the
/// true-peak ceiling. Returns the plan, or fails loud (no writing, no limiting).
pub fn plan_gain(
    input: &Path,
    target: LoudnessTarget,
    true_peak_ceiling_dbtp: f64,
) -> Result<GainPlan, AdjustError> {
    let (spec, pcm) = load_pcm(input)?;
    let (measured_db, input_true_peak_dbtp) = measure(&spec, &pcm, target)?;
    if !measured_db.is_finite() {
        return Err(AdjustError::NonFiniteMeasurement(measured_db));
    }
    let target_db = match target {
        LoudnessTarget::IntegratedLufs(t) | LoudnessTarget::LeqM(t) => t,
    };
    let gain_db = target_db - measured_db;
    // a linear gain shifts true peak by exactly the same dB.
    let resulting_true_peak_dbtp = input_true_peak_dbtp + gain_db;
    if resulting_true_peak_dbtp > true_peak_ceiling_dbtp {
        return Err(AdjustError::TruePeakExceeded {
            gain_db,
            input_true_peak_dbtp,
            resulting_true_peak_dbtp,
            true_peak_ceiling_dbtp,
            headroom_db: true_peak_ceiling_dbtp - input_true_peak_dbtp,
        });
    }
    Ok(GainPlan {
        measured_db,
        target_db,
        gain_db,
        input_true_peak_dbtp,
        resulting_true_peak_dbtp,
        true_peak_ceiling_dbtp,
    })
}

/// Apply a pure sample-domain gain to a WAV, preserving format, bit depth,
/// channel count and sample rate. No resampling, no re-encode, no limiting.
pub fn apply_gain(input: &Path, output: &Path, gain_db: f64) -> Result<(), AdjustError> {
    let (spec, pcm) = load_pcm(input)?;
    let scale = 10f64.powf(gain_db / 20.0);
    let mut writer = WavWriter::create(output, spec)?;
    match pcm {
        Pcm::Int(samples) => {
            for s in samples {
                // scale the raw integer directly, so the bit depth is unchanged.
                writer.write_sample((s as f64 * scale).round() as i32)?;
            }
        }
        Pcm::Float(samples) => {
            let g = scale as f32;
            for s in samples {
                writer.write_sample(s * g)?;
            }
        }
    }
    writer.finalize()?;
    Ok(())
}

/// Measure `input`, compute the gain to hit `target`, guard the true-peak
/// ceiling, then write the adjusted WAV to `output`. Fails loud (writing
/// nothing) if the gain would breach the ceiling.
pub fn adjust_loudness(
    input: &Path,
    output: &Path,
    target: LoudnessTarget,
    true_peak_ceiling_dbtp: f64,
) -> Result<GainPlan, AdjustError> {
    let plan = plan_gain(input, target, true_peak_ceiling_dbtp)?;
    apply_gain(input, output, plan.gain_db)?;
    Ok(plan)
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

    // write a `seconds`-long `freq` Hz tone at `amplitude` (0..1 full-scale) to a
    // wav with `spec` on every channel.
    fn write_tone(path: &Path, spec: WavSpec, freq: f32, amplitude: f32, seconds: f32) {
        let sr = spec.sample_rate;
        let frames = (sr as f32 * seconds) as usize;
        let mut w = WavWriter::create(path, spec).unwrap();
        let full_scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
        for i in 0..frames {
            let s = amplitude * (2.0 * PI * freq * i as f32 / sr as f32).sin();
            for _ in 0..spec.channels {
                match spec.sample_format {
                    SampleFormat::Int => w.write_sample((s * full_scale).round() as i32).unwrap(),
                    SampleFormat::Float => w.write_sample(s).unwrap(),
                }
            }
        }
        w.finalize().unwrap();
    }

    fn int_spec(channels: u16, bits: u16) -> WavSpec {
        WavSpec {
            channels,
            sample_rate: 48000,
            bits_per_sample: bits,
            sample_format: SampleFormat::Int,
        }
    }

    #[test]
    fn adjust_hits_integrated_lufs_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        write_tone(&src, int_spec(2, 16), 1000.0, 0.5, 3.0);

        let target = LoudnessTarget::IntegratedLufs(-23.0);
        let plan = adjust_loudness(&src, &dst, target, DEFAULT_TRUE_PEAK_CEILING_DBTP).unwrap();
        // re-measure the written file; its measured level must now be the target.
        let (remeasured, _) =
            measure(&int_spec(2, 16), &load_pcm(&dst).unwrap().1, target).unwrap();
        assert!(
            (remeasured - (-23.0)).abs() < 0.3,
            "re-measured {remeasured} LUFS, target -23; plan {plan:?}"
        );
    }

    #[test]
    fn adjust_hits_leq_m_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        write_tone(&src, int_spec(1, 16), 1000.0, 0.5, 2.0);

        let target = LoudnessTarget::LeqM(85.0);
        adjust_loudness(&src, &dst, target, DEFAULT_TRUE_PEAK_CEILING_DBTP).unwrap();
        let (remeasured, _) =
            measure(&int_spec(1, 16), &load_pcm(&dst).unwrap().1, target).unwrap();
        assert!(
            (remeasured - 85.0).abs() < 0.15,
            "re-measured {remeasured} dB Leq(m), target 85"
        );
    }

    #[test]
    fn headroom_exceeded_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        write_tone(&src, int_spec(1, 16), 1000.0, 0.5, 2.0);

        // a 120 dB Leq(m) target needs ~+24 dB, pushing the ~-6 dBTP peak well
        // over the -1 dBTP ceiling.
        let err = adjust_loudness(&src, &dst, LoudnessTarget::LeqM(120.0), -1.0).unwrap_err();
        match err {
            AdjustError::TruePeakExceeded {
                gain_db,
                input_true_peak_dbtp,
                resulting_true_peak_dbtp,
                true_peak_ceiling_dbtp,
                headroom_db,
            } => {
                assert!(gain_db > 0.0, "expected positive gain, got {gain_db}");
                assert!((true_peak_ceiling_dbtp - (-1.0)).abs() < 1e-9);
                assert!((resulting_true_peak_dbtp - (input_true_peak_dbtp + gain_db)).abs() < 1e-9);
                assert!(
                    (headroom_db - (true_peak_ceiling_dbtp - input_true_peak_dbtp)).abs() < 1e-9
                );
                assert!(gain_db > headroom_db, "gain must exceed the headroom");
            }
            other => panic!("expected TruePeakExceeded, got {other:?}"),
        }
        // nothing was written.
        assert!(!dst.exists());
    }

    #[test]
    fn bit_depth_and_channels_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        let spec = int_spec(2, 24);
        write_tone(&src, spec, 1000.0, 0.5, 1.0);

        adjust_loudness(
            &src,
            &dst,
            LoudnessTarget::LeqM(80.0),
            DEFAULT_TRUE_PEAK_CEILING_DBTP,
        )
        .unwrap();

        let out = WavReader::open(&dst).unwrap();
        let out_spec = out.spec();
        assert_eq!(out_spec.channels, 2);
        assert_eq!(out_spec.bits_per_sample, 24);
        assert_eq!(out_spec.sample_format, SampleFormat::Int);
        assert_eq!(out.duration(), WavReader::open(&src).unwrap().duration());
    }
}
