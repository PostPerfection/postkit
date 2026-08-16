// Audio channel mapping matrix (DoM's audio mapping): any input channel to any
// output channel with a per-cell gain, summing where several inputs land on one
// output. Channels are numbers here, naming them L/R/C/LFE/Ls/Rs/HI/VI is the
// caller's job. The `IN:OUT@GAIN` spec grammar lives here so a CLI flag and a
// GUI matrix widget cannot drift apart.

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::path::{Path, PathBuf};

const DECIBEL_BASE: f64 = 10.0;
const DECIBEL_AMPLITUDE_DIVISOR: f64 = 20.0;
const UNITY_GAIN: f64 = 1.0;
const UNROUTED_GAIN: f64 = 0.0;
const UNITY_GAIN_DECIBELS: f64 = 0.0;
const SILENCE: f64 = 0.0;

const FIRST_CHANNEL_NUMBER: usize = 1;
const SPEC_ENTRY_SEPARATOR: &str = ",";
const SPEC_CHANNEL_SEPARATOR: &str = ":";
const SPEC_GAIN_SEPARATOR: &str = "@";
const SPEC_DECIBEL_SUFFIX: &str = "dB";
const SPEC_GAIN_DECIMALS: usize = 4;

fn linear_from_decibels(decibels: f64) -> f64 {
    DECIBEL_BASE.powf(decibels / DECIBEL_AMPLITUDE_DIVISOR)
}

fn decibels_from_linear(linear: f64) -> f64 {
    DECIBEL_AMPLITUDE_DIVISOR * linear.log10()
}

/// A gain per (input channel, output channel) cell. Cells at exactly zero
/// linear gain are unrouted and contribute nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct MixMatrix {
    input_channels: usize,
    output_channels: usize,
    gains: Vec<f64>,
}

impl MixMatrix {
    /// A matrix with every cell unrouted.
    pub fn silent(input_channels: usize, output_channels: usize) -> MixMatrix {
        MixMatrix {
            input_channels,
            output_channels,
            gains: vec![UNROUTED_GAIN; input_channels * output_channels],
        }
    }

    /// Each input channel to the output channel of the same number at 0 dB.
    pub fn identity(channels: usize) -> MixMatrix {
        let mut matrix = MixMatrix::silent(channels, channels);
        for channel in 0..channels {
            matrix.gains[channel * channels + channel] = UNITY_GAIN;
        }
        matrix
    }

    pub fn input_channels(&self) -> usize {
        self.input_channels
    }

    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    fn cell(&self, input: usize, output: usize) -> Option<usize> {
        if input >= self.input_channels || output >= self.output_channels {
            return None;
        }
        Some(output * self.input_channels + input)
    }

    /// Route 0-based `input` to 0-based `output` at `gain_db`.
    pub fn set_gain_db(&mut self, input: usize, output: usize, gain_db: f64) -> Result<(), String> {
        if gain_db.is_nan() {
            return Err("gain must be a number of decibels".to_string());
        }
        let cell = self.cell(input, output).ok_or_else(|| {
            format!(
                "cell (input {input}, output {output}) is outside a {} by {} matrix",
                self.input_channels, self.output_channels
            )
        })?;
        self.gains[cell] = linear_from_decibels(gain_db);
        Ok(())
    }

    /// The gain of a routed cell, or None when the cell is unrouted or outside
    /// the matrix.
    pub fn gain_db(&self, input: usize, output: usize) -> Option<f64> {
        let linear = self.gains[self.cell(input, output)?];
        if linear == UNROUTED_GAIN {
            return None;
        }
        Some(decibels_from_linear(linear))
    }

    /// True when every output takes at most one input and every routed cell is
    /// at unity, so `apply` moves samples without touching their values.
    pub fn is_pure_routing(&self) -> bool {
        (0..self.output_channels).all(|output| {
            let mut routed = 0;
            for input in 0..self.input_channels {
                let linear = self.gains[output * self.input_channels + input];
                if linear == UNROUTED_GAIN {
                    continue;
                }
                if linear != UNITY_GAIN {
                    return false;
                }
                routed += 1;
            }
            routed <= 1
        })
    }

    fn contributions_by_output(&self) -> Vec<Vec<(usize, f64)>> {
        (0..self.output_channels)
            .map(|output| {
                (0..self.input_channels)
                    .map(|input| (input, self.gains[output * self.input_channels + input]))
                    .filter(|&(_, linear)| linear != UNROUTED_GAIN)
                    .collect()
            })
            .collect()
    }

    /// Parse comma-separated `IN:OUT` or `IN:OUT@GAIN` entries, where IN and OUT
    /// are 1-based channel numbers and GAIN is decibels with an optional dB
    /// suffix. An entry without a gain routes at 0 dB. Several inputs may feed
    /// one output and they sum there, but the same pair twice is an error.
    pub fn parse(
        spec: &str,
        input_channels: usize,
        output_channels: usize,
    ) -> Result<MixMatrix, String> {
        if spec.trim().is_empty() {
            return Err("audio map is empty".to_string());
        }
        let mut matrix = MixMatrix::silent(input_channels, output_channels);
        let mut routed: Vec<(usize, usize)> = Vec::new();
        for entry in spec.split(SPEC_ENTRY_SEPARATOR) {
            let entry = entry.trim();
            if entry.is_empty() {
                return Err("audio map has an empty entry".to_string());
            }
            let (channels, gain_db) = match entry.split_once(SPEC_GAIN_SEPARATOR) {
                Some((channels, gain)) => (channels, parse_decibels(gain, entry)?),
                None => (entry, UNITY_GAIN_DECIBELS),
            };
            let (input_text, output_text) =
                channels.split_once(SPEC_CHANNEL_SEPARATOR).ok_or_else(|| {
                    format!("audio map entry \"{entry}\" is not IN:OUT or IN:OUT@GAIN")
                })?;
            let input = parse_channel_number(input_text, "input", input_channels, entry)?;
            let output = parse_channel_number(output_text, "output", output_channels, entry)?;
            if routed.contains(&(input, output)) {
                return Err(format!(
                    "audio map routes input {} to output {} twice",
                    input + FIRST_CHANNEL_NUMBER,
                    output + FIRST_CHANNEL_NUMBER
                ));
            }
            routed.push((input, output));
            matrix.set_gain_db(input, output, gain_db)?;
        }
        Ok(matrix)
    }

    /// Render the routed cells back in `parse`'s grammar.
    pub fn to_spec(&self) -> String {
        let mut entries = Vec::new();
        for input in 0..self.input_channels {
            for output in 0..self.output_channels {
                let linear = self.gains[output * self.input_channels + input];
                if linear == UNROUTED_GAIN {
                    continue;
                }
                let input_number = input + FIRST_CHANNEL_NUMBER;
                let output_number = output + FIRST_CHANNEL_NUMBER;
                let pair = format!("{input_number}{SPEC_CHANNEL_SEPARATOR}{output_number}");
                if linear == UNITY_GAIN {
                    entries.push(pair);
                    continue;
                }
                let gain = format_decibels(decibels_from_linear(linear));
                entries.push(format!("{pair}{SPEC_GAIN_SEPARATOR}{gain}"));
            }
        }
        entries.join(SPEC_ENTRY_SEPARATOR)
    }
}

fn parse_channel_number(
    text: &str,
    role: &str,
    channels: usize,
    entry: &str,
) -> Result<usize, String> {
    let text = text.trim();
    let number: usize = text.parse().map_err(|_| {
        format!("audio map entry \"{entry}\" has a non-numeric {role} channel \"{text}\"")
    })?;
    if number < FIRST_CHANNEL_NUMBER || number > channels {
        return Err(format!(
            "audio map entry \"{entry}\" names {role} channel {number}, outside {FIRST_CHANNEL_NUMBER}..={channels}"
        ));
    }
    Ok(number - FIRST_CHANNEL_NUMBER)
}

fn parse_decibels(text: &str, entry: &str) -> Result<f64, String> {
    let text = text.trim();
    // a decibel number holds no letters, so the first one starts the unit and
    // "inf" or "nan" fall out as an unknown unit rather than a usable gain.
    let unit_start = text
        .find(|character: char| character.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(unit_start);
    let unit = unit.trim();
    if !unit.is_empty() && !unit.eq_ignore_ascii_case(SPEC_DECIBEL_SUFFIX) {
        return Err(format!(
            "audio map entry \"{entry}\" has an unknown gain unit \"{unit}\""
        ));
    }
    number
        .trim()
        .parse()
        .map_err(|_| format!("audio map entry \"{entry}\" has a non-numeric gain \"{text}\""))
}

fn format_decibels(decibels: f64) -> String {
    let decimals = SPEC_GAIN_DECIMALS;
    let fixed = format!("{decibels:.decimals$}");
    fixed
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// What `apply` wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixReport {
    pub input_channels: usize,
    pub output_channels: usize,
    pub frames: usize,
    pub clipped_samples: usize,
}

struct InputShape {
    channels: usize,
    frames: usize,
}

fn integer_sample_limits(bits_per_sample: u16) -> (f64, f64) {
    let magnitude = (1i64 << (bits_per_sample - 1)) as f64;
    (-magnitude, magnitude - 1.0)
}

fn read_frame(
    sources: &mut [Box<dyn Iterator<Item = Result<f64, hound::Error>>>],
    shapes: &[InputShape],
    frame_index: usize,
    input_frame: &mut [f64],
) -> Result<(), String> {
    let mut first_lane = 0;
    for (source, shape) in sources.iter_mut().zip(shapes) {
        let lanes = first_lane..first_lane + shape.channels;
        first_lane += shape.channels;
        if frame_index >= shape.frames {
            input_frame[lanes].fill(SILENCE);
            continue;
        }
        for lane in lanes {
            let sample = source
                .next()
                .ok_or_else(|| "wav ended before its declared length".to_string())?;
            input_frame[lane] = sample.map_err(|error| format!("wav read: {error}"))?;
        }
    }
    Ok(())
}

/// Mix `inputs` through `matrix` into `output`. The inputs' channels are
/// concatenated in order to make the matrix's input channels, so their counts
/// must add up to it. All inputs must share sample rate, sample format and bit
/// depth; a shorter one is padded with silence to the longest. Mixing is done in
/// f64 on the file's own sample values, rounded and clamped for integer output,
/// so a pure routing reproduces its source samples bit for bit at any depth.
pub fn apply(matrix: &MixMatrix, inputs: &[PathBuf], output: &Path) -> Result<MixReport, String> {
    if inputs.is_empty() {
        return Err("no input files to mix".to_string());
    }
    let mut readers = Vec::with_capacity(inputs.len());
    for path in inputs {
        let reader =
            WavReader::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
        readers.push(reader);
    }

    let first = inputs[0].display();
    let spec = readers[0].spec();
    for (path, reader) in inputs.iter().zip(&readers).skip(1) {
        let other = reader.spec();
        let path = path.display();
        if other.sample_rate != spec.sample_rate {
            return Err(format!(
                "sample rate mismatch: {first} is {} Hz, {path} is {} Hz",
                spec.sample_rate, other.sample_rate
            ));
        }
        if other.sample_format != spec.sample_format {
            return Err(format!(
                "sample format mismatch: {first} is {:?}, {path} is {:?}",
                spec.sample_format, other.sample_format
            ));
        }
        if other.bits_per_sample != spec.bits_per_sample {
            return Err(format!(
                "bit depth mismatch: {first} is {} bit, {path} is {} bit",
                spec.bits_per_sample, other.bits_per_sample
            ));
        }
    }

    let shapes: Vec<InputShape> = readers
        .iter()
        .map(|reader| InputShape {
            channels: reader.spec().channels as usize,
            frames: reader.duration() as usize,
        })
        .collect();
    let total_input_channels: usize = shapes.iter().map(|shape| shape.channels).sum();
    if total_input_channels != matrix.input_channels {
        return Err(format!(
            "inputs carry {total_input_channels} channels, the matrix takes {}",
            matrix.input_channels
        ));
    }
    let output_spec = WavSpec {
        channels: u16::try_from(matrix.output_channels)
            .map_err(|_| format!("{} output channels is too many", matrix.output_channels))?,
        ..spec
    };
    let frames = shapes.iter().map(|shape| shape.frames).max().unwrap_or(0);

    let mut sources: Vec<Box<dyn Iterator<Item = Result<f64, hound::Error>>>> = readers
        .into_iter()
        .map(|reader| match spec.sample_format {
            SampleFormat::Int => Box::new(reader.into_samples::<i32>().map(|s| s.map(f64::from)))
                as Box<dyn Iterator<Item = Result<f64, hound::Error>>>,
            SampleFormat::Float => Box::new(reader.into_samples::<f32>().map(|s| s.map(f64::from))),
        })
        .collect();

    let mut writer = WavWriter::create(output, output_spec)
        .map_err(|error| format!("{}: {error}", output.display()))?;
    let contributions = matrix.contributions_by_output();
    let (minimum_sample, maximum_sample) = integer_sample_limits(spec.bits_per_sample);
    let mut input_frame = vec![SILENCE; matrix.input_channels];
    let mut clipped_samples = 0;
    for frame_index in 0..frames {
        read_frame(&mut sources, &shapes, frame_index, &mut input_frame)?;
        for cells in &contributions {
            let mixed: f64 = cells
                .iter()
                .map(|&(input, gain)| input_frame[input] * gain)
                .sum();
            let written = match spec.sample_format {
                SampleFormat::Int => {
                    let rounded = mixed.round();
                    let clamped = rounded.clamp(minimum_sample, maximum_sample);
                    if clamped != rounded {
                        clipped_samples += 1;
                    }
                    writer.write_sample(clamped as i32)
                }
                SampleFormat::Float => writer.write_sample(mixed as f32),
            };
            written.map_err(|error| format!("{}: {error}", output.display()))?;
        }
    }
    writer
        .finalize()
        .map_err(|error| format!("{}: {error}", output.display()))?;

    Ok(MixReport {
        input_channels: matrix.input_channels,
        output_channels: matrix.output_channels,
        frames,
        clipped_samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_unity_and_pure() {
        let matrix = MixMatrix::identity(6);
        assert_eq!(matrix.input_channels(), 6);
        assert_eq!(matrix.output_channels(), 6);
        assert!(matrix.is_pure_routing());
        for channel in 0..6 {
            assert_eq!(matrix.gain_db(channel, channel), Some(0.0));
            assert_eq!(matrix.gain_db(channel, (channel + 1) % 6), None);
        }
    }

    #[test]
    fn silent_routes_nothing() {
        let matrix = MixMatrix::silent(2, 6);
        assert!(matrix.is_pure_routing());
        assert_eq!(matrix.to_spec(), "");
        for input in 0..2 {
            for output in 0..6 {
                assert_eq!(matrix.gain_db(input, output), None);
            }
        }
    }

    #[test]
    fn gains_round_trip_through_decibels() {
        let mut matrix = MixMatrix::silent(2, 2);
        matrix.set_gain_db(0, 1, -6.0206).unwrap();
        let gain = matrix.gain_db(0, 1).unwrap();
        assert!((gain - -6.0206).abs() < 1e-9, "got {gain}");
        // -6.0206 dB is a halving to within rounding at any sane bit depth.
        let cell = matrix.cell(0, 1).unwrap();
        assert!((matrix.gains[cell] - 0.5).abs() < 1e-8);
    }

    #[test]
    fn out_of_range_cells_fail_loud() {
        let mut matrix = MixMatrix::silent(2, 4);
        assert!(matrix.set_gain_db(2, 0, 0.0).is_err());
        assert!(matrix.set_gain_db(0, 4, 0.0).is_err());
        assert!(matrix.set_gain_db(0, 0, f64::NAN).is_err());
        assert_eq!(matrix.gain_db(2, 0), None);
        assert_eq!(matrix.gain_db(0, 4), None);
    }

    #[test]
    fn a_gain_or_a_summed_output_is_not_pure_routing() {
        let mut gained = MixMatrix::silent(2, 2);
        gained.set_gain_db(0, 0, -3.0).unwrap();
        assert!(!gained.is_pure_routing());

        let mut summed = MixMatrix::silent(2, 1);
        summed.set_gain_db(0, 0, 0.0).unwrap();
        summed.set_gain_db(1, 0, 0.0).unwrap();
        assert!(!summed.is_pure_routing());

        // one input fanned out to two outputs is still pure.
        let mut fanned = MixMatrix::silent(1, 2);
        fanned.set_gain_db(0, 0, 0.0).unwrap();
        fanned.set_gain_db(0, 1, 0.0).unwrap();
        assert!(fanned.is_pure_routing());
    }

    #[test]
    fn parses_entries_with_and_without_gain() {
        let matrix = MixMatrix::parse("1:1, 2:2 , 1:3@-6dB, 2:3 @ -6 DB", 2, 6).unwrap();
        assert_eq!(matrix.gain_db(0, 0), Some(0.0));
        assert_eq!(matrix.gain_db(1, 1), Some(0.0));
        assert!((matrix.gain_db(0, 2).unwrap() - -6.0).abs() < 1e-9);
        assert!((matrix.gain_db(1, 2).unwrap() - -6.0).abs() < 1e-9);
        assert_eq!(matrix.gain_db(0, 1), None);
        assert!(!matrix.is_pure_routing());
    }

    #[test]
    fn parse_rejects_bad_specs() {
        let cases = [
            ("", "empty"),
            ("   ", "empty"),
            ("1:1,,2:2", "empty entry"),
            ("1:1,3:1", "outside 1..=2"),
            ("0:1", "outside 1..=2"),
            ("1:7", "outside 1..=6"),
            ("1:1,1:1", "twice"),
            ("banana", "not IN:OUT"),
            ("1;1", "not IN:OUT"),
            ("x:1", "non-numeric input"),
            ("1:y", "non-numeric output"),
            ("1:1@loud", "unknown gain unit"),
            ("1:1@-inf", "unknown gain unit"),
            ("1:1@", "non-numeric gain"),
        ];
        for (spec, wanted) in cases {
            let error = MixMatrix::parse(spec, 2, 6).unwrap_err();
            assert!(
                error.contains(wanted),
                "spec {spec:?} said {error:?}, wanted {wanted:?}"
            );
        }
    }

    #[test]
    fn to_spec_round_trips() {
        for spec in ["1:1", "1:1,2:2", "1:1@-6.0206,2:2@3.5,2:6", "1:3,2:3"] {
            let matrix = MixMatrix::parse(spec, 2, 6).unwrap();
            assert_eq!(matrix.to_spec(), spec);
            assert_eq!(MixMatrix::parse(&matrix.to_spec(), 2, 6).unwrap(), matrix);
        }
        // an explicit 0 dB renders back in the short form.
        let matrix = MixMatrix::parse("1:1@0dB", 2, 6).unwrap();
        assert_eq!(matrix.to_spec(), "1:1");
    }
}
