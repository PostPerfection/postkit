//! ffmpeg writing interleaved 16-bit little-endian RGB to the pipe, and TIFF
//! stills packed the same way, with grok's accelerator plugin taking those
//! buffers as one batch source.
//!
//! Its own process, because the plugin is a switch for the whole process.
//!
//! Every check is on decoded samples: the plugin's cinema encode is not
//! reproducible run to run, so a code stream comparison would fail on frames
//! that carry the same picture.

#![cfg(feature = "grok-gpu")]

use postkit::encode::{
    EncodeResult, SourceColour, StreamEncodeOptions, default_rsiz, encode_tiff_sequence_inprocess,
    stream_encode_inprocess,
};
use postkit::grok_encoder::{accelerated_frames, initialize, use_cpu, use_gpu};
use postkit::j2k::{ImfProfile, imf_levels, imf_rsiz};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 256;
const HEIGHT: u32 = 144;
const FRAMES: u64 = 24;
const STILLS: u64 = 8;
const FRAME_RATE: f64 = 24.0;
const IMF_BITS_PER_SECOND: u64 = 200_000_000;
/// The depth every code stream here carries, cinema and IMF alike.
const SAMPLE_PRECISION: u8 = 12;
const MAX_SAMPLE: f64 = ((1u32 << 12) - 1) as f64;
/// Under this the device encode and the host encode are not the same picture.
const LEAST_PSNR_DB: f64 = 50.0;

/// One decoded frame's three components.
type Frame = Vec<Vec<i32>>;

/// Peak signal to noise ratio between one component of two decodes, in dB at
/// the code stream's own sample depth. Infinite for identical samples.
fn psnr_db(device: &[i32], host: &[i32]) -> f64 {
    assert_eq!(
        device.len(),
        host.len(),
        "the two decodes carry different sample counts"
    );
    let squared_error: f64 = device
        .iter()
        .zip(host)
        .map(|(a, b)| {
            let difference = (*a - *b) as f64;
            difference * difference
        })
        .sum();
    if squared_error == 0.0 {
        return f64::INFINITY;
    }
    let mean_squared_error = squared_error / device.len() as f64;
    20.0 * (MAX_SAMPLE / mean_squared_error.sqrt()).log10()
}

/// One run shape: the profile the code streams declare, the colour the frames
/// are in, which decides whether the compressor runs its X'Y'Z' transform, and
/// the pixel format the plugin's answer leaves the decode on.
struct Case {
    label: &'static str,
    rsiz: u16,
    source_colour: SourceColour,
    pipe_pixel_format: &'static str,
    /// Whether the two runs are held to the same picture. The plugin's per-call
    /// compress writes a different picture from the host's for a cinema code
    /// stream at 12 bits out of a 16-bit source with the transform off, which
    /// is the one shape no batch here takes.
    same_picture: bool,
}

fn cases() -> Vec<Case> {
    let profile = ImfProfile::for_raster(WIDTH, HEIGHT).unwrap();
    let levels = imf_levels(WIDTH, HEIGHT, FRAME_RATE, IMF_BITS_PER_SECOND).unwrap();
    vec![
        Case {
            label: "cinema with the X'Y'Z' transform on",
            rsiz: default_rsiz(),
            source_colour: SourceColour::DisplayRgb,
            pipe_pixel_format: "rgb48le",
            same_picture: true,
        },
        // the plugin declines a batch over a 16-bit source at 12 bits with the
        // transform off, interleaved and planar alike, so these two keep
        // deinterleaving on the host
        Case {
            label: "cinema with the X'Y'Z' transform off",
            rsiz: default_rsiz(),
            source_colour: SourceColour::AlreadyPq,
            pipe_pixel_format: "rgb48be",
            same_picture: false,
        },
        Case {
            label: "imf",
            rsiz: imf_rsiz(profile, levels),
            source_colour: SourceColour::KeepRgb,
            pipe_pixel_format: "rgb48be",
            same_picture: true,
        },
    ]
}

/// A yuv444p H.264 clip, a source no planar YUV batch takes, so the run picks
/// packed RGB and the byte order is the only thing left to choose.
fn yuv444p_clip(dir: &Path) -> PathBuf {
    let path = dir.join("source.mp4");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "gradients=size={WIDTH}x{HEIGHT}:rate=24:duration=1"
        ))
        .args(["-frames:v", &FRAMES.to_string()])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv444p"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "fixture encode failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    path
}

/// 16-bit RGB stills where every component differs at every pixel and every
/// still differs from the one before it, so a still filed under the wrong index
/// cannot pass.
fn tiff_sequence(dir: &Path) -> Vec<PathBuf> {
    (0..STILLS)
        .map(|index| {
            let path = dir.join(format!("still_{index:04}.tif"));
            let shift = index as u32 * 4093;
            let mut samples = Vec::with_capacity((WIDTH * HEIGHT * 3) as usize);
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    samples.push(((x * 257 + y * 61 + shift) % 65536) as u16);
                    samples.push(((x * 37 + y * 521 + shift * 3) % 65536) as u16);
                    samples.push(((x * 149 + y * 7 + shift * 5) % 65536) as u16);
                }
            }
            let file = std::fs::File::create(&path).unwrap();
            tiff::encoder::TiffEncoder::new(std::io::BufWriter::new(file))
                .unwrap()
                .write_image::<tiff::encoder::colortype::RGB16>(WIDTH, HEIGHT, &samples)
                .unwrap();
            path
        })
        .collect()
}

fn options(output_dir: &Path, rsiz: u16, source_colour: &SourceColour) -> StreamEncodeOptions {
    StreamEncodeOptions {
        output_dir: output_dir.to_path_buf(),
        rsiz,
        source_colour: source_colour.clone(),
        ..StreamEncodeOptions::default()
    }
}

fn encode_video(input: &Path, output_dir: &Path, case: &Case) -> EncodeResult {
    stream_encode_inprocess(
        &StreamEncodeOptions {
            input: input.to_path_buf(),
            ..options(output_dir, case.rsiz, &case.source_colour)
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
}

/// Every code stream in `dir` decoded back, checked for the run's raster and
/// depth on the way.
fn decoded_frames(dir: &Path, count: u64, label: &str) -> Vec<Frame> {
    (0..count)
        .map(|index| {
            let path = dir.join(format!("frame_{index:08}.j2c"));
            let codestream = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("{label}: frame {index} was not written: {e}"));
            let frame = postkit::grok_decoder::decode(codestream, 0)
                .unwrap_or_else(|e| panic!("{label}: frame {index}: {e}"));
            assert_eq!(
                (frame.width, frame.height),
                (WIDTH, HEIGHT),
                "{label}: frame {index} came back at the wrong raster"
            );
            assert_eq!(
                frame.precision, SAMPLE_PRECISION,
                "{label}: frame {index} came back at the wrong depth"
            );
            assert_eq!(
                frame.components.len(),
                3,
                "{label}: frame {index} came back with the wrong component count"
            );
            frame.components
        })
        .collect()
}

/// Both decodes hold the same picture, component by component and frame by
/// frame. Hands back the worst ratio it saw, so the run reports its margin.
fn assert_same_picture(device: &[Frame], host: &[Frame], label: &str) -> f64 {
    assert_eq!(
        device.len(),
        host.len(),
        "{label}: the two runs wrote different frame counts"
    );
    let mut worst = f64::INFINITY;
    for (index, (on_device, on_host)) in device.iter().zip(host).enumerate() {
        for (component, (a, b)) in on_device.iter().zip(on_host).enumerate() {
            let psnr = psnr_db(a, b);
            assert!(
                psnr > LEAST_PSNR_DB,
                "{label}: frame {index} component {component} is {psnr:.1} dB against the \
                 host encode of the same source, under {LEAST_PSNR_DB} dB"
            );
            worst = worst.min(psnr);
        }
    }
    worst
}

/// One test, because the plugin is a switch for the whole process and two
/// tests would race over it.
#[test]
fn an_interleaved_rgb_source_reaches_the_plugin_as_one_buffer() {
    initialize(0);
    if let Err(reason) = use_gpu() {
        panic!("{reason}");
    }

    let dir = tempfile::tempdir().unwrap();
    let input = yuv444p_clip(dir.path());
    let stills = tiff_sequence(dir.path());
    let cases = cases();

    for (at, case) in cases.iter().enumerate() {
        let output_dir = dir.path().join(format!("device_{at}"));
        let result = encode_video(&input, &output_dir, case);
        assert!(
            result.success,
            "{}: device encode failed: {}",
            case.label, result.error
        );
        assert_eq!(
            result.pipe_pixel_format.as_deref(),
            Some(case.pipe_pixel_format),
            "{}: the decode wrote the wrong byte order for what the plugin takes",
            case.label
        );
        assert_eq!(result.frames_encoded, FRAMES, "{}", case.label);
    }

    let before_stills = accelerated_frames();
    let stills_result = encode_tiff_sequence_inprocess(
        &stills,
        &options(
            &dir.path().join("stills_device"),
            default_rsiz(),
            &SourceColour::DisplayRgb,
        ),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        None,
        |_| {},
    );
    assert!(
        stills_result.success,
        "tiff sequence: device encode failed: {}",
        stills_result.error
    );
    assert_eq!(
        accelerated_frames(),
        before_stills + STILLS,
        "tiff sequence: the batch has to have taken every still"
    );

    use_cpu();

    for (at, case) in cases.iter().enumerate() {
        let output_dir = dir.path().join(format!("host_{at}"));
        let result = encode_video(&input, &output_dir, case);
        assert!(
            result.success,
            "{}: host encode failed: {}",
            case.label, result.error
        );
        assert_eq!(
            result.pipe_pixel_format.as_deref(),
            Some("rgb48be"),
            "{}: a host run deinterleaves the frames itself",
            case.label
        );
    }

    let stills_result = encode_tiff_sequence_inprocess(
        &stills,
        &options(
            &dir.path().join("stills_host"),
            default_rsiz(),
            &SourceColour::DisplayRgb,
        ),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        None,
        |_| {},
    );
    assert!(
        stills_result.success,
        "tiff sequence: host encode failed: {}",
        stills_result.error
    );

    // every decode runs on the host, so the same decoder reads both arms
    for (at, case) in cases.iter().enumerate() {
        if !case.same_picture {
            continue;
        }
        let on_device =
            decoded_frames(&dir.path().join(format!("device_{at}")), FRAMES, case.label);
        let on_host = decoded_frames(&dir.path().join(format!("host_{at}")), FRAMES, case.label);
        let worst = assert_same_picture(&on_device, &on_host, case.label);
        println!("{}: worst {worst:.1} dB", case.label);
    }

    let label = "tiff sequence";
    let on_device = decoded_frames(&dir.path().join("stills_device"), STILLS, label);
    let on_host = decoded_frames(&dir.path().join("stills_host"), STILLS, label);
    let worst = assert_same_picture(&on_device, &on_host, label);
    println!("{label}: worst {worst:.1} dB");
}
