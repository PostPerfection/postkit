#![cfg(all(target_os = "linux", feature = "libmpv"))]

use postkit::audio_levels::SILENCE_FLOOR_DBFS;
use postkit::mpv_render::MpvRenderPlayer;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LEVELS_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const HALF_SCALE_DBFS: f64 = -6.0206;
const HALF_SCALE_SINE_RMS_DBFS: f64 = -9.0309;
const TOLERANCE_DB: f64 = 0.01;
const FILTER_LABEL: &str = "levelmeter";

// a half scale 1 kHz tone on the front left, silence on the other five
fn five_point_one_tone(directory: &Path) -> PathBuf {
    let sound = directory.join("tone.wav");
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.5*sin(2*PI*1000*t)|0|0|0|0|0:s=48000:c=5.1:d=4",
            "-c:a",
            "pcm_s24le",
        ])
        .arg(&sound)
        .output()
        .expect("ffmpeg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    sound
}

fn assert_db(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < TOLERANCE_DB,
        "{actual} dBFS, expected {expected}"
    );
}

#[test]
fn the_meter_reads_each_channel_mpv_plays() {
    let directory = tempfile::tempdir().unwrap();
    let sound = five_point_one_tone(directory.path());
    let player = MpvRenderPlayer::new().expect("create mpv");
    // never the real sound device
    player.set_property("ao", "null").unwrap();
    player.init_software().expect("software render context");
    player.set_property("aid", "auto").unwrap();
    assert!(player.audio_levels().is_none(), "the meter starts off");

    player.set_level_meter(true).unwrap();
    player.load_file(sound.to_str().unwrap()).unwrap();
    let deadline = Instant::now() + LEVELS_TIMEOUT;
    let levels = loop {
        let levels = player.audio_levels();
        let measured = levels
            .as_ref()
            .is_some_and(|levels| levels[0].peak_dbfs > SILENCE_FLOOR_DBFS);
        if measured {
            break levels.unwrap();
        }
        assert!(Instant::now() < deadline, "no levels, last read {levels:?}");
        std::thread::sleep(POLL_INTERVAL);
    };

    assert_eq!(player.get_property_string("current-ao").unwrap(), "null");
    let labels: Vec<&str> = levels.iter().map(|level| level.label.as_str()).collect();
    assert_eq!(labels, ["FL", "FR", "FC", "LFE", "BL", "BR"]);
    assert_db(levels[0].peak_dbfs, HALF_SCALE_DBFS);
    assert_db(levels[0].rms_dbfs, HALF_SCALE_SINE_RMS_DBFS);
    for silent in &levels[1..] {
        assert_eq!(silent.peak_dbfs, SILENCE_FLOOR_DBFS, "{}", silent.label);
        assert_eq!(silent.rms_dbfs, SILENCE_FLOOR_DBFS, "{}", silent.label);
    }

    player.set_property("pause", "yes").unwrap();
    let paused = player.audio_levels().unwrap();
    assert_eq!(
        paused[0].peak_dbfs, SILENCE_FLOOR_DBFS,
        "a paused player is heard as silence"
    );

    player.set_level_meter(false).unwrap();
    assert!(player.audio_levels().is_none());
    let filters = player.get_property_string("af").unwrap();
    assert!(
        !filters.contains(FILTER_LABEL),
        "the filter stays: {filters}"
    );
}
