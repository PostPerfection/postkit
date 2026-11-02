use super::*;

fn stream(index: usize, channels: u16, default: bool) -> AudioStream {
    AudioStream {
        index,
        channels,
        default,
    }
}

#[test]
fn several_mono_tracks_join_in_track_order() {
    let tracks: Vec<_> = (1..=6).map(|index| stream(index, 1, index == 1)).collect();
    assert_eq!(
        sound_of_streams(&tracks),
        Some(EmbeddedSound {
            stream_indexes: vec![1, 2, 3, 4, 5, 6],
            channels: 6,
        })
    );
}

#[test]
fn mixed_tracks_take_the_one_ffmpeg_takes() {
    let stereo_then_five_one = [stream(1, 2, false), stream(2, 6, false)];
    assert_eq!(
        sound_of_streams(&stereo_then_five_one)
            .unwrap()
            .stream_indexes,
        [2]
    );

    let default_stereo = [stream(1, 2, true), stream(2, 6, false)];
    assert_eq!(
        sound_of_streams(&default_stereo).unwrap().stream_indexes,
        [1]
    );

    let two_stereo = [stream(1, 2, false), stream(2, 2, false)];
    assert_eq!(sound_of_streams(&two_stereo).unwrap().stream_indexes, [1]);

    let one_mono = [stream(1, 1, true)];
    assert_eq!(sound_of_streams(&one_mono).unwrap().channels, 1);

    assert_eq!(sound_of_streams(&[]), None);
}

const TRACK_LEVELS: [f32; 3] = [0.1, 0.2, 0.3];

#[test]
fn a_video_with_mono_tracks_gives_one_sound_with_a_channel_per_track() {
    let dir = tempfile::tempdir().unwrap();
    let picture = dir.path().join("mono_tracks.mov");
    let mut make = Command::new("ffmpeg");
    make.args(["-v", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=64x48:rate=24:duration=1");
    for level in TRACK_LEVELS {
        make.args(["-f", "lavfi", "-i"])
            .arg(format!("aevalsrc={level}:sample_rate=48000:duration=1"));
    }
    for input in 0..=TRACK_LEVELS.len() {
        make.args(["-map", &input.to_string()]);
    }
    make.args(["-c:v", "libx264", "-c:a", "pcm_s24le"])
        .arg(&picture);
    assert!(
        make.status().unwrap().success(),
        "ffmpeg did not write the fixture"
    );

    let sound = embedded_sound(&picture)
        .unwrap()
        .expect("the video carries sound");
    assert_eq!(sound.channels, TRACK_LEVELS.len() as u16);
    let wav = dir.path().join("sound.wav");
    let extracted = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&picture)
        .args(sound.ffmpeg_arguments("aresample=48000"))
        .args(["-c:a", "pcm_s24le"])
        .arg(&wav)
        .status()
        .unwrap();
    assert!(extracted.success(), "ffmpeg did not take the sound");

    let mut reader = hound::WavReader::open(&wav).unwrap();
    assert_eq!(reader.spec().channels, TRACK_LEVELS.len() as u16);
    let full_scale = (1 << 23) as f32;
    let first_frame: Vec<f32> = reader
        .samples::<i32>()
        .take(TRACK_LEVELS.len())
        .map(|sample| sample.unwrap() as f32 / full_scale)
        .collect();
    for (channel, level) in TRACK_LEVELS.iter().enumerate() {
        assert!(
            (first_frame[channel] - level).abs() < 0.001,
            "channel {channel} is {} where its track is {level}",
            first_frame[channel]
        );
    }
}
