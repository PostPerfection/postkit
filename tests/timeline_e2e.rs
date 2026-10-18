use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use postkit::encode::{FrameRate, SourceColour};
use postkit::picture_processing::PictureProcessing;
use postkit::pipeline::{EncodeRunOptions, PipelineProgress, run_encode_timeline};
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

fn encode(timeline: &Timeline, output: &Path) -> (postkit::pipeline::EncodeResult, Vec<u64>) {
    let frames_reported = Mutex::new(Vec::new());
    let result = run_encode_timeline(
        timeline,
        output,
        &EncodeRunOptions {
            fps: FrameRate::whole(RATE),
            ..Default::default()
        },
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

    let (result, frames_reported) = encode(&timeline, &directory.path().join("out"));
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
    let (linked, _) = encode(&followed_by_codestreams, &directory.path().join("linked"));
    assert_eq!(
        codestream_names(&linked.j2k_dir),
        numbered_codestreams(24 + total_frames)
    );
    assert_eq!(
        std::fs::read(linked.j2k_dir.join("frame_00000024.j2c")).unwrap(),
        std::fs::read(result.j2k_dir.join("frame_00000000.j2c")).unwrap(),
        "the codestreams follow on at the clip's place"
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
