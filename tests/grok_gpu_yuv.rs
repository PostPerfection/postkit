//! ffmpeg writing the source's own planar YUV to the pipe and grok's
//! accelerator plugin taking those planes, measured against the same clip
//! encoded from packed RGB on the CPU.
//!
//! The two paths convert the colour in different places: the plugin upsamples
//! the chroma and converts on the device, while swscale converts to RGB on the
//! pipe and grok converts on the host. A smooth source keeps the two chroma
//! filters from separating, so what is left is the colour pipeline, and the
//! decoded samples have to agree.
//!
//! Its own process, because the plugin is a switch for the whole process.
//!
//! The packed RGB side of an 8-bit YUV source converts through 16-bit planar
//! RGB, which is what puts the two 8-bit cases over the floor.

#![cfg(feature = "grok-gpu")]

use postkit::encode::{StreamEncodeOptions, stream_encode_inprocess};
use postkit::grok_decoder::DecodedFrame;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 256;
const HEIGHT: u32 = 144;
const FRAMES: u64 = 24;
/// Both paths convert the same colour, so the decoded samples differ only by
/// the rounding each of them does.
const PSNR_FLOOR_DB: f64 = 50.0;
/// What the packed RGB path writes to the pipe.
const PACKED_RGB: &str = "rgb48be";

/// One clip encoded both ways.
struct Case {
    name: &'static str,
    options: StreamEncodeOptions,
    /// what ffmpeg has to write to the pipe once the plugin is on
    pipe_pixel_format: &'static str,
}

/// A clip of a smooth two-colour gradient, encoded by `codec_args` so the
/// container carries the pixel format the case needs.
///
/// The colours and the gradient line are named rather than left to
/// `gradients`, which picks its own afresh on every run.
fn gradients_clip(dir: &Path, name: &str, codec_args: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "gradients=size={WIDTH}x{HEIGHT}:rate=24:nb_colors=2:c0=0x102080:c1=0xe0c060\
             :x0=0:y0=0:x1={}:y1={}",
            WIDTH - 1,
            HEIGHT - 1
        ))
        .args(["-frames:v", &FRAMES.to_string()])
        .args(codec_args)
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

/// PSNR in dB between two decoded components.
///
/// Each side is divided by its own peak first: the plugin writes 12-bit
/// samples where a plain code stream from packed RGB keeps the pipe's 16, and
/// both hold the same normalised value.
fn psnr_db(left: &[i32], left_precision: u8, right: &[i32], right_precision: u8) -> f64 {
    assert_eq!(
        left.len(),
        right.len(),
        "a component cannot be measured against one of another length"
    );
    let left_peak = f64::from((1u32 << left_precision) - 1);
    let right_peak = f64::from((1u32 << right_precision) - 1);
    let squared_error: f64 = left
        .iter()
        .zip(right)
        .map(|(left_sample, right_sample)| {
            let difference =
                f64::from(*left_sample) / left_peak - f64::from(*right_sample) / right_peak;
            difference * difference
        })
        .sum();
    if squared_error == 0.0 {
        return f64::INFINITY;
    }
    -10.0 * (squared_error / left.len() as f64).log10()
}

/// Encode the case into a subdirectory named after the path that ran, checking
/// the pipe carried the format that path takes, and hand back that directory.
fn encode(case: &Case, path: &str, pipe_pixel_format: &str) -> PathBuf {
    let label = format!("{} on the {path}", case.name);
    let options = StreamEncodeOptions {
        output_dir: case.options.output_dir.join(path),
        ..case.options.clone()
    };
    let result = stream_encode_inprocess(
        &options,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    );

    assert!(result.success, "{label}: encode failed: {}", result.error);
    assert_eq!(
        result.pipe_pixel_format.as_deref(),
        Some(pipe_pixel_format),
        "{label}: the pipe format is what says which path ran"
    );
    assert_eq!(result.frames_encoded, FRAMES, "{label}");
    options.output_dir
}

/// Decode every frame of a run.
fn decode_run(label: &str, output_dir: &Path) -> Vec<DecodedFrame> {
    (0..FRAMES)
        .map(|index| {
            let codestream = std::fs::read(output_dir.join(format!("frame_{index:08}.j2c")))
                .unwrap_or_else(|e| panic!("{label}: frame {index} was not written: {e}"));
            let frame = postkit::grok_decoder::decode(codestream, 0)
                .unwrap_or_else(|e| panic!("{label}: frame {index}: {e}"));
            assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT), "{label}");
            assert_eq!(frame.components.len(), 3, "{label}");
            frame
        })
        .collect()
}

/// Measure every component of every frame of one path against the other and
/// hand back the ones under the floor, so a run reports all of them instead of
/// stopping at the first.
///
/// The plugin's cinema encode is not reproducible run to run, so the code
/// stream bytes are never compared, only the samples they decode to.
fn samples_under_the_floor(
    case: &Case,
    device: &[DecodedFrame],
    host: &[DecodedFrame],
) -> Vec<String> {
    let mut worst = f64::INFINITY;
    let mut under_the_floor = Vec::new();
    for (index, (device_frame, host_frame)) in device.iter().zip(host).enumerate() {
        let components = device_frame.components.iter().zip(&host_frame.components);
        for (component, (device_samples, host_samples)) in components.enumerate() {
            let psnr = psnr_db(
                device_samples,
                device_frame.precision,
                host_samples,
                host_frame.precision,
            );
            worst = worst.min(psnr);
            if psnr < PSNR_FLOOR_DB {
                under_the_floor.push(format!(
                    "{} frame {index} component {component}: {psnr:.2} dB",
                    case.name
                ));
            }
        }
    }
    println!(
        "{}: the worst component of the run is {worst:.2} dB",
        case.name
    );
    under_the_floor
}

/// One test, because the plugin is a switch for the whole process and two
/// tests would race over it.
#[test]
fn a_yuv_source_reaches_the_plugin_as_planes() {
    postkit::grok_encoder::initialize(0);
    let dir = tempfile::tempdir().unwrap();

    let yuv420p = gradients_clip(
        dir.path(),
        "yuv420p.mp4",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
    );
    // 10 bits in a two-byte container, and chroma at full height
    let yuv422p10le = gradients_clip(
        dir.path(),
        "yuv422p10le.mov",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "3",
            "-pix_fmt",
            "yuv422p10le",
        ],
    );

    let cases = [
        Case {
            name: "yuv420p",
            options: StreamEncodeOptions {
                input: yuv420p.clone(),
                output_dir: dir.path().join("plain"),
                // 1:1, so what is measured is the colour rather than each
                // path's own quantisation
                compression_ratio: 1.0,
                // a plain code stream, so the profile constrains nothing
                rsiz: 0,
                num_resolutions: 3,
                ..StreamEncodeOptions::default()
            },
            pipe_pixel_format: "yuv420p",
        },
        // the same clip as a DCP frame, which is the path dcpwizard takes: the
        // device converts to X'Y'Z' and grok's host transform is what it has to
        // match
        Case {
            name: "yuv420p cinema",
            options: StreamEncodeOptions {
                input: yuv420p,
                output_dir: dir.path().join("cinema"),
                compression_ratio: 1.0,
                ..StreamEncodeOptions::default()
            },
            pipe_pixel_format: "yuv420p",
        },
        Case {
            name: "yuv422p10le",
            options: StreamEncodeOptions {
                input: yuv422p10le,
                output_dir: dir.path().join("ten_bit"),
                compression_ratio: 1.0,
                rsiz: 0,
                num_resolutions: 3,
                ..StreamEncodeOptions::default()
            },
            pipe_pixel_format: "yuv422p10le",
        },
    ];

    // every device encode runs before the switch: the first plugin decode after
    // a second batch segfaults inside grk_plugin_decompress
    if let Err(reason) = postkit::grok_encoder::use_gpu() {
        panic!("{reason}");
    }
    let device_runs: Vec<PathBuf> = cases
        .iter()
        .map(|case| encode(case, "device", case.pipe_pixel_format))
        .collect();

    postkit::grok_encoder::use_cpu();
    let host_runs: Vec<PathBuf> = cases
        .iter()
        .map(|case| encode(case, "cpu", PACKED_RGB))
        .collect();

    // both paths decode on the cpu, so the encode is the only difference left
    let mut under_the_floor = Vec::new();
    for ((case, device_run), host_run) in cases.iter().zip(&device_runs).zip(&host_runs) {
        let device = decode_run(&format!("{} on the device", case.name), device_run);
        let host = decode_run(&format!("{} on the cpu", case.name), host_run);
        under_the_floor.extend(samples_under_the_floor(case, &device, &host));
    }
    assert!(
        under_the_floor.is_empty(),
        "the device and the cpu run disagree by more than the {PSNR_FLOOR_DB} dB floor: {}",
        under_the_floor.join(", ")
    );
}
