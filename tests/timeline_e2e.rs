use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use postkit::encode::{FrameRate, SourceColour};
use postkit::j2k::{ImfProfile, imf_levels, imf_rsiz};
use postkit::mxf_wrap::{IncrementalWrapOptions, MxfStandard, rec709_sdr_picture_colour};
use postkit::picture_findings::{FrameInterval, PictureFindings};
use postkit::picture_processing::{Fit, PictureProcessing, Placement};
use postkit::pipeline::{
    EncodeRunOptions, PipelineProgress, StretchBase, run_encode_timeline,
    run_encode_timeline_stretch_and_wrap_picture, timeline_stretches,
};
use postkit::subtitle_retime::parse_srt;
use postkit::timeline::{
    Clip, ClipSound, SoundTrack, TIMELINE_FORMAT_VERSION, Timeline, join_subtitles, render_sound,
};
use postkit::wav_io::WavReader;

const RATE: u32 = 24;
const SAMPLES_PER_FRAME: u64 = 48_000 / RATE as u64;
const SOUND_CHANNELS: u16 = 2;

// each 24-bit sample holds its own sample number, so a cut shows where it landed
fn clip_with_sound(directory: &Path, name: &str, rate: u32, seconds: f64) -> PathBuf {
    let clip = directory.join(name);
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "testsrc=size=320x180:rate={rate}:duration={seconds}"
        ))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "aevalsrc=exprs=n/8388608|-n/8388608:sample_rate=48000:duration={seconds}"
        ))
        .args(["-pix_fmt", "yuv420p", "-c:a", "pcm_s24le", "-shortest"])
        .arg(&clip)
        .output()
        .expect("ffmpeg");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    clip
}

fn clip(
    picture: &Path,
    start: u64,
    in_frame: u64,
    out_frame: u64,
    subtitle: Option<PathBuf>,
) -> Clip {
    Clip {
        picture: picture.to_path_buf(),
        start,
        layer: 0,
        opacity: 1.0,
        in_frame,
        out_frame,
        sound: ClipSound::Embedded,
        subtitle,
        source_colour: SourceColour::default(),
        picture_processing: PictureProcessing::default(),
    }
}

fn timeline(clips: Vec<Clip>) -> Timeline {
    Timeline {
        version: TIMELINE_FORMAT_VERSION,
        fps: FrameRate::whole(RATE),
        clips,
        sound: SoundTrack::PerClip,
        subtitle: None,
        reel_starts: vec![24],
    }
}

fn at_the_timeline_rate() -> EncodeRunOptions {
    EncodeRunOptions {
        fps: FrameRate::whole(RATE),
        ..Default::default()
    }
}

fn encode(
    timeline: &Timeline,
    output: &Path,
    options: &EncodeRunOptions,
) -> (postkit::pipeline::EncodeResult, Vec<u64>) {
    let frames_reported = Mutex::new(Vec::new());
    let result = run_encode_timeline(
        timeline,
        output,
        options,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |progress: &PipelineProgress| {
            assert_eq!(progress.total_frames, timeline.total_frames());
            frames_reported.lock().unwrap().push(progress.frame);
        },
        |_: &str| {},
    )
    .expect("timeline encode");
    (result, frames_reported.into_inner().unwrap())
}

fn codestream_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn numbered_codestreams(count: u64) -> Vec<String> {
    (0..count)
        .map(|frame| format!("frame_{frame:08}.j2c"))
        .collect()
}

#[test]
fn two_trimmed_clips_build_one_picture_sound_and_subtitle_track() {
    let directory = tempfile::tempdir().unwrap();
    let first = clip_with_sound(directory.path(), "first.mov", RATE, 1.5);
    let second = clip_with_sound(directory.path(), "second.mov", RATE, 1.0);
    let first_subtitle = directory.path().join("first.srt");
    std::fs::write(
        &first_subtitle,
        "1\n00:00:00,500 --> 00:00:01,000\nin the window\n\n\
         2\n00:00:01,200 --> 00:00:01,400\nruns past the out point\n",
    )
    .unwrap();
    let second_subtitle = directory.path().join("second.srt");
    std::fs::write(
        &second_subtitle,
        "1\n00:00:00,200 --> 00:00:00,400\nsecond clip\n",
    )
    .unwrap();

    let timeline = timeline(vec![
        clip(&first, 0, 6, 30, Some(first_subtitle)),
        clip(&second, 24, 4, 16, Some(second_subtitle)),
    ]);
    let total_frames = 36;
    assert_eq!(timeline.total_frames(), total_frames);
    timeline.check().expect("both clips fit their sources");

    let sound = render_sound(&timeline, &directory.path().join("timeline.wav"))
        .unwrap()
        .expect("both clips carry sound");
    let reader = WavReader::open(&sound).unwrap();
    assert_eq!(reader.spec().channels, SOUND_CHANNELS);
    assert_eq!(reader.spec().sample_rate, 48_000);
    assert_eq!(reader.duration(), total_frames * SAMPLES_PER_FRAME);
    let samples: Vec<i32> = reader.into_samples::<i32>().map(Result::unwrap).collect();
    let source_sample_numbers: Vec<i32> = samples.iter().step_by(2).copied().collect();
    let first_clip_length = (24 * SAMPLES_PER_FRAME) as usize;
    let first_in_point = (6 * SAMPLES_PER_FRAME) as i32;
    let second_in_point = (4 * SAMPLES_PER_FRAME) as i32;
    assert_eq!(source_sample_numbers[0], first_in_point);
    assert_eq!(
        samples[1], -first_in_point,
        "the second channel stays apart"
    );
    assert_eq!(
        source_sample_numbers[first_clip_length - 1],
        first_in_point + first_clip_length as i32 - 1
    );
    assert_eq!(source_sample_numbers[first_clip_length], second_in_point);
    assert_eq!(
        source_sample_numbers.last(),
        Some(&(second_in_point + (12 * SAMPLES_PER_FRAME) as i32 - 1))
    );

    let subtitle = join_subtitles(&timeline, &directory.path().join("joined.srt"))
        .unwrap()
        .expect("both clips carry subtitles");
    let cues: Vec<(u64, u64, String)> = parse_srt(&std::fs::read_to_string(subtitle).unwrap())
        .into_iter()
        .map(|cue| (cue.start_ms, cue.end_ms, cue.text))
        .collect();
    assert_eq!(
        cues,
        vec![
            (250, 750, "in the window".to_string()),
            (950, 1000, "runs past the out point".to_string()),
            (1033, 1233, "second clip".to_string()),
        ]
    );

    let (result, frames_reported) = encode(
        &timeline,
        &directory.path().join("out"),
        &at_the_timeline_rate(),
    );
    assert_eq!(result.frames_encoded, total_frames);
    assert_eq!(
        codestream_names(&result.j2k_dir),
        numbered_codestreams(total_frames)
    );
    assert!(
        frames_reported.windows(2).all(|pair| pair[0] <= pair[1]),
        "progress runs on across clips: {frames_reported:?}"
    );
    assert_eq!(frames_reported.last(), Some(&total_frames));

    let followed_by_codestreams = Timeline {
        reel_starts: Vec::new(),
        ..self::timeline(vec![
            clip(&second, 0, 0, 24, None),
            clip(&result.j2k_dir, 24, 0, total_frames, None),
        ])
    };
    followed_by_codestreams
        .check()
        .expect("a codestream directory takes the timeline's rate");
    let (linked, _) = encode(
        &followed_by_codestreams,
        &directory.path().join("linked"),
        &at_the_timeline_rate(),
    );
    assert_eq!(
        codestream_names(&linked.j2k_dir),
        numbered_codestreams(24 + total_frames)
    );
    assert_eq!(
        std::fs::read(linked.j2k_dir.join("frame_00000024.j2c")).unwrap(),
        std::fs::read(result.j2k_dir.join("frame_00000000.j2c")).unwrap(),
        "the codestreams follow on at the clip's place"
    );

    let after_a_gap = Timeline {
        reel_starts: Vec::new(),
        ..self::timeline(vec![clip(&result.j2k_dir, 4, 0, total_frames, None)])
    };
    let (placed, _) = encode(
        &after_a_gap,
        &directory.path().join("after_a_gap"),
        &EncodeRunOptions {
            picture: fitted_to_the_raster(),
            ..at_the_timeline_rate()
        },
    );
    assert_eq!(
        codestream_names(&placed.j2k_dir),
        numbered_codestreams(4 + total_frames)
    );
    assert_eq!(
        std::fs::read(placed.j2k_dir.join("frame_00000004.j2c")).unwrap(),
        std::fs::read(result.j2k_dir.join("frame_00000000.j2c")).unwrap(),
        "the fit that sizes the gap's black leaves the codestreams alone"
    );
    let gap_pixels = sampled_pixels(&placed.j2k_dir.join("frame_00000000.j2c"));
    assert!(
        gap_pixels.iter().all(|&pixel| close_to(pixel, [0; 3])),
        "the gap is black: {gap_pixels:?}"
    );

    let trimmed_codestreams = self::timeline(vec![clip(&result.j2k_dir, 0, 1, total_frames, None)]);
    let refused = run_encode_timeline(
        &trimmed_codestreams,
        &directory.path().join("trimmed"),
        &EncodeRunOptions::default(),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .err()
    .expect("a codestream directory cannot be trimmed");
    assert!(refused.starts_with("clip 1 ("), "{refused}");
    assert!(refused.contains("cannot be trimmed"), "{refused}");
}

#[test]
fn a_clip_at_another_rate_is_refused_by_name() {
    let directory = tempfile::tempdir().unwrap();
    let matching = clip_with_sound(directory.path(), "matching.mov", RATE, 0.5);
    let other = clip_with_sound(directory.path(), "other.mov", 25, 0.5);
    let refused = timeline(vec![
        clip(&matching, 0, 0, 12, None),
        clip(&other, 12, 0, 12, None),
    ])
    .check()
    .unwrap_err();
    assert!(
        refused.starts_with(&format!("clip 2 ({}) runs at 25/1 fps", other.display())),
        "{refused}"
    );
}

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

fn run_ffmpeg(arguments: &[&str]) {
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(arguments)
        .output()
        .expect("ffmpeg");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
}

// rgb all the way, so the blue reaches the encode exactly
fn blue_video(directory: &Path, seconds: u32) -> PathBuf {
    let clip = directory.join("blue.mkv");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=blue:s={WIDTH}x{HEIGHT}:rate={RATE}:duration={seconds}"),
        "-pix_fmt",
        "gbrp",
        "-c:v",
        "ffv1",
        &clip.to_string_lossy(),
    ]);
    clip
}

// red over the left half, transparent over the right
fn half_red_stills(directory: &Path, frames: u32) -> PathBuf {
    let stills = directory.join("half_red");
    std::fs::create_dir_all(&stills).unwrap();
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=red:s={}x{HEIGHT}:rate={RATE}", WIDTH / 2),
        "-vf",
        &format!("format=rgba,pad={WIDTH}:{HEIGHT}:0:0:color=black@0"),
        "-frames:v",
        &frames.to_string(),
        "-pix_fmt",
        "rgba",
        &stills.join("frame_%04d.png").to_string_lossy(),
    ]);
    stills
}

// opaque with no alpha, half the raster's width, so the fit pads it on both sides
fn red_pillar_stills(directory: &Path, frames: u32) -> PathBuf {
    let stills = directory.join("red_pillar");
    std::fs::create_dir_all(&stills).unwrap();
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=red:s={}x{HEIGHT}:rate={RATE}", WIDTH / 2),
        "-frames:v",
        &frames.to_string(),
        "-pix_fmt",
        "rgb24",
        &stills.join("frame_%04d.png").to_string_lossy(),
    ]);
    stills
}

fn layered(picture: &Path, start: u64, frames: u64, layer: u32, opacity: f64) -> Clip {
    Clip {
        layer,
        opacity,
        sound: ClipSound::None,
        source_colour: SourceColour::KeepRgb,
        ..clip(picture, start, 0, frames, None)
    }
}

fn fitted_to_the_raster() -> PictureProcessing {
    PictureProcessing {
        fit: Some(Fit {
            box_width: WIDTH,
            box_height: HEIGHT,
            raster_width: WIDTH,
            raster_height: HEIGHT,
            placement: Placement::default(),
        }),
        ..PictureProcessing::default()
    }
}

// inside the pillar and left of the half red edge, and outside both
fn sampled_pixels(codestream: &Path) -> [[i32; 3]; 3] {
    let frame = postkit::grok_decoder::decode(std::fs::read(codestream).unwrap(), 0)
        .expect("the codestream decodes");
    assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));
    let to_sixteen_bits = 16 - i32::from(frame.precision);
    [WIDTH / 8, WIDTH * 3 / 8, WIDTH * 7 / 8].map(|x| {
        let at = (HEIGHT / 2 * WIDTH + x) as usize;
        [0, 1, 2].map(|component| frame.components[component][at] << to_sixteen_bits)
    })
}

const FULL: i32 = 65535;
const BLACK: [i32; 3] = [0, 0, 0];
const BLUE: [i32; 3] = [0, 0, FULL];
const RED: [i32; 3] = [FULL, 0, 0];
const RED_AT_HALF_OPACITY_OVER_BLUE: [i32; 3] = [32768, 0, 32767];
// compression at 10:1 takes a flat patch about one percent off, composited or not
const SAMPLE_TOLERANCE: i32 = 1 << 10;

fn close_to(found: [i32; 3], want: [i32; 3]) -> bool {
    found
        .iter()
        .zip(want)
        .all(|(&found, want)| (found - want).abs() <= SAMPLE_TOLERANCE)
}

// a black gap, a half red overlay running from it onto the blue base, a padded
// opaque pillar, and the half red again at half opacity
fn compositing_timeline(directory: &Path) -> Timeline {
    let blue = blue_video(directory, 1);
    let half_red = half_red_stills(directory, 8);
    let red_pillar = red_pillar_stills(directory, 4);
    Timeline {
        reel_starts: Vec::new(),
        ..timeline(vec![
            layered(&half_red, 20, 8, 2, 0.5),
            layered(&blue, 6, 24, 0, 1.0),
            layered(&half_red, 4, 8, 1, 1.0),
            layered(&red_pillar, 14, 4, 1, 1.0),
        ])
    }
}

fn compositing_options() -> EncodeRunOptions {
    EncodeRunOptions {
        source_colour: SourceColour::KeepRgb,
        picture: fitted_to_the_raster(),
        ..at_the_timeline_rate()
    }
}

#[test]
fn upper_layers_composite_over_layer_0_and_its_gaps_are_black() {
    let directory = tempfile::tempdir().unwrap();
    let timeline = compositing_timeline(directory.path());
    let total_frames = 30;
    assert_eq!(timeline.total_frames(), total_frames);

    let (result, frames_reported) = encode(
        &timeline,
        &directory.path().join("out"),
        &compositing_options(),
    );
    assert_eq!(result.frames_encoded, total_frames);
    let names = codestream_names(&result.j2k_dir);
    assert_eq!(names, numbered_codestreams(total_frames));
    assert_eq!(frames_reported.last(), Some(&total_frames));

    let expected = |frame: u64| match frame {
        0..4 => [BLACK, BLACK, BLACK],
        4..6 => [RED, RED, BLACK],
        6..12 => [RED, RED, BLUE],
        14..18 => [BLUE, RED, BLUE],
        20..28 => [
            RED_AT_HALF_OPACITY_OVER_BLUE,
            RED_AT_HALF_OPACITY_OVER_BLUE,
            BLUE,
        ],
        _ => [BLUE, BLUE, BLUE],
    };
    for (frame, name) in names.iter().enumerate() {
        let found = sampled_pixels(&result.j2k_dir.join(name));
        let want = expected(frame as u64);
        assert!(
            found
                .iter()
                .zip(want)
                .all(|(&found, want)| close_to(found, want)),
            "timeline frame {frame} samples {found:?}, not {want:?}"
        );
    }
}

fn refusal(clips: Vec<Clip>, output: &Path, options: &EncodeRunOptions) -> String {
    let timeline = Timeline {
        reel_starts: Vec::new(),
        ..timeline(clips)
    };
    run_encode_timeline(
        &timeline,
        output,
        options,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .err()
    .expect("the timeline has to be refused")
}

#[test]
fn an_overlay_in_another_source_colour_or_a_gap_with_no_raster_is_refused_by_name() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 1);
    let half_red = half_red_stills(directory.path(), 8);
    let mut display_rgb_overlay = layered(&half_red, 4, 8, 1, 1.0);
    display_rgb_overlay.source_colour = SourceColour::default();
    let options = EncodeRunOptions {
        picture: fitted_to_the_raster(),
        ..at_the_timeline_rate()
    };
    let refused = refusal(
        vec![layered(&blue, 0, 24, 0, 1.0), display_rgb_overlay],
        &directory.path().join("colour"),
        &options,
    );
    assert_eq!(
        refused,
        format!(
            "clip 2 ({}) has the source colour DisplayRgb and clip 1 ({}) under it has \
             KeepRgb: a composite takes one source colour",
            half_red.display(),
            blue.display()
        )
    );

    let refused = refusal(
        vec![layered(&blue, 6, 24, 0, 1.0)],
        &directory.path().join("gap"),
        &at_the_timeline_rate(),
    );
    assert_eq!(
        refused,
        "layer 0 is empty from frame 0 to frame 6, and the black there has no raster: the \
         picture options need a fit"
    );
    assert!(
        !directory.path().join("colour").exists() && !directory.path().join("gap").exists(),
        "a refusal comes before anything is encoded"
    );
}

#[test]
fn an_overlay_over_jpeg_2000_or_jpeg_2000_above_layer_0_is_refused_by_name() {
    let directory = tempfile::tempdir().unwrap();
    let codestreams = directory.path().join("codestreams");
    std::fs::create_dir_all(&codestreams).unwrap();
    for frame in numbered_codestreams(8) {
        std::fs::write(codestreams.join(frame), [0u8]).unwrap();
    }
    let half_red = half_red_stills(directory.path(), 8);
    let options = EncodeRunOptions {
        picture: fitted_to_the_raster(),
        ..at_the_timeline_rate()
    };
    let refused = refusal(
        vec![
            layered(&codestreams, 0, 8, 0, 1.0),
            layered(&half_red, 2, 4, 1, 1.0),
        ],
        &directory.path().join("over"),
        &options,
    );
    assert_eq!(
        refused,
        format!(
            "clip 2 ({}) lies over clip 1 ({}), which is already JPEG 2000 and cannot be \
             composited",
            half_red.display(),
            codestreams.display()
        )
    );

    let refused = refusal(
        vec![
            layered(&half_red, 0, 8, 0, 1.0),
            layered(&codestreams, 0, 8, 3, 1.0),
        ],
        &directory.path().join("above"),
        &options,
    );
    assert_eq!(
        refused,
        format!(
            "clip 2 ({}) is on layer 3 and is already JPEG 2000, so it cannot be composited",
            codestreams.display()
        )
    );
}

const IMF_BITS_PER_SECOND: u64 = 200_000_000;

#[test]
fn a_composite_stretch_and_a_black_one_wrap_into_track_files_of_their_own() {
    let directory = tempfile::tempdir().unwrap();
    let timeline = compositing_timeline(directory.path());
    let stretches = timeline_stretches(&timeline, &compositing_options()).unwrap();
    let cuts: Vec<(u64, u64)> = stretches
        .iter()
        .map(|stretch| (stretch.frames.start, stretch.frames.end))
        .collect();
    assert_eq!(
        cuts,
        vec![
            (0, 4),
            (4, 6),
            (6, 12),
            (12, 14),
            (14, 18),
            (18, 20),
            (20, 28),
            (28, 30)
        ]
    );
    let black = &stretches[0];
    assert_eq!(
        black.base,
        StretchBase::Black {
            width: WIDTH,
            height: HEIGHT
        }
    );
    assert!(black.overlays.is_empty());
    let composite = &stretches[2];
    assert_eq!(composite.base, StretchBase::Clip(1));
    assert_eq!(composite.overlays, vec![2]);

    // an IMF track file takes an IMF profile, as imfwizard encodes it
    let levels = imf_levels(WIDTH, HEIGHT, f64::from(RATE), IMF_BITS_PER_SECOND).unwrap();
    let options = EncodeRunOptions {
        rsiz: imf_rsiz(ImfProfile::for_raster(WIDTH, HEIGHT).unwrap(), levels),
        ..compositing_options()
    };
    for (stretch, want) in [
        (black, [BLACK, BLACK, BLACK]),
        (composite, [RED, RED, BLUE]),
    ] {
        let length = stretch.frames.end - stretch.frames.start;
        let output = directory
            .path()
            .join(format!("stretch_{}", stretch.frames.start));
        std::fs::create_dir_all(&output).unwrap();
        let mxf = output.join("picture.mxf");
        let frames_reported = Mutex::new(Vec::new());
        let (encode, track) = run_encode_timeline_stretch_and_wrap_picture(
            &timeline,
            stretch,
            &output,
            &options,
            IncrementalWrapOptions {
                output: mxf.clone(),
                standard: MxfStandard::As02,
                fps_num: RATE,
                fps_den: 1,
                encryption: None,
                hdr: Some(rec709_sdr_picture_colour()),
                asset_uuid: None,
            },
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            |progress: &PipelineProgress| frames_reported.lock().unwrap().push(progress.frame),
            |_: &str| {},
        )
        .expect("the stretch encodes and wraps");
        assert_eq!(encode.frames_encoded, length);
        assert_eq!(track.path, mxf);
        assert_eq!(
            frames_reported.into_inner().unwrap().last(),
            Some(&length),
            "progress counts the stretch's own frames"
        );

        let unwrapped = output.join("unwrapped");
        let info = postkit::mxf_unwrap::unwrap_picture_mxf(
            &mxf,
            &unwrapped,
            &AtomicBool::new(false),
            &mut |_, _| {},
        )
        .expect("the track file reads back");
        assert_eq!(info.frames, length);
        assert_eq!((info.width, info.height), (WIDTH, HEIGHT));
        let names = codestream_names(&unwrapped);
        assert_eq!(names.len() as u64, length);
        for name in names {
            let found = sampled_pixels(&unwrapped.join(&name));
            assert!(
                found
                    .iter()
                    .zip(want)
                    .all(|(&found, want)| close_to(found, want)),
                "{name} of the track file for frames {:?} samples {found:?}, not {want:?}",
                stretch.frames
            );
        }
    }
}

const LIME: [i32; 3] = [0, FULL, 0];

fn lime_card(directory: &Path) -> PathBuf {
    let card = directory.join("lime.png");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=lime:s={WIDTH}x{HEIGHT}"),
        "-frames:v",
        "1",
        "-pix_fmt",
        "rgb24",
        &card.to_string_lossy(),
    ]);
    card
}

const HELD_FRAMES: u64 = 10;
const HOLD_START: u64 = 6;

// blue, the lime card held, then the padded red pillar
fn held_still_timeline(directory: &Path) -> Timeline {
    let blue = blue_video(directory, 1);
    let card = lime_card(directory);
    let red_pillar = red_pillar_stills(directory, 4);
    Timeline {
        reel_starts: Vec::new(),
        ..timeline(vec![
            layered(&blue, 0, HOLD_START, 0, 1.0),
            layered(&card, HOLD_START, HELD_FRAMES, 0, 1.0),
            layered(&red_pillar, HOLD_START + HELD_FRAMES, 4, 0, 1.0),
        ])
    }
}

fn samples_close_to(found: [[i32; 3]; 3], want: [[i32; 3]; 3]) -> bool {
    found
        .iter()
        .zip(want)
        .all(|(&found, want)| close_to(found, want))
}

#[test]
fn a_held_still_encodes_once_and_links_its_codestream_for_the_hold() {
    let directory = tempfile::tempdir().unwrap();
    let timeline = held_still_timeline(directory.path());
    let total_frames = HOLD_START + HELD_FRAMES + 4;

    let (result, frames_reported) = encode(
        &timeline,
        &directory.path().join("out"),
        &compositing_options(),
    );
    assert_eq!(result.frames_encoded, total_frames);
    let names = codestream_names(&result.j2k_dir);
    assert_eq!(names, numbered_codestreams(total_frames));
    assert_eq!(frames_reported.last(), Some(&total_frames));

    let hold = HOLD_START..HOLD_START + HELD_FRAMES;
    for (frame, name) in names.iter().enumerate() {
        let frame = frame as u64;
        let want = match frame {
            _ if frame < hold.start => [BLUE, BLUE, BLUE],
            _ if hold.contains(&frame) => [LIME, LIME, LIME],
            _ => [BLACK, RED, BLACK],
        };
        let found = sampled_pixels(&result.j2k_dir.join(name));
        assert!(
            samples_close_to(found, want),
            "timeline frame {frame} samples {found:?}, not {want:?}"
        );
    }

    // emptying the first held codestream empties the rest of the hold only if they are one file
    let codestream = |frame: u64| result.j2k_dir.join(format!("frame_{frame:08}.j2c"));
    std::fs::File::create(codestream(hold.start)).unwrap();
    for frame in hold.start + 1..hold.end {
        assert!(
            std::fs::read(codestream(frame)).unwrap().is_empty(),
            "frame {frame} is a link to the first frame of the hold"
        );
    }
    assert!(!std::fs::read(codestream(hold.end)).unwrap().is_empty());
}

#[test]
fn a_held_still_stretch_wraps_into_a_track_file_of_its_hold() {
    let directory = tempfile::tempdir().unwrap();
    let timeline = held_still_timeline(directory.path());
    let stretches = timeline_stretches(&timeline, &compositing_options()).unwrap();
    let stretch = &stretches[1];
    assert_eq!(stretch.frames, HOLD_START..HOLD_START + HELD_FRAMES);
    assert_eq!(stretch.base, StretchBase::Clip(1));

    let levels = imf_levels(WIDTH, HEIGHT, f64::from(RATE), IMF_BITS_PER_SECOND).unwrap();
    let options = EncodeRunOptions {
        rsiz: imf_rsiz(ImfProfile::for_raster(WIDTH, HEIGHT).unwrap(), levels),
        ..compositing_options()
    };
    let output = directory.path().join("stretch");
    std::fs::create_dir_all(&output).unwrap();
    let mxf = output.join("picture.mxf");
    let frames_reported = Mutex::new(Vec::new());
    let (encode, track) = run_encode_timeline_stretch_and_wrap_picture(
        &timeline,
        stretch,
        &output,
        &options,
        IncrementalWrapOptions {
            output: mxf.clone(),
            standard: MxfStandard::As02,
            fps_num: RATE,
            fps_den: 1,
            encryption: None,
            hdr: Some(rec709_sdr_picture_colour()),
            asset_uuid: None,
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |progress: &PipelineProgress| frames_reported.lock().unwrap().push(progress.frame),
        |_: &str| {},
    )
    .expect("the held still encodes and wraps");
    assert_eq!(encode.frames_encoded, HELD_FRAMES);
    assert_eq!(
        (track.path.clone(), track.duration),
        (mxf.clone(), HELD_FRAMES)
    );
    assert_eq!(
        frames_reported.into_inner().unwrap().last(),
        Some(&HELD_FRAMES)
    );

    let unwrapped = output.join("unwrapped");
    let info = postkit::mxf_unwrap::unwrap_picture_mxf(
        &mxf,
        &unwrapped,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .expect("the track file reads back");
    assert_eq!(info.frames, HELD_FRAMES);
    let names = codestream_names(&unwrapped);
    assert_eq!(names.len() as u64, HELD_FRAMES);
    for name in names {
        let found = sampled_pixels(&unwrapped.join(&name));
        assert!(
            samples_close_to(found, [LIME, LIME, LIME]),
            "{name} samples {found:?}"
        );
    }
}

#[test]
fn a_still_composites_as_an_overlay_with_alpha_and_as_a_base() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 1);
    let card = lime_card(directory.path());
    let half_red = half_red_stills(directory.path(), 8);
    let half_red_still = half_red.join("frame_0001.png");
    let timeline = Timeline {
        reel_starts: Vec::new(),
        ..timeline(vec![
            layered(&blue, 0, 8, 0, 1.0),
            layered(&half_red_still, 2, 4, 1, 1.0),
            layered(&card, 8, 8, 0, 1.0),
            layered(&half_red, 10, 4, 1, 1.0),
        ])
    };

    let (result, _) = encode(
        &timeline,
        &directory.path().join("out"),
        &compositing_options(),
    );
    let names = codestream_names(&result.j2k_dir);
    assert_eq!(names, numbered_codestreams(16));
    let expected = |frame: u64| match frame {
        2..6 => [RED, RED, BLUE],
        0..8 => [BLUE, BLUE, BLUE],
        10..14 => [RED, RED, LIME],
        _ => [LIME, LIME, LIME],
    };
    for (frame, name) in names.iter().enumerate() {
        let found = sampled_pixels(&result.j2k_dir.join(name));
        let want = expected(frame as u64);
        assert!(
            samples_close_to(found, want),
            "timeline frame {frame} samples {found:?}, not {want:?}"
        );
    }
}

fn black_card(directory: &Path) -> PathBuf {
    let card = directory.join("black.png");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=black:s={WIDTH}x{HEIGHT}"),
        "-frames:v",
        "1",
        "-pix_fmt",
        "rgb24",
        &card.to_string_lossy(),
    ]);
    card
}

const THREE_SECONDS: u64 = 3 * RATE as u64;

fn still_held_three_seconds(directory: &Path, still: &Path) -> Timeline {
    let blue = blue_video(directory, 1);
    let red_pillar = red_pillar_stills(directory, 4);
    Timeline {
        reel_starts: Vec::new(),
        ..timeline(vec![
            layered(&blue, 0, HOLD_START, 0, 1.0),
            layered(still, HOLD_START, THREE_SECONDS, 0, 1.0),
            layered(&red_pillar, HOLD_START + THREE_SECONDS, 4, 0, 1.0),
        ])
    }
}

#[test]
fn a_black_held_still_reports_black_over_its_hold_and_a_lime_one_reports_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let detecting = EncodeRunOptions {
        detect_picture_findings: true,
        ..compositing_options()
    };

    let black = black_card(directory.path());
    let (result, _) = encode(
        &still_held_three_seconds(directory.path(), &black),
        &directory.path().join("black_out"),
        &detecting,
    );
    assert_eq!(
        result.picture_findings,
        PictureFindings {
            black: vec![FrameInterval {
                first_frame: HOLD_START,
                last_frame: HOLD_START + THREE_SECONDS - 1,
            }],
            frozen: Vec::new(),
        }
    );

    let lime = lime_card(directory.path());
    let (result, _) = encode(
        &still_held_three_seconds(directory.path(), &lime),
        &directory.path().join("lime_out"),
        &detecting,
    );
    assert_eq!(result.picture_findings, PictureFindings::default());
}
