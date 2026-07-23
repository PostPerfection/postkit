// Stereo -> 5.1 upmix (dom#921, dom#1080), mirroring dcpomatic's two upmixers
// (src/lib/upmixer_a.cc, upmixer_b.cc) and their windowed-sinc FIR filters
// (src/lib/audio_filter.cc). Output is interleaved 6-channel DCP order
// L,R,C,LFE,Ls,Rs. The FIR filters are linear-phase (blackman-windowed sinc),
// so each adds M/2 samples of group delay exactly as DoM does; channels are not
// re-time-aligned, matching upstream.

use crate::wav_io::{read_interleaved, write_interleaved};
use hound::WavSpec;
use std::f32::consts::PI;
use std::path::Path;

// 10^(-6/20): DoM mixes L+R then apply_gain(-6) dB (amplitude), i.e. -3 dB power.
const MINUS_6DB: f32 = 0.501_187_2;

#[derive(Debug, thiserror::Error)]
pub enum UpmixError {
    #[error("input is not whole stereo frames: {len} samples for 2 channels")]
    RaggedStereo { len: usize },
    #[error("sample rate must be non-zero")]
    ZeroSampleRate,
    #[error("wav input must be 2-channel stereo, got {channels}")]
    NotStereoWav { channels: u16 },
    #[error("wav i/o: {0}")]
    Wav(#[from] hound::Error),
}

/// Which DCP-o-matic upmixer variant to use.
#[derive(Debug, Clone, Copy)]
pub enum Upmixer {
    /// Band-split: L/R keep the highs, C the mids, LFE the lows, Ls/Rs the top.
    A,
    /// Passthrough L/R, summed C, lowpassed LFE, delayed L-R surrounds.
    B,
}

// FIR impulse-response design (audio_filter.cc). cutoffs are normalized (hz/sr).

// _M = 4 / transition_bandwidth, rounded up to even (audio_filter.h).
fn filter_len(transition_bandwidth: f32) -> usize {
    let mut m = (4.0 / transition_bandwidth) as usize;
    if m % 2 == 1 {
        m += 1;
    }
    m
}

// windowed-sinc of length _M+1; `invert` spectrally inverts (lowpass->highpass).
fn sinc_blackman(m: usize, cutoff: f32, invert: bool) -> Vec<f32> {
    let mid = m / 2;
    let mut ir = vec![0.0f32; m + 1];
    for (i, v) in ir.iter_mut().enumerate() {
        if i == mid {
            *v = 2.0 * PI * cutoff;
        } else {
            let x = i as f32 - mid as f32;
            let window = 0.42 - 0.5 * (2.0 * PI * i as f32 / m as f32).cos()
                + 0.08 * (4.0 * PI * i as f32 / m as f32).cos();
            *v = (2.0 * PI * cutoff * x).sin() / x * window;
        }
    }
    let sum: f32 = ir.iter().sum();
    for v in ir.iter_mut() {
        *v /= sum;
    }
    if invert {
        for v in ir.iter_mut() {
            *v = -*v;
        }
        ir[mid] += 1.0;
    }
    ir
}

// bandpass = -(lowpass(lower) + highpass(higher)) spectrally inverted, i.e. the
// band between the two cutoffs (audio_filter.cc BandPassAudioFilter).
fn bandpass(m: usize, lower: f32, higher: f32) -> Vec<f32> {
    let lpf = sinc_blackman(m, lower, false);
    let hpf = sinc_blackman(m, higher, true);
    let mid = m / 2;
    let mut ir: Vec<f32> = lpf.iter().zip(&hpf).map(|(a, b)| a + b).collect();
    for v in ir.iter_mut() {
        *v = -*v;
    }
    ir[mid] += 1.0;
    ir
}

// causal FIR convolution over the whole buffer, zero initial state (the DoM
// filter tail starts silent).
fn convolve(ir: &[f32], x: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0f32; x.len()];
    for j in 0..x.len() {
        let mut s = 0.0f32;
        for k in 0..=j.min(ir.len() - 1) {
            s += x[j - k] * ir[k];
        }
        out[j] = s;
    }
    out
}

fn split_stereo(x: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut l = Vec::with_capacity(x.len() / 2);
    let mut r = Vec::with_capacity(x.len() / 2);
    for f in x.chunks_exact(2) {
        l.push(f[0]);
        r.push(f[1]);
    }
    (l, r)
}

// (L + R) at -6 dB amplitude, DoM's centre/LFE source.
fn mix_lr(l: &[f32], r: &[f32]) -> Vec<f32> {
    l.iter().zip(r).map(|(&a, &b)| (a + b) * MINUS_6DB).collect()
}

fn interleave6(ch: [&[f32]; 6]) -> Vec<f32> {
    let n = ch[0].len();
    let mut out = vec![0.0f32; n * 6];
    for i in 0..n {
        for (c, lane) in ch.iter().enumerate() {
            out[i * 6 + c] = lane[i];
        }
    }
    out
}

fn upmix_a(l: &[f32], r: &[f32], sample_rate: u32) -> Vec<f32> {
    let sr = sample_rate as f32;
    let lr = mix_lr(l, r);
    let side = bandpass(filter_len(0.02), 1900.0 / sr, 4800.0 / sr);
    let centre = bandpass(filter_len(0.01), 150.0 / sr, 1900.0 / sr);
    let lfe = sinc_blackman(filter_len(0.01), 150.0 / sr, false);
    let surround = bandpass(filter_len(0.02), 4800.0 / sr, 20000.0 / sr);

    let out_l = convolve(&side, l);
    let out_r = convolve(&side, r);
    let out_c = convolve(&centre, &lr);
    let out_lfe = convolve(&lfe, &lr);
    let out_ls = convolve(&surround, l);
    let out_rs = convolve(&surround, r);
    interleave6([&out_l, &out_r, &out_c, &out_lfe, &out_ls, &out_rs])
}

fn upmix_b(l: &[f32], r: &[f32], sample_rate: u32) -> Vec<f32> {
    let sr = sample_rate as f32;
    let lr = mix_lr(l, r);
    let out_lfe = convolve(&sinc_blackman(filter_len(0.01), 150.0 / sr, false), &lr);

    // Ls = (L - R) delayed by 0.02 s (audio_delay.cc); Rs copies Ls.
    let delay = (0.02 * sr) as usize;
    let sub: Vec<f32> = l.iter().zip(r).map(|(&a, &b)| a - b).collect();
    let mut ls = vec![0.0f32; sub.len()];
    ls[delay..].copy_from_slice(&sub[..sub.len() - delay]);
    interleave6([l, r, &lr, &out_lfe, &ls, &ls])
}

/// Upmix interleaved stereo f32 to interleaved 6-channel L,R,C,LFE,Ls,Rs.
pub fn upmix_stereo_to_51(
    variant: Upmixer,
    stereo: &[f32],
    sample_rate: u32,
) -> Result<Vec<f32>, UpmixError> {
    if sample_rate == 0 {
        return Err(UpmixError::ZeroSampleRate);
    }
    if !stereo.len().is_multiple_of(2) {
        return Err(UpmixError::RaggedStereo { len: stereo.len() });
    }
    let (l, r) = split_stereo(stereo);
    Ok(match variant {
        Upmixer::A => upmix_a(&l, &r, sample_rate),
        Upmixer::B => upmix_b(&l, &r, sample_rate),
    })
}

/// Upmix a stereo WAV to a 6-channel WAV, same sample rate and bit depth.
pub fn upmix_wav(variant: Upmixer, input: &Path, output: &Path) -> Result<(), UpmixError> {
    let (spec, samples) = read_interleaved(input)?;
    if spec.channels != 2 {
        return Err(UpmixError::NotStereoWav {
            channels: spec.channels,
        });
    }
    let out = upmix_stereo_to_51(variant, &samples, spec.sample_rate)?;
    write_interleaved(
        output,
        WavSpec {
            channels: 6,
            ..spec
        },
        &out,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavReader, WavSpec};

    const SR: u32 = 48000;

    fn tone(freq: f32, amp: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / SR as f32).sin())
            .collect()
    }

    fn interleave(l: &[f32], r: &[f32]) -> Vec<f32> {
        l.iter().zip(r).flat_map(|(&a, &b)| [a, b]).collect()
    }

    // per-channel samples of a 6-channel interleaved buffer.
    fn channel(buf: &[f32], ch: usize) -> Vec<f32> {
        buf.chunks_exact(6).map(|f| f[ch]).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|&v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn lowpass_has_unity_dc_gain() {
        // a normalized lowpass sums to 1, so a constant passes through at steady
        // state.
        let ir = sinc_blackman(filter_len(0.01), 150.0 / SR as f32, false);
        let out = convolve(&ir, &vec![1.0f32; 2000]);
        assert!((out[1500] - 1.0).abs() < 1e-3, "steady state {}", out[1500]);
    }

    #[test]
    fn variant_a_routes_mid_to_centre() {
        // an 800 Hz tone on both channels lands in C (150-1900 band), not in the
        // high L/R/Ls/Rs bands nor the sub-150 LFE.
        let s = tone(800.0, 0.5, SR as usize);
        let out = upmix_stereo_to_51(Upmixer::A, &interleave(&s, &s), SR).unwrap();
        let c = rms(&channel(&out, 2));
        for (name, ch) in [("L", 0), ("R", 1), ("LFE", 3), ("Ls", 4), ("Rs", 5)] {
            let e = rms(&channel(&out, ch));
            assert!(c > e * 8.0, "centre {c} not dominant over {name} {e}");
        }
    }

    #[test]
    fn variant_a_left_isolated_from_right() {
        // 3000 Hz (in the 1900-4800 L/R band) on L only: L present, R silent, and
        // it does not leak into C (150-1900) or Ls (4800-20000).
        let l = tone(3000.0, 0.5, SR as usize);
        let r = vec![0.0f32; l.len()];
        let out = upmix_stereo_to_51(Upmixer::A, &interleave(&l, &r), SR).unwrap();
        let out_l = rms(&channel(&out, 0));
        assert!(out_l > 0.05, "L should carry the tone, got {out_l}");
        assert!(rms(&channel(&out, 1)) < out_l * 0.01, "R must stay silent");
        assert!(rms(&channel(&out, 2)) < out_l * 0.2, "C must not carry 3 kHz");
        assert!(rms(&channel(&out, 4)) < out_l * 0.2, "Ls must not carry 3 kHz");
    }

    #[test]
    fn variant_a_routes_highs_to_surrounds() {
        // 8000 Hz (in the 4800-20000 Ls/Rs band) dominates the surrounds.
        let s = tone(8000.0, 0.5, SR as usize);
        let out = upmix_stereo_to_51(Upmixer::A, &interleave(&s, &s), SR).unwrap();
        let ls = rms(&channel(&out, 4));
        assert!(ls > rms(&channel(&out, 2)) * 8.0, "Ls not above C");
        assert!(ls > rms(&channel(&out, 3)) * 8.0, "Ls not above LFE");
    }

    #[test]
    fn variant_b_copies_and_sums_exactly() {
        let l = tone(1000.0, 0.4, 4000);
        let r = tone(1500.0, 0.3, 4000);
        let out = upmix_stereo_to_51(Upmixer::B, &interleave(&l, &r), SR).unwrap();
        // L and R are exact passthrough copies.
        assert_eq!(channel(&out, 0), l);
        assert_eq!(channel(&out, 1), r);
        // C = (L + R) * -6 dB, computed the same way.
        let c = channel(&out, 2);
        for i in 0..l.len() {
            assert!((c[i] - (l[i] + r[i]) * MINUS_6DB).abs() < 1e-6);
        }
    }

    #[test]
    fn variant_b_surrounds_are_delayed_difference() {
        let l = tone(1000.0, 0.4, 6000);
        let r = tone(1500.0, 0.3, 6000);
        let out = upmix_stereo_to_51(Upmixer::B, &interleave(&l, &r), SR).unwrap();
        let ls = channel(&out, 4);
        let rs = channel(&out, 5);
        assert_eq!(ls, rs, "Rs must copy Ls");
        let delay = (0.02 * SR as f32) as usize;
        for i in 0..delay {
            assert_eq!(ls[i], 0.0, "surround must be silent during the delay");
        }
        for i in delay..l.len() {
            assert!((ls[i] - (l[i - delay] - r[i - delay])).abs() < 1e-6);
        }
    }

    #[test]
    fn variant_b_lfe_is_lowpassed() {
        // the LFE lowpass passes a 60 Hz tone but rejects a 5 kHz one.
        let frames = SR as usize;
        let low = interleave(&tone(60.0, 0.5, frames), &tone(60.0, 0.5, frames));
        let high = interleave(&tone(5000.0, 0.5, frames), &tone(5000.0, 0.5, frames));
        let lfe_low = rms(&channel(&upmix_stereo_to_51(Upmixer::B, &low, SR).unwrap(), 3));
        let lfe_high = rms(&channel(&upmix_stereo_to_51(Upmixer::B, &high, SR).unwrap(), 3));
        assert!(lfe_low > lfe_high * 20.0, "low {lfe_low} vs high {lfe_high}");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(
            upmix_stereo_to_51(Upmixer::A, &[0.0; 5], SR),
            Err(UpmixError::RaggedStereo { len: 5 })
        ));
        assert!(matches!(
            upmix_stereo_to_51(Upmixer::A, &[0.0; 4], 0),
            Err(UpmixError::ZeroSampleRate)
        ));
    }

    #[test]
    fn wav_roundtrip_writes_six_channels() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: SR,
            bits_per_sample: 24,
            sample_format: SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&src, spec).unwrap();
        let fs = (1i64 << 23) as f32;
        for i in 0..SR as usize {
            let s = (0.3 * (2.0 * PI * 1000.0 * i as f32 / SR as f32).sin() * fs) as i32;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();

        upmix_wav(Upmixer::A, &src, &dst).unwrap();
        let out = WavReader::open(&dst).unwrap();
        assert_eq!(out.spec().channels, 6);
        assert_eq!(out.spec().sample_rate, SR);
        assert_eq!(out.spec().bits_per_sample, 24);
        assert_eq!(out.duration(), SR); // frame count preserved
    }

    #[test]
    fn wav_rejects_non_stereo() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("mono.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: SR,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&src, spec).unwrap();
        for _ in 0..100 {
            w.write_sample(0i32).unwrap();
        }
        w.finalize().unwrap();
        assert!(matches!(
            upmix_wav(Upmixer::A, &src, &dir.path().join("o.wav")),
            Err(UpmixError::NotStereoWav { channels: 1 })
        ));
    }
}
