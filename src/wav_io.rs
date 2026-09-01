// wav <-> interleaved samples, preserving sample format and bit depth. The
// exact pair keeps the file's own sample type, so a read/write round-trip is
// byte-identical at any depth hound accepts; the f32 pair normalises for the
// DSP modules (upmix, crossfade, mid-side) and loses the low bits of 32-bit
// int, whose 32 significant bits do not fit an f32 mantissa. loudness keeps its
// own copy.

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::path::Path;

/// Interleaved samples in the file's own sample type.
#[derive(Debug, Clone, PartialEq)]
pub enum Samples {
    Int(Vec<i32>),
    Float(Vec<f32>),
}

impl Samples {
    pub fn len(&self) -> usize {
        match self {
            Samples::Int(v) => v.len(),
            Samples::Float(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How many channels a WAV carries, without reading its samples.
pub fn channel_count(path: &Path) -> Result<usize, String> {
    let reader = WavReader::open(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(reader.spec().channels as usize)
}

/// Read a WAV into (spec, interleaved samples of the file's own type). Every
/// sample survives, so writing the result back reproduces the file byte for
/// byte. Use this for anything that moves samples around rather than doing
/// arithmetic on them.
pub fn read_interleaved_exact(path: &Path) -> Result<(WavSpec, Samples), hound::Error> {
    let reader = WavReader::open(path)?;
    let spec = reader.spec();
    let samples = match spec.sample_format {
        SampleFormat::Int => Samples::Int(reader.into_samples::<i32>().collect::<Result<_, _>>()?),
        SampleFormat::Float => {
            Samples::Float(reader.into_samples::<f32>().collect::<Result<_, _>>()?)
        }
    };
    Ok((spec, samples))
}

/// Write interleaved samples of the file's own type. `samples` must match
/// `spec`'s sample format; hound rejects the mismatch.
pub fn write_interleaved_exact(
    path: &Path,
    spec: WavSpec,
    samples: &Samples,
) -> Result<(), hound::Error> {
    let mut w = WavWriter::create(path, spec)?;
    match samples {
        Samples::Int(v) => {
            for &s in v {
                w.write_sample(s)?;
            }
        }
        Samples::Float(v) => {
            for &s in v {
                w.write_sample(s)?;
            }
        }
    }
    w.finalize()?;
    Ok(())
}

/// Read a WAV into (spec, interleaved f32 in -1.0..=1.0). Int is scaled by
/// 2^(bits-1); float passes through. 32-bit int loses its low bits here, so
/// anything that must stay bit-exact wants `read_interleaved_exact`.
pub fn read_interleaved(path: &Path) -> Result<(WavSpec, Vec<f32>), hound::Error> {
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
pub fn write_interleaved(path: &Path, spec: WavSpec, samples: &[f32]) -> Result<(), hound::Error> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn spec(bits: u16, format: SampleFormat) -> WavSpec {
        WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: bits,
            sample_format: format,
        }
    }

    /// Values that only survive if every bit of the sample does: the extremes,
    /// the ends of the range and a pattern with bits set right down to the LSB.
    fn awkward_ints(bits: u16) -> Vec<i32> {
        let top = 1i64 << (bits - 1);
        let mut v = vec![0, 1, -1, top - 1, -top, top / 3, -(top / 3) - 1];
        for shift in 0..bits {
            v.push((top - 1) >> shift);
            v.push(-top + (1 << shift));
        }
        if !v.len().is_multiple_of(2) {
            v.push(0);
        }
        v.into_iter().map(|s| s as i32).collect()
    }

    #[test]
    fn exact_round_trip_is_byte_identical_for_every_format() {
        let dir = TempDir::new().unwrap();
        let cases = [
            (spec(8, SampleFormat::Int), Samples::Int(awkward_ints(8))),
            (spec(16, SampleFormat::Int), Samples::Int(awkward_ints(16))),
            (spec(24, SampleFormat::Int), Samples::Int(awkward_ints(24))),
            (spec(32, SampleFormat::Int), Samples::Int(awkward_ints(32))),
            (
                spec(32, SampleFormat::Float),
                Samples::Float(vec![0.0, 1.0, -1.0, 1e-9, -0.333_333_34, 0.999_999_94]),
            ),
        ];
        for (spec, samples) in cases {
            let first = dir.path().join(format!("{}.wav", spec.bits_per_sample));
            write_interleaved_exact(&first, spec, &samples).unwrap();
            let (read_spec, read_samples) = read_interleaved_exact(&first).unwrap();
            assert_eq!(read_spec, spec);
            assert_eq!(
                read_samples, samples,
                "{:?} {} bit lost samples",
                spec.sample_format, spec.bits_per_sample
            );

            let second = dir
                .path()
                .join(format!("{}-again.wav", spec.bits_per_sample));
            write_interleaved_exact(&second, read_spec, &read_samples).unwrap();
            assert_eq!(
                std::fs::read(&first).unwrap(),
                std::fs::read(&second).unwrap(),
                "{:?} {} bit round-trip changed the file",
                spec.sample_format,
                spec.bits_per_sample
            );
        }
    }

    #[test]
    fn prepending_silence_keeps_the_rest_bit_exact() {
        let dir = TempDir::new().unwrap();
        for bits in [16, 24, 32] {
            let original = awkward_ints(bits);
            let source = dir.path().join(format!("source{bits}.wav"));
            let spec = spec(bits, SampleFormat::Int);
            write_interleaved_exact(&source, spec, &Samples::Int(original.clone())).unwrap();

            let (spec, samples) = read_interleaved_exact(&source).unwrap();
            let Samples::Int(samples) = samples else {
                panic!("int wav read back as float");
            };
            let mut delayed = vec![0i32; 96];
            delayed.extend_from_slice(&samples);
            let shifted = dir.path().join(format!("shifted{bits}.wav"));
            write_interleaved_exact(&shifted, spec, &Samples::Int(delayed)).unwrap();

            let (_, back) = read_interleaved_exact(&shifted).unwrap();
            let Samples::Int(back) = back else {
                panic!("int wav read back as float");
            };
            assert!(
                back[..96].iter().all(|&s| s == 0),
                "{bits} bit lost silence"
            );
            assert_eq!(&back[96..], &original[..], "{bits} bit lost samples");
        }
    }

    #[test]
    fn the_normalised_pair_holds_up_to_24_bit() {
        let dir = TempDir::new().unwrap();
        for bits in [16, 24] {
            let original = awkward_ints(bits);
            let path = dir.path().join(format!("norm{bits}.wav"));
            let spec = spec(bits, SampleFormat::Int);
            write_interleaved_exact(&path, spec, &Samples::Int(original.clone())).unwrap();

            let (spec, samples) = read_interleaved(&path).unwrap();
            let again = dir.path().join(format!("norm{bits}-again.wav"));
            write_interleaved(&again, spec, &samples).unwrap();

            let (_, back) = read_interleaved_exact(&again).unwrap();
            assert_eq!(back, Samples::Int(original), "{bits} bit lost samples");
        }
    }

    #[test]
    fn writing_the_wrong_sample_type_fails_loud() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mismatch.wav");
        let error = write_interleaved_exact(
            &path,
            spec(24, SampleFormat::Int),
            &Samples::Float(vec![0.0, 0.0]),
        );
        assert!(error.is_err(), "float samples went into an int wav");
    }
}
