// Equal-power crossfade join of two PCM sources (dom#374). Over the caller-given
// overlap the two sources are mixed with cos/sin gains (ga^2 + gb^2 = 1), so the
// summed power stays constant across the join. Fails loud on any format mismatch
// or an overlap longer than a source.

use crate::wav_io::{read_interleaved, write_interleaved};
use std::f32::consts::PI;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum CrossfadeError {
    #[error("channels must be non-zero")]
    ZeroChannels,
    #[error("{side} buffer has {len} samples, not a whole number of {channels}-channel frames")]
    RaggedBuffer {
        side: &'static str,
        len: usize,
        channels: usize,
    },
    #[error(
        "overlap of {overlap_frames} frames exceeds a source (a={a_frames}, b={b_frames} frames)"
    )]
    OverlapTooLong {
        overlap_frames: usize,
        a_frames: usize,
        b_frames: usize,
    },
    #[error("format mismatch: {0}")]
    Mismatch(String),
    #[error("wav i/o: {0}")]
    Wav(#[from] hound::Error),
}

/// Join interleaved `a` and `b` with an equal-power crossfade of
/// `overlap_frames` frames. Output length is a_frames + b_frames - overlap.
pub fn crossfade_join(
    a: &[f32],
    b: &[f32],
    channels: usize,
    overlap_frames: usize,
) -> Result<Vec<f32>, CrossfadeError> {
    if channels == 0 {
        return Err(CrossfadeError::ZeroChannels);
    }
    if !a.len().is_multiple_of(channels) {
        return Err(CrossfadeError::RaggedBuffer {
            side: "a",
            len: a.len(),
            channels,
        });
    }
    if !b.len().is_multiple_of(channels) {
        return Err(CrossfadeError::RaggedBuffer {
            side: "b",
            len: b.len(),
            channels,
        });
    }
    let a_frames = a.len() / channels;
    let b_frames = b.len() / channels;
    if overlap_frames > a_frames || overlap_frames > b_frames {
        return Err(CrossfadeError::OverlapTooLong {
            overlap_frames,
            a_frames,
            b_frames,
        });
    }

    let a_head = a_frames - overlap_frames;
    let out_frames = a_frames + b_frames - overlap_frames;
    let mut out = vec![0.0f32; out_frames * channels];

    // a's head plays untouched up to the overlap.
    out[..a_head * channels].copy_from_slice(&a[..a_head * channels]);

    // crossfade region: a fades out, b fades in, quarter-turn cos/sin gains.
    for f in 0..overlap_frames {
        let t = if overlap_frames > 1 {
            f as f32 / (overlap_frames - 1) as f32
        } else {
            1.0
        };
        let ga = (t * PI / 2.0).cos();
        let gb = (t * PI / 2.0).sin();
        for c in 0..channels {
            let av = a[(a_head + f) * channels + c];
            let bv = b[f * channels + c];
            out[(a_head + f) * channels + c] = av * ga + bv * gb;
        }
    }

    // b's tail after the overlap plays untouched.
    let dst = (a_head + overlap_frames) * channels;
    out[dst..].copy_from_slice(&b[overlap_frames * channels..]);
    Ok(out)
}

/// Crossfade-join two WAVs into `output`. Both must share sample rate, bit
/// depth, sample format and channel count. `overlap_seconds` sets the fade
/// length; it must not exceed either file.
pub fn crossfade_join_wav(
    a: &Path,
    b: &Path,
    output: &Path,
    overlap_seconds: f64,
) -> Result<(), CrossfadeError> {
    let (spec_a, sa) = read_interleaved(a)?;
    let (spec_b, sb) = read_interleaved(b)?;
    if spec_a.sample_rate != spec_b.sample_rate {
        return Err(CrossfadeError::Mismatch(format!(
            "sample rate {} vs {}",
            spec_a.sample_rate, spec_b.sample_rate
        )));
    }
    if spec_a.bits_per_sample != spec_b.bits_per_sample {
        return Err(CrossfadeError::Mismatch(format!(
            "bit depth {} vs {}",
            spec_a.bits_per_sample, spec_b.bits_per_sample
        )));
    }
    if spec_a.sample_format != spec_b.sample_format {
        return Err(CrossfadeError::Mismatch(format!(
            "sample format {:?} vs {:?}",
            spec_a.sample_format, spec_b.sample_format
        )));
    }
    if spec_a.channels != spec_b.channels {
        return Err(CrossfadeError::Mismatch(format!(
            "channel count {} vs {}",
            spec_a.channels, spec_b.channels
        )));
    }

    let overlap_frames = (overlap_seconds * spec_a.sample_rate as f64).round() as usize;
    let out = crossfade_join(&sa, &sb, spec_a.channels as usize, overlap_frames)?;
    write_interleaved(output, spec_a, &out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};

    #[test]
    fn gains_are_equal_power() {
        // reconstruct ga and gb per frame by fading a ones-signal against
        // silence, then assert ga^2 + gb^2 == 1 across the whole overlap.
        let n = 480;
        let ones = vec![1.0f32; n];
        let zeros = vec![0.0f32; n];
        let ga = crossfade_join(&ones, &zeros, 1, n).unwrap();
        let gb = crossfade_join(&zeros, &ones, 1, n).unwrap();
        for i in 0..n {
            assert!(
                (ga[i] * ga[i] + gb[i] * gb[i] - 1.0).abs() < 1e-5,
                "frame {i}: ga={} gb={}",
                ga[i],
                gb[i]
            );
        }
        // endpoints: pure a at the start, pure b at the end.
        assert!((ga[0] - 1.0).abs() < 1e-6 && gb[0].abs() < 1e-6);
        assert!(ga[n - 1].abs() < 1e-6 && (gb[n - 1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn join_is_continuous_at_both_seams() {
        // a and b are the same constant, so a correct join has no jump anywhere.
        let a = vec![1.0f32; 1000];
        let b = vec![1.0f32; 1000];
        let overlap = 480;
        let out = crossfade_join(&a, &b, 1, overlap).unwrap();
        assert_eq!(out.len(), 1000 + 1000 - overlap);
        let max_step = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        // the mid-crossfade equal-power bump is gradual; no sample-to-sample jump.
        assert!(max_step < 0.01, "max step {max_step}");
    }

    #[test]
    fn head_and_tail_are_untouched() {
        let a: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let b: Vec<f32> = (0..100).map(|i| -(i as f32)).collect();
        let overlap = 20;
        let out = crossfade_join(&a, &b, 1, overlap).unwrap();
        // a's first 80 frames verbatim.
        assert_eq!(&out[..80], &a[..80]);
        // b's frames after the overlap verbatim, at the end of the output.
        assert_eq!(&out[out.len() - 80..], &b[20..]);
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(matches!(
            crossfade_join(&[0.0; 4], &[0.0; 4], 0, 1),
            Err(CrossfadeError::ZeroChannels)
        ));
        assert!(matches!(
            crossfade_join(&[0.0; 5], &[0.0; 4], 2, 1),
            Err(CrossfadeError::RaggedBuffer { side: "a", .. })
        ));
        assert!(matches!(
            crossfade_join(&[0.0; 4], &[0.0; 5], 2, 1),
            Err(CrossfadeError::RaggedBuffer { side: "b", .. })
        ));
        assert!(matches!(
            crossfade_join(&[0.0; 10], &[0.0; 4], 1, 6),
            Err(CrossfadeError::OverlapTooLong {
                overlap_frames: 6,
                ..
            })
        ));
    }

    fn write_wav(path: &Path, spec: WavSpec, frames: usize) {
        let mut w = WavWriter::create(path, spec).unwrap();
        for _ in 0..frames {
            for _ in 0..spec.channels {
                match spec.sample_format {
                    SampleFormat::Int => w.write_sample(1000i32).unwrap(),
                    SampleFormat::Float => w.write_sample(0.1f32).unwrap(),
                }
            }
        }
        w.finalize().unwrap();
    }

    fn int_spec(channels: u16, rate: u32, bits: u16) -> WavSpec {
        WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: bits,
            sample_format: SampleFormat::Int,
        }
    }

    #[test]
    fn wav_join_writes_expected_length() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        let out = dir.path().join("out.wav");
        let spec = int_spec(2, 48000, 24);
        write_wav(&a, spec, 48000);
        write_wav(&b, spec, 48000);

        crossfade_join_wav(&a, &b, &out, 0.5).unwrap();
        let reader = hound::WavReader::open(&out).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().bits_per_sample, 24);
        // 48000 + 48000 - 24000 overlap frames.
        assert_eq!(reader.duration(), 72000);
    }

    #[test]
    fn wav_rejects_every_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.wav");
        write_wav(&base, int_spec(2, 48000, 24), 48000);
        let out = dir.path().join("o.wav");

        let cases = [
            ("rate", int_spec(2, 44100, 24)),
            ("bits", int_spec(2, 48000, 16)),
            ("channels", int_spec(1, 48000, 24)),
        ];
        for (name, spec) in cases {
            let other = dir.path().join(format!("{name}.wav"));
            write_wav(&other, spec, 48000);
            assert!(
                matches!(
                    crossfade_join_wav(&base, &other, &out, 0.1),
                    Err(CrossfadeError::Mismatch(_))
                ),
                "{name} mismatch not rejected"
            );
        }

        // float format (hound floats are 32-bit) vs the int base is rejected.
        let fspec = WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let fpath = dir.path().join("f.wav");
        write_wav(&fpath, fspec, 48000);
        assert!(matches!(
            crossfade_join_wav(&base, &fpath, &out, 0.1),
            Err(CrossfadeError::Mismatch(_))
        ));
    }

    #[test]
    fn wav_rejects_overlap_longer_than_source() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        let out = dir.path().join("o.wav");
        let spec = int_spec(1, 48000, 16);
        write_wav(&a, spec, 24000); // 0.5 s
        write_wav(&b, spec, 24000);
        assert!(matches!(
            crossfade_join_wav(&a, &b, &out, 1.0), // 1 s overlap on 0.5 s files
            Err(CrossfadeError::OverlapTooLong { .. })
        ));
    }
}
