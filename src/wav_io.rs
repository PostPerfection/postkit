// wav <-> normalized interleaved f32, preserving sample format and bit depth.
// shared by the upmix and crossfade DSP modules; loudness keeps its own copy.

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::path::Path;

/// Read a WAV into (spec, interleaved f32 in -1.0..=1.0). Int is scaled by
/// 2^(bits-1); float passes through.
pub(crate) fn read_interleaved(path: &Path) -> Result<(WavSpec, Vec<f32>), hound::Error> {
    let reader = WavReader::open(path)?;
    let spec = reader.spec();
    let samples = match spec.sample_format {
        SampleFormat::Int => {
            let fs = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|s| s.map(|v| v as f32 / fs))
                .collect::<Result<_, _>>()?
        }
        SampleFormat::Float => reader.into_samples::<f32>().collect::<Result<_, _>>()?,
    };
    Ok((spec, samples))
}

/// Write interleaved f32 back to WAV in `spec`'s format. Int is scaled by
/// 2^(bits-1) and clamped: dsp can overshoot full scale, and wrapping a
/// narrower int would flip sign.
pub(crate) fn write_interleaved(
    path: &Path,
    spec: WavSpec,
    samples: &[f32],
) -> Result<(), hound::Error> {
    let mut w = WavWriter::create(path, spec)?;
    match spec.sample_format {
        SampleFormat::Int => {
            let fs = (1i64 << (spec.bits_per_sample - 1)) as f64;
            let max = (fs as i64) - 1;
            let min = -(fs as i64);
            for &v in samples {
                let x = (v as f64 * fs).round() as i64;
                w.write_sample(x.clamp(min, max) as i32)?;
            }
        }
        SampleFormat::Float => {
            for &v in samples {
                w.write_sample(v)?;
            }
        }
    }
    w.finalize()?;
    Ok(())
}
