use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use postkit::composited_preview::{
    PreviewRenderProgress, preview_fit, render_grok_preview_stretches, render_mpv_preview_stretches,
};
use postkit::encode::{FrameRate, SourceColour};
use postkit::j2k::{ImfProfile, imf_levels, imf_rsiz};
use postkit::mxf_wrap::{IncrementalWrapOptions, MxfStandard, rec709_sdr_picture_colour};
use postkit::picture_findings::{FrameInterval, PictureFindings};
use postkit::picture_processing::{Fit, PictureProcessing, Placement};
use postkit::pipeline::{
    EncodeRunOptions, PipelineProgress, StretchBase, run_encode_timeline,
    run_encode_timeline_stretch_and_wrap_picture, run_encode_with_options, timeline_stretches,
};
use postkit::still::{StillHold, build_still_frames};
use postkit::subtitle_retime::parse_srt;
use postkit::timeline::{
    Clip, ClipSound, RenderedStretch, SoundTrack, SourceLength, TIMELINE_FORMAT_VERSION, Timeline,
    join_subtitles, mpv_edl_uri, probe_source, render_sound,
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

// transparent white over the left quarter, white at half alpha over the next and opaque lime over the right half
const ALPHA_CARD_FILTER: &str = "format=rgba,geq=r='if(lt(X,W/2),255,0)':g=255:\
    b='if(lt(X,W/2),255,0)':a='if(lt(X,W/4),0,if(lt(X,W/2),128,255))'";
const HALF_WHITE: [i32; 3] = [32768, 32768, 32768];
const ALPHA_CARD_OVER_BLACK: [[i32; 3]; 3] = [BLACK, HALF_WHITE, LIME];
const ALPHA_FRAMES: u64 = 8;

fn alpha_card(directory: &Path) -> PathBuf {
    let card = directory.join("alpha_card.png");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=white:s={WIDTH}x{HEIGHT}"),
        "-vf",
        ALPHA_CARD_FILTER,
        "-frames:v",
        "1",
        "-pix_fmt",
        "rgba",
        &card.to_string_lossy(),
    ]);
    card
}

fn alpha_movie(directory: &Path) -> PathBuf {
    let movie = directory.join("alpha_card.mov");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=white:s={WIDTH}x{HEIGHT}:rate={RATE}"),
        "-vf",
        ALPHA_CARD_FILTER,
        "-frames:v",
        &ALPHA_FRAMES.to_string(),
        "-c:v",
        "prores_ks",
        "-profile:v",
        "4444",
        "-pix_fmt",
        "yuva444p10le",
        &movie.to_string_lossy(),
    ]);
    movie
}

fn alpha_tiffs(directory: &Path) -> PathBuf {
    let stills = directory.join("alpha_tiffs");
    std::fs::create_dir_all(&stills).unwrap();
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=white:s={WIDTH}x{HEIGHT}:rate={RATE}"),
        "-vf",
        ALPHA_CARD_FILTER,
        "-frames:v",
        "2",
        "-pix_fmt",
        "rgba",
        &stills.join("frame_%04d.tif").to_string_lossy(),
    ]);
    stills
}

// red over the right quarter, transparent elsewhere
fn red_quarter_card(directory: &Path) -> PathBuf {
    let card = directory.join("red_quarter.png");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=red:s={}x{HEIGHT}", WIDTH / 4),
        "-vf",
        &format!(
            "format=rgba,pad={WIDTH}:{HEIGHT}:{}:0:color=black@0",
            WIDTH * 3 / 4
        ),
        "-frames:v",
        "1",
        "-pix_fmt",
        "rgba",
        &card.to_string_lossy(),
    ]);
    card
}

fn assert_every_codestream_samples(directory: &Path, want: [[i32; 3]; 3], what: &str) {
    let names = codestream_names(directory);
    assert!(!names.is_empty(), "{what} encoded nothing");
    for name in names {
        let found = sampled_pixels(&directory.join(&name));
        assert!(
            samples_close_to(found, want),
            "{what} {name} samples {found:?}, not {want:?}"
        );
    }
}

#[test]
fn a_single_picture_with_alpha_encodes_over_black() {
    let directory = tempfile::tempdir().unwrap();
    let card = alpha_card(directory.path());
    let held = directory.path().join("held");
    build_still_frames(&StillHold {
        image: &card,
        frames: 2,
        fps: FrameRate::whole(RATE),
        width: WIDTH,
        height: HEIGHT,
        filters: &[],
        apply_xyz_transform: false,
        rsiz: postkit::encode::default_rsiz(),
        encode_threads: 0,
        colour_transform: None,
        burn: None,
        watermark: None,
        out_dir: &held,
    })
    .expect("the card holds");
    assert_every_codestream_samples(&held, ALPHA_CARD_OVER_BLACK, "the held card");

    let options = EncodeRunOptions {
        source_colour: SourceColour::KeepRgb,
        ..at_the_timeline_rate()
    };
    for (picture, what) in [
        (alpha_movie(directory.path()), "the ProRes 4444 movie"),
        (alpha_tiffs(directory.path()), "the RGBA TIFF sequence"),
    ] {
        let result = run_encode_with_options(
            &picture,
            &picture.with_extension("out"),
            &options,
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            |_: &PipelineProgress| {},
            |_: &str| {},
        )
        .unwrap_or_else(|error| panic!("{what} encodes: {error}"));
        assert_every_codestream_samples(&result.j2k_dir, ALPHA_CARD_OVER_BLACK, what);
    }
}

#[test]
fn layer_0_pictures_with_alpha_go_over_black_alone_and_under_an_overlay() {
    let directory = tempfile::tempdir().unwrap();
    let movie = alpha_movie(directory.path());
    let card = alpha_card(directory.path());
    let red_quarter = red_quarter_card(directory.path());
    let timeline = Timeline {
        reel_starts: Vec::new(),
        ..timeline(vec![
            layered(&movie, 0, ALPHA_FRAMES, 0, 1.0),
            layered(&card, ALPHA_FRAMES, ALPHA_FRAMES, 0, 1.0),
            layered(&movie, 2 * ALPHA_FRAMES, ALPHA_FRAMES, 0, 1.0),
            layered(&card, 3 * ALPHA_FRAMES, ALPHA_FRAMES, 0, 1.0),
            layered(&red_quarter, 2 * ALPHA_FRAMES, 2 * ALPHA_FRAMES, 1, 1.0),
        ])
    };

    let (result, _) = encode(
        &timeline,
        &directory.path().join("out"),
        &compositing_options(),
    );
    let names = codestream_names(&result.j2k_dir);
    assert_eq!(names, numbered_codestreams(4 * ALPHA_FRAMES));
    let under_the_overlay = [BLACK, HALF_WHITE, RED];
    for (frame, name) in names.iter().enumerate() {
        let want = if (frame as u64) < 2 * ALPHA_FRAMES {
            ALPHA_CARD_OVER_BLACK
        } else {
            under_the_overlay
        };
        let found = sampled_pixels(&result.j2k_dir.join(name));
        assert!(
            samples_close_to(found, want),
            "timeline frame {frame} samples {found:?}, not {want:?}"
        );
    }
}

fn preview_timeline(clips: Vec<Clip>) -> Timeline {
    Timeline {
        reel_starts: Vec::new(),
        ..timeline(clips)
    }
}

fn not_cancelled() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn render_for_mpv(
    timeline: &Timeline,
    options: &EncodeRunOptions,
    cache: &Path,
) -> (Vec<RenderedStretch>, Vec<PreviewRenderProgress>) {
    let progress = Mutex::new(Vec::new());
    let rendered = render_mpv_preview_stretches(
        timeline,
        options,
        cache,
        &not_cancelled(),
        |reported: PreviewRenderProgress| progress.lock().unwrap().push(reported),
    )
    .expect("the preview renders");
    (rendered, progress.into_inner().unwrap())
}

const PREVIEW_SOUND: &str = "/work/sound.wav";
const PREVIEW_BLACK: &str = "/work/black.mkv";
const EDL_SOUND_STREAM: &str = "!new_stream;!no_clip;";

// each picture segment as its path, first frame and frame count, then the sound segment
fn edl_segments(uri: &str) -> (Vec<(PathBuf, u64, u64)>, String) {
    let to_frames =
        |seconds: &str| (seconds.parse::<f64>().unwrap() * f64::from(RATE)).round() as u64;
    let mut rest = uri.strip_prefix("edl://").unwrap();
    let mut pictures = Vec::new();
    loop {
        if let Some(sound) = rest.strip_prefix(EDL_SOUND_STREAM) {
            return (pictures, sound.to_string());
        }
        let (length, after) = rest.strip_prefix('%').unwrap().split_once('%').unwrap();
        let (path, after) = after.split_at(length.parse().unwrap());
        let (trim, after) = after.split_once(';').unwrap();
        let (start, frames) = trim.strip_prefix(',').unwrap().split_once(',').unwrap();
        pictures.push((PathBuf::from(path), to_frames(start), to_frames(frames)));
        rest = after;
    }
}

#[derive(Debug, PartialEq)]
enum Played {
    Blue,
    Black,
    Rendered,
}

fn rendered_frame_count(path: &Path) -> u64 {
    match probe_source(path).unwrap() {
        SourceLength::Counted { frames, .. } => frames,
        SourceLength::Still => panic!("{} is a movie", path.display()),
    }
}

// renders the timeline for mpv and checks its EDL against `expected`, one entry a picture segment
fn assert_stretch_edl(directory: &Path, clips: Vec<Clip>, expected: &[(Played, u64, u64)]) {
    let timeline = preview_timeline(clips);
    let (rendered, _) = render_for_mpv(&timeline, &compositing_options(), &directory.join("cache"));
    let uri = mpv_edl_uri(
        &timeline,
        Some(Path::new(PREVIEW_SOUND)),
        Some(Path::new(PREVIEW_BLACK)),
        &rendered,
    )
    .unwrap();
    let (pictures, sound) = edl_segments(&uri);
    let played: Vec<(Played, u64, u64)> = pictures
        .iter()
        .map(|(path, first_frame, frames)| {
            let what = if path == Path::new(PREVIEW_BLACK) {
                Played::Black
            } else if let Some(stretch) = rendered.iter().find(|stretch| &stretch.path == path) {
                assert_eq!(stretch.frames.end - stretch.frames.start, *frames);
                assert_eq!(rendered_frame_count(path), *frames, "{}", path.display());
                Played::Rendered
            } else {
                assert_eq!(path.file_name().unwrap(), "blue.mkv");
                Played::Blue
            };
            (what, *first_frame, *frames)
        })
        .collect();
    assert_eq!(played, expected, "{uri}");
    let played_frames: u64 = pictures.iter().map(|(_, _, frames)| frames).sum();
    assert_eq!(played_frames, timeline.total_frames());
    let unrendered = mpv_edl_uri(
        &timeline,
        Some(Path::new(PREVIEW_SOUND)),
        Some(Path::new(PREVIEW_BLACK)),
        &[],
    )
    .unwrap();
    assert_eq!(sound, edl_segments(&unrendered).1, "the sound segment");
}

fn trimmed(picture: &Path, start: u64, in_frame: u64, frames: u64) -> Clip {
    Clip {
        in_frame,
        out_frame: in_frame + frames,
        ..layered(picture, start, frames, 0, 1.0)
    }
}

#[test]
fn an_overlay_inside_one_base_clip_splits_it_in_three() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let half_red = half_red_stills(directory.path(), 8);
    assert_stretch_edl(
        directory.path(),
        vec![trimmed(&blue, 0, 5, 48), layered(&half_red, 12, 8, 1, 1.0)],
        &[
            (Played::Blue, 5, 12),
            (Played::Rendered, 0, 8),
            (Played::Blue, 25, 28),
        ],
    );
}

#[test]
fn an_overlay_across_two_base_clips_renders_a_stretch_on_each_side_of_the_cut() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let half_red = half_red_stills(directory.path(), 16);
    assert_stretch_edl(
        directory.path(),
        vec![
            trimmed(&blue, 0, 0, 24),
            trimmed(&blue, 24, 10, 24),
            layered(&half_red, 16, 16, 1, 1.0),
        ],
        &[
            (Played::Blue, 0, 16),
            (Played::Rendered, 0, 8),
            (Played::Rendered, 0, 8),
            (Played::Blue, 18, 16),
        ],
    );
}

#[test]
fn an_overlay_over_a_gap_leaves_black_on_both_sides_of_it() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let half_red = half_red_stills(directory.path(), 6);
    assert_stretch_edl(
        directory.path(),
        vec![
            trimmed(&blue, 0, 0, 12),
            trimmed(&blue, 24, 30, 12),
            layered(&half_red, 14, 6, 1, 1.0),
        ],
        &[
            (Played::Blue, 0, 12),
            (Played::Black, 0, 2),
            (Played::Rendered, 0, 6),
            (Played::Black, 0, 4),
            (Played::Blue, 30, 12),
        ],
    );
}

#[test]
fn an_overlay_at_frame_0_plays_first() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let half_red = half_red_stills(directory.path(), 8);
    assert_stretch_edl(
        directory.path(),
        vec![trimmed(&blue, 0, 3, 24), layered(&half_red, 0, 8, 1, 1.0)],
        &[(Played::Rendered, 0, 8), (Played::Blue, 11, 16)],
    );
}

#[test]
fn an_overlay_running_past_layer_0_ends_the_timeline_over_black() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let half_red = half_red_stills(directory.path(), 14);
    assert_stretch_edl(
        directory.path(),
        vec![trimmed(&blue, 0, 0, 24), layered(&half_red, 16, 14, 1, 1.0)],
        &[
            (Played::Blue, 0, 16),
            (Played::Rendered, 0, 8),
            (Played::Rendered, 0, 6),
        ],
    );
}

#[test]
fn staggered_overlays_on_layers_1_and_2_render_each_change_of_layers() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let half_red = half_red_stills(directory.path(), 12);
    assert_stretch_edl(
        directory.path(),
        vec![
            trimmed(&blue, 0, 0, 32),
            layered(&half_red, 8, 12, 1, 1.0),
            layered(&half_red, 12, 12, 2, 0.5),
        ],
        &[
            (Played::Blue, 0, 8),
            (Played::Rendered, 0, 4),
            (Played::Rendered, 0, 8),
            (Played::Rendered, 0, 4),
            (Played::Blue, 24, 8),
        ],
    );
}

#[test]
fn two_adjacent_overlays_on_one_layer_render_a_stretch_each() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let half_red = half_red_stills(directory.path(), 4);
    let red_pillar = red_pillar_stills(directory.path(), 4);
    assert_stretch_edl(
        directory.path(),
        vec![
            trimmed(&blue, 0, 0, 24),
            layered(&half_red, 8, 4, 1, 1.0),
            layered(&red_pillar, 12, 4, 1, 1.0),
        ],
        &[
            (Played::Blue, 0, 8),
            (Played::Rendered, 0, 4),
            (Played::Rendered, 0, 4),
            (Played::Blue, 16, 8),
        ],
    );
}

#[test]
fn a_held_still_overlay_renders_its_hold() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let card = alpha_card(directory.path());
    assert_stretch_edl(
        directory.path(),
        vec![trimmed(&blue, 0, 0, 24), layered(&card, 4, 8, 1, 1.0)],
        &[
            (Played::Blue, 0, 4),
            (Played::Rendered, 0, 8),
            (Played::Blue, 12, 12),
        ],
    );
}

#[test]
fn a_held_still_on_layer_0_renders_with_nothing_above_it() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let card = lime_card(directory.path());
    assert_stretch_edl(
        directory.path(),
        vec![layered(&card, 0, 10, 0, 1.0), trimmed(&blue, 10, 0, 10)],
        &[(Played::Rendered, 0, 10), (Played::Blue, 0, 10)],
    );
}

#[test]
fn an_image_sequence_on_layer_0_renders_with_nothing_above_it() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 4);
    let red_pillar = red_pillar_stills(directory.path(), 8);
    assert_stretch_edl(
        directory.path(),
        vec![
            trimmed(&blue, 0, 0, 6),
            layered(&red_pillar, 6, 8, 0, 1.0),
            trimmed(&blue, 14, 20, 6),
        ],
        &[
            (Played::Blue, 0, 6),
            (Played::Rendered, 0, 8),
            (Played::Blue, 20, 6),
        ],
    );
}

fn preview_refusals(timeline: &Timeline, options: &EncodeRunOptions, cache: &Path) -> String {
    let mpv = render_mpv_preview_stretches(timeline, options, cache, &not_cancelled(), |_| {})
        .expect_err("the mpv preview has to be refused");
    let grok = render_grok_preview_stretches(timeline, options, cache, &not_cancelled(), |_| {})
        .expect_err("the grok preview has to be refused");
    assert_eq!(mpv, grok);
    mpv
}

#[test]
fn the_preview_refuses_what_the_build_refuses_with_its_message() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 1);
    let half_red = half_red_stills(directory.path(), 8);
    let codestreams = directory.path().join("codestreams");
    std::fs::create_dir_all(&codestreams).unwrap();
    for frame in numbered_codestreams(8) {
        std::fs::write(codestreams.join(frame), [0u8]).unwrap();
    }
    let mut display_rgb_overlay = layered(&half_red, 4, 8, 1, 1.0);
    display_rgb_overlay.source_colour = SourceColour::default();
    let fitted = EncodeRunOptions {
        picture: fitted_to_the_raster(),
        ..at_the_timeline_rate()
    };
    let refusals = [
        (
            vec![
                layered(&codestreams, 0, 8, 0, 1.0),
                layered(&half_red, 2, 4, 1, 1.0),
            ],
            fitted.clone(),
            format!(
                "clip 2 ({}) lies over clip 1 ({}), which is already JPEG 2000 and cannot be \
                 composited",
                half_red.display(),
                codestreams.display()
            ),
        ),
        (
            vec![
                layered(&half_red, 0, 8, 0, 1.0),
                layered(&codestreams, 0, 8, 3, 1.0),
            ],
            fitted.clone(),
            format!(
                "clip 2 ({}) is on layer 3 and is already JPEG 2000, so it cannot be composited",
                codestreams.display()
            ),
        ),
        (
            vec![layered(&blue, 0, 24, 0, 1.0), display_rgb_overlay],
            fitted,
            format!(
                "clip 2 ({}) has the source colour DisplayRgb and clip 1 ({}) under it has \
                 KeepRgb: a composite takes one source colour",
                half_red.display(),
                blue.display()
            ),
        ),
        (
            vec![
                layered(&blue, 6, 24, 0, 1.0),
                layered(&half_red, 8, 4, 1, 1.0),
            ],
            at_the_timeline_rate(),
            "layer 0 is empty from frame 0 to frame 6, and the black there has no raster: the \
             picture options need a fit"
                .to_string(),
        ),
    ];
    let cache = directory.path().join("cache");
    for (clips, options, message) in refusals {
        let refused = preview_refusals(&preview_timeline(clips.clone()), &options, &cache);
        assert_eq!(refused, message);
        assert_eq!(
            refusal(clips, &directory.path().join("build"), &options),
            message
        );
    }
    assert!(
        !cache.exists(),
        "a refusal comes before anything is rendered"
    );
}

fn cache_entries(cache: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(cache)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn rendered_frames(progress: &[PreviewRenderProgress]) -> u64 {
    let last = progress.last().expect("something was rendered");
    assert_eq!(last.frames_rendered, last.total_frames);
    last.total_frames
}

fn path_of(rendered: &[RenderedStretch], frames: std::ops::Range<u64>) -> PathBuf {
    rendered
        .iter()
        .find(|stretch| stretch.frames == frames)
        .unwrap_or_else(|| panic!("no stretch over {frames:?} in {rendered:?}"))
        .path
        .clone()
}

// a blue base with a card over frames 8 to 16 and the half red over 24 to 32
fn two_overlay_clips(directory: &Path) -> Vec<Clip> {
    let blue = blue_video(directory, 2);
    let card = alpha_card(directory);
    let half_red = half_red_stills(directory, 8);
    vec![
        trimmed(&blue, 0, 0, 40),
        layered(&card, 8, 8, 1, 1.0),
        layered(&half_red, 24, 8, 1, 1.0),
    ]
}

#[test]
fn a_second_render_of_the_same_timeline_renders_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let timeline = preview_timeline(two_overlay_clips(directory.path()));
    let cache = directory.path().join("cache");
    let (first, progress) = render_for_mpv(&timeline, &compositing_options(), &cache);
    assert_eq!(rendered_frames(&progress), 16);
    let (second, progress) = render_for_mpv(&timeline, &compositing_options(), &cache);
    assert_eq!(second, first);
    assert!(progress.is_empty(), "{progress:?}");
    assert_eq!(cache_entries(&cache).len(), 2);
}

#[test]
fn changing_one_overlay_renders_only_its_stretches_again() {
    let directory = tempfile::tempdir().unwrap();
    let clips = two_overlay_clips(directory.path());
    let cache = directory.path().join("cache");
    let (first, _) = render_for_mpv(
        &preview_timeline(clips.clone()),
        &compositing_options(),
        &cache,
    );
    let card_stretch = path_of(&first, 8..16);
    let half_red_stretch = path_of(&first, 24..32);

    let mut faded = clips.clone();
    faded[1].opacity = 0.5;
    let (rendered, progress) =
        render_for_mpv(&preview_timeline(faded), &compositing_options(), &cache);
    assert_eq!(rendered_frames(&progress), 8, "the opacity");
    assert_ne!(path_of(&rendered, 8..16), card_stretch);
    assert_eq!(path_of(&rendered, 24..32), half_red_stretch);

    let mut moved = clips.clone();
    moved[1].start = 4;
    let (rendered, progress) =
        render_for_mpv(&preview_timeline(moved), &compositing_options(), &cache);
    assert_eq!(rendered_frames(&progress), 8, "the start");
    assert_eq!(path_of(&rendered, 24..32), half_red_stretch);

    let lime = lime_card(directory.path());
    std::fs::copy(&lime, &clips[1].picture).unwrap();
    let (rendered, progress) =
        render_for_mpv(&preview_timeline(clips), &compositing_options(), &cache);
    assert_eq!(rendered_frames(&progress), 8, "the file");
    assert_ne!(path_of(&rendered, 8..16), card_stretch);
    assert_eq!(path_of(&rendered, 24..32), half_red_stretch);
}

#[test]
fn a_cancelled_render_keeps_the_stretches_it_finished_and_nothing_part_written() {
    let directory = tempfile::tempdir().unwrap();
    let timeline = preview_timeline(two_overlay_clips(directory.path()));
    let cache = directory.path().join("cache");
    let cancel = not_cancelled();
    let cancelled = render_mpv_preview_stretches(
        &timeline,
        &compositing_options(),
        &cache,
        &cancel,
        |progress: PreviewRenderProgress| {
            if progress.frames_rendered == 10 {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        },
    );
    assert_eq!(cancelled, Err("Cancelled".to_string()));
    let left = cache_entries(&cache);
    assert_eq!(left.len(), 1, "{left:?}");

    let (rendered, progress) = render_for_mpv(&timeline, &compositing_options(), &cache);
    assert_eq!(rendered_frames(&progress), 8);
    assert_eq!(
        path_of(&rendered, 8..16)
            .file_name()
            .unwrap()
            .to_string_lossy(),
        left[0]
    );
    assert_eq!(rendered_frame_count(&path_of(&rendered, 24..32)), 8);
}

#[test]
fn a_load_deletes_the_renders_the_timeline_no_longer_uses_and_keeps_the_rest() {
    let directory = tempfile::tempdir().unwrap();
    let mut clips = two_overlay_clips(directory.path());
    let cache = directory.path().join("cache");
    let (first, _) = render_for_mpv(
        &preview_timeline(clips.clone()),
        &compositing_options(),
        &cache,
    );
    std::fs::write(cache.join("notes.txt"), "not a render").unwrap();

    clips.remove(1);
    let (rendered, progress) =
        render_for_mpv(&preview_timeline(clips), &compositing_options(), &cache);
    assert!(progress.is_empty(), "{progress:?}");
    assert_eq!(rendered, vec![first[1].clone()]);
    assert!(!first[0].path.exists());
    let mut kept = vec![
        "notes.txt".to_string(),
        first[1]
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    kept.sort();
    assert_eq!(cache_entries(&cache), kept);
}

const DCP_4K_WIDTH: u32 = 4096;
const DCP_4K_HEIGHT: u32 = 2160;
const PREVIEW_WIDTH: u32 = 2048;
const PREVIEW_HEIGHT: u32 = 1080;
// the 16:9 picture inside the halved raster, padded on either side
const PREVIEW_PICTURE_LEFT: u32 = 64;
const PREVIEW_PICTURE_WIDTH: u32 = 1920;

fn fitted_to_4k() -> PictureProcessing {
    PictureProcessing {
        fit: Some(Fit {
            box_width: 3840,
            box_height: DCP_4K_HEIGHT,
            raster_width: DCP_4K_WIDTH,
            raster_height: DCP_4K_HEIGHT,
            placement: Placement::default(),
        }),
        ..PictureProcessing::default()
    }
}

fn options_fitted_to_4k() -> EncodeRunOptions {
    EncodeRunOptions {
        picture: fitted_to_4k(),
        ..compositing_options()
    }
}

// the alpha card over a gap, over the blue base, and over it again at half opacity
fn alpha_card_over_a_gap_and_blue(directory: &Path) -> Timeline {
    let blue = blue_video(directory, 1);
    let card = alpha_card(directory);
    let faded = Clip {
        opacity: 0.5,
        ..layered(&card, 8, 4, 1, 1.0)
    };
    preview_timeline(vec![
        layered(&card, 0, 4, 1, 1.0),
        trimmed(&blue, 4, 0, 8),
        layered(&card, 4, 4, 1, 1.0),
        faded,
    ])
}

fn decoded_rgb48(movie: &Path) -> Vec<u8> {
    let decoded = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(movie)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb48le", "-"])
        .output()
        .expect("ffmpeg");
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    decoded.stdout
}

// the middle row at the centre of each quarter of the picture
fn quarter_samples(frame: &[u8]) -> [[i32; 3]; 4] {
    [1, 3, 5, 7].map(|eighth| {
        let x = PREVIEW_PICTURE_LEFT + PREVIEW_PICTURE_WIDTH * eighth / 8;
        let at = ((PREVIEW_HEIGHT / 2 * PREVIEW_WIDTH + x) * 6) as usize;
        [0, 1, 2].map(|channel| {
            let sample = at + channel * 2;
            i32::from(u16::from_le_bytes([frame[sample], frame[sample + 1]]))
        })
    })
}

const HALF_WHITE_OVER_BLUE: [i32; 3] = [32768, 32768, FULL];
const QUARTER_WHITE_OVER_BLUE: [i32; 3] = [16448, 16448, FULL];
const LIME_AT_HALF_OPACITY_OVER_BLUE: [i32; 3] = [0, 32768, 32767];
// a JPEG at quality 2 holds a flat patch within about one percent
const JPEG_TOLERANCE: i32 = 1 << 11;

#[test]
fn an_mpv_preview_decodes_to_the_composite_at_half_a_4k_raster() {
    let directory = tempfile::tempdir().unwrap();
    let timeline = alpha_card_over_a_gap_and_blue(directory.path());
    let (rendered, _) = render_for_mpv(
        &timeline,
        &options_fitted_to_4k(),
        &directory.path().join("cache"),
    );
    let expected = [
        (0..4, [BLACK, HALF_WHITE, LIME, LIME]),
        (4..8, [BLUE, HALF_WHITE_OVER_BLUE, LIME, LIME]),
        (
            8..12,
            [
                BLUE,
                QUARTER_WHITE_OVER_BLUE,
                LIME_AT_HALF_OPACITY_OVER_BLUE,
                LIME_AT_HALF_OPACITY_OVER_BLUE,
            ],
        ),
    ];
    let frame_bytes = (PREVIEW_WIDTH * PREVIEW_HEIGHT * 6) as usize;
    for (frames, want) in expected {
        let decoded = decoded_rgb48(&path_of(&rendered, frames.clone()));
        assert_eq!(
            decoded.len(),
            frame_bytes * 4,
            "{frames:?} at {PREVIEW_WIDTH}x{PREVIEW_HEIGHT}"
        );
        for frame in decoded.chunks(frame_bytes) {
            let found = quarter_samples(frame);
            let close = found.iter().zip(want).all(|(found, want)| {
                found
                    .iter()
                    .zip(want)
                    .all(|(&found, want)| (found - want).abs() <= JPEG_TOLERANCE)
            });
            assert!(close, "{frames:?} samples {found:?}, not {want:?}");
        }
    }
}

fn halved_to_the_preview(timeline: &Timeline) -> Timeline {
    let mut halved = timeline.clone();
    for clip in &mut halved.clips {
        clip.picture_processing.fit = clip.picture_processing.fit.map(preview_fit);
    }
    halved
}

#[test]
fn a_grok_preview_decodes_to_the_samples_the_build_encodes_for_the_stretch() {
    let directory = tempfile::tempdir().unwrap();
    let blue = blue_video(directory.path(), 1);
    let card = alpha_card(directory.path());
    let timeline = preview_timeline(vec![
        trimmed(&blue, 0, 0, 4),
        layered(&card, 4, 4, 1, 1.0),
        trimmed(&blue, 8, 4, 4),
    ]);
    let options = options_fitted_to_4k();
    let rendered = render_grok_preview_stretches(
        &timeline,
        &options,
        &directory.path().join("cache"),
        &not_cancelled(),
        |_| {},
    )
    .expect("the grok preview renders");
    let stretch = path_of(&rendered, 4..8);

    let halved_options = EncodeRunOptions {
        picture: PictureProcessing {
            fit: options.picture.fit.map(preview_fit),
            ..options.picture.clone()
        },
        ..options
    };
    let (built, _) = encode(
        &halved_to_the_preview(&timeline),
        &directory.path().join("build"),
        &halved_options,
    );
    assert_eq!(codestream_names(&stretch), numbered_codestreams(4));
    for frame in 0..4u64 {
        let decode = |path: PathBuf| {
            postkit::grok_decoder::decode(std::fs::read(&path).unwrap(), 0)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        };
        let preview = decode(stretch.join(format!("frame_{frame:08}.j2c")));
        let build = decode(built.j2k_dir.join(format!("frame_{:08}.j2c", frame + 4)));
        assert_eq!(
            (preview.width, preview.height),
            (PREVIEW_WIDTH, PREVIEW_HEIGHT)
        );
        assert!(
            preview.components == build.components,
            "preview frame {frame} decodes to other samples than the build's"
        );
    }
}
