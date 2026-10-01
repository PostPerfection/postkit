// temporary: goes with the pipe it compares the in-process decode against

use postkit::encode::{
    DecodeSource, EncodeResult, FrameRange, FrameRate, SourceColour, StreamEncodeOptions,
    read_decode_in_process, stream_encode_inprocess, write_image_concat_list,
};
use postkit::grok_encoder::{CompressParams, PipelineResult};
use postkit::picture_findings::PictureFindings;
use postkit::picture_processing::{Crop, Fit, PictureProcessing, Rotation};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// the pipe side has to run the FFmpeg source the libraries were built from
const REFERENCE_FFMPEG_VERSION: &str = "ffmpeg version n8.1.3";
// keeps two 4K runs side by side inside the test memory cap
const ENCODE_THREADS: u32 = 4;

fn ffmpeg(args: &[&str]) {
    let run = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg has to run");
    assert!(
        run.status.success(),
        "fixture ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
}

fn lavfi(dir: &Path, name: &str, source: &str, frames: u32, codec_args: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let mut args = vec!["-f", "lavfi", "-i", source];
    let frame_count = frames.to_string();
    args.extend(["-frames:v", &frame_count]);
    args.extend(codec_args);
    args.push(path.to_str().unwrap());
    ffmpeg(&args);
    path
}

fn assert_reference_ffmpeg() {
    for program in ["ffmpeg", "ffprobe"] {
        let version = Command::new(program)
            .arg("-version")
            .output()
            .unwrap_or_else(|e| panic!("{program} has to run: {e}"));
        let first = String::from_utf8_lossy(&version.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            first.contains(REFERENCE_FFMPEG_VERSION.trim_start_matches("ffmpeg ")),
            "put an ffmpeg built from n8.1.3 first on PATH, {program} is: {first}"
        );
    }
}

enum Run {
    Stream(StreamEncodeOptions),
    Resumable {
        input: PathBuf,
        frames: u64,
        width: u32,
        height: u32,
        filters: Option<String>,
        frame_range: Option<FrameRange>,
        source_colour: SourceColour,
        detect_picture_findings: bool,
        // codestreams removed after a first full run, which the resume encodes again
        resume_after_removing: Option<u64>,
    },
}

struct Case {
    name: &'static str,
    run: Run,
    // denoise is the one decode the two readers may disagree on
    bytes_may_differ: bool,
}

#[derive(Debug, PartialEq)]
struct Output {
    success: bool,
    error: String,
    frames_encoded: u64,
    pixel_format: Option<String>,
    findings: PictureFindings,
    codestreams: Vec<(String, Vec<u8>)>,
}

fn codestreams(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|e| e == "j2c"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&path).unwrap())
        })
        .collect()
}

fn from_stream(result: EncodeResult, dir: &Path) -> Output {
    Output {
        success: result.success,
        error: result.error,
        frames_encoded: result.frames_encoded,
        pixel_format: result.encoder_input_pixel_format,
        findings: result.picture_findings,
        codestreams: codestreams(dir),
    }
}

fn from_pipeline(result: PipelineResult, dir: &Path) -> Output {
    Output {
        success: result.success,
        error: result.error,
        frames_encoded: result.frames_encoded,
        pixel_format: None,
        findings: result.picture_findings,
        codestreams: codestreams(dir),
    }
}

fn resumable(run: &Run, dir: &Path, resume: bool) -> PipelineResult {
    let Run::Resumable {
        input,
        frames,
        width,
        height,
        filters,
        frame_range,
        source_colour,
        detect_picture_findings,
        ..
    } = run
    else {
        unreachable!()
    };
    postkit::grok_encoder::encode_video_pipeline_resumable_with_mxf_feed(
        input,
        dir,
        &CompressParams {
            edit_rate: FrameRate::whole(24),
            apply_xyz_transform: source_colour.applies_xyz_transform(),
            encode_threads: ENCODE_THREADS,
            detect_picture_findings: *detect_picture_findings,
            ..CompressParams::default()
        },
        *frames,
        *width,
        *height,
        source_colour,
        &Arc::new(AtomicBool::new(false)),
        resume,
        filters.as_deref(),
        *frame_range,
        None,
        |_| {},
        |_| {},
    )
}

fn encode(run: &Run, dir: &Path) -> Output {
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    match run {
        Run::Stream(options) => {
            let options = StreamEncodeOptions {
                output_dir: dir.to_path_buf(),
                encode_threads: ENCODE_THREADS,
                ..options.clone()
            };
            from_stream(
                stream_encode_inprocess(&options, &cancel, &pause, |_| {}),
                dir,
            )
        }
        Run::Resumable {
            resume_after_removing,
            ..
        } => {
            let Some(removed) = resume_after_removing else {
                return from_pipeline(resumable(run, dir, false), dir);
            };
            let first = resumable(run, dir, false);
            assert!(
                first.success,
                "the run before the resume failed: {}",
                first.error
            );
            let total = postkit::grok_encoder::contiguous_encoded_frames(dir);
            for index in total.saturating_sub(*removed)..total {
                std::fs::remove_file(dir.join(format!("frame_{index:08}.j2c"))).unwrap();
            }
            from_pipeline(resumable(run, dir, true), dir)
        }
    }
}

fn stream(input: &Path, configure: impl FnOnce(&mut StreamEncodeOptions)) -> Run {
    let mut options = StreamEncodeOptions {
        input: input.to_path_buf(),
        fps: FrameRate::whole(24),
        ..StreamEncodeOptions::default()
    };
    configure(&mut options);
    Run::Stream(options)
}

fn image_list(dir: &Path, name: &str, stills: &[PathBuf]) -> PathBuf {
    let list = dir.join(name);
    write_image_concat_list(stills, FrameRate::whole(24), &list).unwrap();
    list
}

fn stills(dir: &Path, folder: &str, source: &str, count: u32, extension: &str) -> Vec<PathBuf> {
    let folder = dir.join(folder);
    std::fs::create_dir_all(&folder).unwrap();
    let pattern = folder.join(format!("still_%03d.{extension}"));
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        source,
        "-frames:v",
        &count.to_string(),
        pattern.to_str().unwrap(),
    ]);
    postkit::encode::find_source_frames(&folder).unwrap()
}

// swaps red and blue, so a LUT that never ran shows in every sample
const CHANNEL_SWAP_CUBE: &str =
    "LUT_3D_SIZE 2\n0 0 0\n0 0 1\n0 1 0\n0 1 1\n1 0 0\n1 0 1\n1 1 0\n1 1 1\n";

fn cpu_cases(dir: &Path) -> Vec<Case> {
    let eight_bit = lavfi(
        dir,
        "eight_bit.mkv",
        "testsrc2=s=320x180:r=24",
        12,
        &["-c:v", "mpeg2video", "-q:v", "2", "-pix_fmt", "yuv420p"],
    );
    let ntsc = lavfi(
        dir,
        "ntsc.mkv",
        "testsrc2=s=160x90:r=24000/1001",
        60,
        &["-c:v", "ffv1", "-pix_fmt", "yuv420p"],
    );
    let ten_bit = |name: &str, tags: &[&str]| {
        let mut args = vec!["-c:v", "ffv1", "-pix_fmt", "yuv420p10le"];
        args.extend(tags);
        lavfi(dir, name, "testsrc2=s=256x144:r=24", 4, &args)
    };
    let bt709 = ten_bit(
        "bt709.mkv",
        &[
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
        ],
    );
    let bt2020 = ten_bit(
        "bt2020.mkv",
        &[
            "-colorspace",
            "bt2020nc",
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "smpte2084",
        ],
    );
    let toms_shape = lavfi(
        dir,
        "toms_shape.mkv",
        "testsrc2=s=4096x1716:r=24",
        2,
        &["-c:v", "ffv1", "-pix_fmt", "yuv422p10le"],
    );
    let twelve_bit_rgb = lavfi(
        dir,
        "rgb12.mkv",
        "testsrc2=s=256x144:r=24",
        4,
        &["-c:v", "ffv1", "-pix_fmt", "gbrp12le"],
    );
    let stored = lavfi(
        dir,
        "stored.mkv",
        "testsrc2=s=256x144:r=24",
        6,
        &["-c:v", "ffv1", "-pix_fmt", "yuv420p"],
    );
    let turned = dir.join("turned.mp4");
    let mpeg4_source = lavfi(
        dir,
        "upright.mp4",
        "testsrc2=s=256x144:r=24",
        6,
        &["-c:v", "mpeg4", "-q:v", "2", "-pix_fmt", "yuv420p"],
    );
    ffmpeg(&[
        "-display_rotation",
        "90",
        "-i",
        mpeg4_source.to_str().unwrap(),
        "-c",
        "copy",
        turned.to_str().unwrap(),
    ]);
    let detection = dir.join("detection.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=black:s=128x72:r=24:d=3",
        "-f",
        "lavfi",
        "-i",
        "testsrc=s=128x72:r=24:d=3",
        "-f",
        "lavfi",
        "-i",
        "color=0x336699:s=128x72:r=24:d=3",
        "-filter_complex",
        "[0][1][2]concat=n=3:v=1",
        "-c:v",
        "ffv1",
        "-pix_fmt",
        "yuv444p",
        detection.to_str().unwrap(),
    ]);
    // every fifth frame dropped and its gap kept, so the constant rate step fills it
    let variable_rate = dir.join("variable_rate.mkv");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=160x90:r=24:d=2",
        "-vf",
        "select=not(eq(mod(n\\,5)\\,2))",
        "-fps_mode",
        "passthrough",
        "-c:v",
        "ffv1",
        variable_rate.to_str().unwrap(),
    ]);
    let lut = dir.join("swap.cube");
    std::fs::write(&lut, CHANNEL_SWAP_CUBE).unwrap();
    let jpegs = stills(dir, "jpegs", "testsrc2=s=160x90:r=24", 6, "jpg");
    let jpeg_list = image_list(dir, "jpegs.ffconcat", &jpegs);
    let pngs = stills(dir, "pngs", "testsrc2=s=160x90:r=24", 4, "png");
    let png_list = image_list(dir, "pngs.ffconcat", &pngs);
    // 8-bit stills then 16-bit stills, so the graph is built again halfway
    let mut mixed = stills(dir, "mixed_a", "testsrc2=s=160x90:r=24", 2, "png");
    mixed.extend(stills(
        dir,
        "mixed_b",
        "testsrc2=s=160x90:r=24,format=rgb48be",
        2,
        "png",
    ));
    let mixed_list = image_list(dir, "mixed.ffconcat", &mixed);

    let fit = |width, height| Fit {
        box_width: width,
        box_height: height,
        raster_width: width,
        raster_height: height,
    };
    let picture = |configure: fn(&mut PictureProcessing)| {
        let mut processing = PictureProcessing::default();
        configure(&mut processing);
        processing
    };

    vec![
        Case {
            name: "8-bit yuv420p, rgb48be through gbrp16le",
            run: stream(&eight_bit, |_| {}),
            bytes_may_differ: false,
        },
        Case {
            name: "23.976 into fps=24, one frame duplicated",
            run: stream(&ntsc, |_| {}),
            bytes_may_differ: false,
        },
        Case {
            name: "23.976 read at 24 (-r on the input)",
            run: stream(&ntsc, |o| o.read_source_at = Some(FrameRate::whole(24))),
            bytes_may_differ: false,
        },
        Case {
            name: "trim window with -frames:v",
            run: stream(&ntsc, |o| {
                o.frame_range = Some(FrameRange {
                    first_frame: 10,
                    frame_count: 7,
                })
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "10-bit BT.709 kept as RGB",
            run: stream(&bt709, |o| o.source_colour = SourceColour::KeepRgb),
            bytes_may_differ: false,
        },
        Case {
            name: "10-bit BT.2020 kept as RGB",
            run: stream(&bt2020, |o| o.source_colour = SourceColour::KeepRgb),
            bytes_may_differ: false,
        },
        Case {
            name: "HDR10 through the zscale head",
            run: stream(&bt2020, |o| {
                o.source_colour = SourceColour::HdrDcdm {
                    source: postkit::colour::HdrSource::Hdr10,
                    source_peak_nits: 1000.0,
                }
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "4096x1716 yuv422p10le padded to 4096x2160",
            run: stream(&toms_shape, |o| o.picture.fit = Some(fit(4096, 2160))),
            bytes_may_differ: false,
        },
        Case {
            name: "12-bit RGB",
            run: stream(&twelve_bit_rgb, |_| {}),
            bytes_may_differ: false,
        },
        Case {
            name: "crop, scale and pad",
            run: stream(&stored, |o| {
                o.picture.crop = Crop {
                    left: 16,
                    right: 16,
                    top: 8,
                    bottom: 8,
                };
                o.picture.fit = Some(fit(320, 180));
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "transpose clockwise",
            run: stream(&stored, |o| {
                o.picture = picture(|p| p.rotation = Rotation::Clockwise90)
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "transpose counterclockwise",
            run: stream(&stored, |o| {
                o.picture = picture(|p| p.rotation = Rotation::CounterClockwise90)
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "half turn",
            run: stream(&stored, |o| {
                o.picture = picture(|p| p.rotation = Rotation::Half)
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "hflip and vflip",
            run: stream(&stored, |o| {
                o.picture = picture(|p| {
                    p.flip_horizontal = true;
                    p.flip_vertical = true;
                })
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "yadif",
            run: stream(&stored, |o| o.picture = picture(|p| p.deinterlace = true)),
            bytes_may_differ: false,
        },
        Case {
            name: "lut3d with a real cube",
            run: stream(&stored, |o| {
                o.source_colour = SourceColour::DciLut(lut.clone())
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "display matrix turned upright",
            run: stream(&turned, |_| {}),
            bytes_may_differ: false,
        },
        Case {
            name: "JPEG image list",
            run: stream(&jpeg_list, |o| o.decode_source = DecodeSource::ImageList),
            bytes_may_differ: false,
        },
        Case {
            name: "PNG image list with a crop",
            run: stream(&png_list, |o| {
                o.decode_source = DecodeSource::ImageList;
                o.picture.crop = Crop {
                    left: 8,
                    right: 8,
                    top: 4,
                    bottom: 4,
                };
            }),
            bytes_may_differ: false,
        },
        Case {
            name: "image list changing pixel format halfway",
            run: stream(&mixed_list, |o| o.decode_source = DecodeSource::ImageList),
            bytes_may_differ: false,
        },
        Case {
            name: "detection branch, stream path",
            run: stream(&detection, |o| o.detect_picture_findings = true),
            bytes_may_differ: false,
        },
        Case {
            name: "detection branch, resumable path",
            run: Run::Resumable {
                input: detection.clone(),
                frames: 216,
                width: 128,
                height: 72,
                filters: None,
                frame_range: None,
                source_colour: SourceColour::DisplayRgb,
                detect_picture_findings: true,
                resume_after_removing: None,
            },
            bytes_may_differ: false,
        },
        Case {
            name: "resumable window, then a resume",
            run: Run::Resumable {
                input: ntsc.clone(),
                frames: 20,
                width: 160,
                height: 90,
                filters: Some("fade=t=in:st=0:d=0.5".to_string()),
                frame_range: Some(FrameRange {
                    first_frame: 5,
                    frame_count: 20,
                }),
                source_colour: SourceColour::DisplayRgb,
                detect_picture_findings: false,
                resume_after_removing: Some(6),
            },
            bytes_may_differ: false,
        },
        Case {
            name: "variable rate source, no fps filter",
            run: Run::Resumable {
                input: variable_rate.clone(),
                frames: 48,
                width: 160,
                height: 90,
                filters: None,
                frame_range: None,
                source_colour: SourceColour::DisplayRgb,
                detect_picture_findings: false,
                resume_after_removing: None,
            },
            bytes_may_differ: false,
        },
        Case {
            name: "denoise (hqdn3d against atadenoise)",
            run: stream(&stored, |o| o.picture = picture(|p| p.denoise = true)),
            bytes_may_differ: true,
        },
    ]
}

struct Row {
    name: &'static str,
    frames: (u64, u64),
    pixel_format: Option<String>,
    verdict: String,
}

fn compare(case: &Case, root: &Path) -> Row {
    let pipe_dir = root.join("pipe");
    let in_process_dir = root.join("in_process");
    if case.bytes_may_differ {
        read_decode_in_process(true);
        let in_process = encode(&case.run, &in_process_dir);
        read_decode_in_process(false);
        let verdict = if in_process.success && in_process.frames_encoded > 0 {
            "runs in process, bytes not compared".to_string()
        } else {
            format!("FAILED: {}", in_process.error)
        };
        return Row {
            name: case.name,
            frames: (0, in_process.frames_encoded),
            pixel_format: in_process.pixel_format,
            verdict,
        };
    }
    read_decode_in_process(false);
    let pipe = encode(&case.run, &pipe_dir);
    read_decode_in_process(true);
    let in_process = encode(&case.run, &in_process_dir);
    read_decode_in_process(false);

    let frames = (pipe.frames_encoded, in_process.frames_encoded);
    let pixel_format = in_process.pixel_format.clone();
    let verdict = if !pipe.success || !in_process.success {
        format!(
            "FAILED: pipe {:?}, in process {:?}",
            pipe.error, in_process.error
        )
    } else if pipe.frames_encoded == 0 {
        "FAILED: no frames".to_string()
    } else if pipe == in_process {
        "identical".to_string()
    } else if pipe.findings != in_process.findings {
        format!(
            "FAILED: findings {:?} against {:?}",
            pipe.findings, in_process.findings
        )
    } else if pipe.codestreams.len() != in_process.codestreams.len() {
        format!(
            "FAILED: {} codestreams against {}",
            pipe.codestreams.len(),
            in_process.codestreams.len()
        )
    } else {
        let first = pipe
            .codestreams
            .iter()
            .zip(&in_process.codestreams)
            .find(|(a, b)| a != b)
            .map(|((name, _), _)| name.clone())
            .unwrap_or_else(|| "metadata".to_string());
        format!("FAILED: first difference in {first}")
    };
    Row {
        name: case.name,
        frames,
        pixel_format,
        verdict,
    }
}

fn report(rows: &[Row]) {
    println!("| case | frames pipe, in process | pixel format | result |");
    println!("|---|---|---|---|");
    for row in rows {
        println!(
            "| {} | {}, {} | {} | {} |",
            row.name,
            row.frames.0,
            row.frames.1,
            row.pixel_format.as_deref().unwrap_or("-"),
            row.verdict
        );
    }
    let failed: Vec<_> = rows
        .iter()
        .filter(|row| row.verdict.starts_with("FAILED"))
        .map(|row| row.name)
        .collect();
    assert!(failed.is_empty(), "these cases differ: {failed:?}");
}

#[test]
fn the_in_process_decode_writes_the_pipes_codestreams() {
    assert_reference_ffmpeg();
    let dir = tempfile::tempdir().unwrap();
    let cases = cpu_cases(dir.path());
    let rows: Vec<Row> = cases
        .iter()
        .enumerate()
        .map(|(index, case)| compare(case, &dir.path().join(format!("case_{index:02}"))))
        .collect();
    report(&rows);
}

#[cfg(feature = "grok-gpu")]
mod device {
    use super::*;

    fn encoded_clip(dir: &Path, name: &str, size: &str, codec_args: &[&str]) -> PathBuf {
        lavfi(
            dir,
            name,
            &format!("testsrc2=s={size}:r=24"),
            24,
            codec_args,
        )
    }

    fn gpu_cases(dir: &Path) -> Vec<Case> {
        let planar = |name: &'static str, pixel_format: &str| {
            let clip = lavfi(
                dir,
                &format!("{pixel_format}.mkv"),
                "testsrc2=s=1920x1080:r=24",
                12,
                &["-c:v", "ffv1", "-pix_fmt", pixel_format],
            );
            (name, clip)
        };
        let mut cases = Vec::new();
        for (name, clip) in [
            planar("yuv420p", "yuv420p"),
            planar("yuv422p", "yuv422p"),
            planar("yuv420p10le", "yuv420p10le"),
            planar("yuv422p10le", "yuv422p10le"),
        ] {
            cases.push(Case {
                name,
                run: stream(&clip, |_| {}),
                bytes_may_differ: false,
            });
            cases.push(Case {
                name,
                run: stream(&clip, |o| {
                    o.picture.crop = Crop {
                        left: 64,
                        right: 64,
                        top: 32,
                        bottom: 32,
                    }
                }),
                bytes_may_differ: false,
            });
            cases.push(Case {
                name,
                run: stream(&clip, |o| {
                    o.picture.fit = Some(Fit {
                        box_width: 2048,
                        box_height: 1080,
                        raster_width: 2048,
                        raster_height: 1080,
                    })
                }),
                bytes_may_differ: false,
            });
        }
        let yuv422p10le = dir.join("yuv422p10le.mkv");
        cases.push(Case {
            name: "yuv422p10le with vflip, negative linesize at the sink",
            run: stream(&yuv422p10le, |o| o.picture.flip_vertical = true),
            bytes_may_differ: false,
        });
        let rgb = lavfi(
            dir,
            "yuv444p.mkv",
            "testsrc2=s=1920x1080:r=24",
            12,
            &["-c:v", "ffv1", "-pix_fmt", "yuv444p"],
        );
        cases.push(Case {
            name: "rgb48le from 4:4:4",
            run: stream(&rgb, |_| {}),
            bytes_may_differ: false,
        });
        let mpeg2 = encoded_clip(
            dir,
            "mpeg2.mkv",
            "1920x1080",
            &["-c:v", "mpeg2video", "-q:v", "2", "-pix_fmt", "yuv420p"],
        );
        cases.push(Case {
            name: "mpeg2 decoded on the device (NV12 download)",
            run: stream(&mpeg2, |_| {}),
            bytes_may_differ: false,
        });
        let hevc_ten_bit = encoded_clip(
            dir,
            "hevc10.mkv",
            "1920x1080",
            &[
                "-c:v",
                "hevc_nvenc",
                "-profile:v",
                "main10",
                "-pix_fmt",
                "p010le",
            ],
        );
        cases.push(Case {
            name: "hevc main10 decoded on the device (P010 download)",
            run: stream(&hevc_ten_bit, |_| {}),
            bytes_may_differ: false,
        });
        let toms_shape = lavfi(
            dir,
            "toms_shape.mkv",
            "testsrc2=s=4096x1716:r=24",
            4,
            &["-c:v", "ffv1", "-pix_fmt", "yuv422p10le"],
        );
        cases.push(Case {
            name: "4096x1716 yuv422p10le padded to 4096x2160",
            run: stream(&toms_shape, |o| {
                o.picture.fit = Some(Fit {
                    box_width: 4096,
                    box_height: 2160,
                    raster_width: 4096,
                    raster_height: 2160,
                })
            }),
            bytes_may_differ: false,
        });
        cases
    }

    #[test]
    fn the_in_process_decode_writes_the_pipes_codestreams_on_the_device() {
        assert_reference_ffmpeg();
        for variable in [
            "GRK_PLUGIN_PATH",
            postkit::grok_encoder::GPU_LICENSE_VARIABLE,
        ] {
            assert!(
                std::env::var_os(variable).is_some(),
                "the device comparison needs {variable} set"
            );
        }
        postkit::grok_encoder::use_gpu_from_environment().expect("the plugin has to start");
        let dir = tempfile::tempdir().unwrap();
        let cases = gpu_cases(dir.path());
        let rows: Vec<Row> = cases
            .iter()
            .enumerate()
            .map(|(index, case)| compare(case, &dir.path().join(format!("case_{index:02}"))))
            .collect();
        report(&rows);
    }
}
