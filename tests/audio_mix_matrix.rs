use hound::{SampleFormat, WavSpec};
use postkit::audio_mix_matrix::{MixMatrix, mix_wav_files};
use postkit::wav_io::{Samples, read_interleaved_exact, write_interleaved_exact};
use std::path::Path;

const SAMPLE_RATE: u32 = 48000;

fn int_spec(channels: u16, bits_per_sample: u16) -> WavSpec {
    WavSpec {
        channels,
        sample_rate: SAMPLE_RATE,
        bits_per_sample,
        sample_format: SampleFormat::Int,
    }
}

fn write_ints(path: &Path, spec: WavSpec, samples: &[i32]) {
    write_interleaved_exact(path, spec, &Samples::Int(samples.to_vec())).unwrap();
}

fn read_ints(path: &Path) -> (WavSpec, Vec<i32>) {
    let (spec, samples) = read_interleaved_exact(path).unwrap();
    let Samples::Int(samples) = samples else {
        panic!("int wav read back as float");
    };
    (spec, samples)
}

fn read_floats(path: &Path) -> (WavSpec, Vec<f32>) {
    let (spec, samples) = read_interleaved_exact(path).unwrap();
    let Samples::Float(samples) = samples else {
        panic!("float wav read back as int");
    };
    (spec, samples)
}

/// Values that only survive if every bit of the sample does.
fn awkward_ints(bits_per_sample: u16, count: usize) -> Vec<i32> {
    let top = 1i64 << (bits_per_sample - 1);
    let mut values = vec![0, 1, -1, top - 1, -top, top / 3, -(top / 3) - 1, 12345];
    for shift in 0..bits_per_sample {
        values.push((top - 1) >> shift);
        values.push(-top + (1 << shift));
    }
    values
        .into_iter()
        .cycle()
        .take(count)
        .map(|value| value as i32)
        .collect()
}

#[test]
fn pure_routing_to_a_wider_output_keeps_every_sample() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("three.wav");
    let output = dir.path().join("six.wav");
    let frames = 500;
    let samples = awkward_ints(24, frames * 3);
    write_ints(&source, int_spec(3, 24), &samples);

    let matrix = MixMatrix::parse("1:1,2:2,3:3", 3, 6).unwrap();
    assert!(matrix.is_pure_routing());
    let report = mix_wav_files(&matrix, &[source], &output).unwrap();
    assert_eq!(report.frames, frames);
    assert_eq!(report.output_channels, 6);
    assert_eq!(report.clipped_samples, 0);

    let (spec, mixed) = read_ints(&output);
    assert_eq!(spec.channels, 6);
    assert_eq!(spec.bits_per_sample, 24);
    assert_eq!(spec.sample_rate, SAMPLE_RATE);
    assert_eq!(mixed.len(), frames * 6);
    for frame in 0..frames {
        for lane in 0..3 {
            assert_eq!(
                mixed[frame * 6 + lane],
                samples[frame * 3 + lane],
                "frame {frame} lane {lane} changed"
            );
        }
        for lane in 3..6 {
            assert_eq!(mixed[frame * 6 + lane], 0, "frame {frame} lane {lane}");
        }
    }
}

#[test]
fn pure_routing_is_bit_exact_at_32_bit_int() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("in32.wav");
    let output = dir.path().join("out32.wav");
    let mut samples = awkward_ints(32, 400);
    samples[0] = i32::MIN;
    samples[1] = i32::MAX;
    write_ints(&source, int_spec(2, 32), &samples);

    // swap the pair: routing must move samples without touching their values.
    let matrix = MixMatrix::parse("1:2,2:1", 2, 2).unwrap();
    mix_wav_files(&matrix, &[source], &output).unwrap();

    let (spec, mixed) = read_ints(&output);
    assert_eq!(spec.bits_per_sample, 32);
    for frame in 0..samples.len() / 2 {
        assert_eq!(
            mixed[frame * 2],
            samples[frame * 2 + 1],
            "frame {frame} left"
        );
        assert_eq!(
            mixed[frame * 2 + 1],
            samples[frame * 2],
            "frame {frame} right"
        );
    }
}

#[test]
fn a_minus_six_db_cell_halves_the_samples() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("mono.wav");
    let output = dir.path().join("half.wav");
    let samples = vec![1000, -1000, 32766, -32768, 0, 2];
    write_ints(&source, int_spec(1, 16), &samples);

    let matrix = MixMatrix::parse("1:1@-6.0206dB", 1, 1).unwrap();
    assert!(!matrix.is_pure_routing());
    let report = mix_wav_files(&matrix, &[source], &output).unwrap();
    assert_eq!(report.clipped_samples, 0);

    let (_, mixed) = read_ints(&output);
    assert_eq!(mixed, vec![500, -500, 16383, -16384, 0, 1]);
}

#[test]
fn summed_inputs_clip_and_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let loud = dir.path().join("loud.wav");
    let quiet = dir.path().join("quiet.wav");
    let output = dir.path().join("summed.wav");
    let frames = 50;
    write_ints(&loud, int_spec(1, 16), &vec![30000; frames]);
    write_ints(&quiet, int_spec(1, 16), &vec![-30000; frames]);

    let matrix = MixMatrix::parse("1:1,2:1", 2, 1).unwrap();
    let inputs = vec![loud.clone(), loud.clone()];
    let report = mix_wav_files(&matrix, &inputs, &output).unwrap();
    assert_eq!(report.frames, frames);
    assert_eq!(report.clipped_samples, frames);
    let (_, mixed) = read_ints(&output);
    assert!(mixed.iter().all(|&sample| sample == 32767), "{mixed:?}");

    // and the negative rail clamps the same way.
    let report = mix_wav_files(&matrix, &[quiet.clone(), quiet], &output).unwrap();
    assert_eq!(report.clipped_samples, frames);
    let (_, mixed) = read_ints(&output);
    assert!(mixed.iter().all(|&sample| sample == -32768), "{mixed:?}");

    // halving each side first leaves the sum inside the rail, so nothing clips.
    let matrix = MixMatrix::parse("1:1@-6.0206,2:1@-6.0206", 2, 1).unwrap();
    let report = mix_wav_files(&matrix, &[loud.clone(), loud], &output).unwrap();
    assert_eq!(report.clipped_samples, 0);
    let (_, mixed) = read_ints(&output);
    assert!(mixed.iter().all(|&sample| sample == 30000), "{mixed:?}");
}

#[test]
fn several_files_concatenate_into_the_input_channels() {
    let dir = tempfile::tempdir().unwrap();
    let stereo = dir.path().join("stereo.wav");
    let mono = dir.path().join("mono.wav");
    let output = dir.path().join("three.wav");
    let frames = 20;
    let stereo_samples: Vec<i32> = (0..frames as i32)
        .flat_map(|frame| [frame + 100, frame + 200])
        .collect();
    let mono_samples: Vec<i32> = (0..frames as i32).map(|frame| frame + 300).collect();
    write_ints(&stereo, int_spec(2, 24), &stereo_samples);
    write_ints(&mono, int_spec(1, 24), &mono_samples);

    let matrix = MixMatrix::identity(3);
    let report = mix_wav_files(&matrix, &[stereo, mono], &output).unwrap();
    assert_eq!(report.input_channels, 3);
    assert_eq!(report.frames, frames);

    let (spec, mixed) = read_ints(&output);
    assert_eq!(spec.channels, 3);
    for frame in 0..frames {
        assert_eq!(mixed[frame * 3], stereo_samples[frame * 2]);
        assert_eq!(mixed[frame * 3 + 1], stereo_samples[frame * 2 + 1]);
        assert_eq!(mixed[frame * 3 + 2], mono_samples[frame]);
    }
}

#[test]
fn a_shorter_input_is_padded_with_silence() {
    let dir = tempfile::tempdir().unwrap();
    let long = dir.path().join("long.wav");
    let short = dir.path().join("short.wav");
    let output = dir.path().join("padded.wav");
    write_ints(&long, int_spec(1, 16), &[7; 10]);
    write_ints(&short, int_spec(1, 16), &[9; 4]);

    let matrix = MixMatrix::identity(2);
    let report = mix_wav_files(&matrix, &[long, short], &output).unwrap();
    assert_eq!(report.frames, 10);

    let (_, mixed) = read_ints(&output);
    let second_channel: Vec<i32> = mixed.chunks_exact(2).map(|frame| frame[1]).collect();
    assert_eq!(second_channel, vec![9, 9, 9, 9, 0, 0, 0, 0, 0, 0]);
    assert!(mixed.chunks_exact(2).all(|frame| frame[0] == 7));
}

#[test]
fn float_files_route_exactly_and_mix_unclamped() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("float.wav");
    let routed = dir.path().join("routed.wav");
    let summed = dir.path().join("summed.wav");
    let spec = WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let samples = vec![0.0, 1.0, -1.0, 0.333_333_34, 0.999_999_94, 0.7];
    write_interleaved_exact(&source, spec, &Samples::Float(samples.clone())).unwrap();

    mix_wav_files(
        &MixMatrix::identity(2),
        std::slice::from_ref(&source),
        &routed,
    )
    .unwrap();
    let (routed_spec, routed_samples) = read_floats(&routed);
    assert_eq!(routed_spec.sample_format, SampleFormat::Float);
    assert_eq!(routed_samples, samples);

    // summing both channels into one overshoots full scale, and float keeps it.
    let matrix = MixMatrix::parse("1:1,2:1", 2, 1).unwrap();
    let report = mix_wav_files(&matrix, &[source], &summed).unwrap();
    assert_eq!(report.clipped_samples, 0);
    let (_, mixed) = read_floats(&summed);
    assert!((mixed[1] - (-1.0 + 0.333_333_34)).abs() < 1e-6, "{mixed:?}");
    assert!(mixed[2] > 1.0, "float mix was clamped: {}", mixed[2]);
}

#[test]
fn mismatched_or_miscounted_inputs_fail_loud() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.wav");
    let output = dir.path().join("out.wav");
    write_ints(&base, int_spec(1, 16), &[1; 10]);

    let other_rate = dir.path().join("rate.wav");
    write_ints(
        &other_rate,
        WavSpec {
            sample_rate: 44100,
            ..int_spec(1, 16)
        },
        &[1; 10],
    );
    let other_depth = dir.path().join("depth.wav");
    write_ints(&other_depth, int_spec(1, 24), &[1; 10]);
    let other_format = dir.path().join("format.wav");
    write_interleaved_exact(
        &other_format,
        WavSpec {
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
            ..int_spec(1, 16)
        },
        &Samples::Float(vec![0.0; 10]),
    )
    .unwrap();

    let matrix = MixMatrix::identity(2);
    let cases = [
        (&other_rate, "sample rate mismatch"),
        (&other_depth, "bit depth mismatch"),
        (&other_format, "sample format mismatch"),
    ];
    for (path, wanted) in cases {
        let error = mix_wav_files(&matrix, &[base.clone(), path.clone()], &output).unwrap_err();
        assert!(error.contains(wanted), "got {error:?}, wanted {wanted:?}");
    }

    let error = mix_wav_files(
        &MixMatrix::identity(6),
        std::slice::from_ref(&base),
        &output,
    )
    .unwrap_err();
    assert!(error.contains("carry 1 channels"), "got {error:?}");

    let missing = dir.path().join("nothing.wav");
    let error = mix_wav_files(&MixMatrix::identity(1), &[missing], &output).unwrap_err();
    assert!(error.contains("nothing.wav"), "got {error:?}");

    let error = mix_wav_files(&MixMatrix::identity(1), &[], &output).unwrap_err();
    assert!(error.contains("no input files"), "got {error:?}");
}
